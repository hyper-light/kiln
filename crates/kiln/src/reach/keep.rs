//! What a runner keeps of the trees other runners synced to it before running there
//! (`~/.kiln/reach/<runner>/<tree>`, docs/reach.md §5.1), and what it lets go.
//!
//! People set [`Rules`] for every runner, for one runner, for a repository's copies and for an
//! agent's; the most particular scope that sets a rule wins, rule by rule. Two rules keep a copy
//! (the newest few, those used lately) and three things let go of what they don't keep: a schedule,
//! too little room on the copies' disk, and a person asking. Nothing goes until someone sets one of
//! those, nor ever a copy in use or used in the last ten minutes, and each copy let go says why.
//!
//! Everything here is pure: the time, the disk and the copies are given, so the runner's decision
//! and the server's preview of it are the one function ([`plan`]).

use std::collections::{BTreeMap, HashMap, HashSet};

use kiln_ci::trigger::cron::{self, Cron, TimeZone};
use serde::{Deserialize, Serialize};

/// A copy used this recently stays, whatever the rules say: a sync or a run of another runner
/// process of its machine may be at it.
pub const GRACE_S: i64 = 600;
/// How far back a look that comes late (its runner was away) catches up on a schedule.
const CATCH_UP_S: i64 = 86_400;
/// How far ahead the next time a schedule lets copies go is looked for.
const AHEAD_S: i64 = 8 * 86_400;

/// One scope's rules. Each one unset is the scope above's (an agent's, then its repository's, then
/// its runner's, then every runner's); `0`, `""` or `false` says there is none here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    /// Keep the newest this many copies (by when each was last used), among the copies of the
    /// scope that sets it: a runner's, a repository's or an agent's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest: Option<u32>,
    /// Keep the copies used within this many hours.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hours: Option<u32>,
    /// With both rules, a copy stays only when both keep it (else when either does).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub both: Option<bool>,
    /// When what the rules don't keep goes: a five-field cron schedule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    /// The time zone the schedule is read in (an IANA name); UTC without one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// What the rules don't keep goes, least recently used first, while the copies' disk has less
    /// than this share of it free (percent)...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_percent: Option<u8>,
    /// ... or less than this many GiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_gib: Option<u32>,
    /// With too little room still once that went, the copies the rules keep go too, least recently
    /// used first, until there's room.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub beyond: Option<bool>,
}

/// The rules' fields, as the API takes them.
pub const FIELDS: &[&str] = &["newest", "hours", "both", "schedule", "timezone", "free_percent", "free_gib", "beyond"];

impl Rules {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Whether kiln takes them: numbers within reason, a schedule cron reads, a time zone it knows.
    pub fn check(&self) -> Result<(), String> {
        if self.newest.is_some_and(|n| n > 100_000) {
            return Err("newest: at most 100000 copies".into());
        }
        if self.hours.is_some_and(|h| h > 24 * 3650) {
            return Err("hours: at most ten years".into());
        }
        if let Some(s) = self.schedule.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Cron::parse(s).map_err(|e| format!("schedule: {e}"))?;
        }
        if let Some(z) = self.timezone.as_deref().map(str::trim).filter(|z| !z.is_empty()) {
            cron::zone(Some(z)).map_err(|e| format!("timezone: {e}"))?;
        }
        if self.free_percent.is_some_and(|p| p > 95) {
            return Err("free_percent: at most 95".into());
        }
        if self.free_gib.is_some_and(|g| g > 1 << 20) {
            return Err("free_gib: at most 1048576 (a PiB)".into());
        }
        Ok(())
    }
}

/// Whose rule a settled one is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Scope {
    System,
    Runner,
    Repo(String),
    Agent(String),
}

impl Scope {
    fn whose(&self) -> String {
        match self {
            Self::System => "every runner's rule".into(),
            Self::Runner => "this runner's rule".into(),
            Self::Repo(r) => format!("{r}'s rule"),
            Self::Agent(_) => "its agent's rule".into(),
        }
    }

    /// The copies its `newest` counts among: a runner's own (every runner's rule counts a runner's
    /// copies too), a repository's or an agent's.
    fn group(&self) -> Scope {
        match self {
            Self::System | Self::Runner => Self::Runner,
            other => other.clone(),
        }
    }

    fn among(&self) -> String {
        match self {
            Self::System | Self::Runner => "on this runner".into(),
            Self::Repo(r) => format!("of {r}"),
            Self::Agent(_) => "of its agent's".into(),
        }
    }
}

