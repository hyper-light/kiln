//! A recorded corpus of real series (`docs/observe.md` §12): the `KOBS1` files
//! `scratchpad/obs-data/export-prom.py` writes from a Prometheus.
#![allow(dead_code)]

use std::io::Read;

pub struct Series {
    pub name: String,
    /// 0 gauge, 1 counter, 2 a histogram's or summary's part, 3 unknown.
    pub kind: u8,
    pub ts: Vec<i64>,
    pub values: Vec<f64>,
}

/// Reads a corpus; the path is `KOBS_CORPUS`.
pub fn load() -> Result<Vec<Series>, String> {
    let path = std::env::var("KOBS_CORPUS").map_err(|_| "set KOBS_CORPUS to a KOBS1 corpus (docs/observe.md §12)".to_owned())?;
    let mut raw = Vec::new();
    std::fs::File::open(&path).and_then(|mut f| f.read_to_end(&mut raw)).map_err(|e| format!("{path}: {e}"))?;
    let mut at = raw.strip_prefix(b"KOBS1\n".as_slice()).ok_or("not a KOBS1 corpus")?;
    let mut out = Vec::new();
    let take = |at: &mut &[u8], n: usize| -> Result<Vec<u8>, String> {
        let (a, b) = at.split_at_checked(n).ok_or("corpus truncated")?;
        *at = b;
        Ok(a.to_vec())
    };
    while !at.is_empty() {
        let len = u32::from_le_bytes(take(&mut at, 4)?.try_into().map_err(|_| "len")?) as usize;
        let name = String::from_utf8_lossy(&take(&mut at, len)?).into_owned();
        let kind = *take(&mut at, 1)?.first().ok_or("kind")?;
        let n = u32::from_le_bytes(take(&mut at, 4)?.try_into().map_err(|_| "n")?) as usize;
        let body = take(&mut at, n * 16)?;
        let mut ts = Vec::with_capacity(n);
        let mut values = Vec::with_capacity(n);
        for s in body.as_chunks::<16>().0 {
            let (t, v) = s.split_at(8);
            ts.push(i64::from_le_bytes(t.try_into().map_err(|_| "ts")?));
            values.push(f64::from_le_bytes(v.try_into().map_err(|_| "v")?));
        }
        // Keep strictly increasing timestamps (a scrape can repeat one).
        let mut keep_ts = Vec::with_capacity(n);
        let mut keep_v = Vec::with_capacity(n);
        for (t, v) in ts.into_iter().zip(values) {
            if keep_ts.last().is_none_or(|&l| t > l) {
                keep_ts.push(t);
                keep_v.push(v);
            }
        }
        out.push(Series { name, kind, ts: keep_ts, values: keep_v });
    }
    Ok(out)
}
