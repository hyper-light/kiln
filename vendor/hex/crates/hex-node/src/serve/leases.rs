//! A node's credential leases over its wire (joint plan step 33; docs/design/credential-grants.md C7): the lessors of a
//! box's runs are its creator's, asked over the connection that created it. A connection that says it leases
//! ([`hex_core::wire::Call::Lessors`]) answers the node's asks for the boxes it creates: a run's lessor as the run boots
//! ([`hex_core::wire::Answer::LessorAsk`]), each of its leases ([`hex_core::wire::Answer::LeaseAsk`]), and it hears each
//! lease given back and each run's end. A box created on a connection that doesn't lease has no lessor here (default
//! deny), and a box granting a credential is refused there as it is created, before any record.
//!
//! [`WireLessors`] is the node's [`Lessors`]: what the node's runtime is built with (`hex node serve`), its routes the
//! socket's, which it keeps as each create is answered. Every wait here follows the connection asked (decision 9): an
//! answer, or the connection's end, ends it, however long that takes; none is bounded by a time of its own.
//!
//! A value is held here only as long as it is handed on: from the message that carried it, read into a
//! [`hex_api::lease::Value`], to the run's lessor's caller (Shards' run host). Nothing of a lease is written: the one
//! thing kept of a box is which connection created it, in memory.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

use hex_api::lease::{Lease, LeaseRefused, Lessor, Lessors, SignInRefused, SignInTokens, Value};
use hex_api::{BoxId, Refusal};
use hex_core::wire::Answer;

/// The most boxes the node keeps a route for: one for each box its runtime may hold (`hex_runtime::MAX_BOXES`), since a
/// route lives no longer than its box is known to the connection that created it.
pub const MAX_ROUTES: usize = hex_runtime::MAX_BOXES;

/// What an ask of a connection's orchestrator is answered with: a run's lessor made or refused, a lease or its
/// refusal, or the connection's end.
pub(crate) enum Reply {
    Lessor(Result<(), Refusal>),
    Leased(Result<Lease, LeaseRefused>),
    /// A sign-in's capture (C8): the placeholders its CLI holds, or why not.
    Captured(Result<hex_api::Captured, SignInRefused>),
    /// A sign-in's refresh (C8): the token response its CLI is to have, or why not.
    Refreshed(Result<Value, SignInRefused>),
    Ended,
}

/// One ask's place for its answer, filled once: by the connection's reader as the answer comes, or by its close.
pub(crate) struct Slot {
    reply: hex_wait::Mutex<Option<Reply>>,
    came: hex_wait::Condvar,
}

impl Slot {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            reply: hex_wait::Mutex::new(None),
            came: hex_wait::Condvar::new(),
        })
    }

    /// Its answer has come: the first one counts.
    pub(crate) fn fill(&self, reply: Reply) {
        if let Ok(mut held) = self.reply.lock() {
            if held.is_none() {
                *held = Some(reply);
            }
            self.came.notify_all();
        }
    }

    /// Its answer, waited for while the connection stands: the party is the connection, whose answer or end fills it.
    fn wait(&self) -> Reply {
        let Ok(mut held) = self.reply.lock() else {
            return Reply::Ended;
        };
        loop {
            if let Some(reply) = held.take() {
                return reply;
            }
            held = match self.came.wait(held) {
                Ok(held) => held,
                Err(_) => return Reply::Ended,
            };
        }
    }
}

/// A connection a box's leases are asked on: the one that created it.
pub(crate) trait Line: Send + Sync {
    /// A fresh ask's id on this connection and its slot, made before the ask goes, so its answer can't come first;
    /// refused past the asks the connection may have waiting, or once it has ended.
    fn ask(&self) -> Result<(u64, Arc<Slot>), Refusal>;
    /// `answer` sent unasked, its header's id `id`; nothing once the connection has ended.
    fn tell(&self, id: u64, answer: &Answer) -> Result<(), Refusal>;
    /// Ask `id`'s slot let go: its ask couldn't go.
    fn forget(&self, id: u64);
}

/// A box's route: the connection that created it, by its token, and its line.
struct Route {
    conn: u64,
    line: Weak<dyn Line>,
}

/// The node's lessors over its wire: each box's runs lease from the connection that created it, where that connection
/// leases.
pub struct WireLessors {
    routes: Mutex<BTreeMap<BoxId, Route>>,
}