/// The rules a runner weighs: every runner's, its own, each repository's and each agent's that
/// set any.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Set {
    pub system: Rules,
    pub runner: Rules,
    pub repos: BTreeMap<String, Rules>,
    pub agents: BTreeMap<String, Rules>,
}

/// One copy, as its runner knows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TreeCopy {
    /// The runner whose tree it is.
    pub from: String,
    /// Its folder there (`super::sync::mirror_key`).
    pub key: String,
    /// The repository it is a copy of (`crate::sync::places::key_of` its `origin`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The agents whose commands ran in it, the latest last.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    /// When it was made and last used (a sync into it, a run in it), unix seconds.
    pub created: i64,
    pub used: i64,
    /// What it takes on the disk, once measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// A sync or a run is at it now.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub busy: bool,
}

impl TreeCopy {
    /// The agent that used it last.
    pub fn agent(&self) -> Option<&str> {
        self.agents.last().map(String::as_str)
    }
}

/// The disk the copies are on: its size and the bytes still free on it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disk {
    pub total: u64,
    pub free: u64,
}

/// What made a copy go.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trigger {
    /// A schedule came round.
    #[default]
    Schedule,
    /// Too little room on the copies' disk.
    Room,
    /// A person asked for the rules to be applied now.
    Asked,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Schedule => "schedule",
            Self::Room => "room",
            Self::Asked => "asked",
        }
    }
}

/// A copy to let go now, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub from: String,
    pub key: String,
    pub trigger: Trigger,
    pub why: String,
    /// Only while the disk has less than this many bytes free (too little room); none: whatever
    /// the room.
    pub room: Option<u64>,
}

/// A copy a runner let go.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Gone {
    pub from: String,
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    pub at: i64,
    pub trigger: Trigger,
    pub why: String,
    /// It couldn't all be deleted: what's left of it waits beside the copies to be.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What a runner tells the server of its copies (`{"op": "keep", "report": ...}`, a reach question).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Report {
    /// Its clock, unix seconds.
    pub at: i64,
    /// Where its copies are.
    pub base: String,
    pub disk: Option<Disk>,
    pub copies: Vec<TreeCopy>,
    /// What it let go, newest first.
    pub gone: Vec<Gone>,
    /// The last ask to let go now it carried out ([`Answer::asked`]).
    pub done: u64,
    /// What stopped it last, if anything has since.
    pub error: Option<String>,
}

/// The server's answer: the rules to weigh, and the last ask to let go now (0: none).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Answer {
    pub set: Set,
    pub asked: u64,
}

/// What a runner weighs its copies with.
#[derive(Debug, Clone, Copy)]
pub struct Look<'a> {
    pub now: i64,
    /// Its last look: a schedule that came round after it, up to `now`, is due.
    pub since: i64,
    pub disk: Option<Disk>,
    /// A person asked for the rules to be applied now.
    pub asked: bool,
    pub copies: &'a [TreeCopy],
}

/// What the rules say of a copy now, whatever comes round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub from: String,
    pub key: String,
    /// It stays: the rules keep it, or it's in use or was just used.
    pub kept: bool,
    pub why: String,
}

/// A copy's rules, each from the most particular scope that sets it.
#[derive(Debug, Clone)]
struct Settled {
    newest: (u32, Scope),
    hours: (u32, Scope),
    both: bool,
    /// The schedule, the time zone it's read in, as written (with the zone's name), and whose.
    schedule: Option<(Cron, TimeZone, String, Scope)>,
    /// Too little room: the free bytes under which copies go, as said, with whose.
    room: Option<(u64, String)>,
    beyond: bool,
}

