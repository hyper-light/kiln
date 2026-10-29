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
//! - OpenMetrics (`application/openmetrics-text`, OpenMetrics 1.0) reads alike, but for what it
//!   changes: a sample's timestamp is in seconds (a fraction allowed), an exemplar (`# {...} v`)
//!   may follow it, `# EOF` ends it, and a counter's, histogram's or summary's `_created` sample is
//!   its creation's time, not a value: skipped (as Prometheus skips it by default).

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

/// The exposition's format: what a scrape's `Content-Type` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Prometheus's text format 0.0.4: timestamps in milliseconds.
    Text,
    /// OpenMetrics 1.0: timestamps in seconds.
    OpenMetrics,
}

impl Format {
    /// From a response's `Content-Type`.
    pub fn of(content_type: &str) -> Self {
        if content_type.split(';').next().is_some_and(|t| t.trim().eq_ignore_ascii_case("application/openmetrics-text")) { Self::OpenMetrics } else { Self::Text }
    }
}

/// A sample line's name, labels and value (and its timestamp, if any, in milliseconds).
fn sample(line: &str, format: Format) -> Option<(&str, Vec<(String, String)>, f64, Option<i64>)> {
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
    // An exemplar (OpenMetrics: ` # {trace_id="..."} 0.67`) is no part of the sample.
    let rest = rest.split(" # ").next().unwrap_or(rest);
    let mut f = rest.split_whitespace();
    let value = match f.next()? {
        "+Inf" | "Inf" => f64::INFINITY,
        "-Inf" => f64::NEG_INFINITY,
        "NaN" => f64::NAN,
        v => v.parse().ok()?,
    };
    let ts = f.next().and_then(|t| match format {
        Format::Text => t.parse::<i64>().ok(),
        Format::OpenMetrics => t.parse::<f64>().ok().filter(|s| s.is_finite() && s.abs() < 1e13).map(|s| (s * 1000.0).round() as i64),
    });
    Some((name, labels, value, ts))
}

/// Every sample of `text`, at `now_ms` unless it states its own time; `tags` added to each.
pub fn parse(text: &str, now_ms: i64, tags: &[(&str, &str)]) -> Scraped {
    parse_format(text, now_ms, tags, Format::Text)
}

/// [`parse`] of an exposition in `format`.
pub fn parse_format(text: &str, now_ms: i64, tags: &[(&str, &str)], format: Format) -> Scraped {
    parse_inner(text, now_ms, tags, format, false)
}

/// [`parse_format`] as a scrape takes a target's exposition: `tags` (its `job`, `instance`, ...)
/// over the sample's own, a sample's label of the same key kept as `exported_<key>` (Prometheus's
/// `honor_labels: false`, its default).
pub fn parse_target(text: &str, now_ms: i64, tags: &[(&str, &str)], format: Format) -> Scraped {
    parse_inner(text, now_ms, tags, format, true)
}

