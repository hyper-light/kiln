//! Today's live data as browser-protocol topics (docs/browser-protocol.md, "Topics"; goal 2's
//! plan step 9, plan `01a1a832eb04756a9f5a6c3fef8cf63a`). The adapters here turn what the hub
//! emits (`ServerEvent`), a session's frames (`SessionDown`) and logs and terminals into topic
//! changes, as typed deltas instead of whole records or "refetch" events. They are pure: a carrier
//! (goal 3's WebSocket, goal 5's WebTransport) publishes their output into `kiln_bp::topic::TopicLog`s
//! and pushes it to each connection's `kiln_bp::server::Server`. Today's endpoints stay as they
//! are; goal 9 moves the UI.
//!
//! Access is decided once, when a page subscribes (`allowed`), from the topic alone: each topic is
//! scoped to what can see it (a repository, a session, a person, or the owner), so no event is
//! read per connection to decide who may have it, as the events socket's `Narrow` does today.

pub mod session;

use std::collections::{BTreeSet, HashMap};

use kiln_bp::conn::{Class, Prio, TopicSpec, Trust};
use kiln_bp::frames::{Delta, Key, Topic};
use kiln_bp::records as rec;
use kiln_bp::state::ItemKey;
use kiln_bp::wire::{Bits, Bytes, Fields, diff};

use crate::wire::events::{Queue, ServerEvent};

/// The topic catalogue: each kind's number on the wire (append-only), its name, and how it's
/// delivered.
pub mod kind {
    pub const RUNS: u32 = 1;
    pub const LEGS: u32 = 2;
    pub const STEPS: u32 = 3;
    pub const RUNNERS: u32 = 4;
    pub const SAMPLES: u32 = 5;
    pub const QUEUE: u32 = 6;
    pub const CHANGED: u32 = 7;
    pub const EVENTS: u32 = 8;
    pub const SESSION: u32 = 9;
    pub const STEP_LOG: u32 = 10;
    pub const TERMINAL: u32 = 11;
    pub const CD_LOG: u32 = 12;
}

/// Every kind: its number, name, and what its key names.
pub const CATALOGUE: &[(u32, &str, &str)] = &[
    (kind::RUNS, "runs", "a repository's name; items by run id"),
    (kind::LEGS, "legs", "a repository's name; items by leg id"),
    (kind::STEPS, "steps", "a repository's name; items by `<leg>.<idx>`"),
    (kind::RUNNERS, "runners", "empty; items by runner name"),
    (kind::SAMPLES, "samples", "empty; items by machine (`server` for the server's own)"),
    (kind::QUEUE, "queue", "empty; one record"),
    (kind::CHANGED, "changed", "empty (the owner's), a repository's name, or `person:<name>`; items by resource"),
    (kind::EVENTS, "events", "empty (the owner's) or a repository's name; items by number"),
    (kind::SESSION, "session", "an agent session's id"),
    (kind::STEP_LOG, "step_log", "`<leg>/<step>`"),
    (kind::TERMINAL, "terminal", "a terminal's id"),
    (kind::CD_LOG, "cd_log", "`<app>/<resource>`"),
];

/// How a kind's changes go: its class (what may be merged while waiting), priority and trust.
pub fn spec(k: u32) -> Option<TopicSpec> {
    let s = |class, prio, trust| Some(TopicSpec { kind: k, class, prio, trust });
    match k {
        kind::RUNS | kind::LEGS | kind::STEPS | kind::RUNNERS => s(Class::State, Prio::State, Trust::Text),
        kind::SAMPLES | kind::QUEUE => s(Class::Telemetry, Prio::State, Trust::Records),
        kind::CHANGED => s(Class::State, Prio::State, Trust::Records),
        kind::EVENTS => s(Class::Stream, Prio::Stream, Trust::Text),
        kind::SESSION => s(Class::Stream, Prio::Interactive, Trust::Text),
        kind::STEP_LOG | kind::CD_LOG => s(Class::Stream, Prio::Stream, Trust::Text),
        kind::TERMINAL => s(Class::Stream, Prio::Interactive, Trust::Text),
        _ => None,
    }
}

/// Who a subscriber is, as far as topics' access goes.
#[derive(Debug, Clone, Default)]
pub struct Caller {
    /// The owner (or an admin token): every topic.
    pub owner: bool,
    pub person: String,
    /// The repositories this caller may see, by name.
    pub repos: BTreeSet<String>,
    /// The agent sessions this caller may see.
    pub sessions: BTreeSet<String>,
}

