//! Messages waiting for their session's inbox to answer (CP-T3.2), by session and message id, with one owner
//! and no lock (goal 5, plan 01a1a59c2aae, its top goal).
//!
//! A message's sender waits for the answer on its request's shard, and the answer comes on the hub's loop (a
//! runner's inbox) or the server's own sessions' loop, or on a session's owner shard. They met in a `Mutex`
//! the registry kept, taken twice a message by two shards. Now a task of its own owns the waits:
//! - a wait, a waiter giving up and an answer each go to it as a message, on a lock-free queue
//!   (`crate::memory::queue`), and it takes them in the order sent;
//! - a wait goes before its message does, and the answer comes back after that message went, so the
//!   owner has the wait before the answer;
//! - what it does with each is what the registry did under its lock.
//!
//! The waits' bound ([`super::registry::WAITING_MAX`]) is counted where a wait is asked for, without the
//! owner. The count covers the messages waited on, and the waits on their way to the owner. At the bound,
//! the waits whose ends were dropped go first, then the wait is asked again, as before.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::channel::oneshot;

use super::inbox::Said;

/// A message waited on: its session and its id.
type Key = (String, String);

/// What the owner is told, taken in the order sent.
enum Ask {
    /// A message's wait, counted: the end its answer goes to.
    Wait { key: Key, tx: oneshot::Sender<Said> },
    /// Its waiter stopped waiting: the message's waits whose ends were dropped go (an answer for those
    /// still waiting still reaches them).
    Unwait { key: Key },
    /// Its session's inbox said what became of the message: whoever waits for it longest is told.
    Answered { key: Key, said: Said },
    /// The bound was reached: every wait whose end was dropped goes; told once done.
    Sweep(oneshot::Sender<()>),
    /// How many wait now, the owner's count (a test's).
    #[cfg(test)]
    Held(oneshot::Sender<usize>),
}

/// The waits, as the registry holds them: what reaches their owner, and their count.
pub struct Answers {
    tx: crate::memory::queue::Sender<Ask>,
    /// The messages waited on, as the owner keeps them, and the waits on their way to it: what the bound
    /// counts.
    counted: Arc<AtomicUsize>,
    most: usize,
}

impl Answers {
    /// Waits for at most `most` messages at once, their owner started now.
    pub fn new(most: usize) -> Self {
        // Unbounded: each wait is counted against `most`, and each giving up and each answer is a message's.
        let (tx, rx) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
        let counted = Arc::new(AtomicUsize::new(0));
        // A service the server keeps, placed round-robin rather than on the shard that starts the server
        // (`crate::rt::spawn_spread_joined`, goal 9's rule); it ends as the registry goes, its queue's senders
        // with it. Its handle isn't needed: dropped, the task goes on.
        drop(crate::rt::spawn_spread_joined(own(rx, Arc::clone(&counted))));
        Self { tx, counted, most }
    }

    /// Waits for the answer to message `message` sent to session `id`: none when too many wait already, once
    /// those given up have gone (it isn't waited for).
    pub async fn wait_for(&self, id: &str, message: &str) -> Option<oneshot::Receiver<Said>> {
        if !self.count() {
            // Those whose waiters went away (and no answer came for) go first.
            let (done, swept) = oneshot::channel();
            if self.tx.try_send(Ask::Sweep(done)).is_err() {
                return None;
            }
            let _ = swept.await;
            if !self.count() {
                return None;
            }
        }
        let (tx, rx) = oneshot::channel();
        if self.tx.try_send(Ask::Wait { key: (id.to_owned(), message.to_owned()), tx }).is_err() {
            // Its owner is gone (the server's going): not waited for after all.
            uncount(&self.counted, 1);
            return None;
        }
        Some(rx)
    }

    /// No longer waiting for message `message` sent to session `id`: the waits whose ends were dropped go
    /// (an answer for those still waiting still reaches them).
    pub fn unwait(&self, id: &str, message: &str) {
        let _ = self.tx.try_send(Ask::Unwait { key: (id.to_owned(), message.to_owned()) });
    }

    /// Session `id`'s inbox said what became of a message: whoever waits for it longest is told. Each send
    /// of it is answered once, in the order they went: two sent under one id at once, one taken and one
    /// refused, are each told their own.
    pub fn answered(&self, id: &str, said: Said) {
        let _ = self.tx.try_send(Ask::Answered { key: (id.to_owned(), said.message.clone()), said });
    }

    /// One more counted, if the bound has room for it.
    fn count(&self) -> bool {
        let mut now = self.counted.load(Ordering::Acquire);
        loop {
            if now >= self.most {
                return false;
            }
            match self.counted.compare_exchange_weak(now, now.saturating_add(1), Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(seen) => now = seen,
            }
        }
    }

    /// How many waits there are now, once the owner has taken what was sent it before.
    #[cfg(test)]
    pub async fn held(&self) -> Option<usize> {
        let (tx, rx) = oneshot::channel();
        self.tx.try_send(Ask::Held(tx)).ok()?;
        rx.await.ok()
    }
}

/// `n` fewer counted: never below none.
fn uncount(counted: &AtomicUsize, n: usize) {
    let mut now = counted.load(Ordering::Acquire);
    while let Err(seen) = counted.compare_exchange_weak(now, now.saturating_sub(n), Ordering::AcqRel, Ordering::Acquire) {
        now = seen;
    }
}

