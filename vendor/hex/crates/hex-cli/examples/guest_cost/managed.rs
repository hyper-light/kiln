//! The Shards class's driver where a node boots its guests managed (Windows' WHP): each guest booted `--host-managed`
//! through the managed launch a node takes (`hex_runtime::guest::managed_plan`, then
//! `hex_runtime::guest::boot_managed`: Shards' own plan with this run's runtime folders, the branch a node whose cost
//! evidence declares no native images takes), its native owner a node's own (`WinRuntime`, with no keeper, so its
//! guests end with this process), its record a canonical one kept in memory ([`Scope`]: a backing of the Shards
//! class's shared execution, as a node's shared guest is, the producer keeping no records). Each guest is held to the
//! census's rate by its own governor (Shards' D191, D255, `ManagedGuest::rate`) once it is ready, and read from then on
//! at that governor's period ([`Driver::hold`]): its own, derived from the step its VM process measured itself (D191's
//! g), so two guests may answer two; one guest's answer is the census's window, its boots read at it
//! ([`Driver::governed_window`]). It is held as a node's learner holds a VM it has booted: its VM process takes
//! a rate on its socket only once its machine runs (its socket listening, its microVM started), which its agent's
//! answer shows. It is read as its node reads it: its vCPUs' guest time first, from its VM process's own answer
//! (`ManagedGuest::host_use`, D260: WHP's run-time counter, which only that process reads), none before that process
//! says it (its socket not yet listening, its microVM not yet started: its guest has run nothing), then its job's
//! kernel accounting (`WinRuntime::managed_usage`: the job's CPU time and committed peak), so its VMM's own CPU, the
//! whole less its vCPUs', is never less than none; a job that counts less CPU than its VM process says it used is
//! refused, told. Its guest RAM's part of its job's commit is its committed region of that size, which Shards' WHP
//! `reserve_ram` commits whole from the start (`hex_runtime::windows::sys::region`) in its VM process (Shards' confined
//! VM process, a child of the process its launch names, as `ManagedGuest::vm_process` names it: Shards 9d8a05f6a). It is
//! ready when its agent answers, and its own report is read from its agent's `oci` capacity, as every OS's.

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use hex_api::{
    BackingKey, BackingPhase, CapacityResources, Class, Execution, Refusal, RuntimeCost,
};
use hex_client::guest::{Client, own_link, shards::ShardsLink};
use hex_core::record::{NativeRecord, NativeStage, PoolRecord, ProcId, RECORD_VERSION};
use hex_runtime::devices::{ManagedLaunch, PoolRecords, Records, Transaction};
use hex_runtime::guest::GuestConfig;
use hex_runtime::windows::WinRuntime;
use hex_runtime::windows::sys::{self, Handle};
use sha2::{Digest, Sha256};

use super::guest::guest_range;
use super::run::{Window, walk};
use super::seam::{Driver, Envelope, Files, Holding, Launched, Lines, Probe, Use};

pub struct Managed {
    config: GuestConfig,
    runtime: Arc<WinRuntime>,
    paths: shards_api::RuntimePaths,
    cache: PathBuf,
    home: PathBuf,
    /// The run's own folder of runtime folders, its guests' sockets among them.
    folder: PathBuf,
    /// The cost each guest is booted under while its own is measured: its envelope's, floor and ceiling, in this
    /// host's quanta, its identity the digest of what this run measures. The cost the run writes is its evidence's.
    cost: RuntimeCost,
    /// The census's rate and the window its governor answered for it ([`Driver::governed_window`]).
    held: Option<(u64, Window)>,
    /// The next guest's number: its backing key's and its socket's.
    next: u64,
}

/// A launched guest: its VM, its scope (its record and native owner), and, once it is ready, its agent's client.
pub struct Guest {
    vm: Arc<shards_api::ManagedGuest>,
    scope: Arc<Scope>,
    client: Option<Client>,
}

/// One guest's canonical record with a node's native owner: the transaction a node's records give its managed launch
/// (`hex_runtime::devices::Transaction`), held in memory, the producer keeping no records. Its checks are the node's
/// own (hex-node's `recorded::Transaction`): a fresh creation launches, its controller is introduced to its record as it
/// is published, and only a draining or released backing is finalized.
pub struct Scope {
    runtime: Arc<WinRuntime>,
    record: Mutex<PoolRecord>,
}

