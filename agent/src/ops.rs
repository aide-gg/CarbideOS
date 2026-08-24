// SPDX-License-Identifier: AGPL-3.0-or-later
//! The privileged operations, independent of who asked.
//!
//! Shared by the socket, the agent's command line, and `carbideos-ops`, so a
//! node repaired by hand and one repaired by the fleet end up identical.
//! Failures stay structured: the command line can flatten them, the socket
//! cannot reconstruct them.

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::protocol::{Code, Failure, Stage};
use crate::state::{self, Phase, State};
use crate::system;

/// Where an image is coming from.
pub enum Source {
    /// The agent fetches it through its own sealed configuration.
    Acquire { version: Option<String> },
    /// The caller already holds it, because the agent cannot reach it.
    Supplied { path: PathBuf, digest: String },
}

pub struct Staged {
    pub name: String,
    /// The base version this image is for.
    pub version: String,
    pub path: String,
    pub acquired: bool,
    /// The extension's own version, as the image declares it. Needed to
    /// activate it, and distinct from `version`, which is the base.
    pub extension_version: Option<String>,
    /// Whether this image is for the base currently running, and therefore
    /// something that can be activated now rather than only at the next boot.
    pub for_running_base: bool,
}

pub struct StagedBase {
    pub version: Option<String>,
    pub pending: bool,
    pub boot_entry: Option<String>,
}

/// A hidden child rather than the merge directory itself. Images must retain a
/// `.raw` suffix for systemd-dissect, while systemd-sysext only scans the top
/// level and therefore cannot merge anything here prematurely.
pub const STAGING_DIR: &str = system::STAGING_DIR;
const MAX_ACQUIRED_IMAGE: u64 = 1024 * 1024 * 1024;

/// Resolve the base version an image is being staged for.
pub fn target_base(requested: Option<&str>) -> Result<String, Failure> {
    match requested {
        Some(base) if !system::version_valid(base) => Err(Failure::new(
            Code::Malformed,
            format!("unusable base version {base:?}"),
        )),
        Some(base) => Ok(base.to_string()),
        None => state::os_version()
            .ok_or_else(|| Failure::new(Code::Failed, "could not read the running base version")),
    }
}

/// Place an image for a base version.
///
/// `base` absent means the caller did not care. A supplied image declares the
/// base it was built for, so that is honoured rather than overridden with
/// whatever happens to be running: an image for a base this node has not
/// booted yet is the normal way to prepare an update, and refusing it made
/// staging one impossible. An explicit `base` is still enforced, because a
/// caller that names one is asserting something worth checking.
pub fn stage_extension(name: &str, base: Option<&str>, source: Source) -> Result<Staged, Failure> {
    if !system::version_valid(name) {
        return Err(Failure::new(
            Code::Malformed,
            format!("unusable extension name {name:?}"),
        ));
    }
    match source {
        // Acquisition selects on the version, so it needs one resolved; the
        // running base is the only sensible default.
        Source::Acquire { version } => {
            let base = match base {
                Some(base) => base.to_string(),
                None => target_base(None)?,
            };
            acquire(name, &base, version.as_deref())
        }
        Source::Supplied { path, digest } => supplied(name, base, &path, &digest),
    }
}

fn acquire(name: &str, base: &str, version: Option<&str>) -> Result<Staged, Failure> {
    if !system::components().iter().any(|c| c == name) {
        return Err(Failure::new(
            Code::NotFound,
            format!("no source is configured for {name}; supply an image instead"),
        )
        .at(Stage::Checking));
    }
    std::fs::create_dir_all(STAGING_DIR).map_err(|error| {
        Failure::new(
            Code::Failed,
            format!("could not create extension staging directory: {error}"),
        )
        .at(Stage::Staging)
    })?;
    // The signed feed caps extension DDIs at 1 GiB. Reserving that full bound
    // before sysupdate starts guarantees even the largest accepted download
    // cannot consume the filesystem's transaction margin.
    require_space(MAX_ACQUIRED_IMAGE.saturating_add(16 * 1024 * 1024))?;
    // Extension images are named for the base they target, so the version
    // sysupdate selects on is that base. Otherwise it takes the newest in the
    // feed, which belongs to whichever base was published last.
    let selector = version.unwrap_or(base);
    let finished = system::acquire_component(name, Some(selector)).map_err(|error| {
        Failure::new(
            Code::Failed,
            format!("could not run systemd-sysupdate: {error}"),
        )
        .at(Stage::Staging)
    })?;
    if !finished.ok {
        return Err(
            Failure::new(Code::Failed, format!("could not acquire {name}"))
                .at(Stage::Staging)
                .command(finished.command)
                .exit_code(finished.code)
                .stderr(finished.stderr),
        );
    }

    // Sysupdate writes into the hidden staging directory. Treat those bytes
    // exactly like a caller-supplied image from here onward: validate their
    // signed DDI metadata, then promote through the candidate lifecycle.
    let staged = Path::new(STAGING_DIR).join(format!("{name}_{base}.raw"));
    if !staged.exists() {
        return Err(Failure::new(
            Code::Failed,
            format!("{name} was acquired but no image for {base} is present"),
        )
        .at(Stage::Staging)
        .command(finished.command)
        .stderr(finished.stdout));
    }
    let downloaded_size = std::fs::metadata(&staged)
        .map_err(|error| Failure::new(Code::Failed, error.to_string()).at(Stage::Staging))?
        .len();
    if downloaded_size > MAX_ACQUIRED_IMAGE {
        let _ = system::durable_remove(&staged);
        return Err(Failure::new(
            Code::NoSpace,
            format!("acquired image is {downloaded_size} bytes; limit is {MAX_ACQUIRED_IMAGE}"),
        )
        .at(Stage::Staging));
    }
    let placed = match validate_and_place(name, Some(base), &staged) {
        Ok(placed) => placed,
        Err(failure) => {
            let _ = system::durable_remove(&staged);
            return Err(failure);
        }
    };
    Ok(Staged {
        name: name.to_string(),
        version: placed.base,
        path: placed.path,
        acquired: true,
        extension_version: placed.extension_version,
        for_running_base: placed.for_running_base,
    })
}