/// Whether `caller` may subscribe to `topic`: decided from the topic alone, once.
pub fn allowed(topic: &Topic, caller: &Caller) -> bool {
    if spec(topic.kind).is_none() {
        return false;
    }
    if caller.owner {
        return true;
    }
    match topic.kind {
        kind::RUNS | kind::LEGS | kind::STEPS | kind::EVENTS => caller.repos.contains(&topic.key),
        kind::CHANGED => topic.key.strip_prefix("person:").is_some_and(|p| p == caller.person) || caller.repos.contains(&topic.key),
        kind::SESSION => caller.sessions.contains(&topic.key),
        // Runners, samples, the queue, logs and terminals: the owner's, as on today's sockets.
        _ => false,
    }
}

/// A change to publish: the topic, and its delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Out {
    pub topic: Topic,
    pub delta: Delta,
}

fn topic(kind: u32, key: &str) -> Topic {
    Topic { kind, key: key.to_owned() }
}

fn status(s: crate::store::Status) -> rec::Status {
    use crate::store::Status as S;
    match s {
        S::Pending => rec::Status::Pending,
        S::Queued => rec::Status::Queued,
        S::Running => rec::Status::Running,
        S::Succeeded => rec::Status::Succeeded,
        S::Failed => rec::Status::Failed,
        S::Cancelled => rec::Status::Cancelled,
        S::Skipped => rec::Status::Skipped,
        S::TimedOut => rec::Status::TimedOut,
    }
}

pub fn run_record(r: &crate::store::Run) -> rec::Run {
    rec::Run {
        repo: r.repo_id.clone(),
        repo_name: r.repo_name.clone(),
        number: r.number,
        pipeline: r.pipeline.clone(),
        event: r.event.clone(),
        branch: r.branch.clone(),
        sha: r.sha.clone(),
        message: r.message.clone(),
        author: r.author.clone(),
        status: status(r.status),
        created_at: r.created_at,
        started_at: r.started_at,
        finished_at: r.finished_at,
        error: r.error.clone(),
    }
}

pub fn leg_record(j: &crate::store::Job) -> rec::Leg {
    rec::Leg {
        run: j.run_id.clone(),
        key: j.key.clone(),
        display: j.display.clone(),
        status: status(j.status),
        labels: j.labels.clone(),
        agent: j.agent.clone(),
        executor: j.executor.clone(),
        attempt: j.attempt,
        queued_at: j.queued_at,
        started_at: j.started_at,
        finished_at: j.finished_at,
        exit_code: j.exit_code.map(i64::from),
        error: j.error.clone(),
    }
}

pub fn step_record(s: &crate::store::Step) -> rec::Step {
    rec::Step { idx: s.idx, name: s.name.clone(), status: status(s.status), started_at: s.started_at, finished_at: s.finished_at, exit_code: s.exit_code.map(i64::from) }
}

pub fn runner_record(a: &crate::hub::AgentLive) -> rec::Runner {
    rec::Runner {
        label: a.label.clone(),
        hostname: a.hostname.clone(),
        os: a.os.clone(),
        arch: a.arch.clone(),
        version: a.version.clone(),
        labels: a.labels.clone(),
        slots: a.slots,
        busy: a.busy,
        connected: a.connected,
        draining: a.draining,
        ephemeral: a.ephemeral,
        terminal: a.terminal,
        last_seen: a.last_seen,
        enrolled_at: a.enrolled_at,
        running: a.running.clone(),
    }
}

/// A percentage or load as an integer in thousandths or hundredths (no floats on the wire).
fn scaled(x: f32, by: f32) -> u32 {
    let v = (x * by).round();
    if v.is_finite() && v > 0.0 { v.min(u32::MAX as f32) as u32 } else { 0 }
}

pub fn sample_record(s: &crate::metrics::HostSample) -> rec::Sample {
    rec::Sample {
        ts_ms: s.ts_ms,
        cpu_pm: scaled(s.cpu_pct, 10.0),
        cpus: s.cpus,
        load1_c: scaled(s.load1, 100.0),
        mem_total: s.mem_total,
        mem_used: s.mem_used,
        disk_total: s.disk_total,
        disk_used: s.disk_used,
        net_rx: s.net_rx,
        net_tx: s.net_tx,
        procs: s.procs,
    }
}

/// How many of a topic's carried events are kept for its snapshots.
pub const EVENTS_KEPT: u64 = 256;

