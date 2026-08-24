// SPDX-License-Identifier: AGPL-3.0-or-later
//! Image slots and the systemd interactions the agent needs.
//!
//! systemd is driven through its command line tools rather than D-Bus. This
//! component is meant to stay small and near-frozen, and a bus client is a
//! large dependency to carry in the one thing that has to keep working when
//! everything else is broken.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::config::Ruleset;

pub const EXTENSIONS_DIR: &str = "/var/lib/extensions";
pub const STAGING_DIR: &str = "/var/lib/extensions/.carbide-staging";
const SPACE_MARGIN: u64 = 16 * 1024 * 1024;

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

pub fn remove_active(name: &str) -> io::Result<()> {
    durable_remove(&scoped_active_path(name, &scope()))?;
    durable_remove(&legacy_active_path(name))
}

pub fn rollback_path(name: &str, version: &str) -> PathBuf {
    Path::new(EXTENSIONS_DIR).join(format!("{name}_{}.raw.{version}.rollback", scope()))
}

pub fn candidate_path(name: &str, version: &str) -> PathBuf {
    Path::new(EXTENSIONS_DIR).join(format!("{name}_{}.raw.{version}.candidate", scope()))
}

fn temporary_path(target: &Path) -> io::Result<PathBuf> {
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("target has no parent directory"))?;
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::other("target has no file name"))?
        .to_string_lossy();
    Ok(parent.join(format!(".{name}.new")))
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent directory"))?;
    File::open(parent)?.sync_all()
}

/// Publish a complete file atomically and durably.
///
/// The temporary name deliberately does not end in `.raw`, so systemd-sysext
/// can never merge bytes that have not reached disk yet.
pub fn durable_copy(source: &Path, target: &Path) -> io::Result<()> {
    let mut input = File::open(source)?;
    let size = input.metadata()?.len();
    durable_copy_file(&mut input, target, size)
}

/// Publish from one already-open inode, refusing growth beyond the bytes the
/// caller budgeted before copying began.
pub fn durable_copy_file(input: &mut File, target: &Path, maximum: u64) -> io::Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("target has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let temporary = temporary_path(target)?;
    let _ = fs::remove_file(&temporary);
    let permissions = input.metadata()?.permissions();

    let result = (|| {
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        let copied = io::copy(&mut input.take(maximum.saturating_add(1)), &mut output)?;
        if copied > maximum {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "source grew beyond its reserved size",
            ));
        }
        output.set_permissions(permissions)?;
        output.sync_all()?;
        fs::rename(&temporary, target)?;
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn durable_remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn durable_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    File::open(path)?.sync_all()?;
    sync_parent(path)
}

/// Move a file without ever copying into its final visible name.
pub fn durable_move(source: &Path, target: &Path) -> io::Result<()> {
    durable_copy(source, target)?;
    durable_remove(source)
}

/// Additional free bytes required after a candidate already exists.
pub fn activation_space(candidate_size: u64, missing_rollback_size: u64) -> u64 {
    candidate_size
        .saturating_add(missing_rollback_size)
        .saturating_add(SPACE_MARGIN)
}

/// Free bytes required before a supplied image is copied into local staging.
pub fn staging_space(image_size: u64, missing_rollback_size: u64) -> u64 {
    image_size
        .saturating_mul(2)
        .saturating_add(missing_rollback_size)
        .saturating_add(SPACE_MARGIN)
}

fn prune_matching_in(
    directory: &Path,
    prefix: &str,
    suffix: &str,
    keep: Option<&Path>,
) -> io::Result<()> {
    let mut changed = false;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name.starts_with(prefix)
            && file_name.ends_with(suffix)
            && keep.is_none_or(|kept| kept != path)
        {
            fs::remove_file(path)?;
            changed = true;
        }
    }
    if changed {
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

pub fn prune_rollbacks(name: &str, keep_version: Option<&str>) -> io::Result<()> {
    let keep = keep_version.map(|version| rollback_path(name, version));
    let prefix = format!("{name}_{}.raw.", scope());
    prune_matching_in(
        Path::new(EXTENSIONS_DIR),
        &prefix,
        ".rollback",
        keep.as_deref(),
    )
}

pub fn prune_candidates(name: &str, keep_version: Option<&str>) -> io::Result<()> {
    let keep = keep_version.map(|version| candidate_path(name, version));
    let prefix = format!("{name}_{}.raw.", scope());
    prune_matching_in(
        Path::new(EXTENSIONS_DIR),
        &prefix,
        ".candidate",
        keep.as_deref(),
    )
}

/// Candidate files for the running base, including ones published immediately
/// before a power loss prevented their state record from being committed.
pub fn candidate_files() -> io::Result<Vec<PathBuf>> {
    let marker = format!("_{}.raw.", scope());
    let mut candidates = Vec::new();
    for entry in fs::read_dir(EXTENSIONS_DIR)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name.contains(&marker) && file_name.ends_with(".candidate") {
            candidates.push(entry.path());
        }
    }
    candidates.sort();
    Ok(candidates)
}

