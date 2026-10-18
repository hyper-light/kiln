//! The server core: agent sessions, the scheduler, job bookkeeping and the UI event bus.
//!
//! All network input arrives as [`NetEvent`]s from the QUIC thread and is handled here; every
//! outcome is persisted through [`Store`] before it is broadcast, so a crash at any point loses
//! at most in-flight log bytes, never job state.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
/// How often the newest runner process is reminded that an older one can be retired.
const RETIRE_REPEAT: Duration = Duration::from_secs(60);
/// Host samples kept per agent for the UI charts (1 h at 5 s).
const HISTORY: usize = 720;
/// How long an agent that declined a job is not offered that job again.
const DECLINE_BACKOFF: Duration = Duration::from_secs(15);
/// How soon a refused pod launch is tried again.
const LAUNCH_RETRY: Duration = Duration::from_secs(15);

/// Where a Kubernetes job's pod stands until its agent connects.
#[derive(Debug, Clone, Copy)]
pub enum Launch {
    /// A pod was created (or adopted after a restart) at this instant.
    Pod(Instant),
    /// No pod is on its way: launch again from `at`. `check_pods` when the failed attempt may
    /// have left a pod behind (an API timeout) or a dead one may hold the name.
    Retry { at: Instant, check_pods: bool },
}

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
    /// The machine's runner processes, while it has more than one (a runner replaced without a
    /// restart: the old one keeps its terminals until they end). See `crate::instances`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub instances: Vec<crate::instances::InstanceView>,
    /// How its connection reaches the server, while connected (`GET /api/agents`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<crate::tunnel::Transport>,
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
    /// The CI engine's dispatcher (docs/ci.md W05): its runs, legs and their messages.
    pub ci: crate::ci::dispatch::Handle,
    peers: Mutex<HashMap<PeerId, String>>,
    pending_peers: Mutex<HashMap<PeerId, String>>,
    /// Runners' HTTP tunnels, and which QUIC connections they carry (see `crate::tunnel`).
    pub tunnels: Arc<crate::tunnel::Registry>,
    pub events: Broadcast<Arc<str>>,
    pub terms: Mutex<HashMap<String, async_channel::Sender<TermOut>>>,
    /// Every terminal session, local and on machines.
    pub terminals: crate::termsess::Sessions,
    /// Every kiln-native agent session, local and on machines.
    pub harness: crate::harness::registry::Registry,
    /// The AI coding sessions each connected machine last reported.
    pub ai_sessions: Mutex<HashMap<String, Vec<crate::aisessions::AiSession>>>,
    /// Transcript requests waiting for their machine's answer, by request number.
    ai_requests: Mutex<HashMap<u64, futures::channel::oneshot::Sender<Option<String>>>>,
    ai_seq: AtomicU64,
    pub history: Mutex<HashMap<String, VecDeque<HostSample>>>,
    pub wake: Notify,
    pub launching: Mutex<HashMap<String, Launch>>,
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
    /// Each machine's runner processes and what each one holds (see `crate::instances`).
    pub instances: Mutex<crate::instances::Instances>,
    /// Runner processes that went away while their machine stayed connected through another
    /// one, with when and the jobs they held (requeued if they do not come back in time).
    lost_instances: Mutex<HashMap<PeerId, (String, Instant, Vec<String>)>>,
    /// Upgrades of runners and of this server, started from Settings → Updates.
    pub updates: crate::updates::Updates,
    /// Runners' front doors: their settings and what each last reported (`crate::frontdoor`).
    pub front_doors: crate::frontdoor::control::FrontDoors,
    /// The tailnet's split DNS for the front doors' names (`crate::frontdoor::tailnet`).
    pub tailnet: crate::frontdoor::tailnet::TailnetDns,
    /// Repo sync: every synced repo's group, led from here (`crate::sync`).
    pub sync: crate::sync::server::Sync,
    /// Reach: the links between runners this server relays, and their identities (`crate::reach`).
    pub reach: Mutex<crate::reach::relay::Relay>,
    /// The git repositories each machine holds, as it last said (`crate::sync::places`).
    pub places: crate::sync::places::Places,
    /// Clones, syncs and moves of repositories between machines (`crate::sync::tasks`).
    pub tasks: crate::sync::tasks::Tasks,
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
        let harness_dir = cfg.data("harness");
        let sync = crate::sync::server::Sync::start(cfg.data("sync"), admin_token.clone());
        let places = crate::sync::places::Places::open(cfg.data("sync").join("places.json"));
        let tasks = crate::sync::tasks::Tasks::open(&cfg.data("sync"));
        Arc::new(Self {
            cfg,
            store,
            logs,
            net,
            pki,
            agents: Mutex::new(HashMap::new()),
            ci: crate::ci::dispatch::Handle::default(),
            peers: Mutex::new(HashMap::new()),
            pending_peers: Mutex::new(HashMap::new()),
            tunnels: Arc::default(),
            events: Broadcast::new(4096),
            terms: Mutex::new(HashMap::new()),
            terminals: crate::termsess::Sessions::new(),
            harness: crate::harness::registry::Registry::new(harness_dir),
            ai_sessions: Mutex::new(HashMap::new()),
            ai_requests: Mutex::new(HashMap::new()),
            ai_seq: AtomicU64::new(1),
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
            instances: Mutex::new(crate::instances::Instances::default()),
            lost_instances: Mutex::new(HashMap::new()),
            updates: crate::updates::Updates::default(),
            front_doors: crate::frontdoor::control::FrontDoors::default(),
            tailnet: crate::frontdoor::tailnet::TailnetDns::default(),
            sync,
            reach: Mutex::new(crate::reach::relay::Relay::default()),
            places,
            tasks,
        })
    }

    /// A job of kiln's own release pipeline succeeded (`[updates]`): the binaries it built are what
    /// each other platform's runners upgrade to now.
    async fn adopt_release(&self, job: &str, run: &crate::store::Run) {
        // The server's commit, or the newest one a runner runs (`crate::fleet`).
        let newest = self.updates.fleet.target().map(|t| t.commit).unwrap_or_default();
        if !crate::updates::adopts(&self.cfg.updates, run, crate::api::COMMIT) && (newest.is_empty() || !crate::updates::adopts(&self.cfg.updates, run, &newest)) {
            return;
        }
        let path = crate::api::artifact_path(self, job);
        if !path.exists() {
            return;
        }
        let (cfg, run_c) = (self.cfg.clone(), run.clone());
        let adopted = crate::rt::blocking(move || crate::updates::adopt_into(&cfg, &path, &run_c)).await;
        match adopted {
            Ok(Ok(kept)) if !kept.is_empty() => {
                let targets: Vec<&str> = kept.iter().map(|b| b.target.as_str()).collect();
                tracing::info!(run = run.number, targets = ?targets, "runners' binaries built by the release pipeline");
                self.store.audit("kiln".into(), "upgrade.binary".into(), format!("built by {} run #{} ({}): {}", run.pipeline, run.number, run.sha.get(..12).unwrap_or(&run.sha), targets.join(", "))).await;
                self.emit(&serde_json::json!({ "type": "updates" }));
                self.updates.fleet.wake();
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) | Err(e) => tracing::warn!(run = run.number, error = %format!("{e:#}"), "the release pipeline's binaries were not kept"),
        }
    }

    /// Sends what repo sync says to each machine's syncing runner process, on the sync lane.
    pub async fn sync_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let out = self.sync.outgoing();
        while let Ok(o) = out.recv().await {
            let peer = self.instances.lock().newest_with(&o.machine, proto::CAP_SYNC);
            let Some(peer) = peer else { continue };
            let msg = ServerMsg::Sync { json: crate::sync::wire::encode(&o.msg), raft: o.raft };
            if let Ok(b) = proto::encode(&msg) {
                self.net.send(peer, proto::LANE_SYNC, Kind::Control, b).await;
            }
        }
        Ok(())
    }

    /// What a machine's runner said on the sync lane: of its copies (to the groups), of the git
    /// repositories it holds, or of its part of a task.
    fn sync_heard(&self, machine: &str, json: &str, raft: Vec<u8>) {
        match crate::sync::wire::decode(json) {
            Some(crate::sync::wire::Msg::Repos(r)) => {
                if self.places.heard(machine, r) {
                    self.emit(&serde_json::json!({ "type": "places" }));
                }
            }
            Some(crate::sync::wire::Msg::Task(news)) => self.tasks.heard(machine, news),
            Some(msg) => self.sync.from_runner(machine, msg, raft),
            None => {}
        }
    }

    /// A runner process of `machine` that syncs said so: the newest such process syncs, and every
    /// other one lets go.
    async fn sync_announced(&self, machine: &str) {
        let (newest, others) = {
            let inst = self.instances.lock();
            let newest = inst.newest_with(machine, proto::CAP_SYNC);
            let others: Vec<PeerId> = inst.of(machine).into_iter().filter(|p| Some(*p) != newest && inst.get(*p).is_some_and(|i| i.caps & proto::CAP_SYNC != 0)).collect();
            (newest, others)
        };
        let release = ServerMsg::Sync { json: crate::sync::wire::encode(&crate::sync::wire::Msg::Release), raft: Vec::new() };
        if let Ok(b) = proto::encode(&release) {
            for p in others {
                self.net.send(p, proto::LANE_SYNC, Kind::Control, b.clone()).await;
            }
        }
        self.sync.machine(machine, newest.is_some());
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
            crate::observe::hub::host_sample(agent, &s);
        } else {
            let host = crate::os::hostname();
            // The server's machine, unless its own runner reports it already.
            if !self.cfg.local_agent.enabled {
                crate::observe::hub::host_sample(&host, &s);
            }
            if let Some(o) = crate::observe::api::get() {
                crate::observe::hub::process_figures(o, &host, "server");
            }
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
                    instances: vec![],
                    transport: None,
                },
            })
            .collect();
        for a in &mut out {
            a.instances = self.instance_views(&a.name);
        }
        for (name, l) in &live {
            if !out.iter().any(|a| &a.name == name) {
                out.push(l.clone());
            }
        }
        for a in &mut out {
            a.transport = a.peer.filter(|_| a.connected).map(|p| self.tunnels.transport(p));
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

    /// The recent messages of the AI session running as `pid` on a machine, as JSON (`None`: it
    /// has no transcript).
    pub async fn ai_transcript(&self, machine: &str, pid: u32) -> anyhow::Result<Option<String>> {
        self.ai_ask(machine, |req| ServerMsg::AiTranscript { req, pid }).await
    }

    /// The normalized view (events, state, changeset) of an AI session running on a machine.
    pub async fn ai_view(&self, machine: &str, pid: u32) -> anyhow::Result<Option<String>> {
        self.ai_ask(machine, |req| ServerMsg::AiView { req, pid }).await
    }

    /// The output of a task of an agent session running on a machine (JSON).
    pub async fn harness_task_output(&self, machine: &str, session: &str, task: &str, since: Option<u64>) -> anyhow::Result<Option<String>> {
        let (session, task) = (session.to_owned(), task.to_owned());
        self.ai_ask(machine, move |req| ServerMsg::HarnessTaskOutput { req, session, task, since }).await
    }

    /// The folders at `path` on a machine, as JSON [`crate::dirs::Listing`], read by its newest
    /// runner process. Only one that announced [`proto::CAP_DIRS`] is sent [`ServerMsg::DirList`]:
    /// `Ok(None)` when it is too old to list folders.
    pub async fn dir_list(&self, machine: &str, path: &str) -> anyhow::Result<Option<String>> {
        let peer = {
            let inst = self.instances.lock();
            if inst.newest(machine).is_none() {
                anyhow::bail!("{machine} is not connected");
            }
            inst.newest_with(machine, proto::CAP_DIRS)
        };
        let Some(peer) = peer else { return Ok(None) };
        let req = self.ai_seq.fetch_add(1, Ordering::Relaxed);
        let frame = proto::encode(&ServerMsg::DirList { req, path: path.to_owned() })?;
        let (tx, rx) = futures::channel::oneshot::channel();
        self.ai_requests.lock().insert(req, tx);
        let sent = self.net.send(peer, LANE_TERM, Kind::Term, frame).await;
        let answer = if sent { crate::rt::timeout(crate::dirs::WAIT, rx).await.ok() } else { None };
        self.ai_requests.lock().remove(&req);
        match answer {
            Some(Ok(Some(json))) => Ok(Some(json)),
            Some(_) => anyhow::bail!("{machine} did not answer"),
            None if !sent => anyhow::bail!("{machine} is not connected"),
            None => anyhow::bail!("{machine} did not answer in time"),
        }
    }

    /// A machine's past sessions (JSON `[PastSession]`).
    pub async fn harness_history(&self, machine: &str, kind: Option<String>, limit: u32) -> anyhow::Result<Option<String>> {
        self.ai_ask(machine, move |req| ServerMsg::HarnessHistory { req, kind, limit }).await
    }

    async fn ai_ask(&self, machine: &str, msg: impl FnOnce(u64) -> ServerMsg) -> anyhow::Result<Option<String>> {
        let req = self.ai_seq.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = futures::channel::oneshot::channel();
        self.ai_requests.lock().insert(req, tx);
        if !self.send_to_agent(machine, LANE_TERM, &msg(req)).await {
            self.ai_requests.lock().remove(&req);
            anyhow::bail!("machine {machine} is not connected");
        }
        let answer = crate::rt::timeout(Duration::from_secs(10), rx).await;
        self.ai_requests.lock().remove(&req);
        match answer {
            Ok(Ok(json)) => Ok(json),
            Ok(Err(_)) => anyhow::bail!("{machine} did not answer"),
            Err(_) => anyhow::bail!("{machine} did not answer in time"),
        }
    }

    /// Send to a machine. While it runs more than one runner process (see `crate::instances`),
    /// a message about something one of them holds (a terminal, an agent session, a job) goes to
    /// that one, anything new to the newest, and Drain to all of them.
    pub async fn send_to_agent(&self, agent: &str, lane: u32, msg: &ServerMsg) -> bool {
        let mut peers: Vec<PeerId> = {
            let mut inst = self.instances.lock();
            if matches!(msg, ServerMsg::Drain) { inst.of(agent) } else { inst.route(agent, msg).into_iter().collect() }
        };
        if peers.is_empty() {
            peers.extend(self.agents.lock().get(agent).and_then(|a| a.peer));
        }
        if peers.is_empty() {
            return false;
        }
        let kind = if lane == LANE_TERM { Kind::Term } else { Kind::Control };
        let b = match proto::encode(msg) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "encode failed");
                return false;
            }
        };
        let mut sent = false;
        for p in peers {
            sent |= self.net.send(p, lane, kind, b.clone()).await;
        }
        sent
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
        let mut peers: Vec<PeerId> = self.agents.lock().remove(agent).and_then(|a| a.peer).into_iter().collect();
        peers.extend(self.instances.lock().of(agent));
        peers.sort_unstable();
        peers.dedup();
        for p in peers {
            self.net.command(crate::net::Out::Disconnect { peer: p }).await;
        }
        self.net.command(crate::net::Out::Revoke { name: agent.to_owned() }).await;
        self.reach.lock().forget(agent);
        metrics::agent_samples().write().remove(agent);
        self.store.delete_agent(agent.to_owned()).await?;
        self.front_doors.forget(&self.store, agent).await;
        self.places.forget(agent);
        self.tailnet.wake.notify();
        self.update_slot_gauges();
        Ok(())
    }

    // ------------------------------------------------------------ reach

    /// Reach's directory (`crate::reach::directory`): every runner that is not a job pod — what it
    /// is, how busy it is, its harnesses, its reach identity and whether it takes links.
    pub fn reach_directory(&self) -> Vec<crate::reach::directory::RunnerInfo> {
        let kinds = self.harness.kinds();
        let relay = self.reach.lock();
        let agents = self.agents.lock();
        let mut out: Vec<crate::reach::directory::RunnerInfo> = agents
            .values()
            .filter(|a| !a.ephemeral)
            .map(|a| {
                let ann = relay.announced(&a.name);
                let s = a.sample.as_ref();
                crate::reach::directory::RunnerInfo {
                    name: a.name.clone(),
                    hostname: a.hostname.clone(),
                    os: a.os.clone(),
                    arch: a.arch.clone(),
                    labels: a.labels.clone(),
                    connected: a.connected,
                    reach: a.connected && ann.is_some_and(|x| x.accept),
                    fingerprint: ann.map(|x| x.fingerprint.clone()),
                    cpus: s.map_or(0, |s| s.cpus),
                    cpu_pct: s.map_or(0.0, |s| s.cpu_pct),
                    load1: s.map_or(0.0, |s| s.load1),
                    mem_total: s.map_or(0, |s| s.mem_total),
                    mem_used: s.map_or(0, |s| s.mem_used),
                    slots: a.slots,
                    busy: a.busy,
                    draining: a.draining,
                    harnesses: kinds.get(&format!("agent:{}", a.name)).map(|ks| ks.iter().filter(|k| k.available).map(|k| k.id.clone()).collect()).unwrap_or_default(),
                    via: a.transport.map(|t| t.to_string()).unwrap_or_default(),
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
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
            NetEvent::Connected { peer, name, binding } => {
                self.tunnels.connected(peer, binding);
                if let Some(n) = name {
                    self.pending_peers.lock().insert(peer, n);
                }
            }
            NetEvent::Closed { peer, reason } | NetEvent::Unreachable { peer, reason } => {
                self.tunnels.closed(peer);
                // Its reach links end; the runners at their other ends hear so (and resume later).
                let closes = self.reach.lock().lost(peer, "disconnected from the kiln server");
                for (p, m) in closes {
                    if let Ok(b) = proto::encode(&m) {
                        self.net.send(p, proto::LANE_REACH, Kind::Term, b).await;
                    }
                }
                self.pending_peers.lock().remove(&peer);
                let name = self.peers.lock().remove(&peer);
                let lost = self.instances.lock().lost(peer);
                if let Some(name) = name {
                    // Sync moves to another runner process of the machine that syncs, if any.
                    let syncing = self.instances.lock().newest_with(&name, proto::CAP_SYNC).is_some();
                    self.sync.machine(&name, syncing);
                    crate::observe::hub::lost(self, &name);
                    if self.instances.lock().of(&name).is_empty() {
                        self.agent_lost(&name, &reason).await;
                    } else {
                        self.instance_lost(&name, peer, lost.map(|(_, owned)| owned).unwrap_or_default(), &reason).await;
                    }
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
        // Its agent sessions keep running there too; they are announced again on reconnect.
        self.harness.machine_lost(self, name);
        if self.ai_sessions.lock().remove(name).is_some() {
            self.emit(&serde_json::json!({ "type": "ai_sessions", "machine": name }));
        }
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
        if jobs.iter().any(|j| self.ci.is_leg(j)) {
            self.ci.send(crate::ci::dispatch::Msg::RunnerLost { agent: name.to_owned() });
        }
        for j in jobs.into_iter().filter(|j| !self.ci.is_leg(j)) {
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
        // Says which runner process of its machine this is; precedes Hello.
        if let AgentMsg::Instance(ih) = msg {
            self.instances.lock().announce(peer, ih);
            return Ok(());
        }
        let Some(agent) = self.agent_of(peer) else {
            anyhow::bail!("message from peer {peer} before hello");
        };
        let now = crate::util::now_unix();
        if let Some(a) = self.agents.lock().get_mut(&agent) {
            a.last_seen = now;
        }
        match msg {
            AgentMsg::Hello(_) | AgentMsg::Instance(_) => {}
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
                    if self.ci.is_leg(&r.job) {
                        self.ci.send(crate::ci::dispatch::Msg::Finished { agent: agent.clone(), result: r });
                    } else {
                        self.job_done(&agent, r).await?;
                    }
                }
            }
            AgentMsg::Reject { job, .. } if self.ci.is_leg(&job) => {
                if self.owns_job(peer, &job) {
                    self.ci.send(crate::ci::dispatch::Msg::Rejected { agent: agent.clone(), leg: job });
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
                {
                    let mut inst = self.instances.lock();
                    for info in &sessions {
                        inst.own(crate::instances::Owned::Terminal(info.session.clone()), peer);
                    }
                }
                for info in &sessions {
                    crate::termsess::adopt_agent(self, &agent, info);
                }
            }
            // One report per machine: its newest runner process's.
            AgentMsg::AiSessions { .. } if self.instances.lock().newest(&agent).is_some_and(|n| n != peer) => {}
            AgentMsg::AiSessions { json } => match serde_json::from_str::<Vec<crate::aisessions::AiSession>>(&json) {
                Ok(sessions) => {
                    let old = self.ai_sessions.lock().insert(agent.clone(), sessions.clone());
                    if old.as_ref() != Some(&sessions) {
                        self.emit(&serde_json::json!({ "type": "ai_sessions", "machine": agent }));
                    }
                }
                Err(e) => tracing::debug!(agent = %agent, error = %e, "unreadable AI session report"),
            },
            AgentMsg::AiTranscript { req, json } => {
                if let Some(tx) = self.ai_requests.lock().remove(&req) {
                    let _ = tx.send(json);
                }
            }
            AgentMsg::AiView { req, json } | AgentMsg::HarnessTaskOutput { req, json } | AgentMsg::HarnessHistory { req, json } => {
                if let Some(tx) = self.ai_requests.lock().remove(&req) {
                    let _ = tx.send(json);
                }
            }
            AgentMsg::DirList { req, json } => {
                if let Some(tx) = self.ai_requests.lock().remove(&req) {
                    let _ = tx.send(Some(json));
                }
            }
            AgentMsg::Build { build, caps } => {
                self.instances.lock().set_build(peer, build.clone(), caps);
                self.updates.on_build(self, &agent, &build);
                self.updates.fleet.wake();
                self.emit(&serde_json::json!({ "type": "updates" }));
                crate::frontdoor::control::on_build(self, &agent, caps).await;
                if caps & proto::CAP_SYNC != 0 {
                    self.sync_announced(&agent).await;
                }
                if caps & proto::CAP_OBSERVE != 0 {
                    crate::observe::hub::announced(self, &agent).await;
                }
                // What it can do decides which legs it may take.
                self.ci.send(crate::ci::dispatch::Msg::RunnerUp { agent: agent.clone(), running: Vec::new() });
            }
            AgentMsg::Sync { json, raft } => self.sync_heard(&agent, &json, raft),
            AgentMsg::Observe { json, bytes } => crate::observe::hub::heard(&agent, json, bytes),
            AgentMsg::FrontDoorStatus { json } => crate::frontdoor::control::on_status(self, peer, &agent, &json).await,
            // CI (docs/ci.md §2.5): the dispatcher's; facts are W13's.
            AgentMsg::JobEvent { job, json } => {
                if self.owns_job(peer, &job) {
                    self.ci.send(crate::ci::dispatch::Msg::Event { agent: agent.clone(), leg: job, json });
                }
            }
            // The kiln it runs (`crate::fleet`); the rest is W13's.
            AgentMsg::Facts { json } => match serde_json::from_str::<kiln_ci::facts::RunnerFacts>(&json) {
                Ok(f) => {
                    if let Some(k) = f.kiln {
                        crate::fleet::remember(self, &k.build, &k.target, &k.commit);
                        self.instances.lock().set_kiln(peer, k);
                        self.updates.fleet.wake();
                    }
                }
                Err(e) => tracing::debug!(agent = %agent, error = %e, "unreadable runner facts"),
            },
            AgentMsg::ReachHello { json } => {
                let pod = self.agents.lock().get(&agent).is_none_or(|a| a.ephemeral);
                match serde_json::from_str::<crate::reach::relay::Announce>(&json) {
                    Ok(a) if !pod => {
                        tracing::info!(agent = %agent, fingerprint = %crate::reach::e2e::short(&a.fingerprint), accept = a.accept, "reach identity");
                        self.reach.lock().announce(&agent, a);
                    }
                    Ok(_) => {}
                    Err(e) => tracing::debug!(agent = %agent, error = %e, "unreadable reach announcement"),
                }
            }
            AgentMsg::Reach { to, link, open, data, close } => {
                // Job pods neither send nor receive reach links.
                if self.agents.lock().get(&agent).is_none_or(|a| a.ephemeral) {
                    return Ok(());
                }
                let out = {
                    let (agents, instances) = (&self.agents, &self.instances);
                    self.reach.lock().forward(&agent, peer, crate::reach::relay::Frame { to, link, open, data, close }, |n| {
                        if agents.lock().get(n).is_none_or(|a| a.ephemeral || !a.connected) {
                            return None;
                        }
                        instances.lock().newest_with(n, proto::CAP_REACH)
                    })
                };
                for (p, m) in out {
                    if let Ok(b) = proto::encode(&m) {
                        self.net.send(p, proto::LANE_REACH, Kind::Term, b).await;
                    }
                }
            }
            AgentMsg::ReachAsk { req, json } => {
                let op = serde_json::from_str::<serde_json::Value>(&json).ok().and_then(|v| v.get("op").and_then(serde_json::Value::as_str).map(str::to_owned));
                let answer = match op.as_deref() {
                    Some("directory") => serde_json::json!({ "runners": self.reach_directory() }),
                    _ => serde_json::json!({ "error": "unknown reach question" }),
                };
                self.send_peer(peer, &ServerMsg::ReachAnswer { req, json: answer.to_string() }).await;
            }
            AgentMsg::UpgradeLog { job, line } => self.updates.on_log(self, &job, line),
            AgentMsg::UpgradeDone { job, ok, error } => self.updates.on_done(self, &job, ok, error),
            AgentMsg::Harness { session, json } => match serde_json::from_str::<crate::harness::core::Update>(&json) {
                Ok(u) => {
                    let closed = matches!(u, crate::harness::core::Update::Closed);
                    {
                        let mut inst = self.instances.lock();
                        let what = crate::instances::Owned::Agent(session.clone());
                        if closed {
                            inst.disown(&what);
                        } else if inst.owner(&what) != Some(peer) {
                            inst.own(what, peer);
                        }
                    }
                    self.harness.ingest(self, &crate::harness::registry::Location::Machine(agent.clone()), &session, u);
                    if closed {
                        self.check_retire().await;
                    }
                }
                Err(e) => tracing::debug!(%agent, error = %e, "unreadable agent session update"),
            },
            AgentMsg::HarnessInventory { json } => match serde_json::from_str::<crate::harness::host::Inventory>(&json) {
                Ok(mut inv) => {
                    // This process's sessions are its own; another process of the machine keeps
                    // the ones it holds (the registry forgets sessions a report leaves out).
                    let others: Vec<String> = {
                        let mut inst = self.instances.lock();
                        for id in &inv.sessions {
                            inst.own(crate::instances::Owned::Agent(id.clone()), peer);
                        }
                        inst.of(&agent)
                            .into_iter()
                            .filter(|p| *p != peer)
                            .flat_map(|p| inst.owned_by(p))
                            .filter_map(|o| match o {
                                crate::instances::Owned::Agent(id) => Some(id),
                                _ => None,
                            })
                            .collect()
                    };
                    inv.sessions.extend(others);
                    self.harness.inventory(self, &agent, inv);
                }
                Err(e) => tracing::debug!(%agent, error = %e, "unreadable agent session inventory"),
            },
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
                self.instances.lock().disown(&crate::instances::Owned::Terminal(session));
                self.check_retire().await;
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
        let now = crate::util::now_unix();
        // A machine may connect more than one runner process (a new one beside an old one that
        // keeps its terminals, see `crate::instances`). The same process on a new connection
        // replaces its stale one.
        let joined = self.instances.lock().join(peer, &name, h.clone(), now);
        for p in &joined.stale {
            self.peers.lock().remove(p);
            self.lost_instances.lock().remove(p);
            self.net.command(crate::net::Out::Disconnect { peer: *p }).await;
        }
        self.lost_instances.lock().remove(&peer);
        let others: Vec<PeerId> = self.instances.lock().of(&name).into_iter().filter(|p| *p != peer).collect();
        let held_by_other = |job: &str| {
            self.instances.lock().owner(&crate::instances::Owned::Job(job.to_owned())).is_some_and(|o| o != peer && others.contains(&o))
        };
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
            } else if held_by_other(&j.id) {
                // Another runner process of this machine runs it.
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
            instances: Vec::new(),
            transport: None,
        };
        let prev = self.agents.lock().get(&name).cloned();
        let draining = prev.as_ref().is_some_and(|a| a.draining);
        {
            let mut inst = self.instances.lock();
            for j in &live.running {
                inst.own(crate::instances::Owned::Job(j.clone()), peer);
            }
        }
        // The machine as a whole: its newest process describes it; its jobs are every process's.
        let mut running = live.running.clone();
        for j in prev.iter().flat_map(|a| a.running.iter()) {
            if !running.contains(j) && held_by_other(j) {
                running.push(j.clone());
            }
        }
        let busy = u32::try_from(running.len()).unwrap_or(u32::MAX);
        let live = match prev {
            Some(prev) if !joined.newest => AgentLive { connected: true, last_seen: now, running, busy, lost_at: None, ..prev },
            _ => AgentLive { draining, running, busy, ..live },
        };
        self.store
            .upsert_agent(AgentRow {
                name: name.clone(),
                labels: live.labels.clone(),
                slots: live.slots,
                hostname: live.hostname.clone(),
                os: live.os.clone(),
                arch: live.arch.clone(),
                version: live.version.clone(),
                ephemeral: live.ephemeral,
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
        self.ci.send(crate::ci::dispatch::Msg::RunnerUp { agent: name.clone(), running: h.running.clone() });
        // Older processes of this machine take no new jobs; what they run, they keep.
        for p in &joined.drain {
            tracing::info!(agent = %name, peer = p, "a newer runner process of this machine connected; the older one keeps what it runs and takes nothing new");
            self.send_peer(*p, &ServerMsg::Drain).await;
        }
        self.update_slot_gauges();
        self.emit_agent(&live);
        self.wake.notify();
        self.check_retire().await;
        Ok(())
    }

    /// One runner process of a machine went away while another keeps the machine connected (see
    /// `crate::instances`): what it held comes back when it reconnects, like a machine's.
    async fn instance_lost(&self, name: &str, peer: PeerId, owned: Vec<crate::instances::Owned>, reason: &str) {
        use crate::instances::Owned;
        tracing::info!(agent = name, peer, reason, "a runner process of this machine disconnected; another keeps it connected");
        let (mut agents, mut jobs) = (Vec::new(), Vec::new());
        for o in owned {
            match o {
                Owned::Terminal(s) => {
                    if let Some(tx) = self.terms.lock().remove(&format!("{name}/{s}")) {
                        let _ = tx.try_send(TermOut::Gone);
                    }
                }
                Owned::Agent(id) => agents.push(id),
                Owned::Job(j) => jobs.push(j),
            }
        }
        if !agents.is_empty() {
            self.harness.sessions_lost(self, &agents);
        }
        if !jobs.is_empty() {
            self.lost_instances.lock().insert(peer, (name.to_owned(), Instant::now(), jobs));
        }
        // The newest process (maybe another one now) describes the machine.
        let newest = {
            let inst = self.instances.lock();
            inst.newest(name).and_then(|p| inst.get(p).map(|i| (p, i.hello.clone())))
        };
        let snapshot = {
            let mut g = self.agents.lock();
            let Some(a) = g.get_mut(name) else { return };
            if let Some((p, h)) = newest {
                a.peer = Some(p);
                a.labels = h.labels;
                a.slots = h.slots.max(1);
                a.hostname = h.hostname;
                a.os = h.os;
                a.arch = h.arch;
                a.version = h.version;
                a.terminal = h.terminal;
            }
            a.clone()
        };
        self.emit_agent(&snapshot);
        self.emit_terminals();
        self.update_slot_gauges();
        self.check_retire().await;
        // The front door moves to the process that is newest now.
        crate::frontdoor::control::on_instance_lost(self, name).await;
    }

    /// A machine's runner processes, while it has more than one, with what each holds.
    fn instance_views(&self, name: &str) -> Vec<crate::instances::InstanceView> {
        if self.instances.lock().of(name).len() < 2 {
            return Vec::new();
        }
        self.runner_processes(name)
    }

    /// Every runner process of a machine and what each one holds (Settings → Updates lists
    /// them always; agent views only while a machine has more than one).
    /// What a machine's newest runner process says it can do ([`crate::proto::CAP_UPGRADE`] …);
    /// 0 for runners from before builds were announced.
    pub fn runner_caps(&self, name: &str) -> u32 {
        let inst = self.instances.lock();
        inst.newest(name).and_then(|p| inst.get(p)).map_or(0, |i| i.caps)
    }

    /// The kiln a machine's newest runner process says it runs (`crate::fleet`).
    pub fn runner_kiln(&self, name: &str) -> Option<kiln_ci::facts::KilnFacts> {
        let inst = self.instances.lock();
        inst.newest(name).and_then(|p| inst.get(p)).and_then(|i| i.kiln.clone())
    }

    pub fn runner_processes(&self, name: &str) -> Vec<crate::instances::InstanceView> {
        use crate::instances::Owned;
        let target = format!("agent:{name}");
        let terms: Vec<Owned> = self.terminals.list().into_iter().filter(|t| t.target == target && !t.exited).map(|t| Owned::Terminal(t.id)).collect();
        let agents: Vec<Owned> = self.harness.list().into_iter().filter(|i| i.target == target).map(|i| Owned::Agent(i.id)).collect();
        let jobs: Vec<Owned> = self.agents.lock().get(name).map(|a| a.running.iter().cloned().map(Owned::Job).collect()).unwrap_or_default();
        let inst = self.instances.lock();
        inst.views(name, |p| {
            let held = |v: &[Owned]| v.iter().filter(|o| inst.owner(o) == Some(p)).count();
            (held(&terms), held(&agents), held(&jobs))
        })
    }

    /// A machine's newest runner process retires the older ones once they hold nothing: no live
    /// terminal, agent session or job of the machine is held by any process but the newest.
    /// Anything whose holder is unknown counts as held by the older ones, so nothing is retired
    /// early (and the newest checks the old process itself before stopping it).
    async fn check_retire(&self) {
        use crate::instances::Owned;
        let crowded = self.instances.lock().crowded();
        if crowded.is_empty() {
            return;
        }
        let mut sends = Vec::new();
        for (name, peer) in crowded {
            let target = format!("agent:{name}");
            let mut held: Vec<Owned> =
                self.terminals.list().into_iter().filter(|t| t.target == target && !t.exited).map(|t| Owned::Terminal(t.id)).collect();
            held.extend(self.harness.list().into_iter().filter(|i| i.target == target).map(|i| Owned::Agent(i.id)));
            held.extend(self.agents.lock().get(&name).into_iter().flat_map(|a| a.running.iter().cloned().map(Owned::Job)));
            let mut inst = self.instances.lock();
            let Some(newest) = inst.newest(&name) else { continue };
            if peer == newest || held.iter().any(|o| inst.owner(o) != Some(newest)) {
                continue;
            }
            let Some(i) = inst.get_mut(peer) else { continue };
            if i.retire_sent.is_some_and(|t| t.elapsed() < RETIRE_REPEAT) {
                continue;
            }
            i.retire_sent = Some(Instant::now());
            sends.push((name, newest, i.label()));
        }
        for (name, newest, instance) in sends {
            tracing::info!(agent = %name, %instance, "an older runner process of this machine holds nothing any more; asking the newest to retire it");
            self.send_peer(newest, &ServerMsg::Retire { instance }).await;
        }
    }

    pub(crate) fn release_slot(&self, agent: &str, job: &str) {
        self.instances.lock().disown(&crate::instances::Owned::Job(job.to_owned()));
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
        if r.status == crate::store::Status::Succeeded
            && let Some(run) = &run
        {
            self.adopt_release(&r.job, run).await;
        }
        if let (Some(status), Some(run)) = (settled, run) {
            crate::gitops::report_status(self, &run, status).await;
        }
        self.update_slot_gauges();
        self.wake.notify();
        self.check_retire().await;
        Ok(())
    }

    // ------------------------------------------------------------ runs

    pub async fn cancel_run(&self, run_id: &str, reason: &str) -> anyhow::Result<()> {
        // The CI engine's runs are cancelled through their plans.
        if self.ci.is_run(run_id) {
            self.ci.send(crate::ci::dispatch::Msg::Cancel { run: run_id.to_owned(), reason: reason.to_owned() });
            return Ok(());
        }
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

    /// The server's secrets, for the CI engine's trusted runs (docs/ci.md W09 narrows them).
    pub fn ci_secrets(&self) -> Vec<(String, String)> {
        self.secrets()
    }

    /// What a job's environment gets from the server: its secrets, and where each image kiln
    /// publishes lives (`KILN_IMAGE_<NAME>`, docs/cd-targets.md §7.1), set in the UI.
    fn job_secrets(&self) -> Vec<(String, String)> {
        let mut s = self.secrets();
        if let Some(cd) = crate::cd::get() {
            s.extend(cd.image_env());
        }
        s
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
                        secrets: self.job_secrets(),
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
                    let timeout = Duration::from_secs(u64::from(self.cfg.kubernetes.start_timeout_seconds));
                    let launch = self.launching.lock().get(&job.id).copied();
                    match launch {
                        Some(Launch::Pod(t)) if t.elapsed() < timeout => continue,
                        Some(Launch::Retry { at, .. }) if Instant::now() < at => continue,
                        // After a server restart the launch table is empty, but the job's pod may
                        // already exist (Pending or about to connect): adopt it instead of duplicating.
                        None => {
                            if k.job_pod_alive(&job.id).await {
                                self.launching.lock().insert(job.id.clone(), Launch::Pod(Instant::now()));
                                continue;
                            }
                        }
                        Some(Launch::Pod(_)) => {
                            // Still Pending for capacity (or pulling a large image) is not a failure:
                            // keep waiting instead of churning pods.
                            if k.job_pod_alive(&job.id).await {
                                self.launching.lock().insert(job.id.clone(), Launch::Pod(Instant::now()));
                                continue;
                            }
                            tracing::warn!(job = %job.id, "pod agent did not connect in time; relaunching");
                            k.delete_job_pods(&job.id).await;
                            // Not again on the next pass if the launch below has to wait.
                            self.launching
                                .lock()
                                .insert(job.id.clone(), Launch::Retry { at: Instant::now(), check_pods: false });
                        }
                        Some(Launch::Retry { check_pods: true, .. }) => {
                            if k.job_pod_alive(&job.id).await {
                                self.launching.lock().insert(job.id.clone(), Launch::Pod(Instant::now()));
                                continue;
                            }
                            k.delete_job_pods(&job.id).await;
                        }
                        Some(Launch::Retry { check_pods: false, .. }) => {}
                    }
                    if pods_live.is_none() {
                        pods_live = Some(self.launching.lock().values().filter(|l| matches!(l, Launch::Pod(_))).count());
                    }
                    let live = pods_live.unwrap_or(0);
                    if u32::try_from(live).unwrap_or(u32::MAX) >= self.cfg.kubernetes.max_pods {
                        continue;
                    }
                    match k.launch(self, &job, run).await {
                        Ok(pod) => {
                            tracing::info!(job = %job.id, pod, "launched agent pod");
                            self.launching.lock().insert(job.id.clone(), Launch::Pod(Instant::now()));
                            pods_live = Some(live + 1);
                            metrics::global().k8s_pods_launched.inc(&["ok"]);
                        }
                        Err(e) => {
                            metrics::global().k8s_pods_launched.inc(&["error"]);
                            // Usually the jobs-namespace quota is full: retry soon rather than after
                            // the full start timeout. Each job retries on its own, since a smaller
                            // one may still fit where this one did not.
                            let msg = format!("{e:#}");
                            let quota = msg.contains("exceeded quota");
                            if quota {
                                tracing::debug!(job = %job.id, "job quota full; waiting for capacity");
                            } else {
                                tracing::error!(job = %job.id, error = %msg, "pod launch failed");
                            }
                            let now = Instant::now();
                            let at = now.checked_add(LAUNCH_RETRY).unwrap_or(now);
                            // A quota refusal created nothing, so its retry needs no pod checks.
                            self.launching.lock().insert(job.id.clone(), Launch::Retry { at, check_pods: !quota });
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
        // Jobs of a runner process that went away while its machine stayed connected through
        // another one, and did not come back in time.
        let expired: Vec<(String, Vec<String>)> = {
            let mut g = self.lost_instances.lock();
            let peers: Vec<PeerId> = g.iter().filter(|(_, (_, t, _))| t.elapsed() > AGENT_GRACE).map(|(p, _)| *p).collect();
            peers.into_iter().filter_map(|p| g.remove(&p)).map(|(name, _, jobs)| (name, jobs)).collect()
        };
        for (name, jobs) in expired {
            if jobs.iter().any(|j| self.ci.is_leg(j)) {
                self.ci.send(crate::ci::dispatch::Msg::RunnerLost { agent: name.clone() });
            }
            for j in jobs.into_iter().filter(|j| !self.ci.is_leg(j)) {
                let still = self.agents.lock().get(&name).is_some_and(|a| a.running.contains(&j));
                if !still {
                    continue;
                }
                self.release_slot(&name, &j);
                match self.store.requeue_job(j.clone(), format!("runner process did not reconnect ({name})")).await {
                    Ok(_) => self.emit_job(&j).await,
                    Err(e) => tracing::error!(error = %e, job = %j, "requeue failed"),
                }
            }
        }
        self.check_retire().await;
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
