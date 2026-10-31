//! U01 receiver ownership. Expiry is a request to fence execution, never permission to reuse
//! capacity. The host ends its durable Scheduler assignment only after the kernel proves its
//! containment empty. Unsupported backends refuse before claim or spawn.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use futures::FutureExt as _;

use super::node::{Handle, In, Local};
use crate::runner::processes::Admission;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Permit {
    pub id: String,
    pub token: String,
    pub epoch: String,
    pub host: String,
}

#[derive(Clone)]
pub(crate) struct Client {
    pub tx: async_channel::Sender<In>,
    pub instance: String,
    pub admission: Admission,
}

fn set_request_field(request: &mut Value, key: &str, value: Value) -> anyhow::Result<()> {
    let fields = request.as_object_mut().ok_or_else(|| anyhow::anyhow!("U01 admission request is not an object"))?;
    fields.insert(key.to_owned(), value);
    Ok(())
}

impl Client {
    pub async fn query(&self, ask: Value) -> anyhow::Result<Value> {
        let (reply, rx) = futures::channel::oneshot::channel();
        self.tx.send(In::Local(Local::Query { json: ask.to_string(), reply })).await?;
        let raw = crate::rt::timeout(Duration::from_secs(10), rx).await.map_err(|_| anyhow::anyhow!("admission did not answer within 10 seconds"))???;
        let v: Value = serde_json::from_str(&raw)?;
        anyhow::ensure!(v.get("ok").and_then(Value::as_bool) == Some(true), "resource admission refused: {}", v.get("refusal").unwrap_or(&v));
        Ok(v)
    }

    pub fn request(&self, action: &str, p: &Permit) -> Value {
        json!({ "op": "admission", "action": action, "id": p.id, "token": p.token, "epoch": p.epoch, "instance": self.instance })
    }

    pub async fn acquire(&self, host: &str, needs: kiln_ci::sched::needs::Needs) -> anyhow::Result<Permit> {
        let v = self.query(json!({ "op": "admission", "action": "acquire", "host": host, "needs": needs })).await?;
        Ok(serde_json::from_value(v)?)
    }

    pub async fn claim(&self, permit: Permit, owner: &str) -> anyhow::Result<Lease> {
        let prepared = async {
            let host = if simulated(&self.instance) { None } else { Some(host_guard(&self.admission, true)?) };
            if host.is_some() {
                crate::runner::processes::reconcile_host(&self.admission).await?;
                // Another reconciler may own an orphan's lock while its child is stopping.
                // A locked record alone cannot let this caller bypass unresolved CI work.
                crate::runner::processes::ensure_no_orphans(&self.admission)?;
                reconcile_reach(&self.admission).await?;
                ensure_host_clear(&self.admission)?;
            }
            let lock = lock(&self.admission, &permit.id)?;
            let group = containment(&self.admission, &permit.id, &self.instance)?;
            if host.is_some() {
                write_marker(&self.admission, &permit.id, &group.identity())?;
            }
            Ok::<_, anyhow::Error>((host, lock, group))
        }.await;
        let (host, lock, group) = match prepared {
            Ok(p) => p,
            Err(e) => {
                // No claim or spawn was attempted. The assigned host may decline this
                // unclaimed offer atomically; a racing successful claim prevents decline.
                let _ = self.query(self.request("decline", &permit)).await;
                return Err(e);
            }
        };
        if let Err(e) = group.initialize(&lock, host.as_ref()) {
            // Initialization cannot open the workload gate. Keep the host fence until a
            // cleanup worker has reconciled any partial kernel allocation.
            let _ = self.query(self.request("decline", &permit)).await;
            retire(self.clone(), permit, group, lock, host, None, None);
            return Err(e);
        }
        if host.is_some() && let Err(e) = write_marker(&self.admission, &permit.id, &group.identity()) {
            retire(self.clone(), permit, group, lock, host, None, None);
            return Err(e);
        }
        let mut ask = self.request("claim", &permit);
        if let Err(e) = set_request_field(&mut ask, "owner", json!(owner))
            .and_then(|()| set_request_field(&mut ask, "containment", json!(group.identity())))
        {
            retire(self.clone(), permit, group, lock, host, None, None);
            return Err(e);
        }
        // The deterministic group exists before claim; a restarted host can fence it even
        // when the claim answer was lost before the original host spawned anything.
        if let Err(e) = self.query(ask).await {
            // A claim may have committed despite a lost answer. Never abandon it as an offer.
            // The cleanup worker can safely acknowledge stopped: no spawn occurred.
            retire(self.clone(), permit, group, lock, host, None, None);
            return Err(e);
        }
        let fenced = Arc::new(AtomicBool::new(false));
        let spawned = Arc::new(AtomicBool::new(false));
        let timeout = Arc::new(AtomicU64::new(u64::MAX));
        let (done, finished) = async_channel::bounded(1);
        retire(self.clone(), permit, group.clone(), lock, host, Some(Watch { done: finished, fenced: Arc::clone(&fenced), spawned: Arc::clone(&spawned), timeout: Arc::clone(&timeout) }), None);
        Ok(Lease { group, fenced, spawned, timeout, done })
    }

