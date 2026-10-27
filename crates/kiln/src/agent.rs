//! `kiln agent`: runs jobs on this machine (bare metal, a bench box, or a Kubernetes pod).
//!
//! * Enrolls once with a join token: generates its key locally, sends a CSR, stores the signed
//!   certificate. Afterwards it authenticates with mutual TLS only.
//! * Keeps one QUIC session to the server (hyper-transport), reconnecting with backoff forever.
//!   Jobs keep running while the server is unreachable; their output and results wait in a
//!   bounded uplink buffer and are delivered after reconnecting.
//! * Serves PTY terminals so the UI can drive this machine.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::FutureExt as _;
use serde::{Deserialize, Serialize};

use crate::http::client::{Client, Roots};
use crate::rt::{Interval, Shutdown};

use crate::exec::{Exec, Up, Uplink};
use crate::net::{self, Kind, NetEvent, Out, SERVER_PEER};
use crate::pki::Leaf;
use crate::proto::{self, AgentMsg, Hello, JobResult, LANE_CONTROL, LANE_CONVO, LANE_FILES, LANE_LOGS, LANE_PORTS, LANE_REACH, LANE_STREAM, LANE_SYNC, LANE_TERM, ServerMsg};
use crate::store::Status;
use crate::terminal::{self, PtyOut, PtySession};

/// Bytes of output/results held while disconnected before the oldest log chunks are shed.
const UPLINK_BUFFER: usize = 128 << 20;
/// How long a job's result is kept for a server that never says it has it: well past the grace a
/// runner gets to reconnect, after which the server has given its legs up.
const UNRECEIVED_FOR: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
pub struct Opts {
    pub server_url: String,
    pub quic: Option<String>,
    pub name: String,
    pub labels: Vec<String>,
    pub slots: u32,
    pub work_dir: PathBuf,
    pub state_dir: PathBuf,
    pub join_token: Option<String>,
    pub ephemeral: bool,
    pub dedicated_job: Option<String>,
    pub terminal: bool,
    /// Terminals into CI legs (docs/ci.md W17-R01): a runner's follow its terminal switch; the
    /// server's in-process runner's follow `[terminal] enabled`.
    pub job_terminal: bool,
    pub shell: Option<String>,
    /// In-process agent: credentials handed over directly, no enrollment.
    pub credentials: Option<(Vec<u8>, Leaf)>,
    /// A runner process beside the machine's usual one (`kiln install --agent --upgrade`): its
    /// own certificate and files under `<state>/instances/<id>` (see `crate::upgrade`).
    pub instance: Option<String>,
    /// The config file it was started with: `[updates] accept` is read from it as it changes
    /// (`None`: it takes upgrades).
    pub config: Option<PathBuf>,
    /// Reach (`crate::reach`): None leaves it off (the server's own in-process agent); otherwise
    /// whether this runner takes commands from other runners (it can always send them).
    pub reach: Option<bool>,
    /// An outpost (`kiln outpost`, [`crate::proto::CAP_OUTPOST`]): kiln run where it isn't
    /// installed. Its terminals, AI sessions and agents work; it takes no jobs, keeps no front
    /// door or repo copies, and is never upgraded by the server.
    pub outpost: bool,
    /// What its administrator declares of it (docs/ci.md W13-R02).
    pub declared: crate::runner::probe::Declared,
}

/// Probes this runner's facts and sends them (docs/ci.md W13-R01), one probe at a time.
fn probe_facts(uplink: &Uplink, work_dir: &std::path::Path, declared: &crate::runner::probe::Declared, probing: &Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::Ordering;
    if probing.swap(true, Ordering::SeqCst) {
        return;
    }
    let (up, dir, declared, busy) = (uplink.clone(), work_dir.to_path_buf(), declared.clone(), Arc::clone(probing));
    crate::rt::spawn(async move {
        let facts = crate::runner::probe::gather(&dir, &declared).await;
        if let Ok(json) = serde_json::to_string(&facts) {
            up.msg(AgentMsg::Facts { json }).await;
        }
        busy.store(false, Ordering::SeqCst);
    });
}

/// Why an outpost turns a job down (an older server may still hand it one).
const OUTPOST_TAKES_NO_JOBS: &str = "an outpost takes no jobs";

#[derive(Serialize, Deserialize)]
pub struct EnrollRequest {
    pub token: String,
    pub name: String,
    pub csr: String,
    /// The machine's hostname: an outpost enrolling again keeps its name ([`crate::api`]).
    #[serde(default)]
    pub hostname: String,
}

/// `POST /api/agents/instance`: see `api::enroll_instance`.
#[derive(Serialize, Deserialize)]
pub struct InstanceRequest {
    pub cert_pem: String,
    pub csr: String,
}

#[derive(Serialize, Deserialize)]
pub struct EnrollResponse {
    pub cert_pem: String,
    pub ca_pem: String,
    pub quic: String,
    pub name: String,
}

fn ca_der(pem: &str) -> anyhow::Result<Vec<u8>> {
    use hyper_quic::rustls::pki_types::CertificateDer;
    use hyper_quic::rustls::pki_types::pem::PemObject as _;
    Ok(CertificateDer::from_pem_slice(pem.as_bytes()).map_err(|e| anyhow::anyhow!("ca pem: {e:?}"))?.to_vec())
}

/// Load stored credentials, enrolling (or renewing) when needed. Retries until it succeeds.
async fn credentials(o: &Opts, shutdown: &Shutdown) -> anyhow::Result<(Vec<u8>, Leaf, Option<String>)> {
    if let Some((ca, leaf)) = &o.credentials {
        return Ok((ca.clone(), leaf.clone(), None));
    }
    if let Some(id) = &o.instance {
        return instance_credentials(o, id, shutdown).await;
    }
    let cert_p = o.state_dir.join("agent.pem");
    let key_p = o.state_dir.join("agent.key");
    let ca_p = o.state_dir.join("ca.pem");
    let quic_p = o.state_dir.join("quic");
    if let (Ok(c), Ok(k), Ok(ca)) = (
        std::fs::read_to_string(&cert_p),
        std::fs::read_to_string(&key_p),
        std::fs::read_to_string(&ca_p),
    )
        && (!crate::pki::needs_renewal(&c) || o.join_token.is_none()) {
            let quic = std::fs::read_to_string(&quic_p).ok().map(|s| s.trim().to_owned());
            return Ok((ca_der(&ca)?, Leaf { cert_pem: c, key_pem: k }, quic));
        }
    let token = o
        .join_token
        .clone()
        .ok_or_else(|| anyhow::anyhow!("not enrolled and no join token (pass --token or KILN_JOIN_TOKEN)"))?;
    let (csr, key_pem) = crate::pki::make_csr(&o.name)?;
    // Enrollment happens before kiln's CA is known: plain http, or https with a public cert.
    let http = Client::public()?;
    let url = format!("{}/api/agents/enroll", o.server_url.trim_end_matches('/'));
    let mut backoff = Duration::from_secs(1);
    loop {
        let r = http
            .post(&url)
            .json(&EnrollRequest { token: token.clone(), name: o.name.clone(), csr: csr.clone(), hostname: crate::config::Config::hostname() })
            .send()
            .await;
        match r {
            Ok(resp) if resp.is_success() => {
                let e: EnrollResponse = resp.json(1 << 20).await.context("enroll response")?;
                crate::util::write_secret(&key_p, key_pem.as_bytes(), 0o600)?;
                crate::util::write_secret(&cert_p, e.cert_pem.as_bytes(), 0o644)?;
                crate::util::write_secret(&ca_p, e.ca_pem.as_bytes(), 0o644)?;
                crate::util::write_secret(&quic_p, e.quic.as_bytes(), 0o644)?;
                tracing::info!(name = %e.name, "enrolled with server");
                return Ok((ca_der(&e.ca_pem)?, Leaf { cert_pem: e.cert_pem, key_pem }, Some(e.quic)));
            }
            Ok(resp) if resp.status() == 401 || resp.status() == 403 => {
                anyhow::bail!("enrollment refused: the join token is invalid or expired");
            }
            Ok(resp) => tracing::warn!(status = resp.status(), "enrollment failed; retrying"),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "server unreachable for enrollment; retrying"),
        }
        if !shutdown.sleep(crate::sup::jitter(backoff)).await {
            anyhow::bail!("shutdown");
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// A runner process beside the machine's usual one: the machine's key with this instance's own
/// certificate, fetched (or renewed) from the server with a current certificate as proof — this
/// instance's own while it is valid, else the machine's. Its own goes once it is for another key
/// (the machine enrolled again since, with a new one): with it, no TLS configuration is made and
/// the runner never reaches the server, so an upgrade that starts it never completes.
async fn instance_credentials(o: &Opts, id: &str, shutdown: &Shutdown) -> anyhow::Result<(Vec<u8>, Leaf, Option<String>)> {
    let dir = crate::upgrade::instance_dir(&o.state_dir, id);
    let ca = std::fs::read_to_string(o.state_dir.join("ca.pem")).context("this machine is not enrolled (no ca.pem)")?;
    let (machine_cert, key) = crate::upgrade::machine_credentials(&o.state_dir)?;
    let quic = std::fs::read_to_string(o.state_dir.join("quic")).ok().map(|s| s.trim().to_owned());
    let cert_p = dir.join("agent.pem");
    let own = std::fs::read_to_string(&cert_p).ok().filter(|c| {
        let mine = crate::pki::cert_matches_key(c, &key);
        if !mine {
            tracing::warn!(instance = %id, "this runner process's certificate is for a key the machine no longer has (it enrolled again): asking for a new one");
        }
        mine
    });
    if let Some(c) = &own
        && !crate::pki::needs_renewal(c)
    {
        return Ok((ca_der(&ca)?, Leaf { cert_pem: c.clone(), key_pem: key }, quic));
    }
    std::fs::create_dir_all(&dir)?;
    let mut backoff = Duration::from_secs(1);
    loop {
        for proof in own.iter().chain(std::iter::once(&machine_cert)) {
            match crate::upgrade::fetch_certificate(&o.server_url, Some(&ca), &o.name, proof, &key).await {
                Ok(e) => {
                    crate::util::write_secret(&cert_p, e.cert_pem.as_bytes(), 0o644)?;
                    tracing::info!(instance = %id, "certificate for this runner process issued");
                    return Ok((ca_der(&e.ca_pem)?, Leaf { cert_pem: e.cert_pem, key_pem: key }, quic.or(Some(e.quic))));
                }
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "no certificate for this runner process yet; retrying"),
            }
        }
        if !shutdown.sleep(crate::sup::jitter(backoff)).await {
            anyhow::bail!("shutdown");
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// A dial that produced no transport event in this long is treated as lost (see `run`).
const DIAL_TIMEOUT: Duration = Duration::from_secs(20);

struct Running {
    cancel: Shutdown,
    /// It runs in a pool's executor, not in one of this runner's slots (docs/ci.md W13-R09).
    pooled: bool,
    /// What it holds of this runner (docs/ci.md W05-R03): a share of its CPUs and memory, the
    /// whole runner, or nothing of its own.
    holds: kiln_ci::sched::state::Incoming,
}

/// A job holding a share of this runner takes its CPUs (docs/ci.md W05-R03): whole cores of its
/// own, the other legs' steps moving off them. Where they can't be pinned (no topology: not
/// Linux), it still holds its room in placement, and the log says so.
fn hold_cpus(job: &str, holds: kiln_ci::sched::state::Incoming) {
    let kiln_ci::sched::state::Incoming::Share(q) = holds else { return };
    let threads = q.cpu.ceil().max(1.0) as usize;
    match crate::runner::cpus::pins().hold(job, threads) {
        Some(cpus) => tracing::info!(job, cpus = %crate::runner::cpus::list(&cpus), "it holds these CPUs; the other legs run on the rest"),
        None => tracing::info!(job, threads, "it holds its share in placement; its cores aren't pinned here"),
    }
}

/// Whether this runner takes one more job or leg (docs/ci.md W05-R03), by the rule its dispatchers
/// use, counting what its jobs and legs from either hold (`jobs`: each one's pooled and holds); a
/// pool's leg runs in its executor, beside the runner's slots. Why not, when it doesn't.
fn admits(slots: u32, size: Option<kiln_ci::sched::state::Resources>, jobs: impl Iterator<Item = (bool, kiln_ci::sched::state::Incoming)>, pooled: bool, incoming: kiln_ci::sched::state::Incoming) -> Result<(), &'static str> {
    use kiln_ci::sched::state::{Incoming, Resources, room};
    let (mut held, mut whole, mut ordinary, mut running) = (Resources::default(), false, 0, 0);
    for (p, h) in jobs {
        match h {
            Incoming::Whole => whole = true,
            Incoming::Share(q) => {
                held.cpu += q.cpu;
                held.mem = held.mem.saturating_add(q.mem);
            }
            Incoming::Ordinary => {}
        }
        if !p {
            running += 1;
            ordinary += usize::from(h == Incoming::Ordinary);
        }
    }
    if whole {
        return Err("an exclusive job has this runner to itself");
    }
    if pooled || room(slots, size, &held, false, ordinary, running, incoming) {
        return Ok(());
    }
    Err(match incoming {
        Incoming::Whole => "an exclusive job waits for this runner to be empty",
        Incoming::Share(_) => "no room for the share it holds",
        Incoming::Ordinary if held.cpu > 0.0 => "no free slot beside what's held",
        Incoming::Ordinary => "no free slot",
    })
}

/// How long a cancelled job has to end before the runner reports it over and lets it go.
const CANCEL_GRACE: Duration = Duration::from_secs(120);

/// One turn of the agent's event loop (computed inside `select!`, handled after it).
enum Ev {
    Shutdown,
    Net(Option<NetEvent>),
    Done(JobResult),
    Heartbeat,
    Sample,
    Watchdog,
    Redial,
    Term(String, PtyOut),
    Up(Up),
    Harness(String, crate::harness::core::Update),
    /// The front door's status changed.
    FrontDoor,
    /// What headroom decides changed (docs/headroom.md).
    Headroom,
    /// A blue/green upgrade step done off the loop (`crate::bluegreen`).
    #[cfg(unix)]
    Bg(Bg),
    /// A server frame about something another runner process of this machine handed over (or a
    /// rollback took back), handled as if the server had sent it here.
    Forwarded(Vec<u8>),
    /// A test build's fault: the connection drops (W22-AC04).
    ChaosDrop,
    /// A test build's fault: a frame held back, due now (W22-AC04).
    Delayed(crate::runner::chaos::Due),
}

/// Blue/green upgrade steps (`crate::bluegreen`) done off the event loop, back to it (none on
/// Windows, which has no blue/green upgrades).
enum Bg {
    /// (The runner being upgraded.) The new build is downloaded and checked: start it beside this one.
    #[cfg(unix)]
    Fetched { job: String, exe: PathBuf, build: String },
    /// The new build's runner is connected and ready: hand it everything.
    #[cfg(unix)]
    Ready { job: String, conn: Arc<crate::bluegreen::Conn> },
    /// Everything was sent: what the new build said of each, in order (`broke`: the connection
    /// failed part-way).
    #[cfg(unix)]
    Moved { job: String, conn: Arc<crate::bluegreen::Conn>, acks: Vec<Ack>, broke: Option<String> },
    /// The new build didn't come up: this runner goes on.
    #[cfg(unix)]
    Failed { job: String, why: String },
    /// (The new build's runner.) A terminal handed over: take it, and say how it went.
    #[cfg(unix)]
    TakeTerminal { session: String, command: Option<String>, cols: u16, rows: u16, replay: Vec<u8>, fd: std::os::fd::OwnedFd, pid: u32, done: futures::channel::oneshot::Sender<Result<(), String>> },
    /// (The new build's runner.) Everything handed over is in: the server hears what runs here.
    #[cfg(unix)]
    TookOver { sessions: usize, terminals: usize, refused: usize },
    /// (The new build's runner.) The runner it took over from is gone (or never answered).
    #[cfg(unix)]
    TakeOverEnded,
    /// (The new build's runner.) It is the runner now: the server hears it's the newest.
    #[cfg(unix)]
    Promoted,
    /// A server frame for [`Ev::Forwarded`].
    #[cfg(unix)]
    Forwarded(Vec<u8>),
    /// A server frame to take again ([`Ev::Forwarded`]): a terminal into a leg this runner holds,
    /// asked for (by its session) before the leg had its shell.
    Again(String, Vec<u8>),
}

/// What the new build said of one thing handed to it (blue/green).
#[cfg(unix)]
enum Ack {
    /// It holds it now.
    Taken,
    /// It didn't take it (an agent session it then started again from its record, if it could).
    Refused(String),
    /// No answer came: the connection broke on it.
    Lost(String),
    /// Never sent: the connection broke before.
    Skipped,
}

/// A blue/green upgrade under way on the runner being upgraded (`crate::bluegreen`).
#[cfg(unix)]
struct Moving {
    green: crate::bluegreen::Green,
    /// The new build's instance (its own directory under `<state>/instances/`).
    instance: String,
    /// The `kiln::upgrade` spans: the whole upgrade, and the new build coming up.
    root: tracing::Span,
    healthy: Option<tracing::Span>,
    /// The new build is ready and things are being handed to it: what the server opens here
    /// meanwhile is held for it too.
    handing: bool,
    /// What was handed over, in the order it was sent, kept until the new build is promoted: a
    /// rollback takes it back.
    items: Vec<Item>,
    /// Server frames about those that reached this runner meanwhile: for the new build once it's
    /// promoted, else handled here.
    held: Vec<Vec<u8>>,
}

/// One thing handed to the new build (blue/green), as this runner keeps it until then.
#[cfg(unix)]
enum Item {
    Session {
        record: Box<crate::harness::host::Saved>,
        /// Its handover as sent (JSON) and this side's copies of its pipes; `None`: it couldn't
        /// be handed over running, and starts again from its record there.
        kept: Option<(Vec<u8>, [std::os::fd::OwnedFd; 3])>,
    },
    Terminal {
        session: String,
        state: Arc<crate::termsess::Session>,
        fd: std::os::fd::OwnedFd,
        pid: u32,
    },
}

#[cfg(unix)]
impl Item {
    fn what(&self) -> &'static str {
        match self {
            Self::Session { .. } => "session",
            Self::Terminal { .. } => "terminal",
        }
    }

    fn id(&self) -> &str {
        match self {
            Self::Session { record, .. } => record.id(),
            Self::Terminal { session, .. } => session,
        }
    }
}

/// After the new build is promoted, this runner waits this long (and as long once its last job
/// ends) before it exits: what it sent last reaches the server, and the server has heard from the
/// new build that it holds what was handed over.
#[cfg(unix)]
const RETIRE_QUIET: Duration = Duration::from_secs(8);
/// How long a terminal into a leg this runner holds waits for the leg's shell (its workspace and
/// environment set up, its images pulled) before it's refused.
const LEG_SHELL_WAIT: Duration = Duration::from_secs(60);

/// How long a new build that took its sessions over, and whose link to the runner it upgraded
/// ended with no word of a promotion, holds their messages still (a runner that takes them back
/// stops it well before).
#[cfg(unix)]
const TAKE_OVER_GRACE: Duration = Duration::from_secs(30);

#[cfg(unix)]
use crate::upgrade::with_instance;

/// This side's copies of a handed-over harness process's pipes, kept (not inherited by anything
/// this runner starts) until the new build has them; `None` when one isn't open.
#[cfg(unix)]
fn keep_pipes(p: &crate::proc::Detached) -> Option<[std::os::fd::OwnedFd; 3]> {
    let take = |raw: i32| {
        let fd = crate::os::reclaim_fd(raw)?;
        rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC).ok()?;
        Some(fd)
    };
    Some([take(p.stdin)?, take(p.stdout)?, take(p.stderr)?])
}

/// Go on with a terminal here from its PTY master (`fd`) and its shell (another process's child,
/// or this one's still reaped where it was spawned): its output flows again, and it's kept with
/// `state`. Whether it could.
#[cfg(unix)]
fn take_terminal_back(terms: &mut HashMap<String, (PtySession, Arc<crate::termsess::Session>)>, term_tx: &async_channel::Sender<(String, PtyOut)>, session: String, state: Arc<crate::termsess::Session>, fd: std::os::fd::OwnedFd, pid: u32) -> bool {
    use std::os::fd::IntoRawFd as _;
    let Some((pty, rx)) = terminal::adopt(terminal::Handover { fd: fd.into_raw_fd(), pid, foreign: true }) else { return false };
    let (tx, s) = (term_tx.clone(), session.clone());
    crate::rt::spawn(async move {
        while let Ok(out) = rx.recv().await {
            let exit = matches!(out, PtyOut::Exit(_));
            if tx.send((s.clone(), out)).await.is_err() || exit {
                break;
            }
        }
    });
    terms.insert(session, (pty, state));
    true
}

/// The terminals this runner process holds, as the server is told of them.
#[cfg(unix)]
fn term_list(terms: &HashMap<String, (PtySession, Arc<crate::termsess::Session>)>) -> Vec<proto::TermInfo> {
    terms
        .iter()
        .map(|(id, (_, s))| {
            let i = s.info();
            proto::TermInfo { session: id.clone(), cols: i.cols, rows: i.rows, command: s.command.clone() }
        })
        .collect()
}

/// Tell the server which terminals (`sessions`, [`term_list`]) and agent sessions this runner
/// process holds: they're its own from now on (another process of the machine had them).
#[cfg(unix)]
async fn announce(sessions: Vec<proto::TermInfo>, harness: Option<&Arc<crate::harness::host::Host>>, uplink: &Uplink) {
    uplink.0.send(Up::Term(AgentMsg::TermList { sessions })).await.ok();
    if let Some(h) = harness
        && let Ok(json) = serde_json::to_string(&h.inventory())
    {
        uplink.0.send(Up::Term(AgentMsg::HarnessInventory { json })).await.ok();
    }
}

/// Start this machine's replica of kiln's records with the server's newest word to join (`join`),
/// trying again until it starts: the process before this one may hold the replica's files a
/// while yet (an old build keeps them until its last job ends and it retires, minutes later). It
/// stops once the server takes the replica away (`join` emptied by a Leave) or this runner is
/// going (`going`). `wait`: how long before each try again ([`replica_wait`]).
async fn start_replica<S, F, Fut>(join: &parking_lot::Mutex<Option<(u64, String)>>, going: impl Fn() -> bool, start: F, wait: fn(u32) -> Duration) -> anyhow::Result<(S, u64)>
where
    F: Fn(u64, String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<S>>,
{
    let mut tries = 0u32;
    loop {
        let Some((member, token)) = join.lock().clone() else {
            anyhow::bail!("the server no longer asks this machine for a replica");
        };
        let e = match start(member, token).await {
            Ok(s) => return Ok((s, member)),
            Err(e) => e,
        };
        tries += 1;
        if going() {
            return Err(e);
        }
        if tries == REPLICA_PATIENCE {
            tracing::warn!(error = %format!("{e:#}"), tries, "this machine's replica of kiln's records hasn't started yet; it's tried again every minute until it does");
        } else {
            tracing::debug!(error = %format!("{e:#}"), tries, "this machine's replica of kiln's records didn't start yet");
        }
        crate::rt::sleep(wait(tries)).await;
    }
}

/// The tries a replica gets 15 s apart, before it's tried each minute instead (and said so).
const REPLICA_PATIENCE: u32 = 5;

/// How long a replica that didn't start waits before it's tried again, after `tries`.
fn replica_wait(tries: u32) -> Duration {
    Duration::from_secs(if tries < REPLICA_PATIENCE { 15 } else { 60 })
}

/// How often a runner checks that its replica follows the server, and how many checks running it
/// may stay behind the server's last write before it's started again.
const REPLICA_CHECK: Duration = Duration::from_secs(60);
const REPLICA_BEHIND: u32 = 5;

/// This machine's replica of kiln's records (docs/p2p.md, P2): where it's kept once started, and
/// what starting it takes.
#[derive(Clone)]
struct ReplicaStart {
    slot: Arc<parking_lot::Mutex<Option<Arc<crate::store::learner::Learner>>>>,
    starting: Arc<std::sync::atomic::AtomicBool>,
    /// The server's last word to join the group as a learner (`None` once it said Leave).
    join: Arc<parking_lot::Mutex<Option<(u64, String)>>>,
    up: async_channel::Sender<Up>,
    dir: PathBuf,
    me: String,
    server: String,
    http: Arc<Client>,
}

impl ReplicaStart {
    /// Starts the replica with the server's newest word to join, unless one is starting already:
    /// on a task of its own (fetching a first copy takes a while).
    fn start(&self) {
        use crate::store::learner::Learner;
        if self.starting.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let r = self.clone();
        crate::rt::spawn(async move {
            let going = r.up.clone();
            let (dir, me, server, http) = (r.dir.clone(), r.me.clone(), r.server.clone(), r.http.clone());
            let started = start_replica(
                &r.join,
                move || going.is_closed(),
                move |member, token| {
                    let (dir, me, server, http) = (dir.clone(), me.clone(), server.clone(), http.clone());
                    async move {
                        // A thread of its own: fetching a copy takes minutes on a large store, and the
                        // blocking pool is the runtime's.
                        let (tx, rx) = async_channel::bounded(1);
                        std::thread::Builder::new()
                            .name("kiln-learner".into())
                            .spawn(move || {
                                let _ = tx.send_blocking(Learner::start(&dir, &me, member, http, &server, &token));
                            })
                            .map_err(|e| anyhow::anyhow!("starting the learner's thread: {e}"))?;
                        rx.recv().await.map_err(|_| anyhow::anyhow!("the learner's thread ended"))?
                    }
                },
                replica_wait,
            )
            .await;
            match started {
                Ok(((learner, said), member)) => {
                    tracing::info!(member, "this machine holds a replica of kiln's records");
                    let learner = Arc::new(learner);
                    crate::store::learner::set_global(Some(Arc::clone(&learner)));
                    *r.slot.lock() = Some(learner);
                    let up = r.up.clone();
                    crate::rt::spawn(async move {
                        while let Ok(m) = said.recv().await {
                            if up.send(Up::Sync(m)).await.is_err() {
                                break;
                            }
                        }
                    });
                }
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "this machine's replica of kiln's records didn't start"),
            }
            r.starting.store(false, std::sync::atomic::Ordering::SeqCst);
        });
    }

    /// Keeps the replica following the server: one whose group stopped, or that stays behind the
    /// server's last write for [`REPLICA_BEHIND`] checks running, is stopped and started again with
    /// the newest word to join. Nothing else would start it again, and its front door would leave
    /// every read to the server for good (adacachy's, on 2026-10-25, applied nothing for hours after
    /// the server restarted).
    async fn watch(self) {
        let mut behind = 0u32;
        loop {
            crate::rt::sleep(REPLICA_CHECK).await;
            if self.up.is_closed() {
                return;
            }
            let Some(learner) = self.slot.lock().clone() else {
                behind = 0;
                continue;
            };
            let why = if let Some(why) = learner.stopped() {
                format!("its cluster group stopped: {why}")
            } else if learner.current(Duration::from_secs(30)).await {
                behind = 0;
                continue;
            } else {
                behind += 1;
                if behind < REPLICA_BEHIND {
                    continue;
                }
                format!("it stayed behind the server's last write for {behind} checks running")
            };
            behind = 0;
            let (seq, applied) = learner.standing();
            drop(learner);
            tracing::warn!(seq, applied, "this machine's replica of kiln's records stopped following the server and starts again: {why}");
            // Taken away as at a Leave (its files let go, waiting for its thread), then started.
            let old = self.slot.lock().take();
            if let Some(old) = old {
                crate::store::learner::set_global(None);
                let _ = crate::rt::blocking(move || drop(old)).await;
            }
            self.start();
        }
    }
}

#[cfg(test)]
mod replica_tests {
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

    /// A replica whose files another process holds is tried again until it starts, with the
    /// server's newest word to join; it stops once the server takes the replica away, or the
    /// runner goes. On adacachy on 2026-10-25 the old build held them for nine minutes, and five
    /// tries gave up after four.
    #[test]
    fn a_replica_is_tried_again_until_it_starts() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let no_wait: fn(u32) -> std::time::Duration = |_| std::time::Duration::ZERO;
            let join = parking_lot::Mutex::new(Some((7u64, "first".to_owned())));
            let tries = AtomicU32::new(0);
            let (got, member) = super::start_replica(
                &join,
                || false,
                |_, token| {
                    let n = tries.fetch_add(1, SeqCst) + 1;
                    if n == 3 {
                        *join.lock() = Some((8, "renewed".into()));
                    }
                    async move { if n < 9 { Err(anyhow::anyhow!("another process still has kiln.db open")) } else { Ok(token) } }
                },
                no_wait,
            )
            .await?;
            anyhow::ensure!(got == "renewed" && member == 8 && tries.load(SeqCst) == 9, "{got} {member} after {} tries", tries.load(SeqCst));
            // Taken away while it's tried: it stops.
            let r = super::start_replica(&join, || false, |_, _| {
                *join.lock() = None;
                async { Err::<String, _>(anyhow::anyhow!("held")) }
            }, no_wait)
            .await;
            anyhow::ensure!(r.as_ref().is_err_and(|e| e.to_string().contains("no longer asks")), "{:?}", r.err());
            // The runner going: it stops.
            *join.lock() = Some((7, "t".into()));
            let n = AtomicU32::new(0);
            let r = super::start_replica(&join, || n.load(SeqCst) >= 2, |_, _| {
                n.fetch_add(1, SeqCst);
                async { Err::<String, _>(anyhow::anyhow!("held")) }
            }, no_wait)
            .await;
            anyhow::ensure!(r.is_err() && n.load(SeqCst) == 2, "{} tries", n.load(SeqCst));
            anyhow::ensure!(super::replica_wait(1).as_secs() == 15 && super::replica_wait(super::REPLICA_PATIENCE).as_secs() == 60);
            Ok(())
        })
    }
}

