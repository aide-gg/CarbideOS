// SPDX-License-Identifier: AGPL-3.0-or-later
//! carbide-agent — CarbideOS system extension supervisor.
//!
//! Base OS updates get A/B slots and boot counting, so a bad image fails and
//! firmware falls back. System extensions get none of that: a broken extension
//! merges perfectly, the node comes up healthy, and whatever the extension
//! provided is simply missing or crash-looping. This closes that gap.
//!
//! It is deliberately generic. It does not know what any extension does, holds
//! no credentials, and opens no network connections. Recovery is a local
//! switch between images already on disk, so it works on a node with no route
//! to anywhere and no help from a provisioner.

mod api;
mod config;
mod state;
mod system;

use std::process::ExitCode;

use config::Ruleset;
use state::{Phase, State};

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let command = arguments.first().map(String::as_str).unwrap_or("help");

    let result = match command {
        "health-gate" => health_gate(),
        "activate" => activate(&arguments[1..]),
        "adopt" => adopt(&arguments[1..]),
        "rollback" => rollback_command(&arguments[1..]),
        "require" => set_required(&arguments[1..], true),
        "unrequire" => set_required(&arguments[1..], false),
        "reset" => reset(&arguments[1..]),
        "status" => status(),
        "serve" => api::serve(arguments.get(1).map(String::as_str)),
        "help" | "--help" | "-h" => {
            usage();
            return ExitCode::SUCCESS;
        }
        other => Err(format!("unknown command {other:?}")),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("carbide-agent: {message}");
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    println!(
        "\
Usage: carbide-agent COMMAND

  health-gate              Verify every required extension is healthy
  activate NAME VERSION    Activate a staged candidate, reverting on failure
  adopt NAME VERSION       Record an already-active, healthy image as known-good
  rollback NAME            Return an extension to its known-good image
  require NAME             Record that this node must have NAME
  unrequire NAME           Stop requiring NAME
  reset NAME               Clear a terminal state so NAME is checked again
  status                   Report agent state as JSON
  serve [PATH]             Answer requests on a socket, systemd's if PATH is absent

Rulesets are read only from /usr/lib/carbide/health.d.
Images live in /var/lib/extensions; only the active one ends in .raw."
    );
}

/// Bring an image installed before the agent under supervision without
/// replacing it. This is primarily the migration path for fleet nodes whose
/// first Watchtower sysext predates candidate activation.
fn adopt(arguments: &[String]) -> Result<(), String> {
    let name = arguments
        .first()
        .ok_or("usage: carbide-agent adopt NAME VERSION")?;
    let version = arguments
        .get(1)
        .ok_or("usage: carbide-agent adopt NAME VERSION")?;
    let active = system::active_path(name);
    if !active.exists() {
        return Err(format!("no active image at {}", active.display()));
    }

    let ruleset = Ruleset::load_named(name)
        .map_err(|error| format!("active image has no usable ruleset: {error}"))?;
    healthy_now(&ruleset)?;

    let retained = system::rollback_path(name, version);
    if !retained.exists() {
        std::fs::copy(&active, &retained).map_err(|error| error.to_string())?;
    }
    let mut state = State::load().map_err(|error| error.to_string())?;
    let entry = state.entry(name);
    entry.required = true;
    entry.active_version = Some(version.clone());
    entry.known_good_version = Some(version.clone());
    entry.phase = Phase::Active;
    entry.terminal_os_version = None;
    entry.last_health = Some("healthy".into());
    entry.last_failure = None;
    state.store().map_err(|error| error.to_string())?;
    println!("{name}: adopted active {version} as known-good");
    Ok(())
}

/// Gate boot assessment on the node actually being able to do its job.
///
/// This runs before boot-complete.target, so failing it withholds the blessing
/// and lets base A/B rollback take over. Extension-level recovery is attempted
/// first, because extensions live on the state partition shared by both base
/// slots — rolling back the base OS cannot restore an extension, so gating
/// before recovery would trigger a rollback that could not possibly help.
fn health_gate() -> Result<(), String> {
    let mut state = State::load().map_err(|error| error.to_string())?;
    if let Some(version) = state::os_version() {
        for name in state.forget_stale_terminals(&version) {
            println!("{name}: clearing a refusal recorded under a previous image");
        }
    }
    let required = state.required();

    if required.is_empty() {
        println!("no required extensions; nothing to verify");
        return Ok(());
    }

    let merged = system::merged_extensions().unwrap_or_default();
    let mut failures = Vec::new();

    for name in required {
        match verify_or_recover(&mut state, &name, &merged) {
            Ok(()) => println!("{name}: healthy"),
            Err(reason) => {
                eprintln!("{name}: {reason}");
                failures.push(name);
            }
        }
    }

    state.store().map_err(|error| error.to_string())?;

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "required extensions unhealthy: {}",
            failures.join(", ")
        ))
    }
}

