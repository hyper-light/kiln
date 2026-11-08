//! [`crate::sense`] where kiln has no reads of its own for the OS: levels `sysinfo` gives, no
//! counters.

use std::path::{Path, PathBuf};

use crate::sense::{Raw, Space};

pub struct Reader {
    work_dir: PathBuf,
    sys: sysinfo::System,
}

impl Reader {
    pub fn new(work_dir: &Path) -> Reader {
        Reader { work_dir: crate::sense::canonical(work_dir), sys: sysinfo::System::new() }
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    pub fn read(&mut self) -> Raw {
        self.sys.refresh_memory();
        Raw {
            cpus: u32::try_from(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)).unwrap_or(1),
            mem_total: self.sys.total_memory(),
            mem_available: self.sys.available_memory(),
            space: crate::os::disk_space(&self.work_dir).map(|(total, free)| Space { total, free, important: None }),
            ..Raw::default()
        }
    }
}

pub fn storage(_work_dir: &Path) -> Option<kiln_ci::facts::Storage> {
    None
}
