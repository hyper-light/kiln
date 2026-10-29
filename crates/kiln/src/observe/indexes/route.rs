//! A log record's way among the indexes, as Datadog's is (`docs/observe.md` §8.10): into the first
//! index, in their order, whose filter it matches (none: not indexed); there, the first of its
//! enabled exclusion filters that matches decides alone, excluding it at its rate. The decision is a
//! hash's, so that every node decides alike and a trace's records go together: of the filter's
//! `sample_attribute`'s value where it names one (a record without it kept), else of the record's
//! trace id where it has one (Datadog's), else of the record itself (its time and message).

use std::sync::OnceLock;

use kiln_obs::hash::Keyed;
use kiln_obs::logs::search::{Fields, RowScratch, Search};

/// An exclusion filter, its query read.
#[derive(Debug, Clone)]
pub struct Exclusion {
    pub enabled: bool,
    pub query: Search,
    /// Of the records it matches, the share excluded: 1.0 every one.
    pub rate: f64,
    /// The attribute whose values are sampled (`@ci.job_id`, `trace_id`, `service`), its `@` gone.
    pub attribute: Option<String>,
}

/// An index's way in: its filter and its exclusion filters, in their order.
#[derive(Debug, Clone)]
pub struct Route {
    pub filter: Search,
    pub exclusions: Vec<Exclusion>,
}

/// Where a record goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routed {
    /// Into the index at this place.
    Indexed(usize),
    /// Out of the index at the first place, by its exclusion filter at the second.
    Excluded(usize, usize),
    /// No index's filter matched it.
    Unindexed,
}

/// The hash sampling decides by: keys of its own, fixed, so that a decision is the same on every
/// node and in every run.
fn keyed() -> &'static Keyed {
    static K: OnceLock<Keyed> = OnceLock::new();
    K.get_or_init(|| Keyed::with_keys(0x6b69_6c6e_2e69_6478, 0x6578_636c_7573_696f))
}

/// The hashes below which a record is excluded at `rate`: `rate` of 2^64.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn below(rate: f64) -> u64 {
    // 2^64 as a float; the cast saturates at the top.
    (rate * 18_446_744_073_709_551_616.0) as u64
}

/// Whether exclusion filter `e`, matching a record, excludes it.
fn excludes<F: Fields>(e: &Exclusion, r: &F, ts: i64) -> bool {
    if e.rate <= 0.0 {
        return false;
    }
    let k = keyed();
    let hash = match e.attribute.as_deref() {
        // Only records with it are sampled: one without it is kept.
        Some("trace_id") => match r.trace_id() {
            Some(t) => k.bytes(&t),
            None => return false,
        },
        Some("span_id") => match r.span_id() {
            Some(t) => k.bytes(&t),
            None => return false,
        },
        Some("service") => k.bytes(r.service().as_bytes()),
        Some("host") => k.bytes(r.host().as_bytes()),
        Some("source") => k.bytes(r.source().as_bytes()),
        Some(a) => match r.attr(a) {
            Some(v) => k.bytes(v.as_bytes()),
            None => return false,
        },
        // A trace's records together, else each its own (its message at its time).
        None => match r.trace_id() {
            Some(t) => k.bytes(&t),
            None => k.word(k.bytes(r.message().as_bytes()) ^ ts.cast_unsigned()),
        },
    };
    e.rate >= 1.0 || hash < below(e.rate)
}

/// Where a record (its time `ts`, ns) goes among `routes`, in their order.
pub fn route<F: Fields>(routes: &[Route], r: &F, ts: i64, s: &mut RowScratch) -> Routed {
    let Some((i, into)) = routes.iter().enumerate().find(|(_, x)| x.filter.fields(r, s)) else { return Routed::Unindexed };
    // The first enabled filter that matches decides, whatever it decides.
    match into.exclusions.iter().enumerate().find(|(_, e)| e.enabled && e.query.fields(r, s)) {
        Some((j, e)) if excludes(e, r, ts) => Routed::Excluded(i, j),
        _ => Routed::Indexed(i),
    }
}

/// When a daily quota's day began: `hour`:00 at `offset` hours from UTC (Datadog's `reset_time` and
/// `reset_utc_offset`), the latest at or before `now_ms`.
pub fn day_start(now_ms: i64, hour: i64, offset: i64) -> i64 {
    const DAY: i64 = 86_400_000;
    let at = (hour - offset).rem_euclid(24) * 3_600_000;
    let start = now_ms.div_euclid(DAY) * DAY + at;
    if start <= now_ms { start } else { start - DAY }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::cast_precision_loss)]
mod tests {
    use kiln_obs::logs::record::Record;
    use kiln_obs::logs::search::parse;

    use super::*;

