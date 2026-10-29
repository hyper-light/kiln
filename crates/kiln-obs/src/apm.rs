//! APM (`docs/observe.md` §8.7): what kiln makes of spans besides keeping them.
//!
//! - **Trace metrics**, Datadog's: `trace.hits`, `trace.errors` and `trace.duration` (a
//!   distribution in seconds, so any percentile) of the requests a service serves, by service,
//!   resource, kind, env, version and status code; and `trace.outbound.*` of the calls it makes, by
//!   the peer it calls ([`trace_metrics`]). The spans measured are those Datadog's trace agent
//!   computes stats of: a service's entry spans (its servers' and consumers', and a trace's root)
//!   and, by their kind, its exits (clients' and producers'); a span inside a service is not (a
//!   function's span would make series of every function). They are counted on the server from the
//!   spans it holds, as metrics from logs are (`crate::logs::metrics`: each window of ten seconds
//!   whole once its spans have come, again when more come for it), so a span sent to any node of
//!   the group counts once, and a resent one not twice. A Datadog Agent's spans are not: it
//!   sends on only the traces it samples, and its stats count every span (`crate::ddtrace::stats`,
//!   the same series, each of its host).
//! - **A trace** ([`assemble`]): its spans in the order a waterfall draws them, each after its
//!   parent and siblings by their start, with its depth; a span twice (an exporter's retry) once.
//! - **A service map** ([`MapBuilder`]): who calls whom, from a range's spans. A span whose parent is
//!   of another service is a call of that service to its own (the parent's duration, when the
//!   parent is the caller's client span, is the call's); a client's or a producer's span with no
//!   such child is a call to what it names its peer (`peer.service`), inferred. Its figures
//!   ([`figures`]) are the trace metrics' (every call, the traces dropped too), each edge its share
//!   of the groups that count its calls.

use std::collections::{BTreeMap, HashMap};

use crate::logs::metrics::{Compute, LogMetric};
use crate::logs::record::{Record, RecordRef};
use crate::logs::search::parse_spans;
use crate::logs::store::Facet;
use crate::query::eval::{Lease, QueryError, Source};

/// A service's entry spans: its servers' and consumers', and a trace's root that is neither a
/// client's nor a producer's.
pub const ENTRY: &str = "@span.kind:server OR @span.kind:consumer OR (-@parent_id:* -@span.kind:client -@span.kind:producer)";
/// A service's exits: its clients' and producers' spans.
pub const EXIT: &str = "@span.kind:client OR @span.kind:producer";
/// The names kiln writes trace metrics under, and keeps for them.
pub const PREFIX: &str = "trace.";

fn def(name: &str, query: &str, by: &[&str], compute: Compute) -> Option<LogMetric> {
    let search = parse_spans(query).ok()?;
    let by = by.iter().map(|b| (Facet::parse_span(b), (*b).trim_start_matches('@').to_owned())).collect();
    Some(LogMetric { name: name.to_owned(), search, by, compute, since: 0 })
}

/// Datadog's trace metrics, as metrics from spans: what each service serves, and what it calls.
pub fn trace_metrics() -> Vec<LogMetric> {
    let entry_by = ["service", "resource_name", "@span.kind", "@env", "@version", "@http.status_code"];
    let exit_by = ["service", "resource_name", "@span.kind", "@env", "@peer.service"];
    // An Agent's spans are counted from its stats; an LLM event of Datadog's intake is no request a
    // service served (its LLM trace is kept apart from its APM trace, whose spans are counted).
    let [entry, exit] = [ENTRY, EXIT].map(|q| format!("({q}) -source:{} -source:{}", crate::ddtrace::SOURCE, crate::llm::intake::SOURCE));
    let errors_in = |q: &str| format!("({q}) status:error");
    [
        def("trace.hits", &entry, &entry_by, Compute::Count),
        def("trace.errors", &errors_in(&entry), &entry_by, Compute::Count),
        def("trace.duration", &entry, &entry_by, Compute::Duration { percentiles: true }),
        def("trace.outbound.hits", &exit, &exit_by, Compute::Count),
        def("trace.outbound.errors", &errors_in(&exit), &exit_by, Compute::Count),
        def("trace.outbound.duration", &exit, &exit_by, Compute::Duration { percentiles: true }),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// What a group's requests (or calls out) came to over a range, from the trace metrics.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Red {
    /// The group's value of the tag grouped by (`N/A` where a series has none).
    pub group: String,
    pub hits: f64,
    pub errors: f64,
    /// Their durations' sum (s): their mean is it over `hits`.
    pub duration_sum: f64,
    /// Their durations' percentiles (s), from the buckets merged over the whole range, and the most.
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
    pub max: Option<f64>,
}

/// The trace metrics (`trace.*`, or with `outbound` `trace.outbound.*`) of the windows that start in
/// `[from, to)` (ms), of the series whose tags hold `filter`, by tag `by`: totals of their counts,
/// and percentiles merged from their buckets over the whole range (as exact as their scale, the
/// windows of ten seconds they are counted in holding a range to within one); the most hits first.
///
/// A long range is read from the series' rollups (`crate::rollup`): the coarsest tier at most a
/// twelfth of the range wide, so its edges, the two windows at most it reads as samples, are at
/// most a sixth of it; a week of a series is 168 hours' sums, not 60,480 samples. A count's total is
/// its windows' sums and its samples' added, the most the greatest of its windows' and samples'.
///
/// Of a store held throughout, or of a shared one read a part at a time (`query::eval::Source`):
/// each part's series found under one hold, then read a round at a time, each round under a hold of
/// its own, so a write waits for one round at the most (`query::eval::Lease`).
pub fn red<'a>(store: impl Into<Source<'a>>, outbound: bool, filter: &[(&str, &str)], by: &str, from: i64, to: i64) -> Result<Vec<Red>, QueryError> {
    store.into().with(|lease| red_on(lease, outbound, filter, by, from, to))
}