/// Serve the machine's local sockets (`kiln run`, `kiln spawn`) from this runner process: the new
/// build once it has taken over, or the old one again after a rollback (the paths are the same).
#[cfg(unix)]
fn serve_sockets(reach: Option<&crate::reach::node::Handle>, harness: Option<&Arc<crate::harness::host::Host>>, name: &str, state_dir: &std::path::Path) {
    crate::browser::set_home(state_dir);
    if let Some(r) = reach
        && let Err(e) = crate::reach::local::serve(r.clone(), name.to_owned(), crate::reach::local::path(name, state_dir), true)
    {
        tracing::warn!(error = %format!("{e:#}"), "`kiln run` will not be available on this machine");
    }
    if let Some(h) = harness
        && let Err(e) = crate::harness::socket::serve(h, crate::harness::socket::path(name, state_dir), true)
    {
        tracing::warn!(error = %format!("{e:#}"), "`kiln spawn` will not be available in this machine's terminals");
    }
}

/// Take back everything handed to the new build (it didn't take over). The new build is killed
/// first, so nothing of it hangs up a terminal or reads a pipe this runner takes back; then each
/// terminal and agent session goes on here as it was (a session the new build had stopped
/// starts again from its record). What couldn't come back.
#[cfg(unix)]
async fn roll_back(
    mv: Moving,
    acks: &[Ack],
    why: &str,
    harness: Option<&Arc<crate::harness::host::Host>>,
    terms: &mut HashMap<String, (PtySession, Arc<crate::termsess::Session>)>,
    term_tx: &async_channel::Sender<(String, PtyOut)>,
    state_dir: &std::path::Path,
) -> Vec<String> {
    use std::os::fd::IntoRawFd as _;
    let empty = tracing::field::Empty;
    let span = tracing::info_span!(target: "kiln::upgrade", parent: &mv.root, "upgrade.rollback", reason = %why, result = empty, error = empty);
    // Nothing is taken back until the new build is seen gone: one left running takes messages again
    // a while after its link here ends (`Bg::TakeOverEnded`), and the two would hold the same
    // sessions, each with an inbox of its own (CP-T3.2, the thirteenth adversarial check's). One
    // that can't be stopped keeps what it took over.
    if !stop_new_build(crate::bluegreen::Supervisor::from_env(), mv.green, Duration::from_secs(5)).await {
        tracing::error!(pid = mv.green.pid, "the new build couldn't be stopped: what it took over stays with it, and nothing is taken back here");
        let kept: Vec<String> = mv
            .items
            .iter()
            .map(|item| match item {
                Item::Terminal { session, .. } => format!("terminal {session}: kept by the new build, which couldn't be stopped"),
                Item::Session { record, .. } => format!("agent session {}: kept by the new build, which couldn't be stopped", record.id()),
            })
            .collect();
        span.record("result", "failed");
        span.record("error", "the new build couldn't be stopped");
        mv.root.record("result", "rolled_back");
        mv.root.record("error", why);
        return kept;
    }
    let mut problems = Vec::new();
    for (i, item) in mv.items.into_iter().enumerate() {
        match item {
            Item::Terminal { session, state, fd, pid } => {
                if !take_terminal_back(terms, term_tx, session.clone(), state, fd, pid) {
                    problems.push(format!("terminal {session}"));
                }
            }
            Item::Session { record, kept } => {
                let Some(h) = harness else { continue };
                let id = record.id().to_owned();
                // The new build stopped it (and started it again, now gone with it): it starts again here.
                let stopped = matches!(acks.get(i), Some(Ack::Refused(_)));
                let handed = kept.filter(|_| !stopped).and_then(|(json, [i, o, e])| {
                    let mut hd: crate::harness::session::Handed = serde_json::from_slice(&json).ok()?;
                    hd.process.stdin = i.into_raw_fd();
                    hd.process.stdout = o.into_raw_fd();
                    hd.process.stderr = e.into_raw_fd();
                    // Still this runner's child, reaped where it was spawned: watched here.
                    hd.process.foreign = true;
                    Some(Box::new(hd))
                });
                if let Err(e) = h.move_in(*record, handed) {
                    problems.push(format!("agent session {id}: {e:#}"));
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(crate::upgrade::instance_dir(state_dir, &mv.instance));
    if problems.is_empty() {
        span.record("result", "ok");
    } else {
        span.record("result", "failed");
        span.record("error", problems.join("; ").as_str());
    }
    mv.root.record("result", "rolled_back");
    mv.root.record("error", why);
    problems
}

/// Stops the new build `green`, a rollback's first step: its supervisor `sup` kills it (none, or it
/// didn't: killed here), and it's seen gone within `patience` of each. Whether it's gone.
#[cfg(unix)]
async fn stop_new_build(sup: Option<crate::bluegreen::Supervisor>, green: crate::bluegreen::Green, patience: Duration) -> bool {
    if let Some(sup) = sup
        && let Ok(Err(e)) | Err(e) = crate::rt::blocking(move || sup.kill(green)).await
    {
        // Gone already (it exited, or it was never started), or its supervisor couldn't.
        tracing::debug!(error = %format!("{e:#}"), pid = green.pid, "stopping the new build");
    }
    if gone_within(green.pid, patience).await {
        return true;
    }
    if let Ok(pid) = i32::try_from(green.pid) {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
    }
    gone_within(green.pid, patience).await
}

/// Whether process `pid` is gone within `patience` (looked at every 20 ms).
#[cfg(unix)]
async fn gone_within(pid: u32, patience: Duration) -> bool {
    let until = std::time::Instant::now() + patience;
    while crate::bluegreen::alive(pid) {
        if std::time::Instant::now() >= until {
            return false;
        }
        crate::rt::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// (The new build's runner, blue/green.) Take over from the runner being upgraded, at `path`: say
/// it's ready, take each agent session and terminal it hands over (saying how each went), then the
/// server's frames about them that reached it meanwhile, until that runner ends. Terminals are
/// taken on this runner's loop (`tx`), which also hears once everything is in.
#[cfg(unix)]
async fn take_over_from(path: PathBuf, harness: Option<Arc<crate::harness::host::Host>>, tx: async_channel::Sender<Bg>, me: String) -> anyhow::Result<()> {
    use crate::bluegreen::Frame;
    use base64::Engine as _;
    let conn = Arc::new(crate::rt::blocking(move || crate::bluegreen::connect(&path)).await??);
    let c = Arc::clone(&conn);
    crate::rt::blocking(move || c.send(&Frame::Ready { pid: std::process::id(), instance: me }, &[])).await??;
    tracing::info!(upgrade = %std::env::var("KILN_UPGRADE_ID").unwrap_or_default(), "connected to the runner being upgraded: taking over");
    // Read on a thread of its own: once everything is in, the connection stays open (idle) until
    // the old runner ends.
    let (ftx, frx) = async_channel::unbounded::<anyhow::Result<(Frame, Vec<std::os::fd::OwnedFd>)>>();
    let c = Arc::clone(&conn);
    std::thread::Builder::new().name("kiln-take-over".into()).spawn(move || {
        loop {
            let got = c.recv();
            let end = got.is_err();
            if ftx.send_blocking(got).is_err() || end {
                break;
            }
        }
    })?;
    let (mut sessions, mut terminals, mut refused) = (0usize, 0usize, 0usize);
    let mut done = false;
    while let Ok(got) = frx.recv().await {
        let (frame, fds) = match got {
            Ok(f) => f,
            Err(e) => {
                if !done {
                    anyhow::bail!("the runner being upgraded broke off the handover: {e:#}");
                }
                tracing::info!("the runner this one took over from has ended");
                break;
            }
        };
        let (id, r) = match frame {
            Frame::Session { id, record, handed } => {
                let r = take_session(harness.as_ref(), &id, *record, handed, fds).await;
                if r.is_ok() {
                    sessions += 1;
                }
                (id, r)
            }
            Frame::Terminal { session, command, cols, rows, replay, pid } => {
                let r = match fds.into_iter().next() {
                    None => Err("its PTY didn't come with it".to_owned()),
                    Some(fd) => {
                        let replay = base64::engine::general_purpose::STANDARD.decode(replay.as_bytes()).unwrap_or_default();
                        let (done_tx, done_rx) = futures::channel::oneshot::channel();
                        if tx.send(Bg::TakeTerminal { session: session.clone(), command, cols, rows, replay, fd, pid, done: done_tx }).await.is_err() {
                            Err("this runner is stopping".to_owned())
                        } else {
                            done_rx.await.unwrap_or_else(|_| Err("this runner is stopping".to_owned()))
                        }
                    }
                };
                if r.is_ok() {
                    terminals += 1;
                }
                (session, r)
            }
            Frame::Done => {
                done = true;
                let _ = tx.send(Bg::TookOver { sessions, terminals, refused }).await;
                continue;
            }
            Frame::Promoted => {
                let _ = tx.send(Bg::Promoted).await;
                continue;
            }
            Frame::Forward { data } => {
                if let Ok(d) = base64::engine::general_purpose::STANDARD.decode(data.as_bytes()) {
                    let _ = tx.send(Bg::Forwarded(d)).await;
                }
                continue;
            }
            Frame::Ready { .. } | Frame::Ack { .. } => continue,
        };
        if r.is_err() {
            refused += 1;
        }
        let ack = Frame::Ack { id, ok: r.is_ok(), error: r.err() };
        let c = Arc::clone(&conn);
        crate::rt::blocking(move || c.send(&ack, &[])).await??;
    }
    Ok(())
}

/// (The new build's runner.) An agent session handed over: with its harness process, whose pipes
/// came as `fds` (stdin, stdout, stderr); else started again from its record. When its process
/// can't go on here it is stopped and the session starts again from its record (an error, saying so).
#[cfg(unix)]
async fn take_session(h: Option<&Arc<crate::harness::host::Host>>, id: &str, rec: crate::harness::host::Saved, handed: Option<Box<crate::harness::session::Handed>>, fds: Vec<std::os::fd::OwnedFd>) -> Result<(), String> {
    use std::os::fd::IntoRawFd as _;
    let Some(h) = h else { return Err("this runner runs no agent sessions".to_owned()) };
    if h.get(id).is_some() {
        return Ok(());
    }
    let Some(mut hd) = handed else {
        return h.move_in(rec, None).map_err(|e| format!("{e:#}"));
    };
    let pid = hd.process.pid;
    let mut fds = fds.into_iter();
    let (Some(i), Some(o), Some(e)) = (fds.next(), fds.next(), fds.next()) else {
        let _ = crate::rt::blocking(move || crate::harness::host::stop_process(pid)).await;
        let again = h.move_in(rec, None).map_err(|e| format!("{e:#}"));
        return Err(format!("its pipes didn't come with it, so its process was stopped{}", again.err().map_or_else(|| ": it started again from its record".to_owned(), |e| format!(", and it couldn't start again: {e}"))));
    };
    hd.process.stdin = i.into_raw_fd();
    hd.process.stdout = o.into_raw_fd();
    hd.process.stderr = e.into_raw_fd();
    hd.process.foreign = true;
    let raw = [hd.process.stdin, hd.process.stdout, hd.process.stderr];
    match h.move_in(rec.clone(), Some(hd)) {
        Ok(()) => Ok(()),
        // Refused before its pipes were taken: they close, its process is stopped.
        Err(e) => {
            for r in raw {
                drop(crate::os::reclaim_fd(r));
            }
            let _ = crate::rt::blocking(move || crate::harness::host::stop_process(pid)).await;
            let again = h.move_in(rec, None).map_err(|e| format!("{e:#}"));
            Err(format!("{e:#}; its process was stopped{}", again.err().map_or_else(|| ", and it started again from its record".to_owned(), |e| format!(", and it couldn't start again: {e}"))))
        }
    }
}

/// Completes when the front door (if this runner has one) reports something new.
async fn headroom_changed(h: Option<&crate::headroom::Handle>) {
    match h {
        Some(h) => h.changed().await,
        None => futures::future::pending().await,
    }
}

async fn front_door_changed(fd: Option<&crate::frontdoor::runner::Handle>) {
    match fd {
        Some(h) => h.changed().await,
        None => futures::future::pending().await,
    }
}

/// The next of `rx`, or never when there's none.
async fn recv_opt<T>(rx: Option<&async_channel::Receiver<T>>) -> Option<T> {
    match rx {
        Some(r) => r.recv().await.ok(),
        None => futures::future::pending().await,
    }
}

async fn sleep_until_opt(t: Option<Instant>) {
    match t {
        Some(t) => crate::rt::sleep_until(t).await,
        None => futures::future::pending().await,
    }
}

pub async fn run(o: Opts, shutdown: Shutdown) -> anyhow::Result<()> {
    crate::os::start_reaper();
    crate::updates::warm_own_binary().await;
    // A Mac set up for kiln signing: launchd's re-sign watcher as this kiln writes it.
    if cfg!(target_os = "macos") && !o.ephemeral {
        std::thread::spawn(crate::os::macsign::keep_watching);
    }
    std::fs::create_dir_all(&o.work_dir).with_context(|| format!("create {}", o.work_dir.display()))?;
    // Where the runner's cache service opens once something asks for it (docs/ci.md W24-R08).
    crate::runner::work::service::set_home(&o.work_dir);
    let orphans = crate::exec::reap_orphans(&o.work_dir);
    if orphans > 0 {
        tracing::warn!(orphans, "killed job processes left over from a previous agent run");
    }
    if let Some(max) = o.declared.cache_max.as_deref().filter(|m| crate::runner::cache::parse_size(m).is_none()) {
        tracing::warn!(cache_max = max, "[agent] cache_max isn't a size (`20G`, `512M`): kiln/cache keeps up to 10G");
    }
    let (ca, leaf, enrolled_quic) = credentials(&o, &shutdown).await?;
    // This process's own files (a runner beside another one of the machine shares the state
    // directory, never a file).
    let own_dir = crate::upgrade::own_dir(&o.state_dir, o.instance.as_deref());
    let _ = std::fs::create_dir_all(&own_dir);
    crate::updates::clean_leftovers(&own_dir);
    let instance_id = o.instance.clone().unwrap_or_else(|| crate::upgrade::MAIN.to_owned());
    // A new build started to take over from the runner being upgraded (blue/green): ranked oldest
    // until it's promoted, so new work stays with that runner, which also keeps the machine's
    // local sockets until everything is handed over.
    let take_over_pending: Option<PathBuf> = if cfg!(unix) { std::env::var_os(crate::bluegreen::TAKE_OVER_ENV).map(PathBuf::from) } else { None };
    #[cfg_attr(not(unix), allow(unused_variables))]
    let real_created_at = crate::upgrade::created_at(&o.state_dir, &instance_id);
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut instance_hello = proto::InstanceHello {
        id: instance_id.clone(),
        created_at: if take_over_pending.is_some() { -1 } else { real_created_at },
        pid: std::process::id(),
    };
    // The certificate names this agent (the server checks Hello against it); an approved join may
    // have given a variant of the requested name.
    let name = crate::pki::leaf_name(&leaf).unwrap_or_else(|| o.name.clone());
    if name != crate::pki::sanitize_name(&o.name) {
        tracing::info!(certificate = %name, requested = %o.name, "using the agent name from this agent's certificate");
    }
    let quic_addr = o
        .quic
        .clone()
        .or(enrolled_quic)
        .ok_or_else(|| anyhow::anyhow!("server QUIC address unknown (pass --quic)"))?;
    // HTTP(S) to the server (source archives, artifacts): public roots plus kiln's own CA.
    let http = Arc::new(Client::new(Roots::WebPkiPlusDer(ca.clone()), None)?);
    // A microVM's leg reaches the server as this runner does, so it trusts the same CA (W13-R05).
    #[cfg(target_os = "linux")]
    let microvm_ca = ca.clone();
    // This machine's front door (DNS, HTTPS, a .local name), when the server gives it one. It runs
    // from the settings it stored at once, and keeps running while the server is away.
    let front_door = (!o.ephemeral && !o.outpost).then(|| {
        crate::frontdoor::runner::start(
            crate::frontdoor::runner::Ctx { dir: o.state_dir.join("front-door"), server_url: o.server_url.clone(), ca_der: ca.clone() },
            &shutdown,
        )
    });
    // What the server last heard of it on this connection.
    let mut front_door_sent: Option<String> = None;
    // This machine's headroom (docs/headroom.md): started before any agent session resumes, so a
    // restart's sessions come back a few at a time; it runs from the settings it kept, server or not.
    let headroom = (!o.ephemeral && !o.outpost)
        .then(|| crate::headroom::start(crate::headroom::Ctx { dir: o.state_dir.join("headroom"), server_url: o.server_url.clone(), data_dir: o.state_dir.clone() }));
    // What the server last heard of it, and whether this server takes it.
    let mut headroom_sent: Option<String> = None;
    let mut server_headroom = false;
    let (net, events) = net::start(net::Spec { bind: "0.0.0.0:0".parse()?, ca_der: ca, leaf, server: false, dial_deadline: net::DIAL_DEADLINE })?;
    // A test build's faults: frames lost, sent twice or sent late below the channel, which it
    // makes up for.
    if let Some(c) = crate::runner::chaos::get() {
        net.command(net::Out::Seed(c.seed)).await;
        for rule in c.faults.iter().copied() {
            net.command(net::Out::Shed(rule)).await;
        }
    }
    // UDP to the QUIC address, or the HTTP tunnel when UDP gets no answer (see `crate::tunnel`).
    let mut paths = if o.ephemeral {
        crate::tunnel::Paths::new(&o.server_url, &quic_addr)
    } else {
        crate::tunnel::Paths::remembering(&o.server_url, &quic_addr, crate::upgrade::own_dir(&o.state_dir, o.instance.as_deref()).join("transport"))
    };

    let (up_tx, up_rx) = async_channel::bounded::<Up>(8192);
    // Repo sync: the copies of synced repos this machine holds (none in a job pod or an outpost).
    let sync = (!o.ephemeral && !o.outpost).then(|| {
        let (sync, said) = crate::sync::runner::Runner::start(&o.state_dir, &name, http.clone(), o.server_url.clone());
        // An agent session moving between machines asks it where its folder stands.
        crate::sync::runner::set_global(sync.clone());
        let up = up_tx.clone();
        crate::rt::spawn(async move {
            while let Ok(m) = said.recv().await {
                if up.send(Up::Sync(m)).await.is_err() {
                    break;
                }
            }
        });
        sync
    });
    // The cluster group's learner: this machine's replica of kiln's records (docs/p2p.md, P2),
    // started once the server says which member it is (never in a job pod or an outpost).
    let cluster: Arc<parking_lot::Mutex<Option<Arc<crate::store::learner::Learner>>>> = Arc::default();
    let cluster_starting = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The server's last word to join the group as a learner (`None` once it said Leave): what a
    // replica being started goes on with.
    let cluster_join: Arc<parking_lot::Mutex<Option<(u64, String)>>> = Arc::default();
    // Reach: links to other runners, and the runs this machine does for them (none in a job pod).
    // Agent sessions other runners hand this one (a session moving here) are taken over by its
    // session host, once there is one (below).
    let (adopt_tx, adopt_rx) = async_channel::bounded::<crate::reach::node::Adoption>(4);
    let reach = o.reach.filter(|_| !o.ephemeral).map(|accept| {
        let cfg = crate::reach::node::Config { adopt: Some(adopt_tx.clone()), objects: None, ..crate::reach::node::Config::for_runner(&name, &o.state_dir, accept) };
        (Arc::clone(&cfg.using), cfg.mirrors.clone(), crate::reach::node::start(cfg))
    });
    let reach = reach.and_then(|(using, mirrors, started)| match started {
        Ok((handle, said)) => {
            crate::reach::node::set_global(handle.clone());
            // What other runners' trees left here, kept as people's rules say (docs/reach.md §5.1).
            crate::reach::keeper::start(handle.clone(), using, mirrors);
            let up = up_tx.clone();
            crate::rt::spawn(async move {
                while let Ok(m) = said.recv().await {
                    if up.send(Up::Reach(m)).await.is_err() {
                        break;
                    }
                }
            });
            let path = crate::reach::local::path(&name, &o.state_dir);
            crate::browser::set_home(&o.state_dir);
            if take_over_pending.is_some() {
                crate::reach::local::set_socket(path.to_string_lossy().into_owned());
            } else {
                match crate::reach::local::serve(handle.clone(), name.clone(), path, false) {
                    Ok(p) => crate::reach::local::set_socket(p.to_string_lossy().into_owned()),
                    Err(e) => tracing::warn!(error = %format!("{e:#}"), "`kiln run` will not be available on this machine"),
                }
            }
            Some(handle)
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "reach is off on this runner");
            None
        }
    });
    let uplink = Uplink(up_tx);
    let replica = ReplicaStart {
        slot: cluster.clone(),
        starting: cluster_starting.clone(),
        join: cluster_join.clone(),
        up: uplink.0.clone(),
        dir: o.state_dir.clone(),
        me: name.clone(),
        server: o.server_url.clone(),
        http: http.clone(),
    };
    if !o.ephemeral && !o.outpost {
        crate::rt::spawn(replica.clone().watch());
    }
    // Agent sessions handed to this machine's newer runner process go by way of the server.
    crate::harness::handing::install(uplink.0.clone());
    // Its facts, probed again every 10 minutes (docs/ci.md W13-R01); once on each connection.
    let probing = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if !o.ephemeral && !o.outpost {
        let (up, dir, declared, busy, stop) = (uplink.clone(), o.work_dir.clone(), o.declared.clone(), Arc::clone(&probing), shutdown.clone());
        crate::rt::spawn(async move {
            while stop.sleep(Duration::from_secs(600)).await {
                probe_facts(&up, &dir, &declared, &busy);
            }
        });
    }
    let (done_tx, done_rx) = async_channel::unbounded::<JobResult>();
    let mut buffer: VecDeque<(u32, Kind, Vec<u8>)> = VecDeque::new();
    let mut buffered = 0usize;
    // Jobs' results the server hasn't said it has (`ServerMsg::Received`), each encoded, by job:
    // sent again after a reconnect, so one sent as the connection dropped isn't lost.
    let mut unreceived: HashMap<String, (Instant, Vec<u8>)> = HashMap::new();
    // Each job's events (`AgentMsg::JobEvent`, numbered by their `seq`), encoded, until its result
    // is received, by job: sent again after a reconnect too, to a server that takes each once (it
    // said so: `receipts`). The count goes on after (a kept leg's step rerun, W17-R04).
    let mut journal: HashMap<String, (Instant, u64, Vec<Vec<u8>>)> = HashMap::new();
    let mut receipts = false;
    let mut connected = false;
    // The server has taken this connection's Hello (it said Welcome). Logs, on a lane of their own,
    // wait for that, or the server, not knowing whose they are yet, drops them; and jobs' messages
    // with them, to come after what was held.
    let mut welcomed = false;
    let mut dial_backoff = Duration::from_millis(500);
    let mut redial: Option<Instant> = Some(Instant::now());
    let mut dialing_since: Option<Instant> = None;
    // A test build's faults (W22-AC04): the connection dropped every so often, and what's sent the
    // server late.
    let chaos = crate::runner::chaos::get();
    let mut chaos_drop: Option<Instant> = None;
    // A test build's link cut for a while: no dialing again before this.
    let mut chaos_down: Option<Instant> = None;
    let (delay_line, delayed) = match chaos.and_then(|c| c.delay).map(crate::runner::chaos::DelayLine::new) {
        Some((line, due)) => (Some(line), Some(due)),
        None => (None, None),
    };
    let mut jobs: HashMap<String, Running> = HashMap::new();
    // Pools' executors this runner started (docs/ci.md W13-R09).
    let pool_execs = crate::runner::pool::Executors::default();
    crate::runner::pool::sweep(&o.work_dir);
    // And the containers and networks this runner left running when it last stopped.
    crate::runner::docker::sweep(&o.work_dir).await;
    // Its microVM deployments (W15-R02), booted again: they end with the runner.
    #[cfg(target_os = "linux")]
    crate::rt::spawn(crate::runner::firecracker::restore(o.work_dir.join("_firecracker")));
    // What this machine is, for what exclusive jobs hold of it (W05-R03).
    let size = crate::runner::probe::size();
    let mut draining = false;
    // Terminals outlive the server connection: kept (with their replay state) across disconnects and
    // announced again on every reconnect.
    let mut terms: HashMap<String, (PtySession, Arc<crate::termsess::Session>)> = HashMap::new();
    // Terminals into legs asked for before the leg had its shell, by session: taken again once it
    // has, with what was typed meanwhile.
    let mut shell_waits: HashMap<String, Vec<Vec<u8>>> = HashMap::new();
    let (term_tx, term_rx) = async_channel::bounded::<(String, PtyOut)>(1024);
    // Upgrades: a new binary on disk is taken up once no job runs, every terminal handed on so
    // shells survive. On Linux this process re-executes itself (same pid). On macOS, where a
    // process that re-executed itself loses the local network, it hands the terminals to its
    // supervisor and exits, and the supervisor starts the new binary as a fresh process.
    let exe = std::env::current_exe().ok();
    let mut exe_mtime = exe.as_deref().and_then(binary_mtime);
    let mut upgrade_pending = false;
    // Only a `kiln agent` process upgrades itself: the server's in-process agent shares its
    // process with the server, whose upgrades its supervisor handles.
    let standalone = std::env::args().skip(1).find(|a| !a.starts_with('-')).as_deref() == Some("agent");
    let started_as = adopt_handover(&o, &mut terms, &term_tx);
    let successor = crate::os::UPGRADE_BY_SUCCESSOR && standalone && !o.ephemeral;
    let channel = crate::os::supervise::has_supervisor_channel();
    let supervised = std::env::var("KILN_SUPERVISED").is_ok_and(|v| v == "1");
    // On macOS a supervised runner its supervisor can't hand over for (an older supervisor, or
    // one an older kiln re-executed in place, which may have cut this process tree off from the
    // local network) has launchd start it afresh, from its launcher, once it is idle.
    let mut relaunch_wanted = successor && supervised && !channel && started_as == Start::ReExecuted;
    let mut relaunch_asked: Option<Instant> = None;
    // AI coding sessions running on this machine (job pods have none), reported as they change.
    let ai = (!o.ephemeral).then(|| crate::aisessions::Reporter::new(crate::os::home_dir()));
    // Kiln-native agent sessions on this machine (not in job pods, and not in the server's own
    // in-process agent: the server hosts those itself). They outlive the server connection and
    // this process's restarts: each continues its harness session.
    let (harness_tx, harness_rx) = async_channel::bounded::<(String, crate::harness::core::Update)>(8192);
    // Their updates over the channel, once the server says it keeps it (docs/wire.md): kept until
    // the server has them, and sent again after a reconnect from where it has them, never whole.
    let mut updates = crate::wire::uplink::Updates::new(format!("{:016x}", rand::random::<u64>()));
    // The server keeps the channel: it said so on this connection, before its Welcome.
    let mut server_wire = false;
    // The server takes what this runner's QUIC layer dropped (`AgentMsg::NetDrops`), and what it
    // was last told of it on this connection.
    let mut server_drops = false;
    let mut drops_said: Option<Vec<proto::NetDrop>> = None;
    // The server takes what this runner holds warm (`AgentMsg::WorkState`, docs/ci.md W24-R18),
    // and when it was last told it on this connection, and as of which change.
    // W24-R31: the reaper of this runner's warm workspaces, on its clock.
    if !o.outpost && !o.ephemeral {
        crate::runner::warm::start_reaper(&o.work_dir);
    }
    let mut server_work = false;
    let mut work_said: Option<(u64, Instant)> = None;
    // The channel's updates go (a server that keeps it welcomed this runner since it started).
    let mut updates_on = false;
    // The server's commands on the channel (`crate::wire::commands`): what this runner has of
    // each stream; and the one being applied, answered at the top of the next turn of the loop
    // (however its arm ended), taken unless its arm refused it.
    let mut wire_in = crate::wire::channel::Receiver::new();
    // Which of the server's command streams are current: an item of one let go of is dropped.
    let mut wire_current = crate::wire::commands::Current::new();
    let mut answer_after: Option<(String, u64, Option<(crate::wire::channel::Refusal, String)>)> = None;
    // What the server's copy of each session here has, said in digests on this connection: a
    // stream that starts sends only what the copy lacks (`crate::wire::sums`).
    let mut server_kept: HashMap<String, proto::KeptSession> = HashMap::new();
    // The sessions that announced themselves in this process (their Open went by): one that
    // hasn't yet starts its stream with its announcement, not before it (which then comes whole).
    let mut announced: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Blue/green: this runner's upgrade under way (it is the old one), its steps' results, and (it is
    // the new one) the runner it takes over from.
    #[cfg_attr(not(unix), allow(unused_variables))]
    let (bg_tx, bg_rx) = async_channel::unbounded::<Bg>();
    #[cfg(unix)]
    let mut moving: Option<Moving> = None;
    #[cfg(unix)]
    let mut take_over: Option<PathBuf> = take_over_pending.clone();
    // Handed everything to the new build: it retires once its own jobs (and any terminal that
    // couldn't go) end, `retire_at` at the soonest. What it handed over, and where the server's
    // frames about those go meanwhile.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut retiring = false;
    let mut retire_at: Option<Instant> = None;
    let mut retire_span: Option<tracing::Span> = None;
    let mut retire_job: Option<String> = None;
    #[cfg(unix)]
    let mut moved: std::collections::HashSet<crate::instances::Owned> = std::collections::HashSet::new();
    #[cfg(unix)]
    let mut forward: Option<async_channel::Sender<Vec<u8>>> = None;
    // The local sockets (`kiln spawn`, `kiln run`) are the machine's: a new build taking over
    // serves them once it holds the sessions.
    #[cfg(unix)]
    let mut sockets_served = take_over_pending.is_none();
    // It took the sessions over (the runner it upgrades said that was everything).
    #[cfg(unix)]
    let mut took_over = false;
    // Conversations kept on disk as they happen (docs/conversations.md), and sent to the server's
    // store over this link (`crate::convo::feed`).
    let mut convo: Option<crate::convo::store::Store> = None;
    // How many agent sessions it has at work at once (docs/agents.md, "How many agents a machine
    // runs"): what was set for this machine from the server (kept here for its restarts), else its
    // config's, else kiln's default. What the server sets is taken as it says it, and the config is
    // read again as it changes.
    let mut limit_asked = crate::harness::limit::kept(&o.state_dir);
    let mut limit_config = o.declared.max_agent_sessions;
    // What the server was told this machine holds, on this connection; and whether it takes that.
    let mut limit_said: Option<crate::harness::limit::Held> = None;
    let mut server_limit = false;
    let harness = (!o.ephemeral && o.terminal).then(|| {
        // Agents' workspaces of their own: one place per machine, whichever runner process holds them.
        crate::workspace::slates::init(&o.state_dir);
        convo = crate::convo::open(&own_dir, crate::config::Conversations::default().upkeep());
        let sink = convo.as_ref().map_or_else(|| harness_tx.clone(), |st| crate::convo::store::tee(harness_tx.clone(), st.clone(), 8192));
        let held = crate::harness::limit::held(limit_asked, limit_config);
        if held.from != crate::harness::limit::Source::Default {
            tracing::info!(max_sessions = held.max_sessions, from = ?held.from, "agent sessions: this machine has at most this many at work at once");
        }
        let limits = crate::harness::host::Limits { per_machine: held.max_sessions };
        let h = crate::harness::host::Host::new(sink, Some(own_dir.join("harness.json")), limits);
        // A new build taking over takes no messages till it's promoted (`Host::hold`).
        if take_over_pending.is_some() {
            h.hold(true);
        }
        h.restore_from(convo.as_ref());
        h
    });
    // The harnesses its user's login shell finds are this machine's too
    // (`crate::harness::kinds::search_path`): that shell is asked now, and every ten minutes.
    if harness.is_some() {
        crate::rt::spawn(crate::reach::serve::login_env());
    }
    // A copy of the server's store this runner keeps, if the server picks it (a mirror), and
    // that copy sent back while the server restores its store from it.
    let mut mirror: Option<crate::convo::feed::Intake> = None;
    let mut mirror_store: Option<crate::convo::store::Store> = None;
    let mut restoring: Option<(String, crate::convo::feed::Shipper)> = None;
    let shipper = convo.clone().map(|st| {
        // The harnesses' own records, followed into the store.
        if let Some(h) = harness.clone() {
            let follow = h;
            if let Err(e) = crate::convo::record::Recorder::start(st.clone(), move || follow.record_sources()) {
                tracing::warn!(error = %format!("{e:#}"), "the harnesses' records won't be kept in the conversation store");
            }
        }
        let (shipper, said) = crate::convo::feed::Shipper::start(st);
        let up = uplink.0.clone();
        crate::rt::spawn(async move {
            while let Ok((json, data)) = said.recv().await {
                if up.send(Up::Convo(AgentMsg::Convo { json, data })).await.is_err() {
                    break;
                }
            }
        });
        shipper
    });
    // What `kiln spawn` in this machine's terminals talks to.
    let harness_sock = harness.as_ref().and_then(|h| {
        let path = crate::harness::socket::path(&name, &o.state_dir);
        if take_over_pending.is_some() {
            return Some(path.to_string_lossy().into_owned());
        }
        match crate::harness::socket::serve(h, path, false) {
            Ok(p) => Some(p.to_string_lossy().into_owned()),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "`kiln spawn` will not be available in this machine's terminals");
                None
            }
        }
    });
    // Sessions moving here: taken over by this machine's session host (none: refused, saying why).
    {
        let host = harness.clone();
        crate::rt::spawn(async move {
            while let Ok(a) = adopt_rx.recv().await {
                let crate::reach::node::Adoption { from, data, reply } = a;
                let answer = match &host {
                    Some(h) => {
                        let h = Arc::clone(h);
                        // Stopped part-way (it panicked): it may have been taken over, so not "not taken".
                        crate::rt::blocking(move || h.adopt(&from, &data)).await.unwrap_or_else(|_| Err(crate::reach::node::DROPPED.to_owned()))
                    }
                    None => Err(format!("{}this runner runs no agent sessions", crate::reach::node::NOT_TAKEN)),
                };
                let _ = reply.send(answer);
            }
        });
    }
    let mut harness_wait: Option<Instant> = None;
    // What it last told the server of its machine's say on upgrades, and the newer kiln it heard of.
    let mut accept_said: Option<bool> = None;
    // Its battery: when it was last read, and what was last said of it, when.
    let mut power_read_at: Option<Instant> = None;
    let mut power_said: Option<(crate::os::battery::Battery, Instant)> = None;
    let mut noticed: Option<String> = None;
    // The harnesses this machine offers, as last reported (rechecked every minute or so).
    let mut harness_kinds_sent: Option<String> = None;
    let mut harness_kinds_tick: u32 = 0;
    // Whether its user's login shell had said its PATH when they were last looked at.
    let mut users_path_known = false;
    let mut sampler = crate::host::Sampler::new(Some(&o.work_dir));
    let mut sample_tick = Interval::new(Duration::from_secs(5));
    let mut heartbeat = Interval::new(Duration::from_secs(10));
    let mut watchdog = Interval::new(Duration::from_secs(5));
    let mut served_dedicated = false;
    let started = Instant::now();

    let hello = |running: Vec<String>| Hello {
        protocol: proto::VERSION,
        name: name.clone(),
        labels: o.labels.clone(),
        // An outpost takes no jobs: none of its slots are free (`Hub::pick_agent`).
        slots: if o.outpost { 0 } else { o.slots.max(1) },
        hostname: crate::config::Config::hostname(),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        ephemeral: o.ephemeral,
        dedicated_job: o.dedicated_job.clone(),
        running,
        terminal: o.terminal,
    };

    'events: loop {
        if let Some((stream, seq, refusal)) = answer_after.take() {
            let a = match refusal {
                Some((code, why)) => wire_in.refused(&stream, seq, code, why),
                None => wire_in.took(&stream, seq),
            };
            uplink.0.send(Up::Term(AgentMsg::Wire(a))).await.ok();
        }
        // Ephemeral agents leave once their one job is done and everything is delivered: its
        // result received, or given 30 s (a server older than receipts never says).
        if o.ephemeral && served_dedicated && jobs.is_empty() && buffer.is_empty() && up_rx.is_empty() && unreceived.values().all(|(at, _)| at.elapsed() > Duration::from_secs(30)) {
            tracing::info!("ephemeral agent finished its job; exiting");
            crate::rt::sleep(Duration::from_millis(500)).await;
            return Ok(());
        }
        if o.ephemeral && !served_dedicated && jobs.is_empty() && started.elapsed() > Duration::from_secs(900) {
            anyhow::bail!("ephemeral agent received no job within 15 minutes");
        }
        if draining && jobs.is_empty() && o.ephemeral {
            return Ok(());
        }
        if retiring && jobs.is_empty() && terms.is_empty() && retire_at.is_some_and(|t| Instant::now() >= t) {
            tracing::info!("handed everything to the new build, and its own jobs are done: this runner retires");
            if let Some(job) = retire_job.take() {
                uplink.msg(AgentMsg::UpgradeLog { job, line: "the old runner's last jobs are done: it retired".to_owned() }).await;
                crate::rt::sleep(Duration::from_millis(300)).await;
            }
            if let Some(sp) = retire_span.take() {
                sp.record("result", "ok");
            }
            // Its other engines' browsers don't outlive it (Chromium's go with their pipes).
            crate::browser::driven::stop_all().await;
            return Ok(());
        }
        let ev = futures::select_biased! {
            () = shutdown.cancelled().fuse() => Ev::Shutdown,
            ev = events.recv().fuse() => Ev::Net(ev.ok()),
            r = done_rx.recv().fuse() => match r { Ok(r) => Ev::Done(r), Err(_) => Ev::Watchdog },
            () = heartbeat.tick().fuse() => Ev::Heartbeat,
            () = sample_tick.tick().fuse() => Ev::Sample,
            () = watchdog.tick().fuse() => Ev::Watchdog,
            () = sleep_until_opt(redial).fuse() => Ev::Redial,
            () = sleep_until_opt(chaos_drop).fuse() => Ev::ChaosDrop,
            d = recv_opt(delayed.as_ref()).fuse() => match d { Some(due) => Ev::Delayed(due), None => Ev::Watchdog },
            t = term_rx.recv().fuse() => match t { Ok((s, out)) => Ev::Term(s, out), Err(_) => Ev::Watchdog },
            up = up_rx.recv().fuse() => match up { Ok(u) => Ev::Up(u), Err(_) => Ev::Watchdog },
            h = harness_rx.recv().fuse() => match h { Ok((s, u)) => Ev::Harness(s, u), Err(_) => Ev::Watchdog },
            () = front_door_changed(front_door.as_ref()).fuse() => Ev::FrontDoor,
            () = headroom_changed(headroom.as_ref()).fuse() => Ev::Headroom,
            b = bg_rx.recv().fuse() => match b {
                #[cfg(unix)]
                Ok(Bg::Forwarded(data)) => Ev::Forwarded(data),
                // Unless the terminal was closed meanwhile.
                Ok(Bg::Again(session, data)) => if shell_waits.contains_key(&session) { Ev::Forwarded(data) } else { Ev::Watchdog },
                #[cfg(unix)]
                Ok(b) => Ev::Bg(b),
                Err(_) => Ev::Watchdog,
            },
            () = sleep_until_opt(retire_at.filter(|_| retiring && jobs.is_empty() && terms.is_empty())).fuse() => Ev::Watchdog,
        };
        match ev {
            Ev::Shutdown => {
                for r in jobs.values() {
                    r.cancel.cancel();
                }
                crate::browser::driven::stop_all().await;
                return Ok(());
            }
            Ev::Delayed(crate::runner::chaos::Due { lane, kind, data, again }) => {
                // As an undelayed frame goes: sent while connected and welcomed, kept otherwise,
                // unless the next connection sends it again in its place (a leg's event or result).
                if connected && (welcomed || (lane != LANE_LOGS && lane != LANE_CONTROL)) {
                    net.send(SERVER_PEER, lane, kind, data).await;
                } else if kind != Kind::Term && !again {
                    buffered += data.len();
                    buffer.push_back((lane, kind, data));
                }
            }
            Ev::ChaosDrop => {
                chaos_drop = None;
                if connected {
                    chaos_down = chaos.and_then(|c| c.down_for).map(|d| Instant::now() + d);
                    tracing::warn!(down_for = ?chaos.and_then(|c| c.down_for), "chaos: dropping the connection to the server");
                    net.command(Out::Disconnect { peer: SERVER_PEER }).await;
                }
            }
            #[cfg(unix)]
            Ev::Bg(b) => {
                use crate::bluegreen::Frame;
                use std::os::fd::AsRawFd as _;
                let empty = tracing::field::Empty;
                match b {
                    Bg::Fetched { job, exe, build } => {
                        let root = tracing::info_span!(target: "kiln::upgrade", "upgrade", runner = %name, from = %crate::updates::own_build(), to = %build, upgrade_id = %job, result = empty, error = empty);
                        let start = tracing::info_span!(target: "kiln::upgrade", parent: &root, "upgrade.start", result = empty, error = empty);
                        // The new build runs as another instance of this machine, newer than this one.
                        let gid = crate::util::new_id();
                        let idir = crate::upgrade::instance_dir(&o.state_dir, &gid);
                        let created_at = crate::util::now_unix().max(crate::upgrade::created_at(&o.state_dir, &instance_id) + 1);
                        let rec = crate::upgrade::InstanceRecord { id: gid.clone(), created_at };
                        let sock = crate::bluegreen::handover_path(&own_dir);
                        let args = with_instance(&std::env::args().skip(1).collect::<Vec<_>>(), &gid);
                        let env = vec![(crate::bluegreen::TAKE_OVER_ENV.to_owned(), sock.display().to_string()), ("KILN_UPGRADE_ID".to_owned(), job.clone())];
                        let started: anyhow::Result<(crate::bluegreen::Green, crate::bluegreen::Listener)> = async {
                            std::fs::create_dir_all(&idir)?;
                            crate::util::write_secret(&idir.join("instance.json"), &serde_json::to_vec_pretty(&rec)?, 0o644)?;
                            let listener = crate::bluegreen::Listener::bind(&sock)?;
                            let sup = crate::bluegreen::Supervisor::from_env().context("this runner's supervisor can't start a new build beside it")?;
                            let green = crate::rt::blocking(move || sup.start(&exe, &args, &env)).await??;
                            Ok((green, listener))
                        }
                        .await;
                        match started {
                            Ok((green, listener)) => {
                                start.record("result", "ok");
                                drop(start);
                                let healthy = tracing::info_span!(target: "kiln::upgrade", parent: &root, "upgrade.healthy", result = empty, error = empty);
                                let line = format!("started the new build beside this runner (pid {}); it has {} s to connect to the server", green.pid, crate::bluegreen::READY_WITHIN.as_secs());
                                uplink.msg(AgentMsg::UpgradeLog { job: job.clone(), line }).await;
                                let (tx, j) = (bg_tx.clone(), job.clone());
                                let waited = std::thread::Builder::new().name("kiln-upgrade".into()).spawn(move || {
                                    let _ = match listener.ready(green.pid, Instant::now() + crate::bluegreen::READY_WITHIN) {
                                        Ok((conn, _)) => tx.send_blocking(Bg::Ready { job: j, conn: Arc::new(conn) }),
                                        Err(e) => tx.send_blocking(Bg::Failed { job: j, why: format!("{e:#}") }),
                                    };
                                });
                                moving = Some(Moving { green, instance: gid, root, healthy: Some(healthy), handing: false, items: Vec::new(), held: Vec::new() });
                                if let Err(e) = waited {
                                    let _ = bg_tx.try_send(Bg::Failed { job, why: format!("couldn't wait for the new build: {e}") });
                                }
                            }
                            Err(e) => {
                                let why = format!("{e:#}");
                                start.record("result", "failed");
                                start.record("error", why.as_str());
                                root.record("result", "failed");
                                root.record("error", why.as_str());
                                let _ = std::fs::remove_dir_all(&idir);
                                tracing::warn!(%job, error = %why, "couldn't start the new build beside this runner");
                                uplink.msg(AgentMsg::UpgradeDone { job, ok: false, error: Some(format!("couldn't start the new build beside this runner: {why}")) }).await;
                            }
                        }
                    }
                    Bg::Ready { job, conn } => {
                        let Some(mv) = moving.as_mut() else { continue };
                        if let Some(h) = mv.healthy.take() {
                            h.record("result", "ok");
                        }
                        mv.handing = true;
                        let mut frames: Vec<(Frame, Vec<std::os::fd::RawFd>)> = Vec::new();
                        let ids: Vec<String> = terms.keys().cloned().collect();
                        for id in ids {
                            let Some((pty, state)) = terms.remove(&id) else { continue };
                            match pty.hand_off() {
                                Ok((fd, pid)) => {
                                    use base64::Engine as _;
                                    let info = state.info();
                                    let a = state.attach();
                                    state.detach();
                                    let replay = base64::engine::general_purpose::STANDARD.encode(&a.replay);
                                    frames.push((Frame::Terminal { session: id.clone(), command: state.command.clone(), cols: info.cols, rows: info.rows, replay, pid }, vec![fd.as_raw_fd()]));
                                    moved.insert(crate::instances::Owned::Terminal(id.clone()));
                                    mv.items.push(Item::Terminal { session: id, state, fd, pid });
                                }
                                // It can't go over: it stays here, and this runner with it until it ends.
                                Err(pty) => {
                                    tracing::warn!(session = %id, "a terminal couldn't be handed to the new build; it stays with this runner");
                                    terms.insert(id, (pty, state));
                                }
                            }
                        }
                        if let Some(h) = &harness {
                            for (rec, handed) in h.move_out().await {
                                let id = rec.id().to_owned();
                                let mut handed = handed;
                                let kept = match handed.as_ref() {
                                    Some(hd) => match keep_pipes(&hd.process) {
                                        Some(pipes) => serde_json::to_vec(hd).ok().map(|json| (json, pipes)),
                                        None => None,
                                    },
                                    None => None,
                                };
                                // Not handed over running after all: stopped, it starts again from its record there.
                                if kept.is_none()
                                    && let Some(hd) = handed.take()
                                {
                                    let pid = hd.process.pid;
                                    let _ = crate::rt::blocking(move || crate::harness::host::stop_process(pid)).await;
                                }
                                let fds = kept.as_ref().map(|(_, p)| p.iter().map(|f| f.as_raw_fd()).collect()).unwrap_or_default();
                                frames.push((Frame::Session { id: id.clone(), record: Box::new(rec.clone()), handed }, fds));
                                moved.insert(crate::instances::Owned::Agent(id));
                                mv.items.push(Item::Session { record: Box::new(rec), kept });
                            }
                        }
                        let (sessions, terminals) = (mv.items.iter().filter(|i| matches!(i, Item::Session { .. })).count(), mv.items.iter().filter(|i| matches!(i, Item::Terminal { .. })).count());
                        let line = format!("the new build is connected to the server: handing it {sessions} agent session(s) and {terminals} terminal(s), their processes running on");
                        uplink.msg(AgentMsg::UpgradeLog { job: job.clone(), line }).await;
                        let tx = bg_tx.clone();
                        let sent = std::thread::Builder::new().name("kiln-handover".into()).spawn(move || {
                            let mut acks = Vec::with_capacity(frames.len());
                            let mut broke: Option<String> = None;
                            for (f, fds) in &frames {
                                if broke.is_some() {
                                    acks.push(Ack::Skipped);
                                    continue;
                                }
                                match conn.send(f, fds).and_then(|()| conn.recv()) {
                                    Ok((Frame::Ack { ok: true, .. }, _)) => acks.push(Ack::Taken),
                                    Ok((Frame::Ack { error, .. }, _)) => acks.push(Ack::Refused(error.unwrap_or_else(|| "refused".to_owned()))),
                                    Ok(_) => {
                                        broke = Some("the new build answered out of turn".to_owned());
                                        acks.push(Ack::Lost("an answer out of turn".to_owned()));
                                    }
                                    Err(e) => {
                                        let e = format!("{e:#}");
                                        acks.push(Ack::Lost(e.clone()));
                                        broke = Some(e);
                                    }
                                }
                            }
                            if broke.is_none()
                                && let Err(e) = conn.send(&Frame::Done, &[])
                            {
                                broke = Some(format!("{e:#}"));
                            }
                            let _ = tx.send_blocking(Bg::Moved { job, conn, acks, broke });
                        });
                        if let Err(e) = sent {
                            tracing::error!(error = %e, "couldn't hand over to the new build");
                        }
                    }
                    Bg::Moved { job, conn, acks, broke } => {
                        let Some(mut mv) = moving.take() else { continue };
                        for (item, ack) in mv.items.iter().zip(&acks) {
                            let sp = tracing::info_span!(target: "kiln::upgrade", parent: &mv.root, "upgrade.move", what = item.what(), id = %item.id(), result = empty, error = empty);
                            match ack {
                                Ack::Taken => sp.record("result", "ok"),
                                Ack::Skipped => sp.record("result", "skipped"),
                                Ack::Refused(e) | Ack::Lost(e) => sp.record("result", "failed").record("error", e.as_str()),
                            };
                        }
                        let promoted = match &broke {
                            Some(e) => Err(format!("the handover broke off: {e}")),
                            None => match crate::bluegreen::Supervisor::from_env() {
                                Some(sup) => {
                                    let green = mv.green;
                                    crate::rt::blocking(move || sup.promote(green)).await.map_err(|e| format!("{e:#}")).and_then(|r| r.map_err(|e| format!("couldn't promote it: {e:#}")))
                                }
                                None => Err("this runner's supervisor is gone".to_owned()),
                            },
                        };
                        match promoted {
                            Ok(()) => {
                                let mut said = Vec::new();
                                let (mut sessions, mut terminals) = (0usize, 0usize);
                                for (item, ack) in std::mem::take(&mut mv.items).into_iter().zip(acks) {
                                    match (item, ack) {
                                        (Item::Session { .. }, Ack::Taken) => sessions += 1,
                                        (Item::Terminal { .. }, Ack::Taken) => terminals += 1,
                                        // The new build started it again from its record.
                                        (Item::Session { record, .. }, Ack::Refused(e) | Ack::Lost(e)) => said.push(format!("agent session {} started again from its record there: {e}", record.id())),
                                        // Back here: it ends with this runner (which waits for it).
                                        (Item::Terminal { session, state, fd, pid }, ack) => {
                                            moved.remove(&crate::instances::Owned::Terminal(session.clone()));
                                            let why = match ack {
                                                Ack::Refused(e) | Ack::Lost(e) => e,
                                                _ => "not sent".to_owned(),
                                            };
                                            if take_terminal_back(&mut terms, &term_tx, session.clone(), state, fd, pid) {
                                                said.push(format!("terminal {session} stays with this runner (it couldn't go: {why}); this runner ends with it"));
                                            }
                                        }
                                        (Item::Session { .. }, Ack::Skipped) => {}
                                    }
                                }
                                // What reached this runner meanwhile, then whatever still does, goes to the new build.
                                let (ftx, frx) = async_channel::unbounded::<Vec<u8>>();
                                for d in std::mem::take(&mut mv.held) {
                                    let _ = ftx.try_send(d);
                                }
                                let c = Arc::clone(&conn);
                                let fwd = std::thread::Builder::new().name("kiln-forward".into()).spawn(move || {
                                    use base64::Engine as _;
                                    // It is the runner now: it says so to the server, which sends it new work.
                                    if c.send(&Frame::Promoted, &[]).is_err() {
                                        return;
                                    }
                                    while let Ok(d) = frx.recv_blocking() {
                                        let data = base64::engine::general_purpose::STANDARD.encode(&d);
                                        if c.send(&Frame::Forward { data }, &[]).is_err() {
                                            break;
                                        }
                                    }
                                });
                                if fwd.is_ok() {
                                    forward = Some(ftx);
                                }
                                for line in said {
                                    uplink.msg(AgentMsg::UpgradeLog { job: job.clone(), line }).await;
                                }
                                let left = jobs.len();
                                let line = format!("handed to the new build: {sessions} agent session(s) and {terminals} terminal(s), their processes still running; {}", if left == 0 { "this runner retires".to_owned() } else { format!("this runner finishes its {left} job(s), then retires") });
                                uplink.msg(AgentMsg::UpgradeLog { job: job.clone(), line }).await;
                                uplink.msg(AgentMsg::UpgradeDone { job: job.clone(), ok: true, error: None }).await;
                                mv.root.record("result", "ok");
                                retire_span = Some(tracing::info_span!(target: "kiln::upgrade", parent: &mv.root, "upgrade.retire", jobs = left, result = empty, error = empty));
                                tracing::info!(%job, sessions, terminals, jobs = left, "handed over to the new build; this runner retires once its jobs end");
                                retiring = true;
                                // The new build keeps the copies of other runners' trees from now on.
                                crate::reach::keeper::retire();
                                // No new jobs here: this runner finishes its own, then retires.
                                draining = true;
                                retire_at = Some(Instant::now() + RETIRE_QUIET);
                                retire_job = Some(job);
                            }
                            Err(why) => {
                                let mut held_back = std::mem::take(&mut mv.held);
                                let problems = roll_back(mv, &acks, &why, harness.as_ref(), &mut terms, &term_tx, &o.state_dir).await;
                                moved.clear();
                                draining = false;
                                serve_sockets(reach.as_ref(), harness.as_ref(), &name, &o.state_dir);
                                announce(term_list(&terms), harness.as_ref(), &uplink).await;
                                for d in held_back.drain(..) {
                                    let _ = bg_tx.try_send(Bg::Forwarded(d));
                                }
                                let mut error = format!("the new build didn't take over ({why}): it was stopped, and this runner goes on with everything");
                                if !problems.is_empty() {
                                    error.push_str(&format!("; but these couldn't come back: {}", problems.join("; ")));
                                }
                                tracing::warn!(%job, %error, "blue/green upgrade rolled back");
                                uplink.msg(AgentMsg::UpgradeDone { job, ok: false, error: Some(error) }).await;
                            }
                        }
                    }
                    Bg::Failed { job, why } => {
                        if let Some(mut mv) = moving.take() {
                            if let Some(h) = mv.healthy.take() {
                                h.record("result", "failed");
                                h.record("error", why.as_str());
                            }
                            let held_back = std::mem::take(&mut mv.held);
                            let _ = roll_back(mv, &[], &why, harness.as_ref(), &mut terms, &term_tx, &o.state_dir).await;
                            serve_sockets(reach.as_ref(), harness.as_ref(), &name, &o.state_dir);
                            for d in held_back {
                                let _ = bg_tx.try_send(Bg::Forwarded(d));
                            }
                        }
                        moved.clear();
                        draining = false;
                        tracing::warn!(%job, error = %why, "the new build didn't come up; this runner goes on");
                        uplink.msg(AgentMsg::UpgradeDone { job, ok: false, error: Some(format!("the new build didn't come up ({why}): it was stopped, and this runner goes on as it was")) }).await;
                    }
                    Bg::TakeTerminal { session, command, cols, rows, replay, fd, pid, done } => {
                        let state = crate::termsess::Session::detached(session.clone(), "agent".into(), command, cols, rows);
                        state.output(&replay);
                        let r = if take_terminal_back(&mut terms, &term_tx, session, state, fd, pid) { Ok(()) } else { Err("its PTY didn't come through".to_owned()) };
                        let _ = done.send(r);
                    }
                    Bg::TookOver { sessions, terminals, refused } => {
                        took_over = true;
                        tracing::info!(sessions, terminals, refused, "took over from the runner being upgraded");
                        // The server learns they run here now.
                        announce(term_list(&terms), harness.as_ref(), &uplink).await;
                        if !std::mem::replace(&mut sockets_served, true) {
                            serve_sockets(reach.as_ref(), harness.as_ref(), &name, &o.state_dir);
                        }
                    }
                    Bg::TakeOverEnded => {
                        if !std::mem::replace(&mut sockets_served, true) {
                            serve_sockets(reach.as_ref(), harness.as_ref(), &name, &o.state_dir);
                        }
                        // Its link to the runner it upgraded ended after it took everything over, and
                        // no word of its promotion: that one is gone (one that takes its sessions
                        // back stops this one first). It takes messages again after a while.
                        if took_over
                            && let Some(h) = harness.as_ref().filter(|h| h.is_held())
                        {
                            let h = Arc::clone(h);
                            crate::rt::spawn(async move {
                                crate::rt::sleep(TAKE_OVER_GRACE).await;
                                if h.is_held() {
                                    tracing::warn!("no word of a promotion after taking over: this runner takes messages again");
                                    h.hold(false);
                                }
                            });
                        }
                    }
                    Bg::Promoted => {
                        // Joined ranked oldest (new work stayed with the old runner): the newest now.
                        tracing::info!("promoted: this runner is the machine's newest; new work comes here");
                        // Its sessions are its own for good: it takes their messages.
                        if let Some(h) = &harness {
                            h.hold(false);
                        }
                        instance_hello.created_at = real_created_at;
                        draining = false;
                        uplink.msg(AgentMsg::Instance(instance_hello.clone())).await;
                    }
                    // Turned into Ev::Forwarded as they're received.
                    Bg::Forwarded(_) | Bg::Again(..) => {}
                }
            }
            Ev::Redial => {
                redial = None;
                match paths.target(&http, Instant::now()).await {
                    Ok(addr) => {
                        dialing_since = Some(Instant::now());
                        net.command(Out::Connect { addr }).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "cannot resolve server");
                        redial = Some(Instant::now() + crate::sup::jitter(dial_backoff));
                        dial_backoff = (dial_backoff * 2).min(Duration::from_secs(30));
                    }
                }
            }
            Ev::Watchdog => {
                if !o.ephemeral && standalone && crate::os::IN_PLACE_UPGRADE && started.elapsed() > Duration::from_secs(5) {
                    let now = exe.as_deref().and_then(binary_mtime);
                    if !upgrade_pending && now.is_some() && now != exe_mtime {
                        upgrade_pending = true;
                        tracing::info!(jobs = jobs.len(), terminals = terms.len(), "new kiln binary on disk; taking it up once running jobs finish");
                    }
                    // In place, agent sessions go on across it with their processes, mid-turn
                    // included (`Host::hand_over_all`). As a fresh process (macOS) they are stopped
                    // and continue (resumed) there, so those mid-turn get up to ten minutes to
                    // finish first. Reach runs (for other runners, or followed on them) get as long.
                    let reach_busy = reach.as_ref().map_or(0, crate::reach::node::Handle::busy);
                    let busy = if successor { harness.as_ref().map_or(0, |h| h.busy()) + reach_busy } else { reach_busy };
                    if upgrade_pending && busy > 0 {
                        harness_wait.get_or_insert_with(Instant::now);
                    }
                    let ready = busy == 0 || harness_wait.is_some_and(|t| t.elapsed() > Duration::from_secs(600));
                    if upgrade_pending && !relaunch_wanted && jobs.is_empty() && ready && let Some(exe) = exe.as_deref() {
                        if let Err(e) = binary_runs(exe).await {
                            // Never switch to a binary that cannot start: keep this one until
                            // the binary on disk changes again.
                            tracing::error!(error = %format!("{e:#}"), exe = %exe.display(), "the new kiln binary does not run; staying on this one");
                            upgrade_pending = false;
                            exe_mtime = now;
                        } else if !successor {
                            if let Some(h) = &harness {
                                let n = h.hand_over_all().await;
                                tracing::info!(sessions = n, "agent sessions handed to the new image, their processes still running");
                            }
                            let err = upgrade_in_place(&o, exe, &terms);
                            // Only reached when the exec failed: carry on with this binary, and
                            // with the agent sessions just handed over.
                            tracing::error!(error = %err, "in-place upgrade failed; continuing on the running binary");
                            if let Some(h) = &harness {
                                h.take_back();
                            }
                            upgrade_pending = false;
                            exe_mtime = now;
                        } else if channel {
                            match write_handover(&o, &terms) {
                                Ok(masters) => {
                                    if let Some(h) = &harness {
                                        h.stop_all();
                                    }
                                    match crate::os::supervise::to_supervisor(&masters) {
                                        Ok(()) => {
                                            tracing::info!(terminals = masters.len(), "handed over to the supervisor, which starts the new binary as a fresh process");
                                            HANDED_OVER.store(true, std::sync::atomic::Ordering::SeqCst);
                                            // The supervisor holds the terminals now: ending
                                            // their sessions here would hang their shells up.
                                            std::mem::forget(std::mem::take(&mut terms));
                                            return Ok(());
                                        }
                                        Err(e) => {
                                            let _ = std::fs::remove_file(handover_path(&o));
                                            tracing::error!(error = %e, "could not hand over to the supervisor; continuing on the running binary");
                                        }
                                    }
                                }
                                Err(e) => tracing::error!(error = %format!("{e:#}"), "could not write the terminal handover; continuing on the running binary"),
                            }
                            upgrade_pending = false;
                            exe_mtime = now;
                        } else if supervised {
                            relaunch_wanted = true;
                        } else {
                            // `kiln agent` run by hand: re-executing would cut it off from the
                            // local network, and nothing would start it afresh.
                            tracing::warn!("a new kiln binary is on disk; restart this runner to run it");
                            upgrade_pending = false;
                            exe_mtime = now;
                        }
                    }
                    if relaunch_wanted && relaunch_asked.is_none() && jobs.is_empty() && ready {
                        relaunch_asked = Some(Instant::now());
                        // Nothing new starts here meanwhile; launchd stops this process shortly.
                        upgrade_pending = true;
                        if let Some(h) = &harness {
                            h.stop_all();
                        }
                        let state_dir = o.state_dir.clone();
                        let instance = o.instance.clone();
                        match crate::rt::blocking(move || crate::install::relaunch_runner(&state_dir, instance.as_deref())).await {
                            Ok(Ok(true)) => tracing::warn!("asked launchd to start this runner afresh from its launcher; it stops in a moment"),
                            Ok(Ok(false)) => {
                                tracing::warn!("no launchd job runs this runner; restart it yourself to take up the new kiln");
                                relaunch_wanted = false;
                            }
                            Ok(Err(e)) | Err(e) => {
                                tracing::error!(error = %format!("{e:#}"), "could not have launchd start this runner afresh; restart it yourself");
                                relaunch_wanted = false;
                            }
                        }
                        if !relaunch_wanted {
                            upgrade_pending = false;
                            exe_mtime = now;
                        }
                    }
                    // launchd should have stopped this process long since: take jobs again.
                    if relaunch_asked.is_some_and(|t| t.elapsed() > Duration::from_secs(600)) {
                        tracing::error!("launchd never started this runner afresh; carrying on with this binary (restart it yourself)");
                        relaunch_asked = None;
                        relaunch_wanted = false;
                        upgrade_pending = false;
                        exe_mtime = now;
                    }
                }
                // Terminals into legs close once their leg ends, or is let go when kept (W17-AC01).
                let gone: Vec<String> = terms.iter().filter(|(_, (_, st))| st.target.strip_prefix("leg:").is_some_and(|job| !crate::runner::kept::live(job))).map(|(id, _)| id.clone()).collect();
                for session in gone {
                    terms.remove(&session);
                    uplink.0.send(Up::Term(AgentMsg::TermExit { session, code: None, error: Some("the leg ended".into()) })).await.ok();
                }
                // A dial that never resolves (no Connected / Closed / Unreachable) would leave the
                // agent waiting forever, e.g. after dialing before the network existed at boot.
                // Treat it as failed: rebuild the endpoint on a fresh socket and dial again.
                if !connected && dialing_since.is_some_and(|t| t.elapsed() > DIAL_TIMEOUT) {
                    tracing::warn!(server = %quic_addr, after = ?DIAL_TIMEOUT, "no answer from server; rebuilding transport and redialing");
                    dialing_since = None;
                    net.command(Out::Rebuild).await;
                    let now = Instant::now();
                    redial = Some(if paths.dial_failed(now) { now } else { now + crate::sup::jitter(dial_backoff) });
                    dial_backoff = (dial_backoff * 2).min(Duration::from_secs(30));
                }
                // Direct dials unanswered for too long: through the tunnel now. Tunnelled and UDP
                // answers again: drop this connection (what is sent meanwhile waits in the buffer)
                // and dial directly; the tunnel stays open until that connection is up.
                if paths.tick(connected, Instant::now()) {
                    connected = false;
                    welcomed = false;
                    if let Some(s) = &sync {
                        s.connected(false);
                    }
                    if let Some(r) = &reach {
                        r.feed(crate::reach::node::In::Server(false)).await;
                    }
                    dialing_since = None;
                    redial = Some(Instant::now());
                }
            }
            Ev::Net(None) => anyhow::bail!("transport stopped"),
            Ev::Net(Some(NetEvent::Connected { binding, .. })) => {
                tracing::info!(server = %quic_addr, via = %paths.via(), "connected to server");
                chaos_drop = chaos.and_then(|c| c.drop_every).map(|d| Instant::now() + d);
                paths.connected(binding, Instant::now());
                dialing_since = None;
                dial_backoff = Duration::from_millis(500);
                // Which process of this machine this is, then Hello (an older server skips the first).
                if !o.ephemeral
                    && let Ok(b) = proto::encode(&AgentMsg::Instance(instance_hello.clone()))
                {
                    net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                }
                // Jobs whose results the server hasn't said it has are its own still: they come
                // again below. A server that never says (one older than receipts) has them for a
                // while only.
                unreceived.retain(|_, (at, _)| at.elapsed() < UNRECEIVED_FOR);
                let mut running: Vec<String> = jobs.keys().cloned().collect();
                running.extend(unreceived.keys().filter(|j| !jobs.contains_key(*j)).cloned());
                let msg = AgentMsg::Hello(hello(running));
                if let Ok(b) = proto::encode(&msg) {
                    net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                }
                // Which build this is and that it takes upgrade commands (an older server skips it);
                // an outpost says it's one instead.
                let mut build = if o.outpost { crate::updates::outpost_build() } else { crate::updates::runner_build().await };
                // It sends its conversations' store: the server keeps them from that.
                if shipper.is_some()
                    && let AgentMsg::Build { caps, .. } = &mut build
                {
                    *caps |= proto::CAP_CONVO;
                }
                if !o.ephemeral
                    && let Ok(b) = proto::encode(&build)
                {
                    net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                }
                // What it can do past Build's 32 bits: it keeps the channel (docs/wire.md), and an
                // outpost aside, it says what it holds warm (docs/ci.md W24-R18); running agent
                // sessions, it takes their limit from the server (docs/agents.md). A test build's
                // `old` says nothing, as a runner from before the channel.
                if !o.ephemeral && !crate::runner::chaos::get().is_some_and(|c| c.old) {
                    let built = if let AgentMsg::Build { caps, .. } = &build { u64::from(*caps) } else { 0 };
                    let work = if o.outpost { 0 } else { proto::CAP_WORK };
                    let headroom_cap = if headroom.is_some() { proto::CAP_HEADROOM } else { 0 };
                    let limit = if harness.is_some() { proto::CAP_AGENT_LIMIT } else { 0 };
                    if let Ok(b) = proto::encode(&AgentMsg::Caps { caps: built | proto::CAP_WIRE | proto::CAP_HANDOVER | proto::CAP_INBOX | work | limit | headroom_cap }) {
                        net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                    }
                }
                // The kiln it runs, so the server knows its commit (`crate::fleet`; outposts aren't
                // part of the fleet).
                if !o.ephemeral
                    && !o.outpost
                    && let Ok(b) = proto::encode(&crate::updates::runner_facts())
                {
                    net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                    // The rest, probed: they take a moment (docs/ci.md W13-R01).
                    probe_facts(&uplink, &o.work_dir, &o.declared, &probing);
                }
                // The pools' executors were the last connection's: the server starts what it wants
                // again (docs/ci.md W13-R09).
                for w in pool_execs.drain() {
                    crate::rt::spawn(crate::runner::pool::discard(w));
                }
                #[cfg(target_os = "linux")]
                crate::rt::spawn(crate::runner::firecracker::drain_warm());
                // Whether this machine takes upgrades (an older server skips it).
                if !o.ephemeral && !o.outpost {
                    let accept = takes_updates(&o);
                    if let Ok(b) = proto::encode(&AgentMsg::UpdatePrefs { accept }) {
                        net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                    }
                    accept_said = Some(accept);
                }
                // Its reach identity, after Hello (an older server skips it).
                if let Some(r) = &reach
                    && let Ok(b) = proto::encode(&r.hello())
                {
                    net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                }
                connected = true;
                // What the server's copies have is its to say again on this connection.
                server_kept.clear();
                if let Some(s) = &sync {
                    s.connected(true);
                }
                if let Some(f) = &shipper {
                    f.link(crate::convo::feed::Link::Up);
                }
                // How the front door does, after everything older (an older server skips it).
                front_door_sent = None;
                if let Some(json) = front_door.as_ref().and_then(crate::frontdoor::runner::Handle::latest)
                    && let Ok(b) = proto::encode(&AgentMsg::FrontDoorStatus { json: json.clone() })
                {
                    net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                    front_door_sent = Some(json);
                }
            }
            Ev::Net(Some(NetEvent::Closed { reason, .. } | NetEvent::Unreachable { reason, .. })) => {
                let now = Instant::now();
                let mut at_once = false;
                if connected {
                    tracing::warn!(%reason, "disconnected from server");
                    paths.disconnected();
                    // No word comes for agent sessions on their way to a newer process: they go on here.
                    crate::harness::handing::runner_link_lost();
                } else {
                    tracing::info!(%reason, via = %paths.via(), retry_in = ?dial_backoff, "dial failed");
                    // The path changed (to the tunnel, or back): dial that way now.
                    at_once = paths.dial_failed(now);
                }
                connected = false;
                welcomed = false;
                // The channel's items in flight go again once the server says what it has.
                updates.down();
                server_wire = false;
                server_drops = false;
                drops_said = None;
                server_work = false;
                work_said = None;
                server_limit = false;
                limit_said = None;
                if let Some(s) = &sync {
                    s.connected(false);
                }
                if let Some(f) = &shipper {
                    f.link(crate::convo::feed::Link::Down);
                }
                // A copy being sent back stops: the server asks again if it still needs it.
                restoring = None;
                if let Some(r) = &reach {
                    r.feed(crate::reach::node::In::Server(false)).await;
                }
                dialing_since = None;
                redial = Some(if at_once { now } else { now + crate::sup::jitter(dial_backoff) });
                if let Some(until) = chaos_down.take() {
                    redial = redial.map(|r| r.max(until));
                }
                dial_backoff = (dial_backoff * 2).min(Duration::from_secs(30));
            }
            Ev::Net(Some(NetEvent::Frame { data, .. })) | Ev::Forwarded(data) => {
                let msg: ServerMsg = match proto::decode(&data) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(error = %e, "bad frame from server");
                        continue;
                    }
                };
                // A command on the channel (docs/wire.md, "Commands"): taken once, however often
                // it comes, then applied as if it had come alone, and answered at the top of the
                // next turn of the loop.
                let (msg, data) = 'item: {
                    match msg {
                        ServerMsg::Wire(crate::wire::channel::Frame::Item { stream, seq, low, body }) => {
                            use crate::wire::channel::{Arrival, Refusal};
                            // A command stream's newer generation: the ones before it are done with,
                            // and an item of one of those that comes late isn't taken again.
                            match wire_current.of(&stream) {
                                crate::wire::commands::Stand::Gone => continue 'events,
                                crate::wire::commands::Stand::Newer(prefix) => wire_in.forget_where(|s| s != stream && crate::wire::commands::prefix_of(s) == Some(prefix)),
                                crate::wire::commands::Stand::Current => {}
                            }
                            let answer = match wire_in.on_item(&stream, seq, low, body) {
                                Arrival::Take(body) => match proto::decode::<ServerMsg>(&body) {
                                    Ok(ServerMsg::Wire(_)) => wire_in.refused(&stream, seq, Refusal::Malformed, "a channel frame inside a command"),
                                    Ok(inner) => {
                                        answer_after = Some((stream, seq, None));
                                        break 'item (inner, body);
                                    }
                                    Err(e) => wire_in.refused(&stream, seq, Refusal::Malformed, e.to_string()),
                                },
                                Arrival::Answered(Some(a)) | Arrival::Gap(a) => a,
                                Arrival::Answered(None) => continue 'events,
                            };
                            uplink.0.send(Up::Term(AgentMsg::Wire(answer))).await.ok();
                            continue 'events;
                        }
                        m => (m, data),
                    }
                };
                // About something handed to the new build (blue/green), or new while this runner
                // hands everything over or retires: it goes there.
                #[cfg(unix)]
                let opens = matches!(msg, ServerMsg::TermOpen { .. } | ServerMsg::HarnessOpen { .. }) && (retiring || moving.as_ref().is_some_and(|m| m.handing));
                #[cfg(unix)]
                if let Some(what) = crate::instances::Owned::of(&msg).filter(|w| moved.contains(w) || opens) {
                    moved.insert(what.clone());
                    match (&forward, moving.as_mut()) {
                        (Some(f), _) => {
                            let _ = f.try_send(data);
                        }
                        (None, Some(mv)) => mv.held.push(data),
                        (None, None) => tracing::debug!(?what, "a frame about something handed over, with nowhere to go"),
                    }
                    continue;
                }
                match msg {
                    // A test build's `old` skips both, as a runner from before the channel can't
                    // read them.
                    ServerMsg::Kept { .. } | ServerMsg::Caps { .. } if crate::runner::chaos::get().is_some_and(|c| c.old) => {}
                    // What the server can do past Build's 32 bits (docs/wire.md): said before its
                    // Welcome, so this runner sends its sessions over the channel, not whole. It
                    // says what it has of the server's streams (none yet).
                    ServerMsg::Kept { sessions } => {
                        server_kept.extend(sessions.into_iter().map(|k| (k.session.clone(), k)));
                    }
                    ServerMsg::Caps { caps } => {
                        server_wire = caps & proto::CAP_WIRE != 0;
                        server_drops = caps & proto::CAP_NET_DROPS != 0;
                        server_work = caps & proto::CAP_WORK != 0;
                        server_limit = caps & proto::CAP_AGENT_LIMIT != 0;
                        server_headroom = caps & proto::CAP_HEADROOM != 0;
                        // How headroom does, now that this server takes it.
                        headroom_sent = None;
                        if server_headroom
                            && let Some(json) = headroom.as_ref().and_then(crate::headroom::Handle::latest)
                        {
                            uplink.0.send(Up::Term(AgentMsg::HeadroomStatus { json: json.clone() })).await.ok();
                            headroom_sent = Some(json);
                        }
                        if server_wire {
                            uplink.0.send(Up::Term(AgentMsg::Wire(wire_in.have()))).await.ok();
                        }
                    }
                    // Taken above, before this match: what it carries came here instead.
                    ServerMsg::Wire(crate::wire::channel::Frame::Item { .. }) => {}
                    // What the server has of this runner's streams, its answers and wants: what it
                    // lacks goes; a stream it lost starts over, its snapshot first.
                    ServerMsg::Wire(frame) => {
                        updates.on_frame(frame);
                        for key in updates.take_stale() {
                            if let Some(id) = key.strip_prefix("s:") {
                                if let Some(s) = harness.as_ref().and_then(|h| h.get(id)) {
                                    start_stream(&mut updates, id, &s, server_kept.get(id));
                                }
                            } else if let Some(id) = key.strip_prefix("t:")
                                && let Some((_, state)) = terms.get(id)
                            {
                                // A terminal's stream starts over with its picture now.
                                let a = state.attach();
                                state.detach();
                                updates.start(&key);
                                if let Ok(b) = proto::encode(&AgentMsg::TermReplay { session: id.to_owned(), data: a.replay }) {
                                    updates.push(&key, b);
                                }
                            }
                        }
                        flush_updates(&mut updates, &uplink).await;
                    }
                    ServerMsg::Welcome { server_version, sample_seconds } => {
                        tracing::info!(%server_version, "server welcomed agent");
                        // What waited for the connection, in order: what was held while it was
                        // down, the jobs' events again, then their results not yet received.
                        welcomed = true;
                        while let Some((lane, kind, data)) = buffer.pop_front() {
                            buffered = buffered.saturating_sub(data.len());
                            net.send(SERVER_PEER, lane, kind, data).await;
                        }
                        if receipts {
                            for (_, _, events) in journal.values() {
                                for data in events {
                                    net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, data.clone()).await;
                                }
                            }
                        }
                        for (_, data) in unreceived.values() {
                            net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, data.clone()).await;
                        }
                        // Drained or not is the server's to say again on each connection (it sends
                        // Drain next when this one isn't the machine's newest).
                        if !o.ephemeral && !retiring {
                            draining = false;
                        }
                        if o.instance.is_some() {
                            // `kiln install --agent --upgrade` waits for this.
                            let _ = std::fs::write(own_dir.join("connected"), crate::util::now_unix().to_string());
                        }
                        let sessions: Vec<proto::TermInfo> = terms
                            .iter()
                            .map(|(id, (_, s))| {
                                let i = s.info();
                                proto::TermInfo { session: id.clone(), cols: i.cols, rows: i.rows, command: s.command.clone() }
                            })
                            .collect();
                        uplink.0.send(Up::Term(AgentMsg::TermList { sessions })).await.ok();
                        if let Some(ai) = &ai {
                            ai.reset();
                        }
                        // Its battery is said again too (a state: the newest, sent on every connect).
                        power_said = None;
                        power_read_at = None;
                        // And what its agent sessions are held to, to a server that takes it.
                        limit_said = None;
                        if let Some(h) = &harness {
                            hold_agent_limit(h, crate::harness::limit::held(limit_asked, limit_config), &mut limit_said, server_limit, &uplink).await;
                        }
                        // The server learns (again) which agent sessions run here, then what it lacks
                        // of each: over the channel only that (a session's stream not started yet
                        // starts with its snapshot), else all of each.
                        if let Some(h) = &harness {
                            if server_wire {
                                updates_on = true;
                                // Updates queued meanwhile go on their streams; those of sessions
                                // whose stream starts now are in its snapshot.
                                while let Ok((session, u)) = harness_rx.try_recv() {
                                    note_announced(&mut announced, &session, &u);
                                    if updates.started(&crate::wire::uplink::session_key(&session)) {
                                        push_update(&mut updates, &session, &u);
                                    }
                                }
                            } else {
                                updates_on = false;
                                updates = crate::wire::uplink::Updates::new(format!("{:016x}", rand::random::<u64>()));
                                while harness_rx.try_recv().is_ok() {}
                            }
                            if let Ok(json) = serde_json::to_string(&h.inventory()) {
                                harness_kinds_sent = serde_json::to_string(&crate::harness::kinds::offered()).ok();
                                uplink.0.send(Up::Term(AgentMsg::HarnessInventory { json })).await.ok();
                            }
                            for info in h.list() {
                                let Some(s) = h.get(&info.id) else { continue };
                                if updates_on {
                                    if !updates.started(&crate::wire::uplink::session_key(&info.id)) && announced.contains(&info.id) {
                                        start_stream(&mut updates, &info.id, &s, server_kept.get(&info.id));
                                    }
                                    continue;
                                }
                                for u in s.snapshot() {
                                    for json in harness_frames(&info.id, &u) {
                                        uplink.0.send(Up::Term(AgentMsg::Harness { session: info.id.clone(), json })).await.ok();
                                    }
                                }
                            }
                            flush_updates(&mut updates, &uplink).await;
                        }
                        sample_tick = Interval::new(Duration::from_secs(u64::from(sample_seconds.max(1))));
                        // A new build started to take over from the runner being upgraded (blue/green):
                        // connected and welcomed, it says it's ready, and takes everything.
                        #[cfg(unix)]
                        if let Some(path) = take_over.take() {
                            let (h, tx, me) = (harness.clone(), bg_tx.clone(), instance_id.clone());
                            crate::rt::spawn(async move {
                                if let Err(e) = take_over_from(path, h, tx.clone(), me).await {
                                    tracing::warn!(error = %format!("{e:#}"), "couldn't take over from the runner being upgraded");
                                }
                                let _ = tx.send(Bg::TakeOverEnded).await;
                            });
                        }
                        // The server knows this runner now: reach may relay (its links resume).
                        if let Some(r) = &reach {
                            r.feed(crate::reach::node::In::Server(true)).await;
                        }
                    }
                    ServerMsg::Refused { reason } => {
                        tracing::error!(%reason, "server refused this agent");
                        if o.ephemeral {
                            anyhow::bail!("refused: {reason}");
                        }
                        connected = false;
                        welcomed = false;
                        if let Some(s) = &sync {
                            s.connected(false);
                        }
                        if let Some(r) = &reach {
                            r.feed(crate::reach::node::In::Server(false)).await;
                        }
                        net.command(Out::Disconnect { peer: SERVER_PEER }).await;
                        redial = Some(Instant::now() + Duration::from_secs(60));
                    }
                    ServerMsg::Assign(mut a) => {
                        // The URL this agent reached the server through is the one that works
                        // from here (NAT, NodePorts, cluster DNS all differ).
                        a.server_url = o.server_url.clone();
                        let id = a.job_id.clone();
                        let holds = crate::hub::v1_incoming(a.spec.exclusive, &a.spec.resources);
                        let room = admits(o.slots, size, jobs.values().map(|j| (j.pooled, j.holds)), false, holds);
                        if o.outpost || draining || upgrade_pending || room.is_err() || jobs.contains_key(&id) {
                            let reason = if o.outpost { OUTPOST_TAKES_NO_JOBS } else if draining { "draining" } else if upgrade_pending { "upgrading" } else { room.err().unwrap_or("no free slot") };
                            uplink.msg(AgentMsg::Reject { job: id, reason: reason.into() }).await;
                            continue;
                        }
                        if o.dedicated_job.as_deref() == Some(id.as_str()) {
                            served_dedicated = true;
                        }
                        let cancel = Shutdown::new();
                        hold_cpus(&id, holds);
                        jobs.insert(id.clone(), Running { cancel: cancel.clone(), pooled: false, holds });
                        tracing::info!(job = %id, key = %a.spec.key, "running job");
                        let exec = Exec {
                            assign: *a,
                            work_dir: o.work_dir.clone(),
                            agent_name: name.clone(),
                            up: uplink.clone(),
                            cancel,
                            http: http.clone(),
                        };
                        let done = done_tx.clone();
                        crate::rt::spawn(async move {
                            let result = match std::panic::AssertUnwindSafe(exec.run()).catch_unwind().await {
                                Ok(r) => r,
                                Err(p) => JobResult {
                                    job: id,
                                    status: Status::Failed,
                                    exit_code: None,
                                    error: Some(format!("executor panicked: {}", crate::sup::panic_message(p.as_ref()))),
                                    peak_rss: None,
                                    cpu_seconds: None,
                                },
                            };
                            let _ = done.send(result).await;
                        });
                    }
                    ServerMsg::Cancel { job, reason } => {
                        if let Some(r) = jobs.get(&job) {
                            tracing::info!(%job, %reason, "cancelling job");
                            r.cancel.cancel();
                            // A cancelled job that hasn't ended within the grace never will by
                            // itself (a process that left its group holds its output, a wait
                            // missed its wake-up): it is reported over, so it can't hold the
                            // machine (an in-place upgrade waits for no job to finish).
                            let (tx, j) = (done_tx.clone(), job.clone());
                            crate::rt::spawn(async move {
                                crate::rt::sleep(CANCEL_GRACE).await;
                                let error = format!("cancelled; it hadn't ended {}s later, so the runner let it go", CANCEL_GRACE.as_secs());
                                let _ = tx.send(JobResult { job: j, status: Status::Cancelled, exit_code: None, error: Some(error), peak_rss: None, cpu_seconds: None }).await;
                            });
                        } else {
                            refuse(&mut answer_after, crate::wire::channel::Refusal::Unknown, format!("no job {job} runs here"));
                        }
                    }
                    ServerMsg::Drain => draining = true,
                    ServerMsg::Retire { instance } => {
                        // An older runner process of this machine holds nothing any more: stop
                        // the service this instance replaces (it checks for terminals itself).
                        let state_dir = o.state_dir.clone();
                        let me = instance_id.clone();
                        let older = instance.clone();
                        crate::rt::spawn(async move {
                            match crate::rt::blocking(move || crate::upgrade::retire(&state_dir, &me, &older)).await {
                                Ok(Ok(crate::upgrade::Retired::Stopped(unit))) => {
                                    tracing::info!(%instance, %unit, "retired the older runner process: its service is stopped and disabled");
                                }
                                Ok(Ok(crate::upgrade::Retired::Busy(n))) => {
                                    tracing::warn!(%instance, held = n, "the older runner process still holds terminals or agent sessions; leaving it running");
                                }
                                Ok(Ok(crate::upgrade::Retired::Nothing)) => {
                                    tracing::info!(%instance, "an older runner process of this machine can be stopped, but not from here (it isn't running here, or it shares this runner's service)");
                                }
                                Ok(Err(e)) | Err(e) => tracing::warn!(%instance, error = %format!("{e:#}"), "could not retire the older runner process"),
                            }
                        });
                    }
                    ServerMsg::TermOpen { session, cols, rows, command } => {
                        if !o.terminal {
                            uplink.0.send(Up::Term(AgentMsg::TermExit { session, code: None, error: Some("terminal disabled on this agent".into()) })).await.ok();
                            continue;
                        }
                        if terms.contains_key(&session) {
                            continue;
                        }
                        // `kiln spawn` in this terminal reaches this agent's sessions.
                        let mut env = vec![("KILN_TERM_SESSION", session.clone())];
                        if let Some(sock) = crate::reach::local::socket() {
                            env.push(("KILN_REACH_SOCK", sock.to_owned()));
                        }
                        if let Some(sock) = &harness_sock {
                            env.push(("KILN_HARNESS_SOCK", sock.clone()));
                        }
                        match terminal::spawn_with(o.shell.as_deref(), command.as_deref(), cols, rows, &env) {
                            Ok((pty, rx)) => {
                                let tx = term_tx.clone();
                                let s = session.clone();
                                crate::rt::spawn(async move {
                                    while let Ok(out) = rx.recv().await {
                                        let exit = matches!(out, PtyOut::Exit(_));
                                        if tx.send((s.clone(), out)).await.is_err() || exit {
                                            break;
                                        }
                                    }
                                });
                                let state = crate::termsess::Session::detached(session.clone(), "agent".into(), command.clone(), cols, rows);
                                terms.insert(session, (pty, state));
                            }
                            Err(e) => {
                                uplink.0.send(Up::Term(AgentMsg::TermExit { session, code: None, error: Some(format!("{e:#}")) })).await.ok();
                            }
                        }
                    }
                    ServerMsg::TermInput { session, data } => {
                        #[cfg(target_os = "linux")]
                        if crate::runner::firecracker::in_microvm(&session) {
                            crate::runner::firecracker::to_session(&session, crate::runner::guest::ToGuest::TermInput { session: session.clone(), data });
                            continue;
                        }
                        if let Some((pty, _)) = terms.get(&session) {
                            pty.write(data);
                        } else if let Some(typed) = shell_waits.get_mut(&session) {
                            typed.push(data);
                        }
                    }
                    ServerMsg::TermResize { session, cols, rows } => {
                        #[cfg(target_os = "linux")]
                        if crate::runner::firecracker::to_session(&session, crate::runner::guest::ToGuest::TermResize { session: session.clone(), cols, rows }) {
                            continue;
                        }
                        if let Some((pty, state)) = terms.get(&session) {
                            pty.resize(cols, rows);
                            state.send(crate::termsess::Input::Resize { cols, rows });
                        }
                    }
                    ServerMsg::TermAttach { session } => {
                        #[cfg(target_os = "linux")]
                        if crate::runner::firecracker::to_session(&session, crate::runner::guest::ToGuest::TermAttach { session: session.clone() }) {
                            continue;
                        }
                        let msg = match terms.get(&session) {
                            Some((_, state)) => {
                                let a = state.attach();
                                state.detach();
                                AgentMsg::TermReplay { session, data: a.replay }
                            }
                            None => AgentMsg::TermExit { session, code: None, error: Some("session no longer exists".into()) },
                        };
                        uplink.0.send(Up::Term(msg)).await.ok();
                    }
                    ServerMsg::TermClose { session } => {
                        #[cfg(target_os = "linux")]
                        if crate::runner::firecracker::to_session(&session, crate::runner::guest::ToGuest::TermClose { session: session.clone() }) {
                            continue;
                        }
                        shell_waits.remove(&session);
                        terms.remove(&session);
                    }
                    ServerMsg::AiTranscript { req, pid } => match &ai {
                        Some(ai) => ai.transcript(req, pid, uplink.0.clone()),
                        None => {
                            uplink.0.send(Up::Term(AgentMsg::AiTranscript { req, json: None })).await.ok();
                        }
                    },
                    ServerMsg::HarnessOpen { session, json } => {
                        use crate::wire::channel::Refusal;
                        // How it fared, as its answer says it (a command on the channel).
                        let opened: Result<(), (Refusal, anyhow::Error)> = match (&harness, serde_json::from_str::<crate::harness::session::Spec>(&json)) {
                            (None, _) => Err((Refusal::Unsupported, anyhow::anyhow!("agent sessions are not enabled on this runner"))),
                            (_, Err(e)) => Err((Refusal::Malformed, anyhow::anyhow!("bad session spec: {e}"))),
                            (Some(h), Ok(mut spec)) => {
                                // Started on a shell: in that shell's directory and environment.
                                if let Some(pid) = spec.shell.as_ref().and_then(|t| terms.get(t)).and_then(|(pty, _)| pty.pid()) {
                                    let (cwd, env) = crate::harness::host::shell_context(pid);
                                    if spec.cwd.is_none() {
                                        spec.cwd = cwd;
                                    }
                                    if spec.env.is_empty() {
                                        spec.env = env;
                                    }
                                }
                                if crate::workspace::needs_preparing(&spec) {
                                    // Its workspace first (a volume made and mounted), off this loop: then it starts there.
                                    let (h, up, session, json) = (Arc::clone(h), uplink.0.clone(), session.clone(), json.clone());
                                    crate::rt::spawn(async move {
                                        let opened = match crate::workspace::prepare(&mut spec, &session).await {
                                            Ok(()) => h.open(Some(session.clone()), spec).map(|_| ()),
                                            Err(e) => Err(e.context("its workspace")),
                                        };
                                        if let Err(e) = opened {
                                            for u in crate::harness::host::refusal(&session, &json, &format!("{e:#}")) {
                                                if let Ok(json) = serde_json::to_string(&u) {
                                                    up.send(Up::Term(AgentMsg::Harness { session: session.clone(), json })).await.ok();
                                                }
                                            }
                                        }
                                    });
                                    Ok(())
                                } else {
                                    h.open(Some(session.clone()), spec).map(|_| ()).map_err(|e| (Refusal::Refused, e))
                                }
                            }
                        };
                        if let Err((code, e)) = opened {
                            refuse(&mut answer_after, code, format!("{e:#}"));
                            for u in crate::harness::host::refusal(&session, &json, &format!("{e:#}")) {
                                if let Ok(json) = serde_json::to_string(&u) {
                                    uplink.0.send(Up::Term(AgentMsg::Harness { session: session.clone(), json })).await.ok();
                                }
                            }
                        }
                    }
                    ServerMsg::HarnessCmd { session, json } => {
                        use crate::wire::channel::Refusal;
                        let known = harness.as_ref().is_some_and(|h| h.get(&session).is_some());
                        match serde_json::from_str::<crate::harness::model::Command>(&json) {
                            // The server's word on a hand-over to this machine's newer runner
                            // process: never "gone here", even once this process has let go.
                            Ok(crate::harness::model::Command::HandedOver { error, unsure, by_newest, none_newer }) => {
                                // With no newer process to take it, a word that it wasn't taken
                                // settles as a refusal does (the machine's record says).
                                if let Some(h) = &harness {
                                    h.handed_over(&session, error, unsure, by_newest || none_newer);
                                }
                            }
                            Ok(c) if known => {
                                if let Some(h) = &harness
                                    && !h.command(&session, c)
                                {
                                    refuse(&mut answer_after, Refusal::Refused, format!("session {session} didn't take it: its lane is full, or it just ended"));
                                }
                            }
                            Ok(_) => {
                                refuse(&mut answer_after, Refusal::Unknown, format!("no session {session} here"));
                                // Gone here (e.g. stopped while the server was away): say so.
                                if let Ok(json) = serde_json::to_string(&crate::harness::core::Update::Closed) {
                                    uplink.0.send(Up::Term(AgentMsg::Harness { session, json })).await.ok();
                                }
                            }
                            Err(e) => {
                                tracing::debug!(error = %e, "unreadable agent session command");
                                refuse(&mut answer_after, Refusal::Malformed, format!("{e}"));
                            }
                        }
                    }
                    // A message for an epoch (CP-T3.2): through its session's inbox, which answers.
                    ServerMsg::HarnessSend { session, epoch, json } => {
                        let known = harness.as_ref().is_some_and(|h| h.get(&session).is_some());
                        match serde_json::from_str::<crate::harness::model::Command>(&json) {
                            Ok(c) if known => {
                                if let Some(h) = &harness {
                                    h.command_at(&session, c, Some(epoch));
                                }
                            }
                            Ok(c) => {
                                // Gone here: the message is refused, and the session said gone.
                                let mut said = Vec::new();
                                if let Some(message) = crate::harness::registry::message_id(&c) {
                                    let refused = crate::harness::inbox::Said { message, answer: Err(crate::harness::inbox::Refusal::NoSession { session: session.clone() }) };
                                    said.push(crate::harness::core::Update::Inbox { said: Box::new(refused) });
                                }
                                said.push(crate::harness::core::Update::Closed);
                                for u in said {
                                    if let Ok(json) = serde_json::to_string(&u) {
                                        uplink.0.send(Up::Term(AgentMsg::Harness { session: session.clone(), json })).await.ok();
                                    }
                                }
                            }
                            Err(e) => tracing::debug!(error = %e, "unreadable agent session message"),
                        }
                    }
                    ServerMsg::AiView { req, pid } => match &ai {
                        Some(ai) => ai.view(req, pid, uplink.0.clone()),
                        None => {
                            uplink.0.send(Up::Term(AgentMsg::AiView { req, json: None })).await.ok();
                        }
                    },
                    ServerMsg::HarnessFile { session, file, offset, data, done } => {
                        if let Some(h) = &harness
                            && let Err(e) = h.receive(&session, &file, offset, &data, done)
                        {
                            tracing::warn!(%session, error = %format!("{e:#}"), "could not store an attachment");
                        }
                    }
                    ServerMsg::HarnessClose { session } => {
                        // One this runner doesn't have is gone here already: said so (`unknown`).
                        if !harness.as_ref().is_some_and(|h| h.close(&session)) {
                            refuse(&mut answer_after, crate::wire::channel::Refusal::Unknown, format!("no session {session} here"));
                        }
                        // What it had going live: its app, its streams.
                        crate::browser::apps::stop(&session);
                        let dir = o.state_dir.clone();
                        crate::rt::spawn(async move { crate::browser::stream::streams().close_session(&dir, &session).await });
                    }
                    ServerMsg::HarnessPackage { session, offset, data, done } => {
                        // A session going on here from the server's store (its machine is gone).
                        let Some(h) = harness.clone() else {
                            refuse(&mut answer_after, crate::wire::channel::Refusal::Unsupported, "agent sessions are not enabled on this runner");
                            continue;
                        };
                        // Handing everything to a new build of it, or retiring: an older process's
                        // hand-over isn't taken here (`Host::adopt_package`).
                        #[cfg(unix)]
                        let leaving = retiring || moving.as_ref().is_some_and(|m| m.handing);
                        #[cfg(not(unix))]
                        let leaving = false;
                        match h.receive_package(&session, offset, &data, done) {
                            Ok(Some(path)) => {
                                let up = uplink.0.clone();
                                crate::rt::spawn(async move {
                                    let h2 = h.clone();
                                    let (handover, taken) = crate::rt::blocking(move || h2.adopt_package(&path, leaving)).await.unwrap_or_else(|e| (false, Err(format!("{e:#}"))));
                                    // Handed over by an older process of this machine: that one
                                    // goes on with it unless this one took it, so it's told either
                                    // way, and nothing else is said here.
                                    if handover {
                                        if let Err(why) = &taken {
                                            tracing::warn!(%session, %why, "couldn't take over an agent session from this machine's older runner process");
                                        } else {
                                            tracing::info!(%session, "took over an agent session from this machine's older runner process");
                                        }
                                        crate::harness::handing::adopted(&session, taken.err()).await;
                                    } else if let Err(why) = taken {
                                        tracing::warn!(%session, %why, "an agent session couldn't go on here");
                                        let spec = serde_json::json!({ "kind": "claude" }).to_string();
                                        for u in crate::harness::host::refusal(&session, &spec, &format!("Couldn't go on here: {why}")) {
                                            if let Ok(json) = serde_json::to_string(&u) {
                                                up.send(Up::Term(AgentMsg::Harness { session: session.clone(), json })).await.ok();
                                            }
                                        }
                                    }
                                });
                            }
                            Ok(None) => {}
                            Err(e) => {
                                tracing::warn!(%session, error = %format!("{e:#}"), "a session package couldn't be kept");
                                refuse(&mut answer_after, crate::wire::channel::Refusal::Refused, format!("its package couldn't be kept: {e:#}"));
                            }
                        }
                    }
                    ServerMsg::HarnessRelease { session } => {
                        if let Some(h) = &harness {
                            h.let_go(&session);
                        }
                    }
                    ServerMsg::Upgrade { job, path, sha256, size } if o.outpost => {
                        // Nothing of an outpost is installed: its next kiln is the one downloaded next.
                        let _ = (path, sha256, size);
                        let error = Some("an outpost isn't installed, so the server doesn't upgrade it".to_owned());
                        uplink.msg(AgentMsg::UpgradeDone { job, ok: false, error }).await;
                    }
                    ServerMsg::Upgrade { job, path, sha256, size } if !takes_updates(&o) => {
                        let _ = (path, sha256, size);
                        tracing::info!(%job, "an upgrade refused: this machine's kiln config says [updates] accept = false");
                        let error = Some("this machine refuses upgrades: its kiln config says [updates] accept = false".to_owned());
                        uplink.msg(AgentMsg::UpgradeDone { job, ok: false, error }).await;
                    }
                    #[cfg(unix)]
                    ServerMsg::Upgrade { job, path, sha256, size } if moving.is_some() || retiring => {
                        let _ = (path, sha256, size);
                        let error = Some("this runner is already being upgraded".to_owned());
                        uplink.msg(AgentMsg::UpgradeDone { job, ok: false, error }).await;
                    }
                    #[cfg(unix)]
                    ServerMsg::Upgrade { job, path, sha256, size } if !o.ephemeral && crate::bluegreen::Supervisor::from_env().is_some() => {
                        // Blue/green: the new build is fetched beside this one and started next to
                        // it by the supervisor; this runner goes on until it's ready.
                        tracing::info!(%job, %path, "upgrading this runner: blue/green");
                        let up = Uplink(uplink.0.clone());
                        let (http, server_url, state, tx) = (Arc::clone(&http), o.server_url.clone(), o.state_dir.clone(), bg_tx.clone());
                        crate::rt::spawn(async move {
                            let order = crate::updates::Order { job: &job, path: &path, sha256: &sha256, size };
                            match crate::updates::fetch_build(&http, &server_url, &order, &state, &up).await {
                                Ok((exe, build)) => {
                                    let _ = tx.send(Bg::Fetched { job, exe, build }).await;
                                }
                                Err(e) => up.msg(AgentMsg::UpgradeDone { job, ok: false, error: Some(format!("{e:#}")) }).await,
                            }
                        });
                    }
                    ServerMsg::UpdateAvailable { commit, build, from } => {
                        if noticed.as_deref() != Some(commit.as_str()) {
                            let how = if takes_updates(&o) { "the server upgrades it as its policy there says" } else { "this machine refuses upgrades ([updates] accept = false)" };
                            tracing::info!(commit = %commit, build = %build, from = %from, "a newer kiln is available for this runner: {how}");
                            let note = serde_json::json!({ "commit": commit, "build": build, "from": from, "at": crate::util::now_unix() });
                            if let Err(e) = crate::util::write_secret(&own_dir.join("update-available.json"), note.to_string().as_bytes(), 0o644) {
                                tracing::debug!(error = %e, "couldn't note the newer kiln");
                            }
                            noticed = Some(commit);
                        }
                    }
                    ServerMsg::Upgrade { job, path, sha256, size } => {
                        // Fetch the new binary from the server and install it beside or over this
                        // runner (`install --agent --upgrade`); nothing running here is stopped.
                        tracing::info!(%job, %path, "upgrading this runner");
                        let up = Uplink(uplink.0.clone());
                        let http = Arc::clone(&http);
                        let server_url = o.server_url.clone();
                        let own = own_dir.clone();
                        let service = crate::updates::runner_service(&o.state_dir, o.instance.as_deref());
                        crate::rt::spawn(async move {
                            let order = crate::updates::Order { job: &job, path: &path, sha256: &sha256, size };
                            let res = crate::updates::run_on_runner(&http, &server_url, &order, &own, &service, &up).await;
                            if let Err(e) = &res {
                                tracing::warn!(%job, error = %format!("{e:#}"), "upgrade failed");
                            }
                            let error = res.err().map(|e| format!("{e:#}"));
                            up.msg(AgentMsg::UpgradeDone { job, ok: error.is_none(), error }).await;
                        });
                    }
                    ServerMsg::HarnessHistory { req, kind, limit } => match &ai {
                        Some(ai) => {
                            // Kiln's own sessions here are running, not past.
                            let running = harness.as_ref().map(|h| h.list().into_iter().filter_map(|i| i.harness_session).collect()).unwrap_or_default();
                            ai.history(req, kind, usize::try_from(limit).unwrap_or(usize::MAX), running, uplink.0.clone());
                        }
                        None => {
                            uplink.0.send(Up::Term(AgentMsg::HarnessHistory { req, json: None })).await.ok();
                        }
                    },
                    ServerMsg::HarnessTaskOutput { req, session, task, since } => {
                        let s = harness.as_ref().and_then(|h| h.get(&session));
                        let up = uplink.0.clone();
                        crate::rt::spawn(async move {
                            let json = match s {
                                Some(s) => crate::rt::blocking(move || s.task_output(&task, since).to_string()).await.ok(),
                                None => None,
                            };
                            let _ = up.send(Up::Term(AgentMsg::HarnessTaskOutput { req, json })).await;
                        });
                    }
                    // What's wrong with this machine, and repairs to it (`crate::doctor`): on the
                    // blocking pool, as they read and remove files.
                    ServerMsg::Diagnose { req } => {
                        let here = crate::doctor::Here { state_dir: o.state_dir.clone(), work_dir: o.work_dir.clone() };
                        let up = uplink.0.clone();
                        crate::rt::spawn(async move {
                            let json = crate::rt::blocking(move || serde_json::to_string(&crate::doctor::examine(&here)).ok()).await.ok().flatten();
                            let _ = up.send(Up::Term(AgentMsg::Answer { req, json })).await;
                        });
                    }
                    ServerMsg::Repair { req, action } => {
                        let here = crate::doctor::Here { state_dir: o.state_dir.clone(), work_dir: o.work_dir.clone() };
                        let up = uplink.0.clone();
                        crate::rt::spawn(async move {
                            tracing::info!(%action, "a repair the server asked for");
                            let json = crate::rt::blocking(move || crate::doctor::answer_repair(&here, &action)).await.ok();
                            let _ = up.send(Up::Term(AgentMsg::Answer { req, json })).await;
                        });
                    }
                    ServerMsg::Reach { from, link, open, data, close } => {
                        if let Some(r) = &reach {
                            r.feed(crate::reach::node::In::Frame { from, link, open, data, close }).await;
                        }
                    }
                    ServerMsg::ReachAnswer { req, json } => {
                        if let Some(r) = &reach {
                            r.feed(crate::reach::node::In::Answer { req, json }).await;
                        }
                    }
                    ServerMsg::Sync { json, raft } => {
                        if let Some(s) = &sync {
                            s.heard(&json, raft);
                        }
                    }
                    ServerMsg::Cluster { json, raft } => {
                        use crate::store::learner::Msg;
                        match crate::store::learner::decode(&json) {
                            Some(Msg::Join { member, token }) if !o.ephemeral && !o.outpost => {
                                // A replica being started goes on with the newest word to join.
                                *cluster_join.lock() = Some((member, token));
                                if cluster.lock().is_some() {
                                    continue;
                                }
                                replica.start();
                            }
                            Some(Msg::Leave) => {
                                // A replica being started stops trying.
                                *cluster_join.lock() = None;
                                // Another process of this machine holds it now: its files are let go
                                // (stopping it waits for its thread: not on this loop).
                                if let Some(old) = cluster.lock().take() {
                                    crate::store::learner::set_global(None);
                                    crate::rt::spawn(async move {
                                        let _ = crate::rt::blocking(move || drop(old)).await;
                                    });
                                }
                            }
                            Some(m @ (Msg::Raft { .. } | Msg::At { .. })) => {
                                let learner = cluster.lock().clone();
                                if let Some(l) = learner {
                                    l.heard(&m, raft);
                                }
                            }
                            _ => {}
                        }
                    }
                    ServerMsg::Convo { json, data } => {
                        let Some(m) = crate::convo::feed::decode(&json) else { continue };
                        if convo.as_ref().is_some_and(|st| st.id() == m.store()) {
                            // The server's answer to this runner's own feed.
                            if let Some(f) = &shipper {
                                f.link(crate::convo::feed::Link::Heard(m));
                            }
                            continue;
                        }
                        // The server lost its store: the copy of it this runner keeps goes back.
                        if let crate::convo::feed::Msg::Restore { .. } = &m {
                            if mirror.is_none()
                                && convo.is_some()
                                && own_dir.join(crate::convo::MIRROR_FILE).exists()
                                && let Some((st, intake)) = open_mirror(&own_dir, &uplink.0)
                            {
                                (mirror_store, mirror) = (Some(st), Some(intake));
                            }
                            if let Some(st) = &mirror_store {
                                let (ship, out) = crate::convo::feed::Shipper::start_restore(st.clone());
                                let up = uplink.0.clone();
                                crate::rt::spawn(async move {
                                    while let Ok((json, data)) = out.recv().await {
                                        if up.send(Up::Convo(AgentMsg::Convo { json, data })).await.is_err() {
                                            break;
                                        }
                                    }
                                });
                                ship.link(crate::convo::feed::Link::Up);
                                tracing::info!("the server lost its conversation store: sending back the copy this runner keeps");
                                restoring = Some((st.id().to_owned(), ship));
                            } else {
                                let none = crate::convo::feed::encode(&crate::convo::feed::Msg::Restored { store: String::new(), rev: 0 });
                                uplink.0.send(Up::Convo(AgentMsg::Convo { json: none, data: Vec::new() })).await.ok();
                            }
                            continue;
                        }
                        // The server's answers about the copy being sent back.
                        if let Some((id, ship)) = &restoring
                            && id == m.store()
                        {
                            ship.link(crate::convo::feed::Link::Heard(m));
                            continue;
                        }
                        // The server's store, of which this runner keeps a copy (a mirror), beside
                        // its own store; opened the first time the server sends it.
                        if mirror.is_none()
                            && convo.is_some()
                            && let Some((st, intake)) = open_mirror(&own_dir, &uplink.0)
                        {
                            (mirror_store, mirror) = (Some(st), Some(intake));
                        }
                        if let Some(intake) = &mirror {
                            intake.heard(SERVER_PEER, "server", json, data);
                        }
                    }
                    ServerMsg::HarnessFileRead { req, session: _, path, offset, len } => {
                        // A file one of its sessions' messages mentions, for whoever views it: a
                        // piece, read off this loop, sent on the files' own lane.
                        let up = uplink.0.clone();
                        crate::rt::spawn(async move {
                            let read = crate::rt::blocking(move || crate::harness::files::read(&path, offset, len)).await.unwrap_or_else(|e| Err(format!("{e:#}")));
                            let json = serde_json::to_string(&crate::harness::files::Answer::of(&read)).unwrap_or_default();
                            let data = read.map(|(_, d)| d).unwrap_or_default();
                            let _ = up.send(Up::File(AgentMsg::HarnessFileData { req, json, data })).await;
                        });
                    }
                    // Pages streamed for whoever views one of its sessions (`crate::browser::stream`).
                    ServerMsg::StreamOpen { stream, session, json } => {
                        crate::rt::spawn(crate::browser::stream::serve_open(o.state_dir.clone(), stream, session, json, uplink.0.clone()));
                    }
                    ServerMsg::StreamControl { stream, session: _, json } => crate::browser::stream::serve_control(&stream, &json),
                    ServerMsg::StreamInput { stream, session: _, json } => crate::browser::stream::serve_input(&stream, &json),
                    ServerMsg::StreamClose { stream, session: _ } => {
                        crate::rt::spawn(async move { crate::browser::stream::streams().close(&stream).await });
                    }
                    // A port of this machine's, for a person's own device (`crate::browser::ports`):
                    // one its session's machine lists, on loopback.
                    ServerMsg::PortOpen { conn, session, port } => {
                        let found = harness.as_ref().and_then(|h| h.get(&session)).map(|s| *s.pid.lock());
                        let up = uplink.0.clone();
                        crate::rt::spawn(async move {
                            let allowed = match found {
                                Some(root) => crate::rt::blocking(move || crate::browser::ports::allowed(&session, root, port)).await.unwrap_or(false),
                                None => false,
                            };
                            crate::browser::ports::serve_open(conn, port, allowed, up).await;
                        });
                    }
                    ServerMsg::PortData { conn, session: _, data } => crate::browser::ports::data(conn, data),
                    ServerMsg::PortAck { conn, session: _, bytes } => crate::browser::ports::ack(conn, bytes),
                    ServerMsg::PortClose { conn, session: _ } => crate::browser::ports::close(conn),
                    // What a person opens live from one of its sessions (`crate::browser::apps`).
                    ServerMsg::Preview { req, session, json } => {
                        let (up, dir) = (uplink.0.clone(), o.state_dir.clone());
                        let found = harness.as_ref().and_then(|h| h.get(&session)).map(|s| (std::path::PathBuf::from(s.info().cwd), *s.pid.lock()));
                        crate::rt::spawn(async move {
                            let answer = match found {
                                None => serde_json::json!({"error": "no such session here"}),
                                Some((cwd, pid)) => crate::browser::apps::serve(&dir, &session, &cwd, pid, &json).await,
                            };
                            let _ = up.send(Up::Term(AgentMsg::Answer { req, json: Some(answer.to_string()) })).await;
                        });
                    }
                    ServerMsg::Workspace { req, session, json } => {
                        // About one of this machine's agents' folders: the session says which, so
                        // a question can only be about a folder an agent here works in.
                        let up = uplink.0.clone();
                        let found = harness.as_ref().and_then(|h| h.get(&session)).map(|s| {
                            let ws = s.spec.lock().workspace.clone();
                            let info = s.info();
                            (std::path::PathBuf::from(&info.cwd), std::path::PathBuf::from(info.work_dir()), ws)
                        });
                        crate::rt::spawn(async move {
                            let answer = match found {
                                None => serde_json::json!({"error": "no such agent on this machine"}),
                                Some((cwd, work, ws)) => crate::workspace::serve(&cwd, &work, ws.as_ref(), &json).await,
                            };
                            let _ = up.send(Up::Term(AgentMsg::Answer { req, json: Some(answer.to_string()) })).await;
                        });
                    }
                    ServerMsg::DirList { req, path } => {
                        // The console's folder picker: this machine's folders, read off this loop.
                        let up = uplink.0.clone();
                        crate::rt::spawn(async move {
                            let json = crate::dirs::answer(path).await;
                            let _ = up.send(Up::Term(AgentMsg::DirList { req, json })).await;
                        });
                    }
                    ServerMsg::Headroom { json } => {
                        if let Some(h) = &headroom {
                            h.push(&json);
                        }
                    }
                    ServerMsg::FrontDoor { json } => {
                        // Settings (and certificates) are never logged: an upload carries a key.
                        if let Some(fd) = &front_door {
                            fd.push(json);
                        }
                    }
                    // What a person set from the server for how many agent sessions this machine
                    // runs at once (none: its config's holds): kept for its restarts, held from now
                    // on, and said back (docs/agents.md, "How many agents a machine runs").
                    ServerMsg::AgentLimit { json } => match serde_json::from_str::<crate::harness::limit::Asked>(&json) {
                        Ok(asked) => {
                            let dir = o.state_dir.clone();
                            if let Err(e) = crate::rt::blocking(move || crate::harness::limit::keep(&dir, asked)).await.and_then(|r| r.map_err(anyhow::Error::from)) {
                                tracing::warn!(error = %format!("{e:#}"), "what was set for this machine's agent limit couldn't be kept: started again, it holds its config's until the server says it again");
                            }
                            limit_asked = asked;
                            if let Some(h) = &harness {
                                hold_agent_limit(h, crate::harness::limit::held(limit_asked, limit_config), &mut limit_said, server_limit && welcomed, &uplink).await;
                            }
                        }
                        Err(e) => refuse(&mut answer_after, crate::wire::channel::Refusal::Malformed, format!("an agent limit this runner can't read: {e}")),
                    },
                    // A leg of the CI engine (docs/ci.md W06): this runner announces `CAP_CI2`.
                    ServerMsg::AssignV2 { json } => {
                        let mut a: kiln_ci::wire::AssignmentV2 = match serde_json::from_str(&json) {
                            Ok(a) => a,
                            Err(e) => {
                                tracing::warn!(error = %e, "an assignment this runner can't read");
                                continue;
                            }
                        };
                        let id = a.job_id.clone();
                        // A pool's leg runs in its executor, beside this runner's own slots.
                        let pooled = a.executor.slot.is_some();
                        let holds = kiln_ci::sched::state::Incoming::of(a.exclusive, &a.holds.unwrap_or_default());
                        let room = admits(o.slots, size, jobs.values().map(|j| (j.pooled, j.holds)), pooled, holds);
                        if o.outpost || draining || upgrade_pending || room.is_err() || jobs.contains_key(&id) {
                            let reason = if o.outpost { OUTPOST_TAKES_NO_JOBS } else if draining { "draining" } else if upgrade_pending { "upgrading" } else { room.err().unwrap_or("no free slot") };
                            uplink.msg(AgentMsg::Reject { job: id, reason: reason.into() }).await;
                            continue;
                        }
                        // An ephemeral runner (a `k8s` pool's pod, docs/ci.md W13-R07) takes one leg,
                        // then leaves.
                        if o.ephemeral {
                            served_dedicated = true;
                        }
                        // A new attempt: its events count from one.
                        journal.remove(&id);
                        unreceived.remove(&id);
                        // Server-relative URLs go through the address this runner reached the server
                        // at (NAT, NodePorts and cluster DNS all differ).
                        let base = o.server_url.trim_end_matches('/').to_owned();
                        let rt = &mut a.runtime;
                        for u in [&mut rt.results_url, &mut rt.cache_url, &mut rt.facade_url, &mut rt.api_url, &mut rt.graphql_url, &mut rt.registry_url] {
                            if u.starts_with('/') {
                                *u = format!("{base}{u}");
                            }
                        }
                        for (_, u) in &mut rt.proxies {
                            if u.starts_with('/') {
                                *u = format!("{base}{u}");
                            }
                        }
                        // `GITHUB_SERVER_URL` has no trailing slash; `ACTIONS_CACHE_URL` and
                        // `ACTIONS_RESULTS_URL` keep theirs, as the toolkit appends paths to them.
                        if rt.facade_url.ends_with('/') {
                            rt.facade_url.pop();
                        }
                        if let Some(u) = rt.id_token_url.as_mut().filter(|u| u.starts_with('/')) {
                            *u = format!("{base}{u}");
                        }
                        for act in a.actions.values_mut() {
                            if act.url.starts_with('/') {
                                act.url = format!("{base}{}", act.url);
                            }
                        }
                        let cancel = Shutdown::new();
                        hold_cpus(&id, holds);
                        jobs.insert(id.clone(), Running { cancel: cancel.clone(), pooled, holds });
                        tracing::info!(job = %id, leg = %a.leg.name, "running leg");
                        // Its pool's executor, when the leg can run in it as it is; else the leg
                        // runs the usual way and the executor goes all the same.
                        let warm = a.executor.slot.as_deref().and_then(|s| pool_execs.take(s));
                        let warm = match warm {
                            Some(w) if crate::runner::pool::adoptable(&a, &w) => Some(w),
                            Some(w) => {
                                crate::rt::spawn(crate::runner::pool::discard(w));
                                None
                            }
                            None => None,
                        };
                        let leg = crate::runner::runtime::Leg { a, work_dir: o.work_dir.clone(), runner_name: name.clone(), up: uplink.clone(), cancel, http: http.clone(), warm: std::sync::Arc::new(parking_lot::Mutex::new(warm)), cache_max: o.declared.cache_max.as_deref().and_then(crate::runner::cache::parse_size) };
                        let done = done_tx.clone();
                        // On the `firecracker` executor, in a microVM of its own (W13-R05).
                        #[cfg(target_os = "linux")]
                        let microvm = (leg.a.executor.kind == "firecracker").then(|| microvm_ca.clone());
                        crate::rt::spawn(async move {
                            #[cfg(target_os = "linux")]
                            let result = match microvm {
                                Some(ca) => crate::runner::firecracker::run(leg, Some(ca)).await,
                                None => leg.run().await,
                            };
                            #[cfg(not(target_os = "linux"))]
                            let result = leg.run().await;
                            let _ = done.send(result).await;
                        });
                    }
                    ServerMsg::ProbeFacts => probe_facts(&uplink, &o.work_dir, &o.declared, &probing),
                    // Start or stop a pool's executor (docs/ci.md W13-R09).
                    ServerMsg::Executor { json } => {
                        let ask: crate::runner::pool::Ask = match serde_json::from_str(&json) {
                            Ok(a) => a,
                            Err(e) => {
                                tracing::warn!(error = %e, "an executor request this runner can't read");
                                continue;
                            }
                        };
                        let (execs, work, up) = (pool_execs.clone(), o.work_dir.clone(), uplink.clone());
                        crate::rt::spawn(async move {
                            if let Some(said) = crate::runner::pool::handle(&execs, &work, ask).await
                                && let Ok(json) = serde_json::to_string(&said)
                            {
                                up.msg(AgentMsg::Executor { json }).await;
                            }
                        });
                    }
                    // A deployment this runner made, taken down: its environment, a preview, went
                    // (docs/ci.md W15-R07).
                    ServerMsg::Undeploy { json } => {
                        let ask: kiln_ci::wire::Undeploy = match serde_json::from_str(&json) {
                            Ok(a) => a,
                            Err(e) => {
                                tracing::warn!(error = %e, "a request to take a deployment down this runner can't read");
                                continue;
                            }
                        };
                        let up = uplink.clone();
                        crate::rt::spawn(async move {
                            let error = crate::runner::runtime::undeploy(&ask).await.err().map(|e| format!("{e:#}"));
                            if let Ok(json) = serde_json::to_string(&kiln_ci::wire::Undeployed { asked: ask, error }) {
                                up.msg(AgentMsg::Undeployed { json }).await;
                            }
                        });
                    }
                    // A terminal into a running or kept leg (docs/ci.md W17-R01), or a command run
                    // there (W17-R05): its workspace and environment, or its container; the leg's
                    // secrets masked in what it shows.
                    m @ (ServerMsg::JobTerminalOpen { .. } | ServerMsg::JobExec { .. }) => {
                        let (job, session, cols, rows, command) = match m {
                            ServerMsg::JobTerminalOpen { job, session, cols, rows } => (job, session, cols, rows, None),
                            ServerMsg::JobExec { job, session, command } => (job, session, 200, 50, Some(command)),
                            _ => continue,
                        };
                        if !o.job_terminal {
                            uplink.0.send(Up::Term(AgentMsg::TermExit { session, code: None, error: Some("terminal disabled on this agent".into()) })).await.ok();
                            continue;
                        }
                        if terms.contains_key(&session) {
                            continue;
                        }
                        // A leg in a microVM here (W13-R05): its terminal opens in there.
                        #[cfg(target_os = "linux")]
                        if crate::runner::firecracker::open_terminal(&job, &session, cols, rows, command.clone()) {
                            continue;
                        }
                        let Some(shell) = crate::runner::kept::shell(&job) else {
                            // A leg this runner holds but is still setting up: its shell comes once
                            // its workspace and environment are ready, and the ask is taken again
                            // then (or after LEG_SHELL_WAIT).
                            if jobs.contains_key(&job) && !shell_waits.contains_key(&session) {
                                shell_waits.insert(session.clone(), Vec::new());
                                let (again, frame, waiting) = (bg_tx.clone(), data.clone(), session.clone());
                                crate::rt::spawn(async move {
                                    let until = Instant::now() + LEG_SHELL_WAIT;
                                    while crate::runner::kept::shell(&job).is_none() && Instant::now() < until {
                                        crate::rt::sleep(Duration::from_millis(100)).await;
                                    }
                                    let _ = again.send(Bg::Again(waiting, frame)).await;
                                });
                                continue;
                            }
                            shell_waits.remove(&session);
                            uplink.0.send(Up::Term(AgentMsg::TermExit { session, code: None, error: Some("the leg isn't running or kept on this runner".into()) })).await.ok();
                            continue;
                        };
                        let typed = shell_waits.remove(&session).unwrap_or_default();
                        let l = shell.launch(command.as_deref(), o.shell.as_deref());
                        let env: Vec<(&str, String)> = l.env.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                        match terminal::spawn_at(l.shell.as_deref(), l.command.as_deref(), l.program.as_deref(), l.cwd.as_deref(), cols, rows, &env) {
                            Ok((pty, rx)) => {
                                let (tx, s, masked) = (term_tx.clone(), session.clone(), crate::runner::kept::masked(rx, &shell.masks));
                                crate::rt::spawn(async move {
                                    while let Ok(out) = masked.recv().await {
                                        if tx.send((s.clone(), out)).await.is_err() {
                                            break;
                                        }
                                    }
                                });
                                let state = crate::termsess::Session::detached(session.clone(), format!("leg:{job}"), None, cols, rows);
                                for data in typed {
                                    pty.write(data);
                                }
                                terms.insert(session, (pty, state));
                            }
                            Err(e) => {
                                uplink.0.send(Up::Term(AgentMsg::TermExit { session, code: None, error: Some(format!("{e:#}")) })).await.ok();
                            }
                        }
                    }
                    // A kept leg let go now, or kept longer (W17-R02).
                    ServerMsg::KeepRelease { job } => {
                        #[cfg(target_os = "linux")]
                        if crate::runner::firecracker::to_leg(&job, crate::runner::guest::ToGuest::Release) {
                            continue;
                        }
                        crate::rt::spawn(async move {
                            crate::runner::kept::release(&job).await;
                        });
                    }
                    ServerMsg::KeepExtend { job, minutes } => {
                        #[cfg(target_os = "linux")]
                        if crate::runner::firecracker::to_leg(&job, crate::runner::guest::ToGuest::Extend(minutes)) {
                            continue;
                        }
                        crate::runner::kept::extend(&job, minutes);
                    }
                    ServerMsg::Received { job } if job.is_empty() => receipts = true,
                    ServerMsg::Received { job } => {
                        unreceived.remove(&job);
                        if let Some((_, _, events)) = journal.get_mut(&job) {
                            events.clear();
                        }
                    }
                    ServerMsg::JobRerunStep { job, step, idx } => {
                        #[cfg(target_os = "linux")]
                        if crate::runner::firecracker::to_leg(&job, crate::runner::guest::ToGuest::RerunStep { step, idx }) {
                            continue;
                        }
                        if !crate::runner::runtime::rerun_step(&job, step, idx) {
                            tracing::info!(leg = %job, "a step rerun for a leg that isn't kept here");
                            refuse(&mut answer_after, crate::wire::channel::Refusal::Unknown, format!("leg {job} isn't kept here"));
                        }
                    }
                    // The rest of CI's messages (W09, W13) land with their workstreams.
                    ServerMsg::SecretsFor { .. } => {
                        tracing::debug!("ignoring a CI message this runner does not take yet");
                    }
                }
            }
            Ev::Headroom => {
                if connected
                    && server_headroom
                    && let Some(json) = headroom.as_ref().and_then(crate::headroom::Handle::latest)
                    && headroom_sent.as_deref() != Some(json.as_str())
                    && let Ok(b) = proto::encode(&AgentMsg::HeadroomStatus { json: json.clone() })
                {
                    net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                    headroom_sent = Some(json);
                }
            }
            Ev::FrontDoor => {
                if connected
                    && let Some(json) = front_door.as_ref().and_then(crate::frontdoor::runner::Handle::latest)
                    && front_door_sent.as_deref() != Some(json.as_str())
                    && let Ok(b) = proto::encode(&AgentMsg::FrontDoorStatus { json: json.clone() })
                {
                    net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                    front_door_sent = Some(json);
                }
            }
            Ev::Term(session, out) => {
                let msg = match out {
                    PtyOut::Data(data) => {
                        if let Some((_, state)) = terms.get(&session) {
                            state.output(&data);
                        }
                        AgentMsg::TermOutput { session, data }
                    }
                    PtyOut::Exit(code) => {
                        terms.remove(&session);
                        AgentMsg::TermExit { session, code, error: None }
                    }
                };
                if updates_on {
                    // Over the channel: its output comes once, in order, through a cut too.
                    let (key, exited) = match &msg {
                        AgentMsg::TermExit { session, .. } => (crate::wire::uplink::terminal_key(session), true),
                        AgentMsg::TermOutput { session, .. } => (crate::wire::uplink::terminal_key(session), false),
                        _ => (String::new(), false),
                    };
                    if !updates.started(&key) {
                        updates.start(&key);
                    }
                    if let Ok(b) = proto::encode(&msg) {
                        updates.push(&key, b);
                    }
                    if exited {
                        updates.closing(&key);
                    }
                    if connected {
                        flush_updates(&mut updates, &uplink).await;
                    }
                } else {
                    uplink.0.send(Up::Term(msg)).await.ok();
                }
            }
            Ev::Up(mut up) => {
                // A job's event: numbered, and kept until the job's result is received.
                let mut journaled = None;
                if let Up::Msg(AgentMsg::JobEvent { job, json }) = &mut up {
                    let (at, seq, _) = journal.entry(job.clone()).or_insert_with(|| (Instant::now(), 0, Vec::new()));
                    *at = Instant::now();
                    *seq += 1;
                    if let Some(rest) = json.strip_prefix('{') {
                        *json = format!("{{\"seq\":{seq}{}{rest}", if rest.trim_start().starts_with('}') { "" } else { "," });
                    }
                    journaled = Some(job.clone());
                }
                let (lane, kind, data) = match &up {
                    Up::Msg(m) => (LANE_CONTROL, Kind::Control, proto::encode(m)),
                    Up::Log(c) => (LANE_LOGS, Kind::Log, proto::encode(c)),
                    Up::Term(m) => (LANE_TERM, Kind::Term, proto::encode(m)),
                    Up::Sync(m) => (LANE_SYNC, Kind::Control, proto::encode(m)),
                    Up::Reach(m) => (LANE_REACH, Kind::Term, proto::encode(m)),
                    Up::Convo(m) => (LANE_CONVO, Kind::Log, proto::encode(m)),
                    Up::File(m) => (LANE_FILES, Kind::Log, proto::encode(m)),
                    Up::Stream(m) => (LANE_STREAM, Kind::Term, proto::encode(m)),
                    Up::Port(m) => (LANE_PORTS, Kind::Term, proto::encode(m)),
                };
                let Ok(data) = data else { continue };
                if let Some((_, _, events)) = journaled.as_ref().and_then(|j| journal.get_mut(j)) {
                    events.push(data.clone());
                }
                // What the next connection sends again (a leg's result not yet received, its
                // events once the server takes them again) needn't be kept for it.
                let again = matches!(&up, Up::Msg(AgentMsg::JobFinished(r)) if unreceived.contains_key(&r.job)) || (receipts && journaled.is_some());
                // Logs and jobs' messages wait for the Welcome too, and come after what waited.
                if connected && (welcomed || (lane != LANE_LOGS && lane != LANE_CONTROL)) {
                    match &delay_line {
                        Some(d) => d.send(lane, kind, data, again),
                        None => {
                            net.send(SERVER_PEER, lane, kind, data).await;
                        }
                    }
                } else if again {
                    // Sent from `unreceived` and `journal` once connected again.
                } else if kind != Kind::Term && lane != LANE_SYNC && lane != LANE_REACH && lane != LANE_CONVO && lane != LANE_FILES {
                    buffered += data.len();
                    buffer.push_back((lane, kind, data));
                    // Shed the oldest log chunks first; results and step events are kept.
                    while buffered > UPLINK_BUFFER {
                        let Some(pos) = buffer.iter().position(|(_, k, _)| *k == Kind::Log) else { break };
                        if let Some((_, _, d)) = buffer.remove(pos) {
                            buffered = buffered.saturating_sub(d.len());
                        }
                    }
                }
            }
            Ev::Harness(session, u) => {
                note_announced(&mut announced, &session, &u);
                if updates_on {
                    // Over the channel: kept until the server has it, down or not. A new session's
                    // stream starts with its Open; a closed one's ends once answered.
                    let key = crate::wire::uplink::session_key(&session);
                    if !updates.started(&key)
                        && let crate::harness::core::Update::Open { info, .. } = &u
                    {
                        // Announced after the server said what its copy has, of this epoch (a
                        // session that started again from the store): its stream starts as the
                        // Welcome starts one, with only what the copy lacks.
                        let kept = server_kept.get(&session).filter(|k| k.epoch == info.epoch);
                        if let (Some(kept), Some(s)) = (kept, harness.as_ref().and_then(|h| h.get(&session))) {
                            start_stream(&mut updates, &session, &s, Some(kept));
                            if connected {
                                flush_updates(&mut updates, &uplink).await;
                            }
                            continue 'events;
                        }
                        updates.start(&key);
                    }
                    push_update(&mut updates, &session, &u);
                    if matches!(u, crate::harness::core::Update::Closed) {
                        updates.closing(&key);
                    }
                    if connected {
                        flush_updates(&mut updates, &uplink).await;
                    }
                } else if connected && welcomed {
                    // A server from before the channel: not kept across a disconnect, the
                    // reconnect sends every session whole (and before its Welcome, nothing: the
                    // Welcome brings the server all of each, over the channel or not).
                    for json in harness_frames(&session, &u) {
                        uplink.0.send(Up::Term(AgentMsg::Harness { session: session.clone(), json })).await.ok();
                    }
                }
            }
            Ev::Done(r) => {
                if retiring {
                    retire_at = Some(Instant::now() + Duration::from_secs(5)).max(retire_at);
                }
                if jobs.remove(&r.job).is_some() {
                    crate::runner::cpus::pins().release(&r.job);
                    tracing::info!(job = %r.job, status = r.status.as_str(), "job finished");
                    if let Ok(b) = proto::encode(&AgentMsg::JobFinished(r.clone())) {
                        unreceived.insert(r.job.clone(), (Instant::now(), b));
                    }
                    uplink.msg(AgentMsg::JobFinished(r)).await;
                } else {
                    // Reported over already: a cancelled job let go of after its grace that ended
                    // after all (or the grace's report, after the job's own).
                    tracing::info!(job = %r.job, "a job already reported over has ended");
                }
            }
            Ev::Heartbeat => {
                // Counts of jobs long done (a kept leg is let go well within a day).
                journal.retain(|_, (at, _, events)| !events.is_empty() || at.elapsed() < Duration::from_secs(86_400));
                // Its config's agent limit, read again: a change holds at once, nothing restarting.
                // A config that can't be read now (a change half written) keeps the last one read.
                if let (Some(h), Some(path)) = (&harness, o.config.clone()) {
                    let read = crate::rt::blocking(move || crate::harness::limit::configured(&path)).await.ok().flatten();
                    if let Some(config) = read.filter(|c| *c != limit_config) {
                        if let Some(n) = config.filter(|n| crate::harness::limit::valid(*n).is_err()) {
                            tracing::warn!(max_agent_sessions = n, "[agent] max_agent_sessions is from 1 to {}: passed over", crate::harness::limit::MAX);
                        }
                        limit_config = config;
                        hold_agent_limit(h, crate::harness::limit::held(limit_asked, limit_config), &mut limit_said, server_limit && welcomed, &uplink).await;
                    }
                }
                if connected {
                    let free = if upgrade_pending || retiring { 0 } else { o.slots.saturating_sub(u32::try_from(jobs.len()).unwrap_or(0)) };
                    uplink.msg(AgentMsg::Heartbeat { free_slots: free, running: jobs.keys().cloned().collect() }).await;
                    // Its machine's say on upgrades, read again: a change goes to the server at once.
                    if !o.ephemeral && !o.outpost {
                        let accept = takes_updates(&o);
                        if accept_said != Some(accept) {
                            uplink.msg(AgentMsg::UpdatePrefs { accept }).await;
                            accept_said = Some(accept);
                        }
                    }
                }
            }
            Ev::Sample => {
                if connected && updates_on {
                    // Items whose answers are overdue (lost below the channel) go again.
                    flush_updates(&mut updates, &uplink).await;
                }
                // What it has of the server's commands, said again: one lost below the channel
                // mustn't hold the server's commands up until the next connection.
                if connected && server_wire {
                    uplink.0.send(Up::Term(AgentMsg::Wire(wire_in.have()))).await.ok();
                }
                if connected {
                    let s = sampler.sample();
                    uplink.msg(AgentMsg::Sample(s)).await;
                    // What its QUIC layer let go below the channel, for its page: once it changed.
                    let dropped = crate::metrics::net().counts();
                    if server_drops && welcomed && drops_said.as_ref() != Some(&dropped) {
                        uplink.msg(AgentMsg::NetDrops { dropped: dropped.clone() }).await;
                        drops_said = Some(dropped);
                    }
                    // What its agent sessions are held to, said again if the server lacks it.
                    if let Some(h) = &harness {
                        hold_agent_limit(h, crate::harness::limit::held(limit_asked, limit_config), &mut limit_said, server_limit && welcomed, &uplink).await;
                    }
                    // What it holds warm, for placement (docs/ci.md W24-R18): once it changed here,
                    // and every 10 minutes for what the machine's other runner processes changed.
                    let now_changes = crate::runner::warm::changes();
                    if server_work && welcomed && !o.ephemeral && !o.outpost && work_said.is_none_or(|(at, when)| at != now_changes || when.elapsed() >= Duration::from_secs(600)) {
                        let dir = o.work_dir.clone();
                        if let Ok(state) = crate::rt::blocking(move || crate::runner::warm::state(&dir)).await
                            && let Ok(json) = serde_json::to_string(&state)
                        {
                            uplink.msg(AgentMsg::WorkState { json }).await;
                        }
                        work_said = Some((now_changes, Instant::now()));
                    }
                    // What its conversation stores are doing, for the server's board (a server from
                    // before takes it for nothing). Never waits: a full link skips one.
                    for (st, role, feed) in [(convo.as_ref(), "own", shipper.as_ref().map(crate::convo::feed::Shipper::stats)), (mirror_store.as_ref(), "mirror", None)] {
                        if let Some(st) = st {
                            let report = crate::convo::telemetry::Report { role: role.into(), version: crate::convo::telemetry::version(), store: st.report(), feed };
                            let json = crate::convo::feed::encode(&crate::convo::feed::Msg::Stats { store: st.id().to_owned(), report: Box::new(report) });
                            let _ = uplink.0.try_send(Up::Convo(AgentMsg::Convo { json, data: Vec::new() }));
                        }
                    }
                    // Its battery, once a minute, when it has one: said when it changes, and every
                    // 10 minutes (a machine on battery sleeps when it runs out).
                    if !o.ephemeral && !o.outpost && power_read_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                        power_read_at = Some(Instant::now());
                        if let Ok(Some(b)) = crate::rt::blocking(crate::os::battery::read).await
                            && (power_said.as_ref().is_none_or(|(said, _)| said != &b) || power_said.as_ref().is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(600)))
                            && let Ok(json) = serde_json::to_string(&b)
                        {
                            uplink.msg(AgentMsg::Power { json }).await;
                            power_said = Some((b, Instant::now()));
                        }
                    }
                    if let Some(ai) = &ai {
                        let shells = terms.iter().filter_map(|(id, (pty, _))| Some((pty.pid()?, id.clone()))).collect();
                        ai.scan(shells, uplink.0.clone());
                    }
                    // A harness installed (or removed) since, or found once its user's login shell
                    // said its PATH (asked again every ten minutes): say so.
                    harness_kinds_tick = harness_kinds_tick.wrapping_add(1);
                    let users_path = crate::reach::serve::login_path().is_some();
                    if let Some(h) = &harness
                        && (harness_kinds_tick.is_multiple_of(12) || users_path != users_path_known)
                    {
                        users_path_known = users_path;
                        crate::rt::spawn(crate::reach::serve::login_env());
                        let now = serde_json::to_string(&crate::harness::kinds::offered()).ok();
                        if now != harness_kinds_sent
                            && let Ok(json) = serde_json::to_string(&h.inventory())
                        {
                            harness_kinds_sent = now;
                            uplink.0.send(Up::Term(AgentMsg::HarnessInventory { json })).await.ok();
                        }
                    }
                }
            }
        }
    }
}

