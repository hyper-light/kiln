//! U03: every CI command belongs to the same kernel containment API as reach. The execution
//! scope owns its commands; a completion or recovered host cannot release capacity before
//! every containment is proven empty. Registry files are local kernel fences, not application
//! records or an alternative database.

use std::cell::RefCell;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, atomic::{AtomicBool, AtomicI32, Ordering}};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Context as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::reach::leases::Containment;

pub(crate) mod migration;
mod admission;
pub(crate) use admission::Admission;
pub(crate) mod captured;
mod executions;
pub(crate) use executions::{StoppedExecution, recovery_evidence, try_acknowledge_evidence, OrdinaryClaim, claim_ordinary_evidence, ordinary_inventory_present};
#[cfg(test)]
pub(crate) use executions::acknowledge_evidence;

thread_local! { static CURRENT: RefCell<Option<Arc<Execution>>> = const { RefCell::new(None) }; }

pub(crate) fn current() -> Option<Arc<Execution>> { CURRENT.with(|c| c.borrow().clone()) }

struct Enter(Option<Arc<Execution>>);
impl Enter {
    fn new(execution: Option<Arc<Execution>>) -> Self { Self(CURRENT.with(|c| c.replace(execution))) }
}
impl Drop for Enter { fn drop(&mut self) { CURRENT.with(|c| { c.replace(self.0.take()); }); } }

/// The runtime propagates this scope to tasks and blocking helpers, without putting a job's
/// commands in another job's scope when their futures share a shard.
pub(crate) struct Scoped<F: ?Sized> { execution: Option<Arc<Execution>>, future: Pin<Box<F>> }
impl<F: Future + ?Sized> Future for Scoped<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let _entered = Enter::new(this.execution.clone());
        this.future.as_mut().poll(cx)
    }
}
pub(crate) fn scoped<F: Future>(execution: Option<Arc<Execution>>, future: F) -> Scoped<F> { Scoped { execution, future: Box::pin(future) } }
pub(crate) fn scoped_boxed<F: Future + ?Sized>(execution: Arc<Execution>, future: Pin<Box<F>>) -> Scoped<F> { Scoped { execution: Some(execution), future } }
pub(crate) fn within<R>(execution: Option<Arc<Execution>>, f: impl FnOnce() -> R) -> R { let _entered = Enter::new(execution); f() }

#[derive(Default)]
struct State { fenced: bool, closed: bool, kept: bool, groups: Vec<Arc<Group>>, host: Option<Arc<std::fs::File>>, gate: Option<Arc<executions::Gate>>, parent_binding: Option<ParentBinding>, vm_shape: Option<kiln_ci::sched::vm::FirecrackerShape>, docker: std::collections::BTreeMap<String, Arc<Group>> }

pub(crate) use kiln_ci::wire::ParentBinding;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExecutorOwner { pub executor: String, pub generation: String, pub allocation: String }

/// This capability is deliberately not deserializable. Only an authenticated executor
/// bootstrap can turn its control envelope into inherited ownership; an environment variable
/// or an ordinary assignment can never select it.
#[derive(Clone, Debug)]
pub(crate) struct ParentExecution(ParentBinding);
impl ParentExecution {
    /// Called only after the guest/pod bootstrap has authenticated its immutable parent
    /// identity. Ordinary assignments and environment configuration cannot invoke this path.
    pub(crate) fn verified(binding: ParentBinding, job: &str, attempt: u32) -> anyhow::Result<Self> {
        anyhow::ensure!(attempt > 0 && binding.attempt == attempt && binding.job == job, "the inherited executor does not own this job attempt");
        for value in [&binding.executor, &binding.generation, &binding.allocation, &binding.physical_execution, &binding.job] {
            anyhow::ensure!(!value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control), "invalid inherited executor identity");
        }
        Ok(Self(binding))
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn binding(&self) -> &ParentBinding { &self.0 }
}