fn kept_nowhere(what: &str) -> Refusal {
    Refusal::Invalid {
        what: "the guest-cost producer's records",
        why: format!("{what}: the producer keeps one backing's record in memory, and no other"),
    }
}

impl Scope {
    fn lock(&self) -> Result<MutexGuard<'_, PoolRecord>, Refusal> {
        self.record.lock().map_err(|_| {
            Refusal::Broken("the guest-cost producer's record lock is poisoned".to_owned())
        })
    }

    /// A copy of the record as it is now, as a node's store reads one back.
    fn snapshot(&self) -> Result<PoolRecord, Refusal> {
        PoolRecord::decode(&self.lock()?.encode()?)
    }

    /// The backing moves to `phase`, as a node's records move it (draining before its VM stops).
    fn moved(&self, phase: BackingPhase) -> Result<(), Refusal> {
        self.lock()?.phase = phase;
        Ok(())
    }
}

impl Transaction for Scope {
    fn records(&self) -> Result<Records, Refusal> {
        Ok(Records::new())
    }
    fn pool_records(&self) -> Result<PoolRecords, Refusal> {
        Ok(vec![self.snapshot()?])
    }
    fn read_pool(&self, key: &BackingKey) -> Result<Option<PoolRecord>, Refusal> {
        let record = self.snapshot()?;
        Ok(if record.key == *key {
            Some(record)
        } else {
            None
        })
    }
    fn put_pool(&self, record: &PoolRecord) -> Result<(), Refusal> {
        let mut held = self.lock()?;
        if held.key != record.key {
            return Err(kept_nowhere("another backing's record"));
        }
        *held = PoolRecord::decode(&record.encode()?)?;
        Ok(())
    }
    fn grant_pool(&self, _: &hex_api::BackingGrant) -> Result<PoolRecord, Refusal> {
        Err(kept_nowhere("a grant"))
    }
    fn remove_pool(&self, _: &BackingKey) -> Result<(), Refusal> {
        Ok(())
    }
    fn check_unsubmitted_pool(&self) -> Result<(), Refusal> {
        Err(kept_nowhere("an unsubmitted backing"))
    }
    fn read(&self, _: &hex_api::BoxId) -> Result<Option<hex_core::record::BoxRecord>, Refusal> {
        Ok(None)
    }
    fn put(&self, _: &hex_core::record::BoxRecord) -> Result<(), Refusal> {
        Err(kept_nowhere("a box's record"))
    }
    fn remove(&self, _: &hex_api::BoxId) -> Result<(), Refusal> {
        Ok(())
    }
    fn launch_managed(
        &self,
        plan: &ManagedLaunch<'_>,
        io: shards_api::LaunchIo,
        patience: Option<Duration>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        let record = self.snapshot()?;
        let native = NativeRecord::Pool(&record);
        if native.class()? != plan.class()
            || native.allocation().is_none()
            || native.stage() != NativeStage::Creating
            || native.init().is_some()
        {
            return Err(native
                .uncertain("its record isn't a fresh VM creation as the producer launches it"));
        }
        let mut introduced = false;
        let mut publish = |identity: &shards_api::Identity| {
            if introduced
                || identity.pid == 0
                || identity.started == 0
                || identity.vsock != plan.vsock()
            {
                return Err(native
                    .uncertain("its launched controller's identity or vsock isn't its plan's"));
            }
            self.lock()?.init = Some(ProcId {
                pid: identity.pid,
                start_ticks: identity.started,
            });
            introduced = true;
            Ok(())
        };
        let receipt = self
            .runtime
            .launch_managed(native, plan, io, patience, &mut publish)?;
        if !introduced {
            return Err(native.uncertain("its launch's controller wasn't introduced to its record"));
        }
        Ok(receipt)
    }
    fn attach_managed(
        &self,
        identity: Option<&shards_api::Identity>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        let record = self.snapshot()?;
        self.runtime
            .attach_managed(NativeRecord::Pool(&record), identity)
    }
    fn finalize_managed(&self, identity: Option<&shards_api::Identity>) -> Result<(), Refusal> {
        let record = self.snapshot()?;
        let native = NativeRecord::Pool(&record);
        if !matches!(
            native.stage(),
            NativeStage::Deleting | NativeStage::Released
        ) {
            return Err(native.uncertain("it isn't draining or released as it is finalized"));
        }
        match (native.init(), identity) {
            (Some(expected), Some(actual))
                if expected.pid == actual.pid && expected.start_ticks == actual.started => {}
            (None, None) => {}
            _ => return Err(native.uncertain("its finalized controller isn't its record's")),
        }
        self.runtime.finalize_managed(native, identity)
    }
    fn managed_usage(&self) -> Result<hex_api::Usage, Refusal> {
        let record = self.snapshot()?;
        self.runtime.managed_usage(NativeRecord::Pool(&record))
    }
}

