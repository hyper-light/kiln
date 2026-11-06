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
/// class (an API read's route: `sessions`, `events`…; empty where its class says it all); and what the
/// server said of its own time with it, where it was asked (an API read: [`split`]).
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub class: Class,
    pub at_us: u64,
    pub took_us: u64,
    pub ok: bool,
    pub what: &'static str,
    pub server: Option<crate::client::ServerTime>,
}

/// Where the slowest of a class's samples went ([`split`]), in milliseconds, each the mean over them, so the three
/// add up to their mean as the lab saw it: the server's wait before its handler started (from the request's head's
/// arrival off its socket), its handler's time, and the rest (the request's way to the server and its answer's
/// back, the answer's transfer, the client's own reading of it); and how many samples they were.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Split {
    pub n: usize,
    pub wait_ms: f64,
    pub took_ms: f64,
    pub rest_ms: f64,
}

/// The samples of `class` that went as they should and that the server told its time with, at or past their
/// `q`-quantile as the lab saw them, split ([`Split`]); none without such samples. Goal 10, plan 01a1a87b2dae step
/// 5: the server's wait and took at p99.9 are each the class's, and percentiles don't add up, so only each slow
/// read's own split says what its time went to.
pub fn split(samples: &[Sample], class: Class, q: f64) -> Option<Split> {
    let mut told: Vec<(u64, crate::client::ServerTime)> = samples.iter().filter(|s| s.class == class && s.ok).filter_map(|s| Some((s.took_us, s.server?))).collect();
    told.sort_unstable_by_key(|(took, _)| *took);
    let sorted: Vec<u64> = told.iter().map(|(took, _)| *took).collect();
    let at = quantile(&sorted, q)?;
    let slow: Vec<(u64, u64, u64)> = told
        .iter()
        .filter(|(took, _)| *took >= at)
        .map(|(seen, t)| {
            let wait = t.wait_us.unwrap_or(0);
            (wait, t.took_us, seen.saturating_sub(wait).saturating_sub(t.took_us))
        })
        .collect();
    let n = slow.len();
    #[allow(clippy::cast_precision_loss)]
    let mean = |part: fn(&(u64, u64, u64)) -> u64| slow.iter().map(part).fold(0u64, u64::saturating_add) as f64 / n.max(1) as f64 / 1000.0;
    Some(Split { n, wait_ms: mean(|s| s.0), took_ms: mean(|s| s.1), rest_ms: mean(|s| s.2) })
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

/// What held the shards past 100 µs between two scrapes, by spawn site and label (`kiln_shard_held_*`, goal 9,
/// plan 01a1a87b2cec step 2), every shard's together: how many polls, how long they held their shards in all
/// and a second, and the longest any had held one by the second scrape (since the server started).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Held {
    pub site: String,
    pub label: String,
    pub polls: f64,
    pub ms: f64,
    pub polls_per_s: f64,
    pub ms_per_s: f64,
    pub longest_ms: f64,
    /// Of those polls, the ones the host held (preempted: `kiln_shard_held_host_*`, steps 3 and 4), and their
    /// time; none from a build from before it said.
    #[serde(default)]
    pub host_polls: f64,
    #[serde(default)]
    pub host_ms: f64,
    /// Of those polls, the ones whose shard's thread said its time on the CPU (`kiln_shard_held_cpu_*`, goal 9, plan
    /// 01a1a87b2cec step 3: Linux), their holds and that time, and the longest any ran on the CPU (since the server
    /// started); none from a build from before it said, or where its OS wouldn't say.
    #[serde(default)]
    pub cpu_polls: f64,
    #[serde(default)]
    pub cpu_held_ms: f64,
    #[serde(default)]
    pub cpu_ms: f64,
    #[serde(default)]
    pub longest_cpu_ms: f64,
}

