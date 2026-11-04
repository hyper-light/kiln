//! What packing the wake bitmap's words costs wakers of different words (docs/runtime.md §15 item 7,
//! its second half): `cargo bench -p hyper-rt --bench wake_lines`.
//!
//! The bitmap packs its words, sixteen to a 128-byte line past the head's fourteen (§3.2); `sbitmap`
//! spreads its words one to a cache line (`docs/research/runtime-sources.md` §2). Packed, wakers of
//! different words on one line write that line between them as well as the head's, whose pending flag
//! every wake swaps whatever its word. One bitmap measures both: `threads` wakers each wake slots of a
//! word of its own past the head, either sixteen words apart (each word on a line of its own) or one
//! word apart (all of them on one line), while one thread drains as a shard does. Each row is
//! `HYPER_RT_WAKE_ROUNDS` rounds (default 5), the two layouts alternating, each round every waker
//! making `HYPER_RT_WAKES` wakes (default 1,000,000); it prints each round's wakes a second over all
//! wakers and the drains' takes.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use hyper_rt::wakes::WakeBitmap;

/// Format: slots past the head's fourteen words (`wakes.rs` `HEAD_WORDS`) and its first summary group,
/// so every waker's word is in the bitmap's body.
const BODY: u32 = 64 * 64;
/// Format: a 128-byte line holds sixteen 8-byte words.
const WORDS_A_LINE: u32 = 16;

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// One round: `threads` wakers of words `apart` words apart, against one drainer. Wakes a second.
fn round(threads: u32, apart: u32, wakes: u64) -> (f64, u64) {
    let bitmap = WakeBitmap::new((BODY + 64 * WORDS_A_LINE * (threads + 1)) as usize);
    let done = AtomicBool::new(false);
    let mut taken = 0u64;
    let started = Instant::now();
    std::thread::scope(|scope| {
        let wakers: Vec<_> = (0..threads)
            .map(|thread| {
                let bitmap = &bitmap;
                scope.spawn(move || {
                    let base = BODY + 64 * apart * thread;
                    for wake in 0..wakes {
                        assert!(bitmap.set(base + (wake % 64) as u32));
                    }
                })
            })
            .collect();
        let drainer = scope.spawn(|| {
            let mut taken = 0u64;
            while !done.load(Ordering::Acquire) {
                taken += bitmap.drain(|_| {}) as u64;
            }
            taken + bitmap.drain(|_| {}) as u64
        });
        for waker in wakers {
            waker.join().unwrap();
        }
        done.store(true, Ordering::Release);
        taken = drainer.join().unwrap();
    });
    let seconds = started.elapsed().as_secs_f64();
    ((u64::from(threads) * wakes) as f64 / seconds, taken)
}

fn main() {
    let rounds = env("HYPER_RT_WAKE_ROUNDS", 5);
    let wakes = env("HYPER_RT_WAKES", 1_000_000);
    println!("threads,layout,round,wakes_per_s,taken");
    for threads in [1u32, 2, 4, 8] {
        for round_index in 0..rounds {
            // The two layouts alternate, the first of each round rotated.
            let order = if round_index % 2 == 0 {
                [WORDS_A_LINE, 1]
            } else {
                [1, WORDS_A_LINE]
            };
            for apart in order {
                let (rate, taken) = round(threads, apart, wakes);
                let layout = if apart == 1 { "one-line" } else { "own-lines" };
                println!("{threads},{layout},{round_index},{rate:.0},{taken}");
            }
        }
    }
}
