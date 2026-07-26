// SPDX-License-Identifier: AGPL-3.0-or-later
//! Health rulesets.
//!
//! A ruleset describes how to tell whether one extension is working. It ships
//! inside the extension it describes, so it cannot drift from that image and a
//! rollback restores the matching rules automatically.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Rulesets are only ever read from here. This path resolves into either the
/// sealed base image or a merged extension, both dm-verity protected and
/// signed. Nothing under `/etc` or `/var` is consulted, because the agent runs
/// as root and executes what a ruleset names.
pub const RULESET_DIR: &str = "/usr/lib/carbide/health.d";

/// A command named by a ruleset must live under one of these prefixes, so a
/// writable path can never become executable input to the recovery component.
const EXECUTABLE_PREFIXES: [&str; 2] = ["/usr/", "/opt/"];

#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    Malformed { path: PathBuf, reason: String },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Malformed { path, reason } => {
                write!(f, "{}: {reason}", path.display())
            }
        }
    }
}

impl From<io::Error> for ConfigError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, Clone)]
pub struct Ruleset {
    pub unit: String,
    pub ready_timeout: Duration,
    pub start_attempts: u32,
    pub health_command: Option<Vec<String>>,
    pub health_interval: Duration,
    pub health_timeout: Duration,
    pub soak: Duration,
}

impl Ruleset {
    fn defaults() -> Self {
        Self {
            unit: String::new(),
            ready_timeout: Duration::from_secs(90),
            start_attempts: 3,
            health_command: None,
            health_interval: Duration::from_secs(10),
            health_timeout: Duration::from_secs(15),
            soak: Duration::from_secs(120),
        }
    }

    /// Load the ruleset for a single named extension.
    pub fn load_named(name: &str) -> Result<Self, ConfigError> {
        Self::load(&Path::new(RULESET_DIR).join(format!("{name}.conf")))
    }

    fn load(path: &Path) -> Result<Self, ConfigError> {
        let contents = fs::read_to_string(path)?;
        let mut ruleset = Self::defaults();
        let mut in_section = false;

        for raw in contents.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') {
                in_section = line.eq_ignore_ascii_case("[Extension]");
                continue;
            }
            if !in_section {
                continue;
            }

            let Some((key, value)) = line.split_once('=') else {
                return Err(ConfigError::Malformed {
                    path: path.to_path_buf(),
                    reason: format!("expected KEY=VALUE, found {line:?}"),
                });
            };
            let value = value.trim();
            let malformed = |reason: String| ConfigError::Malformed {
                path: path.to_path_buf(),
                reason,
            };

            match key.trim().to_ascii_lowercase().as_str() {
                "unit" => ruleset.unit = value.to_string(),
                "readytimeoutsec" => {
                    ruleset.ready_timeout = parse_seconds(value).map_err(malformed)?;
                }
                "startattempts" => {
                    ruleset.start_attempts = value
                        .parse()
                        .map_err(|_| malformed(format!("invalid StartAttempts {value:?}")))?;
                }
                "healthcommand" => {
                    let parts: Vec<String> = value.split_whitespace().map(str::to_string).collect();
                    if parts.is_empty() {
                        return Err(malformed("empty HealthCommand".into()));
                    }
                    ruleset.health_command = Some(parts);
                }
                "healthintervalsec" => {
                    ruleset.health_interval = parse_seconds(value).map_err(malformed)?;
                }
                "healthtimeoutsec" => {
                    ruleset.health_timeout = parse_seconds(value).map_err(malformed)?;
                }
                "soaksec" => ruleset.soak = parse_seconds(value).map_err(malformed)?,
                other => {
                    return Err(malformed(format!("unknown key {other:?}")));
                }
            }
        }

        ruleset.validate(path)?;
        Ok(ruleset)
    }

    fn validate(&self, path: &Path) -> Result<(), ConfigError> {
        let malformed = |reason: String| ConfigError::Malformed {
            path: path.to_path_buf(),
            reason,
        };

        if self.unit.is_empty() {
            return Err(malformed("Unit is required".into()));
        }
        if self.unit.contains('/') || self.unit.starts_with('-') {
            return Err(malformed(format!("implausible unit name {:?}", self.unit)));
        }
        if self.start_attempts == 0 {
            return Err(malformed("StartAttempts must be at least 1".into()));
        }

        // A ruleset is trusted because it arrived through verity. A command it
        // names has to come from the same place, or the trust does not carry.
        if let Some(command) = &self.health_command {
            let program = &command[0];
            if !EXECUTABLE_PREFIXES
                .iter()
                .any(|prefix| program.starts_with(prefix))
            {
                return Err(malformed(format!(
                    "HealthCommand {program:?} is not on a verity-backed path"
                )));
            }
            if program.contains("..") {
                return Err(malformed("HealthCommand must not traverse".into()));
            }
        }

        Ok(())
    }
}

fn parse_seconds(value: &str) -> Result<Duration, String> {
    let digits = value.strip_suffix('s').unwrap_or(value);
    digits
        .parse::<u64>()
        .map(Duration::from_secs)
        .map_err(|_| format!("invalid duration {value:?}"))
}
