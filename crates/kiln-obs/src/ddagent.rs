//! Datadog's Agent's metric payloads (`docs/observe.md` §8.6), as its intake reads them: series
//! (`/api/v2/series`, the `agent-payload` repository's `MetricPayload`) and distributions' sketches
//! (`/api/beta/sketches`, its `SketchPayload`). What an Agent 7.84 sends was recorded and is what
//! the tests hold these to.
//!
//! - A series' tags are its `key:value` strings (a key once: its first value; a tag of no colon a
//!   key with no value), its host its `host` resource's (`host` beside its tags), a `device`
//!   resource a `device` tag; its points' times are seconds.
//! - Its type: a gauge a gauge; a count a count; a rate (what DogStatsD's counts, and a histogram's
//!   `.count`, are sent as: the count over its interval, a second) the count it was, its value
//!   times its interval, so an application's counter is the same series whether kiln's DogStatsD
//!   took it or an Agent did (`.as_rate()` reads it a second, as Datadog shows it). A rate of no
//!   interval is a gauge.
//! - A sketch is the Agent's DDSketch (`pkg/quantile`): a bin's key is the logarithm base γ = 1 +
//!   1/64 of its value, rounded to the even, offset by 1338 (negative for negative values, 0 for
//!   zero), each with its count; besides them the exact count, sum, least and most. Its bins are
//!   counted into kiln's distribution (§4.4), each at its value (within 0.78% of the values in it),
//!   its count, sum, least and most as they came.

use std::collections::BTreeMap;

use crate::batch::{Kind, MAX_TAGS, SeriesKey};
use crate::dist::{self, BUCKET_TAG};
use crate::otlp::metrics::{Decoded, Keys};
use crate::otlp::pb::{Fields, PbError, varints};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    #[error("{0}")]
    Pb(#[from] PbError),
    #[error("a string that is not UTF-8")]
    Utf8,
}

/// What a payload held.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Taken {
    pub series: u64,
    pub points: u64,
    /// Series that are none (no name, too many tags, a name too long) and their points; points
    /// whose value is not a number.
    pub invalid: u64,
}

/// The Agent's sketches' γ, and the key of 1 (`pkg/quantile`'s default configuration: a relative
/// accuracy of 1/128, γ = 1 + 2/128, the least value 1e-9 at key 1).
const GAMMA_LN: f64 = 0.015_504_186_535_965_254; // ln(1 + 1/64)
const BIAS: i64 = 1338;

/// A sketch bin's value: γ to its key less the offset.
pub fn bin_value(key: i64) -> f64 {
    match key {
        0 => 0.0,
        k if k > 0 => ((k - BIAS) as f64 * GAMMA_LN).exp(),
        k => -((-k - BIAS) as f64 * GAMMA_LN).exp(),
    }
}

/// A tag string's key and value.
fn split(t: &str) -> (&str, &str) {
    t.split_once(':').unwrap_or((t, ""))
}

/// Tags into `out` (cleared): the series' strings, each key once, then `host` and `device` where
/// none of the tags is of that key; sorted by key. Whether they make a series (none past
/// [`MAX_TAGS`]).
fn tags_into<'a>(strings: &[&'a str], host: Option<&'a str>, device: Option<&'a str>, out: &mut Vec<(&'a str, &'a str)>) -> bool {
    out.clear();
    for t in strings {
        let (k, v) = split(t);
        if k.is_empty() || out.iter().any(|(x, _)| *x == k) {
            continue;
        }
        out.push((k, v));
    }
    for (k, v) in [("host", host), ("device", device)] {
        if let Some(v) = v.filter(|v| !v.is_empty())
            && !out.iter().any(|(x, _)| *x == k)
        {
            out.push((k, v));
        }
    }
    out.sort_unstable_by(|a, b| a.0.cmp(b.0));
    out.len() <= MAX_TAGS
}