impl Set {
    /// The rules for `copy`, field by field from the most particular scope that sets each.
    fn settle(&self, copy: &TreeCopy, disk: Option<Disk>) -> Settled {
        let mut layers: Vec<(&Rules, Scope)> = Vec::with_capacity(4);
        if let Some((a, r)) = copy.agent().and_then(|a| self.agents.get(a).map(|r| (a, r))) {
            layers.push((r, Scope::Agent(a.to_owned())));
        }
        if let Some((k, r)) = copy.repo.as_deref().and_then(|k| self.repos.get(k).map(|r| (k, r))) {
            layers.push((r, Scope::Repo(k.to_owned())));
        }
        layers.push((&self.runner, Scope::Runner));
        layers.push((&self.system, Scope::System));
        fn pick<T>(layers: &[(&Rules, Scope)], f: impl Fn(&Rules) -> Option<T>, none: T) -> (T, Scope) {
            layers.iter().find_map(|(r, s)| f(r).map(|v| (v, s.clone()))).unwrap_or((none, Scope::System))
        }
        let (schedule, from) = pick(&layers, |r| r.schedule.clone(), String::new());
        let (zone, _) = pick(&layers, |r| r.timezone.clone(), String::new());
        let schedule = Some(schedule.trim().to_owned()).filter(|s| !s.is_empty()).and_then(|s| {
            let zone = zone.trim();
            let tz = cron::zone(Some(zone).filter(|z| !z.is_empty())).ok()?;
            let said = format!("{s} {}", if zone.is_empty() { "UTC" } else { zone });
            Some((Cron::parse(&s).ok()?, tz, said, from))
        });
        // Under either share, there's too little room: the stricter of the two is what's wanted.
        let (percent, percent_from) = pick(&layers, |r| r.free_percent, 0);
        let (gib, gib_from) = pick(&layers, |r| r.free_gib, 0);
        let mut wanted: Option<u64> = None;
        let mut said = Vec::new();
        if let Some(d) = disk.filter(|_| percent > 0) {
            wanted = Some(d.total / 100 * u64::from(percent));
            said.push(format!("{percent}% ({})", percent_from.whose()));
        }
        if gib > 0 {
            let bytes = u64::from(gib) << 30;
            wanted = Some(wanted.map_or(bytes, |w| w.max(bytes)));
            said.push(format!("{gib} GiB ({})", gib_from.whose()));
        }
        Settled {
            newest: pick(&layers, |r| r.newest, 0),
            hours: pick(&layers, |r| r.hours, 0),
            both: pick(&layers, |r| r.both, false).0,
            schedule,
            room: wanted.map(|w| (w, said.join(" or "))),
            beyond: pick(&layers, |r| r.beyond, false).0,
        }
    }
}

/// A copy weighed: its rules, whether they keep it and why, and whether it's held whatever they say.
struct Weighed<'a> {
    copy: &'a TreeCopy,
    s: Settled,
    kept: bool,
    why: String,
    held: Option<String>,
}

fn weigh<'a>(set: &Set, look: &Look<'a>) -> Vec<Weighed<'a>> {
    let settled: Vec<Settled> = look.copies.iter().map(|c| set.settle(c, look.disk)).collect();
    // Each copy's place among those its `newest` counts, the most recently used first.
    let mut groups: HashMap<Scope, Vec<(usize, &TreeCopy)>> = HashMap::new();
    for (i, (c, s)) in look.copies.iter().zip(&settled).enumerate() {
        if s.newest.0 > 0 {
            groups.entry(s.newest.1.group()).or_default().push((i, c));
        }
    }
    let mut place: HashMap<usize, usize> = HashMap::new();
    for members in groups.values_mut() {
        members.sort_by(|(_, a), (_, b)| b.used.cmp(&a.used).then_with(|| a.from.cmp(&b.from)).then_with(|| a.key.cmp(&b.key)));
        place.extend(members.iter().enumerate().map(|(n, (i, _))| (*i, n)));
    }
    look.copies
        .iter()
        .zip(settled)
        .enumerate()
        .map(|(i, (c, s))| {
            let age = look.now.saturating_sub(c.used).max(0);
            let (n, h) = (&s.newest, &s.hours);
            let by_count = (n.0 > 0).then(|| {
                let one = place.get(&i).is_some_and(|p| *p < usize::try_from(n.0).unwrap_or(usize::MAX));
                let words = format!("one of the newest {} {} ({})", n.0, n.1.among(), n.1.whose());
                (one, if one { words } else { format!("not {words}") })
            });
            let by_age = (h.0 > 0).then(|| {
                let within = age < i64::from(h.0).saturating_mul(3600);
                let words = if within { format!("used {} ago, within {} ({})", span(age), hours(h.0), h.1.whose()) } else { format!("not used for {} (kept {}, {})", span(age), hours(h.0), h.1.whose()) };
                (within, words)
            });
            let (kept, why) = match (by_count, by_age) {
                (None, None) => (false, "no rule keeps it".to_owned()),
                (Some((k, w)), None) | (None, Some((k, w))) => (k, w),
                (Some((a, wa)), Some((b, wb))) if s.both => match (a, b) {
                    (true, true) => (true, format!("{wa}, and {wb}")),
                    (true, false) => (false, format!("{wb}; both rules must keep it")),
                    (false, true) => (false, format!("{wa}; both rules must keep it")),
                    (false, false) => (false, format!("{wa}, and {wb}")),
                },
                (Some((a, wa)), Some((b, wb))) => match (a, b) {
                    (true, true) => (true, format!("{wa}, and {wb}")),
                    (true, false) => (true, wa),
                    (false, true) => (true, wb),
                    (false, false) => (false, format!("{wa}, and {wb}")),
                },
            };
            let held = if c.busy {
                Some("in use now".to_owned())
            } else if age < GRACE_S {
                Some(format!("used {} ago: too lately to let go", span(age)))
            } else {
                None
            };
            Weighed { copy: c, s, kept, why, held }
        })
        .collect()
}

