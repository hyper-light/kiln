//! Windows' reads for [`crate::sense`]: the CPUs' times (`GetSystemTimes`), memory (`sysinfo`'s
//! `GlobalMemoryStatusEx`), the work volume's free space to this user (`GetDiskFreeSpaceExW`), and
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

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetSystemTimes(idle: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime) -> i32;
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
    sys: sysinfo::System,
    device: Option<super::sense_disk_windows::Device>,
}

impl Reader {
    pub fn new(work_dir: &Path) -> Reader {
        let work_dir = crate::sense::canonical(work_dir);
        let device = super::sense_disk_windows::Device::of(&work_dir);
        Reader { work_dir, sys: sysinfo::System::new(), device }
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    pub fn read(&mut self) -> Raw {
        self.sys.refresh_memory();
        Raw {
            cpu: cpu(),
            cpus: u32::try_from(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)).unwrap_or(1),
            mem_total: self.sys.total_memory(),
            mem_available: self.sys.available_memory(),
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