impl WireLessors {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            routes: Mutex::new(BTreeMap::new()),
        })
    }

    /// Room for box `id`'s route, checked before the box is made: past [`MAX_ROUTES`] a create granting a credential is
    /// refused by type. Each route is a box's the node holds, so the runtime's own bound holds them first.
    pub(crate) fn room(&self, id: &BoxId) -> Result<(), Refusal> {
        let routes = self.routes.lock().map_err(poisoned)?;
        if !routes.contains_key(id) && routes.len() >= MAX_ROUTES {
            return Err(Refusal::Exhausted {
                what: "boxes whose credentials a connection leases",
                bound: MAX_ROUTES,
            });
        }
        Ok(())
    }

    /// Box `id` was made on connection `conn`: its runs lease over `line` where it is given (the box grants a credential
    /// and the connection leases), else from no one. A box's route is its latest create's, kept before that create is
    /// answered, so no run of an earlier box of the same id leases through it.
    pub(crate) fn route(&self, id: &BoxId, conn: u64, line: Option<Weak<dyn Line>>) {
        let Ok(mut routes) = self.routes.lock() else {
            return;
        };
        match (line, id.try_clone()) {
            (Some(line), Ok(id)) => {
                routes.insert(id, Route { conn, line });
            }
            // No line, or no room to name it: the box leases from no one (default deny).
            _ => {
                routes.remove(id);
            }
        }
    }

    /// Box `id` was deleted: its route goes with it.
    pub(crate) fn deleted(&self, id: &BoxId) {
        if let Ok(mut routes) = self.routes.lock() {
            routes.remove(id);
        }
    }

    /// Connection `conn` has ended: the routes of the boxes it created go, so their runs lease from no one.
    pub(crate) fn closed(&self, conn: u64) {
        if let Ok(mut routes) = self.routes.lock() {
            routes.retain(|_, route| route.conn != conn);
        }
    }

    /// Whether box `id`'s runs lease here: a route to the connection that created it.
    pub fn routed(&self, id: &BoxId) -> bool {
        self.routes.lock().is_ok_and(|routes| {
            routes
                .get(id)
                .is_some_and(|route| route.line.strong_count() > 0)
        })
    }
}

impl Lessors for WireLessors {
    /// Box `id`'s run's lessor, asked of the connection that created it, waiting for its answer while it stands.
    fn lessor(&self, id: &BoxId) -> Result<Arc<dyn Lessor>, Refusal> {
        let line = self
            .routes
            .lock()
            .map_err(poisoned)?
            .get(id)
            .map(|route| Weak::clone(&route.line));
        let Some(line) = line.as_ref().and_then(Weak::upgrade) else {
            return Err(unrouted(id));
        };
        let (run, slot) = line.ask()?;
        if let Err(why) = line.tell(run, &Answer::LessorAsk(id.try_clone()?)) {
            line.forget(run);
            return Err(why);
        }
        match slot.wait() {
            Reply::Lessor(Ok(())) => Ok(Arc::new(WireLessor {
                line: Arc::downgrade(&line),
                run,
                id: id.try_clone()?,
            })),
            Reply::Lessor(Err(why)) => Err(why),
            Reply::Ended => Err(unrouted(id)),
            Reply::Leased(_) | Reply::Captured(_) | Reply::Refreshed(_) => Err(Refusal::Broken(
                format!("box {id}'s orchestrator answered its lessor's ask with another's answer"),
            )),
        }
    }
}

/// One run's lessor over the connection that created its box: each lease asked of it, each give-back and the run's end
/// told to it.
struct WireLessor {
    line: Weak<dyn Line>,
    /// The run's id on the connection: its lessor's ask's.
    run: u64,
    id: BoxId,
}

impl Lessor for WireLessor {
    fn lease(&self, reference: &str, key: Option<[u8; 32]>) -> Result<Lease, LeaseRefused> {
        let Some(line) = self.line.upgrade() else {
            return Err(ended(&self.id));
        };
        let (ask, slot) = line
            .ask()
            .map_err(|why| LeaseRefused::Unavailable(why.to_string()))?;
        let asked = Answer::LeaseAsk {
            run: self.run,
            reference: reference.to_owned(),
            key,
        };
        if let Err(why) = line.tell(ask, &asked) {
            line.forget(ask);
            return Err(LeaseRefused::Unavailable(why.to_string()));
        }
        drop(line);
        match slot.wait() {
            Reply::Leased(answer) => answer,
            Reply::Ended => Err(ended(&self.id)),
            Reply::Lessor(_) | Reply::Captured(_) | Reply::Refreshed(_) => {
                Err(LeaseRefused::Unavailable(format!(
                    "box {}'s orchestrator answered a lease ask with another's answer",
                    self.id
                )))
            }
        }
    }