fn red_on(lease: &Lease<'_>, outbound: bool, filter: &[(&str, &str)], by: &str, from: i64, to: i64) -> Result<Vec<Red>, QueryError> {
    use crate::dist::{Merged, SCALE, parse_tag_value};
    use crate::rollup::{Col, Rolled, TIERS};
    let prefix = if outbound { "trace.outbound" } else { "trace" };
    let mut groups: BTreeMap<String, (Red, Merged)> = BTreeMap::new();
    let (mut ids, mut ts, mut vs) = (Vec::new(), Vec::new(), Vec::new());
    let mut scratch = crate::store::Scratch::default();
    let width = TIERS.iter().rev().copied().find(|&w| w.saturating_mul(RED_WINDOWS) <= to.saturating_sub(from));
    let mut rolled = Rolled::default();
    // A part's series, each its group, its bucket and its kind.
    let mut found: Vec<(u32, String, Option<(i32, crate::dist::Bucket)>, crate::batch::Kind)> = Vec::new();
    for part in ["hits", "errors", "duration.sum", "duration.max", "duration.dist"] {
        let name = format!("{prefix}.{part}");
        let mut sel = crate::index::Selector::metric(&name);
        for (k, v) in filter {
            sel = sel.tag(k, v);
        }
        found.clear();
        {
            let store = lease.hold()?;
            ids.clear();
            store.select(&sel, &mut ids);
            found.extend(ids.iter().filter_map(|&id| {
                let series = store.series(id)?;
                let bucket = if part == "duration.dist" { series.tag(crate::dist::BUCKET_TAG).and_then(parse_tag_value) } else { None };
                Some((id, series.tag(by).unwrap_or(crate::logs::metrics::NONE).to_owned(), bucket, series.kind()))
            }));
        }
        let most = part == "duration.max";
        let cols: &[Col] = if most { &[Col::Max] } else { &[Col::Sum] };
        for round in found.chunks(crate::query::eval::ROUND) {
            let store = lease.hold()?;
            for (id, group, bucket, kind) in round {
                ts.clear();
                vs.clear();
                rolled.clear();
                match width {
                    Some(width) => {
                        let roll = crate::store::Roll { width, from, to, kind: *kind, cols };
                        store.read_rolled(*id, from, to.saturating_sub(1), &roll, &mut scratch, &mut ts, &mut vs, &mut rolled)?;
                    }
                    None => store.read_with(*id, from, to.saturating_sub(1), &mut scratch, &mut ts, &mut vs)?,
                }
                let total = || vs.iter().sum::<f64>() + rolled.sum.iter().sum::<f64>();
                if !groups.contains_key(group.as_str()) {
                    groups.insert(group.clone(), (Red { group: group.clone(), ..Red::default() }, Merged::new(SCALE)));
                }
                let Some((r, m)) = groups.get_mut(group.as_str()) else { continue };
                match part {
                    "hits" => r.hits += total(),
                    "errors" => r.errors += total(),
                    "duration.sum" => r.duration_sum += total(),
                    "duration.max" => {
                        if let Some(x) = vs.iter().chain(&rolled.max).copied().filter(|v| v.is_finite()).reduce(f64::max) {
                            r.max = Some(r.max.map_or(x, |m| m.max(x)));
                        }
                    }
                    _ => {
                        if let Some((scale, b)) = *bucket {
                            m.add(b, scale, total());
                        }
                    }
                }
            }
        }
    }
    let mut out: Vec<Red> = groups
        .into_values()
        .map(|(mut r, m)| {
            let bounds = r.max.map(|hi| (0.0, hi));
            (r.p50, r.p95, r.p99) = (m.quantile(0.5, bounds), m.quantile(0.95, bounds), m.quantile(0.99, bounds));
            r
        })
        .collect();
    out.sort_by(|a, b| b.hits.total_cmp(&a.hits).then_with(|| a.group.cmp(&b.group)));
    Ok(out)
}

/// Windows of a tier a range holds at the least for [`red`] to read the tier: so the two it reads
/// as samples at its edges at the most are a sixth of it.
pub const RED_WINDOWS: i64 = 12;

/// A span's id in hex (`parent_id`'s), read.
pub fn span_id_of(hex: &str) -> Option<[u8; 8]> {
    if hex.len() != 16 {
        return None;
    }
    let mut out = [0u8; 8];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// A span's duration (ns), as its `duration` says.
pub fn duration_of(attrs: &[(String, String)]) -> u64 {
    attrs.iter().find(|(k, _)| k == "duration").and_then(|(_, v)| v.parse().ok()).unwrap_or(0)
}

fn attr<'a>(r: &'a Record, key: &str) -> Option<&'a str> {
    r.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// A trace's span, placed.
#[derive(Debug, Clone, PartialEq)]
pub struct Placed {
    pub span: Record,
    /// How many ancestors the trace holds of it.
    pub depth: u32,
    /// Its parent's place in [`Trace::spans`], if the trace holds its parent.
    pub parent: Option<usize>,
}

/// A trace's spans as a waterfall draws them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Trace {
    /// Each root (a span whose parent the trace does not hold), then its descendants depth first,
    /// siblings by their start; roots by their start.
    pub spans: Vec<Placed>,
    /// Its first start and last end (ns).
    pub start: i64,
    pub end: i64,
    /// The services it passed through, and its spans of each, by name.
    pub services: BTreeMap<String, usize>,
    pub errors: usize,
    /// Spans whose parent it does not hold (one, for a trace held whole).
    pub roots: usize,
}

/// A trace from its spans, in any order.
pub fn assemble(mut spans: Vec<Record>) -> Trace {
    // Once each: an exporter that retried may have sent a span twice.
    spans.sort_by(|a, b| a.span_id.cmp(&b.span_id).then(a.ts.cmp(&b.ts)));
    spans.dedup_by(|a, b| a.span_id.is_some() && a.span_id == b.span_id);
    let at: HashMap<[u8; 8], usize> = spans.iter().enumerate().filter_map(|(i, s)| Some((s.span_id?, i))).collect();
    let parent: Vec<Option<usize>> = spans.iter().map(|s| attr(s, "parent_id").and_then(span_id_of).and_then(|p| at.get(&p).copied())).collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); spans.len()];
    let mut roots: Vec<usize> = Vec::new();
    for (i, p) in parent.iter().enumerate() {
        match p {
            Some(p) if *p != i => {
                if let Some(c) = children.get_mut(*p) {
                    c.push(i);
                }
            }
            _ => roots.push(i),
        }
    }
    let by_start = |list: &mut Vec<usize>, spans: &[Record]| list.sort_by_key(|&i| spans.get(i).map(|s| (s.ts, s.span_id)));
    for c in &mut children {
        by_start(c, &spans);
    }
    by_start(&mut roots, &spans);
    // Depth first, with a stack of its own; a span in a cycle of parents (malformed) is reached by
    // none, and placed as a root after the rest.
    let mut order: Vec<(usize, u32)> = Vec::with_capacity(spans.len());
    let mut seen = vec![false; spans.len()];
    let walk = |root: usize, order: &mut Vec<(usize, u32)>, seen: &mut [bool]| {
        let mut stack = vec![(root, 0u32)];
        while let Some((i, depth)) = stack.pop() {
            match seen.get_mut(i) {
                Some(s) if !*s => *s = true,
                _ => continue,
            }
            order.push((i, depth));
            if let Some(c) = children.get(i) {
                stack.extend(c.iter().rev().map(|&c| (c, depth + 1)));
            }
        }
    };
    let n_roots = roots.len();
    for r in roots {
        walk(r, &mut order, &mut seen);
    }
    let mut cyclic = 0;
    for i in 0..spans.len() {
        if !seen.get(i).copied().unwrap_or(true) {
            cyclic += 1;
            walk(i, &mut order, &mut seen);
        }
    }
    let mut place = vec![0usize; spans.len()];
    for (k, (i, _)) in order.iter().enumerate() {
        if let Some(p) = place.get_mut(*i) {
            *p = k;
        }
    }
    let mut t = Trace { start: i64::MAX, end: i64::MIN, roots: n_roots + cyclic, ..Trace::default() };
    let mut slots: Vec<Option<Record>> = spans.into_iter().map(Some).collect();
    for (i, depth) in order {
        let Some(span) = slots.get_mut(i).and_then(Option::take) else { continue };
        t.start = t.start.min(span.ts);
        t.end = t.end.max(span.ts.saturating_add(i64::try_from(duration_of(&span.attrs)).unwrap_or(i64::MAX)));
        *t.services.entry(span.service.clone()).or_default() += 1;
        t.errors += usize::from(span.severity >= crate::otlp::traces::SEVERITY_ERROR);
        let parent = parent.get(i).copied().flatten().filter(|_| depth > 0).and_then(|p| place.get(p).copied());
        t.spans.push(Placed { span, depth, parent });
    }
    if t.spans.is_empty() {
        (t.start, t.end) = (0, 0);
    }
    t
}

