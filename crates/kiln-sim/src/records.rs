//! A crash at any write of a records store's: [`Crashing`] wraps a kiln-seams `Records` (its
//! `SimRecords`, mostly) and crashes at the commit a model chooses, just before it takes effect or
//! just after (the caller never hears which). That commit and every call after it are refused
//! `Unavailable` until the model has crashed what runs on the store and [`Crashing::restart`]s it
//! on what the store kept: what a crash-at-every-write test of a journal on `Records` runs on (the
//! session journal of CP-T3.1). A model can also have a commit refused, typed, with no crash
//! ([`Crashing::refuse_at`]), and reads how many calls of each kind went through ([`Calls`]).

use std::sync::Arc;

use futures::future::BoxFuture;
use kiln_seams::records::{Changes, Consistency, Page, Records, Refusal, Rev, Row, Scan, Tx};
use parking_lot::Mutex;

/// The calls that went through a [`Crashing`] store, by kind (refused ones included).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Calls {
    pub gets: u64,
    pub scans: u64,
    pub commits: u64,
    pub changes: u64,
    pub waits: u64,
    pub heads: u64,
    /// Commits refused by [`Crashing::refuse_at`], nothing committed.
    pub refused: u64,
}

#[derive(Debug, Default)]
struct State {
    /// Calls made through it.
    calls: Calls,
    /// The crash to come: commits until it, and whether it comes after its commit took effect.
    at: Option<(u64, bool)>,
    /// A refusal to come: commits until it, and what it says.
    refuse: Option<(u64, Refusal)>,
    crashed: bool,
    crashes: u64,
}

/// A records store that crashes where a model says. Clones share it.
pub struct Crashing<R> {
    inner: Arc<R>,
    state: Arc<Mutex<State>>,
}

impl<R> Clone for Crashing<R> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner), state: Arc::clone(&self.state) }
    }
}

fn down() -> Refusal {
    Refusal::Unavailable("the process crashed (kiln-sim)".into())
}

impl<R: Records> Crashing<R> {
    pub fn new(inner: R) -> Self {
        Self { inner: Arc::new(inner), state: Arc::new(Mutex::new(State::default())) }
    }

    /// The store beneath: what survived.
    pub fn inner(&self) -> &R {
        &self.inner
    }

    /// A crash at the `n`th commit from now (0: the next), just before it takes effect, or just
    /// after when `after`.
    pub fn crash_at(&self, n: u64, after: bool) {
        self.state.lock().at = Some((n, after));
    }

    /// Whether the crash has come.
    pub fn crashed(&self) -> bool {
        self.state.lock().crashed
    }

    /// Calls work again: the model has started what runs on the store anew.
    pub fn restart(&self) {
        let mut st = self.state.lock();
        st.crashed = false;
        st.at = None;
    }

    /// The `n`th commit from now (0: the next) is refused with `refusal`, nothing committed and no
    /// crash: the caller is to say so, never to take the write as made.
    pub fn refuse_at(&self, n: u64, refusal: Refusal) {
        self.state.lock().refuse = Some((n, refusal));
    }

    /// Commits made through it (refused ones included), and crashes.
    pub fn commits(&self) -> u64 {
        self.state.lock().calls.commits
    }

    /// The calls made through it, by kind.
    pub fn calls(&self) -> Calls {
        self.state.lock().calls
    }

    pub fn crashes(&self) -> u64 {
        self.state.lock().crashes
    }

    /// Counts a call; refused `Unavailable` once the crash has come.
    fn call(&self, kind: fn(&mut Calls) -> &mut u64) -> Result<(), Refusal> {
        let mut st = self.state.lock();
        let n = kind(&mut st.calls);
        *n = n.saturating_add(1);
        if st.crashed { Err(down()) } else { Ok(()) }
    }

    /// The refusal this commit gets, if it's the one chosen.
    fn refusal_now(&self) -> Option<Refusal> {
        let mut st = self.state.lock();
        match st.refuse.take() {
            Some((0, r)) => {
                st.calls.refused = st.calls.refused.saturating_add(1);
                Some(r)
            }
            Some((n, r)) => {
                st.refuse = Some((n.saturating_sub(1), r));
                None
            }
            None => None,
        }
    }

    /// Whether this commit is the crash's, and if so whether it takes effect first.
    fn crash_now(&self) -> Option<bool> {
        let mut st = self.state.lock();
        match st.at {
            Some((0, after)) => {
                st.at = None;
                st.crashed = true;
                st.crashes = st.crashes.saturating_add(1);
                Some(after)
            }
            Some((n, after)) => {
                st.at = Some((n.saturating_sub(1), after));
                None
            }
            None => None,
        }
    }
}