/// What held the shards between `before` and `after`, `secs` apart, the most time first.
pub fn held(before: &Scrape, after: &Scrape, secs: f64) -> Vec<Held> {
    let mut by: BTreeMap<(String, String), Held> = BTreeMap::new();
    for (name, labels, value) in &after.series {
        let field = |k: &str| labels.iter().find(|(lk, _)| lk == k).map(|(_, v)| v.as_str());
        let (Some(shard), Some(site), Some(label)) = (field("shard"), field("site"), field("label")) else { continue };
        let want = [("shard", shard), ("site", site), ("label", label)];
        let added = value - before.get(name, &want).unwrap_or(0.0);
        let row = by.entry((site.to_owned(), label.to_owned())).or_insert_with(|| Held { site: site.to_owned(), label: label.to_owned(), ..Held::default() });
        match name.as_str() {
            "kiln_shard_held_polls_total" => row.polls += added,
            "kiln_shard_held_seconds_total" => row.ms += added * 1000.0,
            "kiln_shard_held_longest_seconds" => row.longest_ms = row.longest_ms.max(value * 1000.0),
            "kiln_shard_held_host_polls_total" => row.host_polls += added,
            "kiln_shard_held_host_seconds_total" => row.host_ms += added * 1000.0,
            "kiln_shard_held_cpu_polls_total" => row.cpu_polls += added,
            "kiln_shard_held_cpu_held_seconds_total" => row.cpu_held_ms += added * 1000.0,
            "kiln_shard_held_cpu_seconds_total" => row.cpu_ms += added * 1000.0,
            "kiln_shard_held_longest_cpu_seconds" => row.longest_cpu_ms = row.longest_cpu_ms.max(value * 1000.0),
            _ => {}
        }
    }
    let mut out: Vec<Held> = by.into_values().filter(|h| h.polls > 0.0).map(|h| Held { polls_per_s: h.polls / secs.max(1e-9), ms_per_s: h.ms / secs.max(1e-9), ..h }).collect();
    out.sort_by(|a, b| b.ms.total_cmp(&a.ms));
    out
}

