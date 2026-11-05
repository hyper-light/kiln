//! What a round measured: the lab's own samples by class, and the server's figures from its
//! `/metrics` (read before and after the window), each a number of the round's.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What the lab times, as a person, an agent and a runner meet the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// A page of the UI and its files.
    Page,
    /// An API read (the UI's lists and an agent's page, runs).
    ApiRead,
    /// An API change: messages sent to agents, agents' MCP calls.
    ApiWrite,
    /// An agent's event, from its harness writing it to a viewer's socket bringing it.
    Websocket,
    /// A viewer opening an agent's conversation: its socket to the whole of it.
    Attach,
}

impl Class {
    pub const ALL: [Class; 5] = [Class::Page, Class::ApiRead, Class::ApiWrite, Class::Websocket, Class::Attach];

    pub fn name(self) -> &'static str {
        match self {
            Class::Page => "page",
            Class::ApiRead => "api_read",
            Class::ApiWrite => "api_write",
            Class::Websocket => "websocket",
            Class::Attach => "attach",
        }
    }
}

/// One timed thing: its class, when it began (µs since the epoch), how long it took (µs), whether it
/// went as it should (an answer under 400; a socket that opened), and what it was, finer than its
/// class (an API read's route: `sessions`, `events`…; empty where its class says it all).
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub class: Class,
    pub at_us: u64,
    pub took_us: u64,
    pub ok: bool,
    pub what: &'static str,
}

/// `q` of sorted `v` by the nearest rank (the smallest value with at least `q` of them at or under
/// it).
pub fn quantile(sorted: &[u64], q: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted.get(rank - 1).copied()
}

/// What the lab saw of one class in the window.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Seen {
    pub n: u64,
    pub errors: u64,
    pub p50_ms: Option<f64>,
    pub p99_ms: Option<f64>,
    pub p999_ms: Option<f64>,
    pub max_ms: Option<f64>,
}

#[allow(clippy::cast_precision_loss)]
fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

/// The figures of `took` (µs each, in any order), `errors` beside them: each to the microsecond.
pub fn seen_of(mut took: Vec<u64>, errors: u64) -> Seen {
    took.sort_unstable();
    Seen {
        n: took.len() as u64,
        errors,
        p50_ms: quantile(&took, 0.5).map(ms),
        p99_ms: quantile(&took, 0.99).map(ms),
        p999_ms: quantile(&took, 0.999).map(ms),
        max_ms: took.last().copied().map(ms),
    }
}

/// Each class's figures from the window's samples.
pub fn seen(samples: &[Sample]) -> BTreeMap<Class, Seen> {
    let mut out = BTreeMap::new();
    for c in Class::ALL {
        let took: Vec<u64> = samples.iter().filter(|s| s.class == c && s.ok).map(|s| s.took_us).collect();
        let errors = samples.iter().filter(|s| s.class == c && !s.ok).count() as u64;
        out.insert(c, seen_of(took, errors));
    }
    out
}

/// The figures of each kind of sample finer than its class (`api_read.sessions`, `api_read.events`…):
/// where a class's tail is. Kept beside the numbers an A/B compares, not among them.
pub fn seen_by_what(samples: &[Sample]) -> BTreeMap<String, Seen> {
    let mut by: BTreeMap<String, (Vec<u64>, u64)> = BTreeMap::new();
    for s in samples.iter().filter(|s| !s.what.is_empty()) {
        let e = by.entry(format!("{}.{}", s.class.name(), s.what)).or_default();
        if s.ok {
            e.0.push(s.took_us);
        } else {
            e.1 += 1;
        }
    }
    by.into_iter().map(|(k, (took, errors))| (k, seen_of(took, errors))).collect()
}

// ------------------------------------------------------------------------------- the server's

/// A Prometheus text exposition, as series: each one's name, its labels in order, and its value.
#[derive(Clone, Debug, Default)]
pub struct Scrape {
    pub series: Vec<(String, Vec<(String, String)>, f64)>,
}

impl Scrape {
    pub fn parse(text: &str) -> Self {
        let mut series = Vec::new();
        for line in text.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let (head, value) = match line.rsplit_once(' ') {
                Some(x) => x,
                None => continue,
            };
            let Ok(value) = value.trim().parse::<f64>() else { continue };
            let (name, labels) = match head.split_once('{') {
                Some((n, rest)) => (n, rest.trim_end_matches('}')),
                None => (head, ""),
            };
            series.push((name.to_owned(), labels_of(labels), value));
        }
        Self { series }
    }

    /// The value of the series `name` whose labels include each of `want`.
    pub fn get(&self, name: &str, want: &[(&str, &str)]) -> Option<f64> {
        self.series.iter().find(|(n, l, _)| n == name && want.iter().all(|(k, v)| l.iter().any(|(lk, lv)| lk == k && lv == v))).map(|(_, _, v)| *v)
    }

    /// Every value of label `key` that series `name` has.
    pub fn values_of(&self, name: &str, key: &str) -> Vec<String> {
        let mut out: Vec<String> = self.series.iter().filter(|(n, _, _)| n == name).filter_map(|(_, l, _)| l.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())).collect();
        out.sort();
        out.dedup();
        out
    }

    /// Histogram `name`'s cumulative buckets (upper bound, count) for the series with `want`,
    /// `+Inf` last.
    pub fn buckets(&self, name: &str, want: &[(&str, &str)]) -> Vec<(f64, f64)> {
        let bucket = format!("{name}_bucket");
        let mut out: Vec<(f64, f64)> = self
            .series
            .iter()
            .filter(|(n, l, _)| *n == bucket && want.iter().all(|(k, v)| l.iter().any(|(lk, lv)| lk == k && lv == v)))
            .filter_map(|(_, l, v)| {
                let le = l.iter().find(|(k, _)| k == "le")?.1.as_str();
                let bound = if le == "+Inf" { f64::INFINITY } else { le.parse().ok()? };
                Some((bound, *v))
            })
            .collect();
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        out
    }
}

