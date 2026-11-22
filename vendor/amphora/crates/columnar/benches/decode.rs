//! Decode micro-benchmark (CS.3, [c18]): values decoded per cycle and bits per value, both codecs,
//! on columns shaped like the corpus's `events` table (bench/src/schema.rs).
//!
//! This binary counts values and time; cycles come from the CPU's own counter, read around it:
//! `perf stat -e cycles -x, <bench> --column at --codec fastlanes --rows N --reps R` and the same
//! with `--reps 0` (encoding only); values per cycle = N * R / (cycles(R) - cycles(0)).
//! Without `--column`, it prints every column and codec: bits a value and nanoseconds a value.
//! Rows and repetitions are the run's parameters, given by whoever runs it, and printed with it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::hint::black_box;
use std::io::Write;
use std::time::Instant;

use amphora_columnar::{Codec, Column, Strings, Values, encode};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn strs<'a>(v: impl IntoIterator<Item = &'a [u8]>) -> Strings {
    let mut s = Strings::new();
    for e in v {
        s.push(e);
    }
    s
}

fn columns(n: usize) -> Vec<(&'static str, Column)> {
    let mut rng = Rng(11);
    let mut t = 1_795_000_000_000i64;
    let at: Vec<i64> = (0..n)
        .map(|_| {
            t += rng.below(2_000) as i64;
            t
        })
        .collect();
    let kinds: [&[u8]; 6] = [
        b"message",
        b"tool_use",
        b"tool_result",
        b"status",
        b"thinking",
        b"subagent",
    ];
    let levels: [&[u8]; 3] = [b"info", b"warn", b"error"];
    let ids: Vec<Vec<u8>> = (0..n)
        .map(|_| format!("01a1{:016x}{:012x}", rng.next(), rng.next() >> 16).into_bytes())
        .collect();
    vec![
        (
            "seq",
            Column {
                values: Values::Int((0..n as i64).collect()),
                valid: None,
            },
        ),
        (
            "at",
            Column {
                values: Values::Int(at),
                valid: None,
            },
        ),
        (
            "rev",
            Column {
                values: Values::Int((0..n).map(|_| rng.below(8) as i64).collect()),
                valid: None,
            },
        ),
        (
            "epoch",
            Column {
                values: Values::Int((0..n).map(|i| (i / 4096) as i64).collect()),
                valid: None,
            },
        ),
        (
            "seconds",
            Column {
                values: Values::Double(
                    (0..n)
                        .map(|_| (rng.below(600_000) as f64) / 1000.0)
                        .collect(),
                ),
                valid: None,
            },
        ),
        (
            "kind",
            Column {
                values: Values::Str(strs((0..n).map(|_| kinds[rng.below(6) as usize]))),
                valid: None,
            },
        ),
        (
            "level",
            Column {
                values: Values::Str(strs((0..n).map(|_| levels[rng.below(3) as usize]))),
                valid: Some((0..n).map(|_| rng.below(3) != 0).collect()),
            },
        ),
        (
            "id",
            Column {
                values: Values::Str(strs(ids.iter().map(Vec::as_slice))),
                valid: None,
            },
        ),
    ]
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn main() {
    let args: Vec<String> = std::env::args_os()
        .filter_map(|a| a.into_string().ok())
        .collect();
    let rows: usize = arg(&args, "--rows")
        .and_then(|v| v.parse().ok())
        .expect("--rows N: the column's rows");
    let reps: usize = arg(&args, "--reps")
        .and_then(|v| v.parse().ok())
        .expect("--reps R: decodes of each column");
    let only = arg(&args, "--column");
    let codec_only = arg(&args, "--codec");
    let mut out = std::io::stdout().lock();
    writeln!(out, "column\tcodec\tschemes\tbits_per_value\tns_per_value\tvalues_per_ns\trows={rows}\treps={reps}").unwrap();
    for (name, column) in columns(rows) {
        if only.as_deref().is_some_and(|o| o != name) {
            continue;
        }
        for (cname, codec) in [
            ("btrblocks", Codec::BtrBlocks),
            ("fastlanes", Codec::FastLanes),
        ] {
            if codec_only.as_deref().is_some_and(|o| o != cname) {
                continue;
            }
            let e = encode(codec, &column).unwrap();
            assert!(
                column.equal_rows(&e.decode().unwrap()),
                "{name} {cname}: round trip"
            );
            let schemes: Vec<String> = e
                .tree
                .nodes
                .iter()
                .map(|n| {
                    format!("{n:?}")
                        .split([' ', '(', '{'])
                        .next()
                        .unwrap_or("")
                        .to_string()
                })
                .collect();
            let start = Instant::now();
            for _ in 0..reps {
                black_box(black_box(&e).decode().unwrap());
            }
            let ns = start.elapsed().as_nanos() as f64;
            let values = (rows * reps) as f64;
            writeln!(
                out,
                "{name}\t{cname}\t{}\t{:.3}\t{:.4}\t{:.3}",
                schemes.join(">"),
                e.bits_per_value(),
                ns / values.max(1.0),
                values / ns.max(1.0)
            )
            .unwrap();
        }
    }
}