/// The adapters' memory: what each item was last published as (so a change sends only its
/// changed fields), which repository each run is of, and the counters of carried events.
#[derive(Debug, Default)]
pub struct Feed {
    last: HashMap<(u32, String, ItemKey), Vec<Bytes>>,
    record: HashMap<(u32, String), Vec<Bytes>>,
    run_repo: HashMap<String, String>,
    seq: HashMap<String, u64>,
    changed: HashMap<(String, String), u64>,
}

impl Feed {
    pub fn new() -> Self {
        Self::default()
    }

    /// An item's fields as they are now: a `Put` the first time, then a `Patch` of the fields that
    /// changed; nothing when none did.
    pub fn item(&mut self, t: Topic, key: ItemKey, fields: Vec<Bytes>) -> Option<Out> {
        let slot = (t.kind, t.key.clone(), key.clone());
        let delta = match self.last.get(&slot) {
            None => Delta::Put { key: key.key(), fields: fields.clone() },
            Some(old) => {
                let (mask, changed) = diff(old, &fields);
                if mask.0 == 0 {
                    return None;
                }
                Delta::Patch { key: key.key(), mask, fields: changed }
            }
        };
        self.last.insert(slot, fields);
        Some(Out { topic: t, delta })
    }

    /// A topic's one record as it is now: a `Set` of the fields that changed.
    pub fn record(&mut self, t: Topic, fields: Vec<Bytes>) -> Option<Out> {
        let slot = (t.kind, t.key.clone());
        let (mask, changed) = diff(self.record.get(&slot).map_or(&[][..], |v| v.as_slice()), &fields);
        if mask.0 == 0 {
            return None;
        }
        self.record.insert(slot, fields);
        Some(Out { topic: t, delta: Delta::Set { mask, fields: changed } })
    }

    /// Lets go of an item (a topic's `Remove`).
    pub fn remove(&mut self, t: Topic, key: ItemKey) -> Out {
        self.last.remove(&(t.kind, t.key.clone(), key.clone()));
        Out { topic: t, delta: Delta::Remove { key: key.key() } }
    }

    /// That a resource changed: its counter, under the scope that may read it.
    fn changed(&mut self, scope: &str, resource: String) -> Vec<Out> {
        let n = self.changed.entry((scope.to_owned(), resource.clone())).or_insert(0);
        *n = n.saturating_add(1);
        let times = *n;
        self.item(topic(kind::CHANGED, scope), ItemKey::Str(resource), rec::Changed { times }.fields()).into_iter().collect()
    }

    /// An event carried as its JSON in a scope's `events` topic, the oldest past the kept ones let go.
    fn carried(&mut self, scope: &str, tag: &str, ev: &ServerEvent) -> Vec<Out> {
        let n = self.seq.entry(scope.to_owned()).or_insert(0);
        *n = n.saturating_add(1);
        let seq = *n;
        let t = topic(kind::EVENTS, scope);
        let mut out = Vec::new();
        let item = rec::Event { tag: tag.to_owned(), json: Bytes(ev.text().into_bytes()) };
        out.push(Out { topic: t.clone(), delta: Delta::Put { key: Key::Num { n: seq }, fields: item.fields() } });
        if seq > EVENTS_KEPT {
            out.push(Out { topic: t, delta: Delta::Remove { key: Key::Num { n: seq - EVENTS_KEPT } } });
        }
        out
    }

    /// The repository a run is of, learned from its events (or from `lookup`, the store's).
    fn repo_of(&mut self, run: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
        if let Some(r) = self.run_repo.get(run) {
            return Some(r.clone());
        }
        let r = lookup(run)?;
        if self.run_repo.len() > 100_000 {
            self.run_repo.clear();
        }
        self.run_repo.insert(run.to_owned(), r.clone());
        Some(r)
    }

