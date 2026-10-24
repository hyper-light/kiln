//! What a sealed file costs against the same bytes unsealed, through the same device file (docs/seal.md
//! §11, docs/benchmarks.md "Sealed files"): `cargo bench -p hyper-seal --bench files -- [DIR]`
//! (a temporary directory by default; name one on the disk to be measured).
//!
//! For segments of 4 KiB and 64 KiB, a 64 MiB file is written (in pieces of one segment, then
//! flushed) and read back (in pieces of one segment), sealed and unsealed, runs interleaved so the
//! host's load falls on both alike; the median of the runs is printed in MB/s with the spread. The
//! allocator calls of the sealed writes and reads after setup are counted: the law is none.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::indexing_slicing,
    clippy::cognitive_complexity
)]

use std::path::{Path, PathBuf};
use std::time::Instant;

use hyper_block::buf::{AlignedBuf, Alignment};
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_measure::alloc;
use hyper_seal::keys::WrappingKey;
use hyper_seal::sealed_file::{SealedReader, SealedWriter};

#[global_allocator]
static ALLOCATOR: alloc::Counting = alloc::Counting;

/// The file each run writes and reads: large enough that a run takes tens of milliseconds at
/// several GB/s, so a run is not the timer's resolution.
const TOTAL: usize = 64 << 20;
/// Runs of each kind, interleaved.
const RUNS: usize = 7;
/// The device's block, and every transfer's alignment.
const BLOCK: usize = 4096;

fn open(path: &Path, create: bool) -> DeviceFile {
    DeviceFile::open(
        path,
        create,
        CachingRequest::PreferDirect,
        Alignment::new(BLOCK).unwrap(),
    )
    .unwrap()
}

fn mbps(bytes: usize, nanos: u128) -> f64 {
    bytes as f64 / (nanos as f64 / 1e9) / 1e6
}

fn median(mut runs: Vec<f64>) -> (f64, f64, f64) {
    runs.sort_by(f64::total_cmp);
    (runs[0], runs[runs.len() / 2], runs[runs.len() - 1])
}

struct Rows {
    plain_write: Vec<f64>,
    sealed_write: Vec<f64>,
    plain_read: Vec<f64>,
    sealed_read: Vec<f64>,
    write_calls: u64,
    read_calls: u64,
}

fn run(dir: &Path, segment: usize, key: &WrappingKey, rows: &mut Rows) {
    let piece = {
        let mut buf = AlignedBuf::zeroed(segment, Alignment::new(BLOCK).unwrap()).unwrap();
        for (i, b) in buf.as_mut_capacity().iter_mut().enumerate() {
            *b = (i * 31) as u8;
        }
        buf.set_len(segment).unwrap();
        buf
    };

    // Unsealed: the same pieces, at their offsets, then the flush.
    let plain: PathBuf = dir.join("plain");
    let _ = std::fs::remove_file(&plain);
    let file = open(&plain, true);
    let start = Instant::now();
    for i in 0..TOTAL / segment {
        file.write_all_at(piece.as_slice(), (i * segment) as u64)
            .unwrap();
    }
    file.sync_data().unwrap();
    rows.plain_write
        .push(mbps(TOTAL, start.elapsed().as_nanos()));
    let mut back = AlignedBuf::zeroed(segment, Alignment::new(BLOCK).unwrap()).unwrap();
    back.set_len(segment).unwrap();
    let start = Instant::now();
    for i in 0..TOTAL / segment {
        file.read_exact_at(back.as_mut_slice(), (i * segment) as u64)
            .unwrap();
    }
    rows.plain_read
        .push(mbps(TOTAL, start.elapsed().as_nanos()));
    drop(file);

    // Sealed: the same pieces through the writer, then the reader.
    let sealed = dir.join("sealed");
    let _ = std::fs::remove_file(&sealed);
    let mut writer = SealedWriter::new(open(&sealed, true), key, segment as u32).unwrap();
    let start = Instant::now();
    alloc::begin();
    for _ in 0..TOTAL / segment {
        writer.write(piece.as_slice()).unwrap();
    }
    let calls = alloc::end().calls();
    drop(writer.finish().unwrap());
    rows.sealed_write
        .push(mbps(TOTAL, start.elapsed().as_nanos()));
    rows.write_calls = rows.write_calls.max(calls);

    let mut reader = SealedReader::open(open(&sealed, false), key, segment as u32).unwrap();
    let mut out = vec![0u8; segment];
    let start = Instant::now();
    alloc::begin();
    for i in 0..TOTAL / segment {
        reader.read_at(&mut out, (i * segment) as u64).unwrap();
    }
    let calls = alloc::end().calls();
    rows.sealed_read
        .push(mbps(TOTAL, start.elapsed().as_nanos()));
    rows.read_calls = rows.read_calls.max(calls);
    assert_eq!(out, piece.as_slice());
}

fn main() {
    let arg = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let temp;
    let dir: PathBuf = match arg {
        Some(dir) => PathBuf::from(dir),
        None => {
            temp = tempfile::tempdir().unwrap();
            temp.path().to_path_buf()
        }
    };
    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    println!(
        "sealed files: {} MiB a run, {RUNS} runs interleaved, {BLOCK} B direct transfers, in {} {}",
        TOTAL >> 20,
        dir.display(),
        load.trim()
    );
    // The key and each file's data key live in the locked key region (docs/seal.md §8).
    hyper_seal::lock_keys(16).unwrap();
    let key = WrappingKey::generate(0).unwrap();
    for segment in [4096usize, 65536] {
        let mut rows = Rows {
            plain_write: Vec::new(),
            sealed_write: Vec::new(),
            plain_read: Vec::new(),
            sealed_read: Vec::new(),
            write_calls: 0,
            read_calls: 0,
        };
        for _ in 0..RUNS {
            run(&dir, segment, &key, &mut rows);
        }
        println!(
            "segment {} KiB          min / median / max MB/s",
            segment >> 10
        );
        for (name, runs) in [
            ("write+flush, unsealed", rows.plain_write),
            ("write+flush, sealed", rows.sealed_write),
            ("read, unsealed", rows.plain_read),
            ("read, sealed", rows.sealed_read),
        ] {
            let (lo, mid, hi) = median(runs);
            println!("  {name:<24} {lo:>8.0} {mid:>8.0} {hi:>8.0}");
        }
        println!(
            "  allocator calls after setup: writes {} reads {} (over {} segments each)",
            rows.write_calls,
            rows.read_calls,
            TOTAL / segment
        );
    }
    let _ = std::fs::remove_file(dir.join("plain"));
    let _ = std::fs::remove_file(dir.join("sealed"));
}