fn parse_inner(text: &str, now_ms: i64, tags: &[(&str, &str)], format: Format, ours_win: bool) -> Scraped {
    let mut out = Scraped::default();
    let mut families: HashMap<&str, Family> = HashMap::new();
    let mut at: HashMap<std::sync::Arc<[u8]>, usize> = HashMap::new();
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
        let Some((name, mut labels, value, ts)) = sample(line, format) else {
            out.skipped += 1;
            continue;
        };
        // A histogram's or summary's samples are named after their family with a suffix.
        let base = ["_bucket", "_sum", "_count", "_total", "_created"].iter().find_map(|s| name.strip_suffix(s).filter(|b| families.contains_key(b))).unwrap_or(name);
        // A typed family's `_created`: when it was made, no sample of it.
        if base != name && name.ends_with("_created") && families.get(base).is_some_and(|f| *f != Family::Gauge) {
            continue;
        }
        let kind = kind_of(families.get(base).copied(), name, base);
        for (k, v) in tags {
            match labels.iter_mut().find(|(lk, _)| lk == k) {
                None => labels.push(((*k).to_owned(), (*v).to_owned())),
                Some(l) if ours_win => {
                    let theirs = std::mem::replace(&mut l.1, (*v).to_owned());
                    labels.push((format!("exported_{k}"), theirs));
                }
                Some(_) => {}
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
        match at.get(&*key.key) {
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

    /// OpenMetrics' own example's parts (openmetrics.io, the specification's "Text format"): its
    /// timestamps in seconds, exemplars, `_created`, `# EOF`.
    #[test]
    fn openmetrics_reads_its_seconds_its_exemplars_and_its_end() {
        let om = "# TYPE acme_http_router_request_seconds summary\n# UNIT acme_http_router_request_seconds seconds\nacme_http_router_request_seconds_sum{path=\"/api/v1\",method=\"GET\"} 9036.32\nacme_http_router_request_seconds_count{path=\"/api/v1\",method=\"GET\"} 807283.0\nacme_http_router_request_seconds_created{path=\"/api/v1\",method=\"GET\"} 1605281325.0\n# TYPE foo histogram\nfoo_bucket{le=\"0.01\"} 0\nfoo_bucket{le=\"0.1\"} 8 # {trace_id=\"KOO5S4vxi0o\"} 0.054 1520879607.789\nfoo_bucket{le=\"+Inf\"} 17 1520879607.0\nfoo_count 17\nfoo_sum 324789.3\nfoo_created 1520430000.123\n# TYPE go_goroutines gauge\ngo_goroutines 69 1520879607.5\n# EOF\n";
        let s = parse_format(om, 7, &[], Format::OpenMetrics);
        assert_eq!(s.skipped, 0);
        assert_eq!(s.samples, 8, "{:?}", s.series.iter().map(|(k, _)| parse_key(&k.key).unwrap().metric).collect::<Vec<_>>());
        assert!(find(&s, "foo_created", None).is_none() && find(&s, "acme_http_router_request_seconds_created", None).is_none());
        assert_eq!(find(&s, "foo_bucket", Some(("le", "0.1"))).unwrap().1, vec![(7, 8.0)], "the exemplar no part of it");
        assert_eq!(find(&s, "foo_bucket", Some(("le", "+Inf"))).unwrap().1, vec![(1_520_879_607_000, 17.0)]);
        assert_eq!(find(&s, "go_goroutines", None).unwrap().1, vec![(1_520_879_607_500, 69.0)]);
        // The same in the text format reads its timestamp as milliseconds.
        assert_eq!(find(&parse("go_goroutines 69 1520879607500\n", 7, &[]), "go_goroutines", None).unwrap().1, vec![(1_520_879_607_500, 69.0)]);
        assert_eq!(Format::of("application/openmetrics-text; version=1.0.0; charset=utf-8"), Format::OpenMetrics);
        assert_eq!(Format::of("text/plain; version=0.0.4"), Format::Text);
    }

    /// A scrape's target labels win, the sample's own kept as `exported_` (Prometheus's
    /// `honor_labels: false`); kiln's own scrape keeps a sample's.
    #[test]
    fn a_target_s_labels_win_and_the_sample_s_are_kept_aside() {
        let text = "up{job=\"theirs\",x=\"1\"} 1\n";
        let s = parse_target(text, 0, &[("job", "node"), ("instance", "h:9100")], Format::Text);
        let tags = parse_key(&s.series[0].0.key).unwrap().tags;
        assert!(tags.contains(&("job", "node")) && tags.contains(&("exported_job", "theirs")) && tags.contains(&("instance", "h:9100")) && tags.contains(&("x", "1")), "{tags:?}");
        let s = parse(text, 0, &[("job", "node")]);
        assert!(parse_key(&s.series[0].0.key).unwrap().tags.contains(&("job", "theirs")));
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
