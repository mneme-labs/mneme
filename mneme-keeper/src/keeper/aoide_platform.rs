// aoide_platform — Platform-specific WAL I/O implementations for Aoide.
//
// Selected at compile time via cfg gates:
//   Linux   — O_DIRECT + fallocate + pwrite + fdatasync
//   macOS   — F_NOCACHE + ftruncate + pwrite + F_FULLFSYNC
//   Other Unix — ftruncate + pwrite + sync_data
//   Windows — seek + write_all + sync_data (FlushFileBuffers)

// ── Linux ─────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod platform_impl {
    use std::fs::{File, OpenOptions};
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;
    use anyhow::{bail, Result};

    pub fn open_wal_file(path: &Path) -> Result<File> {
        let file = OpenOptions::new()
            .read(true).write(true).create(true)
            // O_DIRECT: bypass page cache for deterministic write latency
            .custom_flags(libc::O_DIRECT)
            .open(path)?;
        Ok(file)
    }

    pub fn preallocate(file: &File, max_bytes: u64) {
        let ret = unsafe {
            libc::fallocate(
                file.as_raw_fd(),
                0, // FALLOC_FL_KEEP_SIZE not set — extend file
                0,
                max_bytes as libc::off_t,
            )
        };
        if ret != 0 {
            tracing::warn!(
                "fallocate failed ({}), continuing without pre-allocation",
                std::io::Error::last_os_error()
            );
        }
    }

    pub fn pwrite_all(file: &mut File, buf: &[u8], offset: u64) -> Result<()> {
        let written = unsafe {
            libc::pwrite(
                file.as_raw_fd(),
                buf.as_ptr() as *const libc::c_void,
                buf.len(),
                offset as libc::off_t,
            )
        };
        if written < 0 || written as usize != buf.len() {
            bail!("pwrite failed: {}", std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn fsync(file: &mut File) -> Result<()> {
        let ret = unsafe { libc::fdatasync(file.as_raw_fd()) };
        if ret != 0 {
            bail!("fdatasync: {}", std::io::Error::last_os_error());
        }
        Ok(())
    }
}

// ── macOS ─────────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod platform_impl {
    use std::fs::{File, OpenOptions};
    use std::os::unix::io::AsRawFd;
    use std::path::Path;
    use anyhow::{bail, Result};

    pub fn open_wal_file(path: &Path) -> Result<File> {
        let file = OpenOptions::new()
            .read(true).write(true).create(true)
            .open(path)?;
        // F_NOCACHE: disable page-cache read-ahead (macOS DIO equivalent)
        let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) };
        if ret < 0 {
            tracing::warn!(
                "F_NOCACHE failed ({}), continuing with page cache",
                std::io::Error::last_os_error()
            );
        }
        Ok(file)
    }

    pub fn preallocate(file: &File, max_bytes: u64) {
        let ret = unsafe {
            libc::ftruncate(file.as_raw_fd(), max_bytes as libc::off_t)
        };
        if ret != 0 {
            tracing::warn!(
                "ftruncate failed ({}), continuing without pre-allocation",
                std::io::Error::last_os_error()
            );
        }
    }

    pub fn pwrite_all(file: &mut File, buf: &[u8], offset: u64) -> Result<()> {
        let written = unsafe {
            libc::pwrite(
                file.as_raw_fd(),
                buf.as_ptr() as *const libc::c_void,
                buf.len(),
                offset as libc::off_t,
            )
        };
        if written < 0 || written as usize != buf.len() {
            bail!("pwrite failed: {}", std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn fsync(file: &mut File) -> Result<()> {
        // F_FULLFSYNC flushes the drive write cache — stronger than fsync
        let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC, 0) };
        if ret < 0 {
            // Fallback for network filesystems that don't support F_FULLFSYNC
            file.sync_data()
                .map_err(|e| anyhow::anyhow!("fsync fallback: {e}"))?;
        }
        Ok(())
    }
}

// ── Generic POSIX (FreeBSD, illumos, …) ──────────────────────────────────────

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
mod platform_impl {
    use std::fs::{File, OpenOptions};
    use std::os::unix::io::AsRawFd;
    use std::path::Path;
    use anyhow::{bail, Result};

    pub fn open_wal_file(path: &Path) -> Result<File> {
        Ok(OpenOptions::new().read(true).write(true).create(true).open(path)?)
    }

    pub fn preallocate(file: &File, max_bytes: u64) {
        let ret = unsafe { libc::ftruncate(file.as_raw_fd(), max_bytes as libc::off_t) };
        if ret != 0 {
            tracing::warn!("ftruncate failed ({})", std::io::Error::last_os_error());
        }
    }

    pub fn pwrite_all(file: &mut File, buf: &[u8], offset: u64) -> Result<()> {
        let written = unsafe {
            libc::pwrite(
                file.as_raw_fd(),
                buf.as_ptr() as *const libc::c_void,
                buf.len(),
                offset as libc::off_t,
            )
        };
        if written < 0 || written as usize != buf.len() {
            bail!("pwrite failed: {}", std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn fsync(file: &mut File) -> Result<()> {
        file.sync_data().map_err(|e| anyhow::anyhow!("sync_data: {e}"))
    }
}

// ── Windows ───────────────────────────────────────────────────────────────────

#[cfg(windows)]
mod platform_impl {
    use std::fs::{File, OpenOptions};
    use std::io::{Seek, SeekFrom, Write};
    use std::path::Path;
    use anyhow::Result;

    pub fn open_wal_file(path: &Path) -> Result<File> {
        Ok(OpenOptions::new().read(true).write(true).create(true).open(path)?)
    }

    pub fn preallocate(file: &File, max_bytes: u64) {
        if let Err(e) = file.set_len(max_bytes) {
            tracing::warn!("set_len pre-alloc failed: {e}");
        }
    }

    pub fn pwrite_all(file: &mut File, buf: &[u8], offset: u64) -> Result<()> {
        // Windows has no pwrite — seek then write (WAL is single-writer)
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| anyhow::anyhow!("seek: {e}"))?;
        file.write_all(buf)
            .map_err(|e| anyhow::anyhow!("write_all: {e}"))?;
        Ok(())
    }

    pub fn fsync(file: &mut File) -> Result<()> {
        file.sync_data().map_err(|e| anyhow::anyhow!("sync_data: {e}"))
    }
}

// ── Re-export as `platform` for use in aoide.rs ──────────────────────────────

pub use platform_impl::*;