fn prune_transaction_files_in(extensions: &Path, staging: &Path) -> io::Result<()> {
    for directory in [extensions, staging] {
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let mut changed = false;
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if directory == staging || (file_name.starts_with('.') && file_name.ends_with(".new")) {
                fs::remove_file(entry.path())?;
                changed = true;
            }
        }
        if changed {
            File::open(directory)?.sync_all()?;
        }
    }
    Ok(())
}

pub fn prune_transaction_files() -> io::Result<()> {
    prune_transaction_files_in(Path::new(EXTENSIONS_DIR), Path::new(STAGING_DIR))
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

fn run_mutating(program: &str, args: &[&str]) -> io::Result<std::process::Output> {
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null());
    crate::state::retain_operation_lock(&mut command);
    command.output()
}

/// A finished command, kept whole.
///
/// Every privileged step here is a subprocess, and the difference between a
/// diagnosable failure and "see journalctl" is whether the exit code and
/// stderr survived. They are carried rather than collapsed to a bool so a
/// reply can say what ran and what it said.
pub struct Finished {
    pub command: String,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub ok: bool,
}

pub fn capture(program: &str, args: &[&str]) -> io::Result<Finished> {
    let output = run(program, args)?;
    finished(program, args, output)
}

fn capture_mutating(program: &str, args: &[&str]) -> io::Result<Finished> {
    let output = run_mutating(program, args)?;
    finished(program, args, output)
}

fn finished(program: &str, args: &[&str], output: std::process::Output) -> io::Result<Finished> {
    let mut command = String::from(program);
    for argument in args {
        command.push(' ');
        command.push_str(argument);
    }
    Ok(Finished {
        command,
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        ok: output.status.success(),
    })
}

const SYSUPDATE: &str = "/usr/lib/systemd/systemd-sysupdate";
const DISSECT: &str = "/usr/bin/systemd-dissect";
const JOURNALCTL: &str = "/usr/bin/journalctl";

/// Only a plain version may be interpolated into a path.
pub fn version_valid(version: &str) -> bool {
    !version.is_empty()
        && version
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
        && version.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
}

/// Verity and signature validation, under the same policy the merge will use.
///
/// An image is not trusted because it arrived on the socket. It is trusted
/// because the kernel will accept it, and that is decided here.
pub fn validate_image(path: &Path) -> io::Result<Finished> {
    capture(
        DISSECT,
        &[
            "--validate",
            &format!("--image-policy={IMAGE_POLICY}"),
            &path.to_string_lossy(),
        ],
    )
}

/// The base version and extension version an image declares.
///
/// `systemd-dissect` reports `sysextRelease` as an array of `KEY=VALUE`
/// strings. Parsed rather than grepped, because `VERSION_ID` is a suffix of
/// `SYSEXT_VERSION_ID` and an unanchored match reads the extension's own
/// version as the base it belongs to, pinning the image to a base that does
/// not exist so nothing ever merges it.
#[derive(Debug, PartialEq, Eq)]
pub struct ImageRelease {
    pub name: Option<String>,
    pub base: Option<String>,
    pub version: Option<String>,
}

pub fn image_release(path: &Path) -> io::Result<ImageRelease> {
    let finished = capture(DISSECT, &["--json=short", &path.to_string_lossy()])?;
    if !finished.ok {
        return Ok(ImageRelease {
            name: None,
            base: None,
            version: None,
        });
    }
    Ok(parse_sysext_release(&finished.stdout))
}

fn parse_sysext_release(json: &str) -> ImageRelease {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json) else {
        return ImageRelease {
            name: None,
            base: None,
            version: None,
        };
    };
    let Some(entries) = parsed.get("sysextRelease").and_then(|v| v.as_array()) else {
        return ImageRelease {
            name: None,
            base: None,
            version: None,
        };
    };
    let mut name = None;
    let mut base = None;
    let mut extension = None;
    for entry in entries.iter().filter_map(|e| e.as_str()) {
        // strip_prefix, not a substring search: SYSEXT_VERSION_ID ends in the
        // same text as VERSION_ID, so a loose match reads the extension's own
        // version as the base it belongs to. That pins the image to a base
        // that does not exist and nothing ever merges it.
        if let Some(value) = entry.strip_prefix("VERSION_ID=") {
            base.get_or_insert_with(|| value.trim_matches('"').to_string());
        } else if let Some(value) = entry.strip_prefix("SYSEXT_VERSION_ID=") {
            extension.get_or_insert_with(|| value.trim_matches('"').to_string());
        } else if let Some(value) = entry.strip_prefix("SYSEXT_ID=") {
            name.get_or_insert_with(|| value.trim_matches('"').to_string());
        }
    }
    ImageRelease {
        name,
        base,
        version: extension,
    }
}

