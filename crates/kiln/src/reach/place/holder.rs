//! kiln's runner holding its runs' room through its machine's capacity keeper: placement's shell of
//! `kiln_ci::place::holder::Holding` (docs/lb.md "A machine's keeper"; Hex's
//! `docs/design/capacity-view.md`, the holder's items 1 to 6; kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, step 13).
//!
//! Where `[capacity]` names the machine's keeper, every start here (an owner's claim, its kernel's
//! start, this node's own runs, a CI leg, an agent session, a word the owner says itself) reserves
//! its room through the keeper before this runner's ledger starts it, and starts only on the
//! keeper's grant; what waits on the start (a spec, a local run, a leg's or a session's answer)
//! waits with it. A reserve not granted (refused, or unanswered once its wait ends: `Waits`) starts
//! nothing: a claim lapses, a kernel start is declined for room, a leg is refused, each saying why. A
//! start that asks nothing reserves nothing and starts at once, its box numbered (its cold start). A
//! run's reservation is kept with its record before its process starts, so this runner's next
//! process lists those whose work outlived it, and goes back to the keeper as the run ends, after its
//! record. A word for the keeper goes on the link it was made for alone (CV9).

use std::collections::BTreeMap;
use std::sync::Arc;

use hex_api::capacity::{self as hex, ReservationId, ToHolder};
use kiln_ci::place::ask::{self, Ask, Peer};
use kiln_ci::place::handoff::Held;
use kiln_ci::place::holder::{BoxName, Event, Holding};
use kiln_ci::place::keeper::{Keeper, NotGranted, Unsynced, WaitEnded};
use kiln_ci::place::msg::ToOwner;
use kiln_ci::place::runner::Start;
use kiln_ci::sched::state::Fence;

use super::{CI, Claim, Out, Placer};

/// A run as its holding knows it: its owner, the owner's epoch, and the run.
type RunKey = (String, u64, String);

/// Placement's holding through its machine's keeper (module docs).
pub(super) struct Holder {
    holding: Holding,
    /// The link its words go on now: one more for each opened ([`Placer::keeper_link`]).
    link: u64,
    /// The starts waiting for their reserves' answers, by reservation; and the reservation each
    /// waits under, by its run, so a start said again while it waits joins it.
    pending: BTreeMap<ReservationId, Pending>,
    asking: BTreeMap<RunKey, ReservationId>,
    /// What each run started here holds through the keeper (its box), until it ends.
    runs: BTreeMap<RunKey, BoxName>,
}

/// A start waiting for its reserve's answer.
struct Pending {
    run: RunKey,
    begin: Begin,
    then: Vec<Then>,
}

/// How a start waiting for its reserve begins once granted.
enum Begin {
    /// Under its live claim.
    Claim(Fence),
    /// The placement kernel's start of `ask` (`ToRunner::Fast`): its owner's handle of it, and
    /// this runner's kernel epoch it was decided under.
    Kernel { ask: Arc<Ask>, kernel: kiln_ci::lb::Handle, at: u32 },
    /// A CI leg, admitted on its grant or not at all.
    Leg(Box<Ask>),
}

/// What waits on a start's outcome besides its owner, which hears it from the ledger.
pub(super) enum Then {
    /// Spec `id` of asker `peer`: its verdict ([`Out::SpecReady`]).
    Spec { peer: String, id: u64 },
    /// This node's own run: whether it started ([`Out::Started`]).
    Local,
    /// An agent session: whether it started ([`Out::Session`]).
    Session(Claim),
}

/// A spec's verdict on its claim's start, as its asker is told it.
pub(super) fn claim_verdict(run: &str, started: Option<Start>) -> Result<(), String> {
    match started {
        Some(Start::New) => Ok(()),
        Some(Start::Already) => Err(format!("{run} ran here already: it isn't started again")),
        None => Err(format!("{run}'s claim here isn't live: it isn't started")),
    }
}

