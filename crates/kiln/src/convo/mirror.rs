//! Mirrors (`docs/conversations.md`, "Mirrors"): runners that keep a copy of the server's store,
//! so a conversation outlives the server's disk and its own machine both. The server picks them
//! (`[conversations] mirrors`) and sends each its store the way a runner sends the server its own:
//! the same feed (`super::feed`), the other way, on the same lane. A mirror keeps the copy beside
//! its own store (`mirror.db`).
//!
//! Which runners: those named first (`mirror_on`), then the ones connected longest (a machine that
//! sleeps reconnects often), then the most free disk. One away for a day is replaced; the copy it
//! has stays where it is.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use hyper_transport::PeerId;
use serde::{Deserialize, Serialize};

use super::feed::{Link, Msg, Shipper};
use super::store::Store;

/// A mirror away this long is replaced.
const AWAY_MS: i64 = 24 * 3600 * 1000;

/// A runner that could keep a copy: connected, keeping a store of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub machine: String,
    pub peer: PeerId,
    /// Unix ms its connection began.
    pub connected_at: i64,
    pub disk_free: u64,
}

/// A machine picked to keep a copy (`<data>/harness/mirrors.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Chosen {
    machine: String,
    chosen_at: i64,
    last_seen: i64,
}

/// What the API shows of a mirror.
#[derive(Debug, Clone, Serialize)]
pub struct MirrorView {
    pub machine: String,
    pub connected: bool,
    pub chosen_at: i64,
    pub last_seen: i64,
}

/// A chosen machine's feed: where it is now, and what sends to it.
struct Feed {
    peer: Arc<Mutex<Option<PeerId>>>,
    shipper: Shipper,
}

/// The server's mirrors.
pub struct Mirrors {
    store: Store,
    want: usize,
    first: Vec<String>,
    file: PathBuf,
    chosen: Mutex<Vec<Chosen>>,
    feeds: Mutex<HashMap<String, Feed>>,
    out: async_channel::Sender<(PeerId, String, Vec<u8>)>,
    /// What each mirror sent back to restore the store from: not sent to it again.
    echoes: Option<Arc<super::feed::Echoes>>,
}

/// The candidates in the order they are picked in.
fn ranked(mut c: Vec<Candidate>, first: &[String]) -> Vec<Candidate> {
    c.sort_by(|a, b| {
        let named = |x: &Candidate| first.iter().position(|n| *n == x.machine).unwrap_or(usize::MAX);
        named(a).cmp(&named(b)).then(a.connected_at.cmp(&b.connected_at)).then(b.disk_free.cmp(&a.disk_free)).then(a.machine.cmp(&b.machine))
    });
    c
}

impl Mirrors {
    /// The server's mirrors of `store`, `want` of them, picked from `first` first; what to send
    /// each comes out of the receiver, `(peer, json, bytes)` for `ServerMsg::Convo`. `echoes`:
    /// what a mirror sent back to restore the store from, which its feed leaves out.
    pub fn start(store: Store, want: usize, first: Vec<String>, file: PathBuf, echoes: Option<Arc<super::feed::Echoes>>) -> (Arc<Self>, async_channel::Receiver<(PeerId, String, Vec<u8>)>) {
        let chosen: Vec<Chosen> = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let (out, rx) = async_channel::bounded(64);
        (Arc::new(Self { store, want, first, file, chosen: Mutex::new(chosen), feeds: Mutex::new(HashMap::new()), out, echoes }), rx)
    }

    /// The machines picked to keep a copy (`mirrors.json`).
    pub fn chosen_machines(&self) -> Vec<String> {
        self.chosen.lock().unwrap_or_else(PoisonError::into_inner).iter().map(|c| c.machine.clone()).collect()
    }

    /// The store the mirrors keep a copy of: messages about it are theirs.
    pub fn store_id(&self) -> &str {
        self.store.id()
    }

    fn save(&self, chosen: &[Chosen]) {
        let written = serde_json::to_vec(chosen).map_err(anyhow::Error::from).and_then(|b| crate::sync::group::save_atomic(&self.file, &b).map_err(anyhow::Error::from));
        if let Err(e) = written {
            tracing::warn!(error = %format!("{e:#}"), "couldn't keep which runners mirror the conversation store");
        }
    }