fn verify_or_recover(state: &mut State, name: &str, merged: &[String]) -> Result<(), String> {
    if state.get(name).is_some_and(|e| e.phase.is_terminal()) {
        return Err("in a terminal state; not retrying automatically".into());
    }

    let ruleset = match Ruleset::load_named(name) {
        Ok(ruleset) => ruleset,
        Err(error) => {
            // No ruleset means the extension is not merged, or shipped without
            // one. Either way a required extension is not usable.
            let reason = format!("no usable ruleset: {error}");
            fail(state, name, &reason);
            return Err(reason);
        }
    };

    if !merged.is_empty() && !merged.iter().any(|m| m == name) {
        return recover(state, name, "required extension is not merged");
    }

    if let Err(reason) = healthy_now(&ruleset) {
        return recover(state, name, &reason);
    }

    let entry = state.entry(name);
    entry.phase = Phase::Active;
    // Proven working, so a refusal recorded under some earlier image is no
    // longer describing anything.
    entry.terminal_os_version = None;
    entry.last_health = Some("healthy".into());
    entry.last_failure = None;
    Ok(())
}

fn healthy_now(ruleset: &Ruleset) -> Result<(), String> {
    system::wait_until_ready(ruleset)?;
    system::probe(ruleset)
}

/// Restore the retained known-good image and confirm it works.
fn recover(state: &mut State, name: &str, reason: &str) -> Result<(), String> {
    let known_good = state.get(name).and_then(|e| e.known_good_version.clone());

    let Some(version) = known_good else {
        let message = format!("{reason}; no known-good image to fall back to");
        fail(state, name, &message);
        return Err(message);
    };

    let image = system::rollback_path(name, &version);
    if !image.exists() {
        let message = format!("{reason}; known-good image {version} is missing");
        fail(state, name, &message);
        return Err(message);
    }

    eprintln!("{name}: {reason}; reverting to {version}");
    state.entry(name).phase = Phase::Reverting;
    let _ = state.store();

    // Stop whatever the currently merged image declares, if it declares
    // anything. A candidate that failed to merge leaves nothing to stop.
    if let Ok(current) = Ruleset::load_named(name) {
        let _ = system::stop_unit(&current.unit);
    }

    system::install_active(&image, name).map_err(|error| error.to_string())?;
    system::sysext_refresh().map_err(|error| error.to_string())?;
    let _ = system::daemon_reload();

    // Read the ruleset only now. It arrives with the image, so this is the
    // restored version's own ruleset rather than the failed candidate's.
    let ruleset = match Ruleset::load_named(name) {
        Ok(ruleset) => ruleset,
        Err(error) => {
            let message = format!("{reason}; restored {version} has no usable ruleset: {error}");
            fail(state, name, &message);
            return Err(message);
        }
    };

    if let Err(error) = healthy_now(&ruleset) {
        let message = format!("{reason}; known-good {version} also failed: {error}");
        fail(state, name, &message);
        return Err(message);
    }

    // Discard the candidate that caused this. Leaving it would be mildly
    // useful for a post-mortem and considerably worse in aggregate: repeated
    // failed activations would fill the state partition with images no longer
    // reachable by anything. The reason is already recorded below.
    let discarded = state.get(name).and_then(|e| e.candidate_version.clone());
    if let Some(candidate) = discarded {
        let _ = std::fs::remove_file(system::candidate_path(name, &candidate));
    }

    let entry = state.entry(name);
    entry.phase = Phase::Recovered;
    entry.active_version = Some(version.clone());
    entry.candidate_version = None;
    entry.last_health = Some("recovered".into());
    entry.last_failure = Some(reason.to_string());
    println!("{name}: recovered onto {version}");
    Ok(())
}

fn fail(state: &mut State, name: &str, reason: &str) {
    let version = state::os_version();
    let entry = state.entry(name);
    entry.phase = Phase::Unrecoverable;
    entry.last_failure = Some(reason.to_string());
    entry.last_health = None;
    // Stamped so a rollback onto a different base image can tell that this
    // refusal belonged to the image that has since been replaced.
    entry.terminal_os_version = version;
}

