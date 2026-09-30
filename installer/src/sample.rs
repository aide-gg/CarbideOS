// SPDX-License-Identifier: AGPL-3.0-or-later
//! Stand-in hardware for the preview and recorder binaries.

use slint::{ModelRc, SharedString, VecModel};

use super::DiskInfo;

fn code(chars: &str) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        chars.chars().map(|c| SharedString::from(c.to_string())).collect::<Vec<_>>(),
    ))
}

pub fn disks() -> Vec<DiskInfo> {
    vec![
        DiskInfo {
            node: "/dev/nvme0n1".into(),
            model: "Samsung SSD 990 PRO 2TB".into(),
            size: "2.00 TB".into(),
            bus: "NVME".into(),
            kind: "SSD".into(),
            serial: "S6Z1NJ0T910481".into(),
            eligible: true,
            boot_media: false,
            note: "".into(),
            code_chars: code("0481"),
            code_kind: "SERIAL".into(),
            code_context: "S6Z1NJ0T91".into(),
        },
        DiskInfo {
            node: "/dev/sda".into(),
            model: "CT500MX500SSD1".into(),
            size: "500.1 GB".into(),
            bus: "SATA".into(),
            kind: "SSD".into(),
            serial: "2138E5F2A9C1".into(),
            eligible: true,
            boot_media: false,
            note: "".into(),
            code_chars: code("A9C1"),
            code_kind: "SERIAL".into(),
            code_context: "2138E5F2".into(),
        },
        DiskInfo {
            node: "/dev/sdb".into(),
            model: "SanDisk Ultra USB 3.0".into(),
            size: "30.8 GB".into(),
            bus: "USB".into(),
            kind: "SSD".into(),
            serial: "4C5300012010".into(),
            eligible: false,
            boot_media: true,
            note: "INSTALLATION MEDIA".into(),
            code_chars: code("2010"),
            code_kind: "SERIAL".into(),
            code_context: "4C530001".into(),
        },
    ]
}