impl Managed {
    /// The guest the environment configures, as `hex node` reads it (`GuestConfig::from_env`).
    pub fn from_env(out: &Path) -> Result<Self, String> {
        Self::configured(GuestConfig::from_env().map_err(|e| e.to_string())?, out)
    }

    /// The `config` guest, booted with `out/runtime/{cache,home}` as its runtime folders, which this user's profile
    /// keeps private as every folder made in it.
    pub fn configured(config: GuestConfig, out: &Path) -> Result<Self, String> {
        let runtime = out.join("runtime");
        let (cache, home) = (runtime.join("cache"), runtime.join("home"));
        for dir in [&runtime, &cache, &home] {
            std::fs::create_dir_all(dir).map_err(|e| format!("making {}: {e}", dir.display()))?;
        }
        let absolute = |dir: &Path| std::path::absolute(dir).map_err(|e| e.to_string());
        let (cache, home, folder) = (absolute(&cache)?, absolute(&home)?, absolute(&runtime)?);
        let paths = shards_api::RuntimePaths::new(cache.clone(), home.clone())
            .map_err(|e| e.to_string())?;
        let envelope = super::guest::envelope(&config)?.resources;
        let (page, block) = super::run::quanta(&home)?;
        let mut id = Sha256::new();
        id.update(b"hex-guest-cost provisional\n");
        for (name, value) in super::guest::describe(&config)? {
            id.update(format!("{name} {value}\n").as_bytes());
        }
        let cost = RuntimeCost {
            id: id.finalize().into(),
            floor: envelope,
            ceiling: envelope,
            quantum: CapacityResources {
                cpu_milli: 1,
                mem_bytes: page,
                disk_bytes: block,
            },
            guest_overhead: CapacityResources::default(),
            overhead: CapacityResources::default(),
        };
        cost.check().map_err(|e| e.to_string())?;
        Ok(Self {
            config,
            runtime: Arc::new(WinRuntime::new()),
            paths,
            cache,
            home,
            folder,
            cost,
            held: None,
            next: 0,
        })
    }

    /// Guest `n`'s backing key: digests of this run's folder and its number, so no two of its guests share one.
    fn key(&self, n: u64) -> BackingKey {
        let digest = |role: &[u8]| -> [u8; 32] {
            let mut hash = Sha256::new();
            hash.update(self.folder.as_os_str().as_encoded_bytes());
            hash.update(role);
            hash.update(n.to_le_bytes());
            hash.finalize().into()
        };
        let mut generation = [0u8; 16];
        generation.copy_from_slice(digest(b"generation").get(..16).unwrap_or(&[0; 16]));
        BackingKey {
            pool: digest(b"pool"),
            generation,
        }
    }

    /// Boots the next guest managed, its record granted the envelope, its plan's guest told no deadline (it keeps its
    /// learned bounds), its native launch within the class's W*.
    fn boot(&mut self) -> Result<(Arc<shards_api::ManagedGuest>, Arc<Scope>), String> {
        let n = self.next;
        self.next = n.checked_add(1).ok_or("the guests' count overflows")?;
        let record = PoolRecord {
            version: RECORD_VERSION,
            key: self.key(n),
            execution: Execution::ShardsShared,
            cost: self.cost,
            allocation: Some(
                self.cost
                    .allocate(super::guest::envelope(&self.config)?.resources)
                    .map_err(|e| e.to_string())?,
            ),
            devices: Vec::new(),
            phase: BackingPhase::Granted,
            init: None,
            exit: None,
        };
        record.check().map_err(|e| e.to_string())?;
        let scope = Arc::new(Scope {
            runtime: Arc::clone(&self.runtime),
            record: Mutex::new(record),
        });
        let socket = self.folder.join(format!("vm-{n}.sock"));
        let spec = self.config.spec().map_err(|e| e.to_string())?;
        let plan = hex_runtime::guest::managed_plan(
            &spec,
            &self.config.binary,
            &socket,
            &self.paths,
            None,
        )
        .map_err(|e| format!("preparing the guest's managed plan: {e}"))?;
        // Followed, as a node's managed boot is (the review's C-9): no deadline of the producer's.
        let vm = hex_runtime::guest::boot_managed(scope.as_ref(), plan)
            .map_err(|e| format!("booting the guest managed: {e}"))?;
        Ok((Arc::new(vm), scope))
    }