fn supplied(name: &str, base: Option<&str>, path: &Path, digest: &str) -> Result<Staged, Failure> {
    if !path.is_absolute() {
        return Err(Failure::new(
            Code::Malformed,
            "an image path must be absolute",
        ));
    }
    if !path.exists() {
        return Err(
            Failure::new(Code::NotFound, format!("no image at {}", path.display()))
                .at(Stage::Checking),
        );
    }

    let mut source = File::open(path)
        .map_err(|error| Failure::new(Code::Failed, error.to_string()).at(Stage::Checking))?;
    let image_size = source
        .metadata()
        .map_err(|error| Failure::new(Code::Failed, error.to_string()).at(Stage::Checking))?
        .len();
    let running = base
        .map(|requested| state::os_version().as_deref() == Some(requested))
        .unwrap_or(true);
    let rollback_size = if running {
        missing_rollback_size(name)
            .map_err(|error| Failure::new(Code::Failed, error.to_string()).at(Stage::Checking))?
    } else {
        0
    };
    require_space(system::staging_space(image_size, rollback_size))?;

    // Copied under a .raw name outside the extensions directory before it is
    // read or hashed. Binding validation to this private durable copy prevents
    // a caller from replacing its source path between digesting and copying.
    // Dissect needs the suffix, and a .raw file in the extensions directory
    // would be merged by the next refresh whether or not it is valid.
    let staging = PathBuf::from(STAGING_DIR).join(format!("{name}.raw"));
    std::fs::create_dir_all(STAGING_DIR)
        .and_then(|()| system::durable_copy_file(&mut source, &staging, image_size))
        .map_err(|error| {
            Failure::new(Code::Failed, format!("could not stage the image: {error}"))
                .at(Stage::Staging)
        })?;

    let expected = digest.strip_prefix("sha256:").unwrap_or(digest);
    let actual = system::sha256(&staging).map_err(|error| {
        let _ = system::durable_remove(&staging);
        Failure::new(
            Code::Failed,
            format!("could not digest the staged image: {error}"),
        )
        .at(Stage::Validating)
    })?;
    if !actual.eq_ignore_ascii_case(expected) {
        let _ = system::durable_remove(&staging);
        return Err(Failure::new(
            Code::DigestMismatch,
            format!("image digest is {actual}, caller declared {expected}"),
        )
        .at(Stage::Validating));
    }

    let outcome = validate_and_place(name, base, &staging);
    let _ = system::durable_remove(&staging);
    outcome.map(|placed| Staged {
        name: name.to_string(),
        version: placed.base,
        path: placed.path,
        acquired: false,
        extension_version: placed.extension_version,
        for_running_base: placed.for_running_base,
    })
}

struct Placed {
    path: String,
    /// The base the image was actually placed for.
    base: String,
    extension_version: Option<String>,
    for_running_base: bool,
}