/// Enroll (or renew) and store credentials without connecting. Used by `kiln install --agent`.
pub async fn enroll_only(o: &Opts) -> anyhow::Result<()> {
    credentials(o, &Shutdown::new()).await.map(|_| ())
}


/// One agent-session update onto its session's channel stream, as the frames it needs.
fn push_update(updates: &mut crate::wire::uplink::Updates, session: &str, u: &crate::harness::core::Update) -> usize {
    let key = crate::wire::uplink::session_key(session);
    let mut bytes = 0;
    for json in harness_frames(session, u) {
        if let Ok(b) = proto::encode(&AgentMsg::Harness { session: session.to_owned(), json }) {
            bytes += b.len();
            updates.push(&key, b);
        }
    }
    bytes
}

/// Starts `session`'s channel stream (again), its snapshot first: when the server's copy is of
/// the same epoch (`kept`), only what the copy lacks (`crate::wire::sums`), else all of it.
fn start_stream(updates: &mut crate::wire::uplink::Updates, session: &str, s: &crate::harness::session::Session, kept: Option<&proto::KeptSession>) {
    use crate::harness::core::Update;
    let key = crate::wire::uplink::session_key(session);
    updates.start(&key);
    let (snapshot, delta) = go_on(s.snapshot(), kept);
    // Its bytes, and of them its newest meta, state and changes, and its events after the copy's.
    let (mut bytes, mut newest, mut after) = (0, 0, 0);
    for u in &snapshot {
        let n = push_update(updates, session, u);
        bytes += n;
        match u {
            Update::Meta { .. } | Update::State { .. } | Update::Changes { .. } => newest += n,
            Update::Events { events } if delta.as_ref().is_some_and(|d| events.first().is_some_and(|e| e.seq > d.since)) => after += n,
            _ => {}
        }
    }
    match delta {
        Some(d) => tracing::info!(
            %session,
            stream = %updates.stream(&key),
            since = d.since,
            again = d.again.len(),
            bytes,
            beyond = bytes - newest - after,
            "an agent session's updates start on the channel after what the server's copy has"
        ),
        None => tracing::info!(%session, stream = %updates.stream(&key), bytes, "an agent session's updates start on the channel, whole"),
    }
}