    /// The feed to `machine`, made when first needed.
    fn feed(&self, machine: &str) -> (Shipper, Arc<Mutex<Option<PeerId>>>) {
        let mut feeds = self.feeds.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(f) = feeds.get(machine) {
            return (f.shipper.clone(), f.peer.clone());
        }
        let (shipper, said) = match self.echoes.clone() {
            Some(echoes) => {
                let m = machine.to_owned();
                Shipper::start_skipping(self.store.clone(), Arc::new(move |rev| echoes.has(&m, rev)))
            }
            None => Shipper::start(self.store.clone()),
        };
        let peer: Arc<Mutex<Option<PeerId>>> = Arc::default();
        let (to, out) = (peer.clone(), self.out.clone());
        crate::rt::spawn(async move {
            while let Ok((json, data)) = said.recv().await {
                let at = *to.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(p) = at
                    && out.send((p, json, data)).await.is_err()
                {
                    break;
                }
            }
        });
        feeds.insert(machine.to_owned(), Feed { peer: peer.clone(), shipper: shipper.clone() });
        (shipper, peer)
    }

    /// A runner process keeping a store connected (`connected`, among the `all` connected now):
    /// the mirrors are picked again as needed, and each connected one is fed.
    pub fn connected(&self, all: Vec<Candidate>) {
        if self.want == 0 {
            return;
        }
        let now = crate::util::now_ms();
        let picked = {
            let mut chosen = self.chosen.lock().unwrap_or_else(PoisonError::into_inner);
            let before = chosen.clone();
            for c in chosen.iter_mut() {
                if all.iter().any(|a| a.machine == c.machine) {
                    c.last_seen = now;
                }
            }
            // Away too long: another keeps the copy from now on.
            chosen.retain(|c| now - c.last_seen < AWAY_MS || all.iter().any(|a| a.machine == c.machine));
            // A runner named first, here now, takes the place of one that wasn't named.
            let missing: Vec<String> = self.first.iter().filter(|n| all.iter().any(|a| &a.machine == *n) && !chosen.iter().any(|c| &c.machine == *n)).cloned().collect();
            for named in missing {
                if chosen.len() < self.want {
                    break;
                }
                if let Some(i) = chosen.iter().rposition(|c| !self.first.contains(&c.machine)) {
                    let was = chosen.remove(i);
                    tracing::info!(machine = %named, instead_of = %was.machine, "a runner named to keep a copy of the conversation store takes the place of another");
                }
            }
            for c in ranked(all.clone(), &self.first) {
                if chosen.len() >= self.want {
                    break;
                }
                if !chosen.iter().any(|x| x.machine == c.machine) {
                    tracing::info!(machine = %c.machine, "this runner keeps a copy of the conversation store");
                    chosen.push(Chosen { machine: c.machine.clone(), chosen_at: now, last_seen: now });
                }
            }
            if before.iter().map(|c| &c.machine).ne(chosen.iter().map(|c| &c.machine)) || before.iter().map(|c| c.last_seen / 60_000).ne(chosen.iter().map(|c| c.last_seen / 60_000)) {
                self.save(&chosen);
            }
            chosen.clone()
        };
        for c in &picked {
            if let Some(a) = all.iter().find(|a| a.machine == c.machine) {
                let (shipper, peer) = self.feed(&c.machine);
                let mut at = peer.lock().unwrap_or_else(PoisonError::into_inner);
                if *at != Some(a.peer) {
                    *at = Some(a.peer);
                    shipper.link(Link::Up);
                }
            }
        }
        // A machine no longer picked: nothing more goes to it.
        let mut feeds = self.feeds.lock().unwrap_or_else(PoisonError::into_inner);
        feeds.retain(|m, f| {
            let keep = picked.iter().any(|c| &c.machine == m);
            if !keep {
                f.shipper.link(Link::Down);
            }
            keep
        });
    }

