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
use futures::{FutureExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};

use crate::http::client::{Client, Roots};
use crate::rt::Shutdown;

use crate::exec::{Exec, Up, Uplink};
use crate::net::{self, Kind, NetEvent, Out, SERVER_PEER};
use crate::pki::Leaf;
use crate::proto::{self, AgentMsg, Hello, JobResult, LANE_CONTROL, LANE_CONVO, LANE_FILES, LANE_LOGS, LANE_OBSERVE, LANE_PORTS, LANE_REACH, LANE_STREAM, LANE_SYNC, LANE_TERM, ServerMsg};
use crate::wire::payload::{Carried, FromServer, Waits};
use crate::store::Status;
use crate::terminal::{self, PtyOut, PtySession};

/// The runner's loops for its agent sessions' frames (runner-fan).
mod loops;
pub(crate) mod redial;

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
fn probe_facts(uplink: &Uplink, work_dir: &std::path::Path, declared: &crate::runner::probe::Declared, probing: &Arc<crate::runner::probe::Probing>) {
    probe_facts_as(uplink, work_dir, declared, probing, true);
}

/// [`probe_facts`], `looked`: asked for by something that may have changed the machine, else by a
/// run's tool not found alone (W13-R01). One at a time: what asks while one runs gives one more
/// after it.
fn probe_facts_as(uplink: &Uplink, work_dir: &std::path::Path, declared: &crate::runner::probe::Declared, probing: &Arc<crate::runner::probe::Probing>, looked: bool) {
    start_facts_probe(uplink, work_dir, declared, probing, looked, false);
}

fn start_facts_probe(uplink: &Uplink, work_dir: &std::path::Path, declared: &crate::runner::probe::Declared, probing: &Arc<crate::runner::probe::Probing>, looked: bool, gpu_only: bool) {
    if !(if gpu_only { probing.take_gpu() } else { probing.take(looked) }) { return; }
    let (up, dir, declared, busy) = (uplink.clone(), work_dir.to_path_buf(), declared.clone(), Arc::clone(probing));
    crate::rt::spawn(async move {
        let mut look = busy.looks();
        loop {
            let facts = busy.read(&dir, &declared, look).await;
            if let Some(node) = crate::reach::node::global() {
                node.feed(crate::reach::node::In::LocalFacts(Box::new(facts.clone()))).await;
            }
            if let Ok(json) = serde_json::to_string(&facts) {
                up.msg(AgentMsg::Facts { json }).await;
            }
            match busy.done() {
                Some(again) => look = again,
                None => break,
            }
        }
    });
}

/// Why an outpost turns a job down (an older server may still hand it one).
const OUTPOST_TAKES_NO_JOBS: &str = "an outpost takes no jobs";

/// Why a runner draining, or about to upgrade, turns a job down: the server takes either as the
/// runner draining on its own (docs/ci.md W05-R35). Older runners say the same words.
pub(crate) const REFUSED_DRAINING: &str = "draining";
pub(crate) const REFUSED_UPGRADING: &str = "upgrading";

#[derive(Serialize, Deserialize)]
pub struct EnrollRequest {
    pub token: String,
    pub name: String,
    pub csr: String,
    /// The machine's hostname: a machine enrolling again keeps its name ([`crate::api`]).
    #[serde(default)]
    pub hostname: String,
    /// The machine's id ([`crate::os::machine_id`]): another machine with the same host name isn't
    /// taken for this one. None from a kiln from before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
}

/// `POST /api/agents/instance`: see `api::enroll_instance`.
#[derive(Serialize, Deserialize)]
pub struct InstanceRequest {
    pub cert_pem: String,
    pub csr: String,
}

/// `POST /api/agents/rekey` (docs/machines.md): a runner known by its name asks to be known by its
/// machine id, proving itself as a second runner process does; see `api::rekey_agent`.
#[derive(Serialize, Deserialize)]
pub struct RekeyRequest {
    pub cert_pem: String,
    pub csr: String,
    /// The machine's id ([`crate::os::machine_id`]): what it asks to be known by.
    pub machine_id: String,
}

#[derive(Serialize, Deserialize)]
pub struct EnrollResponse {
    pub cert_pem: String,
    pub ca_pem: String,
    pub quic: String,
    /// The name the certificate carries: the machine's id when it sent one (docs/machines.md).
    pub name: String,
    /// The name people see, when `name` is the machine's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

fn ca_der(pem: &str) -> anyhow::Result<Vec<u8>> {
    use hyper_quic::rustls::pki_types::CertificateDer;
    use hyper_quic::rustls::pki_types::pem::PemObject as _;
    Ok(CertificateDer::from_pem_slice(pem.as_bytes()).map_err(|e| anyhow::anyhow!("ca pem: {e:?}"))?.to_vec())
}

/// The certificate of a runner known by its name, for its machine id instead (docs/machines.md):
/// asked for as the runner starts, before it connects or holds anything, proving itself as a
/// second runner process does (`/api/agents/rekey`), and kept in place of the one it had. What the
/// server kept under its name goes to the id, and its name becomes its label. None when it's known
/// by its id already, has no id, is a job's pod or an outpost, or the server can't move it now (a
/// server from before, a process of it still connected): it goes on by its name, and asks again as
/// it starts next.
async fn moved_to_its_id(o: &Opts, cert: &str, key: &str, ca: &str, cert_p: &std::path::Path) -> Option<String> {
    use crate::http::client::{Client, Roots};
    if o.ephemeral || o.outpost {
        return None;
    }
    let id = crate::rt::blocking(crate::os::machine_id).await.ok().flatten()?;
    let to = crate::join::machine_key(Some(&id))?;
    let name = crate::pki::leaf_name(&Leaf { cert_pem: cert.to_owned(), key_pem: String::new() })?;
    // Known by a machine id already (its own, or the one of a machine it was copied from): it stays.
    if name == to || crate::join::is_machine_key(&name) {
        return None;
    }
    let csr = crate::pki::make_csr_with_key(&to, key).ok()?;
    let http = Client::new(Roots::WebPkiPlusDer(crate::join::pem_der(ca).ok()?), None).ok()?;
    let url = format!("{}/api/agents/rekey", o.server_url.trim_end_matches('/'));
    let asked = http.post(&url).json(&RekeyRequest { cert_pem: cert.to_owned(), csr, machine_id: id }).timeout(Duration::from_secs(20)).send().await;
    match asked {
        Ok(resp) if resp.is_success() => {
            let e: EnrollResponse = resp.json(1 << 20).await.ok()?;
            if let Err(err) = crate::util::write_secret(cert_p, e.cert_pem.as_bytes(), 0o644) {
                tracing::warn!(error = %err, "couldn't keep the certificate for this runner's machine id; it goes on by its name");
                return None;
            }
            tracing::info!(was = %name, now = %e.name, label = e.label.as_deref().unwrap_or(""), "this runner is known by its machine id now");
            Some(e.cert_pem)
        }
        Ok(resp) => {
            let status = resp.status();
            let why = String::from_utf8_lossy(&resp.bytes(1 << 12).await.unwrap_or_default()).trim().to_owned();
            tracing::info!(status, why = %why, "the server didn't move this runner to its machine id now; it goes on by its name");
            None
        }
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "couldn't ask to move this runner to its machine id");
            None
        }
    }
}

/// Load stored credentials, enrolling (or renewing) when needed. Retries until it succeeds.
/// Shutdown, or (lingering, shutdown already said) the next 50 ms tick, to look again whether the
/// loop may end.
#[allow(clippy::disallowed_methods)] // (e) lingering after shutdown: looks again every 50 ms whether the loop may end; its tasks' ends would tell
async fn shutdown_or_tick(shutdown: &Shutdown, lingering: bool) {
    if lingering {
        crate::rt::sleep(Duration::from_millis(50)).await;
    } else {
        shutdown.cancelled().await;
    }
}