pub(crate) struct Execution { admission: Admission, work: PathBuf, job: String, _attempt: Option<u32>, identity: String, operation: Option<String>, reconciling: bool, parent: Option<ParentExecution>, executor_owner: Option<ExecutorOwner>, ordinary_owner: Option<kiln_ci::wire::OrdinaryOwnerV1>, fenced: AtomicBool, state: Mutex<State> }
impl Execution {
    pub(crate) fn new(work: PathBuf, job: String, attempt: Option<u32>) -> Arc<Self> { Self::new_in(Admission::production(), work, job, attempt) }
    pub(crate) fn new_in(admission: Admission, work: PathBuf, job: String, attempt: Option<u32>) -> Arc<Self> {
        Arc::new(Self { admission, work, job, _attempt: attempt.map(|a| a.max(1)), identity: format!("ci-{}", crate::util::new_id()), operation: None, reconciling: false, parent: None, executor_owner: None, ordinary_owner: None, fenced: AtomicBool::new(false), state: Mutex::new(State::default()) })
    }
    /// U01/U03: bind the server-selected native identity BEFORE any child can be prepared.
    pub(crate) fn ordinary_in(admission: Admission, work: PathBuf, owner: kiln_ci::wire::OrdinaryOwnerV1) -> anyhow::Result<Arc<Self>> {
        owner.validate().map_err(anyhow::Error::msg)?;
        let execution = Arc::new(Self { admission, work, job: owner.job.clone(), _attempt: Some(owner.attempt), identity: owner.execution.clone(), operation: None, reconciling: false, parent: None, executor_owner: None, ordinary_owner: Some(owner), fenced: AtomicBool::new(false), state: Mutex::new(State::default()) });
        { let mut state = execution.state.lock(); execution.gate(&mut state)?; }
        Ok(execution)
    }
    /// Only called AFTER the SAME receiver's exact closed prefix committed in Amphora.
    pub(crate) fn close_ordinary_admission(&self, no_start: bool) -> anyhow::Result<()> {
        let owner = self.ordinary_owner.as_ref().context("execution has no ordinary grant")?;
        let mut state = self.state.lock();
        anyhow::ensure!(!no_start || (state.groups.is_empty() && state.docker.is_empty() && !state.closed), "NoStart cannot erase prior accepted work");
        self.gate(&mut state)?.context("ordinary execution lacks its Gate")?.close_incoming(owner, no_start)?;
        if no_start { state.closed = true; }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    pub(crate) fn inherited(work: PathBuf, job: String, attempt: u32, parent: ParentExecution) -> anyhow::Result<Arc<Self>> { Self::inherited_in(Admission::production(), work, job, attempt, parent) }
    pub(crate) fn inherited_in(admission: Admission, work: PathBuf, job: String, attempt: u32, parent: ParentExecution) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(parent.0.job == job && parent.0.attempt == attempt, "the inherited executor does not own this job attempt");
        Ok(Arc::new(Self { admission, work, job, _attempt: Some(attempt), identity: format!("ci-{}", crate::util::new_id()), operation: None, reconciling: false, parent: Some(parent), executor_owner: None, ordinary_owner: None, fenced: AtomicBool::new(false), state: Mutex::new(State::default()) }))
    }
    /// ctl-9/U03: logical identity must be reachable by the existing Startup owner BEFORE
    /// any fallible physical-root preparation. This allocates no host/kernel resource.
    pub(super) fn allocate_executor(admission: Admission, work: PathBuf, executor: String, generation: String, allocation: String) -> anyhow::Result<Arc<Self>> {
        for value in [&executor, &generation, &allocation] { anyhow::ensure!(!value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control), "invalid existing executor grant"); }
        Ok(Arc::new(Self { admission, work, job: format!("executor:{executor}"), _attempt: None, identity: format!("ci-{}", crate::util::new_id()), operation: None, reconciling: false, parent: None, executor_owner: Some(ExecutorOwner { executor, generation, allocation }), ordinary_owner: None, fenced: AtomicBool::new(false), state: Mutex::new(State::default()) }))
    }

    pub(super) fn prepare_executor_root(&self) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        anyhow::ensure!(self.executor_owner.is_some() && self.accepts_work() && !state.fenced && !state.closed, "executor preparation cannot reopen closed admission");
        self.host(&mut state)?;
        self.gate(&mut state)?;
        Ok(())
    }
    pub(super) fn cleanup(admission: Admission, work: PathBuf) -> Arc<Self> { Arc::new(Self { admission, work, job: "executor-cleanup".into(), _attempt: None, identity: format!("ci-{}", crate::util::new_id()), operation: None, reconciling: true, parent: None, executor_owner: None, ordinary_owner: None, fenced: AtomicBool::new(false), state: Mutex::new(State::default()) }) }
    /// U01: bind only the Scheduler's existing executor shape, never infer from host capacity.
    pub(super) fn bind_vm_shape(&self, shape: kiln_ci::sched::vm::FirecrackerShape) -> anyhow::Result<()> {
        shape.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(self.executor_owner.is_some(), "Firecracker shape requires its original executor grant owner");
        let mut state = self.state.lock();
        anyhow::ensure!(self.accepts_work() && !state.fenced && !state.closed && state.vm_shape.as_ref().is_none_or(|old| old == &shape), "Firecracker shape changed an existing or fenced physical execution");
        if state.vm_shape.is_some() { return Ok(()); }
        anyhow::ensure!(state.groups.is_empty() && state.docker.is_empty(), "aggregate limits must precede every child or daemon command");
        let outer = shape.outer();
        let limits = Containment::cpu_memory_limits(outer.cpu, outer.mem)?;
        anyhow::ensure!(limits.memory_bytes >= shape.guest_resources().mem, "page-rounded memory envelope cannot hold the admitted guest");
        let gate = self.gate(&mut state)?.context("aggregate limits require the original durable Gate")?;
        let kernel = Containment::aggregate(&self.identity)?;
        // Like existing Group::create, allocation is empty until its identity is durable.
        // No initialization or child launch capability is exposed before this bind.
        // Rename can succeed before a parent fsync reports failure. Keep the empty
        // domain on ANY ambiguous bind error; the same exact Stop owner reconciles it.
        gate.bind_domain(&kernel, limits)?;
        // A partial setup remains recorded in v2. Stop/recovery must prove this exact
        // aggregate empty; a missing/unreadable path cannot discharge that obligation.
        kernel.set_limits(limits)?;
        state.vm_shape = Some(shape);
        Ok(())
    }
    pub(super) fn vm_shape(&self) -> anyhow::Result<kiln_ci::sched::vm::FirecrackerShape> {
        self.state.lock().vm_shape.clone().context("U01: no Firecracker grant shape owns this operation; run it through an explicitly shaped pool executor")
    }
    pub(crate) fn scope<F: Future>(self: &Arc<Self>, future: F) -> Scoped<F> { scoped(Some(Arc::clone(self)), future) }
    pub(crate) fn identity(&self) -> &str { &self.identity }
    pub(crate) fn admission(&self) -> &Admission { &self.admission }
    /// A legacy checkout consumes its already accepted native job attempt; this check does
    /// not create a physical owner or grant another slot/resource reservation.
    pub(crate) fn require_native_attempt(&self, job: &str, attempt: u32) -> anyhow::Result<()> {
        if self.parent.is_some() { return Err(crate::proc::CustodyRefusal::InheritedExecution.into()); }
        anyhow::ensure!(self.job == job && self._attempt == Some(attempt) && self.operation.is_none(), "checkout does not own its existing native job attempt");
        let state = self.state.lock();
        anyhow::ensure!(self.accepts_work() && !state.fenced && !state.closed, "checkout's existing native job attempt is closed");
        Ok(())
    }
    pub(crate) fn label(&self) -> String { format!("kiln.execution={}", self.operation.as_deref().unwrap_or(&self.identity)) }
    /// A bounded debug invocation's resource label/journal; physical evidence always uses
    /// identity(), the retained root, and its unchanged Scheduler allocation.
    pub(crate) fn operation_identity(&self) -> Option<&str> { self.operation.as_deref() }
    pub(crate) fn fence(&self) { self.fenced.store(true, Ordering::Release); }
    pub(crate) fn accepts_work(&self) -> bool { !self.fenced.load(Ordering::Acquire) }

    /// The physical executor controller calls this before boot/launch, after its existing
    /// allocation is durable. No child can mutate this immutable ownership binding.
    pub(crate) fn bind_parent(&self, binding: ParentBinding) -> anyhow::Result<()> {
        if let Some(owner) = &self.executor_owner {
            anyhow::ensure!(binding.executor == owner.executor && binding.generation == owner.generation && binding.allocation == owner.allocation, "the assignment does not own this existing executor grant");
            ParentExecution::verified(binding.clone(), &binding.job, binding.attempt)?;
        } else {
            ParentExecution::verified(binding.clone(), &self.job, self._attempt.context("physical ownership requires an exact attempt")?)?;
        }
        anyhow::ensure!(binding.physical_execution == self.identity, "the executor binding names another physical execution");
        let mut state = self.state.lock();
        anyhow::ensure!(self.parent.is_none() && self.accepts_work() && !state.fenced, "physical ownership can only bind an active host execution");
        anyhow::ensure!(state.parent_binding.as_ref().is_none_or(|old| old == &binding), "an executor cannot change its physical owner");
        let gate = self.gate(&mut state)?.context("physical ownership requires a durable execution gate")?;
        gate.bind_parent(&binding)?;
        for group in &state.groups { group.bind_parent_metadata(&binding)?; }
        state.parent_binding = Some(binding);
        Ok(())
    }

    pub(crate) fn docker(&self, owner: super::docker::Ownership) -> anyhow::Result<Arc<Group>> {
        let key = serde_json::to_string(&owner)?;
        let mut state = self.state.lock();
        anyhow::ensure!(self.accepts_work() && !state.fenced, "this execution has already stopped");
        if let Some(group) = state.docker.get(&key).filter(|g| !g.finished.is_cancelled()) {
            anyhow::ensure!(group.parent.is_some() || group.ownership.lock().is_some(), "container cleanup has already fenced this execution");
            return Ok(Arc::clone(group))
        }
        let group = if let Some(parent) = &self.parent { Group::inherited(self.admission.clone(), parent.clone(), Some(owner)) } else {
            let host = self.host(&mut state)?;
            let gate = self.gate(&mut state)?;
            Group::create((&self.admission, &self.work), &self.job, host, gate, self._attempt, Some(owner), false)?
        };
        state.groups.retain(|g| !g.finished.is_cancelled());
        state.groups.push(Arc::clone(&group));
        state.docker.insert(key, Arc::clone(&group));
        Ok(group)
    }

    fn host(&self, state: &mut State) -> anyhow::Result<Arc<std::fs::File>> {
        match &state.host {
            Some(host) => Ok(Arc::clone(host)),
            None => { let host = Arc::new(if self.reconciling { crate::reach::leases::host_guard(&self.admission, true)? } else { crate::reach::leases::ci_guard(&self.admission)? }); state.host = Some(Arc::clone(&host)); Ok(host) }
        }
    }

    fn gate(&self, state: &mut State) -> anyhow::Result<Option<Arc<executions::Gate>>> {
        if self.reconciling { return Ok(None) }
        if let Some(gate) = &state.gate { return Ok(Some(Arc::clone(gate))) }
        let gate = if self.ordinary_owner.is_some() {
            executions::Gate::create_owned(self.admission.clone(), &self.identity, &self.job, self._attempt, &self.work, executions::Ownership { parent: state.parent_binding.clone(), executor_owner: self.executor_owner.clone(), ordinary_owner: self.ordinary_owner.clone() })?
        } else { executions::Gate::create(self.admission.clone(), &self.identity, &self.job, self._attempt, &self.work, state.parent_binding.clone(), self.executor_owner.clone())? };
        state.gate = Some(Arc::clone(&gate));
        Ok(Some(gate))
    }

    /// Reserve one fixed trusted cleanup command while this execution is still accepting
    /// work. Its already-persisted group keeps the same physical owner through final cleanup;
    /// the capability can neither change its command nor reopen ordinary spawn admission.
    pub(super) fn prepare_cleanup(&self, command: std::process::Command) -> anyhow::Result<PreparedCleanup> {
        let command = command_environment(command)?;
        let mut state = self.state.lock();
        anyhow::ensure!(self.accepts_work() && !state.fenced, "cleanup must be reserved before execution admission closes");
        let group = if let Some(parent) = &self.parent { Group::inherited(self.admission.clone(), parent.clone(), None) } else {
            let host = self.host(&mut state)?;
            let gate = self.gate(&mut state)?;
            Group::create((&self.admission, &self.work), &self.job, host, gate, self._attempt, None, false)?
        };
        group.retain();
        group.prepared.store(true, Ordering::Release);
        state.groups.retain(|g| !g.finished.is_cancelled());
        state.groups.push(Arc::clone(&group));
        Ok(PreparedCleanup { command: Some(command), group })
    }

    pub(super) fn retain(self: &Arc<Self>) -> anyhow::Result<RetainedExecution> {
        let mut state = self.state.lock();
        anyhow::ensure!(self.accepts_work() && !state.fenced, "debug ownership must be retained before the execution closes");
        let (host, gate) = if self.parent.is_some() { (None, None) } else {
            let host = self.host(&mut state)?;
            let gate = self.gate(&mut state)?.context("debug ownership has no physical execution gate")?;
            gate.retain()?;
            (Some(host), Some(gate))
        };
        state.kept = true;
        Ok(RetainedExecution { execution: Arc::clone(self), host, gate, docker: state.docker.clone() })
    }

    /// Capturing this exact execution, rather than finding by job id, fences cancellation
    /// timers belonging to a previous attempt. A stopped scope can never spawn again.
    async fn close(self: &Arc<Self>) -> Vec<Arc<Group>> {
        self.fence();
        let groups = loop {
            let this = Arc::clone(self);
            match crate::rt::blocking(move || {
                let mut state = this.state.lock();
                state.fenced = true;
                // A retry also finalizes a previously closed aggregate whose kernel proof
                // was still pending. This reuses the existing Stop/recovery owner.
                if let Some(gate) = &state.gate { gate.close()?; }
                state.closed = true;
                Ok::<_, anyhow::Error>(state.groups.clone())
            }).await {
                Ok(Ok(groups)) => break groups,
                error => tracing::error!(?error, "execution fencing could not persist; retaining physical ownership"),
            }
            crate::rt::sleep(Duration::from_secs(1)).await;
        };
        let mut announced = false;
        while self.parent.is_none() {
            let owners = groups.iter().filter_map(|g| g.docker.clone()).collect::<Vec<_>>();
            let admission = self.admission.clone();
            let quiesced = crate::rt::blocking(move || { for owner in owners { owner.quiesce(&admission)?; } Ok::<_, anyhow::Error>(()) }).await;
            match quiesced {
                Ok(Ok(())) => break,
                error if !announced => { announced = true; tracing::error!(?error, job = %self.job, "daemon operations retain the execution fence before process cleanup"); },
                _ => {},
            }
            crate::rt::sleep(Duration::from_millis(100)).await;
        }
        groups
    }

    /// Stop ordinary job commands before a preplanned workspace utility. The already-owned
    /// daemon and cleanup groups still hold this execution's physical gate and host permit.
    pub(super) async fn stop_commands(self: &Arc<Self>) {
        let groups = self.close().await;
        let commands = groups.into_iter().filter(|g| g.docker.is_none() && !g.prepared.load(Ordering::Acquire)).collect::<Vec<_>>();
        for group in &commands { group.start_retire(); }
        for group in commands { group.finished.cancelled().await; }
    }

    pub(crate) async fn stop(self: &Arc<Self>) {
        let groups = self.close().await;
        let commands = groups.iter().filter(|g| g.docker.is_none() && !g.prepared.load(Ordering::Acquire)).collect::<Vec<_>>();
        for group in &commands { group.start_retire(); }
        for group in commands { group.finished.cancelled().await; }
        let kept = loop {
            let (gate, local_kept) = { let state = self.state.lock(); (state.gate.clone(), state.kept) };
            match crate::rt::blocking(move || gate.map(|gate| gate.retained()).transpose()).await {
                Ok(Ok(retained)) => break self.operation.is_none() && retained.unwrap_or(local_kept),
                error => tracing::error!(?error, "execution cleanup cannot read its retained ownership; keeping the physical fence"),
            }
            crate::rt::sleep(Duration::from_secs(1)).await;
        };
        if !kept {
            // A reserved utility may legitimately run after ordinary command admission is
            // closed. Sweeping its daemon first can kill that utility and strand its intent.
            // Its token must finish (or prove non-submission) before final daemon retirement.
            for group in groups.iter().filter(|g| g.prepared.load(Ordering::Acquire)) { group.finished.cancelled().await; }
            for group in groups.iter().filter(|g| g.docker.is_some()) { group.start_retire(); }
            for group in &groups { group.finished.cancelled().await; }
        }
        let mut state = self.state.lock();
        state.groups.clear();
        state.docker.clear();
        state.host = None;
        // U01: the process-local ordered owner retains this SAME Gate permit until its
        // Stop barrier is committed to the canonical outbox and Startup is reclaimed.
        // Periodic physical recovery may publish the proof meanwhile, but cannot unlink
        // it before an ordered Stop has read it. Ordinary job cleanup keeps its policy.
        if self.executor_owner.is_none() { state.gate = None; }
    }

    /// U01: stop() closes ordinary admission but can preserve retained debug/daemon groups.
    /// true is exact durable physical retirement, false is pending retained cleanup, and an
    /// error is unknown evidence. Neither false nor missing evidence authorizes a stop receipt.
    pub(super) async fn stopped_proven(&self) -> anyhow::Result<bool> {
        let owner = self.executor_owner.clone().context("physical stop proof requires its existing executor grant")?;
        let identity = self.identity.clone();
        let admission = self.admission.clone();
        crate::rt::blocking(move || executions::stopped(&admission, &identity, &owner)).await?
    }

    fn spawn(&self, command: std::process::Command) -> anyhow::Result<(crate::proc::Duplex, Arc<Group>)> {
        let command = command_environment(command)?;
        let mut state = self.state.lock();
        anyhow::ensure!(self.accepts_work() && !state.fenced, "this execution has already stopped");
        let group = if let Some(parent) = &self.parent { Group::inherited(self.admission.clone(), parent.clone(), None) } else {
            let host = self.host(&mut state)?;
            let gate = self.gate(&mut state)?;
            Group::create((&self.admission, &self.work), &self.job, host, gate, self._attempt, None, true)?
        };
        state.groups.retain(|g| !g.finished.is_cancelled());
        state.groups.push(Arc::clone(&group));
        let launched = match &group.kernel {
            Some(kernel) => kernel.spawn(command),
            None => crate::os::proc::spawn_duplex_raw(command),
        };
        let duplex = match launched {
            Ok(duplex) => duplex,
            Err(e) => { group.start_retire(); return Err(e) }
        };
        if group.parent.is_some() { *group.provisional_kill.lock() = Some(duplex.group_killer()); }
        group.pgid.store(i32::try_from(duplex.pid).unwrap_or(0), Ordering::Release);
        Ok((duplex, group))
    }
}

