//! ROOT-only Linux stand-in controls for the actual Supervisor publication/exec gate.
//! No VM/GPU claim. ROOT registers this leaf only after the matching native-scope cohort.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio as ChildStdio};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use hex_api::{BoxId, BoxSpec, Class, Exit, Program, Refusal, Stdio};
use hex_core::record::{BoxRecord, Phase, ProcId};
use hex_runtime::devices::managed_scope_id;
use hex_runtime::linux::cgroup::{BoxCgroup, BoxRoot, CGROUP_ROOT};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::process::{Pid, PidfdFlags, Signal, WaitId, WaitIdOptions};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use shards_api::{ManagedProcess, ManagedStatus, TerminalScope};

use super::{ManagedVm, Stat, Supervisor};
use crate::store::Store;

type Check<T> = Result<T, String>;
const READY: &str = "hex-completion-gate-ready ";
const IDENTITIES: &str = "hex-completion-gate-identities ";
const PARENT_CASE: &str =
    "supervise::completion_native_gate_tests::completion_controller_parent_death_cancels_gate";

fn text<T, E: std::fmt::Display>(value: Result<T, E>) -> Check<T> {
    value.map_err(|error| error.to_string())
}

fn need(value: bool, reason: &str) -> Check<()> {
    if value {
        Ok(())
    } else {
        Err(reason.to_owned())
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    delegated_parent: PathBuf,
    lab: PathBuf,
    spec: BoxSpec,
    controller: Program,
    controller_sha256: String,
    test_binary_sha256: String,
    pidfd_budget: usize,
    guard_ms: u64,
    gate_budget_ms: u64,
    observation_ms: u64,
    /// Historical pinned-config provenance only; the ACK oracle does not sample syscalls.
    gate_wait_syscall: u64,
    byte_limit: u64,
}

impl Config {
    fn load() -> Check<Self> {
        need(
            std::env::var("HEX_COMPLETION_ROOT_AUTHORIZED").as_deref() == Ok("1"),
            "ROOT authorization required",
        )?;
        let limit = text(text(std::env::var("HEX_COMPLETION_GATE_BYTES"))?.parse::<u64>())?;
        need(limit > 0, "positive configuration byte budget required")?;
        let path = PathBuf::from(text(
            std::env::var_os("HEX_COMPLETION_GATE_CONFIG").ok_or("missing gate config"),
        )?);
        let config: Self = text(serde_json::from_slice(&read_bounded(&path, limit)?))?;
        need(
            config.byte_limit == limit
                && config.guard_ms > 0
                && config.gate_budget_ms > config.guard_ms
                && config.observation_ms > 0
                && config.observation_ms <= config.guard_ms
                && config.pidfd_budget >= 2,
            "explicit finite ROOT bounds required; gate budget must exceed guard",
        )?;
        deadline(config.guard())?;
        deadline(config.gate())?;
        text(config.spec.check())?;
        for suffix in ["first", "sibling", "published", "refused"] {
            text(BoxId::new(&format!("{}-{suffix}", config.spec.id.as_str())))?;
        }
        text(config.controller.check())?;
        need(
            (matches!(
                config.spec.class.class(),
                Class::Shards | Class::Firecracker
            ) || (config.spec.class.class() == Class::Oci && config.spec.allocation.is_some()))
                && config.spec.devices.is_empty()
                && config.spec.bounds.cpu_max_milli.is_some()
                && config.spec.bounds.memory_max.is_some()
                && config.spec.bounds.pids_max.is_some(),
            "bounded managed record required for Store's native scope; controller remains a stand-in",
        )?;
        let parent = text(config.delegated_parent.canonicalize())?;
        need(
            parent.starts_with(CGROUP_ROOT) && parent != Path::new(CGROUP_ROOT),
            "private delegated cgroup parent required",
        )?;
        need(
            text(config.lab.canonicalize())?.is_dir(),
            "existing ROOT private lab required",
        )?;
        let executable = config
            .controller
            .args
            .first()
            .ok_or("controller argv missing")?;
        need(
            Path::new(executable).is_absolute(),
            "absolute configured stand-in required",
        )?;
        need(
            digest(Path::new(executable))? == config.controller_sha256,
            "stand-in source/binary pin mismatch",
        )?;
        need(
            digest(&text(std::env::current_exe())?)? == config.test_binary_sha256,
            "native fixture binary pin mismatch",
        )?;
        Ok(config)
    }

    fn guard(&self) -> Duration {
        Duration::from_millis(self.guard_ms)
    }
    fn gate(&self) -> Duration {
        Duration::from_millis(self.gate_budget_ms)
    }
}

#[derive(Serialize, Deserialize)]
struct GateIdentity {
    pid: u32,
    started: u64,
    vsock: PathBuf,
}

impl GateIdentity {
    fn native(self) -> shards_api::Identity {
        shards_api::Identity {
            pid: self.pid,
            started: self.started,
            vsock: self.vsock,
        }
    }
}

fn digest(path: &Path) -> Check<String> {
    let mut file = text(File::open(path))?;
    let mut digest = Sha256::new();
    let mut bytes = [0; 8192];
    loop {
        let count = text(file.read(&mut bytes))?;
        if count == 0 {
            break;
        }
        digest.update(bytes.get(..count).ok_or("invalid file read count")?);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn read_bounded(path: &Path, limit: u64) -> Check<Vec<u8>> {
    let mut bytes = Vec::new();
    text(
        text(File::open(path))?
            .take(limit.checked_add(1).ok_or("byte budget overflow")?)
            .read_to_end(&mut bytes),
    )?;
    need(
        u64::try_from(bytes.len()).map_err(|error| error.to_string())? <= limit,
        "artifact byte budget exceeded",
    )?;
    Ok(bytes)
}

fn deadline(budget: Duration) -> Check<Instant> {
    Instant::now()
        .checked_add(budget)
        .ok_or_else(|| "deadline overflow".to_owned())
}

fn poll_exit(fd: &OwnedFd, end: Instant) -> Check<()> {
    loop {
        let left = end.saturating_duration_since(Instant::now());
        need(!left.is_zero(), "exact pidfd exit guard expired")?;
        let timeout = Timespec {
            tv_sec: text(i64::try_from(left.as_secs()))?,
            tv_nsec: i64::from(left.subsec_nanos()),
        };
        let mut fds = [PollFd::new(fd, PollFlags::IN)];
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(_)
                if fds
                    .first()
                    .is_some_and(|fd| fd.revents().contains(PollFlags::IN)) =>
            {
                return Ok(());
            }
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn new_scope(config: &Config) -> Check<PathBuf> {
    Ok(text(
        tempfile::Builder::new()
            .prefix("gate-")
            .tempdir_in(&config.lab),
    )?
    .keep())
}

fn cgroup_path(config: &Config, scope: &Path) -> Check<PathBuf> {
    Ok(config
        .delegated_parent
        .join(scope.file_name().ok_or("private scope name missing")?))
}

struct Lab {
    config: Config,
    scope: PathBuf,
    groups: BoxRoot,
    store: Store,
    supervisor: Arc<Supervisor>,
    intents: Vec<BoxRecord>,
    natives: Vec<(BoxRecord, triomphe::Arc<ManagedVm>)>,
}

impl Lab {
    fn open(config: Config, scope: PathBuf) -> Check<Self> {
        let native_root = text(BoxRoot::open(&cgroup_path(&config, &scope)?, Vec::new()))?;
        let parent = text(BoxRoot::open(&native_root.dir().join(".vm"), Vec::new()))?;
        let groups = text(BoxRoot::open(&parent.dir().join("gate-owner"), Vec::new()))?;
        let store = text(Store::open(&scope.join("records"), &scope.join("run")))?;
        let supervisor = text(Supervisor::start(
            native_root.dir(),
            store.clone(),
            config.pidfd_budget,
        ))?;
        let lease = text(
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(scope.join("native-vm-owner.lock")),
        )?;
        text(lease.try_lock())?;
        text(supervisor.bind_managed_root(groups.dir(), lease))?;
        Ok(Self {
            config,
            scope,
            groups,
            store,
            supervisor,
            intents: Vec::new(),
            natives: Vec::new(),
        })
    }

    fn reserve(
        &mut self,
        suffix: &str,
    ) -> Check<(
        BoxRecord,
        triomphe::Arc<ManagedVm>,
        PathBuf,
        String,
        Program,
    )> {
        let mut spec = self.config.spec.clone();
        spec.id = text(BoxId::new(&format!("{}-{suffix}", spec.id.as_str())))?;
        // The production Store alone generates the incarnation on its FIRST
        // Creating publication; this fixture neither injects nor regenerates it.
        let intent = BoxRecord::new(spec.clone(), Phase::Creating, vec![self.scope.clone()]);
        text(self.store.begin(&intent))?;
        let record =
            text(self.store.read_lifecycle(&spec.id))?.ok_or("fresh Creating record missing")?;
        need(
            record.phase == Phase::Creating && record.native_scope.is_some(),
            "Store did not introduce native scope before effects",
        )?;
        self.intents.push(record.clone());
        text(fs::write(
            self.scope.join(format!("{suffix}.creating.record.json")),
            text(record.encode())?,
        ))?;
        let group = text(
            self.groups
                .create(&text(managed_scope_id(&record))?, &spec.bounds),
        )?;
        let native = match self.supervisor.reserve_managed(
            hex_core::record::NativeRecord::Box(&record),
            group.clone(),
            &self.scope.join(format!("{suffix}.sock")),
        ) {
            Ok(native) => native,
            Err(error) => {
                let cleanup = group.remove();
                return Err(format!(
                    "reserve: {error}; unsubmitted group cleanup: {cleanup:?}"
                ));
            }
        };
        self.natives
            .push((record.clone(), triomphe::Arc::clone(&native)));
        need(
            Some(native.scope) == record.native_scope
                && native.commitment == text(record.native_commitment())?,
            "native binding does not retain the original scope/spec commitment",
        )?;
        let marker = self.scope.join(format!("{suffix}.marker"));
        need(
            !text(marker.try_exists())?,
            "marker existed before owned launch",
        )?;
        let token = format!(
            "{}-{suffix}",
            self.scope
                .file_name()
                .ok_or("scope name missing")?
                .to_string_lossy()
        );
        let mut program = self.config.controller.clone();
        for arg in &mut program.args {
            *arg = arg
                .replace("@MARKER@", &marker.to_string_lossy())
                .replace("@TOKEN@", &token)
                .replace("@SCOPE@", &self.scope.to_string_lossy());
        }
        need(
            program
                .args
                .iter()
                .any(|arg| arg.contains(&*marker.to_string_lossy()))
                && program.args.iter().any(|arg| arg.contains(&token)),
            "stand-in must receive exact marker path and token",
        )?;
        Ok((record, native, marker, token, program))
    }

    fn publish(&self, original: &BoxRecord, identity: &shards_api::Identity) -> Check<()> {
        let before = self.store.syncs();
        let mut record = original.clone();
        record.init = Some(ProcId {
            pid: identity.pid,
            start_ticks: identity.started,
        });
        text(self.store.put_lifecycle(&record))?;
        text(self.store.made(&record))?;
        record.phase = Phase::Ready;
        let after = self.store.syncs();
        need(
            after.files > before.files && after.dirs > before.dirs,
            "publication was not durably fsynced; tmpfs is not a durable control",
        )?;
        let read = text(self.store.get(&record.spec.id))?.ok_or("publication record missing")?;
        need(
            text(read)? == record,
            "durable publication readback differs",
        )
    }

    fn cleanup(&self) -> Vec<String> {
        let mut errors = Vec::new();
        for original in &self.intents {
            let result = (|| -> Check<()> {
                let mut record = text(self.store.read_lifecycle(&original.spec.id))?
                    .ok_or("owned scope record missing during cleanup")?;
                need(
                    record.native_scope == original.native_scope
                        && text(record.native_commitment())? == text(original.native_commitment())?,
                    "cleanup original scope/spec changed",
                )?;
                let native = self
                    .natives
                    .iter()
                    .find(|(known, _)| known.spec.id == original.spec.id)
                    .map(|(_, native)| native);
                let status = if let Some(native) = native {
                    text(native.stop())?;
                    let end = text(native.wait_for(self.config.guard()))?
                        .ok_or("native cleanup remains unverified")?;
                    need(
                        end.scope == TerminalScope::Released,
                        "controller exit is not complete native release",
                    )?;
                    match end.status {
                        ManagedStatus::Code(code) => Exit::Code(code),
                        ManagedStatus::Signal(signal) => Exit::Signal(signal),
                        ManagedStatus::Unknown => Exit::Unknown,
                    }
                } else {
                    let group = BoxCgroup::at(
                        self.groups
                            .dir()
                            .join(text(managed_scope_id(original))?.as_str()),
                    );
                    if text(group.populated_if_present())?.is_some() {
                        text(group.kill())?;
                        need(
                            text(group.wait_empty(self.config.guard()))?,
                            "unsubmitted owned scope remains populated",
                        )?;
                        text(group.remove())?;
                    }
                    Exit::Unknown
                };
                record.phase = Phase::Deleting;
                record.exit = Some(status);
                text(self.store.put_lifecycle(&record))?;
                if let Some(native) = native {
                    text(self.supervisor.finalize_managed(
                        hex_core::record::NativeRecord::Box(&record),
                        native.identity().ok(),
                    ))?;
                }
                text(self.store.remove(&record.spec.id))?;
                need(
                    text(self.store.get(&record.spec.id))?.is_none(),
                    "owned record remains after release",
                )
            })();
            if let Err(error) = result {
                errors.push(format!("{}: {error}", original.spec.id.as_str()));
            }
        }
        if errors.is_empty() {
            let mut paths = vec![self.groups.dir().to_path_buf()];
            match self.groups.dir().parent() {
                Some(parent) => paths.push(parent.to_path_buf()),
                None => errors.push("managed parent root is unobserved".to_owned()),
            }
            match cgroup_path(&self.config, &self.scope) {
                Ok(root) => paths.push(root),
                Err(error) => errors.push(error),
            }
            for path in paths {
                if let Err(error) = fs::remove_dir(&path) {
                    errors.push(format!(
                        "private cgroup root cleanup {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        errors
    }
}

fn record_result(scope: &Path, case: &str, observed: &Check<()>, cleanup: &[String]) -> Check<()> {
    text(fs::write(
        scope.join("gate-result.json"),
        text(serde_json::to_vec_pretty(&json!({
            "schema":"hex-completion-native-gate-1", "case":case, "scope":"root-native-standin",
            "vm_gpu_claim":false, "observation":observed, "cleanup_uncertainty":cleanup
        })))?,
    ))
}

/// AC-T7.1.2: publication refusal must cancel the actual native gate before any foreign exec.
#[test]
#[ignore = "ROOT only: requires configured private delegated cgroup, durable lab and finite bounds"]
fn completion_publication_refusal_never_executes_marker() {
    let result = (|| -> Check<()> {
        let config = Config::load()?;
        let scope = new_scope(&config)?;
        let mut lab = Lab::open(config, scope.clone())?;
        let observed = (|| -> Check<()> {
            let (_, native, marker, _, program) = lab.reserve("refused")?;
            let mut published = false;
            let result = lab.supervisor.bind_managed(
                &native,
                &program,
                Stdio::null(),
                hex_runtime::linux::process::HeldLaunch::default(),
                lab.config.gate(),
                &mut |_| {
                    published = true;
                    Err(Refusal::Broken(
                        "ROOT publication-refusal control".to_owned(),
                    ))
                },
            );
            need(
                published
                    && matches!(result, Err(Refusal::Broken(ref why)) if why == "ROOT publication-refusal control"),
                "publication refusal was not the actual cancellation source",
            )?;
            let end = text(native.wait_for(lab.config.guard()))?
                .ok_or("refused native controller did not terminate")?;
            need(
                end.scope == TerminalScope::Released && end.status == ManagedStatus::Code(127),
                "gate cancellation exit/scope mismatch",
            )?;
            need(
                !text(marker.try_exists())?,
                "refused publication executed the marker",
            )
        })();
        let cleanup = lab.cleanup();
        record_result(&scope, "publication-refused", &observed, &cleanup)?;
        observed?;
        need(
            cleanup.is_empty(),
            &format!("cleanup uncertainty: {cleanup:?}"),
        )
    })();
    assert!(result.is_ok(), "{result:?}");
}

/// AC-T7.1.2: the same callback durably publishes before release, then executes and reaps.
#[test]
#[ignore = "ROOT only: requires configured private delegated cgroup, durable lab and finite bounds"]
fn completion_durable_publication_releases_and_reaps() {
    let result = (|| -> Check<()> {
        let config = Config::load()?;
        let scope = new_scope(&config)?;
        let mut lab = Lab::open(config, scope.clone())?;
        let observed = (|| -> Check<()> {
            let (spec, native, marker, token, program) = lab.reserve("published")?;
            let mut published = false;
            text(lab.supervisor.bind_managed(
                &native,
                &program,
                Stdio::null(),
                hex_runtime::linux::process::HeldLaunch::default(),
                lab.config.gate(),
                &mut |identity| {
                    let result = (|| -> Check<()> {
                        need(
                            !text(marker.try_exists())?,
                            "stand-in ran before durable publication",
                        )?;
                        lab.publish(&spec, identity)?;
                        published = true;
                        Ok(())
                    })();
                    result.map_err(Refusal::Broken)
                },
            ))?;
            let end = text(native.wait_for(lab.config.guard()))?
                .ok_or("published native controller did not terminate")?;
            need(
                published
                    && end.scope == TerminalScope::Released
                    && end.status == ManagedStatus::Code(0),
                "positive gate execution/reap mismatch",
            )?;
            need(
                read_bounded(&marker, lab.config.byte_limit)? == token.as_bytes(),
                "stand-in marker output mismatch",
            )
        })();
        let cleanup = lab.cleanup();
        record_result(&scope, "durable-publication", &observed, &cleanup)?;
        observed?;
        need(
            cleanup.is_empty(),
            &format!("cleanup uncertainty: {cleanup:?}"),
        )
    })();
    assert!(result.is_ok(), "{result:?}");
}

fn exact_pidfd(identity: &shards_api::Identity, parent: u32) -> Check<OwnedFd> {
    let pid = text(i32::try_from(identity.pid))?;
    let pidfd = text(rustix::process::pidfd_open(
        Pid::from_raw(pid).ok_or("invalid owned PID")?,
        PidfdFlags::empty(),
    ))?;
    // Validate after opening: a PID reused between a prior /proc read and
    // pidfd_open must never grant this fixture signal authority over its reuse.
    let stat = Stat::of(pid).ok_or("owned process identity unavailable")?;
    need(
        stat.start_ticks == identity.started && stat.ppid == text(i32::try_from(parent))?,
        "owned process PID/start/parent mismatch",
    )?;
    Ok(pidfd)
}

fn isolation_ack(reader: &mut std::io::PipeReader, end: Instant) -> Check<Instant> {
    let flags = text(rustix::fs::fcntl_getfl(&*reader))?;
    text(rustix::fs::fcntl_setfl(
        &*reader,
        flags | rustix::fs::OFlags::NONBLOCK,
    ))?;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        need(
            !left.is_zero(),
            "actual isolation ACK window missed; retain uncertainty",
        )?;
        let mut byte = [0_u8];
        match reader.read(&mut byte) {
            Ok(0) => return Err("isolation notification ended before actual byte 2".to_owned()),
            Ok(_) => {
                let at = Instant::now();
                if byte != [2] {
                    return Err(format!(
                        "unexpected isolation notification byte {}",
                        byte[0]
                    ));
                }
                return Ok(at);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let left = end.saturating_duration_since(Instant::now());
                need(!left.is_zero(), "actual isolation ACK guard expired")?;
                let timeout = Timespec {
                    tv_sec: text(i64::try_from(left.as_secs()))?,
                    tv_nsec: i64::from(left.subsec_nanos()),
                };
                let mut fds = [PollFd::new(&*reader, PollFlags::IN | PollFlags::HUP)];
                match rustix::event::poll(&mut fds, Some(&timeout)) {
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn parent_body(config: Config, scope: PathBuf) -> Check<()> {
    let mut lab = Lab::open(config, scope.clone())?;
    let stop = Arc::new((Mutex::new(false), Condvar::new()));
    let mut workers = Vec::new();
    let (ready_tx, ready_rx) = mpsc::sync_channel(2);
    let observed = (|| -> Check<()> {
        let mut identities = Vec::new();
        let mut isolation_acks = Vec::new();
        for suffix in ["first", "sibling"] {
            let (record, native, marker, _, program) = lab.reserve(suffix)?;
            let commitment = text(record.native_commitment())?;
            let end = deadline(lab.config.guard())?;
            let mut notification = text(native.prepare_isolation_ack())?;
            need(
                Instant::now() < end,
                "pre-spawn isolation notification window missed",
            )?;
            let supervisor = Arc::clone(&lab.supervisor);
            let stop = Arc::clone(&stop);
            let ready = ready_tx.clone();
            let guard = lab.config.guard();
            let gate = lab.config.gate();
            workers.push(text(
                std::thread::Builder::new()
                    .name(format!("gate-{suffix}"))
                    .spawn(move || {
                        supervisor
                            .bind_managed(
                                &native,
                                &program,
                                Stdio::null(),
                                hex_runtime::linux::process::HeldLaunch::default(),
                                gate,
                                &mut |identity| {
                                    let (lock, changed) = &*stop;
                                    let stopped = lock
                                        .lock()
                                        .map_err(|error| Refusal::Broken(error.to_string()))?;
                                    ready
                                        .send(identity.clone())
                                        .map_err(|error| Refusal::Broken(error.to_string()))?;
                                    drop(
                                        changed
                                            .wait_timeout_while(stopped, guard, |stopped| !*stopped)
                                            .map_err(|error| Refusal::Broken(error.to_string()))?,
                                    );
                                    Err(Refusal::Broken("ROOT parent held publication".to_owned()))
                                },
                            )
                            .map(|_| ())
                    }),
            )?);
            let ack = isolation_ack(&mut notification, end);
            let within_guard = ack.as_ref().is_ok_and(|at| *at < end);
            let receipt = json!({
                "suffix":suffix,"actual_byte_2_observed":ack.is_ok(),"within_guard":within_guard,
                "notification":ack.as_ref().map(|at| format!("{at:?}")).map_err(String::as_str),
                "eof_required_for_ack":false,
                "native_scope":record.native_scope.as_ref().map(|scope| scope.bytes()),
                "commitment":commitment,
                "legacy_gate_wait_syscall_not_sampled":lab.config.gate_wait_syscall,
                "legacy_observation_ms_not_polled":lab.config.observation_ms,
            });
            let bytes = text(serde_json::to_vec_pretty(&receipt))?;
            need(
                text(u64::try_from(bytes.len()))? <= lab.config.byte_limit,
                "ACK receipt byte bound",
            )?;
            text(fs::write(
                lab.scope.join(format!("{suffix}.isolation-ack.json")),
                bytes,
            ))?;
            // Preserve actual ACK/error facts even if the later held-publication phase fails.
            ack?;
            need(
                within_guard,
                "actual isolation ACK arrived after its original guard",
            )?;
            let left = end.saturating_duration_since(Instant::now());
            need(
                !left.is_zero(),
                "held publication notification window missed",
            )?;
            let identity = text(ready_rx.recv_timeout(left))?;
            need(
                Instant::now() < end,
                "held publication notice arrived after its original guard",
            )?;
            need(
                !text(marker.try_exists())?,
                "stand-in ran while publication was held",
            )?;
            // The callback occurs only after the producer function returns and drops its writer.
            // Take ONE nonblocking tail snapshot at that fence. Open is retained-FD evidence,
            // not an ACK precondition: otherwise the target isolation mutant is masked.
            let mut extra = [0_u8];
            let tail = match notification.read(&mut extra) {
                Ok(0) => Ok("eof"),
                Ok(_) => Err(format!("extra isolation notification byte {}", extra[0])),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok("open"),
                Err(error) => Err(error.to_string()),
            };
            let tail_receipt = json!({
                "suffix":suffix,"pid":identity.pid,"started":identity.started,
                "after_producer_drop_fence":true,"tail":&tail,"eof_required_for_ack":false,
            });
            let bytes = text(serde_json::to_vec_pretty(&tail_receipt))?;
            need(
                text(u64::try_from(bytes.len()))? <= lab.config.byte_limit,
                "ACK tail receipt byte bound",
            )?;
            text(fs::write(
                lab.scope.join(format!("{suffix}.isolation-tail.json")),
                bytes,
            ))?;
            tail?;
            // Start the sibling only after actual isolation and the first held publication.
            // Its clone can then inherit the writer which the isolation control targets.
            isolation_acks.push(json!({
                "pid":identity.pid,"started":identity.started,
                "actual_byte_2_observed":true,"publication_held":true,"within_guard":true,
                "receipt":receipt,"tail_receipt":tail_receipt,
            }));
            identities.push(identity);
        }
        let announced: Vec<_> = identities
            .iter()
            .map(|identity| GateIdentity {
                pid: identity.pid,
                started: identity.started,
                vsock: identity.vsock.clone(),
            })
            .collect();
        let raw = text(serde_json::to_string(&json!({
            "identities":&announced,"isolation_acks":&isolation_acks
        })))?;
        let mut output = std::io::stdout().lock();
        // Complete a possible libtest prefix before this owned protocol line. The outer actor
        // can acquire both exact pidfds before waiting for the kernel STOP notification.
        text(writeln!(output, "\n{IDENTITIES}{raw}"))?;
        text(output.flush())?;
        drop(output);
        let sibling = identities.get(1).ok_or("sibling identity missing")?;
        let pidfd = exact_pidfd(sibling, std::process::id())?;
        // This exact owned parent process is killed/reaped by its outer controller at the ONE
        // outer deadline. A kernel wait here needs no periodic probe or extra waiter thread.
        // STOPPED|NOWAIT cannot consume the Supervisor's EXITED status.
        let mut stop_sent = false;
        let stopped = (|| {
            text(rustix::process::pidfd_send_signal(&pidfd, Signal::STOP))?;
            stop_sent = true;
            text(rustix::process::waitid(
                WaitId::PidFd(pidfd.as_fd()),
                WaitIdOptions::STOPPED | WaitIdOptions::NOWAIT,
            ))
        })();
        let kernel_stop = json!({
            "pid":sibling.pid,"started":sibling.started,"signal_sent":stop_sent,
            "returned_at_instant_debug":format!("{:?}",Instant::now()),
            "waitid":stopped.as_ref().map(|status|status.as_ref().map(gate_wait_status)).map_err(String::as_str),
        });
        write_gate_evidence(
            &scope.join("parent.stop.json"),
            lab.config.byte_limit,
            &kernel_stop,
        )?;
        let stopped = stopped?.ok_or("exact sibling STOP status missing")?;
        need(
            stopped.stopped() && stopped.stopping_signal() == Some(Signal::STOP.as_raw()),
            "exact sibling was not stopped by SIGSTOP",
        )?;
        let raw = text(serde_json::to_string(
            &json!({"identities":&announced,"isolation_acks":&isolation_acks,"sibling_stopped":true,"kernel_stop":&kernel_stop}),
        ))?;
        let mut output = std::io::stdout().lock();
        text(writeln!(output, "{READY}{raw}"))?;
        text(output.flush())?;
        // The outer fixture kills this exact parent after readiness. This guard
        // owns every alternative branch; it never releases the stand-in gate.
        let (lock, changed) = &*stop;
        let stopped = text(lock.lock())?;
        drop(text(changed.wait_timeout_while(
            stopped,
            lab.config.guard(),
            |stopped| !*stopped,
        ))?);
        Err("outer parent-death control did not kill its owned parent".to_owned())
    })();
    if let Ok(mut stopped) = stop.0.lock() {
        *stopped = true;
    }
    stop.1.notify_all();
    for (_, native) in &lab.natives {
        let _ = native.stop();
    }
    let mut joins = Vec::new();
    for worker in workers {
        joins.push(
            worker
                .join()
                .map_err(|_| "owned gate worker panicked".to_owned()),
        );
    }
    let cleanup = lab.cleanup();
    record_result(&scope, "controller-parent-actor", &observed, &cleanup)?;
    observed?;
    need(
        joins.iter().all(Result::is_ok) && cleanup.is_empty(),
        "parent actor cleanup uncertain",
    )
}

fn read_phase(
    output: &mut std::process::ChildStdout,
    config: &Config,
    raw: &mut Vec<u8>,
    cursor: &mut usize,
    expected: &str,
    end: Instant,
) -> Check<serde_json::Value> {
    text(rustix::fs::fcntl_setfl(
        output.as_fd(),
        rustix::fs::OFlags::NONBLOCK,
    ))?;
    loop {
        need(
            Instant::now() < end,
            "owned parent phase guard expired; retain as harness uncertainty",
        )?;
        while let Some(newline) = raw
            .get(*cursor..)
            .ok_or("invalid owned phase cursor")?
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let next = (*cursor)
                .checked_add(newline)
                .and_then(|at| at.checked_add(1))
                .ok_or("owned phase cursor overflow")?;
            let line = text(std::str::from_utf8(
                raw.get(*cursor..next).ok_or("invalid owned phase line")?,
            ))?;
            *cursor = next;
            if let Some(value) = line.strip_prefix(expected) {
                let value = text_result(value)?;
                need(
                    Instant::now() < end,
                    "owned parent phase completed too late",
                )?;
                return Ok(value);
            }
            need(
                !line.starts_with(IDENTITIES) && !line.starts_with(READY),
                "owned parent phases are duplicated or out of order",
            )?;
        }
        let mut bytes = [0; 4096];
        match output.read(&mut bytes) {
            Ok(0) => return Err("owned parent ended before held-child readiness".to_owned()),
            Ok(count) => {
                raw.extend_from_slice(bytes.get(..count).ok_or("invalid readiness read count")?);
                need(
                    text(u64::try_from(raw.len()))? <= config.byte_limit,
                    "parent output byte budget exceeded",
                )?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let left = end.saturating_duration_since(Instant::now());
                need(!left.is_zero(), "parent readiness guard expired")?;
                let timeout = Timespec {
                    tv_sec: text(i64::try_from(left.as_secs()))?,
                    tv_nsec: i64::from(left.subsec_nanos()),
                };
                let mut fds = [PollFd::new(&*output, PollFlags::IN | PollFlags::HUP)];
                text(rustix::event::poll(&mut fds, Some(&timeout)))?;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn text_result(raw: &str) -> Check<serde_json::Value> {
    text(serde_json::from_str(raw))
}

fn gate_time(began: Instant, at: Instant) -> serde_json::Value {
    json!({
        "instant_debug":format!("{at:?}"),
        "ns_since_t0_decimal":at.checked_duration_since(began).map(|elapsed| elapsed.as_nanos().to_string()),
    })
}

fn gate_wait_status(status: &rustix::process::WaitIdStatus) -> serde_json::Value {
    json!({
        "si_signo":status.raw_signo(),"si_errno":status.raw_errno(),"si_code":status.raw_code(),
        "exit_status":status.exit_status(),"terminating_signal":status.terminating_signal(),
        "stopped":status.stopped(),"stopping_signal":status.stopping_signal(),
        "debug":format!("{status:?}"),
    })
}

fn gate_child_status(status: &std::process::ExitStatus) -> serde_json::Value {
    json!({"raw_wait_status":status.into_raw(),"code":status.code(),"signal":status.signal()})
}

fn write_gate_evidence(path: &Path, bound: u64, value: &serde_json::Value) -> Check<()> {
    let bytes = text(serde_json::to_vec_pretty(value))?;
    need(
        text(u64::try_from(bytes.len()))? <= bound,
        "gate evidence byte bound",
    )?;
    text(fs::write(path, bytes))
}

fn child_end(
    child: &mut Child,
    pidfd: Option<&OwnedFd>,
    end: Instant,
) -> Check<std::process::ExitStatus> {
    // A previously reaped Child caches its status. Otherwise the retained exact pidfd supplies
    // readiness and this SAME Child remains the sole consuming waiter for the parent process.
    if let Some(status) = text(child.try_wait())? {
        return Ok(status);
    }
    poll_exit(
        pidfd.ok_or("owned parent pidfd unavailable; retain uncertainty")?,
        end,
    )?;
    text(child.wait())
}

/// AC-T7.1.2: parent death produces EOF before the gate deadline, with a stopped sibling
/// unable to hide a foreign writer by exiting first. Only this fixture's exact PIDs are signaled.
#[test]
#[ignore = "ROOT only: same-binary controller parent, actual per-launch isolation ACK and private delegated cgroup"]
fn completion_controller_parent_death_cancels_gate() {
    let result = (|| -> Check<()> {
        let config = Config::load()?;
        if std::env::var("HEX_COMPLETION_GATE_ROLE").as_deref() == Ok("controller-parent") {
            return parent_body(
                config,
                PathBuf::from(text(std::env::var("HEX_COMPLETION_GATE_SCOPE"))?),
            );
        }
        let scope = new_scope(&config)?;
        let group_root = cgroup_path(&config, &scope)?;
        need(
            !text(group_root.try_exists())?,
            "private cgroup root existed before fixture",
        )?;
        let prior_subreaper = text(rustix::process::child_subreaper())?;
        let mut command = Command::new(text(std::env::current_exe())?);
        command
            .args([
                "--ignored",
                "--exact",
                PARENT_CASE,
                "--nocapture",
                "--test-threads=1",
            ])
            .env("HEX_COMPLETION_GATE_ROLE", "controller-parent")
            .env("HEX_COMPLETION_GATE_SCOPE", &scope)
            .stdin(ChildStdio::null())
            .stdout(ChildStdio::piped())
            .stderr(text(File::create(scope.join("parent.stderr")))?);
        let owner_pid = std::process::id();
        let owner_birth = i32::try_from(owner_pid)
            .ok()
            .and_then(Stat::of)
            .map(|stat| stat.start_ticks);
        let mut events = Vec::new();
        let began = Instant::now();
        let outer_end = began
            .checked_add(config.guard())
            .ok_or("outer parent deadline overflow")?;
        let evidence_path = scope.join("parent-death.evidence.json");
        events.push(
            json!({"event":"origin","owner_pid":owner_pid,"owner_started":owner_birth,
            "t0":gate_time(began,began),"D":gate_time(began,outer_end),
            "G":began.checked_add(config.gate()).map(|at|gate_time(began,at)),
            "guard_ms":config.guard_ms,"gate_budget_ms":config.gate_budget_ms}),
        );
        let subreaper = text(rustix::process::set_child_subreaper(Some(
            rustix::process::getpid(),
        )));
        if let Err(error) = subreaper {
            events.push(json!({"event":"subreaper_refused","at":gate_time(began,Instant::now()),"error":&error}));
            let record = write_gate_evidence(
                &evidence_path,
                config.byte_limit,
                &json!({"events":&events}),
            );
            return Err(format!("subreaper: {error}; evidence: {record:?}"));
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let restore = rustix::process::set_child_subreaper(prior_subreaper);
                events.push(
                    json!({"event":"parent_spawn_refused","at":gate_time(began,Instant::now()),
                    "error":error.to_string(),"subreaper_restore":format!("{restore:?}")}),
                );
                let record = write_gate_evidence(
                    &evidence_path,
                    config.byte_limit,
                    &json!({"events":&events}),
                );
                return Err(format!(
                    "parent spawn: {error}; subreaper restore: {restore:?}; evidence: {record:?}"
                ));
            }
        };
        let parent_pidfd =
            rustix::process::pidfd_open(Pid::from_child(&child), PidfdFlags::empty());
        events.push(
            json!({"event":"parent_owned","at":gate_time(began,Instant::now()),"pid":child.id(),
            "started":i32::try_from(child.id()).ok().and_then(Stat::of).map(|stat|stat.start_ticks),
            "pidfd_open":parent_pidfd.as_ref().map(|_|()).map_err(ToString::to_string)}),
        );
        let mut owned = Vec::new();
        let mut raw = Vec::new();
        let observed = (|| -> Check<()> {
            let parent_fd = parent_pidfd.as_ref().map_err(|error| error.to_string())?;
            let mut output = child.stdout.take().ok_or("owned parent output missing")?;
            let mut cursor = 0;
            events.push(
                json!({"event":"identities_wait_begin","at":gate_time(began,Instant::now())}),
            );
            let announced = read_phase(
                &mut output,
                &config,
                &mut raw,
                &mut cursor,
                IDENTITIES,
                outer_end,
            )?;
            events.push(json!({"event":"identities_received","at":gate_time(began,Instant::now()),"frame":&announced}));
            let original_identities = announced
                .get("identities")
                .ok_or("announced owned identities missing")?;
            let identities: Vec<GateIdentity> =
                text(serde_json::from_value(original_identities.clone()))?;
            let identities: Vec<_> = identities.into_iter().map(GateIdentity::native).collect();
            need(identities.len() == 2, "exactly two held stand-ins required")?;
            let acknowledgments = announced
                .get("isolation_acks")
                .and_then(serde_json::Value::as_array)
                .ok_or("actual per-launch isolation acknowledgments missing")?;
            need(
                acknowledgments.len() == identities.len(),
                "isolation ACK count mismatch",
            )?;
            for (identity, ack) in identities.iter().zip(acknowledgments) {
                need(
                    ack.get("pid").and_then(serde_json::Value::as_u64)
                        == Some(u64::from(identity.pid))
                        && ack.get("started").and_then(serde_json::Value::as_u64)
                            == Some(identity.started)
                        && ack
                            .get("actual_byte_2_observed")
                            .and_then(serde_json::Value::as_bool)
                            == Some(true)
                        && ack
                            .get("publication_held")
                            .and_then(serde_json::Value::as_bool)
                            == Some(true)
                        && ack.get("within_guard").and_then(serde_json::Value::as_bool)
                            == Some(true),
                    "isolation ACK was not the same actual held launch",
                )?;
            }
            for identity in &identities {
                owned.push(exact_pidfd(identity, child.id())?);
            }
            events.push(json!({"event":"exact_native_pidfds_owned","at":gate_time(began,Instant::now()),"count":owned.len()}));
            let ready = read_phase(
                &mut output,
                &config,
                &mut raw,
                &mut cursor,
                READY,
                outer_end,
            )?;
            events.push(json!({"event":"stopped_phase_received","at":gate_time(began,Instant::now()),"frame":&ready}));
            need(
                ready.get("identities") == Some(original_identities)
                    && ready.get("isolation_acks") == announced.get("isolation_acks")
                    && ready
                        .get("sibling_stopped")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true),
                "exact announced sibling stop was not observed",
            )?;
            for suffix in ["first", "sibling"] {
                let marker = scope.join(format!("{suffix}.marker")).try_exists();
                events.push(json!({"event":"canary_before_parent_death","at":gate_time(began,Instant::now()),
                    "suffix":suffix,"exists":marker.as_ref().copied().map_err(ToString::to_string)}));
                need(
                    !text(marker)?,
                    "stand-in marker appeared before owned parent death",
                )?;
            }
            let killed = text(child.kill());
            events.push(
                json!({"event":"parent_kill_returned","at":gate_time(began,Instant::now()),
                "result":&killed}),
            );
            killed?;
            let ended = child_end(&mut child, Some(parent_fd), outer_end);
            events.push(
                json!({"event":"parent_terminal_reap_returned","at":gate_time(began,Instant::now()),
                "status":ended.as_ref().map(gate_child_status).map_err(String::as_str)}),
            );
            ended?;
            let parent_check_at = Instant::now();
            events.push(json!({"event":"parent_D_check","at":gate_time(began,parent_check_at),"before_D":parent_check_at<outer_end}));
            need(
                parent_check_at < outer_end,
                "parent terminal window missed; cannot exclude publication timeout",
            )?;
            let first = owned.first().ok_or("first pidfd missing")?;
            let earliest_gate = began
                .checked_add(config.gate())
                .ok_or("gate deadline overflow")?;
            let first_end = deadline(config.guard())?.min(earliest_gate);
            events.push(json!({"event":"first_terminal_wait_begin","at":gate_time(began,Instant::now()),"end":gate_time(began,first_end)}));
            let first_poll = poll_exit(first, first_end);
            events.push(json!({"event":"first_terminal_notification","at":gate_time(began,Instant::now()),"result":&first_poll}));
            first_poll?;
            let first_check_at = Instant::now();
            events.push(json!({"event":"first_G_check","at":gate_time(began,first_check_at),"before_G":first_check_at<earliest_gate}));
            need(
                first_check_at < earliest_gate,
                "gate timing window missed; cannot attribute exit to EOF",
            )?;
            let status = text(rustix::process::waitid(
                WaitId::PidFd(first.as_fd()),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
            ));
            events.push(json!({"event":"first_waitid_returned","at":gate_time(began,Instant::now()),
                "status":status.as_ref().map(|value|value.as_ref().map(gate_wait_status)).map_err(String::as_str)}));
            let status = status?.ok_or("first held stand-in was not reaped")?;
            need(
                status.exit_status() == Some(127),
                "first held stand-in did not cancel before exec",
            )?;
            let sibling = owned.get(1).ok_or("sibling pidfd missing")?;
            let identity = identities.get(1).ok_or("sibling identity missing")?;
            let stat = read_bounded(
                &PathBuf::from(format!("/proc/{}/stat", identity.pid)),
                config.byte_limit,
            )?;
            let stat = text(std::str::from_utf8(&stat))?;
            events.push(json!({"event":"sibling_kernel_stat_after_first","at":gate_time(began,Instant::now()),"pid":identity.pid,
                "started":identity.started,"raw_stat":stat}));
            need(
                stat.rsplit_once(')')
                    .and_then(|(_, tail)| tail.split_whitespace().next())
                    == Some("T"),
                "sibling did not remain stopped during EOF observation",
            )?;
            let continued = text(rustix::process::pidfd_send_signal(sibling, Signal::CONT));
            events.push(json!({"event":"sibling_continue_returned","at":gate_time(began,Instant::now()),"result":&continued}));
            continued?;
            let sibling_end = deadline(config.guard())?.min(earliest_gate);
            events.push(json!({"event":"sibling_terminal_wait_begin","at":gate_time(began,Instant::now()),"end":gate_time(began,sibling_end)}));
            let sibling_poll = poll_exit(sibling, sibling_end);
            events.push(json!({"event":"sibling_terminal_notification","at":gate_time(began,Instant::now()),"result":&sibling_poll}));
            sibling_poll?;
            let status = text(rustix::process::waitid(
                WaitId::PidFd(sibling.as_fd()),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
            ));
            events.push(json!({"event":"sibling_waitid_returned","at":gate_time(began,Instant::now()),
                "status":status.as_ref().map(|value|value.as_ref().map(gate_wait_status)).map_err(String::as_str)}));
            let status = status?.ok_or("sibling was not reaped")?;
            need(
                status.exit_status() == Some(127),
                "sibling did not cancel before exec",
            )?;
            for suffix in ["first", "sibling"] {
                let marker = scope.join(format!("{suffix}.marker")).try_exists();
                events.push(json!({"event":"canary_after_native_ends","at":gate_time(began,Instant::now()),
                    "suffix":suffix,"exists":marker.as_ref().copied().map_err(ToString::to_string)}));
                need(!text(marker)?, "parent death executed a stand-in marker")?;
            }
            Ok(())
        })();
        // Every observation branch reaches owned-child cleanup before assertions.
        events.push(json!({"event":"observation_finished","at":gate_time(began,Instant::now()),"result":&observed}));
        let mut cleanup = Vec::new();
        let parent_probe = child.try_wait();
        events.push(json!({"event":"cleanup_parent_probe","at":gate_time(began,Instant::now()),
            "status":parent_probe.as_ref().map(|status|status.as_ref().map(gate_child_status)).map_err(ToString::to_string)}));
        match parent_probe {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                if let Err(error) = child.kill() {
                    cleanup.push(error.to_string());
                }
            }
        }
        let parent_cleanup = deadline(config.guard())
            .and_then(|end| child_end(&mut child, parent_pidfd.as_ref().ok(), end));
        events.push(
            json!({"event":"cleanup_parent_terminal_reap","at":gate_time(began,Instant::now()),
            "status":parent_cleanup.as_ref().map(gate_child_status).map_err(String::as_str)}),
        );
        if let Err(error) = parent_cleanup {
            cleanup.push(error);
        }
        for (index, fd) in owned.iter().enumerate() {
            let continued = rustix::process::pidfd_send_signal(fd, Signal::CONT);
            events.push(json!({"event":"cleanup_native_continue","index":index,"at":gate_time(began,Instant::now()),
                "result":continued.as_ref().map(|_|()).map_err(ToString::to_string)}));
            match continued {
                Ok(()) | Err(rustix::io::Errno::SRCH) => {}
                Err(error) => cleanup.push(error.to_string()),
            }
            let killed = rustix::process::pidfd_send_signal(fd, Signal::KILL);
            events.push(json!({"event":"cleanup_native_kill","index":index,"at":gate_time(began,Instant::now()),
                "result":killed.as_ref().map(|_|()).map_err(ToString::to_string)}));
            match killed {
                Ok(()) | Err(rustix::io::Errno::SRCH) => {}
                Err(error) => cleanup.push(error.to_string()),
            }
            let notified = deadline(config.guard()).and_then(|end| poll_exit(fd, end));
            events.push(json!({"event":"cleanup_native_terminal_notification","index":index,"at":gate_time(began,Instant::now()),"result":&notified}));
            match notified {
                Ok(()) => {}
                Err(error) => cleanup.push(error),
            }
            let waited = rustix::process::waitid(
                WaitId::PidFd(fd.as_fd()),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
            );
            events.push(json!({"event":"cleanup_native_waitid","index":index,"at":gate_time(began,Instant::now()),
                "status":waited.as_ref().map(|status|status.as_ref().map(gate_wait_status)).map_err(ToString::to_string)}));
            match waited {
                Ok(Some(_)) | Err(rustix::io::Errno::CHILD) => {}
                other => cleanup.push(format!("exact owned orphan reap uncertain: {other:?}")),
            }
        }
        let lease = (|| -> Check<File> {
            let lease = text(
                fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(scope.join("native-vm-owner.lock")),
            )?;
            text(lease.try_lock())?;
            Ok(lease)
        })();
        events.push(
            json!({"event":"cleanup_native_lease","at":gate_time(began,Instant::now()),
            "result":lease.as_ref().map(|_|()).map_err(String::as_str)}),
        );
        let lease = match lease {
            Ok(lease) => Some(lease),
            Err(error) => {
                cleanup.push(format!("native owner lease remains uncertain: {error}"));
                None
            }
        };
        for suffix in ["first", "sibling"] {
            if lease.is_none() {
                break;
            }
            let result = (|| -> Check<()> {
                let record = text(BoxRecord::decode(&read_bounded(
                    &scope.join(format!("{suffix}.creating.record.json")),
                    config.byte_limit,
                )?))?;
                need(
                    record.spec.id.as_str() == format!("{}-{suffix}", config.spec.id.as_str())
                        && record.native_scope.is_some(),
                    "owned Creating scope/BoxId mismatch",
                )?;
                let path = group_root
                    .join(".vm/gate-owner")
                    .join(text(managed_scope_id(&record))?.as_str());
                let group = BoxCgroup::at(path);
                if text(group.populated_if_present())?.is_some() {
                    text(group.kill())?;
                    need(
                        text(group.wait_empty(config.guard()))?,
                        "owned cgroup remains populated",
                    )?;
                    text(group.remove())?;
                }
                let store = text(Store::open(&scope.join("records"), &scope.join("run")))?;
                let mut retained = text(store.read_lifecycle(&record.spec.id))?
                    .ok_or("owned post-parent record missing")?;
                need(
                    retained.native_scope == record.native_scope
                        && text(retained.native_commitment())? == text(record.native_commitment())?,
                    "owned parent-death scope/spec was transplanted",
                )?;
                retained.phase = Phase::Deleting;
                retained.exit = Some(Exit::Unknown);
                text(store.put_lifecycle(&retained))?;
                text(store.remove(&retained.spec.id))
            })();
            events.push(json!({"event":"cleanup_native_scope_store","suffix":suffix,"at":gate_time(began,Instant::now()),"result":&result}));
            if let Err(error) = result {
                cleanup.push(error);
            }
        }
        for path in [
            group_root.join(".vm/gate-owner"),
            group_root.join(".vm"),
            group_root.clone(),
        ] {
            if lease.is_none() {
                break;
            }
            match path.try_exists() {
                Ok(true) => {
                    if let Err(error) = fs::remove_dir(&path) {
                        cleanup.push(error.to_string());
                    }
                }
                Ok(false) => {}
                Err(error) => cleanup.push(error.to_string()),
            }
        }
        if owned.len() != 2 {
            cleanup.push(
                "pre-readiness native identities incomplete; retain private lab for ROOT review"
                    .to_owned(),
            );
        }
        let restored = rustix::process::set_child_subreaper(prior_subreaper);
        events.push(
            json!({"event":"cleanup_subreaper_restore","at":gate_time(began,Instant::now()),
            "result":restored.as_ref().map(|_|()).map_err(ToString::to_string)}),
        );
        if let Err(error) = restored {
            cleanup.push(error.to_string());
        }
        for suffix in ["first", "sibling"] {
            let marker = scope.join(format!("{suffix}.marker")).try_exists();
            events.push(
                json!({"event":"canary_after_cleanup","at":gate_time(began,Instant::now()),
                "suffix":suffix,"exists":marker.as_ref().copied().map_err(ToString::to_string)}),
            );
        }
        events.push(json!({"event":"cleanup_complete","at":gate_time(began,Instant::now()),"uncertainty":&cleanup}));
        let stdout_saved = text(fs::write(scope.join("parent.stdout"), raw));
        let summary_saved = record_result(&scope, "controller-parent-death", &observed, &cleanup);
        events.push(
            json!({"event":"existing_receipts_written","at":gate_time(began,Instant::now()),
            "parent_stdout":&stdout_saved,"gate_result":&summary_saved}),
        );
        write_gate_evidence(
            &evidence_path,
            config.byte_limit,
            &json!({
                "schema":"hex-parent-death-observations-1","vm_gpu_claim":false,
                "clock":"original std::time::Instant; event times are observer return/check times, not kernel timestamps",
                "events":&events,"observation":&observed,"cleanup_uncertainty":&cleanup,
            }),
        )?;
        stdout_saved?;
        summary_saved?;
        observed?;
        need(
            cleanup.is_empty(),
            &format!("cleanup uncertainty: {cleanup:?}"),
        )
    })();
    assert!(result.is_ok(), "{result:?}");
}
