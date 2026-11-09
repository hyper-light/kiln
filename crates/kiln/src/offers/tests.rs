//! docs/reach.md §2.2's offers view: what counts as a change (AUDIT O2), the waiting work kept as it
//! changes (O3, P1), and the view's bounds.

use std::collections::BTreeMap;

use kiln_ci::sched::state::Class;

use super::demand::{Demand, Item, Key, Source};
use super::hint::{Amounts, Hint, Quote, Quoted, Taking, Wait};
use super::relevant::{self, Said, Unit, shown};
use super::view::{self, Machine, Page};
use super::{Offers, Want};

const GIB: u64 = 1 << 30;
const MIN: u64 = 60_000;

/// 4 CPUs and 8 GiB.
const SMALL: Amounts = [4_000, 8 * GIB, 0, 0];
/// 16 CPUs and 32 GiB.
const BIG: Amounts = [16_000, 32 * GIB, 0, 0];

/// A runner of 32 CPUs and 128 GiB with `room` free, that quotes BIG waiting in line, starting
/// `start_in` ms from its `at`.
fn hint(seq: u64, at: i64, room: Amounts, start_in: u64) -> Hint {
    Hint {
        epoch: 1,
        seq,
        at,
        budget: [32_000, 128 * GIB, 500 * GIB, 0],
        room,
        queued: [0, 0, 1, 2, 0],
        state: Taking::Taking,
        quotes: vec![Quote { ask: BIG, answer: Quoted::Waits { reason: "line".into(), soonest_in: Some(start_in), latest_in: None } }],
        ..Hint::default()
    }
}

/// Every version told since the last look.
fn told(o: &Offers) -> u32 {
    let mut n = 0;
    while o.kick.1.try_recv().is_ok() {
        if o.flush().is_some() {
            n += 1;
        }
    }
    n
}

fn leg_waiting(o: &Offers, id: &str, ask: Amounts) {
    o.leg_wants(id, Want { need: "linux/amd64".into(), ask, class: Class::Batch });
    o.leg_waits(id, "capacity", 1_000);
}

/// docs/reach.md §2.2, AUDIT O2: a stream of samples that change nothing decided by (the room
/// moving within every answer, the start moving within the minute it's shown to) tells nothing:
/// no version, no event. The first hint is told; the 1000 after it aren't.
#[test]
fn o2_samples_that_change_nothing_decision_relevant_emit_no_event() {
    let o = Offers::default();
    leg_waiting(&o, "l1", SMALL);
    assert_eq!(told(&o), 1, "the leg that waits is told");
    let t0 = 1_000_000;
    assert!(o.heard("a", hint(1, t0, [10_000, 40 * GIB, 100 * GIB, 0], 5 * MIN + 20_000), t0));
    assert_eq!(told(&o), 1, "the runner's first word is told");
    for i in 0..1000u64 {
        // A tenth of a second on each time; the room jitters between 8 and 12 CPUs and 36 to 44
        // GiB (SMALL fits all along); the start, 5 minutes and 20 to 48 seconds after the first
        // word, said as so long from each sample's own moment.
        let since = (i + 1) * 100;
        let now = t0 + i64::try_from(since).unwrap_or(0);
        let jitter = i % 5;
        let room = [8_000 + jitter * 1_000, 36 * GIB + jitter * 2 * GIB, 100 * GIB - jitter, 0];
        let start_in = (5 * MIN + 20_000 + jitter * 7_000) - since;
        assert!(!o.heard("a", hint(i + 2, now, room, start_in), now), "sample {i} changed nothing shown");
    }
    assert_eq!(told(&o), 0, "no event for samples that change nothing decided by");
    assert_eq!(o.counts(), (1001, 1));
}

