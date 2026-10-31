//! `kiln agent`: runs jobs on this machine (bare metal, a bench box, or a Kubernetes pod).
//!
//! * Enrolls once with a join token: generates its key locally, sends a CSR, stores the signed
//!   certificate. Afterwards it authenticates with mutual TLS only.
//! * Keeps one QUIC session to the server (hyper-transport), reconnecting with backoff forever.
//!   Jobs keep running while the server is unreachable; their output and results wait in a
//!   bounded uplink buffer and are delivered after reconnecting.
//! * Serves PTY terminals so the UI can drive this machine.

pub(crate) mod retained;
pub use retained::Receipt as RetainedReceipt;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
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
use crate::proto::{self, AgentMsg, Hello, JobResult, LANE_CONTROL, LANE_CONVO, LANE_FILES, LANE_LOGS, LANE_OBSERVE, LANE_PORTS, LANE_REACH, LANE_STREAM, LANE_SYNC, LANE_TERM, ServerMsg};
use crate::wire::payload::{Carried, FromServer, Waits};
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
    /// U05: the in-process runner shares its host's sole SQL/Records owner.
    pub shared_store: Option<crate::store::Store>,
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
    /// Observe (`crate::observe`): whether this runner keeps a replica of the observe group and
    /// writes into it (off in a job pod and in the server's own in-process agent).
    pub observe: bool,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_bootstrap: Option<ExecutorBootstrap>,
}

/// U03: enrollment's server-verified pod identity, bound to the issued certificate. This is
/// input to the inherited-owner factory, never authority derived from environment variables.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutorBootstrap {
    pub identity: crate::k8s::ExecutorIdentity,
    pub certificate_sha256: String,
}

impl ExecutorBootstrap {
    /// U03: the caller holds this credential-verified bootstrap. Assignment data alone cannot
    /// select inherited ownership, and a mismatched parent never falls back to native execution.
    pub(crate) fn execution_for(&self, admission: crate::runner::processes::Admission, assignment: &kiln_ci::wire::AssignmentV2, work: PathBuf) -> anyhow::Result<Arc<crate::runner::processes::Execution>> {
        let binding = assignment.parent_binding.as_ref().context("U03 verified pod assignment lacks its parent binding")?;
        let physical = self.identity.physical_execution()?;
        anyhow::ensure!(binding.executor == self.identity.executor && binding.generation == self.identity.generation
            && binding.allocation == self.identity.allocation && binding.physical_execution == physical
            && binding.job == assignment.job_id && binding.attempt == assignment.attempt && assignment.attempt > 0,
            "U03 assignment does not match the enrolled pod's immutable identity and existing grant");
        anyhow::ensure!(assignment.executor.slot.as_deref().is_none_or(|slot| slot == self.identity.executor), "U03 assignment selected another executor slot");
        let parent = crate::runner::processes::ParentExecution::verified(binding.clone(), &assignment.job_id, assignment.attempt)?;
        crate::runner::processes::Execution::inherited_in(admission, work, assignment.job_id.clone(), assignment.attempt, parent)
    }
}

/// U03: only this certificate's enrollment receipt may select inherited pod ownership.
pub(crate) fn enrolled_executor(state_dir: &Path, leaf: &Leaf) -> anyhow::Result<Option<ExecutorBootstrap>> {
    let path = state_dir.join("executor-bootstrap.json");
    use std::io::Read as _;
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let mut bytes = Vec::new();
    file.take(16385).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= 16384, "U03 executor enrollment receipt exceeds its bound");
    let bootstrap: Option<ExecutorBootstrap> = serde_json::from_slice(&bytes)?;
    if let Some(bootstrap) = &bootstrap {
        anyhow::ensure!(crate::pki::cert_info(&leaf.cert_pem)?.sha256 == bootstrap.certificate_sha256
            && crate::pki::leaf_name(leaf).as_deref() == Some(bootstrap.identity.name.as_str()), "U03 executor bootstrap does not belong to this certificate");
        for field in [&bootstrap.identity.namespace, &bootstrap.identity.name, &bootstrap.identity.uid, &bootstrap.identity.node,
            &bootstrap.identity.executor, &bootstrap.identity.generation, &bootstrap.identity.allocation] {
            anyhow::ensure!(!field.is_empty() && field.len() <= 512 && !field.chars().any(char::is_control), "U03 invalid executor bootstrap identity");
        }
    }
    Ok(bootstrap)
}

fn ca_der(pem: &str) -> anyhow::Result<Vec<u8>> {
    use hyper_quic::rustls::pki_types::CertificateDer;
    use hyper_quic::rustls::pki_types::pem::PemObject as _;
    Ok(CertificateDer::from_pem_slice(pem.as_bytes()).map_err(|e| anyhow::anyhow!("ca pem: {e:?}"))?.to_vec())
}

/// Load stored credentials, enrolling (or renewing) when needed. Retries until it succeeds.
/// Shutdown, or (lingering, shutdown already said) the next 50 ms tick, to look again whether the
/// loop may end.
async fn shutdown_or_tick(shutdown: &Shutdown, lingering: bool) {
    if lingering {
        crate::rt::sleep(Duration::from_millis(50)).await;
    } else {
        shutdown.cancelled().await;
    }
}

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
                // Persist the certificate-bound identity before making the new certificate usable.
                crate::util::write_secret(&o.state_dir.join("executor-bootstrap.json"), &serde_json::to_vec(&e.executor_bootstrap)?, 0o600)?;
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
    /// U01: stale local CI execution fences reach even across transport reconnects.
    _admission: Option<std::fs::File>,
    cancel: Shutdown,
    /// U03: execution identity survives completion and cancellation races.
    attempt: Option<u32>,
    /// U03: a completed execution cancels its pending grace cleanup.
    grace_stop: Shutdown,
    /// U03: exact containment identity shared by runtime and grace cleanup.
    execution: Arc<crate::runner::processes::Execution>,
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

/// U03: keep the attempt in replay storage even when speaking to an old server.
fn completion_wire(data: Vec<u8>, fenced: bool) -> Vec<u8> {
    if !fenced && let Ok(AgentMsg::JobFinishedV2 { result, .. }) = proto::read_agent(&data) {
        return proto::encode(&AgentMsg::JobFinished(result)).unwrap_or(data);
    }
    data
}

/// U03: a stale receipt cannot discard the current attempt's replay envelope.
fn completion_receipt(data: &[u8], attempt: Option<u32>, fenced: bool) -> bool {
    match proto::read_agent(data) {
        Ok(AgentMsg::JobFinishedV2 { attempt: held, .. }) => attempt == Some(held) || (!fenced && attempt.is_none()),
        Ok(AgentMsg::JobFinished(_)) => attempt.is_none(),
        _ => false,
    }
}

/// U03: main-loop producers cannot await the bounded channel that this loop consumes.
/// At most one event's controls wait locally; pending controls force channel draining before
/// another event is selected. The hard bound rejects an oversized event without silent loss.
#[derive(Default)]
struct DeferredControl { pending: VecDeque<Up> }
impl DeferredControl {
    const LIMIT: usize = 64;

    fn enqueue(&mut self, tx: &async_channel::Sender<Up>, up: Up) -> anyhow::Result<()> {
        self.flush(tx)?;
        if self.pending.is_empty() {
            match tx.try_send(up) {
                Ok(()) => return Ok(()),
                Err(async_channel::TrySendError::Full(up)) => self.pending.push_back(up),
                Err(async_channel::TrySendError::Closed(_)) => anyhow::bail!("agent uplink is closed"),
            }
        } else {
            anyhow::ensure!(self.pending.len() < Self::LIMIT, "one agent event exceeds the deferred control budget");
            self.pending.push_back(up);
        }
        Ok(())
    }

    fn flush(&mut self, tx: &async_channel::Sender<Up>) -> anyhow::Result<()> {
        while let Some(up) = self.pending.pop_front() {
            match tx.try_send(up) {
                Ok(()) => {}
                Err(async_channel::TrySendError::Full(up)) => { self.pending.push_front(up); break; }
                Err(async_channel::TrySendError::Closed(_)) => anyhow::bail!("agent uplink is closed"),
            }
        }
        Ok(())
    }
}

/// How long a cancelled job has to end before the runner reports it over and lets it go.
const CANCEL_GRACE: Duration = Duration::from_secs(120);

/// One turn of the agent's event loop (computed inside `select!`, handled after it).
enum Ev {
    Shutdown,
    Net(Option<NetEvent>),
    Done(JobResult, Option<u32>),
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
    /// Completion of the one retained ordinary durability operation. It never runs children.
    Ordinary(Box<OrdinaryDone>),
    /// A periodic U01/U03 retry opportunity; its handler only changes the existing latches.
    OrdinaryRecovery,
    /// An exact buffered receiver ticket whose admission exclusion is already durable.
    OrdinaryInput(crate::wire::channel::OrdinaryTicket),
    /// This SAME local event has already entered the retained outbox.
    UpDurable(Up),
    /// A test build's fault: the connection drops (W22-AC04).
    ChaosDrop,
    /// A test build's fault: a frame held back, due now (W22-AC04).
    Delayed(crate::runner::chaos::Due),
}

// U01/U03: one retained operation, polled by every event-loop select. The future is kept
// here when another event wins; no task-per-frame or detached command continuation exists.
type OrdinaryIo = futures::future::BoxFuture<'static, OrdinaryDone>;
type Incoming = crate::wire::keep::OrdinaryIncomingState;
type OrdinaryTicket = crate::wire::channel::OrdinaryTicket;
type OrdinaryClosure = crate::wire::channel::OrdinaryClosure;
struct PendingOrdinary { ticket: OrdinaryTicket, job: Option<String>, cancelled: bool }
/// At most two typed continuations, paired with the SAME Receiver's urgent reserve.
/// Raw bytes remain solely in Receiver. A duplicate never captures a newer same-id session.
struct OrdinaryUrgent {
    origin:crate::net::Connection,nonce:String,stream:String,seq:u64,hash:String,
    session:String,target:Option<Arc<crate::harness::session::Session>>,command:crate::harness::model::Command,
}
fn complete_ordinary_urgent<T:crate::harness::host::Taker+?Sized>(receiver:&mut crate::wire::channel::Receiver,ticket:&OrdinaryTicket,target:&T,command:crate::harness::model::Command)->anyhow::Result<()> {
    anyhow::ensure!(matches!(command,crate::harness::model::Command::Interrupt|crate::harness::model::Command::ForceStop),"only typed emergency controls may use early dispatch");
    let refusal=(!target.hand(command)).then(||(crate::wire::channel::Refusal::Refused,"the originally captured session did not take its emergency control".into()));
    receiver.ordinary_effect_done(ticket,refusal)?;Ok(())
}
enum SpoolWork {
    Publish { up: Box<Up>, receipt: retained::Receipt },
    Ack(serde_json::Value),
    Sample { hints: Vec<retained::Receipt>, work: PathBuf, ordinary: bool, legacy: bool, records:Arc<dyn kiln_seams::records::Records>, nonce:String },
}
enum OrdinaryDone {
    Initialized { spool: retained::Spool, epoch: Result<String,String> },
    Registered { origin: crate::net::Connection, result: Box<anyhow::Result<Incoming>> },
    Admitted { ticket: OrdinaryTicket, execution: Option<Arc<crate::runner::processes::Execution>>, result: Box<anyhow::Result<Incoming>> },
    Closed { ticket: OrdinaryTicket, execution: Arc<crate::runner::processes::Execution>, reason: String, result: anyhow::Result<()> },
    Committed { closure: OrdinaryClosure, result: Box<anyhow::Result<Incoming>> },
    Spool { spool: retained::Spool, work: Box<SpoolWork>, result: anyhow::Result<Vec<AgentMsg>>, incoming:Box<Option<anyhow::Result<Option<Incoming>>>> },
}
async fn ordinary_completion(io: &mut Option<OrdinaryIo>) -> OrdinaryDone {
    match io.as_mut() { Some(io) => io.await, None => futures::future::pending().await }
}
// U01/U03: one loop owns recovery. Healthy ticks add no duplicate spool sampling.
fn recover_ordinary(paused: &mut bool, sample_due: &mut bool) {
    if std::mem::take(paused) { *sample_due = true; }
}
fn spool_io(mut spool: retained::Spool, work: SpoolWork) -> OrdinaryIo {
    async move {
        let result = async {
            match &work {
                SpoolWork::Publish { receipt, .. } => { spool.publish(receipt).await?; Ok(Vec::new()) },
                SpoolWork::Ack(value) => {
                    match value.get("kind").and_then(serde_json::Value::as_str) {
                        Some("ordinary_frontier") => spool.received_ordinary(&serde_json::from_value::<kiln_ci::wire::OrdinaryFrontierAckV1>(value.clone())?).await?,
                        Some(_) => anyhow::bail!("unknown retained acknowledgement kind"),
                        None if value.get("kind").is_some() => anyhow::bail!("malformed retained acknowledgement kind"),
                        None => spool.received(&serde_json::from_value::<retained::Receipt>(value.clone())?).await?,
                    }
                    Ok(Vec::new())
                },
                SpoolWork::Sample { hints, work, ordinary, legacy, .. } => {
                    for hint in hints { spool.publish(hint).await?; }
                    spool.import_stopped(work).await?;
                    let mut messages=Vec::new();
                    if *ordinary { for publication in spool.pending_ordinary().await? {
                        let job=publication.owner.job.clone();
                        let json=serde_json::to_string(&kiln_ci::wire::JobEventV2::OrdinaryExecutionPublication{publication})?;
                        messages.push(AgentMsg::JobEvent{job,json});
                    } }
                    if *legacy { for receipt in spool.pending().await? { messages.push(receipt.message()?); } }
                    Ok(messages)
                },
            }
        }.await;
        // Positive Gate import may advance this SAME incoming admission row. Return its
        // complete authoritative state before the event owner resumes another normal head.
        let incoming=match &work {
            SpoolWork::Sample{records,nonce,..}=>Some(crate::wire::keep::load_ordinary_incoming(&**records,nonce).await),
            _=>None,
        };
        OrdinaryDone::Spool { spool,work:Box::new(work),result,incoming:Box::new(incoming) }
    }.boxed()
}
fn close_ordinary_io(ticket:OrdinaryTicket, execution:Arc<crate::runner::processes::Execution>, reason:String) -> OrdinaryIo {
    async move {
        let held=Arc::clone(&execution);
        let result=crate::rt::blocking(move || held.close_ordinary_admission(true)).await.and_then(|result|result);
        OrdinaryDone::Closed{ticket,execution,reason,result}
    }.boxed()
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

/// U06/U17: keep each legacy revision space independent while sharing machine handovers.
fn runner_records_namespace(name: &str, instance: Option<&str>) -> String {
    use sha2::Digest as _;
    // Product names allow 64 lowercase ASCII bytes; instance ids themselves are arbitrary.
    // None and the explicit main instance address the same persistent scope and directory.
    let id = instance.unwrap_or(crate::upgrade::MAIN);
    let digest = hex::encode(sha2::Sha256::digest(format!("runner/{name}/{id}").as_bytes()));
    format!("runner.{}", digest.chars().take(56).collect::<String>())
}

fn runner_machine_namespace(name: &str) -> String {
    use sha2::Digest as _;
    let digest = hex::encode(sha2::Sha256::digest(name.as_bytes()));
    format!("machine.{}", digest.chars().take(56).collect::<String>())
}

fn agent_paths(o: &Opts, quic: &str) -> crate::tunnel::Paths {
    if o.ephemeral {
        crate::tunnel::Paths::new(&o.server_url, quic)
    } else {
        crate::tunnel::Paths::remembering(&o.server_url, quic, crate::upgrade::own_dir(&o.state_dir, o.instance.as_deref()).join("transport"))
    }
}

/// U15: the replicated runner has one upstream dial owner, active before SQL bootstrap and
/// throughout application startup. Every raw connection event reaches the carrier in order;
/// the application never reconstructs connectivity from a partial startup snapshot.
fn replicated_uplink(net: net::NetHandle, events: async_channel::Receiver<NetEvent>, mut paths: crate::tunnel::Paths, http: Arc<Client>, shutdown: Shutdown) -> async_channel::Receiver<NetEvent> {
    let (tx, rx) = async_channel::bounded(1);
    crate::rt::spawn(async move {
        let mut connected = false;
        let mut backoff = Duration::from_millis(500);
        let mut redial = Some(Instant::now());
        let mut dialing: Option<Instant> = None;
        let mut tick = Instant::now() + Duration::from_secs(1);
        loop {
            enum Next { Stop, Dial, Tick, Event(Result<NetEvent, async_channel::RecvError>) }
            let next = futures::select_biased! {
                () = shutdown.cancelled().fuse() => Next::Stop,
                () = tx.closed().fuse() => Next::Stop,
                () = crate::rt::sleep_until(tick).fuse() => Next::Tick,
                () = sleep_until_opt(redial).fuse() => Next::Dial,
                event = events.recv().fuse() => Next::Event(event),
            };
            match next {
                Next::Stop => break,
                Next::Dial => {
                    redial = None;
                    let target = futures::select_biased! {
                        () = shutdown.cancelled().fuse() => break,
                        target = paths.target(&http, Instant::now()).fuse() => target,
                    };
                    match target {
                        Ok(addr) => {
                            dialing = Some(Instant::now());
                            net.command(Out::Connect { addr }).await;
                        }
                        Err(error) => {
                            tracing::warn!(error = %format!("{error:#}"), "U15: cannot resolve replicated upstream");
                            redial = Some(Instant::now() + crate::sup::jitter(backoff));
                            backoff = (backoff * 2).min(Duration::from_secs(30));
                        }
                    }
                }
                Next::Tick => {
                    let now = Instant::now();
                    tick = now + Duration::from_secs(1);
                    if !connected && dialing.is_some_and(|at| now.saturating_duration_since(at) > DIAL_TIMEOUT) {
                        dialing = None;
                        net.command(Out::Rebuild).await;
                        redial = Some(if paths.dial_failed(now) { now } else { now + crate::sup::jitter(backoff) });
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                    if paths.tick(connected, now) {
                        connected = false;
                        dialing = None;
                        redial = Some(now);
                    }
                }
                Next::Event(Ok(event)) => {
                    let now = Instant::now();
                    match &event {
                        NetEvent::Connected { binding, .. } => {
                            paths.connected(*binding, now);
                            connected = true;
                            dialing = None;
                            redial = None;
                            backoff = Duration::from_millis(500);
                        }
                        NetEvent::Closed { .. } | NetEvent::Unreachable { .. } => {
                            let immediate = if connected { paths.disconnected(); false } else { paths.dial_failed(now) };
                            connected = false;
                            dialing = None;
                            redial = Some(if immediate { now } else { now + crate::sup::jitter(backoff) });
                            backoff = (backoff * 2).min(Duration::from_secs(30));
                        }
                        NetEvent::Frame { .. } => {}
                    }
                    // Keep one raw event in flight. The carrier buffers application startup
                    // independently, so waiting here cannot wait on SQL or the application loop.
                    let sent = futures::select_biased! {
                        () = shutdown.cancelled().fuse() => break,
                        sent = tx.send(event).fuse() => sent,
                    };
                    if sent.is_err() {
                        shutdown.cancel();
                        break;
                    }
                }
                Next::Event(Err(_)) => { shutdown.cancel(); break; }
            }
        }
    });
    rx
}

/// U05: changing an existing aggregate's authority requires bounded reconciliation and
/// old-producer drain, not an implicit import/merge into another host database.
#[derive(Debug)]
struct HostMigrationNeeded {
    retained: PathBuf,
    canonical: PathBuf,
}
impl std::fmt::Display for HostMigrationNeeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "U05 host database migration needed: retained authority {} differs from configured host {}; no database was opened or switched; quiesce producers and reconcile the preserved aggregate explicitly", self.retained.display(), self.canonical.display())
    }
}
impl std::error::Error for HostMigrationNeeded {}

/// Compare existing directory aliases without creating a destination or rebasing a relative
/// Config path. A missing directory remains relative to the same process CWD as server startup.
fn host_directory_identity(path: &Path) -> anyhow::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(path.canonicalize()?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(std::path::absolute(path)?),
        Err(e) => Err(e.into()),
    }
}

/// U05: a bounded read-only path preflight. Existing aggregate authority is never treated as
/// a generic legacy table import. This is not a concurrent-producer retirement proof.
fn configured_host_database(config: &crate::config::Config, state: &Path, work: &Path) -> anyhow::Result<PathBuf> {
    let canonical = host_directory_identity(&config.server.data_dir)?;
    let mut roots = crate::server::legacy_conversation_sources(state)?.into_iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf)).collect::<Vec<_>>();
    let legacy_cache = work.join("_cas");
    roots.extend([work.to_path_buf(), legacy_cache.clone()]);
    // Preserve every source role before deduplicating aliases: `_cas` can point at a
    // state/instance root whose ledger marker must still be checked as cache authority.
    let mut retained = std::collections::BTreeMap::new();
    for root in roots {
        let identity = host_directory_identity(&root)?;
        let cache = root == legacy_cache;
        // The cache role is evidence even when its directory is the configured destination.
        // A retained directory/fence ledger is never absence of an importable SQLite file.
        if cache {
            let ledger = root.join("ledger.db");
            match std::fs::symlink_metadata(&ledger) {
                Ok(_) => if !std::fs::metadata(&ledger)?.is_file() {
                    return Err(HostMigrationNeeded { retained: ledger, canonical: config.server.data_dir.clone() }.into());
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        if identity == canonical { continue; }
        let (_, is_cache) = retained.entry(identity).or_insert((root, false));
        *is_cache |= cache;
    }
    let mut entries_seen = 0usize;
    for (root, is_cache) in retained.into_values() {
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("U05 inspect retained host authority {}", root.display())),
        };
        for entry in entries {
            entries_seen = entries_seen.checked_add(1).context("U05 retained authority inventory overflow")?;
            anyhow::ensure!(entries_seen <= 65_536, "U05 retained authority inventory exceeds its bound; all paths retained and startup refused");
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Include sidecars and interrupted aggregate cutovers. The raw SQLite ledger
            // remains an input to the existing bounded cache importer, not a second writer.
            let aggregate = name.starts_with("amphora.db") || name.starts_with("kiln.db") || name == "replica"
                || (name == "cluster" && (entry.file_type()?.is_symlink() || match std::fs::symlink_metadata(entry.path().join("cluster.log")) {
                    Ok(_) => true,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(error) => return Err(error.into()),
                }))
                || (is_cache && name == "ledger.db" && !std::fs::metadata(entry.path())?.is_file());
            if aggregate {
                return Err(HostMigrationNeeded { retained: entry.path(), canonical: config.server.data_dir.clone() }.into());
            }
        }
    }
    // Keep the exact configured relative-path semantics and the existing Store cutover API.
    Ok(config.server.data_dir.join("kiln.db"))
}

