//! A shard thread's run time, read with no system call (kiln's goal 9, plan 01a1a87b2cec step 3): what tells a poll the
//! host held (the thread runnable and off the CPU) from one the task ran long. `attribution`'s readings of the thread's
//! account cost a call each (`CLOCK_THREAD_CPUTIME_ID` 155 ns, `getrusage` 130 ns), too dear for every poll, so it
//! reads only around long polls it expects, and on a shared machine left 96% of them unattributed (kiln's lab, g9p6).
//!
//! Linux keeps a self-monitoring perf event's times on a page the thread maps ([B: perf_event_open(2), "The mmap
//! layout"]): for an event of the calling thread alone, its enabled time runs only while the thread runs, and the
//! page carries the scale from the CPU's counter (the TSC; the generic timer on arm64) to the kernel's clock, so the
//! thread reads its run time now as the page's enabled time plus the counter's time since the kernel last wrote the
//! page, under the page's sequence count, in a few loads and one counter read. The event is a software one,
//! `PERF_COUNT_SW_TASK_CLOCK`, counting nothing in the kernel (`exclude_kernel`), which `perf_event_paranoid` 2 lets a
//! process open for its own threads; where it can't be opened (a container whose seccomp refuses the call, paranoid
//! 3), where the page offers no user time, and off Linux, there is none, and the shard keeps its attribution as it
//! was.
//!
//! A clock is read by the thread that opened it only: another reading it would add time its thread didn't run.
#![allow(unsafe_code)]

/// A thread's run-time clock: its event's page, mapped.
#[derive(Debug)]
pub(crate) struct TaskClock {
    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"), not(miri)))]
    page: linux::Page,
}

impl TaskClock {
    /// The calling thread's clock, where it can be had.
    pub(crate) fn open() -> Option<TaskClock> {
        #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"), not(miri)))]
        {
            let page = linux::Page::open()?;
            let clock = TaskClock { page };
            // A page that gives no user time is no clock.
            clock.run_ns()?;
            Some(clock)
        }
        #[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"), not(miri))))]
        {
            None
        }
    }

    /// The thread's run time since its clock was opened, nanoseconds: `None` where the page stops offering it.
    #[inline]
    pub(crate) fn run_ns(&self) -> Option<u64> {
        #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"), not(miri)))]
        {
            self.page.read().and_then(Times::run_ns)
        }
        #[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"), not(miri))))]
        {
            None
        }
    }
}

/// One consistent reading of the page's times, and the counter as it was read: read where the page is (Linux), and
/// worked out by the tests everywhere.
#[cfg(any(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"), not(miri)), test))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Times {
    /// `time_enabled`: the event's enabled time when the kernel last wrote the page, nanoseconds.
    pub(crate) enabled: u64,
    /// `capabilities`.
    pub(crate) caps: u64,
    /// `time_shift`, `time_mult`, `time_offset`: the counter's scale to nanoseconds, and where it starts.
    pub(crate) shift: u16,
    pub(crate) mult: u32,
    pub(crate) offset: u64,
    /// `time_cycles`, `time_mask`: where a short counter wraps (`cap_user_time_short`).
    pub(crate) cycles: u64,
    pub(crate) mask: u64,
    /// The counter, read inside the sequence.
    pub(crate) counter: u64,
}

/// `cap_user_time`: the scale above may be used.
#[cfg(any(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"), not(miri)), test))]
const CAP_USER_TIME: u64 = 1 << 3;
/// `cap_user_time_short`: the counter is narrower than 64 bits, `time_cycles` and `time_mask` say how.
#[cfg(any(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"), not(miri)), test))]
const CAP_USER_TIME_SHORT: u64 = 1 << 5;

#[cfg(any(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"), not(miri)), test))]
impl Times {
    /// The run time this reading gives: the enabled time, and the counter's time since the page was written, as
    /// perf_event_open(2) works it out (`time_offset + quot * time_mult + ((rem * time_mult) >> time_shift)`).
    fn run_ns(self) -> Option<u64> {
        if self.caps & CAP_USER_TIME == 0 || self.shift >= 64 {
            return None;
        }
        let cyc = if self.caps & CAP_USER_TIME_SHORT != 0 {
            self.cycles.wrapping_add(self.counter.wrapping_sub(self.cycles) & self.mask)
        } else {
            self.counter
        };
        let mult = u64::from(self.mult);
        let quot = cyc >> self.shift;
        let rem = cyc & ((1u64 << self.shift).wrapping_sub(1));
        let delta = self.offset.wrapping_add(quot.wrapping_mul(mult)).wrapping_add(rem.wrapping_mul(mult) >> self.shift);
        Some(self.enabled.wrapping_add(delta))
    }
}

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"), not(miri)))]
mod linux {
    use std::sync::atomic::{Ordering, compiler_fence};

