//! Windows' reads for [`crate::sense`]: the CPUs' times (`GetSystemTimes`), memory
//! (`GlobalMemoryStatusEx`), the work volume's free space to this user (`GetDiskFreeSpaceExW`), and
//! its disk's counters. Nothing privileged; Windows keeps no stall times.

#![allow(unsafe_code)]

use std::path::{Path, PathBuf};

use crate::sense::{CpuTicks, Raw, Space};

/// `FILETIME`: 100-ns intervals, in two halves.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FileTime {
    low: u32,
    high: u32,
}

impl FileTime {
    fn get(self) -> u64 {
        (u64::from(self.high) << 32) | u64::from(self.low)
    }
}

/// `MEMORYSTATUSEX`.
#[repr(C)]
#[derive(Default)]
struct MemoryStatus {
    length: u32,
    load: u32,
    total_phys: u64,
    avail_phys: u64,
    total_page_file: u64,
    avail_page_file: u64,
    total_virtual: u64,
    avail_virtual: u64,
    avail_extended_virtual: u64,
}

const _: () = assert!(std::mem::size_of::<MemoryStatus>() == 64);

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetSystemTimes(idle: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime) -> i32;
    fn GlobalMemoryStatusEx(status: *mut MemoryStatus) -> i32;
}

/// Physical memory, all and available (`GlobalMemoryStatusEx`): one call.
fn memory() -> (u64, u64) {
    let mut m = MemoryStatus { length: u32::try_from(std::mem::size_of::<MemoryStatus>()).unwrap_or(0), ..MemoryStatus::default() };
    // SAFETY: a whole MEMORYSTATUSEX with its length set, as the call requires.
    if unsafe { GlobalMemoryStatusEx(&raw mut m) } == 0 {
        return (0, 0);
    }
    (m.total_phys, m.avail_phys)
}

/// All CPUs' idle time and all their time (kernel time includes idle), in 100 ns.
fn cpu() -> Option<CpuTicks> {
    let (mut idle, mut kernel, mut user) = (FileTime::default(), FileTime::default(), FileTime::default());
    // SAFETY: three places for a FILETIME each.
    let ok = unsafe { GetSystemTimes(&raw mut idle, &raw mut kernel, &raw mut user) };
    (ok != 0).then(|| CpuTicks { idle: idle.get(), total: kernel.get().saturating_add(user.get()) })
}

pub struct Reader {
    work_dir: PathBuf,
    device: Option<super::sense_disk_windows::Device>,
}

impl Reader {
    pub fn new(work_dir: &Path) -> Reader {
        let work_dir = crate::sense::canonical(work_dir);
        let device = super::sense_disk_windows::Device::of(&work_dir);
        Reader { work_dir, device }
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    pub fn read(&mut self) -> Raw {
        let (mem_total, mem_available) = memory();
        Raw {
            cpu: cpu(),
            cpus: u32::try_from(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)).unwrap_or(1),
            mem_total,
            mem_available,
            space: crate::os::disk_space(&self.work_dir).map(|(total, free)| Space { total, free, important: None }),
            device: self.device.as_mut().and_then(super::sense_disk_windows::Device::counters),
            ..Raw::default()
        }
    }
}

/// The work directory's storage facts (docs/needs.md, W13-R01).
pub fn storage(work_dir: &Path) -> Option<kiln_ci::facts::Storage> {
    super::sense_disk_windows::facts(&crate::sense::canonical(work_dir))
}
