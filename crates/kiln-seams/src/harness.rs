//! `HarnessPort`: kiln's agents as the code that drives them sees them: who they are, what their
//! harness takes, and commands to them. kiln's harness registry is the real one (local sessions
//! and every runner's); the simulator, [`SimHarness`], keeps agents in memory and records each
//! command it's handed, stamped by its clock, so a test of the goal driver reads exactly what
//! went to an agent, and when.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::Value;

use crate::clock::{Clock, SimClock};
use crate::conformance::{Case, Failure, ensure, same};

/// How a command went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    /// The agent's runner has it.
    Delivered,
    /// No agent by that id is known.
    NoSession,
    /// The agent is known, its machine isn't reachable now: nothing was delivered.
    Unreachable,
    /// The agent's inbox holds its most: nothing was delivered.
    Full,
}

/// kiln's agents, to the code that drives them.
pub trait HarnessPort: Send + Sync + 'static {
    /// An agent as the port sees it (kiln's `harness::session::Info`).
    type Agent: Clone + Send + Sync + 'static;
    /// A command to one (kiln's `harness::model::Command`).
    type Command: Send + 'static;
    /// Every agent known now, each once.
    fn agents(&self) -> Vec<Self::Agent>;
    /// One agent, by id.
    fn agent(&self, id: &str) -> Option<Self::Agent>;
    /// What the agent's harness takes (its command catalog), when it has said.
    fn catalog(&self, id: &str) -> Option<Value>;
    /// Hands `command` to agent `id`.
    fn send<'a>(&'a self, id: &'a str, command: Self::Command) -> BoxFuture<'a, Sent>;
}

// ---------------------------------------------------------------------------------- simulator

/// Most commands a [`SimHarness`] keeps per agent by default; past it, a send is refused `Full`.
pub const SIM_INBOX: usize = 100_000;

/// A command the simulator was handed, and when (its clock's wall time).
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery<C> {
    pub at_ms: i64,
    pub command: C,
}

/// Agents in memory. Clones share them.
pub struct SimHarness<A, C> {
    inner: Arc<Mutex<SimState<A, C>>>,
    clock: Arc<dyn Clock>,
    inbox: usize,
}

impl<A, C> Clone for SimHarness<A, C> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner), clock: Arc::clone(&self.clock), inbox: self.inbox }
    }
}

struct SimState<A, C> {
    order: Vec<String>,
    agents: HashMap<String, A>,
    catalogs: HashMap<String, Value>,
    heard: HashMap<String, Vec<Delivery<C>>>,
    away: HashSet<String>,
}

impl<A, C> Default for SimState<A, C> {
    fn default() -> Self {
        Self { order: Vec::new(), agents: HashMap::new(), catalogs: HashMap::new(), heard: HashMap::new(), away: HashSet::new() }
    }
}

impl<A: Clone, C: Clone> Default for SimHarness<A, C> {
    fn default() -> Self {
        Self::new(Arc::new(SimClock::new()))
    }
}

impl<A: Clone, C: Clone> SimHarness<A, C> {
    /// Agents whose deliveries are stamped by `clock`.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { inner: Arc::new(Mutex::new(SimState::default())), clock, inbox: SIM_INBOX }
    }

    /// At most `n` commands kept per agent.
    pub fn with_inbox(mut self, n: usize) -> Self {
        self.inbox = n;
        self
    }

    /// Opens agent `id` (or sets it again, as it is now).
    pub fn open(&self, id: &str, agent: A) {
        let mut st = self.inner.lock();
        if st.agents.insert(id.to_owned(), agent).is_none() {
            st.order.push(id.to_owned());
        }
    }

    /// Changes agent `id` as it is now; false when there's none.
    pub fn update(&self, id: &str, f: impl FnOnce(&mut A)) -> bool {
        self.inner.lock().agents.get_mut(id).map(f).is_some()
    }

    /// Closes agent `id`: no longer known (what it was sent stays readable).
    pub fn close(&self, id: &str) {
        let mut st = self.inner.lock();
        st.agents.remove(id);
        st.order.retain(|o| o != id);
        st.catalogs.remove(id);
        st.away.remove(id);
    }

    /// Agent `id`'s machine goes away (`true`) or comes back.
    pub fn away(&self, id: &str, away: bool) {
        let mut st = self.inner.lock();
        if away {
            st.away.insert(id.to_owned());
        } else {
            st.away.remove(id);
        }
    }

    /// What agent `id`'s harness says it takes.
    pub fn set_catalog(&self, id: &str, catalog: Value) {
        self.inner.lock().catalogs.insert(id.to_owned(), catalog);
    }

    /// Every command agent `id` was handed, in order.
    pub fn heard(&self, id: &str) -> Vec<Delivery<C>> {
        self.inner.lock().heard.get(id).cloned().unwrap_or_default()
    }
}