    /// The topic changes one of the hub's events makes. Every kind is mapped or says why not
    /// (this match has no catch-all: a new kind must be decided here).
    pub fn event(&mut self, ev: &ServerEvent, run_repo: &dyn Fn(&str) -> Option<String>) -> Vec<Out> {
        use ServerEvent as E;
        match ev {
            // Nothing falls behind into a resync: a subscription resumes from its cursor.
            E::Resync => Vec::new(),
            // What a page sent up that couldn't be read: the protocol's `Unread`, to that page.
            E::Refused(_) => Vec::new(),
            E::Run { run } => {
                self.run_repo.insert(run.id.clone(), run.repo_name.clone());
                self.item(topic(kind::RUNS, &run.repo_name), ItemKey::Str(run.id.clone()), run_record(run).fields()).into_iter().collect()
            }
            E::Job { job } => {
                let Some(repo) = self.repo_of(&job.run_id, run_repo) else { return Vec::new() };
                let mut out: Vec<Out> = self.item(topic(kind::LEGS, &repo), ItemKey::Str(job.id.clone()), leg_record(job).fields()).into_iter().collect();
                for s in &job.steps {
                    out.extend(self.item(topic(kind::STEPS, &repo), ItemKey::Str(format!("{}.{}", job.id, s.idx)), step_record(s).fields()));
                }
                out
            }
            E::Agent { agent } => {
                let mut out: Vec<Out> = self.item(topic(kind::RUNNERS, ""), ItemKey::Str(agent.name.clone()), runner_record(agent).fields()).into_iter().collect();
                if let Some(s) = &agent.sample {
                    out.extend(self.item(topic(kind::SAMPLES, ""), ItemKey::Str(agent.name.clone()), sample_record(s).fields()));
                }
                out
            }
            E::Sample { agent, sample } => self.item(topic(kind::SAMPLES, ""), ItemKey::Str(agent.clone()), sample_record(sample).fields()).into_iter().collect(),
            E::Queue(Queue::Counts { queued, running }) => self.record(topic(kind::QUEUE, ""), rec::QueueCounts { queued: *queued, running: *running }.fields()).into_iter().collect(),
            E::Queue(Queue::Entry(_)) => self.carried("", "queue", ev),
            // "Read it again" events: a counter per resource, the newest waiting merged.
            E::Terminals => self.changed("", "terminals".into()),
            E::Updates => self.changed("", "updates".into()),
            E::Places => self.changed("", "places".into()),
            E::Discovery => self.changed("", "discovery".into()),
            E::JoinRequests => self.changed("", "join-requests".into()),
            E::Tailscale => self.changed("", "tailscale".into()),
            E::Push => self.changed("", "push".into()),
            E::Peers => self.changed("", "peers".into()),
            E::Keep => self.changed("", "keep".into()),
            // The offers view read again, by its version (docs/reach.md §2.2).
            E::Offers { .. } => self.changed("", "offers".into()),
            E::AiSessions { machine } => self.changed("", format!("ai-sessions/{machine}")),
            E::PlanStack { run } => self.changed("", format!("plan-stack/{run}")),
            E::Inbox { person } => self.changed(&format!("person:{person}"), "inbox".into()),
            // One person's Mission control placement changed or a park woke (attention plan 01a199e1b177752e84dad5dc990e6a15).
            E::Attention { principal } => self.changed(&format!("person:{principal}"), "attention".into()),
            // One person's Agentfiles changed (`crate::agentfiles`): their list read again.
            E::Agentfile(a) => self.changed(&format!("person:{}", a.owner), "agentfiles".into()),
            E::Program { program } => self.changed("", format!("program/{program}")),
            E::FrontDoor { runner } => self.changed("", format!("front-door/{runner}")),
            E::AgentLimit { runner } => self.changed("", format!("agent-limit/{runner}")),
            E::Headroom { runner } => self.changed("", format!("headroom/{runner}")),
            E::CdRollout { app, id } => self.changed("", format!("cd-rollout/{app}/{id}")),
            // A repository's rows in Ship's Pipelines (W29-R03): pages showing it read them again.
            E::Pipelines { repo } => self.changed(repo, "pipelines".into()),
            // Events with records of their own but no protocol record yet: carried as their JSON
            // (which either side can grow, f3) in their scope's `events` topic, until goal 9 gives
            // each the record its page reads.
            E::Comment(c) => self.carried(&c.repo, "comment", ev),
            E::Review(r) => self.carried(&r.repo, "review", ev),
            E::DiffRun(d) => self.carried(&d.repo, "diff_run", ev),
            E::Landed(l) => self.carried(&l.repo_name, "landed", ev),
            E::CiImage { repo, .. } => self.carried(repo, "ci_image", ev),
            E::CiVerdict(v) => self.carried(&v.repo, "ci_verdict", ev),
            E::Experiment { repo, .. } => self.carried(repo, "experiment", ev),
            E::PreviewRemoved { repo, .. } => self.carried(repo, "preview.removed", ev),
            E::Conflict(c) => self.carried(&c.repo, "conflict", ev),
            // Ship's rows that changed (W21-R15), in their repository's scope as the events socket narrows them.
            E::Ship(s) => self.carried(&s.repo, "ship", ev),
            E::RunNotice(n) => self.carried(&n.repo.name, "run_notice", ev),
            E::Incident { incident } => self.carried(incident.repo.as_deref().unwrap_or(""), "incident", ev),
            E::CdPlan(p) => self.carried(p.repo.as_deref().unwrap_or(""), "cd_plan", ev),
            E::MachineBatteryLow { .. } => self.carried("", "machine.battery_low", ev),
            E::UpdateJob { .. } => self.carried("", "update_job", ev),
            E::AccountFlow { .. } => self.carried("", "account_flow", ev),
            E::Attachment(_) => self.carried("", "attachment", ev),
            E::RegistryPackage { .. } => self.carried("", "registry_package", ev),
            E::Deploy(_) => self.carried("", "deploy", ev),
            E::DeployRegistration(_) => self.carried("", "deploy_registration", ev),
            E::DeployMove { .. } => self.carried("", "deploy_move", ev),
            E::AgentChange { .. } => self.carried("", "agent_change", ev),
            E::Harness { .. } => self.carried("", "harness", ev),
            E::HarnessClosed { .. } => self.carried("", "harness_closed", ev),
            E::Notices(_) => self.carried("", "notices", ev),
            E::CdAppDeleted { .. } => self.carried("", "cd_app_deleted", ev),
            E::CdApps { .. } => self.carried("", "cd_apps", ev),
            E::RunnerOffline { .. } => self.carried("", "runner.offline", ev),
            E::SelfDeploy(_) => self.carried("", "self_deploy", ev),
            E::AgentNotice(_) => self.carried("", "agent_notice", ev),
        }
    }
}