/// AUDIT O2's other half: each change decided by is told: an answer for a known size flips (the
/// room falls below the size that waits), the line's counts change, the start moves across the
/// minute it's shown to, the runner strains.
#[test]
fn o2_each_decision_relevant_change_is_told() {
    let o = Offers::default();
    leg_waiting(&o, "l1", SMALL);
    let t0 = 1_000_000;
    o.heard("a", hint(1, t0, [10_000, 40 * GIB, 0, 0], 5 * MIN), t0);
    let _ = told(&o);
    // SMALL doesn't fit any more: its answer flips from now to waits.
    assert!(o.heard("a", hint(2, t0 + 1, [2_000, 40 * GIB, 0, 0], 5 * MIN), t0 + 1));
    assert_eq!(told(&o), 1);
    // Back: it flips again.
    assert!(o.heard("a", hint(3, t0 + 2, [10_000, 40 * GIB, 0, 0], 5 * MIN), t0 + 2));
    assert_eq!(told(&o), 1);
    // One more waits in its interactive line.
    let mut h = hint(4, t0 + 3, [10_000, 40 * GIB, 0, 0], 5 * MIN);
    h.queued = [0, 0, 2, 2, 0];
    assert!(o.heard("a", h.clone(), t0 + 3));
    // BIG's start moves from 5 to 7 minutes on: shown as 7 min, not 5.
    h.seq = 5;
    h.quotes = vec![Quote { ask: BIG, answer: Quoted::Waits { reason: "line".into(), soonest_in: Some(7 * MIN), latest_in: None } }];
    assert!(o.heard("a", h.clone(), t0 + 3));
    // It strains.
    h.seq = 6;
    h.state = Taking::Strained { why: "memory pressure 41% (PSI some avg10)".into() };
    assert!(o.heard("a", h, t0 + 3));
    // Three changes in one turn: one version.
    assert_eq!(told(&o), 1, "changes before the task runs go in one event");
}

/// The coalescing (per turn of the runtime, not per change): many runners' changes, then one wake,
/// one version; nothing more till the next change.
#[test]
fn changes_in_one_turn_go_as_one_version() {
    let o = Offers::default();
    for (i, name) in ["a", "b", "c", "d"].iter().enumerate() {
        let at = 1_000 + i64::try_from(i).unwrap_or(0);
        o.heard(name, hint(1, at, SMALL, MIN), at);
        leg_waiting(&o, &format!("l{i}"), BIG);
    }
    assert_eq!(o.kick.1.len(), 1, "one wake for all of them");
    assert_eq!(told(&o), 1);
    assert_eq!(o.version(), 1);
    assert_eq!(o.flush(), None);
}

/// A hint older than the last heard (its epoch, then its sequence) is dropped; a restarted
/// runner's (a higher epoch) is taken, its sequence from 1 again.
#[test]
fn an_older_hint_is_dropped() {
    let o = Offers::default();
    o.heard("a", hint(5, 1_000, SMALL, MIN), 1_000);
    let mut stale = hint(4, 2_000, [0; 4], MIN);
    stale.state = Taking::Draining;
    assert!(!o.heard("a", stale.clone(), 2_000));
    assert_eq!(o.counts().0, 1);
    stale.epoch = 2;
    stale.seq = 1;
    assert!(o.heard("a", stale, 2_000));
}

/// Display granularity (docs/reach.md §2.2): a start is told in the largest clock unit it holds
/// once whole, so many whole units after it was said; only crossing a unit moves it.
#[test]
fn a_start_is_shown_in_the_unit_of_its_distance() {
    assert_eq!(Unit::of(59_999), Unit::S);
    assert_eq!(Unit::of(60_000), Unit::Min);
    assert_eq!(Unit::of(3_599_999), Unit::Min);
    assert_eq!(Unit::of(3_600_000), Unit::H);
    assert_eq!(Unit::of(86_400_000), Unit::D);
    let s = shown(5, 3 * MIN + 59_000);
    assert_eq!((s.unit, s.count, s.from), (Unit::Min, 3, 5));
    // Two hints a minute apart that say the same moment show the same, counted from what's shown.
    let a = Said { hint: hint(1, 0, SMALL, 5 * MIN), heard: 0 };
    let b = Said { hint: hint(2, 60_000, SMALL, 4 * MIN), heard: 60_000 };
    assert!(!relevant::relevant(&a, &b, std::iter::empty()));
    // Ten seconds later than that: still 5 whole minutes after the first.
    let c = Said { hint: hint(3, 60_000, SMALL, 4 * MIN + 10_000), heard: 60_000 };
    assert!(!relevant::relevant(&a, &c, std::iter::empty()));
    // A minute later: 6.
    let d = Said { hint: hint(4, 60_000, SMALL, 5 * MIN), heard: 60_000 };
    assert!(relevant::relevant(&a, &d, std::iter::empty()));
}