/// Whether a span of trace `id` is in a sample of one trace in `k`: by its trace id, so a trace is
/// taken whole or not at all (as tail and head samplers keep traces whole), its low 64 bits
/// random by W3C Trace Context's (level 2) and mixed again for ids that are not.
pub fn sampled(id: Option<[u8; 16]>, k: u64) -> bool {
    if k <= 1 {
        return true;
    }
    let Some(id) = id else { return false };
    let low = u64::from_le_bytes(id.get(8..16).and_then(|b| <[u8; 8]>::try_from(b).ok()).unwrap_or([0; 8]));
    let mut z = low ^ u64::from_le_bytes(id.get(..8).and_then(|b| <[u8; 8]>::try_from(b).ok()).unwrap_or([0; 8])).rotate_left(29);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (z ^ (z >> 31)) % k == 0
}

/// A service on the map: the requests it served (its entry spans) and their errors; whether it is
/// only named, as a peer some service calls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Node {
    pub service: String,
    pub hits: u64,
    pub errors: u64,
    pub inferred: bool,
}

/// The trace metrics' group a span is counted in: a service's entries (`trace.*`), or its calls
/// out to a peer (`trace.outbound.*` by `peer.service`, [`crate::logs::metrics::NONE`] where the
/// call names none).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Group {
    Entries(String),
    Calls(String, String),
}

/// Calls of one service to another: how many, how many failed, their durations' sum (ns).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub calls: u64,
    pub errors: u64,
    pub duration_ns: u64,
    /// To a peer a client named, not a service whose spans were seen answering.
    pub inferred: bool,
    /// Its calls by the group of the trace metrics that counts them (the caller's call out where
    /// it made one, else the callee's span: an entry, or a call of a client library's service),
    /// none where no group does: each one's calls, errors and durations' sum (ns), of the spans
    /// read.
    pub seen: Vec<(Option<Group>, u64, u64, u64)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceMap {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// Spans read.
    pub spans: u64,
    /// Each group's spans read, and their errors: what an edge's share of a group is of.
    pub counted: Vec<(Group, u64, u64)>,
}

/// A span the map keeps until it is built: its service, its duration, whether it erred, whether it
/// is a call out (a client's or a producer's) and the peer it names, and its parent.
struct Seen {
    service: u32,
    duration: u64,
    error: bool,
    call: bool,
    /// A server's or a consumer's.
    serves: bool,
    /// A Datadog tracer's (by its Agent, or kiln in its place): its stats count as its service's
    /// entries the spans that are top-level (marked so, a root, or their parent another
    /// service's), as the Agent's do.
    datadog: bool,
    marked: bool,
    peer: Option<u32>,
    parent: Option<SpanKey>,
}

/// A span's place in the map: its trace's id and its own (a span id is its trace's, not unique
/// across traces: tracers that number spans in order reuse them in every trace).
type SpanKey = ([u8; 16], [u8; 8]);

/// A group, by the names' places: a service's entries (no peer), or its calls to a peer.
type G = (u32, Option<u32>);

/// Builds a service map from spans, one at a time ([`MapBuilder::take`]).
#[derive(Default)]
pub struct MapBuilder {
    names: Vec<String>,
    by_name: HashMap<String, u32>,
    spans: HashMap<SpanKey, Seen>,
    read: u64,
}

