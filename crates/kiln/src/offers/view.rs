//! The offers view as `GET /api/reach/offers` answers it (docs/reach.md §2.2): every runner, with
//! what it can take and its answer for the ask sizes in question, and a page of the waiting work's
//! groups. Bounded by what it shows: one row per runner (the fleet), and as many groups and sizes a
//! page as there are runners, so a page of what waits is never longer than the fleet it waits on;
//! the rest by `after`, the cursor of the last group shown.

use std::collections::BTreeMap;
use std::ops::Bound;

use kiln_ci::sched::state::Class;
use serde::Serialize;

use super::demand::{Demand, Group, Key};
use super::hint::{Amounts, Taking};
use super::relevant::{Kind, Said, Shown};

/// A runner as the hub knows it.
#[derive(Debug, Clone, Default)]
pub struct Machine {
    pub name: String,
    pub label: Option<String>,
    /// `os/arch`.
    pub platform: String,
    pub connected: bool,
    pub draining: bool,
}

/// Which page of the waiting groups.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Page {
    /// After this group's cursor ([`OfferGroup::cursor`]).
    pub after: Option<String>,
    /// At most this many (no more than the view's own bound).
    pub limit: Option<usize>,
}

/// An amount in each dimension.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct OfferAmounts {
    /// Thousandths of a CPU.
    pub cpu_milli: u64,
    pub mem: u64,
    pub disk: u64,
    pub gpu: u64,
}

impl OfferAmounts {
    fn of(a: &Amounts) -> OfferAmounts {
        let [cpu_milli, mem, disk, gpu] = *a;
        OfferAmounts { cpu_milli, mem, disk, gpu }
    }
}

/// A count of something named.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct OfferCount {
    pub key: String,
    pub count: u32,
}

/// Whether a runner takes work, and why not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct OfferHealth {
    pub taking: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub why: Option<String>,
}

/// A runner's answer for one ask size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct OfferAnswer {
    pub ask: OfferAmounts,
    /// `now`, `waits` or `never`.
    pub answer: String,
    /// Why it waits, or why never.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub soonest: Option<Shown>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub latest: Option<Shown>,
    /// The runner didn't quote this size: it follows from its room and budget.
    pub derived: bool,
}

/// A runner, and what it can take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct OfferRunner {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub label: Option<String>,
    pub platform: String,
    pub connected: bool,
    /// It said what its admission holds (a runner before offers says nothing).
    pub said: bool,
    /// When what's shown was said (the server's clock, unix ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub heard: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub budget: Option<OfferAmounts>,
    /// What was free for an ordinary run as it was said.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub room: Option<OfferAmounts>,
    /// How many wait in its line, by class (those with any).
    pub queued: Vec<OfferCount>,
    pub health: OfferHealth,
    /// Its answer for each of the view's sizes, in their order.
    pub answers: Vec<OfferAnswer>,
}

/// What waits of one need and size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct OfferGroup {
    /// What the runner must be (`linux/amd64 +gpu`); empty: anything.
    pub need: String,
    pub ask: OfferAmounts,
    pub runs: u32,
    pub by_class: Vec<OfferCount>,
    pub reasons: Vec<OfferCount>,
    /// When the one waiting longest began to (the server's clock, unix ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub since: Option<i64>,
    /// The page after this group starts here (`?after=`).
    pub cursor: String,
}

/// The waiting work: in all, and a page of its groups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct OffersWaiting {
    /// Runs and legs that wait, in all.
    pub runs: u32,
    /// Groups in all.
    pub groups_total: u32,
    pub groups: Vec<OfferGroup>,
    /// The next page's cursor, when there's more.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub next: Option<String>,
    /// How many groups a page holds.
    pub limit: u32,
}

/// The offers view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct OffersView {
    /// Its version: one up whenever something shown changed. Its ETag, and what
    /// `ServerEvent::Offers` says.
    pub version: u64,
    /// The server's clock as it was made (unix ms).
    pub now: i64,
    pub runners: Vec<OfferRunner>,
    /// The ask sizes answered for each runner: those of this page's groups, then those runners
    /// quote, most waiting first.
    pub sizes: Vec<OfferAmounts>,
    pub waiting: OffersWaiting,
}

/// A group's cursor: its size's amounts, then its need.
pub fn cursor(k: &Key) -> String {
    let [c, m, d, g] = k.ask;
    format!("{c}.{m}.{d}.{g}.{}", k.need)
}

/// The group a cursor names.
pub fn parse_cursor(s: &str) -> Option<Key> {
    let mut it = s.splitn(5, '.');
    let mut ask = [0u64; 4];
    for a in &mut ask {
        *a = it.next()?.parse().ok()?;
    }
    Some(Key { need: it.next()?.to_owned(), ask })
}

