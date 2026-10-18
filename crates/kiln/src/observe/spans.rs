//! kiln's own spans in observe (`docs/observe.md` §8): each closed span of a target named in
//! [`TARGETS`] becomes, as an APM's trace metrics are made:
//! - `kiln.<span name>.duration` (seconds of wall time from its creation to its close; a gauge, one
//!   point each) and `kiln.<span name>.count` (a count), tagged with the fields, its own and its
//!   ancestors', whose names are low in cardinality ([`TAGS`]);
//! - an event: source `kiln.event`, service `kiln.<the target's last word>`, `event.type` the span's
//!   name, every field an attribute (ids too, never tags: `@upgrade_id:…` finds one upgrade's
//!   story), an error when its `result` says it failed or rolled back, when it holds an `error`, or
//!   when it is a `….rollback`.
//!
//! The bridge is a layer of its own, with its own filter: what `KILN_LOG` says of the log's lines
//! does not hide these spans from it, nor do its spans reach the log. A span's fields are kept in
//! its extensions while it lives; the write is the owner's, off the closing thread (a channel).

use std::sync::OnceLock;
use std::time::Instant;

use kiln_obs::batch::{Kind, SeriesKey};
use kiln_obs::logs::record::Record;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record as Values};
use tracing::{Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// The targets whose spans are observe's: runners' blue/green upgrades (`upgrade`, `upgrade.start`,
/// `upgrade.healthy`, `upgrade.move`, `upgrade.retire`, `upgrade.rollback`).
pub const TARGETS: &[&str] = &["kiln::upgrade"];

/// Fields that become tags: few values each. Any other (an id, an error's words) is an attribute of
/// the event alone.
pub const TAGS: &[&str] = &["runner", "from", "to", "what", "result", "reason", "executor", "kind", "status", "policy"];

/// Results that are failures.
const FAILED: &[&str] = &["failed", "failure", "error", "rolled_back", "timed_out"];

/// A span's fields and when it began, kept in its extensions.
struct Open {
    start: Instant,
    at_ms: i64,
    fields: Vec<(&'static str, String)>,
}

struct Visitor<'a>(&'a mut Vec<(&'static str, String)>);

impl Visitor<'_> {
    fn set(&mut self, k: &'static str, v: String) {
        match self.0.iter_mut().find(|(n, _)| *n == k) {
            Some(e) => e.1 = v,
            None => self.0.push((k, v)),
        }
    }
}

impl Visit for Visitor<'_> {
    fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
        self.set(f.name(), format!("{v:?}"));
    }

    fn record_str(&mut self, f: &Field, v: &str) {
        self.set(f.name(), v.to_owned());
    }

    fn record_i64(&mut self, f: &Field, v: i64) {
        self.set(f.name(), v.to_string());
    }

    fn record_u64(&mut self, f: &Field, v: u64) {
        self.set(f.name(), v.to_string());
    }

    fn record_bool(&mut self, f: &Field, v: bool) {
        self.set(f.name(), v.to_string());
    }

    fn record_f64(&mut self, f: &Field, v: f64) {
        self.set(f.name(), v.to_string());
    }
}

/// A span closed: what it was, when, how long, and its fields (its own, then its ancestors' it does
/// not set itself, nearest first).
#[derive(Debug, Clone, PartialEq)]
pub struct Closed {
    pub target: &'static str,
    pub name: &'static str,
    /// Unix ms.
    pub at_ms: i64,
    pub end_ms: i64,
    pub seconds: f64,
    pub fields: Vec<(&'static str, String)>,
}

impl Closed {
    fn field(&self, k: &str) -> Option<&str> {
        self.fields.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str())
    }

    pub fn failed(&self) -> bool {
        self.field("result").is_some_and(|r| FAILED.contains(&r)) || self.field("error").is_some_and(|e| !e.is_empty()) || self.name.ends_with(".rollback")
    }
}

/// The bridge: what each closed span of its targets becomes, `sink`'s.
pub struct Spans<F> {
    sink: F,
}

impl<S, F> Layer<S> for Spans<F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    F: Fn(Closed) + Send + Sync + 'static,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut fields = Vec::with_capacity(attrs.fields().len());
        attrs.record(&mut Visitor(&mut fields));
        span.extensions_mut().insert(Open { start: Instant::now(), at_ms: super::clock_ms(), fields });
    }

    fn on_record(&self, id: &Id, values: &Values<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id)
            && let Some(open) = span.extensions_mut().get_mut::<Open>()
        {
            values.record(&mut Visitor(&mut open.fields));
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(open) = span.extensions_mut().remove::<Open>() else { return };
        let seconds = open.start.elapsed().as_secs_f64();
        let mut fields = open.fields;
        for parent in span.scope().skip(1) {
            if let Some(p) = parent.extensions().get::<Open>() {
                for (k, v) in &p.fields {
                    if !fields.iter().any(|(n, _)| n == k) {
                        fields.push((k, v.clone()));
                    }
                }
            }
        }
        let meta = span.metadata();
        let end_ms = open.at_ms.saturating_add((seconds * 1000.0) as i64);
        (self.sink)(Closed { target: meta.target(), name: meta.name(), at_ms: open.at_ms, end_ms, seconds, fields });
    }
}

