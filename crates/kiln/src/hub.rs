//! The server core: agent sessions, the scheduler, job bookkeeping and the UI event bus.
//!
//! All network input arrives as [`NetEvent`]s from the QUIC thread and is handled here; every
//! outcome is persisted through [`Store`] before it is broadcast, so a crash at any point loses
//! at most in-flight log bytes, never job state.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hyper_transport::PeerId;
use parking_lot::Mutex;
use serde::Serialize;
use futures::FutureExt as _;

use crate::rt::{Broadcast, Interval, Notify, Shutdown};

use crate::config::Config;
use crate::logs::Logs;
use crate::metrics::{self, HostSample};
use crate::net::{Kind, NetEvent, NetHandle};
use crate::pipeline::Context;
use crate::proto::{self, AgentMsg, Assignment, Hello, JobResult, LANE_CONTROL, LANE_TERM, ServerMsg};
use crate::store::{AgentRow, Job, Status, Store};

/// How long a persistent agent may be gone before its jobs are requeued.
const AGENT_GRACE: Duration = Duration::from_secs(45);
/// Host samples kept per agent for the UI charts (1 h at 5 s).
const HISTORY: usize = 720;
/// How long an agent that declined a job is not offered that job again.
const DECLINE_BACKOFF: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Serialize)]
pub struct AgentLive {
    pub name: String,
    #[serde(skip)]
    pub peer: Option<PeerId>,
    pub labels: Vec<String>,
    pub slots: u32,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub version: String,
    pub ephemeral: bool,
    pub connected: bool,
    pub last_seen: i64,
    pub enrolled_at: i64,
    pub running: Vec<String>,
    pub busy: u32,
    pub draining: bool,
    pub sample: Option<HostSample>,
    pub terminal: bool,
    #[serde(skip)]
    pub dedicated_job: Option<String>,
    #[serde(skip)]
    pub exclusive: bool,
    #[serde(skip)]
    pub lost_at: Option<Instant>,
}

/// Output from a remote (agent) terminal session, routed to its websocket.
#[derive(Debug)]
pub enum TermOut {
    Data(Vec<u8>),
    /// The agent's replay for an adopted session.
    Replay(Vec<u8>),
    Exit { code: Option<i32>, error: Option<String> },
    /// The machine disconnected; its shells keep running there.
    Gone,
}

pub struct Hub {
    pub cfg: Arc<Config>,
    pub store: Store,
    pub logs: Logs,
    pub net: NetHandle,
    pub pki: Arc<crate::pki::Pki>,
    pub agents: Mutex<HashMap<String, AgentLive>>,
    peers: Mutex<HashMap<PeerId, String>>,
    pending_peers: Mutex<HashMap<PeerId, String>>,
    pub events: Broadcast<Arc<str>>,
    pub terms: Mutex<HashMap<String, async_channel::Sender<TermOut>>>,
    /// Every terminal session, local and on machines.
    pub terminals: crate::termsess::Sessions,
    pub history: Mutex<HashMap<String, VecDeque<HostSample>>>,
    pub wake: Notify,
    pub launching: Mutex<HashMap<String, Instant>>,
    pub started: Instant,
    pub generation: u64,
    pub admin_token: String,
    pub join_token: String,
    /// One-time enrollment tokens for pod agents: token → (job id, expiry).
    pub ephemeral_tokens: Mutex<HashMap<String, (String, Instant)>>,
    pub k8s: std::sync::OnceLock<Option<Arc<crate::k8s::K8s>>>,
    pub gitops_kick: Notify,
    pub shutdown: Shutdown,
    /// Agents that have been sent Cancel for a job, with when (for the timeout escalation).
    cancel_sent: Mutex<HashMap<String, Instant>>,
    /// (job, agent) pairs recently declined: not offered again until DECLINE_BACKOFF passes.
    declined: Mutex<HashMap<(String, String), Instant>>,
    /// Multicast DNS discovery: what this server announces and the kiln instances it heard.
    pub discovery: Arc<crate::discovery::Discovery>,
    /// Machines asking to join as agents (`kiln join`), waiting for an admin.
    pub joins: crate::join::Requests,
}

