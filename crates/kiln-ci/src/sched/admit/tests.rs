//! Admission's decisions (docs/ci.md W28-R60 to R63, R68), in seconds.

use super::{Admit, Cadence, Coalesce, Decision, Demand, Held, Skipped, Window};

const BUSY: Coalesce = Coalesce { quiet: 45, max_wait: 300, always: false };

fn push(order: u64, coalesce: Option<Coalesce>, cadence: Option<Cadence>) -> Demand {
    Demand { lane: "acme/x\0ci\0main".into(), unit: "test".into(), order, coalesce, cadence, speculative: false }
}

fn admitted(ds: &[Decision]) -> Vec<String> {
    ds.iter().filter_map(|d| if let Decision::Admit { id } = d { Some(id.clone()) } else { None }).collect()
}

fn skipped(ds: &[Decision]) -> Vec<(String, String)> {
    ds.iter().filter_map(|d| if let Decision::Skip { id, by, .. } = d { Some((id.clone(), by.clone())) } else { None }).collect()
}

/// W28-R61: on an idle system a demand is admitted at once: the quiet period applies only while
/// its resources are saturated.
#[test]
fn w28_r61_idle_admits_at_once() {
    let mut a = Admit::default();
    assert_eq!(a.arrive("p0", push(0, Some(BUSY), None), false, 10), vec![Decision::Admit { id: "p0".into() }]);
    assert!(a.idle());
}

/// W28-R61, W28-AC60: pushes every 30 s for 10 min, quiet 45 s, max_wait 5 min, resources
/// saturated throughout: each newer push skips the one held, and the burst is admitted by
/// max_wait, on the newest commit then.
#[test]
fn w28_ac60_a_steady_stream_is_admitted_within_max_wait_on_the_newest() {
    let mut a = Admit::default();
    let mut first: Option<(i64, String)> = None;
    let mut skips = 0;
    for i in 0..20 {
        let t = i * 30;
        // The timers due before this push.
        let due = a.review(t, &|_| false);
        let ds = [due, a.arrive(&format!("p{i}"), push(i as u64, Some(BUSY), None), true, t)].concat();
        skips += skipped(&ds).len();
        if let (None, Some(id)) = (&first, admitted(&ds).first()) {
            first = Some((t, id.clone()));
        }
        for d in &ds {
            if let Decision::Hold { held: Held::Coalescing { until, deadline, .. }, .. } = d {
                assert!(*until <= *deadline && *deadline - t <= 300, "held at {t} until {until}, deadline {deadline}");
                assert!(*until - t <= 45);
            }
        }
    }
    let (at, id) = first.unwrap_or((i64::MAX, String::new()));
    assert!(at <= 300, "first admitted at {at}");
    // The newest push at that time (the one at 300 s, or the one held from 270 s).
    assert!(id == "p10" || id == "p9", "admitted {id}");
    assert!(skips >= 9, "{skips} skipped");
}

/// W28-R61: a held demand is released as soon as its resources stop being saturated.
#[test]
fn w28_r61_released_when_saturation_ends() {
    let mut a = Admit::default();
    let ds = a.arrive("p0", push(0, Some(BUSY), None), true, 0);
    assert!(matches!(ds.first(), Some(Decision::Hold { held: Held::Coalescing { until: 45, deadline: 300, .. }, .. })));
    assert_eq!(a.coalescing().collect::<Vec<_>>(), vec!["p0"]);
    assert!(a.review(10, &|_| false).is_empty(), "still saturated, not due");
    assert_eq!(admitted(&a.review(20, &|id| id == "p0")), vec!["p0"]);
    assert!(a.idle());
}

/// W28-R61: a demand older than the one held (a rerun asked for since) goes on its own, and
/// `when: always` holds whether or not anything is saturated, until its quiet period ends.
#[test]
fn w28_r61_older_goes_alone_and_always_holds() {
    let mut a = Admit::default();
    let _ = a.arrive("p5", push(5, Some(BUSY), None), true, 0);
    assert_eq!(admitted(&a.arrive("p3", push(3, Some(BUSY), None), true, 1)), vec!["p3"]);
    assert!(a.held("p5").is_some(), "the newer one stays held");
    let always = Coalesce { always: true, ..BUSY };
    let mut a = Admit::default();
    assert!(matches!(a.arrive("q0", push(0, Some(always), None), false, 0).first(), Some(Decision::Hold { .. })));
    assert!(a.review(30, &|_| true).is_empty(), "unsaturated doesn't release always");
    assert_eq!(admitted(&a.review(45, &|_| true)), vec!["q0"]);
}

/// W28-R62: on a cadence one demand of a lane and unit waits to start, the newest; an older one
/// arriving after it is skipped for it; one started is never skipped.
#[test]
fn w28_r62_cadence_keeps_the_newest_pending() {
    let mut a = Admit::default();
    assert_eq!(admitted(&a.arrive("b1", push(1, None, Some(Cadence::Free)), true, 0)), vec!["b1"]);
    let ds = a.arrive("b2", push(2, None, Some(Cadence::Free)), true, 5);
    assert_eq!(skipped(&ds), vec![("b1".to_owned(), "b2".to_owned())]);
    assert!(ds.iter().any(|d| matches!(d, Decision::Skip { why: Skipped::Cadence, .. })));
    assert_eq!(admitted(&ds), vec!["b2"]);
    assert_eq!(skipped(&a.arrive("b0", push(0, None, Some(Cadence::Free)), true, 6)), vec![("b0".to_owned(), "b2".to_owned())]);
    a.started("b2", 10);
    assert_eq!(admitted(&a.arrive("b3", push(3, None, Some(Cadence::Free)), true, 11)), vec!["b3"], "b2 runs: b3 waits beside it, nothing skipped");
}