/// CI commands inherit the runner environment plus their evaluated overrides. Native leased
/// launchers do not use std::Command's spawn implementation, so materialize that environment
/// and resolve the executable before handing it to launchd or CreateProcessW.
fn command_environment(command: std::process::Command) -> anyhow::Result<std::process::Command> {
    #[cfg(any(target_os = "macos", windows))]
    {
        let mut env = std::env::vars_os().collect::<std::collections::BTreeMap<_, _>>();
        for (key, value) in command.get_envs() {
            #[cfg(windows)]
            if let Some(existing) = env.keys().find(|existing| existing.to_string_lossy().eq_ignore_ascii_case(&key.to_string_lossy())).cloned() { env.remove(&existing); }
            match value { Some(value) => { env.insert(key.to_owned(), value.to_owned()); }, None => { env.remove(key); } }
        }
        let program = command.get_program();
        let program = if Path::new(program).is_absolute() { PathBuf::from(program) } else {
            let path = env.iter().find(|(key, _)| {
                #[cfg(windows)]
                { key.to_string_lossy().eq_ignore_ascii_case("PATH") }
                #[cfg(not(windows))]
                { key.as_os_str() == std::ffi::OsStr::new("PATH") }
            }).map(|(_, value)| value).context("CI command has no PATH")?;
            crate::os::which_in(&program.to_string_lossy(), path).context("CI command is not on PATH")?
        };
        let mut normalized = std::process::Command::new(program);
        normalized.args(command.get_args()).env_clear().envs(env);
        if let Some(dir) = command.get_current_dir() { normalized.current_dir(dir); }
        Ok(normalized)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    { Ok(command) }
}

#[derive(Clone, Serialize, Deserialize)]
struct Record { id: String, job: String, runner: u32, started: u64, identity: String, #[serde(default)] execution: Option<String>, #[serde(default)] attempt: Option<u32>, #[serde(default)] parent: Option<ParentBinding>, #[serde(default)] work: PathBuf, #[serde(default, skip_serializing_if = "Option::is_none")] local: Option<PathBuf>, #[serde(default)] docker: Option<super::docker::Ownership> }
struct Ownership { _permit: std::fs::File, _host: Arc<std::fs::File>, gate: Option<Arc<executions::Gate>>, fence: PathBuf, permit_path: PathBuf, _custody: Option<Box<dyn Send>> }

pub(crate) struct Group {
    admission: Admission,
    pgid: AtomicI32,
    kernel: Option<Containment>,
    parent: Option<ParentExecution>,
    provisional_kill: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    provisional_retired: AtomicBool,
    initialized: AtomicBool,
    prepared: AtomicBool,
    record: PathBuf,
    ownership: Mutex<Option<Ownership>>,
    captured_stop: Mutex<Option<crate::rt::Shutdown>>,
    finished: crate::rt::Shutdown,
    retained: AtomicBool,
    docker: Option<super::docker::Ownership>,
}
impl std::fmt::Debug for Group {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.debug_struct("ExecutionGroup").field("record", &self.record).finish_non_exhaustive() }
}
impl Group {
    pub(crate) fn identity(&self) -> &Path { &self.record }
    pub(crate) fn pgid(&self) -> i32 { self.pgid.load(Ordering::Acquire) }
    pub(crate) fn kill(&self) { self.start_retire(); }
    pub(crate) fn retain(&self) { self.retained.store(true, Ordering::Release); }
    pub(crate) async fn stop(&self) { self.start_retire(); self.finished.cancelled().await; }

    fn bind_parent_metadata(&self, parent: &ParentBinding) -> anyhow::Result<()> {
        // Taking the existing ownership mutex serializes adoption with retirement. A group
        // already retiring can start no further process and remains covered by the root.
        let held = self.ownership.lock();
        let Some(held) = held.as_ref() else { return Ok(()) };
        let mut record: Record = serde_json::from_slice(&std::fs::read(&held.fence)?)?;
        record.job = parent.job.clone();
        record.attempt = Some(parent.attempt);
        record.parent = Some(parent.clone());
        save(&held.fence, &record)
    }

    fn inherited(admission: Admission, parent: ParentExecution, docker: Option<super::docker::Ownership>) -> Arc<Self> {
        // An in-memory command handle inside the parent's physical domain. It creates no
        // durable local allocation or stop certificate: the parent remains the sole owner.
        Arc::new(Self { admission, pgid: AtomicI32::new(0), kernel: None, parent: Some(parent), provisional_kill: Mutex::new(None), provisional_retired: AtomicBool::new(false), initialized: AtomicBool::new(false), prepared: AtomicBool::new(false), record: PathBuf::from(format!("inherited-ci-{}", crate::util::new_id())), ownership: Mutex::new(None), captured_stop: Mutex::new(None), finished: crate::rt::Shutdown::new(), retained: AtomicBool::new(false), docker })
    }

    fn create((admission, work): (&Admission, &Path), job: &str, host: Arc<std::fs::File>, gate: Option<Arc<executions::Gate>>, attempt: Option<u32>, docker: Option<super::docker::Ownership>, initialize: bool) -> anyhow::Result<Arc<Self>> {
        crate::os::contain::hold_agents();
        let id = format!("ci-{}", crate::util::new_id());
        let root = admission.ci();
        std::fs::create_dir_all(&root)?;
        let fence = root.join(format!("{id}.json"));
        let permit_path = root.join(format!("{id}.lock"));
        let permit = std::fs::OpenOptions::new().create_new(true).read(true).write(true).open(&permit_path)?;
        permit.try_lock()?;
        let child_id = id.clone();
        let admission = admission.clone();
        let native_admission = admission.clone();
        let owner = gate.clone();
        let register = move |kernel: Containment, parent: Option<ParentBinding>| -> anyhow::Result<Arc<Self>> {
            let record = Record { id, job: parent.as_ref().map_or_else(|| job.into(), |p| p.job.clone()), runner: std::process::id(), started: own_started().context("the runner has no process identity")?, identity: kernel.identity(), execution: gate.as_ref().map(|g| g.identity.clone()), attempt: parent.as_ref().map(|p| p.attempt).or(attempt), parent, work: work.into(), local: None, docker: docker.clone() };
            // Registration is durable while the SAME Gate lifecycle lock still excludes
            // retained release/stop proof. No launch can disappear between those owners.
            save(&fence, &record)?;
            let owned = Arc::new(Self { admission: admission.clone(), pgid: AtomicI32::new(0), kernel: Some(kernel), parent: None, provisional_kill: Mutex::new(None), provisional_retired: AtomicBool::new(false), initialized: AtomicBool::new(false), prepared: AtomicBool::new(!initialize && docker.is_none()), record: fence.clone(), ownership: Mutex::new(Some(Ownership { _permit: permit, _host: host, gate, fence: fence.clone(), permit_path, _custody: None })), captured_stop: Mutex::new(None), finished: crate::rt::Shutdown::new(), retained: AtomicBool::new(false), docker });
            if initialize { owned.initialize()?; }
            Ok(owned)
        };
        match owner {
            Some(owner) => owner.with_child(&child_id, register),
            None => register(Containment::create(&native_admission, &child_id)?, None),
        }
    }

    fn initialize(&self) -> anyhow::Result<()> {
        let Some(kernel) = &self.kernel else { return Ok(()) };
        let held = self.ownership.lock();
        let held = held.as_ref().context("execution ownership disappeared")?;
        if self.initialized.load(Ordering::Acquire) { return Ok(()) }
        kernel.initialize(&held._permit, Some(held._host.as_ref()))?;
        let mut record: Record = serde_json::from_slice(&std::fs::read(&held.fence)?)?;
        record.identity = kernel.identity();
        save(&held.fence, &record)?;
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    fn start_retire(&self) {
        // U08: an owned capture's task was admitted before its launch. Signal that SAME
        // owner, which retires inline; cancellation must not need a fresh task admission or
        // wait on the ownership mutex while a blocking native initialization is in progress.
        if let Some(stop) = self.captured_stop.lock().as_ref() { stop.cancel(); return; }
        if self.parent.is_some() {
            if !self.provisional_retired.swap(true, Ordering::AcqRel) {
                if let Some(kill) = self.provisional_kill.lock().take() { kill(); }
                // This only ends local command handling. It is NOT evidence that escaped
                // descendants died; canonical completion waits for the parent's kernel proof.
                self.finished.cancel();
            }
            return;
        }
        let Some(kernel) = self.kernel.clone() else { return };
        let Some(held) = self.ownership.lock().take() else { return };
        let (finished, docker, admission) = (self.finished.clone(), self.docker.clone(), self.admission.clone());
        // Retirement must not retain the job scope, which itself owns these groups.
        within(None, || crate::rt::spawn(Self::retire_native(kernel, held, finished, docker, admission, false)));
    }

    /// U08: the original native proof path, shared by ordinary cleanup and the pre-admitted
    /// captured owner. A refusal, unreadable proof or failed durable removal keeps custody.
    async fn retire_native(kernel: Containment, held: Ownership, finished: crate::rt::Shutdown, docker: Option<super::docker::Ownership>, admission: Admission, strict_pool: bool) {
        let _held = held;
        if let Some(docker) = docker { docker.reconcile(&admission).await; }
        loop {
            let group = kernel.clone();
            let stopped = if strict_pool {
                captured::blocking(move || { group.kill(); group.empty_proven() }).await
            } else {
                crate::rt::blocking(move || { group.kill(); group.empty_proven() }).await
            };
            if matches!(stopped, Ok(Ok(true))) {
                kernel.remove();
                if retire_files(&_held).is_ok() { drop(_held); finished.cancel(); return }
            }
            crate::rt::sleep(Duration::from_millis(100)).await;
        }
    }
}
impl Drop for Group { fn drop(&mut self) { self.start_retire(); } }

/// A kept leg can accept new debug work only through this same physical owner. Keeping the
/// root permit prevents a workspace-only keep from being mistaken for stopped execution.
pub(super) struct RetainedExecution { execution: Arc<Execution>, host: Option<Arc<std::fs::File>>, gate: Option<Arc<executions::Gate>>, docker: std::collections::BTreeMap<String, Arc<Group>> }
impl RetainedExecution {
    pub(super) fn debug_child(&self) -> Arc<Execution> {
        let original = &self.execution;
        Arc::new(Execution { admission: original.admission.clone(), work: original.work.clone(), job: original.job.clone(), _attempt: original._attempt, identity: original.identity.clone(), operation: Some(format!("ci-{}", crate::util::new_id())), reconciling: false, parent: original.parent.clone(), executor_owner: original.executor_owner.clone(), ordinary_owner: original.ordinary_owner.clone(), fenced: AtomicBool::new(false), state: Mutex::new(State { host: self.host.clone(), gate: self.gate.clone(), docker: self.docker.clone(), closed: true, ..State::default() }) })
    }

    pub(super) async fn release(self) {
        self.execution.stop().await;
        // Host jobs can own Docker actions without a job Containers wrapper. The captured
        // alias map is their original daemon authority too; its exact proof precedes both
        // the retained release event and the durable no-future-debug transition.
        for group in self.docker.values() { group.start_retire(); }
        for group in self.docker.values() { group.finished.cancelled().await; }
        self.execution.state.lock().kept = false;
        if let Some(gate) = &self.gate {
            loop {
                let gate = Arc::clone(gate);
                match crate::rt::blocking(move || gate.release()).await {
                    Ok(Ok(())) => break,
                    error => tracing::error!(?error, "retained physical ownership cannot persist its release proof"),
                }
                crate::rt::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

#[derive(Debug)]
pub(super) struct PreparedCleanup { command: Option<std::process::Command>, group: Arc<Group> }
impl PreparedCleanup {
    /// Dropping this waiter cannot interrupt the one preowned response/drain operation. Its
    /// kernel group stays linked to the original execution until the process is proven dead.
    pub(super) async fn run(self) -> anyhow::Result<std::process::Output> { self.run_bound(None).await }

    /// A command deadline ends its raw wait, then kills and proves the owned group empty
    /// before returning. Dropping the caller still leaves this operation owned to completion.
    pub(super) async fn run_with_timeout(self, timeout: Duration) -> anyhow::Result<std::process::Output> {
        self.run_bound(Some(timeout)).await
    }

    async fn run_bound(mut self, timeout: Option<Duration>) -> anyhow::Result<std::process::Output> {
        let command = self.command.take().context("the cleanup command was already consumed")?;
        let group = Arc::clone(&self.group);
        within(None, || crate::rt::spawn(async move {
            let owning = Arc::clone(&group);
            let launched = crate::rt::blocking(move || {
                // launchd's one-shot gate is initialized immediately before this fixed
                // utility, rather than expiring while a long-running job still executes.
                owning.initialize()?;
                let duplex = match &owning.kernel { Some(kernel) => kernel.spawn(command)?, None => crate::os::proc::spawn_duplex_raw(command)? };
                if owning.parent.is_some() { *owning.provisional_kill.lock() = Some(duplex.group_killer()); }
                owning.pgid.store(i32::try_from(duplex.pid).unwrap_or(0), Ordering::Release);
                Ok::<_, anyhow::Error>(duplex)
            }).await;
            let output = match launched {
                Ok(Ok(duplex)) => {
                    duplex.stdin.close();
                    let drain = async move {
                        let (stdout, stderr, code) = futures::join!(collect(duplex.stdout), collect(duplex.stderr), duplex.exit);
                        std::process::Output { status: status(code.ok().flatten()), stdout, stderr }
                    };
                    match timeout {
                        Some(timeout) => crate::rt::timeout(timeout, drain).await.map_err(|_| anyhow::anyhow!("timed out after {timeout:?}")),
                        None => Ok(drain.await),
                    }
                }
                Ok(Err(e)) => Err(e),
                Err(e) => Err(e),
            };
            group.stop().await;
            output
        })).await?
    }
}
impl Drop for PreparedCleanup { fn drop(&mut self) { if self.command.is_some() { self.group.retained.store(false, Ordering::Release); self.group.start_retire(); } } }

fn save(path: &Path, record: &impl Serialize) -> anyhow::Result<()> {
    use std::io::Write as _;
    let parent = path.parent().context("process record has no parent")?;
    std::fs::create_dir_all(parent)?;
    let next = path.with_extension("next");
    let mut file = std::fs::OpenOptions::new().create(true).truncate(true).write(true).open(&next)?;
    file.write_all(&serde_json::to_vec(record)?)?;
    file.sync_all()?;
    std::fs::rename(next, path)?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn remove(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) { Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()), Err(e) => return Err(e.into()) }
    #[cfg(unix)]
    if let Some(parent) = path.parent() { std::fs::File::open(parent)?.sync_all()?; }
    Ok(())
}
fn retire_files(held: &Ownership) -> anyhow::Result<()> {
    if let Some(gate) = &held.gate { executions::registered(&gate.admission, &gate.identity)?; }
    remove(&held.fence)?;
    remove(&held.permit_path)?;
    if let Some(gate) = &held.gate { gate.try_stopped()?; }
    Ok(())
}
fn process_started(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), false, ProcessRefreshKind::nothing());
    system.process(pid).map(sysinfo::Process::start_time)
}
fn own_started() -> Option<u64> {
    static START: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *START.get_or_init(|| process_started(std::process::id()))
}
fn records(admission: &Admission) -> anyhow::Result<Vec<(PathBuf, Record)>> {
    let entries = match std::fs::read_dir(admission.ci()) { Ok(entries) => entries, Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()), Err(e) => return Err(e.into()) };
    let mut out = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|p| p.to_str()) != Some("json") { continue }
        let bytes = match std::fs::read(&path) { Ok(bytes) => bytes, Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue, Err(e) => return Err(e.into()) };
        let record: Record = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(path.file_stem().and_then(|n| n.to_str()) == Some(record.id.as_str()), "invalid process fence identity");
        out.push((path, record));
    }
    Ok(out)
}