/// What the rules say of each copy now.
pub fn verdicts(set: &Set, look: &Look) -> Vec<Verdict> {
    weigh(set, look)
        .into_iter()
        .map(|w| Verdict { from: w.copy.from.clone(), key: w.copy.key.clone(), kept: w.kept || w.held.is_some(), why: w.held.unwrap_or(w.why) })
        .collect()
}

/// What to let go now, in order: what the rules don't keep, when its schedule came round or a
/// person asked; then, while the disk has too little room, what they don't keep of the copies that
/// say so, and after it what they keep of those that let even that go, each least recently used
/// first. A step for room is to be taken only while the room is still short ([`Step::room`]): the
/// bytes it frees are counted here where they're known, and the runner looks at its disk again
/// before each.
pub fn plan(set: &Set, look: &Look) -> Vec<Step> {
    let weighed = weigh(set, look);
    let mut order: Vec<&Weighed> = weighed.iter().filter(|w| w.held.is_none()).collect();
    order.sort_by(|a, b| a.copy.used.cmp(&b.copy.used).then_with(|| a.copy.from.cmp(&b.copy.from)).then_with(|| a.copy.key.cmp(&b.copy.key)));
    let since = look.since.max(look.now.saturating_sub(CATCH_UP_S));
    let mut due: HashMap<String, bool> = HashMap::new();
    let mut free = look.disk.map(|d| d.free);
    let mut steps = Vec::new();
    let mut taken: HashSet<(&str, &str)> = HashSet::new();
    fn step(w: &Weighed, trigger: Trigger, why: String, room: Option<u64>) -> Step {
        Step { from: w.copy.from.clone(), key: w.copy.key.clone(), trigger, why, room }
    }
    for w in order.iter().copied().filter(|w| !w.kept) {
        let when = if look.asked {
            Some((Trigger::Asked, "asked to now".to_owned()))
        } else {
            w.s.schedule
                .as_ref()
                .filter(|(c, tz, said, _)| *due.entry(said.clone()).or_insert_with(|| look.now > since && !c.fires_in(since, look.now, tz, 1).is_empty()))
                .map(|(_, _, said, whose)| (Trigger::Schedule, format!("its schedule came round ({said}, {})", whose.whose())))
        };
        if let Some((trigger, when)) = when {
            steps.push(step(w, trigger, format!("{when}: {}", w.why), None));
            free = free.map(|f| f.saturating_add(w.copy.bytes.unwrap_or(0)));
            taken.insert((w.copy.from.as_str(), w.copy.key.as_str()));
        }
    }
    for beyond in [false, true] {
        for w in order.iter().copied().filter(|w| w.kept == beyond && (!beyond || w.s.beyond)) {
            let (Some(f), Some((wanted, said))) = (free, &w.s.room) else { continue };
            if f >= *wanted || taken.contains(&(w.copy.from.as_str(), w.copy.key.as_str())) {
                continue;
            }
            let why = if beyond {
                format!("free space under {said}, and nothing the rules let go left: the least recently used of those they keep ({})", w.why)
            } else {
                format!("free space under {said}: {}", w.why)
            };
            steps.push(step(w, Trigger::Room, why, Some(*wanted)));
            free = Some(f.saturating_add(w.copy.bytes.unwrap_or(0)));
            taken.insert((w.copy.from.as_str(), w.copy.key.as_str()));
        }
    }
    steps
}