    fn rec(service: &str, message: &str, attrs: &[(&str, &str)], trace: Option<u8>) -> Record {
        Record {
            ts: 0,
            severity: 9,
            service: service.into(),
            host: "web-1".into(),
            source: "app".into(),
            message: message.into(),
            attrs: attrs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect(),
            trace_id: trace.map(|t| [t; 16]),
            span_id: None,
        }
    }

    fn ex(query: &str, rate: f64, attribute: Option<&str>) -> Exclusion {
        Exclusion { enabled: true, query: parse(query).unwrap(), rate, attribute: attribute.map(str::to_owned) }
    }

    /// The first index whose filter matches, whatever the later ones say; none, unindexed; the
    /// first enabled exclusion filter that matches decides alone (a later one excluding all is not
    /// asked), a disabled one passed over.
    #[test]
    fn a_record_goes_into_the_first_index_and_its_first_exclusion_decides() {
        let routes = vec![
            Route { filter: parse("service:payments").unwrap(), exclusions: vec![ex("status:info", 0.0, None), ex("*", 1.0, None)] },
            Route { filter: parse("service:web OR service:payments").unwrap(), exclusions: vec![Exclusion { enabled: false, ..ex("*", 1.0, None) }, ex("GET", 1.0, None)] },
        ];
        let mut s = RowScratch::default();
        assert_eq!(route(&routes, &rec("payments", "charged", &[], None), 1, &mut s), Routed::Indexed(0), "kept by the first that matched, at 0%");
        assert_eq!(route(&routes, &rec("web", "GET /", &[], None), 1, &mut s), Routed::Excluded(1, 1));
        assert_eq!(route(&routes, &rec("web", "POST /", &[], None), 1, &mut s), Routed::Indexed(1), "the disabled one passed over");
        assert_eq!(route(&routes, &rec("batch", "done", &[], None), 1, &mut s), Routed::Unindexed);
    }

    /// At a rate, about that share excluded (100,000 records: within 1%), each decided alike every
    /// time; a trace's records all one way; by an attribute, its values all one way, and a record
    /// without it kept.
    #[test]
    fn exclusion_samples_at_its_rate_and_keeps_traces_and_values_together() {
        let mut s = RowScratch::default();
        let routes = vec![Route { filter: parse("*").unwrap(), exclusions: vec![ex("*", 0.9, None)] }];
        let out = (0..100_000).filter(|&i| route(&routes, &rec("web", &format!("GET /item/{}", i % 7), &[], None), i, &mut s) != Routed::Indexed(0)).count();
        assert!((89_000..=91_000).contains(&out), "90% excluded: {out}");
        let again = (0..100_000).filter(|&i| route(&routes, &rec("web", &format!("GET /item/{}", i % 7), &[], None), i, &mut s) != Routed::Indexed(0)).count();
        assert_eq!(out, again, "decided alike");
        for t in 0..50u8 {
            let ways: Vec<Routed> = (0..20).map(|i| route(&routes, &rec("web", &format!("step {i}"), &[], Some(t)), i64::from(i), &mut s)).collect();
            assert!(ways.iter().all(|w| *w == ways[0]), "a trace's records together");
        }
        let by = vec![Route { filter: parse("*").unwrap(), exclusions: vec![ex("*", 0.5, Some("ci.job_id"))] }];
        let mut kept_jobs = 0;
        for job in 0..1_000 {
            let id = job.to_string();
            let ways: Vec<Routed> = (0..5).map(|i| route(&by, &rec("ci", "line", &[("ci.job_id", &id)], None), i, &mut s)).collect();
            assert!(ways.iter().all(|w| *w == ways[0]), "a job's records together");
            kept_jobs += usize::from(ways[0] == Routed::Indexed(0));
        }
        assert!((430..=570).contains(&kept_jobs), "half the jobs kept: {kept_jobs}");
        assert_eq!(route(&by, &rec("ci", "line", &[], None), 1, &mut s), Routed::Indexed(0), "without the attribute, kept");
    }

    /// A quota's day: 14:00 UTC unless said; at an offset, its hour there.
    #[test]
    fn a_quota_s_day_begins_at_its_hour_where_it_is_said() {
        let h = 3_600_000;
        let day = 86_400_000 * 20_000;
        assert_eq!(day_start(day + 15 * h, 14, 0), day + 14 * h);
        assert_eq!(day_start(day + 13 * h, 14, 0), day - 10 * h, "before 14:00: yesterday's");
        assert_eq!(day_start(day + 13 * h, 14, 2), day + 12 * h, "14:00 at +02:00 is 12:00 UTC");
        assert_eq!(day_start(day + 20 * h, 14, -5), day + 19 * h, "14:00 at -05:00 is 19:00 UTC");
        assert_eq!(day_start(day + 14 * h, 14, 0), day + 14 * h, "at its hour, its own");
    }
}
