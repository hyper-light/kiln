//! A runner's learner of the cluster group (docs/p2p.md, P2): its replica of kiln's records,
//! `<state>/cluster/kiln.db`, which the server's member feeds over the runner's link, and the
//! messages between them ([`Msg`], on [`crate::proto::LANE_SYNC`] beside repo sync's). The replica
//! starts from a copy of kiln.db fetched over HTTP (`GET /api/cluster/copy`) and follows the log.
//!
//! **Custody (docs/ci.md W27-R08; `crate::custody::cluster`).** kiln hands each copy on by its
//! SHA-256, and the learner hashes the copy before it takes it: as handed on, it signs its receipt;
//! else it refuses it, its receipt of what came is the break kiln records, and it fetches another.
//! A learner that can sign says it checks the log's checkpoints ([`Msg::Ask`]'s `checks`): kiln
//! sends it one every [`CHECK_EVERY`] while entries flow ([`Msg::Checkpoint`], [`Periods`]), the
//! learner checks each against the leaves of the entries it applied, signs its receipt of the one
//! closing each period (at most once a [`RECEIPT_EVERY`]), and on one that doesn't match records the
//! break at once and copies the database again.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use kiln_ci::identity::statement::Subject;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use super::group::{Fetch, Group, Short};
use crate::http::client::Client;
use crate::instances::PeerId;
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
    /// waits until its replica holds that write too.) With `checks`, the learner checks the log's
    /// checkpoints (docs/ci.md W27-R08), holding the leaves of the entries after that one; a server
    /// from before reads the ask without it, and sends none.
    Ask {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checks: Option<u64>,
    },
    /// Server to runner: kiln.db's last write when asked `id`.
    At { id: u64, seq: u64 },
    /// Server to runner: a checkpoint of the log (docs/ci.md W27-R08): kiln's hand-on (a Sigstore
    /// bundle, as JSON) of entries `from + 1` to `to`, by the SHA-256 of their leaves, to the runner
    /// process holding the replica; `closes` the last of its period, which the learner signs its
    /// receipt of. Only to a learner that said it checks them.
    Checkpoint {
        from: u64,
        to: u64,
        #[serde(default)]
        closes: bool,
        hand_on: String,
    },
}

pub fn encode(m: &Msg) -> String {
    serde_json::to_string(m).unwrap_or_default()
}

pub fn decode(json: &str) -> Option<Msg> {
    serde_json::from_str(json).ok()
}

/// A raft message's bytes in one frame, at the most: a bigger one goes in parts.
const PART: usize = 512 << 10;

/// How often kiln sends each learner that checks them a checkpoint of the log, while entries flow
/// (docs/ci.md W27-R08: Ada's cadence, 2026-11-03). None while none flow.
pub const CHECK_EVERY: Duration = Duration::from_secs(10);
/// How long a learner's period of checkpoints lasts at the least: it signs its receipt of the one
/// closing it, so at most one a minute.
pub const RECEIPT_EVERY: Duration = Duration::from_secs(60);
/// How long a learner waits to have applied the entries a checkpoint runs to before it gives up
/// checking that one (the next of its period runs over them again).
const CHECK_WAIT: Duration = Duration::from_secs(60);
/// Checkpoints waiting to be checked, at the most: past it, one is let go.
const CHECKS_QUEUED: usize = 64;
/// How long a learner starting waits for its process to sign custody records (its identity
/// certified, kiln's word that it takes them) before it fetches a copy or says it checks: without
/// it, the copy goes as before, unsigned.
const SIGNER_WAIT: Duration = Duration::from_secs(20);
/// How many copies a learner fetches while each isn't what kiln handed on, before it gives up.
const COPY_TRIES: u32 = 3;

/// A raft message's frames: one, or the parts of a bigger one, numbered `id`.
pub fn frames(id: u64, bytes: Vec<u8>) -> Vec<(Msg, Vec<u8>)> {
    parts(id, bytes).collect()
}

/// [`frames`], each made as it's asked for: a long message's parts copied one at a time, so whoever sends
/// them can give its shard up between them (goal 9, plan 01a1a87b2cec step 3: the hub's cluster loop copied
/// every part of a learner's catch-up, then sent them, in one poll, up to 5.7 ms in the lab's g6c2). One that
/// fits a frame goes whole, uncopied.
pub fn parts(id: u64, bytes: Vec<u8>) -> impl Iterator<Item = (Msg, Vec<u8>)> + Send {
    let whole = bytes.len() <= PART;
    let parts = u32::try_from(bytes.len().div_ceil(PART)).unwrap_or(u32::MAX);
    let (mut left, mut part) = (Some(bytes), 0u32);
    std::iter::from_fn(move || {
        if whole {
            return left.take().map(|b| (Msg::Raft { id: 0, part: 0, parts: 0 }, b));
        }
        let start = usize::try_from(part).ok()?.checked_mul(PART)?;
        let rest = left.as_ref()?.get(start..).filter(|r| !r.is_empty())?;
        let chunk = rest.get(..PART.min(rest.len()))?.to_vec();
        let this = part;
        part = part.saturating_add(1);
        Some((Msg::Raft { id, part: this, parts }, chunk))
    })
}