/// A `MetricPayload` into `out` (cleared; its room kept), each series' key found in or made into
/// `keys`.
pub fn series_into(body: &[u8], keys: &Keys, out: &mut Decoded) -> Result<Taken, AgentError> {
    out.keys.clear();
    out.samples.clear();
    out.points = 0;
    out.dropped = Default::default();
    let mut taken = Taken::default();
    let (mut strings, mut tags, mut key, mut points): (Vec<&str>, Vec<(&str, &str)>, Vec<u8>, Vec<(i64, f64)>) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for f in Fields::new(body) {
        let (1, w) = f? else { continue };
        let (mut metric, mut host, mut device, mut kind, mut interval) = ("", None, None, 0u64, 0i64);
        strings.clear();
        points.clear();
        for g in Fields::new(w.bytes().unwrap_or_default()) {
            match g? {
                (1, w) => {
                    let (mut rtype, mut name) = ("", "");
                    for h in Fields::new(w.bytes().unwrap_or_default()) {
                        match h? {
                            (1, w) => rtype = w.str().ok_or(AgentError::Utf8)?,
                            (2, w) => name = w.str().ok_or(AgentError::Utf8)?,
                            _ => {}
                        }
                    }
                    match rtype {
                        "host" => host = Some(name),
                        "device" => device = Some(name),
                        _ => {}
                    }
                }
                (2, w) => metric = w.str().ok_or(AgentError::Utf8)?,
                (3, w) => strings.push(w.str().ok_or(AgentError::Utf8)?),
                (4, w) => {
                    let (mut v, mut ts) = (f64::NAN, 0i64);
                    for h in Fields::new(w.bytes().unwrap_or_default()) {
                        match h? {
                            (1, w) => v = w.f64().unwrap_or(f64::NAN),
                            (2, w) => ts = w.i64().unwrap_or(0),
                            _ => {}
                        }
                    }
                    points.push((ts.saturating_mul(1000), v));
                }
                (5, w) => kind = w.u64().unwrap_or(0),
                (8, w) => interval = w.i64().unwrap_or(0),
                _ => {}
            }
        }
        // MetricType: 1 a count, 2 a rate, 3 a gauge (0: unspecified, a gauge).
        let (kind, scale) = match (kind, interval) {
            (1, _) => (Kind::Count, 1.0),
            (2, i) if i > 0 => (Kind::Count, i as f64),
            _ => (Kind::Gauge, 1.0),
        };
        let made = if metric.is_empty() || !tags_into(&strings, host, device, &mut tags) { None } else { keys.key(metric, &tags, kind, &mut key) };
        let Some(k) = made else {
            taken.invalid += 1 + points.len() as u64;
            continue;
        };
        let place = out.keys.len() as u32;
        let mut any = false;
        for &(ts, v) in &points {
            if !v.is_finite() {
                taken.invalid += 1;
                continue;
            }
            out.samples.push((place, ts, v * scale));
            out.points += 1;
            taken.points += 1;
            any = true;
        }
        if any {
            out.keys.push(k);
            taken.series += 1;
        }
    }
    Ok(taken)
}