impl MapBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    fn name(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.by_name.get(s) {
            return i;
        }
        let i = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
        self.names.push(s.to_owned());
        self.by_name.insert(s.to_owned(), i);
        i
    }

    /// Spans read so far.
    pub fn read(&self) -> u64 {
        self.read
    }

    pub fn take(&mut self, r: &RecordRef<'_>) {
        // An LLM event of Datadog's intake: its LLM trace is kept apart from the APM trace whose
        // spans draw the map.
        if r.source == crate::llm::intake::SOURCE {
            return;
        }
        self.read += 1;
        // Named as the trace metrics' tags name them (cut as a tag's value is), so a group is found.
        let service = self.name(if r.service.is_empty() { "unknown" } else { crate::logs::metrics::tag_value(Some(r.service)) });
        // What the map reads of a span's attributes, in one pass (each key's first, as `attr` finds it).
        let (mut kind, mut duration, mut parent, mut peer, mut dd_top, mut top) = (None, None, None, None, None, None);
        for (k, v) in r.attrs() {
            let slot = match k {
                "span.kind" => &mut kind,
                "duration" => &mut duration,
                "parent_id" => &mut parent,
                "peer.service" => &mut peer,
                "_dd.top_level" => &mut dd_top,
                "_top_level" => &mut top,
                _ => continue,
            };
            slot.get_or_insert(v);
        }
        let kind = kind.unwrap_or("internal");
        let duration = duration.and_then(|d: &str| d.parse().ok()).unwrap_or(0);
        let error = r.severity >= crate::otlp::traces::SEVERITY_ERROR;
        let call = kind == "client" || kind == "producer";
        let peer = if call { Some(self.name(crate::logs::metrics::tag_value(peer))) } else { None };
        let trace = r.trace_id.unwrap_or([0; 16]);
        let parent = parent.and_then(span_id_of).map(|p| (trace, p));
        let (datadog, marked) = (r.source == crate::ddtrace::SOURCE, dd_top == Some("1") || top == Some("1"));
        if let Some(id) = r.span_id {
            self.spans.entry((trace, id)).or_insert(Seen { service, duration, error, call, serves: kind == "server" || kind == "consumer", datadog, marked, peer, parent });
        }
    }

    /// The group of the trace metrics that counts `s`: a call out's, by the peer it names; an
    /// entry's, as [`ENTRY`] says of a span (a server's, a consumer's, a root not a call), and as a
    /// Datadog Agent's stats say of a tracer's (any top-level span not a call); else none.
    fn group(&self, s: &Seen) -> Option<G> {
        if s.call {
            return Some((s.service, s.peer));
        }
        let parent = s.parent.map(|p| self.spans.get(&p).map(|p| p.service));
        let top = match parent {
            None => true,
            Some(None) => s.datadog,
            Some(Some(p)) => s.datadog && (p != s.service || s.marked),
        };
        (s.serves || top).then_some((s.service, None))
    }

    pub fn finish(self) -> ServiceMap {
        let none = self.by_name.get(crate::logs::metrics::NONE).copied();
        let mut edges: BTreeMap<(u32, u32, bool), (Edge, BTreeMap<Option<G>, (u64, u64, u64)>)> = BTreeMap::new();
        let mut counted: BTreeMap<G, (u64, u64)> = BTreeMap::new();
        let mut entries: BTreeMap<u32, (u64, u64)> = BTreeMap::new();
        let mut answered: std::collections::HashSet<SpanKey> = std::collections::HashSet::new();
        for s in self.spans.values() {
            if let Some(g) = self.group(s) {
                let c = counted.entry(g).or_default();
                c.0 += 1;
                c.1 += u64::from(s.error);
            }
            if s.serves || (s.parent.is_none() && !s.call) {
                let e = entries.entry(s.service).or_default();
                e.0 += 1;
                e.1 += u64::from(s.error);
            }
            // A span whose parent is another service's: a call of the parent's service to its own,
            // the call's duration the parent's where the parent is the caller's call out.
            let Some((pk, p)) = s.parent.and_then(|pk| Some((pk, self.spans.get(&pk)?))) else { continue };
            if p.service == s.service {
                continue;
            }
            answered.insert(pk);
            let (e, seen) = edges.entry((p.service, s.service, false)).or_default();
            e.calls += 1;
            e.errors += u64::from(s.error);
            let duration = if p.call { p.duration } else { s.duration };
            e.duration_ns += duration;
            let by = if p.call { p } else { s };
            let o = seen.entry(self.group(by)).or_default();
            o.0 += 1;
            o.1 += u64::from(by.error);
            o.2 += duration;
        }
        // A call out no other service's span answers: a call to the peer it names, inferred.
        for (k, s) in &self.spans {
            if let (true, Some(peer)) = (s.call, s.peer)
                && peer != s.service
                && Some(peer) != none
                && !answered.contains(k)
            {
                let (e, seen) = edges.entry((s.service, peer, true)).or_default();
                e.calls += 1;
                e.errors += u64::from(s.error);
                e.duration_ns += s.duration;
                let o = seen.entry(Some((s.service, Some(peer)))).or_default();
                o.0 += 1;
                o.1 += u64::from(s.error);
                o.2 += s.duration;
            }
        }
        let name = |i: u32| self.names.get(i as usize).cloned().unwrap_or_default();
        let group = |(s, peer): G| match peer {
            Some(p) => Group::Calls(name(s), name(p)),
            None => Group::Entries(name(s)),
        };
        let mut nodes: BTreeMap<u32, Node> = BTreeMap::new();
        for (&s, &(hits, errors)) in &entries {
            nodes.insert(s, Node { service: name(s), hits, errors, inferred: false });
        }
        let mut out_edges = Vec::with_capacity(edges.len());
        for ((from, to, inferred), (mut e, seen)) in edges {
            for (s, inf) in [(from, false), (to, inferred)] {
                nodes.entry(s).or_insert_with(|| Node { service: name(s), inferred: inf, ..Node::default() });
            }
            (e.from, e.to, e.inferred) = (name(from), name(to), inferred);
            e.seen = seen.into_iter().map(|(g, (n, err, d))| (g.map(group), n, err, d)).collect();
            out_edges.push(e);
        }
        let mut nodes: Vec<Node> = nodes.into_values().collect();
        nodes.sort_by(|a, b| a.service.cmp(&b.service));
        out_edges.sort_by(|a, b| (&a.from, &a.to, a.inferred).cmp(&(&b.from, &b.to, b.inferred)));
        let mut counted: Vec<(Group, u64, u64)> = counted.into_iter().map(|(g, (n, e))| (group(g), n, e)).collect();
        counted.sort();
        ServiceMap { nodes, edges: out_edges, spans: self.read, counted }
    }
}

/// A service on the map, with its figures over the range.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeFigures {
    pub service: String,
    pub hits: f64,
    pub errors: f64,
    pub inferred: bool,
}

/// Calls of one service to another over the range, with their figures.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeFigures {
    pub from: String,
    pub to: String,
    pub calls: f64,
    pub errors: f64,
    /// Their mean duration (s).
    pub avg_s: Option<f64>,
    pub inferred: bool,
    /// From the trace metrics, every call whatever traces were kept; else from the spans read alone.
    pub counted: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MapFigures {
    pub nodes: Vec<NodeFigures>,
    pub edges: Vec<EdgeFigures>,
}

