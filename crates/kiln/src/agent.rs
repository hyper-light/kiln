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
/// instance's own while it is valid, else the machine's.
async fn instance_credentials(o: &Opts, id: &str, shutdown: &Shutdown) -> anyhow::Result<(Vec<u8>, Leaf, Option<String>)> {
    let dir = crate::upgrade::instance_dir(&o.state_dir, id);
    let ca = std::fs::read_to_string(o.state_dir.join("ca.pem")).context("this machine is not enrolled (no ca.pem)")?;
    let (machine_cert, key) = crate::upgrade::machine_credentials(&o.state_dir)?;
    let quic = std::fs::read_to_string(o.state_dir.join("quic")).ok().map(|s| s.trim().to_owned());
    let cert_p = dir.join("agent.pem");
    let own = std::fs::read_to_string(&cert_p).ok();
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
    /// A blue/green upgrade step done off the loop (`crate::bluegreen`).
    Bg(Bg),
    /// A server frame about something another runner process of this machine handed over (or a
    /// rollback took back), handled as if the server had sent it here.
    #[cfg_attr(not(unix), allow(dead_code))]
    Forwarded(Vec<u8>),
    /// A test build's fault: the connection drops (W22-AC04).
    ChaosDrop,
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

/// This runner's arguments with `--instance` set to `id` (the new build runs as its own instance).
#[cfg(unix)]
fn with_instance(args: &[String], id: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + 2);
    let mut skip = false;
    for a in args {
        if std::mem::take(&mut skip) {
            continue;
        }
        if a == "--instance" {
            skip = true;
            continue;
        }
        if a.starts_with("--instance=") {
            continue;
        }
        out.push(a.clone());
    }
    out.push("--instance".to_owned());
    out.push(id.to_owned());
    out
}

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

