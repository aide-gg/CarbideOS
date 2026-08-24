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

// A structured failure is around 160 bytes, which clippy flags in a Result.
// Boxing it would put an allocation on the path taken when something has
// already gone wrong, in the component whose job is to keep working when
// everything else has stopped. These are cold branches in a small binary; the
// bytes are cheaper than the allocation.
#![allow(clippy::result_large_err)]

mod api;
mod config;
mod ops;
mod protocol;
mod state;
mod system;

use std::collections::BTreeSet;
use std::process::ExitCode;

use config::Ruleset;
use protocol::{Code, Failure, Stage};
use state::{OperationLock, Phase, State};

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let command = arguments.first().map(String::as_str).unwrap_or("help");

    let result = match command {
        "health-gate" => locked(health_gate),
        "activate" => locked(|| activate(&arguments[1..])),
        "stage-extension" => locked(|| stage_extension(&arguments[1..])),
        "stage-base" => locked(|| stage_base(&arguments[1..])),
        "adopt" => locked(|| adopt(&arguments[1..])),
        "rollback" => locked(|| rollback_command(&arguments[1..])),
        "remove" => locked(|| remove_extension(&arguments[1..])),
        "require" => locked(|| set_required(&arguments[1..], true)),
        "unrequire" => locked(|| set_required(&arguments[1..], false)),
        "reset" => locked(|| reset(&arguments[1..])),
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

fn locked(operation: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    let _lock = OperationLock::acquire()
        .map_err(|error| format!("could not acquire operation lock: {error}"))?;
    reconcile_state()?;
    operation()
}

fn usage() {
    println!(
        "\
Usage: carbide-agent COMMAND

  health-gate              Verify every required extension is healthy
  activate NAME VERSION    Activate a staged candidate, reverting on failure
  stage-extension NAME [--base V] [--path P --digest D] [--version V]
                           Place an image for a base version. Without --path
                           the agent fetches it through its own configuration.
  stage-base [VERSION]     Stage a base image and select its boot entry
  adopt NAME VERSION       Record an already-active, healthy image as known-good
  rollback NAME            Return an extension to its known-good image
  remove NAME              Remove an optional extension from the running base
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
    let mut state = State::load().map_err(|error| error.to_string())?;
    adopt_active(&mut state, name, version)?;
    println!("{name}: adopted active {version} as known-good");
    Ok(())
}

fn adopt_active(state: &mut State, name: &str, version: &str) -> Result<(), String> {
    let active = system::active_path(name);
    if !active.exists() {
        return Err(format!("no active image at {}", active.display()));
    }

    let ruleset = Ruleset::load_named(name)
        .map_err(|error| format!("active image has no usable ruleset: {error}"))?;
    healthy_now(&ruleset)?;

    let retained = system::rollback_path(name, version);
    if !retained.exists() {
        system::durable_copy(&active, &retained).map_err(|error| error.to_string())?;
    }
    let entry = state.entry(name);
    entry.required = true;
    entry.active_version = Some(version.to_string());
    entry.known_good_version = Some(version.to_string());
    entry.phase = Phase::Active;
    entry.terminal_os_version = None;
    entry.last_health = Some("healthy".into());
    entry.last_failure = None;
    state.store().map_err(|error| error.to_string())?;
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
            let reason = format!("no usable ruleset: {error}");
            return recover(state, name, &reason);
        }
    };

    if !merged.is_empty() && !merged.iter().any(|m| m == name) {
        return recover(state, name, "required extension is not merged");
    }

    if let Err(reason) = healthy_now(&ruleset) {
        return recover(state, name, &reason);
    }

    // A/B switches change which scoped extension is merged while agent state
    // survives on /var. Adopt the image that proved healthy under this base so
    // later extension-level recovery never looks for an old base's slot.
    if let Some(version) = ops::merged_version(name) {
        let rollback = system::rollback_path(name, &version);
        let changed = state
            .get(name)
            .and_then(|entry| entry.active_version.as_deref())
            != Some(version.as_str())
            || !rollback.exists();
        if changed {
            system::soak(&ruleset)?;
            let active = system::active_path(name);
            system::durable_copy(&active, &rollback)
                .map_err(|error| format!("could not retain healthy {version}: {error}"))?;
            let entry = state.entry(name);
            entry.active_version = Some(version.clone());
            entry.known_good_version = Some(version);
            entry.candidate_version = None;
        }
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
        return terminal(state, name, message);
    };

    let image = system::rollback_path(name, &version);
    if !image.exists() {
        let message = format!("{reason}; known-good image {version} is missing");
        return terminal(state, name, message);
    }

    eprintln!("{name}: {reason}; reverting to {version}");
    {
        let entry = state.entry(name);
        entry.phase = Phase::Reverting;
        entry.last_failure = Some(reason.to_string());
    }
    state.store().map_err(|error| {
        format!("{reason}; could not record recovery before replacing the image: {error}")
    })?;

    // Stop whatever the currently merged image declares, if it declares
    // anything. A candidate that failed to merge leaves nothing to stop.
    if let Ok(current) = Ruleset::load_named(name)
        && let Err(error) = system::stop_unit(&current.unit)
    {
        // A candidate may declare a unit that never loaded. Recovery must not
        // strand a valid known-good image merely because there was nothing PID
        // 1 could stop; replacing and refreshing the image is authoritative.
        eprintln!(
            "{name}: could not stop {} before recovery: {error}",
            current.unit
        );
    }

    if let Err(error) = system::install_active(&image, name) {
        return terminal(
            state,
            name,
            format!("{reason}; could not restore known-good {version}: {error}"),
        );
    }
    if let Err(error) = system::sysext_refresh() {
        return terminal(
            state,
            name,
            format!("{reason}; restored {version} but could not refresh extensions: {error}"),
        );
    }
    if let Err(error) = system::daemon_reload() {
        return terminal(
            state,
            name,
            format!("{reason}; restored {version} but could not reload systemd: {error}"),
        );
    }

    // Read the ruleset only now. It arrives with the image, so this is the
    // restored version's own ruleset rather than the failed candidate's.
    let ruleset = match Ruleset::load_named(name) {
        Ok(ruleset) => ruleset,
        Err(error) => {
            let message = format!("{reason}; restored {version} has no usable ruleset: {error}");
            return terminal(state, name, message);
        }
    };

    if let Err(error) = healthy_now(&ruleset) {
        let message = format!("{reason}; known-good {version} also failed: {error}");
        return terminal(state, name, message);
    }

    // Discard the candidate that caused this. Leaving it would be mildly
    // useful for a post-mortem and considerably worse in aggregate: repeated
    // failed activations would fill the state partition with images no longer
    // reachable by anything. The reason is already recorded below.
    let entry = state.entry(name);
    entry.phase = Phase::Recovered;
    entry.active_version = Some(version.clone());
    entry.candidate_version = None;
    entry.last_health = Some("recovered".into());
    entry.last_failure = Some(reason.to_string());
    entry.terminal_os_version = None;
    state
        .store()
        .map_err(|error| format!("recovered onto {version}, but could not record it: {error}"))?;
    if let Err(error) = system::prune_candidates(name, None) {
        eprintln!("{name}: recovered, but candidate cleanup failed: {error}");
    }
    if let Err(error) = system::prune_rollbacks(name, Some(&version)) {
        eprintln!("{name}: recovered, but rollback cleanup failed: {error}");
    }
    println!("{name}: recovered onto {version}");
    Ok(())
}