    /// A different receiver process cannot resume the old process's permit. Fence its durable
    /// containment before acknowledging end. Same-process reconnect leaves the live guard in
    /// charge, so it cannot race an old claim still preparing to spawn.
    pub async fn recover(&self) -> anyhow::Result<()> {
        // A local crash can precede the durable server claim. Reconcile its transport fence
        // even when no claimed permit exists on the server.
        if !simulated(&self.instance) { clear_orphan_marker(&self.admission).await?; }
        let v = self.query(json!({ "op": "admission", "action": "recover", "instance": self.instance })).await?;
        if let Some(grants) = v.get("grants").and_then(Value::as_array) {
            for g in grants {
                let p: Permit = serde_json::from_value(g.clone())?;
                let Some(recovery) = Recovery::begin(&self.admission, &p.id)? else { continue };
                let instance = g.get("instance").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("recovered permit lacks receiver identity"))?;
                let identity = g.get("containment").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("recovered permit lacks containment identity"))?.to_owned();
                let mut previous = self.clone();
                previous.instance = instance.to_owned();
                crate::rt::spawn(async move {
                    // The prior runner may still be alive on a partitioned connection. Its
                    // spawner owns this OS lock; never infer death from a transport event.
                    loop {
                        if let Ok(locked) = lock(&previous.admission, &p.id) {
                            if let Ok(group) = Containment::recover(&previous.admission, &identity, &p.id) {
                                retire(previous, p, group, locked, None, None, Some(recovery));
                            }
                            break;
                        }
                        crate::rt::sleep(Duration::from_secs(5)).await;
                    }
                });
            }
        }
        Ok(())
    }
}

impl Handle {
    pub(crate) fn admission(&self) -> Client {
        Client { tx: self.admission_sender(), instance: self.admission_instance().to_owned(), admission: self.admission_paths().clone() }
    }
}

pub struct Lease {
    group: Containment,
    fenced: Arc<AtomicBool>,
    spawned: Arc<AtomicBool>,
    timeout: Arc<AtomicU64>,
    done: async_channel::Sender<()>,
}

impl Lease {
    pub(crate) fn control(&self) -> Arc<AtomicBool> { Arc::clone(&self.fenced) }

    pub(crate) fn timeout(&self, ms: Option<u64>) {
        self.timeout.store(ms.unwrap_or(u64::MAX), Ordering::Release);
    }

    pub fn spawn(&self, cmd: std::process::Command) -> anyhow::Result<crate::os::proc::Duplex> {
        anyhow::ensure!(!self.fenced.load(Ordering::Acquire), "admission fenced this execution before spawn");
        self.group.spawn(cmd)
    }

    /// Set immediately after the one synchronous spawn attempt, successful or otherwise.
    pub fn spawn_complete(&self, pid: Option<u32>) {
        self.group.spawned(pid);
        self.spawned.store(true, Ordering::Release);
    }