pub async fn run(o: Opts, shutdown: Shutdown) -> anyhow::Result<()> { run_in(o, shutdown, crate::runner::processes::Admission::production()).await }

pub(crate) async fn run_in(o: Opts, shutdown: Shutdown, admission_paths: crate::runner::processes::Admission) -> anyhow::Result<()> {
    anyhow::ensure!(o.instance.as_deref() != Some(crate::upgrade::MAIN), "U17: instance name main is reserved for the default runner; omit --instance (retained instance files are not moved)");
    // U05: resolve local host configuration once, before creating any startup state or
    // opening either database. Enrollment/work/instance directories retain their old meaning.
    let standalone_host = if o.shared_store.is_none() {
        let (state, work) = (o.state_dir.clone(), o.work_dir.clone());
        let config_path = o.config.clone().unwrap_or_else(crate::config::default_config_path);
        Some(crate::rt::blocking(move || -> anyhow::Result<_> {
            let config = crate::config::Config::load(&config_path)?;
            let legacy = configured_host_database(&config, &state, &work)?;
            Ok((legacy, config.cluster.replica))
        }).await??)
    } else { None };
    crate::os::start_reaper();
    crate::updates::warm_own_binary().await;
    // A Mac set up for kiln signing: launchd's re-sign watcher as this kiln writes it.
    if cfg!(target_os = "macos") && !o.ephemeral {
        std::thread::spawn(crate::os::macsign::keep_watching);
    }
    std::fs::create_dir_all(&o.work_dir).with_context(|| format!("create {}", o.work_dir.display()))?;
    if let Some(max) = o.declared.cache_max.as_deref().filter(|m| crate::runner::cache::parse_size(m).is_none()) {
        tracing::warn!(cache_max = max, "[agent] cache_max isn't a size (`20G`, `512M`): kiln/cache keeps up to 10G");
    }
    let (ca, leaf, enrolled_quic) = credentials(&o, &shutdown).await?;
    let executor_bootstrap = enrolled_executor(&o.state_dir, &leaf)?;
    // U03: only authenticated pod enrollment can bypass native host containment recovery.
    if executor_bootstrap.is_none() {
        let orphans = crate::runner::processes::reap_in(&o.work_dir, &admission_paths).await?;
        if orphans > 0 {
            tracing::warn!(orphans, "killed job processes left over from a previous agent run");
        }
    }
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
    // U15: replication is explicit configuration, never inferred from a server capability.
    // The server's in-process runner already has the host owner and its carrier.
    let replica = standalone_host.as_ref().and_then(|(_, replica)| replica.clone());
    if let Some(config) = &replica {
        config.validate()?;
        anyhow::ensure!(config.id != config.hub, "U15: standalone runner replica id must differ from the declared hub id");
        anyhow::ensure!(config.source(&name, config.id), "U15: authenticated runner {name} does not match the configured route for replica {}", config.id);
    }
    let replicated_transport = replica.is_some();
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
    // This machine's headroom (docs/headroom.md): started before any agent session resumes, so a
    // restart's sessions come back a few at a time; it runs from the settings it kept, server or not.
    let headroom = (!o.ephemeral && !o.outpost)
        .then(|| crate::headroom::start(crate::headroom::Ctx { dir: o.state_dir.join("headroom"), server_url: o.server_url.clone(), data_dir: o.state_dir.clone() }));
    // What the server last heard of it, and whether this server takes it.
    let mut headroom_sent: Option<String> = None;
    let mut server_headroom = false;
    let (net, events) = net::start(net::Spec { bind: "0.0.0.0:0".parse()?, ca_der: ca.clone(), leaf, server: false, dial_deadline: net::DIAL_DEADLINE, revoked_keys: Vec::new() })?;
    // A test build's faults: frames lost, sent twice or sent late below the channel, which it
    // makes up for.
    if let Some(c) = crate::runner::chaos::get() {
        net.command(net::Out::Seed(c.seed)).await;
        for rule in c.faults.iter().copied() {
            net.command(net::Out::Shed(rule.past(crate::wire::agent_classes().len()))).await;
        }
        // And what it sends, copied for a test to read (W27-R02).
        if let Some(path) = &c.copy {
            net.command(net::Out::Copy(path.clone())).await;
        }
    }
    // UDP to the QUIC address, or the HTTP tunnel when UDP gets no answer (see `crate::tunnel`).
    let mut paths = agent_paths(&o, &quic_addr);

    let (host_store, events, carrier_active) = if let Some(config) = replica {
        let legacy = standalone_host.as_ref().map(|(path, _)| path.clone()).context("U05 missing configured standalone host database")?;
        let log_path = legacy.parent().context("U05 configured host database has no directory")?.join("replica");
        let opening = config.clone();
        let database = crate::rt::blocking(move || crate::server::open_replica(&log_path, &legacy, &opening)).await??;
        // One dial owner for the whole lifetime, including quorum-dependent startup. Its event
        // stream preserves connection transitions for both the carrier and application loop.
        let incoming = replicated_uplink(net.clone(), events, agent_paths(&o, &quic_addr), http.clone(), shutdown.clone());
        let (events, active) = crate::replication::carrier(database.clone(), config, net.clone(), incoming, shutdown.clone())?;
        crate::replication::wait_for_writer(&database, &shutdown).await?;
        let legacy = standalone_host.as_ref().map(|(path, _)| path.clone()).context("U05 missing configured standalone host database")?;
        let store = crate::rt::blocking(move || crate::store::Store::from_database_with_legacy(database, &legacy)).await??;
        (store, events, active)
    } else {
        let store = match &o.shared_store {
            Some(store) => store.clone(),
            None => {
                let path = standalone_host.as_ref().map(|(path, _)| path.clone()).context("U05 missing configured standalone host database")?;
                crate::rt::blocking(move || crate::store::Store::open(&path)).await??
            }
        };
        (store, events, Arc::new(std::sync::atomic::AtomicBool::new(false)))
    };
    let database = host_store.database();
    // This machine's front door (DNS, HTTPS, a .local name), when the server gives it one. It runs
    // from the settings it stored at once, and keeps running while the server is away.
    let front_door = if !o.ephemeral && !o.outpost {
        Some(crate::frontdoor::runner::start(
            crate::frontdoor::runner::Ctx { dir: o.state_dir.join("front-door"), server_url: o.server_url.clone(), ca_der: ca.clone() },
            Arc::new(crate::records::from_database(&database, &runner_machine_namespace(&name))?),
            &shutdown,
        ).await?)
    } else { None };
    // What the server last heard of it on this connection.
    let mut front_door_sent: Option<String> = None;
    let ordinary_records: Arc<dyn kiln_seams::records::Records> = Arc::new(crate::records::from_database(&database, &runner_machine_namespace(&name))?);
    let spool = retained::Spool::new_in(Arc::clone(&ordinary_records), admission_paths.clone());
    let mut retained_spool = None;
    // Initialize the SAME producer before strict readiness, without holding session ingress.
    // Physical pre-feature drain and old-writer quiescence remain external prerequisites.
    let (inventory,records)=(admission_paths.clone(),Arc::clone(&ordinary_records));
    let mut ordinary_io: Option<OrdinaryIo> = Some(async move {
        let epoch = async {
            let pristine=crate::rt::blocking(move || crate::runner::processes::ordinary_inventory_present(&inventory).map(|present|!present)).await??;
            let present=crate::wire::keep::ordinary_incoming_present(&*records).await?;
            spool.initialize_ordinary(&crate::util::new_id(),pristine && !present).await
        }.await.map_err(|error:anyhow::Error|format!("{error:#}"));
        OrdinaryDone::Initialized{spool,epoch}
    }.boxed());
    let mut ordinary_epoch: Result<String,String> = Err("ordinary producer initialization pending".into());
    let mut ordinary_incoming: Option<Incoming> = None;
    let mut ordinary_stream: Option<String> = None;
    // One failed completion may retry only on the periodic recovery opportunity. Unknown
    // admission keeps the exact Gate separately and never retries a fresh creation.
    let mut ordinary_paused=false;
    let mut ordinary_pending: Option<PendingOrdinary> = None;
    let mut ordinary_answer: Option<OrdinaryTicket> = None;
    let mut ordinary_urgents=VecDeque::<OrdinaryUrgent>::new();
    let mut spool_publication_pending=false;
    let mut ordinary_dispatch: Option<OrdinaryTicket> = None;
    let mut ordinary_execution: Option<Arc<crate::runner::processes::Execution>> = None;
    // Unknown preparation keeps its exact Execution/Gate, never a new attempt or NoStart.
    let mut ordinary_unknown: Option<(OrdinaryTicket,Option<Arc<crate::runner::processes::Execution>>)> = None;
    let mut spool_waits=VecDeque::<SpoolWork>::new();
    let mut spool_sample_due=false;
    let mut durable_up:Option<Up>=None;
    let mut ordinary_registered: Option<crate::net::Connection> = None;
    // Once advertised on a connection, strict input cannot fall back to volatile legacy
    // ACKs merely because local readiness later fences. Only actual route retirement clears it.
    let mut ordinary_strict:Option<crate::net::Connection>=None;
    let mut server_ordinary = false;
    let mut local_wide_caps = 0u64;
    crate::runner::work::service::set_home(&o.work_dir, host_store.clone(), &name, o.declared.cache_max.as_deref().and_then(crate::runner::cache::parse_size));
    // U07/U15: finish canonical metadata imports before Hello can advertise runner work.
    crate::runner::work::service::ensure_ready().await?;

    let (up_tx, up_rx) = async_channel::bounded::<Up>(8192);
    let mut deferred_control = DeferredControl::default();
    let mut retained_blocked: Option<(Up, Instant)> = None;
    // Repo sync: the copies of synced repos this machine holds (none in a job pod or an outpost).
    let sync = if !o.ephemeral && !o.outpost {
        // U17: clone path provenance shares this host's authenticated machine namespace.
        let clone_records = Arc::new(crate::records::from_database(&database, &runner_machine_namespace(&name))?);
        let (sync, said) = crate::sync::runner::Runner::start(&o.state_dir, &name, http.clone(), o.server_url.clone(), clone_records).await?;
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
        Some(sync)
    } else { None };
    // Observe: this machine's replica of the observe group and its writes (none in a job pod).
    let observe = (o.observe && !o.ephemeral).then(|| {
        let obs = crate::observe::Observe::start(crate::observe::Settings {
            dir: o.state_dir.join("observe"),
            role: crate::observe::Role::Runner { me: name.clone(), http: http.clone(), server: o.server_url.clone() },
            // The server moves the cutoffs and states the bound.
            metrics_retention: Duration::ZERO,
            logs_retention: Duration::ZERO,
            spans_retention: Duration::ZERO,
            max_series: 0,
        });
        crate::observe::api::set_global(obs.clone());
        // Prometheus's scrapes of the targets the server says this machine reads.
        crate::observe::scrape::start(&name, &name, http.clone(), shutdown.clone());
        // The synthetic tests the server says this machine runs (it is their location by name).
        crate::observe::synthetics::start(&name, shutdown.clone());
        // The DogStatsD listeners the server says this machine opens.
        crate::observe::statsd::start(&name, &name, shutdown.clone());
        // The logs the server says this machine collects (its files, its journal).
        crate::observe::collect::start(&name, &name, &o.state_dir.join("observe"), shutdown.clone());
        // This machine's own figures, as the server's are.
        let every = std::env::var("KILN_OBSERVE_HOST_SECONDS").ok().and_then(|v| v.parse::<u64>().ok()).map_or(crate::observe::host::EVERY, Duration::from_secs);
        crate::observe::host::start(obs.clone(), name.clone(), "runner", every);
        // Every container this machine runs, as Datadog's container check sees them.
        crate::observe::containers::start(obs.clone(), name.clone(), every);
        // What the records this machine takes come to, as Datadog's estimated usage counts it.
        crate::observe::indexes::start_usage(obs.clone(), name.clone(), crate::observe::indexes::USAGE_EVERY);
        // And its own registry (`crate::metrics`: what this process counts, its repositories' sync
        // and its moves among it), as the server's is read, of its host.
        crate::observe::kiln::start_registry(obs.clone(), name.clone(), every, Some(net.clone()));
        // Apps on this machine send OTLP here, as to a collector: OTLP/HTTP's port and OTLP/gRPC's,
        // each taking either; and Datadog's tracers, as to their Agent, on its port too.
        crate::observe::ddtrace::start(obs.clone(), name.clone(), shutdown.clone());
        for addr in [crate::observe::otlp::local_addr(), crate::observe::otlp::local_grpc_addr(), crate::observe::otlp::local_dd_addr()].into_iter().flatten() {
            crate::rt::spawn(crate::observe::otlp::serve_local(addr, obs.clone(), shutdown.clone()));
        }
        let (up, out) = (up_tx.clone(), obs.outgoing());
        crate::rt::spawn(async move {
            while let Ok(m) = out.recv().await {
                let msg = AgentMsg::Observe { json: crate::observe::wire::encode(&m.msg), bytes: m.bytes };
                if up.send(Up::Observe(msg)).await.is_err() {
                    break;
                }
            }
        });
        obs
    });
    // Reach: links to other runners, and the runs this machine does for them (none in a job pod).
    // Agent sessions other runners hand this one (a session moving here) are taken over by its
    // session host, once there is one (below).
    let (adopt_tx, adopt_rx) = async_channel::bounded::<crate::reach::node::Adoption>(4);
    let keeper_records: Arc<dyn kiln_seams::Records> = Arc::new(crate::records::from_database(&database, &runner_machine_namespace(&name))?);
    let reach = if let Some(accept) = o.reach.filter(|_| !o.ephemeral) {
        let cfg = crate::reach::node::Config { adopt: Some(adopt_tx.clone()), objects: None, ..crate::reach::node::Config::for_runner(&name, &o.state_dir, accept) };
        Some((Arc::clone(&cfg.using), cfg.mirrors.clone(), crate::reach::node::start_in(cfg, admission_paths.clone()).await))
    } else { None };
    let reach = reach.and_then(|(using, mirrors, started)| match started {
        Ok((handle, said)) => {
            crate::reach::node::set_global(handle.clone());
            // What other runners' trees left here, kept as people's rules say (docs/reach.md §5.1).
            crate::reach::keeper::start(handle.clone(), using, mirrors, keeper_records.clone());
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
    let (done_tx, done_rx) = async_channel::unbounded::<(JobResult, Option<u32>)>();
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
    // Frame origin belongs to this event owner; a delayed old TLS frame cannot revive a
    // command incarnation after its receiver state was reclaimed.
    let mut server_connection: Option<crate::net::Connection> = None;
    // The server has taken this connection's Hello (it said Welcome). Logs, on a lane of their own,
    // wait for that, or the server, not knowing whose they are yet, drops them; and jobs' messages
    // with them, to come after what was held.
    let mut welcomed = false;
    let mut dial_backoff = Duration::from_millis(500);
    let mut redial: Option<Instant> = (!replicated_transport).then(Instant::now);
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
    // Process identity survives transport reconnects, never a process restart or install/PID reuse.
    let process_nonce = crate::util::random_token();
    let pool_execs = crate::runner::pool::Executors::for_process_in(process_nonce.clone(), admission_paths.clone());
    crate::runner::pool::sweep(&o.work_dir);
    // And the containers and networks this runner left running when it last stopped.
    crate::runner::docker::sweep(&o.work_dir).await?;
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
    // U16: upgrades are explicit CI/CD commands. Legacy terminal handovers are still
    // adopted, but a changed executable pathname never selects or activates a runner build.
    let standalone = crate::supervise::subcommand(&std::env::args().skip(1).collect::<Vec<_>>()) == Some("agent");
    adopt_handover(&o, &mut terms, &term_tx);
    #[cfg(unix)]
    let mut activation: Option<crate::bluegreen::Supervisor> = None;
    // AI coding sessions running on this machine (job pods have none), reported as they change.
    let ai = (!o.ephemeral).then(|| crate::aisessions::Reporter::new(crate::os::home_dir()));
    // Kiln-native agent sessions on this machine (not in job pods, and not in the server's own
    // in-process agent: the server hosts those itself). They outlive the server connection and
    // this process's restarts: each continues its harness session.
    let (harness_tx, harness_rx) = async_channel::bounded::<(String, crate::harness::core::Update)>(8192);
    // Their updates over the channel, once the server says it keeps it (docs/wire.md): kept until
    // the server has them, and sent again after a reconnect from where it has them, never whole.
    let mut updates = crate::wire::uplink::Updates::new(format!("{:016x}", rand::random::<u64>()));
    // What each session's stream carried of its changeset, so a server that takes patches gets
    // only what changed.
    let mut sent_changes = SentChanges::default();
    // The server keeps the channel: it said so on this connection, before its Welcome.
    let mut server_wire = false;
    let mut server_shaped_commands = false;
    let mut server_activation: Option<crate::net::Connection> = None;
    let mut server_layout: Option<crate::net::Connection> = None;
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
    // The server issues identities (docs/ci.md W27-R01): this process's sessions' epochs, and
    // when each identity was last asked for and how long until it's asked for again.
    let mut server_identity = false;
    let mut session_epochs: HashMap<String, u64> = HashMap::new();
    let mut identity_asked: HashMap<String, (Instant, Duration)> = HashMap::new();
    // The server takes word that a frame of its couldn't be read here (`AgentMsg::Unread`): of each
    // kind its newer build sends once an hour on a connection, the rest folded into the next word
    // (`wire::unread::Told`).
    let mut server_unread = false;
    let mut told = crate::wire::unread::Told::default();
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
    // Shutting down, until when the loop runs on: the log sources' last writes come back over its
    // connection before they write their places down (a stop then repeats no record).
    let mut lingering: Option<Instant> = None;
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
    // U15: conversations share the host product database; no application feed ships records.
    let mut convo: Option<crate::convo::store::Store> = None;
    // How many agent sessions it has at work at once (docs/agents.md, "How many agents a machine
    // runs"): what was set for this machine from the server (kept here for its restarts), else its
    // config's, else kiln's default. What the server sets is taken as it says it, and the config is
    // read again as it changes.
    // U17: loading accepted policy must succeed before Host restores or starts queued sessions.
    let mut limit_policy = crate::harness::limit::RunnerPolicy::from_database(&database, &runner_machine_namespace(&name), &o.state_dir).await?;
    let mut limit_asked = limit_policy.asked();
    let mut limit_config = o.declared.max_agent_sessions;
    // What the server was told this machine holds, on this connection; and whether it takes that.
    let mut limit_said: Option<crate::harness::limit::Held> = None;
    let mut server_limit = false;
    let mut server_job_attempt = false;
    let mut server_retained_received = false;
    let harness = if !o.ephemeral && o.terminal {
        // U05/U17: Records, conversations and the work ledger share this host owner. Opening or
        // importing failure is a startup error; continuing empty would discard recovery state.
        crate::workspace::slates::init(&o.state_dir);
        let held = crate::harness::limit::held(limit_asked, limit_config);
        if held.from != crate::harness::limit::Source::Default {
            tracing::info!(max_sessions = held.max_sessions, from = ?held.from, "agent sessions: this machine has at most this many at work at once");
        }
        let limits = crate::harness::host::Limits { per_machine: held.max_sessions };
        let db = database.clone();
        let dir = own_dir.clone();
        let conversations_on = crate::convo::enabled();
        let conversation_sources = if conversations_on { crate::server::legacy_conversation_sources(&o.state_dir)? } else { Vec::new() };
        let conversation_host = name.clone();
        let scope = format!("runner/{name}/{instance_id}");
        let namespace = runner_records_namespace(&name, o.instance.as_deref());
        let machine_namespace = runner_machine_namespace(&name);
        let sink = harness_tx.clone();
        let hold = take_over_pending.is_some();
        // U15/U17: namespace import and Host restoration can wait for product quorum too;
        // keep them off the shard running the independent transport carrier.
        let (st, h) = crate::rt::blocking(move || -> anyhow::Result<_> {
            crate::records::import_host_namespace(&db, &dir.join(crate::records::FILE), &namespace)?;
            let storage = if conversations_on {
                Some(crate::records::SessionStorage::from_host_database(db.clone(), &conversation_host, &conversation_sources, crate::config::Conversations::default().upkeep())?)
            } else { None };
            let st = storage.as_ref().map(crate::records::SessionStorage::conversations);
            let sink = match &st {
                Some(store) => crate::convo::store::tee(sink, store.clone(), 8192),
                None => sink,
            };
            let machine: Arc<dyn kiln_seams::Records> = Arc::new(crate::records::from_database(&db, &machine_namespace)?);
            let h = match storage {
                Some(storage) => crate::harness::host::Host::try_with_session_namespace(sink, Some(dir.join("harness.json")), limits, crate::rt::clock(), storage.bind_namespace(&namespace, None)?, machine, &scope)?,
                None => {
                    let sessions: Arc<dyn kiln_seams::Records> = Arc::new(crate::records::from_database(&db, &namespace)?);
                    crate::harness::host::Host::try_with_namespaces(sink, Some(dir.join("harness.json")), limits, crate::rt::clock(), (sessions, machine), &scope)?
                }
            };
            if hold { h.hold(true); }
            h.try_restore_from(st.as_ref())?;
            Ok((st, h))
        }).await??;
        convo = st;
        Some(h)
    } else { None };
    // The harnesses its user's login shell finds are this machine's too
    // (`crate::harness::kinds::search_path`): that shell is asked now, and every ten minutes.
    if harness.is_some() {
        crate::rt::spawn(crate::reach::serve::login_env());
    }
    // U15: record sources enter the canonical product database; no application feed or mirror.
    if let (Some(st), Some(h)) = (convo.clone(), harness.clone())
        && let Err(e) = crate::convo::record::Recorder::start(st, move || h.record_sources())
    {
        tracing::warn!(error = %format!("{e:#}"), "the harnesses' records won't be kept in the conversation store");
    }
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
    // U01/U03: service this existing-runtime cadence even while healthy, so missed ticks
    // cannot accumulate behind ingress. First tick is immediate; failures never rearm it.
    let mut ordinary_recovery_tick = Interval::new(Duration::from_secs(5));
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
        deferred_control.flush(&uplink.0)?;
        if ordinary_registered.is_some() && ordinary_registered!=server_connection {
            if let Some(origin)=ordinary_registered.take() {wire_in.retire_ordinary(origin);ordinary_urgents.retain(|urgent|urgent.origin!=origin);}
            ordinary_stream=None;
            if let Some(pending)=&mut ordinary_pending {pending.cancelled=true;}
        }
        if let Some((stream, seq, refusal)) = answer_after.take() {
            if let Some(ticket)=ordinary_answer.take() {
                // The strict requirement and origin came from intake, never current mutable Caps.
                if ticket.stream()==stream && ticket.seq()==seq {
                    if let Some(execution)=ordinary_execution.take() {
                        let reason=refusal.map(|(_,why)|why).unwrap_or_else(||"the prepared ordinary Start was not consumed by its original handler".into());
                        ordinary_pending=Some(PendingOrdinary{job:None,ticket:ticket.clone(),cancelled:true});
                        ordinary_io=Some(close_ordinary_io(ticket,execution,reason));
                    } else if let Err(error)=wire_in.ordinary_effect_done(&ticket,refusal) {
                        tracing::warn!(error=%error,"strict effect remains owned without an ACK");
                    }
                }
            } else {
                let a=match refusal {Some((code,why))=>wire_in.refused(&stream,seq,code,why),None=>wire_in.took(&stream,seq)};
                deferred_control.enqueue(&uplink.0,Up::Term(AgentMsg::Wire(a)))?;
            }
        }
        if let (Some(origin),Some(stream))=(ordinary_registered,ordinary_stream.as_deref())
            && Some(origin)==server_connection
            && wire_in.ordinary_peek_urgent(origin,&process_nonce,stream)?.is_some()
        {
            for urgent in ordinary_urgents.iter().take(2).filter(|urgent|urgent.origin==origin && urgent.stream==stream) {
                let Some(target)=urgent.target.as_ref() else {continue;};
                if let Some(ticket)=wire_in.ordinary_urgent(origin,&urgent.nonce,&urgent.stream,urgent.seq)? {
                    if ticket.body_hash()!=urgent.hash {ordinary_paused=true;continue;}
                    if let Err(error)=complete_ordinary_urgent(&mut wire_in,&ticket,&**target,urgent.command.clone()) {
                        ordinary_paused=true;tracing::warn!(error=%format!("{error:#}"),"urgent disposition remains under its original receiver ticket");
                    }
                }
            }
        }
        // Start normal effects in order. Buffered later ordinary frames do not wait for a
        // three-second retransmit; urgent exact tickets remain selectable during this I/O.
        if ordinary_io.is_none() && ordinary_pending.is_none() && ordinary_unknown.is_none() && !ordinary_paused
            && spool_waits.is_empty() && !spool_sample_due
            && let (Some(origin),Some(stream))=(ordinary_registered,ordinary_stream.as_deref())
            && Some(origin)==server_connection
        {
            let ready=match wire_in.ordinary_ready(origin,&process_nonce,stream) {
                Ok(ready)=>ready,
                Err(error)=>{ordinary_paused=true;tracing::warn!(error=%error,"strict completion waits for bounded authoritative progress");None},
            };
            if let Some(closure)=ready {
                let records=Arc::clone(&ordinary_records);
                ordinary_io=Some(async move {
                    let result=crate::wire::keep::commit_ordinary_incoming(&*records,&closure).await;
                    OrdinaryDone::Committed{closure,result:Box::new(result)}
                }.boxed());
            } else if !ordinary_paused && let Some(ticket)=wire_in.ordinary_head(origin,&process_nonce,stream)? {
                let owner=match proto::read_server(ticket.body()) {
                    Ok(ServerMsg::AssignV2{json})=>serde_json::from_str::<kiln_ci::wire::AssignmentV2>(&json).ok().and_then(|a|a.ordinary_owner),
                    _=>None,
                };
                ordinary_pending=Some(PendingOrdinary{job:owner.as_ref().map(|owner|owner.job.clone()),ticket:ticket.clone(),cancelled:false});
                let (paths,work,records,nonce)=(admission_paths.clone(),o.work_dir.clone(),Arc::clone(&ordinary_records),ticket.nonce().to_owned());
                ordinary_io=Some(async move {
                    let mut execution=None;
                    let result=async {
                        if let Some(owner)=owner {
                            execution=Some(crate::rt::blocking(move || crate::runner::processes::Execution::ordinary_in(paths,work,owner)).await??);
                        }
                        let state=crate::wire::keep::accept_ordinary_incoming_state(&*records,&nonce,ticket.origin(),ticket.stream(),ticket.seq()).await?;
                        if let Some(execution)=&execution {
                            let held=Arc::clone(execution);
                            crate::rt::blocking(move || held.close_ordinary_admission(false)).await??;
                        }
                        Ok(state)
                    }.await;
                    OrdinaryDone::Admitted{ticket,execution,result:Box::new(result)}
                }.boxed());
            }
        }
        if ordinary_io.is_none() && ordinary_dispatch.is_none() && ordinary_answer.is_none() && ordinary_execution.is_none() && let Some(spool)=retained_spool.take() {
            if let Some(work)=spool_waits.pop_front() { ordinary_io=Some(spool_io(spool,work)); }
            else if spool_sample_due {
                spool_sample_due=false;
                let hints=pool_execs.closed_owner_hints().iter().map(|said|serde_json::to_string(said).map_err(anyhow::Error::from).and_then(|json|retained::Receipt::executor(&json)?.context("closed executor lacks receipt"))).collect::<anyhow::Result<Vec<_>>>()?;
                ordinary_io=Some(spool_io(spool,SpoolWork::Sample{hints,work:o.work_dir.clone(),ordinary:connected && welcomed && server_retained_received && ordinary_registered==server_connection && ordinary_registered.is_some(),legacy:connected && welcomed && server_retained_received,records:Arc::clone(&ordinary_records),nonce:process_nonce.clone()}));
            } else { retained_spool=Some(spool); }
        }
        if let Some(until) = lingering
            && ((crate::observe::collect::get().is_none_or(|c| c.stopped()) && crate::observe::last_written()) || Instant::now() >= until)
        {
            return Ok(());
        }
        // Ephemeral agents leave once their one job is done and everything is delivered: its
        // result received, or given 30 s (a server older than receipts never says).
        if o.ephemeral && served_dedicated && jobs.is_empty() && buffer.is_empty() && retained_blocked.is_none() && deferred_control.pending.is_empty() && up_rx.is_empty() && unreceived.values().all(|(at, _)| at.elapsed() > Duration::from_secs(30)) {
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
            // What its tracers' spans counted, written before it goes, as a stop's is.
            crate::observe::ddtrace::finish(Duration::from_secs(3)).await;
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
        let ev = if let Some(ticket)=ordinary_dispatch.take() { Ev::OrdinaryInput(ticket) } else if let Some(up)=durable_up.take() { Ev::UpDurable(up) } else if spool_publication_pending {
            futures::select_biased! {
                ()=shutdown_or_tick(&shutdown,lingering.is_some()).fuse()=>Ev::Shutdown,
                result=ordinary_completion(&mut ordinary_io).fuse()=>Ev::Ordinary(Box::new(result)),
                ()=ordinary_recovery_tick.tick().fuse()=>Ev::OrdinaryRecovery,
                ev=events.recv().fuse()=>Ev::Net(ev.ok()),
                ()=sample_tick.tick().fuse()=>Ev::Sample,
            }
        } else if let Some(retry_at) = retained_blocked.as_ref().map(|(_, at)| *at) {
            // U03: hold later local events behind a failed spool commit, but keep consuming
            // server receipts and replay ticks so a full spool can make progress.
            futures::select_biased! {
                () = shutdown_or_tick(&shutdown, lingering.is_some()).fuse() => Ev::Shutdown,
                result = ordinary_completion(&mut ordinary_io).fuse() => Ev::Ordinary(Box::new(result)),
                () = sleep_until_opt(Some(retry_at)).fuse() => match retained_blocked.take() {
                    Some((up, _)) => Ev::Up(up),
                    None => Ev::Watchdog,
                },
                () = ordinary_recovery_tick.tick().fuse() => Ev::OrdinaryRecovery,
                ev = events.recv().fuse() => Ev::Net(ev.ok()),
                () = sample_tick.tick().fuse() => Ev::Sample,
            }
        } else if deferred_control.pending.is_empty() {
            futures::select_biased! {
            () = shutdown_or_tick(&shutdown, lingering.is_some()).fuse() => Ev::Shutdown,
            result = ordinary_completion(&mut ordinary_io).fuse() => Ev::Ordinary(Box::new(result)),
            () = ordinary_recovery_tick.tick().fuse() => Ev::OrdinaryRecovery,
            ev = events.recv().fuse() => Ev::Net(ev.ok()),
            r = done_rx.recv().fuse() => match r { Ok((r, attempt)) => Ev::Done(r, attempt), Err(_) => Ev::Watchdog },
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
            }
        } else {
            // Free room without making retained durability block network urgency.
            futures::select_biased! {
                () = shutdown_or_tick(&shutdown, lingering.is_some()).fuse() => Ev::Shutdown,
                result = ordinary_completion(&mut ordinary_io).fuse() => Ev::Ordinary(Box::new(result)),
                () = ordinary_recovery_tick.tick().fuse() => Ev::OrdinaryRecovery,
                ev = events.recv().fuse() => Ev::Net(ev.ok()),
                up = up_rx.recv().fuse() => match up { Ok(up) => Ev::Up(up), Err(_) => Ev::Watchdog },
            }
        };
        match ev {
            Ev::OrdinaryRecovery => recover_ordinary(&mut ordinary_paused, &mut spool_sample_due),
            Ev::Ordinary(done) => {
                ordinary_io=None;
                match *done {
                    OrdinaryDone::Initialized{spool,epoch}=> {
                        retained_spool=Some(spool);ordinary_epoch=epoch;
                        if let Err(error)=&ordinary_epoch {tracing::warn!(error,"ordinary admission remains fenced; existing sessions retain their owners");}
                    },
                    OrdinaryDone::Registered{origin,result}=>match *result {
                        Ok(state)=> {
                            ordinary_incoming=Some(state);ordinary_paused=false;
                            if server_connection==Some(origin) && server_ordinary {
                                ordinary_registered=Some(origin);ordinary_strict=Some(origin);
                                deferred_control.enqueue(&uplink.0,Up::Term(AgentMsg::Caps{caps:local_wide_caps|proto::CAP_ORDINARY_EXECUTION_DEBT}))?;
                                deferred_control.enqueue(&uplink.0,Up::Term(AgentMsg::Wire(wire_in.have())))?;
                            }
                        },
                        Err(error)=>tracing::warn!(error=%format!("{error:#}"),"incoming binding not registered; strict readiness remains fenced"),
                    },
                    OrdinaryDone::Admitted{ticket,execution,result}=>match *result {
                        Ok(state)=> {
                            ordinary_incoming=Some(state);
                            let same=ordinary_pending.as_ref().is_some_and(|pending|pending.ticket.same_identity(&ticket)
                                && pending.ticket.body_hash()==ticket.body_hash());
                            let valid=same && !shutdown.is_cancelled() && Some(ticket.origin())==server_connection && ordinary_registered==Some(ticket.origin());
                            let cancelled=ordinary_pending.as_ref().is_some_and(|pending|pending.cancelled);
                            if valid && !cancelled && wire_in.ordinary_admitted(&ticket).is_ok() {
                                ordinary_execution=execution;ordinary_dispatch=Some(ticket);
                            } else if let Some(execution)=execution {
                                ordinary_io=Some(close_ordinary_io(ticket,execution,"the original pending Start was cancelled or its incoming authority retired".into()));
                            } else {ordinary_pending=None;}
                        },
                        Err(error)=> {
                            tracing::warn!(error=%format!("{error:#}"),"original admission remains unknown; no effect or wire answer is invented");
                            ordinary_unknown=Some((ticket,execution));ordinary_pending=None;
                        },
                    },
                    OrdinaryDone::Closed{ticket,execution,reason,result}=> {
                        match result {
                            Ok(())=> {
                                if Some(ticket.origin())==server_connection && ordinary_registered==Some(ticket.origin())
                                    && let Err(error)=wire_in.ordinary_effect_done(&ticket,Some((crate::wire::channel::Refusal::Refused,reason)))
                                {
                                    tracing::debug!(error=%error,"retired incoming ticket has durable NoStart evidence; no successor ACK or effect");
                                }
                            },
                            Err(error)=> {
                                tracing::warn!(error=%format!("{error:#}"),"NoStart remains unknown under the original Gate");
                                ordinary_unknown=Some((ticket,Some(execution)));
                            },
                        }
                        ordinary_pending=None;
                    },
                    OrdinaryDone::Committed{closure,result}=>match *result {
                        Ok(state)=> {
                            ordinary_incoming=Some(state);
                            if Some(closure.origin())==server_connection && ordinary_registered==Some(closure.origin()) {
                                match wire_in.ordinary_committed(&closure) {
                                    Ok(frame)=> {
                                        ordinary_urgents.retain(|urgent|urgent.origin!=closure.origin() || urgent.stream!=closure.has().stream || urgent.seq>closure.has().upto);
                                        deferred_control.enqueue(&uplink.0,Up::Term(AgentMsg::Wire(frame)))?;
                                    },
                                    Err(error)=>tracing::debug!(error=%error,"old incoming closure committed; retired opaque authority cannot ACK its successor"),
                                }
                            }
                        },
                        Err(error)=>{ordinary_paused=true;tracing::warn!(error=%format!("{error:#}"),"strict completion remains unacknowledged under its exact origin until the periodic recovery opportunity");},
                    },
                    OrdinaryDone::Spool{spool,work,result,incoming}=> {
                        let (work,incoming)=(*work,*incoming);
                        retained_spool=Some(spool);
                        if matches!(&work,SpoolWork::Publish{..}) {spool_publication_pending=false;}
                        if let Some(incoming)=incoming {
                            match incoming {
                                Ok(Some(state))=> {
                                    if let (Some(origin),Some(stream))=(ordinary_registered,ordinary_stream.as_deref()) {
                                        let refreshed=state.for_stream(origin,&process_nonce,stream).and_then(|(has,admitted)|wire_in.refresh_ordinary(origin,&process_nonce,has,admitted).map_err(anyhow::Error::from));
                                        if let Err(error)=refreshed {
                                            ordinary_paused=true;
                                            tracing::warn!(error=%format!("{error:#}"),"import changed incoming authority; strict readiness fenced before further effects");
                                            wire_in.retire_ordinary(origin);ordinary_urgents.retain(|urgent|urgent.origin!=origin);ordinary_registered=None;ordinary_stream=None;
                                            if let Some(pending)=&mut ordinary_pending {pending.cancelled=true;}
                                        }
                                    }
                                    ordinary_incoming=Some(state);
                                },
                                Ok(None) if ordinary_registered.is_some()=>{ordinary_paused=true;tracing::warn!("registered ordinary incoming metadata missing; no fresh state inferred");},
                                Ok(None)=>{},
                                Err(error)=>{ordinary_paused=true;tracing::warn!(error=%format!("{error:#}"),"incoming cache refresh unknown; normal heads remain held");},
                            }
                        }
                        match (work,result) {
                            (SpoolWork::Publish{up,..},Ok(_))=>durable_up=Some(*up),
                            (SpoolWork::Publish{up,..},Err(error))=> {
                                tracing::warn!(error=%format!("{error:#}"),"retained local event still awaits durable publication");
                                retained_blocked=Some((*up,Instant::now()+Duration::from_secs(1)));
                            },
                            (_,Ok(messages))=>for message in messages {
                                if let AgentMsg::JobEvent{json,..}=&message
                                    && matches!(serde_json::from_str::<kiln_ci::wire::JobEventV2>(json),Ok(kiln_ci::wire::JobEventV2::OrdinaryExecutionPublication{..}))
                                    && (ordinary_registered.is_none() || ordinary_registered!=server_connection) {continue;}
                                if let AgentMsg::Executor{json}=&message && let Ok(said)=serde_json::from_str::<crate::runner::pool::Said>(json) {pool_execs.recovered(&said);}
                                deferred_control.enqueue(&uplink.0,Up::Msg(message))?;
                            },
                            (_,Err(error))=>tracing::warn!(error=%format!("{error:#}"),"retained operation remains recoverable in its original owner"),
                        }
                    },
                }
            }
            Ev::Shutdown => {
                draining=true;
                if let Some(pending)=&mut ordinary_pending {pending.cancelled=true;}
                if lingering.is_some() {
                    continue;
                }
                for r in jobs.values() {
                    r.cancel.cancel();
                }
                // The browsers it drives closed before it goes.
                crate::browser::driven::stop_all().await;
                // What this machine collected and counted written before it goes: three seconds
                // at the most.
                let collecting = match crate::observe::collect::get() {
                    Some(c) if !c.stopped() => {
                        c.stop_signal();
                        true
                    }
                    _ => false,
                };
                if !collecting && crate::observe::last_written() {
                    return Ok(());
                }
                lingering = Some(Instant::now() + Duration::from_secs(3));
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
                    server_connection = None;
                    server_layout = None;
                    server_activation = None;
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
                            let sup = activation.clone().context(crate::updates::UpgradeRefusal::Backend)?;
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
                if replicated_transport { continue; }
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
                // Terminals into legs close once their leg ends, or is let go when kept (W17-AC01).
                let gone: Vec<String> = terms.iter().filter(|(_, (_, st))| st.target.strip_prefix("leg:").is_some_and(|job| !crate::runner::kept::live(job))).map(|(id, _)| id.clone()).collect();
                for session in gone {
                    terms.remove(&session);
                    uplink.0.send(Up::Term(AgentMsg::TermExit { session, code: None, error: Some("the leg ended".into()) })).await.ok();
                }
                // A dial that never resolves (no Connected / Closed / Unreachable) would leave the
                // agent waiting forever, e.g. after dialing before the network existed at boot.
                // Treat it as failed: rebuild the endpoint on a fresh socket and dial again.
                if !replicated_transport && !connected && dialing_since.is_some_and(|t| t.elapsed() > DIAL_TIMEOUT) {
                    tracing::warn!(server = %quic_addr, after = ?DIAL_TIMEOUT, "no answer from server; rebuilding transport and redialing");
                    dialing_since = None;
                    server_connection = None;
                    server_layout = None;
                    server_activation = None;
                    net.command(Out::Rebuild).await;
                    let now = Instant::now();
                    redial = Some(if paths.dial_failed(now) { now } else { now + crate::sup::jitter(dial_backoff) });
                    dial_backoff = (dial_backoff * 2).min(Duration::from_secs(30));
                }
                // Direct dials unanswered for too long: through the tunnel now. Tunnelled and UDP
                // answers again: drop this connection (what is sent meanwhile waits in the buffer)
                // and dial directly; the tunnel stays open until that connection is up.
                if !replicated_transport && paths.tick(connected, Instant::now()) {
                    connected = false;
                    server_connection = None;
                    server_layout = None;
                    server_activation = None;
                    welcomed = false;
                    if let Some(s) = &sync {
                        s.connected(false);
                    }
                    if let Some(ob) = &observe {
                        ob.up("", false);
                    }
                    if let Some(r) = &reach {
                        r.feed(crate::reach::node::In::Server(false)).await;
                    }
                    dialing_since = None;
                    redial = Some(Instant::now());
                }
            }
            Ev::Net(None) => anyhow::bail!("transport stopped"),
            Ev::Net(Some(NetEvent::Connected { peer, epoch, binding, .. })) => {
                if peer == SERVER_PEER {
                    if let Some(old)=ordinary_registered.take() {wire_in.retire_ordinary(old);ordinary_urgents.retain(|urgent|urgent.origin!=old);}
                    ordinary_stream=None;
                    if let Some(pending)=&mut ordinary_pending {pending.cancelled=true;}
                    server_connection = binding.map(|binding| crate::net::Connection::new(peer, epoch, binding));
                    ordinary_registered = None;ordinary_strict=None;ordinary_urgents.clear();
                    server_ordinary = false;
                    server_layout = None;
                    server_activation = None;
                }
                if replicated_transport {
                    tracing::info!(server = %quic_addr, "connected to server through the replicated upstream controller");
                } else {
                    tracing::info!(server = %quic_addr, via = %paths.via(), "connected to server");
                }
                chaos_drop = chaos.and_then(|c| c.drop_every).map(|d| Instant::now() + d);
                // How long saying what it is takes (its build's probes, its facts), told below: the
                // server's frames wait meanwhile.
                let said_from = Instant::now();
                if !replicated_transport { paths.connected(binding, Instant::now()); }
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
                // an outpost says it's one instead. Its binary's hash was worked out as this runner
                // started (`updates::warm_own_binary`), its engine's probes off the event loop.
                #[cfg(unix)]
                {
                    activation = if standalone && !o.ephemeral && !o.outpost {
                        crate::rt::blocking(crate::bluegreen::Supervisor::for_activation).await.ok().and_then(Result::ok)
                    } else { None };
                }
                let explicit_activation = {
                    #[cfg(unix)]
                    { activation.is_some() }
                    #[cfg(not(unix))]
                    { let _ = standalone; false }
                };
                let building = Instant::now();
                let mut build = if o.outpost { crate::updates::outpost_build() } else { crate::updates::runner_build().await };
                let build_took = building.elapsed();
                // U15: the legacy learner/feed bits are reserved, never advertised by this build.
                if let AgentMsg::Build { caps, .. } = &mut build {
                    *caps &= !(proto::CAP_CONVO | proto::CAP_CLUSTER | proto::CAP_BLUE_GREEN);
                    if explicit_activation { *caps |= proto::CAP_BLUE_GREEN; }
                    // Observe is advertised only while this runner owns its replica.
                    if observe.is_none() { *caps &= !proto::CAP_OBSERVE; }
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
                if !crate::runner::chaos::get().is_some_and(|c| c.old) {
                    let built = if let AgentMsg::Build { caps, .. } = &build { u64::from(*caps) } else { 0 };
                    let work = if o.outpost { 0 } else { proto::CAP_WORK };
                    let headroom_cap = if headroom.is_some() { proto::CAP_HEADROOM } else { 0 };
                    let limit = if harness.is_some() { proto::CAP_AGENT_LIMIT } else { 0 };
                    let open_once = if harness.is_some() { proto::CAP_OPEN_ONCE } else { 0 };
                    // The updated paths refuse work if strict containment/admission cannot be established.
                    let ownership = if !o.outpost && cfg!(any(target_os = "linux", target_os = "macos", windows)) { proto::CAP_RESOURCE_OWNERSHIP } else { 0 };
                    let firecracker_shape = if !o.outpost && cfg!(target_os = "linux") { proto::CAP_FIRECRACKER_SHAPE } else { 0 };
                    let amphora = if carrier_active.load(std::sync::atomic::Ordering::Acquire) { proto::CAP_AMPHORA } else { 0 };
                    local_wide_caps = built | proto::CAP_WIRE | proto::CAP_HANDOVER | proto::CAP_INBOX | proto::CAP_HARNESS_RELEASE | proto::CAP_JOB_ATTEMPT | proto::CAP_RETAINED_RECEIVED | ownership | firecracker_shape | open_once | work | limit | proto::CAP_IDENTITY | headroom_cap | amphora | proto::CAP_UNREAD | proto::CAP_UNIFIED_LAYOUT | if explicit_activation { proto::CAP_EXPLICIT_ACTIVATION } else { 0 };
                    if let Ok(b) = proto::encode(&AgentMsg::Caps { caps: local_wide_caps }) {
                        net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                    }
                }
                // The kiln it runs, so the server knows its commit (`crate::fleet`; outposts aren't
                // part of the fleet).
                let started = Instant::now();
                let facts = (!o.ephemeral && !o.outpost).then(crate::updates::runner_facts);
                let facts_took = started.elapsed();
                if let Some(Ok(b)) = facts.as_ref().map(proto::encode) {
                    net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                    // The rest, probed: they take a moment (docs/ci.md W13-R01).
                    probe_facts(&uplink, &o.work_dir, &o.declared, &probing);
                }
                // A reconnect changes transport only. The existing durable grant and ordered
                // executor owner retain every warm VM until an exact Stop proves it quiescent.
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
                if let Some(ob) = &observe {
                    ob.up("", true);
                }
                // How the front door does, after everything older (an older server skips it).
                front_door_sent = None;
                if let Some(json) = front_door.as_ref().and_then(crate::frontdoor::runner::Handle::latest)
                    && let Ok(b) = proto::encode(&AgentMsg::FrontDoorStatus { json: json.clone() })
                {
                    net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                    front_door_sent = Some(json);
                }
                // Its frames waited all that while (the server's Welcome, observe's join): a slow one said.
                let took = said_from.elapsed();
                let (took_ms, build_ms, facts_ms) = (took.as_millis() as u64, build_took.as_millis() as u64, facts_took.as_millis() as u64);
                if took > Duration::from_secs(2) {
                    tracing::warn!(took_ms, build_ms, facts_ms, "saying what this runner is took long: the server's frames waited");
                } else {
                    tracing::debug!(took_ms, build_ms, facts_ms, "said what this runner is to the server");
                }
            }
            Ev::Net(Some(NetEvent::Closed { peer, epoch, .. })) if server_connection.is_some_and(|current| current.peer() != peer || current.epoch() != epoch) => {},
            Ev::Net(Some(NetEvent::Closed { reason, .. } | NetEvent::Unreachable { reason, .. })) => {
                server_connection = None;
                    server_layout = None;
                    server_activation = None;
                let now = Instant::now();
                let mut at_once = false;
                if connected {
                    tracing::warn!(%reason, "disconnected from server");
                    if !replicated_transport { paths.disconnected(); }
                    // No word comes for agent sessions on their way to a newer process: they go on here.
                    crate::harness::handing::runner_link_lost();
                } else {
                    if replicated_transport {
                        tracing::info!(%reason, "replicated upstream dial failed; its controller will retry");
                    } else {
                        tracing::info!(%reason, via = %paths.via(), retry_in = ?dial_backoff, "dial failed");
                    }
                    // The path changed (to the tunnel, or back): dial that way now.
                    if !replicated_transport { at_once = paths.dial_failed(now); }
                }
                connected = false;
                welcomed = false;
                // The channel's items in flight go again once the server says what it has.
                updates.down();
                server_wire = false;
                server_drops = false;
                server_unread = false;
                told = crate::wire::unread::Told::default();
                drops_said = None;
                server_work = false;
                work_said = None;
                server_identity = false;
                server_limit = false;
                server_job_attempt = false;
                server_retained_received = false;
                limit_said = None;
                if let Some(s) = &sync {
                    s.connected(false);
                }
                if let Some(ob) = &observe {
                    ob.up("", false);
                }
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
            event @ (Ev::Net(Some(NetEvent::Frame { .. })) | Ev::Forwarded(_) | Ev::OrdinaryInput(_)) => {
                let (data, origin) = match event {
                    Ev::Net(Some(NetEvent::Frame { data, connection, .. })) => (data, connection),
                    Ev::Forwarded(data) => (data, None),
                    Ev::OrdinaryInput(ticket) => {
                        answer_after=Some((ticket.stream().into(),ticket.seq(),None));
                        ordinary_pending=None;
                        let pair=(ticket.body().to_vec(),Some(ticket.origin()));
                        ordinary_answer=Some(ticket);pair
                    },
                    _ => continue 'events,
                };
                let current_origin = origin.is_some() && origin == server_connection;
                if origin.is_some() && !current_origin { continue 'events; }
                let msg: ServerMsg = match proto::read_server(&data) {
                    Ok(m) => m,
                    // The server hears of it, when it takes word of it (docs/wire.md, "What can't
                    // be read"), and notes it against this runner.
                    Err(e) => {
                        if ordinary_answer.is_some() {refuse(&mut answer_after,e.code().refusal(),e.why("ServerMsg"));continue 'events;}
                        tracing::warn!(what = %e.what(), why = %e.why("ServerMsg"), "a frame from the server couldn't be read");
                        if server_unread && let Some(folded) = told.tell(Instant::now(), crate::wire::unread::Wire::Server, e.code(), &e.what(), &e.why("ServerMsg")) {
                            uplink.msg(AgentMsg::Unread { wire: crate::wire::unread::Wire::Server, what: e.what(), code: e.code(), why: e.why("ServerMsg"), folded, session: None }).await;
                        }
                        continue;
                    }
                };
                // A command on the channel (docs/wire.md, "Commands"): taken once, however often
                // it comes, then applied as if it had come alone, and answered at the top of the
                // next turn of the loop.
                if ordinary_answer.is_some() && matches!(msg,ServerMsg::Wire(_)) {
                    refuse(&mut answer_after,crate::wire::channel::Refusal::Malformed,"a channel frame inside a command");continue 'events;
                }
                let (msg, data) = if ordinary_answer.is_some() {(msg,data)} else { 'item: {
                    match msg {
                        ServerMsg::Wire(crate::wire::channel::Frame::Item { stream, seq, low, body }) => {
                            use crate::wire::channel::{Arrival, Refusal};
                            // Do not let an unproven/forwarded VM command alter Current or the
                            // receiver before its actual TLS producer has been authenticated.
                            if !current_origin && server_shaped_commands && crate::wire::commands::kind_of(&stream).is_some() { continue 'events; }
                            // Read once before either generation or receiver admission changes.
                            // An unbound/forwarded handshake or private item is dropped without
                            // evicting valid channel state or disconnecting its current TLS peer.
                            let inner = proto::read_server(&body);
                            if let Ok(message) = &inner
                                && let Err(reason) = server_layout_on(message, origin, server_connection, server_layout)
                            {
                                if !current_origin {
                                    tracing::debug!(reason, "unbound server channel item supplies no layout authority");
                                    continue 'events;
                                }
                                break 'item (ServerMsg::Refused { reason: reason.into() }, Vec::new());
                            }
                            if !current_origin
                                && let Ok(ServerMsg::Executor { json }) = &inner
                                && let Ok(ask) = crate::wire::payload::<crate::runner::pool::Ask>(crate::wire::unread::Wire::ServerPayload, "Executor", json)
                                && pool_execs.requires_order(&ask)
                            { continue 'events; }
                            let strict_ordinary = current_origin && ordinary_strict==origin && origin.is_some()
                                && crate::wire::commands::kind_of(&stream) == Some(crate::wire::commands::Kind::Live);
                            if let Ok(ServerMsg::AssignV2 { json }) = &inner {
                                #[derive(Deserialize)]
                                struct Header { #[serde(default)] ordinary_owner: Option<kiln_ci::wire::OrdinaryOwnerV1> }
                                if let Ok(header) = serde_json::from_str::<Header>(json) && let Some(owner) = header.ordinary_owner {
                                    let valid = owner.validate().is_ok() && strict_ordinary && owner.host == name && owner.process_nonce == process_nonce
                                        && origin.is_some_and(|origin| owner.connection == origin.channel_id()) && owner.command.stream == stream && owner.command.seq == seq
                                        && ordinary_epoch.as_ref().is_ok_and(|epoch| *epoch == owner.publication_epoch);
                                    if !valid { tracing::warn!("ordinary Start refused before admission: original owner/stamp/domain mismatch"); continue 'events; }
                                }
                            }
                            if strict_ordinary && let Some(actual)=origin {
                                use crate::wire::channel::OrdinaryArrival;
                                if ordinary_registered!=Some(actual) {continue 'events;}
                                if ordinary_stream.as_deref()!=Some(stream.as_str()) {
                                    let Some(state)=&ordinary_incoming else {continue 'events;};
                                    let (has,admitted)=match state.for_stream(actual,&process_nonce,&stream) {
                                        Ok(state)=>state,Err(error)=>{tracing::warn!(error=%format!("{error:#}"),"strict stream lacks cached authoritative admission");continue 'events;},
                                    };
                                    match wire_current.of(&stream) {
                                        crate::wire::commands::Stand::Gone=>continue 'events,
                                        crate::wire::commands::Stand::Newer(_)|crate::wire::commands::Stand::Current=>{},
                                    }
                                    wire_in.retire_ordinary(actual);ordinary_urgents.retain(|urgent|urgent.origin!=actual);
                                    if let Some(pending)=&mut ordinary_pending {pending.cancelled=true;}
                                    if let Err(error)=wire_in.bind_ordinary(actual,&process_nonce,has,admitted) {
                                        ordinary_paused=true;tracing::warn!(error=%error,"strict receiver binding remains fenced");continue 'events;
                                    }
                                    ordinary_stream=Some(stream.clone());
                                }
                                let typed=match &inner {
                                    Ok(ServerMsg::HarnessCmd{session,json})=>serde_json::from_str::<crate::harness::model::Command>(json).ok().filter(|command|matches!(command,crate::harness::model::Command::Interrupt|crate::harness::model::Command::ForceStop)).map(|command|(session.clone(),command)),
                                    _=>None,
                                };
                                let capture=typed.as_ref().filter(|_|!ordinary_urgents.iter().any(|urgent|urgent.origin==actual && urgent.stream==stream && urgent.seq==seq)).map(|(session,command)|OrdinaryUrgent{
                                    origin:actual,nonce:process_nonce.clone(),stream:stream.clone(),seq,hash:crate::util::sha256_hex(&body),session:session.clone(),target:harness.as_ref().and_then(|host|host.get(session)),command:command.clone(),
                                });
                                let staged=match if typed.is_some() {wire_in.stage_ordinary_urgent(actual,&process_nonce,&stream,seq,low,body)} else {wire_in.stage_ordinary(actual,&process_nonce,&stream,seq,low,body)} {
                                    Ok(staged)=>staged,
                                    Err(error)=>{tracing::warn!(error=%error,"strict input was not admitted; existing sender retains it");continue 'events;},
                                };
                                if matches!(&staged,OrdinaryArrival::Queued|OrdinaryArrival::Gap(_)) && let Some(capture)=capture {
                                    // Core bounds TWO urgent items; this holds only their typed target
                                    // custody, never another raw queue or a newest-target lookup.
                                    anyhow::ensure!(ordinary_urgents.len()<2,"urgent continuation reserve diverged from its SAME receiver");
                                    ordinary_urgents.push_back(capture);
                                }
                                match staged {
                                    OrdinaryArrival::Answered(frame)|OrdinaryArrival::Gap(frame)=>deferred_control.enqueue(&uplink.0,Up::Term(AgentMsg::Wire(frame)))?,
                                    OrdinaryArrival::Queued|OrdinaryArrival::Duplicate|OrdinaryArrival::AdmittedPending=>{},
                                }
                                continue 'events;
                            }
                            // A command stream's newer generation: the ones before it are done with,
                            // and an item of one of those that comes late isn't taken again.
                            match wire_current.of(&stream) {
                                crate::wire::commands::Stand::Gone => continue 'events,
                                crate::wire::commands::Stand::Newer(prefix) => wire_in.forget_where(|s| s != stream && crate::wire::commands::prefix_of(s) == Some(prefix)),
                                crate::wire::commands::Stand::Current => {}
                            }
                            let arrival = if strict_ordinary { wire_in.on_item_strict(&stream,seq,low,body) } else { wire_in.on_item(&stream,seq,low,body) };
                            let answer = match arrival {
                                Arrival::Take(body) => match inner {
                                    Ok(ServerMsg::Wire(_)) => {
                                        let why = "a channel frame inside a command";
                                        if server_unread && let Some(folded) = told.tell(Instant::now(), crate::wire::unread::Wire::ServerItem, crate::wire::unread::Code::Malformed, "Wire", why) {
                                            uplink.msg(AgentMsg::Unread { wire: crate::wire::unread::Wire::ServerItem, what: "Wire".to_owned(), code: crate::wire::unread::Code::Malformed, why: why.to_owned(), folded, session: None }).await;
                                        }
                                        if strict_ordinary {
                                            // Refusal closes this SAME incoming sequence just like
                                            // accepted work. Do not ACK a volatile-only prefix.
                                            answer_after = Some((stream,seq,Some((Refusal::Malformed,why.into()))));
                                            continue 'events;
                                        }
                                        wire_in.refused(&stream, seq, Refusal::Malformed, why)
                                    }
                                    Ok(inner) => {
                                        answer_after = Some((stream, seq, None));
                                        break 'item (inner, body);
                                    }
                                    // Its NACK says it (unsupported for a variant this build doesn't
                                    // know), and the server hears of it as of a frame.
                                    Err(e) => {
                                        if server_unread && let Some(folded) = told.tell(Instant::now(), crate::wire::unread::Wire::ServerItem, e.code(), &e.what(), &e.why("ServerMsg")) {
                                            uplink.msg(AgentMsg::Unread { wire: crate::wire::unread::Wire::ServerItem, what: e.what(), code: e.code(), why: e.why("ServerMsg"), folded, session: None }).await;
                                        }
                                        if strict_ordinary {
                                            answer_after = Some((stream,seq,Some((e.code().refusal(),e.why("ServerMsg")))));
                                            continue 'events;
                                        }
                                        wire_in.refused(&stream, seq, e.code().refusal(), e.why("ServerMsg"))
                                    }
                                },
                                Arrival::Answered(Some(a)) | Arrival::Gap(a) => a,
                                Arrival::Answered(None) => continue 'events,
                            };
                            uplink.0.send(Up::Term(AgentMsg::Wire(answer))).await.ok();
                            continue 'events;
                        }
                        m => (m, data),
                    }
                }};
                // Validate the semantic message after channel unwrapping. A forwarded inner
                // Caps cannot establish a TLS connection's proof, nor can a former connection
                // lend that proof to any private effect on a replacement.
                let msg = match server_layout_on(&msg, origin, server_connection, server_layout) {
                    Ok(()) => msg,
                    Err(reason) if !current_origin => {
                        tracing::debug!(reason, "unbound server message supplies no layout authority");
                        continue 'events;
                    }
                    Err(reason) => {
                        refuse(&mut answer_after, crate::wire::channel::Refusal::Unsupported, reason);
                        ServerMsg::Refused { reason: reason.into() }
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
                // What it carries is read here, once (`crate::wire::payload_of`, docs/wire.md "What
                // can't be read"). What can't be read is refused: a command on the channel is NACKed
                // saying why, the server hears of it (`Unread`) either way (of a kind its newer build
                // sends, once a connection), and what waits on it is told why too.
                let msg = match crate::wire::payload_of(msg) {
                    Ok(read) => read,
                    Err(e) => {
                        tracing::warn!(what = %e.what, code = e.code.name(), why = %e.why, "what a message from the server carries couldn't be read");
                        refuse(&mut answer_after, e.code.refusal(), e.why.clone());
                        let tell = if server_unread { told.tell(Instant::now(), crate::wire::unread::Wire::ServerPayload, e.code, &e.what, &e.why) } else { None };
                        for up in unread_payload(&e, tell).into_iter().chain(unread_send(harness.as_deref(), &e)) {
                            uplink.0.send(up).await.ok();
                        }
                        continue;
                    }
                };
                let msg = match msg {
                    Carried::Nothing(m) => m,
                    Carried::Read(FromServer::RetainedReceived(receipt)) => {
                        if !server_retained_received || !current_origin { continue; }
                        match receipt.get("kind").and_then(serde_json::Value::as_str) {
                            Some("ordinary_domain")=> {
                                if ordinary_registered==origin && origin.is_some() {continue;}
                                let epoch=receipt.get("epoch").and_then(serde_json::Value::as_str);
                                if receipt.get("version").and_then(serde_json::Value::as_u64)==Some(1) && server_ordinary
                                    && ordinary_epoch.as_ref().is_ok_and(|own|Some(own.as_str())==epoch) && !o.outpost && executor_bootstrap.is_none()
                                    && ordinary_io.is_none() && let Some(origin)=origin
                                {
                                    let (records,nonce)=(Arc::clone(&ordinary_records),process_nonce.clone());
                                    ordinary_io=Some(async move {
                                        let result=async {
                                            let loaded=crate::wire::keep::load_ordinary_incoming(&*records,&nonce).await?;
                                            crate::wire::keep::register_ordinary_incoming(&*records,&nonce,origin,loaded.as_ref()).await
                                        }.await;
                                        OrdinaryDone::Registered{origin,result:Box::new(result)}
                                    }.boxed());
                                }
                            },
                            Some("ordinary_frontier") if ordinary_registered!=origin || origin.is_none()=>tracing::warn!("ordinary frontier ACK lacks its registered origin"),
                            _=> {
                                // No proof is retired by dropping an unadmitted ACK: the same
                                // bounded outbox replays and obtains it again. No ACK-of-ACK exists.
                                if spool_waits.len()<64 && serde_json::to_vec(&*receipt).is_ok_and(|bytes|bytes.len()<=65_536) {spool_waits.push_back(SpoolWork::Ack(*receipt));}
                                else {tracing::warn!("retained ACK ingress is full; original proof remains replayable");}
                            },
                        }
                        continue;
                    }

                    Carried::Read(FromServer::HarnessOpen { session, spec }) => {
                        let asked = (*spec).clone();
                        use crate::wire::channel::Refusal;
                        // How it fared, as its answer says it (a command on the channel).
                        let opened: Result<(), (Refusal, anyhow::Error)> = match (&harness, *spec) {
                            (None, _) => Err((Refusal::Unsupported, anyhow::anyhow!("agent sessions are not enabled on this runner"))),
                            (Some(h), mut spec) => {
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
                                // U04/U17: Taken is emitted after this arm returns. Preparation
                                // and keyed input acceptance must both finish before that ACK.
                                let prepared = if crate::workspace::needs_preparing(&spec) {
                                    crate::workspace::prepare(&mut spec, &session).await.map_err(|e| e.context("its workspace"))
                                } else {
                                    Ok(())
                                };
                                match prepared {
                                    Ok(()) => {
                                        let (h, id) = (Arc::clone(h), session.clone());
                                        // None means this id was accepted and ended: acknowledge
                                        // the durable acceptance without launching it again.
                                        crate::rt::blocking(move || h.open_once(&id, spec).map(|_| ())).await
                                            .and_then(|opened| opened).map_err(|e| (Refusal::Refused, e))
                                    }
                                    Err(e) => Err((Refusal::Refused, e)),
                                }
                            }
                        };
                        if let Err((code, e)) = opened {
                            refuse(&mut answer_after, code, format!("{e:#}"));
                            for u in crate::harness::host::refusal(&session, &asked, &format!("{e:#}")) {
                                if let Ok(json) = serde_json::to_string(&u) {
                                    uplink.0.send(Up::Term(AgentMsg::Harness { session: session.clone(), json })).await.ok();
                                }
                            }
                        }
                        continue;
                    }
                    Carried::Read(FromServer::HarnessCmd { session, command }) => {
                        use crate::wire::channel::Refusal;
                        if let Some(ticket)=&ordinary_answer
                            && let Some(urgent)=ordinary_urgents.iter().find(|urgent|urgent.origin==ticket.origin() && urgent.stream==ticket.stream() && urgent.seq==ticket.seq() && urgent.hash==ticket.body_hash())
                        {
                            match &urgent.target {
                                Some(target)=> {
                                    if !target.command(*command) {refuse(&mut answer_after,Refusal::Refused,"the originally captured session did not take its emergency control");}
                                },
                                None=>refuse(&mut answer_after,Refusal::Unknown,format!("no session {} existed at the original emergency intake",urgent.session)),
                            }
                            continue;
                        }
                        let known = harness.as_ref().is_some_and(|h| h.get(&session).is_some());
                        match *command {
                            // The server's word on a hand-over to this machine's newer runner
                            // process: never "gone here", even once this process has let go.
                            crate::harness::model::Command::HandedOver { error, unsure, by_newest, none_newer } => {
                                // With no newer process to take it, a word that it wasn't taken
                                // settles as a refusal does (the machine's record says).
                                if let Some(h) = &harness {
                                    h.handed_over(&session, error, unsure, by_newest || none_newer);
                                }
                            }
                            c if known => {
                                if let Some(h) = &harness
                                    && !h.command(&session, c)
                                {
                                    refuse(&mut answer_after, Refusal::Refused, format!("session {session} didn't take it: its lane is full, or it just ended"));
                                }
                            }
                            _ => {
                                refuse(&mut answer_after, Refusal::Unknown, format!("no session {session} here"));
                                // Gone here (e.g. stopped while the server was away): say so.
                                if let Ok(json) = serde_json::to_string(&crate::harness::core::Update::Closed) {
                                    uplink.0.send(Up::Term(AgentMsg::Harness { session, json })).await.ok();
                                }
                            }
                        }
                        continue;
                    }
                    // A message for an epoch (CP-T3.2): through its session's inbox, which answers.
                    Carried::Read(FromServer::HarnessSend { session, epoch, command }) => {
                        let known = harness.as_ref().is_some_and(|h| h.get(&session).is_some());
                        match *command {
                            c if known => {
                                if let Some(h) = &harness {
                                    h.command_at(&session, c, Some(epoch));
                                }
                            }
                            c => {
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
                        }
                        continue;
                    }
                    Carried::Read(FromServer::ReachAnswer { req, answer }) => {
                        if let Some(r) = &reach {
                            r.feed(crate::reach::node::In::Answer { req, answer }).await;
                        }
                        continue;
                    }
                    Carried::Read(FromServer::Sync { msg, raft }) => {
                        if let Some(s) = &sync {
                            s.heard(*msg, raft);
                        }
                        continue;
                    }
                    Carried::Read(FromServer::Observe { msg, bytes }) => {
                        if let Some(ob) = &observe {
                            ob.heard("", *msg, bytes);
                        }
                        continue;
                    }
                    // A certificate for a key this process holds (docs/ci.md W27-R01).
                    Carried::Read(FromServer::Identity(given)) => {
                        let label = o.instance.clone().unwrap_or_else(|| crate::upgrade::MAIN.to_owned());
                        if let Err(e) = crate::runner::identity::given(given, &o.state_dir, &label) {
                            tracing::warn!(error = %format!("{e:#}"), "an identity kiln sent couldn't be kept");
                        }
                        continue;
                    }


                    // Pages streamed for whoever views one of its sessions (`crate::browser::stream`).
                    Carried::Read(FromServer::StreamOpen { stream, session, spec }) => {
                        crate::rt::spawn(crate::browser::stream::serve_open(o.state_dir.clone(), stream, session, *spec, uplink.0.clone()));
                        continue;
                    }
                    Carried::Read(FromServer::StreamControl { stream, session: _, control }) => {
                        crate::browser::stream::serve_control(&stream, control);
                        continue;
                    }
                    Carried::Read(FromServer::StreamInput { stream, session: _, input }) => {
                        crate::browser::stream::serve_input(&stream, input);
                        continue;
                    }
                    // What a person opens live from one of its sessions (`crate::browser::apps`).
                    Carried::Read(FromServer::Preview { req, session, ask }) => {
                        let (up, dir) = (uplink.0.clone(), o.state_dir.clone());
                        let found = harness.as_ref().and_then(|h| h.get(&session)).map(|s| (std::path::PathBuf::from(s.info().cwd), *s.pid.lock()));
                        crate::rt::spawn(async move {
                            let answer = match found {
                                None => serde_json::json!({"error": "no such session here"}),
                                Some((cwd, pid)) => crate::browser::apps::serve(&dir, &session, &cwd, pid, &ask).await,
                            };
                            let _ = up.send(Up::Term(AgentMsg::Answer { req, json: Some(answer.to_string()) })).await;
                        });
                        continue;
                    }
                    Carried::Read(FromServer::Workspace { req, session, ask }) => {
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
                                Some((cwd, work, ws)) => crate::workspace::serve(&cwd, &work, ws.as_ref(), *ask).await,
                            };
                            let _ = up.send(Up::Term(AgentMsg::Answer { req, json: Some(answer.to_string()) })).await;
                        });
                        continue;
                    }
                    Carried::Read(FromServer::FrontDoor(push)) => {
                        // Settings (and certificates) are never logged: an upload carries a key.
                        let accepted = match &front_door {
                            Some(fd) => fd.push(*push).await,
                            None => Err(anyhow::anyhow!("front door is not enabled on this runner")),
                        };
                        if let Err(e) = accepted {
                            refuse(&mut answer_after, crate::wire::channel::Refusal::Refused, format!("front door update was not accepted: {e:#}"));
                        }
                        continue;
                    }
                    Carried::Read(FromServer::Headroom(push)) => {
                        if let Some(h) = &headroom {
                            h.push(*push);
                        }
                        continue;
                    }
                    // What a person set from the server for how many agent sessions this machine
                    // runs at once (none: its config's holds): kept for its restarts, held from now
                    // on, and said back (docs/agents.md, "How many agents a machine runs").
                    Carried::Read(FromServer::AgentLimit(asked)) => {
                        match limit_policy.accept(asked).await {
                            Ok(committed) => {
                                limit_asked = committed;
                                if let Some(h) = &harness {
                                    hold_agent_limit(h, crate::harness::limit::held(limit_asked, limit_config), &mut limit_said, server_limit && welcomed, &uplink, &mut deferred_control)?;
                                }
                            }
                            Err(e) => {
                                let why = format!("agent limit was not accepted durably; the last held policy remains: {e:#}");
                                tracing::warn!(error = %why, "agent limit update refused");
                                refuse(&mut answer_after, crate::wire::channel::Refusal::Refused, why);
                            }
                        }
                        continue;
                    }
                    // A leg of the CI engine (docs/ci.md W06): this runner announces `CAP_CI2`.
                    Carried::Read(FromServer::AssignV2(a)) => {
                        let mut a = *a;
                        let id = a.job_id.clone();
                        let attempt = Some(a.attempt);
                        // A pool's leg runs in its executor, beside this runner's own slots.
                        let pooled = a.executor.slot.is_some();
                        let holds = kiln_ci::sched::state::Incoming::of(a.exclusive, &a.holds.unwrap_or_default());
                        let ordinary = if !pooled && executor_bootstrap.is_none() && !matches!(a.executor.kind.as_str(),"k8s" | "firecracker") {
                            let owner = a.ordinary_owner.clone();
                            let valid = owner.as_ref().is_some_and(|owner| owner.validate().is_ok() && owner.job == id && Some(owner.attempt) == attempt
                                && owner.host == name && owner.process_nonce == process_nonce && ordinary_epoch.as_ref().is_ok_and(|epoch| *epoch == owner.publication_epoch)
                                && current_origin && ordinary_registered == origin && origin.is_some_and(|origin| origin.channel_id() == owner.connection)
                                && answer_after.as_ref().is_some_and(|(stream,seq,_)| stream == &owner.command.stream && *seq == owner.command.seq));
                            if !valid { refuse(&mut answer_after,crate::wire::channel::Refusal::Unsupported,"ordinary assignment requires its initialized, registered original grant and Live stamp"); continue; }
                            let owner = owner.context("ordinary checked owner disappeared")?;
                            if jobs.get(&id).is_some_and(|job| job.execution.identity() == owner.execution) {
                                // Across ALL deliveries this grant already started: never report NoStart.
                                refuse(&mut answer_after,crate::wire::channel::Refusal::Refused,"the original ordinary grant is already accepted");
                                continue;
                            }
                            match ordinary_execution.take() {
                                Some(execution) if execution.identity()==owner.execution=>Some(execution),
                                _=> {
                                    // No raw or inherited fallback. The authoritative head must
                                    // finish Gate/prefix/latch preparation before this continuation.
                                    refuse(&mut answer_after,crate::wire::channel::Refusal::Refused,"ordinary Start lacks its exact prepared continuation");continue;
                                },
                            }
                        } else {
                            if a.ordinary_owner.is_some() { refuse(&mut answer_after,crate::wire::channel::Refusal::Refused,"ordinary grant cannot change to an inherited/managed executor"); continue; }
                            None
                        };
                        let room = admits(o.slots, size, jobs.values().map(|j| (j.pooled, j.holds)), pooled, holds);
                        if o.outpost || draining || shutdown.is_cancelled() || room.is_err() || jobs.contains_key(&id) {
                            let reason = if o.outpost { OUTPOST_TAKES_NO_JOBS } else if draining || shutdown.is_cancelled() { "draining" } else { room.err().unwrap_or("no free slot") };
                            if let Some(execution)=ordinary {
                                let ticket=ordinary_answer.take().context("ordinary refusal lost its exact ticket")?;
                                answer_after=None;
                                ordinary_pending=Some(PendingOrdinary{job:Some(id.clone()),ticket:ticket.clone(),cancelled:true});
                                ordinary_io=Some(close_ordinary_io(ticket,execution,reason.into()));
                            }
                            uplink.msg(AgentMsg::Reject { job:id,reason:reason.into() }).await;
                            continue;
                        }
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
                        // U03: select and bind the exact physical owner before Running or its
                        // cancellation timer exists. A canonical executor can never silently
                        // fall back to a fresh root or acquire another host reservation.
                        let ordinary_refusal = ordinary.clone();
                        let selected: anyhow::Result<(Arc<crate::runner::processes::Execution>, Option<crate::runner::pool::Warm>, Option<std::fs::File>)> = async {
                            if let Some(bootstrap) = &executor_bootstrap {
                                return Ok((bootstrap.execution_for(admission_paths.clone(), &a, o.work_dir.clone())?, None, None));
                            }
                            let admission = crate::reach::leases::ci_guard(&admission_paths)?;
                            if a.executor.slot.is_some() {
                                if a.executor.kind == "docker" {
                                    let warm = pool_execs.take_for(&a)?.context("the canonical Docker executor is unavailable")?;
                                    match crate::runner::pool::adopted_execution(&a, &warm).await {
                                        Ok(execution) => return Ok((execution, Some(warm), Some(admission))),
                                        Err(error) => {
                                            pool_execs.restore(warm)?;
                                            return Err(error);
                                        }
                                    }
                                }
                                #[cfg(target_os = "linux")]
                                if a.executor.kind == "firecracker" {
                                    let execution = crate::runner::firecracker::adopted_execution(&a).await?;
                                    return Ok((execution, None, Some(admission)));
                                }
                                anyhow::bail!("the canonical executor is unavailable or unsupported on this receiver");
                            }
                            anyhow::ensure!(a.parent_binding.is_none(), "parent-bound assignment has no verified enrolled or warm executor");
                            anyhow::ensure!(!matches!(a.executor.kind.as_str(), "firecracker" | "k8s"), "a managed executor assignment requires its canonical startup identity");
                            Ok((ordinary.context("native assignment has no exact ordinary execution")?, None, Some(admission)))
                        }.await;
                        let (execution, warm, admission) = match selected {
                            Ok(selected) => selected,
                            Err(error) => {
                                if let Some(execution)=ordinary_refusal {
                                    let ticket=ordinary_answer.take().context("ordinary selection refusal lost its exact ticket")?;
                                    answer_after=None;
                                    ordinary_pending=Some(PendingOrdinary{job:Some(id.clone()),ticket:ticket.clone(),cancelled:true});
                                    ordinary_io=Some(close_ordinary_io(ticket,execution,format!("U03 exact executor ownership: {error:#}")));
                                }
                                deferred_control.enqueue(&uplink.0, Up::Msg(AgentMsg::Reject { job: id, reason: format!("U03 exact executor ownership: {error:#}") }))?;
                                continue;
                            }
                        };
                        if o.ephemeral { served_dedicated = true; }
                        // W27-R01/U03: ask only after this exact physical assignment was accepted.
                        if server_identity {
                            match crate::runner::identity::ask("leg", Some(&id), None) {
                                Ok(json) => uplink.msg(AgentMsg::Identity { json }).await,
                                Err(e) => tracing::warn!(job = %id, error = %format!("{e:#}"), "no identity asked for"),
                            }
                        }
                        journal.remove(&id);
                        unreceived.remove(&id);
                        let cancel = Shutdown::new();
                        hold_cpus(&id, holds);
                        jobs.insert(id.clone(), Running { _admission: admission, cancel: cancel.clone(), attempt, grace_stop: Shutdown::new(), execution: Arc::clone(&execution), pooled, holds });
                        tracing::info!(job = %id, leg = %a.leg.name, "running leg");
                        let leg = crate::runner::runtime::Leg { a, work_dir: o.work_dir.clone(), runner_name: name.clone(), up: uplink.clone(), cancel, http: http.clone(), warm: std::sync::Arc::new(parking_lot::Mutex::new(warm)), cache_max: o.declared.cache_max.as_deref().and_then(crate::runner::cache::parse_size), legacy: None };
                        let done = done_tx.clone();
                        // On the `firecracker` executor, in a microVM of its own (W13-R05).
                        #[cfg(target_os = "linux")]
                        let microvm = (leg.a.executor.kind == "firecracker").then(|| microvm_ca.clone());
                        let scope = Arc::clone(&execution);
                        crate::rt::spawn(scope.scope(async move {
                            #[cfg(target_os = "linux")]
                            let result = match microvm {
                                Some(ca) => crate::runner::firecracker::run(leg, Some(ca)).await,
                                None => leg.run().await,
                            };
                            #[cfg(not(target_os = "linux"))]
                            let result = leg.run().await;
                            execution.stop().await;
                            let _ = done.send((result, attempt)).await;
                        }));
                        continue;
                    }
                    // Start or stop a pool's executor (docs/ci.md W13-R09).
                    Carried::Read(FromServer::Executor { ask, bytes }) => {
                        let ordered = current_origin && answer_after.as_ref().is_some_and(|(stream, _, _)| crate::wire::commands::kind_of(stream) == Some(crate::wire::commands::Kind::Live));
                        let response = match pool_execs.enqueue(&o.work_dir, *ask, ordered, bytes) {
                            Ok(response) => response,
                            Err(error) => {
                                refuse(&mut answer_after, crate::wire::channel::Refusal::Refused, format!("executor intent was not accepted: {error:#}"));
                                continue 'events;
                            }
                        };
                        let up = uplink.clone();
                        crate::rt::spawn(async move {
                            // Execution was registered synchronously above. This task only
                            // forwards its result; it cannot reorder Start and Stop.
                            if let Ok(Some(said)) = response.await
                                && let Ok(json) = serde_json::to_string(&said)
                            {
                                up.msg(AgentMsg::Executor { json }).await;
                            }
                        });
                        continue;
                    }
                    // A deployment this runner made, taken down: its environment, a preview, went
                    // (docs/ci.md W15-R07).
                    Carried::Read(FromServer::Undeploy(ask)) => {
                        let up = uplink.clone();
                        crate::rt::spawn(async move {
                            let error = crate::runner::runtime::undeploy(&ask).await.err().map(|e| format!("{e:#}"));
                            if let Ok(json) = serde_json::to_string(&kiln_ci::wire::Undeployed { asked: ask, error }) {
                                up.msg(AgentMsg::Undeployed { json }).await;
                            }
                        });
                        continue;
                    }
                    // The rest of CI's messages (W09, W13) land with their workstreams.
                    Carried::Read(FromServer::SecretsFor { .. }) => {
                        tracing::debug!("ignoring a CI message this runner does not take yet");
                        continue;
                    }
                };
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
                        server_layout = origin.filter(|_| caps & proto::CAP_UNIFIED_LAYOUT != 0);
                        server_activation = server_layout.filter(|_| caps & proto::CAP_EXPLICIT_ACTIVATION != 0);
                        server_shaped_commands = caps & proto::CAP_FIRECRACKER_SHAPE != 0;
                        server_ordinary = caps & proto::CAP_ORDINARY_EXECUTION_DEBT != 0 && current_origin;
                        if caps & (proto::CAP_FIRECRACKER_SHAPE | proto::CAP_EXPLICIT_ACTIVATION | proto::CAP_ORDINARY_EXECUTION_DEBT) != 0 && !o.outpost {
                            // U16's process association also applies to a supervised macOS runner.
                            deferred_control.enqueue(&uplink.0, Up::Term(AgentMsg::RunnerProcess { nonce: process_nonce.clone() }))?;
                            if server_ordinary && executor_bootstrap.is_none() && let Ok(epoch) = &ordinary_epoch {
                                deferred_control.enqueue(&uplink.0, Up::Term(AgentMsg::JobEvent { job: "@kiln/ordinary-domain".into(), json: serde_json::to_string(&kiln_ci::wire::JobEventV2::OrdinaryDomain { epoch: epoch.clone(), process_nonce: process_nonce.clone() })? }))?;
                            }
                        }
                        if caps & proto::CAP_FIRECRACKER_SHAPE != 0 && !o.outpost && cfg!(target_os="linux") {spool_sample_due=true;}
                        // An identity of this process's own, for a key it made and keeps, certified
                        // on each connection (docs/ci.md W27-R01).
                        server_identity = caps & proto::CAP_IDENTITY != 0 && !o.ephemeral;
                        if server_identity {
                            match crate::runner::identity::ask("runner", None, None) {
                                Ok(json) => {
                                    uplink.0.send(Up::Term(AgentMsg::Identity { json })).await.ok();
                                }
                                Err(e) => tracing::warn!(error = %format!("{e:#}"), "no identity asked for"),
                            }
                            // And each agent session it holds, at its epoch.
                            for info in harness.as_ref().map(|h| h.list()).unwrap_or_default() {
                                session_epochs.insert(info.id.clone(), info.epoch);
                                ask_agent_identity(&uplink, &mut identity_asked, &info.id, info.epoch).await;
                            }
                        }
                        server_wire = caps & proto::CAP_WIRE != 0;
                        server_drops = caps & proto::CAP_NET_DROPS != 0;
                        server_work = caps & proto::CAP_WORK != 0;
                        server_limit = caps & proto::CAP_AGENT_LIMIT != 0;
                        server_job_attempt = caps & proto::CAP_JOB_ATTEMPT != 0;
                        server_retained_received = caps & proto::CAP_RETAINED_RECEIVED != 0;
                        sent_changes.server(caps & proto::CAP_CHANGES_PATCH != 0);
                        server_headroom = caps & proto::CAP_HEADROOM != 0;
                        // How headroom does, now that this server takes it.
                        headroom_sent = None;
                        if server_headroom
                            && let Some(json) = headroom.as_ref().and_then(crate::headroom::Handle::latest)
                        {
                            uplink.0.send(Up::Term(AgentMsg::HeadroomStatus { json: json.clone() })).await.ok();
                            headroom_sent = Some(json);
                        }
                        server_unread = caps & proto::CAP_UNREAD != 0;
                        if let Some(r) = &reach {
                            r.feed(crate::reach::node::In::Unread(server_unread)).await;
                        }
                        if server_wire {
                            deferred_control.enqueue(&uplink.0, Up::Term(AgentMsg::Wire(wire_in.have())))?;
                        }
                    }
                    // The relay refused a frame of this runner's reach links: as a close, its code
                    // said in the log.
                    ServerMsg::ReachRefused { from, link, code, why } => {
                        tracing::debug!(%from, %link, code = code.name(), %why, "the relay refused a reach frame");
                        if let Some(r) = &reach {
                            r.feed(crate::reach::node::In::Frame { from, link, open: false, data: Vec::new(), close: Some(why) }).await;
                        }
                    }
                    // The server couldn't read a frame of this runner's: it keeps it for this
                    // runner's page; here it's said in the log.
                    ServerMsg::Unread { wire, what, code, why, folded } => {
                        tracing::warn!(wire = wire.name(), %what, code = code.name(), %why, folded, "the server couldn't read a message from this runner");
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
                                    start_stream(&mut updates, id, &s, server_kept.get(id), &mut sent_changes);
                                }
                            } else if let Some(id) = key.strip_prefix("t:")
                                && let Some((_, state)) = terms.get(id)
                            {
                                // A terminal's stream starts over with its picture now.
                                let a = state.attach();
                                state.detach();
                                updates.start(&key);
                                if let Ok(b) = proto::encode(&AgentMsg::TermReplay { session: id.to_owned(), data: a.replay }) {
                                    updates.picture(&key, b);
                                }
                            }
                        }
                        flush_updates(&mut updates, &uplink)?;
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
                            let data = completion_wire(data.clone(), server_job_attempt);
                            net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, data).await;
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
                            hold_agent_limit(h, crate::harness::limit::held(limit_asked, limit_config), &mut limit_said, server_limit, &uplink, &mut deferred_control)?;
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
                                        push_update(&mut updates, &session, &u, false, &mut sent_changes);
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
                                        start_stream(&mut updates, &info.id, &s, server_kept.get(&info.id), &mut sent_changes);
                                    }
                                    continue;
                                }
                                for u in s.snapshot() {
                                    for json in harness_frames(&info.id, &u) {
                                        uplink.0.send(Up::Term(AgentMsg::Harness { session: info.id.clone(), json })).await.ok();
                                    }
                                }
                            }
                            flush_updates(&mut updates, &uplink)?;
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
                        server_connection = None;
                    server_layout = None;
                    server_activation = None;
                        welcomed = false;
                        if let Some(s) = &sync {
                            s.connected(false);
                        }
                        if let Some(ob) = &observe {
                            ob.up("", false);
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
                        if o.outpost || draining || room.is_err() || jobs.contains_key(&id) {
                            let reason = if o.outpost { OUTPOST_TAKES_NO_JOBS } else if draining { "draining" } else { room.err().unwrap_or("no free slot") };
                            uplink.msg(AgentMsg::Reject { job: id, reason: reason.into() }).await;
                            continue;
                        }
                        if o.dedicated_job.as_deref() == Some(id.as_str()) {
                            served_dedicated = true;
                        }
                        let admission = match crate::reach::leases::ci_guard(&admission_paths) {
                            Ok(g) => g,
                            Err(e) => {
                                uplink.msg(AgentMsg::Reject { job: id, reason: format!("U01 execution fence: {e}") }).await;
                                continue;
                            }
                        };
                        let cancel = Shutdown::new();
                        hold_cpus(&id, holds);
                        let execution = crate::runner::processes::Execution::new_in(admission_paths.clone(), o.work_dir.clone(), id.clone(), Some(a.attempt.saturating_add(1)));
                        jobs.insert(id.clone(), Running { _admission: Some(admission), cancel: cancel.clone(), attempt: None, grace_stop: Shutdown::new(), execution: Arc::clone(&execution), pooled: false, holds });
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
                        let scope = Arc::clone(&execution);
                        crate::rt::spawn(scope.scope(async move {
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
                            execution.stop().await;
                            let _ = done.send((result, None)).await;
                        }));
                    }
                    ServerMsg::Cancel { job, reason } => {
                        if let Some(pending)=ordinary_pending.as_mut().filter(|pending|pending.job.as_deref()==Some(job.as_str())) {
                            pending.cancelled=true;
                            continue;
                        }
                        if let Some(r) = jobs.get(&job) {
                            tracing::info!(%job, %reason, "cancelling job");
                            r.cancel.cancel();
                            // U03: bind grace cleanup to this execution before any await. A
                            // normal completion cancels it before another attempt can start.
                            let (tx, j, attempt, grace_stop, execution) = (done_tx.clone(), job.clone(), r.attempt, r.grace_stop.clone(), Arc::clone(&r.execution));
                            crate::rt::spawn(async move {
                                futures::select_biased! {
                                    () = grace_stop.cancelled().fuse() => return,
                                    () = crate::rt::sleep(CANCEL_GRACE).fuse() => {}
                                }
                                // A stuck runtime keeps its slot until this exact execution is
                                // fenced against new commands and every containment is empty.
                                futures::select_biased! {
                                    () = grace_stop.cancelled().fuse() => return,
                                    () = execution.stop().fuse() => {}
                                }
                                let error = format!("cancelled; after {}s the runner proved its execution stopped", CANCEL_GRACE.as_secs());
                                let _ = tx.send((JobResult { job: j, status: Status::Cancelled, exit_code: None, error: Some(error), peak_rss: None, cpu_seconds: None }, attempt)).await;
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
                        let closed = match harness.clone() {
                            Some(host) => {
                                let id = session.clone();
                                crate::rt::blocking(move || host.try_close(&id)).await.and_then(|result| result)
                            }
                            None => Ok(false),
                        };
                        match closed {
                            Ok(true) => {}
                            Ok(false) => refuse(&mut answer_after, crate::wire::channel::Refusal::Unknown, format!("no session {session} here")),
                            Err(error) => {
                                // U17: storage refusal must never acknowledge an accepted close.
                                refuse(&mut answer_after, crate::wire::channel::Refusal::Refused, format!("session stop was not committed: {error:#}"));
                                continue;
                            }
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
                                        let spec = crate::harness::session::Spec { kind: "claude".into(), ..Default::default() };
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
                    ServerMsg::Upgrade { job, path, sha256, size } if !controlled_upgrade_on(origin, server_connection, server_activation) => {
                        let _ = (path, sha256, size);
                        let why = crate::updates::UpgradeRefusal::Coordinator.to_string();
                        refuse(&mut answer_after, crate::wire::channel::Refusal::Unsupported, why.clone());
                        uplink.msg(AgentMsg::UpgradeDone { job, ok: false, error: Some(why) }).await;
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
                    ServerMsg::Upgrade { job, path, sha256, size } if activation.is_some() => {
                        // Blue/green: the new build is fetched beside this one and started next to
                        // it by the supervisor; this runner goes on until it's ready.
                        tracing::info!(%job, %path, "upgrading this runner: blue/green");
                        let up = Uplink(uplink.0.clone());
                        let (http, server_url, state, tx) = (Arc::clone(&http), o.server_url.clone(), o.state_dir.clone(), bg_tx.clone());
                        let supervisor = activation.clone();
                        crate::rt::spawn(async move {
                            let order = crate::updates::Order { job: &job, path: &path, sha256: &sha256, size };
                            let checked = match supervisor {
                                Some(sup) => crate::rt::blocking(move || sup.check_activation()).await.and_then(|r| r),
                                None => Err(crate::updates::UpgradeRefusal::Backend.into()),
                            };
                            if let Err(e) = checked {
                                up.msg(AgentMsg::UpgradeDone { job, ok: false, error: Some(format!("{e:#}")) }).await;
                                return;
                            }
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
                        // U16: legacy/Windows/outpost/in-process backends have no explicit
                        // activation owner. Refuse even an older server before fetch or install.
                        let _ = (path, sha256, size);
                        let why = crate::updates::UpgradeRefusal::Backend.to_string();
                        refuse(&mut answer_after, crate::wire::channel::Refusal::Unsupported, why.clone());
                        uplink.msg(AgentMsg::UpgradeDone { job, ok: false, error: Some(why) }).await;
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
                    ServerMsg::DirList { req, path } => {
                        // The console's folder picker: this machine's folders, read off this loop.
                        let up = uplink.0.clone();
                        crate::rt::spawn(async move {
                            let json = crate::dirs::answer(path).await;
                            let _ = up.send(Up::Term(AgentMsg::DirList { req, json })).await;
                        });
                    }
                    ServerMsg::Amphora { .. } => {
                        tracing::error!(target: "kiln.replication", event = "transport_refused", "Amphora transport requires its dedicated authenticated carrier lane; no product acknowledgement was made");
                    }
                    ServerMsg::ProbeFacts => probe_facts(&uplink, &o.work_dir, &o.declared, &probing),
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
                        // U03: a host debug PTY must belong to the exact active execution or
                        // a child registered with the retained root before any raw PTY spawn.
                        let (execution, retained_child) = if let Some(running) = jobs.get(&job) {
                            (Arc::clone(&running.execution), false)
                        } else if let Some(child) = crate::runner::kept::debug_child(&job) {
                            if !crate::runner::kept::debug_execution(&job, &child) {
                                crate::rt::spawn(async move { child.stop().await; });
                                refuse(&mut answer_after, crate::wire::channel::Refusal::Refused, "retained execution is being released");
                                continue;
                            }
                            (child, true)
                        } else {
                            refuse(&mut answer_after, crate::wire::channel::Refusal::Refused, "the job has no exact debug execution owner");
                            continue;
                        };
                        match terminal::spawn_at_execution(l.shell.as_deref(), l.command.as_deref(), l.program.as_deref(), l.cwd.as_deref(), cols, rows, &env, Some(Arc::clone(&execution))) {
                            Ok((pty, rx)) => {
                                let (tx, s, masked) = (term_tx.clone(), session.clone(), crate::runner::kept::masked(rx, &shell.masks));
                                crate::rt::spawn(async move {
                                    while let Ok(out) = masked.recv().await {
                                        let exited = matches!(&out, PtyOut::Exit(_));
                                        // The bridge proves its own group empty for live jobs.
                                        // Only retained children may fence their whole scope;
                                        // stopping a live job's root would kill unrelated steps.
                                        if exited && retained_child { execution.stop().await; }
                                        if tx.send((s.clone(), out)).await.is_err() || exited {
                                            break;
                                        }
                                    }
                                    if retained_child { execution.stop().await; }
                                });
                                let state = crate::termsess::Session::detached(session.clone(), format!("leg:{job}"), None, cols, rows);
                                for data in typed {
                                    pty.write(data);
                                }
                                terms.insert(session, (pty, state));
                            }
                            Err(e) => {
                                if retained_child { crate::rt::spawn(async move { execution.stop().await; }); }
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
                    ServerMsg::RetainedReceived { .. } => {}
                    ServerMsg::ReceivedV2 { job, attempt } => {
                        if unreceived.get(&job).is_some_and(|(_, data)| completion_receipt(data, Some(attempt), server_job_attempt)) {
                            unreceived.remove(&job);
                            if let Some((_, _, events)) = journal.get_mut(&job) { events.clear(); }
                        }
                    }
                    ServerMsg::Received { job } => {
                        // U03: an unfenced receipt cannot clear a fenced completion.
                        if !unreceived.get(&job).is_some_and(|(_, data)| completion_receipt(data, None, server_job_attempt)) { continue; }
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
                    // What carries JSON came read (`wire::payload_of`), above.
                    ServerMsg::HarnessOpen { .. }
                    | ServerMsg::Identity { .. }
                    | ServerMsg::Observe { .. }
                    | ServerMsg::Headroom { .. }
                    | ServerMsg::HarnessCmd { .. }
                    | ServerMsg::HarnessSend { .. }
                    | ServerMsg::Sync { .. }
                    | ServerMsg::FrontDoor { .. }
                    | ServerMsg::AssignV2 { .. }
                    | ServerMsg::SecretsFor { .. }
                    | ServerMsg::ReachAnswer { .. }
                    | ServerMsg::Convo { .. }
                    | ServerMsg::StreamOpen { .. }
                    | ServerMsg::StreamControl { .. }
                    | ServerMsg::StreamInput { .. }
                    | ServerMsg::Preview { .. }
                    | ServerMsg::Workspace { .. }
                    | ServerMsg::Executor { .. }
                    | ServerMsg::Undeploy { .. }
                    | ServerMsg::Cluster { .. }
                    | ServerMsg::AgentLimit { .. } => {}
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
                        flush_updates(&mut updates, &uplink)?;
                    }
                } else {
                    uplink.0.send(Up::Term(msg)).await.ok();
                }
            }
            event @ (Ev::Up(_) | Ev::UpDurable(_)) => {
                let (mut up,already_durable)=match event {Ev::Up(up)=>(up,false),Ev::UpDurable(up)=>(up,true),_=>continue};
                // U03: commit retained evidence before its first transmission. Completion
                // receipts never clear this independent crash-replay outbox.
                let retained_evidence = match &up {
                    Up::Msg(AgentMsg::JobEvent { job, json }) => retained::Receipt::of(job, json)?.filter(|receipt| receipt.event.get("ordinary_grant").is_none()),
                    Up::Msg(AgentMsg::Executor { json }) => retained::Receipt::executor(json)?,
                    _ => None,
                };
                if !already_durable && let Some(receipt)=retained_evidence {
                    if ordinary_io.is_none() && let Some(spool)=retained_spool.take() {
                        spool_publication_pending=true;
                        ordinary_io=Some(spool_io(spool,SpoolWork::Publish{up:Box::new(up),receipt}));
                    } else {retained_blocked=Some((up,Instant::now()+Duration::from_secs(1)));}
                    continue 'events;
                }
                if let Up::Msg(AgentMsg::Executor { json }) = &up
                    && let Ok(said) = serde_json::from_str::<crate::runner::pool::Said>(json)
                { pool_execs.published(&said); }
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
                    Up::Observe(m) => (LANE_OBSERVE, Kind::Control, proto::encode(m)),
                    Up::Convo(m) => (LANE_CONVO, Kind::Log, proto::encode(m)),
                    Up::File(m) => (LANE_FILES, Kind::Log, proto::encode(m)),
                    Up::Stream(m) => (LANE_STREAM, Kind::Term, proto::encode(m)),
                    Up::Port(m) => (LANE_PORTS, Kind::Term, proto::encode(m)),
                };
                let Ok(data) = data else { continue };
                let data = completion_wire(data, server_job_attempt);
                if let Some((_, _, events)) = journaled.as_ref().and_then(|j| journal.get_mut(j)) {
                    events.push(data.clone());
                }
                // What the next connection sends again (a leg's result not yet received, its
                // events once the server takes them again) needn't be kept for it.
                let again = matches!(&up, Up::Msg(AgentMsg::JobFinished(r)) if unreceived.contains_key(&r.job)) || matches!(&up, Up::Msg(AgentMsg::JobFinishedV2 { result, .. }) if unreceived.contains_key(&result.job)) || (receipts && journaled.is_some());
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
                } else if kind != Kind::Term && lane != LANE_SYNC && lane != LANE_REACH && lane != LANE_OBSERVE && lane != LANE_CONVO && lane != LANE_FILES {
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
                // Each session this process holds has an identity at its epoch, asked for until it
                // has one; one that closed lets go of its keys (docs/ci.md W27-R01).
                match &u {
                    crate::harness::core::Update::Open { info, .. } => {
                        session_epochs.insert(session.clone(), info.epoch);
                    }
                    crate::harness::core::Update::Closed => {
                        session_epochs.remove(&session);
                        crate::runner::identity::forget_prefix(&format!("agent/{session}/"));
                    }
                    _ => {}
                }
                if server_identity
                    && connected
                    && let Some(epoch) = session_epochs.get(&session).copied()
                {
                    ask_agent_identity(&uplink, &mut identity_asked, &session, epoch).await;
                }
                if updates_on {
                    // Over the channel: kept until the server has it, down or not. A new session's
                    // stream starts with its Open; a closed one's ends once answered.
                    let key = crate::wire::uplink::session_key(&session);
                    let mut opens = false;
                    if !updates.started(&key)
                        && let crate::harness::core::Update::Open { info, .. } = &u
                    {
                        // Announced after the server said what its copy has, of this epoch (a
                        // session that started again from the store): its stream starts as the
                        // Welcome starts one, with only what the copy lacks.
                        let kept = server_kept.get(&session).filter(|k| k.epoch == info.epoch);
                        if let (Some(kept), Some(s)) = (kept, harness.as_ref().and_then(|h| h.get(&session))) {
                            start_stream(&mut updates, &session, &s, Some(kept), &mut sent_changes);
                            if connected {
                                flush_updates(&mut updates, &uplink)?;
                            }
                            continue 'events;
                        }
                        updates.start(&key);
                        opens = true;
                    }
                    // A new session's Open is its stream's picture.
                    push_update(&mut updates, &session, &u, opens, &mut sent_changes);
                    if matches!(u, crate::harness::core::Update::Closed) {
                        updates.closing(&key);
                    }
                    if connected {
                        flush_updates(&mut updates, &uplink)?;
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
            Ev::Done(r, attempt) => {
                if retiring {
                    retire_at = Some(Instant::now() + Duration::from_secs(5)).max(retire_at);
                }
                if jobs.get(&r.job).is_some_and(|j| j.attempt == attempt) {
                    if let Some(running) = jobs.remove(&r.job) {
                        running.grace_stop.cancel();
                    }
                    crate::runner::cpus::pins().release(&r.job);
                    // The leg's key goes with it (docs/ci.md W27-R01).
                    crate::runner::identity::forget(&crate::runner::identity::slot("leg", Some(&r.job), None));
                    tracing::info!(job = %r.job, status = r.status.as_str(), "job finished");
                    let msg = match attempt {
                        Some(attempt) => AgentMsg::JobFinishedV2 { result: r.clone(), attempt },
                        None => AgentMsg::JobFinished(r.clone()),
                    };
                    if let Ok(b) = proto::encode(&msg) {
                        unreceived.insert(r.job.clone(), (Instant::now(), b));
                    }
                    deferred_control.enqueue(&uplink.0, Up::Msg(msg))?;
                } else {
                    // Reported over already: a cancelled job let go of after its grace that ended
                    // after all (or the grace's report, after the job's own).
                    tracing::info!(job = %r.job, "a job already reported over has ended");
                }
            }
            Ev::Heartbeat => {
                // Each agent session it holds and has no identity for yet, asked for again: a quiet
                // one's first ask may have come before the server had its Open, and was refused.
                if server_identity && connected {
                    for (session, epoch) in &session_epochs {
                        ask_agent_identity(&uplink, &mut identity_asked, session, *epoch).await;
                    }
                }
                // What it folded of the server's it couldn't read, and no word carried for a
                // minute: told now (`wire::unread::Told::flush`).
                if server_unread && let Some((f, folded)) = told.flush(Instant::now()) {
                    uplink.msg(AgentMsg::Unread { wire: f.wire, what: f.what, code: f.code, why: f.why, folded, session: None }).await;
                }
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
                        hold_agent_limit(h, crate::harness::limit::held(limit_asked, limit_config), &mut limit_said, server_limit && welcomed, &uplink, &mut deferred_control)?;
                    }
                }
                if connected {
                    let free = if retiring { 0 } else { o.slots.saturating_sub(u32::try_from(jobs.len()).unwrap_or(0)) };
                    deferred_control.enqueue(&uplink.0, Up::Msg(AgentMsg::Heartbeat { free_slots: free, running: jobs.keys().cloned().collect() }))?;
                    // Its machine's say on upgrades, read again: a change goes to the server at once.
                    if !o.ephemeral && !o.outpost {
                        let accept = takes_updates(&o);
                        if accept_said != Some(accept) {
                            deferred_control.enqueue(&uplink.0, Up::Msg(AgentMsg::UpdatePrefs { accept }))?;
                            accept_said = Some(accept);
                        }
                    }
                }
            }
            Ev::Sample => {
                if connected && welcomed && server_ordinary && ordinary_registered != server_connection && !o.outpost && executor_bootstrap.is_none()
                    && let Ok(epoch) = &ordinary_epoch
                {
                    // Registration is idempotent and retried by this existing sample owner;
                    // a lost ACK never promotes readiness or needs another controller.
                    deferred_control.enqueue(&uplink.0, Up::Term(AgentMsg::RunnerProcess { nonce:process_nonce.clone() }))?;
                    deferred_control.enqueue(&uplink.0, Up::Term(AgentMsg::JobEvent { job:"@kiln/ordinary-domain".into(),json:serde_json::to_string(&kiln_ci::wire::JobEventV2::OrdinaryDomain { epoch:epoch.clone(),process_nonce:process_nonce.clone() })? }))?;
                }
                spool_sample_due=true;
                if connected && updates_on {
                    // Items whose answers are overdue (lost below the channel) go again.
                    flush_updates(&mut updates, &uplink)?;
                }
                // What it has of the server's commands, said again: one lost below the channel
                // mustn't hold the server's commands up until the next connection.
                if connected && server_wire {
                    deferred_control.enqueue(&uplink.0, Up::Term(AgentMsg::Wire(wire_in.have())))?;
                }
                if connected {
                    let s = sampler.sample();
                    deferred_control.enqueue(&uplink.0, Up::Msg(AgentMsg::Sample(s)))?;
                    // What its QUIC layer let go below the channel, for its page: once it changed.
                    let dropped = crate::metrics::net().counts();
                    if server_drops && welcomed && drops_said.as_ref() != Some(&dropped) {
                        deferred_control.enqueue(&uplink.0, Up::Msg(AgentMsg::NetDrops { dropped: dropped.clone() }))?;
                        drops_said = Some(dropped);
                    }
                    // What its agent sessions are held to, said again if the server lacks it.
                    if let Some(h) = &harness {
                        hold_agent_limit(h, crate::harness::limit::held(limit_asked, limit_config), &mut limit_said, server_limit && welcomed, &uplink, &mut deferred_control)?;
                    }
                    // What it holds warm, for placement (docs/ci.md W24-R18): once it changed here,
                    // and every 10 minutes for what the machine's other runner processes changed.
                    let now_changes = crate::runner::warm::changes();
                    if server_work && welcomed && !o.ephemeral && !o.outpost && work_said.is_none_or(|(at, when)| at != now_changes || when.elapsed() >= Duration::from_secs(600)) {
                        let dir = o.work_dir.clone();
                        if let Ok(state) = crate::rt::blocking(move || crate::runner::warm::state(&dir)).await
                            && let Ok(json) = serde_json::to_string(&state)
                        {
                            deferred_control.enqueue(&uplink.0, Up::Msg(AgentMsg::WorkState { json }))?;
                        }
                        work_said = Some((now_changes, Instant::now()));
                    }
                    // Its battery, once a minute, when it has one: said when it changes, and every
                    // 10 minutes (a machine on battery sleeps when it runs out).
                    if !o.ephemeral && !o.outpost && power_read_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                        power_read_at = Some(Instant::now());
                        if let Ok(Some(b)) = crate::rt::blocking(crate::os::battery::read).await
                            && (power_said.as_ref().is_none_or(|(said, _)| said != &b) || power_said.as_ref().is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(600)))
                            && let Ok(json) = serde_json::to_string(&b)
                        {
                            deferred_control.enqueue(&uplink.0, Up::Msg(AgentMsg::Power { json }))?;
                            power_said = Some((b, Instant::now()));
                        }
                    }
                    if let Some(ai) = &ai {
                        let shells = terms.iter().filter_map(|(id, (pty, _))| Some((pty.pid()?, id.clone()))).collect();
                        let managed = harness.as_ref().map(|h| h.harness_pids()).unwrap_or_default();
                        ai.scan(shells, managed, uplink.0.clone());
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


/// What each session's channel stream has carried of its changeset: a digest of each of its files,
/// by key. To a server that takes patches (`proto::CAP_CHANGES_PATCH`), a changeset then goes as
/// what changed in it: an agent touching one file of hundreds sends that one, not every file's diff
/// again (a long session's changeset ran to 690 KB, sent whole for every edit).
#[derive(Default)]
struct SentChanges {
    /// The server takes changesets as patches.
    patches: bool,
    sessions: HashMap<String, HashMap<crate::harness::model::FileKey, u64>>,
}

impl SentChanges {
    /// The server says (again) whether it takes patches: one that doesn't has nothing to compare
    /// with.
    fn server(&mut self, patches: bool) {
        if !patches {
            self.sessions.clear();
        }
        self.patches = patches;
    }

    /// `session`'s stream carried an Open (its server's copy starts its changeset over) or ended.
    fn forget(&mut self, session: &str) {
        self.sessions.remove(session);
    }

    /// `session`'s changeset `files` as frames that fit, and what its stream carries of it noted:
    /// to a server that takes patches, what changed since what the stream carried (or, in a
    /// picture, all of it: its first part whole, the rest as patches); else whole, its largest
    /// diffs left out to fit.
    fn frames(&mut self, session: &str, files: &[crate::harness::model::FileChange], picture: bool) -> Vec<String> {
        use crate::harness::core::Update;
        if !self.patches {
            return fit_changes(files).into_iter().collect();
        }
        let budget = HARNESS_FRAME - 1024;
        let digest = |f: &crate::harness::model::FileChange| {
            use std::hash::{Hash as _, Hasher as _};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            serde_json::to_vec(f).unwrap_or_default().hash(&mut h);
            h.finish()
        };
        let now: HashMap<crate::harness::model::FileKey, u64> = files.iter().map(|f| (f.key(), digest(f))).collect();
        let frames = if picture {
            let mut parts = file_parts(files, budget).into_iter();
            let first = Update::Changes { files: parts.next().unwrap_or_default() };
            std::iter::once(first).chain(parts.map(|set| Update::ChangesPatch { set, gone: Vec::new() })).filter_map(|u| serde_json::to_string(&u).ok()).collect()
        } else {
            let had = self.sessions.get(session);
            let set: Vec<crate::harness::model::FileChange> = files.iter().filter(|f| had.and_then(|h| h.get(&f.key())) != now.get(&f.key())).cloned().collect();
            let mut gone: Vec<crate::harness::model::FileKey> = had.map(|h| h.keys().filter(|k| !now.contains_key(*k)).cloned().collect()).unwrap_or_default();
            gone.sort();
            patch_frames(&set, gone)
        };
        self.sessions.insert(session.to_owned(), now);
        frames
    }
}

/// A changeset's patch as frames that fit, in order: what's gone first, then the files set, as
/// many to a frame as fit (a file too large alone goes as its counts only).
fn patch_frames(set: &[crate::harness::model::FileChange], gone: Vec<crate::harness::model::FileKey>) -> Vec<String> {
    use crate::harness::core::Update;
    let budget = HARNESS_FRAME - 1024;
    let gone = (!gone.is_empty()).then_some(Update::ChangesPatch { set: Vec::new(), gone });
    gone.into_iter().chain(file_parts(set, budget).into_iter().map(|set| Update::ChangesPatch { set, gone: Vec::new() })).filter_map(|u| serde_json::to_string(&u).ok()).collect()
}

/// A changeset's files as parts that each fit a frame of `budget` bytes, in order: a file too large
/// to fit alone goes as its counts only (`FileChange::counts_only`: "Diff shortened" where it's
/// shown).
fn file_parts(files: &[crate::harness::model::FileChange], budget: usize) -> Vec<Vec<crate::harness::model::FileChange>> {
    // What a part's frame holds besides its files.
    let room = budget.saturating_sub(64);
    let mut parts = Vec::new();
    let mut part = Vec::new();
    let mut size = 0usize;
    for f in files {
        let mut n = json_len(f).saturating_add(1);
        let f = if n > room {
            let short = f.counts_only();
            n = json_len(&short).saturating_add(1);
            short
        } else {
            f.clone()
        };
        if size.saturating_add(n) > room && !part.is_empty() {
            parts.push(std::mem::take(&mut part));
            size = 0;
        }
        size = size.saturating_add(n);
        part.push(f);
    }
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

/// A changeset whole, in a frame that fits: its largest files' diffs left out, the largest first,
/// each file's counts kept (`FileChange::counts_only`), until it fits; then its longest texts cut
/// (a path out of all reason). Its hundreds of short diff lines are what no text cut shortens: as
/// it was cut before, a large changeset was dropped whole, and its page went on showing the old one.
/// None: it can't fit even so.
fn fit_changes(files: &[crate::harness::model::FileChange]) -> Option<String> {
    use crate::harness::core::Update;
    let budget = HARNESS_FRAME - 1024;
    let whole = Update::Changes { files: files.to_vec() };
    let json = serde_json::to_string(&whole).ok()?;
    if json.len() <= budget {
        return Some(json);
    }
    let Update::Changes { mut files } = whole else { return None };
    let mut size = json.len();
    let mut largest: Vec<(usize, usize)> = files.iter().enumerate().map(|(i, f)| (json_len(f), i)).collect();
    largest.sort_unstable_by(|a, b| b.cmp(a));
    for (n, i) in largest {
        if size <= budget {
            break;
        }
        if let Some(f) = files.get_mut(i) {
            let short = f.counts_only();
            size = size.saturating_sub(n).saturating_add(json_len(&short));
            *f = short;
        }
    }
    let fitted = serde_json::to_value(Update::Changes { files }).ok().map(|v| cut_value(v, budget))?;
    serde_json::to_string(&fitted).ok().filter(|j| j.len() <= HARNESS_FRAME)
}

/// One agent-session update onto its session's channel stream, as the frames it needs: part of the
/// picture its stream starts with (`picture`: its snapshot, a new session's Open), which goes
/// whatever its size, or after it. A changeset goes as what changed in it where the server takes
/// that (`sent`).
fn push_update(updates: &mut crate::wire::uplink::Updates, session: &str, u: &crate::harness::core::Update, picture: bool, sent: &mut SentChanges) -> usize {
    use crate::harness::core::Update;
    let key = crate::wire::uplink::session_key(session);
    let frames = match u {
        Update::Changes { files } => sent.frames(session, files, picture),
        // Its server's copy starts its changeset over (an Open), or lets the session go.
        Update::Open { .. } | Update::Closed => {
            sent.forget(session);
            harness_frames(session, u)
        }
        _ => harness_frames(session, u),
    };
    let mut bytes = 0;
    for json in frames {
        if let Ok(b) = proto::encode(&AgentMsg::Harness { session: session.to_owned(), json }) {
            bytes += b.len();
            if picture {
                updates.picture(&key, b);
            } else {
                updates.push(&key, b);
            }
        }
    }
    bytes
}

/// Starts `session`'s channel stream (again), its snapshot first: when the server's copy is of
/// the same epoch (`kept`), only what the copy lacks (`crate::wire::sums`), else all of it.
fn start_stream(updates: &mut crate::wire::uplink::Updates, session: &str, s: &crate::harness::session::Session, kept: Option<&proto::KeptSession>, sent: &mut SentChanges) {
    use crate::harness::core::Update;
    let key = crate::wire::uplink::session_key(session);
    updates.start(&key);
    let (snapshot, delta) = go_on(s.snapshot(), kept);
    // Its bytes, and of them its newest meta, state and changes, and its events after the copy's.
    let (mut bytes, mut newest, mut after) = (0, 0, 0);
    for u in &snapshot {
        let n = push_update(updates, session, u, true, sent);
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

/// Capability negotiation and private extensions belong to the actual TLS connection that
/// declared the reconciled layout. A forwarded command never supplies that proof.
fn server_layout_on(msg: &ServerMsg, origin: Option<crate::net::Connection>, current: Option<crate::net::Connection>, layout: Option<crate::net::Connection>) -> Result<(), &'static str> {
    let on_current = origin.is_some() && origin == current;
    if let ServerMsg::Caps { caps } = msg {
        if !on_current { return Err("capability declaration is not from the current authenticated server connection"); }
        proto::check_unified_layout(*caps)?;
    }
    if proto::server_extension(msg) && (!on_current || layout != origin) {
        return Err("private protocol extension before this connection validated its unified layout");
    }
    Ok(())
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
/// How long after asking for an agent session's identity it's asked for again, at first; each time
/// after, twice as long, up to [`AGENT_IDENTITY_MOST`].
const AGENT_IDENTITY_AGAIN: Duration = Duration::from_secs(5);
const AGENT_IDENTITY_MOST: Duration = Duration::from_secs(600);

/// Asks for agent session `session`'s identity at `epoch` (docs/ci.md W27-R01), unless this
/// process holds one or asked lately ([`AGENT_IDENTITY_AGAIN`], doubling each time). A session kiln
/// hasn't heard of yet is refused (the ask can come before its Open, which rides the channel), and
/// asked for again as it goes on and on the heartbeat, so one that goes quiet gets it too.
async fn ask_agent_identity(uplink: &Uplink, asked: &mut HashMap<String, (Instant, Duration)>, session: &str, epoch: u64) {
    let slot = crate::runner::identity::slot("agent", Some(session), Some(epoch));
    if crate::runner::identity::cert(&slot).is_some() || asked.get(&slot).is_some_and(|(at, wait)| at.elapsed() < *wait) {
        return;
    }
    let wait = asked.get(&slot).map_or(AGENT_IDENTITY_AGAIN, |(_, wait)| wait.saturating_mul(2).min(AGENT_IDENTITY_MOST));
    asked.insert(slot, (Instant::now(), wait));
    match crate::runner::identity::ask("agent", Some(session), Some(epoch)) {
        Ok(json) => {
            uplink.0.send(Up::Term(AgentMsg::Identity { json })).await.ok();
        }
        Err(e) => tracing::warn!(session = %session, error = %format!("{e:#}"), "no identity asked for"),
    }
}

/// Sends what the channel has to send now.
fn flush_updates(updates: &mut crate::wire::uplink::Updates, uplink: &Uplink) -> anyhow::Result<()> {
    for f in updates.poll(Instant::now()) {
        match uplink.0.try_send(Up::Term(AgentMsg::Wire(f))) {
            Ok(()) => {}
            // U03: Updates retains unacknowledged frames for its next retransmission poll.
            // The main loop must consume this channel before it can send any more itself.
            Err(async_channel::TrySendError::Full(_)) => break,
            Err(async_channel::TrySendError::Closed(_)) => anyhow::bail!("agent uplink is closed"),
        }
    }
    Ok(())
}

/// Most of one agent-session update a frame carries. They go on the interactive lane, which
/// refuses frames past 1 MiB ([`net::Kiln`]); a resumed session's history can be several.
const HARNESS_FRAME: usize = 512 * 1024;

/// What a runner sends the server of a message of the server's whose payload it couldn't read
/// (`crate::wire::payload`): word of it when `told` says (`Unread`, with the session it was about
/// and how many it folded before it: `wire::unread::Told`, for a server that takes it), and what
/// waits on it told why (a question answered so, a stream said to have failed, a session said
/// refused). A send whose id could be read its session's inbox answers instead
/// ([`unread_send`]). The command it came in, when it came in one, is NACKed besides ([`refuse`]).
pub(crate) fn unread_payload(e: &crate::wire::payload::Unreadable, told: Option<u64>) -> Vec<Up> {
    let mut out = Vec::new();
    if let Some(folded) = told {
        out.push(Up::Msg(AgentMsg::Unread { wire: crate::wire::unread::Wire::ServerPayload, what: e.what.clone(), code: e.code, why: e.why.clone(), folded, session: e.session.clone() }));
    }
    match &e.waits {
        Some(Waits::Asking(req)) => out.push(Up::Term(AgentMsg::Answer { req: *req, json: Some(e.error().to_string()) })),
        Some(Waits::Stream(stream)) => out.push(Up::Stream(AgentMsg::StreamEvent { stream: stream.clone(), json: e.error().to_string() })),
        Some(Waits::Opening(session)) => {
            for u in crate::harness::host::refusal(session, &crate::harness::session::Spec::default(), &e.to_string()) {
                if let Ok(json) = serde_json::to_string(&u) {
                    out.push(Up::Term(AgentMsg::Harness { session: session.clone(), json }));
                }
            }
        }
        Some(Waits::Answering(_) | Waits::Sending { .. }) | None => {}
    }
    out
}

/// A send of the server's this runner couldn't read, its id read (`Waits::Sending`): with its
/// session here, down its lane, whose inbox answers it (`crate::harness::host::unread_message`), so
/// a retry of a message it took is that message's repeat, never a second one; a full lane refuses
/// it `full`, to be sent again, and one ending says nothing, as for a send it could read. Its
/// session not here: refused so, and said gone.
pub(crate) fn unread_send(harness: Option<&crate::harness::host::Host>, e: &crate::wire::payload::Unreadable) -> Vec<Up> {
    let Some(Waits::Sending { session, message, epoch }) = &e.waits else { return Vec::new() };
    if let Some(h) = harness.filter(|h| h.get(session).is_some()) {
        h.unreadable_at(session, message.clone(), Some(*epoch), format!("{}: {}", e.code.name(), e.why));
        return Vec::new();
    }
    let refused = crate::harness::inbox::Said { message: message.clone(), answer: Err(crate::harness::inbox::Refusal::NoSession { session: session.clone() }) };
    [crate::harness::core::Update::Inbox { said: Box::new(refused) }, crate::harness::core::Update::Closed]
        .iter()
        .filter_map(|u| serde_json::to_string(u).ok())
        .map(|json| Up::Term(AgentMsg::Harness { session: session.clone(), json }))
        .collect()
}

/// The command being applied on the channel (`answer_after`) is refused, saying how and why: a
/// message that came alone has no one to tell.
fn refuse(answer_after: &mut Option<(String, u64, Option<(crate::wire::channel::Refusal, String)>)>, code: crate::wire::channel::Refusal, why: impl Into<String>) {
    if let Some((_, _, refusal)) = answer_after.as_mut() {
        *refusal = Some((code, why.into()));
    }
}

/// An agent-session update as frames that fit: a long run of events goes in parts (events are
/// upserts by id, so the parts mean what the whole did), and an event too large on its own has
/// its longest texts cut, saying so. Anything else too large (a state) is cut the same way:
/// dropped, its page would go on showing what no longer is, the agent looking stuck. A state still
/// too large sheds its finished tasks, the oldest first; a changeset leaves out its largest diffs,
/// their counts kept (`fit_changes`), and a changeset's patch goes in parts. Only what still doesn't
/// fit is dropped, and logged.
fn harness_frames(session: &str, u: &crate::harness::core::Update) -> Vec<String> {
    use crate::harness::core::Update;
    let Ok(json) = serde_json::to_string(u) else { return Vec::new() };
    if json.len() <= HARNESS_FRAME {
        return vec![json];
    }
    // A changeset fits with its largest diffs left out; a patch of one goes in as many frames as
    // it takes. Neither is dropped: a changeset's page would go on showing what no longer is.
    if let Update::Changes { files } = u {
        let fitted = fit_changes(files);
        match &fitted {
            Some(j) => tracing::debug!(session, bytes = json.len(), sent = j.len(), "agent session changeset too large whole: its largest diffs left out"),
            None => tracing::warn!(session, bytes = json.len(), "agent session changeset too large to send, even as counts; dropped"),
        }
        return fitted.into_iter().collect();
    }
    if let Update::ChangesPatch { set, gone } = u {
        return patch_frames(set, gone.clone());
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
fn hold_agent_limit(h: &crate::harness::host::Host, held: crate::harness::limit::Held, said: &mut Option<crate::harness::limit::Held>, tell: bool, uplink: &Uplink, deferred: &mut DeferredControl) -> anyhow::Result<()> {
    if h.set_max_sessions(held.max_sessions) {
        tracing::info!(max_sessions = held.max_sessions, from = ?held.from, "agent sessions: this machine has at most this many at work at once");
    }
    if tell
        && *said != Some(held)
        && let Ok(json) = serde_json::to_string(&held)
    {
        deferred.enqueue(&uplink.0, Up::Msg(AgentMsg::AgentLimit { json }))?;
        *said = Some(held);
    }
    Ok(())
}

/// U16: the same authenticated server connection negotiated the controlled-upgrade law.
fn controlled_upgrade_on(origin: Option<crate::net::Connection>, current: Option<crate::net::Connection>, announced: Option<crate::net::Connection>) -> bool {
    origin.is_some() && origin == current && announced == origin
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

/// How this process started, as far as its terminals go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    /// Nothing was handed over.
    Fresh,
    /// A previous image of this process re-executed itself into this one (the legacy in-place path).
    ReExecuted,
    /// The supervisor started this process with the terminals the runner before it handed over.
    Successor,
}

/// Take over the terminals handed over before this process started: by a previous image of it
/// that re-executed itself (the legacy in-place path, the masters kept at the numbers the handover
/// file says), or by the runner process before it, through the supervisor (the legacy handover file,
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


#[cfg(test)]
mod tests {
    #[test]
    fn u03_inherited_pod_factory_fences_uid_node_grant_and_job_attempt() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let work = dir.path().join("unopened-work");
        let bootstrap = ExecutorBootstrap {
            identity: crate::k8s::ExecutorIdentity { namespace: "jobs".into(), name: "executor".into(), uid: "pod-uid".into(), node: "node".into(), executor: "slot".into(), generation: "generation".into(), allocation: "existing-grant".into() },
            certificate_sha256: "a".repeat(64),
        };
        let binding = kiln_ci::wire::ParentBinding {
            executor: bootstrap.identity.executor.clone(), generation: bootstrap.identity.generation.clone(), allocation: bootstrap.identity.allocation.clone(),
            physical_execution: bootstrap.identity.physical_execution()?, job: "leg".into(), attempt: 2,
        };
        let assignment = kiln_ci::wire::AssignmentV2 { job_id: "leg".into(), attempt: 2, parent_binding: Some(binding.clone()), ..Default::default() };
        assert!(bootstrap.execution_for(crate::runner::processes::Admission::production(), &assignment, work.clone()).is_ok());
        for changed in [
            kiln_ci::wire::ParentBinding { executor: "another-slot".into(), ..binding.clone() },
            kiln_ci::wire::ParentBinding { generation: "another-generation".into(), ..binding.clone() },
            kiln_ci::wire::ParentBinding { allocation: "another-grant".into(), ..binding.clone() },
            kiln_ci::wire::ParentBinding { physical_execution: "another-physical-owner".into(), ..binding.clone() },
            kiln_ci::wire::ParentBinding { job: "another-leg".into(), ..binding.clone() },
            kiln_ci::wire::ParentBinding { attempt: 1, ..binding },
        ] {
            let changed = kiln_ci::wire::AssignmentV2 { parent_binding: Some(changed), ..assignment.clone() };
            assert!(bootstrap.execution_for(crate::runner::processes::Admission::production(), &changed, work.clone()).is_err());
        }
        let mut replaced = bootstrap.clone();
        replaced.identity.uid = "replacement-uid".into();
        assert!(replaced.execution_for(crate::runner::processes::Admission::production(), &assignment, work.clone()).is_err());
        replaced = bootstrap.clone();
        replaced.identity.node = "another-node".into();
        assert!(replaced.execution_for(crate::runner::processes::Admission::production(), &assignment, work.clone()).is_err());
        let absent = kiln_ci::wire::AssignmentV2 { parent_binding: None, ..assignment };
        assert!(bootstrap.execution_for(crate::runner::processes::Admission::production(), &absent, work.clone()).is_err());
        assert!(!work.try_exists()?);
        Ok(())
    }

    #[test]
    fn u03_executor_bootstrap_requires_the_exact_issued_certificate() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open(&dir.path().join("pki"))?;
        let leaf = pki.issue_local("executor")?;
        let replacement = pki.issue_local("executor")?;
        let bootstrap = ExecutorBootstrap {
            identity: crate::k8s::ExecutorIdentity { namespace: "jobs".into(), name: "executor".into(), uid: "uid".into(), node: "node".into(), executor: "slot".into(), generation: "generation".into(), allocation: "existing-grant".into() },
            certificate_sha256: crate::pki::cert_info(&leaf.cert_pem)?.sha256,
        };
        crate::util::write_secret(&dir.path().join("executor-bootstrap.json"), &serde_json::to_vec(&Some(bootstrap.clone()))?, 0o600)?;
        assert_eq!(enrolled_executor(dir.path(), &leaf)?.map(|b| b.identity), Some(bootstrap.identity));
        assert!(enrolled_executor(dir.path(), &replacement).is_err());
        Ok(())
    }

    #[test]
    fn u17_reserved_main_instance_refuses_before_opening_either_state_path() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = dir.path().join("state");
        let work = dir.path().join("work");
        let options = Opts {
            server_url: "http://127.0.0.1:1".into(), quic: None, name: "runner".into(),
            labels: Vec::new(), slots: 1, work_dir: work.clone(), state_dir: state.clone(),
            shared_store: None, join_token: None, ephemeral: false, dedicated_job: None,
            terminal: true, job_terminal: false, shell: None, credentials: None,
            instance: Some(crate::upgrade::MAIN.into()), config: None, reach: None, observe: false,
            outpost: false, declared: crate::runner::probe::Declared::default(),
        };
        let error = crate::rt::block_on(run(options, Shutdown::default())).err().ok_or_else(|| anyhow::anyhow!("reserved main instance unexpectedly started"))?;
        assert!(error.to_string().contains("reserved"));
        assert!(!state.try_exists()?);
        assert!(!state.join("instances").join(crate::upgrade::MAIN).try_exists()?);
        assert!(!work.try_exists()?);
        Ok(())
    }

    #[test]
    fn u17_instance_revision_namespaces_accept_arbitrary_ids_without_sharing_history() -> anyhow::Result<()> {
        let first = super::runner_records_namespace("runner-a", Some("UPGRADE/实例"));
        amphora_api::family(&first)?;
        assert_eq!(first, super::runner_records_namespace("runner-a", Some("UPGRADE/实例")));
        assert_ne!(first, super::runner_records_namespace("runner-a", Some("upgrade/实例")));
        assert_ne!(first, super::runner_records_namespace("runner-b", Some("UPGRADE/实例")));
        let main = super::runner_records_namespace("runner-a", None);
        assert_ne!(first, main);
        assert_eq!(super::runner_records_namespace("runner-a", Some(crate::upgrade::MAIN)), main);
        assert_ne!("host", main);
        amphora_api::family(&main)?;
        let machine = super::runner_machine_namespace("runner-a");
        amphora_api::family(&machine)?;
        assert_ne!(machine, main);
        assert_ne!(machine, super::runner_machine_namespace("runner-b"));
        // U05: one configured host path, with retained state/work/instance import identities.
        let root = tempfile::tempdir()?;
        let (host, state, work) = (root.path().join("host"), root.path().join("retained"), root.path().join("work"));
        let mut config = crate::config::Config::default();
        config.server.data_dir = host.clone();
        assert_eq!(configured_host_database(&config, &state, &work)?, host.join("kiln.db"));
        assert!(!host.exists() && !state.exists() && !work.exists(), "path preflight creates nothing");
        let instance = state.join("instances/upgrade");
        std::fs::create_dir_all(&instance)?;
        std::fs::create_dir_all(work.join("_cas"))?;
        let imports = [state.join(crate::convo::FILE), state.join(crate::records::FILE), instance.join(crate::convo::FILE), instance.join(crate::records::FILE), work.join("_cas/ledger.db")];
        for path in &imports { std::fs::write(path, b"retained legacy input")?; }
        assert_eq!(configured_host_database(&config, &state, &work)?, host.join("kiln.db"));
        for path in [state.join("kiln.db"), state.join("amphora.db"), instance.join("amphora.db"), work.join("_cas/amphora.db"), work.join("amphora.db")] {
            std::fs::write(&path, b"distinct authoritative aggregate")?;
            let refused = configured_host_database(&config, &state, &work).err().ok_or_else(|| anyhow::anyhow!("distinct aggregate accepted"))?;
            assert!(refused.downcast_ref::<HostMigrationNeeded>().is_some());
            assert_eq!(std::fs::read(&path)?, b"distinct authoritative aggregate");
            assert!(!host.exists(), "migration refusal precedes canonical creation");
            std::fs::remove_file(path)?;
        }
        for path in &imports { assert_eq!(std::fs::read(path)?, b"retained legacy input"); }
        #[cfg(unix)]
        {
            let cluster = state.join("cluster");
            std::fs::create_dir(&cluster)?;
            std::os::unix::fs::symlink("missing-retained-log", cluster.join("cluster.log"))?;
            let refused = configured_host_database(&config, &state, &work).err().ok_or_else(|| anyhow::anyhow!("dangling retained log accepted"))?;
            assert!(refused.downcast_ref::<HostMigrationNeeded>().is_some());
            assert!(std::fs::symlink_metadata(cluster.join("cluster.log"))?.file_type().is_symlink());
            assert!(!host.exists());
            std::fs::remove_file(cluster.join("cluster.log"))?;
            std::fs::remove_dir(cluster)?;
            // Cache authority must survive identity dedup with an earlier state root.
            let alias_work = root.path().join("alias-work");
            std::fs::create_dir(&alias_work)?;
            std::os::unix::fs::symlink(&state, alias_work.join("_cas"))?;
            std::fs::create_dir(state.join("ledger.db"))?;
            let refused = configured_host_database(&config, &state, &alias_work).err().ok_or_else(|| anyhow::anyhow!("aliased fenced cache accepted"))?;
            assert!(refused.downcast_ref::<HostMigrationNeeded>().is_some());
            assert!(state.join("ledger.db").is_dir() && !host.exists());
            std::fs::remove_dir(state.join("ledger.db"))?;
        }
        let canonical_cache_work = root.path().join("canonical-cache-work");
        let canonical_cache = canonical_cache_work.join("_cas");
        std::fs::create_dir_all(canonical_cache.join("ledger.db"))?;
        config.server.data_dir = canonical_cache.clone();
        let refused = configured_host_database(&config, &state, &canonical_cache_work).err().ok_or_else(|| anyhow::anyhow!("canonical cache hid retained ledger authority"))?;
        assert!(refused.downcast_ref::<HostMigrationNeeded>().is_some());
        assert!(canonical_cache.join("ledger.db").is_dir());
        assert!(!canonical_cache.join("kiln.db").exists() && !canonical_cache.join("amphora.db").exists(), "refusal precedes fresh canonical creation");
        config.server.data_dir = state.clone();
        std::fs::write(state.join("amphora.db"), b"existing canonical aggregate")?;
        assert_eq!(configured_host_database(&config, &state, &work)?, state.join("kiln.db"));
        assert_eq!(std::fs::read(state.join("amphora.db"))?, b"existing canonical aggregate");
        config.server.data_dir = PathBuf::from("u05-relative-configured-host");
        assert_eq!(configured_host_database(&config, &root.path().join("fresh-state"), &root.path().join("fresh-work"))?, config.server.data_dir.join("kiln.db"), "relative config paths keep server CWD semantics");
        Ok(())
    }

    use super::*;

    #[test]
    fn u03_full_uplink_defers_completion_behind_retained_events_without_blocking() -> anyhow::Result<()> {
        let (tx, rx) = async_channel::bounded(2);
        for json in [r#"{"t":"kept_owned","execution":"exec","attempt":3,"until":10}"#, r#"{"t":"kept","until":10}"#] {
            tx.try_send(Up::Msg(AgentMsg::JobEvent { job: "leg".into(), json: json.into() }))?;
        }
        let result = JobResult { job: "leg".into(), status: Status::Failed, exit_code: Some(1), error: None, peak_rss: None, cpu_seconds: None };
        let completion = AgentMsg::JobFinishedV2 { result, attempt: 3 };
        // Completion replay storage precedes enqueue, just as Ev::Done does.
        let replay = proto::encode(&completion)?;
        let mut deferred = DeferredControl::default();
        deferred.enqueue(&tx, Up::Msg(completion.clone()))?;
        deferred.enqueue(&tx, Up::Msg(AgentMsg::Heartbeat { free_slots: 1, running: vec![] }))?;
        assert_eq!(deferred.pending.len(), 2);
        assert!(matches!(rx.try_recv()?, Up::Msg(AgentMsg::JobEvent { json, .. }) if json.contains("kept_owned")));
        deferred.flush(&tx)?;
        assert!(matches!(rx.try_recv()?, Up::Msg(AgentMsg::JobEvent { json, .. }) if json.contains("kept")));
        deferred.flush(&tx)?;
        assert!(matches!(rx.try_recv()?, Up::Msg(AgentMsg::JobFinishedV2 { attempt: 3, .. })));
        assert!(matches!(rx.try_recv()?, Up::Msg(AgentMsg::Heartbeat { .. })));
        assert!(deferred.pending.is_empty());
        assert_eq!(proto::read_agent(&replay).map_err(|e| anyhow::anyhow!("{}: {}", e.what(), e.why("AgentMsg")))?, completion);
        crate::rt::block_on(async {
            use crate::wire::channel::{Receiver,Has};
            use crate::harness::model::Command;
            struct Target(async_channel::Sender<Command>);
            impl crate::harness::host::Taker for Target {
                fn is_moving(&self)->bool {false}
                fn epoch(&self)->u64 {7}
                fn hand(&self,c:Command)->bool {self.0.try_send(c).is_ok()}
            }
            let origin=crate::net::Connection::new(SERVER_PEER,9,[9;32]);
            let nonce="u".repeat(43);let stream="l:9.1";
            let mut receiver=Receiver::new();
            receiver.bind_ordinary(origin,&nonce,Has{stream:stream.into(),upto:0,nacks:vec![]},0)?;
            receiver.stage_ordinary(origin,&nonce,stream,1,0,b"held Start".to_vec())?;
            let head=receiver.ordinary_head(origin,&nonce,stream)?.context("ordinary test head missing")?;
            receiver.stage_ordinary_urgent(origin,&nonce,stream,2,0,b"interrupt".to_vec())?;
            receiver.stage_ordinary_urgent(origin,&nonce,stream,3,0,b"force stop".to_vec())?;
            let (release,held)=futures::channel::oneshot::channel::<()>();
            let mut io:Option<OrdinaryIo>=Some(async move {
                let _=held.await;
                OrdinaryDone::Registered{origin,result:Box::new(Err(anyhow::anyhow!("held storage result")))}
            }.boxed());
            let (controls,events)=async_channel::bounded(2);
            controls.try_send(Command::Interrupt)?;controls.try_send(Command::ForceStop)?;
            let (taken,applied)=async_channel::bounded(2);let target=Target(taken);
            for seq in [2,3] {
                // The exact production retained future stays in its slot when ingress wins.
                let command=futures::select_biased! {
                    _=ordinary_completion(&mut io).fuse()=>anyhow::bail!("held durability completed before release"),
                    command=events.recv().fuse()=>command?,
                };
                let ticket=receiver.ordinary_urgent(origin,&nonce,stream,seq)?.context("urgent test ticket missing")?;
                complete_ordinary_urgent(&mut receiver,&ticket,&target,command)?;
                assert!(receiver.ordinary_urgent(origin,&nonce,stream,seq)?.is_none(),"replay cannot repeat Interrupt or ForceStop");
            }
            assert_eq!(applied.try_recv()?,Command::Interrupt);
            assert_eq!(applied.try_recv()?,Command::ForceStop);
            assert!(applied.is_empty());
            assert_eq!(receiver.state(stream).upto,0,"neither Start nor urgent controls have a durable ACK");
            assert!(receiver.ordinary_ready(origin,&nonce,stream)?.is_none(),"held Start cannot be bypassed by ACK/Have");
            assert!(io.is_some(),"select did not drop the pending owner");
            release.send(()).map_err(|_|anyhow::anyhow!("held storage owner disappeared"))?;
            // A1: readiness is progress, even when ingress is also ready. The four
            // production select modes use this SAME completion-before-Net ordering.
            let mut recovery_tick=Interval::new(Duration::from_secs(5));
            controls.try_send(Command::Interrupt)?;
            let completed=futures::select_biased! {
                result=ordinary_completion(&mut io).fuse()=>result,
                ()=recovery_tick.tick().fuse()=>anyhow::bail!("recovery tick overtook ready retained I/O"),
                _=events.recv().fuse()=>anyhow::bail!("ready ingress starved the ready retained owner"),
            };
            assert!(matches!(completed,OrdinaryDone::Registered{..}));
            assert_eq!(events.len(),1,"known I/O completion did not consume urgent ingress");
            assert!(applied.is_empty(),"servicing durability cannot fabricate another control effect");
            // U01/U03: a due recovery opportunity cannot wait behind ready production
            // ingress once the retained owner has completed. It changes latches only.
            io=None;
            let (mut paused,mut sample_due)=(true,false);
            let recovered=futures::select_biased! {
                _=ordinary_completion(&mut io).fuse()=>anyhow::bail!("idle I/O completed"),
                ()=recovery_tick.tick().fuse()=>Ev::OrdinaryRecovery,
                _=events.recv().fuse()=>anyhow::bail!("ready ingress starved due recovery"),
            };
            match recovered {
                Ev::OrdinaryRecovery=>recover_ordinary(&mut paused,&mut sample_due),
                _=>anyhow::bail!("unexpected recovery event"),
            }
            assert!(!paused && sample_due,"the same paused closure gets a bounded retry opportunity");
            assert_eq!(events.len(),1,"pure recovery does not consume urgent ingress");
            assert!(applied.is_empty(),"recovery cannot invent an effect or durable ACK");
            // The immediate first tick is consumed even while healthy. The next tick
            // must wait; another failure cannot rearm an immediately-ready retry loop.
            paused=true;sample_due=false;
            let command=futures::select_biased! {
                ()=recovery_tick.tick().fuse()=>anyhow::bail!("recovery cadence retried hot"),
                command=events.recv().fuse()=>command?,
            };
            assert_eq!(command,Command::Interrupt);
            assert!(paused && !sample_due,"pending recovery still permits urgent ingress");
            let (mut healthy,mut healthy_sample)=(false,false);
            recover_ordinary(&mut healthy,&mut healthy_sample);
            assert!(!healthy && !healthy_sample,"healthy recovery ticks add no spool sample");
            // The existing failed retained-publication deadline also precedes ingress,
            // keeping exactly its original Up value outside the ordinary recovery latch.
            controls.try_send(Command::ForceStop)?;
            let retry_at=Instant::now();
            let retried=futures::select_biased! {
                _=ordinary_completion(&mut io).fuse()=>anyhow::bail!("idle I/O completed"),
                ()=sleep_until_opt(Some(retry_at)).fuse()=>true,
                ()=recovery_tick.tick().fuse()=>anyhow::bail!("future recovery tick completed"),
                _=events.recv().fuse()=>false,
            };
            assert!(retried,"ready ingress starved the original retained-publication deadline");
            assert_eq!(events.try_recv()?,Command::ForceStop);
            // A known completion records the SAME head, then the contiguous prefix only.
            receiver.ordinary_admitted(&head)?;receiver.ordinary_effect_done(&head,None)?;
            let closure=receiver.ordinary_ready(origin,&nonce,stream)?.context("ordered completion missing")?;
            assert_eq!(closure.has().upto,3);
            receiver.retire_ordinary(origin);
            receiver.bind_ordinary(origin,&nonce,Has{stream:"l:9.2".into(),upto:0,nacks:vec![]},0)?;
            assert!(receiver.ordinary_committed(&closure).is_err(),"same-origin later generation cannot use old completion");
            anyhow::Ok(())
        })?;
        Ok(())
    }

    /// U15/U03: a server's Caps wrapped in a forwarded item cannot claim an authenticated
    /// layout, and a new TLS connection cannot reuse the old one's private receipt authority.
    #[test]
    fn u15_forwarded_caps_and_replaced_server_cannot_supply_private_layout() -> anyhow::Result<()> {
        let current = crate::net::Connection::new(SERVER_PEER, 2, [2; 32]);
        let former = crate::net::Connection::new(SERVER_PEER, 1, [1; 32]);
        let body = proto::encode(&ServerMsg::Caps { caps: proto::CAP_UNIFIED_LAYOUT | proto::CAP_RETAINED_RECEIVED })?;
        let inner = proto::read_server(&body).map_err(|e| anyhow::anyhow!("{}: {}", e.what(), e.why("ServerMsg")))?;
        assert!(server_layout_on(&inner, None, Some(current), None).is_err(), "an unwrapped forwarded Caps supplies no TLS origin");
        assert!(server_layout_on(&inner, Some(former), Some(current), Some(former)).is_err(), "old Caps cannot rebind proof");
        assert!(server_layout_on(&inner, Some(current), Some(current), None).is_ok(), "the actual server may negotiate");
        assert!(!controlled_upgrade_on(Some(current), Some(current), None), "an old server cannot activate a runner");
        assert!(!controlled_upgrade_on(None, Some(current), Some(current)), "forwarded frames cannot acquire activation authority");
        assert!(!controlled_upgrade_on(Some(former), Some(current), Some(former)), "a stale connection cannot activate");
        assert!(!controlled_upgrade_on(Some(current), Some(current), Some(former)), "a new connection must negotiate again");
        assert!(controlled_upgrade_on(Some(current), Some(current), Some(current)));
        let receipt = ServerMsg::ReceivedV2 { job: "leg".into(), attempt: 3 };
        assert!(server_layout_on(&receipt, Some(current), Some(current), Some(former)).is_err(), "a replacement cannot retire evidence on an old proof");
        assert!(server_layout_on(&receipt, None, Some(current), Some(current)).is_err(), "a forwarded receipt has no current TLS origin");
        assert!(server_layout_on(&receipt, Some(current), Some(current), Some(current)).is_ok());
        Ok(())
    }

    #[test]
    fn u03_stale_receipt_cannot_clear_current_completion() -> anyhow::Result<()> {
        let result = JobResult { job: "leg".into(), status: Status::Succeeded, exit_code: Some(0), error: None, peak_rss: None, cpu_seconds: None };
        let kept = proto::encode(&AgentMsg::JobFinishedV2 { result, attempt: 9 })?;
        assert!(!completion_receipt(&kept, Some(8), true));
        assert!(!completion_receipt(&kept, None, true), "new servers cannot clear a fenced attempt with an old receipt");
        assert!(completion_receipt(&kept, Some(9), true));
        assert!(completion_receipt(&kept, None, false), "old servers still acknowledge their legacy encoding");
        Ok(())
    }

    #[test]
    fn u03_completion_replay_preserves_attempt_across_old_server_encoding() -> anyhow::Result<()> {
        let result = JobResult { job: "leg".into(), status: Status::Succeeded, exit_code: Some(0), error: None, peak_rss: None, cpu_seconds: None };
        let message = AgentMsg::JobFinishedV2 { result: result.clone(), attempt: 9 };
        let kept = proto::encode(&message)?;
        assert_eq!(proto::read_agent(&completion_wire(kept.clone(), false)).map_err(|e| anyhow::anyhow!("{}: {}", e.what(), e.why("AgentMsg")))?, AgentMsg::JobFinished(result));
        assert_eq!(proto::read_agent(&completion_wire(kept.clone(), true)).map_err(|e| anyhow::anyhow!("{}: {}", e.what(), e.why("AgentMsg")))?, message);
        assert_eq!(proto::read_agent(&kept).map_err(|e| anyhow::anyhow!("{}: {}", e.what(), e.why("AgentMsg")))?, message, "legacy transmission cannot erase replay identity");
        Ok(())
    }


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

    /// A long session's snapshot, larger than a stream's room (a Codex session's was 10 to 16 MB
    /// against 8 MiB), starts its stream whole as its picture, and its updates go on after it.
    /// Pushed against the room, as it was, it went stale part-way, and the stream started over
    /// with the same snapshot at each frame from the server, without end: the server never heard
    /// from the session again.
    #[test]
    fn a_snapshot_larger_than_a_streams_room_starts_it() {
        let mut updates = crate::wire::uplink::Updates::new("boot");
        let history = Update::Events { events: (0..5000).map(|i| said(i, "x".repeat(2000))).collect() };
        let frames = harness_frames("s", &history).len();
        let key = crate::wire::uplink::session_key("s");
        let mut sent = SentChanges::default();
        updates.start(&key);
        let bytes = push_update(&mut updates, "s", &history, true, &mut sent);
        assert!(bytes > crate::wire::uplink::ROOM, "{bytes} bytes");
        push_update(&mut updates, "s", &Update::Events { events: vec![said(5000, "after".into())] }, false, &mut sent);
        assert!(updates.take_stale().is_empty(), "its picture went whole");
        assert_eq!(updates.unanswered(), frames + 1, "the snapshot and what came after it");
        // As it was pushed before: held to the room, it goes stale part-way.
        let other = crate::wire::uplink::session_key("t");
        updates.start(&other);
        push_update(&mut updates, "t", &history, false, &mut sent);
        assert_eq!(updates.take_stale(), vec![other]);
    }

    /// A file of a changeset with `lines` added lines, each about as long as a line of code.
    fn change(path: &str, lines: usize) -> crate::harness::model::FileChange {
        let n = u32::try_from(lines).unwrap_or(u32::MAX);
        let lines = (0..lines).map(|i| format!("+    let value_{i} = compute(argument_{i}, &context); // line {i}")).collect();
        crate::harness::model::FileChange { path: path.into(), tree: None, created: false, deleted: false, added: n, removed: 0, hunks: vec![crate::harness::model::Hunk { old_start: 1, old_lines: 0, new_start: 1, new_lines: n, lines }], truncated: false, at: 1 }
    }

    /// A long session's changeset, as one's was: 43 files whose diffs are thousands of short lines,
    /// about 700 KB together.
    fn long_changeset() -> Vec<crate::harness::model::FileChange> {
        (1..=43).map(|i| change(&format!("crates/kiln/src/f{i:02}.rs"), i * 10)).collect()
    }

    /// A changeset too large for a frame goes whole all the same, to a server that takes no patches:
    /// its largest files as their counts only ("Diff shortened"), the rest as they are. Cut as other
    /// updates are, its short lines left it too large, and it was dropped: its page showed the
    /// changeset as it was before.
    #[test]
    fn a_changeset_too_large_for_a_frame_goes_with_its_largest_diffs_left_out() -> anyhow::Result<()> {
        let files = long_changeset();
        let whole = serde_json::to_string(&Update::Changes { files: files.clone() })?;
        anyhow::ensure!(whole.len() > HARNESS_FRAME, "{} bytes", whole.len());
        let frames = harness_frames("s", &Update::Changes { files: files.clone() });
        let [frame] = frames.as_slice() else { anyhow::bail!("{} frames", frames.len()) };
        anyhow::ensure!(frame.len() <= HARNESS_FRAME);
        let Ok(Update::Changes { files: sent }) = serde_json::from_str::<Update>(frame) else { anyhow::bail!("not a changeset: {}", frame.get(..80).unwrap_or_default()) };
        // Every file, each with its counts.
        anyhow::ensure!(sent.iter().map(|f| (&f.path, f.added)).eq(files.iter().map(|f| (&f.path, f.added))));
        // The largest left out, the smallest whole.
        anyhow::ensure!(sent.last().is_some_and(|f| f.truncated && f.hunks.is_empty()));
        anyhow::ensure!(sent.first() == files.first());
        Ok(())
    }

    /// To a server that takes patches, a changeset goes as what changed in it: its picture whole, in
    /// as many frames as it takes, every diff kept; then only the file an agent touched, or the one
    /// it put back. The server's copy, taking them in, is the session's changeset each time; an Open
    /// (the copy starting its changeset over) has it sent whole again.
    #[test]
    fn a_changeset_goes_as_what_changed_in_it_to_a_server_that_takes_that() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = crate::harness::registry::Location::Machine("box".into());
            let info = crate::harness::session::Info { id: "s".into(), kind: "codex".into(), ..Default::default() };
            hub.harness.ingest(&hub, &there, "s", Update::Open { info: Box::new(info.clone()), since: None });
            let mut sent = SentChanges::default();
            sent.server(true);
            let take = |frames: &[String]| -> anyhow::Result<()> {
                for f in frames {
                    anyhow::ensure!(f.len() <= HARNESS_FRAME, "a frame of {} bytes", f.len());
                    hub.harness.ingest(&hub, &there, "s", serde_json::from_str(f)?);
                }
                Ok(())
            };
            let copy = || hub.harness.conclusion("s").map(|(_, _, changes)| changes).unwrap_or_default();
            let mut files = long_changeset();
            let frames = sent.frames("s", &files, true);
            anyhow::ensure!(frames.len() > 1 && frames.first().is_some_and(|f| f.starts_with(r#"{"t":"changes""#)));
            take(&frames)?;
            anyhow::ensure!(copy() == serde_json::to_value(&files)?, "the picture, whole, every diff kept");
            // The agent edits one file of 43: one small frame, that file.
            if let Some(f) = files.get_mut(20) {
                *f = change(&f.path, 300);
            }
            let frames = sent.frames("s", &files, false);
            let [edit] = frames.as_slice() else { anyhow::bail!("{} frames", frames.len()) };
            anyhow::ensure!(edit.starts_with(r#"{"t":"changes_patch""#) && edit.len() < 32 << 10, "{} bytes", edit.len());
            take(&frames)?;
            anyhow::ensure!(copy() == serde_json::to_value(&files)?, "the edit taken in place");
            // It puts one back: that file's key alone.
            let back = files.remove(5);
            let frames = sent.frames("s", &files, false);
            anyhow::ensure!(frames == vec![serde_json::to_string(&Update::ChangesPatch { set: Vec::new(), gone: vec![back.key()] })?]);
            take(&frames)?;
            anyhow::ensure!(copy() == serde_json::to_value(&files)?, "the file put back gone");
            anyhow::ensure!(sent.frames("s", &files, false).is_empty(), "nothing changed, nothing sent");
            // The copy starts over (an Open on the stream): every file again.
            let mut updates = crate::wire::uplink::Updates::new("boot");
            updates.start(&crate::wire::uplink::session_key("s"));
            push_update(&mut updates, "s", &Update::Open { info: Box::new(info), since: None }, true, &mut sent);
            let again: Vec<crate::harness::model::FileChange> = sent.frames("s", &files, false).iter().filter_map(|f| match serde_json::from_str::<Update>(f) { Ok(Update::ChangesPatch { set, .. }) => Some(set), _ => None }).flatten().collect();
            anyhow::ensure!(again == files);
            Ok(())
        })
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