/// A CI assignment cannot pass an unresolved crash fence. Live executions carry their own
/// shared host locks and remain the Scheduler's responsibility.
pub(crate) fn ensure_no_orphans(admission: &Admission) -> anyhow::Result<()> {
    ensure_legacy_clear(admission)?;
    let mut owners = std::collections::HashMap::new();
    for (fence, record) in records(admission)? {
        let started = owners.entry(record.runner).or_insert_with(|| if record.runner == std::process::id() { own_started() } else { process_started(record.runner) });
        anyhow::ensure!(record.started > 0 && *started == Some(record.started), "a previous CI execution needs kernel containment reconciliation");
        let path = fence.with_extension("lock");
        let permit = match std::fs::OpenOptions::new().read(true).write(true).open(path) {
            Ok(permit) => permit,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !fence.try_exists()? => continue,
            Err(e) => return Err(e.into()),
        };
        match permit.try_lock() {
            Ok(()) => anyhow::bail!("a previous CI execution needs kernel containment reconciliation"),
            Err(std::fs::TryLockError::WouldBlock) => {},
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Call while owning the host lock, before admission accepts work. A missing, unreadable or
/// still-populated kernel group keeps its durable fence; it is never interpreted as stopped.
pub(crate) async fn reconcile_host(admission: &Admission) -> anyhow::Result<()> {
    for (fence, record) in records(admission)? {
        let permit_path = fence.with_extension("lock");
        let permit = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&permit_path)?;
        match permit.try_lock() {
            Ok(()) => {},
            Err(std::fs::TryLockError::WouldBlock) => continue,
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        recover_record(admission.clone(), fence, record, permit).await?;
    }
    executions::recover(admission)?;
    ensure_legacy_clear(admission)?;
    Ok(())
}

fn ensure_legacy_clear(admission: &Admission) -> anyhow::Result<()> {
    let path = admission.root().join("legacy-ci");
    let mut entries = match std::fs::read_dir(&path) { Ok(entries) => entries, Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()), Err(e) => return Err(e.into()) };
    if let Some(entry) = entries.next() {
        anyhow::bail!("legacy CI ownership at {} lacks a kernel termination proof; drain legacy runners and reconcile this host before admitting work", entry?.path().display());
    }
    Ok(())
}

async fn recover_record(admission: Admission, fence: PathBuf, record: Record, _permit: std::fs::File) -> anyhow::Result<()> {
    if let Some(docker) = &record.docker { docker.reconcile(&admission).await; }
    let kernel = Containment::recover(&admission, &record.identity, &record.id)?;
    loop {
        let group = kernel.clone();
        if crate::rt::blocking(move || { group.kill(); group.empty_proven() }).await?? { break }
        crate::rt::sleep(Duration::from_millis(100)).await;
    }
    kernel.remove();
    if let Some(execution) = &record.execution { executions::registered(&admission, execution)?; }
    if let Some(local) = &record.local { remove(local)?; }
    remove(&fence)?;
    remove(&fence.with_extension("lock"))?;
    // Aggregate finalization belongs to the live Gate owner, or executions::recover
    // under its exclusive Gate permit after this recovered leaf has retired.
    Ok(())
}

/// Compatibility PGID records cannot prove that a process which escaped its old process
/// group is dead. Kill marked groups, retain their evidence and refuse new execution until
/// the old runner is drained and the host's legacy records have been reconciled.
pub async fn reap(work: &Path) -> anyhow::Result<usize> { reap_in(work, &Admission::production()).await }

pub(crate) async fn reap_in(work: &Path, admission: &Admission) -> anyhow::Result<usize> {
    let _host = crate::reach::leases::host_guard(admission, true)?;
    reconcile_host(admission).await?;
    for dir in ["processes", "pgids"] {
        let entries = match std::fs::read_dir(work.join(dir)) { Ok(entries) => entries, Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue, Err(e) => return Err(e.into()) };
        for entry in entries {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with("ci-") { continue }
            let bytes = std::fs::read(entry.path())?;
            let pgid = entry.file_name().to_string_lossy().split('.').next().unwrap_or_default().parse::<i32>().unwrap_or(0);
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            let job = value.get("job").and_then(serde_json::Value::as_str).map(str::to_owned).unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
            let marker = format!("{}={job}", if value.get("job").is_some() { "KILN_LEG" } else { "KILN_JOB_ID" });
            let source = std::path::absolute(entry.path())?;
            let fence = admission.root().join("legacy-ci").join(format!("{}.json", crate::util::sha256_hex(source.to_string_lossy().as_bytes())));
            #[cfg(target_os = "linux")]
            let boot = Some(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim().to_owned());
            #[cfg(not(target_os = "linux"))]
            let boot: Option<String> = None;
            save(&fence, &serde_json::json!({ "source": source, "sha256": crate::util::sha256_hex(&bytes), "recorded_at": crate::util::now_unix(), "boot_id": boot, "reason": "legacy process groups cannot prove termination of escaped descendants" }))?;
            let live_owner = value.get("runner").and_then(serde_json::Value::as_u64).and_then(|p| u32::try_from(p).ok()).zip(value.get("started").and_then(serde_json::Value::as_u64)).is_some_and(|(pid, started)| started > 0 && process_started(pid) == Some(started));
            // A replacement never controls a live runner's jobs. Their owner must drain them;
            // only abandoned groups may be killed during recovery.
            if !live_owner { crate::proc::kill_orphan_group(pgid, &marker); }
            anyhow::bail!("legacy process ownership at {} has no kernel containment proof; reconcile this host before admitting work", entry.path().display());
        }
    }
    Ok(0)
}

/// Adapt the shared kernel launcher to the existing step pipe interface.
pub(crate) fn spawn_group(command: std::process::Command, execution: &Arc<Execution>) -> anyhow::Result<crate::proc::Running> {
    let (duplex, owned) = execution.spawn(command)?;
    Ok(crate::os::proc::from_contained(duplex, owned))
}

pub(crate) fn spawn_duplex(command: std::process::Command, execution: &Arc<Execution>) -> anyhow::Result<crate::proc::Duplex> {
    execution.spawn(command).map(|(duplex, _)| duplex)
}

pub(crate) fn spawn_pty_bridge(command: std::process::Command, execution: &Arc<Execution>) -> anyhow::Result<(crate::proc::Duplex, Arc<Group>)> { execution.spawn(command) }

fn status(code: Option<i32>) -> std::process::ExitStatus {
    #[cfg(unix)]
    { use std::os::unix::process::ExitStatusExt as _; std::process::ExitStatus::from_raw(code.map_or(9, |c| c << 8)) }
    #[cfg(windows)]
    { use std::os::windows::process::ExitStatusExt as _; std::process::ExitStatus::from_raw(code.and_then(|c| u32::try_from(c).ok()).unwrap_or(1)) }
}
async fn collect(rx: async_channel::Receiver<Vec<u8>>) -> Vec<u8> { let mut out = Vec::new(); while let Ok(chunk) = rx.recv().await { out.extend(chunk); } out }

pub(crate) async fn output(command: std::process::Command, timeout: Duration, execution: Arc<Execution>) -> anyhow::Result<std::process::Output> {
    let (duplex, group) = crate::rt::blocking(move || execution.spawn(command)).await??;
    struct Stop(Arc<Group>);
    impl Drop for Stop { fn drop(&mut self) { self.0.start_retire(); } }
    let _stop = Stop(group);
    duplex.stdin.close();
    crate::rt::timeout(timeout, async move {
        let (stdout, stderr, code) = futures::join!(collect(duplex.stdout), collect(duplex.stderr), duplex.exit);
        Ok(std::process::Output { status: status(code.ok().flatten()), stdout, stderr })
    }).await.map_err(|_| anyhow::anyhow!("timed out after {timeout:?}"))?
}

/// Blocking repository/image helpers use the same scoped launcher. Their waits run off the
/// shards; the actual pipes and deadline remain on the runtime.
pub(crate) fn output_blocking(mut command: std::process::Command) -> std::io::Result<std::process::Output> {
    match current() {
        Some(execution) => crate::rt::block_on(async move { output(command, Duration::from_secs(1800), execution).await }).map_err(std::io::Error::other),
        None => command.output(),
    }
}

/// Streaming blocking helpers (OCI image archives, a microVM console) keep bounded pipe
/// buffers while still using the scoped kernel launcher.
pub(crate) struct Child {
    pub stdout: Option<Box<dyn std::io::Read + Send>>,
    pub stderr: Option<Box<dyn std::io::Read + Send>>,
    inner: ChildInner,
}
enum ChildInner {
    Raw(std::process::Child),
    Contained { exit: futures::channel::oneshot::Receiver<Option<i32>>, group: Arc<Group>, status: Option<std::process::ExitStatus> },
}
struct Reader { input: async_channel::Receiver<Vec<u8>>, chunk: std::io::Cursor<Vec<u8>> }
impl std::io::Read for Reader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if bytes.is_empty() { return Ok(0) }
        loop {
            let n = self.chunk.read(bytes)?;
            if n > 0 { return Ok(n) }
            match self.input.recv_blocking() { Ok(chunk) => self.chunk = std::io::Cursor::new(chunk), Err(_) => return Ok(0) }
        }
    }
}
pub(crate) fn spawn_blocking(mut command: std::process::Command) -> anyhow::Result<Child> {
    if let Some(execution) = current() {
        let (duplex, group) = execution.spawn(command)?;
        duplex.stdin.close();
        let reader = |input| Some(Box::new(Reader { input, chunk: std::io::Cursor::new(Vec::new()) }) as Box<dyn std::io::Read + Send>);
        Ok(Child { stdout: reader(duplex.stdout), stderr: reader(duplex.stderr), inner: ChildInner::Contained { exit: duplex.exit, group, status: None } })
    } else {
        let mut child = command.spawn()?;
        Ok(Child { stdout: child.stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>), stderr: child.stderr.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>), inner: ChildInner::Raw(child) })
    }
}
impl Child {
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        match &mut self.inner {
            ChildInner::Raw(child) => child.try_wait(),
            ChildInner::Contained { exit, status: saved, .. } => {
                if saved.is_none() && let Some(code) = exit.try_recv().map_err(std::io::Error::other)? { *saved = Some(status(code)); }
                Ok(*saved)
            }
        }
    }
    pub(crate) fn kill(&mut self) -> std::io::Result<()> {
        match &mut self.inner { ChildInner::Raw(child) => child.kill(), ChildInner::Contained { group, .. } => { group.start_retire(); Ok(()) } }
    }
    pub(crate) fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        loop { if let Some(status) = self.try_wait()? { return Ok(status) } std::thread::sleep(Duration::from_millis(20)); }
    }
    /// One sequential phase ends only after its actual group is empty. Keep the outer
    /// execution open: its original grant also owns the next builder/template/guest phase.
    #[cfg(target_os = "linux")]
    pub(crate) async fn retire(mut self) -> std::io::Result<()> {
        self.stdout.take();
        self.stderr.take();
        let group = match &self.inner {
            ChildInner::Contained { group, .. } => Some(Arc::clone(group)),
            ChildInner::Raw(_) => None,
        };
        if let Some(group) = group {
            if group.parent.is_some() {
                group.kill();
                return Err(std::io::Error::other("an inherited command cannot prove its parent's physical group empty"));
            }
            group.stop().await;
            return Ok(());
        }
        crate::rt::blocking(move || {
            if self.try_wait()?.is_none() { self.kill()?; }
            self.wait().map(|_| ())
        }).await.map_err(std::io::Error::other)?
    }
    pub(crate) fn wait_with_output(mut self) -> std::io::Result<std::process::Output> {
        let read = |pipe: Option<Box<dyn std::io::Read + Send>>| std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
            use std::io::Read as _;
            let mut bytes = Vec::new(); if let Some(mut pipe) = pipe { pipe.read_to_end(&mut bytes)?; } Ok(bytes)
        });
        let (out, err) = (read(self.stdout.take()), read(self.stderr.take()));
        let status = self.wait()?;
        let stdout = out.join().map_err(|_| std::io::Error::other("command stdout reader failed"))??;
        let stderr = err.join().map_err(|_| std::io::Error::other("command stderr reader failed"))??;
        Ok(std::process::Output { status, stdout, stderr })
    }
}
impl Drop for Child { fn drop(&mut self) { let _ = self.kill(); } }