    pub fn group(&self) -> Option<crate::os::contain::Group> {
        match &self.group {
            #[cfg(not(target_os = "macos"))]
            Containment::Kernel(g) => Some(g.clone()),
            #[cfg(target_os = "macos")]
            Containment::Mac(_) => None,
            #[cfg(test)]
            Containment::Sim(_) => None,
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.fenced.store(true, Ordering::Release);
        let _ = self.done.try_send(());
    }
}

/// U01: the receiver checks its still-live executions before obeying an assignment. This
/// cannot choose placement; it only fences stale executions that outlive a server connection.
/// Live CI and reach share the host OS lock. Per-execution locks and durable fences protect
/// crashed executions; they never choose placement or serialize unrelated admitted work.
pub(crate) fn ci_guard(admission: &Admission) -> anyhow::Result<std::fs::File> {
    let guard = host_guard(admission, true)?;
    ensure_host_clear(admission)?;
    crate::runner::processes::ensure_no_orphans(admission)?;
    Ok(guard)
}

fn marker_path(admission: &Admission) -> std::path::PathBuf {
    admission.root().join("active")
}

fn read_marker(admission: &Admission) -> anyhow::Result<Vec<u8>> {
    use std::io::Read as _;
    let file = match std::fs::File::open(marker_path(admission)) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut bytes = Vec::new();
    file.take(8193).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= 8192, "invalid containment fence size");
    Ok(bytes)
}

pub(crate) fn ensure_host_clear(admission: &Admission) -> anyhow::Result<()> {
    anyhow::ensure!(read_marker(admission)?.is_empty(), "a previous reach execution needs containment reconciliation");
    for marker in markers(admission)? {
        anyhow::ensure!(try_lock(admission, &marker.id)?.is_none(), "a previous reach execution needs containment reconciliation");
    }
    Ok(())
}

fn write_legacy_marker(admission: &Admission, text: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    let path = marker_path(admission);
    let next = path.with_extension("next");
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&next)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&next, &path)?;
    // Persist creation too. The file contains only a containment identity, never a token.
    #[cfg(unix)]
    if let Some(parent) = path.parent() { std::fs::File::open(parent)?.sync_all()?; }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Marker { id: String, identity: String }

fn markers_root(admission: &Admission) -> std::path::PathBuf { admission.root().join("reach") }

fn marker(admission: &Admission, id: &str) -> anyhow::Result<Option<Marker>> {
    use std::io::Read as _;
    valid_id(id)?;
    let file = match std::fs::File::open(markers_root(admission).join(format!("{id}.json"))) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut bytes = Vec::new();
    file.take(8193).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= 8192, "invalid containment fence size");
    let marker: Marker = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(marker.id == id && !marker.identity.is_empty() && marker.identity.len() <= 4096, "invalid containment fence identity");
    Ok(Some(marker))
}

fn markers(admission: &Admission) -> anyhow::Result<Vec<Marker>> {
    let entries = match std::fs::read_dir(markers_root(admission)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") { continue; }
        anyhow::ensure!(out.len() < 4096, "bounded reach containment registry exceeded");
        let id = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| anyhow::anyhow!("invalid containment fence filename"))?;
        if let Some(record) = marker(admission, id)? { out.push(record); }
    }
    Ok(out)
}

fn index_guard(admission: &Admission) -> anyhow::Result<std::fs::File> {
    let root = markers_root(admission);
    std::fs::create_dir_all(&root)?;
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(root.join("registry.lock"))?;
    // The critical section does only bounded synchronous filesystem writes, never awaits.
    file.lock()?;
    Ok(file)
}

fn write_marker(admission: &Admission, id: &str, identity: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    valid_id(id)?;
    anyhow::ensure!(!identity.is_empty() && identity.len() <= 4096, "invalid containment identity");
    let _index = index_guard(admission)?;
    let path = markers_root(admission).join(format!("{id}.json"));
    anyhow::ensure!(path.try_exists()? || markers(admission)?.len() < 4096, "bounded reach containment registry is full");
    let next = path.with_extension("next");
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&next)?;
    file.write_all(&serde_json::to_vec(&Marker { id: id.to_owned(), identity: identity.to_owned() })?)?;
    file.sync_all()?;
    std::fs::rename(next, &path)?;
    #[cfg(unix)]
    std::fs::File::open(markers_root(admission))?.sync_all()?;
    Ok(())
}

fn remove_marker(admission: &Admission, id: &str) -> anyhow::Result<()> {
    valid_id(id)?;
    let _index = index_guard(admission)?;
    match std::fs::remove_file(markers_root(admission).join(format!("{id}.json"))) {
        Ok(()) => {},
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    }
    #[cfg(unix)]
    std::fs::File::open(markers_root(admission))?.sync_all()?;
    Ok(())
}

