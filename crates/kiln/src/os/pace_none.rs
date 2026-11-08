//! Elsewhere: nothing of [`crate::sense::pace`]'s is read, and no event is watched.

use std::path::Path;

use crate::sense::pace::Raw;

pub struct Reader;

impl Reader {
    pub fn new(_work_dir: &Path) -> Reader {
        Reader
    }

    pub fn read(&mut self) -> Raw {
        Raw::default()
    }
}

pub fn wakes() -> u64 {
    0
}

pub fn watch(_on: impl Fn() + Send + 'static) {}
