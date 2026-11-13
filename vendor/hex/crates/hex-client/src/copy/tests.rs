//! The stdio copy's tests (AC-T7.4.1).

use std::io::{ErrorKind, PipeReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use hex_core::patience::Patience;
use hex_testkit::waits::answered;
use hex_testkit::waits::follow::{self, Party, Thread};

use super::{Cut, DuplexIo, copy};
use crate::guest::own_duplex;

// Each wait is for its own fact, named for it: no census records either yet, so each measures, for the fact alone
// (hex-testkit's rule).
/// The copied line in the caller's pipe.
static LINE: Patience = Patience::measuring("test.hex_client.copy.line");
/// The caller's pipe ending once the stream's peer has ended its half, the copier's thread its party.
static END: Patience = Patience::measuring("test.hex_client.copy.end");

/// The stream as its copier reads it: the copier's thread named at its first read, so a test can follow it.
struct Followed {
    from: DuplexIo,
    copier: Arc<OnceLock<Thread>>,
}

/// The caller's pipe's end as a party: its copier's thread while it lives, asleep through the stream's end the stall
/// the test is for. Once the copier has ended, having closed its end of the pipe, the pipe's end waits only on a
/// child another test forked meanwhile, which holds a copy of that end until its exec closes it: followed by its life
/// alone, so the copier's end is never taken for the pipe's.
fn ending(copier: &OnceLock<Thread>) -> Party {
    match copier.get().map_or(Party::Live, Thread::party) {
        Party::Ended => Party::Live,
        party => party,
    }
}

impl Read for Followed {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if self.copier.get().is_none()
            && let Ok(thread) = Thread::current()
        {
            let _ = self.copier.set(thread);
        }
        self.from.read(bytes)
    }
}

/// The rounds, and the spread of the half-close's spin, of Shards' race harness for its M236 (Shards
/// `docs/research/platform-measurements.md` M236 and `docs/research/measurements/half-close/`, at 04abb68d): 50,000
/// rounds, each side spinning 0 to 399 iterations, which left 360 macOS readers asleep through their end.
const ROUNDS: u32 = 50_000;
const SPREAD: u32 = 400;

/// What the caller's pipe holds now, read without waiting and kept in `got`: whether its end has come.
fn drain(pipe: &mut PipeReader, got: &mut Vec<u8>) -> bool {
    let mut buf = [0u8; 64];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => return true,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => return false,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => panic!("reading the caller's pipe: {e}"),
        }
    }
}

/// AC-T7.4.1 (HEX-QUIC-25 B3's hang, 2026-11-12; Shards M236 and D175): a program's output, copied from its stream's
/// connection into the caller's pipe, ends as the stream does. Do, each round: the stream's peer writes a line; as soon
/// as the line is in the caller's pipe, so the copier is reading again, the peer spins its round's share of M236's
/// spread and ends its half (SHUT_WR), keeping its end open. Expect: the caller reads the line and then its end, every
/// round. On macOS a read that waits in recv(2) on a Unix stream socket sleeps through a half-close landing as XNU's
/// `uipc_rcvd` lets go of the reader's lock (M236: 360 of 50,000 readers asleep), so the copier waits in poll(2),
/// which sees it. The end's wait follows the copier's thread (decision 9): past W* it goes on while the copier runs or
/// works, and fails only once the copier has slept through a whole window, as one asleep through its end would.
#[test]
fn ac_t7_4_1_a_copied_stream_ends_with_its_peers_half_close() {
    const SAID: &[u8] = b"round trip\n";
    for round in 0..ROUNDS {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (mut caller, pipe) = std::io::pipe().unwrap();
        let mode = rustix::fs::fcntl_getfl(&caller).unwrap();
        rustix::fs::fcntl_setfl(&caller, mode | rustix::fs::OFlags::NONBLOCK).unwrap();
        let copier = Arc::new(OnceLock::new());
        copy(
            Followed {
                from: DuplexIo(own_duplex(ours).unwrap()),
                copier: Arc::clone(&copier),
            },
            pipe,
            Cut::default(),
            drop,
        )
        .unwrap();
        (&theirs).write_all(SAID).unwrap();
        // The line is looked for without giving the CPU up, so the half-close follows the copier's next read closely.
        let mut got = Vec::new();
        let began = Instant::now();
        while !drain(&mut caller, &mut got) && got.len() < SAID.len() {
            std::hint::spin_loop();
        }
        answered(&LINE, began.elapsed());
        for _ in 0..round % SPREAD {
            std::hint::spin_loop();
        }
        theirs.shutdown(Shutdown::Write).unwrap();
        follow::until(
            &END,
            "the copied stream's end",
            || ending(&copier),
            || drain(&mut caller, &mut got),
        )
        .unwrap();
        assert_eq!(got, SAID, "round {round}");
        drop(theirs);
    }
}