fn labels_of(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some((k, after)) = rest.split_once("=\"") {
        let mut value = String::new();
        let mut chars = after.char_indices();
        let mut end = after.len();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => {
                    if let Some((_, n)) = chars.next() {
                        value.push(if n == 'n' { '\n' } else { n });
                    }
                }
                '"' => {
                    end = i + 1;
                    break;
                }
                c => value.push(c),
            }
        }
        out.push((k.trim_start_matches(',').trim().to_owned(), value));
        rest = after.get(end..).unwrap_or("");
    }
    out
}

/// Two scrapes' histogram buckets, the second less the first: what the window added.
pub fn delta(before: &[(f64, f64)], after: &[(f64, f64)]) -> Vec<(f64, f64)> {
    after.iter().map(|(b, n)| (*b, n - before.iter().find(|(bb, _)| bb == b).map_or(0.0, |(_, m)| *m))).collect()
}

/// `q` of a histogram's cumulative buckets, as Prometheus's `histogram_quantile` takes it: linear
/// within the bucket it falls in; the last finite bound where it falls past every one.
pub fn histogram_quantile(q: f64, buckets: &[(f64, f64)]) -> Option<f64> {
    let total = buckets.last()?.1;
    if total <= 0.0 {
        return None;
    }
    let rank = q * total;
    let mut lower = (0.0, 0.0);
    for &(bound, count) in buckets {
        if count >= rank {
            if bound.is_infinite() {
                return Some(lower.0);
            }
            let width = count - lower.1;
            let within = if width > 0.0 { (rank - lower.1) / width } else { 1.0 };
            return Some(lower.0 + (bound - lower.0) * within);
        }
        lower = (bound, count);
    }
    Some(lower.0)
}

/// The largest bound any of a histogram's observations reached (`+Inf`: past its last bound).
pub fn histogram_top(buckets: &[(f64, f64)]) -> Option<f64> {
    let total = buckets.last()?.1;
    if total <= 0.0 {
        return None;
    }
    buckets.iter().find(|(_, n)| *n >= total).map(|(b, _)| *b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 10, plan 01a1a87b2dae step 1: each route's figures, to the microsecond, apart from its
    /// class's.
    #[test]
    fn g10_s1_each_route_is_seen_to_the_microsecond() {
        let s = |what: &'static str, took_us: u64, ok: bool| Sample { class: Class::ApiRead, at_us: 0, took_us, ok, what };
        let by = seen_by_what(&[s("sessions", 1_234, true), s("sessions", 17, true), s("events", 999, true), s("events", 5, false), s("", 3, true)]);
        assert_eq!(by.keys().cloned().collect::<Vec<_>>(), vec!["api_read.events".to_owned(), "api_read.sessions".to_owned()]);
        let sessions = by.get("api_read.sessions").cloned().unwrap_or_default();
        assert_eq!((sessions.n, sessions.p50_ms, sessions.max_ms), (2, Some(0.017), Some(1.234)));
        assert_eq!(by.get("api_read.events").map(|e| (e.n, e.errors)), Some((1, 1)));
    }

    #[test]
    fn quantiles_by_the_nearest_rank() {
        let v: Vec<u64> = (1..=1000).collect();
        assert_eq!(quantile(&v, 0.5), Some(500));
        assert_eq!(quantile(&v, 0.99), Some(990));
        assert_eq!(quantile(&v, 0.999), Some(999));
        assert_eq!(quantile(&[7], 0.999), Some(7));
        assert_eq!(quantile(&[], 0.5), None);
    }

    #[test]
    fn a_scrape_reads_series_labels_and_histograms() {
        let text = "# HELP x y\n# TYPE x histogram\nkiln_route_seconds_bucket{class=\"page\",le=\"0.001\"} 3\nkiln_route_seconds_bucket{class=\"page\",le=\"0.01\"} 9\nkiln_route_seconds_bucket{class=\"page\",le=\"+Inf\"} 10\nkiln_route_seconds_count{class=\"page\"} 10\nprocess_cpu_seconds_total 12.5\nweird{a=\"q\\\"uote\",b=\"2\"} 1\n";
        let s = Scrape::parse(text);
        assert_eq!(s.get("process_cpu_seconds_total", &[]), Some(12.5));
        assert_eq!(s.get("kiln_route_seconds_count", &[("class", "page")]), Some(10.0));
        assert_eq!(s.get("weird", &[("a", "q\"uote"), ("b", "2")]), Some(1.0));
        let b = s.buckets("kiln_route_seconds", &[("class", "page")]);
        assert_eq!(b, vec![(0.001, 3.0), (0.01, 9.0), (f64::INFINITY, 10.0)]);
        // The 50th of 10 is the 5th: two of the six in (1 ms, 10 ms].
        let p50 = histogram_quantile(0.5, &b).unwrap_or_default();
        assert!((p50 - 0.004).abs() < 1e-9, "{p50}");
        // Past the last bound: the last finite bound.
        assert_eq!(histogram_quantile(0.99, &b), Some(0.01));
        assert_eq!(histogram_top(&b), Some(f64::INFINITY));
        assert_eq!(delta(&[(0.001, 1.0), (f64::INFINITY, 2.0)], &[(0.001, 3.0), (f64::INFINITY, 5.0)]), vec![(0.001, 2.0), (f64::INFINITY, 3.0)]);
    }
}
