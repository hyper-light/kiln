//! A runner's learner of the cluster group (docs/p2p.md, P2): its replica of kiln's records,
//! `<state>/cluster/kiln.db`, which the server's member feeds over the runner's link, and the
//! messages between them ([`Msg`], on [`crate::proto::LANE_SYNC`] beside repo sync's). The replica
//! starts from a copy of kiln.db fetched over HTTP (`GET /api/cluster/copy`) and follows the log.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::group::{Fetch, Group};
use crate::http::client::Client;
use crate::proto::AgentMsg;
use crate::sync::wire::Joiner;

/// What the server and a runner say of the cluster group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Msg {
    /// Server to runner: hold a replica as `member` of the group, fetching copies of kiln.db with
    /// `token`.
    Join { member: u64, token: String },
    /// Server to runner: hold none now (it's no longer a learner); the replica stays on disk.
    Leave,
    /// A hyper-raft message of the group, its bytes beside: whole (`parts` 0), or part `part` of
    /// the `parts` of the one numbered `id`.
    Raft { id: u64, part: u32, parts: u32 },
    /// Runner to server: the last write kiln.db holds now? (Before a read the runner answers: it
    /// waits until its replica holds that write too.)
    Ask { id: u64 },
    /// Server to runner: kiln.db's last write when asked `id`.
    At { id: u64, seq: u64 },
}

pub fn encode(m: &Msg) -> String {
    serde_json::to_string(m).unwrap_or_default()
}

pub fn decode(json: &str) -> Option<Msg> {
    serde_json::from_str(json).ok()
}

/// A raft message's bytes in one frame, at the most: a bigger one goes in parts.
const PART: usize = 512 << 10;

/// A raft message's frames: one, or the parts of a bigger one, numbered `id`.
pub fn frames(id: u64, bytes: Vec<u8>) -> Vec<(Msg, Vec<u8>)> {
    if bytes.len() <= PART {
        return vec![(Msg::Raft { id: 0, part: 0, parts: 0 }, bytes)];
    }
    let parts = u32::try_from(bytes.chunks(PART).count()).unwrap_or(u32::MAX);
    bytes.chunks(PART).zip(0..).map(|(chunk, part)| (Msg::Raft { id, part, parts }, chunk.to_vec())).collect()
}

/// The token a runner fetches copies of kiln.db with: signed by the server's key, for it.
pub fn token(key: &str, machine: &str) -> String {
    crate::util::hmac_hex(key.as_bytes(), format!("cluster:{machine}").as_bytes())
}

/// This process's learner, when it holds one (the front door answers reads from it).
static GLOBAL: parking_lot::RwLock<Option<Arc<Learner>>> = parking_lot::RwLock::new(None);

/// Says this process holds `learner` (or none now).
pub fn set_global(learner: Option<Arc<Learner>>) {
    *GLOBAL.write() = learner;
}

/// This process's learner, if it holds one.
pub fn global() -> Option<Arc<Learner>> {
    GLOBAL.read().clone()
}

/// Where a runner keeps its replica and its log.
pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("cluster")
}

/// A runner's learner: its replica, and the parts of the server's messages coming in.
pub struct Learner {
    group: Arc<Group>,
    joiner: parking_lot::Mutex<Joiner>,
    /// The replica: kiln's records as the runner holds them.
    pub db: PathBuf,
    /// The replica, to read (its learner alone writes it).
    store: super::Store,
    /// This machine's name.
    me: String,
    /// What it says to the server.
    say: async_channel::Sender<AgentMsg>,
    /// Questions to the server of where kiln.db stands, waiting for their answers.
    asks: parking_lot::Mutex<std::collections::HashMap<u64, async_channel::Sender<u64>>>,
    next_ask: std::sync::atomic::AtomicU64,
}

