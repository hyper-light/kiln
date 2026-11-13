//! What a viewer's attach to an agent session waits for while the server starts (docs/wire.md, "Restarts").
//!
//! A server that starts again rebuilds its copies of the sessions its conversation store has, and once its runners are
//! back they send each copy what it lacks. A session the store has none of (the store's off, or a runner's feed hadn't
//! brought it any yet) has no copy until its runner is back and sends it whole. Its first attach used to be told it was
//! closed (a frame, then the socket's end) though its runner still had it. Now an attach waits instead, in two cases:
//! while the store's copies and the server's own sessions are still being listed; and, for a session the server has no
//! copy of, while a runner connected just before the start hasn't yet said what it holds, or one has said it holds that
//! session and its Open is on its way. It waits no longer than those runners' grace to come back, as long as runners
//! took to come back before the start ([`crate::comeback`]). Past that, a runner not back is taken for gone, as its jobs
//! are.

use std::collections::HashSet;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::rcu::Rcu;

/// A server's start, as its sessions' attaches wait on it ([`Returning::wait`]): nothing to wait for until it's
/// armed ([`Returning::arm`]), as a registry no server started (a test's) never is.
#[derive(Default)]
pub struct Returning {
    /// Until when the runners connected just before the server started may still come back: its start and their
    /// grace.
    until: OnceLock<Instant>,
    /// Those runners, by machine (`crate::store::Store::runners_named_before`).
    expected: Rcu<HashSet<String>>,
    /// The machines that have said what they hold since the start, and the sessions they said, until `until`.
    reported: Rcu<Reported>,
    /// The store's copies and the server's own sessions are being listed.
    listing: AtomicBool,
    /// Told as any of that changes, and as a machine announces a session ([`Returning::came`]).
    changed: crate::rt::Changed,
}

#[derive(Default, Clone)]
struct Reported {
    machines: HashSet<String>,
    sessions: HashSet<String>,
}

impl Returning {
    /// As the server starts: `expected`, the runners connected just before it, which may come back until `until`,
    /// and its listing of the store's copies and its own sessions under way.
    pub fn arm(&self, expected: impl IntoIterator<Item = String>, until: Instant) {
        self.expected.set(expected.into_iter().collect());
        self.listing.store(true, Ordering::Release);
        let _ = self.until.set(until);
        self.changed.notify();
    }

    /// The store's copies and the server's own sessions are listed.
    pub fn listed(&self) {
        self.listing.store(false, Ordering::Release);
        self.changed.notify();
    }

    /// Machine `machine` said what it holds since the start: `sessions` (none for a runner that holds no agent
    /// sessions). Kept only until the runners' grace ends.
    pub fn reported(&self, machine: &str, sessions: impl IntoIterator<Item = String>) {
        self.note(Some(machine), sessions);
    }

    /// The server's own sessions, restored as it starts: each has a copy once its Open is taken, as a runner's does.
    pub fn own(&self, sessions: impl IntoIterator<Item = String>) {
        self.note(None, sessions);
    }

    fn note(&self, machine: Option<&str>, sessions: impl IntoIterator<Item = String>) {
        if !self.until.get().is_some_and(|until| Instant::now() < *until) {
            return;
        }
        let sessions: Vec<String> = sessions.into_iter().collect();
        self.reported.update(|r| {
            if let Some(m) = machine {
                r.machines.insert(m.to_owned());
            }
            r.sessions.extend(sessions.iter().cloned());
        });
        self.changed.notify();
    }

    /// A machine announced a session, its copy published (its Open taken).
    pub fn came(&self) {
        self.changed.notify();
    }

    /// Whether the server can say it has session `id` or hasn't: its listing is done, and it has a copy of it
    /// (`has`), or none of the runners that may come back holds it (each has said what it holds, and none said it).
    fn settled(&self, id: &str, has: impl FnOnce() -> bool) -> bool {
        if self.listing.load(Ordering::Acquire) {
            return false;
        }
        if has() {
            return true;
        }
        let reported = self.reported.load();
        !reported.sessions.contains(id) && self.expected.load().iter().all(|m| reported.machines.contains(m))
    }

    /// Whether the server can say now, without waiting, that it has session `id` or hasn't: as [`Returning::wait`]
    /// would be back at once (no server armed it, past the runners' grace, or settled).
    pub fn says(&self, id: &str, has: impl FnOnce() -> bool) -> bool {
        self.until.get().is_none_or(|until| Instant::now() >= *until) || self.settled(id, has)
    }