async fn reconcile_reach(admission: &Admission) -> anyhow::Result<()> {
    // Legacy single-host fence from the first U01 implementation remains readable. Only its
    // own spawner lock and exact kernel proof can clear it; no time-based migration shortcut.
    let bytes = read_marker(admission)?;
    if !bytes.is_empty() {
        let old: Marker = serde_json::from_slice(&bytes)?;
        if let Some(_permit) = try_lock(admission, &old.id)? {
            let group = Containment::recover(admission, &old.identity, &old.id)?;
            while !empty(group.clone()).await { crate::rt::sleep(Duration::from_millis(200)).await; }
            group.remove();
            write_legacy_marker(admission, "")?;
        }
    }
    for old in markers(admission)? {
        let Some(_permit) = try_lock(admission, &old.id)? else { continue };
        // The old owner may have retired after the directory read and before this lock.
        let Some(current) = marker(admission, &old.id)? else { continue };
        let group = Containment::recover(admission, &current.identity, &current.id)?;
        while !empty(group.clone()).await { crate::rt::sleep(Duration::from_millis(200)).await; }
        group.remove();
        remove_marker(admission, &current.id)?;
    }
    Ok(())
}

async fn clear_orphan_marker(admission: &Admission) -> anyhow::Result<()> {
    let _host = host_guard(admission, true)?;
    crate::runner::processes::reconcile_host(admission).await?;
    reconcile_reach(admission).await
}

pub(crate) fn host_guard(admission: &Admission, shared: bool) -> anyhow::Result<std::fs::File> {
    let root = admission.root().to_owned();
    std::fs::create_dir_all(&root)?;
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(root.join("host.lock"))?;
    if shared {
        f.try_lock_shared().map_err(|e| anyhow::anyhow!("a reach execution still owns this host: {e}"))?;
    } else {
        f.try_lock().map_err(|e| anyhow::anyhow!("CI or another reach execution still owns this host: {e}"))?;
    }
    Ok(f)
}

fn valid_id(id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(id.starts_with("reach-") && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'), "invalid admission permit id");
    Ok(())
}

fn try_lock(admission: &Admission, id: &str) -> anyhow::Result<Option<std::fs::File>> {
    valid_id(id)?;
    let root = admission.root().to_owned();
    std::fs::create_dir_all(&root)?;
    // Keep striped lock inodes permanently. A live owner, including an orphaned macOS
    // bootstrap, retains its file description; a missing transport never overrides it.
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(root.join(format!("{}.lock", id.bytes().fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(b))) % 4096)))?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

fn lock(admission: &Admission, id: &str) -> anyhow::Result<std::fs::File> {
    try_lock(admission, id)?.ok_or_else(|| anyhow::anyhow!("another receiver still owns this execution"))
}

fn containment(admission: &Admission, id: &str, instance: &str) -> anyhow::Result<Containment> {
    #[cfg(test)]
    if simulated(instance) {
        return Ok(Containment::Sim(Arc::new(std::sync::atomic::AtomicU32::new(0))));
    }
    let _ = instance;
    Containment::create(admission, id)
}

async fn empty(group: Containment) -> bool {
    crate::rt::blocking(move || { group.kill(); group.empty() }).await.unwrap_or(false)
}

/// The kernel backend is mandatory in production. The explicit simulator below lets the
/// encrypted-link fixtures model separate hosts without touching the live runner's cgroups.
#[derive(Clone, Debug)]
pub(crate) enum Containment {
    #[cfg(not(target_os = "macos"))]
    Kernel(crate::os::contain::Group),
    #[cfg(target_os = "macos")]
    Mac(crate::os::leased::Group),
    #[cfg(test)]
    Sim(Arc<std::sync::atomic::AtomicU32>),
}