/// A size the runner doesn't quote is answered from its room and budget: never past its budget,
/// now within its room while it takes work, else it waits.
#[test]
fn an_unquoted_size_follows_from_room_and_budget() {
    let s = Said { hint: hint(1, 0, SMALL, MIN), heard: 0 };
    let kind = |ask: Amounts, state: Taking| {
        let mut s = s.clone();
        s.hint.state = state;
        s.answer(&ask, 0).kind
    };
    assert_eq!(kind(SMALL, Taking::Taking), relevant::Kind::Now);
    assert_eq!(kind([4_000, 8 * GIB, 0, 1], Taking::Taking), relevant::Kind::Never { why: "past its budget of GPUs".into() });
    assert_eq!(kind([6_000, 8 * GIB, 0, 0], Taking::Taking), relevant::Kind::Waits { reason: "room".into() });
    assert_eq!(kind(SMALL, Taking::Draining), relevant::Kind::Waits { reason: "draining".into() });
    assert!(s.answer(&SMALL, 0).derived && !s.answer(&BIG, 0).derived);
}

/// A size only another runner quotes is held to every runner's answer too: once runner b quotes
/// it, runner a's room falling below it is told.
#[test]
fn a_size_another_runner_quotes_is_known_to_all() {
    let o = Offers::default();
    o.heard("a", Hint { quotes: Vec::new(), ..hint(1, 0, [20_000, 64 * GIB, 0, 0], MIN) }, 0);
    o.heard("b", hint(1, 0, SMALL, MIN), 0);
    let _ = told(&o);
    // a's room falls below BIG (which b quotes): a's answer for BIG flips from now to waits.
    assert!(o.heard("a", Hint { quotes: Vec::new(), ..hint(2, 1, [12_000, 64 * GIB, 0, 0], MIN) }, 1));
}

/// A runner's own runs that wait come with its hints as deltas, their start in the server's clock
/// (as far before the hint was heard as before it was said); said whole after a reconnect, they
/// replace what it said; gone with it.
#[test]
fn an_owners_runs_wait_by_its_word() -> anyhow::Result<()> {
    let o = Offers::default();
    let w = |run: &str, since: i64| Wait { run: run.into(), need: "macos".into(), ask: SMALL, class: Class::Interactive, since, reason: "room".into() };
    // Its clock is an hour behind the server's.
    let h = Hint { waits: vec![w("r1", 1_000), w("r2", 2_000)], whole: true, ..hint(1, 10_000, SMALL, MIN) };
    o.heard("mac", h, 3_610_000);
    let v = o.view(&[Machine { name: "mac".into(), connected: true, platform: "macos/arm64".into(), ..Machine::default() }], &Page::default(), 3_610_000);
    let g = v.waiting.groups.first().ok_or_else(|| anyhow::anyhow!("no group"))?;
    assert_eq!((g.runs, g.since, g.need.as_str()), (2, Some(3_601_000), "macos"));
    // One started; another waits.
    o.heard("mac", Hint { waits: vec![w("r3", 11_000)], gone: vec!["r1".into()], ..hint(2, 11_000, SMALL, MIN) }, 3_611_000);
    assert_eq!(o.view(&[], &Page::default(), 0).waiting.runs, 2);
    // Back after a reconnect: whole, only r3.
    o.heard("mac", Hint { waits: vec![w("r3", 11_000)], whole: true, ..hint(3, 12_000, SMALL, MIN) }, 3_612_000);
    assert_eq!(o.view(&[], &Page::default(), 0).waiting.runs, 1);
    assert!(o.gone("mac"));
    assert_eq!(o.view(&[], &Page::default(), 0).waiting.runs, 0);
    Ok(())
}

