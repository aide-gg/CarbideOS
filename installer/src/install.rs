// SPDX-License-Identifier: AGPL-3.0-or-later
//! The write path.
//!
//! Scope is deliberately narrow. Partitioning, growing the layout, moving the
//! secondary GPT, encrypting state, and enrolling Secure Boot keys are all
//! first-boot concerns that CarbideOS already handles through systemd-repart
//! and its own enrolment path. Duplicating any of it here would be a second,
//! worse implementation. This gets the image onto the disk and gets out.
//!
//! Decompression is not in this path. The image is decoded into RAM while the
//! operator is still choosing a disk, so by the time a write starts there is
//! nothing to do but move bytes, and the only limit is the device.

use std::io;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::image::Image;
use crate::sys;

/// Written in spans rather than one call so several threads can keep an NVMe
/// queue busy, and so progress moves smoothly.
const SPAN: u64 = 32 << 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    Preparing,
    Discarding,
    Writing,
    Flushing,
    Verifying,
    Complete,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Preparing => "PREPARING",
            Stage::Discarding => "CLEARING",
            Stage::Writing => "WRITING",
            Stage::Flushing => "FLUSHING",
            Stage::Verifying => "VERIFYING",
            Stage::Complete => "COMPLETE",
        }
    }
}

#[derive(Default)]
pub struct Progress {
    written: AtomicU64,
    total: AtomicU64,
    stage: AtomicUsize,
    finished: AtomicBool,
    error: Mutex<Option<String>>,
}

impl Progress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    pub fn fraction(&self) -> f32 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        (self.written() as f64 / total as f64).clamp(0.0, 1.0) as f32
    }

    pub fn stage(&self) -> Stage {
        match self.stage.load(Ordering::Relaxed) {
            0 => Stage::Preparing,
            1 => Stage::Discarding,
            2 => Stage::Writing,
            3 => Stage::Flushing,
            4 => Stage::Verifying,
            _ => Stage::Complete,
        }
    }

    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    fn set_stage(&self, stage: Stage) {
        self.stage.store(stage as usize, Ordering::Relaxed);
    }
}

fn fail(progress: &Progress, context: &str, message: String) {
    *progress.error.lock().unwrap() = Some(format!("{context}: {message}"));
    progress.finished.store(true, Ordering::Relaxed);
}

/// Writes `image` to `target`. Intended to run on its own thread; the UI polls
/// `progress` rather than being called back on every span.
pub fn run(target: &Path, image: Arc<Image>, progress: Arc<Progress>) {
    progress.set_stage(Stage::Preparing);
    progress.total.store(image.total(), Ordering::Relaxed);

    if let Some(message) = image.error() {
        return fail(&progress, "preparing image", message);
    }

    let fd = match sys::open_block(target, true) {
        Ok(fd) => fd,
        Err(e) => return fail(&progress, "opening target device", e.to_string()),
    };

    let device_size = match sys::device_size(&fd) {
        Ok(s) => s,
        Err(e) => return fail(&progress, "querying device size", e.to_string()),
    };
    if device_size < image.total() {
        return fail(
            &progress,
            "target device is too small",
            format!(
                "{} available, {} required",
                crate::disk::format_size(device_size),
                crate::disk::format_size(image.total())
            ),
        );
    }

    // Best effort and intentionally unchecked. Discarding lets the controller
    // treat the whole write as fresh pages rather than read-modify-write,
    // which is a large part of why this finishes in seconds. Devices that do
    // not implement it simply proceed.
    progress.set_stage(Stage::Discarding);
    let _ = sys::discard(&fd, device_size);

    progress.set_stage(Stage::Writing);
    let total = image.total();
    let spans = total.div_ceil(SPAN);
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);
    let next = AtomicUsize::new(0);
    let failure: Mutex<Option<String>> = Mutex::new(None);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    if failure.lock().unwrap().is_some() {
                        return;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed) as u64;
                    if index >= spans {
                        return;
                    }

                    let offset = index * SPAN;
                    let len = SPAN.min(total - offset);

                    // Normally already satisfied: decoding began at startup
                    // and the operator spent seconds picking a disk.
                    if let Err(message) = image.wait_until(offset + len) {
                        *failure.lock().unwrap() = Some(message);
                        return;
                    }

                    let bytes = image.slice(offset, len as usize);
                    if let Err(e) = sys::pwrite_all(&fd, bytes, offset) {
                        *failure.lock().unwrap() = Some(e.to_string());
                        return;
                    }
                    progress.written.fetch_add(len, Ordering::Relaxed);
                }
            });
        }
    });

    if let Some(message) = failure.lock().unwrap().take() {
        return fail(&progress, "writing image", message);
    }

    progress.set_stage(Stage::Flushing);
    if let Err(e) = sys::fdatasync(&fd) {
        return fail(&progress, "flushing to device", e.to_string());
    }

    progress.set_stage(Stage::Verifying);
    if let Err(e) = spot_check(&fd, &image) {
        return fail(&progress, "verifying written image", e.to_string());
    }

    progress.set_stage(Stage::Complete);
    progress.finished.store(true, Ordering::Relaxed);
}

/// Reads a spread of the disk back and compares it against the image still
/// held in memory.
///
/// dm-verity already authenticates every block on every boot, so this is not
/// an integrity guarantee. It is a cheap way to catch the failures that would
/// otherwise surface as an unbootable machine: a device that acknowledged
/// writes it dropped, a dying controller, or a cable that came loose halfway
/// through.
fn spot_check(fd: &OwnedFd, image: &Image) -> io::Result<()> {
    const SAMPLES: u64 = 12;
    const WINDOW: usize = 1 << 20;

    let total = image.total();
    if total == 0 {
        return Ok(());
    }

    let mut buf = sys::AlignedBuf::new(WINDOW)?;
    for i in 0..SAMPLES {
        let span = total / SAMPLES;
        let mut offset = i * span;
        let len = WINDOW.min((total - offset) as usize);
        // Keep the read block aligned; O_DIRECT rejects anything else.
        offset -= offset % 4096;

        sys::pread_exact(fd, &mut buf.as_mut_slice()[..len], offset)?;
        if buf.as_slice()[..len] != *image.slice(offset, len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("data at offset {offset} did not read back as written"),
            ));
        }
    }
    Ok(())
}