fn terminal(state: &mut State, name: &str, reason: String) -> Result<(), String> {
    fail(state, name, &reason);
    match state.store() {
        Ok(()) => Err(reason),
        Err(error) => Err(format!(
            "{reason}; could not record terminal state: {error}"
        )),
    }
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

/// Resolve transactions interrupted by a process crash or power loss before a
/// new command is allowed to mutate anything.
pub fn reconcile_state() -> Result<(), String> {
    system::prune_transaction_files()
        .map_err(|error| format!("could not clean interrupted staging files: {error}"))?;
    let mut state = State::load().map_err(|error| error.to_string())?;
    if let Some(version) = state::os_version() {
        for name in state.forget_stale_terminals(&version) {
            println!("{name}: clearing a refusal recorded under a previous image");
        }
    }

    let entries: Vec<(String, Phase, Option<String>, Option<String>)> = state
        .extensions
        .iter()
        .map(|(name, entry)| {
            (
                name.clone(),
                entry.phase,
                entry.candidate_version.clone(),
                entry.known_good_version.clone(),
            )
        })
        .collect();
    let kept_candidates: BTreeSet<_> = entries
        .iter()
        .filter(|(_, phase, _, _)| *phase == Phase::Staged)
        .filter_map(|(name, _, candidate, _)| {
            candidate
                .as_deref()
                .map(|version| system::candidate_path(name, version))
        })
        .collect();
    let mut failures = Vec::new();

    for (name, phase, candidate, known_good) in entries {
        match phase {
            Phase::Staged => {
                let present = candidate
                    .as_deref()
                    .is_some_and(|version| system::candidate_path(&name, version).exists());
                if !present {
                    let active = system::active_path(&name).exists();
                    let entry = state.entry(&name);
                    entry.candidate_version = None;
                    entry.phase = if active { Phase::Active } else { Phase::Idle };
                    if !active && !entry.required {
                        entry.active_version = None;
                        entry.known_good_version = None;
                    }
                    entry.last_failure =
                        Some("staged candidate disappeared before activation".into());
                }
            }
            Phase::Activating | Phase::Starting | Phase::Soaking | Phase::Reverting => {
                if let Err(error) = recover(
                    &mut state,
                    &name,
                    &format!("interrupted while in phase {phase:?}"),
                ) {
                    failures.push(format!("{name}: {error}"));
                }
            }
            Phase::Removing => {
                if let Err(error) = complete_removal(&mut state, &name) {
                    failures.push(format!("{name}: could not finish removal: {error}"));
                }
            }
            Phase::Active | Phase::Recovered => {
                if !system::active_path(&name).exists() {
                    let required = state.get(&name).is_some_and(|entry| entry.required);
                    if required {
                        if let Err(error) = recover(
                            &mut state,
                            &name,
                            "active image disappeared between operations",
                        ) {
                            failures.push(format!("{name}: {error}"));
                        }
                    } else {
                        let entry = state.entry(&name);
                        entry.phase = Phase::Idle;
                        entry.active_version = None;
                        entry.known_good_version = None;
                        entry.candidate_version = None;
                    }
                }
                if let Err(error) = system::prune_candidates(&name, None) {
                    failures.push(format!("{name}: could not prune candidates: {error}"));
                }
            }
            Phase::Idle => {
                let absent_and_optional = !system::active_path(&name).exists()
                    && state.get(&name).is_some_and(|entry| !entry.required);
                if absent_and_optional {
                    let entry = state.entry(&name);
                    entry.active_version = None;
                    entry.known_good_version = None;
                    entry.candidate_version = None;
                }
                if let Err(error) = system::prune_candidates(&name, None) {
                    failures.push(format!("{name}: could not prune candidates: {error}"));
                }
            }
            Phase::Unrecoverable => {
                if let Err(error) = system::prune_candidates(&name, None) {
                    failures.push(format!("{name}: could not prune candidates: {error}"));
                }
            }
        }
        if let Err(error) = system::prune_rollbacks(&name, known_good.as_deref()) {
            failures.push(format!("{name}: could not prune rollbacks: {error}"));
        }
    }

    match system::candidate_files() {
        Ok(candidates) => {
            for candidate in candidates {
                if !kept_candidates.contains(&candidate)
                    && let Err(error) = system::durable_remove(&candidate)
                {
                    failures.push(format!(
                        "could not remove orphan candidate {}: {error}",
                        candidate.display()
                    ));
                }
            }
        }
        Err(error) => failures.push(format!("could not enumerate candidates: {error}")),
    }

    state.store().map_err(|error| error.to_string())?;
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// Read `--name value` style options without pulling in an argument parser.
///
/// This binary is deliberately dependency-light, and these are three flags on
/// a handful of subcommands used almost entirely by other programs.
fn option(arguments: &[String], name: &str) -> Option<String> {
    arguments
        .iter()
        .position(|argument| argument == name)
        .and_then(|index| arguments.get(index + 1))
        .cloned()
}

/// Place an extension image, for whoever is not sandboxed enough to need the
/// socket. `carbideos-ops` reaches this, so an operator over SSH and the fleet
/// over the socket run identical code.
fn stage_extension(arguments: &[String]) -> Result<(), String> {
    let name = arguments
        .first()
        .filter(|name| !name.starts_with("--"))
        .ok_or("usage: carbide-agent stage-extension NAME [options]")?;
    let base = option(arguments, "--base");

    let source = match option(arguments, "--path") {
        Some(path) => ops::Source::Supplied {
            path: std::path::PathBuf::from(path),
            digest: option(arguments, "--digest")
                .ok_or("a supplied image must declare --digest")?,
        },
        None => ops::Source::Acquire {
            version: option(arguments, "--version"),
        },
    };

    let activate = arguments.iter().any(|argument| argument == "--activate");
    let staged = ops::stage_extension(name, base.as_deref(), source).map_err(|f| describe(&f))?;

    // An image for a base this node has not booted merges when that base
    // boots; there is nothing to activate now, and restarting a healthy
    // service for it would interrupt it for no reason.
    if !activate || !staged.for_running_base {
        println!(
            "{}: staged for CarbideOS {} at {}",
            staged.name, staged.version, staged.path
        );
        return Ok(());
    }

    let Some(version) = staged.extension_version.clone() else {
        return Err(format!("{name} declares no usable extension version"));
    };

    let request_id = std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|id| id.trim().to_string())
        .ok();
    match activate_extension(name, &version, request_id) {
        Ok(_) => {
            println!("{name}: active on {version}");
            Ok(())
        }
        Err(failure) => Err(describe(&failure)),
    }
}

fn stage_base(arguments: &[String]) -> Result<(), String> {
    let requested = arguments.first().filter(|value| !value.starts_with("--"));
    match ops::stage_base(requested.map(String::as_str)) {
        Ok(staged) if staged.pending => {
            println!(
                "staged CarbideOS {} for the next boot ({})",
                staged.version.unwrap_or_default(),
                staged.boot_entry.unwrap_or_default()
            );
            Ok(())
        }
        Ok(_) => {
            println!("no CarbideOS update to stage");
            Ok(())
        }
        Err(failure) => Err(describe(&failure)),
    }
}

/// Flatten a structured failure for a human at a terminal.
///
/// The socket keeps the fields; this is the one place they are allowed to
/// become a sentence, and even here the exit code and stderr are kept because
/// they are what an operator acts on.
fn describe(failure: &Failure) -> String {
    let mut message = failure.message.clone();
    if let Some(command) = &failure.command {
        message.push_str(&format!("\n  command: {command}"));
    }
    if let Some(code) = failure.exit_code {
        message.push_str(&format!("\n  exit: {code}"));
    }
    if let Some(stderr) = &failure.stderr {
        message.push_str(&format!("\n  stderr: {}", stderr.trim()));
    }
    message
}

/// What an activation settled on, for a caller that needs more than "it worked".
pub struct Activation {
    pub version: String,
    pub known_good: Option<String>,
    /// True when the request was a replay of one already applied.
    pub replayed: bool,
}

fn activate(arguments: &[String]) -> Result<(), String> {
    let name = arguments
        .first()
        .ok_or("usage: carbide-agent activate NAME VERSION")?;
    let version = arguments
        .get(1)
        .ok_or("usage: carbide-agent activate NAME VERSION")?;
    let request_id = arguments.get(2).cloned();

    match activate_extension(name, version, request_id) {
        Ok(activation) if activation.replayed => {
            println!("{name}: request already applied");
            Ok(())
        }
        Ok(activation) => {
            println!("{name}: active on {}", activation.version);
            Ok(())
        }
        Err(failure) => Err(failure.message),
    }
}

/// Promote a staged candidate, reverting if it does not prove itself.
///
/// Shared by the CLI and the socket. Failures are structured rather than
/// formatted, because the socket has to hand a caller something it can branch
/// on, and flattening to a sentence here would make that impossible.
pub fn activate_extension(
    name: &str,
    version: &str,
    request_id: Option<String>,
) -> Result<Activation, Failure> {
    let mut state = State::load().map_err(|error| {
        Failure::new(Code::Failed, format!("could not read agent state: {error}"))
    })?;

    if let (Some(id), Some(entry)) = (request_id.as_ref(), state.get(name))
        && entry.already_completed(id)
    {
        return Ok(Activation {
            version: version.to_string(),
            known_good: entry.known_good_version.clone(),
            replayed: true,
        });
    }

    let candidate = system::candidate_path(name, version);
    if !candidate.exists() {
        return Err(Failure::new(
            Code::NotFound,
            format!("no staged candidate at {}", candidate.display()),
        )
        .at(Stage::Checking));
    }

    // The socket path must have the same migration safety as the CLI path. If
    // an already-merged image predates agent state, adopt it before replacing
    // it so candidate failure still has a local known-good image.
    let unrecorded = state
        .get(name)
        .and_then(|entry| entry.known_good_version.as_ref())
        .is_none();
    if unrecorded
        && system::active_path(name).exists()
        && let Some(current) = ops::merged_version(name)
    {
        adopt_active(&mut state, name, &current).map_err(|error| {
            Failure::new(
                Code::Failed,
                format!("could not adopt the active image: {error}"),
            )
            .at(Stage::Checking)
        })?;
    }

    let candidate_size = std::fs::metadata(&candidate)
        .map_err(|error| Failure::new(Code::Failed, error.to_string()).at(Stage::Checking))?
        .len();
    let previous = state.get(name).and_then(|e| e.active_version.clone());
    let rollback_size = match previous.as_deref() {
        Some(previous_version) if !system::rollback_path(name, previous_version).exists() => {
            std::fs::metadata(system::active_path(name))
                .map_err(|error| Failure::new(Code::Failed, error.to_string()).at(Stage::Checking))?
                .len()
        }
        _ => 0,
    };
    let required = system::activation_space(candidate_size, rollback_size);
    let available = system::available_bytes()
        .map_err(|error| Failure::new(Code::Failed, error.to_string()).at(Stage::Checking))?;
    if available < required {
        return Err(Failure::new(
            Code::NoSpace,
            format!(
                "insufficient space: transaction needs {required} bytes, {available} available"
            ),
        )
        .at(Stage::Checking));
    }

    // Retain the outgoing image before overwriting it, so there is always
    // something to fall back to even if the machine dies mid-activation.
    if let Some(previous_version) = &previous {
        let active = system::active_path(name);
        let retained = system::rollback_path(name, previous_version);
        if active.exists() && !retained.exists() {
            system::durable_copy(&active, &retained).map_err(|error| {
                Failure::new(
                    Code::Failed,
                    format!("could not retain the outgoing image: {error}"),
                )
                .at(Stage::Staging)
            })?;
        }
    }

    {
        let entry = state.entry(name);
        entry.candidate_version = Some(version.to_string());
        entry.request_id = request_id.clone();
        entry.phase = Phase::Activating;
        if let Some(previous_version) = &previous {
            entry.known_good_version = Some(previous_version.clone());
        }
    }
    state
        .store()
        .map_err(|error| Failure::new(Code::Failed, error.to_string()).at(Stage::Staging))?;

    let mut unit = None;
    let outcome: Result<(), (String, Stage)> = (|| {
        if let Ok(current) = Ruleset::load_named(name) {
            system::stop_unit(&current.unit)
                .map_err(|error| (error.to_string(), Stage::Activating))?;
        }
        system::install_active(&candidate, name).map_err(|error| {
            (
                format!("could not install the image: {error}"),
                Stage::Activating,
            )
        })?;
        system::sysext_refresh().map_err(|error| {
            (
                format!("could not merge the image: {error}"),
                Stage::Activating,
            )
        })?;
        system::daemon_reload().map_err(|error| {
            (
                format!("could not reload systemd: {error}"),
                Stage::Activating,
            )
        })?;

        // The ruleset arrives with the image and is deliberately loaded only
        // after the candidate has merged.
        let ruleset = Ruleset::load_named(name).map_err(|error| {
            (
                format!("candidate supplied no usable ruleset: {error}"),
                Stage::Activating,
            )
        })?;
        unit = Some(ruleset.unit.clone());
        state.entry(name).phase = Phase::Starting;
        state.store().map_err(|error| {
            (
                format!("could not record starting phase: {error}"),
                Stage::Activating,
            )
        })?;
        system::wait_until_ready(&ruleset).map_err(|reason| (reason, Stage::Activating))?;
        state.entry(name).phase = Phase::Soaking;
        state.store().map_err(|error| {
            (
                format!("could not record soaking phase: {error}"),
                Stage::Soaking,
            )
        })?;
        system::soak(&ruleset).map_err(|reason| (reason, Stage::Soaking))?;

        let mut committed = state.clone();
        {
            let entry = committed.entry(name);
            entry.phase = Phase::Active;
            entry.terminal_os_version = None;
            entry.active_version = Some(version.to_string());
            entry.candidate_version = None;
            entry.last_health = Some("healthy".into());
            entry.last_failure = None;
            entry.required = true;
            if let Some(id) = &request_id {
                entry.record_completed(id);
            }
        }
        committed.store().map_err(|error| {
            (
                format!("could not commit active state: {error}"),
                Stage::Activating,
            )
        })?;
        state = committed;
        Ok(())
    })();

    if let Err((reason, stage)) = outcome {
        // Collected before reverting, because reverting restarts the unit and
        // the lines that explain the failure scroll away behind the recovery.
        let log = unit
            .as_deref()
            .and_then(|unit| system::journal(Some(unit), 40, None, None).ok())
            .map(|(lines, _)| lines)
            .unwrap_or_default();
        let reverted = recover(&mut state, name, &reason);
        let mut failure = Failure::new(
            Code::Failed,
            match &reverted {
                Ok(()) => format!("{reason}; reverted"),
                Err(error) => error.clone(),
            },
        )
        .at(if reverted.is_ok() {
            Stage::Reverting
        } else {
            stage
        })
        .log(log);
        if let Some(unit) = unit {
            failure = failure.unit(unit);
        }
        return Err(failure);
    }

    // The candidate has proven itself, so it becomes the image to fall back to
    // next time and the staged copy is no longer needed.
    if let Err(error) = system::prune_candidates(name, None) {
        eprintln!("{name}: active, but candidate cleanup failed: {error}");
    }
    if let Err(error) = system::prune_rollbacks(name, previous.as_deref()) {
        eprintln!("{name}: active, but rollback cleanup failed: {error}");
    }
    Ok(Activation {
        version: version.to_string(),
        known_good: previous,
        replayed: false,
    })
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

fn remove_extension(arguments: &[String]) -> Result<(), String> {
    let name = arguments
        .first()
        .ok_or("usage: carbide-agent remove NAME")?;
    let mut state = State::load().map_err(|error| error.to_string())?;
    {
        let entry = state.entry(name);
        entry.required = false;
        entry.phase = Phase::Removing;
    }
    state.store().map_err(|error| error.to_string())?;
    complete_removal(&mut state, name)?;
    state.store().map_err(|error| error.to_string())?;
    println!("{name}: removed from the running base");
    Ok(())
}

fn complete_removal(state: &mut State, name: &str) -> Result<(), String> {
    if let Ok(ruleset) = Ruleset::load_named(name) {
        let _ = system::stop_unit(&ruleset.unit);
    }
    system::remove_active(name).map_err(|error| error.to_string())?;
    system::sysext_refresh().map_err(|error| error.to_string())?;
    system::daemon_reload().map_err(|error| error.to_string())?;
    system::prune_candidates(name, None).map_err(|error| error.to_string())?;
    system::prune_rollbacks(name, None).map_err(|error| error.to_string())?;
    let entry = state.entry(name);
    entry.phase = Phase::Idle;
    entry.active_version = None;
    entry.known_good_version = None;
    entry.candidate_version = None;
    entry.last_health = None;
    entry.last_failure = None;
    Ok(())
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