/// When a schedule next comes round for this runner: its own (every runner's, or its own rule), or
/// a copy's.
pub fn next(set: &Set, copies: &[TreeCopy], disk: Option<Disk>, now: i64) -> Option<i64> {
    let mut seen = HashSet::new();
    std::iter::once(set.settle(&TreeCopy::default(), disk))
        .chain(copies.iter().map(|c| set.settle(c, disk)))
        .filter_map(|s| s.schedule)
        .filter(|(_, _, said, _)| seen.insert(said.clone()))
        .filter_map(|(c, tz, ..)| c.fires_in(now, now.saturating_add(AHEAD_S), &tz, 1).first().copied())
        .min()
}

/// Seconds as a person says them: "40 seconds", "5 minutes", "3 hours", "9 days".
fn span(s: i64) -> String {
    let (n, unit) = match s {
        s if s < 90 => (s, "second"),
        s if s < 90 * 60 => (s / 60, "minute"),
        s if s < 36 * 3600 => (s / 3600, "hour"),
        s => (s / 86_400, "day"),
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// Hours as a person says them: "an hour", "36 hours", "a day", "7 days".
fn hours(h: u32) -> String {
    match h {
        1 => "an hour".to_owned(),
        24 => "a day".to_owned(),
        h if h % 24 == 0 => format!("{} days", h / 24),
        h => format!("{h} hours"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3600;
    const NOW: i64 = 1_800_000_000;

    fn copy(key: &str, used_ago: i64) -> TreeCopy {
        TreeCopy { from: "mac".into(), key: format!("{key}-0123456789"), created: NOW - used_ago, used: NOW - used_ago, ..TreeCopy::default() }
    }

    fn look(copies: &[TreeCopy]) -> Look<'_> {
        Look { now: NOW, since: NOW - 60, disk: None, asked: false, copies }
    }

    fn keys(steps: &[Step]) -> Vec<&str> {
        steps.iter().map(|s| s.key.split('-').next().unwrap_or("")).collect()
    }

    /// A schedule that came round in the last minute: every minute.
    fn every_minute(rules: Rules) -> Rules {
        Rules { schedule: Some("* * * * *".into()), ..rules }
    }

    #[test]
    fn nothing_goes_until_a_schedule_room_or_a_person_says_when() {
        let copies = vec![copy("a", 40 * 24 * H), copy("b", 30 * 24 * H)];
        let set = Set { system: Rules { newest: Some(0), hours: Some(1), ..Rules::default() }, ..Set::default() };
        assert!(plan(&set, &look(&copies)).is_empty());
        // Free space under 90% of a disk with plenty free: nothing either.
        let set = Set { system: Rules { free_percent: Some(10), ..set.system }, ..Set::default() };
        assert!(plan(&set, &Look { disk: Some(Disk { total: 100 << 30, free: 50 << 30 }), ..look(&copies) }).is_empty());
        // Asked, the rules apply now.
        assert_eq!(keys(&plan(&set, &Look { asked: true, ..look(&copies) })), ["a", "b"]);
    }

    #[test]
    fn a_schedule_lets_go_what_the_newest_and_the_recent_dont_keep_oldest_first() {
        let copies = vec![copy("new", 2 * H), copy("old", 9 * 24 * H), copy("mid", 2 * 24 * H), copy("older", 20 * 24 * H)];
        let set = Set { system: every_minute(Rules { newest: Some(2), ..Rules::default() }), ..Set::default() };
        let steps = plan(&set, &look(&copies));
        assert_eq!(keys(&steps), ["older", "old"]);
        let why = steps.first().map(|s| s.why.as_str()).unwrap_or("");
        assert!(why.contains("its schedule came round (* * * * * UTC, every runner's rule)") && why.contains("not one of the newest 2 on this runner"), "{why}");
        assert!(steps.iter().all(|s| s.trigger == Trigger::Schedule && s.room.is_none()));
        // Its schedule didn't come round since the last look (NOW is on the hour): nothing.
        let hourly = Set { system: Rules { schedule: Some("0 * * * *".into()), ..set.system.clone() }, ..Set::default() };
        assert!(plan(&hourly, &Look { now: NOW + 30 * 60, since: NOW + 60, ..look(&copies) }).is_empty());
        // A look that comes late (the runner was away) catches up: once.
        assert_eq!(keys(&plan(&hourly, &Look { since: NOW - 5 * H, ..look(&copies) })), ["older", "old"]);
    }

    #[test]
    fn either_rule_keeps_a_copy_unless_both_must() {
        let copies = vec![copy("newest", 3 * 24 * H), copy("recent", 4 * 24 * H), copy("stale", 20 * 24 * H)];
        let rules = every_minute(Rules { newest: Some(1), hours: Some(5 * 24), ..Rules::default() });
        let either = Set { system: rules.clone(), ..Set::default() };
        assert_eq!(keys(&plan(&either, &look(&copies))), ["stale"]);
        let v = verdicts(&either, &look(&copies));
        assert!(v.iter().any(|v| v.key.starts_with("recent") && v.kept && v.why.contains("used 4 days ago, within 5 days (every runner's rule)")), "{v:?}");
        let both = Set { system: Rules { both: Some(true), ..rules }, ..Set::default() };
        assert_eq!(keys(&plan(&both, &look(&copies))), ["stale", "recent"]);
        let v = verdicts(&both, &look(&copies));
        assert!(v.iter().any(|v| v.key.starts_with("recent") && !v.kept && v.why.contains("both rules must keep it")), "{v:?}");
    }

    #[test]
    fn a_copy_in_use_or_just_used_stays_whatever_the_rules_say() {
        let mut busy = copy("busy", 30 * 24 * H);
        busy.busy = true;
        let copies = vec![busy, copy("just", 5 * 60), copy("idle", 30 * 24 * H)];
        let set = Set { system: every_minute(Rules::default()), ..Set::default() };
        assert_eq!(keys(&plan(&set, &look(&copies))), ["idle"]);
        let v = verdicts(&set, &look(&copies));
        assert!(v.iter().any(|v| v.key.starts_with("busy") && v.kept && v.why == "in use now"), "{v:?}");
        assert!(v.iter().any(|v| v.key.starts_with("just") && v.kept && v.why.contains("too lately")), "{v:?}");
        assert!(v.iter().any(|v| v.key.starts_with("idle") && !v.kept && v.why == "no rule keeps it"), "{v:?}");
    }

    #[test]
    fn the_most_particular_scope_sets_each_rule_and_counts_its_own_copies() {
        let mut kiln = vec![copy("k1", 3 * 24 * H), copy("k2", 4 * 24 * H), copy("k3", 5 * 24 * H)];
        for c in &mut kiln {
            c.repo = Some("github.com/hyper-light/kiln".into());
        }
        let mut mine = copy("mine", 50 * 24 * H);
        mine.agents = vec!["older".into(), "a1".into()];
        let copies: Vec<TreeCopy> = kiln.into_iter().chain([copy("x1", 2 * 24 * H), copy("x2", 3 * 24 * H), mine]).collect();
        let set = Set {
            system: every_minute(Rules { newest: Some(1), ..Rules::default() }),
            // This runner keeps no number of its own copies: 0 says none.
            runner: Rules { newest: Some(0), ..Rules::default() },
            repos: BTreeMap::from([("github.com/hyper-light/kiln".into(), Rules { newest: Some(2), ..Rules::default() })]),
            agents: BTreeMap::from([("a1".into(), Rules { hours: Some(24 * 60), ..Rules::default() })]),
        };
        // The repository's newest two of its own stay; the runner's 0 lets the others go; the
        // agent's copy stays by its own rule, though older than any.
        assert_eq!(keys(&plan(&set, &look(&copies))), ["k3", "x2", "x1"]);
        let v = verdicts(&set, &look(&copies));
        assert!(v.iter().any(|v| v.key.starts_with("k1") && v.why.contains("one of the newest 2 of github.com/hyper-light/kiln (github.com/hyper-light/kiln's rule)")), "{v:?}");
        assert!(v.iter().any(|v| v.key.starts_with("mine") && v.kept && v.why.contains("its agent's rule")), "{v:?}");
        assert!(v.iter().any(|v| v.key.starts_with("x1") && v.why == "no rule keeps it"), "{v:?}");
    }

    #[test]
    fn too_little_room_lets_go_until_there_is_room_and_past_the_rules_only_when_they_say() {
        let gib = 1u64 << 30;
        let sized = |key: &str, ago: i64, g: u64| TreeCopy { bytes: Some(g * gib), ..copy(key, ago) };
        let copies = vec![sized("kept", 2 * 24 * H, 30), sized("a", 9 * 24 * H, 10), sized("b", 8 * 24 * H, 10), sized("c", 7 * 24 * H, 10)];
        let disk = Some(Disk { total: 1000 * gib, free: 85 * gib });
        let rules = Rules { newest: Some(1), free_gib: Some(100), ..Rules::default() };
        let set = Set { system: rules.clone(), ..Set::default() };
        // 85 free, 100 wanted: a and b (the least recently used) go, c stays.
        let steps = plan(&set, &Look { disk, ..look(&copies) });
        assert_eq!(keys(&steps), ["a", "b"]);
        assert!(steps.iter().all(|s| s.trigger == Trigger::Room && s.room == Some(100 * gib)));
        assert!(steps.first().is_some_and(|s| s.why.starts_with("free space under 100 GiB (every runner's rule): not one of the newest 1")), "{steps:?}");
        // 20 free of 1000 under 10%: everything the rules let go, and the copy they keep stays...
        let tight = Some(Disk { total: 1000 * gib, free: 20 * gib });
        let pct = Set { system: Rules { free_percent: Some(10), free_gib: None, ..rules.clone() }, ..Set::default() };
        assert_eq!(keys(&plan(&pct, &Look { disk: tight, ..look(&copies) })), ["a", "b", "c"]);
        // ... unless they say past them too.
        let beyond = Set { system: Rules { beyond: Some(true), ..pct.system.clone() }, ..Set::default() };
        let steps = plan(&beyond, &Look { disk: tight, ..look(&copies) });
        assert_eq!(keys(&steps), ["a", "b", "c", "kept"]);
        assert!(steps.last().is_some_and(|s| s.why.contains("the least recently used of those they keep")), "{steps:?}");
        // Sizes not known yet: every copy the rules let go is a step, each taken only while the room is short.
        let unknown: Vec<TreeCopy> = copies.iter().map(|c| TreeCopy { bytes: None, ..c.clone() }).collect();
        assert_eq!(keys(&plan(&set, &Look { disk, ..look(&unknown) })), ["a", "b", "c"]);
    }

    #[test]
    fn the_next_time_a_schedule_comes_round() {
        let set = Set { system: Rules { schedule: Some("0 3 * * *".into()), ..Rules::default() }, ..Set::default() };
        let at = next(&set, &[], None, NOW).unwrap_or(0);
        assert!(at > NOW && at - NOW <= 24 * H && at % 86_400 == 3 * H, "{at}");
        let mut c = copy("k", H);
        c.repo = Some("r".into());
        let set = Set { repos: BTreeMap::from([("r".into(), Rules { schedule: Some("*/5 * * * *".into()), ..Rules::default() })]), ..set };
        assert!(next(&set, &[c], None, NOW).is_some_and(|t| t - NOW <= 300));
        assert_eq!(next(&Set::default(), &[], None, NOW), None);
    }

    #[test]
    fn rules_kiln_takes_and_those_it_refuses() {
        assert!(Rules::default().check().is_ok());
        assert!(Rules { schedule: Some("0 3 * * *".into()), timezone: Some("Europe/Berlin".into()), newest: Some(5), hours: Some(168), free_percent: Some(10), ..Rules::default() }.check().is_ok());
        // "" and 0 say none here.
        assert!(Rules { schedule: Some(String::new()), timezone: Some(String::new()), newest: Some(0), ..Rules::default() }.check().is_ok());
        for bad in [
            Rules { schedule: Some("every day".into()), ..Rules::default() },
            Rules { timezone: Some("Mars/Olympus".into()), ..Rules::default() },
            Rules { free_percent: Some(99), ..Rules::default() },
            Rules { newest: Some(1_000_000), ..Rules::default() },
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
        // Unset is left out, so the scope above's shows through; what this side doesn't know is skipped.
        assert_eq!(serde_json::to_string(&Rules { newest: Some(0), ..Rules::default() }).ok().as_deref(), Some(r#"{"newest":0}"#));
        let later: Answer = serde_json::from_str(r#"{"set":{"system":{"hours":24,"later":1}},"asked":3,"more":true}"#).unwrap_or_default();
        assert_eq!((later.set.system.hours, later.asked), (Some(24), 3));
    }

    #[test]
    fn spans_and_hours_read_as_people_say_them() {
        assert_eq!(span(40), "40 seconds");
        assert_eq!(span(5 * 60), "5 minutes");
        assert_eq!(span(3 * H), "3 hours");
        assert_eq!(span(9 * 24 * H), "9 days");
        assert_eq!(hours(1), "an hour");
        assert_eq!(hours(36), "36 hours");
        assert_eq!(hours(24 * 7), "7 days");
    }
}