/// The figures of a map built from the spans of `[from, to)` (ms) that were read, one trace in `k`.
///
/// A trace's spans are kept or not as a whole (a tracer's sampling, the Agent's, the map's own one
/// in `k`), but the trace metrics count every span (`red`). So an edge's figures are its share of
/// the groups that count its calls ([`Edge::seen`]): a group's calls, errors and durations from
/// the trace metrics, times the part of the group's spans read that are the edge's (its errors by
/// the part of the group's errors read, where some were). The ratio estimator (Cochran, *Sampling
/// Techniques*, ch. 6), each trace a cluster of spans: consistent, its bias of the order of one
/// over the traces read, as the spans read of a group are a sample of it independent of the edge
/// (a trace kept or not by its id and its root's rate); and exact where a group's calls all go one
/// way, as most do: an edge's figures are then its group's, as its services' are their groups'
/// (all of them as of the windows the metrics have counted, some seconds behind the spans). A group the metrics have
/// nothing of in the range, or that none counts, gives its spans read, `k` times.
/// A call out the metrics count and no span read names (every trace of it dropped) is an edge of
/// its own, to the peer it names (inferred, unless a service of that name has requests). A
/// service's requests and errors are its entries' (every request), else its calls out's (a service
/// only of calls: a client library's), else its spans read's; an inferred one's, what its callers
/// call it.
#[allow(clippy::cast_precision_loss)]
pub fn figures<'a>(store: impl Into<Source<'a>>, map: &ServiceMap, filter: &[(&str, &str)], from: i64, to: i64, k: u64) -> Result<MapFigures, QueryError> {
    use crate::logs::metrics::NONE;
    let store = store.into();
    let entries: HashMap<String, Red> = red(store, false, filter, "service", from, to)?.into_iter().filter(|r| r.hits > 0.0).map(|r| (r.group.clone(), r)).collect();
    let exits: HashMap<String, Red> = red(store, true, filter, "service", from, to)?.into_iter().filter(|r| r.hits > 0.0).map(|r| (r.group.clone(), r)).collect();
    // Each calling service's calls by the peer they name.
    let mut calls: BTreeMap<(String, String), Red> = BTreeMap::new();
    let mut by: Vec<(&str, &str)> = filter.to_vec();
    for service in exits.keys().filter(|s| s.as_str() != NONE) {
        by.truncate(filter.len());
        by.push(("service", service.as_str()));
        for r in red(store, true, &by, "peer.service", from, to)? {
            if r.hits > 0.0 {
                calls.insert((service.clone(), r.group.clone()), r);
            }
        }
    }
    let stats = |g: &Group| match g {
        Group::Entries(s) => entries.get(s),
        Group::Calls(s, p) => calls.get(&(s.clone(), p.clone())),
    };
    let read: HashMap<&Group, (u64, u64)> = map.counted.iter().map(|(g, n, e)| (g, (*n, *e))).collect();
    let times = |n: u64| n.saturating_mul(k) as f64;
    // Calls, errors, durations' sum (s) and the calls timed, by edge.
    let mut edges: BTreeMap<(String, String, bool), (f64, f64, f64, f64, bool)> = BTreeMap::new();
    for e in &map.edges {
        let f = edges.entry((e.from.clone(), e.to.clone(), e.inferred)).or_default();
        for (g, n, err, dur) in &e.seen {
            let counted = g.as_ref().and_then(|g| Some((stats(g)?, read.get(g).copied().filter(|r| r.0 > 0)?)));
            match counted {
                Some((r, (all, all_err))) => {
                    let share = *n as f64 / all as f64;
                    let err_share = if all_err > 0 { *err as f64 / all_err as f64 } else { share };
                    f.0 += r.hits * share;
                    f.1 += r.errors * err_share;
                    f.2 += r.duration_sum * share;
                    f.3 += r.hits * share;
                    f.4 = true;
                }
                None => {
                    f.0 += times(*n);
                    f.1 += times(*err);
                    f.2 += times(*dur) / 1e9;
                    f.3 += times(*n);
                }
            }
        }
    }
    // Calls the metrics count that no span read shows.
    let served: std::collections::HashSet<&str> = entries.keys().map(String::as_str).chain(map.nodes.iter().filter(|n| !n.inferred).map(|n| n.service.as_str())).collect();
    for ((service, peer), r) in &calls {
        if peer == NONE || read.contains_key(&Group::Calls(service.clone(), peer.clone())) {
            continue;
        }
        let f = edges.entry((service.clone(), peer.clone(), !served.contains(peer.as_str()))).or_default();
        f.0 += r.hits;
        f.1 += r.errors;
        f.2 += r.duration_sum;
        f.3 += r.hits;
        f.4 = true;
    }
    let edges: Vec<EdgeFigures> = edges
        .into_iter()
        .map(|((from, to, inferred), (calls, errors, dur, timed, counted))| EdgeFigures { from, to, calls, errors, avg_s: (timed > 0.0).then(|| dur / timed), inferred, counted })
        .collect();
    let mut nodes: BTreeMap<String, NodeFigures> = BTreeMap::new();
    for n in map.nodes.iter().filter(|n| !n.inferred) {
        let (hits, errors) = entries.get(&n.service).or_else(|| exits.get(&n.service)).map_or((times(n.hits), times(n.errors)), |r| (r.hits, r.errors));
        nodes.insert(n.service.clone(), NodeFigures { service: n.service.clone(), hits, errors, inferred: false });
    }
    // A service of requests in the range none of whose spans were read is on the map still.
    for r in entries.values().filter(|r| r.group != NONE) {
        nodes.entry(r.group.clone()).or_insert_with(|| NodeFigures { service: r.group.clone(), hits: r.hits, errors: r.errors, inferred: false });
    }
    for e in &edges {
        if !nodes.contains_key(&e.from) {
            let (hits, errors) = exits.get(&e.from).map_or((0.0, 0.0), |r| (r.hits, r.errors));
            nodes.insert(e.from.clone(), NodeFigures { service: e.from.clone(), hits, errors, inferred: false });
        }
        let to = nodes.entry(e.to.clone()).or_insert_with(|| NodeFigures { service: e.to.clone(), hits: 0.0, errors: 0.0, inferred: e.inferred });
        if to.inferred {
            to.hits += e.calls;
            to.errors += e.errors;
        }
    }
    Ok(MapFigures { nodes: nodes.into_values().collect(), edges })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::record::{Rows, put_row};

    #[allow(clippy::too_many_arguments)]
    fn span(service: &str, id: u8, parent: Option<u8>, kind: &str, start: i64, dur: u64, error: bool, extra: &[(&str, &str)]) -> Record {
        let mut attrs: Vec<(String, String)> = vec![("span.kind".into(), kind.into()), ("duration".into(), dur.to_string())];
        if let Some(p) = parent {
            attrs.push(("parent_id".into(), format!("{p:02x}").repeat(8)));
        }
        attrs.extend(extra.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
        Record {
            ts: start,
            severity: if error { 17 } else { 9 },
            service: service.into(),
            host: "h".into(),
            source: "otel".into(),
            message: format!("op{id}"),
            attrs,
            trace_id: Some([1; 16]),
            span_id: Some([id; 8]),
        }
    }

    /// A checkout calls the cart service over HTTP, which queries its database; checkout also
    /// publishes to a queue nobody's spans answer.
    fn trace() -> Vec<Record> {
        vec![
            span("cart", 4, Some(3), "server", 120, 700, false, &[]),
            span("checkout", 1, None, "server", 100, 1_000, true, &[]),
            span("cart", 5, Some(4), "client", 150, 400, false, &[("peer.service", "postgresql")]),
            span("checkout", 3, Some(1), "client", 110, 800, false, &[("peer.service", "cart.internal")]),
            span("checkout", 2, Some(1), "internal", 105, 2, false, &[]),
            span("checkout", 6, Some(1), "producer", 950, 10, true, &[("peer.service", "kafka")]),
            // Sent twice.
            span("checkout", 2, Some(1), "internal", 105, 2, false, &[]),
        ]
    }

    #[test]
    fn a_trace_is_placed_as_its_waterfall_draws_it() {
        let t = assemble(trace());
        let order: Vec<(u8, u32, Option<usize>)> = t.spans.iter().map(|p| (p.span.span_id.unwrap()[0], p.depth, p.parent)).collect();
        assert_eq!(order, vec![(1, 0, None), (2, 1, Some(0)), (3, 1, Some(0)), (4, 2, Some(2)), (5, 3, Some(3)), (6, 1, Some(0))]);
        assert_eq!((t.start, t.end, t.errors, t.roots), (100, 1_100, 2, 1));
        assert_eq!(t.services.into_iter().collect::<Vec<_>>(), vec![("cart".into(), 2), ("checkout".into(), 4)]);
        // A trace held in part: what is held, its missing parent's children as roots.
        let part: Vec<Record> = trace().into_iter().filter(|s| s.span_id != Some([3; 8])).collect();
        let t = assemble(part);
        assert_eq!(t.roots, 2);
        assert_eq!(t.spans.iter().filter(|p| p.depth == 0).map(|p| p.span.span_id.unwrap()[0]).collect::<Vec<_>>(), vec![1, 4]);
        // Parents in a cycle (malformed): every span placed once all the same.
        let cycle = vec![span("a", 1, Some(2), "internal", 0, 1, false, &[]), span("a", 2, Some(1), "internal", 1, 1, false, &[])];
        let t = assemble(cycle);
        assert_eq!(t.spans.len(), 2);
        assert_eq!(assemble(Vec::new()), Trace::default());
    }

    #[test]
    fn the_map_has_who_calls_whom_and_what_is_only_named() {
        let mut m = MapBuilder::new();
        let mut raw = Vec::new();
        let mut prev = 0;
        let spans = trace();
        for s in &spans {
            put_row(&mut raw, s, prev);
            prev = s.ts;
        }
        let mut rows = Rows::new(&raw, spans.len());
        while let Some(r) = rows.next_row().unwrap() {
            m.take(&r);
        }
        let map = m.finish();
        let edges: Vec<(&str, &str, u64, u64, u64, bool)> = map.edges.iter().map(|e| (e.from.as_str(), e.to.as_str(), e.calls, e.errors, e.duration_ns, e.inferred)).collect();
        assert_eq!(
            edges,
            vec![
                // The cart's server span answers checkout's client span: the client's duration.
                ("cart", "postgresql", 1, 0, 400, true),
                ("checkout", "cart", 1, 0, 800, false),
                ("checkout", "kafka", 1, 1, 10, true),
            ]
        );
        let nodes: Vec<(&str, u64, u64, bool)> = map.nodes.iter().map(|n| (n.service.as_str(), n.hits, n.errors, n.inferred)).collect();
        assert_eq!(nodes, vec![("cart", 1, 0, false), ("checkout", 1, 1, false), ("kafka", 0, 0, true), ("postgresql", 0, 0, true)]);
        assert_eq!(map.spans, 7);
        // Each span in the trace metrics' group that counts it (the span inside checkout in none),
        // and each edge by the group that counts its calls: the caller's calls out, by peer.
        let calls = |s: &str, p: &str| Group::Calls(s.into(), p.into());
        assert_eq!(
            map.counted,
            vec![
                (Group::Entries("cart".into()), 1, 0),
                (Group::Entries("checkout".into()), 1, 1),
                (calls("cart", "postgresql"), 1, 0),
                (calls("checkout", "cart.internal"), 1, 0),
                (calls("checkout", "kafka"), 1, 1),
            ]
        );
        let seen: Vec<Vec<(Option<Group>, u64, u64, u64)>> = map.edges.iter().map(|e| e.seen.clone()).collect();
        assert_eq!(seen, vec![vec![(Some(calls("cart", "postgresql")), 1, 0, 400)], vec![(Some(calls("checkout", "cart.internal")), 1, 0, 800)], vec![(Some(calls("checkout", "kafka")), 1, 1, 10)]]);
    }

    /// `traces`' spans, each trace its own id, at `base` (ns) on: their rows.
    fn rows_of(traces: &[Vec<Record>]) -> (Vec<u8>, usize) {
        let (mut raw, mut prev, mut n) = (Vec::new(), 0, 0);
        for s in traces.iter().flatten() {
            put_row(&mut raw, s, prev);
            prev = s.ts;
            n += 1;
        }
        (raw, n)
    }

    fn map_of(traces: &[Vec<Record>]) -> ServiceMap {
        let (raw, n) = rows_of(traces);
        let mut m = MapBuilder::new();
        let mut rows = Rows::new(&raw, n);
        while let Some(r) = rows.next_row().unwrap() {
            m.take(&r);
        }
        m.finish()
    }

    /// Eleven checkouts (each its own trace), their trace metrics counted from all eleven, the map
    /// read from two of them (as a tracer keeping one in five keeps them): each call is every call
    /// the metrics count, not the two read; a peer only named, what its callers call it; a call no
    /// trace read shows (the eleventh's, to payments), an edge all the same. With no metrics, the
    /// spans read, `k` times.
    #[test]
    fn the_map_s_figures_are_every_call_the_trace_metrics_count_not_the_spans_read() {
        let base = 1_700_000_000_000_000_000i64;
        let traces: Vec<Vec<Record>> = (1..=11u8)
            .map(|t| {
                let mut spans = trace();
                if t == 11 {
                    spans.push(span("checkout", 7, Some(1), "client", 960, 50, false, &[("peer.service", "payments")]));
                }
                spans
                    .into_iter()
                    .map(|mut s| {
                        s.trace_id = Some([t; 16]);
                        s.ts += base;
                        s
                    })
                    .collect()
            })
            .collect();
        let mut logs = crate::logs::store::LogStore::spans();
        let (raw, n) = rows_of(&traces);
        logs.append(raw, n as u32).unwrap();
        let window = crate::logs::metrics::window_of(base);
        let counted = crate::logs::metrics::count(&logs.snapshot(), &trace_metrics(), window).unwrap();
        let mut st = crate::store::Store::new(crate::hash::Keyed::new());
        let mut b = crate::batch::Builder::new();
        for (k, points) in &counted.series {
            b.define(k);
            b.run(k.fp, points.iter().copied());
        }
        st.apply(&b.finish()).unwrap();
        let map = map_of(traces.get(..2).unwrap());
        let ms = window / 1_000_000;
        let f = figures(&st, &map, &[], ms, ms + 10_000, 1).unwrap();
        assert_eq!(figures(&crate::lock::RwLock::new(st), &map, &[], ms, ms + 10_000, 1).unwrap(), f, "of a shared store, read a round of series at a time");
        let edges: Vec<(&str, &str, f64, f64, bool, bool)> = f.edges.iter().map(|e| (e.from.as_str(), e.to.as_str(), e.calls, e.errors, e.inferred, e.counted)).collect();
        assert_eq!(
            edges,
            vec![
                ("cart", "postgresql", 11.0, 0.0, true, true),
                ("checkout", "cart", 11.0, 0.0, false, true),
                ("checkout", "kafka", 11.0, 11.0, true, true),
                ("checkout", "payments", 1.0, 0.0, true, true),
            ]
        );
        let avg = f.edges.iter().find(|e| e.to == "cart").and_then(|e| e.avg_s).unwrap();
        assert!((avg - 800e-9).abs() < 1e-15, "its calls' mean, 800 ns: {avg}");
        let nodes: Vec<(&str, f64, f64, bool)> = f.nodes.iter().map(|n| (n.service.as_str(), n.hits, n.errors, n.inferred)).collect();
        assert_eq!(nodes, vec![("cart", 11.0, 0.0, false), ("checkout", 11.0, 11.0, false), ("kafka", 11.0, 11.0, true), ("payments", 1.0, 0.0, true), ("postgresql", 11.0, 0.0, true)]);
        // No trace metrics in the range: the two traces read, as one in five of them.
        let f = figures(&crate::store::Store::new(crate::hash::Keyed::new()), &map, &[], ms, ms + 10_000, 5).unwrap();
        let edges: Vec<(&str, &str, f64, f64, bool)> = f.edges.iter().map(|e| (e.from.as_str(), e.to.as_str(), e.calls, e.errors, e.counted)).collect();
        assert_eq!(edges, vec![("cart", "postgresql", 10.0, 0.0, false), ("checkout", "cart", 10.0, 0.0, false), ("checkout", "kafka", 10.0, 10.0, false)]);
        let nodes: Vec<(&str, f64, f64)> = f.nodes.iter().map(|n| (n.service.as_str(), n.hits, n.errors)).collect();
        assert_eq!(nodes, vec![("cart", 10.0, 0.0), ("checkout", 10.0, 10.0), ("kafka", 10.0, 10.0), ("postgresql", 10.0, 0.0)]);
    }

    /// A span inside a service whose parent is another service's: a Datadog tracer's is its
    /// service's entry (top-level, as the Agent's stats count it), so the call to it is its entries'
    /// share; OpenTelemetry's is in no group (kiln's metrics from spans count no such span), so the
    /// call to it is the spans read.
    #[test]
    fn a_call_to_a_datadog_tracer_s_top_level_span_is_its_entries_share() {
        let base = 1_700_000_000_000_000_000i64;
        let two = |source: &str| -> Vec<Vec<Record>> {
            (1..=2u8)
                .map(|t| {
                    [span("cart", 1, None, "server", 0, 900, false, &[]), span("cart-db", 2, Some(1), "internal", 10, 500, t == 1, &[])]
                        .into_iter()
                        .map(|mut s| {
                            (s.trace_id, s.ts, s.source) = (Some([t; 16]), s.ts + base, source.to_owned());
                            s
                        })
                        .collect()
                })
                .collect()
        };
        let ms = crate::logs::metrics::window_of(base) / 1_000_000;
        // What an Agent's stats said of the database: ten requests, two failed, 2.5 s in all.
        let mut st = crate::store::Store::new(crate::hash::Keyed::new());
        let keys = crate::otlp::metrics::Keys::new();
        let (mut b, mut key) = (crate::batch::Builder::new(), Vec::new());
        for (name, v) in [("trace.hits", 10.0), ("trace.errors", 2.0), ("trace.duration.sum", 2.5)] {
            let k = keys.key(name, &[("service", "cart-db")], crate::batch::Kind::Count, &mut key).unwrap();
            b.define(&k);
            b.run(k.fp, [(ms, v)]);
        }
        st.apply(&b.finish()).unwrap();
        let edge = |source: &str| {
            let f = figures(&st, &map_of(&two(source)), &[], ms, ms + 10_000, 1).unwrap();
            f.edges.into_iter().map(|e| (e.from, e.to, e.calls, e.errors, e.avg_s, e.counted)).collect::<Vec<_>>()
        };
        assert_eq!(edge(crate::ddtrace::SOURCE), vec![("cart".into(), "cart-db".into(), 10.0, 2.0, Some(0.25), true)]);
        assert_eq!(edge("otel"), vec![("cart".into(), "cart-db".into(), 2.0, 1.0, Some(500e-9), false)]);
    }

    /// Two traces whose spans are numbered alike (as tracers that count spans in order number
    /// them): each span's parent is its own trace's, no call drawn between the traces' services.
    #[test]
    fn spans_numbered_alike_in_two_traces_are_not_each_other_s_parents() {
        let mut m = MapBuilder::new();
        let mut raw = Vec::new();
        let mut prev = 0;
        let mut spans = Vec::new();
        for (t, (front, back)) in [("web-a", "db-a"), ("web-b", "db-b")].into_iter().enumerate() {
            let mut a = span(front, 1, None, "client", 0, 10, false, &[]);
            let mut b = span(back, 2, Some(1), "server", 1, 5, false, &[]);
            a.trace_id = Some([t as u8 + 1; 16]);
            b.trace_id = Some([t as u8 + 1; 16]);
            spans.extend([a, b]);
        }
        for s in &spans {
            put_row(&mut raw, s, prev);
            prev = s.ts;
        }
        let mut rows = Rows::new(&raw, spans.len());
        while let Some(r) = rows.next_row().unwrap() {
            m.take(&r);
        }
        let edges: Vec<(String, String)> = m.finish().edges.into_iter().map(|e| (e.from, e.to)).collect();
        assert_eq!(edges, vec![("web-a".into(), "db-a".into()), ("web-b".into(), "db-b".into())]);
    }

    #[test]
    fn trace_metrics_are_well_formed_and_count_entries_and_exits_apart() {
        let defs = trace_metrics();
        assert_eq!(defs.len(), 6);
        for d in &defs {
            d.check().unwrap();
            assert!(d.name.starts_with(PREFIX));
        }
        // Counted from a store of the trace's spans: checkout serves one request (an error), cart
        // one; checkout makes two calls out (cart, kafka: one failed), cart one.
        let mut store = crate::logs::store::LogStore::spans();
        let mut raw = Vec::new();
        let mut prev = 0;
        let spans: Vec<Record> = trace()
            .into_iter()
            .map(|mut s| {
                s.ts += 1_700_000_000_000_000_000;
                s
            })
            .collect();
        for s in &spans {
            put_row(&mut raw, s, prev);
            prev = s.ts;
        }
        store.append(raw, spans.len() as u32).unwrap();
        let window = crate::logs::metrics::window_of(1_700_000_000_000_000_000);
        let counted = crate::logs::metrics::count(&store.snapshot(), &defs, window).unwrap();
        let value = |metric: &str, tags: &[(&str, &str)]| -> f64 {
            counted
                .series
                .iter()
                .filter(|(k, _)| {
                    let key = crate::batch::parse_key(&k.key).unwrap();
                    key.metric == metric && tags.iter().all(|t| key.tags.contains(t))
                })
                .map(|(_, p)| p.iter().map(|x| x.1).sum::<f64>())
                .sum()
        };
        assert_eq!(value("trace.hits", &[("service", "checkout")]), 1.0);
        assert_eq!(value("trace.errors", &[("service", "checkout")]), 1.0);
        assert_eq!(value("trace.hits", &[("service", "cart"), ("span.kind", "server")]), 1.0);
        assert_eq!(value("trace.outbound.hits", &[("service", "checkout")]), 2.0, "its client's span and its producer's: not the span inside it");
        assert_eq!(value("trace.outbound.hits", &[("service", "checkout"), ("peer.service", "kafka")]), 1.0);
        assert_eq!(value("trace.outbound.errors", &[("service", "checkout"), ("peer.service", "kafka")]), 1.0);
        assert_eq!(value("trace.duration.sum", &[("service", "checkout")]), 1e-6, "1,000 ns in seconds");
        assert_eq!(value("trace.duration.count", &[("service", "cart")]), 1.0);
        // Written to a store and read back as a services table: each service's requests.
        let mut st = crate::store::Store::new(crate::hash::Keyed::new());
        let mut b = crate::batch::Builder::new();
        for (k, points) in &counted.series {
            b.define(k);
            b.run(k.fp, points.iter().copied());
        }
        st.apply(&b.finish()).unwrap();
        let ms = window / 1_000_000;
        let services = red(&st, false, &[], "service", ms, ms + 10_000).unwrap();
        let row = |s: &str| services.iter().find(|r| r.group == s).cloned().unwrap();
        assert_eq!((row("checkout").hits, row("checkout").errors, row("cart").hits, row("cart").errors), (1.0, 1.0, 1.0, 0.0));
        let p50 = row("cart").p50.unwrap();
        assert!((p50 - 700e-9).abs() / 700e-9 < 0.011, "the cart's one request, 700 ns, within the buckets' 1.09%: {p50}");
        assert_eq!(row("cart").max, Some(700e-9));
        let calls = red(&st, true, &[("service", "checkout")], "peer.service", ms, ms + 10_000).unwrap();
        assert_eq!(calls.iter().map(|r| (r.group.as_str(), r.hits, r.errors)).collect::<Vec<_>>(), vec![("cart.internal", 1.0, 0.0), ("kafka", 1.0, 1.0)]);
        // A range without its window: nothing.
        assert!(red(&st, false, &[], "service", ms + 10_000, ms + 20_000).unwrap().iter().all(|r| r.hits == 0.0));
        // Of a shared store, read a round of series at a time: the same.
        let shared = crate::lock::RwLock::new(st);
        assert_eq!(red(&shared, false, &[], "service", ms, ms + 10_000).unwrap(), services);
        assert_eq!(red(&shared, true, &[("service", "checkout")], "peer.service", ms, ms + 10_000).unwrap(), calls);
    }

    #[test]
    fn a_long_range_s_figures_read_from_rollups_are_its_samples_figures() {
        use crate::batch::{Builder, Kind, SeriesKey};
        use crate::rollup::{Col, Rolled};
        use crate::store::{Roll, Scratch, Store};
        // Two days of a service's trace metrics, a point each ten seconds as metrics from spans
        // write them, written six hours at a time, each six hours a block.
        // A whole hour.
        let start = 1_800_000_000_000;
        let hours = 48i64;
        let mut rng = crate::testkit::Rng::new(11);
        let dir = tempfile::tempdir().unwrap();
        let mut st = Store::open(dir.path(), crate::hash::Keyed::with_keys(1, 2)).unwrap();
        let tags = [("env", "prod"), ("resource_name", "GET /"), ("service", "web"), ("span.kind", "server")];
        let key = |name: &str, kind: Kind, extra: Option<(&'static str, &str)>| SeriesKey::new(name, tags.iter().copied().chain(extra), kind).unwrap();
        let (hits, errors, sum, max) =
            (key("trace.hits", Kind::Count, None), key("trace.errors", Kind::Count, None), key("trace.duration.sum", Kind::Count, None), key("trace.duration.max", Kind::Gauge, None));
        let buckets: Vec<(SeriesKey, u64)> =
            [30i64, 35, 40, -20].iter().map(|&b| (key("trace.duration.dist", Kind::Count, Some((crate::dist::BUCKET_TAG, &format!("5:p{b}").replace("p-", "n")))), b.unsigned_abs())).collect();
        for chunk in 0..hours / 6 {
            let mut b = Builder::new();
            for k in [&hits, &errors, &sum, &max].into_iter().chain(buckets.iter().map(|(k, _)| k)) {
                b.define(k);
            }
            let times: Vec<i64> = (0..6 * 360).map(|i| start + chunk * 6 * 3_600_000 + i * 10_000).collect();
            let mut col = |k: &SeriesKey, f: &mut dyn FnMut() -> f64| b.run(k.fp, times.iter().map(|&t| (t, f())).collect::<Vec<_>>());
            col(&hits, &mut || rng.below(20) as f64);
            col(&errors, &mut || rng.below(3) as f64);
            col(&sum, &mut || rng.f64() * 2.0);
            col(&max, &mut || rng.f64() * 0.5);
            for (k, n) in &buckets {
                col(k, &mut || rng.below(*n) as f64);
            }
            st.apply(&b.finish()).unwrap();
            st.cut(crate::manifest::Point { index: chunk as u64 + 1, term: 1 }).unwrap();
            st.compact().unwrap();
        }
        // The hour's tier is there to be read.
        let mut ids = Vec::new();
        st.select(&crate::index::Selector::metric("trace.hits"), &mut ids);
        let (mut ts, mut vs, mut rolled) = (Vec::new(), Vec::new(), Rolled::default());
        let roll = Roll { width: 3_600_000, from: start, to: start + hours * 3_600_000, kind: Kind::Count, cols: &[Col::Sum] };
        st.read_rolled(ids[0], start, start + hours * 3_600_000 - 1, &roll, &mut Scratch::default(), &mut ts, &mut vs, &mut rolled).unwrap();
        assert!(rolled.len() > 40, "{} hours read rolled up", rolled.len());
        // Edges inside windows, read as samples; the middle from the hour's sums.
        for (from, to) in [(start + 1_234, start + hours * 3_600_000 - 5_678), (start + 7 * 3_600_000 + 10_000, start + 31 * 3_600_000 + 20_000)] {
            st.set_read_rollups(true);
            let up = red(&st, false, &[("service", "web")], "service", from, to).unwrap();
            st.set_read_rollups(false);
            let raw = red(&st, false, &[("service", "web")], "service", from, to).unwrap();
            let ([a], [b]) = (up.as_slice(), raw.as_slice()) else { panic!("{up:?} {raw:?}") };
            assert_eq!((a.hits, a.errors, a.max, a.p50, a.p95, a.p99), (b.hits, b.errors, b.max, b.p50, b.p95, b.p99));
            assert!((a.duration_sum - b.duration_sum).abs() <= 1e-9 * b.duration_sum, "{} {}", a.duration_sum, b.duration_sum);
            assert!(a.hits > 0.0);
        }
    }
}