    use super::Times;

    /// `PERF_TYPE_SOFTWARE`, `PERF_COUNT_SW_TASK_CLOCK` (linux/perf_event.h).
    const TYPE_SOFTWARE: u32 = 1;
    const SW_TASK_CLOCK: u64 = 1;
    /// `exclude_kernel` and `exclude_hv`, bits 5 and 6 of the attribute's flags.
    const EXCLUDE_KERNEL_HV: u64 = (1 << 5) | (1 << 6);
    /// Format: how many times a reading is tried against a page the kernel is writing.
    const TRIES: usize = 8;

    /// `struct perf_event_attr` as its first version laid it out (`PERF_ATTR_SIZE_VER0`, 64 bytes): the kernel takes
    /// a shorter attribute as the newer one with the rest zero.
    #[repr(C)]
    struct Attr {
        kind: u32,
        size: u32,
        config: u64,
        sample_period: u64,
        sample_type: u64,
        read_format: u64,
        flags: u64,
        wakeup_events: u32,
        bp_type: u32,
        config1: u64,
    }

    /// The page's fields' offsets in `struct perf_event_mmap_page` (linux/perf_event.h).
    mod at {
        pub(super) const LOCK: usize = 8;
        pub(super) const TIME_ENABLED: usize = 24;
        pub(super) const CAPABILITIES: usize = 40;
        pub(super) const TIME_SHIFT: usize = 50;
        pub(super) const TIME_MULT: usize = 52;
        pub(super) const TIME_OFFSET: usize = 56;
        pub(super) const TIME_CYCLES: usize = 80;
        pub(super) const TIME_MASK: usize = 88;
    }

    /// The event's descriptor and its page, mapped read-only; both let go with it.
    #[derive(Debug)]
    pub(super) struct Page {
        /// Held for the event's life: closed as the page goes, after it's unmapped.
        _fd: std::os::fd::OwnedFd,
        /// The mapping's address, as a number: read by its own thread only (the module's notes).
        addr: usize,
        len: usize,
    }