impl Containment {
    pub(crate) fn cpu_memory_limits(cpu: f64, memory: u64) -> anyhow::Result<crate::os::cgroup::CpuMemoryLimit> {
        #[cfg(target_os = "linux")]
        {
            let page = nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)?.and_then(|p| u64::try_from(p).ok()).ok_or_else(|| anyhow::anyhow!("kernel page size is unavailable"))?;
            crate::os::cgroup::CpuMemoryLimit::from_budget(cpu, memory, page)
        }
        #[cfg(not(target_os = "linux"))]
        { let _ = (cpu, memory); anyhow::bail!("aggregate Firecracker CPU/memory enforcement requires delegated Linux cgroup v2") }
    }

    pub(crate) fn aggregate(id: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(id.starts_with("ci-") && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "invalid aggregate execution id");
        #[cfg(target_os = "linux")]
        { crate::os::contain::hold_agents(); Ok(Self::Kernel(crate::os::contain::Group::aggregate(id)?)) }
        #[cfg(not(target_os = "linux"))]
        { anyhow::bail!("aggregate Firecracker CPU/memory enforcement requires delegated Linux cgroup v2") }
    }

    pub(crate) fn set_limits(&self, limits: crate::os::cgroup::CpuMemoryLimit) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        match self {
            Self::Kernel(group) => group.set_limits(limits),
            #[cfg(test)]
            Self::Sim(_) => anyhow::bail!("simulated containment cannot attest native CPU/memory limits"),
        }
        #[cfg(not(target_os = "linux"))]
        { let _ = limits; anyhow::bail!("aggregate CPU/memory enforcement has no native Linux domain") }
    }

    pub(crate) fn child(&self, id: &str, limits: crate::os::cgroup::CpuMemoryLimit) -> anyhow::Result<Self> {
        #[cfg(target_os = "linux")]
        match self {
            Self::Kernel(group) => Ok(Self::Kernel(group.child(id, limits)?)),
            #[cfg(test)]
            Self::Sim(_) => anyhow::bail!("simulated containment cannot create native limited children"),
        }
        #[cfg(not(target_os = "linux"))]
        { let _ = (id, limits); anyhow::bail!("aggregate child containment has no native Linux domain") }
    }

    pub(crate) fn empty_aggregate_proven(&self) -> anyhow::Result<bool> {
        #[cfg(target_os = "linux")]
        match self {
            Self::Kernel(group) => Ok(group.empty_proven_strict()?),
            #[cfg(test)]
            Self::Sim(_) => anyhow::bail!("simulated containment cannot attest aggregate kernel retirement"),
        }
        #[cfg(not(target_os = "linux"))]
        { anyhow::bail!("aggregate stop proof has no native Linux domain") }
    }

    /// U01/U03: one strict kernel containment API for CI and reach. Callers durably record
    /// identity before initialization and after it, before any spawn; retain their OS locks
    /// until no future spawn is possible and empty_proven succeeds. No TTL implies emptiness.
    pub(crate) fn create(admission: &Admission, id: &str) -> anyhow::Result<Self> {
        let _ = admission;
        anyhow::ensure!((id.starts_with("reach-") || id.starts_with("ci-")) && id.len() <= 128
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "invalid execution id");
        #[cfg(target_os = "linux")]
        {
            let group = crate::os::contain::Group::of(id).ok_or_else(|| anyhow::anyhow!("strict execution requires writable cgroup v2 containment"))?;
            let _ = group.empty_proven()?;
            Ok(Self::Kernel(group))
        }
        #[cfg(windows)]
        { Ok(Self::Kernel(crate::os::contain::Group::of_leased(id)?)) }
        #[cfg(target_os = "macos")]
        { Ok(Self::Mac(crate::os::leased::Group::new(admission, id)?)) }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        { anyhow::bail!("strict execution requires a supported kernel containment backend") }
    }

    pub(crate) fn initialize(&self, lock: &std::fs::File, host: Option<&std::fs::File>) -> anyhow::Result<()> {
        #[cfg(target_os = "macos")]
        match self {
            Self::Mac(g) => g.initialize(lock, host)?,
            #[cfg(test)]
            Self::Sim(_) => {},
        }
        let _ = (lock, host);
        Ok(())
    }

    pub(crate) fn recover(admission: &Admission, identity: &str, id: &str) -> anyhow::Result<Self> {
        let _ = admission;
        #[cfg(any(target_os = "linux", windows))]
        { Ok(Self::Kernel(crate::os::contain::Group::recover_leased(identity, id)?)) }
        #[cfg(target_os = "macos")]
        { Ok(Self::Mac(crate::os::leased::Group::recover(admission, identity, id)?)) }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        { let _ = (identity, id); anyhow::bail!("leased execution requires kernel containment") }
    }

    pub(crate) fn spawn(&self, cmd: std::process::Command) -> anyhow::Result<crate::os::proc::Duplex> {
        match self {
            #[cfg(not(target_os = "macos"))]
            Self::Kernel(g) => {
                #[cfg(target_os = "linux")]
                { let mut cmd = cmd; g.enter_leased(&mut cmd)?; crate::os::proc::spawn_duplex_raw(cmd) }
                #[cfg(windows)]
                { crate::os::proc::spawn_duplex_leased(cmd, g) }
                #[cfg(not(any(target_os = "linux", windows)))]
                { let _ = (g, cmd); anyhow::bail!("leased execution requires kernel containment") }
            }
            #[cfg(target_os = "macos")]
            Self::Mac(g) => g.spawn(cmd),
            #[cfg(test)]
            Self::Sim(_) => crate::os::proc::spawn_duplex_raw(cmd),
        }
    }

    pub(crate) fn identity(&self) -> String {
        match self {
            #[cfg(not(target_os = "macos"))]
            Self::Kernel(g) => {
                #[cfg(any(target_os = "linux", windows))]
                { g.lease_identity() }
                #[cfg(not(any(target_os = "linux", windows)))]
                { let _ = g; String::new() }
            }
            #[cfg(target_os = "macos")]
            Self::Mac(g) => g.identity(),
            #[cfg(test)]
            Self::Sim(_) => "simulated-kernel-containment".to_owned(),
        }
    }

    fn spawned(&self, pid: Option<u32>) {
        #[cfg(test)]
        if let Self::Sim(cell) = self { cell.store(pid.unwrap_or(0), Ordering::Release); }
        let _ = pid;
    }

    pub(crate) fn kill(&self) {
        match self {
            #[cfg(not(target_os = "macos"))]
            Self::Kernel(g) => g.kill(),
            #[cfg(target_os = "macos")]
            Self::Mac(g) => g.kill(),
            #[cfg(test)]
            Self::Sim(pid) => {
                let p = pid.load(Ordering::Acquire);
                if p > 0 { super::serve::signal_group(p, super::wire::Signal::Kill); }
            }
        }
    }

    pub(crate) fn empty_proven(&self) -> std::io::Result<bool> {
        match self {
            #[cfg(not(target_os = "macos"))]
            Self::Kernel(g) => {
                #[cfg(any(target_os = "linux", windows))]
                { g.empty_proven() }
                #[cfg(not(any(target_os = "linux", windows)))]
                { let _ = g; Err(std::io::Error::other("strict kernel containment unavailable")) }
            }
            #[cfg(target_os = "macos")]
            Self::Mac(g) => g.empty_proven(),
            #[cfg(test)]
            Self::Sim(pid) => {
                let p = pid.load(Ordering::Acquire);
                #[cfg(unix)]
                { Ok(p == 0 || i32::try_from(p).is_ok_and(|p| matches!(nix::sys::signal::killpg(nix::unistd::Pid::from_raw(p), None), Err(nix::errno::Errno::ESRCH)))) }
                #[cfg(not(unix))]
                { let _ = p; Ok(true) }
            }
        }
    }

    fn empty(&self) -> bool { self.empty_proven().unwrap_or(false) }

    pub(crate) fn remove(&self) {
        match self {
            #[cfg(not(target_os = "macos"))]
            Self::Kernel(g) => g.remove(),
            #[cfg(target_os = "macos")]
            Self::Mac(g) => g.remove(),
            #[cfg(test)]
            Self::Sim(_) => {}
        }
    }
}