/// The step quantum the server's shards ran with, µs, where the build says it (`kiln_shard_quantum_seconds`, goal 9,
/// plan 01a1a87b2cec step 3): the largest any shard said.
pub fn quantum_us(s: &Scrape) -> Option<f64> {
    s.series.iter().filter(|(n, _, _)| n == "kiln_shard_quantum_seconds").map(|(_, _, v)| *v * 1e6).reduce(f64::max)
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
        let s = |what: &'static str, took_us: u64, ok: bool| Sample { class: Class::ApiRead, at_us: 0, took_us, ok, what, server: None };
        let by = seen_by_what(&[s("sessions", 1_234, true), s("sessions", 17, true), s("events", 999, true), s("events", 5, false), s("", 3, true)]);
        assert_eq!(by.keys().cloned().collect::<Vec<_>>(), vec!["api_read.events".to_owned(), "api_read.sessions".to_owned()]);
        let sessions = by.get("api_read.sessions").cloned().unwrap_or_default();
        assert_eq!((sessions.n, sessions.p50_ms, sessions.max_ms), (2, Some(0.017), Some(1.234)));
        assert_eq!(by.get("api_read.events").map(|e| (e.n, e.errors)), Some((1, 1)));
    }

    /// Goal 10, plan 01a1a87b2dae step 5: the slowest of a class's reads, as the lab saw them, are split into the
    /// server's wait, its handler's time and the rest, each the mean over them, the three adding up to their mean;
    /// reads the server didn't time, failed ones and other classes' aren't among them.
    #[test]
    fn g10_s5_the_slowest_reads_are_split_into_the_servers_wait_took_and_the_rest() {
        use crate::client::ServerTime;
        let read = |took_us: u64, server: Option<(Option<u64>, u64)>| Sample { class: Class::ApiRead, at_us: 0, took_us, ok: true, what: "", server: server.map(|(wait_us, took_us)| ServerTime { wait_us, took_us }) };
        // 1,000 reads of 200 µs, the server's share 50 µs of each; the slowest two, 3 ms and 5 ms, its share 2 ms and 1 ms.
        let mut v: Vec<Sample> = (0..998).map(|_| read(200, Some((Some(30), 20)))).collect();
        v.push(read(3_000, Some((Some(1_500), 500))));
        v.push(read(5_000, Some((None, 1_000))));
        // Not among them: one not timed, one failed, another class's.
        v.push(read(9_000, None));
        v.push(Sample { ok: false, ..read(9_000, Some((Some(1), 1))) });
        v.push(Sample { class: Class::Page, ..read(9_000, Some((Some(1), 1))) });
        let s = split(&v, Class::ApiRead, 0.999);
        assert_eq!(s, Some(Split { n: 2, wait_ms: 0.75, took_ms: 0.75, rest_ms: 2.5 }), "(3 + 5) / 2 = 4 ms: 0.75 + 0.75 + 2.5");
        let all = split(&v, Class::ApiRead, 0.0).map(|s| (s.n, s.wait_ms + s.took_ms + s.rest_ms));
        assert!(all.is_some_and(|(n, sum)| n == 1000 && (sum - (998.0 * 0.2 + 8.0) / 1000.0).abs() < 1e-9), "{all:?}");
        assert_eq!(split(&v, Class::Websocket, 0.999), None, "none timed");
        assert_eq!(ServerTime::parse("wait;dur=1.234, took;dur=0.056"), Some(ServerTime { wait_us: Some(1_234), took_us: 56 }));
        assert_eq!(ServerTime::parse("took;dur=0.5"), Some(ServerTime { wait_us: None, took_us: 500 }));
        assert_eq!(ServerTime::parse("wait;dur=1"), None, "no took");
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

    /// Goal 9, plan 01a1a87b2cec step 2: what held the shards in a window is each site and label's polls and
    /// time added between the scrapes, every shard's together, the most time first; one that held nothing in
    /// the window isn't listed.
    #[test]
    fn g9_s2_what_held_the_shards_is_the_window_s_own() {
        let before = Scrape::parse("kiln_shard_held_polls_total{shard=\"0\",site=\"a.rs:1:1\",label=\"page\"} 5\nkiln_shard_held_seconds_total{shard=\"0\",site=\"a.rs:1:1\",label=\"page\"} 0.001\nkiln_shard_held_polls_total{shard=\"0\",site=\"b.rs:2:1\",label=\"\"} 1\nkiln_shard_held_seconds_total{shard=\"0\",site=\"b.rs:2:1\",label=\"\"} 0.5\n");
        let after = Scrape::parse(
            "kiln_shard_held_polls_total{shard=\"0\",site=\"a.rs:1:1\",label=\"page\"} 7\nkiln_shard_held_seconds_total{shard=\"0\",site=\"a.rs:1:1\",label=\"page\"} 0.003\nkiln_shard_held_polls_total{shard=\"1\",site=\"a.rs:1:1\",label=\"page\"} 1\nkiln_shard_held_seconds_total{shard=\"1\",site=\"a.rs:1:1\",label=\"page\"} 0.004\nkiln_shard_held_longest_seconds{shard=\"1\",site=\"a.rs:1:1\",label=\"page\"} 0.004\nkiln_shard_held_polls_total{shard=\"0\",site=\"b.rs:2:1\",label=\"\"} 1\nkiln_shard_held_seconds_total{shard=\"0\",site=\"b.rs:2:1\",label=\"\"} 0.5\n",
        );
        let h = held(&before, &after, 2.0);
        assert_eq!(h.len(), 1, "{h:?}");
        let page = h.first().cloned().unwrap_or_default();
        assert_eq!((page.site.as_str(), page.label.as_str()), ("a.rs:1:1", "page"));
        assert!((page.polls - 3.0).abs() < 1e-9 && (page.ms - 6.0).abs() < 1e-9 && (page.ms_per_s - 3.0).abs() < 1e-9 && (page.longest_ms - 4.0).abs() < 1e-9, "{page:?}");
        assert!(page.host_polls == 0.0 && page.host_ms == 0.0, "a build that doesn't say the host's: none {page:?}");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: the quantum the shards ran with is read off the server's `/metrics`, the
    /// largest any shard said, in µs; a build that doesn't say it has none.
    #[test]
    fn g9_s3_the_shards_quantum_is_read_off_metrics() {
        let s = Scrape::parse("kiln_shard_quantum_seconds{shard=\"0\"} 0.000266666\nkiln_shard_quantum_seconds{shard=\"1\"} 0.00025\n");
        assert!(quantum_us(&s).is_some_and(|q| (q - 266.666).abs() < 1e-6), "{:?}", quantum_us(&s));
        assert_eq!(quantum_us(&Scrape::parse("kiln_shard_polls_total{shard=\"0\"} 5\n")), None);
    }

    /// Goal 9, plan 01a1a87b2cec step 3: of what held the shards in a window, the polls whose thread said its time on
    /// the CPU, their holds and that time are the window's own; the longest on the CPU is since the server started.
    #[test]
    fn g9_s3_a_polls_time_on_the_cpu_is_the_window_s_own() {
        let series = |polls: u32, held: f64, cpu: f64, longest: f64| {
            format!(
                "kiln_shard_held_polls_total{{shard=\"0\",site=\"a.rs:1:1\",label=\"\"}} {polls}\nkiln_shard_held_cpu_polls_total{{shard=\"0\",site=\"a.rs:1:1\",label=\"\"}} {polls}\nkiln_shard_held_cpu_held_seconds_total{{shard=\"0\",site=\"a.rs:1:1\",label=\"\"}} {held}\nkiln_shard_held_cpu_seconds_total{{shard=\"0\",site=\"a.rs:1:1\",label=\"\"}} {cpu}\nkiln_shard_held_longest_cpu_seconds{{shard=\"0\",site=\"a.rs:1:1\",label=\"\"}} {longest}\n"
            )
        };
        let (before, after) = (Scrape::parse(&series(10, 0.010, 0.004, 0.001)), Scrape::parse(&series(14, 0.020, 0.006, 0.0025)));
        let h = held(&before, &after, 1.0);
        let a = h.first().cloned().unwrap_or_default();
        assert!((a.cpu_polls - 4.0).abs() < 1e-9 && (a.cpu_held_ms - 10.0).abs() < 1e-9 && (a.cpu_ms - 2.0).abs() < 1e-9 && (a.longest_cpu_ms - 2.5).abs() < 1e-9, "{a:?}");
    }

    /// Goal 9, plan 01a1a87b2cec steps 3 and 4: of what held the shards in a window, the polls the host held and
    /// their time are the window's own too.
    #[test]
    fn g9_s3_what_the_host_held_is_the_window_s_own() {
        let series = |polls: u32, secs: f64, host: u32, host_secs: f64| {
            format!(
                "kiln_shard_held_polls_total{{shard=\"0\",site=\"a.rs:1:1\",label=\"\"}} {polls}\nkiln_shard_held_seconds_total{{shard=\"0\",site=\"a.rs:1:1\",label=\"\"}} {secs}\nkiln_shard_held_host_polls_total{{shard=\"0\",site=\"a.rs:1:1\",label=\"\"}} {host}\nkiln_shard_held_host_seconds_total{{shard=\"0\",site=\"a.rs:1:1\",label=\"\"}} {host_secs}\n"
            )
        };
        let (before, after) = (Scrape::parse(&series(10, 0.01, 2, 0.004)), Scrape::parse(&series(14, 0.02, 5, 0.01)));
        let h = held(&before, &after, 1.0);
        let a = h.first().cloned().unwrap_or_default();
        assert!((a.polls - 4.0).abs() < 1e-9 && (a.host_polls - 3.0).abs() < 1e-9 && (a.host_ms - 6.0).abs() < 1e-9, "{a:?}");
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
