// SPDX-License-Identifier: AGPL-3.0-or-later
//! Thin wrappers over the block-device ioctls and the aligned buffers that
//! `O_DIRECT` requires.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

const BLKGETSIZE64: libc::c_ulong = 0x80081272;
const BLKDISCARD: libc::c_ulong = 0x1277;

/// Page-aligned heap buffer. `O_DIRECT` rejects transfers whose memory
/// address, file offset, or length are not multiples of the logical block
/// size, and a plain `Vec<u8>` gives no alignment guarantee at all.
pub struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

// The pointer is uniquely owned; the buffer is handed to exactly one worker
// thread at a time and never aliased.
unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    pub fn new(len: usize) -> io::Result<Self> {
        const ALIGN: usize = 4096;
        let len = len.div_ceil(ALIGN) * ALIGN;
        let mut ptr: *mut libc::c_void = std::ptr::null_mut();
        let rc = unsafe { libc::posix_memalign(&mut ptr, ALIGN, len) };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        unsafe { std::ptr::write_bytes(ptr as *mut u8, 0, len) };
        Ok(Self {
            ptr: ptr as *mut u8,
            len,
        })
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { libc::free(self.ptr as *mut libc::c_void) };
    }
}

fn cpath(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

/// Opens a block device for writing. `direct` bypasses the page cache, which
/// keeps a 1.2 GiB write from evicting the payload we are reading it from.
pub fn open_block(path: &Path, direct: bool) -> io::Result<OwnedFd> {
    let mut flags = libc::O_RDWR | libc::O_CLOEXEC;
    if direct {
        flags |= libc::O_DIRECT;
    }
    let c = cpath(path)?;
    let fd = unsafe { libc::open(c.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn open_read(path: &Path, direct: bool) -> io::Result<OwnedFd> {
    let mut flags = libc::O_RDONLY | libc::O_CLOEXEC;
    if direct {
        flags |= libc::O_DIRECT;
    }
    let c = cpath(path)?;
    let fd = unsafe { libc::open(c.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn device_size(fd: &OwnedFd) -> io::Result<u64> {
    let mut size: u64 = 0;
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), BLKGETSIZE64, &mut size) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(size)
}

/// Best effort. Discarding first lets an SSD controller treat the whole write
/// as fresh pages instead of read-modify-write, which is a large part of why
/// the install finishes in seconds. Failure is not fatal: plenty of devices
/// simply do not implement it.
pub fn discard(fd: &OwnedFd, size: u64) -> io::Result<()> {
    let range: [u64; 2] = [0, size];
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), BLKDISCARD, range.as_ptr()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn pwrite_all(fd: &OwnedFd, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = unsafe {
            libc::pwrite(
                fd.as_raw_fd(),
                buf.as_ptr() as *const libc::c_void,
                buf.len(),
                offset as libc::off_t,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short write"));
        }
        buf = &buf[n as usize..];
        offset += n as u64;
    }
    Ok(())
}

pub fn pread_exact(fd: &OwnedFd, buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = unsafe {
            libc::pread(
                fd.as_raw_fd(),
                buf[done..].as_mut_ptr() as *mut libc::c_void,
                buf.len() - done,
                offset as libc::off_t,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
        }
        done += n as usize;
        offset += n as u64;
    }
    Ok(())
}

pub fn fdatasync(fd: &OwnedFd) -> io::Result<()> {
    if unsafe { libc::fdatasync(fd.as_raw_fd()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Read-only shared mapping of the payload. The payload already lives in the
/// initramfs tmpfs, so mapping it hands every worker thread the same physical
/// pages with no copy and no disk read.
pub struct Mapping {
    ptr: *const u8,
    len: usize,
}

unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    pub fn open(path: &Path) -> io::Result<Self> {
        let fd = open_read(path, false)?;
        let len = {
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } < 0 {
                return Err(io::Error::last_os_error());
            }
            st.st_size as usize
        };
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "empty payload"));
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            ptr: ptr as *const u8,
            len,
        })
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
    }
}

pub fn sync_all() {
    unsafe { libc::sync() };
}

pub fn reboot() -> ! {
    sync_all();
    unsafe { libc::reboot(libc::RB_AUTOBOOT) };
    std::process::exit(0)
}

pub fn power_off() -> ! {
    sync_all();
    unsafe { libc::reboot(libc::RB_POWER_OFF) };
    std::process::exit(0)
}
