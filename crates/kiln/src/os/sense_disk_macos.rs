//! macOS: the work volume's device (docs/needs.md). Its IOKit reads come with the next commit.

use std::path::Path;

use crate::sense::disk::Counters;

pub struct Device;

impl Device {
    pub fn of(_work_dir: &Path) -> Option<Device> {
        None
    }

    pub fn counters(&mut self) -> Option<Counters> {
        None
    }
}

pub fn facts(_work_dir: &Path) -> Option<kiln_ci::facts::Storage> {
    None
}