fn validate_and_place(
    name: &str,
    requested: Option<&str>,
    staging: &Path,
) -> Result<Placed, Failure> {
    let validated = system::validate_image(staging).map_err(|error| {
        Failure::new(
            Code::Failed,
            format!("could not validate the image: {error}"),
        )
        .at(Stage::Validating)
    })?;
    if !validated.ok {
        return Err(Failure::new(
            Code::Untrusted,
            format!("{name} failed signed image validation"),
        )
        .at(Stage::Validating)
        .command(validated.command)
        .exit_code(validated.code)
        .stderr(validated.stderr));
    }

    let release = system::image_release(staging).map_err(|error| {
        Failure::new(Code::Failed, format!("could not read the image: {error}"))
            .at(Stage::Validating)
    })?;
    if release.name.as_deref() != Some(name) {
        return Err(Failure::new(
            Code::Untrusted,
            format!(
                "image declares extension {:?}, staging was requested for {name}",
                release.name.as_deref().unwrap_or("none")
            ),
        )
        .at(Stage::Validating));
    }
    let declared_base = release
        .base
        .filter(|v| system::version_valid(v))
        .ok_or_else(|| {
            Failure::new(
                Code::BaseMismatch,
                format!("{name} declares no usable base version"),
            )
            .at(Stage::Validating)
        })?;
    // Only when the caller named one. Otherwise the image decides, which is
    // what makes staging for a base this node has not booted yet possible.
    if let Some(requested) = requested
        && declared_base != requested
    {
        return Err(Failure::new(
            Code::BaseMismatch,
            format!("{name} declares base {declared_base}, staging was requested for {requested}"),
        )
        .at(Stage::Validating));
    }
    let base = declared_base.as_str();

    let running = state::os_version();
    let for_running_base = running.as_deref() == Some(base);
    let declared_version = release.version.filter(|v| system::version_valid(v));
    let target = if for_running_base {
        // For the running base the image becomes a candidate, so activation
        // can prove it before it replaces what is working.
        let version = declared_version.clone().ok_or_else(|| {
            Failure::new(
                Code::Failed,
                format!("{name} declares no usable extension version"),
            )
            .at(Stage::Validating)
        })?;
        system::candidate_path(name, &version)
    } else {
        // For a base this node has not booted, the image is simply placed. The
        // incoming base merges it at boot, and until then nothing reads it.
        system::scoped_active_path(name, base)
    };

    let image_size = std::fs::metadata(staging)
        .map_err(|error| Failure::new(Code::Failed, error.to_string()).at(Stage::Staging))?
        .len();
    let rollback_size = if for_running_base {
        missing_rollback_size(name)
            .map_err(|error| Failure::new(Code::Failed, error.to_string()).at(Stage::Checking))?
    } else {
        0
    };
    require_space(system::activation_space(image_size, rollback_size))?;

    if for_running_base {
        system::prune_candidates(name, None).map_err(|error| {
            Failure::new(
                Code::Failed,
                format!("could not prune old candidates: {error}"),
            )
            .at(Stage::Staging)
        })?;
    }
    system::durable_mode(staging, 0o644).map_err(|error| {
        Failure::new(
            Code::Failed,
            format!("could not set staged image mode: {error}"),
        )
        .at(Stage::Staging)
    })?;
    system::durable_move(staging, &target).map_err(|error| {
        Failure::new(Code::Failed, format!("could not place the image: {error}")).at(Stage::Staging)
    })?;

    if for_running_base {
        let version = declared_version.as_deref().expect("checked above");
        let mut state = State::load().map_err(|error| {
            Failure::new(Code::Failed, format!("could not read agent state: {error}"))
                .at(Stage::Staging)
        })?;
        let entry = state.entry(name);
        entry.candidate_version = Some(version.to_string());
        entry.phase = Phase::Staged;
        if let Err(error) = state.store() {
            let _ = system::durable_remove(&target);
            return Err(Failure::new(
                Code::Failed,
                format!("could not record staged image: {error}"),
            )
            .at(Stage::Staging));
        }
    }
    Ok(Placed {
        path: target.to_string_lossy().into_owned(),
        base: base.to_string(),
        extension_version: declared_version,
        for_running_base,
    })
}

fn require_space(required: u64) -> Result<(), Failure> {
    let available = system::available_bytes()
        .map_err(|error| Failure::new(Code::Failed, error.to_string()).at(Stage::Checking))?;
    if available >= required {
        return Ok(());
    }
    Err(Failure::new(
        Code::NoSpace,
        format!("insufficient space: transaction needs {required} bytes, {available} available"),
    )
    .at(Stage::Checking))
}