/// A log or terminal's bytes, as appends by offset: each stream's end kept so the next append
/// starts where the last ended.
#[derive(Debug, Default)]
pub struct Streams {
    ends: HashMap<(u32, String), u64>,
}

impl Streams {
    pub fn new() -> Self {
        Self::default()
    }

    /// `data` written to a stream (a step's log: `kind::STEP_LOG`, `<leg>/<step>`; a terminal:
    /// `kind::TERMINAL`, its id; a deployment's log: `kind::CD_LOG`), cut into appends of at most
    /// `most` bytes so none holds up an urgent frame for long.
    pub fn append(&mut self, kind: u32, key: &str, data: &[u8], most: usize) -> Vec<Out> {
        let end = self.ends.entry((kind, key.to_owned())).or_insert(0);
        let mut out = Vec::new();
        for chunk in data.chunks(most.max(1)) {
            out.push(Out { topic: topic(kind, key), delta: Delta::Append { at: *end, data: Bytes(chunk.to_vec()) } });
            *end = end.saturating_add(chunk.len() as u64);
        }
        out
    }
}

/// What a connection's refusal is, for the one place refusals are noted
/// (`crate::wire::unread`: `kiln_wire_refused_total{wire,code}`, the audit log, the person's
/// page): a frame a page sent that couldn't be read (`bp_up`), one it says it couldn't read of
/// the server's (`bp_down`), or an open refused. Other events aren't refusals.
pub fn unread(e: &kiln_bp::server::Event, from: crate::wire::unread::Sender) -> Option<crate::wire::unread::Unread> {
    use crate::wire::unread::{Code, Unread, Wire};
    use kiln_bp::server::Event as E;
    let code = |c: kiln_bp::wire::Code| match c {
        kiln_bp::wire::Code::Unknown => Code::Unknown,
        kiln_bp::wire::Code::Malformed => Code::Malformed,
    };
    match e {
        E::Unreadable { kind, why } => Some(Unread { wire: Wire::BpUp, from, what: kind.map_or_else(|| "frame".to_owned(), |k| format!("kind {k}")), code: code(why.code()), why: why.name().to_owned() }),
        E::Unread { kind, code: c, why } => {
            let c = match c {
                kiln_bp::frames::UnreadCode::Unknown => Code::Unknown,
                kiln_bp::frames::UnreadCode::Malformed => Code::Malformed,
            };
            Some(Unread { wire: Wire::BpDown, from, what: format!("kind {kind}"), code: c, why: kiln_bp::frames::UnreadWhy::VARIANTS.get(why.variant() as usize).copied().unwrap_or("").to_owned() })
        }
        E::OpenRefused { code: c, why } => Some(Unread { wire: Wire::BpUp, from, what: format!("open: {}", kiln_bp::frames::OpenRefusal::VARIANTS.get(c.variant() as usize).copied().unwrap_or("")), code: Code::Malformed, why: why.clone() }),
        _ => None,
    }
}

/// The mask of every field of a record of `n` fields.
pub fn all(n: usize) -> Bits {
    Bits(if n >= 64 { u64::MAX } else { (1u64 << n) - 1 })
}

#[cfg(test)]
mod measure;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_support;
