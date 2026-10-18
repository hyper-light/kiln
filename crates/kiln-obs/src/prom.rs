//! Prometheus's text exposition format into series (`docs/observe.md` §8): what `/metrics`
//! endpoints serve (Prometheus "Exposition formats", text format 0.0.4), kiln's own included.
//!
//! - `# TYPE` names a family's kind: `counter` → counters; `gauge` and `untyped` → gauges;
//!   `histogram`'s `_bucket` (tagged `le`), `_sum` and `_count` → counters; `summary`'s quantiles
//!   (tagged `quantile`) → gauges, its `_sum` and `_count` → counters. A sample of no stated type
//!   is a gauge.
//! - Names keep Prometheus's spelling (PromQL's users know them so); labels are tags, their values
//!   unescaped (`\\`, `\"`, `\n`); a sample's own timestamp (ms) is kept, else the scrape's.
//! - A line that does not parse is counted and skipped, never a panic; past `MAX_TAGS` labels the
//!   sample is skipped.

use std::collections::HashMap;

use crate::batch::{Kind, MAX_TAGS, SeriesKey};

#[derive(Debug, Default)]
pub struct Scraped {
    pub series: Vec<(SeriesKey, Vec<(i64, f64)>)>,
    pub samples: u64,
    /// Lines that did not parse, or whose series could not be made.
    pub skipped: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Counter,
    Gauge,
    Histogram,
    Summary,
}

/// The kind of a sample of family `fam` named `name` (the family's name, `base`).
fn kind_of(fam: Option<Family>, name: &str, base: &str) -> Kind {
    match fam {
        Some(Family::Counter) => Kind::Counter,
        Some(Family::Histogram) => Kind::Counter,
        Some(Family::Summary) if name != base => Kind::Counter,
        _ => Kind::Gauge,
    }
}

/// A sample line's name, labels and value (and its timestamp, if any).
fn sample(line: &str) -> Option<(&str, Vec<(String, String)>, f64, Option<i64>)> {
    let line = line.trim();
    let name_end = line.find(|c: char| c == '{' || c.is_whitespace())?;
    let (name, mut rest) = line.split_at(name_end);
    let mut labels = Vec::new();
    if let Some(r) = rest.strip_prefix('{') {
        // label="value", ... } : values may hold any character, escaped.
        let mut at = r;
        loop {
            at = at.trim_start_matches([' ', ',']);
            if let Some(r) = at.strip_prefix('}') {
                at = r;
                break;
            }
            let eq = at.find('=')?;
            let key = at.get(..eq)?.trim();
            at = at.get(eq + 1..)?.trim_start().strip_prefix('"')?;
            let mut value = String::new();
            let mut chars = at.char_indices();
            let end = loop {
                let (i, c) = chars.next()?;
                match c {
                    '\\' => match chars.next()?.1 {
                        'n' => value.push('\n'),
                        other => value.push(other),
                    },
                    '"' => break i,
                    c => value.push(c),
                }
            };
            at = at.get(end + 1..)?;
            if labels.len() >= MAX_TAGS {
                return None;
            }
            labels.push((key.to_owned(), value));
        }
        rest = at;
    }
    let mut f = rest.split_whitespace();
    let value = match f.next()? {
        "+Inf" | "Inf" => f64::INFINITY,
        "-Inf" => f64::NEG_INFINITY,
        "NaN" => f64::NAN,
        v => v.parse().ok()?,
    };
    let ts = f.next().and_then(|t| t.parse::<i64>().ok());
    Some((name, labels, value, ts))
}