    /// Asks `vm`'s governor to hold `rate` milli-CPU, as a node's learner asks a managed VM's (D255), and the period it
    /// holds it over. One dial, which its VM process takes once its machine runs: a ready guest's.
    fn govern(vm: &shards_api::ManagedGuest, rate: u64) -> Result<Duration, String> {
        vm.rate(NonZeroU64::new(rate), Duration::MAX)
            .map_err(|e| format!("asking the guest's governor to hold {rate} milli-CPU: {e}"))?;
        vm.rate_period()
            .ok_or_else(|| "the guest's governor answered no period".to_owned())
    }

    /// Ends `vm` as a node deletes a backing (hex-runtime's shared release): drained, stopped and followed to its end
    /// ([`super::guest::follow_end`]), its whole scope released, then finalized through its record while it drains, as
    /// a node's records finalize a backing before they write its end (the producer writes no record).
    fn end(vm: &shards_api::ManagedGuest, scope: &Scope) -> Result<(), String> {
        scope
            .moved(BackingPhase::Draining)
            .map_err(|e| e.to_string())?;
        let identity = shards_api::Guest::identity(vm).ok();
        shards_api::Guest::stop(vm).map_err(|e| format!("stopping the guest: {e}"))?;
        super::guest::follow_end(vm)?;
        match shards_api::Guest::terminal_scope(vm) {
            Ok(Some(shards_api::TerminalScope::Released)) => {}
            other => {
                return Err(format!(
                    "the guest's scope ended {other:?}, not released whole"
                ));
            }
        }
        scope
            .finalize_managed(identity.as_ref())
            .map_err(|e| format!("finalizing the guest's scope: {e}"))
    }

    /// The probes of `vm`, whose record and native owner `scope` holds.
    fn probes(
        &self,
        vm: &Arc<shards_api::ManagedGuest>,
        scope: &Arc<Scope>,
    ) -> Result<(VmmCpu, VmmHolds), String> {
        let identity = shards_api::Guest::identity(vm.as_ref())
            .map_err(|e| format!("naming the guest's VM process: {e}"))?;
        let socket = identity
            .vsock
            .parent()
            .ok_or("the guest's socket has no folder")?
            .to_owned();
        Ok((
            VmmCpu {
                vm: Arc::clone(vm),
                scope: Arc::clone(scope),
                said: Said::Not("no reading yet".to_owned()),
            },
            VmmHolds {
                vm: Arc::clone(vm),
                scope: Arc::clone(scope),
                ram: super::guest::ram(&self.config)?,
                guest: None,
                folders: vec![self.cache.clone(), self.home.clone(), socket],
            },
        ))
    }
}

impl Driver for Managed {
    type Guest = Guest;
    type Cpu = VmmCpu;
    type Holds = VmmHolds;

    fn class(&self) -> String {
        hex_runtime::guest::CLASS.to_owned()
    }

    fn describe(&self) -> Result<Lines, String> {
        let mut lines = super::guest::describe(&self.config)?;
        lines.push((
            "host".to_owned(),
            format!(
                "windows {} {} logical processors",
                std::env::consts::ARCH,
                sys::active_processors()
            ),
        ));
        lines.push((
            "plan".to_owned(),
            "Shards' own (prepare_boot) with this run's runtime folders, booted --host-managed: the branch a node whose cost evidence declares no native images takes (hex_runtime::guest::managed_plan, boot_managed)".to_owned(),
        ));
        lines.push((
            "booked".to_owned(),
            format!(
                "each guest a Shards backing of shared execution granted its envelope under a provisional cost {} (its identity the digest of the lines above)",
                super::run::hex(&self.cost.id)
            ),
        ));
        Ok(lines)
    }