/// Shelled rather than pulled in as a crate. A digest implementation is a
/// supply-chain input to the recovery path, and coreutils is already here.
pub fn sha256(path: &Path) -> io::Result<String> {
    let finished = capture("/usr/bin/sha256sum", &[&path.to_string_lossy()])?;
    if !finished.ok {
        return Err(io::Error::other("sha256sum failed"));
    }
    finished
        .stdout
        .split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| io::Error::other("could not read digest"))
}

/// Extension names this agent can acquire on its own, from the sysupdate
/// components configured in the sealed image.
pub fn components() -> Vec<String> {
    let Ok(finished) = capture(SYSUPDATE, &["components"]) else {
        return Vec::new();
    };
    if !finished.ok {
        return Vec::new();
    }
    // Reported as a heading followed by one name per line, with the base
    // (unnamed) component listed separately. Anything that is not a plausible
    // name is skipped rather than guessed at.
    finished
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            line.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        })
        .filter(|line| !line.eq_ignore_ascii_case("components"))
        .map(str::to_string)
        .collect()
}

/// Fetch an extension through its own sysupdate component.
///
/// This is where "the agent owns what is a property of the OS" is actually
/// implemented: the feed URL lives in the component's transfer definition,
/// sealed in the base, so a caller never has to know it.
pub fn acquire_component(name: &str, version: Option<&str>) -> io::Result<Finished> {
    let component = format!("--component={name}");
    let mut args = vec![component.as_str(), "update"];
    if let Some(version) = version {
        args.push(version);
    }
    capture_mutating(SYSUPDATE, &args)
}

/// Download and install a base image into the spare slot.
pub fn stage_base(version: Option<&str>) -> io::Result<Finished> {
    let mut args = vec!["update"];
    if let Some(version) = version {
        args.push(version);
    }
    capture_mutating(SYSUPDATE, &args)
}

/// The extension images present for a given base version.
pub fn installed_extensions_for(base: &str) -> Vec<String> {
    let suffix = format!("_{base}.raw");
    let mut names = Vec::new();
    let Ok(entries) = fs::read_dir(EXTENSIONS_DIR) else {
        return names;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if let Some(name) = file_name.strip_suffix(&suffix)
            && !name.is_empty()
            && !names.iter().any(|existing| existing == name)
        {
            names.push(name.to_string());
        }
    }
    names.sort();
    names
}

/// The base version an installed but not yet booted image would boot into.
pub fn pending_base_version() -> Result<Option<String>, String> {
    let image_id = fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix("IMAGE_ID=")
                    .map(|v| v.trim_matches('"').to_string())
            })
        })
        .ok_or_else(|| "could not read IMAGE_ID from /etc/os-release".to_string())?;
    let prefix = format!("{image_id}_");
    let mut found = None;
    let entries = fs::read_dir("/boot/EFI/Linux")
        .map_err(|error| format!("could not read pending UKIs: {error}"))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        // A counted entry looks like <id>_<version>+<tries>-<done>.efi; an
        // uncounted one is the image already blessed and is not pending.
        let Some((version, counter)) = rest.split_once('+') else {
            continue;
        };
        if !counter.ends_with(".efi") || !version_valid(version) {
            continue;
        }
        // More than one candidate means the state is ambiguous, and guessing
        // which base an extension must match is exactly how a node boots into
        // an image nothing supports.
        if found.is_some() {
            return Err("more than one counted base image is pending".to_string());
        }
        found = Some(version.to_string());
    }
    Ok(found)
}

pub fn set_oneshot(entry: &str) -> io::Result<Finished> {
    capture_mutating("/usr/bin/bootctl", &["set-oneshot", entry])
}

/// Flush the staged slot before anything points a boot entry at it.
///
/// Without this a crash between writing the image and selecting it leaves a
/// boot entry aimed at a partially written partition. Boot counting would
/// eventually roll that back, but spending a boot to discover it is worse than
/// waiting here.
pub fn sync() -> io::Result<Finished> {
    capture_mutating("/usr/bin/sync", &[])
}

