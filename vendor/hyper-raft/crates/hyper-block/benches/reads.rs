//! A seek's page reads, one after another or as one batch through the device's issuer:
//! `cargo bench -p hyper-block --bench reads [-- DIR]` (docs/benchmarks.md, "hyper-block: a
//! batch of reads through the issuer").
//!
//! A file of pages on a real disk in DIR (the target directory's scratch by default), opened for
//! direct I/O where the file system takes it, so a read reaches the device rather than the page
//! cache. Each operation reads `FAN_OUT` pages at pseudo-random page offsets, the pages an engine's
//! seek reads from as many branches. **sequential** reads them on the calling thread, one
//! `read_exact_at` after another. **batched** hands them to an issuer of `FAN_OUT` workers as one
//! batch (`Attached::submit_reads`) and takes its answer. The batch's vector, its buffers and
//! offsets, is the same each operation and goes back to the caller with the answer, so the count is
//! the read path's own. **native AIO** hands the batch to the kernel from the calling thread
//! (Linux, direct files). The **durable writes** rows write `FAN_OUT` pages at such offsets and
//! flush them, in place or through native AIO.
//!
//! Each row takes Wilks' least sample for a one-sided 95 % bound on the p99.9, `n = ⌈ln 0.05 / ln
//! 0.999⌉ = 2,995` operations [WILKS], after as many unmeasured. Allocator calls are counted across
//! the process, the issuer's threads included (`hyper_measure::alloc::begin_process`), and page
//! faults from the OS (`hyper_measure::faults`), each per operation.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::time::Instant;

use hyper_block::aio::AioFile;
use hyper_block::buf::{AlignedBuf, Alignment};
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_block::issuer::Issuer;
use hyper_measure::alloc::{self, Counting};
use hyper_measure::faults;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Pages a seek reads: mantle's engine measured about 4.5 from different branches per seek at 10M
/// keys (2026-10-07), so four, the whole pages of that.
const FAN_OUT: usize = 4;
/// The page, the device's block and the engine's page.
const PAGE: usize = 4096;
/// Pages in the file, 256 MiB: offsets drawn across it rarely repeat within a row (2,995 operations of
/// four pages among 65,536), so a row's reads are mostly first reads of their page.
const PAGES: u64 = 65_536;

fn wilks(quantile: f64, confidence: f64) -> usize {
    ((1.0 - confidence).ln() / quantile.ln()).ceil() as usize
}

/// SplitMix64 (Steele, Lea and Flood, OOPSLA 2014): the page offsets, the same every run.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn page_buf(align: Alignment) -> AlignedBuf {
    let mut buf = AlignedBuf::zeroed(PAGE, align).unwrap();
    buf.set_len(PAGE).unwrap();
    buf
}

/// The bench's file, written once, opened with `request`.
fn file(dir: &Path, request: CachingRequest) -> DeviceFile {
    let path = dir.join("reads.bench");
    let align = Alignment::new(PAGE).unwrap();
    let fresh = !path.exists();
    let file = DeviceFile::open(&path, fresh, request, align).unwrap();
    if file.len().unwrap() < PAGES * PAGE as u64 {
        let mut chunk = AlignedBuf::zeroed(1 << 20, align).unwrap();
        chunk.extend_from_slice(&[0x5a; 1 << 20]).unwrap();
        for at in (0..PAGES * PAGE as u64).step_by(1 << 20) {
            file.write_all_at(chunk.as_slice(), at).unwrap();
        }
        file.sync_data().unwrap();
    }
    file
}

struct Row {
    p50_ns: u64,
    p99_ns: u64,
    max_ns: u64,
    allocs: f64,
    faults: f64,
}

fn summarize(mut lat: Vec<u64>, allocs: u64, faults: u64) -> Row {
    let n = lat.len();
    lat.sort_unstable();
    let at = |q: f64| lat[((n as f64 * q) as usize).min(n - 1)];
    Row {
        p50_ns: at(0.5),
        p99_ns: at(0.99),
        max_ns: lat[n - 1],
        allocs: allocs as f64 / n as f64,
        faults: faults as f64 / n as f64,
    }
}

fn print(name: &str, row: &Row) {
    println!(
        "{name:<18} p50 {:>8.2} µs  p99 {:>8.2} µs  max (p99.9 bound) {:>8.2} µs  \
         allocs/op {:.2}  faults/op {:.2}",
        row.p50_ns as f64 / 1e3,
        row.p99_ns as f64 / 1e3,
        row.max_ns as f64 / 1e3,
        row.allocs,
        row.faults,
    );
}

/// Runs `op` `n` times unmeasured, then `n` times measured.
fn measure(n: usize, mut op: impl FnMut(&mut u64)) -> Row {
    let mut seed = 1;
    for _ in 0..n {
        op(&mut seed);
    }
    let mut lat = Vec::with_capacity(n);
    let before = faults::read().unwrap();
    alloc::begin_process();
    for _ in 0..n {
        let started = Instant::now();
        op(&mut seed);
        lat.push(started.elapsed().as_nanos() as u64);
    }
    let allocs = alloc::end_process().calls();
    let after = faults::read().unwrap();
    summarize(lat, allocs, after.since(&before).total())
}