/// Every sample of `text`, at `now_ms` unless it states its own time; `tags` added to each.
pub fn parse(text: &str, now_ms: i64, tags: &[(&str, &str)]) -> Scraped {
    let mut out = Scraped::default();
    let mut families: HashMap<&str, Family> = HashMap::new();
    let mut at: HashMap<Vec<u8>, usize> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(meta) = line.strip_prefix('#') {
            let mut f = meta.split_whitespace();
            if f.next() == Some("TYPE")
                && let (Some(name), Some(kind)) = (f.next(), f.next())
            {
                let fam = match kind {
                    "counter" => Family::Counter,
                    "histogram" => Family::Histogram,
                    "summary" => Family::Summary,
                    _ => Family::Gauge,
                };
                families.insert(name, fam);
            }
            continue;
        }
        let Some((name, mut labels, value, ts)) = sample(line) else {
            out.skipped += 1;
            continue;
        };
        // A histogram's or summary's samples are named after their family with a suffix.
        let base = ["_bucket", "_sum", "_count", "_total", "_created"].iter().find_map(|s| name.strip_suffix(s).filter(|b| families.contains_key(b))).unwrap_or(name);
        let kind = kind_of(families.get(base).copied(), name, base);
        for (k, v) in tags {
            if !labels.iter().any(|(lk, _)| lk == k) {
                labels.push(((*k).to_owned(), (*v).to_owned()));
            }
        }
        if labels.len() > MAX_TAGS {
            out.skipped += 1;
            continue;
        }
        let Ok(key) = SeriesKey::new(name, labels.iter().map(|(k, v)| (k.as_str(), v.as_str())), kind) else {
            out.skipped += 1;
            continue;
        };
        out.samples += 1;
        let point = (ts.unwrap_or(now_ms), value);
        match at.get(&key.key) {
            Some(&i) => {
                if let Some(s) = out.series.get_mut(i) {
                    s.1.push(point);
                }
            }
            None => {
                at.insert(key.key.clone(), out.series.len());
                out.series.push((key, vec![point]));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::parse_key;

    /// The format's own example (prometheus.io "Exposition formats", "Text format example").
    const EXAMPLE: &str = r#"# HELP http_requests_total The total number of HTTP requests.
# TYPE http_requests_total counter
http_requests_total{method="post",code="200"} 1027 1395066363000
http_requests_total{method="post",code="400"}    3 1395066363000

# Escaping in label values:
msdos_file_access_time_seconds{path="C:\\DIR\\FILE.TXT",error="Cannot find file:\n\"FILE.TXT\""} 1.458255915e9

# Minimalistic line:
metric_without_timestamp_and_labels 12.47

# A weird metric from before the epoch:
something_weird{problem="division by zero"} +Inf -3982045

# A histogram, which has a pretty complex representation in the text format:
# HELP http_request_duration_seconds A histogram of the request duration.
# TYPE http_request_duration_seconds histogram
http_request_duration_seconds_bucket{le="0.05"} 24054
http_request_duration_seconds_bucket{le="0.1"} 33444
http_request_duration_seconds_bucket{le="0.2"} 100392
http_request_duration_seconds_bucket{le="0.5"} 129389
http_request_duration_seconds_bucket{le="1"} 133988
http_request_duration_seconds_bucket{le="+Inf"} 144320
http_request_duration_seconds_sum 53423
http_request_duration_seconds_count 144320

# Finally a summary, which has a complex representation, too:
# HELP rpc_duration_seconds A summary of the RPC duration in seconds.
# TYPE rpc_duration_seconds summary
rpc_duration_seconds{quantile="0.01"} 3102
rpc_duration_seconds{quantile="0.05"} 3272
rpc_duration_seconds{quantile="0.5"} 4773
rpc_duration_seconds{quantile="0.9"} 9001
rpc_duration_seconds{quantile="0.99"} 76656
rpc_duration_seconds_sum 1.7560473e+07
rpc_duration_seconds_count 2693
"#;

    fn find<'a>(s: &'a Scraped, name: &str, tag: Option<(&str, &str)>) -> Option<&'a (SeriesKey, Vec<(i64, f64)>)> {
        s.series.iter().find(|(k, _)| parse_key(&k.key).is_ok_and(|p| p.metric == name && tag.is_none_or(|t| p.tags.contains(&t))))
    }

    #[test]
    fn the_format_s_own_example_reads_whole() {
        let s = parse(EXAMPLE, 7, &[("host", "h")]);
        assert_eq!(s.skipped, 0);
        assert_eq!(s.samples, 20);
        let post = find(&s, "http_requests_total", Some(("code", "200"))).unwrap();
        assert_eq!((post.0.kind, post.1.clone()), (Kind::Counter, vec![(1_395_066_363_000, 1027.0)]));
        let file = find(&s, "msdos_file_access_time_seconds", None).unwrap();
        let tags = parse_key(&file.0.key).unwrap().tags;
        assert!(tags.contains(&("path", r"C:\DIR\FILE.TXT")) && tags.contains(&("error", "Cannot find file:\n\"FILE.TXT\"")), "{tags:?}");
        assert_eq!(find(&s, "metric_without_timestamp_and_labels", None).unwrap().1, vec![(7, 12.47)]);
        let weird = find(&s, "something_weird", None).unwrap();
        assert_eq!(weird.1, vec![(-3_982_045, f64::INFINITY)]);
        let inf = find(&s, "http_request_duration_seconds_bucket", Some(("le", "+Inf"))).unwrap();
        assert_eq!((inf.0.kind, inf.1[0].1), (Kind::Counter, 144_320.0));
        assert_eq!(find(&s, "http_request_duration_seconds_sum", None).unwrap().0.kind, Kind::Counter);
        let q = find(&s, "rpc_duration_seconds", Some(("quantile", "0.99"))).unwrap();
        assert_eq!((q.0.kind, q.1[0].1), (Kind::Gauge, 76_656.0));
        assert_eq!(find(&s, "rpc_duration_seconds_count", None).unwrap().0.kind, Kind::Counter);
        // The tags given are added to every sample.
        assert!(s.series.iter().all(|(k, _)| parse_key(&k.key).unwrap().tags.contains(&("host", "h"))));
    }

    #[test]
    fn junk_is_skipped_never_a_panic() {
        let junk = "a{b=\"c} 1\nno_value\nx{y=\"z\"} notanumber\n{} 3\nok 1\n";
        let s = parse(junk, 0, &[]);
        assert_eq!((s.samples, s.skipped), (1, 4));
        let mut rng = crate::testkit::Rng::new(3);
        let bytes = EXAMPLE.as_bytes();
        for _ in 0..3000 {
            let mut b = bytes.to_vec();
            for _ in 0..4 {
                let i = rng.below(b.len() as u64) as usize;
                b[i] = rng.next_u64() as u8;
            }
            let _ = parse(&String::from_utf8_lossy(&b), 0, &[]);
        }
    }
}