/// The counted boot entry for a pending base version.
pub fn pending_boot_entry(version: &str) -> Option<String> {
    let image_id = fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix("IMAGE_ID=")
                    .map(|v| v.trim_matches('"').to_string())
            })
        })?;
    let prefix = format!("{image_id}_{version}+");
    fs::read_dir("/boot/EFI/Linux")
        .ok()?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with(&prefix) && name.ends_with(".efi"))
}

/// Read the journal.
///
/// The agent is unsandboxed and can; the callers that most need this cannot.
/// Paginated by cursor rather than streamed, so the framing stays one line of
/// JSON per reply.
pub fn journal(
    unit: Option<&str>,
    lines: u64,
    cursor: Option<&str>,
    priority: Option<&str>,
) -> io::Result<(Vec<String>, Option<String>)> {
    let mut args: Vec<String> = vec![
        "--no-pager".into(),
        "--output=short-iso".into(),
        "--show-cursor".into(),
    ];
    match cursor {
        // A cursor already fixes the start, and --lines would then be counted
        // from the end of the journal instead, silently skipping the range the
        // caller asked to resume from.
        Some(cursor) => {
            args.push(format!("--after-cursor={cursor}"));
            args.push(format!("--lines={lines}"));
        }
        None => args.push(format!("--lines={lines}")),
    }
    if let Some(unit) = unit {
        args.push(format!("--unit={unit}"));
    }
    if let Some(priority) = priority {
        args.push(format!("--priority={priority}"));
    }
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let finished = capture(JOURNALCTL, &borrowed)?;
    if !finished.ok {
        return Err(io::Error::other(finished.stderr.trim().to_string()));
    }

    let mut collected = Vec::new();
    let mut next = None;
    for line in finished.stdout.lines() {
        if let Some(value) = line.trim().strip_prefix("-- cursor:") {
            next = Some(value.trim().to_string());
            continue;
        }
        if line.starts_with("-- No entries") {
            continue;
        }
        collected.push(line.to_string());
    }
    Ok((collected, next))
}