#[allow(clippy::disallowed_methods)] // (b) enrollment retry, jittered doubling backoff
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
            // Known by its name: it moves to its machine id now, as it starts, holding nothing yet.
            let c = moved_to_its_id(o, &c, &k, &ca, &cert_p).await.unwrap_or(c);
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
    // Read off the runtime's threads: it may run a program (macOS's `ioreg`).
    let machine_id = crate::rt::blocking(crate::os::machine_id).await.ok().flatten();
    let mut backoff = Duration::from_secs(1);
    loop {
        let r = http
            .post(&url)
            .json(&EnrollRequest { token: token.clone(), name: o.name.clone(), csr: csr.clone(), hostname: crate::config::Config::hostname(), machine_id: machine_id.clone() })
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
#[allow(clippy::disallowed_methods)] // (b) enrollment retry, jittered doubling backoff
async fn instance_credentials(o: &Opts, id: &str, shutdown: &Shutdown) -> anyhow::Result<(Vec<u8>, Leaf, Option<String>)> {
    let dir = crate::upgrade::instance_dir(&o.state_dir, id);
    let ca = std::fs::read_to_string(o.state_dir.join("ca.pem")).context("this machine is not enrolled (no ca.pem)")?;
    let (machine_cert, key) = crate::upgrade::machine_credentials(&o.state_dir)?;
    // Known by its name: the machine moves to its machine id as this process starts too, an
    // upgrade's runner process being its runner since (docs/machines.md). While another process of
    // it is connected (the one an upgrade takes over from) the server refuses, and it asks again
    // as it starts next.
    let machine_cert = moved_to_its_id(o, &machine_cert, &key, &ca, &o.state_dir.join("agent.pem")).await.unwrap_or(machine_cert);
    let machine = crate::pki::leaf_name(&Leaf { cert_pem: machine_cert.clone(), key_pem: String::new() });
    let quic = std::fs::read_to_string(o.state_dir.join("quic")).ok().map(|s| s.trim().to_owned());
    let cert_p = dir.join("agent.pem");
    let own = std::fs::read_to_string(&cert_p).ok().filter(|c| {
        let mine = crate::pki::cert_matches_key(c, &key);
        if !mine {
            tracing::warn!(instance = %id, "this runner process's certificate is for a key the machine no longer has (it enrolled again): asking for a new one");
        }
        // Its certificate names the machine as it was: one known by its machine id since gets one
        // for that id.
        let same = crate::pki::leaf_name(&Leaf { cert_pem: c.clone(), key_pem: String::new() }) == machine;
        if mine && !same {
            tracing::info!(instance = %id, "the machine is known by its machine id now: asking for this runner process's certificate for it");
        }
        mine && same
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

struct Running {
    cancel: Shutdown,
    /// Its lease lost, it's killed (docs/ci.md W05-R37): no cancel's grace, nothing more run.
    kill: Shutdown,
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
    let threads = usize::try_from(q.cpu.ceil().max(1)).unwrap_or(usize::MAX);
    match crate::runner::cpus::pins().hold(job, threads) {
        Some(cpus) => tracing::info!(job, cpus = %crate::runner::cpus::list(&cpus), "it holds these CPUs; the other legs run on the rest"),
        None => tracing::info!(job, threads, "it holds its share in placement; its cores aren't pinned here"),
    }
}

/// Whether this runner takes one more job or leg (docs/ci.md W05-R03), by the rule its dispatchers
/// use, counting what its jobs and legs from either hold (`jobs`: each one's pooled and holds); a
/// pool's leg runs in its executor, beside the runner's slots. Why not, when it doesn't.
pub(crate) fn admits(slots: u32, size: Option<kiln_ci::sched::state::Resources>, jobs: impl Iterator<Item = (bool, kiln_ci::sched::state::Incoming)>, pooled: bool, incoming: kiln_ci::sched::state::Incoming) -> Result<(), &'static str> {
    use kiln_ci::sched::state::{Incoming, Resources, room};
    let (mut held, mut whole, mut ordinary, mut running) = (Resources::default(), false, 0, 0);
    for (p, h) in jobs {
        match h {
            Incoming::Whole => whole = true,
            Incoming::Share(q) => {
                held.cpu = held.cpu.saturating_add(q.cpu);
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
        Incoming::Ordinary if !held.cpu.is_zero() => "no free slot beside what's held",
        Incoming::Ordinary => "no free slot",
    })
}

/// How long a cancelled job has to end before the runner reports it over and lets it go.
const CANCEL_GRACE: Duration = Duration::from_secs(120);

/// How long after a link comes up a runner says again what it has of the server's commands
/// (`Have`), and then again as long after: its server sends its commands on a link only once it
/// has heard one, so one lost below the channel would hold them up until its next sample.
const HAVE_SOON: Duration = Duration::from_secs(1);

/// How many times a runner says what it has again so ([`HAVE_SOON`]), beside its samples'.
const HAVES_SOON: u32 = 2;

/// What a job stopped for its lease ends with (docs/ci.md W05-R37).
const LEASE_LOST: &str = "lease lost: this runner's lease on it ended unrenewed, and it stops on losing it";

/// A frame as the server takes it (docs/ci.md W05-R34): to a server that takes leases, what this
/// runner sends about a leg it holds a grant for goes fenced by that grant (its steps, events and
/// result carried whole, a refusal naming it, its output on [`proto::LANE_FENCED_LOGS`]); the rest,
/// and everything to an older server, as it is. What's kept to send again after a reconnect is kept
/// as it is, and goes this way then, to whichever server that is.
fn wire_form(lane: u32, kind: Kind, data: Vec<u8>, grants: &HashMap<String, proto::Grant>, leases: bool) -> (u32, Kind, Vec<u8>) {
    if !leases || grants.is_empty() {
        return (lane, kind, data);
    }
    if lane == LANE_LOGS
        && let Ok(c) = proto::read_whole::<proto::LogChunk>(&data)
        && let Some(grant) = grants.get(&c.job)
        && let Ok(b) = proto::encode(&proto::FencedLog { grant: grant.clone(), idx: c.idx, data: c.data })
    {
        return (proto::LANE_FENCED_LOGS, kind, b);
    }
    if lane == LANE_CONTROL
        && let Ok(m) = proto::read_agent(&data)
        && let Some(f) = proto::fenced(m, grants)
        && let Ok(b) = proto::encode(&f)
    {
        return (lane, kind, b);
    }
    (lane, kind, data)
}

/// The grants this runner holds (docs/ci.md W05-R34): each leg it runs, or whose result the server
/// hasn't said it has, that its assignment gave a grant; in the order of their legs.
fn held_grants(grants: &HashMap<String, proto::Grant>, jobs: &HashMap<String, Running>, unreceived: &HashMap<String, (Instant, Vec<u8>)>) -> Vec<proto::Grant> {
    let mut held: Vec<proto::Grant> = grants.iter().filter(|(job, _)| jobs.contains_key(*job) || unreceived.contains_key(*job)).map(|(_, g)| g.clone()).collect();
    held.sort_by(|a, b| a.leg.cmp(&b.leg));
    held
}

/// The grants `held` names, by leg and fence, as [`crate::runner::lease::Leases`] keeps them.
fn named(held: &[proto::Grant]) -> Vec<(String, kiln_ci::sched::Fence)> {
    held.iter().map(|g| (g.leg.clone(), g.fence())).collect()
}

/// One turn of the agent's event loop (computed inside `select!`, handled after it).
enum Ev {
    Shutdown,
    Net(Option<NetEvent>),
    Done(JobResult),
    /// The runner's chores, each as it falls due or as what it waits on moves (goal 9): custody's
    /// records to send and answers due, an identity's retry, what it couldn't read told, an older
    /// server's heartbeat, long-done jobs' counts let go.
    Heartbeat,
    /// A lease runner's renewal of what it holds, due two RTOs before the earliest end of it
    /// (docs/ci.md W05-R37, `crate::runner::lease::Leases::renew_due`).
    Renew,
    /// A grant's lease ended (`crate::runner::lease::Leases::stop_due`): stopped if it stops on
    /// losing it, on the lease clock, which counts a sleep (docs/ci.md W05-R37).
    Lease,
    /// Something moved its host's sample (a run's start or end, the OS's word on its pressure,
    /// clocks or power, a connection): taken and told (goal 9, W05-R26).
    Sample,
    /// The OS said its power source or its battery's charge moved: the battery read and said.
    Power,
    /// Its QUIC layer let frames go below the channel: what it has said again, its count told.
    Dropped,
    /// An item of its updates' channel is overdue (`crate::wire::uplink::Updates::due`).
    Overdue,
    /// Its config file changed: its agent limit and its say on upgrades read again.
    Config,
    /// One of its conversation stores committed: what they're doing told.
    ConvoStats,
    /// What it holds warm may have changed (`crate::runner::warm`, W24-R18).
    Work,
    /// What harnesses it offers may have changed (`crate::harness::kinds`).
    Harnesses,
    /// What the upgrade's watch and the paths' wait on moved, or one of them fell due: a new binary
    /// on disk, a job's or a session's end, a leg let go, a direct dial's patience out. Also what a
    /// closed channel's arm stands for: nothing but a look at the loop's state.
    Watchdog,
    Redial,
    Term(String, PtyOut),
    Up(Up),
    /// A session's update, and when its session handed it on: when this runner took it in (goal 10, plan
    /// 01a1a87b2dae step 5).
    Harness(String, crate::harness::core::Update, Instant),
    /// A retained archive/restore finished while the loop kept delivering Reach exits.
    HarnessApplied(HarnessApplied),
    /// The one event-triggered source-journal replay completed, after its bounded sink drained.
    MailReplayed(bool),
    /// Actual shared input-pool availability changed while a header copy was held.
    MailBudget,
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
    /// A gap in the server's commands it asked for is still there: it asks again.
    Wants,
    /// It says again what it has of the server's commands, soon after a link came up.
    HaveSoon,
    /// A test build's fault: a frame held back, due now (W22-AC04).
    Delayed(crate::runner::chaos::Due),
    /// What the fan handed this loop (runner-fan): taken once the server's earlier frames it may not overtake are
    /// (`crate::wire::fan::Routed::after`), then as the event it carries.
    Routed(crate::wire::fan::Routed),
    /// A frame session loop `k` handed this loop while blue/green decides what goes on to a new build
    /// (`loops::Deferral`): its number from the fan, which this loop marks taken for that loop once it applied it, and
    /// the frame, taken as one the server sent.
    Deferred(usize, u64, Vec<u8>),
}

/// What event `ev` is, for the legs of what the runner's loop takes (`crate::route_latency::InKind`; runner-fan): one of
/// the server's frames by what it is (`crate::wire::fan::peek_server`), the link coming or going, a session's update on its
/// way up, what the runner says of its own, or the rest.
fn in_kind(ev: &Ev) -> crate::route_latency::InKind {
    use crate::route_latency::InKind;
    match ev {
        Ev::Net(Some(NetEvent::Frame { kind, lane, data, .. })) => crate::wire::fan::peek_server(*kind, *lane, data).1,
        Ev::Forwarded(data) | Ev::Deferred(_, _, data) => crate::wire::fan::peek_server(Kind::Control, LANE_CONTROL, data).1,
        Ev::Routed(r) => match &r.ev {
            NetEvent::Frame { kind, lane, data, .. } => crate::wire::fan::peek_server(*kind, *lane, data).1,
            _ => InKind::Link,
        },
        Ev::Net(_) => InKind::Link,
        Ev::Harness(..) => InKind::Update,
        Ev::Up(_) => InKind::Own,
        _ => InKind::Rest,
    }
}

enum HarnessApplied {
    Command { reply: Option<(String, u64)>, result: Result<(), (crate::wire::channel::Refusal, String)> },
    Native(NativePrepared),
    #[cfg(unix)]
    LegacyPromotion(Result<(), crate::harness::inbox::PromotionError>),
}
type HarnessOperations=futures::stream::FuturesUnordered<futures::future::BoxFuture<'static,HarnessApplied>>;

async fn next_harness_operation(operations:&mut HarnessOperations)->HarnessApplied{
    match operations.next().await{Some(answer)=>answer,None=>futures::future::pending().await}
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum LegacyCause { Observe, Promoted }

#[cfg(unix)]
impl LegacyCause {
    fn joined(self, other: Self) -> Self { if matches!(self, Self::Promoted) || matches!(other, Self::Promoted) { Self::Promoted } else { Self::Observe } }
}

/// One metadata operation in the existing owner. A failed promotion keeps its actual event
/// until another real connection/identity event retries it; bookkeeping never self-retries.
#[cfg(unix)]
#[derive(Default)]
enum LegacyProgress {
    #[default]
    Idle,
    Reading { cause: LegacyCause, again: Option<LegacyCause> },
    Held(LegacyCause),
}

#[cfg(unix)]
impl LegacyProgress {
    fn request(&mut self, host: Option<&Arc<crate::harness::host::Host>>, operations: &mut HarnessOperations, tx: &crate::memory::queue::Sender<Bg>, cause: LegacyCause) {
        let cause = match self {
            Self::Reading { again, .. } => { *again = Some(again.map_or(cause, |before| before.joined(cause))); return; }
            Self::Held(before) => before.joined(cause),
            Self::Idle => cause,
        };
        let Some(host) = host else { return };
        let (host, tx) = (Arc::clone(host), tx.clone());
        *self = Self::Reading { cause, again: None };
        operations.push(async move { HarnessApplied::LegacyPromotion(legacy_promotion_progress(&host, cause, &tx).await) }.boxed());
    }

    fn finish(&mut self, result: &Result<(), crate::harness::inbox::PromotionError>, host: Option<&Arc<crate::harness::host::Host>>, operations: &mut HarnessOperations, tx: &crate::memory::queue::Sender<Bg>) {
        let Self::Reading { cause, again } = std::mem::take(self) else { return };
        if result.is_err() { *self = Self::Held(cause); }
        if let Some(again) = again { self.request(host, operations, tx, again); }
    }
}

#[cfg(unix)]
async fn legacy_promotion_progress(host: &Arc<crate::harness::host::Host>, cause: LegacyCause, tx: &crate::memory::queue::Sender<Bg>) -> Result<(), crate::harness::inbox::PromotionError> {
    let mut after = None;
    let mut first_error = None;
    while let Some((session, promotion)) = host.legacy_promotion_next(after).await? {
        after = Some(session.clone());
        if let Err(error) = legacy_session_progress(host, &session, promotion, cause, tx).await && first_error.is_none() { first_error = Some(error); }
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(unix)]
async fn legacy_session_progress(host: &Arc<crate::harness::host::Host>, session: &str, mut promotion: crate::harness::inbox::Promotion, cause: LegacyCause, tx: &crate::memory::queue::Sender<Bg>) -> Result<(), crate::harness::inbox::PromotionError> {
    use crate::harness::inbox::{PromotionError, PromotionPhase};
    if promotion.phase == PromotionPhase::Active && !host.legacy_recovery_pending(session, promotion.from_epoch, promotion.to_epoch).await? { return Ok(()); }
    crate::runner::custody::legacy_waiting(session, &promotion).map_err(|why| PromotionError::Invalid { why })?;
    if matches!(cause, LegacyCause::Promoted) && promotion.phase == PromotionPhase::AwaitingPromotion {
        host.promote_legacy(session, promotion.to_epoch).await?;
        promotion = host.legacy_promotion(session).await?.ok_or(PromotionError::Stale)?;
        crate::runner::custody::legacy_waiting(session, &promotion).map_err(|why| PromotionError::Invalid { why })?;
    }
    if matches!(promotion.phase, PromotionPhase::AwaitingCertificate | PromotionPhase::Rebinding | PromotionPhase::Active) {
        if crate::runner::custody::legacy_certificate_ready(session, promotion.to_epoch, &promotion.proof).is_ok() {
            host.certify_legacy(session, promotion.to_epoch).await?;
        } else {
            tx.send(Bg::LegacyIdentity { session: session.to_owned(), epoch: promotion.to_epoch }).await.map_err(|_| PromotionError::Storage { why: "the existing runner control owner is closed; promotion remains retained".into() })?;
        }
    }
    Ok(())
}

#[cfg(unix)]
type PreparedSessions = Vec<(crate::harness::host::Saved, Option<Box<crate::harness::session::Handed>>, Option<serde_json::Value>)>;

#[cfg(unix)]
struct BlueGreenReady { job: String, conn: Arc<crate::bluegreen::Conn>, instance: String, custody: bool, role: Option<String>, relays: bool, stores: bool, mail: bool }

enum NativePreparation {
    #[cfg(unix)]
    BlueGreen { ready: BlueGreenReady, machine: String },
    InPlace { exe: PathBuf, mtime: Option<std::time::SystemTime> },
    #[cfg(unix)]
    Rollback { job: String, why: String, acks: Vec<Ack>, records: Vec<crate::harness::host::Saved> },
}

enum NativePrepared {
    #[cfg(unix)]
    BlueGreen { ready: BlueGreenReady, sessions: Result<PreparedSessions, String> },
    InPlace { exe: PathBuf, mtime: Option<std::time::SystemTime>, result: Result<usize, String> },
    #[cfg(unix)]
    Rollback { job: String, why: String, acks: Vec<Ack>, allowed: Result<bool, String> },
}

#[derive(PartialEq, Eq)]
enum NativeKind { #[cfg(unix)] BlueGreen(String), InPlace, #[cfg(unix)] Rollback(String) }

/// One native preparation in the existing operation set. Cancellation records intent; it does
/// not drop the future that may already own detached native descriptors. Its completion hands
/// those descriptors to the existing rollback owner before cancellation is acted on.
#[derive(Default)]
enum NativeHandover {
    #[default]
    Idle,
    Preparing { kind: NativeKind, cancel: Option<String>, wake: crate::rt::Shutdown },
}

impl NativeHandover {
    fn idle(&self) -> bool { matches!(self, Self::Idle) }
    fn cancel(&mut self, job: Option<&str>, why: String) -> bool {
        let Self::Preparing { kind, cancel, wake } = self else { return false };
        let same = match (job, &*kind) {
            (None, _) => true,
            #[cfg(unix)]
            (Some(job), NativeKind::BlueGreen(active) | NativeKind::Rollback(active)) => active == job,
            (Some(_), _) => false,
        };
        if !same { return false; }
        if cancel.is_none() { *cancel = Some(why); }
        wake.cancel();
        true
    }
    fn finish(&mut self) -> Option<String> {
        match std::mem::take(self) { Self::Preparing { cancel, .. } => cancel, Self::Idle => None }
    }
    fn prepare(&mut self, operations: &mut HarnessOperations, host: Option<Arc<crate::harness::host::Host>>, operation: NativePreparation) -> bool {
        if !self.idle() { return false; }
        let kind = match &operation {
            #[cfg(unix)]
            NativePreparation::BlueGreen { ready, .. } => NativeKind::BlueGreen(ready.job.clone()),
            NativePreparation::InPlace { .. } => NativeKind::InPlace,
            #[cfg(unix)]
            NativePreparation::Rollback { job, .. } => NativeKind::Rollback(job.clone()),
        };
        let wake = crate::rt::Shutdown::new();
        *self = Self::Preparing { kind, cancel: None, wake: wake.clone() };
        operations.push(async move {
            HarnessApplied::Native(match operation {
                #[cfg(unix)]
                NativePreparation::BlueGreen { ready, machine } => {
                    let destination = crate::harness::host::transfer::NativeDestination { machine, instance: ready.instance.clone(), mail: ready.mail, cancel: wake };
                    let sessions = match host { Some(host) => host.move_out_signing(ready.custody, Some(destination)).await, None => Ok(Vec::new()) };
                    NativePrepared::BlueGreen { ready, sessions }
                }
                NativePreparation::InPlace { exe, mtime } => {
                    let result = match host { Some(host) => host.hand_over_all().await, None => Ok(0) };
                    NativePrepared::InPlace { exe, mtime, result }
                }
                #[cfg(unix)]
                NativePreparation::Rollback { job, why, acks, records } => {
                    let allowed = match host { Some(host) => host.abort_native_records(&records).await, None if records.is_empty() => Ok(true), None => Err("the rollback source journal is unavailable; native owners remain retained".into()) };
                    NativePrepared::Rollback { job, why, acks, allowed }
                }
            })
        }.boxed());
        true
    }
}

/// One operation on the existing Host, polled beside the sink this loop drains. A second
/// connection/stream-reset event coalesces into one further read; no receipt/body queue lives here.
#[derive(Clone, Copy)]
// Counts are (used, bound). Unfavourable changes update only the observed cursor, so a
// reserve/release pulse back to the failure baseline cannot manufacture new availability.
struct ReplayBudget { before: (usize, usize), observed: (usize, usize) }
impl ReplayBudget {
    fn new(counts: (usize, usize)) -> Self { Self { before: counts, observed: counts } }
    fn improved(&mut self, now: (usize, usize)) -> bool {
        self.observed = now;
        now.1 > self.before.1 || now.0 < self.before.0
    }
}

enum MailReplay {
    Idle { budget: Option<ReplayBudget> },
    Reading { read: futures::future::BoxFuture<'static, bool>, before: (usize, usize) },
    Again { read: futures::future::BoxFuture<'static, bool>, before: (usize, usize) },
}
impl Default for MailReplay { fn default() -> Self { Self::Idle { budget: None } } }

impl MailReplay {
    fn request(&mut self, host: Option<&Arc<crate::harness::host::Host>>) {
        *self = match std::mem::take(self) {
            Self::Idle { .. } => match host.cloned() {
                Some(host) => {
                    let before = host.native_budget_counts();
                    Self::Reading { read: Box::pin(async move { let held = host.replay_mail_transfers().await; host.replay_mail().await; held }), before }
                }
                None => Self::default(),
            },
            Self::Reading { read, before } | Self::Again { read, before } => Self::Again { read, before },
        };
    }

    async fn completed(&mut self) -> bool {
        match self {
            Self::Reading { read, .. } | Self::Again { read, .. } => read.await,
            Self::Idle { .. } => futures::future::pending().await,
        }
    }

    fn budget_observed(&self) -> Option<(usize, usize)> {
        match self { Self::Idle { budget: Some(budget) } => Some(budget.observed), _ => None }
    }

    async fn budget_changed(host: Option<&Arc<crate::harness::host::Host>>, observed: Option<(usize, usize)>) {
        match (host, observed) {
            (Some(host), Some(observed)) => host.native_budget_changed(observed).await,
            _ => futures::future::pending().await,
        }
    }

    fn budget_event(&mut self, host: Option<&Arc<crate::harness::host::Host>>) {
        if let (Self::Idle { budget: Some(budget) }, Some(host)) = (&mut *self, host)
            && budget.improved(host.native_budget_counts()) { self.request(Some(host)); }
    }

    fn finish(&mut self, host: Option<&Arc<crate::harness::host::Host>>, held: bool) {
        let (again, before) = match std::mem::take(self) {
            Self::Reading { before, .. } => (false, before),
            Self::Again { before, .. } => (true, before),
            Self::Idle { .. } => return,
        };
        let Some(host) = host else { return };
        let after = host.native_budget_counts();
        if again || (held && ReplayBudget::new(before).improved(after)) { self.request(Some(host)); }
        else if held { *self = Self::Idle { budget: Some(ReplayBudget::new(after)) }; }
    }
}

/// Blue/green upgrade steps (`crate::bluegreen`) done off the event loop, back to it (none on
/// Windows, which has no blue/green upgrades).
enum Bg {
    /// (The runner being upgraded.) The new build is downloaded and checked: start it beside this one.
    #[cfg(unix)]
    Fetched { job: String, exe: PathBuf, build: String },
    /// The new build's runner is connected and ready: hand it everything.
    #[cfg(unix)]
    /// `custody`: the new build takes sessions signed for (docs/ci.md W27-R08); `role`: its instance,
    /// when it takes the machine's runner role signed for too; `relays`: it checks this runner's
    /// records of the frames it forwards (docs/ci.md W27-R08's relays); `stores`: it takes blue's
    /// conversation stores over too.
    Ready(BlueGreenReady),
    /// Source native preparation completed while the control loop kept draining its sink.
    #[cfg(unix)]
    Prepared { ready: BlueGreenReady, sessions: Result<PreparedSessions, String> },
    /// Everything was sent: what the new build said of each, in order (`broke`: the connection
    /// failed part-way), and where it set aside each conversation store that wouldn't open there
    /// (`aside`, as [`Given::aside`]).
    #[cfg(unix)]
    Moved { job: String, conn: Arc<crate::bluegreen::Conn>, acks: Vec<Ack>, broke: Option<String>, aside: Option<[Option<PathBuf>; 2]> },
    /// The new build didn't come up: this runner goes on.
    #[cfg(unix)]
    Failed { job: String, why: String },
    /// Exact source manifest Abort is durable; only this event may stop green and resume blue.
    #[cfg(unix)]
    RollbackAuthorized { job: String, why: String, acks: Vec<Ack> },
    /// (The new build's runner.) A terminal handed over: take it, and say how it went.
    #[cfg(unix)]
    TakeTerminal { session: String, command: Option<String>, cols: u16, rows: u16, replay: Vec<u8>, fd: std::os::fd::OwnedFd, pid: u32, done: futures::channel::oneshot::Sender<Result<(), String>> },
    /// (The new build's runner.) Everything handed over is in: the server hears what runs here.
    #[cfg(unix)]
    TookOver { sessions: usize, terminals: usize, refused: usize },
    /// (The new build's runner.) The conversation stores it goes on with: the files the runner it
    /// takes over from moved into this one's folder (`crate::bluegreen::Frame::Stores`: its own and
    /// its copy of the server's), or none (one of its own, then). Say how it went: what it set aside
    /// and went on without, or why it keeps none.
    #[cfg(unix)]
    Stores { own: Option<PathBuf>, mirror: Option<PathBuf>, done: futures::channel::oneshot::Sender<Taken> },
    /// (The new build's runner.) The runner it took over from is gone (or never answered).
    #[cfg(unix)]
    TakeOverEnded,
    /// (The new build's runner.) It is the runner now: the server hears it's the newest.
    #[cfg(unix)]
    Promoted,
    /// A durable promotion asks only its exact destination epoch, outside the metadata worker.
    #[cfg(unix)]
    LegacyIdentity { session: String, epoch: u64 },
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
    /// The new build checks this runner's records of the frames it forwards (`Frame::Ready`'s
    /// `relays`, docs/ci.md W27-R08's relays).
    relays: bool,
    /// Its conversation stores, given to the new build: a rollback takes them back.
    stores: Option<Given>,
}

/// The conversation stores a runner gave the new build ([`Moving::stores`]): each file, moved from
/// and to, and the tee its own store was taken from, to give it back to.
#[cfg(unix)]
struct Given {
    held: crate::convo::store::Held,
    moves: Vec<(PathBuf, PathBuf)>,
    own_dir: PathBuf,
    /// Where the new build set aside each (its own, the copy) that wouldn't open there, as it said
    /// (`crate::bluegreen::Frame::StoresTaken`); None: its answer never came.
    aside: Option<[Option<PathBuf>; 2]>,
}

/// (The new build's runner.) How it took the conversation stores handed to it ([`Bg::Stores`],
/// `crate::bluegreen::Frame::StoresTaken`): whether it keeps one, where it set aside each that
/// wouldn't open, and what it did.
#[cfg(unix)]
#[derive(Default)]
struct Taken {
    kept: bool,
    own_aside: Option<PathBuf>,
    mirror_aside: Option<PathBuf>,
    note: Option<String>,
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

/// How long a new build whose link to the runner it upgraded ended with no word of a promotion
/// (everything handed over, or part of it) holds messages still (a runner that takes its sessions
/// back stops it well before).
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
fn take_terminal_back(terms: &mut HashMap<String, (PtySession, Arc<crate::termsess::Session>)>, term_tx: &crate::memory::queue::Sender<(String, PtyOut)>, session: String, state: Arc<crate::termsess::Session>, fd: std::os::fd::OwnedFd, pid: u32) -> bool {
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
#[allow(clippy::disallowed_methods)] // (b) retry backoff, picked
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
    up: crate::memory::queue::Sender<Up>,
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
                        // Its one answer, from its thread (goal 5, plan 01a1a59c2aae: no channel whose wait locks).
                        let (tx, rx) = crate::memory::queue::places(&crate::memory::owners::CHANNELS, 1);
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
    #[allow(clippy::disallowed_methods)] // (e) polls its replica's progress every REPLICA_CHECK; the replica's applied index could tell
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

/// CI leg `id` (its kind `key`), asking `request` (`exclusive`: the runner whole), admitted against this runner's
/// placement ledger beside the runs owners placed here (docs/needs.md E5, AUDIT M4: one ledger
/// for CI legs, commands and sessions, so together they never pass its budget, G1): `Err(why)`
/// when it leaves no room. Without reach, as before.
async fn leg_room(reach: Option<&crate::reach::node::Handle>, id: &str, key: String, exclusive: bool, request: kiln_ci::sched::state::Resources, requires: &[String]) -> Result<(), String> {
    let Some(r) = reach else { return Ok(()) };
    let needs = kiln_ci::sched::needs::Needs { request, exclusive, requires: if request.gpu > 0 { requires.to_vec() } else { Vec::new() }, ..kiln_ci::sched::needs::Needs::default() };
    // Its kind as its run here is keyed (`sense::run::leg_key`, as the executor keys it): a cause
    // its own runs met here bars it (AUDIT L2).
    let ask = kiln_ci::place::Ask { run: id.to_owned(), needs, class: kiln_ci::sched::state::Class::Batch, key: Some(key), ..kiln_ci::place::Ask::default() };
    r.admit_leg(ask).await.unwrap_or(Ok(()))
}

/// Serve the machine's local sockets (`kiln run`, `kiln spawn`) from this runner process (runner
/// `name`, its sockets named by `sock`): the new build once it has taken over, or the old one again
/// after a rollback (the paths are the same).
#[cfg(unix)]
fn serve_sockets(reach: Option<&crate::reach::node::Handle>, harness: Option<&Arc<crate::harness::host::Host>>, name: &str, sock: &str, state_dir: &std::path::Path) {
    crate::browser::set_home(state_dir);
    if let Some(r) = reach
        && let Err(e) = crate::reach::local::serve(r.clone(), name.to_owned(), crate::reach::local::path(sock, state_dir), true,harness.cloned())
    {
        tracing::warn!(error = %format!("{e:#}"), "`kiln run` will not be available on this machine");
    }
    if let Some(h) = harness
        && let Err(e) = crate::harness::socket::serve(h, crate::harness::socket::path(sock, state_dir), true)
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
    term_tx: &crate::memory::queue::Sender<(String, PtyOut)>,
    state_dir: &std::path::Path,
) -> (Vec<String>, Option<crate::convo::store::Store>) {
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
        return (kept, None);
    }
    let mut problems = Vec::new();
    // Its conversation stores first, before what comes back writes into them: moved back out of
    // the new build's folder (removed below) and opened again, the same ids and numbers. One that
    // couldn't be moved back keeps that folder.
    let store = mv.stores.as_ref().and_then(|g| take_stores_back(g, &mut problems));
    let stranded = !problems.is_empty();
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
                if let Err(e) = h.move_in(*record, handed).await {
                    problems.push(format!("agent session {id}: {e:#}"));
                }
            }
        }
    }
    if !stranded {
        let _ = std::fs::remove_dir_all(crate::upgrade::instance_dir(state_dir, &mv.instance));
    }
    if problems.is_empty() {
        span.record("result", "ok");
    } else {
        span.record("result", "failed");
        span.record("error", problems.join("; ").as_str());
    }
    mv.root.record("result", "rolled_back");
    mv.root.record("error", why);
    (problems, store)
}

/// (The runner being upgraded.) Its conversation stores, let go of for the new build
/// (`Frame::Stores`): the tee lets go of its own, everything its sessions said in it already (each
/// update is kept as it's handed on); each store is closed, as long as its disk takes to sync it (the
/// new build waits for this frame, and opens no store meanwhile), and its files moved as `moves` says
/// (its own first, then its copy of the server's). What couldn't be is left out, and the new build
/// opens a store of its own for it.
#[cfg(unix)]
fn give_stores(held: &crate::convo::store::Held, own: Option<crate::convo::store::Store>, mirror: Option<crate::convo::store::Store>, moves: &[(PathBuf, PathBuf)]) -> crate::bluegreen::Frame {
    let _ = held.take();
    let closed = |st: Option<crate::convo::store::Store>, what: &str| {
        st.is_some_and(|st| {
            let began = Instant::now();
            match st.close() {
                Ok(()) => {
                    tracing::info!(store = what, ms = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX), "a conversation store let go of its file for the new build");
                    true
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), store = what, "a conversation store couldn't be let go of; the new build opens one of its own");
                    false
                }
            }
        })
    };
    let (own_ok, mirror_ok) = (closed(own, "own"), closed(mirror, "mirror"));
    let moved = |i: usize, ok: bool| -> Option<String> {
        let (from, to) = moves.get(i).filter(|_| ok)?;
        match crate::convo::move_file(from, to) {
            Ok(()) => Some(to.to_string_lossy().into_owned()),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), from = %from.display(), "a conversation store couldn't be moved to the new build; it opens one of its own");
                None
            }
        }
    };
    crate::bluegreen::Frame::Stores { own: moved(0, own_ok), mirror: moved(1, mirror_ok) }
}

/// (The runner being upgraded, rolling back.) Its conversation stores, given to the new build
/// ([`Given`]), moved back where they were (each that's in the new build's folder) and its own
/// opened again for its tee: the store to go on with. One the new build set aside (it wouldn't open
/// there) is this runner's own as it was: that comes back, from where the new build said it put it,
/// not the new store in its place. What went wrong goes in `problems`.
#[cfg(unix)]
fn take_stores_back(g: &Given, problems: &mut Vec<String>) -> Option<crate::convo::store::Store> {
    for (i, (from, to)) in g.moves.iter().enumerate() {
        let there = match &g.aside {
            Some(said) => said.get(i).cloned().flatten().unwrap_or_else(|| to.clone()),
            // Its answer never came: one it set aside in its folder, new with it, has the first name.
            None => {
                let mut first = to.as_os_str().to_owned();
                first.push(crate::convo::SET_ASIDE);
                Some(PathBuf::from(first)).filter(|f| f.exists()).unwrap_or_else(|| to.clone())
            }
        };
        let there = &there;
        if there.exists()
            && !from.exists()
            && let Err(e) = crate::convo::move_file(there, from)
        {
            problems.push(format!("conversation store {}: {e:#} (it stays at {})", from.display(), there.display()));
        }
    }
    let st = crate::convo::open(&g.own_dir, crate::config::Conversations::default().upkeep())?;
    g.held.give(st.clone());
    Some(st)
}

/// The store's feed to the server (`crate::convo::feed`), and the harnesses' own records followed
/// into the store (`crate::convo::record`): as the runner starts, or once a new build taking over
/// has the store it goes on with.
fn start_feed(st: &crate::convo::store::Store, harness: Option<&Arc<crate::harness::host::Host>>, up: &crate::memory::queue::Sender<Up>) -> crate::convo::feed::Shipper {
    if let Some(h) = harness {
        let follow = Arc::clone(h);
        if let Err(e) = crate::convo::record::Recorder::start(st.clone(), move || follow.record_sources()) {
            tracing::warn!(error = %format!("{e:#}"), "the harnesses' records won't be kept in the conversation store");
        }
    }
    let (shipper, said) = crate::convo::feed::Shipper::start(st.clone());
    // Each batch signed for as it goes (docs/ci.md W27-R08).
    shipper.sign_with(Arc::new(crate::runner::custody::Feed));
    let up = up.clone();
    crate::rt::spawn(async move {
        while let Ok((json, data)) = said.recv().await {
            if up.send(Up::Convo(AgentMsg::Convo { json, data })).await.is_err() {
                break;
            }
        }
    });
    shipper
}

/// (The new build's runner.) The stores it goes on with ([`Bg::Stores`]): how it went.
#[cfg(unix)]
async fn set_stores(tx: &crate::memory::queue::Sender<Bg>, own: Option<PathBuf>, mirror: Option<PathBuf>) -> Taken {
    let stopping = || Taken { note: Some("this runner is stopping".to_owned()), ..Taken::default() };
    let (done, said) = futures::channel::oneshot::channel();
    if tx.send(Bg::Stores { own, mirror, done }).await.is_err() {
        return stopping();
    }
    said.await.unwrap_or_else(|_| stopping())
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
#[allow(clippy::disallowed_methods)] // (e) polls a process's exit; its exit would tell
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

/// A Welcome does not settle named features: that reply follows independently on the
/// same connection. Only an actual Caps-negative or Features reply releases native Ready.
#[cfg(unix)]
#[derive(Default, Debug, PartialEq, Eq)]
enum FeatureNegotiation {
    #[default]
    AwaitingCaps,
    AwaitingNames,
    Legacy,
    Named,
}

#[cfg(unix)]
impl FeatureNegotiation {
    fn caps(&mut self, caps: u64) {
        if caps & proto::CAP_FEATURES == 0 {
            *self = Self::Legacy;
        } else if *self != Self::Named {
            *self = Self::AwaitingNames;
        }
    }

    fn names(&mut self) { *self = Self::Named; }

    fn take_over(&self, welcomed: bool, pending: &mut Option<PathBuf>) -> Option<PathBuf> {
        if welcomed && matches!(self, Self::Legacy | Self::Named) { pending.take() } else { None }
    }
}

/// Uses the existing one-shot takeover owner after this connection's handshake is settled.
#[cfg(unix)]
fn start_take_over(path: PathBuf, harness: Option<Arc<crate::harness::host::Host>>, tx: crate::memory::queue::Sender<Bg>, me: Me, takes: bool) {
    crate::rt::spawn(async move {
        if let Err(e) = take_over_from(path, harness, tx.clone(), me, takes).await {
            tracing::warn!(error = %format!("{e:#}"), "couldn't take over from the runner being upgraded");
        }
        crate::reach::told::predecessor_ended();
        let _ = tx.send(Bg::TakeOverEnded).await;
    });
}

/// (The new build's runner, blue/green.) Take over from the runner being upgraded, at `path`: say
/// it's ready, take each agent session and terminal it hands over (saying how each went), then the
/// server's frames about them that reached it meanwhile, until that runner ends. Terminals are
/// taken on this runner's loop (`tx`), which also hears once everything is in. `takes_stores`: it
/// waits for that runner's conversation stores (it keeps conversations).
#[cfg(unix)]
async fn take_over_from(path: PathBuf, harness: Option<Arc<crate::harness::host::Host>>, tx: crate::memory::queue::Sender<Bg>, me: Me, takes_stores: bool) -> anyhow::Result<()> {
    use crate::bluegreen::Frame;
    use base64::Engine as _;
    let conn = Arc::new(crate::rt::blocking(move || crate::bluegreen::connect(&path)).await??);
    let c = Arc::clone(&conn);
    let (custody, instance) = (crate::runner::custody::signs(), me.instance.clone());
    let mail_transfer = harness.as_ref().is_some_and(|host| host.inbox().is_some()) && crate::harness::handing::takes_mail();
    crate::rt::blocking(move || c.send(&Frame::Ready { pid: std::process::id(), instance, custody, role: custody, relays: custody, stores: takes_stores, mail_transfer }, &[])).await??;
    tracing::info!(upgrade = %std::env::var("KILN_UPGRADE_ID").unwrap_or_default(), "connected to the runner being upgraded: taking over");
    // Read on a thread of its own: once everything is in, the connection stays open (idle) until
    // the old runner ends.
    let (ftx, frx) = crate::memory::queue::unbounded::<anyhow::Result<(Frame, Vec<std::os::fd::OwnedFd>)>>(&crate::memory::owners::CHANNELS);
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
    // Whether it knows which conversation store it goes on with: the runner it takes over from says
    // first (`Frame::Stores`); one from before says nothing, and this one opens its own then. One
    // that takes none knows already.
    let mut stores = !takes_stores;
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
        if let Frame::Stores { own, mirror } = frame {
            stores = true;
            // Whether it keeps one, where it set aside what wouldn't open, and what it did.
            let t = set_stores(&tx, own.map(PathBuf::from), mirror.map(PathBuf::from)).await;
            let path = |p: Option<PathBuf>| p.map(|p| p.to_string_lossy().into_owned());
            let taken = Frame::StoresTaken { kept: t.kept, own_aside: path(t.own_aside), mirror_aside: path(t.mirror_aside), note: t.note };
            let c = Arc::clone(&conn);
            crate::rt::blocking(move || c.send(&taken, &[])).await??;
            continue;
        }
        if !stores && matches!(frame, Frame::Session { .. } | Frame::Terminal { .. } | Frame::Done) {
            stores = true;
            let t = set_stores(&tx, None, None).await;
            if !t.kept {
                tracing::warn!(error = %t.note.unwrap_or_default(), "the conversation store couldn't be opened");
            }
        }
        let (id, r) = match frame {
            Frame::Session { id, record, handed, hand_on } => {
                let r = take_session(harness.as_ref(), &id, *record, handed, hand_on, fds).await;
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
            Frame::Forward { data, passed_on: record } => {
                if let Some(d) = passed_on(&data, record.as_deref(), &me) {
                    let _ = tx.send(Bg::Forwarded(d)).await;
                }
                continue;
            }
            // The machine's runner role: this process's receipt, signed once its own identity is
            // certified, goes to the server with blue's hand-on (docs/ci.md W27-R08).
            Frame::Role { hand_on } => {
                crate::runner::custody::role_taken(&hand_on);
                ("runner role".to_owned(), Ok(()))
            }
            Frame::Ready { .. } | Frame::Ack { .. } | Frame::Stores { .. } | Frame::StoresTaken { .. } => continue,
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

/// This runner process, as a hand-over's word names the process it's for (docs/ci.md W27-R08): its
/// machine and instance, and kiln's root, which the server's signature on a word chains to.
#[cfg(unix)]
struct Me {
    machine: String,
    instance: String,
    root: Vec<u8>,
}

/// The session a server frame is a hand-over's word about (a local MoveTo, a HandedOver: about the
/// hold of the process it's sent to, never another's).
#[cfg(unix)]
fn hand_overs_word(msg: &ServerMsg) -> Option<&str> {
    let ServerMsg::HarnessCmd { session, json } = msg else { return None };
    crate::runner::custody::hand_overs_word(json).then_some(session.as_str())
}

/// (The runner being upgraded, blue/green.) Whether a server frame goes on to the new build: what
/// it's about, when that was handed there, or is new while this runner hands everything over or
/// retires (`leaving`); else nothing, and this runner takes it. A hand-over's word stays here
/// ([`hand_overs_word`]): about a session handed over, it was meant for this runner's hold.
#[cfg(unix)]
fn goes_on(msg: &ServerMsg, moved: &std::collections::HashSet<crate::instances::Owned>, leaving: bool) -> Option<crate::instances::Owned> {
    if hand_overs_word(msg).is_some() {
        return None;
    }
    let opens = leaving && matches!(msg, ServerMsg::TermOpen { .. } | ServerMsg::HarnessOpen { .. });
    crate::instances::Owned::of(msg).filter(|w| opens || moved.contains(w))
}

/// (The new build's runner.) A server frame the runner being upgraded passed on: its bytes, taken
/// as the server's own once its record of passing it on (`record`) holds, numbered next on its
/// stream here (docs/ci.md W27-R08's relays: else refused, and kiln told). Not a hand-over's word
/// ([`hand_overs_word`]), which was meant for that runner (one from before this build passes those
/// on too): refused, and kiln told whom it was for.
#[cfg(unix)]
fn passed_on(data: &str, record: Option<&str>, me: &Me) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let d = base64::engine::general_purpose::STANDARD.decode(data.as_bytes()).ok()?;
    if let Err(why) = crate::runner::custody::forwarded(&d, record, &me.root, &me.machine, &me.instance) {
        tracing::warn!(target: "kiln.custody", %why, "a frame the runner taken over from passed on, refused");
        return None;
    }
    if let Ok(m) = proto::read_server(&d)
        && let Some(session) = hand_overs_word(&m)
        && let ServerMsg::HarnessCmd { json, .. } = &m
    {
        if crate::runner::custody::word_for_me(session, json, true, &me.root, &me.machine, &me.instance, None).is_ok() {
            crate::runner::custody::refused_word(session, None, "a hand-over's word passed on by the runner this one took over from: that runner's, not this one's");
        }
        return None;
    }
    Some(d)
}

/// Whether an agent session this runner doesn't hold is gone, as it says when asked about it: not
/// one it handed to its new build (blue/green), which runs it now.
#[cfg(unix)]
fn gone_here(session: &str, moved: &std::collections::HashSet<crate::instances::Owned>) -> bool {
    !moved.contains(&crate::instances::Owned::Agent(session.to_owned()))
}

/// (The new build's runner.) An agent session handed over: with its harness process, whose pipes
/// came as `fds` (stdin, stdout, stderr); else started again from its record. When its process
/// can't go on here it is stopped and the session starts again from its record (an error, saying so).
#[cfg(unix)]
async fn take_session(h: Option<&Arc<crate::harness::host::Host>>, id: &str, mut rec: crate::harness::host::Saved, handed: Option<Box<crate::harness::session::Handed>>, hand_on: Option<String>, fds: Vec<std::os::fd::OwnedFd>) -> Result<(), String> {
    use std::os::fd::IntoRawFd as _;
    let Some(h) = h else { return Err("this runner runs no agent sessions".to_owned()) };
    if h.get(id).is_some() {
        return Ok(());
    }
    let manifest = rec.mail_manifest().cloned();
    let given = hand_on.as_deref().and_then(crate::runner::custody::bundle_of);
    if let Some(manifest) = &manifest {
        if manifest.session != id { return Err("the native journal names a different session".into()); }
        native_journal_hand_on(manifest, handed.as_deref().map(|h| &h.core), given.as_ref())?;
    }
    let native_count = if handed.is_some() { 3 } else { 0 };
    let expected = native_count + usize::from(rec.mail_manifest().is_some());
    if fds.len() != expected { return Err("the native handover did not carry its exact native/journal descriptors; the source retains custody".into()); }
    // A live process's hand-over is promoted on its frozen timeline. A session with none running
    // (idle, its harness exited) is its record alone, and goes on from it as after a restart: its
    // refusal for want of a timeline left it with neither process, as the one it came from counts
    // every refusal as started again here (Amphora, 2026-11-11 21:12: 43 handed, 42 taken).
    let durable_legacy = manifest.is_none() && handed.is_some() && crate::runner::custody::signs();
    if durable_legacy {
        let frozen = &handed.as_ref().ok_or_else(|| "the legacy native handover has no retained source timeline; its source keeps custody".to_owned())?.core;
        let given = given.as_ref().ok_or_else(|| "the legacy native handover has no source hand-on; its source keeps custody".to_owned())?;
        let (from, events) = frozen.handover_source();
        let to = from.checked_add(1).ok_or_else(|| "the legacy ownership epoch cannot advance".to_owned())?;
        let mut proof = crate::runner::custody::LegacyProof::of(id, from, events, Some(given))?;
        proof.hand_on = hand_on;
        h.prepare_legacy_promotion(id, from, to, proof).await.map_err(|error| error.to_string())?;
    }
    let mut fds = fds;
    if rec.mail_manifest().is_some() {
        let journal = fds.pop().ok_or_else(|| "the native journal descriptor is missing".to_owned())?;
        rec.attach_journal(std::fs::File::from(journal));
    }
    // This await occurs on the handover receiver, never the control loop draining its source
    // sink. Native descriptors are still owned and no process is stopped on precommit refusal.
    let mut prepared = h.prepare_native_import(rec, handed).await?;
    if let Some(manifest) = &manifest {
        // The journal commit already advanced ownership. Register its one receipt before Open
        // can request the destination certificate; legacy promotion would advance it again.
        native_journal_receipt(manifest, prepared.handed.as_deref().map(|h| &h.core), given.as_ref())?;
    }
    // Signed for by the runner it came from, or by none (one from before): it goes on at its epoch
    // until this runner is promoted, its receipt waiting till then (docs/ci.md W27-R08).
    let taken = |r: Result<(), String>| {
        if manifest.is_none() && !durable_legacy && r.is_ok()
            && let Some(s) = h.get(id)
        {
            let (epoch, events) = {
                let core = s.core.lock();
                (core.state.epoch, core.events().cloned().collect::<Vec<_>>())
            };
            crate::runner::custody::took_blue_green(id, epoch, &events, given.as_ref());
        }
        r
    };
    if let Some(handed) = &mut prepared.handed {
            let mut fds = fds.into_iter();
            let (Some(stdin), Some(stdout), Some(stderr)) = (fds.next(), fds.next(), fds.next()) else { return Err("native process descriptors are incomplete".into()) };
            handed.process.stdin = stdin.into_raw_fd();
            handed.process.stdout = stdout.into_raw_fd();
            handed.process.stderr = stderr.into_raw_fd();
            handed.process.foreign = true;
    }
    taken(h.move_in_prepared(prepared).await.map_err(|e| format!("{e:#}")))
}

/// W27-R08: bind the untrusted hand-on to this retained timeline before building its receipt.
/// The server alone validates its signer, root and complete revocations before certification.
#[cfg(unix)]
fn native_journal_hand_on(manifest: &crate::harness::mail::TransferManifest, frozen: Option<&crate::harness::core::Frozen>, hand_on: Option<&serde_json::Value>) -> Result<(), String> {
    use kiln_ci::identity::{chain::{Actor, Identity}, custody, statement};
    if !crate::runner::custody::signs() { return Ok(()); }
    let frozen = frozen.ok_or_else(|| "the signed native journal has no retained source timeline; custody stays held".to_owned())?;
    let (source_epoch, events) = frozen.handover_source();
    if source_epoch != manifest.source_epoch || source_epoch.checked_add(1) != Some(manifest.destination_epoch) {
        return Err("the signed native timeline does not match the journal ownership epochs".into());
    }
    let hand_on = hand_on.ok_or_else(|| "the signed native journal has no source hand-on; custody stays held".to_owned())?;
    let given = crate::runner::custody::statement_of(hand_on).ok_or_else(|| "the native source hand-on has no readable statement".to_owned())?;
    let body: custody::Body = crate::wire::payload(crate::wire::unread::Wire::RunnerPayload, "NativeHandOn", &given.body.to_string()).map_err(|error| format!("the native source hand-on has no readable custody body: {error}"))?;
    let source = Identity::parse(&body.by).ok().is_some_and(|identity| matches!(identity.actor, Actor::Agent { agent, epoch, .. } if agent == manifest.session && epoch == source_epoch));
    if !source || hand_on.pointer("/dsseEnvelope/payloadType").and_then(serde_json::Value::as_str) != Some(statement::PAYLOAD_CUSTODY)
        || given.v != statement::VERSION || body.v != custody::VERSION || body.kind != custody::Kind::HandedOn || body.epoch != Some(manifest.destination_epoch)
        || body.to != crate::custody::at_epoch(&body.by, manifest.destination_epoch)
        || given.subject.as_slice() != [crate::custody::timeline(&manifest.session, source_epoch, events)]
    {
        return Err("the native source hand-on does not name this exact session timeline and destination epoch".into());
    }
    Ok(())
}

#[cfg(unix)]
fn native_journal_receipt(manifest: &crate::harness::mail::TransferManifest, frozen: Option<&crate::harness::core::Frozen>, hand_on: Option<&serde_json::Value>) -> Result<(), String> {
    native_journal_hand_on(manifest, frozen, hand_on)?;
    if let (Some(frozen), Some(hand_on)) = (frozen, hand_on) {
        let (_, events) = frozen.handover_source();
        crate::runner::custody::took(&manifest.session, manifest.destination_epoch, events, hand_on)?;
    }
    Ok(())
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

/// The next event taken ahead of its turn, or never when there's none.
async fn next_taken(taken: &mut VecDeque<Ev>) -> Ev {
    match taken.pop_front() {
        Some(ev) => ev,
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

/// A file watched for the runner's loop (goal 9): its folder, for changes to its name alone, so an
/// editor's write-and-rename is seen as the write in place is. None for a path with no folder.
fn watch_file(path: &std::path::Path) -> Option<Arc<crate::os::watch::Watched>> {
    let (dir, name) = (path.parent()?, path.file_name()?.to_str()?);
    Some(crate::os::watch::Watched::new(&[dir.to_path_buf()], false, &[name]))
}

/// A watched file's next change, or never without one.
async fn watched(w: Option<&Arc<crate::os::watch::Watched>>) {
    match w {
        Some(w) => w.changed().await,
        None => futures::future::pending().await,
    }
}

/// A conversation store's writer's next commit, or never without one.
async fn committed(st: Option<&crate::convo::store::Store>) {
    match st {
        Some(st) => st.committed().await,
        None => futures::future::pending().await,
    }
}

/// An older server's cadence for a runner's heartbeat (no `CAP_LEASES`): it renews a leg's grant
/// only from a `Heartbeat` naming it, on a lease it doesn't tell, and its runners said one every
/// 10 s. Kept for that server alone, and only while legs run (goal 9's (c): another system's
/// protocol, with no word of its own); a lease server's renewals are each due on their own
/// (`crate::runner::lease::Leases::renew_due`).
const OLD_BEAT: Duration = Duration::from_secs(10);

/// What a heartbeat says: its free slots (none while it drains) and the jobs it runs, sorted.
fn beat_of(slots: u32, draining: bool, jobs: &HashMap<String, Running>) -> (u32, Vec<String>) {
    let free = if draining { 0 } else { slots.saturating_sub(u32::try_from(jobs.len()).unwrap_or(u32::MAX)) };
    let mut running: Vec<String> = jobs.keys().cloned().collect();
    running.sort();
    (free, running)
}

/// When an older server's next heartbeat is due ([`OLD_BEAT`]): now when what it says changed,
/// [`OLD_BEAT`] after the last while legs run, else none.
fn beat_due(said: &Option<(u32, Vec<String>, Instant)>, slots: u32, draining: bool, jobs: &HashMap<String, Running>) -> Option<Instant> {
    let (free, running) = beat_of(slots, draining, jobs);
    match said {
        None => Some(Instant::now()),
        Some((f, r, _)) if *f != free || *r != running => Some(Instant::now()),
        Some((_, r, at)) if !r.is_empty() => at.checked_add(OLD_BEAT),
        Some(_) => None,
    }
}

/// When the next agent session's identity is asked for again: its last ask's wait over (a
/// retry's backoff, `ask_agent_identity`). None while none waits.
fn identity_due(asked: &HashMap<String, (Instant, Duration)>) -> Option<Instant> {
    asked.values().filter_map(|(at, wait)| at.checked_add(*wait)).min()
}

/// The identity asks that wait no more: their session has its identity now, or is gone.
fn prune_identity_asks(asked: &mut HashMap<String, (Instant, Duration)>, sessions: &HashMap<String, u64>) {
    let live: std::collections::HashSet<String> = sessions.iter().filter_map(|(s, e)| crate::runner::custody::to_ask(s, *e).map(|e| crate::runner::identity::slot("agent", Some(s), Some(e)))).collect();
    asked.retain(|slot, _| live.contains(slot) && crate::runner::identity::cert(slot).is_none());
}

/// A runner's first look at an upgrade or a relaunch, this long after it started: as it was.
const WATCHDOG_SETTLE: Duration = Duration::from_secs(5);
/// How long agent sessions mid-turn (and reach runs) get to finish before an upgrade that would
/// stop them goes ahead.
const HARNESS_GRACE: Duration = Duration::from_secs(600);
/// How long a relaunch asked of launchd may take before this runner carries on as it is.
const RELAUNCH_WAIT: Duration = Duration::from_secs(600);
/// An ephemeral runner's wait for its result's receipt, then for a job at all.
const EPHEMERAL_RECEIPT_WAIT: Duration = Duration::from_secs(30);
const EPHEMERAL_JOB_WAIT: Duration = Duration::from_secs(900);
/// How long a job's count is kept once its events are let go (a kept leg is let go well within).
const JOURNAL_KEPT: Duration = Duration::from_secs(86_400);

#[allow(clippy::disallowed_methods)] // (e) picked graces before it acts (a cancel's, a take-over's, an exit's last word) and a test build's delays; its loop's every wait is an event or one item's due time (goal 9)
pub async fn run(o: Opts, shutdown: Shutdown) -> anyhow::Result<()> {
    crate::os::start_reaper();
    // Its binary read beside its first connection, which says its build (`updates::runner_build`).
    crate::updates::open_own();
    crate::updates::read_own();
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
    // The machine it is, said to the server as it connects (`AgentMsg::Machine`).
    let machine = crate::rt::blocking(crate::os::machine_id).await.ok().flatten().and_then(|id| crate::join::machine_key(Some(&id)));
    if name != crate::pki::sanitize_name(&o.name) {
        tracing::info!(certificate = %name, requested = %o.name, "using the agent name from this agent's certificate");
    }
    // Its sockets (`kiln run`'s, `kiln spawn`'s) are named by the name it was started with, never by
    // its machine id: their paths stay the ones its sessions were given before it moved, and a Mac's
    // temporary folder leaves a socket's path no room for an id (docs/machines.md).
    let sock = crate::pki::sanitize_name(&o.name);
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
    // kiln's root, which a hand-over's word the server signs chains to (docs/ci.md W27-R08).
    let root = ca.clone();
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
    // What the server sends, handed out by the net thread (runner-fan; goal 10, plan 01a1a87b2dae step 4): an agent session's
    // frames to the loop on the shard that owns the session (`loops::session_loop`), the rest to this loop, the runner's
    // order kept across them by the fan's watermark (`crate::wire::fan`, read by the server's heads: `peek_server`).
    let (fan, crate::wire::fan::Loops { queues, marks }) = crate::wire::fan::Fan::with(crate::wire::fan::Server, crate::wire::fan::shards(), net::EVENTS_BYTES);
    let net = net::start_with(net::Spec { bind: "0.0.0.0:0".parse()?, ca_der: ca, leaf, server: false, dial_deadline: net::DIAL_DEADLINE, revoked_keys: Vec::new() }, net::OUT_BYTES, Box::new(fan))?;
    let mut queues = queues.into_iter();
    let events = queues.next().ok_or_else(|| anyhow::anyhow!("the runner's fan made no control loop"))?;
    let session_queues: Vec<crate::memory::queue::Receiver<crate::wire::fan::Routed>> = queues.collect();
    // What the session loops hand this loop while blue/green decides what goes on to a new build (`loops::Deferral`).
    let (deferral, deferred_rx) = loops::Deferral::new(session_queues.len());
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
    let mut paths = if o.ephemeral {
        crate::tunnel::Paths::new(&o.server_url, &quic_addr)
    } else {
        crate::tunnel::Paths::remembering(&o.server_url, &quic_addr, crate::upgrade::own_dir(&o.state_dir, o.instance.as_deref()).join("transport"))
    };

    // The uplink, and every queue this loop takes from, lock-free (`crate::memory::queue`; goal 5, plan
    // 01a1a59c2aae, its top goal), each with the bound its channel had: an async-channel's wait took
    // event-listener's `Mutex`, its senders' shards and this loop's taking it, this loop on every turn for
    // each it waited on. The uplink holds `UPLINK_PLACES` messages at most, its senders waiting past them.
    let (Uplink(up_tx), up_rx) = Uplink::new();
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
    let (adopt_tx, adopt_rx) = crate::memory::queue::places::<crate::reach::node::Adoption>(&crate::memory::owners::CHANNELS, 4);
    let reach = o.reach.filter(|_| !o.ephemeral).map(|accept| {
        // `[runner] on_battery` and `[reach]`'s placement settings of its own kiln config, kiln's
        // own without one (W05-R26; W05-R09, W05-R39, LB08).
        let own = o.config.as_deref().and_then(|p| crate::config::Config::load(p).ok());
        let on_battery = own.as_ref().map(|c| c.runner.on_battery).unwrap_or_default();
        let place = own.as_ref().map_or_else(kiln_ci::place::runner::Settings::default, |c| {
            c.reach.place().unwrap_or_else(|e| {
                tracing::warn!(error = %e, "placement here decides by kiln's own settings");
                kiln_ci::place::runner::Settings::default()
            })
        });
        let cfg = crate::reach::node::Config { adopt: Some(adopt_tx.clone()), objects: None, root: root.clone(), on_battery, place, ..crate::reach::node::Config::for_runner(&name, &o.state_dir, accept) };
        (Arc::clone(&cfg.using), cfg.mirrors.clone(), crate::reach::node::start(cfg))
    });
    let reach = reach.and_then(|(using, mirrors, started)| match started {
        Ok((handle, said)) => {
            crate::reach::node::set_global(handle.clone());
            // The runs this runner follows for its agents, kept across restarts (docs/reach.md §7, T3).
            crate::reach::told::start(&o.state_dir.join("reach"), take_over_pending.is_some());
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
            Some(handle)
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "reach is off on this runner");
            None
        }
    });
    let uplink = Uplink(up_tx);
    // What this loop says itself goes on a channel of its own, never waiting: the uplink is
    // bounded and this loop is all that drains it, so a send of its own into a full one waited
    // for good. Its jobs, sessions and feeds kept the uplink full (a conversation store's
    // catch-up after a reconnect), the loop stopped, its heartbeats with it, and the server
    // dropped a runner that never heard so: offline for hours while it read connected.
    // What it says is as much as the loop does, so this one needs no bound.
    let (own_tx, own_rx) = crate::memory::queue::unbounded::<Up>(&crate::memory::owners::CHANNELS);
    let own = Uplink(own_tx);
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
    // What each run here does goes to the server as it ends (docs/needs.md).
    if !o.ephemeral && !o.outpost {
        crate::sense::run::install(uplink.clone());
    }
    // Its facts, probed on each connection, when the server asks, and again as what they're found
    // from changes (docs/ci.md W13-R01; goal 9: they were probed again every 10 minutes): a folder
    // they find tools in, a container engine starting or stopping, a run failing for a tool not
    // found (`crate::runner::probe::Asks`). What comes while a probe runs gives one after it.
    let probing = Arc::new(crate::runner::probe::Probing::default());
    if !o.ephemeral && !o.outpost {
        let (up, dir, declared, busy, stop) = (uplink.clone(), o.work_dir.clone(), o.declared.clone(), Arc::clone(&probing), shutdown.clone());
        crate::rt::spawn(async move {
            use futures::FutureExt as _;
            let asks = crate::runner::probe::Asks::new();
            loop {
                futures::select_biased! {
                    () = stop.cancelled().fuse() => return,
                    looked = asks.next().fuse() => probe_facts_as(&up, &dir, &declared, &busy, looked),
                    () = crate::runner::probe::gpu_change().fuse() => start_facts_probe(&up, &dir, &declared, &busy, false, true),
                }
            }
        });
    }
    let (done_tx, done_rx) = crate::memory::queue::unbounded::<JobResult>(&crate::memory::owners::CHANNELS);
    let mut buffer: VecDeque<(u32, Kind, Vec<u8>)> = VecDeque::new();
    let mut buffered = 0usize;
    // Jobs' results the server hasn't said it has (`ServerMsg::Received`), each encoded, by job:
    // sent again after a reconnect, so one sent as the connection dropped isn't lost.
    let mut unreceived: HashMap<String, (Instant, Vec<u8>)> = HashMap::new();
    // Each leg's signed results (`AgentMsg::LegResults`), encoded, by job: sent again with its
    // result, before it, until the server says it has that (docs/ci.md W27-R05).
    let mut signed_unreceived: HashMap<String, Vec<u8>> = HashMap::new();
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
    // When to dial the server and how long to give a dial, learned from this machine's dials, kept across restarts.
    let kept = (!o.ephemeral).then(|| o.state_dir.join("redials"));
    let learned = kept.as_deref().map(redial::Learned::kept).transpose().unwrap_or_else(|e| {
        tracing::warn!(error = %format!("{e:#}"), "what this runner learned of its dials doesn't read: learning it again");
        None
    });
    let mut redials = redial::Redials::new(learned.unwrap_or_default(), kept, Instant::now());
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
    let (term_tx, term_rx) = crate::memory::queue::places::<(String, PtyOut)>(&crate::memory::owners::CHANNELS, 1024);
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
    // Each with when its session handed it on (`crate::harness::session::Taken`): its frame says how long this
    // runner held it (`crate::wire::stamp`; goal 10, plan 01a1a87b2dae step 5).
    let (harness_tx, harness_rx) = crate::memory::queue::places::<crate::harness::session::Taken>(&crate::memory::owners::CHANNELS, 8192);
    // Their updates over the channel, once the server says it keeps it (docs/wire.md): kept until
    // the server has them, and sent again after a reconnect from where it has them, never whole.
    let mut updates = crate::wire::uplink::Updates::new(format!("{:016x}", rand::random::<u64>()));
    // What each session's stream carried of its changeset, so a server that takes patches gets
    // only what changed.
    let mut sent_changes = SentChanges::default();
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
    let mut work_said: Option<u64> = None;
    // The server issues identities (docs/ci.md W27-R01): this process's sessions' epochs, and
    // when each identity was last asked for and how long until it's asked for again.
    let mut server_identity = false;
    // And takes what legs sign as themselves of what they hand on (docs/ci.md W27-R05, R06).
    let mut server_signs = false;
    // The server takes custody records (docs/ci.md W27-R08).
    let mut server_custody = false;
    // The server takes runners back as fast as it can, and so takes word that what this runner sends on its Welcome is
    // out (`AgentMsg::Settled`; docs/resilience.md R5).
    let mut server_admission = false;
    let mut session_epochs: HashMap<String, u64> = HashMap::new();
    let mut identity_asked: HashMap<String, (Instant, Duration)> = HashMap::new();
    // The server takes word that a frame of its couldn't be read here (`AgentMsg::Unread`): of each
    // kind its newer build sends once an hour on a connection, the rest folded into the next word
    // (`wire::unread::Told`).
    let mut server_unread = false;
    let mut told = crate::wire::unread::Told::default();
    // The channel's updates go (a server that keeps it welcomed this runner since it started).
    let mut updates_on = false;
    let mut mail_replay = MailReplay::default();
    // The server's commands on the channel (`crate::wire::commands`): what this runner has of
    // each stream; and the one being applied, answered at the top of the next turn of the loop
    // (however its arm ended), taken unless its arm refused it.
    let mut wire_in = crate::wire::channel::Receiver::timed();
    // Which of the server's command streams are current: an item of one let go of is dropped.
    let mut wire_current = crate::wire::commands::Current::new();
    let mut answer_after: Option<(String, u64, Option<(crate::wire::channel::Refusal, String)>)> = None;
    let mut harness_operations=HarnessOperations::new();
    #[cfg(unix)]
    let mut legacy_progress = LegacyProgress::default();
    let mut native_handover = NativeHandover::default();
    // When it says what it has of them again soon after a link came up, and how many more times.
    let mut have_soon: Option<(Instant, u32)> = None;
    // What the server's copy of each session here has, said in digests on this connection: a
    // stream that starts sends only what the copy lacks (`crate::wire::sums`).
    let mut server_kept: HashMap<String, proto::KeptSession> = HashMap::new();
    // The sessions that announced themselves in this process (their Open went by): one that
    // hasn't yet starts its stream with its announcement, not before it (which then comes whole).
    let mut announced: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Blue/green: this runner's upgrade under way (it is the old one), its steps' results, and (it is
    // the new one) the runner it takes over from.
    #[cfg_attr(not(unix), allow(unused_variables))]
    let (bg_tx, bg_rx) = crate::memory::queue::unbounded::<Bg>(&crate::memory::owners::CHANNELS);
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
    let mut forward: Option<crate::memory::queue::Sender<Vec<u8>>> = None;
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
    // The store's tee, which holds it so that it can be handed to a new build
    // (`crate::bluegreen::Frame::Stores`), or in a new build taking over, come from the runner it
    // takes over from.
    #[cfg_attr(not(unix), allow(unused_variables, unused_assignments))]
    let mut convo_held: Option<crate::convo::store::Held> = None;
    // A new build taking over doesn't know yet which store it goes on with: its feed starts then.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut stores_wait = false;
    // How many agent sessions it has at work at once (docs/agents.md, "How many agents a machine
    // runs"): what was set for this machine from the server (kept here for its restarts), else its
    // config's, else kiln's default. What the server sets is taken as it says it, and the config is
    // read again as it changes.
    let mut limit_asked = crate::harness::limit::kept(&o.state_dir);
    let mut limit_config = o.declared.max_agent_sessions;
    // What the server was told this machine holds, on this connection; and whether it takes that.
    let mut limit_said: Option<crate::harness::limit::Held> = None;
    let mut server_limit = false;
    // The server takes leases (docs/ci.md W05-R34), said on this connection: this runner renews its
    // grants (`Renew`, numbered) and fences what it sends about their legs by them. Each leg's grant,
    // as its assignment named it, is kept while the leg runs and until the server has its result.
    let mut server_leases = false;
    // The features the server speaks by name (docs/wire.md, "Named features"), said on this
    // connection after its caps say `CAP_FEATURES`: none from a server before them.
    let mut server_features: Vec<String> = Vec::new();
    #[cfg(unix)]
    let mut feature_negotiation = FeatureNegotiation::default();
    let mut grants: HashMap<String, proto::Grant> = HashMap::new();
    let mut renew_seq = 0u64;
    // Where each grant's lease ends by this machine's clock, which counts sleep, and which grants
    // stop once it has (W05-R37); the jobs stopped so, whose results say it.
    let mut leases = crate::runner::lease::Leases::default();
    let mut lease_lost: std::collections::HashSet<String> = std::collections::HashSet::new();
    let harness = (!o.ephemeral && o.terminal).then(|| {
        // Agents' workspaces of their own: one place per machine, whichever runner process holds them.
        crate::workspace::slates::init(&o.state_dir);
        // What an agent changes in the runner's own folders is an agent's own (docs/agents.md).
        crate::harness::whose::note_kiln(&o.state_dir);
        let waits = cfg!(unix) && take_over_pending.is_some() && !crate::convo::off();
        if !waits {
            convo = crate::convo::open(&own_dir, crate::config::Conversations::default().upkeep());
        }
        let sink = if convo.is_some() || waits {
            let (sink, tee) = crate::convo::store::tee_held(harness_tx.clone(), convo.clone());
            convo_held = Some(tee);
            sink
        } else {
            harness_tx.clone().into()
        };
        stores_wait = waits;
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
        h
    });
    // Bind the local Reach endpoint to the actual Host before restoring adapters that
    // can immediately ask it to run work (attention plan 01a199e1b177752e84dad5dc990e6a15).
    if let Some(handle)=&reach{
        let path=crate::reach::local::path(&sock,&o.state_dir);
        crate::browser::set_home(&o.state_dir);
        if take_over_pending.is_some(){crate::reach::local::set_socket(path.to_string_lossy().into_owned());}
        else{match crate::reach::local::serve(handle.clone(),name.clone(),path,false,harness.clone()){
            Ok(p)=>crate::reach::local::set_socket(p.to_string_lossy().into_owned()),
            Err(e)=>tracing::warn!(error=%format!("{e:#}"),"`kiln run` will not be available on this machine"),
        }}
    }
    if let Some(h)=&harness{h.restore_from(convo.as_ref());}
    #[cfg(unix)]
    legacy_progress.request(harness.as_ref(), &mut harness_operations, &bg_tx, LegacyCause::Observe);
    // Which of the machine's processes are its agent sessions' own: a run they ask for elsewhere
    // is their act (docs/ci.md W27-R08).
    if let Some(h) = &harness {
        let h = Arc::clone(h);
        crate::runner::custody::set_sessions(move || h.harness_pids());
    }
    // W24-R31: the reaper of this machine's projects (the folders its agent sessions work in) and
    // its ~/.cache, which asks an idle agent of a project to clean what it doesn't recognize.
    if let Some(h) = harness.as_ref().filter(|_| !o.outpost && !o.ephemeral) {
        use crate::harness::model::{Command, SendMode, Status};
        let (list, tell) = (Arc::clone(h), Arc::clone(h));
        crate::runner::reap_projects::set_sessions(move || {
            list.list()
                .into_iter()
                .filter(|i| !i.subagent)
                .map(|i| crate::runner::reap_projects::Session { id: i.id, cwd: i.cwd, idle: i.status == Status::Idle, queued: i.queue.into_iter().map(|q| q.id).collect() })
                .collect()
        });
        crate::runner::reap_projects::set_tell(move |session, id, text| tell.command(session, Command::Send { id, text, mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None }));
        crate::runner::reap_projects::start(name.clone());
    }
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
    // Started again by a blue/green upgrade's hand-over and its rollback (Unix).
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut shipper = convo.as_ref().map(|st| start_feed(st, harness.as_ref(), &uplink.0));
    // What `kiln spawn` in this machine's terminals talks to.
    let harness_sock = harness.as_ref().and_then(|h| {
        let path = crate::harness::socket::path(&sock, &o.state_dir);
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
                        h.adopt_spool(&from, data).await
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
    // Its battery, as last said: read again as the OS says its power source or its charge moved,
    // and on each connection (goal 9; it was read once a minute and said again every 10 minutes).
    let mut power_said: Option<crate::os::battery::Battery> = None;
    let mut power_seen = crate::sense::pace::powers();
    let mut noticed: Option<String> = None;
    // The harnesses this machine offers, as last reported: looked at again as what they're found
    // by changes (`crate::harness::kinds`; goal 9: it was every minute).
    let mut harness_kinds_sent: Option<String> = None;
    // Its host's sample is taken on the blocking pool, never on this loop: on a Mac it reads the
    // work dir's volume through CacheDelete (an XPC call, slow on a full disk) and, before, every
    // process; inline it held this loop's shard for all of every interval (2026-11-04), so the
    // server's commands went unanswered and its agent sessions read offline. One at a time.
    let sampler = Arc::new(parking_lot::Mutex::new(crate::host::Sampler::new(Some(&o.work_dir))));
    let sampling = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The topology's reader of this machine's interfaces (LB08), read at the host sample.
    let topo_reader = Arc::new(parking_lot::Mutex::new(crate::reach::topo::Reader::default()));
    // Its samples are taken as something moves (goal 9, W05-R26): a run's start or end, the OS's
    // word on its pressure, clocks or power (`crate::sense::pace`), a connection. They were every
    // `sample_seconds`; the server's own stays at it, a person's setting.
    if !o.ephemeral && !o.outpost {
        crate::sense::pace::hear();
    }
    let mut moves_seen = crate::sense::pace::moves();
    // Its grants' leases (W05-R37), each due on the lease clock, which counts a sleep: the next to
    // stop, and the next renewal (`crate::runner::lease::Leases::renew_due`). They were looked at
    // twice a second, and renewed three times a lease.
    let stop_alarm = crate::runner::lease::Alarm::start(shutdown.clone());
    let renew_alarm = crate::runner::lease::Alarm::start(shutdown.clone());
    // What custody has to send or answer, as last taken (`crate::runner::custody::moves`), and what
    // its QUIC layer let go below the channel: each is an event (they were looked at every 10 s
    // and every 5 s).
    let mut custody_seen = 0u64;
    let mut drops_seen = crate::metrics::net().dropped_so_far();
    let mut lost_said = 0u64;
    // Its config file and its own binary, watched (goal 9): the config's agent limit and its say on
    // upgrades read again as it changes, a new kiln on disk taken up as it lands. They were read
    // every 10 s and every 5 s.
    let config_watch = o.config.as_deref().and_then(watch_file);
    let exe_watch = if !o.ephemeral && standalone && crate::os::IN_PLACE_UPGRADE { exe.as_deref().and_then(watch_file) } else { None };
    // An older server (no `CAP_LEASES`) renews a leg's grant only from a heartbeat naming it, on a
    // lease it doesn't say: to it alone, while legs run, a heartbeat goes at that server's own
    // cadence (`OLD_BEAT`), and as what it says changes.
    let mut beat_said: Option<(u32, Vec<String>, Instant)> = None;
    // Legs let go, as last looked at: a terminal into one closes as it goes.
    let mut legs_gone_seen = crate::runner::kept::gones();
    // When the first job's count kept past its events falls due to be let go.
    let mut journal_next: Option<Instant> = None;
    // Its terminals' shells as last told the scan of agent CLIs (`crate::aisessions`).
    let mut shells_told: Option<u64> = None;
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
    // Events taken off their channels ahead of their turn: each goes before anything else but
    // what the network says (a job's end waits behind what its job sent before it, `Ev::Done`).
    let mut taken: VecDeque<Ev> = VecDeque::new();
    // The legs of what this loop takes (`crate::route_latency::InLeg`; runner-fan): the event it holds now, since when and
    // what it is; and the answers to the server's commands on their way to the net thread through `own`, each with when it
    // was made and what it answered, in the order they went there.
    let mut holding: Option<(Instant, crate::route_latency::InKind)> = None;
    let mut answers_at: VecDeque<(Instant, crate::route_latency::InKind)> = VecDeque::new();
    // What the session loops share with this one, and the session loops, each on its shard beside the sessions it owns
    // (`crate::harness::host::owner_shard`; runner-fan).
    let shared = Arc::new(loops::Shared {
        harness: harness.clone(),
        own: own.clone(),
        uplink: uplink.clone(),
        root: root.clone(),
        name: name.clone(),
        instance_id: instance_id.clone(),
        state_dir: o.state_dir.clone(),
        server_unread: std::sync::atomic::AtomicBool::new(false),
        server_custody: std::sync::atomic::AtomicBool::new(false),
        net: net.clone(),
        have: session_queues.iter().map(|_| crate::rcu::Rcu::new(Vec::new())).collect(),
        link: std::sync::atomic::AtomicU64::new(0),
        adopt: crate::rcu::Rcu::new(HashMap::new()),
        deferral,
    });
    for (shard, rx) in session_queues.into_iter().enumerate() {
        let (s, m, me) = (Arc::clone(&shared), Arc::clone(&marks), crate::wire::fan::session_loop(shard));
        crate::rt::spawn_owner(shard, async move {
            if let Err(p) = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(loops::session_loop(s, me, rx, m))).await {
                crate::metrics::global().panics_caught.inc();
                tracing::error!(shard, panic = %crate::sup::panic_message(p.as_ref()), "a session loop of the runner's panicked; contained");
            }
        });
    }
    // Said ended as it goes, however it goes: nothing waits for this loop then.
    let _ending = marks.ending(crate::wire::fan::CONTROL);
    // The fan's number of the frame this loop holds, marked taken once it's done with it; and of one a session loop
    // handed it, with that loop's place.
    let mut routed_n: Option<u64> = None;
    let mut deferred_n: Option<(usize, u64)> = None;

    // What this runner's placement was last told of its draining (AUDIT O1: its runs wait, and its
    // hint says it).
    let mut drain_said = false;
    'events: loop {
        // The event this loop held last is done with: its hold, what the events behind it waited for; the frame it was,
        // taken (what waits for it goes).
        let last = holding.take();
        if let Some((since, kind)) = last {
            crate::route_latency::observe_in(crate::route_latency::InLeg::Held, kind, since.elapsed());
        }
        // The command it applied, answered: one of a session's own streams' (taken here while blue/green decided) said to
        // that session's loop too, which takes it up before its stream's next item (runner-fan). Then the frame marked
        // taken, so what's ordered after it, and a session loop that handed it here, go on after its answer.
        if let Some((stream, seq, refusal)) = answer_after.take() {
            let a = match refusal {
                Some((code, why)) => wire_in.refused(&stream, seq, code, why),
                None => wire_in.took(&stream, seq),
            };
            if crate::wire::commands::session_of(&stream).is_some() {
                let has = wire_in.state(&stream);
                shared.adopt.update(|m| {
                    m.insert(stream.clone(), has.clone());
                });
            }
            answers_at.push_back((Instant::now(), last.map_or(crate::route_latency::InKind::Command, |(_, k)| k)));
            own.0.send(Up::Term(AgentMsg::Wire(a))).await.ok();
        }
        if let Some(n) = routed_n.take() {
            marks.took(crate::wire::fan::CONTROL, n);
        }
        if let Some((k, n)) = deferred_n.take() {
            marks.took(crate::wire::fan::session_loop(k), n);
            shared.deferral.taken(k);
        }
        // Draining, an upgrade pending or retiring, as the last event left it: its admission told
        // once it changes (an event, not a timer).
        let drain_now = draining || upgrade_pending || retiring;
        if drain_now != drain_said
            && let Some(r) = &reach
        {
            r.feed(crate::reach::node::In::Drain(drain_now)).await;
            drain_said = drain_now;
        }
        // Blue/green (Unix): from a new build's start until nothing's handed to it any more, this loop decides what goes on
        // there, the session loops handing it their frames (`loops::Deferral`).
        #[cfg(unix)]
        {
            let handing = retiring || moving.is_some() || !moved.is_empty();
            if handing && !shared.deferral.under_way() {
                shared.deferral.begin().await;
            } else if !handing && shared.deferral.under_way() {
                shared.deferral.end();
            }
        }
        if let Some(until) = lingering
            && ((crate::observe::collect::get().is_none_or(|c| c.stopped()) && crate::observe::last_written()) || Instant::now() >= until)
            && native_handover.idle()
        {
            return Ok(());
        }
        // Ephemeral agents leave once their one job is done and everything is delivered: its
        // result received, or given 30 s (a server older than receipts never says).
        if o.ephemeral && served_dedicated && jobs.is_empty() && buffer.is_empty() && up_rx.is_empty() && own_rx.is_empty() && unreceived.values().all(|(at, _)| at.elapsed() >= EPHEMERAL_RECEIPT_WAIT) {
            tracing::info!("ephemeral agent finished its job; exiting");
            crate::rt::sleep(Duration::from_millis(500)).await;
            return Ok(());
        }
        if o.ephemeral && !served_dedicated && jobs.is_empty() && started.elapsed() >= EPHEMERAL_JOB_WAIT {
            anyhow::bail!("ephemeral agent received no job within 15 minutes");
        }
        if draining && jobs.is_empty() && o.ephemeral {
            return Ok(());
        }
        if retiring && jobs.is_empty() && terms.is_empty() && harness_operations.is_empty() && retire_at.is_some_and(|t| Instant::now() >= t) {
            tracing::info!("handed everything to the new build, and its own jobs are done: this runner retires");
            // What its tracers' spans counted, written before it goes, as a stop's is.
            crate::observe::ddtrace::finish(Duration::from_secs(3)).await;
            if let Some(job) = retire_job.take() {
                own.msg(AgentMsg::UpgradeLog { job, line: "the old runner's last jobs are done: it retired".to_owned() }).await;
                crate::rt::sleep(Duration::from_millis(300)).await;
            }
            if let Some(sp) = retire_span.take() {
                sp.record("result", "ok");
            }
            // Its other engines' browsers don't outlive it (Chromium's go with their pipes).
            crate::browser::driven::stop_all().await;
            return Ok(());
        }
        // What falls due, and when, asked again each turn (goal 9: a clock only for a due item, armed
        // for the earliest): what this turn moved moves them.
        let holds_legs = grants.keys().any(|j| jobs.contains_key(j) || unreceived.contains_key(j));
        stop_alarm.set(leases.stop_due());
        renew_alarm.set(if connected && server_leases { leases.renew_due(holds_legs) } else { None });
        let old_beat = (connected && !server_leases).then(|| beat_due(&beat_said, o.slots, upgrade_pending || retiring, &jobs)).flatten();
        let chores_due = [crate::runner::custody::reach_due_at(), told.flush_due().filter(|_| server_unread), journal_next, identity_due(&identity_asked).filter(|_| server_identity && connected), old_beat].into_iter().flatten().min();
        let updates_due = if connected && updates_on { updates.due(Instant::now()) } else { None };
        let watchdog_due = [
            // Its first look, as it was: an upgrade or a relaunch is taken up no sooner than this.
            (started.elapsed() < WATCHDOG_SETTLE).then(|| started + WATCHDOG_SETTLE),
            // Past it, what's still running ends the wait as it ends (`Ev::Done`), not a clock.
            harness_wait.filter(|_| upgrade_pending).and_then(|t| t.checked_add(HARNESS_GRACE)).filter(|t| *t > Instant::now()),
            relaunch_asked.and_then(|t| t.checked_add(RELAUNCH_WAIT)),
            paths.due(connected),
        ]
        .into_iter()
        .flatten()
        .min();
        // An ephemeral runner's ends, below: given its result's receipt, or no job in its time.
        let ephemeral_due = o.ephemeral.then(|| {
            if served_dedicated { unreceived.values().map(|(at, _)| *at + EPHEMERAL_RECEIPT_WAIT).max() } else { Some(started + EPHEMERAL_JOB_WAIT) }
        })
        .flatten()
        .filter(|t| jobs.is_empty() && *t > Instant::now());
        // Its terminals' shells, told the scan of agent CLIs as they change (`crate::aisessions`, which
        // scans as their folders change; goal 9: it scanned with every sample).
        if let Some(ai) = &ai {
            let now = terms.iter().fold(u64::try_from(terms.len()).unwrap_or(0), |acc, (id, (pty, _))| {
                let mut h = std::hash::DefaultHasher::new();
                std::hash::Hash::hash(&(id, pty.pid()), &mut h);
                acc ^ std::hash::Hasher::finish(&h)
            });
            if shells_told != Some(now) {
                shells_told = Some(now);
                ai.terminals(terms.iter().filter_map(|(id, (pty, _))| Some((pty.pid()?, id.clone()))).collect(), uplink.0.clone());
            }
        }
        // Whether this turn's event comes off its own channel (none was taken ahead of its turn): a
        // job's end that does waits behind what its job sent (`Ev::Done`).
        let in_turn = taken.is_empty();
        let retirement_due=retire_at.filter(|_|retiring && jobs.is_empty() && terms.is_empty() && harness_operations.is_empty());
        let mail_budget = mail_replay.budget_observed();
        let ev = futures::select_biased! {
            () = shutdown_or_tick(&shutdown, lingering.is_some()).fuse() => Ev::Shutdown,
            // Taken ahead of their turn, in order: a frame put back behind what the session loops handed this loop before
            // it (runner-fan), a job's messages before its end.
            e = next_taken(&mut taken).fuse() => e,
            // A retained archive or restore that finished (attention plan 01a199e1b177752e84dad5dc990e6a15).
            answer = next_harness_operation(&mut harness_operations).fuse() => Ev::HarnessApplied(answer),
            held = mail_replay.completed().fuse() => Ev::MailReplayed(held),
            () = MailReplay::budget_changed(harness.as_ref(), mail_budget).fuse() => Ev::MailBudget,
            r = events.recv().fuse() => match r { Ok(r) => Ev::Routed(r), Err(_) => Ev::Net(None) },
            d = deferred_rx.recv().fuse() => match d { Ok((k, n, data)) => Ev::Deferred(k, n, data), Err(_) => Ev::Watchdog },
            r = done_rx.recv().fuse() => match r { Ok(r) => Ev::Done(r), Err(_) => Ev::Watchdog },
            () = renew_alarm.rung().fuse() => Ev::Renew,
            () = stop_alarm.rung().fuse() => Ev::Lease,
            () = crate::runner::custody::moved_since(custody_seen).fuse() => Ev::Heartbeat,
            () = sleep_until_opt(chores_due).fuse() => Ev::Heartbeat,
            () = crate::sense::pace::moved_since(moves_seen).fuse() => Ev::Sample,
            () = crate::sense::pace::powered_since(power_seen).fuse() => Ev::Power,
            () = crate::metrics::net().dropped_since(drops_seen).fuse() => Ev::Dropped,
            () = sleep_until_opt(updates_due).fuse() => Ev::Overdue,
            () = watched(config_watch.as_ref()).fuse() => Ev::Config,
            () = watched(exe_watch.as_ref()).fuse() => Ev::Watchdog,
            () = sleep_until_opt(watchdog_due).fuse() => Ev::Watchdog,
            () = paths.probed().fuse() => Ev::Watchdog,
            () = crate::runner::kept::gone_since(legs_gone_seen).fuse() => Ev::Watchdog,
            () = committed(convo.as_ref()).fuse() => Ev::ConvoStats,
            () = committed(mirror_store.as_ref()).fuse() => Ev::ConvoStats,
            () = crate::runner::warm::changed().fuse() => Ev::Work,
            () = crate::harness::kinds::changed().fuse() => Ev::Harnesses,
            () = sleep_until_opt(ephemeral_due).fuse() => Ev::Watchdog,
            () = sleep_until_opt(redials.deadline()).fuse() => Ev::Redial,
            () = sleep_until_opt(chaos_drop).fuse() => Ev::ChaosDrop,
            () = sleep_until_opt(wire_in.want_due().filter(|_| connected && server_wire)).fuse() => Ev::Wants,
            () = sleep_until_opt(have_soon.map(|(at, _)| at)).fuse() => Ev::HaveSoon,
            d = recv_opt(delayed.as_ref()).fuse() => match d { Some(due) => Ev::Delayed(due), None => Ev::Watchdog },
            t = term_rx.recv().fuse() => match t { Ok((s, out)) => Ev::Term(s, out), Err(_) => Ev::Watchdog },
            up = own_rx.recv().fuse() => match up { Ok(u) => Ev::Up(u), Err(_) => Ev::Watchdog },
            up = up_rx.recv().fuse() => match up { Ok(u) => Ev::Up(u), Err(_) => Ev::Watchdog },
            h = harness_rx.recv().fuse() => match h { Ok(t) => Ev::Harness(t.session, t.update, t.at), Err(_) => Ev::Watchdog },
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
            () = sleep_until_opt(retirement_due).fuse() => Ev::Watchdog,
        };
        // What the fan handed this loop: once the server's earlier frames it may not overtake are taken where they went. A
        // frame a session loop handed this loop meanwhile (blue/green) goes first, this one put back behind it, as each
        // may be one of those (runner-fan).
        let kind = in_kind(&ev);
        let ev = match ev {
            Ev::Routed(crate::wire::fan::Routed { ev, n, after, handed }) => {
                let dequeued = Instant::now();
                if !after.is_empty() {
                    let waited = futures::select_biased! {
                        _ = marks.wait(&after).fuse() => None,
                        d = deferred_rx.recv().fuse() => Some(d),
                    };
                    match waited {
                        Some(Ok((k, dn, data))) => {
                            taken.push_back(Ev::Deferred(k, dn, data));
                            while let Ok((k, dn, data)) = deferred_rx.try_recv() {
                                taken.push_back(Ev::Deferred(k, dn, data));
                            }
                            taken.push_back(Ev::Routed(crate::wire::fan::Routed { ev, n, after, handed }));
                            continue 'events;
                        }
                        // Nothing hands this loop frames any more: the marks alone.
                        Some(Err(_)) => {
                            marks.wait(&after).await;
                        }
                        None => {}
                    }
                }
                let order = dequeued.elapsed();
                if let NetEvent::Frame { at, .. } = &ev {
                    use crate::route_latency::{InLeg, observe_in};
                    observe_in(InLeg::Net, kind, handed.saturating_duration_since(*at));
                    observe_in(InLeg::Wait, kind, at.elapsed().saturating_sub(order));
                    if !after.is_empty() {
                        observe_in(InLeg::Order, kind, order);
                    }
                }
                routed_n = Some(n);
                Ev::Net(Some(ev))
            }
            Ev::Deferred(k, n, data) => {
                deferred_n = Some((k, n));
                Ev::Forwarded(data)
            }
            ev => ev,
        };
        // What this loop holds from now (runner-fan), by what it is.
        holding = Some((Instant::now(), kind));
        match ev {
            #[cfg(unix)]
            Ev::HarnessApplied(HarnessApplied::LegacyPromotion(result)) => {
                if let Err(error) = &result { tracing::warn!(%error, "legacy ownership promotion remains durably held"); }
                legacy_progress.finish(&result, harness.as_ref(), &mut harness_operations, &bg_tx);
            }
            Ev::MailReplayed(held) => mail_replay.finish(harness.as_ref(), held),
            Ev::MailBudget => mail_replay.budget_event(harness.as_ref()),
            Ev::HarnessApplied(HarnessApplied::Command { reply, result })=>{
                if let Some((stream,seq))=reply{
                    let answer=match result{Ok(())=>wire_in.took(&stream,seq),Err((code,why))=>wire_in.refused(&stream,seq,code,why)};
                    // On this loop's own channel, never the uplink it alone drains (W22-AC04), counted as an answer.
                    answers_at.push_back((Instant::now(), crate::route_latency::InKind::Command));
                    own.0.send(Up::Term(AgentMsg::Wire(answer))).await.ok();
                }else if let Err((code,why))=result{tracing::warn!(?code,%why,"an untracked archive or restore was refused");}
            }
            Ev::HarnessApplied(HarnessApplied::Native(NativePrepared::InPlace { exe, mtime, result })) => {
                let cancellation = native_handover.finish();
                match result {
                    Ok(sessions) if cancellation.is_none() => {
                        tracing::info!(sessions, "agent sessions handed to the new image, their processes still running");
                        let error = upgrade_in_place(&o, &exe, &terms);
                        tracing::error!(%error, "in-place upgrade failed; continuing on the running binary");
                        if let Some(host) = &harness { host.take_back(); }
                    }
                    Ok(_) => {
                        // Preparation succeeded, but no exec took place: restore the retained
                        // native handover record before shutdown/cancellation can end this turn.
                        if let Some(host) = &harness { host.take_back(); }
                        tracing::warn!(why = cancellation.as_deref().unwrap_or("handover cancelled"), "native handover cancelled; the running owner is retained");
                    }
                    Err(why) => tracing::warn!(%why, "the source handover was refused; the running owner is retained"),
                }
                upgrade_pending = false;
                exe_mtime = mtime;
            }
            #[cfg(unix)]
            Ev::HarnessApplied(HarnessApplied::Native(NativePrepared::BlueGreen { ready, sessions })) => {
                taken.push_front(Ev::Bg(Bg::Prepared { ready, sessions }));
            }
            #[cfg(unix)]
            Ev::HarnessApplied(HarnessApplied::Native(NativePrepared::Rollback { job, why, acks, allowed })) => {
                native_handover.finish();
                match allowed {
                    Ok(true) => taken.push_front(Ev::Bg(Bg::RollbackAuthorized { job, why, acks })),
                    Ok(false) => {
                        // A verified green owns the journal now. Keep both control/FD records
                        // fenced; neither a lost socket nor failed supervisor promotion can
                        // authorize killing that owner or resuming blue's old snapshot.
                        own.msg(AgentMsg::UpgradeDone { job, ok: false, error: Some("the native destination committed custody; the failed promotion needs reconciliation and the source remains fenced".into()) }).await;
                    }
                    Err(error) => own.msg(AgentMsg::UpgradeDone { job, ok: false, error: Some(format!("native ownership is uncertain; both retained owners stay fenced: {error}")) }).await,
                }
            }
            Ev::Shutdown => {
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
                native_handover.cancel(None, "the runner is shutting down".into());
                if !collecting && crate::observe::last_written()
                    && native_handover.idle()
                {
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
            Ev::Wants => {
                // Never waiting for the uplink's room: this loop is what drains it, and a want it
                // has no room for now is asked again on the gap's clock.
                for f in wire_in.wants(Instant::now()) {
                    let _ = uplink.0.try_send(Up::Term(AgentMsg::Wire(f)));
                }
            }
            Ev::HaveSoon => {
                // Never waiting for the uplink's room (this loop drains it): one with no room now
                // is said again soon, or with the next sample.
                if connected && server_wire {
                    let _ = uplink.0.try_send(Up::Term(AgentMsg::Wire(loops::have_of(wire_in.have(), &shared))));
                }
                have_soon = have_soon.filter(|(_, left)| *left > 1).map(|(_, left)| (Instant::now() + HAVE_SOON, left - 1));
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
                        if moving.is_some() || !native_handover.idle() {
                            own.msg(AgentMsg::UpgradeDone { job, ok: false, error: Some("a native handover already owns this runner's preparation; the existing owner is retained".into()) }).await;
                            continue;
                        }
                        // Known since this runner said its build; this loop never waits for it either way.
                        let from = crate::updates::own_binary_now().and_then(Result::ok).map_or_else(|| "unknown".to_owned(), |o| o.build);
                        let root = tracing::info_span!(target: "kiln::upgrade", "upgrade", runner = %name, from = %from, to = %build, upgrade_id = %job, result = empty, error = empty);
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
                                own.msg(AgentMsg::UpgradeLog { job: job.clone(), line }).await;
                                let (tx, j) = (bg_tx.clone(), job.clone());
                                let waited = std::thread::Builder::new().name("kiln-upgrade".into()).spawn(move || {
                                    let _ = match listener.ready(green.pid, Instant::now() + crate::bluegreen::READY_WITHIN) {
                                        Ok((conn, instance, custody, role, relays, stores, mail)) => tx.send_blocking(Bg::Ready(BlueGreenReady { job: j, conn: Arc::new(conn), instance: instance.clone(), custody, role: role.then_some(instance), relays, stores, mail })),
                                        Err(e) => tx.send_blocking(Bg::Failed { job: j, why: format!("{e:#}") }),
                                    };
                                });
                                moving = Some(Moving { green, instance: gid, root, healthy: Some(healthy), handing: false, items: Vec::new(), held: Vec::new(), relays: false, stores: None });
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
                                own.msg(AgentMsg::UpgradeDone { job, ok: false, error: Some(format!("couldn't start the new build beside this runner: {why}")) }).await;
                            }
                        }
                    }
                    Bg::Ready(ready) => {
                        if moving.is_some() { native_handover.prepare(&mut harness_operations, harness.clone(), NativePreparation::BlueGreen { ready, machine: name.clone() }); }
                    }
                    Bg::Prepared { ready: BlueGreenReady { job, conn, custody: _, role, relays, stores, .. }, sessions } => {
                        let cancellation = native_handover.finish();
                        let sessions = match sessions {
                            Ok(sessions) => sessions,
                            Err(why) => { taken.push_back(Ev::Bg(Bg::Failed { job, why })); continue; }
                        };
                        let Some(mv) = moving.as_mut() else {
                            // A stale completion cannot silently discard detached Saved/native
                            // handles. No destination was sent them from this operation yet.
                            if let Some(host) = &harness {
                                for (record, handed, _) in sessions {
                                    if let Err(error) = host.move_in(record, handed).await { tracing::error!(%error, "a stale native preparation could not resume; its source journal remains retained"); }
                                }
                            }
                            continue;
                        };
                        if let Some(h) = mv.healthy.take() {
                            h.record("result", "ok");
                        }
                        mv.handing = true;
                        mv.relays = relays;
                        let mut frames: Vec<(Frame, Vec<std::os::fd::RawFd>)> = Vec::new();
                        {
                            for (rec, handed, hand_on) in sessions {
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
                                let mut fds: Vec<_> = kept.as_ref().map(|(_, p)| p.iter().map(|f| f.as_raw_fd()).collect()).unwrap_or_default();
                                if let Some(journal) = rec.journal() { fds.push(journal.as_raw_fd()); }
                                // Signed for as it goes, to a new build that takes it so (docs/ci.md W27-R08).
                                let hand_on = hand_on.map(|b| b.to_string());
                                frames.push((Frame::Session { id: id.clone(), record: Box::new(rec.clone()), handed, hand_on }, fds));
                                moved.insert(crate::instances::Owned::Agent(id));
                                mv.items.push(Item::Session { record: Box::new(rec), kept });
                            }
                        }
                        if let Some(why) = cancellation {
                            // Every prepared native process now belongs to Moving's existing
                            // rollback records/pipes. Cancellation must never drop that result.
                            taken.push_front(Ev::Bg(Bg::Failed { job, why }));
                            continue;
                        }
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
                        // The machine's runner role, last, signed for as it goes to a new build that
                        // takes it so (docs/ci.md W27-R08; the Transfers plan's step 8). Its answer
                        // comes after every item's, so it pairs with none of them.
                        if let Some(to) = role.as_deref()
                            && let Some(hand_on) = crate::runner::custody::role_hand_on(to)
                        {
                            frames.push((Frame::Role { hand_on }, Vec::new()));
                        }
                        let (sessions, terminals) = (mv.items.iter().filter(|i| matches!(i, Item::Session { .. })).count(), mv.items.iter().filter(|i| matches!(i, Item::Terminal { .. })).count());
                        let line = format!("the new build is connected to the server: handing it {sessions} agent session(s) and {terminals} terminal(s), their processes running on");
                        own.msg(AgentMsg::UpgradeLog { job: job.clone(), line }).await;
                        // Its conversation stores go too, to a new build that takes them, first: let
                        // go of once what its sessions said last is in, and moved into the new
                        // build's folder, so that the server's feed of them, and the server's to
                        // this copy of its own, go on where they are and nothing is sent again.
                        // Its own feed and its copy end here (what the server answers them goes
                        // nowhere now); a rollback brings them back.
                        let giving = match (&convo_held, convo.is_some()) {
                            (Some(held), true) if stores => {
                                let there = crate::upgrade::instance_dir(&o.state_dir, &mv.instance);
                                let own = convo.take();
                                shipper = None;
                                let copy = if restoring.is_none() { mirror_store.take() } else { None };
                                if copy.is_some() {
                                    mirror = None;
                                }
                                let mut moves = vec![(own_dir.join(crate::convo::FILE), there.join(crate::convo::FILE))];
                                if copy.is_some() {
                                    moves.push((own_dir.join(crate::convo::MIRROR_FILE), there.join(crate::convo::MIRROR_FILE)));
                                }
                                mv.stores = Some(Given { held: held.clone(), moves: moves.clone(), own_dir: own_dir.clone(), aside: None });
                                Some((held.clone(), own, copy, moves))
                            }
                            _ => None,
                        };
                        let tx = bg_tx.clone();
                        let sent = std::thread::Builder::new().name("kiln-handover".into()).spawn(move || {
                            let mut acks = Vec::with_capacity(frames.len());
                            let mut broke: Option<String> = None;
                            let mut aside = None;
                            if let Some((held, own, copy, moves)) = giving {
                                let said = give_stores(&held, own, copy, &moves);
                                // The stores never hold an upgrade back: one the new build can't open
                                // it sets aside, kept whole, and goes on in a new one; one that keeps
                                // none (it couldn't even open a new one) still takes the rest.
                                match conn.send(&said, &[]).and_then(|()| conn.recv()) {
                                    Ok((Frame::StoresTaken { kept, own_aside, mirror_aside, note }, _)) => {
                                        aside = Some([own_aside.map(PathBuf::from), mirror_aside.map(PathBuf::from)]);
                                        match (kept, note) {
                                            (true, None) => {}
                                            (true, Some(note)) => tracing::warn!(%note, "the new build went on with this runner's conversation stores, not all as they were"),
                                            (false, note) => tracing::warn!(error = %note.unwrap_or_default(), "the new build keeps no conversation store; the upgrade goes on"),
                                        }
                                    }
                                    Ok(_) => broke = Some("the new build answered out of turn".to_owned()),
                                    Err(e) => broke = Some(format!("{e:#}")),
                                }
                            }
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
                            let _ = tx.send_blocking(Bg::Moved { job, conn, acks, broke, aside });
                        });
                        if let Err(e) = sent {
                            tracing::error!(error = %e, "couldn't hand over to the new build");
                        }
                    }
                    Bg::Moved { job, conn, acks, broke, aside } => {
                        let Some(mut mv) = moving.take() else { continue };
                        if let Some(g) = mv.stores.as_mut() {
                            g.aside = aside;
                        }
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
                                        (Item::Session { record, .. }, Ack::Refused(e) | Ack::Lost(e)) => {
                                            tracing::warn!(session = %record.id(), why = %e, "the new build didn't take an agent session as it was handed over");
                                            said.push(format!("agent session {} wasn't taken by the new build as handed over: {e}", record.id()));
                                        }
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
                                let (ftx, frx) = crate::memory::queue::unbounded::<Vec<u8>>(&crate::memory::owners::CHANNELS);
                                for d in std::mem::take(&mut mv.held) {
                                    let _ = ftx.try_send(d);
                                }
                                let c = Arc::clone(&conn);
                                // Each with this runner's record of passing it on, numbered on its
                                // stream to the new build, when it checks them (docs/ci.md W27-R08's
                                // relays).
                                let to = if mv.relays { crate::runner::custody::forwarding_to(&mv.instance) } else { None };
                                let fwd = std::thread::Builder::new().name("kiln-forward".into()).spawn(move || {
                                    use base64::Engine as _;
                                    use crate::runner::chaos::RelayFault;
                                    // It is the runner now: it says so to the server, which sends it new work.
                                    if c.send(&Frame::Promoted, &[]).is_err() {
                                        return;
                                    }
                                    let (mut seq, mut before) = (0u64, None::<Vec<u8>>);
                                    while let Ok(d) = frx.recv_blocking() {
                                        // Numbered only as recorded: a frame with no signed
                                        // statement goes on unrecorded.
                                        let passed_on = to.as_ref().and_then(|(to, stream)| crate::runner::custody::forward_record(&d, to, stream, seq + 1));
                                        if passed_on.is_some() {
                                            seq += 1;
                                        }
                                        // A test build's faulty forwarder (`relay_forward`): every second
                                        // frame dropped, swapped for the one before, or a byte of it
                                        // changed, once recorded.
                                        let d = match crate::runner::chaos::get().and_then(|f| f.relay_forward).filter(|_| passed_on.is_some() && RelayFault::strikes(seq)) {
                                            Some(RelayFault::Drop) => continue,
                                            Some(RelayFault::Swap) => before.replace(d.clone()).unwrap_or(d),
                                            Some(RelayFault::Alter) => {
                                                let mut d = d;
                                                if let Some(b) = d.last_mut() {
                                                    *b ^= 1;
                                                }
                                                d
                                            }
                                            None => {
                                                before = Some(d.clone());
                                                d
                                            }
                                        };
                                        let data = base64::engine::general_purpose::STANDARD.encode(&d);
                                        if c.send(&Frame::Forward { data, passed_on }, &[]).is_err() {
                                            break;
                                        }
                                    }
                                });
                                if fwd.is_ok() {
                                    forward = Some(ftx);
                                }
                                for line in said {
                                    own.msg(AgentMsg::UpgradeLog { job: job.clone(), line }).await;
                                }
                                let left = jobs.len();
                                let line = format!("handed to the new build: {sessions} agent session(s) and {terminals} terminal(s), their processes still running; {}", if left == 0 { "this runner retires".to_owned() } else { format!("this runner finishes its {left} job(s), then retires") });
                                own.msg(AgentMsg::UpgradeLog { job: job.clone(), line }).await;
                                own.msg(AgentMsg::UpgradeDone { job: job.clone(), ok: true, error: None }).await;
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
                                let records = mv.items.iter().filter_map(|item| match item { Item::Session { record, .. } => Some((**record).clone()), Item::Terminal { .. } => None }).collect();
                                moving = Some(mv);
                                native_handover.prepare(&mut harness_operations, harness.clone(), NativePreparation::Rollback { job, why, acks, records });
                            }
                        }
                    }
                    Bg::Failed { job, why } => {
                        if native_handover.cancel(Some(&job), why.clone()) { continue; }
                        let records = moving.as_ref().map(|moving| moving.items.iter().filter_map(|item| match item { Item::Session { record, .. } => Some((**record).clone()), Item::Terminal { .. } => None }).collect()).unwrap_or_default();
                        native_handover.prepare(&mut harness_operations, harness.clone(), NativePreparation::Rollback { job, why, acks: Vec::new(), records });
                    }
                    Bg::RollbackAuthorized { job, why, acks } => {
                        if let Some(mut mv) = moving.take() {
                            if let Some(h) = mv.healthy.take() {
                                h.record("result", "failed");
                                h.record("error", why.as_str());
                            }
                            let held_back = std::mem::take(&mut mv.held);
                            let (_, back) = roll_back(mv, &acks, &why, harness.as_ref(), &mut terms, &term_tx, &o.state_dir).await;
                            if let Some(st) = back {
                                let ship = start_feed(&st, harness.as_ref(), &uplink.0);
                                if connected {
                                    ship.link(crate::convo::feed::Link::Up);
                                }
                                (convo, shipper) = (Some(st), Some(ship));
                            }
                            serve_sockets(reach.as_ref(), harness.as_ref(), &name, &sock, &o.state_dir);
                            for d in held_back {
                                let _ = bg_tx.try_send(Bg::Forwarded(d));
                            }
                        }
                        moved.clear();
                        draining = false;
                        tracing::warn!(%job, error = %why, "the new build didn't come up; this runner goes on");
                        own.msg(AgentMsg::UpgradeDone { job, ok: false, error: Some(format!("the new build didn't come up ({why}): it was stopped, and this runner goes on as it was")) }).await;
                    }
                    Bg::Stores { own, mirror: copy, done } => {
                        // Moved into this runner's folder by the runner it takes over from (its
                        // copy of the server's beside it, opened when the server first sends it),
                        // or none came: one of its own, there. One that won't open is set aside,
                        // kept whole, and a new one takes its place: the stores never hold an
                        // upgrade back.
                        let here = [own_dir.join(crate::convo::FILE), own_dir.join(crate::convo::MIRROR_FILE)];
                        // Handed where this one doesn't look for them (a build that lays its folder
                        // out otherwise): moved where it does.
                        for (handed, here) in [own.as_ref(), copy.as_ref()].into_iter().zip(&here) {
                            if let Some(p) = handed.filter(|p| *p != here && !here.exists())
                                && let Err(e) = crate::convo::move_file(p, here)
                            {
                                tracing::warn!(error = %format!("{e:#}"), from = %p.display(), "a conversation store handed over couldn't be moved where this runner keeps it");
                            }
                        }
                        // One already open here (said again): kept as it is.
                        let mut taken = Taken { kept: convo.is_some(), ..Taken::default() };
                        if convo.is_none()
                            && let Some(held) = convo_held.clone()
                        {
                            let (dir, upkeep, with_copy) = (own_dir.clone(), crate::config::Conversations::default().upkeep(), copy.is_some());
                            let (st, aside, copy_aside) = crate::rt::blocking(move || {
                                let (st, aside) = crate::convo::open_or_set_aside(&dir, crate::convo::FILE, upkeep);
                                // The copy of the server's handed over: tried now, so that one that
                                // won't open is set aside and the server's next batch starts a new
                                // copy (opened then, as ever).
                                let copy_aside = if with_copy {
                                    let (c, aside) = crate::convo::open_or_set_aside(&dir, crate::convo::MIRROR_FILE, upkeep);
                                    if let Some(c) = c
                                        && let Err(e) = c.close()
                                    {
                                        tracing::warn!(error = %format!("{e:#}"), "the copy of the server's conversation store handed over couldn't be let go of after a look");
                                    }
                                    aside
                                } else {
                                    None
                                };
                                (st, aside, copy_aside)
                            })
                            .await
                            .unwrap_or_default();
                            let mut notes = Vec::new();
                            if let Some(a) = &aside {
                                notes.push(format!("its conversation store wouldn't open here: set aside, kept whole, at {}{}", a.display(), if st.is_some() { ", and a new one took its place" } else { "" }));
                            }
                            if let Some(a) = &copy_aside {
                                notes.push(format!("its copy of the server's store wouldn't open here: set aside, kept whole, at {}, and a new copy is taken", a.display()));
                            }
                            match st {
                                Some(st) => {
                                    held.give(st.clone());
                                    let ship = start_feed(&st, harness.as_ref(), &uplink.0);
                                    if connected {
                                        ship.link(crate::convo::feed::Link::Up);
                                    }
                                    tracing::info!(store = %st.id(), handed = own.is_some(), set_aside = aside.is_some(), "the conversations go on in this store");
                                    (convo, shipper) = (Some(st), Some(ship));
                                    taken.kept = true;
                                }
                                // What was handed stays here, under its own name, for its next start.
                                None => notes.push("no conversation store could be opened here".to_owned()),
                            }
                            (taken.own_aside, taken.mirror_aside, taken.note) = (aside, copy_aside, (!notes.is_empty()).then(|| notes.join("; ")));
                        }
                        stores_wait = false;
                        let _ = done.send(taken);
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
                        announce(term_list(&terms), harness.as_ref(), &own).await;
                        if !std::mem::replace(&mut sockets_served, true) {
                            serve_sockets(reach.as_ref(), harness.as_ref(), &name, &sock, &o.state_dir);
                        }
                    }
                    Bg::TakeOverEnded => {
                        if !std::mem::replace(&mut sockets_served, true) {
                            serve_sockets(reach.as_ref(), harness.as_ref(), &name, &sock, &o.state_dir);
                        }
                        // It ended before the runner it upgraded said which conversation store this
                        // one goes on with (that one died, or their link broke, before its first
                        // frame): the store in its own folder, with what that one moved there if it
                        // got that far.
                        if stores_wait {
                            let (done, _) = futures::channel::oneshot::channel();
                            let _ = bg_tx.try_send(Bg::Stores { own: None, mirror: None, done });
                        }
                        // Its link to the runner it upgraded ended, and no word of its promotion: that
                        // one is gone (one that takes its sessions back stops this one first), whether
                        // it had handed everything over or ended part-way, with what this one took
                        // then (CP-T3.2). It takes messages again after a while.
                        if let Some(h) = harness.as_ref().filter(|h| h.is_held()) {
                            let (h, whole) = (Arc::clone(h), took_over);
                            crate::rt::spawn(async move {
                                crate::rt::sleep(TAKE_OVER_GRACE).await;
                                if h.is_held() {
                                    tracing::warn!(took_over = whole, "no word of a promotion after the take-over ended: this runner takes messages again");
                                    h.hold(false);
                                }
                            });
                        }
                    }
                    Bg::Promoted => {
                        // Joined ranked oldest (new work stayed with the old runner): the newest now.
                        tracing::info!("promoted: this runner is the machine's newest; new work comes here");
                        // Its sessions are its own for good: it takes their messages, and each it
                        // was handed signed for asks for its next epoch now (docs/ci.md W27-R08).
                        if let Some(h) = &harness {
                            h.hold(false);
                        }
                        legacy_progress.request(harness.as_ref(), &mut harness_operations, &bg_tx, LegacyCause::Promoted);
                        instance_hello.created_at = real_created_at;
                        draining = false;
                        own.msg(AgentMsg::Instance(instance_hello.clone())).await;
                    }
                    Bg::LegacyIdentity { session, epoch } => {
                        if server_identity && connected && crate::runner::custody::legacy_identity_pending(&session, epoch) {
                            // The current waiter authorizes starting this request. A request already
                            // in flight can finish later; completion prevents queued events starting one.
                            ask_agent_identity(&own, &mut identity_asked, &session, epoch).await;
                        }
                    }
                    // Turned into Ev::Forwarded as they're received.
                    Bg::Forwarded(_) | Bg::Again(..) => {}
                }
            }
            Ev::Redial => match redials.on_deadline(Instant::now()) {
                redial::Step::Dial => match paths.target(&http, Instant::now()).await {
                    Ok(addr) => {
                        redials.dialing(Instant::now());
                        net.command(Out::Connect { addr }).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "cannot resolve server");
                        redials.dialing(Instant::now());
                        redials.failed(Instant::now(), rand::random());
                    }
                },
                // Its end comes back from the transport as the dial's failure.
                redial::Step::GiveUp => {
                    net.command(Out::Disconnect { peer: SERVER_PEER }).await;
                }
                redial::Step::Wait => {}
            },
            Ev::Watchdog => {
                legs_gone_seen = crate::runner::kept::gones();
                if !o.ephemeral && standalone && crate::os::IN_PLACE_UPGRADE && started.elapsed() >= WATCHDOG_SETTLE {
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
                    let ready = busy == 0 || harness_wait.is_some_and(|t| t.elapsed() >= HARNESS_GRACE);
                    if upgrade_pending && !relaunch_wanted && jobs.is_empty() && ready && native_handover.idle() && let Some(exe) = exe.as_deref() {
                        if let Err(e) = binary_runs(exe).await {
                            // Never switch to a binary that cannot start: keep this one until
                            // the binary on disk changes again.
                            tracing::error!(error = %format!("{e:#}"), exe = %exe.display(), "the new kiln binary does not run; staying on this one");
                            upgrade_pending = false;
                            exe_mtime = now;
                        } else if !successor {
                            native_handover.prepare(&mut harness_operations, harness.clone(), NativePreparation::InPlace { exe: exe.to_path_buf(), mtime: now });
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
                    if relaunch_asked.is_some_and(|t| t.elapsed() >= RELAUNCH_WAIT) {
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
                    own.0.send(Up::Term(AgentMsg::TermExit { session, code: None, error: Some("the leg ended".into()) })).await.ok();
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
                    if let Some(ob) = &observe {
                        ob.up("", false);
                    }
                    if let Some(r) = &reach {
                        r.feed(crate::reach::node::In::Server(false)).await;
                        crate::reach::told::server_down();
                    }
                    redials.switch(Instant::now());
                }
            }
            Ev::Net(None) => anyhow::bail!("transport stopped"),
            Ev::Net(Some(NetEvent::Connected { binding, .. })) => {
                #[cfg(unix)]
                { feature_negotiation = FeatureNegotiation::default(); }
                crate::harness::handing::mail_supported(false);
                tracing::info!(server = %quic_addr, via = %paths.via(), "connected to server");
                chaos_drop = chaos.and_then(|c| c.drop_every).map(|d| Instant::now() + d);
                // How long saying what it is takes (its build's probes, its facts), told below: the
                // server's frames wait meanwhile.
                let said_from = Instant::now();
                paths.connected(binding, Instant::now());
                if let Err(e) = redials.connected(Instant::now()) {
                    tracing::warn!(error = %format!("{e:#}"), "what this runner learned of its dials isn't kept");
                }
                // Its admission, first (docs/resilience.md R5): a server over its limit tells it its turn, and lets go of
                // what follows until it closes; one that admits it takes the rest. An older server skips it, as a test
                // build's `old` runner never says it.
                if !crate::runner::chaos::get().is_some_and(|c| c.old)
                    && let Ok(b) = proto::encode(&AgentMsg::Admit)
                {
                    net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                }
                // The machine it is (docs/machines.md, "One record a machine"), which process of it
                // this is, then Hello (an older server skips the first two).
                if !o.ephemeral
                    && let Some(id) = &machine
                    && let Ok(b) = proto::encode(&AgentMsg::Machine { id: id.clone() })
                {
                    net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                }
                if !o.ephemeral
                    && let Ok(b) = proto::encode(&AgentMsg::Instance(instance_hello.clone()))
                {
                    net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                }
                // Jobs whose results the server hasn't said it has are its own still: they come
                // again below. A server that never says (one older than receipts) has them for a
                // while only.
                unreceived.retain(|_, (at, _)| at.elapsed() < UNRECEIVED_FOR);
                signed_unreceived.retain(|job, _| unreceived.contains_key(job));
                let mut running: Vec<String> = jobs.keys().cloned().collect();
                running.extend(unreceived.keys().filter(|j| !jobs.contains_key(*j)).cloned());
                let msg = AgentMsg::Hello(hello(running));
                // What's granted on this connection is granted after the server has this hello
                // (W05-R37).
                leases.connected(crate::runner::lease::now_ms());
                if let Ok(b) = proto::encode(&msg) {
                    net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                }
                // Which build this is and that it takes upgrade commands (an older server skips it);
                // an outpost says it's one instead. Its binary is read from this runner's start, beside the
                // connection (`updates::read_own`), its engine's probes off the event loop.
                let building = Instant::now();
                let mut build = if o.outpost { crate::updates::outpost_build().await } else { crate::updates::runner_build().await };
                let build_took = building.elapsed();
                if let AgentMsg::Build { caps, .. } = &mut build {
                    // It sends its conversations' store: the server keeps them from that (a new build
                    // taking over, the one it goes on with, once it knows which).
                    if shipper.is_some() || stores_wait {
                        *caps |= proto::CAP_CONVO;
                    }
                    // A runner that keeps no replica does not say it replicates.
                    if observe.is_none() {
                        *caps &= !proto::CAP_OBSERVE;
                    }
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
                    // Pause and Resume freeze a job in place (W26-R22): its processes stopped on Unix,
                    // suspended on Windows.
                    let freeze = if cfg!(any(unix, windows)) { proto::CAP_FREEZE } else { 0 };
                    let custody = if crate::runner::custody::signs() { proto::CAP_CUSTODY | proto::CAP_RELAYS } else { 0 };
                    // Running agents, it answers what their branches' commits are and compares two points
                    // of their repositories (the Changes tab's Commits and Compare).
                    let commits = if harness.is_some() { proto::CAP_WORKSPACE_COMMITS | proto::CAP_WORKSPACE_COMPARE } else { 0 };
                    // An agent session's commands on the session's own streams, which the loop on its shard takes (runner-fan).
                    let lanes = if harness.is_some() { proto::CAP_SESSION_STREAMS } else { 0 };
                    let archive = if harness.is_some() { proto::CAP_HARNESS_ARCHIVE } else { 0 };
                    if let Ok(b) = proto::encode(&AgentMsg::Caps { caps: built | proto::CAP_WIRE | proto::CAP_HANDOVER | proto::CAP_INBOX | work | limit | proto::CAP_IDENTITY | proto::CAP_LEG_SIGNS | headroom_cap | proto::CAP_UNREAD | freeze | custody | commits | lanes | proto::CAP_ADMISSION | proto::CAP_DEPENDENCY_LOCKS | archive | proto::CAP_LEASES | proto::CAP_FEATURES }) {
                        net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                    }
                }
                // W29-R24: a dedicated pod also states its enforcement capability; old pods
                // must not acquire it merely because their server can create an assignment.
                if o.ephemeral && !crate::runner::chaos::get().is_some_and(|c| c.old) {
                    let built = if let AgentMsg::Build { caps, .. } = &build { u64::from(*caps) } else { 0 };
                    if let Ok(bytes) = proto::encode(&AgentMsg::Caps { caps: built | proto::CAP_DEPENDENCY_LOCKS }) {
                        net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, bytes).await;
                    }
                }
                // The kiln it runs, so the server knows its commit (`crate::fleet`; outposts aren't
                // part of the fleet).
                let started = Instant::now();
                let facts = if !o.ephemeral && !o.outpost { Some(crate::updates::runner_facts().await) } else { None };
                let facts_took = started.elapsed();
                if let Some(Ok(b)) = facts.as_ref().map(proto::encode) {
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
                if let Some(ob) = &observe {
                    ob.up("", true);
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
                // Its frames waited all that while (the server's Welcome, observe's join): a slow one said.
                let took = said_from.elapsed();
                let (took_ms, build_ms, facts_ms) = (took.as_millis() as u64, build_took.as_millis() as u64, facts_took.as_millis() as u64);
                if took > Duration::from_secs(2) {
                    tracing::warn!(took_ms, build_ms, facts_ms, "saying what this runner is took long: the server's frames waited");
                } else {
                    tracing::debug!(took_ms, build_ms, facts_ms, "said what this runner is to the server");
                }
            }
            Ev::Net(Some(NetEvent::Closed { reason, .. } | NetEvent::Unreachable { reason, .. })) => {
                let now = Instant::now();
                redials.failed(now, rand::random());
                if connected {
                    tracing::warn!(%reason, "disconnected from server");
                    paths.disconnected();
                    // No word comes for agent sessions on their way to a newer process: they go on here.
                    crate::harness::handing::runner_link_lost();
                } else {
                    tracing::debug!(%reason, via = %paths.via(), next = ?redials.deadline().map(|at| at.saturating_duration_since(now)), "dial failed");
                    // The path changed (to the tunnel, or back): dial that way now.
                    if paths.dial_failed(now) {
                        redials.switch(now);
                    }
                }
                connected = false;
                welcomed = false;
                mail_replay = MailReplay::default();
                crate::harness::handing::mail_supported(false);
                #[cfg(unix)]
                { feature_negotiation = FeatureNegotiation::default(); }
                leases.away();
                // The channel's items in flight go again once the server says what it has, and
                // the server's from what this side says it has: the session loops' too (runner-fan).
                updates.down();
                wire_in.down();
                shared.link.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                have_soon = None;
                server_wire = false;
                server_drops = false;
                server_unread = false;
                shared.server_unread.store(false, std::sync::atomic::Ordering::Release);
                told = crate::wire::unread::Told::default();
                drops_said = None;
                server_work = false;
                work_said = None;
                server_identity = false;
                server_admission = false;
                server_signs = false;
                server_limit = false;
                limit_said = None;
                server_leases = false;
                server_features.clear();
                sent_changes.state_pages = false;
                if let Some(s) = &sync {
                    s.connected(false);
                }
                if let Some(ob) = &observe {
                    ob.up("", false);
                }
                if let Some(f) = &shipper {
                    f.link(crate::convo::feed::Link::Down);
                }
                // A copy being sent back stops: the server asks again if it still needs it.
                restoring = None;
                if let Some(r) = &reach {
                    r.feed(crate::reach::node::In::Server(false)).await;
                    crate::reach::told::server_down();
                }
                if let Some(until) = chaos_down.take() {
                    redials.hold(until);
                }
            }
            Ev::Net(Some(NetEvent::Frame { data, .. })) | Ev::Forwarded(data) => {
                let msg: ServerMsg = match proto::read_server(&data) {
                    Ok(m) => m,
                    // The server hears of it, when it takes word of it (docs/wire.md, "What can't
                    // be read"), and notes it against this runner.
                    Err(e) => {
                        tracing::warn!(what = %e.what(), why = %e.why("ServerMsg"), "a frame from the server couldn't be read");
                        if server_unread && let Some(folded) = told.tell(Instant::now(), crate::wire::unread::Wire::Server, e.code(), &e.what(), &e.why("ServerMsg")) {
                            own.msg(AgentMsg::Unread { wire: crate::wire::unread::Wire::Server, what: e.what(), code: e.code(), why: e.why("ServerMsg"), folded, session: None }).await;
                        }
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
                                crate::wire::commands::Stand::Newer => wire_in.forget_where(|s| s != stream && crate::wire::commands::superseded(&stream, s)),
                                crate::wire::commands::Stand::Current => {}
                            }
                            let answer = match wire_in.on_item(&stream, seq, low, Instant::now()) {
                                Arrival::Take(_) => match proto::read_server(&body) {
                                    Ok(ServerMsg::Wire(_)) => {
                                        let why = "a channel frame inside a command";
                                        if server_unread && let Some(folded) = told.tell(Instant::now(), crate::wire::unread::Wire::ServerItem, crate::wire::unread::Code::Malformed, "Wire", why) {
                                            own.msg(AgentMsg::Unread { wire: crate::wire::unread::Wire::ServerItem, what: "Wire".to_owned(), code: crate::wire::unread::Code::Malformed, why: why.to_owned(), folded, session: None }).await;
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
                                            own.msg(AgentMsg::Unread { wire: crate::wire::unread::Wire::ServerItem, what: e.what(), code: e.code(), why: e.why("ServerMsg"), folded, session: None }).await;
                                        }
                                        wire_in.refused(&stream, seq, e.code().refusal(), e.why("ServerMsg"))
                                    }
                                },
                                Arrival::Answered(Some(a)) | Arrival::Gap(a) => a,
                                Arrival::Answered(None) => continue 'events,
                            };
                            // An answer, timed to the net thread as one made at the next turn's top is (runner-fan).
                            if matches!(answer, crate::wire::channel::Frame::Ack(_)) {
                                answers_at.push_back((Instant::now(), holding.map_or(crate::route_latency::InKind::Command, |(_, k)| k)));
                            }
                            own.0.send(Up::Term(AgentMsg::Wire(answer))).await.ok();
                            continue 'events;
                        }
                        m => (m, data),
                    }
                };
                // About something handed to the new build (blue/green), or new while this runner
                // hands everything over or retires: it goes there.
                #[cfg(unix)]
                if let Some(what) = goes_on(&msg, &moved, retiring || moving.as_ref().is_some_and(|m| m.handing)) {
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
                // What's signed is checked over the bytes the server sent: a hand-over's word, another
                // agent's words kiln relayed, and an assignment a leg signs its receipt of (docs/ci.md
                // W27-R05, R08).
                let raw = match &msg {
                    ServerMsg::HarnessCmd { json, .. } | ServerMsg::HarnessSend { json, .. } | ServerMsg::AssignV2 { json } => Some(json.clone()),
                    _ => None,
                };
                let msg = match crate::wire::payload_of(msg) {
                    Ok(read) => read,
                    Err(e) => {
                        tracing::warn!(what = %e.what, code = e.code.name(), why = %e.why, "what a message from the server carries couldn't be read");
                        refuse(&mut answer_after, e.code.refusal(), e.why.clone());
                        let tell = if server_unread { told.tell(Instant::now(), crate::wire::unread::Wire::ServerPayload, e.code, &e.what, &e.why) } else { None };
                        for up in unread_payload(&e, tell).into_iter().chain(unread_send(harness.as_deref(), &e)) {
                            own.0.send(up).await.ok();
                        }
                        continue;
                    }
                };
                let msg = match msg {
                    Carried::Nothing(m) => m,
                    Carried::Read(FromServer::HarnessOpen { session, spec }) => {
                        use crate::wire::channel::Refusal;
                        // As it was asked, for the updates that show it refused.
                        let asked = (*spec).clone();
                        // How it fared, as its answer says it (a command on the channel).
                        let opened: Result<(), (Refusal, anyhow::Error)> = match (&harness, *spec) {
                            (None, _) => Err((Refusal::Unsupported, anyhow::anyhow!("agent sessions are not enabled on this runner"))),
                            // Asked again for a session it has (the answer to the first ask lost on its
                            // way back, the start asked again under its id): it's there, and nothing is
                            // made or started again, its workspace included.
                            (Some(h), _) if h.get(&session).is_some() => Ok(()),
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
                                if crate::workspace::needs_preparing(&spec) {
                                    // Its workspace first (a volume made and mounted), off this loop: then it starts there.
                                    let (h, up, session, asked) = (Arc::clone(h), uplink.0.clone(), session.clone(), asked.clone());
                                    crate::rt::spawn(async move {
                                        let opened = match crate::workspace::prepare(&mut spec, &session).await {
                                            Ok(()) => {
                                                let made = spec.workspace.as_ref().and_then(|w| w.made.clone());
                                                let opened = h.open(Some(session.clone()), spec).map(|_| ());
                                                // What was made for it goes with it: asked again under its id, it's made anew.
                                                if opened.is_err()
                                                    && let Some(m) = made
                                                    && let Err(gone) = crate::workspace::slates::remove(&m).await
                                                {
                                                    tracing::warn!(volume = %m.volume, error = %format!("{gone:#}"), "the workspace of an agent that couldn't start stayed");
                                                }
                                                opened
                                            }
                                            Err(e) => Err(e.context("its workspace")),
                                        };
                                        if let Err(e) = opened {
                                            for u in crate::harness::host::refusal(&session, &asked, &format!("{e:#}")) {
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
                            for u in crate::harness::host::refusal(&session, &asked, &format!("{e:#}")) {
                                if let Ok(json) = serde_json::to_string(&u) {
                                    own.0.send(Up::Term(AgentMsg::Harness { session: session.clone(), json })).await.ok();
                                }
                            }
                        }
                        continue;
                    }
                    // A command for one agent session (`loops::command`): here from a server that sends none on a session's own
                    // streams, or one a session loop handed this loop while blue/green decides (runner-fan).
                    Carried::Read(FromServer::HarnessCmd { session, command }) => {
                        #[cfg(unix)]
                        let gone = gone_here(&session, &moved);
                        #[cfg(not(unix))]
                        let gone = true;
                        if let Some((code, why)) = loops::command(&shared, session, *command, raw.as_deref().unwrap_or_default(), gone).await {
                            refuse(&mut answer_after, code, why);
                        }
                        continue;
                    }
                    // A message for an epoch (CP-T3.2): through its session's inbox, which answers (`loops::send`). Here only
                    // as one a session loop handed this loop, or forwarded (runner-fan).
                    Carried::Read(FromServer::HarnessSend { session, epoch, command }) => {
                        loops::send(&shared, session, epoch, *command, raw.as_deref()).await;
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
                        // Its certificate's file written on the blocking pool, waited for here so what follows finds it
                        // kept (goal 9, plan 01a1a87b2cec step 3: written and synced on this loop's shard).
                        let taken_over = match installed_identity(given, o.state_dir.clone(), label).await {
                            Ok(installed) => installed,
                            Err(e) => { tracing::warn!(error = %format!("{e:#}"), "an identity kiln sent couldn't be kept"); None }
                        };
                        // The runner role handed to this process, signed for once its own
                        // identity is (W27-R08), what relays passed on to it, and the work its sessions
                        // took up before they held theirs: an identity given is what each waits on
                        // (goal 9: they were looked at every heartbeat). What has its identity now asks
                        // no more.
                        crate::runner::custody::role_due();
                        crate::runner::custody::relay_due();
                        crate::runner::custody::take_up_due();
                        prune_identity_asks(&mut identity_asked, &session_epochs);
                        // A session's identity at an epoch it was taken over at: its receipt went
                        // with the ask, and kiln recorded it. Taken over blue/green, it goes on at
                        // that epoch now.
                        if let Some((session, epoch)) = taken_over
                        {
                            // Durable legacy waiters survive this notification. The session owner
                            // commits Head/leases/Controller/Saved before retiring their witness.
                                match crate::runner::custody::certified(&session, epoch) {
                                    crate::runner::custody::Certification::BlueGreen => {
                                        if let Some(session) = harness.as_ref().and_then(|host| host.get(&session)) { session.go_on_at(epoch); }
                                    }
                                    crate::runner::custody::Certification::DurablePromotion | crate::runner::custody::Certification::Receipt | crate::runner::custody::Certification::Untracked => {}
                                }
                        }
                        #[cfg(unix)]
                        legacy_progress.request(harness.as_ref(), &mut harness_operations, &bg_tx, LegacyCause::Observe);
                        if server_custody {
                            for json in crate::runner::custody::drain() {
                                own.0.send(Up::Term(AgentMsg::Custody { json })).await.ok();
                            }
                        }
                        continue;
                    }
                    // Work kiln handed one of its agent sessions: its receipt is signed as the
                    // session takes it up, at its next turn (docs/ci.md W27-R08); a leg's inputs; and
                    // kiln's records of the reach links' frames its relay passed on here, answered
                    // with this process's receipts of them as they came.
                    Carried::Read(FromServer::Custody(handed)) => {
                        crate::runner::custody::handed(&handed);
                        // A session's first work, handed after its first turn began, is signed for
                        // at once.
                        if server_custody {
                            for json in crate::runner::custody::drain() {
                                own.0.send(Up::Term(AgentMsg::Custody { json })).await.ok();
                            }
                        }
                        continue;
                    }
                    Carried::Read(FromServer::Cluster { msg, raft }) => {
                        use crate::store::learner::Msg;
                        match *msg {
                            Msg::Join { member, token } if !o.ephemeral && !o.outpost => {
                                // A replica being started goes on with the newest word to join.
                                *cluster_join.lock() = Some((member, token));
                                if cluster.lock().is_some() {
                                    continue;
                                }
                                replica.start();
                            }
                            Msg::Leave => {
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
                            m @ (Msg::Raft { .. } | Msg::At { .. } | Msg::Checkpoint { .. }) => {
                                let learner = cluster.lock().clone();
                                if let Some(l) = learner {
                                    l.heard(&m, raft);
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }
                    Carried::Read(FromServer::Convo { msg, data }) => {
                        let m = *msg;
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
                                ship.sign_with(Arc::new(crate::runner::custody::Feed));
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
                                own.0.send(Up::Convo(AgentMsg::Convo { json: none, data: Vec::new() })).await.ok();
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
                            intake.heard(SERVER_PEER, "server", m, data);
                        }
                        continue;
                    }
                    // Pages streamed for whoever views one of its sessions (`crate::browser::stream`).
                    Carried::Read(FromServer::StreamOpen { stream, session, spec }) => {
                        loops::stream_open(&shared, stream, session, *spec);
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
                    // What a person opens live from one of its sessions (`crate::browser::apps`), and a question about the
                    // folder one works in (`loops::preview`, `loops::workspace`).
                    Carried::Read(FromServer::Preview { req, session, ask }) => {
                        loops::preview(&shared, req, session, ask);
                        continue;
                    }
                    Carried::Read(FromServer::Workspace { req, session, ask }) => {
                        loops::workspace(&shared, req, session, ask);
                        continue;
                    }
                    Carried::Read(FromServer::FrontDoor(push)) => {
                        // Settings (and certificates) are never logged: an upload carries a key.
                        if let Some(fd) = &front_door {
                            fd.push(*push);
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
                        let dir = o.state_dir.clone();
                        if let Err(e) = crate::rt::blocking(move || crate::harness::limit::keep(&dir, asked)).await.and_then(|r| r.map_err(anyhow::Error::from)) {
                            tracing::warn!(error = %format!("{e:#}"), "what was set for this machine's agent limit couldn't be kept: started again, it holds its config's until the server says it again");
                        }
                        limit_asked = asked;
                        if let Some(h) = &harness {
                            hold_agent_limit(h, crate::harness::limit::held(limit_asked, limit_config), &mut limit_said, server_limit && welcomed, &own).await;
                        }
                        continue;
                    }
                    // A leg of the CI engine (docs/ci.md W06): this runner announces `CAP_CI2`.
                    Carried::Read(FromServer::AssignV2(a)) => {
                        let mut a = *a;
                        let id = a.job_id.clone();
                        // From an epoch older than the newest this runner has seen (W05-R37): not taken.
                        if let Some(f) = a.fence.filter(|f| leases.stale(f.epoch)) {
                            refuse(&mut answer_after, crate::wire::channel::Refusal::StaleEpoch, format!("an assignment from epoch {}, older than this runner has seen", f.epoch));
                            continue;
                        }
                        // A pool's leg runs in its executor, beside this runner's own slots.
                        let pooled = a.executor.slot.is_some();
                        let holds = kiln_ci::sched::state::Incoming::of(a.exclusive, &a.holds.unwrap_or_default());
                        let room = admits(o.slots, probing.sized(size), jobs.values().map(|j| (j.pooled, j.holds)), pooled, holds);
                        // A test build's `refuse=<job>`: refused as one with no room would refuse it.
                        let room = room.and_then(|()| if crate::runner::chaos::refuses(&a.leg.job_id) { Err("no free slot") } else { Ok(()) });
                        if o.outpost || draining || upgrade_pending || room.is_err() || jobs.contains_key(&id) {
                            let reason = if o.outpost { OUTPOST_TAKES_NO_JOBS } else if draining { REFUSED_DRAINING } else if upgrade_pending { REFUSED_UPGRADING } else { room.err().unwrap_or("no free slot") };
                            // To a server that takes leases, naming the grant refused (W05-R34).
                            let refusal = match a.fence {
                                Some(fence) if server_leases => AgentMsg::Refused { grant: proto::Grant::of(&id, a.attempt, fence), reason: reason.into() },
                                _ => AgentMsg::Reject { job: id, reason: reason.into() },
                            };
                            own.msg(refusal).await;
                            continue;
                        }
                        // Admitted against the same ledger as the runs placed here (AUDIT M4, G1):
                        // refused, saying why, when they leave it no room. A pool's leg runs in its
                        // executor, not on this machine's room.
                        if !pooled
                            && let Err(why) = leg_room(reach.as_ref(), &id, crate::sense::run::leg_key(&a.repo, a.job_workflow.as_ref().map_or("", |w| w.workflow_file_path.as_str()), &a.leg.job_id, &a.leg.matrix.to_string()), a.exclusive, a.request.or(a.holds).unwrap_or_default(), &a.leg.requires).await
                        {
                            let refusal = match a.fence {
                                Some(fence) if server_leases => AgentMsg::Refused { grant: proto::Grant::of(&id, a.attempt, fence), reason: why },
                                _ => AgentMsg::Reject { job: id, reason: why },
                            };
                            own.msg(refusal).await;
                            continue;
                        }
                        // An ephemeral runner (a `k8s` pool's pod, docs/ci.md W13-R07) takes one leg,
                        // then leaves.
                        if o.ephemeral {
                            served_dedicated = true;
                        }
                        // The leg's own identity: a key made here, outside anything its steps can
                        // reach (docs/ci.md W27-R01).
                        if server_identity {
                            match crate::runner::identity::ask("leg", Some(&id), None) {
                                Ok(json) => own.msg(AgentMsg::Identity { json }).await,
                                Err(e) => tracing::warn!(job = %id, error = %format!("{e:#}"), "no identity asked for"),
                            }
                        }
                        // A new attempt: its events count from one, and it runs under its own grant
                        // (W05-R34), none from an older server.
                        journal.remove(&id);
                        unreceived.remove(&id);
                        signed_unreceived.remove(&id);
                        match a.fence {
                            Some(fence) => grants.insert(id.clone(), proto::Grant::of(&id, a.attempt, fence)),
                            None => grants.remove(&id),
                        };
                        // Its lease, by this machine's clock (W05-R37).
                        match (a.fence, a.lease_ms) {
                            (Some(fence), Some(ms)) if server_leases => {
                                leases.granted(&id, fence, ms, a.on_lease_loss == kiln_ci::sched::LeaseLoss::Stop);
                            }
                            _ => leases.ended(&id),
                        }
                        crate::runner::signed::forget(&id);
                        // What it was given, as it came: its receipt's (docs/ci.md W27-R05).
                        if server_identity && let Some(json) = raw.as_deref() {
                            crate::runner::signed::assigned(&id, json);
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
                        let (cancel, kill) = (Shutdown::new(), Shutdown::new());
                        hold_cpus(&id, holds);
                        jobs.insert(id.clone(), Running { cancel: cancel.clone(), kill: kill.clone(), pooled, holds });
                        // W29-R24: before adopting or starting any executor, enforce the snapshot.
                        if let Some(locks) = a.dependency_locks.clone() && let Err(error) = locks.assignment(&mut a) {
                            let _ = done_tx.send(JobResult { job: id, status: Status::Failed, exit_code: None, error: Some(error), peak_rss: None, cpu_seconds: None }).await;
                            continue;
                        }
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
                        let leg = crate::runner::runtime::Leg { a, work_dir: o.work_dir.clone(), runner_name: name.clone(), up: uplink.clone(), cancel, kill, http: http.clone(), warm: std::sync::Arc::new(parking_lot::Mutex::new(warm)), cache_max: o.declared.cache_max.as_deref().and_then(crate::runner::cache::parse_size) };
                        let done = done_tx.clone();
                        // On the `firecracker` executor, in a microVM of its own (W13-R05).
                        #[cfg(target_os = "linux")]
                        let microvm = (leg.a.executor.kind == "firecracker").then(|| microvm_ca.clone());
                        let first = server_signs.then_some(id);
                        crate::rt::spawn(async move {
                            // Its first step waits for kiln to answer its identity, 2 s at most, so
                            // all it signs is signed as the leg (docs/ci.md W27-R05).
                            if let Some(job) = first {
                                crate::runner::signed::identity_first(&job).await;
                            }
                            #[cfg(target_os = "linux")]
                            let result = match microvm {
                                Some(ca) => crate::runner::firecracker::run(leg, Some(ca)).await,
                                None => leg.run().await,
                            };
                            #[cfg(not(target_os = "linux"))]
                            let result = leg.run().await;
                            let _ = done.send(result).await;
                        });
                        continue;
                    }
                    // Start or stop a pool's executor (docs/ci.md W13-R09).
                    Carried::Read(FromServer::Executor(ask)) => {
                        let (execs, work, up) = (pool_execs.clone(), o.work_dir.clone(), uplink.clone());
                        crate::rt::spawn(async move {
                            if let Some(said) = crate::runner::pool::handle(&execs, &work, ask).await
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
                        #[cfg(unix)]
                        feature_negotiation.caps(caps);
                        // An identity of this process's own, for a key it made and keeps, certified
                        // on each connection (docs/ci.md W27-R01); an ephemeral runner's too (a `k8s`
                        // pool's pod, a v1 job's), enrolled by kiln, so its leg signs as the leg.
                        server_identity = caps & proto::CAP_IDENTITY != 0;
                        server_signs = server_identity && caps & proto::CAP_LEG_SIGNS != 0;
                        crate::runner::identity::set_enabled(server_identity);
                        // Its sessions' git pushes to kiln as their agents (W27-R03), where the
                        // server gives them a credential of their own: at the URL this runner
                        // reaches it at, and where it says it's reached (`/api/me`).
                        let git = caps & proto::CAP_AGENT_GIT != 0;
                        crate::runner::identity::set_kiln_git(if git { vec![o.server_url.clone()] } else { Vec::new() });
                        if git {
                            let (http, base) = (Arc::clone(&http), o.server_url.trim_end_matches('/').to_owned());
                            crate::rt::spawn(async move {
                                let me = async {
                                    let v: serde_json::Value = http.get(&format!("{base}/api/me")).timeout(std::time::Duration::from_secs(10)).send().await?.json(1 << 20).await?;
                                    anyhow::Ok(v)
                                };
                                match me.await {
                                    Ok(v) => {
                                        let public = v.get("public_url").and_then(serde_json::Value::as_str).map(str::to_owned);
                                        let doors = v.get("origins").and_then(serde_json::Value::as_array).into_iter().flatten().filter_map(|o| o.as_str().map(str::to_owned));
                                        crate::runner::identity::also_kiln_git(public.into_iter().chain(doors).collect());
                                    }
                                    Err(e) => tracing::debug!(error = %format!("{e:#}"), "where the server is reached wasn't said: agents' git takes their credential at this runner's URL alone"),
                                }
                            });
                        }
                        // Custody records sent before go again: the server takes each once.
                        server_custody = caps & proto::CAP_CUSTODY != 0 && !o.ephemeral;
                        server_admission = caps & proto::CAP_ADMISSION != 0;
                        shared.server_custody.store(server_custody, std::sync::atomic::Ordering::Release);
                        crate::runner::custody::server_checks(server_custody);
                        crate::runner::custody::server_relays(caps & proto::CAP_RELAYS != 0 && !o.ephemeral);
                        if server_custody {
                            for json in crate::runner::custody::sent() {
                                own.0.send(Up::Term(AgentMsg::Custody { json })).await.ok();
                            }
                        }
                        if server_identity {
                            match crate::runner::identity::ask("runner", None, None) {
                                Ok(json) => {
                                    own.0.send(Up::Term(AgentMsg::Identity { json })).await.ok();
                                }
                                Err(e) => tracing::warn!(error = %format!("{e:#}"), "no identity asked for"),
                            }
                            // And each agent session it holds, at its epoch.
                            for info in harness.as_ref().map(|h| h.list()).unwrap_or_default() {
                                session_epochs.insert(info.id.clone(), info.epoch);
                                ask_agent_identity(&own, &mut identity_asked, &info.id, info.epoch).await;
                            }
                        }
                        server_wire = caps & proto::CAP_WIRE != 0;
                        server_drops = caps & proto::CAP_NET_DROPS != 0;
                        server_work = caps & proto::CAP_WORK != 0;
                        if server_work {
                            taken.push_back(Ev::Work);
                        }
                        server_limit = caps & proto::CAP_AGENT_LIMIT != 0;
                        // What its agent sessions are held to, said now the server says it takes it (it
                        // was said again with every sample if the server lacked it); and what it holds
                        // warm, to a server that takes it.
                        if let Some(h) = &harness {
                            hold_agent_limit(h, crate::harness::limit::held(limit_asked, limit_config), &mut limit_said, server_limit && welcomed, &own).await;
                        }
                        sent_changes.server(caps & proto::CAP_CHANGES_PATCH != 0);
                        server_headroom = caps & proto::CAP_HEADROOM != 0;
                        // How headroom does, now that this server takes it.
                        headroom_sent = None;
                        if server_headroom
                            && let Some(json) = headroom.as_ref().and_then(crate::headroom::Handle::latest)
                        {
                            own.0.send(Up::Term(AgentMsg::HeadroomStatus { json: json.clone() })).await.ok();
                            headroom_sent = Some(json);
                        }
                        server_unread = caps & proto::CAP_UNREAD != 0;
                        // For the session loops too: what they take after this `Caps`, a barrier, sees it.
                        shared.server_unread.store(server_unread, std::sync::atomic::Ordering::Release);
                        if let Some(r) = &reach {
                            r.feed(crate::reach::node::In::Unread(server_unread)).await;
                        }
                        server_leases = caps & proto::CAP_LEASES != 0;
                        // It speaks named features: it's told this runner's (docs/wire.md, "Named
                        // features"), and says its own.
                        if caps & proto::CAP_FEATURES != 0 {
                            own.0.send(Up::Term(AgentMsg::Features { names: proto::feature::all() })).await.ok();
                        }
                        if server_wire {
                            own.0.send(Up::Term(AgentMsg::Wire(loops::have_of(wire_in.have(), &shared)))).await.ok();
                            have_soon = Some((Instant::now() + HAVE_SOON, HAVES_SOON));
                        }
                        #[cfg(unix)]
                        legacy_progress.request(harness.as_ref(), &mut harness_operations, &bg_tx, LegacyCause::Observe);
                        #[cfg(unix)]
                        if let Some(path) = feature_negotiation.take_over(welcomed, &mut take_over) {
                            start_take_over(path, harness.clone(), bg_tx.clone(), Me { machine: name.clone(), instance: instance_id.clone(), root: root.clone() }, stores_wait);
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
                        if restart_updates(&mut updates, harness.as_ref(), &terms, &server_kept, &mut sent_changes)
                            && proto::feature::speaks(&server_features, proto::feature::MAIL_FLOW)
                        {
                            mail_replay.request(harness.as_ref());
                        }
                        flush_updates(&mut updates, &own).await;
                    }
                    ServerMsg::Welcome { server_version, sample_seconds: _ } => {
                        tracing::info!(%server_version, "server welcomed agent");
                        // What waited for the connection, in order: what was held while it was
                        // down, the jobs' events again, then their results not yet received.
                        welcomed = true;
                        // Its grants first, renewed before anything else (W05-R34).
                        if server_leases {
                            renew_seq += 1;
                            let held = held_grants(&grants, &jobs, &unreceived);
                            leases.renew_sent(renew_seq, crate::runner::lease::now_ms(), named(&held));
                            if let Ok(b) = proto::encode(&AgentMsg::Renew { seq: renew_seq, held }) {
                                net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                            }
                        }
                        while let Some((lane, kind, data)) = buffer.pop_front() {
                            buffered = buffered.saturating_sub(data.len());
                            let (lane, kind, data) = wire_form(lane, kind, data, &grants, server_leases);
                            net.send(SERVER_PEER, lane, kind, data).await;
                        }
                        if receipts {
                            for (_, _, events) in journal.values() {
                                for data in events {
                                    let (lane, kind, data) = wire_form(LANE_CONTROL, Kind::Control, data.clone(), &grants, server_leases);
                                    net.send(SERVER_PEER, lane, kind, data).await;
                                }
                            }
                        }
                        for data in signed_unreceived.values() {
                            net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, data.clone()).await;
                        }
                        for (_, data) in unreceived.values() {
                            let (lane, kind, data) = wire_form(LANE_CONTROL, Kind::Control, data.clone(), &grants, server_leases);
                            net.send(SERVER_PEER, lane, kind, data).await;
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
                        own.0.send(Up::Term(AgentMsg::TermList { sessions })).await.ok();
                        if let Some(ai) = &ai {
                            ai.reset();
                        }
                        // Its battery is said again too (a state: the newest, sent on every connect), and its
                        // host's sample taken: a connection is one of what moves them (goal 9). What it holds
                        // warm, its harnesses and its config's say are told as this connection begins too.
                        power_said = None;
                        taken.push_back(Ev::Power);
                        taken.push_back(Ev::Sample);
                        taken.push_back(Ev::Work);
                        taken.push_back(Ev::Config);
                        taken.push_back(Ev::Dropped);
                        // And what its agent sessions are held to, to a server that takes it.
                        limit_said = None;
                        if let Some(h) = &harness {
                            hold_agent_limit(h, crate::harness::limit::held(limit_asked, limit_config), &mut limit_said, server_limit, &own).await;
                        }
                        // The server learns (again) which agent sessions run here, then what it lacks
                        // of each: over the channel only that (a session's stream not started yet
                        // starts with its snapshot), else all of each.
                        if let Some(h) = &harness {
                            if server_wire {
                                updates_on = true;
                                // Updates queued meanwhile go on their streams; those of sessions
                                // whose stream starts now are in its snapshot.
                                while let Ok(crate::harness::session::Taken { session, update: u, at }) = harness_rx.try_recv() {
                                    note_announced(&mut announced, &session, &u);
                                    if matches!(u, crate::harness::core::Update::MailFlow { .. }) && !proto::feature::speaks(&server_features, proto::feature::MAIL_FLOW) { continue; }
                                    if updates.started(&crate::wire::uplink::session_key(&session)) {
                                        push_update(&mut updates, &session, &u, false, &mut sent_changes, Some(at));
                                    }
                                }
                            } else {
                                updates_on = false;
                                updates = crate::wire::uplink::Updates::new(format!("{:016x}", rand::random::<u64>()));
                                while harness_rx.try_recv().is_ok() {}
                            }
                            if let Ok(json) = serde_json::to_string(&h.inventory()) {
                                harness_kinds_sent = serde_json::to_string(&crate::harness::kinds::offered()).ok();
                                own.0.send(Up::Term(AgentMsg::HarnessInventory { json })).await.ok();
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
                                    raw_harness_update(&own, &info.id, &u).await;
                                }
                            }
                            flush_updates(&mut updates, &own).await;
                        }
                        if proto::feature::speaks(&server_features, proto::feature::MAIL_FLOW) {
                            request_mail_replay(&mut mail_replay, harness.as_ref(), &own, !updates_on).await;
                        }
                        // Keep the existing takeover pending until this connection's feature
                        // answer is known; Welcome alone cannot advertise journal support.
                        #[cfg(unix)]
                        legacy_progress.request(harness.as_ref(), &mut harness_operations, &bg_tx, LegacyCause::Observe);
                        #[cfg(unix)]
                        if let Some(path) = feature_negotiation.take_over(welcomed, &mut take_over) {
                            start_take_over(path, harness.clone(), bg_tx.clone(), Me { machine: name.clone(), instance: instance_id.clone(), root: root.clone() }, stores_wait);
                        }
                        // The server knows this runner now: reach may relay (its links resume).
                        if let Some(r) = &reach {
                            r.feed(crate::reach::node::In::Server(true)).await;
                            crate::reach::told::server_up();
                        }
                        // What it sent on its Welcome is out, before this on its lane: its admission ends.
                        if server_admission {
                            own.0.send(Up::Term(AgentMsg::Settled)).await.ok();
                        }
                    }
                    // Over its admission limit (docs/resilience.md R5): this runner's next dial at its turn, drawn within
                    // it; it closes its link, whose frames the server lets go meanwhile.
                    ServerMsg::Later { after_us, width_us } => {
                        let (after, width) = (Duration::from_micros(after_us), Duration::from_micros(width_us));
                        tracing::info!(after_ms = after.as_millis() as u64, width_ms = width.as_millis() as u64, "the server is taking runners back at the rate it can: dialing again at this runner's turn");
                        redials.later(Instant::now(), after, width, rand::random());
                        net.command(Out::Disconnect { peer: SERVER_PEER }).await;
                    }
                    // The server's drain (docs/resilience.md C3, step 5): it says when it expects to answer again, and
                    // this runner's turn after that: the next dial then, not to a server that's down. It closes its link.
                    ServerMsg::Back { after_us, width_us } => {
                        let (after, width) = (Duration::from_micros(after_us), Duration::from_micros(width_us));
                        tracing::info!(after_ms = after.as_millis() as u64, width_ms = width.as_millis() as u64, "the server is going down: dialing it again when it expects to answer");
                        redials.back(Instant::now(), after, width, rand::random());
                        net.command(Out::Disconnect { peer: SERVER_PEER }).await;
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
                        if let Some(ob) = &observe {
                            ob.up("", false);
                        }
                        if let Some(r) = &reach {
                            r.feed(crate::reach::node::In::Server(false)).await;
                            crate::reach::told::server_down();
                        }
                        // Its Closed has the runner dial again when its redials say.
                        net.command(Out::Disconnect { peer: SERVER_PEER }).await;
                    }
                    ServerMsg::Assign(mut a) => {
                        // The URL this agent reached the server through is the one that works
                        // from here (NAT, NodePorts, cluster DNS all differ).
                        a.server_url = o.server_url.clone();
                        let id = a.job_id.clone();
                        let holds = crate::hub::v1_incoming(a.spec.exclusive, &a.spec.resources);
                        let room = admits(o.slots, probing.sized(size), jobs.values().map(|j| (j.pooled, j.holds)), false, holds);
                        if o.outpost || draining || upgrade_pending || room.is_err() || jobs.contains_key(&id) {
                            let reason = if o.outpost { OUTPOST_TAKES_NO_JOBS } else if draining { REFUSED_DRAINING } else if upgrade_pending { REFUSED_UPGRADING } else { room.err().unwrap_or("no free slot") };
                            own.msg(AgentMsg::Reject { job: id, reason: reason.into() }).await;
                            continue;
                        }
                        // Admitted against the same ledger as the runs placed here (AUDIT M4, G1).
                        if let Err(why) = leg_room(reach.as_ref(), &id, crate::sense::run::leg_key(&a.repo, "", &a.spec.key, ""), a.spec.exclusive, crate::hub::v1_asked(&a.spec.resources), &a.spec.requires).await {
                            own.msg(AgentMsg::Reject { job: id, reason: why }).await;
                            continue;
                        }
                        if o.dedicated_job.as_deref() == Some(id.as_str()) {
                            served_dedicated = true;
                        }
                        // Its own identity, as a leg's: a key made here, outside anything its steps
                        // can reach, that signs what it sent at its end, as a v2 leg's does
                        // (docs/ci.md W27-R01, R05).
                        if server_identity {
                            match crate::runner::identity::ask("leg", Some(&id), None) {
                                Ok(json) => own.msg(AgentMsg::Identity { json }).await,
                                Err(e) => tracing::warn!(job = %id, error = %format!("{e:#}"), "no identity asked for"),
                            }
                        }
                        crate::runner::signed::forget(&id);
                        let cancel = Shutdown::new();
                        hold_cpus(&id, holds);
                        jobs.insert(id.clone(), Running { cancel: cancel.clone(), kill: Shutdown::new(), pooled: false, holds });
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
                        let first = server_signs;
                        crate::rt::spawn(async move {
                            // Its first step waits for its identity, as a v2 leg's (docs/ci.md W27-R05).
                            if first {
                                crate::runner::signed::identity_first(&id).await;
                            }
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
                    // Pause and Resume (docs/ci.md W26-R22): the job stops where it runs, keeping
                    // this runner, or goes on from where it was. Answered once done.
                    ServerMsg::Freeze { job, frozen } => match (jobs.contains_key(&job), crate::runner::freeze::of(&job)) {
                        (false, _) => refuse(&mut answer_after, crate::wire::channel::Refusal::Unknown, format!("no job {job} runs here")),
                        (true, None) => refuse(&mut answer_after, crate::wire::channel::Refusal::Unsupported, "nothing of it runs here that could be frozen in place yet: it's starting, or runs in a microVM of its own"),
                        (true, Some(f)) => match f.freeze(frozen).await {
                            Ok(()) => tracing::info!(%job, frozen, "a job frozen in place, or let go on"),
                            Err(why) => {
                                tracing::warn!(%job, frozen, %why, "a job couldn't be frozen in place, or let go on");
                                refuse(&mut answer_after, crate::wire::channel::Refusal::Refused, why);
                            }
                        },
                    },
                    // The features the server speaks by name (docs/wire.md): each says them all.
                    ServerMsg::Features { names } => {
                        let pages = proto::feature::speaks(&names, proto::feature::STATE_PAGES);
                        let refresh = pages && !sent_changes.state_pages && welcomed;
                        let mail = proto::feature::speaks(&names, proto::feature::MAIL_FLOW);
                        let replay = welcomed && mail && (refresh || !proto::feature::speaks(&server_features, proto::feature::MAIL_FLOW));
                        let transfers = proto::feature::speaks(&names, proto::feature::MAIL_TRANSFER);
                        let transfer_replay = welcomed && transfers && !proto::feature::speaks(&server_features, proto::feature::MAIL_TRANSFER);
                        crate::harness::handing::mail_supported(transfers);
                        #[cfg(unix)]
                        feature_negotiation.names();
                        sent_changes.state_pages = pages;
                        tracing::debug!(?names, "the server speaks these features");
                        crate::reach::told::server_features(&names);
                        // Its admission's hints go to a server that takes them (AUDIT O1).
                        if let Some(r) = &reach {
                            r.feed(crate::reach::node::In::Offers(proto::feature::speaks(&names, proto::feature::OFFERS))).await;
                        }
                        server_features = names;
                        // Welcome precedes the named feature reply. Re-send through the existing
                        // resumable stream so a quiet session's first oversized state is recovered too.
                        if refresh {
                            if let Some(h) = &harness {
                                for info in h.list() {
                                    if let Some(s) = h.get(&info.id) {
                                        start_stream(&mut updates, &info.id, &s, server_kept.get(&info.id), &mut sent_changes);
                                    }
                                }
                            }
                            flush_updates(&mut updates, &own).await;
                        }
                        if replay || transfer_replay { request_mail_replay(&mut mail_replay, harness.as_ref(), &own, !updates_on).await; }
                        if !mail && !transfers { mail_replay = MailReplay::default(); }
                        #[cfg(unix)]
                        legacy_progress.request(harness.as_ref(), &mut harness_operations, &bg_tx, LegacyCause::Observe);
                        #[cfg(unix)]
                        if let Some(path) = feature_negotiation.take_over(welcomed, &mut take_over) {
                            start_take_over(path, harness.clone(), bg_tx.clone(), Me { machine: name.clone(), instance: instance_id.clone(), root: root.clone() }, stores_wait);
                        }
                    }
                    ServerMsg::HarnessMailTransfer { json } => {
                        if !proto::feature::speaks(&server_features, proto::feature::MAIL_TRANSFER) {
                            refuse(&mut answer_after, crate::wire::channel::Refusal::Unsupported, "durable mail transfer was not negotiated");
                        } else if let (Some(host), Ok(answer)) = (harness.as_ref(), crate::wire::payload::<crate::harness::mail::TransferDown>(crate::wire::unread::Wire::ServerPayload, "HarnessMailTransfer", &json)) {
                            match host.mail_transfer_event(answer).await {
                                Ok(crate::harness::host::transfer::Effect::Offered { manifest, policy, role }) => {
                                    if let Some(reach) = &reach { reach.feed(crate::reach::node::In::AdoptPolicy(policy)).await; }
                                    if role == crate::harness::inbox::transfer::Role::Destination && crate::harness::handing::post_transfer(&crate::harness::mail::TransferUp::Ready { manifest }).await.is_err() {
                                        refuse(&mut answer_after, crate::wire::channel::Refusal::Unknown, "durable mail transfer readiness could not be sent; ownership remains pending");
                                    }
                                }
                                Ok(crate::harness::host::transfer::Effect::Progress { recover }) => { if recover { mail_replay.request(harness.as_ref()); } },
                                Err(_) => refuse(&mut answer_after, crate::wire::channel::Refusal::Unknown, "durable mail transfer phase could not be persisted; ownership remains pending"),
                            }
                        } else {
                            refuse(&mut answer_after, crate::wire::channel::Refusal::Unsupported, "durable mail transfer metadata or source storage is unavailable");
                        }
                    }
                    // Its rules for the copies of other runners' trees here, and an ask to let go
                    // now, as they change: its keeper looks at once (docs/reach.md §5.1, goal 9).
                    ServerMsg::Copies { json } => crate::reach::keeper::told(&json),
                    // Installed skills (docs/agents.md, "Skills kiln installs"): what this machine
                    // lays out and which session has which, a folder to read, and the files it asked for.
                    ServerMsg::Skills { json } => crate::skills::runner::told(&json, own.clone(), harness.clone()),
                    ServerMsg::SkillFile { sha256, offset, data, done } => crate::skills::runner::file(sha256, offset, data, done, own.clone(), harness.clone()).await,
                    // Another runner's room, for this runner's owner to choose where to probe by
                    // (docs/needs.md E5, AUDIT O1).
                    ServerMsg::Offers { json } => {
                        if let Some(r) = &reach {
                            r.feed(crate::reach::node::In::Hint(json)).await;
                        }
                    }
                    // Own an agent session's start (AUDIT M4): its node places it, and says back.
                    ServerMsg::Place { req, json } => match &reach {
                        Some(r) => {
                            r.feed(crate::reach::node::In::SessionPlace { req, json }).await;
                        }
                        None => {
                            let word = crate::reach::place::SessionWord::Fallback { runners: Vec::new() };
                            own.msg(AgentMsg::Placed { req, json: serde_json::to_string(&word).unwrap_or_default() }).await;
                        }
                    },
                    // From an epoch older than the newest this runner has seen (W05-R37): not applied.
                    ServerMsg::Stop { grant, .. } if leases.stale(grant.epoch) => {
                        refuse(&mut answer_after, crate::wire::channel::Refusal::StaleEpoch, format!("a Stop from epoch {}, older than this runner has seen", grant.epoch));
                    }
                    // Its grant to stop (W05-R34): another of the leg's grants it runs since is let be.
                    ServerMsg::Stop { grant, reason } => match jobs.get(&grant.leg).filter(|_| grants.get(&grant.leg) == Some(&grant)) {
                        Some(r) => {
                            tracing::info!(job = %grant.leg, attempt = grant.attempt, %reason, "stopping job");
                            r.cancel.cancel();
                            let (tx, j) = (done_tx.clone(), grant.leg.clone());
                            crate::rt::spawn(async move {
                                crate::rt::sleep(CANCEL_GRACE).await;
                                let error = format!("stopped; it hadn't ended {}s later, so the runner let it go", CANCEL_GRACE.as_secs());
                                let _ = tx.send(JobResult { job: j, status: Status::Cancelled, exit_code: None, error: Some(error), peak_rss: None, cpu_seconds: None }).await;
                            });
                        }
                        None => refuse(&mut answer_after, crate::wire::channel::Refusal::Unknown, format!("no grant {}.{} of {} runs here", grant.epoch, grant.n, grant.leg)),
                    },
                    // What the server renewed (W05-R34): each grant it named lives its lease from
                    // when its Renew was sent, less a tenth; one it says isn't this runner's any more
                    // ends now, and if it stops on losing its lease it's stopped at the next check
                    // (W05-R37). An answer from an older epoch changes nothing.
                    ServerMsg::Renewed { seq, lease_ms, epoch, lost } => {
                        for g in &lost {
                            tracing::warn!(job = %g.leg, attempt = g.attempt, "the server says this runner's grant for the leg isn't its own any more");
                        }
                        leases.renewed(seq, lease_ms, epoch, &named(&lost), crate::runner::lease::now_ms());
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
                            own.0.send(Up::Term(AgentMsg::TermExit { session, code: None, error: Some("terminal disabled on this agent".into()) })).await.ok();
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
                                own.0.send(Up::Term(AgentMsg::TermExit { session, code: None, error: Some(format!("{e:#}")) })).await.ok();
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
                        own.0.send(Up::Term(msg)).await.ok();
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
                            own.0.send(Up::Term(AgentMsg::AiTranscript { req, json: None })).await.ok();
                        }
                    },
                    // The provenance kiln wrote of what a leg built, signed as the leg when it's of
                    // what the leg reported (docs/ci.md W27-R06).
                    ServerMsg::SignAsLeg { job, nonce, payload_type, payload } => {
                        let (bundle, error) = match crate::runner::signed::sign_provenance(&job, &nonce, &payload_type, &payload) {
                            Ok(b) => (Some(b), None),
                            Err(e) => (None, Some(e)),
                        };
                        own.msg(AgentMsg::LegSignature { job, nonce, bundle, error }).await;
                    }
                    ServerMsg::AiView { req, pid } => match &ai {
                        Some(ai) => ai.view(req, pid, uplink.0.clone()),
                        None => {
                            own.0.send(Up::Term(AgentMsg::AiView { req, json: None })).await.ok();
                        }
                    },
                    ServerMsg::HarnessFile { session, file, offset, data, done } => {
                        if let Some((code, why)) = loops::file(&shared, &session, &file, offset, &data, done) { refuse(&mut answer_after, code, &why); }
                    }
                    ServerMsg::HarnessRestoreCopy { session } => {
                        let Some(host)=harness.clone() else{refuse(&mut answer_after,crate::wire::channel::Refusal::Unknown,"this runner has no agent host");continue;};
                        let reply=answer_after.take().map(|(stream,seq,_)|(stream,seq));
                        harness_operations.push(async move{
                            let result=host.restore_archive_copy(&session).await.map_err(|why|(crate::wire::channel::Refusal::Unknown,why));
                            HarnessApplied::Command { reply, result }
                        }.boxed());
                    }
                    ServerMsg::HarnessArchive { session, restore, only_if_quiet } => {
                        let Some(host)=harness.clone() else{refuse(&mut answer_after,crate::wire::channel::Refusal::Unknown,"this runner has no agent host");continue;};
                        let dir=o.state_dir.clone();let reply=answer_after.take().map(|(stream,seq,_)|(stream,seq));
                        // The operation may wait for a Reach Exit delivered by this very loop.
                        // Retain its future and withhold the command ACK until every stop confirms.
                        harness_operations.push(async move{
                            use crate::wire::channel::Refusal;
                            let result=if restore && only_if_quiet{Err((Refusal::Refused,"quiet-only archive admission cannot restore a session".into()))}else if restore{host.restore_archive(&session).await.map_err(|why|(Refusal::Unknown,why))}else if only_if_quiet{
                                match host.archive_if_quiet(&session).await{
                                    Ok(true)=>Ok(()),
                                    Ok(false)=>Err((Refusal::Refused,"the agent received work before automatic archive; it remains active".into())),
                                    Err(why)=>Err((Refusal::Unknown,why)),
                                }
                            }else{host.archive(&session).await.map_err(|why|(Refusal::Unknown,why))};
                            if result.is_ok() && !restore{crate::browser::apps::stop(&session);crate::browser::stream::streams().close_session(&dir,&session).await;}
                            HarnessApplied::Command { reply, result }
                        }.boxed());
                    }
                    ServerMsg::HarnessClose { session } => {
                        if let Some((code, why)) = loops::close(&shared, session) {
                            refuse(&mut answer_after, code, why);
                        }
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
                    ServerMsg::HarnessRelease { session } => loops::release(&shared, &session),
                    ServerMsg::Upgrade { job, path, sha256, size } if o.outpost => {
                        // Nothing of an outpost is installed: its next kiln is the one downloaded next.
                        let _ = (path, sha256, size);
                        let error = Some("an outpost isn't installed, so the server doesn't upgrade it".to_owned());
                        own.msg(AgentMsg::UpgradeDone { job, ok: false, error }).await;
                    }
                    ServerMsg::Upgrade { job, path, sha256, size } if !takes_updates(&o) => {
                        let _ = (path, sha256, size);
                        tracing::info!(%job, "an upgrade refused: this machine's kiln config says [updates] accept = false");
                        let error = Some("this machine refuses upgrades: its kiln config says [updates] accept = false".to_owned());
                        own.msg(AgentMsg::UpgradeDone { job, ok: false, error }).await;
                    }
                    #[cfg(unix)]
                    ServerMsg::Upgrade { job, path, sha256, size } if moving.is_some() || retiring => {
                        let _ = (path, sha256, size);
                        let error = Some("this runner is already being upgraded".to_owned());
                        own.msg(AgentMsg::UpgradeDone { job, ok: false, error }).await;
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
                            own.0.send(Up::Term(AgentMsg::HarnessHistory { req, json: None })).await.ok();
                        }
                    },
                    ServerMsg::HarnessTaskOutput { req, session, task, since } => loops::task_output(&shared, req, session, task, since),
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
                            own.0.send(Up::Term(AgentMsg::TermExit { session, code: None, error: Some("terminal disabled on this agent".into()) })).await.ok();
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
                                    crate::runner::kept::shell_within(&job, Instant::now() + LEG_SHELL_WAIT).await;
                                    let _ = again.send(Bg::Again(waiting, frame)).await;
                                });
                                continue;
                            }
                            shell_waits.remove(&session);
                            own.0.send(Up::Term(AgentMsg::TermExit { session, code: None, error: Some("the leg isn't running or kept on this runner".into()) })).await.ok();
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
                                own.0.send(Up::Term(AgentMsg::TermExit { session, code: None, error: Some(format!("{e:#}")) })).await.ok();
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
                        signed_unreceived.remove(&job);
                        if !jobs.contains_key(&job) {
                            grants.remove(&job);
                        }
                        if let Some((at, _, events)) = journal.get_mut(&job) {
                            events.clear();
                            let due = at.checked_add(JOURNAL_KEPT);
                            journal_next = [journal_next, due].into_iter().flatten().min();
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
                    | ServerMsg::Custody { .. }
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
                        flush_updates(&mut updates, &own).await;
                    }
                } else {
                    own.0.send(Up::Term(msg)).await.ok();
                }
            }
            Ev::Up(mut up) => {
                // A test build's runner behind its jobs (`lag=`): what they send, taken late.
                if let Some(lag) = chaos.and_then(|c| c.lag)
                    && matches!(up, Up::Log(_) | Up::Msg(AgentMsg::JobEvent { .. } | AgentMsg::StepFinished { .. }))
                {
                    crate::rt::sleep(lag).await;
                }
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
                // What a leg hands on, folded in as it's sent, for it to sign at its end (docs/ci.md
                // W27-R05, R06): its logs' chunks, its steps' ends, its attest steps' subjects.
                match &up {
                    Up::Log(c) => crate::runner::signed::log(&c.job, c.idx, &c.data),
                    Up::Msg(AgentMsg::StepFinished { job, idx, status, exit_code }) => crate::runner::signed::step(job, *idx, *status, *exit_code),
                    Up::Msg(AgentMsg::JobEvent { job, json }) if json.contains("\"t\":\"attest\"") || json.contains("\"t\":\"step_finished\"") => crate::runner::signed::event(job, json),
                    _ => {}
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
                // An answer to one of the server's commands: when it was made, and what it answered (runner-fan).
                let answer = if matches!(&up, Up::Term(AgentMsg::Wire(crate::wire::channel::Frame::Ack(_)))) { answers_at.pop_front() } else { None };
                let Ok(data) = data else { continue };
                if let Some((_, _, events)) = journaled.as_ref().and_then(|j| journal.get_mut(j)) {
                    events.push(data.clone());
                }
                // What the next connection sends again (a leg's result not yet received, its
                // events once the server takes them again) needn't be kept for it.
                let again = matches!(&up, Up::Msg(AgentMsg::JobFinished(r)) if unreceived.contains_key(&r.job)) || (receipts && journaled.is_some());
                // Logs and jobs' messages wait for the Welcome too, and come after what waited.
                if connected && (welcomed || (lane != LANE_LOGS && lane != LANE_CONTROL)) {
                    let (lane, kind, data) = wire_form(lane, kind, data, &grants, server_leases);
                    match &delay_line {
                        Some(d) => d.send(lane, kind, data, again),
                        None => {
                            net.send(SERVER_PEER, lane, kind, data).await;
                        }
                    }
                    if let Some((made, answered)) = answer {
                        crate::route_latency::observe_in(crate::route_latency::InLeg::Answer, answered, made.elapsed());
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
            Ev::Harness(session, u, taken_at) => {
                // Admission/terminal receipts stay in the source journal until acknowledged.
                // A peer not yet welcomed/negotiated cannot own these events; its next real
                // connection/feature event replays the journal and current capacity.
                if matches!(u, crate::harness::core::Update::MailFlow { .. }) && (!connected || !welcomed || !proto::feature::speaks(&server_features, proto::feature::MAIL_FLOW)) { continue; }
                note_announced(&mut announced, &session, &u);
                // A session's turn ending is what an upgrade waiting on its sessions waits on.
                if upgrade_pending && jobs.is_empty() && harness.as_ref().is_some_and(|h| h.busy() == 0) {
                    taken.push_back(Ev::Watchdog);
                }
                // Each session this process holds has an identity at its epoch, asked for until it
                // has one; one that closed lets go of its keys (docs/ci.md W27-R01).
                let mut took_up = false;
                match &u {
                    crate::harness::core::Update::Open { info, .. } => {
                        session_epochs.insert(session.clone(), info.epoch);
                    }
                    // It went on at a later epoch (a blue/green take-over, once recorded). A turn
                    // starting takes up the work kiln handed it, signed for (W27-R08).
                    crate::harness::core::Update::State { state } => {
                        if let Some(e) = session_epochs.get_mut(&session) {
                            *e = (*e).max(state.epoch);
                        }
                        took_up = crate::runner::custody::status(&session, &state.status);
                    }
                    crate::harness::core::Update::Closed => {
                        session_epochs.remove(&session);
                        crate::runner::identity::forget_prefix(&format!("agent/{session}/"));
                        crate::runner::custody::closed(&session);
                    }
                    _ => {}
                }
                if took_up && server_custody && connected {
                    for json in crate::runner::custody::drain() {
                        own.0.send(Up::Term(AgentMsg::Custody { json })).await.ok();
                    }
                }
                if server_identity
                    && connected
                    && let Some(epoch) = session_epochs.get(&session).copied()
                {
                    ask_agent_identity(&own, &mut identity_asked, &session, epoch).await;
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
                                let behind = !own_rx.is_empty() || !taken.is_empty();
                                flush_updates_now(&mut updates, &own, behind, |data| send_term(&net, delay_line.as_ref(), data)).await;
                            }
                            continue 'events;
                        }
                        updates.start(&key);
                        opens = true;
                    }
                    // A new session's Open is its stream's picture. Its frames say how long this runner held it
                    // (goal 10, plan 01a1a87b2dae step 5).
                    push_update(&mut updates, &session, &u, opens, &mut sent_changes, Some(taken_at));
                    if matches!(u, crate::harness::core::Update::Closed) {
                        updates.closing(&key);
                    }
                    // Overflow can discard an earlier receipt even when this update is ordinary
                    // history. Do not wait for another server frame when the discarded stream was
                    // the only unanswered one. The existing picture gate still bounds resets;
                    // while disconnected no new snapshot/journal read is made.
                    if connected && welcomed
                        && restart_updates(&mut updates, harness.as_ref(), &terms, &server_kept, &mut sent_changes)
                        && proto::feature::speaks(&server_features, proto::feature::MAIL_FLOW)
                    {
                        mail_replay.request(harness.as_ref());
                    }
                    // Straight to the link from this turn, unless what this loop said or took before waits to go
                    // (`flush_updates_now`; goal 10, plan 01a1a87b2dae step 5).
                    if connected {
                        let behind = !own_rx.is_empty() || !taken.is_empty();
                        flush_updates_now(&mut updates, &own, behind, |data| send_term(&net, delay_line.as_ref(), data)).await;
                    }
                } else if connected && welcomed {
                    // A server from before the channel: not kept across a disconnect, the
                    // reconnect sends every session whole (and before its Welcome, nothing: the
                    // Welcome brings the server all of each, over the channel or not).
                    raw_harness_update(&own, &session, &u).await;
                }
            }
            Ev::Done(mut r) => {
                // What its job sent before it ended (its log's last chunks, its last step's end)
                // may still wait on their own channel, this loop behind its jobs: they go first, so
                // the leg signs all it sent (docs/ci.md W27-R05), then its end.
                if in_turn && !up_rx.is_empty() {
                    for _ in 0..up_rx.len() {
                        let Ok(up) = up_rx.try_recv() else { break };
                        taken.push_back(Ev::Up(up));
                    }
                    taken.push_back(Ev::Done(r));
                    continue 'events;
                }
                // A job's end is what a new binary waiting on disk, or a relaunch, waits on (goal 9:
                // the watchdog looked every 5 s).
                if upgrade_pending || relaunch_wanted {
                    taken.push_back(Ev::Watchdog);
                }
                if retiring {
                    retire_at = Some(Instant::now() + Duration::from_secs(5)).max(retire_at);
                }
                if jobs.remove(&r.job).is_some() {
                    leases.ended(&r.job);
                    // Its room in placement's ledger is free at once (AUDIT M4).
                    if let Some(h) = &reach {
                        h.leg_ended(&r.job, r.exit_code.is_some()).await;
                    }
                    // Stopped for its lease: it ends so, whatever it said, as a runner's loss, so
                    // that a server that takes it before its own lease ran out tries the leg again
                    // (W05-R37, R16).
                    if lease_lost.remove(&r.job) {
                        r.status = Status::Failed;
                        if !r.error.as_deref().is_some_and(|e| e.starts_with(LEASE_LOST)) {
                            r.error = Some(LEASE_LOST.to_owned());
                        }
                        if let Ok(json) = serde_json::to_string(&kiln_ci::wire::JobEventV2::FailureClass { class: kiln_ci::sched::retry::FailureClass::RunnerLost.as_str().to_owned() }) {
                            own.msg(AgentMsg::JobEvent { job: r.job.clone(), json }).await;
                        }
                    }
                    crate::runner::cpus::pins().release(&r.job);
                    // The leg signs what it received and hands on while it still holds its key
                    // (docs/ci.md W27-R05): its records go before its result, and again with it.
                    if server_signs {
                        match crate::runner::signed::sign_records(&r.job, &r) {
                            Ok(bundles) => {
                                let signed = AgentMsg::LegResults { job: r.job.clone(), bundles };
                                if let Ok(b) = proto::encode(&signed) {
                                    signed_unreceived.insert(r.job.clone(), b);
                                }
                                own.msg(signed).await;
                            }
                            Err(e) => tracing::warn!(job = %r.job, error = %format!("{e:#}"), "the leg's records couldn't be signed"),
                        }
                    } else {
                        crate::runner::signed::forget(&r.job);
                    }
                    // The leg's key goes with it (docs/ci.md W27-R01).
                    crate::runner::identity::forget(&crate::runner::identity::slot("leg", Some(&r.job), None));
                    tracing::info!(job = %r.job, status = r.status.as_str(), "job finished");
                    if let Ok(b) = proto::encode(&AgentMsg::JobFinished(r.clone())) {
                        unreceived.insert(r.job.clone(), (Instant::now(), b));
                    }
                    own.msg(AgentMsg::JobFinished(r)).await;
                } else {
                    // Reported over already: a cancelled job let go of after its grace that ended
                    // after all (or the grace's report, after the job's own).
                    tracing::info!(job = %r.job, "a job already reported over has ended");
                }
            }
            // A grant whose lease ended unrenewed, and that stops on losing it, is stopped now, as a
            // Cancel stops it; a finish grant runs on (W05-R37). By this machine's clock, which
            // counts sleep: one waking past its lease end stops them within a second.
            Ev::Lease => {
                let now = crate::runner::lease::now_ms();
                for job in leases.due(now) {
                    let Some(r) = jobs.get(&job) else { continue };
                    let end = leases.end(&job).unwrap_or_default();
                    tracing::warn!(%job, "its lease ended unrenewed (at {end} ms by this machine's clock, now {now} ms), and it stops on losing it: stopping it");
                    // Killed, not cancelled: no grace past its lease, nothing more run.
                    r.kill.cancel();
                    r.cancel.cancel();
                    lease_lost.insert(job.clone());
                    let tx = done_tx.clone();
                    crate::rt::spawn(async move {
                        crate::rt::sleep(CANCEL_GRACE).await;
                        let error = format!("{LEASE_LOST}; it hadn't ended {}s later, so the runner let it go", CANCEL_GRACE.as_secs());
                        let _ = tx.send(JobResult { job, status: Status::Failed, exit_code: None, error: Some(error), peak_rss: None, cpu_seconds: None }).await;
                    });
                }
            }
            // A lease runner renews the grants it holds (W05-R34), each lease counted from now
            // (W05-R37): at its own due time, two RTOs of its link before the earliest end of what it
            // holds (`Leases::renew_due`), in place of its heartbeat.
            Ev::Renew => {
                grants.retain(|job, _| jobs.contains_key(job) || unreceived.contains_key(job));
                if connected && server_leases {
                    renew_seq += 1;
                    let held = held_grants(&grants, &jobs, &unreceived);
                    leases.renew_sent(renew_seq, crate::runner::lease::now_ms(), named(&held));
                    own.msg(AgentMsg::Renew { seq: renew_seq, held }).await;
                }
            }
            // Its chores, each as it falls due or as what it waits on moves (goal 9): they were all
            // looked at every 10 s.
            Ev::Heartbeat => {
                // Custody records signed since (a session's hand-on or receipt, docs/ci.md W27-R08),
                // with the receipts of work taken up before this process held the session's identity;
                // and the reach links' records whose waits are over (`custody::reach_due_at`).
                custody_seen = crate::runner::custody::moves();
                crate::runner::custody::take_up_due();
                crate::runner::custody::role_due();
                crate::runner::custody::relay_due();
                crate::runner::custody::reach_due();
                if server_custody && connected {
                    for json in crate::runner::custody::drain() {
                        own.0.send(Up::Term(AgentMsg::Custody { json })).await.ok();
                    }
                }
                // Each agent session it holds and has no identity for yet, asked for again once its
                // last ask's wait is over (a retry's backoff, `ask_agent_identity`): a quiet one's
                // first ask may have come before the server had its Open, and was refused. What has
                // its identity now, or whose session is gone, waits no more.
                if server_identity && connected {
                    for (session, epoch) in &session_epochs {
                        ask_agent_identity(&own, &mut identity_asked, session, *epoch).await;
                    }
                }
                prune_identity_asks(&mut identity_asked, &session_epochs);
                // What it folded of the server's it couldn't read, and no word carried for a
                // minute: told now (`wire::unread::Told::flush`, due at `flush_due`).
                if server_unread && let Some((f, folded)) = told.flush(Instant::now()) {
                    own.msg(AgentMsg::Unread { wire: f.wire, what: f.what, code: f.code, why: f.why, folded, session: None }).await;
                }
                // Counts of jobs long done (a kept leg is let go well within a day): let go as the
                // first falls due.
                if journal_next.is_some_and(|t| Instant::now() >= t) {
                    journal.retain(|_, (at, _, events)| !events.is_empty() || at.elapsed() < JOURNAL_KEPT);
                    journal_next = journal.values().filter(|(_, _, events)| events.is_empty()).filter_map(|(at, _, _)| at.checked_add(JOURNAL_KEPT)).min();
                }
                // A lease runner's heartbeat is its `Renew` (`Ev::Renew`). An older server's, from a
                // runner holding legs, renews their grants: as what it says changes, and while legs
                // run at that server's own cadence (`OLD_BEAT`).
                if connected && !server_leases {
                    let (free, running) = beat_of(o.slots, upgrade_pending || retiring, &jobs);
                    if beat_said.as_ref().is_none_or(|(f, r, at)| *f != free || *r != running || (!running.is_empty() && at.elapsed() >= OLD_BEAT)) {
                        own.msg(AgentMsg::Heartbeat { free_slots: free, running: running.clone() }).await;
                        beat_said = Some((free, running, Instant::now()));
                    }
                }
            }
            // Its config file changed (goal 9; it was read every 10 s): its agent limit, a change
            // held at once with nothing restarting, and its machine's say on upgrades, told the
            // server at once. A config that can't be read now (a change half written) keeps the
            // last one read: the write that finishes it is another change.
            Ev::Config => {
                if let (Some(h), Some(path)) = (&harness, o.config.clone()) {
                    let read = crate::rt::blocking(move || crate::harness::limit::configured(&path)).await.ok().flatten();
                    if let Some(config) = read.filter(|c| *c != limit_config) {
                        if let Some(n) = config.filter(|n| crate::harness::limit::valid(*n).is_err()) {
                            tracing::warn!(max_agent_sessions = n, "[agent] max_agent_sessions is from 1 to {}: passed over", crate::harness::limit::MAX);
                        }
                        limit_config = config;
                        hold_agent_limit(h, crate::harness::limit::held(limit_asked, limit_config), &mut limit_said, server_limit && welcomed, &own).await;
                    }
                }
                if connected && !o.ephemeral && !o.outpost {
                    let accept = takes_updates(&o);
                    if accept_said != Some(accept) {
                        own.msg(AgentMsg::UpdatePrefs { accept }).await;
                        accept_said = Some(accept);
                    }
                }
            }
            // An item of its updates' channel is overdue (lost below the channel): it goes again, at
            // its own RTO (`Updates::due`), where every sample looked.
            Ev::Overdue => {
                if connected && updates_on {
                    flush_updates(&mut updates, &own).await;
                }
            }
            // Its QUIC layer let frames go below the channel (goal 9): what it has of the server's
            // commands said again, one lost with them mustn't hold the server's commands up until
            // the next connection (they were said with every sample); and what it let go told, once
            // it changed, for its page.
            Ev::Dropped => {
                drops_seen = crate::metrics::net().dropped_so_far();
                let dropped = crate::metrics::net().counts();
                // Said again for what a live link lost (shed, or a test build's chaos), as the server's
                // is (docs/wire.md): not for a frame refused, nor one its link went with.
                let lost: u64 = dropped.iter().filter(|d| d.reason == "shed" || d.reason == "chaos").map(|d| d.frames).sum();
                if connected && server_wire && lost != lost_said {
                    own.0.send(Up::Term(AgentMsg::Wire(loops::have_of(wire_in.have(), &shared)))).await.ok();
                }
                lost_said = lost;
                if connected && server_drops && welcomed && drops_said.as_ref() != Some(&dropped) {
                    own.msg(AgentMsg::NetDrops { dropped: dropped.clone() }).await;
                    drops_said = Some(dropped);
                }
            }
            // Something moved its host's sample (W05-R26, goal 9): a run's start or end, the OS's
            // word on its pressure, clocks or power, a connection. Taken on the blocking pool, one at
            // a time, and told the server and placement's room (docs/needs.md E5).
            Ev::Sample => {
                moves_seen = crate::sense::pace::moves();
                if connected && !sampling.swap(true, std::sync::atomic::Ordering::AcqRel) {
                    let (sampler, sampling, said) = (Arc::clone(&sampler), Arc::clone(&sampling), own.clone());
                    let (quic, reader) = (net.clone(), Arc::clone(&topo_reader));
                    let room = reach.clone();
                    crate::rt::spawn(async move {
                        if let Ok(s) = crate::rt::blocking(move || sampler.lock().sample()).await {
                            if let Some(r) = &room {
                                r.feed(crate::reach::node::In::Sampled(s.clone())).await;
                            }
                            said.msg(AgentMsg::Sample(s)).await;
                        }
                        // The topology's reading at the same sample (LB08): QUIC's own round trip to
                        // the server and the interfaces' counters, nothing sent for it.
                        if let Some(r) = &room
                            && let Ok(read) = crate::rt::blocking(move || crate::reach::topo::read_now(&quic, &reader, crate::util::now_ms())).await
                        {
                            r.feed(crate::reach::node::In::Topo(read)).await;
                        }
                        sampling.store(false, std::sync::atomic::Ordering::Release);
                    });
                }
            }
            // The OS said its power source or its battery's charge moved (notify(3)'s power-source
            // and time-remaining keys, power-supply uevents, the power-setting callbacks), or it
            // connected: read, and said when it changed. A machine on battery sleeps when it runs
            // out, and the server says so as it gets low (it was read once a minute, and said again
            // every 10 minutes).
            Ev::Power => {
                power_seen = crate::sense::pace::powers();
                if connected
                    && !o.ephemeral
                    && !o.outpost
                    && let Ok(Some(b)) = crate::rt::blocking(crate::os::battery::read).await
                    && power_said.as_ref() != Some(&b)
                    && let Ok(json) = serde_json::to_string(&b)
                {
                    own.msg(AgentMsg::Power { json }).await;
                    power_said = Some(b);
                }
            }
            // One of its conversation stores committed: what they're doing told, for the server's
            // board (a server from before takes it for nothing). Never waits: a full link skips one.
            Ev::ConvoStats => {
                if connected {
                    for (st, role, feed) in [(convo.as_ref(), "own", shipper.as_ref().map(crate::convo::feed::Shipper::stats)), (mirror_store.as_ref(), "mirror", None)] {
                        if let Some(st) = st {
                            let report = crate::convo::telemetry::Report { role: role.into(), version: crate::convo::telemetry::version(), store: st.report(), feed };
                            let json = crate::convo::feed::encode(&crate::convo::feed::Msg::Stats { store: st.id().to_owned(), report: Box::new(report) });
                            let _ = uplink.0.try_send(Up::Convo(AgentMsg::Convo { json, data: Vec::new() }));
                        }
                    }
                }
            }
            // What it holds warm, for placement (docs/ci.md W24-R18): its own legs' changes and the
            // records the machine's other runner processes write (`crate::runner::warm`), told once
            // it changed (it was looked at every 5 s, and sent every 10 minutes for the others').
            Ev::Work => {
                let now_changes = crate::runner::warm::changes();
                if connected && server_work && welcomed && !o.ephemeral && !o.outpost && work_said != Some(now_changes) {
                    let dir = o.work_dir.clone();
                    if let Ok(state) = crate::rt::blocking(move || crate::runner::warm::state(&dir)).await
                        && let Ok(json) = serde_json::to_string(&state)
                    {
                        own.msg(AgentMsg::WorkState { json }).await;
                    }
                    work_said = Some(now_changes);
                }
            }
            // A harness installed (or removed) since, or found once its user's login shell said its
            // PATH (`crate::harness::kinds`): said (it was looked at every minute).
            Ev::Harnesses => {
                if connected && let Some(h) = &harness {
                    let now = serde_json::to_string(&crate::harness::kinds::offered()).ok();
                    if now != harness_kinds_sent
                        && let Ok(json) = serde_json::to_string(&h.inventory())
                    {
                        harness_kinds_sent = now;
                        own.0.send(Up::Term(AgentMsg::HarnessInventory { json })).await.ok();
                    }
                }
            }
            Ev::Routed(_) | Ev::Deferred(..) => {}
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
    state_pages: bool,
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
/// that (`sent`). `taken`: when this runner took it in (its session handed it on), for an update that
/// came so: each of its frames says how long before it was made (`crate::wire::stamp`; goal 10, plan
/// 01a1a87b2dae step 5). A stream's snapshot is the session as it stands, taken in before: it says none.
fn push_update(updates: &mut crate::wire::uplink::Updates, session: &str, u: &crate::harness::core::Update, picture: bool, sent: &mut SentChanges, taken: Option<Instant>) -> usize {
    use crate::harness::core::Update;
    let key = crate::wire::uplink::session_key(session);
    let frames = match u {
        Update::State { .. } | Update::Open { .. } if !sent.state_pages => legacy_harness_frames(session, u),
        Update::Changes { files } => sent.frames(session, files, picture),
        // Its server's copy starts its changeset over (an Open), or lets the session go.
        Update::Open { .. } | Update::Closed => {
            sent.forget(session);
            harness_frames(session, u)
        }
        _ => harness_frames(session, u),
    };
    // Read once its frames are made: their making is this runner's time with it too.
    let held = taken.map(|at| at.elapsed());
    let bodies: Vec<Vec<u8>> = frames
        .into_iter()
        .filter_map(|mut json| {
            if let Some(held) = held {
                crate::wire::stamp::put(&mut json, held);
            }
            proto::encode(&AgentMsg::Harness { session: session.to_owned(), json }).ok()
        })
        .collect();
    let bytes = bodies.iter().map(Vec::len).sum();
    if picture {
        for b in bodies {
            updates.picture(&key, b);
        }
    } else if matches!(u, Update::State { .. }) {
        // The session whole as it stands: held behind its last one while that waits for its
        // answer, a newer taking its place (`Updates::push_state`).
        updates.push_state(&key, bodies);
    } else if matches!(u, Update::Closed | Update::Reset { .. } | Update::Open { .. }) {
        // What ends or starts its timeline over: no state of before it comes after it.
        for b in bodies {
            updates.push_after_state(&key, b);
        }
    } else {
        for b in bodies {
            updates.push(&key, b);
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
        let n = push_update(updates, session, u, true, sent, None);
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
/// How long after asking for an agent session's identity it's asked for again, at first; each time
/// after, twice as long, up to [`AGENT_IDENTITY_MOST`].
const AGENT_IDENTITY_AGAIN: Duration = Duration::from_secs(5);
const AGENT_IDENTITY_MOST: Duration = Duration::from_secs(600);

/// A certificate notification counts only after the current key and exact subject were installed.
async fn installed_identity(given: crate::proto::IdentityGiven, state_dir: PathBuf, label: String) -> anyhow::Result<Option<(String, u64)>> {
    let expected = (given.kind == "agent").then(|| (given.of.clone(), given.epoch, given.uri.clone()));
    crate::runner::identity::given_off(given, state_dir, label).await?;
    let Some((Some(session), Some(epoch), uri)) = expected else { return Ok(None) };
    let slot = crate::runner::identity::slot("agent", Some(&session), Some(epoch));
    Ok((crate::runner::identity::cert(&slot).is_some() && crate::runner::identity::uri(&slot).as_deref() == Some(uri.as_str())).then_some((session, epoch)))
}

/// Asks for agent session `session`'s identity at `epoch` (docs/ci.md W27-R01), unless this
/// process holds one or asked lately ([`AGENT_IDENTITY_AGAIN`], doubling each time). A session kiln
/// hasn't heard of yet is refused (the ask can come before its Open, which rides the channel), and
/// asked for again as it goes on and on the heartbeat, so one that goes quiet gets it too.
async fn ask_agent_identity(uplink: &Uplink, asked: &mut HashMap<String, (Instant, Duration)>, session: &str, epoch: u64) {
    // Taken over blue/green: its next epoch's, once this runner is promoted (docs/ci.md W27-R08).
    let Some(epoch) = crate::runner::custody::to_ask(session, epoch) else { return };
    let slot = crate::runner::identity::slot("agent", Some(session), Some(epoch));
    if crate::runner::identity::cert(&slot).is_some() || asked.get(&slot).is_some_and(|(at, wait)| at.elapsed() < *wait) {
        return;
    }
    let wait = asked.get(&slot).map_or(AGENT_IDENTITY_AGAIN, |(_, wait)| wait.saturating_mul(2).min(AGENT_IDENTITY_MOST));
    asked.insert(slot, (Instant::now(), wait));
    // With the hand-on it was taken over with, and this process's receipt (docs/ci.md W27-R08).
    let ask = crate::runner::identity::request("agent", Some(session), Some(epoch)).and_then(|a| Ok(serde_json::to_string(&crate::runner::custody::carrying(session, epoch, a))?));
    match ask {
        Ok(json) => {
            uplink.0.send(Up::Term(AgentMsg::Identity { json })).await.ok();
        }
        Err(e) => tracing::warn!(session = %session, error = %format!("{e:#}"), "no identity asked for"),
    }
}

async fn flush_updates(updates: &mut crate::wire::uplink::Updates, uplink: &Uplink) {
    for f in updates.poll(Instant::now()) {
        uplink.0.send(Up::Term(AgentMsg::Wire(f))).await.ok();
    }
}

/// Resume actual stale streams from their pictures. A session picture does not contain the
/// durable source mail journal, so the caller also schedules one journal replay when any session
/// restarted. `take_stale` retains streams whose preceding picture still waits for its ACK.
fn restart_updates(
    updates: &mut crate::wire::uplink::Updates,
    host: Option<&Arc<crate::harness::host::Host>>,
    terms: &HashMap<String, (PtySession, Arc<crate::termsess::Session>)>,
    kept: &HashMap<String, proto::KeptSession>,
    sent: &mut SentChanges,
) -> bool {
    let mut session_reset = false;
    for key in updates.take_stale() {
        if let Some(id) = key.strip_prefix("s:") {
            if let Some(s) = host.and_then(|host| host.get(id)) {
                start_stream(updates, id, &s, kept.get(id), sent);
                session_reset = true;
            }
        } else if let Some(id) = key.strip_prefix("t:")
            && let Some((_, state)) = terms.get(id)
        {
            let a = state.attach();
            state.detach();
            updates.start(&key);
            if let Ok(b) = proto::encode(&AgentMsg::TermReplay { session: id.to_owned(), data: a.replay }) {
                updates.picture(&key, b);
            }
        }
    }
    session_reset
}

async fn raw_harness_update(own: &Uplink, session: &str, update: &crate::harness::core::Update) {
    let critical = matches!(update, crate::harness::core::Update::Open { .. } | crate::harness::core::Update::MailFlow { .. });
    for json in harness_frames(session, update) {
        let message = AgentMsg::Harness { session: session.to_owned(), json };
        own.0.send(if critical { Up::Msg(message) } else { Up::Term(message) }).await.ok();
    }
}

async fn request_mail_replay(replay: &mut MailReplay, host: Option<&Arc<crate::harness::host::Host>>, own: &Uplink, raw: bool) {
    if raw && let Some(host) = host {
        // Legacy transport has no ordered session stream. The current owner header must use
        // the same reliable control lane and precede its critical receipts, not race a TERM Open.
        for info in host.list() {
            let session = info.id.clone();
            raw_harness_update(own, &session, &crate::harness::core::Update::Open { info: Box::new(info), since: None }).await;
        }
    }
    replay.request(host);
}

/// [`flush_updates`] from the turn of the runner's loop that made them (goal 10, plan 01a1a87b2dae step 5): each frame
/// straight to the link (`send`, as `Ev::Up` sends a `Up::Term`) where nothing the loop said or took before still waits
/// to go (`behind` false: its own channel, `own`, empty, and nothing taken ahead of its turn). Through `own` each was
/// the next frame the loop sent anyway, a turn of the loop later: that turn's select, a dozen of its sources' waits
/// armed and withdrawn before it came to `own`. Where something waits (a custody record or an identity's ask said this
/// turn, a channel's answer, a job's word taken ahead), the frames go behind it on `own`, as before, so the link takes
/// everything in the order the loop said it: custody's records keep their place among the session's updates.
async fn flush_updates_now<F, Fut>(updates: &mut crate::wire::uplink::Updates, own: &Uplink, behind: bool, mut send: F)
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: Future<Output = ()>,
{
    if behind {
        flush_updates(updates, own).await;
        return;
    }
    for f in updates.poll(Instant::now()) {
        // What `Ev::Up` encodes of a `Up::Term`, and as it passes over one that can't be.
        if let Ok(data) = proto::encode(&AgentMsg::Wire(f)) {
            send(data).await;
        }
    }
}

/// A frame on the terminal lane, as `Ev::Up` sends a `Up::Term` on a connection: by a test build's delay line where it
/// has one (`crate::runner::chaos`), else to the server's peer.
async fn send_term(net: &net::NetHandle, delay: Option<&crate::runner::chaos::DelayLine>, data: Vec<u8>) {
    match delay {
        Some(d) => d.send(LANE_TERM, Kind::Term, data, false),
        None => {
            net.send(SERVER_PEER, LANE_TERM, Kind::Term, data).await;
        }
    }
}

/// Most of one agent-session update a frame carries. They go on the interactive lane, which
/// refuses frames past 1 MiB ([`net::Kiln`]); a resumed session's history can be several.
const HARNESS_FRAME: usize = crate::harness::state_view::FRAME;

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
    let room = proto::harness_json_room(session, HARNESS_FRAME);
    let stamp = format!(",\"taken_us\":{}", u64::MAX).len();
    if matches!(u, crate::harness::core::Update::State { .. })
        && let Ok(bytes) = crate::harness::state_view::encode(u, room.saturating_sub(stamp)) {
        return String::from_utf8(bytes).ok().into_iter().collect();
    }
    if let crate::harness::core::Update::Open { info, since } = u {
        let envelope = serde_json::to_vec(&serde_json::json!({"t":"open","info":null,"since":since})).map_or(HARNESS_FRAME, |v| v.len());
        return crate::harness::state_view::info_view(info, room.saturating_sub(envelope).saturating_sub(stamp)).ok()
            .and_then(|info| serde_json::to_string(&serde_json::json!({"t":"open","info":info,"since":since})).ok())
            .filter(|json| json.len().saturating_add(stamp) <= HARNESS_FRAME).into_iter().collect();
    }
    let crate::harness::core::Update::State { state } = u else { return legacy_harness_frames(session, u) };
    // The maximum stamp, rather than a guessed allowance: it is appended after encoding.
    let envelope = r#"{"t":"state","state":null}"#.len();
    let budget = room.saturating_sub(stamp).saturating_sub(envelope);
    let view = match crate::harness::state_view::view(state, budget) {
        Ok(view) => view,
        Err(error) => {
            tracing::warn!(session, ?error, "a bounded session view could not be encoded");
            return Vec::new();
        }
    };
    let Ok(projected) = view.into_state() else { return Vec::new() };
    serde_json::to_string(&serde_json::json!({"t":"state", "state":projected})).ok()
        .filter(|json| json.len().saturating_add(stamp) <= HARNESS_FRAME).into_iter().collect()
}

fn legacy_harness_frames(session: &str, u: &crate::harness::core::Update) -> Vec<String> {
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
    let out = crate::os::proc::output(cmd, crate::proc::VERSION_WITHIN).await?;
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
    term_tx: &crate::memory::queue::Sender<(String, PtyOut)>,
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
fn open_mirror(dir: &std::path::Path, up: &crate::memory::queue::Sender<Up>) -> Option<(crate::convo::store::Store, crate::convo::feed::Intake)> {
    let st = crate::convo::open_file(dir, crate::convo::MIRROR_FILE, crate::config::Conversations::default().upkeep())?;
    match crate::convo::feed::Intake::start(st.clone(), crate::convo::feed::Brings::AsItSays, None) {
        Ok((intake, out)) => {
            // What it takes signed for as it takes it (docs/ci.md W27-R08).
            intake.sign_with(Arc::new(crate::runner::custody::Feed));
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

    /// A session with no process running (idle, its harness exited) is handed to the new build as
    /// its record alone, and the new build takes it from that record: refused for want of a frozen
    /// timeline, it was left with neither runner process (Amphora, 2026-11-11).
    #[cfg(unix)]
    #[test]
    fn a_blue_green_hand_over_takes_a_session_with_no_process_from_its_record() -> anyhow::Result<()> {
        use crate::harness::host::{Host, Limits};
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let spec = crate::harness::session::Spec { kind: "claude".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), program: Some(dir.path().join("no-such-claude").to_string_lossy().into_owned()), ..Default::default() };
            let (tx, _rx) = async_channel::unbounded();
            let old = Host::new(tx, None, Limits::default());
            old.open(Some("idle".into()), spec)?;
            let out = old.move_out_signing(true, None).await.map_err(anyhow::Error::msg)?;
            let (tx, _rx) = async_channel::unbounded();
            let new = Host::new(tx, None, Limits::default());
            for (rec, handed, hand_on) in out {
                anyhow::ensure!(handed.is_none(), "no process ran");
                let id = rec.id().to_owned();
                take_session(Some(&new), &id, rec, handed, hand_on.map(|b| b.to_string()), Vec::new()).await.map_err(anyhow::Error::msg)?;
            }
            anyhow::ensure!(new.get("idle").is_some(), "the new build holds it");
            Ok(())
        })
    }

    #[test]
    fn an_ordinary_state_keeps_its_exact_existing_wire_representation() -> anyhow::Result<()> {
        let update = crate::harness::core::Update::State { state: Box::default() };
        assert_eq!(harness_frames("ordinary-state", &update), vec![serde_json::to_string(&update)?]);
        Ok(())
    }

    /// CP-T3.2: Shards' queued messages must not prevent its owner from publishing its status.
    #[test]
    fn state_delivery_survives_a_queue_larger_than_a_frame() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{QueueItem, State, Status};
        let state = State {
            status: Status::Running,
            queue: (0..600).map(|i| QueueItem {
                id: format!("queued-{i}"), text: "same authoritative message ".repeat(100), at: i,
                attachments: Vec::new(), overrides: None, reply_to: None,
            }).collect(),
            ..State::default()
        };
        let update = Update::State { state: Box::new(state.clone()) };
        let frames = harness_frames("offline-regression", &update);
        anyhow::ensure!(!frames.is_empty(), "an online session's state was dropped because its queue exceeded the frame cap");
        anyhow::ensure!(frames.iter().all(|f| f.len() <= HARNESS_FRAME), "the frame cap must remain enforced");
        let first: serde_json::Value = serde_json::from_str(frames.first().ok_or_else(|| anyhow::anyhow!("no state frame"))?)?;
        anyhow::ensure!(first.pointer("/state/status").and_then(serde_json::Value::as_str) == Some("running"));
        anyhow::ensure!(first.pointer("/state/view/deferred/queue").is_some(), "a deferred queue must never look empty or contain clipped messages");
        anyhow::ensure!(matches!(&update, Update::State { state: original } if **original == state), "publishing a view must preserve the owner's queue");
        let recorded = crate::convo::rows::of("offline-regression", &update, 0);
        let stored = recorded.iter().find_map(|row| match row {
            crate::convo::rows::Row::Session { state: Some(state), .. } => Some(state), _ => None,
        }).ok_or_else(|| anyhow::anyhow!("no durable state row"))?;
        let restored: State = serde_json::from_str(stored)?;
        anyhow::ensure!(restored.queue == state.queue, "the durable source state must retain every complete message");
        Ok(())
    }

    /// Goal 9: an idle runner, connected, wakes for nothing. A server runs here with its own runner
    /// (the local agent, this process's `run`); once it's connected and settled, the timers armed in
    /// its loop and its leases wake no shard over a quiet span longer than its old periods (samples
    /// and the watchdog every 5 s, heartbeats and renewals every 10 s, the leases' check twice a
    /// second). Planted: any of those tickers back in its loop wakes it here. Not run by its author
    /// (the lead runs it).
    #[test]
    fn g9_an_idle_runner_wakes_for_nothing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let http = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        let quic = std::net::UdpSocket::bind("127.0.0.1:0")?.local_addr()?.port();
        let mut cfg = crate::config::Config::default();
        cfg.server.http_addr = format!("127.0.0.1:{http}");
        cfg.server.quic_addr = format!("127.0.0.1:{quic}");
        cfg.server.public_url = format!("http://127.0.0.1:{http}");
        cfg.server.quic_advertise = cfg.server.quic_addr.clone();
        cfg.server.data_dir = dir.path().to_path_buf();
        cfg.server.https = false;
        cfg.repos.clear();
        cfg.gitops.orgs.clear();
        cfg.secrets.clear();
        cfg.local_agent.enabled = true;
        cfg.local_agent.slots = 1;
        cfg.local_agent.work_dir = Some(dir.path().join("work"));
        cfg.kubernetes.enabled = false;
        cfg.terminal.enabled = false;
        cfg.discovery.enabled = false;
        cfg.discovery.announce = false;
        cfg.discovery.browse = false;
        cfg.discovery.join_requests = false;
        cfg.observe.enabled = false;
        let data = dir.path().to_path_buf();
        // Each bound only ends a hang: a loaded machine takes what time it takes.
        let until = Instant::now() + Duration::from_secs(300);
        crate::rt::block_on(async move {
            let shutdown = crate::rt::Shutdown::new();
            drop(crate::rt::spawn(crate::server::run(cfg, shutdown.clone())));
            let client = crate::http::client::Client::public()?;
            let connected = async || -> bool {
                let token = std::fs::read_to_string(data.join("admin.token")).unwrap_or_default();
                let Ok(r) = client.get(&format!("http://127.0.0.1:{http}/api/agents")).bearer(token.trim()).send().await else { return false };
                let v: serde_json::Value = r.json(1 << 20).await.unwrap_or_default();
                let list = v.as_array().cloned().or_else(|| v.get("agents").and_then(serde_json::Value::as_array).cloned()).unwrap_or_default();
                list.iter().any(|a| a.get("connected").and_then(serde_json::Value::as_bool) == Some(true))
            };
            while !connected().await {
                anyhow::ensure!(Instant::now() < until, "the local runner never connected");
                crate::rt::sleep(Duration::from_millis(200)).await;
            }
            // Settled: past its first look (WATCHDOG_SETTLE) and its link's first `Have`s (HAVE_SOON).
            crate::rt::sleep(WATCHDOG_SETTLE + HAVE_SOON * (HAVES_SOON + 1)).await;
            let files = ["src/agent.rs", "runner/lease.rs"];
            let before: Vec<u64> = files.iter().map(|f| crate::rt::timer_wakes_in(f)).collect();
            let woke = crate::rt::timer_wakes_over("src/agent.rs", Duration::from_secs(11)).await;
            let after: Vec<u64> = files.iter().map(|f| crate::rt::timer_wakes_in(f)).collect();
            shutdown.cancel();
            for ((file, b), a) in files.iter().zip(&before).zip(&after) {
                anyhow::ensure!(a == b, "{file}: {} timer wakes in 11 s, idle", a.saturating_sub(*b));
            }
            anyhow::ensure!(woke == 0, "src/agent.rs: {woke} timer wakes in 11 s, idle");
            Ok(())
        })
    }

    /// Goal 9: an older server's heartbeat goes as what it says changes, and while legs run at that
    /// server's own cadence (OLD_BEAT); idle, never.
    #[test]
    fn g9_an_older_servers_heartbeat_is_due_on_change_and_while_legs_run() {
        let mut jobs: HashMap<String, Running> = HashMap::new();
        assert!(beat_due(&None, 2, false, &jobs).is_some_and(|t| t <= Instant::now()), "never said: now");
        let said = Some((2, Vec::new(), Instant::now()));
        assert_eq!(beat_due(&said, 2, false, &jobs), None, "idle and said: nothing due");
        assert!(beat_due(&said, 2, true, &jobs).is_some_and(|t| t <= Instant::now()), "draining changes its free slots: now");
        jobs.insert("j".into(), Running { cancel: Shutdown::new(), kill: Shutdown::new(), pooled: false, holds: kiln_ci::sched::state::Incoming::Ordinary });
        assert!(beat_due(&said, 2, false, &jobs).is_some_and(|t| t <= Instant::now()), "a job started: now");
        let at = Instant::now();
        let said = Some((1, vec!["j".to_owned()], at));
        assert_eq!(beat_due(&said, 2, false, &jobs), at.checked_add(OLD_BEAT), "a leg running: at its cadence");
    }

    /// Goal 9: an identity is asked for again when its last ask's wait is over, and an ask whose
    /// session is gone waits no more.
    #[test]
    fn g9_an_identity_ask_is_due_at_its_wait_and_a_gone_sessions_is_let_go() {
        let at = Instant::now();
        let mut asked: HashMap<String, (Instant, Duration)> = HashMap::new();
        assert_eq!(identity_due(&asked), None);
        asked.insert(crate::runner::identity::slot("agent", Some("s1"), Some(1)), (at, Duration::from_secs(5)));
        asked.insert(crate::runner::identity::slot("agent", Some("s2"), Some(1)), (at, Duration::from_secs(20)));
        assert_eq!(identity_due(&asked), at.checked_add(Duration::from_secs(5)));
        let sessions: HashMap<String, u64> = [("s2".to_owned(), 1)].into();
        prune_identity_asks(&mut asked, &sessions);
        assert_eq!(identity_due(&asked), at.checked_add(Duration::from_secs(20)), "s1 is gone: its ask waits no more");
    }


    /// A runner handing its store to the new build lets go of it once what its sessions said is in,
    /// moves it into the new build's folder and says where; the new build goes on with it, the same
    /// store. A rollback moves it back, and the tee keeps what passes in it again.
    #[cfg(unix)]
    #[test]
    fn a_store_handed_to_the_new_build_goes_whole_and_comes_back_on_a_rollback() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event};
        let said = |id: &str, seq: u64, text: &str| Update::Events { events: vec![Event { id: id.into(), seq, at: 1_000 + seq as i64, parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } }] };
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let (blue, green) = (dir.path().join("blue"), dir.path().join("green"));
            std::fs::create_dir_all(&blue)?;
            let upkeep = || crate::config::Conversations::default().upkeep();
            let texts = |st: &crate::convo::store::Store| -> anyhow::Result<Vec<String>> {
                st.sync()?;
                Ok(st.kept("a1", 10)?.map(|k| k.events.into_iter().map(|e| e.text).collect()).unwrap_or_default())
            };
            let st = crate::convo::open(&blue, upkeep()).ok_or_else(|| anyhow::anyhow!("no store"))?;
            let (out, passed) = async_channel::unbounded();
            let (sink, held) = crate::convo::store::tee_held(out, Some(st.clone()));
            sink.send(("a1".to_owned(), said("m1", 1, "before the upgrade"))).await.map_err(|_| anyhow::anyhow!("closed"))?;
            crate::rt::timeout(Duration::from_secs(5), passed.recv()).await??;
            let id = st.id().to_owned();
            let moves = vec![(blue.join(crate::convo::FILE), green.join(crate::convo::FILE))];
            let (h, m) = (held.clone(), moves.clone());
            let frame = crate::rt::blocking(move || give_stores(&h, Some(st), None, &m)).await?;
            let to = green.join(crate::convo::FILE).to_string_lossy().into_owned();
            assert!(matches!(&frame, crate::bluegreen::Frame::Stores { own: Some(o), mirror: None } if *o == to));
            assert!(!blue.join(crate::convo::FILE).exists());
            // The new build opens it where it was moved: the same store, what it held in it.
            let there = crate::convo::open(&green, upkeep()).ok_or_else(|| anyhow::anyhow!("no store there"))?;
            assert_eq!(there.id(), id);
            assert_eq!(texts(&there)?, vec!["before the upgrade".to_owned()]);
            there.close()?;
            // Rolled back: moved back, opened again, given to the tee, which keeps what passes in it.
            let g = Given { held: held.clone(), moves, own_dir: blue.clone(), aside: Some([None, None]) };
            let mut problems = Vec::new();
            let back = take_stores_back(&g, &mut problems).ok_or_else(|| anyhow::anyhow!("nothing came back"))?;
            assert!(problems.is_empty(), "{problems:?}");
            assert_eq!(back.id(), id);
            sink.send(("a1".to_owned(), said("m2", 2, "after the rollback"))).await.map_err(|_| anyhow::anyhow!("closed"))?;
            crate::rt::timeout(Duration::from_secs(5), passed.recv()).await??;
            assert_eq!(texts(&back)?, vec!["before the upgrade".to_owned(), "after the rollback".to_owned()]);
            Ok(())
        })
    }

    /// A runner hands its store to the new build however long its disk takes to let go of it. Here
    /// the store's sync takes half again as long as any read waits for its writer (`READ_WAIT`,
    /// which bounded the close too: past it the store was left out as one that couldn't be let go
    /// of, and the new build opened a fresh store while this one was still syncing, as a disk that
    /// held the sync 11 s made it). The handover waits for the sync, then names the store moved, and
    /// the new build goes on with it: the same store, what it held in it.
    #[cfg(unix)]
    #[test]
    fn a_store_whose_sync_outlasts_any_reads_wait_is_still_handed_to_the_new_build_whole() -> anyhow::Result<()> {
        use crate::convo::store::{READ_WAIT, SlowToLetGo, Store, Upkeep};
        let said = |id: &str, seq: u64, text: &str| Update::Events { events: vec![Event { id: id.into(), seq, at: 1_000 + seq as i64, parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } }] };
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let (blue, green) = (dir.path().join("blue"), dir.path().join("green"));
            std::fs::create_dir_all(&blue)?;
            let hold = READ_WAIT + READ_WAIT / 2;
            let engine = crate::convo::turso::Turso::open(&blue.join(crate::convo::FILE))?;
            let st = Store::start(Box::new(SlowToLetGo { inner: Box::new(engine), hold }), Upkeep::default())?;
            st.send("a1", &said("m1", 1, "before the upgrade"));
            let id = st.id().to_owned();
            let (out, _passed) = async_channel::unbounded();
            let (_sink, held) = crate::convo::store::tee_held(out, Some(st.clone()));
            let moves = vec![(blue.join(crate::convo::FILE), green.join(crate::convo::FILE))];
            let began = Instant::now();
            let frame = crate::rt::blocking(move || give_stores(&held, Some(st), None, &moves)).await?;
            let took = began.elapsed();
            let to = green.join(crate::convo::FILE).to_string_lossy().into_owned();
            let named = match &frame {
                crate::bluegreen::Frame::Stores { own, mirror: None } => own.clone(),
                _ => None,
            };
            anyhow::ensure!(named.as_deref() == Some(to.as_str()), "a sync of {hold:?} left the store out of the handover after {took:?}: it named {named:?}, not {to}");
            anyhow::ensure!(took >= hold, "handed over after {took:?}, before its sync of {hold:?} was done");
            anyhow::ensure!(!blue.join(crate::convo::FILE).exists(), "the store stayed in the runner's own folder");
            // Let go of whole: another process takes its file's lock at once, so the new build opens it
            // once, with nothing to wait for (`crate::convo::open_or_set_aside`).
            let free = std::process::Command::new("python3")
                .args(["-c", "import fcntl, sys; fcntl.lockf(open(sys.argv[1], 'r+'), fcntl.LOCK_EX | fcntl.LOCK_NB)", &to])
                .status();
            match free {
                Ok(free) => anyhow::ensure!(free.success(), "the store handed over is still held"),
                Err(_) => eprintln!("its lock not looked at: no python3"),
            }
            // The new build opens it where it was moved: the same store, what it held in it.
            let there = crate::convo::open(&green, crate::config::Conversations::default().upkeep()).ok_or_else(|| anyhow::anyhow!("no store there"))?;
            anyhow::ensure!(there.id() == id, "the new build went on in another store");
            there.sync()?;
            let texts = there.kept("a1", 10)?.map(|k| k.events.into_iter().map(|e| e.text).collect::<Vec<_>>());
            anyhow::ensure!(texts == Some(vec!["before the upgrade".to_owned()]), "what it held didn't come with it: {texts:?}");
            Ok(())
        })
    }

    /// A store the new build couldn't open it sets aside there, kept whole, and goes on in a new
    /// one; rolled back, the runner being upgraded takes its own back as it was, from where the new
    /// build said it set it aside (or, its answer lost, under the first name), not the new build's.
    #[cfg(unix)]
    #[test]
    fn a_store_the_new_build_set_aside_comes_back_as_it_was_on_a_rollback() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event};
        let said = |id: &str, seq: u64, text: &str| Update::Events { events: vec![Event { id: id.into(), seq, at: 1_000 + seq as i64, parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } }] };
        crate::rt::block_on(async move {
            for answered in [true, false] {
                let dir = tempfile::tempdir()?;
                let (blue, green) = (dir.path().join("blue"), dir.path().join("green"));
                std::fs::create_dir_all(&blue)?;
                let upkeep = || crate::config::Conversations::default().upkeep();
                let st = crate::convo::open(&blue, upkeep()).ok_or_else(|| anyhow::anyhow!("no store"))?;
                st.send("a1", &said("m1", 1, "before the upgrade"));
                let id = st.id().to_owned();
                let (out, _passed) = async_channel::unbounded();
                let (_sink, held) = crate::convo::store::tee_held(out, Some(st.clone()));
                let moves = vec![(blue.join(crate::convo::FILE), green.join(crate::convo::FILE))];
                let (h, m) = (held.clone(), moves.clone());
                crate::rt::blocking(move || give_stores(&h, Some(st), None, &m)).await?;
                // The new build couldn't open it: set aside there, and a new store in its place.
                let aside = crate::convo::set_aside(&green.join(crate::convo::FILE))?;
                let fresh = crate::convo::open(&green, upkeep()).ok_or_else(|| anyhow::anyhow!("no new store"))?;
                assert_ne!(fresh.id(), id.as_str());
                fresh.close()?;
                let answer = answered.then(|| [Some(aside.clone()), None]);
                let mut problems = Vec::new();
                let back = take_stores_back(&Given { held, moves, own_dir: blue.clone(), aside: answer }, &mut problems).ok_or_else(|| anyhow::anyhow!("nothing came back"))?;
                assert!(problems.is_empty(), "{problems:?}");
                assert_eq!(back.id(), id, "its own store, as it was (answered: {answered})");
                assert!(!aside.exists());
                back.sync()?;
                assert_eq!(back.kept("a1", 10)?.map(|k| k.events.len()), Some(1));
            }
            Ok(())
        })
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
        let size = Some(Resources { cpu: kiln_ci::sched::state::Cpus::whole(32), mem: 64 << 30, ..Resources::default() });
        let bench = Incoming::Share(Resources { cpu: kiln_ci::sched::state::Cpus::whole(16), mem: 16 << 30, ..Resources::default() });
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
        // On a runner whose size isn't known a share holds it whole (W05-R03, docs/sim.md's bug 12).
        assert_eq!(admits(4, None, std::iter::empty(), false, bench), Ok(()));
        assert_eq!(admits(4, None, [(false, bench)].into_iter(), false, o), Err("no free slot beside what's held"));
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
                    hub.harness.ingest(&there, "s", serde_json::from_str(&json)?).await;
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

    /// fedora, 2026-10-25 14:18 (W27-R08; the Transfers plan's step 4, which this fix's
    /// fix/upgrade-keeps-agents 7940fcafec, unsigned, came before): a blue/green upgrade's new build
    /// let go of six agent sessions it had just taken over. The server, still counting the old
    /// runner as their holder, asked it to hand them to the newest (a local MoveTo), then told it
    /// to let go (HandedOver); it passed both on, and the new build did as they said with its own
    /// copy. A hand-over's word is for the process it's sent to: it stays with the old runner, and
    /// the new build refuses one passed on anyway (the next upgrade's old runner may run a build
    /// from before this). Everything else about a session handed over goes on.
    #[cfg(unix)]
    #[test]
    fn w27_ac05_a_hand_overs_word_for_the_runner_being_upgraded_never_reaches_the_new_build() -> anyhow::Result<()> {
        use crate::handovers::Word;
        use crate::harness::model::{Command, SendMode};
        use crate::instances::Owned;
        use base64::Engine as _;
        let cmd = |c: &Command| -> anyhow::Result<ServerMsg> { Ok(ServerMsg::HarnessCmd { session: "s1".into(), json: serde_json::to_string(c)? }) };
        let send = Command::Send { id: "m1".into(), text: "go on".into(), mode: SendMode::Queue, attachments: vec![], overrides: None, reply_to: None, task: None };
        let moved: std::collections::HashSet<Owned> = [Owned::Agent("s1".into())].into_iter().collect();
        let me = Me { machine: "box".into(), instance: "01a17502".into(), root: Vec::new() };
        let mut words = vec![("a local MoveTo".to_owned(), cmd(&Command::MoveTo { to: "box".into(), cwd: String::new(), note: None, now: false, kind: None, model: None, local: true })?)];
        for w in [Word::Taken, Word::Refused("no room".into()), Word::NotPassed("x".into()), Word::Unsure("x".into()), Word::NoneNewer("x".into())] {
            words.push((format!("HandedOver ({w:?})"), cmd(&w.command())?));
        }
        let on = [
            ("a person's message", cmd(&send)?),
            ("a message for its epoch", ServerMsg::HarnessSend { session: "s1".into(), epoch: 2, json: serde_json::to_string(&send)? }),
            ("an interrupt", cmd(&Command::Interrupt)?),
            ("a move to another machine", cmd(&Command::MoveTo { to: "other".into(), cwd: "~/w".into(), note: None, now: false, kind: None, model: None, local: false })?),
            ("a close", ServerMsg::HarnessClose { session: "s1".into() }),
        ];
        let b64 = |m: &ServerMsg| -> anyhow::Result<String> { Ok(base64::engine::general_purpose::STANDARD.encode(proto::encode(m)?)) };
        let mut wrong = vec![];
        for (what, m) in &words {
            if [false, true].into_iter().any(|leaving| goes_on(m, &moved, leaving).is_some()) {
                wrong.push(format!("{what} went on to the new build"));
            }
            if passed_on(&b64(m)?, None, &me).is_some() {
                wrong.push(format!("the new build took {what} passed on"));
            }
        }
        for (what, m) in &on {
            if goes_on(m, &moved, false) != Some(Owned::Agent("s1".into())) {
                wrong.push(format!("{what} stayed with the runner being upgraded"));
            }
            if passed_on(&b64(m)?, None, &me) != Some(proto::encode(m)?) {
                wrong.push(format!("the new build dropped {what} passed on"));
            }
        }
        anyhow::ensure!(wrong.is_empty(), "{}", wrong.join("; "));
        // Asked to hand over a session it handed to the new build, the old runner refuses: it
        // doesn't say the session closed, as it would of one gone here.
        anyhow::ensure!(!gone_here("s1", &moved), "a hand-over's word for a session handed to the new build said it closed");
        anyhow::ensure!(gone_here("s2", &moved));
        // As before: a frame about something this runner kept stays, and a session opened while it
        // hands everything over goes on.
        let open = ServerMsg::HarnessOpen { session: "s2".into(), json: "{}".into() };
        anyhow::ensure!(goes_on(&ServerMsg::HarnessClose { session: "s2".into() }, &moved, true).is_none() && goes_on(&open, &moved, false).is_none());
        anyhow::ensure!(goes_on(&open, &moved, true) == Some(Owned::Agent("s2".into())));
        anyhow::ensure!(passed_on("not base64!", None, &me).is_none());
        Ok(())
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
        let bytes = push_update(&mut updates, "s", &history, true, &mut sent, None);
        assert!(bytes > crate::wire::uplink::ROOM, "{bytes} bytes");
        push_update(&mut updates, "s", &Update::Events { events: vec![said(5000, "after".into())] }, false, &mut sent, None);
        assert!(updates.take_stale().is_empty(), "its picture went whole");
        assert_eq!(updates.unanswered(), frames + 1, "the snapshot and what came after it");
        // As it was pushed before: held to the room, it goes stale part-way.
        let other = crate::wire::uplink::session_key("t");
        updates.start(&other);
        push_update(&mut updates, "t", &history, false, &mut sent, None);
        assert_eq!(updates.take_stale(), vec![other]);
    }

    /// Goal 10, plan 01a1a87b2dae step 5: an update this runner took in goes out saying how long it held it, in every
    /// frame of it (a long one goes in parts), at least the time since its session handed it on, and each frame reads
    /// as its part of the update once that's read off. A stream's snapshot, the session as it stands, says none.
    #[test]
    fn g10_s5_an_update_taken_in_says_how_long_the_runner_held_it() -> anyhow::Result<()> {
        let mut updates = crate::wire::uplink::Updates::new("boot");
        // The link up and the server's word in (it has nothing yet): what's pushed goes at the next poll.
        updates.on_frame(crate::wire::channel::Frame::Have(Vec::new()));
        let mut sent = SentChanges::default();
        updates.start(&crate::wire::uplink::session_key("s"));
        push_update(&mut updates, "s", &Update::Events { events: vec![said(1, "as it stands".into())] }, true, &mut sent, None);
        let long = Update::Events { events: (2..2000).map(|i| said(i, "x".repeat(2000))).collect() };
        let parts = harness_frames("s", &long).len();
        anyhow::ensure!(parts > 1, "{parts} part");
        let held_for = Duration::from_millis(3);
        let taken = Instant::now().checked_sub(held_for).ok_or_else(|| anyhow::anyhow!("no time 3 ms ago"))?;
        push_update(&mut updates, "s", &long, false, &mut sent, Some(taken));
        let mut said_held = Vec::new();
        for f in updates.poll(Instant::now()) {
            let crate::wire::channel::Frame::Item { body, .. } = f else { continue };
            let Ok(AgentMsg::Harness { mut json, .. }) = proto::read_agent(&body) else { anyhow::bail!("not a session's update") };
            said_held.push(crate::wire::stamp::take(&mut json));
            anyhow::ensure!(matches!(serde_json::from_str::<Update>(&json), Ok(Update::Events { .. })), "its part of the update, once read off");
        }
        anyhow::ensure!(said_held.len() == 1 + parts, "{} frames", said_held.len());
        anyhow::ensure!(said_held.first() == Some(&None), "the snapshot says none");
        anyhow::ensure!(said_held.iter().skip(1).all(|h| h.is_some_and(|d| d >= held_for)), "each part at least {held_for:?}: {said_held:?}");
        Ok(())
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a turn of the runner's loop sends the channel's frames straight to the link where
    /// nothing it said or took before waits to go, in the channel's order, as `Ev::Up` would have sent them a turn later
    /// (the same bytes), and puts nothing on its own channel; where something waits there (a custody record said this
    /// turn), the frames go behind it on that channel, none straight on, so the link takes the record first.
    #[test]
    fn g10_s5_a_turn_sends_its_frames_straight_on_where_nothing_waits_before_them() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut updates = crate::wire::uplink::Updates::new("boot");
            // The link up and the server's word in (it has nothing yet): what's pushed goes at the next poll.
            updates.on_frame(crate::wire::channel::Frame::Have(Vec::new()));
            let mut sent = SentChanges::default();
            updates.start(&crate::wire::uplink::session_key("s"));
            let one = |n: usize| Update::Events { events: vec![said(n, format!("event {n}"))] };
            push_update(&mut updates, "s", &one(1), true, &mut sent, None);
            push_update(&mut updates, "s", &one(2), false, &mut sent, Some(Instant::now()));
            let (own_tx, own_rx) = crate::memory::queue::unbounded::<Up>(&crate::memory::owners::CHANNELS);
            let own = Uplink(own_tx);
            // What `Ev::Up` sends of a `Up::Term`: the channel's items, in order.
            let item = |data: &[u8]| -> anyhow::Result<u64> {
                match proto::read_agent(data) {
                    Ok(AgentMsg::Wire(crate::wire::channel::Frame::Item { seq, .. })) => Ok(seq),
                    other => anyhow::bail!("not a channel's item: {other:?}"),
                }
            };
            let mut linked: Vec<Vec<u8>> = Vec::new();
            flush_updates_now(&mut updates, &own, false, |data| {
                linked.push(data);
                async {}
            })
            .await;
            let seqs = linked.iter().map(|d| item(d)).collect::<anyhow::Result<Vec<u64>>>()?;
            anyhow::ensure!(seqs.len() == 2 && seqs.windows(2).all(|w| w.first() < w.get(1)), "both straight on, in the channel's order: {seqs:?}");
            anyhow::ensure!(own_rx.is_empty(), "nothing on the loop's own channel");
            // A custody record said this turn waits on the loop's own channel: the next frame goes behind it there.
            own.0.send(Up::Term(AgentMsg::Custody { json: "{\"t\":\"custody\"}".into() })).await.map_err(|_| anyhow::anyhow!("closed"))?;
            push_update(&mut updates, "s", &one(3), false, &mut sent, Some(Instant::now()));
            let before = linked.len();
            flush_updates_now(&mut updates, &own, !own_rx.is_empty(), |data| {
                linked.push(data);
                async {}
            })
            .await;
            anyhow::ensure!(linked.len() == before, "none straight on past the record");
            anyhow::ensure!(matches!(own_rx.try_recv(), Ok(Up::Term(AgentMsg::Custody { .. }))), "the record first");
            match own_rx.try_recv() {
                Ok(Up::Term(m @ AgentMsg::Wire(_))) => anyhow::ensure!(seqs.last().is_some_and(|last| item(&proto::encode(&m).unwrap_or_default()).is_ok_and(|s| s > *last)), "the next item, after the record"),
                other => anyhow::bail!("not the frame behind the record: {other:?}"),
            }
            anyhow::ensure!(own_rx.is_empty(), "nothing else");
            Ok(())
        })
    }

    /// A file of a changeset with `lines` added lines, each about as long as a line of code.
    fn change(path: &str, lines: usize) -> crate::harness::model::FileChange {
        let n = u32::try_from(lines).unwrap_or(u32::MAX);
        let lines = (0..lines).map(|i| format!("+    let value_{i} = compute(argument_{i}, &context); // line {i}")).collect();
        crate::harness::model::FileChange { path: path.into(), tree: None, created: false, deleted: false, added: n, removed: 0, hunks: vec![crate::harness::model::Hunk { old_start: 1, old_lines: 0, new_start: 1, new_lines: n, lines }], truncated: false, at: 1, by: Vec::new(), place: None }
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
            hub.harness.ingest(&there, "s", Update::Open { info: Box::new(info.clone()), since: None }).await;
            let mut sent = SentChanges::default();
            sent.server(true);
            let take = async |frames: &[String]| -> anyhow::Result<()> {
                for f in frames {
                    anyhow::ensure!(f.len() <= HARNESS_FRAME, "a frame of {} bytes", f.len());
                    hub.harness.ingest(&there, "s", serde_json::from_str(f)?).await;
                }
                Ok(())
            };
            let copy = || hub.harness.changes_value("s");
            let mut files = long_changeset();
            let frames = sent.frames("s", &files, true);
            anyhow::ensure!(frames.len() > 1 && frames.first().is_some_and(|f| f.starts_with(r#"{"t":"changes""#)));
            take(&frames).await?;
            anyhow::ensure!(copy() == serde_json::to_value(&files)?, "the picture, whole, every diff kept");
            // The agent edits one file of 43: one small frame, that file.
            if let Some(f) = files.get_mut(20) {
                *f = change(&f.path, 300);
            }
            let frames = sent.frames("s", &files, false);
            let [edit] = frames.as_slice() else { anyhow::bail!("{} frames", frames.len()) };
            anyhow::ensure!(edit.starts_with(r#"{"t":"changes_patch""#) && edit.len() < 32 << 10, "{} bytes", edit.len());
            take(&frames).await?;
            anyhow::ensure!(copy() == serde_json::to_value(&files)?, "the edit taken in place");
            // It puts one back: that file's key alone.
            let back = files.remove(5);
            let frames = sent.frames("s", &files, false);
            anyhow::ensure!(frames == vec![serde_json::to_string(&Update::ChangesPatch { set: Vec::new(), gone: vec![back.key()] })?]);
            take(&frames).await?;
            anyhow::ensure!(copy() == serde_json::to_value(&files)?, "the file put back gone");
            anyhow::ensure!(sent.frames("s", &files, false).is_empty(), "nothing changed, nothing sent");
            // The copy starts over (an Open on the stream): every file again.
            let mut updates = crate::wire::uplink::Updates::new("boot");
            updates.start(&crate::wire::uplink::session_key("s"));
            push_update(&mut updates, "s", &Update::Open { info: Box::new(info), since: None }, true, &mut sent, None);
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

#[cfg(test)]
mod attention_archive_ack_tests{
    use super::*;
    /// Plan 01a199e1b177752e84dad5dc990e6a15: a deferred operation keeps its
    /// transport identity until confirmation, while the loop can deliver its Exit.
    #[test]
    fn mc_attention_archive_ack_waits_for_exit_without_blocking_delivery()->anyhow::Result<()>{
        crate::rt::block_on(async{
            use crate::wire::channel::{Arrival,Receiver,Refusal};
            for outcome in [Ok(()),Err((Refusal::Unknown,"the owned run did not confirm its stop".to_owned())),Err((Refusal::Refused,"fresh work prevented automatic archive".to_owned()))]{
                let mut wire=Receiver::timed();let now=Instant::now();
                assert!(matches!(wire.on_item("archive",1,0,now),Arrival::Take(_)));
                let(exit,heard)=futures::channel::oneshot::channel::<Result<(),(Refusal,String)>>();
                let mut operations=HarnessOperations::new();
                operations.push(async move{
                    let result=heard.await.unwrap_or_else(|_|Err((Refusal::Unknown,"the runner stopped delivering exits".into())));
                    HarnessApplied::Command { reply: Some(("archive".into(),1)), result }
                }.boxed());
                assert!(next_harness_operation(&mut operations).now_or_never().is_none());
                assert_eq!(wire.on_item("archive",1,0,now),Arrival::Answered(None),"a retry remains pending and does not run twice");
                exit.send(outcome.clone()).map_err(|_|anyhow::anyhow!("the deferred operation was abandoned"))?;
                let HarnessApplied::Command { reply, result } = next_harness_operation(&mut operations).await else { anyhow::bail!("not the pending archive operation") };
                let(stream,seq)=reply.ok_or_else(||anyhow::anyhow!("lost the command identity"))?;
                assert_eq!(result,outcome);
                let answer=match result{Ok(())=>wire.took(&stream,seq),Err((code,why))=>wire.refused(&stream,seq,code,why)};
                assert_eq!(wire.on_item("archive",1,0,now),Arrival::Answered(Some(answer)));
            }Ok(())
        })
    }
}

#[cfg(test)]
mod native_handover_tests {
    use super::*;

    /// W27-R08: Open's first identity ask carries the committed 0→1 receipt. Promotion must
    /// never schedule epoch 2 for a native journal that already changed ownership to epoch 1.
    #[test]
    #[cfg(unix)]
    fn a_native_journal_registers_its_one_epoch_receipt_before_the_first_open_ask() -> anyhow::Result<()> {
        use base64::Engine as _;
        use kiln_ci::identity::{chain::{Actor, Holder, Identity}, custody::{Body, Kind}, statement::{self, PAYLOAD_CUSTODY, Statement}};
        use rcgen::SigningKey as _;
        let directory = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open_scratch(&directory.path().join("pki"))?;
        let session = format!("native-epoch-{}", crate::util::new_id());
        let manifest = crate::harness::mail::TransferManifest { attempt: crate::util::new_id(), session: session.clone(), source_epoch: 0, destination_epoch: 1, count: 1, sha256: "a".repeat(64) };
        let frozen = crate::harness::core::Core::new().freeze();
        let (epoch, events) = frozen.handover_source();
        let identity = Identity::new(pki.organization(), Actor::Agent { person: "ada".into(), agent: session.clone(), epoch, held_by: Some(Holder { machine: "native-box".into(), instance: "old".into() }) });
        let by = identity.uri()?;
        let key = crate::pki::new_key_pem()?;
        let cert = pki.issue_identity(&crate::pki::make_csr_with_key("kiln-identity", &key)?, &identity, &[], time::Duration::hours(1))?;
        let signing = rcgen::KeyPair::from_pem(&key)?;
        let subject = crate::custody::timeline(&session, epoch, events);
        let sign = |epoch: u64| -> anyhow::Result<serde_json::Value> {
            let body = Body { to: crate::custody::at_epoch(&by, epoch), epoch: Some(epoch), ..Body::new(Kind::HandedOn, &by) };
            let payload = statement::payload(&Statement { subject: vec![subject.clone()], ..Statement::new(serde_json::to_value(body)?) })?;
            Ok(statement::bundle(PAYLOAD_CUSTODY, &payload, &signing.sign(&statement::to_sign(PAYLOAD_CUSTODY, &payload))?, &crate::pki::der_of(&cert)?))
        };
        let hand_on = sign(1)?;
        assert!(native_journal_hand_on(&manifest, None, Some(&hand_on)).is_err(), "no invented empty timeline when native state is missing");
        assert!(native_journal_hand_on(&manifest, Some(&frozen), None).is_err(), "signed custody cannot silently become unsigned");
        assert!(native_journal_hand_on(&manifest, Some(&frozen), Some(&sign(2)?)).is_err(), "an epoch-2 statement cannot form the epoch-1 receipt");
        native_journal_receipt(&manifest, Some(&frozen), Some(&hand_on)).map_err(anyhow::Error::msg)?;
        let open_epoch = crate::runner::custody::to_ask(&session, manifest.destination_epoch);
        assert_eq!(open_epoch, Some(1), "the receipt is installed before the first Open asks for its identity");
        let ask = crate::runner::identity::request("agent", Some(&session), open_epoch)?;
        let ask = crate::runner::custody::carrying(&session, 1, ask);
        assert_eq!(ask.epoch, Some(1));
        assert_eq!(ask.hand_on.as_deref(), Some(hand_on.to_string().as_str()));
        let receipt = ask.receipt.ok_or_else(|| anyhow::anyhow!("Open would ask without its source custody receipt"))?;
        let payload = base64::engine::general_purpose::STANDARD.decode(&receipt.payload)?;
        let receipt: Statement = serde_json::from_slice(&payload)?;
        let body: Body = serde_json::from_value(receipt.body)?;
        assert_eq!(body.epoch, Some(1));
        assert_eq!(receipt.subject, vec![subject]);
        assert_eq!(crate::runner::custody::certified(&session, 2), crate::runner::custody::Certification::Untracked, "no second-epoch identity transition exists");
        assert_eq!(crate::runner::custody::certified(&session, 1), crate::runner::custody::Certification::Receipt, "the committed journal epoch needs no blue-green go_on_at increment");
        let proof = crate::runner::custody::LegacyProof::of(&session, 0, events, Some(&hand_on)).map_err(anyhow::Error::msg)?;
        let proof: crate::runner::custody::LegacyProof = serde_json::from_slice(&serde_json::to_vec(&proof)?)?;
        proof.validate(&session, 0, 1).map_err(anyhow::Error::msg)?;
        assert!(proof.validate("another-session", 0, 1).is_err());
        assert!(proof.validate(&session, 1, 2).is_err());
        let mut promotion = crate::harness::inbox::Promotion { from_epoch: 0, to_epoch: 1, phase: crate::harness::inbox::PromotionPhase::AwaitingPromotion, proof, after: None };
        crate::runner::custody::legacy_waiting(&session, &promotion).map_err(anyhow::Error::msg)?;
        assert_eq!(crate::runner::custody::to_ask(&session, 0), None, "persisted proof alone cannot authorize promotion");
        promotion.phase = crate::harness::inbox::PromotionPhase::AwaitingCertificate;
        crate::runner::custody::legacy_waiting(&session, &promotion).map_err(anyhow::Error::msg)?;
        assert_eq!(crate::runner::custody::to_ask(&session, 0), Some(1));
        assert_eq!(crate::runner::custody::certified(&session, 1), crate::runner::custody::Certification::DurablePromotion);
        assert_eq!(crate::runner::custody::certified(&session, 1), crate::runner::custody::Certification::DurablePromotion, "certificate notifications cannot retire the durable transition");
        let ask = crate::runner::custody::carrying(&session, 1, crate::runner::identity::request("agent", Some(&session), Some(1))?);
        assert!(ask.receipt.is_some() && ask.hand_on.is_some(), "persisted compact proof recreates the exact receipt without event reassembly");
        crate::runner::identity::forget_prefix(&format!("agent/{session}/"));
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn a_wrong_key_or_uninstalled_identity_cannot_complete_a_legacy_promotion() -> anyhow::Result<()> {
        use kiln_ci::identity::chain::{Actor, Holder, Identity};
        crate::rt::block_on(async {
            let directory = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open_scratch(&directory.path().join("pki"))?;
            let session = format!("legacy-key-{}", crate::util::new_id());
            let frozen = crate::harness::core::Core::new().freeze();
            let (_, events) = frozen.handover_source();
            let proof = crate::runner::custody::LegacyProof::of(&session, 0, events, None).map_err(anyhow::Error::msg)?;
            let promotion = crate::harness::inbox::Promotion { from_epoch: 0, to_epoch: 1, phase: crate::harness::inbox::PromotionPhase::AwaitingCertificate, proof, after: None };
            crate::runner::custody::legacy_waiting(&session, &promotion).map_err(anyhow::Error::msg)?;
            let _requested = crate::runner::identity::request("agent", Some(&session), Some(1))?;
            let identity = Identity::new(pki.organization(), Actor::Agent { person: "ada".into(), agent: session.clone(), epoch: 1, held_by: Some(Holder { machine: "box".into(), instance: "green".into() }) });
            let wrong_key = crate::pki::new_key_pem()?;
            let wrong_cert = pki.issue_identity(&crate::pki::make_csr_with_key("kiln-identity", &wrong_key)?, &identity, &[], time::Duration::hours(1))?;
            let given = crate::proto::IdentityGiven { kind: "agent".into(), of: Some(session.clone()), epoch: Some(1), uri: identity.uri()?, cert_pem: Some(wrong_cert), ..Default::default() };
            assert!(installed_identity(given.clone(), directory.path().to_path_buf(), "green".into()).await.is_err(), "a valid certificate for a different private key is not installed");
            let slot = crate::runner::identity::slot("agent", Some(&session), Some(1));
            assert!(crate::runner::identity::cert(&slot).is_none());
            assert_eq!(crate::runner::custody::certified(&session, 1), crate::runner::custody::Certification::DurablePromotion);
            crate::runner::identity::forget(&slot);
            assert_eq!(installed_identity(given, directory.path().to_path_buf(), "green".into()).await?, None, "given_off Ok for a vanished key is no installed certificate notification");
            assert_eq!(crate::runner::custody::to_ask(&session, 0), Some(1), "no certificate failure may retire the epoch witness or ask for epoch0");
            assert!(crate::runner::custody::legacy_certificate_ready(&session, 1, &promotion.proof).is_err());
            Ok(())
        })
    }

    #[test]
    #[cfg(unix)]
    fn native_ready_waits_for_actual_features_on_the_current_connection() {
        let pending = || Some(PathBuf::from("old-runner"));
        let mut path = pending();
        let mut features = FeatureNegotiation::default();
        assert_eq!(features.take_over(true, &mut path), None, "Welcome before Caps is not a negative capability");
        features.caps(proto::CAP_FEATURES);
        assert_eq!(features.take_over(true, &mut path), None, "named features still pending");
        features.names();
        assert_eq!(features.take_over(true, &mut path), pending());
        features.names();
        features.caps(proto::CAP_FEATURES);
        assert_eq!(features.take_over(true, &mut path), None, "duplicate handshake events cannot start a second owner");

        let mut path = pending();
        let mut features = FeatureNegotiation::default();
        features.names();
        assert_eq!(features.take_over(false, &mut path), None, "Features before Welcome does not start takeover");
        assert_eq!(features.take_over(true, &mut path), pending());

        let mut path = pending();
        let mut features = FeatureNegotiation::default();
        features.caps(0);
        assert_eq!(features, FeatureNegotiation::Legacy);
        assert_eq!(features.take_over(false, &mut path), None);
        assert_eq!(features.take_over(true, &mut path), pending(), "an actual legacy Caps reply settles the handshake without inventing mail support");

        let mut path = pending();
        let mut features = FeatureNegotiation::default();
        features.caps(proto::CAP_FEATURES);
        features.names();
        features = FeatureNegotiation::default();
        assert_eq!(features.take_over(false, &mut path), None, "disconnect retains the pending operation");
        assert_eq!(features.take_over(true, &mut path), None, "a reconnect Welcome cannot reuse previous named features");
        features.caps(proto::CAP_FEATURES);
        assert_eq!(features.take_over(true, &mut path), None);
        features.names();
        assert_eq!(features.take_over(true, &mut path), pending());
    }

    use crate::harness::core::Update;
    use crate::harness::host::{Host, Limits};
    use crate::harness::model::{State, Status};
    use crate::harness::session::Spec;
    use kiln_seams::clock::SimClock;

    /// W20: a held mail replay waits for room of its own, the share each session's budget is.
    /// Another session's copy, held or let go of, changes nothing of it (no pool spans every
    /// session), its own pulse schedules nothing, and the share measured larger lets it go once.
    #[test]
    fn a_held_mail_replay_retries_once_its_share_grows_never_for_another_sessions_release() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (sink, _updates) = async_channel::unbounded();
            let host = Host::with_records(sink, None, Limits::default(), Arc::new(SimClock::new()), Some(Arc::new(kiln_seams::records::SimRecords::new())));
            host.set_input_budget(100);
            let other = host.reserve_input_buffer(90).map_err(|_| anyhow::anyhow!("another session's reserve failed"))?.ok_or_else(|| anyhow::anyhow!("another session's reserve refused"))?;
            let before = host.native_budget_counts();
            let own = Arc::clone(&host);
            let mut replay = MailReplay::Reading { before, read: Box::pin(async move {
                // Its own room is whole, whatever another session holds.
                let charge = own.reserve_input_buffer(20).ok().flatten();
                assert!(charge.is_some());
                drop(charge);
                true
            }) };
            assert!(replay.budget_observed().is_none(), "budget watches are disabled for the one in-flight replay");
            let held = replay.completed().await;
            replay.finish(Some(&host), held);
            assert!(matches!(replay, MailReplay::Idle { budget: Some(_) }), "own temporary drop cannot schedule another attempt");
            assert!(MailReplay::budget_changed(Some(&host), replay.budget_observed()).now_or_never().is_none(), "own net-unchanged pulse cannot wake idle recovery");
            drop(other);
            assert!(MailReplay::budget_changed(Some(&host), replay.budget_observed()).now_or_never().is_none(), "another session's release is no room of its own");
            host.set_input_budget(200);
            assert_eq!(MailReplay::budget_changed(Some(&host), replay.budget_observed()).now_or_never(), Some(()));
            replay.budget_event(Some(&host));
            assert!(matches!(replay, MailReplay::Reading { .. }), "a larger share permits one existing-owner attempt");
            replay.budget_event(Some(&host));
            assert!(matches!(replay, MailReplay::Reading { .. }), "an in-flight budget event cannot enqueue a second attempt");
            let held = replay.completed().await;
            replay.finish(Some(&host), held);
            assert!(matches!(replay, MailReplay::Idle { budget: None }));

            // A share measured larger during the replay cannot be lost while its budget waiter is disabled.
            let before = host.native_budget_counts();
            let grows = Arc::clone(&host);
            let mut replay = MailReplay::Reading { before, read: Box::pin(async move { grows.set_input_budget(300); true }) };
            let held = replay.completed().await;
            replay.finish(Some(&host), held);
            assert!(matches!(replay, MailReplay::Reading { .. }), "post-operation comparison catches a larger share during replay");
            let held = replay.completed().await;
            replay.finish(Some(&host), held);
            assert!(matches!(replay, MailReplay::Idle { budget: None }));
            Ok(())
        })
    }

    /// CP-T3.2: the real Host cannot handle its native handover control until its initial
    /// snapshot gets through this full sink. The runner must keep taking that snapshot while
    /// its operation waits. Cancellation retains the operation through its actual completion.
    #[test]
    fn a_native_handover_drains_the_full_source_sink_and_keeps_cancellation_until_completion() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            for cancelled in [false, true] {
                let directory = tempfile::tempdir()?;
                let (tx, rx) = async_channel::bounded(1);
                tx.send(("stale".to_owned(), Update::State { state: Box::new(State { status: Status::Running, ..State::default() }) })).await?;
                let host = Host::with_records(tx, None, Limits::default(), Arc::new(SimClock::new()), Some(Arc::new(kiln_seams::records::SimRecords::new())));
                host.open(Some("native-prepare".into()), Spec {
                    kind: "claude".into(), cwd: Some(directory.path().to_string_lossy().into_owned()),
                    program: Some(directory.path().join("no-provider").to_string_lossy().into_owned()),
                    ..Spec::default()
                })?;
                let mut operations = HarnessOperations::new();
                let mut native = NativeHandover::default();
                assert!(native.prepare(&mut operations, Some(Arc::clone(&host)), NativePreparation::InPlace { exe: directory.path().join("next"), mtime: None }));
                assert!(!native.prepare(&mut operations, Some(Arc::clone(&host)), NativePreparation::InPlace { exe: directory.path().join("duplicate"), mtime: None }));
                assert_eq!(operations.len(), 1, "one existing operation owner, not a new handover queue");
                assert!(next_harness_operation(&mut operations).now_or_never().is_none(), "the native owner still waits behind its full sink");
                assert!(!native.idle());
                if cancelled { assert!(native.cancel(None, "cancel before native completion".into())); }
                let mut stale = false;
                let mut announced = false;
                let result = crate::rt::timeout(Duration::from_secs(5), async {
                    loop {
                        futures::select! {
                            done = next_harness_operation(&mut operations).fuse() => break Ok::<_, anyhow::Error>(done),
                            update = rx.recv().fuse() => {
                                let (session, update) = update?;
                                stale |= session == "stale" && matches!(&update, Update::State { state } if state.status == Status::Running);
                                announced |= session == "native-prepare" && matches!(&update, Update::Open { .. });
                            }
                        }
                    }
                }).await.map_err(|_| anyhow::anyhow!("native preparation blocked the loop that must drain its source sink"))??;
                let HarnessApplied::Native(NativePrepared::InPlace { result, .. }) = result else { anyhow::bail!("the wrong operation completed") };
                assert!(result.is_ok(), "a missing provider has no native process to detach: {result:?}");
                assert!(stale && announced, "both an earlier state and the actual source owner snapshot must drain");
                assert!(host.get("native-prepare").is_some(), "no session disappeared while preparation waited");
                assert_eq!(native.finish().as_deref(), cancelled.then_some("cancel before native completion"));
                assert!(native.idle());
                assert!(operations.is_empty());
                drop(rx);
                let _ = host.end("native-prepare");
            }
            Ok(())
        })
    }
}
