//! `Capacity`: a machine's capacity keeper, as kiln's runner holds room through it (Hex's
//! `docs/design/capacity-view.md`; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 13). The runner
//! opens links to the keeper, each a connection to its port: the holder's words go on it in order,
//! and the keeper's answers come back in that order, with the hints it owes the link between them
//! (CV9). The words are Hex's (`hex_api::capacity::{ToKeeper, ToHolder}`), and the order of a
//! holder's words is Hex's holder core's, which kiln-ci's `place::holder` drives.
//!
//! Hex's keeper on its node is the real one, reached over QUIC on its `CAPACITY_PORT` (kiln's
//! `capacity` module, through `hex_quic`). [`SimCapacity`] is Hex's own keeper over records in memory
//! (`hex_api::capacity::sim::SimView`) behind links in memory, so the same keeper's rules answer
//! both. Its bounds are its keeper's (CV8): two links for each holder it serves (one, and the one
//! replacing it: Hex's hex-quic.md Q8), and on each, the answers to as many words as one holder may
//! leave unheard (for each reservation it may hold a reserve and two releases, one for a reserve
//! given up and one for a grant that came after, and its hello and its sync), the hints between them
//! coalesced; past either, it refuses `Full`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use futures::future::BoxFuture;
use hex_api::capacity::sim::SimView;
use hex_api::capacity::{Budget, CapacityView, Host};
use parking_lot::Mutex;

use crate::conformance::{Case, Failure, ensure, fine, same};

pub use hex_api::capacity::{Amounts, Close, Fence, Held, ReservationId, ToHolder, ToKeeper, View, Why};

/// Why a link wasn't opened, or a word wasn't said or heard.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// The link ended, and why: its transport found the keeper gone or the connection failed, or
    /// (`reset`) the keeper started again or reset it, every word said on it since its last answer
    /// unanswered. What came on it before is heard first.
    #[error("the link to the capacity keeper ended{}: {why}", if *reset { ", reset" } else { "" })]
    Ended { reset: bool, why: String },
    /// No link was opened, and why: the keeper unreachable, its certificate not for the name the
    /// runner's configuration gives it, a suite other than `TLS13_AES_256_GCM_SHA384` negotiated, or
    /// this holder not enrolled.
    #[error("no link to the capacity keeper: {0}")]
    Unreachable(String),
    /// As many links, or as many words unheard on one, as it holds.
    #[error("the capacity keeper holds its most ({limit})")]
    Full { limit: usize },
}

/// One link to a machine's capacity keeper (module docs). Dropped, it ends, and the keeper hears
/// its holder is away: what that holds stays held (CV5).
pub trait Link: Send + Sync {
    /// Says `msg` on the link, after every word said before it; refused `Ended` once the link has
    /// ended.
    fn say(&self, msg: ToKeeper) -> BoxFuture<'_, Result<(), Refusal>>;
    /// The keeper's next word on the link: the answer to its oldest word unanswered, or a hint
    /// owed it; `Ended`, and why, once the link has ended and every word that came is heard.
    fn hear(&self) -> BoxFuture<'_, Result<ToHolder, Refusal>>;
}

/// A machine's capacity keeper.
pub trait Capacity: Send + Sync + 'static {
    /// Opens a link to the keeper: a connection of its own, its sessions good on it alone (CV4).
    fn link(&self) -> BoxFuture<'_, Result<Box<dyn Link>, Refusal>>;
}

// ---------------------------------------------------------------------------------- simulator

/// Hex's keeper over records in memory, behind links in memory (module docs). Clones share it.
#[derive(Clone)]
pub struct SimCapacity {
    inner: Arc<SimInner>,
}

struct SimInner {
    state: Mutex<SimState>,
    changed: event_listener::Event,
}

struct SimState {
    keeper: SimView,
    budget: Budget,
    links: BTreeMap<u64, Owed>,
    next: u64,
}

/// What a link owes its holder: the words that came, in order, how many of them are answers, and
/// its end, once it ended.
#[derive(Default)]
struct Owed {
    words: VecDeque<ToHolder>,
    answers: usize,
    ended: Option<Refusal>,
}

/// The links a keeper of `budget` holds at once: two for each holder it serves (Q8).
fn links_most(budget: &Budget) -> usize {
    budget.holders.saturating_mul(2)
}

/// The answers one link holds unheard: as many as one holder of `budget` may leave unheard.
fn answers_most(budget: &Budget) -> usize {
    budget.reservations.saturating_mul(3).saturating_add(2)
}