    fn envelope(&self) -> Result<Envelope, String> {
        super::guest::envelope(&self.config)
    }

    fn runtime(&self) -> &Path {
        &self.home
    }

    fn sources(&self) -> Lines {
        vec![
            ("cpu".to_owned(), "as its node reads it (D260): each vCPU's guest time from its VM process's answer, read first (ManagedGuest::host_use: WHP's run-time counter, TotalRuntime100ns less HypervisorRuntime100ns), none before its VM process says it (its socket not yet listening, its microVM not yet started: its guest has run nothing) and said at every reading from its first on, then its job's CPU time (WinRuntime::managed_usage: the job's kernel and user time, its VM process's and every process it starts), its whole that and, where WHP runs its vCPUs apart from its threads or its VM process hasn't told (HostUse::charged, Shards' D265), their guest time beside it, so its VMM's own, the whole less its vCPUs', is never less than it ran; a job counting less than its VM process says it used (HostUse::cpu_ns) refused, told".to_owned()),
            ("memory".to_owned(), "as its node reads it: its job's committed peak (WinRuntime::managed_usage: PeakJobMemoryUsed, at most its bound), less its guest RAM's commit, the committed private read/write region of exactly its RAM (VirtualQueryEx) in its VM process (Shards' confined VM process, as ManagedGuest::vm_process names it by pid and start), which Shards' WHP reserve_ram commits whole from the start".to_owned()),
            ("disk".to_owned(), format!(
                "the allocated bytes (FileStandardInfo's AllocationSize, a file linked twice once, by its volume and index) of every path under SHARDS_CACHE {}, SHARDS_HOME {} and the guest's socket folder",
                self.cache.display(),
                self.home.display()
            )),
        ]
    }

    fn held(&self) -> Result<Files, String> {
        walk(&[self.cache.clone(), self.home.clone()])
    }

    fn launch(&mut self) -> Result<Launched<Self>, String> {
        if self.held.is_none() {
            return Err("no census window was asked before a guest's launch".to_owned());
        }
        let (vm, scope) = self.boot()?;
        match self.probes(&vm, &scope) {
            Ok((cpu, holds)) => Ok((
                Guest {
                    vm,
                    scope,
                    client: None,
                },
                cpu,
                holds,
            )),
            // Whatever kept it from being read, the guest is ended first: none is left running.
            Err(why) => Err(match Self::end(&vm, &scope) {
                Ok(()) => why,
                Err(end) => format!("{why}; then {end}"),
            }),
        }
    }

    fn ready(&mut self, guest: &mut Guest) -> Result<(), String> {
        // Followed to its answer: its link's dials past their own W* tell the run-out once and follow the guest's VM by
        // its life, with no bound of the record's, and its hello waits on its connection (decision 9; the review's C-9).
        let vm: Arc<dyn shards_api::Guest> = Arc::clone(&guest.vm) as Arc<dyn shards_api::Guest>;
        let client = own_link(ShardsLink::new(vm, None))
            .and_then(Client::connect)
            .map_err(|why| format!("reaching the guest's agent: {why}"))?;
        guest.client = Some(client);
        Ok(())
    }

    fn hold(&mut self, guest: &Guest) -> Result<Option<Duration>, String> {
        // Ready, it is held to the census's rate for its idle hold, as a node's learner holds a VM it has booted, over
        // its own governor's period (Shards' D191: its VM process's own g), which it is read at from then on.
        let (rate, _) = self
            .held
            .as_ref()
            .ok_or("no census window was asked before a guest's hold")?;
        Self::govern(&guest.vm, *rate).map(Some)
    }

    fn idle(&mut self, guest: &Guest, until: Instant) -> Result<(), String> {
        // The hold is a wait on the guest's end, until `until`: it must not come.
        match shards_api::Guest::wait_for(
            guest.vm.as_ref(),
            until.saturating_duration_since(Instant::now()),
        ) {
            Ok(None) => Ok(()),
            Ok(Some(end)) => Err(format!("the guest ended in its idle hold: {end:?}")),
            Err(e) => Err(format!("holding the guest: {e}")),
        }
    }