/// Promote a staged candidate, reverting if it does not prove itself.
fn activate(arguments: &[String]) -> Result<(), String> {
    let name = arguments
        .first()
        .ok_or("usage: carbide-agent activate NAME VERSION")?;
    let version = arguments
        .get(1)
        .ok_or("usage: carbide-agent activate NAME VERSION")?;
    let request_id = arguments.get(2).cloned();

    let mut state = State::load().map_err(|error| error.to_string())?;

    if let (Some(id), Some(entry)) = (request_id.as_ref(), state.get(name))
        && entry.already_completed(id)
    {
        println!("{name}: request {id} already applied");
        return Ok(());
    }

    let candidate = system::candidate_path(name, version);
    if !candidate.exists() {
        return Err(format!("no staged candidate at {}", candidate.display()));
    }

    let candidate_size = std::fs::metadata(&candidate)
        .map_err(|error| error.to_string())?
        .len();
    let available = system::available_bytes().map_err(|error| error.to_string())?;
    if available < candidate_size {
        return Err(format!(
            "insufficient space: candidate needs {candidate_size} bytes, {available} available"
        ));
    }

    let previous = state.get(name).and_then(|e| e.active_version.clone());

    // Retain the outgoing image before overwriting it, so there is always
    // something to fall back to even if the machine dies mid-activation.
    if let Some(previous_version) = &previous {
        let active = system::active_path(name);
        let retained = system::rollback_path(name, previous_version);
        if active.exists() && !retained.exists() {
            std::fs::copy(&active, &retained).map_err(|error| error.to_string())?;
        }
    }

    {
        let entry = state.entry(name);
        entry.candidate_version = Some(version.clone());
        entry.request_id = request_id.clone();
        entry.phase = Phase::Activating;
        if let Some(previous_version) = &previous {
            entry.known_good_version = Some(previous_version.clone());
        }
    }
    state.store().map_err(|error| error.to_string())?;

    // Stop what the outgoing image declared, if anything is merged yet.
    if let Ok(current) = Ruleset::load_named(name) {
        let _ = system::stop_unit(&current.unit);
    }
    system::install_active(&candidate, name).map_err(|error| error.to_string())?;
    system::sysext_refresh().map_err(|error| error.to_string())?;
    let _ = system::daemon_reload();

    // The ruleset ships inside the extension, so it only exists once the
    // candidate has merged. A first activation has nothing to read before this
    // point, and a candidate that fails to merge supplies nothing at all.
    let outcome = match Ruleset::load_named(name) {
        Ok(ruleset) => {
            state.entry(name).phase = Phase::Starting;
            let _ = state.store();
            system::wait_until_ready(&ruleset).and_then(|()| {
                state.entry(name).phase = Phase::Soaking;
                let _ = state.store();
                system::soak(&ruleset)
            })
        }
        Err(error) => Err(format!("candidate supplied no usable ruleset: {error}")),
    };

    if let Err(reason) = outcome {
        let result = recover(&mut state, name, &reason);
        state.store().map_err(|error| error.to_string())?;
        return match result {
            Ok(()) => Err(format!("{name}: {reason}; reverted")),
            Err(error) => Err(error),
        };
    }

    {
        let entry = state.entry(name);
        entry.phase = Phase::Active;
        // Proven working, so a refusal recorded under some earlier image is no
        // longer describing anything.
        entry.terminal_os_version = None;
        entry.active_version = Some(version.clone());
        entry.candidate_version = None;
        entry.last_health = Some("healthy".into());
        entry.last_failure = None;
        entry.required = true;
        if let Some(id) = &request_id {
            entry.record_completed(id);
        }
    }
    state.store().map_err(|error| error.to_string())?;

    // The candidate has proven itself, so it becomes the image to fall back to
    // next time and the staged copy is no longer needed.
    let _ = std::fs::remove_file(&candidate);
    println!("{name}: active on {version}");
    Ok(())
}

fn rollback_command(arguments: &[String]) -> Result<(), String> {
    let name = arguments
        .first()
        .ok_or("usage: carbide-agent rollback NAME")?;
    let mut state = State::load().map_err(|error| error.to_string())?;
    let result = recover(&mut state, name, "operator requested rollback");
    state.store().map_err(|error| error.to_string())?;
    result
}

fn set_required(arguments: &[String], required: bool) -> Result<(), String> {
    let name = arguments
        .first()
        .ok_or("usage: carbide-agent require|unrequire NAME")?;
    let mut state = State::load().map_err(|error| error.to_string())?;
    state.entry(name).required = required;
    state.store().map_err(|error| error.to_string())?;
    println!(
        "{name}: {}",
        if required {
            "required"
        } else {
            "no longer required"
        }
    );
    Ok(())
}

/// Return an extension to a checkable state.
///
/// The agent deliberately stops rather than looping once it reaches a terminal
/// state, so an operator who has fixed the underlying cause needs a way to say
/// so. Without this, the only route back is editing the state file by hand.
fn reset(arguments: &[String]) -> Result<(), String> {
    let name = arguments.first().ok_or("usage: carbide-agent reset NAME")?;
    let mut state = State::load().map_err(|error| error.to_string())?;
    {
        let entry = state.entry(name);
        entry.phase = Phase::Idle;
        entry.last_failure = None;
        entry.last_health = None;
    }
    state.store().map_err(|error| error.to_string())?;
    println!("{name}: reset; will be checked again");
    Ok(())
}

fn status() -> Result<(), String> {
    let state = State::load().map_err(|error| error.to_string())?;
    let encoded = serde_json::to_string_pretty(&state).map_err(|error| error.to_string())?;
    println!("{encoded}");
    Ok(())
}
