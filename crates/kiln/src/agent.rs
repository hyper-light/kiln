//! `kiln agent`: runs jobs on this machine (bare metal, a bench box, or a Kubernetes pod).
//!
//! * Enrolls once with a join token: generates its key locally, sends a CSR, stores the signed
//!   certificate. Afterwards it authenticates with mutual TLS only.
//! * Keeps one QUIC session to the server (hyper-transport), reconnecting with backoff forever.
//!   Jobs keep running while the server is unreachable; their output and results wait in a
//!   bounded uplink buffer and are delivered after reconnecting.
//! * Serves PTY terminals so the UI can drive this machine.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::exec::{Exec, Up, Uplink};
use crate::net::{self, Kind, NetEvent, Out, SERVER_PEER};
use crate::pki::Leaf;
use crate::proto::{self, AgentMsg, Hello, JobResult, LANE_CONTROL, LANE_LOGS, LANE_TERM, ServerMsg};
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
    pub shell: Option<String>,
    /// In-process agent: credentials handed over directly, no enrollment.
    pub credentials: Option<(Vec<u8>, Leaf)>,
}

#[derive(Serialize, Deserialize)]
pub struct EnrollRequest {
    pub token: String,
    pub name: String,
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
async fn credentials(o: &Opts, shutdown: &CancellationToken) -> anyhow::Result<(Vec<u8>, Leaf, Option<String>)> {
    if let Some((ca, leaf)) = &o.credentials {
        return Ok((ca.clone(), leaf.clone(), None));
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
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    let url = format!("{}/api/agents/enroll", o.server_url.trim_end_matches('/'));
    let mut backoff = Duration::from_secs(1);
    loop {
        let r = http
            .post(&url)
            .json(&EnrollRequest { token: token.clone(), name: o.name.clone(), csr: csr.clone() })
            .send()
            .await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                let e: EnrollResponse = resp.json().await.context("enroll response")?;
                crate::util::write_secret(&key_p, key_pem.as_bytes(), 0o600)?;
                crate::util::write_secret(&cert_p, e.cert_pem.as_bytes(), 0o644)?;
                crate::util::write_secret(&ca_p, e.ca_pem.as_bytes(), 0o644)?;
                crate::util::write_secret(&quic_p, e.quic.as_bytes(), 0o644)?;
                tracing::info!(name = %e.name, "enrolled with server");
                return Ok((ca_der(&e.ca_pem)?, Leaf { cert_pem: e.cert_pem, key_pem }, Some(e.quic)));
            }
            Ok(resp) if resp.status() == reqwest::StatusCode::UNAUTHORIZED || resp.status() == reqwest::StatusCode::FORBIDDEN => {
                anyhow::bail!("enrollment refused: the join token is invalid or expired");
            }
            Ok(resp) => tracing::warn!(status = %resp.status(), "enrollment failed; retrying"),
            Err(e) => tracing::warn!(error = %e, "server unreachable for enrollment; retrying"),
        }
        tokio::select! {
            () = tokio::time::sleep(crate::sup::jitter(backoff)) => {}
            () = shutdown.cancelled() => anyhow::bail!("shutdown"),
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn resolve(addr: &str) -> anyhow::Result<SocketAddr> {
    tokio::net::lookup_host(addr)
        .await
        .with_context(|| format!("resolve {addr}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("{addr} did not resolve"))
}

struct Running {
    cancel: CancellationToken,
}

pub async fn run(o: Opts, shutdown: CancellationToken) -> anyhow::Result<()> {
    std::fs::create_dir_all(&o.work_dir).with_context(|| format!("create {}", o.work_dir.display()))?;
    let orphans = crate::exec::reap_orphans(&o.work_dir);
    if orphans > 0 {
        tracing::warn!(orphans, "killed job processes left over from a previous agent run");
    }
    let (ca, leaf, enrolled_quic) = credentials(&o, &shutdown).await?;
    let quic_addr = o
        .quic
        .clone()
        .or(enrolled_quic)
        .ok_or_else(|| anyhow::anyhow!("server QUIC address unknown (pass --quic)"))?;
    let (net, mut events) = net::start(net::Spec {
        bind: "0.0.0.0:0".parse()?,
        ca_der: ca,
        leaf,
        server: false,
    })?;

    let (up_tx, mut up_rx) = mpsc::channel::<Up>(8192);
    let uplink = Uplink(up_tx);
    let mut buffer: VecDeque<(u32, Kind, Vec<u8>)> = VecDeque::new();
    let mut buffered = 0usize;
    let mut connected = false;
    let mut dial_backoff = Duration::from_millis(500);
    let mut redial: Option<tokio::time::Instant> = Some(tokio::time::Instant::now());
    let mut jobs: HashMap<String, Running> = HashMap::new();
    let mut tasks: JoinSet<JobResult> = JoinSet::new();
    let mut exclusive = false;
    let mut draining = false;
    let mut terms: HashMap<String, PtySession> = HashMap::new();
    let (term_tx, mut term_rx) = mpsc::channel::<(String, PtyOut)>(1024);
    let mut sampler = crate::host::Sampler::new(Some(&o.work_dir));
    let mut sample_every = Duration::from_secs(5);
    let mut sample_tick = tokio::time::interval(sample_every);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
    let mut served_dedicated = false;
    let started = tokio::time::Instant::now();

    let hello = |running: Vec<String>| Hello {
        protocol: proto::VERSION,
        name: o.name.clone(),
        labels: o.labels.clone(),
        slots: o.slots.max(1),
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
            tokio::time::sleep(Duration::from_millis(500)).await;
            return Ok(());
        }
        if o.ephemeral && !served_dedicated && jobs.is_empty() && started.elapsed() > Duration::from_secs(900) {
            anyhow::bail!("ephemeral agent received no job within 15 minutes");
        }
        if draining && jobs.is_empty() && o.ephemeral {
            return Ok(());
        }
        let redial_at = redial;
        tokio::select! {
            () = shutdown.cancelled() => {
                for r in jobs.values() { r.cancel.cancel(); }
                return Ok(());
            }
            () = async { if let Some(t) = redial_at { tokio::time::sleep_until(t).await } else { std::future::pending().await } } => {
                redial = None;
                match resolve(&quic_addr).await {
                    Ok(addr) => { net.command(Out::Connect { addr }).await; }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "cannot resolve server");
                        redial = Some(tokio::time::Instant::now() + crate::sup::jitter(dial_backoff));
                        dial_backoff = (dial_backoff * 2).min(Duration::from_secs(30));
                    }
                }
            }
            ev = events.recv() => {
                let Some(ev) = ev else { anyhow::bail!("transport stopped") };
                match ev {
                    NetEvent::Connected { .. } => {
                        tracing::info!(server = %quic_addr, "connected to server");
                        dial_backoff = Duration::from_millis(500);
                        let msg = AgentMsg::Hello(hello(jobs.keys().cloned().collect()));
                        if let Ok(b) = proto::encode(&msg) {
                            net.send(SERVER_PEER, LANE_CONTROL, Kind::Control, b).await;
                        }
                        connected = true;
                        while let Some((lane, kind, data)) = buffer.pop_front() {
                            buffered = buffered.saturating_sub(data.len());
                            net.send(SERVER_PEER, lane, kind, data).await;
                        }
                    }
                    NetEvent::Closed { reason, .. } | NetEvent::Unreachable { reason, .. } => {
                        if connected { tracing::warn!(%reason, "disconnected from server"); }
                        else { tracing::debug!(%reason, "dial failed"); }
                        connected = false;
                        terms.clear();
                        redial = Some(tokio::time::Instant::now() + crate::sup::jitter(dial_backoff));
                        dial_backoff = (dial_backoff * 2).min(Duration::from_secs(30));
                    }
                    NetEvent::Frame { data, .. } => {
                        let msg: ServerMsg = match proto::decode(&data) {
                            Ok(m) => m,
                            Err(e) => { tracing::warn!(error = %e, "bad frame from server"); continue; }
                        };
                        match msg {
                            ServerMsg::Welcome { server_version, sample_seconds } => {
                                tracing::info!(%server_version, "server welcomed agent");
                                sample_every = Duration::from_secs(u64::from(sample_seconds.max(1)));
                                sample_tick = tokio::time::interval(sample_every);
                            }
                            ServerMsg::Refused { reason } => {
                                tracing::error!(%reason, "server refused this agent");
                                if o.ephemeral { anyhow::bail!("refused: {reason}"); }
                                connected = false;
                                net.command(Out::Disconnect { peer: SERVER_PEER }).await;
                                redial = Some(tokio::time::Instant::now() + Duration::from_secs(60));
                            }
                            ServerMsg::Assign(mut a) => {
                                // The URL this agent reached the server through is the one that
                                // works from here (NAT, NodePorts, cluster DNS all differ).
                                a.server_url = o.server_url.clone();
                                let id = a.job_id.clone();
                                let full = u32::try_from(jobs.len()).unwrap_or(u32::MAX) >= o.slots.max(1);
                                if draining || full || exclusive || (a.spec.exclusive && !jobs.is_empty()) || jobs.contains_key(&id) {
                                    let reason = if draining { "draining" } else { "no free slot" };
                                    uplink.msg(AgentMsg::Reject { job: id, reason: reason.into() }).await;
                                    continue;
                                }
                                if o.dedicated_job.as_deref() == Some(id.as_str()) { served_dedicated = true; }
                                exclusive = a.spec.exclusive;
                                let cancel = CancellationToken::new();
                                jobs.insert(id.clone(), Running { cancel: cancel.clone() });
                                tracing::info!(job = %id, key = %a.spec.key, "running job");
                                let exec = Exec {
                                    assign: *a,
                                    work_dir: o.work_dir.clone(),
                                    agent_name: o.name.clone(),
                                    up: uplink.clone(),
                                    cancel,
                                };
                                let fallback_id = id.clone();
                                tasks.spawn(async move {
                                    match futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(exec.run())).await {
                                        Ok(r) => r,
                                        Err(p) => JobResult {
                                            job: fallback_id,
                                            status: Status::Failed,
                                            exit_code: None,
                                            error: Some(format!("executor panicked: {}", crate::sup::panic_message(p.as_ref()))),
                                            peak_rss: None,
                                            cpu_seconds: None,
                                        },
                                    }
                                });
                            }
                            ServerMsg::Cancel { job, reason } => {
                                if let Some(r) = jobs.get(&job) {
                                    tracing::info!(%job, %reason, "cancelling job");
                                    r.cancel.cancel();
                                }
                            }
                            ServerMsg::Drain => draining = true,
                            ServerMsg::TermOpen { session, cols, rows, command } => {
                                if !o.terminal {
                                    uplink.0.send(Up::Term(AgentMsg::TermExit { session, code: None, error: Some("terminal disabled on this agent".into()) })).await.ok();
                                    continue;
                                }
                                match terminal::spawn(o.shell.as_deref(), command.as_deref(), cols, rows) {
                                    Ok((pty, mut rx)) => {
                                        let tx = term_tx.clone();
                                        let s = session.clone();
                                        tokio::spawn(async move {
                                            while let Some(out) = rx.recv().await {
                                                let exit = matches!(out, PtyOut::Exit(_));
                                                if tx.send((s.clone(), out)).await.is_err() || exit { break; }
                                            }
                                        });
                                        terms.insert(session, pty);
                                    }
                                    Err(e) => {
                                        uplink.0.send(Up::Term(AgentMsg::TermExit { session, code: None, error: Some(format!("{e:#}")) })).await.ok();
                                    }
                                }
                            }
                            ServerMsg::TermInput { session, data } => {
                                if let Some(t) = terms.get(&session) { t.write(data); }
                            }
                            ServerMsg::TermResize { session, cols, rows } => {
                                if let Some(t) = terms.get(&session) { t.resize(cols, rows); }
                            }
                            ServerMsg::TermClose { session } => { terms.remove(&session); }
                        }
                    }
                }
            }
            Some((session, out)) = term_rx.recv() => {
                let msg = match out {
                    PtyOut::Data(data) => AgentMsg::TermOutput { session, data },
                    PtyOut::Exit(code) => { terms.remove(&session); AgentMsg::TermExit { session, code, error: None } }
                };
                uplink.0.send(Up::Term(msg)).await.ok();
            }
            Some(up) = up_rx.recv() => {
                let (lane, kind, data) = match &up {
                    Up::Msg(m) => (LANE_CONTROL, Kind::Control, proto::encode(m)),
                    Up::Log(c) => (LANE_LOGS, Kind::Log, proto::encode(c)),
                    Up::Term(m) => (LANE_TERM, Kind::Term, proto::encode(m)),
                };
                let Ok(data) = data else { continue };
                if connected {
                    net.send(SERVER_PEER, lane, kind, data).await;
                } else if kind != Kind::Term {
                    buffered += data.len();
                    buffer.push_back((lane, kind, data));
                    // Shed the oldest log chunks first; results and step events are kept.
                    while buffered > UPLINK_BUFFER {
                        let Some(pos) = buffer.iter().position(|(_, k, _)| *k == Kind::Log) else { break };
                        if let Some((_, _, d)) = buffer.remove(pos) { buffered = buffered.saturating_sub(d.len()); }
                    }
                }
            }
            Some(done) = tasks.join_next() => {
                match done {
                    Ok(r) => {
                        tracing::info!(job = %r.job, status = r.status.as_str(), "job finished");
                        jobs.remove(&r.job);
                        if jobs.is_empty() { exclusive = false; }
                        uplink.msg(AgentMsg::JobFinished(r)).await;
                    }
                    Err(e) => tracing::error!(error = %e, "job task failed to join"),
                }
            }
            _ = heartbeat.tick() => {
                if connected {
                    let free = o.slots.saturating_sub(u32::try_from(jobs.len()).unwrap_or(0));
                    uplink.msg(AgentMsg::Heartbeat { free_slots: free, running: jobs.keys().cloned().collect() }).await;
                }
            }
            _ = sample_tick.tick() => {
                if connected {
                    let s = sampler.sample();
                    uplink.msg(AgentMsg::Sample(s)).await;
                }
            }
        }
    }
}

/// Enroll (or renew) and store credentials without connecting. Used by `kiln install --agent`.
pub async fn enroll_only(o: &Opts) -> anyhow::Result<()> {
    credentials(o, &CancellationToken::new()).await.map(|_| ())
}
