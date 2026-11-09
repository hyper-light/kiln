//! macOS's reads for [`crate::sense`]: the CPUs' ticks (`host_statistics(HOST_CPU_LOAD_INFO)`),
//! memory (`sysinfo`'s `host_statistics64`), the work volume's free space (`statfs`) and what
//! APFS would make available for important use (Core Foundation's
//! `kCFURLVolumeAvailableCapacityForImportantUsageKey`), and the memory pressure level. Nothing
//! privileged.

#![allow(unsafe_code)]

use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use crate::sense::{CpuTicks, Raw, Space};

type MachPort = u32;
type KernReturn = i32;
/// `HOST_CPU_LOAD_INFO` (`mach/host_info.h`).
const HOST_CPU_LOAD_INFO: i32 = 3;
/// `CPU_STATE_MAX`: user, system, idle, nice.
const CPU_STATES: usize = 4;
const CPU_STATE_IDLE: usize = 2;

unsafe extern "C" {
    fn mach_host_self() -> MachPort;
    fn host_statistics(host: MachPort, flavor: i32, info: *mut i32, count: *mut u32) -> KernReturn;
    fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
    static mach_task_self_: MachPort;
}

pub(crate) mod cf {
    //! The Core Foundation calls kiln's reads need, declared by hand.
    use std::ffi::c_void;

    pub type CFTypeRef = *const c_void;
    pub type CFStringRef = *const c_void;
    pub type CFIndex = isize;
    /// `kCFNumberSInt64Type`.
    pub const NUMBER_SINT64: CFIndex = 4;
    /// `kCFStringEncodingUTF8`.
    pub const UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub static kCFURLVolumeAvailableCapacityForImportantUsageKey: CFStringRef;
        pub fn CFURLCreateFromFileSystemRepresentation(alloc: CFTypeRef, buffer: *const u8, len: CFIndex, is_dir: u8) -> CFTypeRef;
        pub fn CFURLCopyResourcePropertyForKey(url: CFTypeRef, key: CFStringRef, value: *mut CFTypeRef, error: *mut CFTypeRef) -> u8;
        pub fn CFNumberGetValue(number: CFTypeRef, kind: CFIndex, value: *mut c_void) -> u8;
        pub fn CFGetTypeID(cf: CFTypeRef) -> usize;
        pub fn CFNumberGetTypeID() -> usize;
        pub fn CFStringGetTypeID() -> usize;
        pub fn CFDictionaryGetTypeID() -> usize;
        pub fn CFBooleanGetTypeID() -> usize;
        pub fn CFBooleanGetValue(b: CFTypeRef) -> u8;
        pub fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const std::ffi::c_char, encoding: u32) -> CFStringRef;
        pub fn CFStringGetCString(s: CFStringRef, buf: *mut std::ffi::c_char, len: CFIndex, encoding: u32) -> u8;
        pub fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
        pub fn CFRelease(cf: CFTypeRef);
    }

    /// An owned Core Foundation object, released when dropped.
    pub struct Owned(pub CFTypeRef);

    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: an object this holds the one reference to (a Create or Copy call's).
                unsafe { CFRelease(self.0) }
            }
        }
    }

    /// A CFString of `s`.
    pub fn string(s: &std::ffi::CStr) -> Owned {
        // SAFETY: a NUL-terminated string; the default allocator.
        Owned(unsafe { CFStringCreateWithCString(std::ptr::null(), s.as_ptr(), UTF8) })
    }

    /// A CFNumber's value as an i64, or a CFBoolean's as 0 or 1; none for anything else.
    pub fn number(v: CFTypeRef) -> Option<i64> {
        if v.is_null() {
            return None;
        }
        // SAFETY: `v` is a live CF object (the caller's borrow); its type is checked before use.
        unsafe {
            if CFGetTypeID(v) == CFBooleanGetTypeID() {
                return Some(i64::from(CFBooleanGetValue(v)));
            }
            if CFGetTypeID(v) != CFNumberGetTypeID() {
                return None;
            }
            let mut out: i64 = 0;
            (CFNumberGetValue(v, NUMBER_SINT64, (&raw mut out).cast::<c_void>()) != 0).then_some(out)
        }
    }

    /// A CFString's text; none for anything else.
    pub fn text(v: CFTypeRef) -> Option<String> {
        if v.is_null() {
            return None;
        }
        let mut buf = [0 as std::ffi::c_char; 256];
        // SAFETY: a live CF object, its type checked; the buffer's length is passed.
        let ok = unsafe { CFGetTypeID(v) == CFStringGetTypeID() && CFStringGetCString(v, buf.as_mut_ptr(), CFIndex::try_from(buf.len()).unwrap_or(0), UTF8) != 0 };
        // SAFETY: CFStringGetCString wrote a NUL-terminated string into `buf` when it succeeded.
        ok.then(|| unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
    }

    /// A dictionary's value for `key` (a borrowed reference, valid while the dictionary is).
    pub fn get(dict: CFTypeRef, key: &std::ffi::CStr) -> CFTypeRef {
        if dict.is_null() {
            return std::ptr::null();
        }
        let k = string(key);
        // SAFETY: a live object, checked to be a dictionary; the key a live CFString.
        unsafe { if CFGetTypeID(dict) == CFDictionaryGetTypeID() && !k.0.is_null() { CFDictionaryGetValue(dict, k.0) } else { std::ptr::null() } }
    }
}