/// A session's `snapshot` as a stream that starts sends it to a server whose copy is `kept`: when
/// that's of the same epoch, under an Open that says through which event the copy keeps its own
/// (`since`), with no Reset, its newest info, meta, state and changes, and only the events after
/// that and those through it whose digest differs from the copy's. Else the snapshot as it is.
fn go_on(snapshot: Vec<crate::harness::core::Update>, kept: Option<&proto::KeptSession>) -> (Vec<crate::harness::core::Update>, Option<crate::wire::sums::Delta>) {
    use crate::harness::core::Update;
    let epoch = snapshot.iter().find_map(|u| if let Update::Open { info, .. } = u { Some(info.epoch) } else { None });
    let Some(kept) = kept.filter(|k| Some(k.epoch) == epoch) else { return (snapshot, None) };
    // Each event as the copy keeps it, and its digest.
    let mine: Vec<(u64, u64)> = snapshot
        .iter()
        .filter_map(|u| if let Update::Events { events } = u { Some(events) } else { None })
        .flatten()
        .filter_map(sent_event)
        .filter_map(|e| serde_json::to_value(&e).ok().map(|v| (e.seq, crate::wire::sums::event(&v))))
        .collect();
    let Some(d) = crate::wire::sums::delta(kept, &mine) else { return (snapshot, None) };
    // Those it sends again go first, on their own, then those after the copy's newest: by number.
    let mut again = Vec::new();
    let mut out: Vec<Update> = Vec::new();
    for u in snapshot {
        match u {
            Update::Open { info, .. } => out.push(Update::Open { info, since: Some(d.since) }),
            Update::Reset { .. } => {}
            Update::Events { events } => {
                let (before, after): (Vec<_>, Vec<_>) = events.into_iter().partition(|e| e.seq <= d.since);
                again.extend(before.into_iter().filter(|e| d.again.binary_search(&e.seq).is_ok()));
                if !after.is_empty() {
                    out.push(Update::Events { events: after });
                }
            }
            u => out.push(u),
        }
    }
    if !again.is_empty() {
        let at = out.iter().position(|u| matches!(u, Update::Events { .. })).unwrap_or(out.len());
        out.insert(at, Update::Events { events: again });
    }
    (out, Some(d))
}