impl Learner {
    /// Starts learner `member` on its replica in `state_dir`, fetching a copy of kiln.db from
    /// `server` (as `me`, with `token`) when it has none; what it says to the server comes out of
    /// the receiver.
    pub fn start(state_dir: &Path, me: &str, member: u64, http: Arc<Client>, server: &str, token: &str) -> anyhow::Result<(Learner, async_channel::Receiver<AgentMsg>)> {
        let d = dir(state_dir);
        let db = d.join("kiln.db");
        let group = Arc::new(Group::learner(&db, &d.join("log"), member, fetcher(http, copy_url(server, me), token.to_owned()))?);
        Self::around(group, db, me)
    }

    /// A learner around its member of the group, on its replica at `db`.
    pub fn around(group: Arc<Group>, db: PathBuf, me: &str) -> anyhow::Result<(Learner, async_channel::Receiver<AgentMsg>)> {
        let store = super::Store::open_replica(&db)?;
        let (say_tx, say_rx) = async_channel::bounded(1024);
        let out = group.outgoing();
        let say = say_tx.clone();
        crate::rt::spawn(async move {
            let mut id = 0u64;
            while let Ok((_, bytes)) = out.recv().await {
                id = id.wrapping_add(1);
                for (msg, raft) in frames(id, bytes) {
                    if say_tx.send(AgentMsg::Cluster { json: encode(&msg), raft }).await.is_err() {
                        return;
                    }
                }
            }
        });
        let learner = Learner {
            group,
            joiner: parking_lot::Mutex::new(Joiner::default()),
            db,
            store,
            me: me.to_owned(),
            say,
            asks: parking_lot::Mutex::new(std::collections::HashMap::new()),
            next_ask: std::sync::atomic::AtomicU64::new(1),
        };
        Ok((learner, say_rx))
    }

    /// The replica, to read.
    pub fn store(&self) -> &super::Store {
        &self.store
    }

    /// This machine's name.
    pub fn machine(&self) -> &str {
        &self.me
    }

    /// Waits until the replica holds every write kiln.db held when asked (the server says which),
    /// within `within`: whether it does. A read answered after it is as current as the server's.
    pub async fn current(&self, within: Duration) -> bool {
        let id = self.next_ask.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = async_channel::bounded(1);
        self.asks.lock().insert(id, tx);
        let asked = async {
            self.say.send(AgentMsg::Cluster { json: encode(&Msg::Ask { id }), raft: Vec::new() }).await.ok()?;
            let seq = rx.recv().await.ok()?;
            while self.standing().0 < seq {
                crate::rt::sleep(Duration::from_millis(2)).await;
            }
            Some(())
        };
        let done = matches!(crate::rt::timeout(within, asked).await, Ok(Some(())));
        self.asks.lock().remove(&id);
        done
    }

    /// What the server said: a raft message, or a part of one, for the replica.
    pub fn heard(&self, msg: &Msg, raft: Vec<u8>) {
        if let Msg::At { id, seq } = *msg {
            let waiting = self.asks.lock().remove(&id);
            if let Some(tx) = waiting {
                tx.try_send(seq).ok();
            }
            return;
        }
        let Msg::Raft { id, part, parts } = *msg else { return };
        let Some(bytes) = self.joiner.lock().add("server", id, part, parts, raft) else { return };
        if let Err(e) = self.group.step(&bytes) {
            tracing::debug!("the cluster group's learner didn't take a message: {e:#}");
        }
    }

    /// Where the replica stands: its last write, and the last entry applied.
    pub fn standing(&self) -> (u64, u64) {
        self.group.standing()
    }
}

/// The server's side of the runners' learners: which machine each is, and the parts of their
/// messages coming in.
#[derive(Default)]
pub struct Learners {
    /// Each learner's machine, by its member: those that said they hold one since the server
    /// started (the group's other learners are away).
    machines: parking_lot::Mutex<std::collections::HashMap<u64, String>>,
    joiner: parking_lot::Mutex<Joiner>,
    next: std::sync::atomic::AtomicU64,
}