impl<R: Records> Records for Crashing<R> {
    fn get<'a>(&'a self, family: &'a str, key: &'a [u8], at: Consistency) -> BoxFuture<'a, Result<Option<Row>, Refusal>> {
        Box::pin(async move {
            self.call(|c| &mut c.gets)?;
            self.inner.get(family, key, at).await
        })
    }

    fn scan<'a>(&'a self, family: &'a str, scan: Scan) -> BoxFuture<'a, Result<Page, Refusal>> {
        Box::pin(async move {
            self.call(|c| &mut c.scans)?;
            self.inner.scan(family, scan).await
        })
    }

    fn commit<'a>(&'a self, tx: Tx) -> BoxFuture<'a, Result<Rev, Refusal>> {
        Box::pin(async move {
            self.call(|c| &mut c.commits)?;
            if let Some(r) = self.refusal_now() {
                return Err(r);
            }
            match self.crash_now() {
                None => self.inner.commit(tx).await,
                Some(after) => {
                    if after {
                        // It took effect; the process went before it heard so.
                        let _ = self.inner.commit(tx).await;
                    }
                    Err(down())
                }
            }
        })
    }

    fn changes<'a>(&'a self, family: &'a str, after: Rev, limit: usize) -> BoxFuture<'a, Result<Changes, Refusal>> {
        Box::pin(async move {
            self.call(|c| &mut c.changes)?;
            self.inner.changes(family, after, limit).await
        })
    }

    fn wait<'a>(&'a self, family: &'a str, after: Rev) -> BoxFuture<'a, Result<Rev, Refusal>> {
        Box::pin(async move {
            self.call(|c| &mut c.waits)?;
            self.inner.wait(family, after).await
        })
    }

    fn head(&self) -> BoxFuture<'_, Result<Rev, Refusal>> {
        Box::pin(async move {
            self.call(|c| &mut c.heads)?;
            self.inner.head().await
        })
    }
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use kiln_seams::records::{SimRecords, Write};

    use super::*;

    /// A toy journal: entry `i` is a row `i`, acknowledged once its commit returns.
    async fn append(r: &dyn Records, i: u32) -> Result<(), Refusal> {
        let tx = Tx { conds: Vec::new(), writes: vec![Write::Put { family: "journal".into(), key: i.to_be_bytes().to_vec(), value: vec![1] }] };
        r.commit(tx).await.map(|_| ())
    }

    async fn kept(r: &dyn Records, n: u32) -> Result<Vec<u32>, Refusal> {
        let mut out = Vec::new();
        for i in 0..n {
            if r.get("journal", &i.to_be_bytes(), Consistency::Linearizable).await?.is_some() {
                out.push(i);
            }
        }
        Ok(out)
    }

    /// CP-T6.1, the goal's nemesis (a crash at every write), for CP-T3.1's crash test: a crash at
    /// every commit, before it takes effect and after: what was acknowledged is always kept,
    /// recovery is the committed prefix (the crash's own commit at most besides), and nothing goes
    /// through a crashed store.
    #[test]
    fn a_crash_at_every_commit_keeps_what_was_acknowledged() -> Result<(), Refusal> {
        for at in 0..10u64 {
            for after in [false, true] {
                let store = Crashing::new(SimRecords::new());
                store.crash_at(at, after);
                let mut acked = Vec::new();
                for i in 0..10u32 {
                    if block_on(append(&store, i)).is_err() {
                        break;
                    }
                    acked.push(i);
                }
                assert!(store.crashed() && store.crashes() == 1, "the crash came at commit {at}");
                assert!(block_on(store.head()).is_err() && block_on(append(&store, 99)).is_err(), "nothing goes through a crashed store");
                store.restart();
                let back = block_on(kept(store.inner(), 10))?;
                let want: Vec<u32> = (0..u32::try_from(at).unwrap_or(0).saturating_add(u32::from(after))).collect();
                assert_eq!(back, want, "crash at commit {at} (after: {after}): acknowledged {acked:?}");
                assert!(acked.iter().all(|a| back.contains(a)));
                block_on(append(&store, 50))?;
            }
        }
        Ok(())
    }

    /// CP-T6.1, for CP-T3.1's journal: a commit refused, typed, with no crash: nothing committed,
    /// the calls after it go through, and every call is counted by its kind.
    #[test]
    fn a_refused_commit_commits_nothing_and_the_store_goes_on() -> Result<(), Refusal> {
        let store = Crashing::new(SimRecords::new());
        store.refuse_at(1, Refusal::Full { limit: 3 });
        block_on(append(&store, 0))?;
        assert_eq!(block_on(append(&store, 1)), Err(Refusal::Full { limit: 3 }));
        block_on(append(&store, 2))?;
        assert!(!store.crashed());
        assert_eq!(block_on(kept(&store, 3))?, [0, 2], "the refused commit's write isn't there");
        let calls = store.calls();
        assert_eq!((calls.commits, calls.refused, calls.gets), (3, 1, 3));
        Ok(())
    }
}