impl SimState {
    /// The hints the keeper owes, each on its link, a hint behind an unheard hint replacing it (the
    /// newest view: CV8).
    fn hint(&mut self) -> Result<(), Refusal> {
        let hints = self.keeper.hints().map_err(|e| Refusal::Unreachable(e.to_string()))?;
        for (link, hint) in hints {
            let Some(owed) = self.links.get_mut(&link) else { continue };
            if owed.ended.is_some() {
                continue;
            }
            if let Some(last @ ToHolder::Hint { .. }) = owed.words.back_mut() {
                *last = hint;
            } else {
                owed.words.push_back(hint);
            }
        }
        Ok(())
    }
}

impl SimCapacity {
    /// A machine of `room` whose keeper keeps `budget` (CV8), nothing held.
    pub fn new(budget: Budget, room: Amounts) -> Result<Self, Refusal> {
        let keeper = SimView::new(budget, room).map_err(|e| Refusal::Unreachable(e.to_string()))?;
        let state = SimState { keeper, budget, links: BTreeMap::new(), next: 0 };
        Ok(Self { inner: Arc::new(SimInner { state: Mutex::new(state), changed: event_listener::Event::new() }) })
    }

    /// The keeper's process starts again from its records: every link ends, reset, once what came
    /// on it is heard.
    pub fn restart(&self) -> Result<(), Refusal> {
        let mut state = self.inner.state.lock();
        state.keeper.restart().map_err(|e| Refusal::Unreachable(e.to_string()))?;
        for owed in state.links.values_mut() {
            owed.ended.get_or_insert(Refusal::Ended { reset: true, why: "the capacity keeper started again".into() });
        }
        drop(state);
        self.inner.changed.notify(usize::MAX);
        Ok(())
    }

    /// The machine's host acting on its keeper (Hex's `Host`: its own admissions, its room measured
    /// anew), and the hints that owes said on their links.
    pub fn host<T>(&self, act: impl FnOnce(&mut SimView) -> T) -> Result<T, Refusal> {
        let mut state = self.inner.state.lock();
        let out = act(&mut state.keeper);
        state.hint()?;
        drop(state);
        self.inner.changed.notify(usize::MAX);
        Ok(out)
    }
}

impl Capacity for SimCapacity {
    fn link(&self) -> BoxFuture<'_, Result<Box<dyn Link>, Refusal>> {
        let out = (|| {
            let mut state = self.inner.state.lock();
            let most = links_most(&state.budget);
            if state.links.len() >= most {
                return Err(Refusal::Full { limit: most });
            }
            state.next = state.next.saturating_add(1);
            let id = state.next;
            state.links.insert(id, Owed::default());
            Ok(Box::new(SimLink { id, inner: Arc::clone(&self.inner) }) as Box<dyn Link>)
        })();
        Box::pin(async move { out })
    }
}

/// A link of a [`SimCapacity`].
struct SimLink {
    id: u64,
    inner: Arc<SimInner>,
}

fn gone() -> Refusal {
    Refusal::Ended { reset: false, why: "the link was let go".into() }
}

impl Link for SimLink {
    fn say(&self, msg: ToKeeper) -> BoxFuture<'_, Result<(), Refusal>> {
        let out = (|| {
            let mut guard = self.inner.state.lock();
            let state = &mut *guard;
            let most = answers_most(&state.budget);
            let owed = state.links.get(&self.id).ok_or_else(gone)?;
            if let Some(why) = &owed.ended {
                return Err(why.clone());
            }
            if owed.answers >= most {
                return Err(Refusal::Full { limit: most });
            }
            let answer = state.keeper.hear(self.id, msg).map_err(|e| Refusal::Ended { reset: true, why: e.to_string() })?;
            let owed = state.links.get_mut(&self.id).ok_or_else(gone)?;
            owed.words.push_back(answer);
            owed.answers = owed.answers.saturating_add(1);
            state.hint()
        })();
        self.inner.changed.notify(usize::MAX);
        Box::pin(async move { out })
    }

    fn hear(&self) -> BoxFuture<'_, Result<ToHolder, Refusal>> {
        Box::pin(async move {
            loop {
                let listener = self.inner.changed.listen();
                {
                    let mut state = self.inner.state.lock();
                    let owed = state.links.get_mut(&self.id).ok_or_else(gone)?;
                    if let Some(word) = owed.words.pop_front() {
                        if !matches!(word, ToHolder::Hint { .. }) {
                            owed.answers = owed.answers.saturating_sub(1);
                        }
                        return Ok(word);
                    }
                    if let Some(why) = &owed.ended {
                        return Err(why.clone());
                    }
                }
                listener.await;
            }
        })
    }
}