/// A message to a learner in its frames, each made as it's asked for ([`Learners::frames`]).
pub type Frames = Box<dyn Iterator<Item = (Msg, Vec<u8>)> + Send>;

/// A message's frames as the server sends them on the sync lane: each a [`crate::proto::ServerMsg::Cluster`], its
/// words beside the part of the group's message it carries, a whole message's words written once by the caller
/// (`whole`: goal 9, plan 01a1a87b2cec step 5). Each a copy of up to [`PART`] of the message: made on the compute pool
/// (the hub's cluster loop, `crate::net::send_made`; goal 9, plan 01a1a87b2cec step 3).
pub fn encoded(frames: Frames, whole: &str) -> Vec<Vec<u8>> {
    let whole_msg = Msg::Raft { id: 0, part: 0, parts: 0 };
    frames
        .filter_map(|(msg, raft)| {
            let json = if msg == whole_msg { whole.to_owned() } else { encode(&msg) };
            crate::proto::encode(&crate::proto::ServerMsg::Cluster { json, raft }).ok()
        })
        .collect()
}

/// The token a runner fetches copies of kiln.db with: signed by the server's key, for it.
pub fn token(key: &str, machine: &str) -> String {
    crate::util::hmac_hex(key.as_bytes(), format!("cluster:{machine}").as_bytes())
}

/// This process's learner, when it holds one (the front door answers reads from it): read on every
/// front-door connection without a lock, swapped as the learner starts and stops (goal 5, plan
/// 01a1a59c2aae step 2; P2, plan 01a1a283e14c).
static GLOBAL: arc_swap::ArcSwapOption<Learner> = arc_swap::ArcSwapOption::const_empty();

/// Says this process holds `learner` (or none now).
pub fn set_global(learner: Option<Arc<Learner>>) {
    GLOBAL.store(learner);
}

/// This process's learner, if it holds one.
pub fn global() -> Option<Arc<Learner>> {
    GLOBAL.load_full()
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
    /// Questions to the server of where kiln.db stands, waiting for their answers: each answer, or its
    /// asker giving up, takes it once, with no lock (`crate::rcu::Pending`; goal 5, plan 01a1a59c2aae).
    asks: crate::rcu::Pending<u64, async_channel::Sender<u64>>,
    next_ask: std::sync::atomic::AtomicU64,
    /// The server's checkpoints of the log, to check in the order they came.
    checkpoints: async_channel::Sender<Checkpoint>,
    /// A checkpoint didn't match: the replica is being copied again, and answers nothing meanwhile.
    diverged: Arc<AtomicBool>,
}

/// A checkpoint of the log the server sent ([`Msg::Checkpoint`]).
#[derive(Debug, Clone)]
struct Checkpoint {
    from: u64,
    to: u64,
    closes: bool,
    hand_on: String,
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
        let (checkpoints, came) = async_channel::bounded(CHECKS_QUEUED);
        let diverged = Arc::new(AtomicBool::new(false));
        crate::rt::spawn(check_log(Arc::downgrade(&group), came, diverged.clone(), say.clone()));
        let learner = Learner {
            group,
            joiner: parking_lot::Mutex::new(Joiner::default()),
            db,
            store,
            me: me.to_owned(),
            say,
            asks: crate::rcu::Pending::default(),
            next_ask: std::sync::atomic::AtomicU64::new(1),
            checkpoints,
            diverged,
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
        // Copied again over entries that came changed: until it's in, the server answers.
        if self.diverged.load(Ordering::SeqCst) {
            return false;
        }
        let id = self.next_ask.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = async_channel::bounded(1);
        self.asks.put(id, tx);
        let checks = checks(&self.group);
        let asked = async {
            self.say.send(AgentMsg::Cluster { json: encode(&Msg::Ask { id, checks }), raft: Vec::new() }).await.ok()?;
            let seq = rx.recv().await.ok()?;
            while self.standing().0 < seq {
                crate::rt::sleep(Duration::from_millis(2)).await;
            }
            Some(())
        };
        let done = matches!(crate::rt::timeout(within, asked).await, Ok(Some(())));
        drop(self.asks.take(&id));
        done
    }