/// The layer kiln's subscriber holds: [`TARGETS`]' spans, at any level, written to this node's
/// observe once it started (none before).
pub fn layer<S>() -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let filter = TARGETS.iter().fold(Targets::new(), |t, target| t.with_target(*target, Level::TRACE));
    Spans { sink: write }.with_filter(filter)
}

fn write(c: Closed) {
    let Some(o) = super::api::get() else { return };
    let (points, record) = observed(&c);
    o.write(points);
    o.log(vec![record]);
}

fn hostname() -> &'static str {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(crate::os::hostname)
}

/// A duration as people say it: `340 ms`, `1.2 s`, `2 min 3 s`.
fn took(seconds: f64) -> String {
    if seconds < 1.0 {
        format!("{} ms", (seconds * 1000.0).round())
    } else if seconds < 60.0 {
        format!("{seconds:.1} s")
    } else {
        let s = seconds.round() as u64;
        format!("{} min {} s", s / 60, s % 60)
    }
}

/// A closed span as observe's: its two points and its event.
pub fn observed(c: &Closed) -> (Vec<(SeriesKey, Vec<(i64, f64)>)>, Record) {
    let tags: Vec<(&str, &str)> = c.fields.iter().filter(|(k, v)| TAGS.contains(k) && !v.is_empty()).map(|(k, v)| (*k, v.as_str())).collect();
    let mut points = Vec::with_capacity(2);
    for (suffix, kind, v) in [("duration", Kind::Gauge, c.seconds), ("count", Kind::Count, 1.0)] {
        if let Ok(k) = SeriesKey::new(&format!("kiln.{}.{suffix}", c.name), tags.iter().copied(), kind) {
            points.push((k, vec![(c.end_ms, v)]));
        }
    }
    let what: Vec<&str> = ["what", "id"].iter().filter_map(|k| c.field(k)).collect();
    let on = c.field("runner").map(|r| format!(" on {r}")).unwrap_or_default();
    let outcome = c.field("result").unwrap_or(if c.failed() { "failed" } else { "done" });
    let error = c.field("error").filter(|e| !e.is_empty()).map(|e| format!(": {e}")).unwrap_or_default();
    let message = format!("{}{}{on}: {outcome} in {}{error}", c.name, if what.is_empty() { String::new() } else { format!(" {}", what.join(" ")) }, took(c.seconds));
    let mut attrs: Vec<(String, String)> = vec![("event.type".into(), c.name.into()), ("duration_ms".into(), format!("{}", (c.seconds * 1000.0).round()))];
    attrs.extend(c.fields.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| ((*k).to_owned(), v.clone())));
    let record = Record {
        ts: c.end_ms.saturating_mul(1_000_000),
        severity: if c.failed() { 17 } else { 9 },
        service: format!("kiln.{}", c.target.rsplit("::").next().unwrap_or(c.target)),
        host: hostname().to_owned(),
        source: "kiln.event".into(),
        message,
        attrs,
        trace_id: None,
        span_id: None,
    };
    (points, record)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use tracing_subscriber::prelude::*;

    use super::*;

    fn closed_by(f: impl FnOnce()) -> Vec<Closed> {
        let got: Arc<Mutex<Vec<Closed>>> = Arc::default();
        let sink = got.clone();
        let filter = TARGETS.iter().fold(Targets::new(), |t, target| t.with_target(*target, Level::TRACE));
        // The log's own layer, filtered to warnings: the bridge sees what it hides.
        let subscriber = tracing_subscriber::registry().with(Spans { sink: move |c| sink.lock().unwrap_or_else(PoisonError::into_inner).push(c) }.with_filter(filter)).with(tracing_subscriber::fmt::layer().with_test_writer().with_filter(tracing_subscriber::filter::LevelFilter::WARN));
        tracing::subscriber::with_default(subscriber, f);
        got.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// An upgrade as the runners' will make one: the root's fields reach its children, results
    /// recorded at the end are kept, a child moved to another thread closes there, spans of other
    /// targets are not observe's (nor do they hide a bridged span beneath them).
    #[test]
    fn an_upgrade_s_spans_close_into_what_observe_keeps() {
        let closed = closed_by(|| {
            let root = tracing::info_span!(target: "kiln::upgrade", "upgrade", runner = "adacachy", from = "40f0c83", to = "5025744", upgrade_id = "u-1", result = tracing::field::Empty, error = tracing::field::Empty);
            root.in_scope(|| {
                tracing::info_span!(target: "kiln::upgrade", "upgrade.start").in_scope(|| std::thread::sleep(Duration::from_millis(20)));
                let moved = tracing::info_span!(target: "kiln::upgrade", "upgrade.move", what = "session", id = "s-7f3a", result = tracing::field::Empty);
                // Every thread has the subscriber, as kiln's global one is on every thread (a span
                // closed on a thread without it would never release its parent).
                let dispatch = tracing::dispatcher::get_default(Clone::clone);
                let t = std::thread::spawn(move || {
                    tracing::dispatcher::with_default(&dispatch, || {
                        moved.in_scope(|| std::thread::sleep(Duration::from_millis(5)));
                        moved.record("result", "ok");
                        drop(moved);
                    });
                });
                let _ = t.join();
                let other = tracing::info_span!(target: "kiln::terminal", "terminal.attach");
                other.in_scope(|| {
                    tracing::debug_span!(target: "kiln::upgrade", "upgrade.rollback", reason = "health check failed").in_scope(|| {});
                });
            });
            root.record("result", "rolled_back");
            root.record("error", "the new instance never became healthy");
            drop(root);
        });
        let names: Vec<&str> = closed.iter().map(|c| c.name).collect();
        assert_eq!(names, ["upgrade.start", "upgrade.move", "upgrade.rollback", "upgrade"]);
        let [start, moved, rollback, root] = closed.as_slice() else { return };
        assert!(start.seconds >= 0.02, "{start:?}");
        assert_eq!(start.field("runner"), Some("adacachy"), "a child takes its root's fields");
        assert_eq!(start.field("upgrade_id"), Some("u-1"));
        assert_eq!((moved.field("what"), moved.field("id"), moved.field("result")), (Some("session"), Some("s-7f3a"), Some("ok")));
        assert!(!moved.failed());
        assert!(rollback.failed() && rollback.field("runner") == Some("adacachy"), "a rollback beneath another target's span: bridged, its root's fields its own");
        assert!(root.failed() && root.field("result") == Some("rolled_back") && root.seconds >= start.seconds);
        assert!(root.end_ms >= root.at_ms);
    }

    #[test]
    fn a_closed_span_is_two_points_tagged_low_and_an_event_holding_every_field() {
        let c = Closed {
            target: "kiln::upgrade",
            name: "upgrade.move",
            at_ms: 1_800_000_000_000,
            end_ms: 1_800_000_001_240,
            seconds: 1.24,
            fields: vec![("what", "job".into()), ("id", "j-42".into()), ("result", "failed".into()), ("error", "the host refused".into()), ("runner", "mac-mini".into()), ("upgrade_id", "u-9".into())],
        };
        let (points, r) = observed(&c);
        let keys: Vec<(String, Vec<(String, String)>, Vec<(i64, f64)>)> = points
            .iter()
            .filter_map(|(k, p)| {
                let key = kiln_obs::batch::parse_key(&k.key).ok()?;
                Some((key.metric.to_owned(), key.tags.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect(), p.clone()))
            })
            .collect();
        let tags: Vec<(String, String)> = [("result", "failed"), ("runner", "mac-mini"), ("what", "job")].iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect();
        assert_eq!(keys, vec![("kiln.upgrade.move.duration".to_owned(), tags.clone(), vec![(1_800_000_001_240, 1.24)]), ("kiln.upgrade.move.count".to_owned(), tags, vec![(1_800_000_001_240, 1.0)])]);
        assert_eq!(points.get(1).map(|(k, _)| k.kind), Some(Kind::Count));
        assert_eq!(r.service, "kiln.upgrade");
        assert_eq!(r.source, "kiln.event");
        assert_eq!(r.severity, 17);
        assert_eq!(r.message, "upgrade.move job j-42 on mac-mini: failed in 1.2 s: the host refused");
        for (k, v) in [("event.type", "upgrade.move"), ("id", "j-42"), ("upgrade_id", "u-9"), ("duration_ms", "1240"), ("error", "the host refused")] {
            assert!(r.attrs.iter().any(|(a, b)| a == k && b == v), "{k}={v} in {:?}", r.attrs);
        }
        assert_eq!(took(0.34), "340 ms");
        assert_eq!(took(123.4), "2 min 3 s");
    }
}
