fn main() {
    let os   = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    // ── Platform-specific cfg flags ──────────────────────────────────────────
    // mneme_linux  — Linux kernel APIs: io_uring, perf_event_open, O_DIRECT,
    //                fallocate, MAP_HUGETLB, MADV_HUGEPAGE, MADV_FREE.
    // mneme_unix   — POSIX APIs: pwrite, mmap (anonymous), fdatasync / F_FULLFSYNC.
    // mneme_macos  — macOS-specific: F_NOCACHE, F_FULLFSYNC, MADV_FREE_REUSABLE.
    // mneme_windows— Windows-specific: VirtualAlloc, FlushFileBuffers.

    if os == "linux" {
        println!("cargo:rustc-cfg=mneme_linux");
        println!("cargo:rustc-cfg=mneme_unix");
    } else if os == "macos" {
        println!("cargo:rustc-cfg=mneme_macos");
        println!("cargo:rustc-cfg=mneme_unix");
        println!(
            "cargo:warning=MnemeCache on macOS: io_uring and perf_event_open \
             are unavailable. WAL uses F_NOCACHE+F_FULLFSYNC; hardware metrics \
             are disabled. All other features work natively."
        );
    } else if os == "windows" {
        println!("cargo:rustc-cfg=mneme_windows");
        println!(
            "cargo:warning=MnemeCache on Windows: io_uring, perf_event_open, \
             O_DIRECT and mmap huge-pages are unavailable. WAL uses standard \
             Win32 I/O; pool uses VirtualAlloc. All other features work natively."
        );
    } else if !os.is_empty() {
        // Unknown POSIX-ish target (FreeBSD, illumos, …)
        println!("cargo:rustc-cfg=mneme_unix");
        println!(
            "cargo:warning=MnemeCache: unknown target OS '{os}'. \
             Compiling with generic POSIX I/O. Linux-only features \
             (io_uring, perf counters, huge pages) are disabled."
        );
    }

    // Emit the CPU architecture for future SIMD / cache-line tuning
    if arch == "x86_64" {
        println!("cargo:rustc-cfg=mneme_x86_64");
    } else if arch == "aarch64" {
        println!("cargo:rustc-cfg=mneme_aarch64");
    }

    println!("cargo:rerun-if-changed=build.rs");
}