/// A spec's verdict on its kernel start, as its asker is told it.
pub(super) fn kernel_verdict(run: &str, started: Option<Start>) -> Result<(), String> {
    match started {
        Some(Start::New) => Ok(()),
        Some(Start::Already) => Err(format!("{run} ran here already: it isn't started again")),
        None => Err(format!("{run} has no room here now: it isn't started")),
    }
}

impl Placer {
    /// Its machine's capacity keeper is named (`[capacity]`): every start here holds its room
    /// through it from now on. Its holding starts under this node's epoch, durable and one more at
    /// each start of its process (the holder's item 2), with its survivors (item 3): the
    /// reservations its records keep of runs whose work outlived its last process ([`Self::restore`]
    /// took them up), each its run's ask's room. Before any word: until its first sync is answered
    /// it stands joining and takes nothing new (CV11). A holding that can't start (more survivors
    /// than one sync lists, or one its core refuses) leaves it unsynced for good, malformed, saying
    /// why.
    pub fn keeper_start(&mut self, now: i64) -> Vec<Out> {
        let mut survivors = Vec::new();
        let mut runs = BTreeMap::new();
        for (owner, by_epoch) in self.admission.records() {
            for (epoch, by_run) in by_epoch {
                for (run, record) in by_run {
                    let (Some((e, seq)), None) = (record.reservation, record.ended) else { continue };
                    let Some(Held::Running { ask: Some(asked), .. }) = self.admission.holding(Peer { name: owner, epoch: *epoch }, run) else { continue };
                    let id = ReservationId { epoch: e, seq };
                    survivors.push(hex::Held { id, amount: hex::Amounts::from_array(ask::amounts(&asked.needs.request)) });
                    runs.insert((owner.clone(), *epoch, run.clone()), BoxName::Reserved(id));
                }
            }
        }
        let mut said = Vec::new();
        match Holding::start(self.me.clone(), self.epoch, &survivors) {
            Ok(holding) => {
                let stands = holding.keeper();
                if !survivors.is_empty() {
                    tracing::info!(survivors = survivors.len(), "this runner holds through its machine's capacity keeper what its last process's runs that go on held");
                }
                self.keeper = Some(Holder { holding, link: 0, pending: BTreeMap::new(), asking: BTreeMap::new(), runs });
                self.admission.keeper_said(stands, now, &mut said);
            }
            Err(why) => {
                tracing::error!(error = ?why, "this runner's holding through its machine's capacity keeper couldn't start: it takes nothing new");
                self.admission.keeper_said(Keeper::Unsynced(Unsynced::Malformed), now, &mut said);
            }
        }
        self.said(said, now)
    }

    /// Whether it holds through its machine's keeper: one is named, and its holding started.
    pub fn holds(&self) -> bool {
        self.keeper.is_some()
    }

    /// The link its holding's words go on now ([`Out::KeeperSay`]): none before the first.
    pub fn keeper_link(&self) -> u64 {
        self.keeper.as_ref().map_or(0, |h| h.link)
    }

    /// A link to its machine's capacity keeper opened at `now` (CV9): its hello said on it.
    pub fn keeper_linked(&mut self, now: i64) -> Vec<Out> {
        let Some(h) = self.keeper.as_mut() else { return Vec::new() };
        h.link = h.link.saturating_add(1);
        let out = h.holding.linked(now);
        self.keeper_out(out, now)
    }

    /// The keeper's word on its link at `now`: the answer to its oldest word waiting, or a hint.
    pub fn keeper_heard(&mut self, word: ToHolder, now: i64) -> Vec<Out> {
        let Some(h) = self.keeper.as_mut() else { return Vec::new() };
        let out = h.holding.heard(word, now);
        self.keeper_out(out, now)
    }

    /// Its link to the keeper ended (`how`): each reserve asked on it isn't granted, what is held
    /// stays held (CV5), and nothing new starts here until a new link's sync is answered (CV11).
    pub fn keeper_ended(&mut self, how: WaitEnded, now: i64) -> Vec<Out> {
        let Some(h) = self.keeper.as_mut() else { return Vec::new() };
        let out = h.holding.ended(how);
        self.keeper_out(out, now)
    }