/// Notes a session's announcement in this process (and that it's gone, once closed).
fn note_announced(announced: &mut std::collections::HashSet<String>, session: &str, u: &crate::harness::core::Update) {
    match u {
        crate::harness::core::Update::Open { .. } => {
            announced.insert(session.to_owned());
        }
        crate::harness::core::Update::Closed => {
            announced.remove(session);
        }
        _ => {}
    }
}

/// Sends what the channel has to send now.
async fn flush_updates(updates: &mut crate::wire::uplink::Updates, uplink: &Uplink) {
    for f in updates.poll(Instant::now()) {
        uplink.0.send(Up::Term(AgentMsg::Wire(f))).await.ok();
    }
}

/// Most of one agent-session update a frame carries. They go on the interactive lane, which
/// refuses frames past 1 MiB ([`net::Kiln`]); a resumed session's history can be several.
const HARNESS_FRAME: usize = 512 * 1024;

/// An agent-session update as frames that fit: a long run of events goes in parts (events are
/// upserts by id, so the parts mean what the whole did), and an event too large on its own has
/// its longest texts cut, saying so. Anything else too large (a state, the changeset) is cut the
/// same way: dropped, its page would go on showing what no longer is, the agent looking stuck. A
/// state still too large sheds its finished tasks, the oldest first. Only what still doesn't fit
/// is dropped, and logged.
/// The command being applied on the channel (`answer_after`) is refused, saying how and why: a
/// message that came alone has no one to tell.
fn refuse(answer_after: &mut Option<(String, u64, Option<(crate::wire::channel::Refusal, String)>)>, code: crate::wire::channel::Refusal, why: impl Into<String>) {
    if let Some((_, _, refusal)) = answer_after.as_mut() {
        *refusal = Some((code, why.into()));
    }
}