/// Both rows on one opening of the file: sequential reads on the calling thread, then one batch
/// through an issuer of `FAN_OUT` workers.
fn rows(dir: &Path, n: usize, request: CachingRequest, label: &str) {
    let file = file(dir, request);
    let align = file.alignment();
    let offset = |seed: &mut u64| (next(seed) % PAGES) * PAGE as u64;
    let mut bufs: Vec<AlignedBuf> = (0..FAN_OUT).map(|_| page_buf(align)).collect();
    let sequential = measure(n, |seed| {
        for buf in &mut bufs {
            file.read_exact_at(buf.as_mut_slice(), offset(seed))
                .unwrap();
        }
    });
    print(&format!("{label} sequential"), &sequential);
    let issuer = Issuer::start(dir, FAN_OUT).unwrap();
    let mut attached = issuer.attach(&file).unwrap();
    // The reads vector is given back with its answer, through the issuer as through native AIO, so
    // neither row allocates per operation for it.
    let mut reads: Option<Vec<(AlignedBuf, u64)>> =
        Some(bufs.into_iter().map(|buf| (buf, 0)).collect());
    let batched = measure(n, |seed| {
        let mut batch = reads.take().unwrap();
        for (_, at) in &mut batch {
            *at = offset(seed);
        }
        attached.submit_reads(batch).unwrap();
        reads = Some(attached.answer().unwrap().1.unwrap());
    });
    print(&format!("{label} batched"), &batched);
    drop(attached);
    // The same batch through the kernel's native AIO, issued and reaped on this thread (Linux,
    // direct files only; refused elsewhere, and the row is then not printed).
    if let Ok(mut aio) = AioFile::new(file, FAN_OUT, 1) {
        let native = measure(n, |seed| {
            let mut batch = reads.take().unwrap();
            for (_, at) in &mut batch {
                *at = offset(seed);
            }
            aio.submit_reads(batch).unwrap();
            reads = Some(aio.answer().unwrap().1.unwrap());
        });
        print(&format!("{label} native AIO"), &native);
    }
}

/// Durable writes of `FAN_OUT` pages at the read rows' offsets over a direct file: **in place**
/// writes each with `write_all_at` and then flushes with `sync_data`; **native AIO** hands the
/// writes and the flush to the kernel as one batch (`AioFile::submit_writes`) and takes its answer.
fn write_rows(dir: &Path, n: usize) {
    let file = file(dir, CachingRequest::PreferDirect);
    let align = file.alignment();
    let offset = |seed: &mut u64| (next(seed) % PAGES) * PAGE as u64;
    let mut bufs: Vec<AlignedBuf> = (0..FAN_OUT)
        .map(|i| {
            let mut buf = AlignedBuf::zeroed(PAGE, align).unwrap();
            buf.extend_from_slice(&[i as u8 + 1; PAGE]).unwrap();
            buf
        })
        .collect();
    let in_place = measure(n, |seed| {
        for buf in &bufs {
            file.write_all_at(buf.as_slice(), offset(seed)).unwrap();
        }
        file.sync_data().unwrap();
    });
    print("durable writes in place", &in_place);
    if let Ok(mut aio) = AioFile::new(file, FAN_OUT + 1, 1) {
        let mut writes: Option<Vec<(AlignedBuf, u64)>> =
            Some(bufs.drain(..).map(|buf| (buf, 0)).collect());
        let native = measure(n, |seed| {
            let mut batch = writes.take().unwrap();
            for (_, at) in &mut batch {
                *at = offset(seed);
            }
            aio.submit_writes(batch, true).unwrap();
            writes = Some(aio.answer().unwrap().1.unwrap());
        });
        print("durable writes native AIO", &native);
    }
}

fn main() {
    let dir: PathBuf = std::env::args()
        .skip(1)
        .find(|a| a != "--bench")
        .map_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")), PathBuf::from);
    let n = wilks(0.999, 0.95);
    println!("{n} operations a row, {FAN_OUT} pages of {PAGE} bytes each, {PAGES} pages");
    // The device's reads, past the page cache where the file system allows it.
    rows(&dir, n, CachingRequest::PreferDirect, "direct");
    write_rows(&dir, n);
    // The page cache's: the whole file read once first, so every row's read is a cached page's,
    // the case of an engine whose working set the OS caches.
    let cached = file(&dir, CachingRequest::Buffered);
    let mut buf = AlignedBuf::zeroed(1 << 20, Alignment::new(PAGE).unwrap()).unwrap();
    buf.set_len(1 << 20).unwrap();
    for at in (0..PAGES * PAGE as u64).step_by(1 << 20) {
        cached.read_exact_at(buf.as_mut_slice(), at).unwrap();
    }
    drop(cached);
    rows(&dir, n, CachingRequest::Buffered, "cached");
}
