//! The machine's physical memory as the platform reports it, for Amphora's process memory budget
//! (crates/amphora/src/memory.rs; docs/design/store.md, "Decision (lane constants): one process
//! memory budget"; MB.1). Amphora's own crate denies `unsafe`, so the two platform calls that need
//! it live here, each behind a safe function that answers `None` where the platform doesn't say.
//! Linux is read from `/proc/meminfo` by Amphora itself and answers `None` here.

/// The machine's physical memory in bytes: macOS's `hw.memsize` (sysctl(3), "hw.memsize: the
/// number of bytes of physical memory"), Windows' `MEMORYSTATUSEX::ullTotalPhys`
/// (`GlobalMemoryStatusEx`, Microsoft's Win32 documentation). `None` elsewhere or when the call fails.
#[cfg(target_os = "macos")]
#[must_use]
pub fn physical_memory() -> Option<u64> {
    let mut bytes: u64 = 0;
    let mut len: libc::size_t = std::mem::size_of::<u64>();
    // SAFETY: the name is a NUL-terminated C string; `oldp` points to a live `u64` and `oldlenp`
    // to its size, which the kernel writes at most; no new value is set (`newp` null, `newlen` 0).
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut bytes).cast::<libc::c_void>(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == std::mem::size_of::<u64>() && bytes > 0).then_some(bytes)
}

/// See the macOS reading above.
#[cfg(target_os = "windows")]
#[must_use]
pub fn physical_memory() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: MEMORYSTATUSEX is plain data (integers only), for which all-zero bytes are a value.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>()).ok()?;
    // SAFETY: `status` is a live MEMORYSTATUSEX whose `dwLength` is set, as the call requires.
    let ok = unsafe { GlobalMemoryStatusEx(&raw mut status) };
    (ok != 0 && status.ullTotalPhys > 0).then_some(status.ullTotalPhys)
}

/// Neither macOS nor Windows: this reading has nothing to say (Linux reads `/proc/meminfo`).
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[must_use]
pub fn physical_memory() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    /// Do: read the machine's memory where the platform says it. Expect: some bytes on macOS and
    /// Windows, none elsewhere (MB.1: the budget learned where it runs).
    #[test]
    fn the_platform_says_its_physical_memory() {
        let got = super::physical_memory();
        if cfg!(any(target_os = "macos", target_os = "windows")) {
            assert!(got.is_some_and(|bytes| bytes > 0));
        } else {
            assert_eq!(got, None);
        }
    }
}