fn harness_frames(session: &str, u: &crate::harness::core::Update) -> Vec<String> {
    use crate::harness::core::Update;
    let Ok(json) = serde_json::to_string(u) else { return Vec::new() };
    if json.len() <= HARNESS_FRAME {
        return vec![json];
    }
    let Update::Events { events } = u else {
        let budget = HARNESS_FRAME - 1024;
        // A state still too large once cut sheds its finished tasks first (their calls stay in
        // the timeline): dropped whole, its page would go on showing ended work as running.
        let fitted = serde_json::to_value(u)
            .ok()
            .map(|v| cut_value(v, budget))
            .map(|v| if json_len(&v) > budget { cut_value(shed_ended_tasks(v, budget), budget) } else { v })
            .and_then(|v| serde_json::to_string(&v).ok());
        return match fitted {
            Some(j) if j.len() <= HARNESS_FRAME => {
                tracing::debug!(session, bytes = json.len(), sent = j.len(), "agent session update cut to fit");
                vec![j]
            }
            _ => match essentials(u, budget) {
                // Still too large (its live tasks, or lists of many short entries, no text cut
                // shortens): what a viewer can't do without, rather than nothing. Dropped whole, the
                // page went on without its state at all: no effort, a switch never shown done.
                Some(j) if j.len() <= HARNESS_FRAME => {
                    tracing::warn!(session, bytes = json.len(), sent = j.len(), "agent session update too large to send whole; sent what it can't do without");
                    vec![j]
                }
                _ => {
                    tracing::warn!(session, bytes = json.len(), "agent session update too large to send, even cut; dropped");
                    Vec::new()
                }
            },
        };
    };
    let one = HARNESS_FRAME - 1024;
    let mut frames = Vec::new();
    let mut part = Vec::new();
    let mut size = 0;
    for e in events {
        let Some(e) = sent_event(e) else {
            tracing::warn!(session, event = %e.id, "agent session event too large to send, even cut; dropped");
            continue;
        };
        let n = json_len(&e).saturating_add(1);
        if size + n > one && !part.is_empty() {
            frames.extend(serde_json::to_string(&Update::Events { events: std::mem::take(&mut part) }));
            size = 0;
        }
        size += n;
        part.push(e);
    }
    if !part.is_empty() {
        frames.extend(serde_json::to_string(&Update::Events { events: part }));
    }
    frames
}