/// What the volume holding `dir` can make available for important use: its free space and the
/// purgeable space the system would free for it (APFS). None where the volume doesn't say.
pub(crate) fn important(dir: &Path) -> Option<u64> {
    let bytes = dir.as_os_str().as_bytes();
    // SAFETY: the path's bytes and their length; the default allocator. The URL is released when
    // `url` drops.
    let url = cf::Owned(unsafe { cf::CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), cf::CFIndex::try_from(bytes.len()).ok()?, 1) });
    if url.0.is_null() {
        return None;
    }
    let mut value: cf::CFTypeRef = std::ptr::null();
    // SAFETY: a live URL, the framework's own key, an out pointer for the value (a +1 reference,
    // released below) and none for the error.
    let ok = unsafe { cf::CFURLCopyResourcePropertyForKey(url.0, cf::kCFURLVolumeAvailableCapacityForImportantUsageKey, &raw mut value, std::ptr::null_mut()) };
    let value = cf::Owned(value);
    if ok == 0 {
        return None;
    }
    cf::number(value.0).and_then(|n| u64::try_from(n).ok())
}

/// The host's CPU ticks, all CPUs together: user, system, idle, nice, each a 32-bit count that
/// wraps (at 100 Hz, a 16-CPU machine wraps it in about 31 days).
fn cpu_load(host: MachPort) -> Option<[u32; CPU_STATES]> {
    let mut ticks = [0u32; CPU_STATES];
    let mut count = u32::try_from(CPU_STATES).ok()?;
    // SAFETY: `ticks` is `host_cpu_load_info_data_t` (four `natural_t`), `count` its length in
    // `integer_t`s, as `HOST_CPU_LOAD_INFO_COUNT` is.
    let r = unsafe { host_statistics(host, HOST_CPU_LOAD_INFO, ticks.as_mut_ptr().cast::<i32>(), &raw mut count) };
    (r == 0 && usize::try_from(count).ok() == Some(CPU_STATES)).then_some(ticks)
}

pub struct Reader {
    work_dir: PathBuf,
    host: MachPort,
    sys: sysinfo::System,
    /// The ticks as last read, and their sum since this reader began, widened to 64 bits.
    last: Option<[u32; CPU_STATES]>,
    wide: CpuTicks,
    device: Option<super::sense_disk_macos::Device>,
}

impl Drop for Reader {
    fn drop(&mut self) {
        // SAFETY: the send right `mach_host_self` gave this reader, released once.
        unsafe {
            mach_port_deallocate(mach_task_self_, self.host);
        }
    }
}

impl Reader {
    pub fn new(work_dir: &Path) -> Reader {
        let work_dir = crate::sense::canonical(work_dir);
        // SAFETY: no arguments; it returns a send right to this host, released in `drop`.
        let host = unsafe { mach_host_self() };
        let device = super::sense_disk_macos::Device::of(&work_dir);
        Reader { work_dir, host, sys: sysinfo::System::new(), last: None, wide: CpuTicks::default(), device }
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// The ticks since this reader began, each state's 32-bit counter's wrap undone.
    fn cpu(&mut self) -> Option<CpuTicks> {
        let now = cpu_load(self.host)?;
        if let Some(last) = self.last {
            let d: Vec<u64> = now.iter().zip(last.iter()).map(|(n, l)| u64::from(n.wrapping_sub(*l))).collect();
            self.wide.total += d.iter().sum::<u64>();
            self.wide.idle += d.get(CPU_STATE_IDLE).copied().unwrap_or(0);
        }
        self.last = Some(now);
        Some(self.wide)
    }

    pub fn read(&mut self) -> Raw {
        self.sys.refresh_memory();
        let space = crate::os::disk_space(&self.work_dir).map(|(total, free)| Space { total, free, important: important(&self.work_dir).map(|i| i.max(free)) });
        Raw {
            cpu: self.cpu(),
            cpus: u32::try_from(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)).unwrap_or(1),
            mem_total: self.sys.total_memory(),
            mem_available: self.sys.available_memory(),
            space,
            memory_level: super::metrics::memory_level(),
            device: self.device.as_mut().and_then(super::sense_disk_macos::Device::counters),
            ..Raw::default()
        }
    }
}

/// The work directory's storage facts (docs/needs.md, W13-R01).
pub fn storage(work_dir: &Path) -> Option<kiln_ci::facts::Storage> {
    super::sense_disk_macos::facts(&crate::sense::canonical(work_dir))
}