/// W28-R62: `every` starts at most once per interval, the newest demand then.
#[test]
fn w28_r62_cadence_every() {
    let mut a = Admit::default();
    let every = Some(Cadence::Every(3_600));
    assert_eq!(admitted(&a.arrive("n1", push(1, None, every), false, 0)), vec!["n1"]);
    a.started("n1", 0);
    let ds = a.arrive("n2", push(2, None, every), false, 600);
    assert!(matches!(ds.first(), Some(Decision::Hold { held: Held::Cadence { next: 3_600, .. }, .. })));
    assert!(ds.contains(&Decision::Timer { at: 3_600 }));
    let ds = a.arrive("n3", push(3, None, every), false, 1_200);
    assert_eq!(skipped(&ds), vec![("n2".to_owned(), "n3".to_owned())]);
    assert!(a.review(3_000, &|_| true).is_empty());
    assert_eq!(admitted(&a.review(3_600, &|_| true)), vec!["n3"]);
}

/// W28-R68, W28-AC64: three speculative demands and a window of two: the third waits, a pass
/// widens the window and admits it, a failure halves it, never below its floor.
#[test]
fn w28_ac64_the_speculation_window_is_aimd() {
    let mut a = Admit::new(Window { initial: 2, floor: 1, cap: 4 });
    let spec = |order: u64| Demand { speculative: true, ..push(order, None, None) };
    assert_eq!(admitted(&a.arrive("s1", spec(1), true, 0)), vec!["s1"]);
    assert_eq!(admitted(&a.arrive("s2", spec(2), true, 0)), vec!["s2"]);
    let ds = a.arrive("s3", spec(3), true, 0);
    assert!(matches!(ds.first(), Some(Decision::Hold { held: Held::Speculation { size: 2, in_flight: 2, .. }, .. })));
    // A lane's newest, not speculative, is never held by it.
    assert_eq!(admitted(&a.arrive("head", push(4, None, None), true, 0)), vec!["head"]);
    a.started("s1", 1);
    assert_eq!(admitted(&a.ended("s1", Some(true), 2)), vec!["s3"], "a pass: window 3, and room");
    assert_eq!(a.window("acme/x\0ci\0main"), (3, 2));
    assert!(a.ended("s2", Some(false), 3).is_empty());
    assert_eq!(a.window("acme/x\0ci\0main"), (1, 1), "a failure halves it");
    let _ = a.ended("s3", Some(false), 4);
    assert_eq!(a.window("acme/x\0ci\0main"), (1, 0), "never below the floor");
}

/// W28-R61, R62: the dialect's `coalesce:` and `cadence:`, with the week's defaults, and what isn't
/// either refused with what it takes.
#[test]
fn w28_r61_r62_the_dialect() {
    use super::{MAX_WAIT_S, QUIET_S, Spec};
    use crate::wf::ir::{Tpl, YVal};
    let s = |t: &str| YVal::Str(Tpl::new(t.to_owned(), crate::wf::diag::Range::default()));
    let map = |kv: Vec<(&str, YVal)>| YVal::Map(kv.into_iter().map(|(k, v)| (k.to_owned(), v)).collect());
    assert_eq!(Spec::parse(None, None), Ok(Spec::default()));
    assert!(!Spec::default().any(), "off until declared");
    let on = Spec::parse(Some(&YVal::Bool(true)), Some(&s("free"))).unwrap_or_default();
    assert_eq!(on.coalesce, Some(Coalesce { quiet: QUIET_S, max_wait: MAX_WAIT_S, always: false }));
    assert_eq!(on.cadence, Some(Cadence::Free));
    let c = map(vec![("quiet", s("45s")), ("max-wait", s("5m")), ("when", s("always"))]);
    let every = map(vec![("every", s("30m"))]);
    let p = Spec::parse(Some(&c), Some(&every)).unwrap_or_default();
    assert_eq!(p.coalesce, Some(Coalesce { quiet: 45, max_wait: 300, always: true }));
    assert_eq!(p.cadence, Some(Cadence::Every(1_800)));
    assert_eq!(p.millis().coalesce.map(|c| (c.quiet, c.max_wait)), Some((45_000, 300_000)));
    assert!(Spec::parse(Some(&map(vec![("quite", s("45s"))])), None).is_err_and(|e| e.contains("quite")));
    assert!(Spec::parse(Some(&map(vec![("when", s("sometimes"))])), None).is_err());
    assert!(Spec::parse(None, Some(&map(vec![("on", s("push"))]))).is_err());
    assert!(Spec::parse(None, Some(&map(vec![("every", s("soon"))]))).is_err());
}
