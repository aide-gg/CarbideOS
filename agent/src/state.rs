// SPDX-License-Identifier: AGPL-3.0-or-later
//! Durable agent state.
//!
//! This records which extensions the node is supposed to have, which image is
//! active, and which one to fall back to. The requirement in particular has to
//! live outside the extensions themselves: a node that has lost an extension
//! entirely would otherwise have nothing left to tell it anything is wrong,
//! which is the exact failure this component exists to catch.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const STATE_DIR: &str = "/var/lib/carbide/agent";
const STATE_FILE: &str = "state.json";

/// Bounded so a long-lived node cannot grow this without limit, while still
/// being deep enough that a replayed activation cannot invert a rollback.
const COMPLETED_REQUEST_HISTORY: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    Staged,
    Activating,
    Starting,
    Soaking,
    Active,
    Reverting,
    Recovered,
    Unrecoverable,
}

impl Phase {
    /// A phase the agent should not automatically retry out of.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Unrecoverable)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtensionState {
    /// Whether this node is supposed to have this extension at all.
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_good_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_version: Option<String>,
    #[serde(default)]
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_health: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
    /// The base image that was running when a terminal phase was recorded.
    ///
    /// Extension state lives on the state partition and so outlives any base
    /// rollback. Without this, a node that gave up on an extension while
    /// running a bad image kept refusing it after rolling back to a good one:
    /// the extension was merged and working, the gate failed anyway, and the
    /// next legitimate update would have been reverted on a healthy image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_os_version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_requests: Vec<String>,
}

/// The running base image version, as the agent should record it.
///
/// Read from the sealed image rather than tracked in state, so it cannot drift
/// from the image actually booted.
pub fn os_version() -> Option<String> {
    let contents = fs::read_to_string("/etc/os-release").ok()?;
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("VERSION_ID=") {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

impl ExtensionState {
    pub fn record_completed(&mut self, request_id: &str) {
        if self.completed_requests.iter().any(|id| id == request_id) {
            return;
        }
        self.completed_requests.push(request_id.to_string());
        let excess = self
            .completed_requests
            .len()
            .saturating_sub(COMPLETED_REQUEST_HISTORY);
        if excess > 0 {
            self.completed_requests.drain(0..excess);
        }
    }

    pub fn already_completed(&self, request_id: &str) -> bool {
        self.completed_requests.iter().any(|id| id == request_id)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub extensions: BTreeMap<String, ExtensionState>,
}

impl State {
    /// Forget a refusal that belongs to a base image no longer running.
    ///
    /// A terminal phase means "do not retry this automatically", which is right
    /// while the conditions that caused it still hold. A base rollback replaces
    /// exactly those conditions, so carrying the refusal across would leave a
    /// recovered node permanently ungated for no reason.
    pub fn forget_stale_terminals(&mut self, current: &str) -> Vec<String> {
        let mut cleared = Vec::new();
        for (name, entry) in self.extensions.iter_mut() {
            if !entry.phase.is_terminal() {
                continue;
            }
            if entry.terminal_os_version.as_deref() == Some(current) {
                continue;
            }
            entry.phase = Phase::Idle;
            entry.last_failure = None;
            entry.terminal_os_version = None;
            cleared.push(name.clone());
        }
        cleared
    }

    fn path() -> PathBuf {
        Path::new(STATE_DIR).join(STATE_FILE)
    }

    pub fn load() -> io::Result<Self> {
        match fs::read(Self::path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("corrupt agent state: {error}"),
                )
            }),
            // No state yet is the normal first boot, not an error.
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    /// Write to a temporary file, flush it to disk, rename over the target, and
    /// flush the directory. An interrupted update must leave either the old
    /// state or the new one, never a truncated file, because this record is
    /// what tells the node which image to fall back to.
    pub fn store(&self) -> io::Result<()> {
        let directory = Path::new(STATE_DIR);
        fs::create_dir_all(directory)?;

        let target = Self::path();
        let staging = directory.join(format!(".{STATE_FILE}.new"));

        let mut encoded = serde_json::to_vec_pretty(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        encoded.push(b'\n');

        {
            let mut file = File::create(&staging)?;
            file.write_all(&encoded)?;
            file.sync_all()?;
        }

        fs::rename(&staging, &target)?;
        File::open(directory)?.sync_all()?;
        Ok(())
    }

    pub fn entry(&mut self, name: &str) -> &mut ExtensionState {
        self.extensions.entry(name.to_string()).or_default()
    }

    pub fn get(&self, name: &str) -> Option<&ExtensionState> {
        self.extensions.get(name)
    }

    /// Extensions this node is supposed to have, in deterministic order.
    pub fn required(&self) -> Vec<String> {
        self.extensions
            .iter()
            .filter(|(_, state)| state.required)
            .map(|(name, _)| name.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{ExtensionState, Phase, State};

    fn terminal_on(version: &str) -> ExtensionState {
        ExtensionState {
            required: true,
            phase: Phase::Unrecoverable,
            last_failure: Some("no usable ruleset".into()),
            terminal_os_version: Some(version.into()),
            ..Default::default()
        }
    }

    /// A node that gave up on an extension while running a bad image kept
    /// refusing it after boot counting rolled the base back to a good one. The
    /// extension was merged and working, but the gate failed anyway, so the
    /// next legitimate update would have been reverted on a healthy image.
    #[test]
    fn a_refusal_does_not_survive_the_rollback_that_fixes_it() {
        let mut state = State::default();
        state
            .extensions
            .insert("watchtower".into(), terminal_on("0.1.45"));

        assert_eq!(state.forget_stale_terminals("0.1.43"), vec!["watchtower"]);
        let entry = &state.extensions["watchtower"];
        assert_eq!(entry.phase, Phase::Idle);
        assert!(entry.last_failure.is_none());
        assert!(entry.required, "clearing a refusal must not unrequire it");
    }

    /// Still running the image that failed, so the refusal is what stops the
    /// agent looping on an extension it has already proven it cannot fix.
    #[test]
    fn a_refusal_survives_a_reboot_onto_the_same_image() {
        let mut state = State::default();
        state
            .extensions
            .insert("watchtower".into(), terminal_on("0.1.45"));

        assert!(state.forget_stale_terminals("0.1.45").is_empty());
        assert_eq!(state.extensions["watchtower"].phase, Phase::Unrecoverable);
    }
}
