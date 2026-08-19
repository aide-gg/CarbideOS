// SPDX-License-Identifier: AGPL-3.0-or-later
//! A socket for asking the agent to do privileged things.
//!
//! Watchtower runs with PrivateDevices and an empty capability set, so it
//! cannot resolve $BOOT and every sysupdate call it made failed. Shelling out
//! to a setuid path would hand the sandbox a way out; a socket does not, and
//! the answers come back typed rather than scraped from a program's output.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::{UnixListener, UnixStream};

use serde::{Deserialize, Serialize};

use crate::system;

/// The descriptor systemd hands a socket-activated service.
const LISTEN_FD: i32 = 3;

#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
enum Request {
    /// What the feed offers and whether an image is already staged.
    UpdateStatus,
}

#[derive(Debug, Serialize)]
struct Reply {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    available: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending: Option<bool>,
}

impl Reply {
    fn failed(error: String) -> Self {
        Self {
            ok: false,
            error: Some(error),
            available: None,
            pending: None,
        }
    }
}

fn handle(request: Request) -> Reply {
    match request {
        Request::UpdateStatus => match system::available_base_version() {
            Ok(available) => Reply {
                ok: true,
                error: None,
                available,
                pending: Some(system::update_pending()),
            },
            Err(error) => Reply::failed(error),
        },
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
    let reply = match serde_json::from_str::<Request>(line.trim()) {
        Ok(request) => handle(request),
        Err(error) => Reply::failed(format!("unreadable request: {error}")),
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