impl Drop for SimLink {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock();
        state.links.remove(&self.id);
        // Its holder is away, and keeps what it holds (CV5); a keeper that stopped has no link.
        let _ = state.keeper.link_down(self.id);
        let _ = state.hint();
        drop(state);
        self.inner.changed.notify(usize::MAX);
    }
}

// -------------------------------------------------------------------------------- conformance

/// What a `Capacity` conformance suite runs against: a fresh machine per case, its room Hex's
/// conformance room (`hex_api::capacity::conformance::ROOM`: 8 CPUs, 16 GiB of memory and of disk,
/// a GPU), nothing held, its keeper serving two holders, `conformance-a` and `conformance-b`, and
/// the keeper as each of them reaches it: a real link authenticates one holder alone (CV8, CV9), so
/// each has its own.
pub trait CapacityFixture: Send + Sync {
    fn capacity(&self, holder: &str) -> Result<&dyn Capacity, Failure>;
}

/// The fixture for a [`SimCapacity`]: its keeper serves the two holders, and as many reservations
/// for each as these cases hold.
pub struct SimCapacityFixture(Result<SimCapacity, Refusal>);

impl Default for SimCapacityFixture {
    fn default() -> Self {
        let budget = Budget { holders: 2, reservations: 8, name_bytes: NAME_BYTES };
        Self(SimCapacity::new(budget, hex_api::capacity::conformance::ROOM))
    }
}

impl CapacityFixture for SimCapacityFixture {
    /// The one keeper, which every holder's links reach alike.
    fn capacity(&self, _holder: &str) -> Result<&dyn Capacity, Failure> {
        self.0.as_ref().map(|c| c as &dyn Capacity).map_err(|e| format!("the simulator didn't start: {e}"))
    }
}

/// A holder's name at its longest: a DNS name's 255 octets (RFC 1035 §2.3.4), as its certificate
/// carries it.
const NAME_BYTES: usize = 255;

const A: &str = "conformance-a";
const B: &str = "conformance-b";

fn hello(name: &str, epoch: u64) -> ToKeeper {
    ToKeeper::Hello { holder: name.to_owned(), epoch, scope: None }
}

fn cpus(n: u64) -> Amounts {
    Amounts { cpu_milli: n.saturating_mul(1000), ..Amounts::ZERO }
}

/// Says `msg` on `link` and hears the next word.
async fn ask(link: &dyn Link, msg: ToKeeper) -> Result<ToHolder, Failure> {
    fine("say", link.say(msg).await)?;
    fine("hear", link.hear().await)
}

/// A link of `name` under `epoch`, welcomed and synced with nothing listed: the link, its fence and
/// the view it was welcomed with.
async fn synced(c: &dyn Capacity, name: &str, epoch: u64) -> Result<(Box<dyn Link>, Fence, View), Failure> {
    let link = fine("link", c.link().await)?;
    let (fence, view) = match ask(&*link, hello(name, epoch)).await? {
        ToHolder::Welcome { fence, view } => (fence, view),
        other => return Err(format!("{name}'s hello: {other:?}")),
    };
    match ask(&*link, ToKeeper::Sync { fence, held: Vec::new() }).await? {
        ToHolder::Synced { .. } => Ok((link, fence, view)),
        other => Err(format!("{name}'s sync: {other:?}")),
    }
}

fn c1<F: CapacityFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let link = fine("link", f.capacity(A)?.link().await)?;
        match ask(&*link, hello(A, 1)).await? {
            ToHolder::Welcome { fence, view } => {
                same("its fence's holder epoch", fence.holder, 1)?;
                same("the view's room", view.room, hex_api::capacity::conformance::ROOM)?;
                same("what's held", view.held, Amounts::ZERO)
            }
            other => Err(format!("a hello on a new link: {other:?}")),
        }
    })
}