#[cfg(test)]
fn simulators() -> &'static parking_lot::Mutex<std::collections::HashSet<String>> {
    static SIM: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    SIM.get_or_init(Default::default)
}

#[cfg(all(test, unix))]
pub(super) fn simulate_receiver(handle: &Handle) {
    simulators().lock().insert(handle.admission_instance().to_owned());
}

fn simulated(instance: &str) -> bool {
    #[cfg(test)]
    { simulators().lock().contains(instance) }
    #[cfg(not(test))]
    { let _ = instance; false }
}


type Recoveries = parking_lot::Mutex<std::collections::HashSet<(std::path::PathBuf, String)>>;

fn recoveries() -> &'static Recoveries {
    static RECOVERING: std::sync::OnceLock<Recoveries> = std::sync::OnceLock::new();
    RECOVERING.get_or_init(Default::default)
}

struct Recovery((std::path::PathBuf, String));

impl Recovery {
    fn begin(admission: &Admission, id: &str) -> anyhow::Result<Option<Self>> {
        let key = (admission.root().to_owned(), id.to_owned());
        let mut pending = recoveries().lock();
        if pending.contains(&key) { return Ok(None); }
        anyhow::ensure!(pending.len() < 4096, "bounded execution recovery queue is full");
        pending.insert(key.clone());
        Ok(Some(Self(key)))
    }
}

