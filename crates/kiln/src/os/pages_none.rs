//! This binary's own pages in resident memory where kiln leaves them as the system keeps them: macOS and
//! Windows (goal 11, plan 01a1a87b2e56 step 5). Only Linux takes them out (`pages_linux.rs`): the server the
//! bar measures runs there, and Linux says page by page which of them are still the file's own
//! (`/proc/self/pagemap`), so a drop never loses a page something wrote.

use std::io;

/// Whether this platform can take pages out of resident memory.
pub const CAN: bool = false;

/// What a drop took out of resident memory: nothing, here.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Dropped {
    pub bytes: u64,
    pub runs: usize,
    pub kept: u64,
}

/// Leaves `spans`' pages as they are.
pub fn drop_spans(_spans: &[(usize, usize)]) -> io::Result<Dropped> {
    Ok(Dropped::default())
}

/// Leaves the binary's pages as they are.
pub fn drop_all_own() -> io::Result<Dropped> {
    Ok(Dropped::default())
}