impl<A, C> HarnessPort for SimHarness<A, C>
where
    A: Clone + Send + Sync + 'static,
    C: Clone + Send + 'static,
{
    type Agent = A;
    type Command = C;

    fn agents(&self) -> Vec<A> {
        let st = self.inner.lock();
        st.order.iter().filter_map(|id| st.agents.get(id).cloned()).collect()
    }

    fn agent(&self, id: &str) -> Option<A> {
        self.inner.lock().agents.get(id).cloned()
    }

    fn catalog(&self, id: &str) -> Option<Value> {
        self.inner.lock().catalogs.get(id).cloned()
    }

    fn send<'a>(&'a self, id: &'a str, command: C) -> BoxFuture<'a, Sent> {
        let at_ms = self.clock.unix_ms();
        let sent = {
            let mut st = self.inner.lock();
            if !st.agents.contains_key(id) {
                Sent::NoSession
            } else if st.away.contains(id) {
                Sent::Unreachable
            } else {
                let inbox = st.heard.entry(id.to_owned()).or_default();
                if inbox.len() >= self.inbox {
                    Sent::Full
                } else {
                    inbox.push(Delivery { at_ms, command });
                    Sent::Delivered
                }
            }
        };
        Box::pin(async move { sent })
    }
}

// -------------------------------------------------------------------------------- conformance

/// What a `HarnessPort` conformance suite runs against.
pub trait HarnessFixture: Send + Sync {
    type Port: HarnessPort;
    fn port(&self) -> &Self::Port;
    /// Opens an agent the port then knows as `id`.
    fn open<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), Failure>>;
    /// Closes agent `id`.
    fn close<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), Failure>>;
    /// A command that tells an agent `text`.
    fn say(&self, text: &str) -> <Self::Port as HarnessPort>::Command;
    /// What agent `id` was told, in order (its texts).
    fn heard<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Vec<String>>;
    /// An agent's id.
    fn id_of(&self, agent: &<Self::Port as HarnessPort>::Agent) -> String;
}

/// An agent of the simulator's fixture: its id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimAgent {
    pub id: String,
}

/// The fixture for a [`SimHarness`].
pub struct SimHarnessFixture(pub SimHarness<SimAgent, String>);

impl HarnessFixture for SimHarnessFixture {
    type Port = SimHarness<SimAgent, String>;
    fn port(&self) -> &Self::Port {
        &self.0
    }
    fn open<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), Failure>> {
        self.0.open(id, SimAgent { id: id.to_owned() });
        Box::pin(async { Ok(()) })
    }
    fn close<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), Failure>> {
        self.0.close(id);
        Box::pin(async { Ok(()) })
    }
    fn say(&self, text: &str) -> String {
        text.to_owned()
    }
    fn heard<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Vec<String>> {
        let h = self.0.heard(id).into_iter().map(|d| d.command).collect();
        Box::pin(async move { h })
    }
    fn id_of(&self, agent: &SimAgent) -> String {
        agent.id.clone()
    }
}

fn ids<F: HarnessFixture>(f: &F) -> Vec<String> {
    f.port().agents().iter().map(|a| f.id_of(a)).collect()
}