/// A `SketchPayload` as kiln's distributions: each sketch's bins counted into its buckets of
/// [`dist::SCALE`] (`<metric>.dist`), and its `.count`, `.sum`, `.min` and `.max`, a point each of
/// its intervals.
pub fn sketches(body: &[u8], keys: &Keys) -> Result<(Vec<(SeriesKey, Vec<(i64, f64)>)>, Taken), AgentError> {
    let mut out: Vec<(SeriesKey, Vec<(i64, f64)>)> = Vec::new();
    let mut taken = Taken::default();
    let (mut strings, mut tags, mut key): (Vec<&str>, Vec<(&str, &str)>, Vec<u8>) = (Vec::new(), Vec::new(), Vec::new());
    let (mut k, mut n): (Vec<u64>, Vec<u64>) = (Vec::new(), Vec::new());
    for f in Fields::new(body) {
        let (1, w) = f? else { continue };
        let (mut metric, mut host) = ("", None);
        strings.clear();
        let mut dogs: Vec<&[u8]> = Vec::new();
        for g in Fields::new(w.bytes().unwrap_or_default()) {
            match g? {
                (1, w) => metric = w.str().ok_or(AgentError::Utf8)?,
                (2, w) => host = Some(w.str().ok_or(AgentError::Utf8)?),
                (4, w) => strings.push(w.str().ok_or(AgentError::Utf8)?),
                (7, w) => dogs.push(w.bytes().unwrap_or_default()),
                _ => {}
            }
        }
        if metric.is_empty() || !tags_into(&strings, host, None, &mut tags) {
            taken.invalid += 1;
            continue;
        }
        // Each interval's figures and its bins, by kiln's bucket.
        let mut figures: [Vec<(i64, f64)>; 4] = Default::default();
        let mut buckets: BTreeMap<(u8, i64), (dist::Bucket, Vec<(i64, f64)>)> = BTreeMap::new();
        for d in dogs {
            let (mut ts, mut cnt, mut min, mut max, mut sum) = (0i64, 0i64, f64::NAN, f64::NAN, f64::NAN);
            k.clear();
            n.clear();
            for h in Fields::new(d) {
                match h? {
                    (1, w) => ts = w.i64().unwrap_or(0),
                    (2, w) => cnt = w.i64().unwrap_or(0),
                    (3, w) => min = w.f64().unwrap_or(f64::NAN),
                    (4, w) => max = w.f64().unwrap_or(f64::NAN),
                    (6, w) => sum = w.f64().unwrap_or(f64::NAN),
                    (7, w) => varints(&w, &mut k)?,
                    (8, w) => varints(&w, &mut n)?,
                    _ => {}
                }
            }
            let at = ts.saturating_mul(1000);
            for (&zk, &c) in k.iter().zip(&n) {
                // Keys are sint32: zigzag.
                let key = ((zk >> 1) as i64) ^ -((zk & 1) as i64);
                if c == 0 {
                    continue;
                }
                if let Some(b) = dist::bucket_of(bin_value(key), dist::SCALE) {
                    let e = buckets.entry(b.order()).or_insert_with(|| (b, Vec::new()));
                    match e.1.last_mut() {
                        Some(last) if last.0 == at => last.1 += c as f64,
                        _ => e.1.push((at, c as f64)),
                    }
                }
            }
            for (i, v) in [cnt as f64, sum, min, max].into_iter().enumerate() {
                if v.is_finite()
                    && let Some(f) = figures.get_mut(i)
                {
                    f.push((at, v));
                }
            }
            taken.points += 1;
        }
        let dist_name = format!("{metric}{}", dist::SUFFIX);
        for (b, pts) in buckets.into_values() {
            let value = dist::tag_value(dist::SCALE, b);
            let mut with = tags.clone();
            with.push((BUCKET_TAG, value.as_str()));
            with.sort_unstable_by(|a, b| a.0.cmp(b.0));
            if let Some(sk) = keys.key(&dist_name, &with, Kind::Count, &mut key) {
                out.push((sk, pts));
            }
        }
        for ((suffix, kind), pts) in [(".count", Kind::Count), (".sum", Kind::Count), (".min", Kind::Gauge), (".max", Kind::Gauge)].into_iter().zip(figures) {
            if pts.is_empty() {
                continue;
            }
            if let Some(sk) = keys.key(&format!("{metric}{suffix}"), &tags, kind, &mut key) {
                out.push((sk, pts));
            }
        }
        taken.series += 1;
    }
    Ok((out, taken))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::otlp::pb::Writer;

    /// What an Agent 7.84 sent for DogStatsD's distribution values 1, 10, 100, 1000, 0.5 and -3,
    /// three of each: these keys. Read back, each bin's value is the one sent, within the half
    /// bin's 0.78%.
    #[test]
    fn the_agent_s_sketch_keys_read_back_as_the_values_sent() {
        for (key, value) in [(1338, 1.0), (1487, 10.0), (1635, 100.0), (1784, 1000.0), (1293, 0.5), (-1409, -3.0)] {
            let v = bin_value(key);
            assert!((v - value).abs() <= value.abs() * 0.0078, "key {key}: {v}, sent {value}");
        }
        assert_eq!(bin_value(0), 0.0);
    }

    fn series(w: &mut Writer, metric: &str, tags: &[&str], host: &str, kind: u64, interval: i64, points: &[(i64, f64)]) {
        w.message(1, |s| {
            s.message(1, |r| {
                r.str(1, "host");
                r.str(2, host);
            });
            s.str(2, metric);
            for t in tags {
                s.str(3, t);
            }
            for (ts, v) in points {
                s.message(4, |p| {
                    p.double(1, *v);
                    p.varint(2, *ts as u64);
                });
            }
            s.varint(5, kind);
            s.varint(8, interval as u64);
        });
    }

    fn read(d: &Decoded) -> Vec<(String, Kind, Vec<(i64, f64)>)> {
        d.keys
            .iter()
            .enumerate()
            .map(|(i, k)| {
                let p = crate::batch::parse_key(&k.key).unwrap();
                let tags: Vec<String> = p.tags.iter().map(|(a, b)| format!("{a}:{b}")).collect();
                (format!("{}{{{}}}", p.metric, tags.join(",")), k.kind, d.samples.iter().filter(|s| s.0 as usize == i).map(|s| (s.1, s.2)).collect())
            })
            .collect()
    }

    #[test]
    fn a_metric_payload_s_series_keep_datadog_s_meaning() {
        let mut w = Writer::default();
        series(&mut w, "system.load.1", &["env:prod", "role:web", "role:db"], "web-1", 3, 0, &[(1_800_000_000, 0.5)]);
        // A DogStatsD count, sent as a rate over its 10 s: the count it was.
        series(&mut w, "app.requests", &["route:/a"], "web-1", 2, 10, &[(1_800_000_010, 1.5)]);
        series(&mut w, "app.errors", &[], "web-1", 1, 15, &[(1_800_000_010, 3.0)]);
        series(&mut w, "", &[], "web-1", 3, 0, &[(1_800_000_000, 1.0)]);
        series(&mut w, "app.temp", &["flag"], "", 0, 0, &[(1_800_000_000, f64::NAN), (1_800_000_015, 21.0)]);
        let (keys, mut d) = (Keys::new(), Decoded::default());
        let t = series_into(&w.buf, &keys, &mut d).unwrap();
        assert_eq!(
            read(&d),
            vec![
                ("system.load.1{env:prod,host:web-1,role:web}".into(), Kind::Gauge, vec![(1_800_000_000_000, 0.5)]),
                ("app.requests{host:web-1,route:/a}".into(), Kind::Count, vec![(1_800_000_010_000, 15.0)]),
                ("app.errors{host:web-1}".into(), Kind::Count, vec![(1_800_000_010_000, 3.0)]),
                ("app.temp{flag:}".into(), Kind::Gauge, vec![(1_800_000_015_000, 21.0)]),
            ]
        );
        assert_eq!((t.series, t.points, t.invalid), (4, 4, 3));
    }

    #[test]
    fn a_sketch_payload_is_a_distribution_s_buckets_and_figures() {
        let mut w = Writer::default();
        w.message(1, |s| {
            s.str(1, "lat");
            s.str(2, "web-1");
            s.str(4, "env:prod");
            s.message(7, |d| {
                d.varint(1, 1_800_000_000);
                d.varint(2, 6);
                d.double(3, 1.0);
                d.double(4, 100.0);
                d.double(6, 333.0);
                // Keys 1338 (1) three times, 1635 (100) three times: sint32, zigzag, packed.
                let mut ks = Vec::new();
                for k in [1338i64, 1635] {
                    crate::varint::put(&mut ks, ((k << 1) ^ (k >> 63)) as u64);
                }
                d.bytes(7, &ks);
                d.bytes(8, &[3, 3]);
            });
        });
        let keys = Keys::new();
        let (series, t) = sketches(&w.buf, &keys).unwrap();
        assert_eq!((t.series, t.points), (1, 1));
        let named: Vec<(String, Vec<(i64, f64)>)> = series
            .iter()
            .map(|(k, p)| {
                let key = crate::batch::parse_key(&k.key).unwrap();
                (format!("{}{:?}", key.metric, key.tags), p.clone())
            })
            .collect();
        let get = |m: &str| named.iter().filter(|(n, _)| n.starts_with(m)).map(|(_, p)| p.clone()).collect::<Vec<_>>();
        assert_eq!(get("lat.count"), vec![vec![(1_800_000_000_000, 6.0)]]);
        assert_eq!(get("lat.sum"), vec![vec![(1_800_000_000_000, 333.0)]]);
        assert_eq!((get("lat.min"), get("lat.max")), (vec![vec![(1_800_000_000_000, 1.0)]], vec![vec![(1_800_000_000_000, 100.0)]]));
        let buckets = get("lat.dist");
        assert_eq!(buckets.len(), 2);
        assert!(buckets.iter().all(|p| p == &vec![(1_800_000_000_000, 3.0)]));
        assert!(named.iter().all(|(n, _)| n.contains("(\"env\", \"prod\")") && n.contains("(\"host\", \"web-1\")")), "{named:?}");
    }

    #[test]
    fn whatever_comes_is_an_error_or_series() {
        let keys = Keys::new();
        let mut d = Decoded::default();
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x % 80) as usize;
            let junk: Vec<u8> = (0..len).map(|i| (x >> (i % 8 * 8)) as u8 ^ (i as u8).wrapping_mul(13)).collect();
            let _ = series_into(&junk, &keys, &mut d);
            let _ = sketches(&junk, &keys);
        }
    }
}