/// The CI engine's legs: given, waiting (counted by the reason the engine gives), granted, settled.
#[test]
fn a_legs_wait_follows_the_engines_commands() {
    let o = Offers::default();
    o.leg_wants("l1", Want { need: "linux/amd64".into(), ask: BIG, class: Class::Land });
    assert_eq!(o.view(&[], &Page::default(), 0).waiting.runs, 0, "given, it doesn't wait yet");
    assert!(o.leg_waits("l1", "capacity", 5));
    assert!(!o.leg_waits("l1", "capacity", 9), "the same wait again changes nothing");
    assert!(o.leg_waits("l1", "exclusive", 9), "another reason does");
    let v = o.view(&[], &Page::default(), 0);
    let g = v.waiting.groups.first().map(|g| (g.runs, g.since, g.reasons.clone(), g.by_class.clone()));
    assert_eq!(g, Some((1, Some(5), vec![view::OfferCount { key: "exclusive".into(), count: 1 }], vec![view::OfferCount { key: "land".into(), count: 1 }])));
    assert!(o.leg_placed("l1"));
    assert!(!o.leg_placed("l1"));
    assert!(o.leg_waits("l1", "retry", 20));
    assert!(o.leg_done("l1"));
    assert_eq!(o.view(&[], &Page::default(), 0).waiting.runs, 0);
}

fn item(need: &str, ask: Amounts, reason: &str, since: i64) -> Item {
    Item { need: need.into(), ask, class: Class::Batch, since, reason: reason.into() }
}

/// AUDIT O3, P1: the waiting work is kept as it changes, not rebuilt: a change costs the same few
/// steps with ten waiting as with fifty thousand (each change one group's counts, or two for a
/// move between groups), and the books always hold.
#[test]
fn o3_p1_waiting_is_kept_in_o_changes() -> anyhow::Result<()> {
    let mut cost = Vec::new();
    for n in [10u32, 50_000] {
        let mut d = Demand::default();
        for i in 0..n {
            d.put(Source::Leg, &format!("l{i}"), item("linux/amd64", [u64::from(i % 7 + 1) * 1000, GIB, 0, 0], "capacity", i64::from(i)));
        }
        let before = d.touched;
        // One starts to wait, one waits for another reason, one stops, one waits as it did.
        d.put(Source::Leg, "new", item("macos", SMALL, "capacity", 0));
        d.put(Source::Leg, "l3", item("linux/amd64", [4_000, GIB, 0, 0], "exclusive", 3));
        d.remove(&Source::Leg, "l5");
        d.put(Source::Leg, "l6", item("linux/amd64", [7_000, GIB, 0, 0], "capacity", 6));
        cost.push(d.touched - before);
        d.check().map_err(|e| anyhow::anyhow!(e))?;
        assert_eq!(d.total(), n);
    }
    assert_eq!(cost, [4, 4], "work per change is independent of how many wait");
    Ok(())
}

/// The books hold through a long run of changes from many owners and the engine, said whole and
/// in deltas, owners going away: every group counts exactly its items.
#[test]
fn demand_books_hold_through_any_changes() -> anyhow::Result<()> {
    let mut d = Demand::default();
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = |m: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % m
    };
    let reasons = ["room", "line", "capacity", "exclusive"];
    for step in 0..20_000i64 {
        let owner = format!("o{}", next(5));
        let id = format!("r{}", next(40));
        let it = item(["linux/amd64", "macos", ""].get(usize::try_from(next(3)).unwrap_or(0)).copied().unwrap_or(""), [next(4) * 1000, GIB, 0, 0], reasons.get(usize::try_from(next(4)).unwrap_or(0)).copied().unwrap_or("room"), step);
        match next(6) {
            0 => {
                d.put(Source::Leg, &id, it);
            }
            1 => {
                d.remove(&Source::Leg, &id);
            }
            2 => {
                d.owner_says(&owner, false, [(id, it)], &[]);
            }
            3 => {
                d.owner_says(&owner, false, Vec::<(String, Item)>::new(), &[id]);
            }
            4 => {
                d.owner_says(&owner, true, [(id, it)], &[]);
            }
            _ => {
                d.forget_owner(&owner);
            }
        }
        if step % 997 == 0 {
            d.check().map_err(|e| anyhow::anyhow!("step {step}: {e}"))?;
        }
    }
    d.check().map_err(|e| anyhow::anyhow!(e))
}