/// The waits' owner: it alone holds them, taking each message in the order sent, until the registry goes.
async fn own(rx: crate::memory::queue::Receiver<Ask>, counted: Arc<AtomicUsize>) {
    let mut waiting: HashMap<Key, Vec<oneshot::Sender<Said>>> = HashMap::new();
    while let Ok(ask) = rx.recv().await {
        match ask {
            Ask::Wait { key, tx } => {
                let waits = waiting.entry(key).or_default();
                // Counted as a message of its own: one waited on already isn't.
                if !waits.is_empty() {
                    uncount(&counted, 1);
                }
                waits.push(tx);
            }
            Ask::Unwait { key } => {
                if let Some(waits) = waiting.get_mut(&key) {
                    waits.retain(|tx| !tx.is_canceled());
                    if waits.is_empty() {
                        waiting.remove(&key);
                        uncount(&counted, 1);
                    }
                }
            }
            Ask::Answered { key, said } => {
                let Some(waits) = waiting.get_mut(&key) else { continue };
                waits.retain(|tx| !tx.is_canceled());
                let first = (!waits.is_empty()).then(|| waits.remove(0));
                if waits.is_empty() {
                    waiting.remove(&key);
                    uncount(&counted, 1);
                }
                if let Some(tx) = first {
                    let _ = tx.send(said);
                }
            }
            Ask::Sweep(done) => {
                let before = waiting.len();
                waiting.retain(|_, waits| {
                    waits.retain(|tx| !tx.is_canceled());
                    !waits.is_empty()
                });
                uncount(&counted, before.saturating_sub(waiting.len()));
                let _ = done.send(());
            }
            #[cfg(test)]
            Ask::Held(tx) => {
                let _ = tx.send(waiting.values().map(Vec::len).sum());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::inbox::{self, Answer};

    fn refused(session: &str, message: &str) -> Said {
        Said { message: message.into(), answer: Err(inbox::Refusal::NoSession { session: session.into() }) }
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal), CP-T3.2's waits kept: messages sent from every shard at once
    /// each wait on their own, and each answer, from another shard, reaches its own message's waiter once.
    /// Nothing is lost or doubled, and the owner holds no wait after.
    #[test]
    fn g5_answers_from_every_shard_reach_their_own_waiters_once() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let answers = Arc::new(Answers::new(10_000));
            let (senders, per) = (8usize, 200usize);
            let mut sending = Vec::new();
            for s in 0..senders {
                let answers = Arc::clone(&answers);
                sending.push(crate::rt::spawn_spread_joined(async move {
                    let mut waits = Vec::new();
                    for n in 0..per {
                        let Some(rx) = answers.wait_for(&format!("s{s}"), &format!("m{n}")).await else { return Err(format!("message {s}/{n} not waited for")) };
                        waits.push((n, rx));
                    }
                    // The answers come from another shard, in no order of theirs.
                    let a = Arc::clone(&answers);
                    crate::rt::spawn_spread(async move {
                        for n in (0..per).rev() {
                            a.answered(&format!("s{s}"), refused(&format!("s{s}"), &format!("m{n}")));
                        }
                    });
                    for (n, rx) in waits {
                        match rx.await {
                            Ok(said) if said.message == format!("m{n}") => {}
                            other => return Err(format!("message {s}/{n} was told {other:?}")),
                        }
                    }
                    Ok(())
                }));
            }
            for s in sending {
                s.await.map_err(|_| anyhow::anyhow!("a sender ended"))?.map_err(|e| anyhow::anyhow!(e))?;
            }
            anyhow::ensure!(answers.held().await == Some(0), "waits left: {:?}", answers.held().await);
            anyhow::ensure!(answers.counted.load(Ordering::Acquire) == 0, "{} still counted", answers.counted.load(Ordering::Acquire));
            Ok(())
        })
    }

    /// CP-T3.2's bound, kept: past `most` messages waited on, a message isn't waited for, unless waits whose
    /// ends were dropped make room (they go first). One of a message waited on already is counted with it.
    #[test]
    fn g5_the_bound_holds_and_given_up_waits_make_room() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let answers = Answers::new(3);
            let a = answers.wait_for("s1", "m1").await;
            let b = answers.wait_for("s1", "m2").await;
            let c = answers.wait_for("s1", "m3").await;
            anyhow::ensure!(a.is_some() && b.is_some() && c.is_some(), "within the bound");
            anyhow::ensure!(answers.wait_for("s1", "m4").await.is_none(), "past the bound, none given up");
            // One given up without a word: it makes room.
            drop(b);
            let d = answers.wait_for("s1", "m4").await.ok_or_else(|| anyhow::anyhow!("a given-up wait made no room"))?;
            anyhow::ensure!(answers.held().await == Some(3), "{:?}", answers.held().await);
            // Answered in turn, each to its own.
            let taken = |m: &str| Said { message: m.into(), answer: Ok(Answer { session: "s1".into(), message: m.into(), epoch: 0, at: 1, seq: 7, repeat: false }) };
            for m in ["m1", "m3", "m4"] {
                answers.answered("s1", taken(m));
            }
            anyhow::ensure!(a.ok_or_else(|| anyhow::anyhow!("no wait"))?.await.ok() == Some(taken("m1")), "m1");
            anyhow::ensure!(c.ok_or_else(|| anyhow::anyhow!("no wait"))?.await.ok() == Some(taken("m3")), "m3");
            anyhow::ensure!(d.await.ok() == Some(taken("m4")), "m4");
            anyhow::ensure!(answers.held().await == Some(0) && answers.counted.load(Ordering::Acquire) == 0, "all gone");
            Ok(())
        })
    }
}
