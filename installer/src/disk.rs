// SPDX-License-Identifier: AGPL-3.0-or-later
//! Block device discovery straight out of sysfs.

use std::fs;
use std::path::{Path, PathBuf};

/// First boot reserves roughly 1.9 GiB before any writable partition exists,
/// and the encrypted state filesystem carries a hard 2 GiB floor. Anything
/// under this provisions into a failure, so it is refused here instead.
pub const MIN_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Virtual, removable-media, and device-mapper nodes. None of them are a
/// sensible install target and several are actively dangerous to offer.
const SKIP_PREFIXES: [&str; 8] = ["loop", "ram", "zram", "sr", "fd", "dm-", "md", "nbd"];

#[derive(Clone, Debug)]
pub struct Disk {
    pub node: String,
    pub model: String,
    pub serial: String,
    pub bus: String,
    pub kind: String,
    pub bytes: u64,
    pub read_only: bool,
    pub boot_media: bool,
}

pub struct ConfirmCode {
    pub kind: &'static str,
    pub context: String,
    pub chars: Vec<String>,
}

impl Disk {
    pub fn eligible(&self) -> bool {
        !self.read_only && !self.boot_media && self.bytes >= MIN_BYTES
    }

    /// What the operator types to confirm erasing this disk. The end of the
    /// serial is printed on the drive itself, so typing it proves they are
    /// looking at the disk about to be erased. Without a usable serial, the
    /// end of the device name stands in.
    pub fn confirm_code(&self) -> ConfirmCode {
        const LEN: usize = 4;
        let upper = |c: char| c.to_ascii_uppercase().to_string();

        let serial: Vec<char> = self.serial.chars().collect();
        if serial.len() >= LEN
            && serial[serial.len() - LEN..].iter().all(char::is_ascii_alphanumeric)
        {
            let (context, code) = serial.split_at(serial.len() - LEN);
            return ConfirmCode {
                kind: "SERIAL",
                context: context.iter().collect(),
                chars: code.iter().copied().map(upper).collect(),
            };
        }

        let name = self.node.rsplit('/').next().unwrap_or(&self.node);
        let take = name.chars().count().min(LEN);
        let split = self.node.len()
            - name.chars().rev().take(take).map(char::len_utf8).sum::<usize>();
        ConfirmCode {
            kind: "DEVICE",
            context: self.node[..split].to_string(),
            chars: self.node[split..].chars().map(upper).collect(),
        }
    }

    pub fn note(&self) -> String {
        if self.boot_media {
            "INSTALLATION MEDIA".into()
        } else if self.read_only {
            "READ ONLY".into()
        } else if self.bytes < MIN_BYTES {
            "TOO SMALL".into()
        } else {
            String::new()
        }
    }
}

fn read_trimmed(path: PathBuf) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Drive capacity is marketed and labelled in decimal units, so a disk the
/// vendor calls 512 GB should read as 512 GB here. Matching the silkscreen
/// matters more than matching `lsblk` when someone is picking the right
/// physical device.
pub fn format_size(bytes: u64) -> String {
    const TB: f64 = 1e12;
    const GB: f64 = 1e9;
    let b = bytes as f64;
    if b >= TB {
        format!("{:.2} TB", b / TB)
    } else if b >= GB {
        format!("{:.1} GB", b / GB)
    } else {
        format!("{:.0} MB", b / 1e6)
    }
}

pub fn format_bytes_exact(bytes: u64) -> String {
    format!(
        "{} GiB",
        (bytes as f64 / (1024.0 * 1024.0 * 1024.0) * 100.0).round() / 100.0
    )
}

fn bus_of(link: &str) -> &'static str {
    // Ordered most specific first: an NVMe device also sits under a PCI path,
    // and a USB enclosure presents as SCSI further down the chain.
    if link.contains("/usb") {
        "USB"
    } else if link.contains("/nvme/") || link.contains("nvme") {
        "NVME"
    } else if link.contains("/ata") {
        "SATA"
    } else if link.contains("virtio") {
        "VIRTIO"
    } else if link.contains("/mmc") {
        "MMC"
    } else if link.contains("/scsi") {
        "SCSI"
    } else {
        "—"
    }
}

/// The ESP that systemd-boot loaded us from. Present only when we were
/// chained through the boot loader; a UKI launched straight from firmware
/// leaves this unset, in which case nothing is excluded on these grounds.
fn boot_disk() -> Option<String> {
    const VAR: &str = "/sys/firmware/efi/efivars/\
        LoaderDevicePartUUID-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";
    let raw = fs::read(VAR).ok()?;
    if raw.len() < 6 {
        return None;
    }
    // Four bytes of EFI variable attributes, then a NUL-terminated UTF-16LE
    // string.
    let units: Vec<u16> = raw[4..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| u16::from_le_bytes([p[0], p[1]]))
        .take_while(|&u| u != 0)
        .collect();
    let uuid = String::from_utf16(&units).ok()?.to_lowercase();

    let link = fs::read_link(Path::new("/dev/disk/by-partuuid").join(&uuid)).ok()?;
    let part = link.file_name()?.to_str()?.to_string();

    // Walk back from the partition node to the disk that holds it.
    for entry in fs::read_dir("/sys/block").ok()? {
        let disk = entry.ok()?.file_name().to_string_lossy().into_owned();
        if Path::new("/sys/block").join(&disk).join(&part).exists() {
            return Some(disk);
        }
    }
    None
}

pub fn enumerate() -> Vec<Disk> {
    let boot = boot_disk();
    let mut disks = Vec::new();

    let Ok(entries) = fs::read_dir("/sys/block") else {
        return disks;
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if SKIP_PREFIXES.iter().any(|p| name.starts_with(p)) {
            continue;
        }

        let base = Path::new("/sys/block").join(&name);
        let sectors: u64 = read_trimmed(base.join("size"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if sectors == 0 {
            continue;
        }

        let link = fs::read_link(&base)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        let rotational = read_trimmed(base.join("queue/rotational"))
            .map(|s| s == "1")
            .unwrap_or(false);
        let read_only = read_trimmed(base.join("ro"))
            .map(|s| s == "1")
            .unwrap_or(false);

        let model = read_trimmed(base.join("device/model"))
            .or_else(|| read_trimmed(base.join("device/name")))
            .unwrap_or_else(|| name.clone());

        let serial = read_trimmed(base.join("device/serial"))
            .or_else(|| read_trimmed(base.join("device/wwid")))
            .map(|s| s.chars().take(20).collect())
            .unwrap_or_default();

        disks.push(Disk {
            node: format!("/dev/{name}"),
            model,
            serial,
            bus: bus_of(&link).to_string(),
            kind: if rotational {
                "HDD".into()
            } else {
                "SSD".into()
            },
            bytes: sectors * 512,
            read_only,
            boot_media: boot.as_deref() == Some(name.as_str()),
        });
    }

    // Eligible devices first, then largest, so the machine's real system disk
    // lands under the cursor on arrival.
    disks.sort_by(|a, b| {
        b.eligible()
            .cmp(&a.eligible())
            .then(b.bytes.cmp(&a.bytes))
            .then(a.node.cmp(&b.node))
    });
    disks
}