    /// Back once the server can say it has session `id` or hasn't ([`Returning::settled`]), `has` saying whether it
    /// has a copy of it. Back at once past the runners' grace, and in a registry no server armed.
    pub async fn wait(&self, id: &str, has: impl Fn() -> bool) {
        let Some(until) = self.until.get().copied() else { return };
        loop {
            if Instant::now() >= until {
                return;
            }
            // Listened to before it looks: a change after the look ends this wait.
            let changed = self.changed.listen();
            if self.settled(id, &has) {
                return;
            }
            let _ = crate::rt::timeout(until.saturating_duration_since(Instant::now()), changed).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    /// docs/wire.md, "Restarts": an attach to a session a restarted server has no copy of waits while the session may
    /// still come back with its runner, and only while it may. Do: a server's start armed with two runners expected
    /// back. Expect: nothing settled while its listing is under way; then a session with a copy settled; one without
    /// none until the runners say what they hold; one a runner says it holds not until its copy is there; one no runner
    /// holds settled once every expected runner has said what it holds.
    #[test]
    fn a_session_with_no_copy_is_settled_once_no_runner_that_may_come_back_holds_it() -> anyhow::Result<()> {
        let r = Returning::default();
        anyhow::ensure!(r.settled("s1", || false), "no server armed it: nothing to wait for");
        r.arm(["m1".to_owned(), "m2".to_owned()], Instant::now() + Duration::from_secs(60));
        anyhow::ensure!(!r.settled("s1", || true), "listing under way: not even a copy is settled");
        r.listed();
        anyhow::ensure!(r.settled("s1", || true), "a copy: settled");
        anyhow::ensure!(!r.settled("s2", || false), "no copy, and no runner back yet");
        r.reported("m1", ["s2".to_owned()]);
        r.reported("m2", Vec::new());
        anyhow::ensure!(!r.settled("s2", || false), "a runner back holds it: its Open is on its way");
        anyhow::ensure!(r.settled("s2", || true), "its copy came");
        anyhow::ensure!(r.settled("s3", || false), "every runner that may come back said what it holds, and none holds it");
        r.own(["s4".to_owned()]);
        anyhow::ensure!(!r.settled("s4", || false), "one of the server's own, restored: its Open is on its way");
        Ok(())
    }

    /// docs/wire.md, "Restarts": a runner's word comes in only while the runners' grace lasts, and an attach waits no
    /// longer than that grace. Do: armed with a grace already over. Expect: what a runner says isn't kept, and a wait
    /// for a session no runner said is back at once.
    #[test]
    fn past_the_grace_nothing_is_kept_and_nothing_waits() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let r = Returning::default();
            r.arm(["m1".to_owned()], Instant::now());
            r.reported("m1", ["s1".to_owned()]);
            anyhow::ensure!(r.reported.load().machines.is_empty(), "kept past the grace");
            let began = Instant::now();
            r.wait("s1", || false).await;
            anyhow::ensure!(began.elapsed() < Duration::from_secs(5), "waited past the grace: {:?}", began.elapsed());
            Ok(())
        })
    }

    /// docs/wire.md, "Restarts": an attach waiting for a session its runner is bringing back goes on as soon as its copy
    /// is there, woken by it, not at the grace's end. Do: armed with a minute's grace, listed, the runner's word that it
    /// holds the session; a wait for it on a thread of its own; the copy made and announced 200 ms later. Expect: the
    /// wait is back after those 200 ms and within a few seconds, not a minute.
    #[test]
    fn a_wait_ends_as_the_sessions_copy_comes() -> anyhow::Result<()> {
        let r = Arc::new(Returning::default());
        r.arm(["m1".to_owned()], Instant::now() + Duration::from_secs(60));
        r.listed();
        r.reported("m1", ["s1".to_owned()]);
        let has = Arc::new(AtomicBool::new(false));
        let (w, h) = (Arc::clone(&r), Arc::clone(&has));
        let waiter = std::thread::spawn(move || -> anyhow::Result<Duration> {
            let began = Instant::now();
            crate::rt::block_on(async move {
                w.wait("s1", || h.load(Ordering::Acquire)).await;
                Ok(())
            })?;
            Ok(began.elapsed())
        });
        std::thread::sleep(Duration::from_millis(200));
        has.store(true, Ordering::Release);
        r.came();
        let took = waiter.join().map_err(|_| anyhow::anyhow!("the waiter's thread panicked"))??;
        anyhow::ensure!(took >= Duration::from_millis(150) && took < Duration::from_secs(10), "the wait took {took:?}");
        Ok(())
    }
}
