//! What each runner can take and what waits, and why (docs/reach.md §2.2): the server's view of the
//! fleet's offers, for people (Runners → Offers) and agents (`GET /api/reach/offers`).
//!
//! - **Runners say** their admission's state as hints ([`hint::Hint`], lb/core's
//!   `AgentMsg::Offers` under the named feature [`FEATURE`]): budget, room, line by class, the
//!   answer for each ask size they know, whether they take work. The runner's admission decides;
//!   this only shows it.
//! - **What waits** is kept as it changes ([`demand::Demand`], AUDIT O3, P1): the CI engine's legs
//!   from its commands (`Wait`, `Assign`, `Settled`), and runners' own runs from their hints'
//!   deltas, grouped by need and size. Nothing is rebuilt or sorted per message.
//! - **Events, not storms** (AUDIT O2): a hint changes the view only when it changes something
//!   decided by ([`relevant`]); what changed in one turn of the runtime goes as one
//!   `ServerEvent::Offers` with the view's new version, which is also its ETag.

pub mod demand;
pub mod hint;
pub mod relevant;
pub mod view;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use kiln_ci::sched::state::{Class, Resources};
use parking_lot::Mutex;

use crate::hub::Hub;
use demand::{Demand, Item, Moved, Source};
use hint::{Amounts, Hint};
use relevant::Said;

/// The named feature a runner speaks when it sends hints, and a server when it reads them.
/// TODO(lb/core): `proto::feature::OFFERS`, once lb/core names it in `feature::ALL` (docs/wire.md,
/// "Named features"; ~/.cache/kiln-lb/notes/asks/core.md).
pub const FEATURE: &str = "offers";

/// What a CI leg needs and asks, as the engine was given it, until it waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Want {
    pub need: String,
    pub ask: Amounts,
    pub class: Class,
}

impl Want {
    /// A leg's: its labels and its workflow's requirements, spelled in one order, and its request.
    pub fn of_leg(leg: &kiln_ci::sched::LegIn) -> Want {
        let mut words: Vec<&str> = leg.route.labels.iter().chain(&leg.route.requires).map(String::as_str).collect();
        words.sort_unstable();
        words.dedup();
        Want { need: words.join(" "), ask: amounts(&leg.request), class: leg.class }
    }
}

/// `r` in the hint's dimensions.
pub fn amounts(r: &Resources) -> Amounts {
    [r.cpu.get(), r.mem, r.disk, u64::from(r.gpu)]
}

/// A runner's quoted sizes, `had` before, now `now`, in the count of runners quoting each: whether
/// a size is quoted that none quoted before. O(its quotes).
fn requote(quoted: &mut BTreeMap<Amounts, u32>, had: &[Amounts], now: impl Iterator<Item = Amounts>) -> bool {
    for a in had {
        if let Some(n) = quoted.get_mut(a) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                quoted.remove(a);
            }
        }
    }
    let mut fresh = false;
    for a in now {
        let n = quoted.entry(a).or_insert(0);
        fresh |= *n == 0;
        *n = n.saturating_add(1);
    }
    fresh
}

/// A runner as the view has it: what it said last, and what was last shown of it.
#[derive(Debug, Clone)]
struct Runner {
    latest: Said,
    shown: Said,
}

#[derive(Debug, Default)]
struct Inner {
    runners: BTreeMap<String, Runner>,
    demand: Demand,
    /// The CI engine's legs it was given, until they settle.
    legs: HashMap<String, Want>,
    /// The sizes runners quote, each counted once for each runner quoting it last: with the
    /// waiting work's, the sizes known, which every runner's answer is held to.
    quoted: BTreeMap<Amounts, u32>,
    /// The view's version: one up for each turn in which what's shown changed.
    version: u64,
    /// Something shown changed since the last version.
    dirty: bool,
    /// Hints taken, and of them those that changed what's shown.
    heard: u64,
    told: u64,
}

/// The server's offers: the runners' hints and the waiting work.
pub struct Offers {
    inner: Mutex<Inner>,
    /// A change since the last version wakes the one task that tells it ([`spawn`]): one at a time,
    /// so what changes before it runs goes with it.
    kick: (async_channel::Sender<()>, async_channel::Receiver<()>),
    /// When this server's offers began (unix ms): with the version, the ETag, so a version of an
    /// earlier process is never taken for one of this.
    boot: i64,
}

impl Default for Offers {
    fn default() -> Self {
        Offers { inner: Mutex::new(Inner::default()), kick: async_channel::bounded(1), boot: crate::util::now_ms() }
    }
}

