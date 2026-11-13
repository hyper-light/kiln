//! macOS's reads for [`crate::sense`]: each CPU's ticks (`host_processor_info(PROCESSOR_CPU_LOAD_INFO)`,
//! [`cpu_loads`]), memory (`host_statistics64(HOST_VM_INFO64)`'s pages, the room in them
//! [`kiln_ci::facts::VmPages::room`]), the work volume's free space (`statfs`) and what APFS would
//! make available for important use (Core Foundation's
//! `kCFURLVolumeAvailableCapacityForImportantUsageKey`), and the memory pressure level. Nothing
//! privileged.

#![allow(unsafe_code)]

use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use crate::sense::{CpuTicks, Raw, Space};

type MachPort = u32;
type KernReturn = i32;
/// `CPU_STATE_MAX`: user, system, idle, nice.
const CPU_STATES: usize = 4;
const CPU_STATE_IDLE: usize = 2;

unsafe extern "C" {
    fn mach_host_self() -> MachPort;
    /// `mach/mach_host.h`: the size of the pages the host counts, as `vm_stat` reads it.
    fn host_page_size(host: MachPort, size: *mut usize) -> KernReturn;
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

/// The array of each CPU's ticks `host_processor_info` maps into this task, given back as it drops.
struct Loads {
    info: nix::libc::processor_info_array_t,
    count: nix::libc::mach_msg_type_number_t,
}

impl Drop for Loads {
    fn drop(&mut self) {
        let size = usize::try_from(self.count).map_or(0, |n| n.saturating_mul(std::mem::size_of::<nix::libc::integer_t>()));
        // SAFETY: the region the kernel mapped into this task for the call's answer, `count`
        // integers long, given back once.
        unsafe {
            nix::libc::vm_deallocate(mach_task_self_, self.info.addr(), size);
        }
    }
}

/// Each CPU's ticks into `into`, one entry a CPU: user, system, idle and nice, each a 32-bit count
/// of 10 ms ticks that wraps (in about 497 days), as `host_processor_info(PROCESSOR_CPU_LOAD_INFO)`
/// gives them (`processor_cpu_load_info`, osfmk/kern/processor.c). None when they can't be read.
///
/// Not their sum, `host_statistics(HOST_CPU_LOAD_INFO)`: XNU answers that call, and
/// `host_statistics64`, from one copy all processes but Apple's share, fresh for the first 2 to 10
/// calls of each second and that copy for the rest of it (`rate_limit_host_statistics`,
/// osfmk/kern/host.c, xnu-12377.121.6). On a Mac where something else read it too, both reads of an
/// interval had that copy, so the interval had no ticks and no idle share (a measure over 44 ms,
/// kiln runs r407 and r600). `host_processor_info` reads the CPUs each call.
fn cpu_loads(host: MachPort, into: &mut Vec<[u32; CPU_STATES]>) -> Option<()> {
    let (mut cpus, mut info, mut count): (nix::libc::natural_t, nix::libc::processor_info_array_t, nix::libc::mach_msg_type_number_t) = (0, std::ptr::null_mut(), 0);
    // SAFETY: out pointers to three live locals; on success the kernel maps `count` integers at
    // `info` into this task, given back as `loads` drops.
    let r = unsafe { nix::libc::host_processor_info(host, nix::libc::PROCESSOR_CPU_LOAD_INFO, &raw mut cpus, &raw mut info, &raw mut count) };
    if r != 0 || info.is_null() {
        return None;
    }
    let loads = Loads { info, count };
    // SAFETY: the `count` integers the kernel mapped at `info`, mapped until `loads` drops.
    let ints = unsafe { std::slice::from_raw_parts(loads.info.cast_const(), usize::try_from(loads.count).ok()?) };
    into.clear();
    // Each `processor_cpu_load_info`'s `natural_t`s, in an array the call types `integer_t`.
    into.extend(ints.as_chunks::<CPU_STATES>().0.iter().map(|cpu| cpu.map(i32::cast_unsigned)));
    (into.len() == usize::try_from(cpus).ok()?).then_some(())
}

/// The host's pages as `host_statistics64(HOST_VM_INFO64)` counts them, and the size of each
/// (`host_page_size`): what its room for new work is read from ([`kiln_ci::facts::VmPages::room`]).
/// None when either can't be read.
fn pages_of(host: MachPort) -> Option<(kiln_ci::facts::VmPages, u64)> {
    let mut page: usize = 0;
    // SAFETY: an out pointer to one live `vm_size_t`.
    let sized = unsafe { host_page_size(host, &raw mut page) } == 0;
    let page = u64::try_from(page).ok().filter(|p| sized && *p > 0)?;
    // SAFETY: plain integers, for which zero is a value.
    let mut v: nix::libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = nix::libc::HOST_VM_INFO64_COUNT;
    // SAFETY: `v` is a whole `vm_statistics64` and `count` its length in `integer_t`s, as
    // `HOST_VM_INFO64_COUNT` is; the kernel writes no more than that.
    let r = unsafe { nix::libc::host_statistics64(host, nix::libc::HOST_VM_INFO64, (&raw mut v).cast(), &raw mut count) };
    let pages = kiln_ci::facts::VmPages { free: u64::from(v.free_count), active: u64::from(v.active_count), inactive: u64::from(v.inactive_count), compressor: u64::from(v.compressor_page_count), swapins: v.swapins };
    (r == 0).then_some((pages, page))
}

/// This host's pages and their size now ([`pages_of`]): what its facts ([`kiln_ci::facts::Mem`])
/// and its headroom's guard read its room for new work from, as its sample does, and its pace its
/// swap-ins (`crate::os::pace`).
pub(crate) fn memory_pages() -> Option<(kiln_ci::facts::VmPages, u64)> {
    // SAFETY: no arguments; it returns a send right to this host, released below.
    let host = unsafe { mach_host_self() };
    let pages = pages_of(host);
    // SAFETY: the send right taken above, released once.
    unsafe {
        mach_port_deallocate(mach_task_self_, host);
    }
    pages
}

pub struct Reader {
    work_dir: PathBuf,
    host: MachPort,
    /// The machine's memory (`hw.memsize`), read once.
    mem_total: u64,
    /// Each CPU's ticks as last read (none before the first read), the buffer the next read fills,
    /// and their sum since this reader began, widened to 64 bits.
    last: Vec<[u32; CPU_STATES]>,
    now: Vec<[u32; CPU_STATES]>,
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
        let mut sys = sysinfo::System::new();
        sys.refresh_memory_specifics(sysinfo::MemoryRefreshKind::nothing().with_ram());
        Reader { work_dir, host, mem_total: sys.total_memory(), last: Vec::new(), now: Vec::new(), wide: CpuTicks::default(), device }
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// The ticks since this reader began, all CPUs together, each CPU's and state's 32-bit
    /// counter's wrap undone. A read whose CPUs aren't the last read's (the first; CPUs that came
    /// or went) adds no interval: a CPU's ticks count from its own start.
    fn cpu(&mut self) -> Option<CpuTicks> {
        cpu_loads(self.host, &mut self.now)?;
        if self.last.len() == self.now.len() {
            for (now, last) in self.now.iter().zip(&self.last) {
                for (state, (n, l)) in now.iter().zip(last).enumerate() {
                    let d = u64::from(n.wrapping_sub(*l));
                    self.wide.total = self.wide.total.saturating_add(d);
                    if state == CPU_STATE_IDLE {
                        self.wide.idle = self.wide.idle.saturating_add(d);
                    }
                }
            }
        }
        std::mem::swap(&mut self.last, &mut self.now);
        Some(self.wide)
    }

    pub fn read(&mut self) -> Raw {
        let space = crate::os::disk_space(&self.work_dir).map(|(total, free)| Space { total, free, important: important(&self.work_dir).map(|i| i.max(free)) });
        Raw {
            cpu: self.cpu(),
            cpus: u32::try_from(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)).unwrap_or(1),
            mem_total: self.mem_total,
            // No room said while the host's pages can't be read: none is placed on a guess.
            mem_available: pages_of(self.host).map_or(0, |(pages, page)| pages.room(page)),
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Duration;

    /// XNU's tick as `kern.clockrate` says it (`struct clockinfo`'s `tick`, in µs; `hz` is 100,
    /// bsd/kern/kern_clock.c): the unit each CPU's time is counted in, each state's time in whole
    /// ticks (`processor_cpu_load_info`, osfmk/kern/processor.c).
    fn tick() -> anyhow::Result<Duration> {
        #[repr(C)]
        #[derive(Default)]
        struct ClockInfo {
            hz: i32,
            tick: i32,
            tickadj: i32,
            stathz: i32,
            profhz: i32,
        }
        let mut v = ClockInfo::default();
        let mut len = std::mem::size_of::<ClockInfo>();
        // SAFETY: `v` is `len` bytes, written by the kernel up to that.
        let rc = unsafe { nix::libc::sysctlbyname(c"kern.clockrate".as_ptr(), (&raw mut v).cast(), &raw mut len, std::ptr::null_mut(), 0) };
        anyhow::ensure!(rc == 0 && len == std::mem::size_of::<ClockInfo>() && v.hz > 0 && v.tick > 0, "kern.clockrate: {rc}, {len} bytes, hz {}, tick {} µs", v.hz, v.tick);
        Ok(Duration::from_micros(u64::try_from(v.tick)?))
    }

    /// Stops the other reader as it drops, however the test leaves its scope.
    struct Stop<'a>(&'a AtomicBool);

    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    /// W05: a measure's idle share while another program reads the host's CPU ticks through
    /// `host_statistics(HOST_CPU_LOAD_INFO)`, as Macs' monitors and other kiln processes do: what
    /// `sense::tests::w05_this_machine_measures_itself` met beside other tests on a loaded Mac (its
    /// idle share none over 44 ms; kiln runs r407 and r600, passing alone). XNU answers that call
    /// from one copy every process that isn't Apple's shares, fresh for the first 2 to 10 calls of
    /// each second and that copy after (`rate_limit_host_statistics`, osfmk/kern/host.c,
    /// xnu-12377.121.6): two reads that both come after a window's fresh calls read one copy, so
    /// their interval has no ticks at all.
    #[test]
    fn w05_cpu_idle_holds_while_others_read_the_hosts_ticks() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // Each CPU's user, system and idle times are counted in whole ticks apart: three ticks on,
        // a CPU's count has moved.
        let span = tick()? * 3;
        let (stop, calls, changes) = (AtomicBool::new(false), AtomicU64::new(0), AtomicU64::new(0));
        let measures = std::thread::scope(|s| -> anyhow::Result<Vec<crate::sense::Measure>> {
            let _stop = Stop(&stop);
            std::thread::Builder::new().name("other-reader".into()).spawn_scoped(s, || {
                // SAFETY: no arguments; a send right to this host, released below.
                let host = unsafe { super::mach_host_self() };
                let mut last = None;
                while !stop.load(Ordering::Relaxed) {
                    let mut ticks = [0i32; super::CPU_STATES];
                    let mut count = nix::libc::HOST_CPU_LOAD_INFO_COUNT;
                    // SAFETY: `host_cpu_load_info_data_t`'s four integers, and their count.
                    let r = unsafe { nix::libc::host_statistics(host, nix::libc::HOST_CPU_LOAD_INFO, ticks.as_mut_ptr(), &raw mut count) };
                    if r == 0 && last.replace(ticks) != Some(ticks) {
                        changes.fetch_add(1, Ordering::Relaxed);
                    }
                    calls.fetch_add(1, Ordering::Relaxed);
                }
                // SAFETY: the send right taken above, released once.
                unsafe {
                    super::mach_port_deallocate(super::mach_task_self_, host);
                }
            })?;
            // Past the most calls one of XNU's windows answers fresh (`HOST_STATISTICS_MAX_REQUESTS`).
            while calls.load(Ordering::Relaxed) <= 10 {
                std::thread::yield_now();
            }
            let mut sensor = crate::sense::Sensor::new(dir.path());
            // Two measures, within one of XNU's one-second windows or across one boundary at most:
            // at least one of them lies wholly in a window.
            let mut out = Vec::new();
            for _ in 0..2 {
                std::thread::sleep(span);
                out.push(sensor.measure());
            }
            Ok(out)
        })?;
        eprintln!("sense: the other reader called {} times and saw the ticks change {} times; each measure {span:?} or more", calls.into_inner(), changes.into_inner());
        for m in &measures {
            assert!(m.cpu_idle.is_some_and(|i| (0.0..=1.0).contains(&i)), "{m:?}");
        }
        // What each CPU's ticks cost a read here, said for the record (no bound asserted, as the
        // sensor's own costs aren't: `sense::tests::w05_this_machine_measures_itself`).
        let (reads, mut loads) = (200u32, Vec::new());
        // SAFETY: no arguments; a send right to this host, released below.
        let host = unsafe { super::mach_host_self() };
        let t = std::time::Instant::now();
        for _ in 0..reads {
            std::hint::black_box(super::cpu_loads(host, &mut loads));
        }
        let took = t.elapsed();
        // SAFETY: the send right taken above, released once.
        unsafe {
            super::mach_port_deallocate(super::mach_task_self_, host);
        }
        eprintln!("sense: each CPU's ticks ({} CPUs) {:.1} µs a read over {reads} reads", loads.len(), took.as_secs_f64() * 1e6 / f64::from(reads));
        Ok(())
    }
}
