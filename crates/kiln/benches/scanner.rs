//! The Sensitive Data Scanner on recorded logs (`docs/observe.md` §8.9, §12.28): the journal of
//! §12.8 as records, and what each part of a scan costs a record: the check a reader makes of a line
//! before it makes a record of it (`Scanner::reads`), and a scan (`Scanner::scan`), each with every
//! standard pattern but the network addresses, with those without keywords alone, with those with
//! keywords alone, and with one pattern of one's own; and the records the library changes, as a
//! share. The time of each (the median of five) a record, and allocations a record of one.
//!
//! `KOBS_LOGS=<journal>:<pods> cargo bench -p kiln --bench scanner`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

use std::time::Instant;

use hyper_measure::alloc;
use kiln::observe::scanner::patterns::STANDARD;
use kiln::observe::scanner::{GroupDef, Scanner, Scratch};
use kiln_obs::logs::record::Record;

#[path = "../../kiln-obs/benches/common/loglines.rs"]
mod loglines;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

fn group(rules: Vec<serde_json::Value>) -> Vec<GroupDef> {
    vec![serde_json::from_value(serde_json::json!({ "name": "bench", "rules": rules })).unwrap()]
}

fn library(keep: impl Fn(&kiln::observe::scanner::patterns::Standard) -> bool) -> Vec<GroupDef> {
    group(STANDARD.iter().filter(|p| p.category != "network" && keep(p)).map(|p| serde_json::json!({ "name": p.id, "standard_pattern_id": p.id, "text_replacement": { "type": "hash" } })).collect())
}

/// `r` copied into `c`, each string into its room (a derived `clone_from` clones the whole).
fn copy_into(c: &mut Record, r: &Record) {
    (c.ts, c.severity, c.trace_id, c.span_id) = (r.ts, r.severity, r.trace_id, r.span_id);
    c.service.clone_from(&r.service);
    c.host.clone_from(&r.host);
    c.source.clone_from(&r.source);
    c.message.clone_from(&r.message);
    c.attrs.truncate(r.attrs.len());
    for (i, (k, v)) in r.attrs.iter().enumerate() {
        match c.attrs.get_mut(i) {
            Some((ck, cv)) => {
                ck.clone_from(k);
                cv.clone_from(v);
            }
            None => c.attrs.push((k.clone(), v.clone())),
        }
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// `f` over every record five times: ns a record (the median pass), allocations a record (the
/// first pass), and how many it said yes of.
fn time(records: &[Record], mut f: impl FnMut(&Record) -> bool) -> (f64, f64, usize) {
    let (mut said, mut allocs, mut times) = (0, 0.0, Vec::new());
    for pass in 0..5 {
        if pass == 0 {
            alloc::begin();
        }
        let t = Instant::now();
        let n = records.iter().filter(|r| f(r)).count();
        times.push(t.elapsed().as_nanos() as f64 / records.len() as f64);
        if pass == 0 {
            allocs = alloc::end().calls() as f64 / records.len() as f64;
            said = n;
        }
    }
    (median(times), allocs, said)
}

fn main() {
    let (records, raw_bytes) = match loglines::load() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    };
    println!("{} records ({:.1} MB of lines)", records.len(), raw_bytes as f64 / 1e6);
    let cases: Vec<(&str, Vec<GroupDef>)> = vec![
        ("the library (no network)", library(|_| true)),
        ("its patterns without keywords", library(|p| !p.keywords_required)),
        ("its patterns with keywords", library(|p| p.keywords_required)),
        ("one pattern of one's own", group(vec![serde_json::json!({ "name": "orders", "pattern": "order-(?P<v>[0-9]{6})", "text_replacement": { "type": "replacement_string" } })])),
    ];
    // A record's copy, what each scan below starts with (its time apart: subtracted).
    let mut copies: Vec<Record> = records.clone();
    let n = copies.len();
    let mut i = 0;
    let (copy, _, _) = time(&records, |r| {
        copy_into(&mut copies[i % n], r);
        i += 1;
        true
    });
    println!("a record's copy: {copy:.1} ns");
    println!("\n{:<32} {:>12} {:>12} {:>10} {:>12} {:>12} {:>10}", "", "check ns/rec", "allocs/rec", "read", "scan ns/rec", "allocs/rec", "changed");
    for (name, groups) in cases {
        let s = Scanner::compile(&groups, b"bench-key");
        assert!(!s.is_empty(), "{name} compiles");
        let mut scratch = Scratch::default();
        let (check, check_allocs, read) = time(&records, |r| s.reads(r, || r.attrs.iter().map(|(k, v)| (k.as_str(), v.as_str())), &mut scratch));
        // A scan of a copy of each record (the copy's time taken out).
        let (scan, scan_allocs, changed) = time(&records, |r| {
            let c = &mut copies[i % n];
            copy_into(c, r);
            i += 1;
            s.scan(c, &mut scratch)
        });
        println!(
            "{:<32} {:>12.1} {:>12.4} {:>9.2}% {:>12.1} {:>12.4} {:>9.2}%",
            name,
            check,
            check_allocs,
            100.0 * read as f64 / records.len() as f64,
            scan - copy,
            scan_allocs,
            100.0 * changed as f64 / records.len() as f64
        );
    }
}