fn c2<F: CapacityFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let link = fine("link", f.capacity(A)?.link().await)?;
        let fence = match ask(&*link, hello(A, 1)).await? {
            ToHolder::Welcome { fence, .. } => fence,
            other => return Err(format!("its hello: {other:?}")),
        };
        let id = ReservationId { epoch: 1, seq: 0 };
        fine("say its sync", link.say(ToKeeper::Sync { fence, held: Vec::new() }).await)?;
        fine("say a reserve", link.say(ToKeeper::Reserve { fence, id, amount: cpus(1) }).await)?;
        fine("say its release", link.say(ToKeeper::Release { fence, id }).await)?;
        let heard = [fine("hear", link.hear().await)?, fine("hear", link.hear().await)?, fine("hear", link.hear().await)?];
        ensure(
            matches!(&heard, [ToHolder::Synced { .. }, ToHolder::Granted { id: g, .. }, ToHolder::Released { id: r, .. }] if *g == id && *r == id),
            || format!("answered out of the order said: {heard:?}"),
        )
    })
}

fn c3<F: CapacityFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = f.capacity(A)?;
        let id = ReservationId { epoch: 1, seq: 0 };
        let (link, fence, _) = synced(c, A, 1).await?;
        match ask(&*link, ToKeeper::Reserve { fence, id, amount: cpus(2) }).await? {
            ToHolder::Granted { .. } => {}
            other => return Err(format!("a reserve that fits: {other:?}")),
        }
        drop(link);
        let again = fine("a second link", c.link().await)?;
        let fence = match ask(&*again, hello(A, 1)).await? {
            ToHolder::Welcome { fence, view } => {
                same("held once its link ended", view.held, cpus(2))?;
                fence
            }
            other => return Err(format!("its hello again: {other:?}")),
        };
        match ask(&*again, ToKeeper::Sync { fence, held: vec![Held { id, amount: cpus(2) }] }).await? {
            ToHolder::Synced { count, sum, released, adopted, disagreed, .. } => same("the sync's counts", (count, sum, released, adopted, disagreed), (1, cpus(2), 0, 0, 0)),
            other => Err(format!("its sync again: {other:?}")),
        }
    })
}

fn c4<F: CapacityFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let (a, a_fence, _) = synced(f.capacity(A)?, A, 1).await?;
        let (b, _, _) = synced(f.capacity(B)?, B, 1).await?;
        let id = ReservationId { epoch: 1, seq: 0 };
        match ask(&*b, ToKeeper::Reserve { fence: a_fence, id, amount: cpus(1) }).await? {
            ToHolder::Closed { why: Close::Session, .. } => {}
            other => return Err(format!("a word under another link's session: {other:?}")),
        }
        match ask(&*a, ToKeeper::Reserve { fence: a_fence, id, amount: cpus(1) }).await? {
            ToHolder::Granted { view, .. } => same("held, once its own link reserved", view.held, cpus(1)),
            other => Err(format!("its own reserve after another link's word: {other:?}")),
        }
    })
}

fn c5<F: CapacityFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let (link, fence, view) = synced(f.capacity(A)?, A, 1).await?;
        let past = Amounts { cpu_milli: view.room.cpu_milli.saturating_add(1), ..Amounts::ZERO };
        match ask(&*link, ToKeeper::Reserve { fence, id: ReservationId { epoch: 1, seq: 0 }, amount: past }).await? {
            ToHolder::Refused { why: Why::Room, view, .. } => same("the refusal's view: its room", view.room, hex_api::capacity::conformance::ROOM)?,
            other => return Err(format!("a reserve past the room: {other:?}")),
        }
        match ask(&*link, ToKeeper::Reserve { fence, id: ReservationId { epoch: 1, seq: 1 }, amount: Amounts { gpus: 1, ..cpus(1) } }).await? {
            ToHolder::Refused { why: Why::Devices, .. } => Ok(()),
            other => Err(format!("a GPU by count: {other:?}")),
        }
    })
}

fn c6<F: CapacityFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let (a, _, _) = synced(f.capacity(A)?, A, 1).await?;
        let (b, b_fence, _) = synced(f.capacity(B)?, B, 1).await?;
        match ask(&*b, ToKeeper::Reserve { fence: b_fence, id: ReservationId { epoch: 1, seq: 0 }, amount: cpus(3) }).await? {
            ToHolder::Granted { .. } => {}
            other => return Err(format!("another holder's reserve: {other:?}")),
        }
        match fine("hear", a.hear().await)? {
            ToHolder::Hint { view, .. } => same("the hint's view", view.held, cpus(3)),
            other => Err(format!("a synced link, after another holder reserved: {other:?}")),
        }
    })
}