    /// Sign-in `sign_in`'s tokens handed to the orchestrator over the connection that created the box (C8), each a value
    /// wiped as it drops, and its placeholders waited for while the connection stands.
    fn capture(
        &self,
        sign_in: &str,
        tokens: SignInTokens,
        key: Option<[u8; 32]>,
    ) -> Result<hex_api::Captured, SignInRefused> {
        let asked = Answer::CaptureAsk {
            run: self.run,
            sign_in: sign_in.to_owned(),
            tokens,
            key,
        };
        match self.ask_sign_in(asked)? {
            Reply::Captured(answer) => answer,
            _ => Err(SignInRefused::Unavailable(format!(
                "box {}'s orchestrator answered a capture with another's answer",
                self.id
            ))),
        }
    }

    fn refresh(&self, sign_in: &str, key: Option<[u8; 32]>) -> Result<Value, SignInRefused> {
        let asked = Answer::RefreshAsk {
            run: self.run,
            sign_in: sign_in.to_owned(),
            key,
        };
        match self.ask_sign_in(asked)? {
            Reply::Refreshed(answer) => answer,
            _ => Err(SignInRefused::Unavailable(format!(
                "box {}'s orchestrator answered a refresh with another's answer",
                self.id
            ))),
        }
    }

    fn revoked(&self, sign_in: &str, key: Option<[u8; 32]>) {
        if let Some(line) = self.line.upgrade() {
            let told = Answer::SignInRevoked {
                run: self.run,
                sign_in: sign_in.to_owned(),
                key,
            };
            let _ = line.tell(self.run, &told);
        }
    }

    fn release(&self, lease: &str) {
        if let Some(line) = self.line.upgrade() {
            let _ = line.tell(self.run, &Answer::LeaseGiven(lease.to_owned()));
        }
    }
}

impl WireLessor {
    /// `asked` sent over the run's line as an ask of its own, and its answer waited for while the connection stands; the
    /// connection's end refuses it `Unavailable`, naming the end. Whatever it carried (a capture's tokens) is wiped as it
    /// drops here.
    fn ask_sign_in(&self, asked: Answer) -> Result<Reply, SignInRefused> {
        let ended = || {
            SignInRefused::Unavailable(format!(
                "the connection of box {}'s orchestrator, which serves its sign-ins, has ended",
                self.id
            ))
        };
        let Some(line) = self.line.upgrade() else {
            return Err(ended());
        };
        let (ask, slot) = line
            .ask()
            .map_err(|why| SignInRefused::Unavailable(why.to_string()))?;
        if let Err(why) = line.tell(ask, &asked) {
            line.forget(ask);
            return Err(SignInRefused::Unavailable(why.to_string()));
        }
        drop((line, asked));
        match slot.wait() {
            Reply::Ended => Err(ended()),
            reply => Ok(reply),
        }
    }
}

/// The run has ended: its lessor is let go, and its orchestrator told, as an in-process lessor is dropped.
impl Drop for WireLessor {
    fn drop(&mut self) {
        if let Some(line) = self.line.upgrade() {
            let _ = line.tell(self.run, &Answer::LessorEnded);
        }
    }
}

/// Box `id` has no lessor here: it wasn't created on a connection that leases, or that connection has ended.
fn unrouted(id: &BoxId) -> Refusal {
    Refusal::Unconfigured {
        what: "lessors for its image VMs' credentials",
        why: format!(
            "box {id} leases its credentials from the connection that created it, and no connection that leases \
             (one that sent Lessors) created it, or that connection has ended"
        ),
    }
}

/// The connection that leases box `id`'s credentials has ended: nothing more is leased to its run here.
fn ended(id: &BoxId) -> LeaseRefused {
    LeaseRefused::Unavailable(format!(
        "the connection of box {id}'s orchestrator, which leases its credentials, has ended"
    ))
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> Refusal {
    Refusal::Broken("the node's lease routes' lock was poisoned".to_owned())
}

#[cfg(test)]
mod tests;
