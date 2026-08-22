// SPDX-License-Identifier: AGPL-3.0-or-later
//! The privileged socket. See `PROTOCOL.md` for the contract.
//!
//! Watchtower runs with PrivateDevices and no capabilities, so it cannot
//! resolve $BOOT, write the extensions directory, or merge an image. A setuid
//! path would hand the sandbox a way out; a socket does not, and the answers
//! come back typed rather than scraped.
//!
//! Wire format only — the work lives in `ops`. Requests are matched on a
//! string rather than a serde enum so an unknown command returns `unsupported`
//! rather than a parse error; callers are routinely newer than the agent.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;

use crate::ops;
use crate::protocol::{COMMANDS, Code, Failure, Fields, Reply, Stage};
use crate::state::{self, State};
use crate::system;

/// The descriptor systemd hands a socket-activated service.
const LISTEN_FD: i32 = 3;

/// One privileged operation at a time.
///
/// Activation stops a unit, swaps an image and soaks; a base stage writes the
/// spare slot. Two of those at once corrupts whichever finishes second, so the
/// second caller is told `busy` rather than allowed to interleave.
static BUSY: AtomicBool = AtomicBool::new(false);

struct Guard;

impl Guard {
    fn take() -> Option<Self> {
        (!BUSY.swap(true, Ordering::AcqRel)).then_some(Guard)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}

fn busy() -> Reply {
    Reply::failed(Failure::new(
        Code::Busy,
        "another privileged operation is in flight",
    ))
}

fn dispatch(request: &Value) -> Reply {
    let Some(command) = request.text("command") else {
        return Reply::failed(Failure::new(Code::Malformed, "request has no command"));
    };

    match command.as_str() {
        "hello" => hello(),
        "update-status" => update_status(),
        "stage-extension" => match Guard::take() {
            Some(_guard) => stage_extension(request),
            None => busy(),
        },
        "activate-extension" => match Guard::take() {
            Some(_guard) => activate_extension(request),
            None => busy(),
        },
        "stage-base" => match Guard::take() {
            Some(_guard) => stage_base(request),
            None => busy(),
        },
        "logs" => logs(request),
        other => Reply::failed(Failure::unsupported(other)),
    }
}

fn hello() -> Reply {
    Reply::ok()
        .with("agent_version", env!("CARGO_PKG_VERSION"))
        .with(
            "capabilities",
            COMMANDS
                .iter()
                .map(|c| (*c).to_string())
                .collect::<Vec<_>>(),
        )
        .with("components", system::components())
        .maybe("base_version", state::os_version())
}

/// Unchanged in shape from the first version of this socket, so a caller that
/// predates the rest of this protocol keeps working. The added `protocol` key
/// is ignored by anything that does not know to look for it.
fn update_status() -> Reply {
    match system::available_base_version() {
        Ok(available) => Reply::ok()
            .maybe("available", available)
            .with("pending", system::update_pending()),
        Err(error) => Reply::failed(Failure::new(Code::Failed, error).at(Stage::Checking)),
    }
}

fn stage_extension(request: &Value) -> Reply {
    let Some(name) = request.text("name") else {
        return Reply::failed(Failure::new(
            Code::Malformed,
            "stage-extension needs a name",
        ));
    };
    // Left as the caller sent it. Absent means "whatever the image says",
    // which is how an image for a base this node has not booted gets staged.
    let base = request.text("base");
    if let Some(base) = base.as_deref()
        && !system::version_valid(base)
    {
        return Reply::failed(Failure::new(
            Code::Malformed,
            format!("unusable base version {base:?}"),
        ));
    }

    let source = match request.text("path") {
        Some(path) => {
            let Some(digest) = request.text("digest") else {
                return Reply::failed(Failure::new(
                    Code::Malformed,
                    "a supplied image must declare a digest",
                ));
            };
            ops::Source::Supplied {
                path: PathBuf::from(path),
                digest,
            }
        }
        None => ops::Source::Acquire {
            version: request.text("version"),
        },
    };

    match ops::stage_extension(&name, base.as_deref(), source) {
        Ok(staged) => Reply::ok()
            .with("name", staged.name)
            .with("version", staged.version)
            .with("staged", staged.path)
            .with("acquired", staged.acquired),
        Err(failure) => Reply::failed(failure),
    }
}

fn activate_extension(request: &Value) -> Reply {
    let Some(name) = request.text("name") else {
        return Reply::failed(Failure::new(
            Code::Malformed,
            "activate-extension needs a name",
        ));
    };
    let Some(version) = request.text("version") else {
        return Reply::failed(Failure::new(
            Code::Malformed,
            "activate-extension needs a version",
        ));
    };
    if !system::version_valid(&name) || !system::version_valid(&version) {
        return Reply::failed(Failure::new(Code::Malformed, "unusable name or version"));
    }

    match crate::activate_extension(&name, &version, request.text("request_id")) {
        Ok(activation) => Reply::ok()
            .with("name", name)
            .with("version", activation.version)
            .with("phase", "active")
            .with("replayed", activation.replayed)
            .maybe("known_good", activation.known_good),
        Err(failure) => Reply::failed(failure),
    }
}

fn stage_base(request: &Value) -> Reply {
    match ops::stage_base(request.text("version").as_deref()) {
        Ok(staged) => Reply::ok()
            .maybe("version", staged.version)
            .with("pending", staged.pending)
            .maybe("boot_entry", staged.boot_entry),
        Err(failure) => Reply::failed(failure),
    }
}

fn logs(request: &Value) -> Reply {
    // Capped so one reply cannot outgrow whatever the caller has to carry it
    // over. A caller that wants more pages with the cursor.
    const MAX_LINES: u64 = 2000;
    let lines = request.number("lines").unwrap_or(200).clamp(1, MAX_LINES);
    let unit = request.text("unit");
    let cursor = request.text("cursor");
    let priority = request.text("priority");

    match system::journal(
        unit.as_deref(),
        lines,
        cursor.as_deref(),
        priority.as_deref(),
    ) {
        Ok((collected, next)) => {
            let complete = (collected.len() as u64) < lines;
            Reply::ok()
                .with("lines", collected)
                .maybe("cursor", next)
                .with("eof", complete)
        }
        Err(error) => Reply::failed(
            Failure::new(Code::Failed, format!("could not read the journal: {error}"))
                .at(Stage::Checking),
        ),
    }
}

fn serve_connection(stream: UnixStream) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(stream) => stream,
        Err(_) => return,
    });
    let mut line = String::new();
    // One request per connection, newline terminated, so a caller cannot hold
    // the socket open and starve the next one.
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let reply = match serde_json::from_str::<Value>(line.trim()) {
        Ok(request) => dispatch(&request),
        Err(error) => Reply::failed(Failure::new(
            Code::Malformed,
            format!("unreadable request: {error}"),
        )),
    };
    let mut stream = stream;
    if let Ok(mut encoded) = serde_json::to_vec(&reply) {
        encoded.push(b'\n');
        let _ = stream.write_all(&encoded);
        let _ = stream.flush();
    }
}

/// Serve the socket systemd passed, or bind one when run by hand.
pub fn serve(path: Option<&str>) -> Result<(), String> {
    // Touched so a first request does not pay for creating it, and so a
    // caller staging an image has somewhere to put it.
    let _ = std::fs::create_dir_all(ops::STAGING_DIR);
    let _ = State::load();

    let listener = match path {
        Some(path) => {
            let _ = std::fs::remove_file(path);
            UnixListener::bind(path).map_err(|error| format!("could not bind {path}: {error}"))?
        }
        None => {
            let count = std::env::var("LISTEN_FDS").unwrap_or_default();
            if count.trim() != "1" {
                return Err("expected exactly one socket from systemd".to_string());
            }
            // Safe: systemd guarantees this descriptor and nothing else has it.
            unsafe { UnixListener::from_raw_fd(LISTEN_FD) }
        }
    };
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => serve_connection(stream),
            Err(error) => eprintln!("carbide-agent: connection failed: {error}"),
        }
    }
    Ok(())
}