pub(crate) fn stdout_stream(command: std::process::Command, execution: &Arc<Execution>) -> anyhow::Result<async_channel::Receiver<bytes::Bytes>> {
    let (duplex, group) = execution.spawn(command)?;
    duplex.stdin.close();
    let (tx, rx) = async_channel::bounded(16);
    crate::rt::spawn(async move { while duplex.stderr.recv().await.is_ok() {} });
    crate::rt::spawn(async move {
        while let Ok(chunk) = duplex.stdout.recv().await { if tx.send(bytes::Bytes::from(chunk)).await.is_err() { break } }
        group.start_retire();
    });
    Ok(rx)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn u03_executor_adoption_and_retained_debug_keep_the_existing_physical_owner() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let admission = Admission::private(&tempfile::tempdir()?.keep())?;
            let work = tempfile::tempdir()?;
            let execution = Execution::allocate_executor(admission.clone(), work.path().into(), "executor-1".into(), "generation-1".into(), "existing-grant-1".into())?;
            execution.prepare_executor_root()?;
            let parent = ParentBinding { executor: "executor-1".into(), generation: "generation-1".into(), allocation: "existing-grant-1".into(), physical_execution: execution.identity().into(), job: "assigned-leg".into(), attempt: 2 };
            let mut wrong = parent.clone();
            wrong.allocation = "another-grant".into();
            assert!(execution.bind_parent(wrong).is_err());
            execution.bind_parent(parent.clone())?;
            let mut newer = parent.clone();
            newer.attempt = 3;
            assert!(execution.bind_parent(newer).is_err(), "an executor cannot be rebound to a different job attempt");
            assert!(!execution.stopped_proven().await?, "an open Gate has no physical stop proof");
            let retained = execution.retain()?;
            execution.stop().await;
            assert!(!execution.stopped_proven().await?, "ordinary stop cannot release a retained physical owner");
            assert!(!recovery_evidence(&admission)?.iter().any(|p| p.execution == execution.identity()), "a workspace-only keep still permits future debug work");
            let debug = retained.debug_child();
            assert_eq!(debug.identity(), execution.identity());
            let out = debug.scope(async {
                let mut command = std::process::Command::new("sh");
                command.args(["-c", "printf retained"]);
                crate::proc::output(command, Duration::from_secs(5)).await
            }).await?;
            assert!(out.status.success());
            assert_eq!(out.stdout, b"retained");
            debug.stop().await;
            assert!(!recovery_evidence(&admission)?.iter().any(|p| p.execution == execution.identity()));
            retained.release().await;
            assert!(execution.stopped_proven().await?, "retained release and all-groups retirement prove this same physical owner");
            let proof = recovery_evidence(&admission)?.into_iter().find(|p| p.execution == execution.identity()).context("the original physical owner's stopped evidence is missing")?;
            assert_eq!(proof.job, parent.job);
            assert_eq!(proof.attempt, Some(2));
            assert_eq!(proof.parent, Some(parent));
            assert_eq!(proof.was_retained, Some(true));
            assert!(!try_acknowledge_evidence(&admission, &proof)?, "physical outbox import cannot retire a live ordered owner's Gate");
            assert!(execution.stopped_proven().await?, "the barrier can still read the exact same proof after physical import");
            drop(debug);
            drop(execution); // the real owner drops only after its ordered-barrier outbox commit
            // The same positive proof may precede the final scoped pipe task's drop.
            crate::rt::timeout(Duration::from_secs(5), async {
                while !try_acknowledge_evidence(&admission, &proof)? {
                    crate::rt::sleep(Duration::from_millis(10)).await;
                }
                Ok::<_, anyhow::Error>(())
            }).await.context("stopped owner tasks still hold their exact Gate")??;
            acknowledge_evidence(&admission, &proof)?;
            let owner = proof.executor_owner.as_ref().context("executor proof lost its grant owner")?;
            assert!(executions::stopped(&admission, &proof.execution, owner).is_err(), "missing evidence is unknown, never another stop proof");

            // C1a is an aggregate CHILD-PROCESS envelope. A membership-only host exercises
            // refusal before any child; positive enforcement requires delegated controllers.
            let limited = Execution::allocate_executor(admission.clone(), work.path().into(), "limited-executor".into(), "limited-generation".into(), "limited-grant".into())?;
            limited.prepare_executor_root()?;
            let outer = kiln_ci::sched::state::Resources { cpu: 1.5, mem: 128 << 20, disk: 1 << 30, gpu: 0 };
            let overhead = kiln_ci::sched::state::Resources { cpu: 0.25, mem: 64 << 20, disk: 64 << 20, gpu: 0 };
            let shape = kiln_ci::sched::vm::FirecrackerShape::derive(outer, overhead).map_err(anyhow::Error::msg)?;
            let candidate = Arc::clone(&limited);
            match crate::rt::blocking(move || candidate.bind_vm_shape(shape)).await? {
                Err(error) => {
                    assert!(limited.state.lock().groups.is_empty(), "unavailable limits cannot launch a first child");
                    assert!(limited.vm_shape().is_err(), "failed setup cannot attest a bound guest");
                    eprintln!("C1a native child-limit admission refused before spawn: {error:#}");
                    limited.stop().await;
                }
                Ok(()) => {
                    let kernel_path = admission.ci().join("executions").join(format!("{}.json", limited.identity()));
                    let record: serde_json::Value = serde_json::from_slice(&std::fs::read(&kernel_path)?)?;
                    assert_eq!(record.get("phase").and_then(serde_json::Value::as_str), Some("v2_open"));
                    #[derive(Deserialize)]
                    #[serde(rename_all = "snake_case")]
                    enum LegacyPhase { Open, Closed, Stopped }
                    let old_phase = record.get("phase").context("limited Gate has no reader fence")?.clone();
                    assert!(serde_json::from_value::<LegacyPhase>(old_phase).is_err(), "an older reader cannot ignore the aggregate proof obligation");
                    let domain = record.get("resource_domain").and_then(|d| d.get("identity")).and_then(serde_json::Value::as_str).context("aggregate identity missing")?;
                    let domain = PathBuf::from(domain);
                    let (one, two) = limited.scope(crate::rt::blocking(|| -> anyhow::Result<_> {
                        let child = || { let mut c = std::process::Command::new("sleep"); c.arg("30"); spawn_blocking(c) };
                        Ok((child()?, child()?))
                    })).await??;
                    let groups = limited.state.lock().groups.clone();
                    assert_eq!(groups.len(), 2);
                    for group in &groups {
                        let identity = group.kernel.as_ref().context("limited child lost its kernel domain")?.identity();
                        assert_eq!(Path::new(&identity).parent(), Some(domain.as_path()), "all children share one original outer envelope");
                    }
                    let kept = limited.retain()?;
                    limited.stop().await;
                    assert!(!limited.stopped_proven().await?, "retained debug still owns the exact aggregate domain");
                    let debug = kept.debug_child();
                    let out = debug.scope(async {
                        let mut c = std::process::Command::new("sh"); c.args(["-c", "cat /proc/self/cgroup"]);
                        crate::proc::output(c, Duration::from_secs(5)).await
                    }).await?;
                    assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains(limited.identity()), "retained debug inherits the SAME aggregate envelope");
                    debug.stop().await;
                    kept.release().await;
                    let record: serde_json::Value = serde_json::from_slice(&std::fs::read(&kernel_path)?)?;
                    assert_eq!(record.get("phase").and_then(serde_json::Value::as_str), Some("v2_stopped"), "durable positive proof never downgrades the reader fence");
                    assert!(!domain.exists(), "aggregate metadata is removed only after durable positive proof");
                    drop((one, two, groups, debug));
                    eprintln!("C1a native aggregate child CPU/memory and retained-domain proof passed");
                }
            }
            assert!(limited.stopped_proven().await?);
            let proof = recovery_evidence(&admission)?.into_iter().find(|p| p.execution == limited.identity()).context("limited owner lost its physical proof")?;
            drop(limited);
            // The same positive proof may precede the final scoped pipe task's drop.
            crate::rt::timeout(Duration::from_secs(5), async {
                while !try_acknowledge_evidence(&admission, &proof)? {
                    crate::rt::sleep(Duration::from_millis(10)).await;
                }
                Ok::<_, anyhow::Error>(())
            }).await.context("stopped owner tasks still hold their exact Gate")??;
            acknowledge_evidence(&admission, &proof)?;
            Ok(())
        })
    }

    #[test]
    fn u03_inherited_commands_keep_the_parent_owner_and_fence_the_exact_attempt() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let binding = ParentBinding { executor: "vm-1".into(), generation: "nonce-1".into(), allocation: "grant-1".into(), physical_execution: "ci-physical-1".into(), job: "leg-1".into(), attempt: 2 };
            assert!(ParentExecution::verified(binding.clone(), "leg-1", 1).is_err());
            assert!(ParentExecution::verified(binding.clone(), "another-leg", 2).is_err());
            let owner = ParentExecution::verified(binding.clone(), "leg-1", 2)?;
            assert_eq!(owner.binding(), &binding);
            let admission = Admission::private(&tempfile::tempdir()?.keep())?;
            let work = tempfile::tempdir()?;
            let execution = Execution::inherited_in(admission.clone(), work.path().into(), "leg-1".into(), 2, owner)?;
            let refused = execution.scope(crate::proc::output_with_custody(std::process::Command::new("true"), Duration::from_secs(1), ())).await;
            assert!(matches!(refused, Err(error) if error.downcast_ref::<crate::proc::CustodyRefusal>() == Some(&crate::proc::CustodyRefusal::InheritedExecution)), "local inherited completion cannot release physical custody");
            assert!(execution.state.lock().groups.is_empty(), "unsupported custody launches no command or alternate physical owner");
            let out = execution.scope(async {
                let mut command = std::process::Command::new("sh");
                command.args(["-c", "printf inherited"]);
                crate::proc::output(command, Duration::from_secs(5)).await
            }).await?;
            assert!(out.status.success());
            assert_eq!(out.stdout, b"inherited");
            {
                let state = execution.state.lock();
                assert!(state.host.is_none(), "a child never acquires a second physical host grant");
                assert!(state.groups.iter().all(|g| g.parent.is_some() && g.kernel.is_none() && !g.identity().exists()));
            }
            execution.stop().await;
            assert!(execution.scope(async { crate::proc::output(std::process::Command::new("true"), Duration::from_secs(1)).await }).await.is_err());
            // The binding remains intact: this local finish supplies no release certificate
            // for grant-1. Only the parent's separate physical stop may release its capacity.
            assert_eq!(execution.parent.as_ref().map(ParentExecution::binding), Some(&binding));
            Ok(())
        })
    }

    #[test]
    fn u03_cleanup_contains_setsid_children_and_fences_old_attempts() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let admission = Admission::private(&tempfile::tempdir()?.keep())?;
            let dir = tempfile::tempdir()?;
            let marker = dir.path().join("escaped");
            let execution = Execution::new_in(admission.clone(), dir.path().into(), "contained-fixture".into(), Some(1));
            let output = execution.scope(async {
                // Both task and blocking boundaries retain the same execution. Clearing the
                // environment and leaving the process group cannot leave its kernel group.
                crate::rt::spawn(async {
                    crate::rt::blocking(|| {
                        let mut command = std::process::Command::new("sh");
                        command.arg("-c").arg("cat /proc/self/cgroup").env_clear();
                        output_blocking(command)
                    }).await
                }).await
            }).await???;
            assert!(output.status.success());
            assert!(String::from_utf8_lossy(&output.stdout).contains("/ci-"));
            // A sequential builder phase can lose its leader while an escaped descendant
            // still holds CPU/memory. Retire only that group before starting the next phase.
            let sequence_marker = dir.path().join("previous-phase");
            let path = sequence_marker.clone();
            let (child, phase) = execution.scope(crate::rt::blocking(move || -> anyhow::Result<_> {
                let mut command = std::process::Command::new("sh");
                command.args(["-c", "setsid sh -c 'sleep 30; printf late > \"$1\"' sh \"$1\" </dev/null >/dev/null 2>&1 &", "sh"]).arg(path);
                let mut child = spawn_blocking(command)?;
                let phase = match &child.inner {
                    ChildInner::Contained { group, .. } => Arc::clone(group),
                    ChildInner::Raw(_) => anyhow::bail!("the phase lost its physical owner"),
                };
                assert!(child.wait()?.success());
                Ok((child, phase))
            })).await??;
            assert!(!phase.kernel.as_ref().context("the phase has no kernel group")?.empty_proven()?, "leader exit alone does not empty the previous phase");
            child.retire().await?;
            assert!(phase.finished.is_cancelled());
            assert!(phase.kernel.as_ref().context("the phase has no kernel group")?.empty_proven()?);
            assert!(execution.accepts_work(), "phase retirement keeps the original outer grant open");
            let following = execution.scope(async {
                let mut command = std::process::Command::new("sh");
                command.args(["-c", "printf next-phase"]);
                crate::proc::output(command, Duration::from_secs(5)).await
            }).await?;
            assert!(following.status.success());
            assert_eq!(following.stdout, b"next-phase");
            // U08: return the SAME physical fence only after a successful command's escaped
            // descendant is physically gone, while the original execution remains open.
            let fence_path = dir.path().join("captured-custody.lock");
            let fence = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&fence_path)?;
            fence.try_lock()?;
            let probe = std::fs::OpenOptions::new().read(true).write(true).open(&fence_path)?;
            let owned_marker = dir.path().join("captured-late");
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "setsid sh -c 'sleep 30; printf late > \"$1\"' sh \"$1\" </dev/null >/dev/null 2>&1 & printf owned", "sh"]).arg(&owned_marker);
            let output = execution.scope(crate::proc::output_with_custody(command, Duration::from_secs(5), fence)).await?;
            assert!(output.status.success());
            assert_eq!(output.stdout, b"owned");
            let owned = execution.state.lock().groups.last().cloned().context("the captured command lost its existing group")?;
            assert!(owned.finished.is_cancelled());
            assert!(owned.kernel.as_ref().context("the captured command lost native containment")?.empty_proven()?);
            probe.try_lock()?;
            drop(probe);
            assert!(execution.accepts_work(), "command custody never waits for the whole parent job to stop");
            assert!(!owned_marker.exists());
            // Dropping the actual waiter requests Stop without dropping the SAME File
            // custody, including a child which daemonized beyond the shell's process group.
            let fence = std::fs::OpenOptions::new().read(true).write(true).open(&fence_path)?;
            fence.try_lock()?;
            let probe = std::fs::OpenOptions::new().read(true).write(true).open(&fence_path)?;
            let started = dir.path().join("captured-started");
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "setsid sh -c 'sleep 30' </dev/null >/dev/null 2>&1 & printf ready > \"$1\"; sleep 30", "sh"]).arg(&started);
            {
                use futures::FutureExt as _;
                let output = execution.scope(crate::proc::output_with_custody(command, Duration::from_secs(30), fence)).fuse();
                let ready = crate::rt::timeout(Duration::from_secs(5), async {
                    while !started.exists() { crate::rt::sleep(Duration::from_millis(20)).await; }
                }).fuse();
                futures::pin_mut!(output, ready);
                futures::select_biased! {
                    result = output => { let _ = result?; anyhow::bail!("the long captured command ended before waiter cancellation"); },
                    result = ready => { result.context("captured command never started")?; },
                }
            }
            let cancelled = execution.state.lock().groups.last().cloned().context("the canceled command lost its existing group")?;
            match probe.try_lock() {
                Ok(()) => assert!(cancelled.kernel.as_ref().context("the released fence lost native containment")?.empty_proven()?, "File release requires this command's positive physical proof"),
                Err(std::fs::TryLockError::WouldBlock) => {},
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
            crate::rt::timeout(Duration::from_secs(5), cancelled.finished.cancelled()).await.context("canceled command custody did not retire")?;
            assert!(cancelled.kernel.as_ref().context("the canceled command lost native containment")?.empty_proven()?);
            probe.try_lock()?;
            drop(probe);
            assert!(execution.accepts_work(), "canceling a command does not stop the original execution grant");
            // U03: a descendant may attempt its write only AFTER the original stopped
            // proof releases capacity. Wall-clock delay before Stop is not a containment test.
            let ready = dir.path().join("escaped-ready");
            let release = dir.path().join("escaped-release");
            let (path, started, released) = (marker.clone(), ready.clone(), release.clone());
            let escaped = execution.scope(async move {
                let mut command = std::process::Command::new("sh");
                command.args(["-c", "setsid sh -c 'printf ready > \"$2\"; while [ ! -f \"$3\" ]; do sleep 0.01; done; printf orphan > \"$1\"' sh \"$1\" \"$2\" \"$3\" </dev/null >/dev/null 2>&1 &", "sh"]).arg(path).arg(started.clone()).arg(released).env_clear();
                let mut child = crate::proc::spawn_group(command)?;
                let escaped = child.owned.clone().context("the setsid descendant lost its original physical owner")?;
                assert_eq!(child.wait().await, Some(0));
                crate::rt::timeout(Duration::from_secs(5), async {
                    while !started.try_exists()? { crate::rt::sleep(Duration::from_millis(20)).await; }
                    Ok::<_, anyhow::Error>(())
                }).await.context("the setsid descendant never reached its release barrier")??;
                Ok::<_, anyhow::Error>(escaped)
            }).await?;
            assert!(!escaped.kernel.as_ref().context("the setsid descendant lost native containment")?.empty_proven()?, "the ready descendant remains live before Stop");
            assert!(!marker.exists(), "the descendant must wait for its explicit release barrier");
            let cleanup_marker = dir.path().join("trusted-cleanup");
            let mut cleanup_command = std::process::Command::new("sh");
            cleanup_command.args(["-c", "printf cleaned > \"$1\"", "sh"]).arg(&cleanup_marker);
            let cleanup = execution.prepare_cleanup(cleanup_command)?;
            let late_marker = dir.path().join("timed-out-cleanup");
            let mut bounded_command = std::process::Command::new("sh");
            bounded_command.args(["-c", "sleep 30; printf late > \"$1\"", "sh"]).arg(&late_marker);
            let bounded = execution.prepare_cleanup(bounded_command)?;
            execution.stop_commands().await;
            assert!(escaped.finished.is_cancelled(), "ordinary Stop waits for this original descendant group");
            assert!(escaped.kernel.as_ref().context("the stopped descendant lost native containment")?.empty_proven()?, "ordinary Stop requires the original kernel group's positive empty proof");
            assert!(!recovery_evidence(&admission)?.iter().any(|p| p.execution == execution.identity()), "the preowned cleanup keeps capacity fenced");
            assert!(execution.scope(async { crate::proc::output(std::process::Command::new("true"), Duration::from_secs(1)).await }).await.is_err());
            let (stopping, finished) = (Arc::clone(&execution), crate::rt::Shutdown::new());
            let signal = finished.clone();
            let waiter = crate::rt::spawn(async move { stopping.stop().await; signal.cancel(); });
            crate::rt::sleep(Duration::from_millis(50)).await;
            assert!(!finished.is_cancelled(), "cancellation cannot acknowledge completion before its preowned utility finishes");
            let timeout_error = match bounded.run_with_timeout(Duration::from_millis(50)).await {
                Ok(_) => anyhow::bail!("the bounded cleanup completed before its deadline"),
                Err(error) => error,
            };
            assert!(timeout_error.to_string().contains("timed out after"), "{timeout_error:#}");
            assert!(!late_marker.exists(), "the bounded cleanup cannot write after its deadline");
            assert!(!finished.is_cancelled(), "the other preowned cleanup still holds physical capacity");
            assert!(cleanup.run().await?.status.success());
            assert_eq!(std::fs::read(&cleanup_marker)?, b"cleaned");
            crate::rt::timeout(Duration::from_secs(5), waiter).await??;
            execution.stop().await;
            assert!(recovery_evidence(&admission)?.iter().any(|p| p.execution == execution.identity() && p.job == "contained-fixture" && p.attempt == Some(1) && p.work == dir.path()), "the release barrier requires this exact stopped execution proof");
            std::fs::write(&release, b"released")?;
            crate::rt::sleep(Duration::from_millis(700)).await;
            assert!(!marker.exists(), "setsid descendants end before capacity is released");
            assert!(!sequence_marker.exists(), "a retired sequential phase cannot write after its successor starts");
            assert!(!dir.path().join("processes").exists(), "CI has one host registry, with no per-work-directory mirror");
            let stopped = execution.scope(crate::proc::output(std::process::Command::new("true"), Duration::from_secs(5))).await;
            assert!(stopped.is_err(), "a cancelled runtime cannot spawn after its proof");
            let next = Execution::new_in(admission.clone(), dir.path().into(), "contained-fixture".into(), Some(2));
            let (duplex, group) = next.spawn({ let mut command = std::process::Command::new("sh"); command.args(["-c", "sleep 1"]); command })?;
            execution.stop().await;
            assert!(!group.kernel.as_ref().context("the native command has no kernel owner")?.empty_proven()?, "an old timer cannot stop a newer execution");
            next.stop().await;
            let _ = duplex.exit.await;
            for execution in [&execution, &next] {
                let proof = recovery_evidence(&admission)?.into_iter().find(|p| p.execution == execution.identity()).context("stopped execution proof did not survive process cleanup")?;
                let mut stale = proof.clone();
                stale.attempt = Some(3);
                assert!(acknowledge_evidence(&admission, &stale).is_err(), "another attempt cannot retire physical evidence");
                acknowledge_evidence(&admission, &proof)?;
                acknowledge_evidence(&admission, &proof)?;
            }
            Ok(())
        })
    }

    #[test]
    fn u03_recovery_preserves_the_fence_until_kernel_members_are_gone() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            crate::os::contain::hold_agents();
            let admission = Admission::private(&tempfile::tempdir()?.keep())?;
            let dir = tempfile::tempdir()?;
            let id = format!("ci-{}", crate::util::new_id());
            let execution = format!("ci-{}", crate::util::new_id());
            let gate = executions::Gate::create(admission.clone(), &execution, "recovery-fixture", Some(1), dir.path(), None, None)?;
            let kernel = Containment::create(&admission, &id)?;
            let fence = admission.ci().join(format!("{id}.json"));
            let local = dir.path().join("processes").join(format!("{id}.json"));
            let record = Record { id: id.clone(), job: "recovery-fixture".into(), runner: std::process::id(), started: process_started(std::process::id()).context("fixture identity")?, identity: kernel.identity(), execution: Some(execution.clone()), attempt: Some(1), parent: None, work: dir.path().into(), local: Some(local.clone()), docker: None };
            std::fs::create_dir_all(admission.ci())?;
            let permit = std::fs::OpenOptions::new().create_new(true).read(true).write(true).open(fence.with_extension("lock"))?;
            permit.try_lock()?;
            // This exact held permit is handed to the same recovery operation used after a
            // crash, keeping concurrent fixtures correctly fenced throughout the proof.
            save(&fence, &record)?;
            save(&local, &record)?;
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "exec env -i sh -c 'sleep 30'"]);
            let duplex = kernel.spawn(command)?;
            assert!(!kernel.empty_proven()?);
            assert!(fence.exists());
            gate.close()?;
            assert!(!recovery_evidence(&admission)?.iter().any(|p| p.execution == execution), "closing future spawns is not process death");
            drop(gate);
            recover_record(admission.clone(), fence.clone(), record, permit).await?;
            executions::recover(&admission)?; // Production host reconciliation finalizes abandoned Gates next.
            assert!(kernel.empty_proven()?);
            assert!(!fence.exists());
            assert!(!local.exists());
            let _ = duplex.exit.await;
            let proof = recovery_evidence(&admission)?.into_iter().find(|p| p.execution == execution).context("the final cleanup lost its exact durable release evidence")?;
            assert_eq!(proof.attempt, Some(1));
            assert_eq!(proof.job, "recovery-fixture");
            assert!(Admission::private(Path::new(".")).is_err(), "relative paths cannot select fixture authority");
            // ctl-12: selecting an ancestor of production cannot become fixture authority.
            assert!(Admission::private(&crate::os::home_dir()).is_err());
            // The same identity is invisible to a second explicitly owned domain.
            let other = Admission::private(&tempfile::tempdir()?.keep())?;
            assert!(recovery_evidence(&other)?.is_empty());
            assert!(try_acknowledge_evidence(&other, &proof)?, "absent evidence remains idempotent");
            assert!(recovery_evidence(&admission)?.iter().any(|saved| saved == &proof), "another domain's ACK cannot touch the original proof");
            let reopened = Admission::private(admission.root())?;
            assert!(recovery_evidence(&reopened)?.iter().any(|saved| saved == &proof), "restart retains the exact explicit registry");
            acknowledge_evidence(&reopened, &proof)?;
            assert!(recovery_evidence(&admission)?.is_empty());
            Ok(())
        })
    }
}
