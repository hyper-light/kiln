//! Log archives' encoding on recorded data (`docs/observe.md` §10.5, §12.26): the lines of §12.6 as
//! records, cut into a store's logs files as a busy store cuts them, each file then read back and
//! written as Datadog's archived events (`kiln_obs::logs::archive::event`), gzip'd at the levels a
//! writer might choose. One thread; throughput is of the events' JSON, per second of wall time and
//! per second of CPU (what a loaded machine's wall time overstates), with instructions where the OS
//! counts them.
//!
//! `KOBS_LOGS=<journal>:<pods> cargo bench -p kiln-obs --bench archive`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

use std::io::Write as _;
use std::time::Instant;

use hyper_measure::alloc;
use kiln_obs::batch::Builder;
use kiln_obs::hash::Keyed;
use kiln_obs::logs::archive;
use kiln_obs::logs::file::LogFile;
use kiln_obs::logs::search::{RowScratch, Search, parse};
use kiln_obs::manifest::Point;
use kiln_obs::store::Store;

#[path = "common/loglines.rs"]
mod loglines;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

/// What one pass over the files did: events, their JSON's bytes, gzip's bytes, allocations, wall
/// and CPU seconds, instructions.
#[derive(Default)]
struct Pass {
    events: u64,
    json: u64,
    gz: u64,
    allocs: u64,
    secs: f64,
    cpu: f64,
    instructions: Option<u64>,
}

/// Every file's records read and written as events (`tags` when asked), each through `search` when
/// given, gzip'd at `level` when given (a stream a file, as an object holds them).
fn pass(files: &[std::sync::Arc<LogFile>], tags: bool, search: Option<&Search>, level: Option<u32>) -> Pass {
    let (mut text, mut raw, mut line, mut id) = (Vec::new(), Vec::new(), Vec::new(), String::new());
    let mut scratch = archive::Scratch::default();
    let mut rs = RowScratch::default();
    let seed = [7u8; 8];
    let mut p = Pass::default();
    let u0 = hyper_measure::usage::this().unwrap();
    alloc::begin();
    let t = Instant::now();
    for f in files {
        let mut gz = level.map(|l| flate2::write::GzEncoder::new(Vec::with_capacity(1 << 20), flate2::Compression::new(l)));
        let mut n = 0u32;
        for i in 0..f.blocks.len() {
            let b = f.read_text(i, &mut text).unwrap();
            b.decompress_text(&text, &mut raw).unwrap();
            let mut rows = b.rows(&raw);
            while let Some(r) = rows.next_row().unwrap() {
                let at = n;
                n += 1;
                if search.is_some_and(|s| !s.row(&r, &mut rs)) {
                    continue;
                }
                id.clear();
                archive::id(r.ts.div_euclid(1_000_000), seed, at, &mut id);
                line.clear();
                archive::event(&r, &id, tags, &mut line, &mut scratch);
                if let Some(g) = gz.as_mut() {
                    g.write_all(&line).unwrap();
                }
                p.events += 1;
                p.json += line.len() as u64;
            }
        }
        if let Some(g) = gz {
            p.gz += g.finish().unwrap().len() as u64;
        }
    }
    p.secs = t.elapsed().as_secs_f64();
    p.allocs = alloc::end().calls();
    let u1 = hyper_measure::usage::this().unwrap();
    p.cpu = u1.cpu_ns().saturating_sub(u0.cpu_ns()) as f64 / 1e9;
    p.instructions = u1.instructions.zip(u0.instructions).map(|(a, b)| a.saturating_sub(b));
    p
}

/// A row: what `p` did, per event and per second.
fn row(name: &str, p: &Pass, raw_bytes: usize) {
    let ratio = if p.gz > 0 { format!("{:.2}", p.json as f64 / p.gz as f64) } else { String::new() };
    let per_line = if p.gz > 0 { p.gz as f64 / raw_bytes as f64 } else { p.json as f64 / raw_bytes as f64 };
    let ins = p.instructions.map_or_else(String::new, |i| format!("{:.0}", i as f64 / p.events.max(1) as f64));
    println!(
        "{:<44} {:>11.0} {:>11.0} {:>9.1} {:>10.4} {:>7} {:>9.3} {:>10}",
        name,
        p.events as f64 / p.secs,
        p.events as f64 / p.cpu.max(1e-9),
        p.json as f64 / p.cpu.max(1e-9) / 1e6,
        p.allocs as f64 / p.events.max(1) as f64,
        ratio,
        per_line,
        ins
    );
}

fn main() {
    let (records, raw_bytes) = match loglines::load() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    };
    let dir = std::env::temp_dir().join(format!("kobs-archive-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut store = Store::open(&dir, Keyed::new()).unwrap();
    // Cut as the logs bench cuts (§12.8): batches of 1,000 records, a cut every 50.
    let mut b = Builder::new();
    for (i, chunk) in records.chunks(1000).enumerate() {
        b.logs(chunk);
        store.apply(&b.finish()).unwrap();
        if i % 50 == 49 {
            store.cut(Point { index: i as u64 + 1, term: 1 }).unwrap();
        }
    }
    store.cut(Point { index: u64::MAX, term: 1 }).unwrap();
    let files: Vec<std::sync::Arc<LogFile>> = store.log_files().into_iter().map(|(_, f)| f).collect();
    println!("{} records ({:.1} MB of lines) in {} logs files", records.len(), raw_bytes as f64 / 1e6, files.len());
    // Warm: the files' pages in memory, as a file just cut is.
    let _ = pass(&files, false, None, None);

    println!("\n{:<44} {:>11} {:>11} {:>9} {:>10} {:>7} {:>9} {:>10}", "", "events/s", "events/cpu", "MB/cpu", "allocs/ev", "ratio", "out/line", "instr/ev");
    for (name, tags) in [("read, written as events", false), ("read, written as events with tags", true)] {
        row(name, &pass(&files, tags, None, None), raw_bytes);
    }
    for level in [1u32, 3, 6, 9] {
        row(&format!("and gzip'd, level {level}"), &pass(&files, false, None, Some(level)), raw_bytes);
    }
    // The archiver's path: each record through an archive's search (kiln's events left out), then
    // written and gzip'd as it writes them.
    let all = parse(&kiln_obs::monitor::logs_only("*")).unwrap();
    let errors = parse(&kiln_obs::monitor::logs_only("status:error")).unwrap();
    for (name, s) in [("searched (*), written, gzip'd 6", &all), ("searched (status:error), written, gzip'd 6", &errors)] {
        row(name, &pass(&files, false, Some(s), Some(6)), raw_bytes);
    }
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}
