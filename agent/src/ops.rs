// SPDX-License-Identifier: AGPL-3.0-or-later
//! The privileged operations, independent of who asked.
//!
//! Shared by the socket, the agent's command line, and `carbideos-ops`, so a
//! node repaired by hand and one repaired by the fleet end up identical.
//! Failures stay structured: the command line can flatten them, the socket
//! cannot reconstruct them.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::protocol::{Code, Failure, Stage};
use crate::state;
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

/// Not the extensions directory: sysext merges every `.raw` in there, and the
/// suffix cannot be dropped because dissect reads the format from it.
pub const STAGING_DIR: &str = "/var/lib/carbide/staging";

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

pub fn stage_extension(name: &str, base: &str, source: Source) -> Result<Staged, Failure> {
    if !system::version_valid(name) {
        return Err(Failure::new(
            Code::Malformed,
            format!("unusable extension name {name:?}"),
        ));
    }
    match source {
        Source::Acquire { version } => acquire(name, base, version.as_deref()),
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

    // sysupdate wrote straight into the extensions directory under the name
    // its transfer defines, so there is nothing to move; confirm it landed
    // where the merge will look for it.
    let staged = system::scoped_active_path(name, base);
    if !staged.exists() {
        return Err(Failure::new(
            Code::Failed,
            format!("{name} was acquired but no image for {base} is present"),
        )
        .at(Stage::Staging)
        .command(finished.command)
        .stderr(finished.stdout));
    }
    Ok(Staged {
        name: name.to_string(),
        version: base.to_string(),
        path: staged.to_string_lossy().into_owned(),
        acquired: true,
        // sysupdate placed this directly as the active image for its base, so
        // there is no candidate to activate and nothing to read a version from.
        extension_version: None,
        for_running_base: false,
    })
}

fn supplied(name: &str, base: &str, path: &Path, digest: &str) -> Result<Staged, Failure> {
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

    let expected = digest.strip_prefix("sha256:").unwrap_or(digest);
    let actual = system::sha256(path).map_err(|error| {
        Failure::new(Code::Failed, format!("could not digest the image: {error}"))
            .at(Stage::Validating)
    })?;
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(Failure::new(
            Code::DigestMismatch,
            format!("image digest is {actual}, caller declared {expected}"),
        )
        .at(Stage::Validating));
    }

    // Copied under a .raw name outside the extensions directory before it is
    // read: dissect needs the suffix, and a .raw file in the extensions
    // directory would be merged by the next refresh whether or not it turns
    // out to be valid.
    let staging = PathBuf::from(STAGING_DIR).join(format!("{name}.raw"));
    std::fs::create_dir_all(STAGING_DIR)
        .and_then(|()| std::fs::copy(path, &staging).map(|_| ()))
        .map_err(|error| {
            Failure::new(Code::Failed, format!("could not stage the image: {error}"))
                .at(Stage::Staging)
        })?;

    let outcome = validate_and_place(name, base, &staging);
    let _ = std::fs::remove_file(&staging);
    outcome.map(|placed| Staged {
        name: name.to_string(),
        version: base.to_string(),
        path: placed.path,
        acquired: false,
        extension_version: placed.extension_version,
        for_running_base: placed.for_running_base,
    })
}

struct Placed {
    path: String,
    extension_version: Option<String>,
    for_running_base: bool,
}

fn validate_and_place(name: &str, base: &str, staging: &Path) -> Result<Placed, Failure> {
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

    let (declared_base, declared_version) = system::image_versions(staging).map_err(|error| {
        Failure::new(Code::Failed, format!("could not read the image: {error}"))
            .at(Stage::Validating)
    })?;
    let declared_base = declared_base
        .filter(|v| system::version_valid(v))
        .ok_or_else(|| {
            Failure::new(
                Code::BaseMismatch,
                format!("{name} declares no usable base version"),
            )
            .at(Stage::Validating)
        })?;
    if declared_base != base {
        return Err(Failure::new(
            Code::BaseMismatch,
            format!("{name} declares base {declared_base}, staging was requested for {base}"),
        )
        .at(Stage::Validating));
    }

    let running = state::os_version();
    let for_running_base = running.as_deref() == Some(base);
    let declared_version = declared_version.filter(|v| system::version_valid(v));
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

    std::fs::rename(staging, &target)
        .or_else(|_| std::fs::copy(staging, &target).map(|_| ()))
        .map_err(|error| {
            Failure::new(Code::Failed, format!("could not place the image: {error}"))
                .at(Stage::Staging)
        })?;
    let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644));
    Ok(Placed {
        path: target.to_string_lossy().into_owned(),
        extension_version: declared_version,
        for_running_base,
    })
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

    let finished = system::stage_base(version).map_err(|error| {
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

    let Some(pending) = system::pending_base_version() else {
        // sysupdate reported success, so either the node was already current
        // or the slot did not end up bootable. Neither is an error, and the
        // caller can tell which from `pending`.
        return Ok(StagedBase {
            version: version.map(str::to_string),
            pending: false,
            boot_entry: None,
        });
    };

    // Refused before the boot entry is set, not after. An extension with no
    // image for the incoming base is a node that boots without it, and for a
    // node whose fleet agent is an extension that is indistinguishable from
    // bricking it.
    if let Some(missing) = missing_for(&pending) {
        return Err(Failure::new(
            Code::Precondition,
            format!("{missing} is installed but has no image staged for {pending}"),
        )
        .at(Stage::Checking));
    }

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

/// The first extension installed for the running base with nothing staged for
/// the incoming one.
pub fn missing_for(pending: &str) -> Option<String> {
    let running = state::os_version()?;
    if running == pending {
        return None;
    }
    let staged = system::installed_extensions_for(pending);
    system::installed_extensions_for(&running)
        .into_iter()
        .find(|name| !staged.iter().any(|present| present == name))
}
