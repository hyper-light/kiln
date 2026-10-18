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
use crate::proto::{self, AgentMsg, Hello, JobResult, LANE_CONTROL, LANE_CONVO, LANE_LOGS, LANE_REACH, LANE_SYNC, LANE_TERM, ServerMsg};
use crate::store::Status;
use crate::terminal::{self, PtyOut, PtySession};

/// Bytes of output/results held while disconnected before the oldest log chunks are shed.
const UPLINK_BUFFER: usize = 128 << 20;

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
    /// Reach (`crate::reach`): None leaves it off (the server's own in-process agent); otherwise
    /// whether this runner takes commands from other runners (it can always send them).
    pub reach: Option<bool>,
    /// An outpost (`kiln outpost`, [`crate::proto::CAP_OUTPOST`]): kiln run where it isn't
    /// installed. Its terminals, AI sessions and agents work; it takes no jobs, keeps no front
    /// door or repo copies, and is never upgraded by the server.
    pub outpost: bool,
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
    let instance_hello = proto::InstanceHello {
        id: instance_id.clone(),
        created_at: crate::upgrade::created_at(&o.state_dir, &instance_id),
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
    let mut paths = crate::tunnel::Paths::new(&o.server_url, &quic_addr);

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
            match crate::reach::local::serve(handle.clone(), name.clone(), crate::reach::local::path(&name, &own_dir)) {
                Ok(p) => crate::reach::local::set_socket(p.to_string_lossy().into_owned()),
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "`kiln run` will not be available on this machine"),
            }
            Some(handle)
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "reach is off on this runner");
            None
        }
    });
    let uplink = Uplink(up_tx);
    let (done_tx, done_rx) = async_channel::unbounded::<JobResult>();
    let mut buffer: VecDeque<(u32, Kind, Vec<u8>)> = VecDeque::new();
    let mut buffered = 0usize;
    let mut connected = false;
    let mut dial_backoff = Duration::from_millis(500);
    let mut redial: Option<Instant> = Some(Instant::now());
    let mut dialing_since: Option<Instant> = None;
    let mut jobs: HashMap<String, Running> = HashMap::new();
    let mut exclusive = false;
    let mut draining = false;
    // Terminals outlive the server connection: kept (with their replay state) across disconnects and
    // announced again on every reconnect.
    let mut terms: HashMap<String, (PtySession, Arc<crate::termsess::Session>)> = HashMap::new();
    let (term_tx, term_rx) = async_channel::bounded::<(String, PtyOut)>(1024);
    // In-place upgrades: a new binary on disk is taken up by re-executing this process (same pid)
    // once no job runs, handing every terminal to the new image so shells survive.
    let exe = std::env::current_exe().ok();
    let mut exe_mtime = exe.as_deref().and_then(binary_mtime);
    let mut upgrade_pending = false;
    // Only a `kiln agent` process re-executes itself: the server's in-process agent shares its
    // process with the server, whose upgrades its supervisor handles.
    let standalone = std::env::args().skip(1).find(|a| !a.starts_with('-')).as_deref() == Some("agent");
    adopt_handover(&o, &mut terms, &term_tx);
    // AI coding sessions running on this machine (job pods have none), reported as they change.
    let ai = (!o.ephemeral).then(|| crate::aisessions::Reporter::new(crate::os::home_dir()));
    // Kiln-native agent sessions on this machine (not in job pods, and not in the server's own
    // in-process agent: the server hosts those itself). They outlive the server connection and
    // this process's restarts: each continues its harness session.
    let (harness_tx, harness_rx) = async_channel::bounded::<(String, crate::harness::core::Update)>(8192);
    // Conversations kept on disk as they happen (docs/conversations.md), and sent to the server's
    // store over this link (`crate::convo::feed`).
    let mut convo: Option<crate::convo::store::Store> = None;
    let harness = (!o.ephemeral && o.terminal).then(|| {
        convo = crate::convo::open(&own_dir);
        let sink = convo.as_ref().map_or_else(|| harness_tx.clone(), |st| crate::convo::store::tee(harness_tx.clone(), st.clone(), 8192));
        let h = crate::harness::host::Host::new(sink, Some(own_dir.join("harness.json")), Default::default());
        h.restore_from(convo.as_ref());
        h
    });
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
    let harness_sock = harness.as_ref().and_then(|h| match crate::harness::socket::serve(h, crate::harness::socket::path(&name, &own_dir)) {
        Ok(p) => Some(p.to_string_lossy().into_owned()),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "`kiln spawn` will not be available in this machine's terminals");
            None
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
        // Ephemeral agents leave once their one job is done and everything is delivered.
        if o.ephemeral && served_dedicated && jobs.is_empty() && buffer.is_empty() && up_rx.is_empty() {
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
        let ev = futures::select_biased! {
            () = shutdown.cancelled().fuse() => Ev::Shutdown,
            ev = events.recv().fuse() => Ev::Net(ev.ok()),
            r = done_rx.recv().fuse() => match r { Ok(r) => Ev::Done(r), Err(_) => Ev::Watchdog },
            () = heartbeat.tick().fuse() => Ev::Heartbeat,
            () = sample_tick.tick().fuse() => Ev::Sample,
            () = watchdog.tick().fuse() => Ev::Watchdog,
            () = sleep_until_opt(redial).fuse() => Ev::Redial,
            t = term_rx.recv().fuse() => match t { Ok((s, out)) => Ev::Term(s, out), Err(_) => Ev::Watchdog },
            up = up_rx.recv().fuse() => match up { Ok(u) => Ev::Up(u), Err(_) => Ev::Watchdog },
            h = harness_rx.recv().fuse() => match h { Ok((s, u)) => Ev::Harness(s, u), Err(_) => Ev::Watchdog },
            () = front_door_changed(front_door.as_ref()).fuse() => Ev::FrontDoor,
        };
        match ev {
            Ev::Shutdown => {
                for r in jobs.values() {
                    r.cancel.cancel();
                }
                return Ok(());
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
                        tracing::info!(jobs = jobs.len(), terminals = terms.len(), "new kiln binary on disk; upgrading in place once running jobs finish");
                    }
                    // Agent sessions mid-turn get up to ten minutes to finish it; then they are
                    // stopped and continue (resumed) in the new image.
                    // Reach runs (for other runners, or followed on them) count like agents mid-turn.
                    let agents_busy = harness.as_ref().map_or(0, |h| h.busy()) + reach.as_ref().map_or(0, crate::reach::node::Handle::busy);
                    if upgrade_pending && agents_busy > 0 {
                        harness_wait.get_or_insert_with(Instant::now);
                    }
                    let agents_ready = agents_busy == 0 || harness_wait.is_some_and(|t| t.elapsed() > Duration::from_secs(600));
                    if upgrade_pending && jobs.is_empty() && agents_ready && let Some(exe) = exe.as_deref() {
                        if let Some(h) = &harness {
                            h.stop_all();
                        }
                        let err = upgrade_in_place(&o, exe, &terms);
                        // Only reached when the exec failed: carry on with this binary.
                        tracing::error!(error = %err, "in-place upgrade failed; continuing on the running binary");
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
                paths.connected(binding, Instant::now());
                dialing_since = None;
                dial_backoff = Duration::from_millis(500);
                // Which process of this machine this is, then Hello (an older server skips the first).
                if !o.ephemeral
                    && let Ok(b) = proto::encode(&AgentMsg::Instance(instance_hello.clone()))
                {
                    net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                }
                let msg = AgentMsg::Hello(hello(jobs.keys().cloned().collect()));
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
                while let Some((lane, kind, data)) = buffer.pop_front() {
                    buffered = buffered.saturating_sub(data.len());
                    net.send(SERVER_PEER, lane, kind, data).await;
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
                if let Some(s) = &sync {
                    s.connected(false);
                }
                if let Some(f) = &shipper {
                    f.link(crate::convo::feed::Link::Down);
                }
                if let Some(r) = &reach {
                    r.feed(crate::reach::node::In::Server(false)).await;
                }
                dialing_since = None;
                redial = Some(if at_once { now } else { now + crate::sup::jitter(dial_backoff) });
                dial_backoff = (dial_backoff * 2).min(Duration::from_secs(30));
            }
            Ev::Net(Some(NetEvent::Frame { data, .. })) => {
                let msg: ServerMsg = match proto::decode(&data) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(error = %e, "bad frame from server");
                        continue;
                    }
                };
                match msg {
                    ServerMsg::Welcome { server_version, sample_seconds } => {
                        tracing::info!(%server_version, "server welcomed agent");
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
                        let full = u32::try_from(jobs.len()).unwrap_or(u32::MAX) >= o.slots.max(1);
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
                        jobs.insert(id.clone(), Running { cancel: cancel.clone() });
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
                    }
                    ServerMsg::Upgrade { job, path, sha256, size } if o.outpost => {
                        // Nothing of an outpost is installed: its next kiln is the one downloaded next.
                        let _ = (path, sha256, size);
                        let error = Some("an outpost isn't installed, so the server doesn't upgrade it".to_owned());
                        uplink.msg(AgentMsg::UpgradeDone { job, ok: false, error }).await;
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
                    ServerMsg::Convo { json, .. } => {
                        if let (Some(f), Some(m)) = (&shipper, crate::convo::feed::decode(&json)) {
                            f.link(crate::convo::feed::Link::Heard(m));
                        }
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
                        let full = u32::try_from(jobs.len()).unwrap_or(u32::MAX) >= o.slots.max(1);
                        if o.outpost || draining || upgrade_pending || full || exclusive || jobs.contains_key(&id) {
                            let reason = if o.outpost { OUTPOST_TAKES_NO_JOBS } else if draining { "draining" } else if upgrade_pending { "upgrading" } else { "no free slot" };
                            uplink.msg(AgentMsg::Reject { job: id, reason: reason.into() }).await;
                            continue;
                        }
                        // Server-relative URLs go through the address this runner reached the server
                        // at (NAT, NodePorts and cluster DNS all differ).
                        let base = o.server_url.trim_end_matches('/').to_owned();
                        let rt = &mut a.runtime;
                        for u in [&mut rt.results_url, &mut rt.cache_url, &mut rt.facade_url, &mut rt.api_url, &mut rt.graphql_url] {
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
                        jobs.insert(id.clone(), Running { cancel: cancel.clone() });
                        tracing::info!(job = %id, leg = %a.leg.name, "running leg");
                        let leg = crate::runner::runtime::Leg { a, work_dir: o.work_dir.clone(), runner_name: name.clone(), up: uplink.clone(), cancel, http: http.clone() };
                        let done = done_tx.clone();
                        crate::rt::spawn(async move {
                            let result = leg.run().await;
                            let _ = done.send(result).await;
                        });
                    }
                    ServerMsg::ProbeFacts => {
                        if let Ok(b) = proto::encode(&crate::updates::runner_facts()) {
                            net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                        }
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
            Ev::Up(up) => {
                let (lane, kind, data) = match &up {
                    Up::Msg(m) => (LANE_CONTROL, Kind::Control, proto::encode(m)),
                    Up::Log(c) => (LANE_LOGS, Kind::Log, proto::encode(c)),
                    Up::Term(m) => (LANE_TERM, Kind::Term, proto::encode(m)),
                    Up::Sync(m) => (LANE_SYNC, Kind::Control, proto::encode(m)),
                    Up::Reach(m) => (LANE_REACH, Kind::Term, proto::encode(m)),
                    Up::Convo(m) => (LANE_CONVO, Kind::Log, proto::encode(m)),
                };
                let Ok(data) = data else { continue };
                if connected {
                    net.send(SERVER_PEER, lane, kind, data).await;
                } else if kind != Kind::Term && lane != LANE_SYNC && lane != LANE_REACH && lane != LANE_CONVO {
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
                if jobs.remove(&r.job).is_some() {
                    tracing::info!(job = %r.job, status = r.status.as_str(), "job finished");
                    if jobs.is_empty() {
                        exclusive = false;
                    }
                    uplink.msg(AgentMsg::JobFinished(r)).await;
                } else {
                    // Reported over already: a cancelled job let go of after its grace that ended
                    // after all (or the grace's report, after the job's own).
                    tracing::info!(job = %r.job, "a job already reported over has ended");
                }
            }
            Ev::Heartbeat => {
                if connected {
                    let free = if upgrade_pending { 0 } else { o.slots.saturating_sub(u32::try_from(jobs.len()).unwrap_or(0)) };
                    uplink.msg(AgentMsg::Heartbeat { free_slots: free, running: jobs.keys().cloned().collect() }).await;
                }
            }
            Ev::Sample => {
                if connected {
                    let s = sampler.sample();
                    uplink.msg(AgentMsg::Sample(s)).await;
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

fn handover_path(o: &Opts) -> PathBuf {
    crate::upgrade::own_dir(&o.state_dir, o.instance.as_deref()).join("terminal-handover.json")
}

/// Re-execute this process from `exe` (same pid), handing every terminal to the new image.
/// Returns only if the exec failed.
fn upgrade_in_place(o: &Opts, exe: &std::path::Path, terms: &HashMap<String, (PtySession, Arc<crate::termsess::Session>)>) -> std::io::Error {
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
    let path = handover_path(o);
    let written = serde_json::to_vec(&handed).map_err(std::io::Error::other).and_then(|json| {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        crate::os::with_mode(&mut opts, 0o600);
        use std::io::Write as _;
        opts.open(&path)?.write_all(&json)
    });
    if let Err(e) = written {
        return e;
    }
    tracing::info!(terminals = handed.len(), exe = %exe.display(), "upgrading in place");
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let err = crate::os::reexec(exe, &args, &[]);
    let _ = std::fs::remove_file(&path);
    err
}

/// Take over the terminals a previous image of this process handed over (see
/// [`upgrade_in_place`]): reopen each PTY, rebuild its replay state, and keep it for the server,
/// which re-adopts it from the next TermList.
fn adopt_handover(o: &Opts, terms: &mut HashMap<String, (PtySession, Arc<crate::termsess::Session>)>, term_tx: &async_channel::Sender<(String, PtyOut)>) {
    use base64::Engine as _;
    let path = handover_path(o);
    let Ok(raw) = std::fs::read(&path) else { return };
    let _ = std::fs::remove_file(&path);
    let handed: Vec<HandedTerm> = match serde_json::from_slice(&raw) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, "unreadable terminal handover; its shells are lost");
            return;
        }
    };
    let mut adopted = 0usize;
    for h in handed {
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