    fn report(&mut self, guest: &Guest) -> Result<CapacityResources, String> {
        let client = guest
            .client
            .as_ref()
            .ok_or("the guest's agent never answered")?;
        hex_api::Runtime::capacity(client, Class::Oci)
            .map_err(|e| format!("reading the guest agent's capacity: {e}"))?
            .map(|capacity| capacity.free)
            .ok_or_else(|| "the guest's agent reports no oci capacity".to_owned())
    }

    fn tear_down(&mut self, guest: Guest) -> Result<(), String> {
        let Guest { vm, scope, client } = guest;
        drop(client);
        Self::end(&vm, &scope)
    }

    fn governed_window(&mut self, rate: u64) -> Result<Window, String> {
        let began = Instant::now();
        let (vm, scope) = self.boot()?;
        let mut guest = Guest {
            vm,
            scope,
            client: None,
        };
        // Asked once it is ready, as a node asks a VM's governor once it has booted it: its VM process takes a rate
        // only once its machine runs, which its agent's answer shows. Its boot is followed to that answer, told no
        // deadline (decision 9), and its boot-to-ready time is the window's evidence.
        let held = self.ready(&mut guest).and_then(|()| {
            let ready = began.elapsed();
            Self::govern(&guest.vm, rate).map(|period| (period, ready))
        });
        let ended = self.tear_down(guest);
        let (length, ready) = match (held, ended) {
            (Ok(held), Ok(())) => held,
            (Err(why), Ok(())) | (Ok(_), Err(why)) => return Err(why),
            (Err(why), Err(end)) => return Err(format!("{why}; then {end}")),
        };
        let window = Window {
            length,
            source: format!(
                "the period a guest's governor holds {rate} milli-CPU over, as its VM process answered it, a guest booted for it alone, told no deadline and followed to its agent's answer, ready {} ns after its launch, and asked once ready (ManagedGuest::rate and rate_period; Shards' D191, D255): the CPU control a node holds a managed VM's rate with; each guest of the census read at it through its boot, then held to the rate once ready and read at its own governor's period, its VM process's own (D191's g, which each measures itself)",
                ready.as_nanos()
            ),
        };
        self.held = Some((rate, window.clone()));
        Ok(window)
    }
}

/// Whether a guest's VM process has said its vCPUs' guest time (D260). It can't before its machine runs: its socket
/// listens as it builds its machine, before any vCPU runs, and its answer says why not until its microVM has started.
/// Its guest has run nothing until then. Once said, it is said at every reading.
enum Said {
    /// Not yet, and why.
    Not(String),
    Yes,
}

/// A launched guest's VMM's CPU, as its node reads it: its vCPUs' guest time from its VM process's answer, then its
/// job's.
pub struct VmmCpu {
    vm: Arc<shards_api::ManagedGuest>,
    scope: Arc<Scope>,
    said: Said,
}

impl Probe for VmmCpu {
    type Reading = Use;