/// The base version the update feed offers, if it is newer than this one.
///
/// Runs here rather than in a caller because sysupdate has to resolve $BOOT,
/// which needs the block devices a sandboxed service does not have.
pub fn available_base_version() -> Result<Option<String>, String> {
    let output = run(SYSUPDATE, &["check-new"])
        .map_err(|error| format!("could not run systemd-sysupdate: {error}"))?;
    if output.status.success() {
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok((!version.is_empty()).then_some(version));
    }

    // check-new exits non-zero both when there is nothing newer and when it
    // failed, and a node that is up to date is the common case. Enumerating
    // separates them: if that works, the feed was read and there is genuinely
    // nothing to install.
    //
    // Reporting the two alike told every healthy node its version check had
    // failed, and made an update request fail before it began, because
    // resolving the available version is the first thing it does.
    let listed = run(SYSUPDATE, &["list"])
        .map_err(|error| format!("could not run systemd-sysupdate: {error}"))?;
    if listed.status.success() {
        return Ok(None);
    }
    Err(format!(
        "systemd-sysupdate check-new failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
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
    let output = run_mutating(
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
    let output = run_mutating("/usr/bin/systemctl", &["daemon-reload"])?;
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
    let _ = run_mutating("/usr/bin/systemctl", &["reset-failed", unit]);
    // Do not block on the job. Readiness is judged by the ruleset's own
    // deadline, not by how long systemctl is willing to wait.
    let output = run_mutating("/usr/bin/systemctl", &["--no-block", "start", unit])?;
    if output.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "failed to start {unit}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

pub fn stop_unit(unit: &str) -> io::Result<()> {
    let output = run_mutating("/usr/bin/systemctl", &["stop", unit])?;
    if output.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "failed to stop {unit}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
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
    // Hide a legacy image before publishing the scoped one. A retained
    // known-good already exists at this point, and a crash here is reconciled
    // from that image rather than booting with two merge-visible copies.
    let legacy = legacy_active_path(name);
    if legacy != target && legacy.exists() {
        durable_remove(&legacy)?;
    }
    durable_copy(source, &target)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        ImageRelease, activation_space, durable_copy, durable_copy_file, extension_name,
        parse_sysext_release, prune_matching_in, prune_transaction_files_in, staging_space,
        version_valid,
    };

    fn temporary_directory(name: &str) -> std::path::PathBuf {
        let directory =
            std::env::temp_dir().join(format!("carbide-agent-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("create test directory");
        directory
    }

    /// SYSEXT_VERSION_ID ends in the same text as VERSION_ID. Reading the base
    /// version with a loose match picks up the extension's own version, pins
    /// the image to a base that does not exist, and nothing ever merges it.
    #[test]
    fn the_base_version_is_not_confused_with_the_extension_version() {
        let json = r#"{"sysextRelease":[
            "ID=carbideos",
            "SYSEXT_VERSION_ID=0.2.92",
            "VERSION_ID=0.1.58",
            "SYSEXT_ID=watchtower"
        ]}"#;
        assert_eq!(
            parse_sysext_release(json),
            ImageRelease {
                name: Some("watchtower".into()),
                base: Some("0.1.58".into()),
                version: Some("0.2.92".into()),
            }
        );
    }

    #[test]
    fn an_image_that_declares_nothing_reports_nothing() {
        let empty = ImageRelease {
            name: None,
            base: None,
            version: None,
        };
        assert_eq!(parse_sysext_release("{}"), empty);
        assert_eq!(
            parse_sysext_release("not json"),
            ImageRelease {
                name: None,
                base: None,
                version: None,
            }
        );
    }

    /// Versions are interpolated into image paths, so anything that could
    /// escape the extensions directory has to be refused before it is used.
    #[test]
    fn a_version_may_not_escape_a_path() {
        assert!(version_valid("0.1.58"));
        assert!(version_valid("0.2.92"));
        assert!(version_valid("rat-game-16"));
        assert!(!version_valid(""));
        assert!(!version_valid("../etc"));
        assert!(!version_valid("0.1/58"));
        assert!(!version_valid(".hidden"));
        assert!(!version_valid("has space"));
    }

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

    #[test]
    fn durable_copy_replaces_only_after_the_source_is_complete() {
        let directory = temporary_directory("durable-copy");
        let source = directory.join("source");
        let target = directory.join("extension.raw");
        fs::write(&source, b"new complete image").expect("write source");
        fs::write(&target, b"old complete image").expect("write target");

        durable_copy(&source, &target).expect("replace target");
        assert_eq!(
            fs::read(&target).expect("read target"),
            b"new complete image"
        );
        assert!(!directory.join(".extension.raw.new").exists());

        let missing = directory.join("missing");
        assert!(durable_copy(&missing, &target).is_err());
        assert_eq!(
            fs::read(&target).expect("read old target"),
            b"new complete image"
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn durable_copy_never_exceeds_the_budgeted_inode_size() {
        let directory = temporary_directory("bounded-copy");
        let source = directory.join("source");
        let target = directory.join("extension.raw");
        fs::write(&source, b"oversized").expect("write source");
        let mut source = fs::File::open(source).expect("open source");

        assert!(durable_copy_file(&mut source, &target, 4).is_err());
        assert!(!target.exists());
        assert!(!directory.join(".extension.raw.new").exists());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn transaction_space_covers_temporary_and_retained_images() {
        const MARGIN: u64 = 16 * 1024 * 1024;
        assert_eq!(activation_space(32, 0), 32 + MARGIN);
        assert_eq!(activation_space(32, 64), 96 + MARGIN);
        assert_eq!(staging_space(32, 64), 128 + MARGIN);
    }

    #[test]
    fn pruning_keeps_one_slot_and_never_crosses_base_scopes() {
        let directory = temporary_directory("prune");
        let keep = directory.join("watchtower_0.3.4.raw.2.rollback");
        let remove = directory.join("watchtower_0.3.4.raw.1.rollback");
        let other_base = directory.join("watchtower_0.3.3.raw.1.rollback");
        let other_extension = directory.join("chrome_0.3.4.raw.1.rollback");
        for path in [&keep, &remove, &other_base, &other_extension] {
            fs::write(path, b"image").expect("write image");
        }

        prune_matching_in(
            &directory,
            "watchtower_0.3.4.raw.",
            ".rollback",
            Some(&keep),
        )
        .expect("prune rollbacks");
        assert!(keep.exists());
        assert!(!remove.exists());
        assert!(other_base.exists());
        assert!(other_extension.exists());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn interrupted_private_files_are_pruned() {
        let directory = temporary_directory("transaction-prune");
        let staging = directory.join(".carbide-staging");
        fs::create_dir(&staging).expect("create staging directory");
        let active = directory.join("watchtower_0.3.4.raw");
        let temporary = directory.join(".watchtower_0.3.4.raw.new");
        let staged = staging.join("watchtower.raw");
        for path in [&active, &temporary, &staged] {
            fs::write(path, b"image").expect("write image");
        }

        prune_transaction_files_in(&directory, &staging).expect("prune transaction files");
        assert!(active.exists());
        assert!(!temporary.exists());
        assert!(!staged.exists());
        fs::remove_dir_all(directory).expect("remove test directory");
    }
}