impl Drop for Recovery {
    fn drop(&mut self) { recoveries().lock().remove(&self.0); }
}

/// The guard owns cleanup independently of output backpressure or a client disconnect.
struct Watch {
    done: async_channel::Receiver<()>,
    fenced: Arc<AtomicBool>,
    spawned: Arc<AtomicBool>,
    timeout: Arc<AtomicU64>,
}

fn retire(client: Client, permit: Permit, group: Containment, lock: std::fs::File, host: Option<std::fs::File>, running: Option<Watch>, recovery: Option<Recovery>) {
    crate::rt::spawn(async move {
        let _recovery = recovery;
        let _lock = lock;
        let mut group = group;
        let mut host_hold = host;
        if let Some(Watch { done, fenced, spawned, timeout }) = running {
            let started = std::time::Instant::now();
            loop {
                let stop = futures::select_biased! {
                    _ = done.recv().fuse() => true,
                    _ = crate::rt::sleep(Duration::from_secs(5)).fuse() => false,
                };
                if stop { break; }
                if fenced.load(Ordering::Acquire) || started.elapsed().as_millis() >= u128::from(timeout.load(Ordering::Acquire)) || client.query(client.request("renew", &permit)).await.is_err() {
                    fenced.store(true, Ordering::Release);
                    // Do not release on emptiness yet: the owner could still be preparing a
                    // spawn. Only dropping Lease proves there can be no later spawn.
                    let g = group.clone();
                    let _ = crate::rt::blocking(move || g.kill()).await;
                    if spawned.load(Ordering::Acquire) && empty(group.clone()).await {
                        break;
                    }
                }
            }
        }
        // Kernel proof is mandatory. Kill, permission and inspection failures keep ownership.
        while !empty(group.clone()).await {
            crate::rt::sleep(Duration::from_millis(200)).await;
        }
        if !simulated(&client.instance) {
            // This exact execution cannot spawn again and the kernel proved it empty. Other
            // admitted CI/reach work may stay live; only this execution's crash fence ends.
            loop {
                match remove_marker(&client.admission, &permit.id) {
                    Ok(()) => break,
                    Err(e) => tracing::error!(error = %e, "U01 stopped containment awaits local fence cleanup"),
                }
                crate::rt::sleep(Duration::from_secs(1)).await;
            }
        }
        drop(host_hold.take());
        let mut end = client.request("end", &permit);
        loop {
            if let Err(e) = set_request_field(&mut end, "stopped", json!(true)) {
                tracing::error!(error = %e, "U01 stopped execution retains ownership until its release request can be encoded");
                end = client.request("end", &permit);
                crate::rt::sleep(Duration::from_secs(5)).await;
                continue;
            }
            match client.query(end.clone()).await {
                Ok(_) => {
                    group.remove();
                    return;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "U01 stopped execution awaits durable release");
                    // A delayed RunLeased may have reached a new receiver after the original
                    // one crashed. Holding its OS lock and proving its exact containment empty
                    // permits acknowledgement of that previous receiver's durable instance.
                    // This also covers a claim whose successful answer was lost.
                    if let Ok(v) = client.query(client.request("inspect", &permit)).await
                        && v.get("claimed") == Some(&json!(true))
                        && let Some(instance) = v.get("instance").and_then(Value::as_str)
                    {
                        let _ = instance;
                        if let Some(identity) = v.get("containment").and_then(Value::as_str)
                            && let Ok(previous) = Containment::recover(&client.admission, identity, &permit.id)
                        {
                            // A runner may now live in a different cgroup. Prove the recorded
                            // old identity empty, never a newly created group's emptiness.
                            while !empty(previous.clone()).await {
                                crate::rt::sleep(Duration::from_millis(200)).await;
                            }
                            group.remove();
                            group = previous;
                            if let Err(e) = set_request_field(&mut end, "instance", json!(instance)) {
                                tracing::error!(error = %e, "U01 stopped execution retains ownership until its receiver identity can be encoded");
                                end = client.request("end", &permit);
                            }
                        }
                    }
                },
            }
            crate::rt::sleep(Duration::from_secs(5)).await;
        }
    });
}
