// SPDX-License-Identifier: AGPL-3.0-or-later
//! Wire types for the agent socket. See `PROTOCOL.md`.
//!
//! Requests are not deserialised into an enum. An unknown command must come
//! back as `unsupported` with the list of what exists, never as a parse error,
//! because a caller that cannot tell "I asked for something you do not have"
//! from "you are not there" ends up guessing — and the guess this protocol
//! replaced was to run a privileged tool from inside a sandbox and report its
//! failure as if it described the node.

use serde::Serialize;
use serde_json::{Map, Value};

pub const PROTOCOL_VERSION: u32 = 1;

pub const COMMANDS: &[&str] = &[
    "hello",
    "update-status",
    "stage-extension",
    "activate-extension",
    "stage-base",
    "logs",
];

/// How far the agent got before it failed. Absent when it never started.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Checking,
    Validating,
    Staging,
    Activating,
    Soaking,
    Reverting,
}

/// Stable, machine-readable. `message` is for humans and may change; this may
/// not, because callers branch on it.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    Unsupported,
    Malformed,
    NotFound,
    DigestMismatch,
    Untrusted,
    BaseMismatch,
    Busy,
    NoSpace,
    Precondition,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub code: Code,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<Stage>,
    /// The command line that failed, so an operator can run it by hand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub log: Vec<String>,
    /// Only populated for `unsupported`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub supported: Vec<String>,
}

/// Enough to keep a reply well under any plausible transport limit while still
/// carrying the part of a failure that explains it. Callers that want more ask
/// for `logs`.
const STDERR_LIMIT: usize = 4096;

impl Failure {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            stage: None,
            command: None,
            exit_code: None,
            stderr: None,
            unit: None,
            log: Vec::new(),
            supported: Vec::new(),
        }
    }

    pub fn at(mut self, stage: Stage) -> Self {
        self.stage = Some(stage);
        self
    }

    pub fn command(mut self, command: impl Into<String>) -> Self {
        self.command = Some(command.into());
        self
    }

    pub fn exit_code(mut self, code: Option<i32>) -> Self {
        self.exit_code = code;
        self
    }

    /// Keeps the tail, which is where a program says why it gave up.
    pub fn stderr(mut self, stderr: impl AsRef<str>) -> Self {
        let text = stderr.as_ref().trim();
        if text.is_empty() {
            return self;
        }
        let trimmed = if text.len() > STDERR_LIMIT {
            let start = text.len() - STDERR_LIMIT;
            let start = text
                .char_indices()
                .map(|(index, _)| index)
                .find(|index| *index >= start)
                .unwrap_or(text.len());
            format!("...{}", &text[start..])
        } else {
            text.to_string()
        };
        self.stderr = Some(trimmed);
        self
    }

    pub fn unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }

    pub fn log(mut self, lines: Vec<String>) -> Self {
        self.log = lines;
        self
    }

    pub fn unsupported(command: &str) -> Self {
        let mut failure = Self::new(Code::Unsupported, format!("unknown command {command:?}"));
        failure.supported = COMMANDS.iter().map(|name| (*name).to_string()).collect();
        failure
    }
}

#[derive(Debug, Serialize)]
pub struct Reply {
    pub ok: bool,
    pub protocol: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Failure>,
    #[serde(flatten)]
    pub data: Map<String, Value>,
}

impl Reply {
    pub fn ok() -> Self {
        Self {
            ok: true,
            protocol: PROTOCOL_VERSION,
            error: None,
            data: Map::new(),
        }
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.data.insert(key.to_string(), value.into());
        self
    }

    /// Omits the key entirely rather than writing null, so a caller reading an
    /// absent field and a caller reading a null one cannot disagree.
    pub fn maybe(self, key: &str, value: Option<impl Into<Value>>) -> Self {
        match value {
            Some(value) => self.with(key, value),
            None => self,
        }
    }

    pub fn failed(failure: Failure) -> Self {
        Self {
            ok: false,
            protocol: PROTOCOL_VERSION,
            error: Some(failure),
            data: Map::new(),
        }
    }
}

/// Field access that tolerates a caller newer or older than this agent.
pub trait Fields {
    fn text(&self, key: &str) -> Option<String>;
    fn number(&self, key: &str) -> Option<u64>;
}

impl Fields for Value {
    fn text(&self, key: &str) -> Option<String> {
        self.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    fn number(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(Value::as_u64)
    }
}