fn counts(by: &[u32; 5]) -> Vec<OfferCount> {
    Class::ALL.iter().zip(by).filter(|(_, n)| **n > 0).map(|(c, n)| OfferCount { key: c.as_str().to_owned(), count: *n }).collect()
}

fn group(k: &Key, g: &Group) -> OfferGroup {
    OfferGroup {
        need: k.need.clone(),
        ask: OfferAmounts::of(&k.ask),
        runs: g.runs,
        by_class: counts(&g.by_class),
        reasons: g.reasons.iter().map(|(r, n)| OfferCount { key: r.clone(), count: *n }).collect(),
        since: g.oldest(),
        cursor: cursor(k),
    }
}

/// The view of `machines` (the hub's runners), what each last showed (`shown`), and the waiting
/// work's page `page`.
pub fn build(machines: &[Machine], shown: &BTreeMap<&str, &Said>, demand: &Demand, version: u64, page: &Page, now: i64) -> OffersView {
    // A page is as long as the fleet (one at least), or what's asked if that's less.
    let bound = machines.len().max(1);
    let limit = page.limit.filter(|l| *l > 0).map_or(bound, |l| l.min(bound));
    let after = page.after.as_deref().and_then(parse_cursor);
    let lower = after.as_ref().map_or(Bound::Unbounded, Bound::Excluded);
    let mut range = demand.groups().range((lower, Bound::Unbounded));
    let groups: Vec<OfferGroup> = range.by_ref().take(limit).map(|(k, g)| group(k, g)).collect();
    let next = if range.next().is_some() { groups.last().map(|g| g.cursor.clone()) } else { None };

    // The sizes in question: this page's groups' (by how many wait), then those runners quote (by
    // how many quote them), as many as a page holds.
    let mut weight: BTreeMap<Amounts, (u64, u64)> = BTreeMap::new();
    for g in &groups {
        let e = weight.entry([g.ask.cpu_milli, g.ask.mem, g.ask.disk, g.ask.gpu]).or_default();
        e.0 += u64::from(g.runs);
    }
    for s in shown.values() {
        for q in &s.hint.quotes {
            weight.entry(q.ask).or_default().1 += 1;
        }
    }
    let mut sizes: Vec<(Amounts, (u64, u64))> = weight.into_iter().collect();
    sizes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    sizes.truncate(limit);
    let sizes: Vec<Amounts> = sizes.into_iter().map(|(a, _)| a).collect();

    let runners = machines.iter().map(|m| runner(m, shown.get(m.name.as_str()).copied(), &sizes)).collect();
    OffersView {
        version,
        now,
        runners,
        sizes: sizes.iter().map(OfferAmounts::of).collect(),
        waiting: OffersWaiting { runs: demand.total(), groups_total: u32::try_from(demand.groups().len()).unwrap_or(u32::MAX), groups, next, limit: u32::try_from(limit).unwrap_or(u32::MAX) },
    }
}

fn runner(m: &Machine, said: Option<&Said>, sizes: &[Amounts]) -> OfferRunner {
    let health = match (m.connected, said.map(|s| &s.hint.state)) {
        (false, _) => OfferHealth { taking: false, why: Some("offline".into()) },
        (true, Some(Taking::Draining)) => OfferHealth { taking: false, why: Some("draining: an upgrade is pending".into()) },
        (true, Some(Taking::Strained { why })) => OfferHealth { taking: false, why: Some(why.clone()) },
        (true, _) if m.draining => OfferHealth { taking: false, why: Some("draining".into()) },
        (true, Some(Taking::Taking)) => OfferHealth { taking: true, why: None },
        (true, None) => OfferHealth { taking: true, why: Some("says nothing of its admission (a runner before offers)".into()) },
    };
    let Some(s) = said.filter(|_| m.connected) else {
        return OfferRunner { name: m.name.clone(), label: m.label.clone(), platform: m.platform.clone(), connected: m.connected, said: false, heard: None, budget: None, room: None, queued: Vec::new(), health, answers: Vec::new() };
    };
    let answers = sizes
        .iter()
        .map(|ask| {
            let a = s.answer(ask, s.heard);
            let (answer, reason) = match a.kind {
                Kind::Now => ("now", None),
                Kind::Waits { reason } => ("waits", Some(reason)),
                Kind::Never { why } => ("never", Some(why)),
            };
            OfferAnswer { ask: OfferAmounts::of(ask), answer: answer.into(), reason, soonest: a.soonest, latest: a.latest, derived: a.derived }
        })
        .collect();
    OfferRunner {
        name: m.name.clone(),
        label: m.label.clone(),
        platform: m.platform.clone(),
        connected: true,
        said: true,
        heard: Some(s.heard),
        budget: Some(OfferAmounts::of(&s.hint.budget)),
        room: Some(OfferAmounts::of(&s.hint.room)),
        queued: counts(&s.hint.queued),
        health,
        answers,
    }
}