fn c1<F: HarnessFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        f.open("conformance-a").await?;
        f.open("conformance-b").await?;
        let listed = ids(f);
        for id in ["conformance-a", "conformance-b"] {
            same(&format!("times {id} is listed"), listed.iter().filter(|l| *l == id).count(), 1)?;
            ensure(f.port().agent(id).is_some_and(|a| f.id_of(&a) == id), || format!("agent({id}) didn't find it"))?;
        }
        ensure(!listed.iter().any(|l| l == "conformance-never") && f.port().agent("conformance-never").is_none(), || "an agent never opened is known".into())
    })
}

fn c2<F: HarnessFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move { same("a command to an unknown agent", f.port().send("conformance-never", f.say("hello")).await, Sent::NoSession) })
}

fn c3<F: HarnessFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        f.open("conformance-a").await?;
        for t in ["one", "two", "three"] {
            same(&format!("sending {t:?}"), f.port().send("conformance-a", f.say(t)).await, Sent::Delivered)?;
        }
        let heard = f.heard("conformance-a").await;
        same("what the agent was told", heard.iter().map(String::as_str).filter(|h| ["one", "two", "three"].contains(h)).collect::<Vec<_>>(), vec!["one", "two", "three"])
    })
}

fn c4<F: HarnessFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        f.open("conformance-a").await?;
        f.close("conformance-a").await?;
        ensure(!ids(f).iter().any(|l| l == "conformance-a") && f.port().agent("conformance-a").is_none(), || "a closed agent is still known".into())?;
        same("a command to a closed agent", f.port().send("conformance-a", f.say("hello")).await, Sent::NoSession)
    })
}

fn c5<F: HarnessFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        f.open("conformance-a").await?;
        f.open("conformance-b").await?;
        same("sending to a", f.port().send("conformance-a", f.say("for a only")).await, Sent::Delivered)?;
        ensure(!f.heard("conformance-b").await.iter().any(|h| h == "for a only"), || "what was sent to one agent reached another".into())
    })
}

/// The `HarnessPort` contract, clause by clause (docs/seams.md, "HarnessPort").
pub fn cases<F: HarnessFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "HARNESS-1", says: "an open agent is listed once by agents() and found by agent(id); one never opened is neither", check: c1::<F> },
        Case { id: "HARNESS-2", says: "a command to an agent the port doesn't know is refused NoSession", check: c2::<F> },
        Case { id: "HARNESS-3", says: "commands delivered to an agent reach it in the order they were sent, each once", check: c3::<F> },
        Case { id: "HARNESS-4", says: "a closed agent is no longer known, and a command to it is refused NoSession", check: c4::<F> },
        Case { id: "HARNESS-5", says: "what's sent to one agent never reaches another", check: c5::<F> },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run;

    #[test]
    fn the_simulated_harness_passes_the_harness_port_suite() -> Result<(), String> {
        futures::executor::block_on(run("HarnessPort", &cases::<SimHarnessFixture>(), || async { SimHarnessFixture(SimHarness::default()) })).into_result().map(|_| ())
    }

    #[test]
    fn deliveries_are_stamped_by_the_clock_and_refusals_are_typed() {
        let clock = SimClock::new();
        let h: SimHarness<SimAgent, String> = SimHarness::new(Arc::new(clock.clone())).with_inbox(1);
        h.open("a", SimAgent { id: "a".into() });
        clock.advance(std::time::Duration::from_secs(30));
        assert_eq!(futures::executor::block_on(h.send("a", "first".into())), Sent::Delivered);
        assert_eq!(h.heard("a"), vec![Delivery { at_ms: crate::clock::SIM_EPOCH_MS + 30_000, command: "first".to_owned() }]);
        assert_eq!(futures::executor::block_on(h.send("a", "second".into())), Sent::Full, "its inbox holds one");
        h.away("a", true);
        assert_eq!(futures::executor::block_on(h.send("a", "third".into())), Sent::Unreachable);
        assert!(h.update("a", |a| a.id = "a".into()) && !h.update("b", |_| {}));
    }
}
