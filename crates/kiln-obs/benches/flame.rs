//! Profiles summed into flame graphs (`docs/observe.md` §8.11, §12.36): a busy service's minute of
//! CPU (synthetic, its shape a real one's: thousands of functions under a few entry points, stacks
//! 10 to 40 deep sharing their callers, tens of thousands of distinct stacks), read (`pprof::read`)
//! and summed (`Flame::add`) into a new graph and into one that has its nodes; ten profiles of it
//! summed in turn. Time (the least of nine, and their median), allocations. Only `Flame::new` and
//! `Flame::add` are used, so the same bench measures an older flame graph against a newer, built in
//! turn.
//!
//! `cargo bench -p kiln-obs --bench flame` (`KOBS_FUNCTIONS`, 8,000; `KOBS_STACKS`, 40,000).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::time::{Duration, Instant};

use hyper_measure::alloc;
use kiln_obs::otlp::pb::Writer;
use kiln_obs::profile::flame::Flame;
use kiln_obs::profile::pprof;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

/// A deterministic generator (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// A minute's CPU profile of a busy service as Go writes it (inflated): `functions` functions, a
/// location each; `stacks` distinct stacks 10 to 40 deep, each a walk down a call tree whose callees
/// are drawn by the path so far (so stacks share their callers); values a count and its
/// nanoseconds at 100 Hz.
fn cpu_profile(functions: usize, stacks: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed);
    let mut strings: Vec<String> = vec![String::new(), "samples".into(), "count".into(), "cpu".into(), "nanoseconds".into()];
    let mut w = Writer::default();
    for (t, u) in [(1, 2), (3, 4)] {
        w.message(1, |m| {
            m.varint(1, t);
            m.varint(2, u);
        });
    }
    let pkgs = [
        "runtime",
        "net/http",
        "encoding/json",
        "github.com/acme/shop/internal/cart",
        "github.com/acme/shop/internal/orders",
        "github.com/jackc/pgx/v5",
        "google.golang.org/grpc",
        "crypto/sha256",
        "main",
    ];
    for f in 1..=functions {
        let pkg = pkgs[f % pkgs.len()];
        strings.push(format!("{pkg}.(*handler{}).step{f}", f % 37));
        let name = strings.len() as u64 - 1;
        strings.push(format!("/src/{pkg}/file{}.go", f % 211));
        let file = strings.len() as u64 - 1;
        w.message(5, |m| {
            m.varint(1, f as u64);
            m.varint(2, name);
            m.varint(3, name);
            m.varint(4, file);
        });
        w.message(4, |m| {
            m.varint(1, f as u64);
            m.varint(3, 0x40_0000 + f as u64 * 16);
            m.message(4, |l| {
                l.varint(1, f as u64);
                l.varint(2, 10 + f as u64 % 500);
            });
        });
    }
    for _ in 0..stacks {
        let depth = 10 + rng.below(31) as usize;
        let mut path = Vec::with_capacity(depth);
        let mut at = 1 + rng.below(4);
        for d in 0..depth {
            path.push(at);
            let fan = if d < 4 { 3 } else { 12 };
            at = 1 + (at.wrapping_mul(2_654_435_761).wrapping_add(rng.below(fan)) % functions as u64);
        }
        path.reverse();
        let count = 1 + rng.below(4);
        w.message(2, |m| {
            let mut ids = Writer::default();
            for id in &path {
                // Packed varints: each id its bytes, no keys.
                let mut v = *id;
                while v >= 0x80 {
                    ids.buf.push((v as u8) | 0x80);
                    v >>= 7;
                }
                ids.buf.push(v as u8);
            }
            m.bytes(1, &ids.buf);
            let mut vals = Writer::default();
            for mut v in [count, count * 10_000_000] {
                while v >= 0x80 {
                    vals.buf.push((v as u8) | 0x80);
                    v >>= 7;
                }
                vals.buf.push(v as u8);
            }
            m.bytes(2, &vals.buf);
        });
    }
    for s in &strings {
        w.str(6, s);
    }
    w.varint(9, 1_792_771_200_000_000_000);
    w.varint(10, 60_000_000_000);
    w.message(11, |m| {
        m.varint(1, 3);
        m.varint(2, 4);
    });
    w.varint(12, 10_000_000);
    w.buf
}

fn measure<T>(mut f: impl FnMut() -> T) -> (Duration, Duration, u64) {
    let mut times = Vec::new();
    let mut calls = 0;
    for n in 0..9 {
        let t = Instant::now();
        if n == 1 {
            alloc::begin();
        }
        std::hint::black_box(f());
        if n == 1 {
            calls = alloc::end().calls();
        }
        times.push(t.elapsed());
    }
    times.sort();
    (times[0], times[times.len() / 2], calls)
}

fn line(what: &str, (least, median, calls): (Duration, Duration, u64), more: &str) {
    println!("{what:<52} {:>9.3} / {:>9.3}ms {calls:>9} allocations  {more}", least.as_secs_f64() * 1e3, median.as_secs_f64() * 1e3);
}

fn main() {
    assert!(alloc::installed(), "the counting allocator");
    let functions = std::env::var("KOBS_FUNCTIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(8_000);
    let stacks = std::env::var("KOBS_STACKS").ok().and_then(|v| v.parse().ok()).unwrap_or(40_000);
    let profiles: Vec<Vec<u8>> = (0..10).map(|i| cpu_profile(functions, stacks, i + 1)).collect();
    let p0 = pprof::read(&profiles[0]).unwrap();
    let frames = p0.stacks.len();
    println!("{} KiB a profile, {} samples, {frames} frames\n", profiles[0].len() >> 10, p0.samples.len());
    line(
        "read (pprof::read)",
        measure(|| pprof::read(&profiles[0]).unwrap().samples.len()),
        &format!("{:.0} MB/s", profiles[0].len() as f64 / measure(|| pprof::read(&profiles[0]).unwrap().samples.len()).0.as_secs_f64() / 1e6),
    );
    let t = measure(|| {
        let mut g = Flame::new();
        g.add(&p0, 1);
        g.nodes.len()
    });
    line("summed into a new graph", t, &format!("{:.1} ns a frame", t.0.as_secs_f64() * 1e9 / frames as f64));
    let mut g = Flame::new();
    g.add(&p0, 1);
    let t = measure(|| {
        g.add(&p0, 1);
        g.nodes.len()
    });
    line("summed into a graph that has its nodes", t, &format!("{:.1} ns a frame", t.0.as_secs_f64() * 1e9 / frames as f64));
    let read: Vec<pprof::Profile<'_>> = profiles.iter().map(|b| pprof::read(b).unwrap()).collect();
    let t = measure(|| {
        let mut g = Flame::new();
        for p in &read {
            g.add(p, 1);
        }
        g.nodes.len()
    });
    let mut all = Flame::new();
    for p in &read {
        all.add(p, 1);
    }
    line("ten profiles summed in turn", t, &format!("{} nodes, {:.1} ns a frame", all.nodes.len(), t.0.as_secs_f64() * 1e9 / (frames * 10) as f64));
}
