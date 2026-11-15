//! The review's C-7 (AC-T7.4.1): `first_readable`, the poll every container create, claim and park waits in on Linux
//! (`hex_runtime::linux::oci::run`, and `hex-node`'s create pool), allocates nothing. Its poll set was a `Vec` collected
//! for every poll and every `EINTR`. A counting global allocator (the `cap` crate's, as `hex-gpu`'s hardware test
//! counts) reads the bytes allocated around the calls. This binary holds one test, so nothing else allocates while it
//! measures.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::alloc::System;
use std::io::Write;
use std::os::fd::AsFd;
use std::time::Duration;

use cap::Cap;
use hex_runtime::linux::oci::run::first_readable;

#[global_allocator]
static ALLOC: Cap<System> = Cap::new(System, usize::MAX);

/// Do: poll two pipes, one holding a byte, with no time to wait; then the quiet one alone, its bound already passed.
/// Expect: the second pipe named, then none, and no byte allocated by either poll.
#[test]
fn ac_t7_4_1_c7_first_readable_polls_without_allocating() {
    let (quiet, _quiet_writer) = std::io::pipe().unwrap();
    let (ready, mut ready_writer) = std::io::pipe().unwrap();
    ready_writer.write_all(&[1]).unwrap();
    let both = [quiet.as_fd(), ready.as_fd()];
    let alone = [quiet.as_fd()];
    let before = ALLOC.total_allocated();
    let first = first_readable(&both, Some(Duration::ZERO), "polling two test pipes");
    let none = first_readable(&alone, Some(Duration::ZERO), "polling a quiet test pipe");
    let allocated = ALLOC.total_allocated() - before;
    assert_eq!(first.unwrap(), Some(1));
    assert_eq!(none.unwrap(), None);
    assert_eq!(allocated, 0, "the polls allocated {allocated} bytes");
}