/// FNV-1a of `b`: an ETag's short name for a page's cursor (not a secret, not a key).
fn fnv(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, x| (h ^ u64::from(*x)).wrapping_mul(0x0000_0100_0000_01b3))
}

impl Offers {
    /// The ETag of `page` of the view at `version`.
    pub fn etag(&self, version: u64, page: &view::Page) -> String {
        format!("W/\"offers-{}-{version}-{:x}-{}\"", self.boot, page.after.as_deref().map_or(0, |a| fnv(a.as_bytes())), page.limit.unwrap_or(0))
    }

    /// Something shown changed: the next version is due, and its task woken if this is the first
    /// change since the last.
    fn mark(&self, g: &mut Inner) {
        if !g.dirty {
            g.dirty = true;
            let _ = self.kick.0.try_send(());
        }
    }

    /// Runner `runner`'s hint, heard at `now` (the server's clock): whether it changed what's shown.
    /// An older one than the last heard (its epoch, then its sequence) is dropped.
    pub fn heard(&self, runner: &str, mut hint: Hint, now: i64) -> bool {
        let mut g = self.inner.lock();
        if let Some(r) = g.runners.get(runner)
            && (hint.epoch, hint.seq) <= (r.latest.hint.epoch, r.latest.hint.seq)
        {
            return false;
        }
        g.heard += 1;
        // What it says of its own runs that wait, in the server's clock: each wait's start as far
        // before now as it was before the hint's `at`.
        let waits: Vec<(String, Item)> = std::mem::take(&mut hint.waits)
            .into_iter()
            .map(|w| (w.run, Item { need: w.need, ask: w.ask, class: w.class, since: now.saturating_sub(hint.at.saturating_sub(w.since).max(0)), reason: w.reason }))
            .collect();
        let gone = std::mem::take(&mut hint.gone);
        let mut moved = g.demand.owner_says(runner, hint.whole, waits, &gone);
        let had = g.runners.get(runner).map(|r| r.latest.hint.quotes.iter().map(|q| q.ask).collect::<Vec<_>>()).unwrap_or_default();
        moved.new_size |= requote(&mut g.quoted, &had, hint.quotes.iter().map(|q| q.ask));
        let said = Said { hint, heard: now };
        let Inner { runners, demand, quoted, .. } = &mut *g;
        let changed = match runners.get_mut(runner) {
            Some(r) => {
                let changed = relevant::relevant(&r.shown, &said, demand.sizes().chain(quoted.keys()));
                if changed {
                    r.shown = said.clone();
                }
                r.latest = said;
                changed
            }
            None => {
                runners.insert(runner.to_owned(), Runner { latest: said.clone(), shown: said });
                true
            }
        };
        let changed = self.moved(&mut g, moved) || changed;
        if changed {
            g.told += 1;
            self.mark(&mut g);
        }
        changed
    }

    /// What the waiting work's change `m` changes: a count shown, and with a size no group asked
    /// before, each runner's answer for it (O(runners), once for each new size).
    fn moved(&self, g: &mut Inner, m: Moved) -> bool {
        if m.new_size {
            let Inner { runners, demand, quoted, .. } = g;
            for r in runners.values_mut() {
                if relevant::relevant(&r.shown, &r.latest, demand.sizes().chain(quoted.keys())) {
                    r.shown = r.latest.clone();
                }
            }
        }
        m.counts
    }

    /// Runner `runner` went away or was forgotten: what it said of its admission goes (nothing it
    /// said holds while it can't be asked), and so do its own runs that waited, until it says them
    /// again (whole, once it's back).
    pub fn gone(&self, runner: &str) -> bool {
        let mut g = self.inner.lock();
        let was = g.runners.remove(runner);
        if let Some(r) = &was {
            let asks: Vec<Amounts> = r.latest.hint.quotes.iter().map(|q| q.ask).collect();
            requote(&mut g.quoted, &asks, std::iter::empty());
        }
        let had = was.is_some();
        let waited = g.demand.forget_owner(runner);
        if had || waited {
            self.mark(&mut g);
        }
        had || waited
    }

    /// The CI engine was given leg `leg`, wanting `want`.
    pub fn leg_wants(&self, leg: &str, want: Want) {
        self.inner.lock().legs.insert(leg.to_owned(), want);
    }

