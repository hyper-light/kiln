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
//!   the group counts once, and a resent one not twice.
//! - **A trace** ([`assemble`]): its spans in the order a waterfall draws them, each after its
//!   parent and siblings by their start, with its depth; a span twice (an exporter's retry) once.
//! - **A service map** ([`MapBuilder`]): who calls whom, from a range's spans. A span whose parent is
//!   of another service is a call of that service to its own (the parent's duration, when the
//!   parent is the caller's client span, is the call's); a client's or a producer's span with no
//!   such child is a call to what it names its peer (`peer.service`), inferred.

use std::collections::{BTreeMap, HashMap};

use crate::logs::metrics::{Compute, LogMetric};
use crate::logs::record::{Record, RecordRef};
use crate::logs::search::parse_spans;
use crate::logs::store::Facet;

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
    let errors_in = |q: &str| format!("({q}) status:error");
    [
        def("trace.hits", ENTRY, &entry_by, Compute::Count),
        def("trace.errors", &errors_in(ENTRY), &entry_by, Compute::Count),
        def("trace.duration", ENTRY, &entry_by, Compute::Duration { percentiles: true }),
        def("trace.outbound.hits", EXIT, &exit_by, Compute::Count),
        def("trace.outbound.errors", &errors_in(EXIT), &exit_by, Compute::Count),
        def("trace.outbound.duration", EXIT, &exit_by, Compute::Duration { percentiles: true }),
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
pub fn red(store: &crate::store::Store, outbound: bool, filter: &[(&str, &str)], by: &str, from: i64, to: i64) -> Result<Vec<Red>, crate::store::StoreError> {
    use crate::dist::{Merged, SCALE, parse_tag_value};
    let prefix = if outbound { "trace.outbound" } else { "trace" };
    let mut groups: BTreeMap<String, (Red, Merged)> = BTreeMap::new();
    let (mut ids, mut ts, mut vs) = (Vec::new(), Vec::new(), Vec::new());
    let mut scratch = crate::store::Scratch::default();
    for part in ["hits", "errors", "duration.sum", "duration.max", "duration.dist"] {
        let name = format!("{prefix}.{part}");
        let mut sel = crate::index::Selector::metric(&name);
        for (k, v) in filter {
            sel = sel.tag(k, v);
        }
        ids.clear();
        store.select(&sel, &mut ids);
        for &id in &ids {
            let Some(series) = store.series(id) else { continue };
            let group = series.tag(by).unwrap_or(crate::logs::metrics::NONE).to_owned();
            let bucket = if part == "duration.dist" { series.tag(crate::dist::BUCKET_TAG).and_then(parse_tag_value) } else { None };
            ts.clear();
            vs.clear();
            store.read_with(id, from, to.saturating_sub(1), &mut scratch, &mut ts, &mut vs)?;
            let (r, m) = groups.entry(group.clone()).or_insert_with(|| (Red { group, ..Red::default() }, Merged::new(SCALE)));
            match part {
                "hits" => r.hits += vs.iter().sum::<f64>(),
                "errors" => r.errors += vs.iter().sum::<f64>(),
                "duration.sum" => r.duration_sum += vs.iter().sum::<f64>(),
                "duration.max" => {
                    if let Some(x) = vs.iter().copied().filter(|v| v.is_finite()).reduce(f64::max) {
                        r.max = Some(r.max.map_or(x, |m| m.max(x)));
                    }
                }
                _ => {
                    if let Some((scale, b)) = bucket {
                        m.add(b, scale, vs.iter().sum());
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
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceMap {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// Spans read.
    pub spans: u64,
}

/// A span the map keeps until it is built: its service, its duration, whether it erred, whether it
/// is a call out (a client's or a producer's) and the peer it names.
struct Seen {
    service: u32,
    duration: u64,
    error: bool,
    call: bool,
    peer: Option<u32>,
    /// A span of another service has it as its parent.
    answered: bool,
}

/// A span's place in the map: its trace's id and its own (a span id is its trace's, not unique
/// across traces: tracers that number spans in order reuse them in every trace).
type SpanKey = ([u8; 16], [u8; 8]);

/// Builds a service map from spans, one at a time ([`MapBuilder::take`]).
#[derive(Default)]
pub struct MapBuilder {
    names: Vec<String>,
    by_name: HashMap<String, u32>,
    spans: HashMap<SpanKey, Seen>,
    /// Spans with a parent: the parent's place, the span's service, whether it erred, its duration.
    links: Vec<(SpanKey, u32, bool, u64)>,
    entries: HashMap<u32, (u64, u64)>,
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
        self.read += 1;
        let service = self.name(if r.service.is_empty() { "unknown" } else { r.service });
        let kind = r.attr("span.kind").unwrap_or("internal");
        let duration = r.attr("duration").and_then(|d| d.parse().ok()).unwrap_or(0);
        let error = r.severity >= crate::otlp::traces::SEVERITY_ERROR;
        let parent = r.attr("parent_id").and_then(span_id_of);
        let call = kind == "client" || kind == "producer";
        if kind == "server" || kind == "consumer" || (parent.is_none() && !call) {
            let e = self.entries.entry(service).or_default();
            e.0 += 1;
            e.1 += u64::from(error);
        }
        let peer = if call { r.attr("peer.service").filter(|p| !p.is_empty()).map(|p| self.name(p)) } else { None };
        let trace = r.trace_id.unwrap_or([0; 16]);
        if let Some(id) = r.span_id {
            self.spans.entry((trace, id)).or_insert(Seen { service, duration, error, call, peer, answered: false });
        }
        if let Some(p) = parent {
            self.links.push(((trace, p), service, error, duration));
        }
    }

    pub fn finish(mut self) -> ServiceMap {
        let mut edges: BTreeMap<(u32, u32, bool), Edge> = BTreeMap::new();
        for (parent, service, error, duration) in std::mem::take(&mut self.links) {
            let Some(p) = self.spans.get_mut(&parent) else { continue };
            if p.service == service {
                continue;
            }
            p.answered = true;
            let e = edges.entry((p.service, service, false)).or_default();
            e.calls += 1;
            e.errors += u64::from(error);
            e.duration_ns += if p.call { p.duration } else { duration };
        }
        for s in self.spans.values() {
            if let (true, false, Some(peer)) = (s.call, s.answered, s.peer)
                && peer != s.service
            {
                let e = edges.entry((s.service, peer, true)).or_default();
                e.calls += 1;
                e.errors += u64::from(s.error);
                e.duration_ns += s.duration;
            }
        }
        let name = |i: u32| self.names.get(i as usize).cloned().unwrap_or_default();
        let mut nodes: BTreeMap<u32, Node> = BTreeMap::new();
        for (&s, &(hits, errors)) in &self.entries {
            nodes.insert(s, Node { service: name(s), hits, errors, inferred: false });
        }
        let mut out_edges = Vec::with_capacity(edges.len());
        for ((from, to, inferred), mut e) in edges {
            for (s, inf) in [(from, false), (to, inferred)] {
                nodes.entry(s).or_insert_with(|| Node { service: name(s), inferred: inf, ..Node::default() });
            }
            (e.from, e.to, e.inferred) = (name(from), name(to), inferred);
            out_edges.push(e);
        }
        let mut nodes: Vec<Node> = nodes.into_values().collect();
        nodes.sort_by(|a, b| a.service.cmp(&b.service));
        out_edges.sort_by(|a, b| (&a.from, &a.to, a.inferred).cmp(&(&b.from, &b.to, b.inferred)));
        ServiceMap { nodes, edges: out_edges, spans: self.read }
    }
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
        let counted = crate::logs::metrics::count(&store, &defs, window).unwrap();
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
    }
}