    /// When its holding's oldest wait on the keeper ends ([`Self::timer`]).
    pub(super) fn keeper_due(&self) -> Option<i64> {
        self.keeper.as_ref().and_then(|h| h.holding.due())
    }

    /// The time, for its holding's waits: a keeper that, its link alive, has answered nothing for
    /// longer than the slowest answer it has given is stalled, and each reserve waiting on it isn't
    /// granted (docs/lb.md "Waits").
    pub(super) fn keeper_woke(&mut self, now: i64) -> Vec<Out> {
        let Some(h) = self.keeper.as_mut() else { return Vec::new() };
        let out = h.holding.woke(now);
        self.keeper_out(out, now)
    }

    /// The box run `run` of owner `owner` (`epoch`) runs as here, as its machine's keeper pairs it
    /// (`BoxName`): its reservation's, or one numbered for a run that asks nothing. None for a run
    /// not started here, or where no keeper is named.
    pub fn box_of(&self, owner: &str, epoch: u64, run: &str) -> Option<BoxName> {
        self.keeper.as_ref()?.runs.get(&(owner.to_owned(), epoch, run.to_owned())).copied()
    }

    /// Where it stands with its machine's keeper, for its view: none where none is named.
    pub(super) fn keeper_view(&self) -> serde_json::Value {
        let Some(h) = &self.keeper else { return serde_json::Value::Null };
        let stands = match h.holding.keeper() {
            Keeper::None => "none".to_owned(),
            Keeper::Unsynced(why) => format!("unsynced: {}", why.why()),
            Keeper::Synced(_) => "synced".to_owned(),
        };
        let boxes: Vec<String> = h.runs.values().map(ToString::to_string).collect();
        serde_json::json!({ "link": format!("{:?}", h.holding.link()).to_lowercase(), "stands": stands, "asking": h.pending.len(), "held": h.holding.held().count(), "boxes": boxes })
    }