    fn read(&mut self) -> Result<Use, String> {
        // D260's order: what its vCPUs ran of its guest first, then its job's whole, so the whole less the vCPUs' is
        // never less than none.
        let asked =
            |why: &dyn std::fmt::Display| format!("asking the guest's VM process its use: {why}");
        let used = match self.vm.host_use(Duration::MAX) {
            Ok(shards_api::ManagedUsed::Running(used)) => Some(used),
            // Its end (Shards' D260), in the words its refusal of the ask said it before.
            Ok(shards_api::ManagedUsed::Ended(ended)) => {
                return Err(asked(&shards_api::Refusal::Ended(ended.end)));
            }
            Err(shards_api::Refusal::ManagedIo {
                kind: std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound,
                ..
            }) if matches!(self.said, Said::Not(_)) => None,
            Err(e) => return Err(asked(&e)),
        };
        let times = match used.as_ref().map(|used| &used.microvm.guest) {
            Some(Ok(times)) => {
                self.said = Said::Yes;
                Some(times)
            }
            Some(Err(why)) if matches!(self.said, Said::Not(_)) => {
                self.said = Said::Not(why.clone());
                None
            }
            Some(Err(why)) => {
                return Err(format!(
                    "the guest's VM process stopped saying its guest time: {why}"
                ));
            }
            None => {
                self.said = Said::Not("its socket didn't listen yet".to_owned());
                None
            }
        };
        let usage = self
            .scope
            .managed_usage()
            .map_err(|e| format!("reading the guest's job: {e}"))?;
        // Its VM process's own count, read before its job's: the job, which holds that process, counts no less.
        if let Some(Ok(host)) = used.as_ref().map(|used| &used.microvm.host)
            && usage.cpu_usec < host.cpu_ns / 1000
        {
            return Err(format!(
                "the guest's job counts {} us of CPU, less than its VM process says it used ({} ns)",
                usage.cpu_usec, host.cpu_ns
            ));
        }
        let guest_cpu = times.map_or(Duration::ZERO, |times| {
            Duration::from_nanos(times.total_ns())
        });
        let job = Duration::from_micros(usage.cpu_usec);
        // Whether its job's CPU holds its vCPUs' guest time (Shards' D265): it does where WHP runs them on its threads
        // (Windows' root scheduler); where it runs them apart (the classic and core schedulers, every Windows Server's)
        // or its VM process hasn't told, its whole is its job's and its vCPUs' beside it, so its VMM's own is never read
        // as less than it ran.
        let charged = used
            .as_ref()
            .and_then(|used| used.microvm.host.as_ref().ok())
            .map_or(shards_api::image::Charged::Unknown, |host| host.charged);
        let cpu = match charged {
            shards_api::image::Charged::ToThread => job,
            shards_api::image::Charged::Apart | shards_api::image::Charged::Unknown => job
                .checked_add(guest_cpu)
                .ok_or("the guest's CPU overflows a duration")?,
        };
        Ok(Use {
            cpu,
            guest_cpu,
            guest_threads: times
                .map(|times| u32::try_from(times.vcpus.len()))
                .transpose()
                .map_err(|e| e.to_string())?,
        })
    }

    /// After its last reading, its guest ready and held: its VM process has said its guest's time.
    fn verify(&mut self) -> Result<(), String> {
        match &self.said {
            Said::Yes => Ok(()),
            Said::Not(why) => Err(format!(
                "the guest's VM process never said its guest time, though its guest was ready: {why}"
            )),
        }
    }
}

/// What a launched guest's VMM holds, as its node reads it: its job's committed peak, its guest RAM's commit (its VM
/// process's committed region of that size), and its folders.
pub struct VmmHolds {
    vm: Arc<shards_api::ManagedGuest>,
    scope: Arc<Scope>,
    ram: u64,
    /// Its VM process, which maps its guest's RAM, once that RAM is found there, and that RAM's range in it.
    guest: Option<Mapped>,
    folders: Vec<PathBuf>,
}

/// A process that maps a guest's RAM, and that RAM's range in it.
type Mapped = (Handle, (u64, u64));

/// `process`'s address space region by region (`VirtualQueryEx`), each with whether it is committed private read/write
/// memory, as its guest RAM is.
fn entries(process: &Handle) -> Result<Vec<(u64, u64, bool)>, String> {
    let mut entries = Vec::new();
    let mut at = 0u64;
    while let Some(region) =
        sys::region(process, at).map_err(|e| format!("reading a VM process's memory map: {e}"))?
    {
        let end = region
            .base
            .checked_add(region.size)
            .filter(|end| *end > at)
            .ok_or("a VM process's memory map doesn't advance")?;
        entries.push((region.base, end, region.private_rw));
        at = end;
    }
    Ok(entries)
}

impl VmmHolds {
    /// Its guest's RAM: the one region of exactly its size in its VM process's map, Shards' confined VM process
    /// (its `reserve_ram`), as `ManagedGuest::vm_process` names it by pid and start (Shards 9d8a05f6a); none while that
    /// process isn't recorded yet (its `shards` records it as it starts it, before it runs) or has no such region yet,
    /// looked at again at the next reading. The process is named again once its handle is open, so the handle is the one
    /// named, never another that took its pid.
    fn find(&self) -> Result<Option<Mapped>, String> {
        let named = match self.vm.vm_process() {
            Ok(named) => named,
            Err(shards_api::Refusal::VmProcess(shards_api::VmProcessRefusal::Unrecorded)) => {
                return Ok(None);
            }
            Err(e) => return Err(format!("naming the guest's VM process: {e}")),
        };
        let process = sys::open_for_reading(named.pid)
            .map_err(|e| format!("opening the guest's VM process {}: {e}", named.pid))?
            .ok_or_else(|| format!("the guest's VM process {} is gone", named.pid))?;
        let again = self
            .vm
            .vm_process()
            .map_err(|e| format!("naming the guest's VM process again: {e}"))?;
        if again != named {
            return Err(format!(
                "the guest's VM process was {named:?} and is now {again:?}"
            ));
        }
        Ok(guest_range(&entries(&process)?, self.ram)?.map(|range| (process, range)))
    }
}