impl Hub {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Arc<Config>,
        store: Store,
        logs: Logs,
        net: NetHandle,
        pki: Arc<crate::pki::Pki>,
        admin_token: String,
        join_token: String,
        shutdown: Shutdown,
    ) -> Arc<Self> {
        let generation =
            std::env::var("KILN_GENERATION").ok().and_then(|g| g.parse().ok()).unwrap_or(1);
        let discovery = Arc::new(crate::discovery::Discovery::for_server(&cfg, &pki.ca_der));
        Arc::new(Self {
            cfg,
            store,
            logs,
            net,
            pki,
            agents: Mutex::new(HashMap::new()),
            peers: Mutex::new(HashMap::new()),
            pending_peers: Mutex::new(HashMap::new()),
            events: Broadcast::new(4096),
            terms: Mutex::new(HashMap::new()),
            terminals: crate::termsess::Sessions::new(),
            history: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            launching: Mutex::new(HashMap::new()),
            started: Instant::now(),
            generation,
            admin_token,
            join_token,
            ephemeral_tokens: Mutex::new(HashMap::new()),
            k8s: std::sync::OnceLock::new(),
            gitops_kick: Notify::new(),
            shutdown,
            cancel_sent: Mutex::new(HashMap::new()),
            declined: Mutex::new(HashMap::new()),
            discovery,
            joins: crate::join::Requests::default(),
        })
    }

    // ------------------------------------------------------------ UI events

    /// Close every runner's connection (server shutdown): runners see the close at once and
    /// reconnect to the next server process, instead of waiting out the QUIC idle timeout.
    pub async fn disconnect_all(&self) {
        let peers: Vec<PeerId> = self.peers.lock().keys().chain(self.pending_peers.lock().keys()).copied().collect();
        for peer in peers {
            let _ = self.net.command(crate::net::Out::Disconnect { peer }).await;
        }
    }

    /// Tell every UI that the session list changed.
    pub fn emit_terminals(&self) {
        self.emit(&serde_json::json!({ "type": "terminals" }));
    }

    pub fn emit(&self, v: &serde_json::Value) {
        if self.events.receiver_count() > 0 {
            self.events.send(Arc::from(v.to_string()));
        }
    }

    pub async fn emit_job(&self, job_id: &str) {
        if let Ok(Some(mut j)) = self.store.job(job_id.to_owned()).await {
            j.spec = None;
            self.emit(&serde_json::json!({"type": "job", "job": j}));
        }
    }

    pub async fn emit_run(&self, run_id: &str) {
        if let Ok(Some(r)) = self.store.run(run_id.to_owned()).await {
            self.emit(&serde_json::json!({"type": "run", "run": r}));
        }
    }

    fn emit_agent(&self, a: &AgentLive) {
        self.emit(&serde_json::json!({"type": "agent", "agent": a}));
    }

    pub fn emit_queue(&self) {
        let m = metrics::global();
        self.emit(&serde_json::json!({
            "type": "queue", "queued": m.jobs_queued.get(), "running": m.jobs_running.get()
        }));
    }

    pub fn record_sample(&self, agent: &str, s: HostSample) {
        {
            let mut h = self.history.lock();
            let q = h.entry(agent.to_owned()).or_default();
            if q.len() >= HISTORY {
                q.pop_front();
            }
            q.push_back(s.clone());
        }
        if agent != "server" {
            metrics::agent_samples().write().insert(agent.to_owned(), s.clone());
        }
        self.emit(&serde_json::json!({"type": "sample", "agent": agent, "sample": s}));
    }

    // ------------------------------------------------------------ agent view

    /// Persistent agents from the DB merged with live sessions.
    pub async fn agent_list(&self) -> Vec<AgentLive> {
        let rows = self.store.agents().await.unwrap_or_default();
        let live = self.agents.lock().clone();
        let mut out: Vec<AgentLive> = rows
            .into_iter()
            .map(|r| match live.get(&r.name) {
                Some(l) => l.clone(),
                None => AgentLive {
                    name: r.name,
                    peer: None,
                    labels: r.labels,
                    slots: r.slots,
                    hostname: r.hostname,
                    os: r.os,
                    arch: r.arch,
                    version: r.version,
                    ephemeral: r.ephemeral,
                    connected: false,
                    last_seen: r.last_seen,
                    enrolled_at: r.enrolled_at,
                    running: vec![],
                    busy: 0,
                    draining: false,
                    sample: None,
                    terminal: false,
                    dedicated_job: None,
                    exclusive: false,
                    lost_at: None,
                },
            })
            .collect();
        for (name, l) in &live {
            if !out.iter().any(|a| &a.name == name) {
                out.push(l.clone());
            }
        }
        out.sort_by(|a, b| b.connected.cmp(&a.connected).then(a.name.cmp(&b.name)));
        out
    }

    fn update_slot_gauges(&self) {
        let g = self.agents.lock();
        let (mut conn, mut total, mut busy) = (0i64, 0i64, 0i64);
        for a in g.values().filter(|a| a.connected) {
            conn += 1;
            total += i64::from(a.slots);
            busy += i64::from(a.busy);
        }
        let m = metrics::global();
        m.agents_connected.set(conn);
        m.agent_slots_total.set(total);
        m.agent_slots_busy.set(busy);
    }

    pub async fn send_to_agent(&self, agent: &str, lane: u32, msg: &ServerMsg) -> bool {
        let peer = self.agents.lock().get(agent).and_then(|a| a.peer);
        let Some(peer) = peer else { return false };
        let kind = if lane == LANE_TERM { Kind::Term } else { Kind::Control };
        match proto::encode(msg) {
            Ok(b) => self.net.send(peer, lane, kind, b).await,
            Err(e) => {
                tracing::warn!(error = %e, "encode failed");
                false
            }
        }
    }

    async fn send_peer(&self, peer: PeerId, msg: &ServerMsg) {
        if let Ok(b) = proto::encode(msg) {
            self.net.send(peer, LANE_CONTROL, Kind::Control, b).await;
        }
    }

    pub async fn set_draining(&self, agent: &str, draining: bool) -> bool {
        let snapshot = {
            let mut g = self.agents.lock();
            let Some(a) = g.get_mut(agent) else { return false };
            a.draining = draining;
            a.clone()
        };
        self.emit_agent(&snapshot);
        self.wake.notify();
        true
    }

    pub async fn forget_agent(&self, agent: &str) -> anyhow::Result<()> {
        let peer = self.agents.lock().remove(agent).and_then(|a| a.peer);
        if let Some(p) = peer {
            self.net.command(crate::net::Out::Disconnect { peer: p }).await;
        }
        self.net.command(crate::net::Out::Revoke { name: agent.to_owned() }).await;
        metrics::agent_samples().write().remove(agent);
        self.store.delete_agent(agent.to_owned()).await?;
        self.update_slot_gauges();
        Ok(())
    }

    // ------------------------------------------------------------ network

    /// Consume transport events forever (supervised).
    pub async fn net_loop(self: Arc<Self>, rx: async_channel::Receiver<NetEvent>) -> anyhow::Result<()> {
        while let Ok(ev) = rx.recv().await {
            if let Err(e) = self.on_net(ev).await {
                tracing::warn!(error = %format!("{e:#}"), "agent message handling failed");
            }
        }
        anyhow::bail!("transport event stream ended")
    }

    async fn on_net(self: &Arc<Self>, ev: NetEvent) -> anyhow::Result<()> {
        match ev {
            NetEvent::Connected { peer, name } => {
                if let Some(n) = name {
                    self.pending_peers.lock().insert(peer, n);
                }
            }
            NetEvent::Closed { peer, reason } | NetEvent::Unreachable { peer, reason } => {
                self.pending_peers.lock().remove(&peer);
                let name = self.peers.lock().remove(&peer);
                if let Some(name) = name {
                    self.agent_lost(&name, &reason).await;
                }
            }
            NetEvent::Frame { peer, kind: Kind::Log, data, .. } => {
                let chunk: proto::LogChunk = proto::decode(&data)?;
                if self.owns_job(peer, &chunk.job) {
                    self.logs.append(&chunk.job, chunk.idx, &chunk.data);
                }
            }
            NetEvent::Frame { peer, data, .. } => {
                let msg: AgentMsg = proto::decode(&data)?;
                self.on_agent_msg(peer, msg).await?;
            }
        }
        Ok(())
    }

    fn agent_of(&self, peer: PeerId) -> Option<String> {
        self.peers.lock().get(&peer).cloned()
    }

    fn owns_job(&self, peer: PeerId, job: &str) -> bool {
        let Some(name) = self.agent_of(peer) else { return false };
        self.agents.lock().get(&name).is_some_and(|a| a.running.iter().any(|j| j == job))
    }

    async fn agent_lost(&self, name: &str, reason: &str) {
        tracing::warn!(agent = name, reason, "agent disconnected");
        let snapshot = {
            let mut g = self.agents.lock();
            let Some(a) = g.get_mut(name) else { return };
            a.connected = false;
            a.peer = None;
            a.lost_at = Some(Instant::now());
            a.clone()
        };
        if let Err(e) = self.store.agent_disconnected(name.to_owned()).await {
            tracing::warn!(error = %e, "agent_disconnected");
        }
        // That machine's terminals keep running there; park them until it reconnects.
        let sessions: Vec<String> = self
            .terms
            .lock()
            .keys()
            .filter(|k| k.starts_with(&format!("{name}/")))
            .cloned()
            .collect();
        for s in sessions {
            if let Some(tx) = self.terms.lock().remove(&s) {
                let _ = tx.try_send(TermOut::Gone);
            }
        }
        metrics::agent_samples().write().remove(name);
        self.update_slot_gauges();
        self.emit_agent(&snapshot);
        // Connections drop transiently (network blips, server restarts): every agent, pods
        // included, gets AGENT_GRACE to reconnect and reclaim its jobs. A pod that is really gone
        // (deleted, evicted, OOM-killed) is requeued right away.
        if snapshot.ephemeral && !snapshot.running.is_empty() {
            let gone = match self.k8s.get().and_then(Option::as_ref) {
                Some(k) => !k.agent_pod_alive(name).await,
                None => false,
            };
            if gone {
                self.requeue_agent_jobs(name, "agent pod is gone").await;
                self.agents.lock().remove(name);
            }
        }
    }

    async fn requeue_agent_jobs(&self, name: &str, reason: &str) {
        let jobs: Vec<String> = {
            let mut g = self.agents.lock();
            match g.get_mut(name) {
                Some(a) => {
                    a.busy = 0;
                    a.exclusive = false;
                    std::mem::take(&mut a.running)
                }
                None => vec![],
            }
        };
        for j in jobs {
            match self.store.requeue_job(j.clone(), format!("{reason} ({name})")).await {
                Ok(true) => tracing::info!(job = %j, agent = name, "job requeued"),
                Ok(false) => tracing::warn!(job = %j, agent = name, "job failed: out of retries"),
                Err(e) => tracing::error!(error = %e, job = %j, "requeue failed"),
            }
            self.logs.append(&j, 0, format!("\r\n\x1b[33m[kiln] {reason}; job requeued\x1b[0m\r\n").as_bytes());
            self.emit_job(&j).await;
        }
        self.update_slot_gauges();
        self.wake.notify();
    }

    async fn on_agent_msg(self: &Arc<Self>, peer: PeerId, msg: AgentMsg) -> anyhow::Result<()> {
        if let AgentMsg::Hello(h) = msg {
            return self.on_hello(peer, h).await;
        }
        let Some(agent) = self.agent_of(peer) else {
            anyhow::bail!("message from peer {peer} before hello");
        };
        let now = crate::util::now_unix();
        if let Some(a) = self.agents.lock().get_mut(&agent) {
            a.last_seen = now;
        }
        match msg {
            AgentMsg::Hello(_) => {}
            AgentMsg::Heartbeat { .. } | AgentMsg::JobStarted { .. } => {}
            AgentMsg::Sample(s) => {
                if let Some(a) = self.agents.lock().get_mut(&agent) {
                    a.sample = Some(s.clone());
                }
                self.record_sample(&agent, s);
            }
            AgentMsg::StepStarted { job, idx } => {
                if self.owns_job(peer, &job) {
                    self.store.step_update(job.clone(), idx, Status::Running, None).await?;
                    self.logs.step(&job, idx, "running");
                    self.emit_job(&job).await;
                }
            }
            AgentMsg::StepFinished { job, idx, status, exit_code } => {
                if self.owns_job(peer, &job) {
                    self.store.step_update(job.clone(), idx, status, exit_code).await?;
                    self.logs.step(&job, idx, status.as_str());
                    metrics::global().steps_finished.inc(&[status.as_str()]);
                    self.emit_job(&job).await;
                }
            }
            AgentMsg::JobSample { .. } => {}
            AgentMsg::JobFinished(r) => {
                if self.owns_job(peer, &r.job) {
                    self.job_done(&agent, r).await?;
                }
            }
            AgentMsg::Reject { job, reason } => {
                if self.owns_job(peer, &job) {
                    self.release_slot(&agent, &job);
                    self.declined.lock().insert((job.clone(), agent.clone()), Instant::now());
                    self.store.unclaim_job(job.clone(), format!("declined by {agent}: {reason}")).await?;
                    self.emit_job(&job).await;
                    self.wake.notify();
                }
            }
            AgentMsg::TermOutput { session, data } => {
                let key = format!("{agent}/{session}");
                let tx = self.terms.lock().get(&key).cloned();
                if let Some(tx) = tx {
                    let _ = tx.send(TermOut::Data(data)).await;
                }
            }
            AgentMsg::TermList { sessions } => {
                for info in &sessions {
                    crate::termsess::adopt_agent(self, &agent, info);
                }
            }
            AgentMsg::TermReplay { session, data } => {
                let key = format!("{agent}/{session}");
                let tx = self.terms.lock().get(&key).cloned();
                if let Some(tx) = tx {
                    let _ = tx.send(TermOut::Replay(data)).await;
                }
            }
            AgentMsg::TermExit { session, code, error } => {
                let key = format!("{agent}/{session}");
                let tx = self.terms.lock().remove(&key);
                if let Some(tx) = tx {
                    let _ = tx.send(TermOut::Exit { code, error }).await;
                }
            }
        }
        Ok(())
    }

    async fn on_hello(&self, peer: PeerId, h: Hello) -> anyhow::Result<()> {
        let cert_name = self.pending_peers.lock().get(&peer).cloned();
        let refuse = |reason: String| async move {
            tracing::warn!(peer, %reason, "agent refused");
            self.send_peer(peer, &ServerMsg::Refused { reason }).await;
        };
        if h.protocol != proto::VERSION {
            refuse(format!("protocol {} != server {}", h.protocol, proto::VERSION)).await;
            return Ok(());
        }
        let name = crate::pki::sanitize_name(&h.name);
        if cert_name.as_deref() != Some(name.as_str()) {
            refuse(format!("hello name {name} does not match certificate {cert_name:?}")).await;
            return Ok(());
        }
        // A newer session for the same agent replaces the old one.
        let old_peer = self.agents.lock().get(&name).and_then(|a| a.peer).filter(|p| *p != peer);
        if let Some(p) = old_peer {
            self.peers.lock().remove(&p);
            self.net.command(crate::net::Out::Disconnect { peer: p }).await;
        }
        let now = crate::util::now_unix();
        // Reconcile what the agent says it runs with what we think it runs.
        let db_running: Vec<Job> = self
            .store
            .running_jobs()
            .await?
            .into_iter()
            .filter(|j| j.agent.as_deref() == Some(name.as_str()))
            .collect();
        let mut keep = Vec::new();
        for j in &db_running {
            if h.running.contains(&j.id) {
                keep.push(j.id.clone());
            } else {
                self.store
                    .requeue_job(j.id.clone(), format!("agent {name} restarted without the job"))
                    .await?;
                self.emit_job(&j.id).await;
            }
        }
        for j in &h.running {
            if keep.contains(j) {
                continue;
            }
            // The server requeued this job while the agent was away, but the agent kept working:
            // take it back (claim) rather than throwing the work away. Only a job that has since
            // been taken by someone else (or settled) is cancelled on this agent.
            let executor = if h.ephemeral { "k8s" } else { "baremetal" };
            if self.store.claim_job(j.clone(), name.clone(), executor.into()).await? {
                tracing::info!(job = %j, agent = %name, "agent reclaimed a requeued job");
                self.launching.lock().remove(j);
                keep.push(j.clone());
                self.emit_job(j).await;
            } else {
                self.send_peer(peer, &ServerMsg::Cancel { job: j.clone(), reason: "job was reassigned".into() })
                    .await;
            }
        }
        let live = AgentLive {
            name: name.clone(),
            peer: Some(peer),
            labels: h.labels.clone(),
            slots: h.slots.max(1),
            hostname: h.hostname.clone(),
            os: h.os.clone(),
            arch: h.arch.clone(),
            version: h.version.clone(),
            ephemeral: h.ephemeral,
            connected: true,
            last_seen: now,
            enrolled_at: now,
            busy: u32::try_from(keep.len()).unwrap_or(u32::MAX),
            running: keep,
            draining: false,
            sample: None,
            terminal: h.terminal,
            dedicated_job: h.dedicated_job.clone(),
            exclusive: false,
            lost_at: None,
        };
        let draining = self.agents.lock().get(&name).is_some_and(|a| a.draining);
        let live = AgentLive { draining, ..live };
        self.store
            .upsert_agent(AgentRow {
                name: name.clone(),
                labels: h.labels,
                slots: live.slots,
                hostname: h.hostname,
                os: h.os,
                arch: h.arch,
                version: h.version,
                ephemeral: h.ephemeral,
                connected: true,
                last_seen: now,
                enrolled_at: now,
            })
            .await?;
        self.peers.lock().insert(peer, name.clone());
        self.pending_peers.lock().remove(&peer);
        self.agents.lock().insert(name.clone(), live.clone());
        if let Some(j) = &h.dedicated_job {
            self.launching.lock().remove(j);
        }
        self.send_peer(
            peer,
            &ServerMsg::Welcome {
                server_version: env!("CARGO_PKG_VERSION").into(),
                sample_seconds: self.cfg.telemetry.sample_seconds,
            },
        )
        .await;
        tracing::info!(agent = %name, labels = ?live.labels, slots = live.slots, "agent connected");
        self.update_slot_gauges();
        self.emit_agent(&live);
        self.wake.notify();
        Ok(())
    }

    fn release_slot(&self, agent: &str, job: &str) {
        let mut g = self.agents.lock();
        if let Some(a) = g.get_mut(agent) {
            a.running.retain(|j| j != job);
            a.busy = u32::try_from(a.running.len()).unwrap_or(0);
            if a.running.is_empty() {
                a.exclusive = false;
            }
        }
    }

    async fn job_done(&self, agent: &str, r: JobResult) -> anyhow::Result<()> {
        self.release_slot(agent, &r.job);
        self.cancel_sent.lock().remove(&r.job);
        let job = self.store.job(r.job.clone()).await?;
        let (run_id, settled) = self
            .store
            .finish_job(r.job.clone(), r.status, r.exit_code, r.error.clone(), r.peak_rss, r.cpu_seconds)
            .await?;
        self.logs.finish(&r.job);
        let m = metrics::global();
        let run = self.store.run(run_id.clone()).await?;
        let repo = run.as_ref().map(|r| r.repo_name.clone()).unwrap_or_default();
        let executor = job.as_ref().and_then(|j| j.executor.clone()).unwrap_or_default();
        m.jobs_finished.inc(&[&repo, r.status.as_str(), &executor]);
        if let Some(started) = job.as_ref().and_then(|j| j.started_at) {
            #[allow(clippy::cast_precision_loss)]
            m.job_duration.observe((crate::util::now_unix() - started) as f64);
        }
        tracing::info!(job = %r.job, agent, status = r.status.as_str(), "job finished");
        let snapshot = self.agents.lock().get(agent).cloned();
        if let Some(a) = snapshot {
            self.emit_agent(&a);
            if a.ephemeral {
                self.send_to_agent(agent, LANE_CONTROL, &ServerMsg::Drain).await;
            }
        }
        self.emit_job(&r.job).await;
        self.emit_run(&run_id).await;
        if let (Some(status), Some(run)) = (settled, run) {
            crate::gitops::report_status(self, &run, status).await;
        }
        self.update_slot_gauges();
        self.wake.notify();
        Ok(())
    }

    // ------------------------------------------------------------ runs

    pub async fn cancel_run(&self, run_id: &str, reason: &str) -> anyhow::Result<()> {
        let running = self.store.cancel_run(run_id.to_owned(), reason.to_owned()).await?;
        for (job, agent) in running {
            if let Some(agent) = agent {
                self.send_to_agent(&agent, LANE_CONTROL, &ServerMsg::Cancel { job: job.clone(), reason: reason.into() })
                    .await;
                self.release_slot(&agent, &job);
            }
            self.logs.finish(&job);
            self.emit_job(&job).await;
            if let Some(Some(k)) = self.k8s.get() {
                k.delete_job_pods(&job).await;
            }
        }
        self.emit_run(run_id).await;
        self.update_slot_gauges();
        self.wake.notify();
        Ok(())
    }

    // ------------------------------------------------------------ scheduler

    /// Assign queued jobs to agents (or launch pods) until nothing more fits. Runs on every wake
    /// and once a second as a safety net.
    pub async fn scheduler_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let mut tick = Interval::new(Duration::from_secs(1));
        let mut slow = Instant::now();
        loop {
            futures::select_biased! {
                () = self.shutdown.cancelled().fuse() => return Ok(()),
                () = self.wake.notified().fuse() => {}
                () = tick.tick().fuse() => {}
            }
            self.schedule_once().await?;
            if slow.elapsed() >= Duration::from_secs(5) {
                slow = Instant::now();
                self.check_timeouts().await?;
                self.check_lost_agents().await;
            }
        }
    }

    fn secrets(&self) -> Vec<(String, String)> {
        self.cfg
            .secrets
            .iter()
            .map(|(k, v)| {
                let val = match v.strip_prefix("file:") {
                    Some(path) => std::fs::read_to_string(path).map(|s| s.trim().to_owned()).unwrap_or_default(),
                    None => v.clone(),
                };
                (k.clone(), val)
            })
            .collect()
    }

    async fn schedule_once(&self) -> anyhow::Result<()> {
        let queued = self.store.queued_jobs(2000).await?;
        metrics::global().jobs_queued.set(i64::try_from(queued.len()).unwrap_or(i64::MAX));
        let running = i64::try_from(self.store.running_jobs().await?.len()).unwrap_or(0);
        metrics::global().jobs_running.set(running);
        if queued.is_empty() {
            return Ok(());
        }
        let mut runs = HashMap::new();
        let k8s = self.k8s.get().and_then(Option::as_ref);
        let mut pods_live = None;
        for job in queued {
            let Some(spec) = job.spec.clone() else { continue };
            if !runs.contains_key(&job.run_id) {
                let r = self.store.run(job.run_id.clone()).await?;
                runs.insert(job.run_id.clone(), r);
            }
            let Some(Some(run)) = runs.get(&job.run_id) else { continue };
            // `if:` conditions are evaluated against the run.
            if let Some(cond) = &spec.condition {
                let mut ctx = Context::default();
                ctx.set("branch", run.branch.clone());
                ctx.set("event", run.event.clone());
                ctx.set("sha", run.sha.clone());
                ctx.set("repo", run.repo_name.clone());
                if !ctx.eval(cond) {
                    self.store.claim_job(job.id.clone(), "-".into(), "none".into()).await?;
                    self.store.finish_job(job.id.clone(), Status::Skipped, None, Some(format!("if: {cond}")), None, None).await?;
                    self.emit_job(&job.id).await;
                    self.emit_run(&run.id).await;
                    continue;
                }
            }
            let chosen = self.pick_agent(&job);
            match chosen {
                Some(agent) => {
                    let executor = if job.labels.iter().any(|l| l == "k8s") && self.agents.lock().get(&agent).is_some_and(|a| a.ephemeral) {
                        "k8s"
                    } else {
                        "baremetal"
                    };
                    if !self.store.claim_job(job.id.clone(), agent.clone(), executor.into()).await? {
                        continue;
                    }
                    {
                        let mut g = self.agents.lock();
                        if let Some(a) = g.get_mut(&agent) {
                            a.running.push(job.id.clone());
                            a.busy = u32::try_from(a.running.len()).unwrap_or(u32::MAX);
                            a.exclusive = spec.exclusive;
                        }
                    }
                    if let Some(q) = job.queued_at {
                        #[allow(clippy::cast_precision_loss)]
                        metrics::global().queue_wait.observe((crate::util::now_unix() - q).max(0) as f64);
                    }
                    let clone_url = self
                        .store
                        .repos()
                        .await?
                        .into_iter()
                        .find(|r| r.id == run.repo_id)
                        .map(|r| r.clone_url)
                        .unwrap_or_default();
                    let assign = Assignment {
                        job_id: job.id.clone(),
                        run_id: run.id.clone(),
                        repo: run.repo_name.clone(),
                        clone_url,
                        sha: run.sha.clone(),
                        branch: run.branch.clone(),
                        event: run.event.clone(),
                        run_number: run.number,
                        attempt: job.attempt,
                        spec,
                        secrets: self.secrets(),
                        job_token: crate::api::job_token(&self.admin_token, &job.id),
                        server_url: self.server_url_for(&agent),
                    };
                    tracing::info!(job = %job.id, key = %job.key, agent = %agent, "job assigned");
                    let _rx = self.logs.subscribe(&job.id); // create the live channel early
                    if !self.send_to_agent(&agent, LANE_CONTROL, &ServerMsg::Assign(Box::new(assign))).await {
                        self.release_slot(&agent, &job.id);
                        self.store.requeue_job(job.id.clone(), "assignment could not be delivered".into()).await?;
                    }
                    self.emit_job(&job.id).await;
                    self.emit_run(&run.id).await;
                    self.update_slot_gauges();
                }
                None => {
                    let Some(k) = k8s else { continue };
                    if !job.labels.iter().any(|l| l == "k8s") {
                        continue;
                    }
                    let mut launched_at = self.launching.lock().get(&job.id).copied();
                    // After a server restart the launch table is empty, but the job's pod may
                    // already exist (Pending or about to connect): adopt it instead of duplicating.
                    if launched_at.is_none() && k.job_pod_alive(&job.id).await {
                        self.launching.lock().insert(job.id.clone(), Instant::now());
                        launched_at = Some(Instant::now());
                    }
                    let timeout = Duration::from_secs(u64::from(self.cfg.kubernetes.start_timeout_seconds));
                    if let Some(t) = launched_at {
                        if t.elapsed() < timeout {
                            continue;
                        }
                        // Still Pending for capacity (or pulling a large image) is not a failure:
                        // keep waiting instead of churning pods.
                        if k.job_pod_alive(&job.id).await {
                            self.launching.lock().insert(job.id.clone(), Instant::now());
                            continue;
                        }
                        tracing::warn!(job = %job.id, "pod agent did not connect in time; relaunching");
                        k.delete_job_pods(&job.id).await;
                    }
                    if pods_live.is_none() {
                        pods_live = Some(self.launching.lock().len());
                    }
                    let live = pods_live.unwrap_or(0);
                    if u32::try_from(live).unwrap_or(u32::MAX) >= self.cfg.kubernetes.max_pods {
                        continue;
                    }
                    match k.launch(self, &job, run).await {
                        Ok(pod) => {
                            tracing::info!(job = %job.id, pod, "launched agent pod");
                            self.launching.lock().insert(job.id.clone(), Instant::now());
                            pods_live = Some(live + 1);
                            metrics::global().k8s_pods_launched.inc(&["ok"]);
                        }
                        Err(e) => {
                            metrics::global().k8s_pods_launched.inc(&["error"]);
                            // Usually the jobs-namespace quota is full: retry in ~15 s rather
                            // than after the full start timeout.
                            let msg = format!("{e:#}");
                            if msg.contains("exceeded quota") {
                                tracing::debug!(job = %job.id, "job quota full; waiting for capacity");
                            } else {
                                tracing::error!(job = %job.id, error = %msg, "pod launch failed");
                            }
                            let retry_at = Instant::now()
                                .checked_sub(timeout.saturating_sub(Duration::from_secs(15)))
                                .unwrap_or_else(Instant::now);
                            self.launching.lock().insert(job.id.clone(), retry_at);
                        }
                    }
                }
            }
        }
        self.emit_queue();
        Ok(())
    }

    /// The HTTP base URL an agent should use: loopback for the in-process agent, the in-cluster
    /// service for pods, the public URL for everyone else.
    fn server_url_for(&self, agent: &str) -> String {
        let local = crate::pki::sanitize_name(&self.cfg.local_agent_name());
        if self.cfg.local_agent.enabled && agent == local {
            let port = self.cfg.server.http_addr.rsplit_once(':').map_or("7070", |(_, p)| p);
            return format!("http://127.0.0.1:{port}");
        }
        let ephemeral = self.agents.lock().get(agent).is_some_and(|a| a.ephemeral);
        if ephemeral
            && let Some(u) = &self.cfg.kubernetes.server_url {
                return u.clone();
            }
        self.cfg.server.public_url.clone()
    }

    fn pick_agent(&self, job: &Job) -> Option<String> {
        let declined = self.declined.lock().clone();
        let g = self.agents.lock();
        let exclusive = job.spec.as_ref().is_some_and(|s| s.exclusive);
        g.values()
            .filter(|a| a.connected && !a.draining && !a.exclusive)
            .filter(|a| !a.running.contains(&job.id))
            .filter(|a| {
                declined
                    .get(&(job.id.clone(), a.name.clone()))
                    .is_none_or(|t| t.elapsed() >= DECLINE_BACKOFF)
            })
            .filter(|a| match &a.dedicated_job {
                Some(d) => d == &job.id,
                None => true,
            })
            .filter(|a| job.labels.iter().all(|l| a.labels.contains(l) || l == &a.name))
            .filter(|a| if exclusive { a.running.is_empty() } else { a.busy < a.slots })
            // Least loaded first, then most free slots.
            .min_by(|a, b| {
                let la = f64::from(a.busy) / f64::from(a.slots.max(1));
                let lb = f64::from(b.busy) / f64::from(b.slots.max(1));
                la.total_cmp(&lb).then(b.slots.cmp(&a.slots))
            })
            .map(|a| a.name.clone())
    }

    async fn check_timeouts(&self) -> anyhow::Result<()> {
        let now = crate::util::now_unix();
        for job in self.store.running_jobs().await? {
            let Some(spec) = &job.spec else { continue };
            let Some(started) = job.started_at else { continue };
            let limit = i64::from(spec.timeout_minutes) * 60;
            if now - started <= limit {
                continue;
            }
            let sent = self.cancel_sent.lock().get(&job.id).copied();
            match sent {
                None => {
                    if let Some(agent) = &job.agent {
                        self.send_to_agent(agent, LANE_CONTROL, &ServerMsg::Cancel { job: job.id.clone(), reason: "timeout".into() })
                            .await;
                    }
                    self.cancel_sent.lock().insert(job.id.clone(), Instant::now());
                }
                Some(t) if t.elapsed() > Duration::from_secs(60) => {
                    // The agent did not report back: settle it ourselves.
                    if let Some(agent) = &job.agent {
                        self.release_slot(agent, &job.id);
                    }
                    self.cancel_sent.lock().remove(&job.id);
                    let (run_id, _) = self
                        .store
                        .finish_job(job.id.clone(), Status::TimedOut, None, Some(format!("exceeded {} min", spec.timeout_minutes)), None, None)
                        .await?;
                    self.logs.finish(&job.id);
                    self.emit_job(&job.id).await;
                    self.emit_run(&run_id).await;
                }
                Some(_) => {}
            }
        }
        // Jobs whose agent session vanished entirely (e.g. server restart, agent never returned).
        Ok(())
    }

    async fn check_lost_agents(&self) {
        let lost: Vec<String> = self
            .agents
            .lock()
            .values()
            .filter(|a| !a.connected && a.lost_at.is_some_and(|t| t.elapsed() > AGENT_GRACE) && !a.running.is_empty())
            .map(|a| a.name.clone())
            .collect();
        for name in lost {
            self.requeue_agent_jobs(&name, "agent did not reconnect").await;
        }
        self.agents.lock().retain(|_, a| {
            !(a.ephemeral && !a.connected && a.running.is_empty() && a.lost_at.is_some_and(|t| t.elapsed() > AGENT_GRACE))
        });
        self.declined.lock().retain(|_, t| t.elapsed() < DECLINE_BACKOFF);
        // Running jobs assigned to agents we have no session for at all (after a server restart).
        if self.started.elapsed() > AGENT_GRACE
            && let Ok(running) = self.store.running_jobs().await {
                // Jobs held by a connected agent, or by one still inside its reconnect grace, are
                // not orphans.
                let known: HashSet<String> = self
                    .agents
                    .lock()
                    .values()
                    .filter(|a| a.connected || a.lost_at.is_some_and(|t| t.elapsed() <= AGENT_GRACE))
                    .flat_map(|a| a.running.clone())
                    .collect();
                for j in running {
                    if !known.contains(&j.id) && j.agent.as_deref() != Some("-")
                        && let Ok(true) = self.store.requeue_job(j.id.clone(), "orphaned after restart".into()).await {
                            self.emit_job(&j.id).await;
                        }
                }
            }
        // Expire unused pod enrollment tokens.
        self.ephemeral_tokens.lock().retain(|_, (_, exp)| *exp > Instant::now());
    }

    /// Sample the server host itself for the UI ("server" series).
    pub async fn sampler_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let mut sampler = crate::host::Sampler::new(Some(&self.cfg.server.data_dir));
        let mut tick = Interval::new(Duration::from_secs(u64::from(self.cfg.telemetry.sample_seconds.max(1))));
        loop {
            futures::select_biased! {
                () = self.shutdown.cancelled().fuse() => return Ok(()),
                () = tick.tick().fuse() => {}
            }
            let s = sampler.sample();
            self.record_sample("server", s);
        }
    }

    /// Daily retention: delete old runs and their logs.
    /// Close terminal sessions left idle with nobody attached; forget ended ones.
    pub async fn terminal_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let idle = Duration::from_secs(self.cfg.terminal.idle_minutes.max(1).saturating_mul(60));
        loop {
            if !self.shutdown.sleep(Duration::from_secs(30)).await {
                return Ok(());
            }
            if self.terminals.sweep(idle) {
                self.emit_terminals();
            }
        }
    }

    pub async fn retention_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let mut tick = Interval::new(Duration::from_secs(3600));
        loop {
            futures::select_biased! {
                () = self.shutdown.cancelled().fuse() => return Ok(()),
                () = tick.tick().fuse() => {}
            }
            let jobs = self.store.prune(self.cfg.retention.days).await?;
            if !jobs.is_empty() {
                tracing::info!(jobs = jobs.len(), "pruned old runs");
                self.logs.remove(jobs).await;
            }
        }
    }
}