/// An event as it goes to the server: cut to fit a frame when it's larger. None: too large even cut.
fn sent_event(e: &crate::harness::model::Event) -> Option<crate::harness::model::Event> {
    let one = HARNESS_FRAME - 1024;
    let mut e = e.clone();
    if json_len(&e) > one {
        e = cut(e, one);
    }
    (json_len(&e).saturating_add(1) <= one).then_some(e)
}

fn json_len<T: Serialize>(v: &T) -> usize {
    serde_json::to_vec(v).map_or(usize::MAX, |b| b.len())
}

/// A state update too large even cut, down to what a viewer can't do without: how the session
/// runs (status, harness, model, permissions, effort), what waits on a person, its queue, what it
/// has going (its live tasks, cut), its usage and limits. Its lists of what it can use (servers'
/// tools, skills, plugins, commands) and its finished tasks wait for an update that fits. None:
/// not a state, or still too large.
fn essentials(u: &crate::harness::core::Update, budget: usize) -> Option<String> {
    let crate::harness::core::Update::State { state } = u else { return None };
    let mut s = (**state).clone();
    s.tasks.retain(crate::harness::model::Task::live);
    s.mcp.clear();
    s.skills.clear();
    s.plugins.clear();
    s.inventory = Default::default();
    let v = serde_json::to_value(crate::harness::core::Update::State { state: Box::new(s) }).ok()?;
    serde_json::to_string(&cut_value(v, budget)).ok()
}