    /// Runner process `peer` of `machine` went away.
    pub fn disconnected(&self, machine: &str, peer: PeerId) {
        let feeds = self.feeds.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(f) = feeds.get(machine) {
            let mut at = f.peer.lock().unwrap_or_else(PoisonError::into_inner);
            if *at == Some(peer) {
                *at = None;
                f.shipper.link(Link::Down);
            }
        }
    }

    /// What mirror `machine` said of the server's store.
    pub fn heard(&self, machine: &str, msg: Msg) {
        let feeds = self.feeds.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(f) = feeds.get(machine) {
            f.shipper.link(Link::Heard(msg));
        }
    }

    pub fn view(&self) -> Vec<MirrorView> {
        let feeds = self.feeds.lock().unwrap_or_else(PoisonError::into_inner);
        let connected = |m: &str| feeds.get(m).is_some_and(|f| f.peer.lock().unwrap_or_else(PoisonError::into_inner).is_some());
        self.chosen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|c| MirrorView { machine: c.machine.clone(), connected: connected(&c.machine), chosen_at: c.chosen_at, last_seen: c.last_seen })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(machine: &str, connected_at: i64, disk_free: u64) -> Candidate {
        Candidate { machine: machine.into(), peer: 1, connected_at, disk_free }
    }

    #[test]
    fn named_runners_come_first_then_the_longest_connected_then_the_most_disk() {
        let all = vec![cand("laptop", 900, 900), cand("rack-b", 100, 50), cand("rack-a", 100, 500), cand("named", 950, 10)];
        let order: Vec<String> = ranked(all.clone(), &["named".into()]).into_iter().map(|c| c.machine).collect();
        assert_eq!(order, vec!["named", "rack-a", "rack-b", "laptop"]);
        let order: Vec<String> = ranked(all, &[]).into_iter().map(|c| c.machine).collect();
        assert_eq!(order, vec!["rack-a", "rack-b", "laptop", "named"]);
    }

    #[test]
    fn a_runner_named_first_takes_the_place_of_one_that_was_not() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (dir, store) = super::super::store::scratch()?;
            let (m, _out) = Mirrors::start(store, 1, vec!["named".into()], dir.path().join("mirrors.json"), None);
            // The first to connect keeps the copy until the one named comes.
            m.connected(vec![cand("early", 100, 10)]);
            assert_eq!(m.view().iter().map(|v| v.machine.as_str()).collect::<Vec<_>>(), vec!["early"]);
            m.connected(vec![cand("early", 100, 10), cand("named", 200, 10)]);
            assert_eq!(m.view().iter().map(|v| v.machine.as_str()).collect::<Vec<_>>(), vec!["named"]);
            Ok(())
        })
    }

    #[test]
    fn mirrors_are_picked_kept_and_replaced_after_a_day_away() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (dir, store) = super::super::store::scratch()?;
            let file = dir.path().join("mirrors.json");
            let (m, _out) = Mirrors::start(store.clone(), 1, Vec::new(), file.clone(), None);
            m.connected(vec![cand("b", 200, 10), cand("a", 100, 10)]);
            assert_eq!(m.view().iter().map(|v| v.machine.as_str()).collect::<Vec<_>>(), vec!["a"]);
            // Kept across a restart of the server, and while it's away (a day at most).
            let (m2, _out2) = Mirrors::start(store.clone(), 1, Vec::new(), file.clone(), None);
            m2.connected(vec![cand("b", 200, 10)]);
            assert_eq!(m2.view().iter().map(|v| (v.machine.as_str(), v.connected)).collect::<Vec<_>>(), vec![("a", false)]);
            // Away for longer: replaced.
            let stale = vec![Chosen { machine: "a".into(), chosen_at: 0, last_seen: crate::util::now_ms() - AWAY_MS - 1 }];
            std::fs::write(&file, serde_json::to_vec(&stale)?)?;
            let (m3, _out3) = Mirrors::start(store, 1, Vec::new(), file, None);
            m3.connected(vec![cand("b", 200, 10)]);
            assert_eq!(m3.view().iter().map(|v| (v.machine.as_str(), v.connected)).collect::<Vec<_>>(), vec![("b", true)]);
            Ok(())
        })
    }
}
