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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_requests: Vec<String>,
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