impl Probe for VmmHolds {
    type Reading = Holding;

    fn read(&mut self) -> Result<Holding, String> {
        let usage = self
            .scope
            .managed_usage()
            .map_err(|e| format!("reading the guest's job: {e}"))?;
        if self.guest.is_none() {
            self.guest = self.find()?;
        }
        Ok(Holding {
            footprint: usage.memory_bytes,
            file_pages: None,
            peak_footprint: Some(usage.memory_peak_bytes),
            guest_memory: self
                .guest
                .as_ref()
                .map(|(_, (start, end))| end.saturating_sub(*start)),
            files: walk(&self.folders)?,
        })
    }

    /// The map of the member it was found in read whole again: the guest's RAM is still the one region found, so no
    /// reading took another for it.
    fn verify(&mut self) -> Result<(), String> {
        let Some((process, range)) = &self.guest else {
            return Ok(());
        };
        let now = guest_range(&entries(process)?, self.ram)?;
        if now != Some(*range) {
            return Err(format!(
                "the guest's RAM was read at {range:x?} and is now at {now:x?}"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::run::{Plan, Windows, produce};
    use super::*;

    /// AC-T7.4.1 (light smoke, few boots), Windows' managed driver natively under WHP: produce a pair over the
    /// configured Shards guest with SEEDABLE two, through the same census, evidence and output the measurement uses:
    /// the governed window's guest booted, held and ended, then two guests each launched, read from their launch,
    /// held at the census's rate once ready and read from then on at their own governor's period, and torn down. Expect
    /// the pair written and loaded as a node loads it, its envelope the configured guest's, its window the governor's
    /// period, two guests in its evidence, and no guest
    /// leaving anything on disk: its native owner releases the AppContainer profiles and records its VM process made
    /// as it ends its scope (Shards' D141, shards_api::release_app_containers), which the census would refuse. Needs
    /// SHARDS_BIN, HEX_GUEST_KERNEL, HEX_GUEST_INITRD, HEX_GUEST_CPUS, HEX_GUEST_MEMORY_MIB and GUEST_COST_SMOKE_OUT (a
    /// folder of the run's own under the user's profile).
    #[test]
    #[ignore = "boots three real guests under WHP: run with --ignored where a Shards guest is configured"]
    fn two_managed_guests_make_a_pair_the_node_loads() {
        let out =
            PathBuf::from(std::env::var_os("GUEST_COST_SMOKE_OUT").expect("GUEST_COST_SMOKE_OUT"));
        let mut driver = Managed::from_env(&out).unwrap();
        let envelope = driver.envelope().unwrap().resources;
        let made = produce(
            &mut driver,
            &Plan { seedable: 2 },
            &out,
            &Windows::Governed,
            &[],
        )
        .unwrap();
        let (cost, _, rows) = hex_runtime::guest::load_runtime_evidence(
            &made.cost_file,
            made.cost_sha256,
            &made.evidence_file,
        )
        .unwrap();
        assert!(rows.is_empty(), "{rows:?}");
        assert_eq!((cost.floor, cost.ceiling), (envelope, envelope));
        let text = std::fs::read_to_string(&made.evidence_file).unwrap();
        assert!(
            text.lines()
                .any(|l| l.starts_with("window ") && l.contains("governor")),
            "{text}"
        );
        assert_eq!(text.lines().filter(|l| l.starts_with("guest ")).count(), 2);
        // Its native owner released each guest's AppContainer profiles with its scope, the window's guest's too: no
        // record of one is left in its cache.
        let records = out.join("runtime").join("cache").join("containers");
        let left: Vec<_> = std::fs::read_dir(&records)
            .map(|dir| dir.flatten().map(|entry| entry.file_name()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "{left:?} left in {records:?}");
        println!("{}\n{text}", made.summary);
    }
}