impl Learners {
    /// `machine`'s runner holds a replica: it's a learner of the group (added if it's new; the
    /// learners the group has stay, away or not). Its member.
    pub fn announced(&self, group: &Group, machine: &str) -> u64 {
        let member = super::group::member_of(machine);
        let mut wanted: std::collections::BTreeSet<u64> = group.point().1.learners.iter().copied().collect();
        {
            let mut m = self.machines.lock();
            m.insert(member, machine.to_owned());
            wanted.extend(m.keys().copied());
        }
        group.learners(wanted);
        member
    }

    /// The machine of learner `member`, if it said so since the server started.
    pub fn machine(&self, member: u64) -> Option<String> {
        self.machines.lock().get(&member).cloned()
    }

    /// A raft message from `machine`'s learner, or a part of one: to the group once whole.
    pub fn heard(&self, group: &Group, machine: &str, msg: &Msg, raft: Vec<u8>) {
        let Msg::Raft { id, part, parts } = *msg else { return };
        let Some(bytes) = self.joiner.lock().add(machine, id, part, parts, raft) else { return };
        if let Err(e) = group.step(&bytes) {
            tracing::debug!(machine, "the cluster group didn't take a learner's message: {e:#}");
        }
    }

    /// A message to a learner, in its frames.
    pub fn frames(&self, bytes: Vec<u8>) -> Vec<(Msg, Vec<u8>)> {
        frames(self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed), bytes)
    }
}

fn copy_url(server: &str, me: &str) -> String {
    let machine: String = me.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") }).collect();
    format!("{}/api/cluster/copy?machine={machine}", server.trim_end_matches('/'))
}

/// A learner's way to a copy of kiln.db: the server's, over HTTP, into a file, whole or not at all.
fn fetcher(http: Arc<Client>, url: String, token: String) -> Fetch {
    Arc::new(move |dest: &Path| {
        let (http, url, token, dest) = (http.clone(), url.clone(), token.clone(), dest.to_path_buf());
        crate::rt::block_on(async move {
            let mut resp = http.get(&url).bearer(&token).timeout(Duration::from_secs(3600)).send().await?;
            anyhow::ensure!(resp.is_success(), "the server answered {}", resp.status());
            let length = resp.header("content-length").and_then(|v| v.parse::<u64>().ok());
            std::fs::remove_file(&dest).ok();
            let (sink, done) = crate::rt::file_sink(dest.clone(), 1 << 40)?;
            let mut broke = None;
            loop {
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        if sink.send(chunk).await.is_err() {
                            broke = Some("writing it failed".to_owned());
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        broke = Some(format!("{e:#}"));
                        break;
                    }
                }
            }
            drop(sink);
            let wrote = done.await;
            let whole = match (&broke, &wrote) {
                (None, Ok(n)) => length.is_none_or(|l| l == *n),
                _ => false,
            };
            if !whole {
                std::fs::remove_file(&dest).ok();
                anyhow::bail!("the copy broke off: {}", broke.or_else(|| wrote.err().map(|e| format!("{e:#}"))).unwrap_or_else(|| "short".to_owned()));
            }
            Ok(())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_big_message_goes_in_parts_and_comes_back_whole() {
        let bytes: Vec<u8> = (0..(PART * 2 + 17)).map(|i| (i % 251) as u8).collect();
        let sent = frames(9, bytes.clone());
        assert_eq!(sent.len(), 3);
        let mut j = Joiner::default();
        let mut got = None;
        for (msg, raft) in sent.into_iter().rev() {
            assert!(matches!(msg, Msg::Raft { .. }), "a raft frame");
            if let Msg::Raft { id, part, parts } = msg {
                got = j.add("server", id, part, parts, raft).or(got);
            }
        }
        assert_eq!(got, Some(bytes));
        assert_eq!(decode(&encode(&Msg::Join { member: 7, token: "t".into() })), Some(Msg::Join { member: 7, token: "t".into() }));
    }
}