/// A state update (`{"t":"state","state":{..}}`) without as many of its finished tasks, the oldest
/// first, as it takes to come within `budget` bytes of JSON (when finished tasks can).
fn shed_ended_tasks(mut v: serde_json::Value, budget: usize) -> serde_json::Value {
    let excess = json_len(&v).saturating_sub(budget);
    let Some(tasks) = v.pointer_mut("/state/tasks").and_then(serde_json::Value::as_array_mut) else { return v };
    let live = |t: &serde_json::Value| matches!(t.get("status").and_then(serde_json::Value::as_str), Some("running" | "pending" | "paused"));
    let mut freed = 0usize;
    let mut drop = std::collections::HashSet::new();
    for (i, t) in tasks.iter().enumerate() {
        if freed >= excess {
            break;
        }
        if !live(t) {
            freed = freed.saturating_add(json_len(t).saturating_add(1));
            drop.insert(i);
        }
    }
    let mut i = 0;
    tasks.retain(|_| {
        i += 1;
        !drop.contains(&(i - 1))
    });
    v
}

/// An event cut to about `budget` bytes of JSON: its longest texts shortened, each saying so.
fn cut(e: crate::harness::model::Event, budget: usize) -> crate::harness::model::Event {
    let Ok(v) = serde_json::to_value(&e) else { return e };
    serde_json::from_value(cut_value(v, budget)).unwrap_or(e)
}

/// JSON cut to about `budget` bytes: its longest texts shortened, each saying so.
fn cut_value(mut v: serde_json::Value, budget: usize) -> serde_json::Value {
    for _ in 0..64 {
        let len = json_len(&v);
        if len <= budget {
            break;
        }
        let mut best = (0, String::new());
        longest(&v, String::new(), &mut best);
        let Some(serde_json::Value::String(s)) = v.pointer_mut(&best.1) else { break };
        if s.len() < 256 {
            break;
        }
        let mut at = s.len().saturating_sub(len - budget + 64);
        while !s.is_char_boundary(at) {
            at -= 1;
        }
        let dropped = s.len() - at;
        s.truncate(at);
        s.push_str(&format!("\n… [{dropped} bytes cut by kiln to fit]"));
    }
    v
}

/// The JSON pointer of the longest string in `v`.
fn longest(v: &serde_json::Value, at: String, best: &mut (usize, String)) {
    match v {
        serde_json::Value::String(s) if s.len() > best.0 => *best = (s.len(), at),
        serde_json::Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                longest(x, format!("{at}/{i}"), best);
            }
        }
        serde_json::Value::Object(o) => {
            for (k, x) in o {
                longest(x, format!("{at}/{}", k.replace('~', "~0").replace('/', "~1")), best);
            }
        }
        _ => {}
    }
}

/// The binary's modification time (an upgrade replaces the file).
fn binary_mtime(p: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// One terminal carried across an in-place upgrade.
#[derive(Serialize, Deserialize)]
struct HandedTerm {
    session: String,
    command: Option<String>,
    cols: u16,
    rows: u16,
    handover: crate::terminal::Handover,
    /// Bytes that rebuild the session's screen and scrollback in the new image's emulator.
    replay: String,
}

/// What this machine holds its agent sessions to (`crate::harness::limit`), held by its sessions'
/// host from now on; and said to the server when `tell` (it takes it, on a connection it welcomed)
/// and it changed since the server last heard it.
async fn hold_agent_limit(h: &crate::harness::host::Host, held: crate::harness::limit::Held, said: &mut Option<crate::harness::limit::Held>, tell: bool, uplink: &Uplink) {
    if h.set_max_sessions(held.max_sessions) {
        tracing::info!(max_sessions = held.max_sessions, from = ?held.from, "agent sessions: this machine has at most this many at work at once");
    }
    if tell
        && *said != Some(held)
        && let Ok(json) = serde_json::to_string(&held)
    {
        uplink.msg(AgentMsg::AgentLimit { json }).await;
        *said = Some(held);
    }
}

/// Whether this machine takes upgrades from its server: its config's `[updates] accept`, read
/// now (a config that can't be read takes them, as before).
fn takes_updates(o: &Opts) -> bool {
    o.config.as_deref().and_then(|p| crate::config::Config::load(p).ok()).is_none_or(|c| c.updates.accept)
}

fn handover_path(o: &Opts) -> PathBuf {
    crate::upgrade::own_dir(&o.state_dir, o.instance.as_deref()).join("terminal-handover.json")
}

/// Whether this process stopped to take up a new binary, its terminals handed to its supervisor
/// (`main` then exits with [`crate::supervise::HANDOVER_EXIT`]).
static HANDED_OVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn handed_over() -> bool {
    HANDED_OVER.load(std::sync::atomic::Ordering::SeqCst)
}

/// Whether `exe` starts at all (`--version` succeeds, saying it is kiln): what an upgrade checks
/// before switching to it.
async fn binary_runs(exe: &std::path::Path) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--version");
    let out = crate::os::proc::output(cmd, Duration::from_secs(30)).await?;
    let said = String::from_utf8_lossy(&out.stdout);
    anyhow::ensure!(out.status.success() && said.starts_with("kiln"), "`{} --version` gave {} ({})", exe.display(), out.status, said.trim());
    Ok(())
}

/// Write every terminal's handover (what the process after this one needs to take it over) to
/// the handover file; returns the PTY masters in the file's order. Terminals that cannot be
/// handed over end with this upgrade.
fn write_handover(o: &Opts, terms: &HashMap<String, (PtySession, Arc<crate::termsess::Session>)>) -> std::io::Result<Vec<i32>> {
    use base64::Engine as _;
    let mut handed = Vec::new();
    for (id, (pty, state)) in terms {
        let Some(handover) = pty.handover() else {
            tracing::warn!(session = %id, "terminal cannot be handed over; it ends with this upgrade");
            continue;
        };
        let info = state.info();
        let a = state.attach();
        state.detach();
        handed.push(HandedTerm {
            session: id.clone(),
            command: state.command.clone(),
            cols: info.cols,
            rows: info.rows,
            handover,
            replay: base64::engine::general_purpose::STANDARD.encode(&a.replay),
        });
    }
    let json = serde_json::to_vec(&handed).map_err(std::io::Error::other)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    crate::os::with_mode(&mut opts, 0o600);
    use std::io::Write as _;
    opts.open(handover_path(o))?.write_all(&json)?;
    Ok(handed.iter().map(|h| h.handover.fd).collect())
}

/// Re-execute this process from `exe` (same pid), handing every terminal to the new image.
/// Returns only if the exec failed.
fn upgrade_in_place(o: &Opts, exe: &std::path::Path, terms: &HashMap<String, (PtySession, Arc<crate::termsess::Session>)>) -> std::io::Error {
    let handed = match write_handover(o, terms) {
        Ok(h) => h,
        Err(e) => return e,
    };
    tracing::info!(terminals = handed.len(), exe = %exe.display(), "upgrading in place");
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let err = crate::os::reexec(exe, &args, &[]);
    let _ = std::fs::remove_file(handover_path(o));
    err
}

/// How this process started, as far as its terminals go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    /// Nothing was handed over.
    Fresh,
    /// A previous image of this process re-executed itself into this one ([`upgrade_in_place`]).
    ReExecuted,
    /// The supervisor started this process with the terminals the runner before it handed over.
    Successor,
}

/// Take over the terminals handed over before this process started: by a previous image of it
/// that re-executed itself ([`upgrade_in_place`], the masters kept at the numbers the handover
/// file says), or by the runner process before it, through the supervisor ([`write_handover`],
/// the masters at the numbers the supervisor names, in the file's order). Reopen each PTY,
/// rebuild its replay state, and keep it for the server, which re-adopts it from the next TermList.
fn adopt_handover(
    o: &Opts,
    terms: &mut HashMap<String, (PtySession, Arc<crate::termsess::Session>)>,
    term_tx: &async_channel::Sender<(String, PtyOut)>,
) -> Start {
    use base64::Engine as _;
    let path = handover_path(o);
    let Ok(raw) = std::fs::read(&path) else { return Start::Fresh };
    let _ = std::fs::remove_file(&path);
    let given = crate::os::supervise::handed_terminals();
    let start = match &given {
        Some(_) => Start::Successor,
        // A fresh process its supervisor handed nothing: the file's numbers aren't this
        // process's descriptors.
        None if crate::os::supervise::has_supervisor_channel() => {
            tracing::warn!("the terminals of the runner before this one did not come across; their shells are lost");
            return Start::Fresh;
        }
        None => Start::ReExecuted,
    };
    let handed: Vec<HandedTerm> = match serde_json::from_slice(&raw) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, "unreadable terminal handover; its shells are lost");
            return start;
        }
    };
    let mut adopted = 0usize;
    for (i, mut h) in handed.into_iter().enumerate() {
        if let Some(fds) = &given {
            let Some(fd) = fds.get(i) else {
                tracing::warn!(session = %h.session, "terminal did not come across the upgrade");
                continue;
            };
            h.handover.fd = *fd;
        }
        let Some((pty, rx)) = terminal::adopt(h.handover) else {
            tracing::warn!(session = %h.session, "terminal did not survive the upgrade");
            continue;
        };
        let tx = term_tx.clone();
        let s = h.session.clone();
        crate::rt::spawn(async move {
            while let Ok(out) = rx.recv().await {
                let exit = matches!(out, PtyOut::Exit(_));
                if tx.send((s.clone(), out)).await.is_err() || exit {
                    break;
                }
            }
        });
        let state = crate::termsess::Session::detached(h.session.clone(), "agent".into(), h.command.clone(), h.cols, h.rows);
        if let Ok(replay) = base64::engine::general_purpose::STANDARD.decode(h.replay.as_bytes()) {
            state.output(&replay);
        }
        terms.insert(h.session, (pty, state));
        adopted += 1;
    }
    if adopted > 0 {
        tracing::info!(terminals = adopted, "terminals carried across the upgrade");
    }
    start
}


/// The copy of the server's conversation store this runner keeps (a mirror), beside its own, and
/// the intake that takes the server's feed into it, its answers going up the link.
fn open_mirror(dir: &std::path::Path, up: &async_channel::Sender<Up>) -> Option<(crate::convo::store::Store, crate::convo::feed::Intake)> {
    let st = crate::convo::open_file(dir, crate::convo::MIRROR_FILE, crate::config::Conversations::default().upkeep())?;
    match crate::convo::feed::Intake::start(st.clone(), crate::convo::feed::Brings::AsItSays, None) {
        Ok((intake, out)) => {
            let up = up.clone();
            crate::rt::spawn(async move {
                while let Ok(o) = out.recv().await {
                    if up.send(Up::Convo(AgentMsg::Convo { json: o.json, data: Vec::new() })).await.is_err() {
                        break;
                    }
                }
            });
            Some((st, intake))
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "this runner can't keep a copy of the conversation store");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CP-T3.2 (the thirteenth adversarial check's): a rollback takes nothing back until the new
    /// build is seen gone. One its supervisor didn't stop (none found here) is killed, and seen gone.
    #[cfg(unix)]
    #[test]
    fn a_new_build_its_supervisor_didn_t_stop_is_stopped_before_anything_is_taken_back() -> anyhow::Result<()> {
        let mut child = std::process::Command::new("sleep").arg("60").spawn()?;
        let pid = child.id();
        // Reaped as it goes (a zombie still counts as running where the system can't tell).
        let reaper = std::thread::spawn(move || child.wait());
        // No supervisor (one the tests' runner runs under is never asked).
        let gone = crate::rt::block_on(async move { anyhow::Ok(stop_new_build(None, crate::bluegreen::Green { pid }, Duration::from_millis(300)).await) })?;
        let reaped = reaper.join().map_err(|_| anyhow::anyhow!("the reaper panicked"))??;
        anyhow::ensure!(gone && !crate::bluegreen::alive(pid), "the new build still runs (gone {gone}; it ended {reaped:?})");
        Ok(())
    }
    use crate::harness::core::Update;
    use crate::harness::model::{Body, Event};

    /// The runner admits by the rule its dispatchers use, counting what jobs from either hold: on
    /// a 32-CPU, 64 GiB runner of 4 slots, a bench holding 16 CPUs leaves two slots, refusals say
    /// why, and a pool's leg runs in its executor beside them.
    #[test]
    fn a_runner_admits_work_beside_what_a_bench_holds() {
        use kiln_ci::sched::state::{Incoming, Resources};
        let size = Some(Resources { cpu: 32.0, mem: 64 << 30, ..Resources::default() });
        let bench = Incoming::Share(Resources { cpu: 16.0, mem: 16 << 30, ..Resources::default() });
        let o = Incoming::Ordinary;
        assert_eq!(admits(4, size, std::iter::empty(), false, bench), Ok(()));
        assert_eq!(admits(4, size, [(false, bench), (false, o)].into_iter(), false, o), Ok(()));
        assert_eq!(admits(4, size, [(false, bench), (false, o), (false, o)].into_iter(), false, o), Err("no free slot beside what's held"));
        assert_eq!(admits(4, size, [(false, o), (false, o), (false, o)].into_iter(), false, bench), Err("no room for the share it holds"));
        assert_eq!(admits(4, size, [(false, bench), (false, o), (false, o)].into_iter(), true, o), Ok(()), "a pool's leg runs beside the slots");
        // An exclusive job asking nothing has the runner to itself, as before.
        assert_eq!(admits(4, size, [(false, o)].into_iter(), false, Incoming::Whole), Err("an exclusive job waits for this runner to be empty"));
        assert_eq!(admits(4, size, [(false, Incoming::Whole)].into_iter(), true, o), Err("an exclusive job has this runner to itself"));
        // Without exclusive jobs, slots as ever.
        assert_eq!(admits(2, size, [(false, o), (false, o)].into_iter(), false, o), Err("no free slot"));
    }

    /// A stream that starts again sends a server whose copy has part of a session only what the
    /// copy lacks. The digests it compares are of the copy as the server keeps it: every kind of
    /// event, one cut to fit among them, taken in from the frames they went in. So an event the
    /// copy has is never sent again, and one that changed since is.
    #[test]
    fn a_stream_that_starts_again_sends_only_what_the_copy_lacks() -> anyhow::Result<()> {
        use crate::harness::model::{Level, ToolCall, ToolKind, ToolStatus};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = crate::harness::registry::Location::Machine("box".into());
            let info = crate::harness::session::Info { id: "s".into(), kind: "claude".into(), epoch: 2, ..Default::default() };
            let tool = |n: u64, status| {
                Body::Tool(Box::new(ToolCall {
                    tool_id: format!("t{n}"),
                    name: "Bash".into(),
                    tool: ToolKind::Shell,
                    title: "ls".into(),
                    detail: None,
                    input: serde_json::json!({"command": "ls -la", "timeout": 1.5, "nested": {"z": [1, 2.25, "\u{fc}"], "a": null}}),
                    status,
                    output: Some(format!("out {n}")),
                    truncated: false,
                    diffs: Vec::new(),
                    todos: Vec::new(),
                    task: None,
                    exit_code: Some(0),
                    started: 5,
                    ended: None,
                    server: None,
                    tool_short: None,
                    skill: None,
                    runner: None,
                }))
            };
            let body = |n: u64| match n % 5 {
                0 => Body::User { text: format!("ask {n}"), images: 1, client_id: Some("c".into()), origin: None, attachments: vec!["/a.png".into()], reply_to: None },
                1 => Body::Assistant { text: format!("line {n} \u{2713}"), streaming: false },
                2 => Body::Thinking { text: format!("think {n}"), streaming: false, redacted: n.is_multiple_of(2) },
                3 => tool(n, ToolStatus::Done),
                _ => Body::Notice { level: Level::Warn, text: format!("notice {n}") },
            };
            let ev = |n: u64| Event { id: format!("e{n}"), seq: n, at: 1_000 + i64::try_from(n).unwrap_or(0), parent: n.is_multiple_of(7).then(|| "e1".to_owned()), by: None, body: body(n) };
            let mut timeline: Vec<Event> = (1..=200).map(ev).collect();
            // One too large to go whole: cut on its way, and kept by the copy as cut.
            if let Some(e) = timeline.get_mut(41) {
                e.body = Body::Assistant { text: "y".repeat(HARNESS_FRAME * 2), streaming: false };
            }
            // The copy: the session's first 150 events, as they went over the wire.
            for u in [Update::Open { info: Box::new(info.clone()), since: None }, Update::Events { events: timeline.iter().take(150).cloned().collect() }] {
                for json in harness_frames("s", &u) {
                    hub.harness.ingest(&hub, &there, "s", serde_json::from_str(&json)?);
                }
            }
            let kept = hub.harness.kept_on("box");
            let kept = kept.first().ok_or_else(|| anyhow::anyhow!("no copy"))?;
            // Meanwhile its 120th changed (a tool call that ended), and 50 more came.
            if let Some(e) = timeline.get_mut(119) {
                e.body = tool(120, ToolStatus::Error);
            }
            let snapshot = || {
                vec![
                    Update::Open { info: Box::new(info.clone()), since: None },
                    Update::Reset { from: Some(1) },
                    Update::Meta { meta: Box::default() },
                    Update::State { state: Box::default() },
                    Update::Events { events: timeline.iter().take(100).cloned().collect() },
                    Update::Events { events: timeline.iter().skip(100).cloned().collect() },
                ]
            };
            let (out, delta) = go_on(snapshot(), Some(kept));
            anyhow::ensure!(delta == Some(crate::wire::sums::Delta { since: 150, again: vec![120] }), "{delta:?}");
            anyhow::ensure!(matches!(out.first(), Some(Update::Open { since: Some(150), .. })), "under an Open that says through which event the copy keeps its own");
            anyhow::ensure!(!out.iter().any(|u| matches!(u, Update::Reset { .. })), "no Reset: the copy keeps what it has");
            anyhow::ensure!(out.iter().any(|u| matches!(u, Update::Meta { .. })) && out.iter().any(|u| matches!(u, Update::State { .. })), "its newest meta and state");
            let sent: Vec<u64> = out.iter().filter_map(|u| if let Update::Events { events } = u { Some(events) } else { None }).flatten().map(|e| e.seq).collect();
            anyhow::ensure!(sent == std::iter::once(120).chain(151..=200).collect::<Vec<_>>(), "{sent:?}");
            // A copy of another epoch (it went on elsewhere since): all of it.
            let (whole, none) = go_on(snapshot(), Some(&proto::KeptSession { epoch: 1, ..kept.clone() }));
            anyhow::ensure!(none.is_none() && matches!(whole.get(1), Some(Update::Reset { .. })) && whole.len() == 6);
            Ok(())
        })
    }

    fn said(id: usize, text: String) -> Event {
        Event { id: format!("e{id}"), seq: id as u64, at: 0, parent: None, by: None, body: Body::Assistant { text, streaming: false } }
    }

    fn events(frame: &str) -> Vec<Event> {
        match serde_json::from_str::<Update>(frame) {
            Ok(Update::Events { events }) => events,
            _ => Vec::new(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_new_build_runs_as_an_instance_of_its_own() {
        let args = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(with_instance(&args(&["agent", "--config", "/c.toml"]), "g1"), args(&["agent", "--config", "/c.toml", "--instance", "g1"]));
        // An instance's own runner (upgraded before) gives way to the new one, in either spelling.
        assert_eq!(with_instance(&args(&["agent", "--instance", "old", "--slots", "2"]), "g2"), args(&["agent", "--slots", "2", "--instance", "g2"]));
        assert_eq!(with_instance(&args(&["agent", "--instance=old"]), "g3"), args(&["agent", "--instance", "g3"]));
    }

    #[test]
    fn a_long_history_goes_in_parts_that_fit() {
        // A resumed session's history: 900 events, about 1.8 MB.
        let all: Vec<Event> = (0..900).map(|i| said(i, "x".repeat(2000))).collect();
        let frames = harness_frames("s", &Update::Events { events: all.clone() });
        assert!(frames.len() >= 4, "{} frames", frames.len());
        assert!(frames.iter().all(|f| f.len() <= HARNESS_FRAME));
        let sent: Vec<Event> = frames.iter().flat_map(|f| events(f)).collect();
        assert_eq!(sent, all, "every event, whole and in order");
        // Small ones go as they are.
        let small = Update::Events { events: all.get(..3).unwrap_or_default().to_vec() };
        assert_eq!(harness_frames("s", &small), vec![serde_json::to_string(&small).unwrap_or_default()]);
    }

    #[test]
    fn an_event_too_large_alone_is_cut_and_says_so() {
        let huge = said(7, format!("start {}", "é".repeat(1_500_000)));
        let frames = harness_frames("s", &Update::Events { events: vec![said(1, "hi".into()), huge] });
        assert!(frames.iter().all(|f| f.len() <= HARNESS_FRAME));
        let sent: Vec<Event> = frames.iter().flat_map(|f| events(f)).collect();
        assert_eq!(sent.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["e1", "e7"]);
        let text = match sent.last().map(|e| &e.body) {
            Some(Body::Assistant { text, .. }) => text.as_str(),
            _ => "",
        };
        assert!(text.starts_with("start ") && text.ends_with("bytes cut by kiln to fit]"), "{}", text.get(..40).unwrap_or(text));
    }

    #[test]
    fn a_state_too_large_is_cut_not_dropped() {
        use crate::harness::model::{State, Status};
        // Dropped, its page would go on showing a turn under way that no longer is.
        let state = State { status: Status::Idle, summary: format!("begin {}", "x".repeat(900_000)), ..State::default() };
        let frames = harness_frames("s", &Update::State { state: Box::new(state) });
        assert_eq!(frames.len(), 1);
        assert!(frames.iter().all(|f| f.len() <= HARNESS_FRAME));
        let state = match serde_json::from_str::<Update>(frames.first().map_or("", String::as_str)) {
            Ok(Update::State { state }) => Some(state),
            _ => None,
        };
        assert!(state.as_ref().is_some_and(|s| s.status == Status::Idle), "the state, whole");
        let summary = state.map(|s| s.summary).unwrap_or_default();
        assert!(summary.starts_with("begin ") && summary.ends_with("bytes cut by kiln to fit]"));
    }

    #[test]
    fn a_state_too_large_for_its_tasks_sheds_the_oldest_finished_ones_not_the_update() {
        use crate::harness::model::{State, Status, Task};
        // A long Codex session's state: thousands of finished terminals, none long enough to cut.
        // Dropped, its console showed a finished command running for 37 minutes, and missed two
        // subagents that started after it.
        let ended =
            |i: usize| Task { id: format!("terminal-{i}"), description: "d".repeat(200), command: Some("c".repeat(200)), status: "completed".into(), class: Some("shell".into()), ..Task::default() };
        let mut tasks: Vec<Task> = (0..3000).map(ended).collect();
        tasks.insert(1, Task { id: "server".into(), status: "running".into(), class: Some("shell".into()), ..Task::default() });
        let state = State { status: Status::Running, tasks, ..State::default() };
        let frames = harness_frames("s", &Update::State { state: Box::new(state) });
        assert_eq!(frames.len(), 1);
        assert!(frames.iter().all(|f| f.len() <= HARNESS_FRAME));
        let state = match serde_json::from_str::<Update>(frames.first().map_or("", String::as_str)) {
            Ok(Update::State { state }) => Some(state),
            _ => None,
        };
        let ids: Vec<String> = state.as_ref().map(|s| s.tasks.iter().map(|t| t.id.clone()).collect()).unwrap_or_default();
        assert!(state.as_ref().is_some_and(|s| s.status == Status::Running));
        assert!(ids.iter().any(|id| id == "server") && ids.iter().any(|id| id == "terminal-2999"), "the live and the newest stay");
        assert!(!ids.iter().any(|id| id == "terminal-0"), "the oldest finished go first");
    }

    #[test]
    fn a_state_too_large_even_shed_still_says_how_the_session_runs() {
        use crate::harness::model::{McpServer, State, Status, Task};
        // Too large for a list of thousands of short entries (a server's tools) beside its live
        // tasks: no text long enough to cut, no finished task to shed. Dropped whole, its page went
        // on without its state at all: no effort, a harness switch never shown done.
        let tools: Vec<serde_json::Value> = (0..12_000).map(|i| serde_json::json!({ "name": format!("tool_{i:05}"), "description": "d".repeat(40) })).collect();
        let server = serde_json::from_value::<McpServer>(serde_json::json!({ "name": "big", "scope": "user", "transport": "stdio", "status": "connected", "enabled": true, "tools": tools }));
        assert!(server.is_ok(), "{server:?}");
        let Ok(server) = server else { return };
        let live = |i: usize| Task { id: format!("bg-{i}"), status: "running".into(), class: Some("shell".into()), command: Some(format!("serve {i}")), ..Task::default() };
        let state = State { status: Status::Running, model: "gpt-6.1-sol".into(), mode: "full-access".into(), effort: "ultra".into(), tasks: (0..7).map(live).collect(), mcp: vec![server], ..State::default() };
        assert!(json_len(&state) > HARNESS_FRAME);
        let frames = harness_frames("s", &Update::State { state: Box::new(state) });
        assert_eq!(frames.len(), 1, "sent, not dropped");
        assert!(frames.iter().all(|f| f.len() <= HARNESS_FRAME));
        let got = match serde_json::from_str::<Update>(frames.first().map_or("", String::as_str)) {
            Ok(Update::State { state }) => Some(state),
            _ => None,
        };
        let got = got.as_deref();
        assert_eq!(got.map(|s| (s.model.as_str(), s.mode.as_str(), s.effort.as_str(), s.status)), Some(("gpt-6.1-sol", "full-access", "ultra", Status::Running)));
        assert_eq!(got.map(|s| s.tasks.len()), Some(7), "its live tasks stay");
        assert!(got.is_some_and(|s| s.mcp.is_empty()), "its lists wait for an update that fits");
    }
}
