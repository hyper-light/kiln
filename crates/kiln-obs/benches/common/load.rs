//! The corpus into a store, as the store bench ingests it (one batch a minute across every series).
#![allow(dead_code, clippy::unwrap_used, clippy::indexing_slicing)]

use kiln_obs::batch::{Builder, Kind, SeriesKey};
use kiln_obs::hash::Keyed;
use kiln_obs::store::Store;

use super::corpus::Series;

/// `name{a="1",b="x\\"y"}` into the metric and its tags.
pub fn parse_series(s: &str) -> (String, Vec<(String, String)>) {
    let Some((name, rest)) = s.split_once('{') else { return (s.to_owned(), Vec::new()) };
    let mut tags = Vec::new();
    let body = rest.strip_suffix('}').unwrap_or(rest);
    let mut chars = body.chars().peekable();
    loop {
        let key: String = chars.by_ref().take_while(|&c| c != '=').collect();
        if key.is_empty() {
            break;
        }
        if chars.next() != Some('"') {
            break;
        }
        let mut val = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(n) = chars.next() {
                        val.push(n);
                    }
                }
                '"' => break,
                c => val.push(c),
            }
        }
        tags.push((key.trim_start_matches(',').to_owned(), val));
        if chars.peek() == Some(&',') {
            chars.next();
        }
    }
    (name.to_owned(), tags)
}

/// The corpus's batches, a minute a batch.
pub fn batches(series: &[Series]) -> Vec<Vec<u8>> {
    let parsed: Vec<(String, Vec<(String, String)>)> = series.iter().map(|s| parse_series(&s.name)).collect();
    let keys: Vec<SeriesKey> = series
        .iter()
        .zip(&parsed)
        .map(|(s, (m, tags))| SeriesKey::new(m, tags.iter().map(|(k, v)| (k.as_str(), v.as_str())), if s.kind == 1 { Kind::Counter } else { Kind::Gauge }).unwrap())
        .collect();
    let t_min = series.iter().filter_map(|s| s.ts.first()).min().copied().unwrap_or(0);
    let t_max = series.iter().filter_map(|s| s.ts.last()).max().copied().unwrap_or(0);
    let mut cursors = vec![0usize; series.len()];
    let mut defined = vec![false; series.len()];
    let mut out = Vec::new();
    let mut b = Builder::new();
    let mut w = t_min;
    while w <= t_max {
        let end = w + 60_000;
        for (i, s) in series.iter().enumerate() {
            let c = cursors[i];
            let n = s.ts[c..].partition_point(|&t| t < end);
            if n == 0 {
                continue;
            }
            if !defined[i] {
                b.define(&keys[i]);
                defined[i] = true;
            }
            b.run(keys[i].fp, s.ts[c..c + n].iter().copied().zip(s.values[c..c + n].iter().copied()));
            cursors[i] = c + n;
        }
        out.push(b.finish());
        w = end;
    }
    out
}

pub fn store_of(series: &[Series]) -> Store {
    let mut s = Store::new(Keyed::new());
    for b in batches(series) {
        s.apply(&b).unwrap();
    }
    s
}
