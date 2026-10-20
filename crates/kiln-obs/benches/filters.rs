//! Log blocks' token filters across blocks (`docs/observe.md` §12.7): a word no block holds, how
//! many blocks' filters pass it. Each filter is sized for its block's tokens at 1%, so a word
//! should pass about 1% of them; but filters of one size put a token at the same places in every
//! one, and the tokens every block holds (a service's name, a level, `the`) set those places in
//! every one, so a word whose places fall on them passes most. Measured both ways: the filters as
//! log blocks have them, and salted by their block as span blocks' are (`bloom::salted`).
//!
//! `KOBS_LOGS=<file>[:<file>...] cargo bench -p kiln-obs --bench filters`: each file's lines a log
//! record's message, sealed into blocks of 256 KiB as the log store seals them.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

use kiln_obs::logs::block::{BLOCK_TEXT, LogBlock};
use kiln_obs::logs::bloom::{salted, token_hashes};
use kiln_obs::logs::record::Record;

fn main() {
    let Ok(paths) = std::env::var("KOBS_LOGS") else {
        eprintln!("KOBS_LOGS=<file>[:<file>...]: the log lines to seal");
        return;
    };
    let mut blocks: Vec<(LogBlock, LogBlock)> = Vec::new();
    let (mut recs, mut bytes, mut lines) = (Vec::new(), 0usize, 0usize);
    let seal = |recs: &mut Vec<Record>, blocks: &mut Vec<(LogBlock, LogBlock)>| {
        if recs.is_empty() {
            return;
        }
        let plain = LogBlock::seal_owned(recs);
        // Salted: the same tokens (the lines hold no attributes nor trace ids), salted by the block.
        let mut raw = Vec::new();
        let mut prev = 0;
        for r in recs.iter() {
            kiln_obs::logs::record::put_row(&mut raw, r, prev);
            prev = r.ts;
        }
        let mut rows = kiln_obs::logs::record::Rows::new(&raw, recs.len());
        let mut refs = Vec::new();
        while let Ok(Some(r)) = rows.next_row() {
            refs.push(r);
        }
        let salty = LogBlock::seal_in(&refs, true);
        blocks.push((plain, salty));
        recs.clear();
    };
    for path in paths.split(':') {
        let text = std::fs::read_to_string(path).unwrap();
        for line in text.lines() {
            lines += 1;
            bytes += line.len() + 40;
            recs.push(Record { ts: lines as i64, severity: 9, service: "kiln".into(), host: "h".into(), source: "journal".into(), message: line.to_owned(), ..Record::default() });
            if bytes >= BLOCK_TEXT {
                seal(&mut recs, &mut blocks);
                bytes = 0;
            }
        }
    }
    seal(&mut recs, &mut blocks);
    let sizes: std::collections::BTreeMap<usize, usize> = blocks.iter().fold(Default::default(), |mut m, (b, _)| {
        *m.entry(b.bloom.bits.len()).or_default() += 1;
        m
    });
    let most_of_one = sizes.values().max().copied().unwrap_or(0);
    println!("{lines} lines in {} blocks; {} filter sizes, the commonest {} blocks", blocks.len(), sizes.len(), most_of_one);
    // Words no block holds.
    let queries = 2_000;
    let mut plain: Vec<usize> = Vec::with_capacity(queries);
    let mut salty: Vec<usize> = Vec::with_capacity(queries);
    for q in 0..queries {
        let word = format!("absentword{q}x{}", q * 7919);
        let h = token_hashes(&word);
        plain.push(blocks.iter().filter(|(b, _)| b.bloom.may_contain(h)).count());
        salty.push(blocks.iter().filter(|(_, s)| s.bloom.may_contain(salted(h, s.salt()))).count());
    }
    for (name, mut v) in [("as logs have them", plain), ("salted by their block", salty)] {
        v.sort_unstable();
        let n = v.len();
        let mean = v.iter().sum::<usize>() as f64 / n as f64;
        println!(
            "{name:<24} blocks a word passes: mean {mean:.1} ({:.2}% of {}), median {}, p90 {}, p99 {}, most {}",
            100.0 * mean / blocks.len() as f64,
            blocks.len(),
            v[n / 2],
            v[n * 9 / 10],
            v[n * 99 / 100],
            v[n - 1]
        );
    }
}