    /// What the server said: a raft message, or a part of one, for the replica; where kiln.db
    /// stands; or a checkpoint of the log, to check.
    pub fn heard(&self, msg: &Msg, raft: Vec<u8>) {
        if let Msg::At { id, seq } = *msg {
            if let Some(tx) = self.asks.take(&id) {
                tx.try_send(seq).ok();
            }
            return;
        }
        if let Msg::Checkpoint { from, to, closes, hand_on } = msg {
            let c = Checkpoint { from: *from, to: *to, closes: *closes, hand_on: hand_on.clone() };
            if self.checkpoints.try_send(c).is_err() {
                tracing::debug!(from, to, "a checkpoint of the cluster log wasn't checked: too many wait");
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

    /// Why the replica's group stopped applying, if it has (its runner starts it again).
    pub fn stopped(&self) -> Option<String> {
        self.group.stopped()
    }
}

/// What a learner's ask says of its checks: the entry its leaves start after, once its process
/// signs its receipts (none: it checks no checkpoints, and kiln sends it none).
fn checks(group: &Group) -> Option<u64> {
    crate::runner::custody::cluster_signer().map(|_| group.leaves_from())
}

/// A learner's checks of the server's checkpoints of the log, one at a time in the order they came
/// (docs/ci.md W27-R08). First it says it checks them, once its process can sign its receipts. Each
/// it checks once it has applied the entries it runs to: one closing its period as handed on, its
/// receipt signed; one that doesn't match, its receipt of what it applied signed at once (the break
/// kiln records) and the replica copied again, answering nothing meanwhile. Ends with its learner.
async fn check_log(group: Weak<Group>, came: async_channel::Receiver<Checkpoint>, diverged: Arc<AtomicBool>, say: async_channel::Sender<AgentMsg>) {
    if crate::runner::custody::cluster_signer_within(SIGNER_WAIT).await.is_some()
        && let Some(holds) = group.upgrade().map(|g| g.leaves_from())
    {
        say.send(AgentMsg::Cluster { json: encode(&Msg::Ask { id: 0, checks: Some(holds) }), raft: Vec::new() }).await.ok();
    }
    // After a copy over entries that came changed, the checkpoints sent before it run over them
    // still: those before this entry aren't checked.
    let mut copied_over = 0u64;
    while let Ok(c) = came.recv().await {
        if c.from < copied_over {
            continue;
        }
        let Some((thing, want)) = crate::custody::statement_in(&c.hand_on).and_then(|s| s.subject.into_iter().next()).and_then(|t| t.digest.get("sha256").cloned().map(|d| (t, d))) else {
            tracing::warn!(target: "kiln.custody", from = c.from, to = c.to, "a checkpoint of the cluster log names nothing it hands on: not checked");
            continue;
        };
        let deadline = Instant::now() + CHECK_WAIT;
        let got = loop {
            let Some(g) = group.upgrade() else { return };
            match g.digest(c.from, c.to) {
                Err(Short::Ahead(_)) if Instant::now() < deadline => {}
                got => break got,
            }
            drop(g);
            crate::rt::sleep(Duration::from_millis(50)).await;
        };
        let applied = |d: &str| Subject { name: thing.name.clone(), digest: BTreeMap::from([("sha256".to_owned(), d.to_owned())]), ..Subject::default() };
        match got {
            Ok(d) if d == want => {
                tracing::debug!(target: "kiln.custody", from = c.from, to = c.to, closes = c.closes, "a checkpoint of the cluster log holds");
                if c.closes {
                    crate::runner::custody::cluster_received(crate::runner::custody::Handed::Bundle(&c.hand_on), applied(&d), "entries of kiln's cluster log, as this runner applied them");
                }
            }
            Ok(d) => {
                tracing::warn!(target: "kiln.custody", from = c.from, to = c.to, applied = %d, handed_on = %want, "a checkpoint of the cluster log doesn't match what this replica applied: an entry came changed; the break is recorded and the database copied again");
                crate::runner::custody::cluster_received(crate::runner::custody::Handed::Bundle(&c.hand_on), applied(&d), "entries of kiln's cluster log, as this runner applied them: not as kiln handed them on");
                copied_over = c.to;
                let Some(g) = group.upgrade() else { return };
                diverged.store(true, Ordering::SeqCst);
                match crate::rt::blocking(move || g.copy_again()).await.and_then(|r| r) {
                    Ok(()) => tracing::info!(target: "kiln.custody", "the replica was copied again over entries that came changed"),
                    Err(e) => tracing::error!(target: "kiln.custody", error = %format!("{e:#}"), "the replica couldn't be copied again over entries that came changed: it's copied as it starts again"),
                }
                diverged.store(false, Ordering::SeqCst);
            }
            Err(Short::Before(base)) => tracing::info!(target: "kiln.custody", from = c.from, to = c.to, base, "a checkpoint of the cluster log runs over entries before this replica's: not checked"),
            Err(Short::Ahead(last)) => tracing::info!(target: "kiln.custody", from = c.from, to = c.to, last, "a checkpoint of the cluster log runs past what this replica applied in time: not checked"),
        }
    }
}

/// The checkpoints of the log kiln owes each learner that checks them (docs/ci.md W27-R08: Ada's
/// cadence, 2026-11-03). A learner's period of them starts after the entry its copy of kiln.db
/// stands at, or the one it holds the leaves of the entries after; each of them runs from there to
/// where the group stands, one every [`CHECK_EVERY`] while entries flow (none while none do), and the
/// first at least [`RECEIPT_EVERY`] into the period closes it, the next starting after it. The
/// learner signs its receipt of each that closes, so of every entry of its period, at most once a
/// minute. Pure: the time comes in.
#[derive(Debug, Default)]
pub struct Periods {
    by: HashMap<u64, Period>,
}

/// One learner's period of checkpoints.
#[derive(Debug, Clone)]
struct Period {
    machine: String,
    /// The runner process holding its replica that said it checks them (none yet: a copy made for
    /// it before it said so).
    peer: Option<PeerId>,
    /// The entry its stretch starts after.
    from: u64,
    since: Instant,
    /// The last entry its last checkpoint ran to.
    sent: u64,
}

/// A checkpoint due: to which learner (its member, machine and the runner process holding it), of
/// which stretch of the log, and whether it closes its period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Due {
    pub member: u64,
    pub machine: String,
    pub peer: PeerId,
    pub from: u64,
    pub to: u64,
    pub closes: bool,
}

impl Periods {
    /// `machine`'s learner (`member`), held by runner process `peer`, checks the log's checkpoints,
    /// holding the leaves of the entries after `holds`: its period goes on if it holds all of it,
    /// else one starts there.
    pub fn checks(&mut self, member: u64, machine: &str, peer: PeerId, holds: u64, now: Instant) {
        let p = self.by.entry(member).or_insert_with(|| Period { machine: machine.to_owned(), peer: None, from: holds, since: now, sent: holds });
        if p.from < holds {
            (p.from, p.since, p.sent) = (holds, now, holds);
        }
        p.machine = machine.to_owned();
        p.peer = Some(peer);
    }

    /// A copy of kiln.db made for `machine`'s learner (`member`) at entry `at`: its period starts
    /// there.
    pub fn copied(&mut self, member: u64, machine: &str, at: u64, now: Instant) {
        let p = self.by.entry(member).or_insert_with(|| Period { machine: machine.to_owned(), peer: None, from: at, since: now, sent: at });
        (p.from, p.since, p.sent) = (at, now, at);
    }

    /// The checkpoints due now the group has applied through `applied`: one for each learner that
    /// checks them with entries since its last.
    pub fn due(&self, applied: u64, now: Instant) -> Vec<Due> {
        let mut due: Vec<Due> = self
            .by
            .iter()
            .filter_map(|(member, p)| {
                let peer = p.peer?;
                (applied > p.sent && applied > p.from).then(|| Due { member: *member, machine: p.machine.clone(), peer, from: p.from, to: applied, closes: now.saturating_duration_since(p.since) >= RECEIPT_EVERY })
            })
            .collect();
        due.sort_by_key(|d| d.member);
        due
    }

    /// Checkpoint `d` went: its period goes on, or, if it closed it, the next starts after it. One
    /// of a period started again since (a copy made meanwhile) changes nothing.
    pub fn sent(&mut self, d: &Due, now: Instant) {
        let Some(p) = self.by.get_mut(&d.member).filter(|p| p.from == d.from) else { return };
        p.sent = p.sent.max(d.to);
        if d.closes {
            (p.from, p.since) = (d.to, now);
        }
    }

    /// The server no longer holds the leaves `member`'s period starts after (it started since): a
    /// period starts at `at`.
    pub fn restart(&mut self, member: u64, at: u64, now: Instant) {
        if let Some(p) = self.by.get_mut(&member) {
            (p.from, p.since, p.sent) = (at, now, at);
        }
    }

    /// `member`'s replica is no longer held by the process that said it checks: none is sent until
    /// one says so again.
    pub fn away(&mut self, member: u64) {
        if let Some(p) = self.by.get_mut(&member) {
            p.peer = None;
        }
    }

    /// Whether `member`'s learner checks the log's checkpoints.
    pub fn checking(&self, member: u64) -> bool {
        self.by.get(&member).is_some_and(|p| p.peer.is_some())
    }
}

/// The server's side of the runners' learners: which machine each is, and the parts of their
/// messages coming in.
#[derive(Default)]
pub struct Learners {
    /// Each learner's machine, by its member: those that said they hold one since the server
    /// started (the group's other learners are away). Read for every message the group sends a
    /// learner without a lock, a copy swapped in as learners come and go (goal 5, plan 01a1a59c2aae).
    machines: crate::rcu::Rcu<std::collections::HashMap<u64, String>>,
    joiner: parking_lot::Mutex<Joiner>,
    next: std::sync::atomic::AtomicU64,
    /// The checkpoints of the log owed to the learners that check them (docs/ci.md W27-R08).
    periods: parking_lot::Mutex<Periods>,
    /// A test build's `entry_changed`: for each learner that said it checks the log, the entry the
    /// group had applied as it first said so. The fault changes one past it, never one the learner
    /// may hold already (a leader catching a learner up sends it entries its copy holds).
    changes_after: parking_lot::Mutex<HashMap<u64, u64>>,
}

impl Learners {
    /// `machine`'s runner holds a replica: it's a learner of the group (added if it's new; the
    /// learners the group has stay, away or not). Its member.
    pub fn announced(&self, group: &Group, machine: &str) -> u64 {
        let member = super::group::member_of(machine);
        let mut wanted: std::collections::BTreeSet<u64> = group.point().1.learners.iter().copied().collect();
        let known: Vec<u64> = self.machines.update(|m| {
            m.insert(member, machine.to_owned());
            m.keys().copied().collect()
        });
        wanted.extend(known);
        group.learners(wanted);
        member
    }

    /// Machine `from` is known by another name from now on (docs/machines.md): its learner leaves
    /// the group, and the machine comes back as a new one when it announces itself as it is now.
    pub fn moved(&self, group: &Group, from: &str) {
        let gone = super::group::member_of(from);
        let known: Vec<u64> = self.machines.update(|m| {
            m.remove(&gone);
            m.keys().copied().collect()
        });
        let wanted: std::collections::BTreeSet<u64> = group.point().1.learners.iter().copied().filter(|l| *l != gone).chain(known).collect();
        group.learners(wanted);
    }

    /// The machine of learner `member`, if it said so since the server started.
    pub fn machine(&self, member: u64) -> Option<String> {
        self.machines.load().get(&member).cloned()
    }

    /// A raft message from `machine`'s learner, or a part of one: to the group once whole.
    pub fn heard(&self, group: &Group, machine: &str, msg: &Msg, raft: Vec<u8>) {
        let Msg::Raft { id, part, parts } = *msg else { return };
        let Some(bytes) = self.joiner.lock().add(machine, id, part, parts, raft) else { return };
        if let Err(e) = group.step(&bytes) {
            tracing::debug!(machine, "the cluster group didn't take a learner's message: {e:#}");
        }
    }

    /// A message to learner `member`, in its frames, each made as it's asked for ([`parts`]): its number taken here,
    /// in the order the messages come, and nothing copied, decoded or changed (a test build's fault) until its first
    /// frame is asked for, so whoever asks makes them where it asks (the hub's cluster loop, on the compute pool: goal
    /// 9, plan 01a1a87b2cec step 3).
    pub fn frames(&self, member: u64, bytes: Vec<u8>) -> Frames {
        let after = if crate::runner::chaos::get().is_some_and(|c| c.entry_changed) { self.changes_after.lock().get(&member).copied() } else { None };
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Box::new(std::iter::once(bytes).flat_map(move |bytes| {
            let bytes = match after {
                Some(after) => changed_in_transit(bytes, after),
                None => bytes,
            };
            parts(id, bytes)
        }))
    }

    /// `machine`'s learner, held by runner process `peer`, checks the log's checkpoints, holding the
    /// leaves of the entries after `holds` ([`Periods::checks`]); `group` is where the log stands.
    pub fn checks(&self, group: &Group, machine: &str, peer: PeerId, holds: u64) {
        let member = super::group::member_of(machine);
        if crate::runner::chaos::get().is_some_and(|c| c.entry_changed) {
            self.changes_after.lock().entry(member).or_insert_with(|| group.standing().1);
        }
        self.periods.lock().checks(member, machine, peer, holds, Instant::now());
    }

    /// A copy of kiln.db made for `machine`'s learner, at entry `at` ([`Periods::copied`]).
    pub fn copied(&self, machine: &str, at: u64) {
        self.periods.lock().copied(super::group::member_of(machine), machine, at, Instant::now());
    }

    /// The checkpoints due now the group has applied through `applied` ([`Periods::due`]).
    pub fn due(&self, applied: u64) -> Vec<Due> {
        self.periods.lock().due(applied, Instant::now())
    }

    /// Checkpoint `d` went ([`Periods::sent`]).
    pub fn sent(&self, d: &Due) {
        self.periods.lock().sent(d, Instant::now());
    }

    /// `member`'s period starts again at `at` ([`Periods::restart`]).
    pub fn restart(&self, member: u64, at: u64) {
        self.periods.lock().restart(member, at, Instant::now());
    }

    /// `member`'s replica is no longer held by the process that said it checks ([`Periods::away`]).
    pub fn away(&self, member: u64) {
        self.periods.lock().away(member);
    }

}

/// A test build's `entry_changed` (the server's, docs/ci.md W27-R08), for a message to a learner
/// that checks the log: the first entry with a write in it past `after` (what the group had applied
/// as the learner said it checks) that the server sends one has its last byte changed after kiln
/// logged it, and so has every copy of that entry sent again (an append the learner refused, sent
/// again), as a link or a relay that changes what it carries would (the message made again whole).
/// The learner's next checkpoint doesn't match.
fn changed_in_transit(bytes: Vec<u8>, after: u64) -> Vec<u8> {
    use hyper_raft::wire::Record as _;
    // The entry it changes, once picked (0: none yet).
    static CHANGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let Ok(mut m) = hyper_raft::proto::Message::decode(&bytes) else { return bytes };
    let mut changed = false;
    for e in m.entries.iter_mut().filter(|e| !e.data.is_empty() && e.index > after) {
        let picked = match CHANGED.compare_exchange(0, e.index, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => e.index,
            Err(picked) => picked,
        };
        if e.index == picked
            && let Some(b) = e.data.last_mut()
        {
            *b ^= 0x01;
            changed = true;
        }
    }
    if !changed {
        return bytes;
    }
    tracing::warn!(target: "kiln.custody", entry = CHANGED.load(Ordering::SeqCst), "chaos: an entry of the cluster log sent to a learner changed on its way");
    m.encode_to_vec()
}

fn copy_url(server: &str, me: &str) -> String {
    format!("{}/api/cluster/copy?machine={}", server.trim_end_matches('/'), escaped(me))
}

/// `s` as a query's value.
fn escaped(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") }).collect()
}

/// A learner's way to a copy of kiln.db: the server's, over HTTP, into a file, whole or not at all.
/// Once its process can sign its receipt it says who it is (`instance=`), and kiln hands the copy on
/// to it by its SHA-256 (docs/ci.md W27-R08): hashed as it comes, a copy that isn't what kiln handed
/// on is refused, its receipt of what came the break kiln records, and another fetched, [`COPY_TRIES`]
/// in all.
fn fetcher(http: Arc<Client>, url: String, token: String) -> Fetch {
    Arc::new(move |dest: &Path| {
        let (http, url, token, dest) = (http.clone(), url.clone(), token.clone(), dest.to_path_buf());
        crate::rt::block_on(async move {
            let url = match crate::runner::custody::cluster_signer_within(SIGNER_WAIT).await {
                Some(instance) => format!("{url}&instance={}", escaped(&instance)),
                None => url,
            };
            let mut tried = 0;
            loop {
                tried += 1;
                match fetch_copy(&http, &url, &token, &dest).await? {
                    None => return Ok(()),
                    Some(why) if tried < COPY_TRIES => tracing::warn!(target: "kiln.custody", tried, "{why}: another is fetched"),
                    Some(why) => anyhow::bail!("{why}"),
                }
            }
        })
    })
}

/// One copy of kiln.db fetched into `dest`, whole or not at all, and hashed as it comes: none once
/// it's taken, or why it's refused (not what kiln handed on: the file gone, the break recorded).
async fn fetch_copy(http: &Client, url: &str, token: &str, dest: &Path) -> anyhow::Result<Option<String>> {
    use crate::custody::cluster::{COPY_HEADER, CopyHanded, copy_thing};
    let mut resp = http.get(url).bearer(token).timeout(Duration::from_secs(3600)).send().await?;
    anyhow::ensure!(resp.is_success(), "the server answered {}", resp.status());
    let length = resp.header("content-length").and_then(|v| v.parse::<u64>().ok());
    // kiln's hand-on of the copy, when it signed one (a server from before signs none).
    let handed: Option<CopyHanded> = resp.header(COPY_HEADER).and_then(|h| serde_json::from_str(h).ok());
    std::fs::remove_file(dest).ok();
    let (sink, done) = crate::rt::file_sink(dest.to_path_buf(), 1 << 40)?;
    let mut hash = sha2::Sha256::new();
    let mut broke = None;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                hash.update(&chunk);
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
        std::fs::remove_file(dest).ok();
        anyhow::bail!("the copy broke off: {}", broke.or_else(|| wrote.err().map(|e| format!("{e:#}"))).unwrap_or_else(|| "short".to_owned()));
    }
    let Some(h) = handed else { return Ok(None) };
    let got = hex::encode(hash.finalize());
    let taken = got.eq_ignore_ascii_case(&h.sha256);
    let why = if taken { "a copy of kiln.db, for the replica this runner keeps" } else { "a copy of kiln.db, for the replica this runner keeps: not as kiln handed it on" };
    crate::runner::custody::cluster_received(crate::runner::custody::Handed::Digest(&h.hand_on), copy_thing(h.at, &got), why);
    if taken {
        return Ok(None);
    }
    std::fs::remove_file(dest).ok();
    Ok(Some(format!("the copy of kiln.db at entry {} isn't what kiln handed on (its sha256 is {got}, not {}): refused", h.at, h.sha256)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 9, plan 01a1a87b2cec step 3: a learner's message's frames, made where they're asked for (the hub's cluster
    /// loop asks on the compute pool), are the frames the loop made on its shard: a whole message in one, its words
    /// the whole one's written once; a long one in its parts, in their order, numbered in the order the messages
    /// came; a small one left uncopied until it's encoded.
    #[test]
    fn g9_s3_frames_made_off_the_shard_are_those_the_loop_made() -> anyhow::Result<()> {
        let learners = Learners::default();
        let whole = encode(&Msg::Raft { id: 0, part: 0, parts: 0 });
        let small = vec![7u8; 1000];
        let at = small.as_ptr();
        let mut uncopied = learners.frames(1, small);
        anyhow::ensure!(matches!(uncopied.next(), Some((Msg::Raft { id: 0, part: 0, parts: 0 }, b)) if b.as_ptr() == at), "a small one whole, uncopied");
        let small = vec![7u8; 1000];
        let long: Vec<u8> = (0..(PART * 2 + 17)).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
        for bytes in [small, long.clone(), long] {
            let id = learners.next.load(std::sync::atomic::Ordering::Relaxed);
            let before: Vec<Vec<u8>> = frames(id, bytes.clone())
                .into_iter()
                .map(|(msg, raft)| crate::proto::encode(&crate::proto::ServerMsg::Cluster { json: encode(&msg), raft }))
                .collect::<anyhow::Result<_>>()?;
            let made = encoded(learners.frames(1, bytes), &whole);
            anyhow::ensure!(made == before, "message {id}: {} frames made, {} before", made.len(), before.len());
        }
        Ok(())
    }

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

    /// Goal 9, plan 01a1a87b2cec step 3: a message's parts made one at a time are its frames: a small one whole and
    /// uncopied; a long one in parts of `PART`, the last what's left, numbered in order, together the message.
    #[test]
    fn g9_s3_parts_made_one_at_a_time_are_the_frames() {
        let small = vec![1u8, 2, 3];
        let at = small.as_ptr();
        let mut one = parts(5, small);
        let first = one.next();
        assert!(matches!(&first, Some((Msg::Raft { id: 0, part: 0, parts: 0 }, b)) if b.as_ptr() == at), "whole, uncopied: {first:?}");
        assert!(one.next().is_none());
        let bytes: Vec<u8> = (0..(PART * 2 + 17)).map(|i| (i % 251) as u8).collect();
        let made: Vec<(Msg, Vec<u8>)> = parts(9, bytes.clone()).collect();
        assert_eq!(made.iter().map(|(_, b)| b.len()).collect::<Vec<_>>(), vec![PART, PART, 17]);
        assert!(made.iter().zip(0u32..).all(|((m, _), n)| matches!(m, Msg::Raft { id: 9, part, parts: 3 } if *part == n)));
        assert_eq!(made.into_iter().flat_map(|(_, b)| b).collect::<Vec<u8>>(), bytes);
    }

    /// W27-R08: a learner from before asks with no `checks`, and is read as one that checks
    /// nothing; one that checks says from where; a checkpoint comes back as it went. What a learner
    /// from before sends is what it sent: no field it didn't have.
    #[test]
    fn w27_r08_asks_and_checkpoints_read_back_and_an_ask_from_before_checks_nothing() {
        assert_eq!(decode(r#"{"t":"ask","id":3}"#), Some(Msg::Ask { id: 3, checks: None }));
        assert_eq!(encode(&Msg::Ask { id: 3, checks: None }), r#"{"t":"ask","id":3}"#);
        let ask = Msg::Ask { id: 4, checks: Some(120) };
        assert_eq!(decode(&encode(&ask)), Some(ask));
        let c = Msg::Checkpoint { from: 120, to: 180, closes: true, hand_on: r#"{"dsseEnvelope":{}}"#.into() };
        assert_eq!(decode(&encode(&c)), Some(c));
    }

    /// W27-R08 (docs/ci.md, the cluster group's custody; Ada's cadence, 2026-11-03): kiln sends a
    /// learner that checks them a checkpoint of the log every 10 s while entries flow, and none while
    /// none do. Each runs from where the learner's period began, and the first a minute or more into
    /// it closes it, the next starting after it: the learner signs at most one receipt a minute, and
    /// its periods tile the log from its copy on. A learner that hasn't said it checks them is sent
    /// none; a copy starts a period at its entry; a learner that holds less of the log than its
    /// period starts one where it holds; a checkpoint of a period started again since changes
    /// nothing.
    #[test]
    fn w27_r08_checkpoints_come_every_10_s_while_entries_flow_and_close_at_most_once_a_minute() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut p = Periods::default();
        p.copied(7, "box-a", 100, at(0));
        assert!(p.due(120, at(0)).is_empty(), "none to a learner that hasn't said it checks them");
        p.checks(7, "box-a", 1, 100, at(1));
        // Entries flow for 30 s, none flow until 75 s, then they flow again.
        let applied = |s: u64| match s {
            0..=30 => 100 + s,
            31..=74 => 130,
            _ => 130 + (s - 70),
        };
        let mut sent = Vec::new();
        for s in (10..=200).step_by(10) {
            for d in p.due(applied(s), at(s)) {
                p.sent(&d, at(s));
                sent.push((s, d.from, d.to, d.closes));
            }
        }
        assert_eq!(
            sent,
            [
                (10, 100, 110, false),
                (20, 100, 120, false),
                (30, 100, 130, false),
                // None while no entries flow, from 40 s to 70 s.
                (80, 100, 140, true),
                (90, 140, 150, false),
                (100, 140, 160, false),
                (110, 140, 170, false),
                (120, 140, 180, false),
                (130, 140, 190, false),
                (140, 140, 200, true),
                (150, 200, 210, false),
                (160, 200, 220, false),
                (170, 200, 230, false),
                (180, 200, 240, false),
                (190, 200, 250, false),
                (200, 200, 260, true),
            ]
        );
        let closed: Vec<u64> = sent.iter().filter(|c| c.3).map(|c| c.0).collect();
        assert!(closed.windows(2).all(|w| matches!(w, [a, b] if b - a >= 60)), "at most one receipt a minute: {closed:?}");
        // A copy starts a period at its entry, and a checkpoint of the period before it, due when
        // the copy came, changes nothing.
        let stale = p.due(270, at(205));
        p.copied(7, "box-a", 300, at(206));
        for d in &stale {
            p.sent(d, at(207));
        }
        assert_eq!(p.due(310, at(210)), [Due { member: 7, machine: "box-a".into(), peer: 1, from: 300, to: 310, closes: false }]);
        // A new process holding the replica, its leaves starting after entry 305: a period starts
        // there. One holding more of the log goes on with the period.
        p.checks(7, "box-a", 2, 305, at(211));
        assert_eq!(p.due(320, at(220)), [Due { member: 7, machine: "box-a".into(), peer: 2, from: 305, to: 320, closes: false }]);
        p.checks(7, "box-a", 3, 250, at(221));
        assert_eq!(p.due(320, at(222)).first().map(|d| (d.peer, d.from)), Some((3, 305)));
        // Away, none; and kiln without the leaves the period starts after starts one where it stands.
        p.away(7);
        assert!(p.due(330, at(230)).is_empty() && !p.checking(7));
        p.checks(7, "box-a", 3, 305, at(231));
        p.restart(7, 330, at(232));
        assert_eq!(p.due(340, at(240)).first().map(|d| (d.from, d.to)), Some((330, 340)));
    }
}