    /// The CI engine says leg `leg` waits, for `reason`, at `now`: whether a count shown changed.
    /// One it wasn't given (from before a restart of this) is counted with what it's known by:
    /// nothing, any size.
    pub fn leg_waits(&self, leg: &str, reason: &str, now: i64) -> bool {
        let mut g = self.inner.lock();
        let w = g.legs.get(leg).cloned().unwrap_or(Want { need: String::new(), ask: [0; hint::DIMS], class: Class::default() });
        let moved = g.demand.put(Source::Leg, leg, Item { need: w.need, ask: w.ask, class: w.class, since: now, reason: reason.to_owned() });
        let changed = self.moved(&mut g, moved);
        if changed {
            self.mark(&mut g);
        }
        changed
    }

    /// Leg `leg` waits no more (granted, retried, cancelled): whether it did.
    pub fn leg_placed(&self, leg: &str) -> bool {
        let mut g = self.inner.lock();
        let was = g.demand.remove(&Source::Leg, leg);
        if was {
            self.mark(&mut g);
        }
        was
    }

    /// Leg `leg` is done for good: out of what waits and what's known.
    pub fn leg_done(&self, leg: &str) -> bool {
        let was = self.leg_placed(leg);
        self.inner.lock().legs.remove(leg);
        was
    }

    /// The next version, if something shown changed since the last: what [`spawn`]'s task tells.
    pub fn flush(&self) -> Option<u64> {
        let mut g = self.inner.lock();
        if !g.dirty {
            return None;
        }
        g.dirty = false;
        g.version += 1;
        Some(g.version)
    }

    /// The view's version now.
    pub fn version(&self) -> u64 {
        self.inner.lock().version
    }

    /// Hints taken, and of them those that changed what's shown.
    pub fn counts(&self) -> (u64, u64) {
        let g = self.inner.lock();
        (g.heard, g.told)
    }

    /// The view: every runner `machines` names (the hub's), with what it last showed, and a page of
    /// the waiting groups ([`view::Page`]).
    pub fn view(&self, machines: &[view::Machine], page: &view::Page, now: i64) -> view::OffersView {
        let g = self.inner.lock();
        view::build(machines, &g.runners.iter().map(|(n, r)| (n.as_str(), &r.shown)).collect(), &g.demand, g.version, page, now)
    }
}

/// Runner `runner`'s hint as it sent it (`AgentMsg::Offers { json }`): one line in the hub's
/// handler. One that can't be read is noted and dropped; the runner's admission is unaffected.
pub fn heard(hub: &Hub, runner: &str, json: &str) {
    match serde_json::from_str::<Hint>(json) {
        Ok(h) => {
            hub.offers.heard(runner, h, crate::util::now_ms());
        }
        Err(e) => tracing::debug!(runner, error = %e, "an offers hint that can't be read"),
    }
}

/// Runner `runner` went away, or was forgotten.
pub fn gone(hub: &Hub, runner: &str) {
    hub.offers.gone(runner);
}

/// The CI engine was given `leg`: what it needs and asks, for when it waits.
pub fn ci_leg(hub: &Hub, leg: &kiln_ci::sched::LegIn) {
    hub.offers.leg_wants(&leg.id, Want::of_leg(leg));
}

/// The dispatcher let leg `leg` go (its run finished, its repository went): out of what waits.
pub fn leg_done(hub: &Hub, leg: &str) {
    hub.offers.leg_done(leg);
}

/// What a CI engine's command says of what waits: one line at the top of the dispatcher's loop.
pub fn ci(hub: &Hub, c: &kiln_ci::sched::Command) {
    use kiln_ci::sched::Command;
    match c {
        Command::Wait { leg, reason } => {
            let kind = serde_json::to_value(reason).ok().and_then(|v| v.get("reason").and_then(serde_json::Value::as_str).map(str::to_owned)).unwrap_or_default();
            hub.offers.leg_waits(leg, &kind, crate::util::now_ms());
        }
        // A failed attempt waits to be tried again: what the next `Wait` says replaces it.
        Command::Retry { leg, .. } => {
            hub.offers.leg_waits(leg, "retry", crate::util::now_ms());
        }
        Command::Assign { leg, .. } | Command::Lost { leg, .. } | Command::Adopted { leg, .. } => {
            hub.offers.leg_placed(leg);
        }
        Command::Settled { leg, .. } => {
            hub.offers.leg_done(leg);
        }
        _ => {}
    }
}

/// The task that tells the pages each new version: woken by a change ([`Offers::mark`]), never on
/// a timer. Whatever changed before it runs goes in the one event.
pub fn spawn(hub: Arc<Hub>) {
    let rx = hub.offers.kick.1.clone();
    crate::rt::spawn(async move {
        while rx.recv().await.is_ok() {
            if let Some(version) = hub.offers.flush() {
                hub.emit(&crate::wire::events::ServerEvent::Offers { version });
            }
        }
    });
}
