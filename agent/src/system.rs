// SPDX-License-Identifier: AGPL-3.0-or-later
//! Image slots and the systemd interactions the agent needs.
//!
//! systemd is driven through its command line tools rather than D-Bus. This
//! component is meant to stay small and near-frozen, and a bus client is a
//! large dependency to carry in the one thing that has to keep working when
//! everything else is broken.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::config::Ruleset;

pub const EXTENSIONS_DIR: &str = "/var/lib/extensions";

/// Must match the policy in the systemd-sysext drop-in, or a manual refresh
/// would apply weaker rules than boot does.
const IMAGE_POLICY: &str = "root=verity+signed+absent:usr=verity+signed+absent";

/// Slot naming.
///
/// `systemd-sysext` merges every `*.raw` in the extensions directory, so only
/// an image meant to merge may carry that suffix. A retained image whose name
/// ended in `.raw` would be merged at the same time as the active one.
///
/// The active image is additionally scoped to the base version it was built
/// for. This directory lives on the state partition and so is shared by both
/// base slots, while `/usr` is replaced wholesale by an update. With a single
/// unscoped file there is one image for two base versions, and no ordering
/// works: replacing it breaks the running system, and leaving it means the new
/// base boots to an extension it must refuse. Scoping lets both sit here at
/// once, lets systemd merge whichever matches, and leaves a base rollback with
/// its own extension already in place.
fn scope() -> String {
    crate::state::os_version().unwrap_or_else(|| "unknown".to_string())
}

fn legacy_active_path(name: &str) -> PathBuf {
    Path::new(EXTENSIONS_DIR).join(format!("{name}.raw"))
}

/// An underscore, because systemd derives an extension's name from its file
/// name and strips a version only after that separator. With any other one it
/// looks for an extension-release file named after the whole string, fails to
/// find it, and reports the image unreadable rather than merely incompatible,
/// which fails the entire merge and takes every other extension down with it.
pub fn scoped_active_path(name: &str, os_version: &str) -> PathBuf {
    Path::new(EXTENSIONS_DIR).join(format!("{name}_{os_version}.raw"))
}

pub fn active_path(name: &str) -> PathBuf {
    let scoped = scoped_active_path(name, &scope());
    if scoped.exists() {
        return scoped;
    }
    // A node provisioned before scoping carries an unscoped image, which is by
    // definition the one merged into the running base. Keep reading it until
    // something installs a scoped one, so an upgrade cannot strand a node.
    let legacy = legacy_active_path(name);
    if legacy.exists() {
        return legacy;
    }
    scoped
}

pub fn rollback_path(name: &str, version: &str) -> PathBuf {
    Path::new(EXTENSIONS_DIR).join(format!("{name}_{}.raw.{version}.rollback", scope()))
}

pub fn candidate_path(name: &str, version: &str) -> PathBuf {
    Path::new(EXTENSIONS_DIR).join(format!("{name}_{}.raw.{version}.candidate", scope()))
}

/// Bytes available on the filesystem backing the extensions directory.
///
/// Filling the state partition part way through an update wedges a node in a
/// way no amount of rollback logic recovers from, so staging is refused unless
/// the candidate demonstrably fits.
pub fn available_bytes() -> io::Result<u64> {
    let output = Command::new("/usr/bin/df")
        .args(["--output=avail", "-B1", EXTENSIONS_DIR])
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("df failed"));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .nth(1)
        .and_then(|line| line.trim().parse::<u64>().ok())
        .ok_or_else(|| io::Error::other("could not read available space"))
}

fn run(program: &str, args: &[&str]) -> io::Result<std::process::Output> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
}

const SYSUPDATE: &str = "/usr/lib/systemd/systemd-sysupdate";

/// The base version the update feed offers, if it is newer than this one.
///
/// Runs here rather than in a caller because sysupdate has to resolve $BOOT,
/// which needs the block devices a sandboxed service does not have.
pub fn available_base_version() -> Result<Option<String>, String> {
    let output = run(SYSUPDATE, &["check-new"])
        .map_err(|error| format!("could not run systemd-sysupdate: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "systemd-sysupdate check-new failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!version.is_empty()).then_some(version))
}