    /// Run `run` of owner `owner` (`epoch`) starts here under claim `fence` (its owner's `Start`,
    /// this node's own run, an agent session): at once where no keeper is named, and where the
    /// claim isn't live (answered as ever: it ran here, or lapsed); else on its reserve's grant, of
    /// the room the claim holds (`Admission::live_claim`), its outcome then told to `then`. A claim
    /// that asks nothing starts at once; a reserve its holding can't say (unsynced, full, its ids
    /// spent) lapses the claim, saying why. `None` while its reserve waits, else what the ledger
    /// said (`Admission::start`). The ledger's words go in `said`, the keeper's in `out`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn claim_start(&mut self, owner: &str, epoch: u64, run: &str, fence: Fence, then: Option<Then>, now: i64, said: &mut Vec<(String, ToOwner)>, out: &mut Vec<Out>) -> Option<Option<Start>> {
        let from = Peer { name: owner, epoch };
        let key = (owner.to_owned(), epoch, run.to_owned());
        let asked = match (self.keeper.as_mut(), self.admission.live_claim(from, run, fence, now)) {
            (Some(h), Some(amounts)) => {
                // Said again while its reserve waits: it waits with it.
                if let Some(p) = h.asking.get(&key).copied().and_then(|id| h.pending.get_mut(&id))
                    && matches!(p.begin, Begin::Claim(f) if f == fence)
                {
                    p.then.extend(then);
                    return None;
                }
                Some(h.holding.reserve(amounts, now))
            }
            _ => None,
        };
        match asked {
            None => Some(self.admission.start(from, run, fence, now, said)),
            Some(Err(why)) => {
                self.admission.not_granted(from, run, fence, why, now, said);
                Some(None)
            }
            Some(Ok(None)) => {
                let started = self.admission.start(from, run, fence, now, said);
                self.unreserved(key, started);
                Some(started)
            }
            Some(Ok(Some((id, word)))) => {
                self.reserving(id, word, Pending { run: key, begin: Begin::Claim(fence), then: then.into_iter().collect() }, out);
                None
            }
        }
    }

    /// The placement kernel's start of `ask` from owner `owner` (`epoch`), its handle `kernel`,
    /// decided under this runner's kernel epoch `at` (`ToRunner::Fast`, this node's own kernel
    /// start, a word its owner says itself): as ever where no keeper is named, and where it starts
    /// nothing new (said again: it runs or ran here; or its kind of work barred here, L2); else on
    /// its reserve's grant, of its need, its outcome then told to `then`. One that asks nothing
    /// starts at once; a reserve its holding can't say declines it for room in this runner's word,
    /// so its owner's kernel picks again (`Admission::fast_not_granted`). `None` while its reserve
    /// waits, else what the ledger said (`Admission::fast`).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn kernel_start(&mut self, owner: &str, epoch: u64, ask: Arc<Ask>, kernel: kiln_ci::lb::Handle, at: u32, then: Option<Then>, now: i64, said: &mut Vec<(String, ToOwner)>, out: &mut Vec<Out>) -> Option<Option<Start>> {
        let from = Peer { name: owner, epoch };
        let fresh = self.keeper.is_some() && self.admission.holding(from, &ask.run).is_none() && !self.kernel_barred(&ask);
        let key = (owner.to_owned(), epoch, ask.run.clone());
        let asked = match self.keeper.as_mut() {
            Some(h) if fresh => {
                if let Some(p) = h.asking.get(&key).copied().and_then(|id| h.pending.get_mut(&id))
                    && matches!(p.begin, Begin::Kernel { kernel: k, .. } if k == kernel)
                {
                    p.then.extend(then);
                    return None;
                }
                Some(h.holding.reserve(ask::amounts(&ask.needs.request), now))
            }
            _ => None,
        };
        match asked {
            None => Some(self.fast(owner, epoch, ask, kernel, at, now, said)),
            Some(Err(why)) => {
                tracing::debug!(run = %ask.run, why = why.why(), "a kernel start not reserved here: declined for room");
                self.admission.fast_not_granted(from, &ask.run, kernel, now, said);
                Some(None)
            }
            Some(Ok(None)) => {
                let started = self.fast(owner, epoch, ask, kernel, at, now, said);
                self.unreserved(key, started);
                Some(started)
            }
            Some(Ok(Some((id, word)))) => {
                self.reserving(id, word, Pending { run: key, begin: Begin::Kernel { ask, kernel, at }, then: then.into_iter().collect() }, out);
                None
            }
        }
    }

    /// CI leg `ask.run` ([`Self::leg`]): admitted now where no keeper is named; else its room
    /// reserved first, its request, and admitted on the grant alone, answered then as
    /// [`Out::Leg`]. One that asks nothing is admitted now; a reserve its holding can't say refuses
    /// it, saying why. `None` while its reserve waits.
    pub(super) fn leg_start(&mut self, ask: Ask, now: i64, out: &mut Vec<Out>) -> Option<Result<(), String>> {
        let key = (CI.to_owned(), self.epoch, ask.run.clone());
        let asked = match self.keeper.as_mut() {
            Some(h) => {
                // Asked again while its reserve waits: answered with it.
                if h.asking.get(&key).copied().and_then(|id| h.pending.get(&id)).is_some_and(|p| matches!(p.begin, Begin::Leg(_))) {
                    return None;
                }
                Some(h.holding.reserve(ask::amounts(&ask.needs.request), now))
            }
            None => None,
        };
        match asked {
            Some(Err(why)) => Some(Err(why.why().to_owned())),
            Some(Ok(Some((id, word)))) => {
                self.reserving(id, word, Pending { run: key, begin: Begin::Leg(Box::new(ask)), then: Vec::new() }, out);
                None
            }
            reserved => {
                let (verdict, outs) = self.leg_now(ask, now);
                out.extend(outs);
                if reserved.is_some() {
                    self.unreserved(key, verdict.is_ok().then_some(Start::New));
                }
                Some(verdict)
            }
        }
    }

    /// Run `run` of owner `owner` (`epoch`) ended here: what it held through its machine's keeper
    /// goes back (the holder's item 4), said after its record, as the node publishes nothing before
    /// what it came with is kept (LB03). A run that reserved nothing gives nothing back.
    pub(super) fn keeper_release(&mut self, owner: &str, epoch: u64, run: &str, now: i64) -> Vec<Out> {
        let Some(h) = self.keeper.as_mut() else { return Vec::new() };
        let Some(BoxName::Reserved(id)) = h.runs.remove(&(owner.to_owned(), epoch, run.to_owned())) else { return Vec::new() };
        let out = h.holding.release(id, now);
        self.keeper_out(out, now)
    }

    /// Whether sense's health bars `ask`'s kind of work here now, by its last measure (L2): its
    /// kernel start is declined as ever, nothing reserved.
    fn kernel_barred(&self, ask: &Ask) -> bool {
        self.strained.is_none() && self.last.as_ref().is_some_and(|m| super::barred(ask, self.key_here(ask), m).is_some())
    }

    /// Reserve `id` said for `pending`, which waits for its answer: its word goes on the link it
    /// was made for.
    fn reserving(&mut self, id: ReservationId, word: hex::ToKeeper, pending: Pending, out: &mut Vec<Out>) {
        let Some(h) = self.keeper.as_mut() else { return };
        out.push(Out::KeeperSay { link: h.link, word });
        h.asking.insert(pending.run.clone(), id);
        h.pending.insert(id, pending);
    }

    /// A start that asked nothing, so reserved nothing (Hex's holder and keeper take no empty
    /// amount): its box numbered once it started, its cold start (docs/lb.md).
    fn unreserved(&mut self, key: RunKey, started: Option<Start>) {
        if started == Some(Start::New)
            && let Some(h) = self.keeper.as_mut()
        {
            let named = h.holding.unreserved();
            h.runs.insert(key, named);
        }
    }

    /// What its holding said, as placement's: its words, on the link they were made for, then each
    /// event in order, where it stands to its ledger first (`Admission::keeper_said`: a refusal's
    /// view before the refusal), each reserve granted or not, each sync's counts.
    fn keeper_out(&mut self, held: kiln_ci::place::holder::Out, now: i64) -> Vec<Out> {
        let link = self.keeper_link();
        let mut out: Vec<Out> = held.send.into_iter().map(|word| Out::KeeperSay { link, word }).collect();
        for event in held.events {
            match event {
                Event::Keeper(stands) => {
                    let mut said = Vec::new();
                    self.admission.keeper_said(stands, now, &mut said);
                    out.extend(self.said(said, now));
                }
                Event::Granted(id) => self.granted(id, now, &mut out),
                Event::NotGranted(id, why) => self.not_granted(id, why, now, &mut out),
                Event::Synced { agree: true, released, adopted, disagreed } => tracing::debug!(released, adopted, disagreed, "synced with this machine's capacity keeper"),
                Event::Synced { agree: false, released, adopted, disagreed } => {
                    tracing::warn!(released, adopted, disagreed, "this runner's sync with its machine's capacity keeper disagreed with what it holds: the keeper's books are the machine's");
                }
            }
        }
        out
    }

    /// Reservation `id` granted: its start goes ahead now, never sooner (the holder's item 1). The
    /// ledger starts it as it would have at once (under its claim, as its kernel's start, as a
    /// leg), and where it started its reservation is kept with its record, before its process
    /// starts; where it didn't (it ran here, its claim lapsed meanwhile, no room is left here for
    /// it), its room goes back to the keeper at once. A grant no start here waits for goes back too.
    fn granted(&mut self, id: ReservationId, now: i64, out: &mut Vec<Out>) {
        let Some(h) = self.keeper.as_mut() else { return };
        let Some(Pending { run: key, begin, then }) = h.pending.remove(&id) else {
            let back = h.holding.release(id, now);
            out.extend(self.keeper_out(back, now));
            return;
        };
        h.asking.remove(&key);
        let (owner, epoch, run) = key.clone();
        let from = Peer { name: &owner, epoch };
        let (mut said, mut told) = (Vec::new(), Vec::new());
        let (started, verdict, leg) = match begin {
            Begin::Claim(fence) => {
                let started = self.admission.start(from, &run, fence, now, &mut said);
                (started, claim_verdict(&run, started), false)
            }
            Begin::Kernel { ask, kernel, at } => {
                let started = self.fast(&owner, epoch, ask, kernel, at, now, &mut said);
                (started, kernel_verdict(&run, started), false)
            }
            Begin::Leg(ask) => {
                let (verdict, outs) = self.leg_now(*ask, now);
                out.extend(outs);
                (verdict.is_ok().then_some(Start::New), verdict, true)
            }
        };
        if started == Some(Start::New) {
            self.admission.reserved(from, &run, (id.epoch, id.seq));
            if let Some(h) = self.keeper.as_mut() {
                h.runs.insert(key, BoxName::Reserved(id));
            }
        }
        self.thens(then, &run, started, &verdict, &mut said, &mut told);
        out.extend(self.said(said, now));
        if started != Some(Start::New)
            && let Some(h) = self.keeper.as_mut()
        {
            let back = h.holding.release(id, now);
            out.extend(self.keeper_out(back, now));
        }
        if leg {
            out.push(Out::Leg { run, verdict });
        }
        out.extend(told);
    }

    /// Reservation `id` not granted (`why`): its start doesn't happen (the holder's item 5). A
    /// claim lapses, its run waiting here again on the room there is (the refusal's view said
    /// first) and its owner told why; a kernel start is declined for room in this runner's word, so
    /// its owner's kernel picks again; a leg is refused, saying why.
    fn not_granted(&mut self, id: ReservationId, why: NotGranted, now: i64, out: &mut Vec<Out>) {
        let Some(h) = self.keeper.as_mut() else { return };
        let Some(Pending { run: key, begin, then }) = h.pending.remove(&id) else { return };
        h.asking.remove(&key);
        let (owner, epoch, run) = key;
        let from = Peer { name: &owner, epoch };
        let (mut said, mut told) = (Vec::new(), Vec::new());
        match begin {
            Begin::Claim(fence) => self.admission.not_granted(from, &run, fence, why, now, &mut said),
            Begin::Kernel { kernel, .. } => self.admission.fast_not_granted(from, &run, kernel, now, &mut said),
            Begin::Leg(_) => told.push(Out::Leg { run: run.clone(), verdict: Err(why.why().to_owned()) }),
        }
        self.thens(then, &run, None, &Err(format!("{run} isn't started: {}", why.why())), &mut said, &mut told);
        out.extend(self.said(said, now));
        out.extend(told);
    }

    /// What waited on a start's outcome, told it: a spec its verdict, this node's own run whether it
    /// started, an agent session whether it started (told to its owner as spawned where it did).
    fn thens(&mut self, then: Vec<Then>, run: &str, started: Option<Start>, verdict: &Result<(), String>, said: &mut Vec<(String, ToOwner)>, out: &mut Vec<Out>) {
        for t in then {
            match t {
                Then::Spec { peer, id } => {
                    if let Some((_, _, decided)) = self.specs.get_mut(&(peer.clone(), id)) {
                        *decided = Some(verdict.clone());
                    }
                    out.push(Out::SpecReady { peer, id });
                }
                Then::Local => out.push(Out::Started { run: run.to_owned(), ok: started == Some(Start::New) }),
                Then::Session(claim) => {
                    let ok = self.session_began(&claim, started, said);
                    out.push(Out::Session { claim, ok });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
