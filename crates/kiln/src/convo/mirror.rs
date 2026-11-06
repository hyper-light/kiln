//! Mirrors (`docs/conversations.md`, "Mirrors"): runners that keep a copy of the server's store,
//! so a conversation outlives the server's disk and its own machine both. The server picks them
//! (`[conversations] mirrors`) and sends each its store the way a runner sends the server its own:
//! the same feed (`super::feed`), the other way, on the same lane. A mirror keeps the copy beside
//! its own store (`mirror.db`).
//!
//! Which runners: those named first (`mirror_on`), then the ones connected longest (a machine that
//! sleeps reconnects often), then the most free disk. One away for a day is replaced; the copy it
//! has stays where it is.
//!
//! Each batch to a mirror goes signed by the server (docs/ci.md W27-R08), once the server gives
//! its mirrors a custodian for each ([`Mirrors::sign_with`]): its hand-on to the runner process
//! keeping the copy there now, which signs its receipt as it takes it (`super::feed::Custodian`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use hyper_transport::PeerId;
use serde::{Deserialize, Serialize};

use super::feed::{Custodian, Link, Msg, Shipper};
use super::store::Store;

/// Which runner process of a mirror's machine keeps its copy now: read for each part its feed sends and
/// each batch its custodian hands on, set as one connects or goes, with no lock (goal 5, plan
/// 01a1a59c2aae, its top goal: a `Mutex` each feed's task and the hub's loop took).
pub type Peer = Arc<arc_swap::ArcSwapOption<PeerId>>;

/// The custodian of each mirror's feed: made for its machine, which runner process of it
/// (`peer`) keeps the copy now said as it changes.
pub type Custodians = Arc<dyn Fn(&str, Peer) -> Arc<dyn Custodian> + Send + Sync>;

/// A mirror away this long is replaced.
const AWAY_MS: i64 = 24 * 3600 * 1000;

/// A runner that could keep a copy: connected, keeping a store of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub machine: String,
    /// The name people see, for a runner kiln knows by its machine id (`machine`): `mirror_on`
    /// names it by this too (docs/machines.md).
    pub label: Option<String>,
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
#[derive(Clone)]
struct Feed {
    peer: Peer,
    shipper: Shipper,
}

/// What the server sends a mirror: the link it goes on, the message's JSON and the bytes after it (a
/// batch's part, up to `super::feed::PART`).
pub type Out = (PeerId, String, Vec<u8>);

impl crate::memory::queue::Weigh for Out {
    /// Its place, its JSON and the bytes it carries.
    fn weight(&self) -> usize {
        size_of::<Out>().saturating_add(self.1.len()).saturating_add(self.2.len())
    }
}

/// The bytes waiting to go to mirrors, at most (goal 11, plan 01a1a87b2e56 step 3): as many as the net
/// thread's queue they're sent into ([`crate::net::OUT_BYTES`]), past which each mirror's feed waits, as it
/// did past 64 messages of up to 3 MiB (192 MiB). Each feed sends at most [`super::feed::WINDOW`] batches
/// ahead of what its mirror has acknowledged.
pub const OUT_BYTES: usize = crate::net::OUT_BYTES;

/// The server's mirrors.
pub struct Mirrors {
    store: Store,
    want: usize,
    first: Vec<String>,
    file: PathBuf,
    /// Taken as runners connect and the mirrors are picked again ([`Mirrors::connected`]), and for
    /// telemetry: never for a message.
    chosen: Mutex<Vec<Chosen>>,
    /// Each chosen machine's feed: read for each message a mirror sends ([`Mirrors::heard`]) with no lock, a
    /// copy swapped in whole as one's made or let go of (`crate::rcu`; goal 5, plan 01a1a59c2aae, its top
    /// goal). Made and let go of under `chosen`'s lock, so by one caller at a time.
    feeds: crate::rcu::Rcu<HashMap<String, Feed>>,
    out: crate::memory::queue::Sender<Out>,
    /// What each mirror sent back to restore the store from: not sent to it again.
    echoes: Option<Arc<super::feed::Echoes>>,
    /// Who signs for each feed, once the server can ([`Mirrors::sign_with`]).
    custodians: crate::rcu::Rcu<Option<Custodians>>,
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
    pub fn start(store: Store, want: usize, first: Vec<String>, file: PathBuf, echoes: Option<Arc<super::feed::Echoes>>) -> (Arc<Self>, crate::memory::queue::Receiver<Out>) {
        let chosen: Vec<Chosen> = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let (out, rx) = crate::memory::queue::bounded::<Out>(&crate::memory::owners::MIRRORS_OUT, OUT_BYTES);
        (Arc::new(Self { store, want, first, file, chosen: Mutex::new(chosen), feeds: crate::rcu::Rcu::default(), out, echoes, custodians: crate::rcu::Rcu::new(None) }), rx)
    }

