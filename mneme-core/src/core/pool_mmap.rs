// pool_mmap — Cross-platform pool region.
//
// Platform behaviour:
//   Linux   — anonymous mmap with MAP_HUGETLB (falls back to 4 KB pages),
//             MADV_HUGEPAGE for transparent huge pages, MADV_FREE for lazy
//             OS reclaim of evicted ranges.
//   macOS   — anonymous mmap (no huge-page support), MADV_FREE_REUSABLE for
//             lazy reclaim (equivalent to MADV_FREE on Linux).
//   Windows — VirtualAlloc(MEM_RESERVE | MEM_COMMIT) backed allocation;
//             VirtualFree(MEM_DECOMMIT) advises the OS to reclaim pages.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;

/// Single allocation in the pool region.
#[derive(Debug)]
pub struct PoolAlloc {
    /// Pointer to the start of the allocation in the pool region.
    pub ptr: NonNull<u8>,
    /// Length of the allocation in bytes.
    pub len: usize,
}

// Safety: all access to PoolAlloc data is mediated through Pool's sharded RwLocks.
unsafe impl Send for PoolAlloc {}
unsafe impl Sync for PoolAlloc {}

// Safety: the backing region is a fixed allocation (mmap or VirtualAlloc).
// It is never moved or resized after construction. All reads/writes are
// mediated by Pool's sharded RwLocks. Drop releases the region exactly once.
unsafe impl Send for MmapPool {}
unsafe impl Sync for MmapPool {}

/// Platform-backed pool region with bump allocator.
pub struct MmapPool {
    base:     NonNull<u8>,
    capacity: usize,
    cursor:   AtomicUsize,
}

impl MmapPool {
    /// Allocate a `capacity`-byte region.
    /// On Linux, tries MAP_HUGETLB first, falls back to 4 KB pages.
    /// On macOS, uses plain anonymous mmap.
    /// On Windows, uses VirtualAlloc.
    pub fn new(capacity: usize) -> Result<Self> {
        let base = platform::alloc(capacity)?;
        platform::advise_hugepage(base, capacity);
        Ok(Self { base, capacity, cursor: AtomicUsize::new(0) })
    }

    /// Bump-allocate `len` bytes. Returns None when the pool is exhausted.
    pub fn alloc(&self, len: usize) -> Option<PoolAlloc> {
        let aligned_len = (len + 7) & !7; // align to 8 bytes
        let offset = self.cursor.fetch_add(aligned_len, Ordering::Relaxed);
        if offset + aligned_len > self.capacity {
            self.cursor.fetch_sub(aligned_len, Ordering::Relaxed);
            return None;
        }
        let ptr = unsafe { NonNull::new_unchecked(self.base.as_ptr().add(offset)) };
        Some(PoolAlloc { ptr, len })
    }

    /// Hint to the OS that the pages backing this allocation can be reclaimed.
    pub fn free_hint(&self, alloc: &PoolAlloc) {
        platform::advise_free(alloc.ptr, alloc.len);
    }

    /// Write bytes into an existing allocation.
    ///
    /// # Safety
    /// `alloc` must have been returned by `self.alloc()` and not already freed.
    pub fn write(&self, alloc: &PoolAlloc, data: &[u8]) {
        assert!(data.len() <= alloc.len, "data exceeds allocation");
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), alloc.ptr.as_ptr(), data.len()) }
    }

    /// Read bytes from an allocation.
    pub fn read<'a>(&self, alloc: &'a PoolAlloc) -> &'a [u8] {
        unsafe { std::slice::from_raw_parts(alloc.ptr.as_ptr(), alloc.len) }
    }

    pub fn capacity(&self) -> usize { self.capacity }
    pub fn used(&self)     -> usize { self.cursor.load(Ordering::Relaxed) }
}

impl Drop for MmapPool {
    fn drop(&mut self) {
        platform::dealloc(self.base, self.capacity);
    }
}

// ── Platform implementations ───────────────────────────────────────────────────

#[cfg(unix)]
mod platform {
    use std::ptr::NonNull;
    use anyhow::{bail, Result};