/// Whether an update has been downloaded and is waiting for a reboot.
pub fn update_pending() -> bool {
    run(SYSUPDATE, &["pending"])
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Re-merge extensions. The drop-in sets EXTENSION_RELOAD_MANAGER, so units
/// arriving from an extension become visible without a separate reload.
pub fn sysext_refresh() -> io::Result<()> {
    let output = run(
        "/usr/bin/systemd-sysext",
        &[&format!("--image-policy={IMAGE_POLICY}"), "refresh"],
    )?;
    if output.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "systemd-sysext refresh failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

pub fn daemon_reload() -> io::Result<()> {
    let output = run("/usr/bin/systemctl", &["daemon-reload"])?;
    if output.status.success() {
        return Ok(());
    }
    Err(io::Error::other("daemon-reload failed"))
}

/// The extension name behind an image name, with any base version scope removed.
///
/// Mirrors how systemd derives a name from an image file: everything from the
/// first underscore is the version.
fn extension_name(image: &str) -> String {
    match image.split_once('_') {
        Some((name, _)) => name.to_string(),
        None => image.to_string(),
    }
}

pub fn merged_extensions() -> io::Result<Vec<String>> {
    let output = run("/usr/bin/systemd-sysext", &["status", "--json=short"])?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let parsed: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(value) => value,
        Err(_) => return Ok(Vec::new()),
    };
    let mut names = Vec::new();
    if let Some(entries) = parsed.as_array() {
        for entry in entries {
            if let Some(extensions) = entry.get("extensions").and_then(|e| e.as_array()) {
                for extension in extensions {
                    // Reported under the image's file name, which carries the
                    // base version the image is scoped to. Callers ask about
                    // the extension, so the scope is stripped back off here;
                    // otherwise a scoped image looks like an extension nobody
                    // required and the one that was required looks missing.
                    if let Some(name) = extension.as_str().map(extension_name)
                        && !names.iter().any(|n| n == &name)
                    {
                        names.push(name);
                    }
                }
            }
        }
    }
    Ok(names)
}

pub fn unit_is_active(unit: &str) -> bool {
    run("/usr/bin/systemctl", &["is-active", "--quiet", unit])
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub fn unit_failed(unit: &str) -> bool {
    run("/usr/bin/systemctl", &["is-failed", "--quiet", unit])
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub fn start_unit(unit: &str) -> io::Result<()> {
    let _ = run("/usr/bin/systemctl", &["reset-failed", unit]);
    // Do not block on the job. Readiness is judged by the ruleset's own
    // deadline, not by how long systemctl is willing to wait.
    let output = run("/usr/bin/systemctl", &["--no-block", "start", unit])?;
    if output.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "failed to start {unit}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

pub fn stop_unit(unit: &str) -> io::Result<()> {
    let _ = run("/usr/bin/systemctl", &["stop", unit]);
    Ok(())
}

/// Wait for a unit to report active, restarting it up to the permitted number
/// of attempts. A `Type=notify` unit only reports active once it has signalled
/// readiness, so this is the first real gate.
pub fn wait_until_ready(ruleset: &Ruleset) -> Result<(), String> {
    for attempt in 1..=ruleset.start_attempts {
        if let Err(error) = start_unit(&ruleset.unit) {
            return Err(error.to_string());
        }

        let deadline = Instant::now() + ruleset.ready_timeout;
        while Instant::now() < deadline {
            if unit_is_active(&ruleset.unit) {
                return Ok(());
            }
            if unit_failed(&ruleset.unit) {
                break;
            }
            sleep(Duration::from_millis(500));
        }

        if unit_is_active(&ruleset.unit) {
            return Ok(());
        }
        if attempt < ruleset.start_attempts {
            let _ = stop_unit(&ruleset.unit);
        }
    }

    Err(format!(
        "{} did not become ready within {}s across {} attempts",
        ruleset.unit,
        ruleset.ready_timeout.as_secs(),
        ruleset.start_attempts
    ))
}

/// Run the ruleset's deeper probe once, if it defines one.
///
/// Readiness alone cannot distinguish a working service from one that is
/// running but useless, which is the failure mode this exists to catch.
pub fn probe(ruleset: &Ruleset) -> Result<(), String> {
    let Some(command) = &ruleset.health_command else {
        return Ok(());
    };

    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("health command failed to start: {error}"))?;

    let deadline = Instant::now() + ruleset.health_timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("health command exited with {status}")),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "health command exceeded {}s",
                        ruleset.health_timeout.as_secs()
                    ));
                }
                sleep(Duration::from_millis(200));
            }
            Err(error) => return Err(format!("health command failed: {error}")),
        }
    }
}

/// Require the extension to stay healthy for the soak period before it is
/// trusted. Without this, a service that dies after thirty seconds would be
/// recorded as the version to fall back to.
pub fn soak(ruleset: &Ruleset) -> Result<(), String> {
    if ruleset.soak.is_zero() {
        return probe(ruleset);
    }

    let deadline = Instant::now() + ruleset.soak;
    while Instant::now() < deadline {
        if !unit_is_active(&ruleset.unit) {
            return Err(format!("{} stopped during soak", ruleset.unit));
        }
        probe(ruleset)?;

        let remaining = deadline.saturating_duration_since(Instant::now());
        sleep(ruleset.health_interval.min(remaining));
    }

    if !unit_is_active(&ruleset.unit) {
        return Err(format!("{} stopped during soak", ruleset.unit));
    }
    probe(ruleset)
}

/// Replace the active image, flushing the directory so the swap is durable
/// before anything is merged from it.
pub fn install_active(source: &Path, name: &str) -> io::Result<()> {
    // Always writes the scoped name, even when a legacy image is still what
    // active_path reads, so that activating anything migrates the node.
    let target = scoped_active_path(name, &scope());
    fs::copy(source, &target)?;
    fs::set_permissions(&target, fs::metadata(source)?.permissions())?;
    // Both would otherwise merge at once, giving two copies of one extension.
    let legacy = legacy_active_path(name);
    if legacy != target && legacy.exists() {
        fs::remove_file(&legacy)?;
    }
    std::fs::File::open(EXTENSIONS_DIR)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::extension_name;

    /// systemd reports a merged extension under its image file name. A scoped
    /// image therefore arrives as `watchtower_0.1.50`, and comparing that to
    /// the required name made a merged extension look missing, which failed
    /// the gate and rolled a perfectly good image back.
    #[test]
    fn a_scoped_image_reports_the_extension_it_contains() {
        assert_eq!(extension_name("watchtower_0.1.50"), "watchtower");
        assert_eq!(extension_name("chrome_0.1.50"), "chrome");
    }

    #[test]
    fn an_unscoped_image_is_left_alone() {
        assert_eq!(extension_name("watchtower"), "watchtower");
        assert_eq!(extension_name("rat-game-16"), "rat-game-16");
    }
}