/// Serve the machine's local sockets (`kiln run`, `kiln spawn`) from this runner process: the new
/// build once it has taken over, or the old one again after a rollback (the paths are the same).
#[cfg(unix)]
fn serve_sockets(reach: Option<&crate::reach::node::Handle>, harness: Option<&Arc<crate::harness::host::Host>>, name: &str, state_dir: &std::path::Path) {
    crate::browser::set_home(state_dir);
    if let Some(r) = reach
        && let Err(e) = crate::reach::local::serve(r.clone(), name.to_owned(), crate::reach::local::path(name, state_dir))
    {
        tracing::warn!(error = %format!("{e:#}"), "`kiln run` will not be available on this machine");
    }
    if let Some(h) = harness
        && let Err(e) = crate::harness::socket::serve(h, crate::harness::socket::path(name, state_dir))
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
    if let Some(sup) = crate::bluegreen::Supervisor::from_env() {
        let green = mv.green;
        if let Ok(Err(e)) | Err(e) = crate::rt::blocking(move || sup.kill(green)).await {
            // Gone already (it exited, or it was never started).
            tracing::debug!(error = %format!("{e:#}"), pid = green.pid, "stopping the new build");
        }
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
async fn front_door_changed(fd: Option<&crate::frontdoor::runner::Handle>) {
    match fd {
        Some(h) => h.changed().await,
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
    // A Mac set up for kiln signing: launchd's re-sign watcher as this kiln writes it.
    if cfg!(target_os = "macos") && !o.ephemeral {
        std::thread::spawn(crate::os::macsign::keep_watching);
    }
    std::fs::create_dir_all(&o.work_dir).with_context(|| format!("create {}", o.work_dir.display()))?;
    let orphans = crate::exec::reap_orphans(&o.work_dir);
    if orphans > 0 {
        tracing::warn!(orphans, "killed job processes left over from a previous agent run");
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
    let (net, events) = net::start(net::Spec { bind: "0.0.0.0:0".parse()?, ca_der: ca, leaf, server: false, dial_deadline: net::DIAL_DEADLINE })?;
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
    // Reach: links to other runners, and the runs this machine does for them (none in a job pod).
    // Agent sessions other runners hand this one (a session moving here) are taken over by its
    // session host, once there is one (below).
    let (adopt_tx, adopt_rx) = async_channel::bounded::<crate::reach::node::Adoption>(4);
    let reach = o.reach.filter(|_| !o.ephemeral).and_then(|accept| match crate::reach::node::start(crate::reach::node::Config {
        adopt: Some(adopt_tx.clone()),
        ..crate::reach::node::Config::for_runner(&name, &o.state_dir, accept)
    }) {
        Ok((handle, said)) => {
            crate::reach::node::set_global(handle.clone());
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
                match crate::reach::local::serve(handle.clone(), name.clone(), path) {
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
    let delay_line = chaos.and_then(|c| c.delay).map(|by| crate::runner::chaos::DelayLine::new(net.clone(), by));
    let mut jobs: HashMap<String, Running> = HashMap::new();
    // Pools' executors this runner started (docs/ci.md W13-R09).
    let pool_execs = crate::runner::pool::Executors::default();
    crate::runner::pool::sweep(&o.work_dir);
    // And the containers and networks this runner left running when it last stopped.
    crate::runner::docker::sweep(&o.work_dir).await;
    let mut exclusive = false;
    let mut draining = false;
    // Terminals outlive the server connection: kept (with their replay state) across disconnects and
    // announced again on every reconnect.
    let mut terms: HashMap<String, (PtySession, Arc<crate::termsess::Session>)> = HashMap::new();
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
    // Conversations kept on disk as they happen (docs/conversations.md), and sent to the server's
    // store over this link (`crate::convo::feed`).
    let mut convo: Option<crate::convo::store::Store> = None;
    let harness = (!o.ephemeral && o.terminal).then(|| {
        convo = crate::convo::open(&own_dir, crate::config::Conversations::default().upkeep());
        let sink = convo.as_ref().map_or_else(|| harness_tx.clone(), |st| crate::convo::store::tee(harness_tx.clone(), st.clone(), 8192));
        let h = crate::harness::host::Host::new(sink, Some(own_dir.join("harness.json")), Default::default());
        h.restore_from(convo.as_ref());
        h
    });
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
        match crate::harness::socket::serve(h, path) {
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
                        crate::rt::blocking(move || h.adopt(&from, &data)).await.unwrap_or_else(|e| Err(format!("{e:#}")))
                    }
                    None => Err("this runner runs no agent sessions".into()),
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

    loop {
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
            t = term_rx.recv().fuse() => match t { Ok((s, out)) => Ev::Term(s, out), Err(_) => Ev::Watchdog },
            up = up_rx.recv().fuse() => match up { Ok(u) => Ev::Up(u), Err(_) => Ev::Watchdog },
            h = harness_rx.recv().fuse() => match h { Ok((s, u)) => Ev::Harness(s, u), Err(_) => Ev::Watchdog },
            () = front_door_changed(front_door.as_ref()).fuse() => Ev::FrontDoor,
            b = bg_rx.recv().fuse() => match b {
                #[cfg(unix)]
                Ok(Bg::Forwarded(data)) => Ev::Forwarded(data),
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
                return Ok(());
            }
            Ev::ChaosDrop => {
                chaos_drop = None;
                if connected {
                    tracing::warn!("chaos: dropping the connection to the server");
                    net.command(Out::Disconnect { peer: SERVER_PEER }).await;
                }
            }
            #[cfg(not(unix))]
            Ev::Bg(b) => match b {},
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
                    }
                    Bg::Promoted => {
                        // Joined ranked oldest (new work stayed with the old runner): the newest now.
                        tracing::info!("promoted: this runner is the machine's newest; new work comes here");
                        instance_hello.created_at = real_created_at;
                        draining = false;
                        uplink.msg(AgentMsg::Instance(instance_hello.clone())).await;
                    }
                    // Turned into Ev::Forwarded as it's received.
                    Bg::Forwarded(_) => {}
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
                let mut build = if o.outpost { crate::updates::outpost_build() } else { crate::updates::runner_build() };
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
                } else {
                    tracing::info!(%reason, via = %paths.via(), retry_in = ?dial_backoff, "dial failed");
                    // The path changed (to the tunnel, or back): dial that way now.
                    at_once = paths.dial_failed(now);
                }
                connected = false;
                welcomed = false;
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
                        // The server learns (again) which agent sessions run here, then all of each.
                        if let Some(h) = &harness {
                            while harness_rx.try_recv().is_ok() {}
                            if let Ok(json) = serde_json::to_string(&h.inventory()) {
                                harness_kinds_sent = serde_json::to_string(&crate::harness::kinds::offered()).ok();
                                uplink.0.send(Up::Term(AgentMsg::HarnessInventory { json })).await.ok();
                            }
                            for info in h.list() {
                                let Some(s) = h.get(&info.id) else { continue };
                                for u in s.snapshot() {
                                    for json in harness_frames(&info.id, &u) {
                                        uplink.0.send(Up::Term(AgentMsg::Harness { session: info.id.clone(), json })).await.ok();
                                    }
                                }
                            }
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
                        let full = u32::try_from(jobs.values().filter(|j| !j.pooled).count()).unwrap_or(u32::MAX) >= o.slots.max(1);
                        if o.outpost || draining || upgrade_pending || full || exclusive || (a.spec.exclusive && !jobs.is_empty()) || jobs.contains_key(&id) {
                            let reason = if o.outpost { OUTPOST_TAKES_NO_JOBS } else if draining { "draining" } else if upgrade_pending { "upgrading" } else { "no free slot" };
                            uplink.msg(AgentMsg::Reject { job: id, reason: reason.into() }).await;
                            continue;
                        }
                        if o.dedicated_job.as_deref() == Some(id.as_str()) {
                            served_dedicated = true;
                        }
                        exclusive = a.spec.exclusive;
                        let cancel = Shutdown::new();
                        jobs.insert(id.clone(), Running { cancel: cancel.clone(), pooled: false });
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
                        }
                    }
                    ServerMsg::Drain => draining = true,
                    ServerMsg::Retire { instance } => {
                        // An older runner process of this machine holds nothing any more: stop
                        // the service this instance replaces (it checks for terminals itself).
                        let state_dir = o.state_dir.clone();
                        let me = instance_id.clone();
                        crate::rt::spawn(async move {
                            match crate::rt::blocking(move || crate::upgrade::retire(&state_dir, &me)).await {
                                Ok(Ok(crate::upgrade::Retired::Stopped(unit))) => {
                                    tracing::info!(%instance, %unit, "retired the older runner process: its service is stopped and disabled");
                                }
                                Ok(Ok(crate::upgrade::Retired::Busy(n))) => {
                                    tracing::warn!(%instance, terminals = n, "the older runner process still holds terminals; leaving it running");
                                }
                                Ok(Ok(crate::upgrade::Retired::Nothing)) => {
                                    tracing::info!(%instance, "an older runner process of this machine can be stopped; this one did not start it, so it is left to whoever did");
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
                        if let Some((pty, _)) = terms.get(&session) {
                            pty.write(data);
                        }
                    }
                    ServerMsg::TermResize { session, cols, rows } => {
                        if let Some((pty, state)) = terms.get(&session) {
                            pty.resize(cols, rows);
                            state.send(crate::termsess::Input::Resize { cols, rows });
                        }
                    }
                    ServerMsg::TermAttach { session } => {
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
                        terms.remove(&session);
                    }
                    ServerMsg::AiTranscript { req, pid } => match &ai {
                        Some(ai) => ai.transcript(req, pid, uplink.0.clone()),
                        None => {
                            uplink.0.send(Up::Term(AgentMsg::AiTranscript { req, json: None })).await.ok();
                        }
                    },
                    ServerMsg::HarnessOpen { session, json } => {
                        let opened = match (&harness, serde_json::from_str::<crate::harness::session::Spec>(&json)) {
                            (None, _) => Err(anyhow::anyhow!("agent sessions are not enabled on this runner")),
                            (_, Err(e)) => Err(anyhow::anyhow!("bad session spec: {e}")),
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
                                h.open(Some(session.clone()), spec).map(|_| ())
                            }
                        };
                        if let Err(e) = opened {
                            for u in crate::harness::host::refusal(&session, &json, &format!("{e:#}")) {
                                if let Ok(json) = serde_json::to_string(&u) {
                                    uplink.0.send(Up::Term(AgentMsg::Harness { session: session.clone(), json })).await.ok();
                                }
                            }
                        }
                    }
                    ServerMsg::HarnessCmd { session, json } => {
                        let known = harness.as_ref().is_some_and(|h| h.get(&session).is_some());
                        match serde_json::from_str::<crate::harness::model::Command>(&json) {
                            Ok(c) if known => {
                                if let Some(h) = &harness {
                                    h.command(&session, c);
                                }
                            }
                            Ok(_) => {
                                // Gone here (e.g. stopped while the server was away): say so.
                                if let Ok(json) = serde_json::to_string(&crate::harness::core::Update::Closed) {
                                    uplink.0.send(Up::Term(AgentMsg::Harness { session, json })).await.ok();
                                }
                            }
                            Err(e) => tracing::debug!(error = %e, "unreadable agent session command"),
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
                        if let Some(h) = &harness {
                            h.close(&session);
                        }
                        // What it had going live: its app, its streams.
                        crate::browser::apps::stop(&session);
                        let dir = o.state_dir.clone();
                        crate::rt::spawn(async move { crate::browser::stream::streams().close_session(&dir, &session).await });
                    }
                    ServerMsg::HarnessPackage { session, offset, data, done } => {
                        // A session going on here from the server's store (its machine is gone).
                        let Some(h) = harness.clone() else { continue };
                        match h.receive_package(&session, offset, &data, done) {
                            Ok(Some(path)) => {
                                let up = uplink.0.clone();
                                crate::rt::spawn(async move {
                                    let h2 = h.clone();
                                    let taken = crate::rt::blocking(move || h2.adopt_package(&path)).await.unwrap_or_else(|e| Err(format!("{e:#}")));
                                    if let Err(why) = taken {
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
                            Err(e) => tracing::warn!(%session, error = %format!("{e:#}"), "a session package couldn't be kept"),
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
                    ServerMsg::StreamControl { stream, session: _, json } => {
                        crate::rt::spawn(crate::browser::stream::serve_control(stream, json));
                    }
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
                    ServerMsg::DirList { req, path } => {
                        // The console's folder picker: this machine's folders, read off this loop.
                        let up = uplink.0.clone();
                        crate::rt::spawn(async move {
                            let json = crate::dirs::answer(path).await;
                            let _ = up.send(Up::Term(AgentMsg::DirList { req, json })).await;
                        });
                    }
                    ServerMsg::FrontDoor { json } => {
                        // Settings (and certificates) are never logged: an upload carries a key.
                        if let Some(fd) = &front_door {
                            fd.push(json);
                        }
                    }
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
                        let own = jobs.values().filter(|j| !j.pooled).count();
                        let full = !pooled && u32::try_from(own).unwrap_or(u32::MAX) >= o.slots.max(1);
                        if o.outpost || draining || upgrade_pending || full || exclusive || jobs.contains_key(&id) {
                            let reason = if o.outpost { OUTPOST_TAKES_NO_JOBS } else if draining { "draining" } else if upgrade_pending { "upgrading" } else { "no free slot" };
                            uplink.msg(AgentMsg::Reject { job: id, reason: reason.into() }).await;
                            continue;
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
                        jobs.insert(id.clone(), Running { cancel: cancel.clone(), pooled });
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
                        let leg = crate::runner::runtime::Leg { a, work_dir: o.work_dir.clone(), runner_name: name.clone(), up: uplink.clone(), cancel, http: http.clone(), warm: std::sync::Arc::new(parking_lot::Mutex::new(warm)) };
                        let done = done_tx.clone();
                        crate::rt::spawn(async move {
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
                        let Some(shell) = crate::runner::kept::shell(&job) else {
                            uplink.0.send(Up::Term(AgentMsg::TermExit { session, code: None, error: Some("the leg isn't running or kept on this runner".into()) })).await.ok();
                            continue;
                        };
                        let (program, env) = match (&shell.exec, &command) {
                            (Some(_), Some(c)) => shell.exec_with(c).map_or((None, Vec::new()), |(argv, env)| (Some(argv), env)),
                            (Some((argv, env)), None) => (Some(argv.clone()), env.clone()),
                            (None, _) => (None, shell.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
                        };
                        let env: Vec<(&str, String)> = env.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                        let cwd = program.is_none().then_some(shell.workdir.as_path());
                        // A command runs as a step would, in bash; a terminal opens the runner's shell.
                        let sh = if command.is_some() && !cfg!(windows) { Some("bash") } else { o.shell.as_deref() };
                        let cmd = command.as_deref().filter(|_| program.is_none());
                        match terminal::spawn_at(sh, cmd, program.as_deref(), cwd, cols, rows, &env) {
                            Ok((pty, rx)) => {
                                let tx = term_tx.clone();
                                let s = session.clone();
                                let mut masker = kiln_ci::runner::mask::Masker::new(&shell.masks);
                                crate::rt::spawn(async move {
                                    // The masker holds back what could be the start of a secret (as
                                    // long as the leg's longest, a token of a kilobyte); a terminal
                                    // shows the rest, masked, as soon as output pauses.
                                    loop {
                                        let out = match crate::rt::timeout(std::time::Duration::from_millis(25), rx.recv()).await {
                                            Ok(Ok(out)) => out,
                                            Ok(Err(_)) => break,
                                            Err(_) => {
                                                let held = masker.finish();
                                                if !held.is_empty() && tx.send((s.clone(), PtyOut::Data(held))).await.is_err() {
                                                    break;
                                                }
                                                continue;
                                            }
                                        };
                                        let (out, exit) = match out {
                                            PtyOut::Data(d) => (PtyOut::Data(masker.push(&d)), false),
                                            PtyOut::Exit(code) => {
                                                let rest = masker.finish();
                                                if !rest.is_empty() && tx.send((s.clone(), PtyOut::Data(rest))).await.is_err() {
                                                    break;
                                                }
                                                (PtyOut::Exit(code), true)
                                            }
                                        };
                                        let empty = matches!(&out, PtyOut::Data(d) if d.is_empty());
                                        if (!empty && tx.send((s.clone(), out)).await.is_err()) || exit {
                                            break;
                                        }
                                    }
                                });
                                let state = crate::termsess::Session::detached(session.clone(), format!("leg:{job}"), None, cols, rows);
                                terms.insert(session, (pty, state));
                            }
                            Err(e) => {
                                uplink.0.send(Up::Term(AgentMsg::TermExit { session, code: None, error: Some(format!("{e:#}")) })).await.ok();
                            }
                        }
                    }
                    // A kept leg let go now, or kept longer (W17-R02).
                    ServerMsg::KeepRelease { job } => {
                        crate::rt::spawn(async move {
                            crate::runner::kept::release(&job).await;
                        });
                    }
                    ServerMsg::KeepExtend { job, minutes } => {
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
                        if !crate::runner::runtime::rerun_step(&job, step, idx) {
                            tracing::info!(leg = %job, "a step rerun for a leg that isn't kept here");
                        }
                    }
                    // The rest of CI's messages (W09, W13) land with their workstreams.
                    ServerMsg::SecretsFor { .. } => {
                        tracing::debug!("ignoring a CI message this runner does not take yet");
                    }
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
                uplink.0.send(Up::Term(msg)).await.ok();
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
                // Logs and jobs' messages wait for the Welcome too, and come after what waited.
                if connected && (welcomed || (lane != LANE_LOGS && lane != LANE_CONTROL)) {
                    match &delay_line {
                        Some(d) => d.send(lane, kind, data),
                        None => {
                            net.send(SERVER_PEER, lane, kind, data).await;
                        }
                    }
                } else if matches!(&up, Up::Msg(AgentMsg::JobFinished(r)) if unreceived.contains_key(&r.job)) || (receipts && journaled.is_some()) {
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
                // Not kept across a disconnect: the reconnect sends every session whole.
                if connected {
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
                    tracing::info!(job = %r.job, status = r.status.as_str(), "job finished");
                    if jobs.is_empty() {
                        exclusive = false;
                    }
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
                if connected {
                    let s = sampler.sample();
                    uplink.msg(AgentMsg::Sample(s)).await;
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
                    // A harness installed (or removed) since: say so.
                    harness_kinds_tick = harness_kinds_tick.wrapping_add(1);
                    if let Some(h) = &harness
                        && harness_kinds_tick.is_multiple_of(12)
                    {
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


/// Most of one agent-session update a frame carries. They go on the interactive lane, which
/// refuses frames past 1 MiB ([`net::Kiln`]); a resumed session's history can be several.
const HARNESS_FRAME: usize = 512 * 1024;

/// An agent-session update as frames that fit: a long run of events goes in parts (events are
/// upserts by id, so the parts mean what the whole did), and an event too large on its own has
/// its longest texts cut, saying so. Anything else too large (a state, the changeset) is cut the
/// same way: dropped, its page would go on showing what no longer is, the agent looking stuck.
/// Only what still doesn't fit is dropped, and logged.
fn harness_frames(session: &str, u: &crate::harness::core::Update) -> Vec<String> {
    use crate::harness::core::Update;
    let Ok(json) = serde_json::to_string(u) else { return Vec::new() };
    if json.len() <= HARNESS_FRAME {
        return vec![json];
    }
    let Update::Events { events } = u else {
        let fitted = serde_json::to_value(u).ok().map(|v| cut_value(v, HARNESS_FRAME - 1024)).and_then(|v| serde_json::to_string(&v).ok());
        return match fitted {
            Some(j) if j.len() <= HARNESS_FRAME => {
                tracing::debug!(session, bytes = json.len(), sent = j.len(), "agent session update cut to fit");
                vec![j]
            }
            _ => {
                tracing::warn!(session, bytes = json.len(), "agent session update too large to send, even cut; dropped");
                Vec::new()
            }
        };
    };
    let one = HARNESS_FRAME - 1024;
    let mut frames = Vec::new();
    let mut part = Vec::new();
    let mut size = 0;
    for e in events {
        let mut e = e.clone();
        if json_len(&e) > one {
            e = cut(e, one);
        }
        let n = json_len(&e).saturating_add(1);
        if n > one {
            tracing::warn!(session, event = %e.id, bytes = n, "agent session event too large to send, even cut; dropped");
            continue;
        }
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

fn json_len<T: Serialize>(v: &T) -> usize {
    serde_json::to_vec(v).map_or(usize::MAX, |b| b.len())
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
    use crate::harness::core::Update;
    use crate::harness::model::{Body, Event};

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
}