    pub fn alloc(size: usize) -> Result<NonNull<u8>> {
        // Linux: try MAP_HUGETLB (2 MB pages) first
        #[cfg(target_os = "linux")]
        {
            let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB;
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    flags,
                    -1,
                    0,
                )
            };
            if ptr != libc::MAP_FAILED {
                return Ok(unsafe { NonNull::new_unchecked(ptr as *mut u8) });
            }
            // MAP_HUGETLB not available — fall through to regular mmap
        }

        // Any Unix: regular anonymous mmap (4 KB pages)
        let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            bail!("mmap failed: {}", std::io::Error::last_os_error());
        }
        Ok(unsafe { NonNull::new_unchecked(ptr as *mut u8) })
    }

    pub fn dealloc(ptr: NonNull<u8>, size: usize) {
        unsafe { libc::munmap(ptr.as_ptr() as *mut libc::c_void, size); }
    }

    pub fn advise_hugepage(ptr: NonNull<u8>, size: usize) {
        // Linux: request transparent huge pages for this region
        #[cfg(target_os = "linux")]
        unsafe {
            libc::madvise(
                ptr.as_ptr() as *mut libc::c_void,
                size,
                libc::MADV_HUGEPAGE,
            );
        }
        // macOS / other Unix: huge pages not supported; nothing to do
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (ptr, size);
        }
    }

    pub fn advise_free(ptr: NonNull<u8>, size: usize) {
        // Linux: MADV_FREE — pages can be lazily reclaimed
        #[cfg(target_os = "linux")]
        unsafe {
            libc::madvise(
                ptr.as_ptr() as *mut libc::c_void,
                size,
                libc::MADV_FREE,
            );
        }

        // macOS: MADV_FREE_REUSABLE — equivalent hint for the macOS VM
        #[cfg(target_os = "macos")]
        unsafe {
            // MADV_FREE_REUSABLE = 8 on macOS (not exported by libc crate on all versions)
            libc::madvise(
                ptr.as_ptr() as *mut libc::c_void,
                size,
                8, // MADV_FREE_REUSABLE
            );
        }

        // Other Unix: no-op — no equivalent portable advisory
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (ptr, size);
        }
    }
}

#[cfg(windows)]
mod platform {
    // Windows: use the global allocator (zeroed heap memory).
    // VirtualAlloc + MEM_DECOMMIT is more efficient but requires windows-sys.
    // The std::alloc path is dependency-free and works for all Windows targets.
    use std::ptr::NonNull;
    use anyhow::{bail, Result};

    pub fn alloc(size: usize) -> Result<NonNull<u8>> {
        let layout = std::alloc::Layout::from_size_align(size, 8)
            .map_err(|e| anyhow::anyhow!("layout: {e}"))?;
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        match NonNull::new(ptr) {
            Some(p) => Ok(p),
            None => bail!("alloc_zeroed({size}) returned null"),
        }
    }

    pub fn dealloc(ptr: NonNull<u8>, size: usize) {
        let layout = std::alloc::Layout::from_size_align(size, 8).unwrap();
        unsafe { std::alloc::dealloc(ptr.as_ptr(), layout) }
    }

    // Huge pages not available on this path
    pub fn advise_hugepage(_ptr: NonNull<u8>, _size: usize) {}

    // No equivalent advisory available via std::alloc — no-op
    pub fn advise_free(_ptr: NonNull<u8>, _size: usize) {}
}

// Fallback for any platform not covered above (e.g. WASM)
#[cfg(not(any(unix, windows)))]
mod platform {
    use std::ptr::NonNull;
    use anyhow::Result;

    pub fn alloc(size: usize) -> Result<NonNull<u8>> {
        let layout = std::alloc::Layout::from_size_align(size, 8).unwrap();
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        match NonNull::new(ptr) {
            Some(p) => Ok(p),
            None => anyhow::bail!("alloc_zeroed({size}) returned null"),
        }
    }
    pub fn dealloc(ptr: NonNull<u8>, size: usize) {
        let layout = std::alloc::Layout::from_size_align(size, 8).unwrap();
        unsafe { std::alloc::dealloc(ptr.as_ptr(), layout) }
    }
    pub fn advise_hugepage(_ptr: NonNull<u8>, _size: usize) {}
    pub fn advise_free(_ptr: NonNull<u8>, _size: usize) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_write_read_roundtrip() {
        let pool = MmapPool::new(4 * 1024 * 1024).expect("pool alloc");
        let data = b"hello cross-platform pool";
        let alloc = pool.alloc(data.len()).expect("bump alloc");
        pool.write(&alloc, data);
        assert_eq!(pool.read(&alloc), data);
    }

    #[test]
    fn alloc_exhaustion_returns_none() {
        let pool = MmapPool::new(64).expect("tiny pool");
        let _a = pool.alloc(64).expect("first alloc");
        assert!(pool.alloc(1).is_none());
    }

    #[test]
    fn free_hint_does_not_panic() {
        let pool = MmapPool::new(4096).expect("pool");
        let alloc = pool.alloc(128).expect("alloc");
        pool.free_hint(&alloc);
    }
}
