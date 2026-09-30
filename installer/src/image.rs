// SPDX-License-Identifier: AGPL-3.0-or-later
//! The carried image, decompressed into RAM ahead of the write.
//!
//! The installer ships the published `carbideos.raw.zst` byte for byte, so
//! what gets written is provably the artifact in the signed manifest rather
//! than something repacked along the way. That image is one zstd frame with a
//! 2 GiB long-distance window, which buys roughly 100 MiB over any chunked
//! alternative but can only be decoded from its start, on one core.
//!
//! So decoding starts the moment the installer does, while the operator is
//! still reading the first screen and choosing a disk. By the time they
//! finish confirming, the image is already resident and the install is a
//! straight memory-to-disk write with no decompression in the critical path.
//!
//! Nothing here authenticates the payload. It lives in the `.initrd` section
//! of the UKI, so the Secure Boot signature over that PE file already covers
//! it, and a second check inside the thing being verified would prove
//! nothing.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::sys::{AlignedBuf, Mapping};

/// Sidecar written at build time. The frame header does not declare the
/// uncompressed size, and the destination has to be allocated before the
/// first byte is decoded.
pub struct Meta {
    pub size: u64,
    pub label: String,
}

impl Meta {
    pub fn read(path: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(path)?;
        let mut size = None;
        let mut label = String::new();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "size" => size = value.trim().parse::<u64>().ok(),
                "label" => label = value.trim().to_string(),
                _ => {}
            }
        }
        let size = size.filter(|s| *s > 0).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "image metadata has no usable size",
            )
        })?;
        Ok(Self { size, label })
    }
}

pub struct Image {
    buf: *mut u8,
    len: usize,
    /// Bytes decoded so far. Written only by the decode thread with Release,
    /// read by everyone else with Acquire; that ordering is what makes the
    /// bytes below it safe to read.
    ready: AtomicU64,
    failed: Mutex<Option<String>>,
    pub label: String,
    _storage: Mutex<Option<AlignedBuf>>,
}

// The buffer is append-only: the decode thread writes strictly ahead of the
// watermark and never revisits what it published, and readers never look past
// it. That discipline is the whole reason this is safe to share.
unsafe impl Send for Image {}
unsafe impl Sync for Image {}

impl Image {
    /// Allocates the destination and starts decoding in the background.
    pub fn start(payload: &Path, meta: Meta) -> io::Result<Arc<Self>> {
        let mut storage = AlignedBuf::new(meta.size as usize)?;
        let buf = storage.as_mut_slice().as_mut_ptr();

        let image = Arc::new(Self {
            buf,
            len: meta.size as usize,
            ready: AtomicU64::new(0),
            failed: Mutex::new(None),
            label: meta.label,
            _storage: Mutex::new(Some(storage)),
        });

        let mapping = Mapping::open(payload)?;
        let worker = Arc::clone(&image);
        std::thread::spawn(move || {
            if let Err(e) = worker.decode(mapping) {
                *worker.failed.lock().unwrap() = Some(e);
            }
        });

        Ok(image)
    }

    fn decode(&self, mapping: Mapping) -> Result<(), String> {
        let mut dctx = zstd_safe::DCtx::create();
        // The image is compressed with a 2 GiB window; the default decoder
        // ceiling is far below that and would reject the frame outright.
        dctx.set_parameter(zstd_safe::DParameter::WindowLogMax(31))
            .map_err(|_| "could not raise the zstd window limit".to_string())?;

        let dst = unsafe { std::slice::from_raw_parts_mut(self.buf, self.len) };
        let source = mapping.as_slice();
        let mut consumed = 0usize;
        let mut produced = 0usize;

        // Input is fed in slices so the watermark advances steadily. Handing
        // zstd the whole frame at once decodes correctly but reports no
        // progress until it finishes.
        const FEED: usize = 4 << 20;

        while consumed < source.len() {
            let end = (consumed + FEED).min(source.len());
            let mut input = zstd_safe::InBuffer::around(&source[consumed..end]);

            loop {
                let mut output = zstd_safe::OutBuffer::around_pos(dst, produced);
                let hint = dctx
                    .decompress_stream(&mut output, &mut input)
                    .map_err(|code| {
                        format!("image decode failed: {}", zstd_safe::get_error_name(code))
                    })?;

                produced = output.pos();
                self.ready.store(produced as u64, Ordering::Release);

                if input.pos() == input.src.len() || hint == 0 {
                    break;
                }
                if produced == self.len {
                    break;
                }
            }

            consumed = end;
            if produced == self.len {
                break;
            }
        }

        if produced != self.len {
            return Err(format!(
                "image decoded to {produced} bytes, expected {}",
                self.len
            ));
        }
        Ok(())
    }

    pub fn total(&self) -> u64 {
        self.len as u64
    }

    pub fn ready(&self) -> u64 {
        self.ready.load(Ordering::Acquire)
    }

    pub fn complete(&self) -> bool {
        self.ready() == self.len as u64
    }

    pub fn error(&self) -> Option<String> {
        self.failed.lock().unwrap().clone()
    }

    /// Fraction decoded, for the preparing indicator.
    pub fn fraction(&self) -> f32 {
        if self.len == 0 {
            return 1.0;
        }
        (self.ready() as f64 / self.len as f64).clamp(0.0, 1.0) as f32
    }

    /// Blocks until `upto` bytes are available. Writers normally find the
    /// image already resident and never park here at all; this only matters
    /// when somebody confirms faster than the decoder finishes.
    pub fn wait_until(&self, upto: u64) -> Result<(), String> {
        while self.ready() < upto {
            if let Some(message) = self.error() {
                return Err(message);
            }
            std::thread::sleep(Duration::from_micros(250));
        }
        Ok(())
    }

    /// Caller must have waited for `offset + len`.
    pub fn slice(&self, offset: u64, len: usize) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.buf.add(offset as usize), len) }
    }
}
