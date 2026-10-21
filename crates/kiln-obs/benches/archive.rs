//! Log archives' encoding on recorded data (`docs/observe.md` §10.5, §12.26): the lines of §12.6 as
//! records, cut into a store's logs files as a busy store cuts them, each file then read back and
//! written as Datadog's archived events (`kiln_obs::logs::archive::event`), gzip'd at the levels a
//! writer might choose. One core; throughput is of the events' JSON.
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

/// What one pass over the files did: events, their JSON's bytes, gzip's bytes, allocations, seconds.
#[derive(Default)]
struct Pass {
    events: u64,
    json: u64,
    gz: u64,
    allocs: u64,
    secs: f64,
}

/// Every file's records read and written as events (`tags` when asked), each through `search` when
/// given, gzip'd at `level` when given (a stream a file, as an object holds them).
fn pass(files: &[std::sync::Arc<LogFile>], tags: bool, search: Option<&Search>, level: Option<u32>) -> Pass {
    let (mut text, mut raw, mut line, mut id) = (Vec::new(), Vec::new(), Vec::new(), String::new());
    let mut scratch = archive::Scratch::default();
    let mut rs = RowScratch::default();
    let seed = [7u8; 8];
    let mut p = Pass::default();
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
    p
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

    println!("\n{:<44} {:>12} {:>10} {:>12} {:>10} {:>12}", "", "events/s", "MB/s", "allocs/event", "ratio", "JSON/line");
    for (name, tags) in [("read, written as events", false), ("read, written as events with tags", true)] {
        let p = pass(&files, tags, None, None);
        println!(
            "{:<44} {:>12.0} {:>10.1} {:>12.4} {:>10} {:>12.2}",
            name,
            p.events as f64 / p.secs,
            p.json as f64 / p.secs / 1e6,
            p.allocs as f64 / p.events as f64,
            "",
            p.json as f64 / raw_bytes as f64
        );
    }
    for level in [1u32, 3, 6, 9] {
        let p = pass(&files, false, None, Some(level));
        println!(
            "{:<44} {:>12.0} {:>10.1} {:>12.4} {:>10.2} {:>12.3}",
            format!("and gzip'd, level {level}"),
            p.events as f64 / p.secs,
            p.json as f64 / p.secs / 1e6,
            p.allocs as f64 / p.events as f64,
            p.json as f64 / p.gz as f64,
            p.gz as f64 / raw_bytes as f64
        );
    }
    // The archiver's path: each record through an archive's search (kiln's events left out), then
    // written and gzip'd as it writes them.
    let all = parse(&kiln_obs::monitor::logs_only("*")).unwrap();
    let errors = parse(&kiln_obs::monitor::logs_only("status:error")).unwrap();
    for (name, s) in [("searched (*), written, gzip'd 6", &all), ("searched (status:error), written, gzip'd 6", &errors)] {
        let p = pass(&files, false, Some(s), Some(6));
        println!(
            "{:<44} {:>12.0} {:>10.1} {:>12.4} {:>10.2} {:>12.3}",
            name,
            p.events as f64 / p.secs,
            p.json as f64 / p.secs / 1e6,
            p.allocs as f64 / p.events.max(1) as f64,
            p.json as f64 / p.gz.max(1) as f64,
            p.gz as f64 / raw_bytes as f64
        );
    }
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}