    impl Page {
        pub(super) fn open() -> Option<Page> {
            use std::os::fd::FromRawFd as _;
            let attr = Attr {
                kind: TYPE_SOFTWARE,
                size: u32::try_from(size_of::<Attr>()).ok()?,
                config: SW_TASK_CLOCK,
                sample_period: 0,
                sample_type: 0,
                read_format: 0,
                flags: EXCLUDE_KERNEL_HV,
                wakeup_events: 0,
                bp_type: 0,
                config1: 0,
            };
            // SAFETY: `attr` is this frame's own, live for the call and laid out as the kernel's first version of the
            // attribute (its size said in it); pid 0 and cpu -1 name the calling thread on any CPU, -1 no group, and
            // no flags. The result is checked before it's used.
            let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, std::ptr::from_ref(&attr), 0, -1, -1, 0u64) };
            let fd = i32::try_from(fd).ok().filter(|fd| *fd >= 0)?;
            // SAFETY: the call returned a new descriptor of this process's, owned from here on alone.
            let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
            let len = rustix::param::page_size();
            // SAFETY: a read-only shared mapping of the event's first page, its control page, which the kernel keeps
            // for as long as it's mapped; the address is checked before it's used, and unmapped once, in `drop`.
            let addr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, std::os::fd::AsRawFd::as_raw_fd(&fd), 0) };
            if addr == libc::MAP_FAILED || addr.is_null() {
                return None;
            }
            Some(Page { _fd: fd, addr: addr.expose_provenance(), len })
        }

        /// The field at `offset`, as the kernel last wrote it.
        #[inline]
        fn field<T: Copy>(&self, offset: usize) -> T {
            let p = std::ptr::with_exposed_provenance::<u8>(self.addr).wrapping_add(offset).cast::<T>();
            // SAFETY: the page is mapped for as long as `self` lives, `offset` is one of the page's fields' (`at`),
            // naturally aligned for its type and within the page, and the kernel writes it while it's read, which a
            // volatile read of a plain integer allows: the sequence count tells a torn reading.
            unsafe { std::ptr::read_volatile(p) }
        }

        /// One consistent reading of the page's times, with the counter read inside it.
        #[inline]
        pub(super) fn read(&self) -> Option<Times> {
            for _ in 0..TRIES {
                let seq: u32 = self.field(at::LOCK);
                compiler_fence(Ordering::SeqCst);
                let times = Times {
                    enabled: self.field(at::TIME_ENABLED),
                    caps: self.field(at::CAPABILITIES),
                    shift: self.field(at::TIME_SHIFT),
                    mult: self.field(at::TIME_MULT),
                    offset: self.field(at::TIME_OFFSET),
                    cycles: self.field(at::TIME_CYCLES),
                    mask: self.field(at::TIME_MASK),
                    counter: counter(),
                };
                compiler_fence(Ordering::SeqCst);
                if self.field::<u32>(at::LOCK) == seq {
                    return Some(times);
                }
            }
            None
        }
    }

    impl Drop for Page {
        fn drop(&mut self) {
            // SAFETY: the mapping `open` made, unmapped once; nothing reads it after (it goes with `self`).
            let _ = unsafe { libc::munmap(std::ptr::with_exposed_provenance_mut::<libc::c_void>(self.addr), self.len) };
        }
    }

    /// The CPU's counter the page's scale is for: the TSC.
    #[cfg(target_arch = "x86_64")]
    #[inline]
    fn counter() -> u64 {
        // SAFETY: `rdtsc` reads the time-stamp counter, on every x86-64 CPU, and touches no memory.
        unsafe { core::arch::x86_64::_rdtsc() }
    }

    /// The CPU's counter the page's scale is for: the generic timer's virtual count.
    #[cfg(target_arch = "aarch64")]
    #[inline]
    fn counter() -> u64 {
        let v: u64;
        // SAFETY: `cntvct_el0` is readable at EL0 where the kernel offers user time (it sets `cap_user_time` only so),
        // and the read touches no memory.
        unsafe { core::arch::asm!("mrs {v}, cntvct_el0", v = out(reg) v, options(nomem, nostack, preserves_flags)) };
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 9, plan 01a1a87b2cec step 3: the page's arithmetic as perf_event_open(2) gives it: the enabled time and the
    /// counter's time since, by the scale; a short counter wrapped where `time_mask` says; none without user time.
    #[test]
    fn g9_s3_the_pages_time_is_worked_out_as_the_kernel_says() {
        // A scale of 1/2 (mult 1, shift 1), from an offset of -100 (the page written at count 200).
        let t = Times { enabled: 1_000, caps: CAP_USER_TIME, shift: 1, mult: 1, offset: 0u64.wrapping_sub(100), cycles: 0, mask: 0, counter: 300 };
        assert_eq!(t.run_ns(), Some(1_050), "150 counts at 1/2 less 100: 50 ns since the page was written");
        assert_eq!(Times { caps: 0, ..t }.run_ns(), None);
        assert_eq!(Times { shift: 64, ..t }.run_ns(), None);
        let short = Times { caps: CAP_USER_TIME | CAP_USER_TIME_SHORT, cycles: 0x100, mask: 0xff, counter: 0x2_0105, ..t };
        // The count past `time_cycles` taken within the mask: 0x105, so as `counter` 0x105 would read.
        assert_eq!(short.run_ns(), Times { counter: 0x105, ..t }.run_ns());
    }

    /// Goal 9, plan 01a1a87b2cec step 3: what a reading costs, said, a few loads and the counter's read against the
    /// attribution's 155 ns call; held only far from a call's cost, as a loaded machine slows any reading.
    #[test]
    fn g9_s3_a_reading_takes_no_call() {
        let Some(clock) = TaskClock::open() else { return };
        let reads: u32 = 1_000_000;
        let began = std::time::Instant::now();
        let mut sum = 0u64;
        for _ in 0..reads {
            sum = sum.wrapping_add(std::hint::black_box(clock.run_ns()).unwrap_or(0));
        }
        let each = began.elapsed() / reads;
        eprintln!("a task clock reading: {each:?} ({} readings summed to {sum})", reads);
        assert!(each < std::time::Duration::from_micros(10), "{each:?} a reading");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a thread's clock runs while it runs and stands while it sleeps, as the
    /// thread's CPU clock does; where it can't be opened (a container, a stricter paranoid level, off Linux) there's
    /// nothing to hold it to.
    #[test]
    fn g9_s3_a_threads_run_time_runs_while_it_runs_and_not_while_it_sleeps() {
        let Some(clock) = TaskClock::open() else {
            eprintln!("no task clock here (perf_event_open refused, or not Linux): nothing to hold it to");
            return;
        };
        eprintln!("task clock opened");
        let read = || clock.run_ns().unwrap_or_default();
        let a = read();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let asleep = read().saturating_sub(a);
        assert!(asleep < 5_000_000, "asleep 30 ms, ran {asleep} ns");
        let began = std::time::Instant::now();
        let b = read();
        let mut spins = 0u64;
        while began.elapsed() < std::time::Duration::from_millis(30) {
            spins = std::hint::black_box(spins.wrapping_add(1));
        }
        let ran = read().saturating_sub(b);
        let wall = u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX);
        assert!(ran <= wall.saturating_add(1_000_000), "ran {ran} ns in {wall} ns of wall time");
        assert!(ran >= 5_000_000, "spun 30 ms, ran {ran} ns (spins {spins})");
    }
}