/// The view is bounded by what it shows: as many groups (and sizes) a page as runners, the next
/// page from the last group's cursor, a smaller page when asked, never a bigger one.
#[test]
fn the_view_pages_by_the_fleets_size() -> anyhow::Result<()> {
    let o = Offers::default();
    for i in 0..10u64 {
        o.leg_wants(&format!("l{i}"), Want { need: "linux/amd64".into(), ask: [(i + 1) * 1000, GIB, 0, 0], class: Class::Batch });
        o.leg_waits(&format!("l{i}"), "capacity", 0);
    }
    let fleet: Vec<Machine> = ["a", "b", "c"].iter().map(|n| Machine { name: (*n).into(), connected: true, platform: "linux/amd64".into(), ..Machine::default() }).collect();
    for m in &fleet {
        o.heard(&m.name, hint(1, 0, SMALL, MIN), 0);
    }
    let first = o.view(&fleet, &Page::default(), 0);
    assert_eq!((first.waiting.groups.len(), first.waiting.groups_total, first.waiting.runs, first.waiting.limit), (3, 10, 10, 3));
    assert!(first.sizes.len() <= 3 && first.runners.iter().all(|r| r.answers.len() == first.sizes.len()));
    let mut seen: Vec<String> = first.waiting.groups.iter().map(|g| g.cursor.clone()).collect();
    let mut after = first.waiting.next.clone();
    while let Some(a) = after {
        let p = o.view(&fleet, &Page { after: Some(a), limit: None }, 0);
        seen.extend(p.waiting.groups.iter().map(|g| g.cursor.clone()));
        after = p.waiting.next;
    }
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 10, "every group once, across the pages");
    assert_eq!(o.view(&fleet, &Page { after: None, limit: Some(2) }, 0).waiting.groups.len(), 2);
    assert_eq!(o.view(&fleet, &Page { after: None, limit: Some(500) }, 0).waiting.groups.len(), 3);
    let k = view::parse_cursor(&view::cursor(&Key { need: "linux/amd64 +gpu".into(), ask: [1, 2, 3, 4] })).ok_or_else(|| anyhow::anyhow!("cursor"))?;
    assert_eq!(k, Key { need: "linux/amd64 +gpu".into(), ask: [1, 2, 3, 4] });
    Ok(())
}

/// A runner's health: offline, draining, strained (and why), taking; one before offers says nothing
/// of its admission and is shown so.
#[test]
fn a_runners_health_says_why_it_isnt_taking_work() {
    let o = Offers::default();
    let mut h = hint(1, 0, SMALL, MIN);
    h.state = Taking::Strained { why: "memory pressure".into() };
    o.heard("a", h, 0);
    o.heard("b", hint(1, 0, SMALL, MIN), 0);
    let m = |n: &str, connected: bool| Machine { name: n.into(), connected, platform: "linux/amd64".into(), ..Machine::default() };
    let v = o.view(&[m("a", true), m("b", true), m("c", true), m("d", false)], &Page::default(), 0);
    let health: BTreeMap<&str, (bool, Option<&str>, bool)> = v.runners.iter().map(|r| (r.name.as_str(), (r.health.taking, r.health.why.as_deref(), r.said))).collect();
    assert_eq!(health.get("a"), Some(&(false, Some("memory pressure"), true)));
    assert_eq!(health.get("b"), Some(&(true, None, true)));
    assert_eq!(health.get("c"), Some(&(true, Some("says nothing of its admission (a runner before offers)"), false)));
    assert_eq!(health.get("d"), Some(&(false, Some("offline"), false)));
}

/// A hint as it travels: JSON, its fields optional, one from a later runner with fields this one
/// doesn't know read all the same.
#[test]
fn a_hint_reads_from_json_with_fields_unknown_or_missing() -> anyhow::Result<()> {
    let h = hint(3, 7, SMALL, MIN);
    assert_eq!(serde_json::from_str::<Hint>(&serde_json::to_string(&h)?)?, h);
    let later = r#"{"epoch":2,"seq":1,"at":5,"state":{"state":"draining"},"quotes":[{"ask":[1000,0,0,0],"answer":{"answer":"never","why":"no GPU"}}],"something_new":true}"#;
    let h: Hint = serde_json::from_str(later)?;
    assert_eq!((h.epoch, h.state, h.quotes.len()), (2, Taking::Draining, 1));
    Ok(())
}