fn c7<F: CapacityFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = f.capacity(A)?;
        let newer = fine("link", c.link().await)?;
        match ask(&*newer, hello(A, 2)).await? {
            ToHolder::Welcome { .. } => {}
            other => return Err(format!("its newer process's hello: {other:?}")),
        }
        let older = fine("link", c.link().await)?;
        match ask(&*older, hello(A, 1)).await? {
            ToHolder::Closed { rejected: None, why: Close::OlderEpoch { current } } => same("the newest epoch", current, 2),
            other => Err(format!("an older process's hello: {other:?}")),
        }
    })
}

/// The `Capacity` contract, clause by clause (docs/seams.md, "Capacity").
pub fn cases<F: CapacityFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "CAPACITY-1", says: "a hello on a new link is welcomed under its epoch, with the machine's room and what is held there", check: c1::<F> },
        Case { id: "CAPACITY-2", says: "a link's words are answered on it in the order said: a sync, a reserve and a release said together are synced, granted and released in turn", check: c2::<F> },
        Case { id: "CAPACITY-3", says: "what a holder holds outlives its link: the next link is welcomed with it held, and its sync listing it is answered agreeing", check: c3::<F> },
        Case { id: "CAPACITY-4", says: "a word under another link's session is closed Session, and the other link's session goes on", check: c4::<F> },
        Case { id: "CAPACITY-5", says: "a reserve past the room is refused Room with the view, and one asking a GPU by count Devices", check: c5::<F> },
        Case { id: "CAPACITY-6", says: "another holder's reserve owes a synced link a hint with the view as it is", check: c6::<F> },
        Case { id: "CAPACITY-7", says: "a hello under an older epoch than its holder's newest is closed OlderEpoch, naming the newest", check: c7::<F> },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run;

    /// Step 13, CP-M2: the simulator passes the Capacity suite.
    #[test]
    fn the_simulated_keeper_passes_the_capacity_suite() -> Result<(), String> {
        futures::executor::block_on(run("Capacity", &cases::<SimCapacityFixture>(), || async { SimCapacityFixture::default() })).into_result().map(|_| ())
    }

    /// Step 13 ("the keeper conformance suite passes"): the keeper the simulator holds passes Hex's
    /// own suite (`hex_api::capacity::conformance`, HEX-CV-1 on), under the budget Hex's own run of
    /// it keeps (its `capacity::tests`).
    #[test]
    fn the_keeper_it_holds_passes_hex_s_own_suite() -> Result<(), String> {
        let budget = Budget { holders: 2, reservations: 8, name_bytes: 32 };
        let failures = hex_api::capacity::conformance::view(&mut || Ok(Box::new(SimView::new(budget, hex_api::capacity::conformance::ROOM)?) as Box<dyn Host>));
        if failures.is_empty() { Ok(()) } else { Err(format!("{failures:?}")) }
    }

    /// The simulator's own rules: a restart ends every link reset, once what came on it is heard; a
    /// link let go is away to the keeper, and keeps what it holds; its bounds refuse `Full`.
    #[test]
    fn a_restart_ends_its_links_reset_and_its_bounds_refuse_full() -> Result<(), String> {
        futures::executor::block_on(async {
            let c = SimCapacity::new(Budget { holders: 1, reservations: 1, name_bytes: NAME_BYTES }, hex_api::capacity::conformance::ROOM).map_err(|e| e.to_string())?;
            let (a, fence, _) = synced(&c, A, 1).await?;
            fine("say", a.say(ToKeeper::Reserve { fence, id: ReservationId { epoch: 1, seq: 0 }, amount: cpus(1) }).await)?;
            c.restart().map_err(|e| e.to_string())?;
            ensure(matches!(a.hear().await, Ok(ToHolder::Granted { .. })), || "what came before the restart is heard first".into())?;
            ensure(matches!(a.hear().await, Err(Refusal::Ended { reset: true, .. })), || "then its end, reset".into())?;
            ensure(matches!(a.say(hello(A, 1)).await, Err(Refusal::Ended { reset: true, .. })), || "nothing more is said on it".into())?;
            let second = fine("a second link", c.link().await)?;
            ensure(matches!(c.link().await, Err(Refusal::Full { limit: 2 })), || "two links a holder".into())?;
            drop(second);
            drop(a);
            let (b, fence, view) = synced(&c, A, 1).await?;
            same("held through the restart and its links' ends", view.held, cpus(1))?;
            let id = ReservationId { epoch: 1, seq: 9 };
            let mut said = 0;
            while b.say(ToKeeper::Release { fence, id }).await.is_ok() {
                said += 1;
            }
            same("words unheard on one link", said, 5)
        })
    }
}