fn missing_rollback_size(name: &str) -> std::io::Result<u64> {
    let state = State::load()?;
    if let Some(version) = state
        .get(name)
        .and_then(|entry| entry.known_good_version.as_deref())
        && system::rollback_path(name, version).exists()
    {
        return Ok(0);
    }
    let active = system::active_path(name);
    match std::fs::metadata(active) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

/// The version of an extension currently merged into the running base.
///
/// Read from the merged tree rather than from agent state, because a node
/// whose first image predates candidate activation has an image merged and no
/// state describing it. Adopting that as known-good before replacing it is
/// what gives such a node something to roll back to.
pub fn merged_version(name: &str) -> Option<String> {
    let release = format!("/usr/lib/extension-release.d/extension-release.{name}");
    let contents = std::fs::read_to_string(release).ok()?;
    contents.lines().find_map(|line| {
        line.strip_prefix("SYSEXT_VERSION_ID=")
            .map(|value| value.trim().trim_matches('"').to_string())
            .filter(|value| system::version_valid(value))
    })
}

/// Download and stage a base image, then select its boot entry.
pub fn stage_base(version: Option<&str>) -> Result<StagedBase, Failure> {
    if let Some(version) = version
        && !system::version_valid(version)
    {
        return Err(Failure::new(
            Code::Malformed,
            format!("unusable base version {version:?}"),
        ));
    }

    let target = match version {
        Some(version) => Some(version.to_string()),
        None => system::available_base_version()
            .map_err(|error| Failure::new(Code::Failed, error).at(Stage::Checking))?,
    };
    let Some(target) = target else {
        return Ok(StagedBase {
            version: None,
            pending: false,
            boot_entry: None,
        });
    };
    verify_extensions_for(&target)?;

    let finished = system::stage_base(Some(&target)).map_err(|error| {
        Failure::new(
            Code::Failed,
            format!("could not run systemd-sysupdate: {error}"),
        )
        .at(Stage::Staging)
    })?;
    if !finished.ok {
        return Err(Failure::new(Code::Failed, "could not stage the base image")
            .at(Stage::Staging)
            .command(finished.command)
            .exit_code(finished.code)
            .stderr(finished.stderr));
    }
    let _ = system::sync();

    let pending = system::pending_base_version()
        .map_err(|error| Failure::new(Code::Failed, error).at(Stage::Checking))?;
    let Some(pending) = pending else {
        // sysupdate reported success, so either the node was already current
        // or the slot did not end up bootable. Neither is an error, and the
        // caller can tell which from `pending`.
        return Ok(StagedBase {
            version: Some(target),
            pending: false,
            boot_entry: None,
        });
    };
    if pending != target {
        return Err(Failure::new(
            Code::Precondition,
            format!("requested base {target}, but sysupdate left {pending} pending"),
        )
        .at(Stage::Checking));
    }

    // Refused before the boot entry is set, not after. An extension with no
    // image for the incoming base is a node that boots without it, and for a
    // node whose fleet agent is an extension that is indistinguishable from
    // bricking it.
    verify_extensions_for(&pending)?;

    let entry = system::pending_boot_entry(&pending).ok_or_else(|| {
        Failure::new(
            Code::Failed,
            format!("no counted boot entry was written for {pending}"),
        )
        .at(Stage::Staging)
    })?;
    let selected = system::set_oneshot(&entry).map_err(|error| {
        Failure::new(Code::Failed, format!("could not run bootctl: {error}")).at(Stage::Staging)
    })?;
    if !selected.ok {
        return Err(
            Failure::new(Code::Failed, "could not select the staged boot entry")
                .at(Stage::Staging)
                .command(selected.command)
                .exit_code(selected.code)
                .stderr(selected.stderr),
        );
    }
    Ok(StagedBase {
        version: Some(pending),
        pending: true,
        boot_entry: Some(entry),
    })
}

fn verify_extensions_for(pending: &str) -> Result<(), Failure> {
    let Some(running) = state::os_version() else {
        return Err(Failure::new(
            Code::Failed,
            "could not read the running base version",
        ));
    };
    if running == pending {
        return Ok(());
    }
    let mut names = system::installed_extensions_for(&running);
    if let Ok(merged) = system::merged_extensions() {
        names.extend(merged);
    }
    if let Ok(state) = State::load() {
        names.extend(state.required());
    }
    names.sort();
    names.dedup();
    for name in names {
        let image = system::scoped_active_path(&name, pending);
        if !image.exists() {
            return Err(Failure::new(
                Code::Precondition,
                format!("{name} is installed but has no image staged for {pending}"),
            )
            .at(Stage::Checking));
        }
        let validated = system::validate_image(&image).map_err(|error| {
            Failure::new(
                Code::Failed,
                format!("could not validate {}: {error}", image.display()),
            )
            .at(Stage::Validating)
        })?;
        if !validated.ok {
            return Err(Failure::new(
                Code::Untrusted,
                format!("{name} has an untrusted image staged for {pending}"),
            )
            .at(Stage::Validating)
            .command(validated.command)
            .exit_code(validated.code)
            .stderr(validated.stderr));
        }
        let release = system::image_release(&image).map_err(|error| {
            Failure::new(
                Code::Failed,
                format!("could not inspect {}: {error}", image.display()),
            )
            .at(Stage::Validating)
        })?;
        if release.name.as_deref() != Some(name.as_str())
            || release.base.as_deref() != Some(pending)
        {
            return Err(Failure::new(
                Code::BaseMismatch,
                format!("{name} has an incompatible image staged for {pending}"),
            )
            .at(Stage::Validating));
        }
    }
    Ok(())
}
