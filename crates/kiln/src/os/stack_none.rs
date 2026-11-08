//! The calling thread's own stack where kiln leaves it as the system keeps it: macOS and Windows (goal 11, plan
//! 01a1a87b2e56 step 5). Only Linux's (`stack_linux.rs`) is marked against huge pages and given back below a cold
//! worker's frame: the server the bar measures runs there, and macOS and Windows give a thread's stack no
//! transparent huge pages to keep off.

use std::io;
use std::ops::Range;

/// Where the calling thread's stack is: not said here.
pub fn bounds() -> Option<Range<usize>> {
    None
}

/// Nothing to keep off the stack: this system gives it no transparent huge pages.
pub fn no_huge_pages() -> io::Result<()> {
    Ok(())
}

/// Leaves the stack's pages as they are: nothing given back.
pub fn give_back_unused() -> usize {
    0
}