    /// Each mirror's feed goes signed by the custodian `make` makes for it from now on (those made
    /// already, too).
    pub fn sign_with(&self, make: Custodians) {
        // Set first: a feed made meanwhile signs itself with it, or is among those found here.
        self.custodians.set(Some(Arc::clone(&make)));
        for (machine, f) in self.feeds.load().iter() {
            f.shipper.sign_with(make(machine, Arc::clone(&f.peer)));
        }
    }

    /// The machines picked to keep a copy (`mirrors.json`).
    pub fn chosen_machines(&self) -> Vec<String> {
        self.chosen.lock().unwrap_or_else(PoisonError::into_inner).iter().map(|c| c.machine.clone()).collect()
    }

    /// The store the mirrors keep a copy of: messages about it are theirs.
    pub fn store_id(&self) -> &str {
        self.store.id()
    }

    /// Which runners were chosen, kept as `mirrors.json`, whole and synced to the disk, off the caller's
    /// thread (`crate::keep::synced`): a runner's Build picks them again on the hub's loop, whose shard
    /// waited on the disk for the file and its folder, 5 to 34 ms in the lab's g9p6 (goal 9, plan
    /// 01a1a87b2cec step 3). Kept in the order chosen, the newest written.
    fn save(&self, chosen: &[Chosen]) {
        match serde_json::to_vec(chosen) {
            Ok(b) => crate::keep::synced(self.file.clone(), b),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "couldn't keep which runners mirror the conversation store"),
        }
    }

    /// The feed to `machine`, made when first needed: by [`Mirrors::connected`] under `chosen`'s lock, so
    /// one at a time.
    fn feed(&self, machine: &str) -> Feed {
        if let Some(f) = self.feeds.load().get(machine) {
            return f.clone();
        }
        let (shipper, said) = match self.echoes.clone() {
            Some(echoes) => {
                let m = machine.to_owned();
                Shipper::start_skipping(self.store.clone(), Arc::new(move |rev| echoes.has(&m, rev)))
            }
            None => Shipper::start(self.store.clone()),
        };
        let peer: Peer = Arc::default();
        let (to, out) = (Arc::clone(&peer), self.out.clone());
        crate::rt::spawn(async move {
            while let Ok((json, data)) = said.recv().await {
                let at = to.load().as_deref().copied();
                if let Some(p) = at
                    && out.send((p, json, data)).await.is_err()
                {
                    break;
                }
            }
        });
        let f = Feed { peer, shipper };
        self.feeds.update(|feeds| {
            feeds.insert(machine.to_owned(), f.clone());
        });
        // Signed once the server can (`sign_with` sets its custodians before it signs the feeds it finds,
        // and this one is found from now on). Not linked yet, so nothing went unsigned.
        if let Some(make) = Option::clone(&self.custodians.load()) {
            f.shipper.sign_with(make(machine, Arc::clone(&f.peer)));
        }
        f
    }

    /// A runner process keeping a store connected (`connected`, among the `all` connected now):
    /// the mirrors are picked again as needed, and each connected one is fed.
    pub fn connected(&self, all: Vec<Candidate>) {
        if self.want == 0 {
            return;
        }
        // The runners named first, by what kiln knows them by: a name that is a runner's label is that
        // runner.
        let first: Vec<String> = self.first.iter().map(|n| all.iter().find(|a| a.label.as_deref() == Some(n.as_str())).map_or_else(|| n.clone(), |a| a.machine.clone())).collect();
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
            let missing: Vec<String> = first.iter().filter(|n| all.iter().any(|a| &a.machine == *n) && !chosen.iter().any(|c| &c.machine == *n)).cloned().collect();
            for named in missing {
                if chosen.len() < self.want {
                    break;
                }
                if let Some(i) = chosen.iter().rposition(|c| !first.contains(&c.machine)) {
                    let was = chosen.remove(i);
                    tracing::info!(machine = %named, instead_of = %was.machine, "a runner named to keep a copy of the conversation store takes the place of another");
                }
            }
            for c in ranked(all.clone(), &first) {
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
            chosen
        };
        // Still under `chosen`'s lock: feeds are made and let go of by one caller at a time.
        for c in picked.iter() {
            if let Some(a) = all.iter().find(|a| a.machine == c.machine) {
                let f = self.feed(&c.machine);
                let before = f.peer.swap(Some(Arc::new(a.peer)));
                if before.as_deref() != Some(&a.peer) {
                    f.shipper.link(Link::Up);
                }
            }
        }
        // A machine no longer picked: nothing more goes to it.
        let gone = self.feeds.update(|feeds| {
            let gone: Vec<Feed> = feeds.iter().filter(|(m, _)| !picked.iter().any(|c| &c.machine == *m)).map(|(_, f)| f.clone()).collect();
            feeds.retain(|m, _| picked.iter().any(|c| &c.machine == m));
            gone
        });
        drop(picked);
        for f in gone {
            f.shipper.link(Link::Down);
        }
    }

    /// Runner process `peer` of `machine` went away.
    pub fn disconnected(&self, machine: &str, peer: PeerId) {
        if let Some(f) = self.feeds.load().get(machine) {
            // Only while it's that process that keeps the copy: another of the machine's may have since.
            let before = f.peer.rcu(|at| if at.as_deref() == Some(&peer) { None } else { at.clone() });
            if before.as_deref() == Some(&peer) {
                f.shipper.link(Link::Down);
            }
        }
    }

    /// What mirror `machine` said of the server's store.
    pub fn heard(&self, machine: &str, msg: Msg) {
        if let Some(f) = self.feeds.load().get(machine) {
            f.shipper.link(Link::Heard(msg));
        }
    }

    /// Where the feed to each chosen mirror stands, for telemetry.
    pub fn feeds(&self) -> Vec<(MirrorView, super::telemetry::ShipView)> {
        let feeds = self.feeds.load();
        self.chosen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|c| {
                let f = feeds.get(&c.machine);
                let connected = f.is_some_and(|f| f.peer.load().is_some());
                let view = MirrorView { machine: c.machine.clone(), connected, chosen_at: c.chosen_at, last_seen: c.last_seen };
                (view, f.map(|f| f.shipper.stats()).unwrap_or_default())
            })
            .collect()
    }

    /// How many mirrors the server keeps.
    pub fn wanted(&self) -> usize {
        self.want
    }

    pub fn view(&self) -> Vec<MirrorView> {
        let feeds = self.feeds.load();
        let connected = |m: &str| feeds.get(m).is_some_and(|f| f.peer.load().is_some());
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
        Candidate { machine: machine.into(), label: None, peer: 1, connected_at, disk_free }
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): a mirror's feed and the runner process it goes to are read
    /// with no lock now. Its parts go to the process that keeps the copy now, a connect of that same process
    /// starts nothing over, another process's going leaves it be, and its own going stops it until one
    /// connects again. Its words reach its feed whatever shard they come on.
    #[test]
    fn g5_a_mirrors_feed_goes_to_the_process_that_keeps_its_copy_now() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (dir, store) = super::super::store::scratch()?;
            let (m, out) = Mirrors::start(store, 1, Vec::new(), dir.path().join("mirrors.json"), None);
            let at = |peer: PeerId| Candidate { machine: "m1".into(), label: None, peer, connected_at: 1, disk_free: 1 };
            // The first part a feed sends as its link comes up is its hello.
            let next = || async { crate::rt::timeout(std::time::Duration::from_secs(5), out.recv()).await.ok().and_then(Result::ok) };
            let quiet = || async { crate::rt::timeout(std::time::Duration::from_millis(300), out.recv()).await.is_err() };
            m.connected(vec![at(5)]);
            let first = next().await.ok_or_else(|| anyhow::anyhow!("nothing went to the mirror"))?;
            anyhow::ensure!(first.0 == 5 && matches!(super::super::feed::decode(&first.1), Some(Msg::Hello { .. })), "{first:?}");
            m.connected(vec![at(5)]);
            anyhow::ensure!(quiet().await, "the same process connected again: its feed started over");
            m.disconnected("m1", 9);
            anyhow::ensure!(m.view().first().is_some_and(|v| v.connected), "another process's going took the feed's");
            m.disconnected("m1", 5);
            anyhow::ensure!(m.view().first().is_some_and(|v| !v.connected), "its own process's going left it connected");
            m.connected(vec![at(6)]);
            let again = next().await.ok_or_else(|| anyhow::anyhow!("nothing went to its new process"))?;
            anyhow::ensure!(again.0 == 6 && matches!(super::super::feed::decode(&again.1), Some(Msg::Hello { .. })), "{again:?}");
            // Its words, from every shard at once, reach its feed (whatever they say, none is refused).
            let mut hearing = Vec::new();
            for rev in 0..8u64 {
                let m = std::sync::Arc::clone(&m);
                let store_id = m.store_id().to_owned();
                hearing.push(crate::rt::spawn_spread_joined(async move { m.heard("m1", Msg::Ack { store: store_id, rev }) }));
            }
            for h in hearing {
                h.await.map_err(|_| anyhow::anyhow!("a word was never heard"))?;
            }
            Ok(())
        })
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
            // Written before the test's folder goes.
            kept(&dir.path().join("mirrors.json"), &["named"]).await?;
            Ok(())
        })
    }

    /// The chosen as `file` holds them, once the writer has written them (`crate::keep`).
    async fn kept(file: &std::path::Path, machines: &[&str]) -> anyhow::Result<()> {
        for _ in 0..500 {
            let held: Vec<Chosen> = std::fs::read(file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
            if held.iter().map(|c| c.machine.as_str()).eq(machines.iter().copied()) {
                return Ok(());
            }
            crate::rt::sleep(std::time::Duration::from_millis(10)).await;
        }
        anyhow::bail!("{} doesn't hold {machines:?}: {:?}", file.display(), std::fs::read_to_string(file))
    }

    #[test]
    fn mirrors_are_picked_kept_and_replaced_after_a_day_away() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (dir, store) = super::super::store::scratch()?;
            let file = dir.path().join("mirrors.json");
            let (m, _out) = Mirrors::start(store.clone(), 1, Vec::new(), file.clone(), None);
            m.connected(vec![cand("b", 200, 10), cand("a", 100, 10)]);
            assert_eq!(m.view().iter().map(|v| v.machine.as_str()).collect::<Vec<_>>(), vec!["a"]);
            kept(&file, &["a"]).await?;
            // Kept across a restart of the server, and while it's away (a day at most).
            let (m2, _out2) = Mirrors::start(store.clone(), 1, Vec::new(), file.clone(), None);
            m2.connected(vec![cand("b", 200, 10)]);
            assert_eq!(m2.view().iter().map(|v| (v.machine.as_str(), v.connected)).collect::<Vec<_>>(), vec![("a", false)]);
            // Away for longer: replaced.
            let stale = vec![Chosen { machine: "a".into(), chosen_at: 0, last_seen: crate::util::now_ms() - AWAY_MS - 1 }];
            std::fs::write(&file, serde_json::to_vec(&stale)?)?;
            let (m3, _out3) = Mirrors::start(store, 1, Vec::new(), file.clone(), None);
            m3.connected(vec![cand("b", 200, 10)]);
            assert_eq!(m3.view().iter().map(|v| (v.machine.as_str(), v.connected)).collect::<Vec<_>>(), vec![("b", true)]);
            kept(&file, &["b"]).await?;
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: the mirrors chosen as a runner's Build comes (on the hub's loop) are
    /// kept by the writer, synced, off the caller's thread, which makes no file call for them; a choice made
    /// while the last is on its way is written as the newest.
    #[test]
    fn g9_s3_the_mirrors_chosen_are_kept_off_the_callers_thread() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (dir, store) = super::super::store::scratch()?;
            let file = dir.path().join("mirrors.json");
            let (m, _out) = Mirrors::start(store, 2, Vec::new(), file.clone(), None);
            m.connected(vec![cand("a", 100, 10)]);
            anyhow::ensure!(crate::keep::kept_through(&file) == Some(true), "the choice goes to the writer, to be written synced");
            m.connected(vec![cand("a", 100, 10), cand("b", 200, 10)]);
            kept(&file, &["a", "b"]).await?;
            // The code that chooses makes no file call of its own to keep them: the writer's alone.
            let code: String = include_str!("mirror.rs").split("\n#[cfg(test)]\nmod tests {").next().unwrap_or_default().lines().map(|l| l.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n");
            for call in ["save_atomic", "fs::write", "sync_all", "File::create"] {
                anyhow::ensure!(!code.contains(call), "mirror.rs calls {call}");
            }
            Ok(())
        })
    }
}
