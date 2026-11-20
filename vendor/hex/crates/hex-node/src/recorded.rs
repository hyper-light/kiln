//! A node over any OS's runtime (native plan step 8): each box's record kept durably in the node's state
//! directory beside the runtime that makes the box ([`crate::store`], the records Linux's node keeps), and the
//! programs the socket's disconnect rule ends counted into it ([`crate::abandoned`]), so a node served on
//! any OS's socket ([`crate::serve`]) answers as Linux's does. Linux's node ([`crate::Node`]) keeps the same
//! records and counts through the same code, with its cgroups and containers beside them; macOS's node is
//! this over its runtime (the `process` class, and Linux images in its Shards guest).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use hex_api::{
    Ask, BoxId, BoxInfo, BoxSpec, Class, DeviceId, Proc, Program, Refusal, Runtime, Signal, Stdio,
    Usage,
};
use hex_core::netns;
use hex_core::record::{BoxRecord, NativeRecord, NativeStage, Phase, PoolRecord, ProcId};
use hex_core::recovery::{self, Decision, Parts, Seen, Why};

use crate::abandoned::Abandoned;
use crate::serve::{self, Forward, Forwarded, Host, Loop, Reply};
use crate::store::Store;
use crate::turns::{Hold, Turn, Turns};
use hex_core::wire::{Answer, Call};

/// What a recovery did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoverySummary {
    /// Boxes taken back as they were.
    pub adopted: Vec<BoxId>,
    /// Boxes removed, each with why.
    pub removed: Vec<(BoxId, String)>,
    /// Slots removed that no box's record named: idle when the agent stopped, or a create's caught
    /// before its record (goal 5 extended, step 6). Linux's node's alone: no other OS's hands a box's
    /// cgroup on.
    pub slots_removed: Vec<u64>,
    /// Boxes whose record was torn and whose end nothing proved, each with why: the record kept, so the
    /// name is never reused and the box stays uncertain (T-T7.1.2).
    pub held: Vec<(BoxId, String)>,
    /// Why the guest couldn't say which boxes it holds, when it couldn't: none was proved an orphan, so
    /// none was deleted, and a box no record names waits for the next recovery (T-T7.1.2).
    pub orphans_unknown: Option<String>,
    /// Why the host's boot session couldn't be read, when it couldn't: no record was known for an earlier
    /// boot's, and each keeps recovery's other rules (T-T7.1.2).
    pub boot_unknown: Option<String>,
    /// The node VM rows an earlier node process left, each by its run with what this start did with it (joint plan
    /// step 39).
    pub node_vms: Vec<(hex_api::NodeRun, crate::store::NodeVmSettled)>,
    /// The last-resort ends those rows kept, carried into the charge learner's ends; none on record told as such.
    pub node_vm_ends: crate::store::NodeVmEnds,
}

/// Why a native owner that runs no node VM refuses `call` about node VM `run` (joint plan step 39).
fn no_node_vm(run: hex_api::NodeRun, call: &str) -> Refusal {
    let name: String = run
        .bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Refusal::Invalid {
        what: "node VM",
        why: format!("run {name}: this native owner runs no node VM, so it has no {call} of one"),
    }
}

/// Why recovery keeps a Shards image box's record and claim: its run is an earlier node process's, which
/// Shards' image boot can neither take back nor prove ended (docs/design/shards-image-class.md §5).
pub(crate) const IMAGE_HELD: &str = "a Shards image VM's run of an earlier node process: Shards' image \
    boot can neither take it back nor prove it ended (Awaits AttachAfterCrash), so its record and claim \
    are kept until a reboot of the host ends it";

/// Why a box was removed, for its summary.
pub(crate) fn why_text(why: &Why) -> String {
    match why {
        Why::MidCreate => "the node agent stopped while it was being made".to_owned(),
        Why::MidDelete => "the node agent stopped while it was being removed".to_owned(),
        Why::InitGone => "its container's init had ended".to_owned(),
        Why::RunEnded => "the program it was run for had ended".to_owned(),
        Why::NoCgroup => "its record had no cgroup".to_owned(),
        Why::NoRecord(parts) => {
            let left: Vec<&str> = [
                (parts.cgroup, "its cgroup"),
                (parts.state_dir, "its container's state"),
                (parts.bundle, "its bundle"),
            ]
            .into_iter()
            .filter(|(found, _)| *found)
            .map(|(_, what)| what)
            .collect();
            format!(
                "no record named it, made only in part: {} left",
                left.join(", ")
            )
        }
        Why::Unreadable(w) => format!("its record couldn't be read: {w}"),
        Why::StartInterrupted => {
            "the node agent stopped while it started the box, in its startContainer hooks"
                .to_owned()
        }
        Why::EarlierBoot => {
            "it was made in an earlier boot of the host, whose end ended everything that ran of it"
                .to_owned()
        }
    }
}

/// Why a torn record was held, for its summary: its file, why it couldn't be read, why nothing proved its
/// box ended, and what to do (T-T7.1.2).
fn held_text(path: &Path, why: &str, because: &str) -> String {
    format!(
        "its record {} couldn't be read ({why}) and {because}: kept, so its name isn't reused; once \
         nothing of the box runs, move the file aside",
        path.display()
    )
}

pub(crate) mod charges;

/// What a node needs of its runtime to take its boxes back after a crash (T-T7.1.2).
pub trait Recover: Runtime {
    /// Pure initialized encoding forms appended to the SAME transient global tuple union. A native
    /// component may explicitly reserve conservative possible forms, never fresh eligibility.
    /// No component calls capacity/Books/native I/O while the publication fence holds.
    fn project_routes(
        &self,
        pools: &[PoolRecord],
        introduced: Option<(hex_api::Execution, &[DeviceId])>,
        projection: &mut hex_runtime::devices::RouteProjection,
    ) -> Result<(), Refusal> {
        let _ = (pools, introduced, projection);
        Err(broken("runtime has no initialized route projection"))
    }

    /// Check the union once, including ordinary host prefixes and every initialized VM alias.
    fn check_route_projection(
        &self,
        pools: &[PoolRecord],
        introduced: Option<(hex_api::Execution, &[DeviceId])>,
    ) -> Result<(), Refusal> {
        if pools.len() > hex_runtime::MAX_BOXES {
            return Err(Refusal::Full(hex_runtime::MAX_BOXES));
        }
        for pool in pools {
            pool.check()?;
        }
        let mut projection = hex_runtime::devices::RouteProjection::new();
        self.project_routes(pools, introduced, &mut projection)?;
        projection.count()?;
        Ok(())
    }

    /// A view of the SAME initialized immutable catalog, never another load or holder owner.
    fn device_catalog(&self) -> Result<Option<hex_runtime::devices::SharedCatalog>, Refusal> {
        Ok(None)
    }

    /// Actual initialized VM observations. Ordinary native hosts add none; the direct SAME Prober
    /// report remains required. This never calls capacity/physical_report or opens another provider.
    fn physical_observations(&self) -> Result<Vec<hex_api::PhysicalDevice>, Refusal> {
        Ok(Vec::new())
    }

    /// Register a weak view of the SAME canonical owner after the existing Recorded Arc exists.
    /// A native persistent owner may read records/pools for its restart fence, never capacity or
    /// callbacks; a runtime without one keeps nothing.
    fn bind_native_books(
        &self,
        books: std::sync::Weak<dyn hex_runtime::devices::Books>,
    ) -> Result<(), Refusal> {
        let _ = books;
        Ok(())
    }

    /// A pending/cancelled source handoff has no boot Intent. This is metadata validation only,
    /// never scope termination, permission to adopt a Guest, or release from absence alone.
    fn handoff_unsubmitted(&self, record: &BoxRecord, pool: &PoolRecord) -> Result<(), Refusal> {
        let _ = pool;
        Err(uncertain(
            &record.spec.id,
            "this runtime holds no native backing to hand it to",
        ))
    }

    /// Native half of the same pending no-Intent check. Concrete owners must prove that no
    /// conflicting submission receipt is retained; missing cold proof remains typed uncertainty.
    fn preflight_unsubmitted(&self, record: &BoxRecord) -> Result<(), Refusal> {
        Err(uncertain(
            &record.spec.id,
            "this runtime holds no native backing to prove it unsubmitted",
        ))
    }

    /// Read-only initialized configuration and retained metadata validation, before ANY owner is
    /// recovered. No capacity sample, GPU probe, native mutation, publication or cleanup belongs here.
    fn preflight_record(&self, record: &BoxRecord) -> Result<(), Refusal> {
        record.spec.check()?;
        if !self.classes().contains(&record.spec.backend_class()?) {
            return Err(uncertain(
                &record.spec.id,
                "this node serves no runtime of its record's class",
            ));
        }
        Ok(())
    }

    /// An unknown backing configuration retains its grant; absence of an adapter is not cleanup.
    fn preflight_pool(&self, record: &PoolRecord) -> Result<(), Refusal> {
        Err(NativeRecord::Pool(record)
            .uncertain("its node has no native owner to preflight its pool"))
    }

    /// The SAME native owner's scope/receipt metadata, without attaching or adopting that scope.
    fn preflight_managed(&self, record: NativeRecord<'_>) -> Result<(), Refusal> {
        Err(record.uncertain("its node has no native owner to preflight its managed scope"))
    }

    /// Read-only cross-check of native metadata names against the complete retained cohort. Every
    /// runtime implements its own, one with no native metadata saying so: left to this default,
    /// recovery is refused rather than go on unchecked (AC-T7.4.1).
    fn preflight_retained(
        &self,
        records: &hex_runtime::devices::Records,
        pools: &[PoolRecord],
    ) -> Result<(), Refusal> {
        let _ = (records, pools);
        Err(broken(
            "runtime has no cross-check of its native metadata against the retained cohort",
        ))
    }

    /// Deferred native initialization is entered only after the complete read-only barrier.
    fn initialize_native(&self) -> Result<(), Refusal> {
        Ok(())
    }

    /// Launches within this same native owner's envelope. The process remains suspended until
    /// `publish` durably introduces its immutable identity in the canonical Creating record.
    fn launch_managed(
        &self,
        record: NativeRecord<'_>,
        plan: &hex_runtime::devices::ManagedLaunch<'_>,
        io: shards_api::LaunchIo,
        patience: Option<std::time::Duration>,
        publish: &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        let _ = (plan, io, patience, publish);
        Err(record.uncertain("its node has no native owner to launch it"))
    }

    fn attach_managed(
        &self,
        record: NativeRecord<'_>,
        identity: Option<&shards_api::Identity>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        let _ = identity;
        Err(record.uncertain("its node has no native owner to attach it"))
    }

    /// Called only after the selected VM receipt durably records positive whole-scope release.
    fn finalize_managed(
        &self,
        record: NativeRecord<'_>,
        identity: Option<&shards_api::Identity>,
    ) -> Result<(), Refusal> {
        let _ = identity;
        Err(record.uncertain("its node has no native owner to finalize it"))
    }

    fn managed_usage(&self, record: NativeRecord<'_>) -> Result<Usage, Refusal> {
        Err(record.uncertain("its node has no native owner to read its usage"))
    }

    /// Joint plan step 14 (C1; ROOT, 2026-11-13; 2c-4b): the cycle this owner's cap holds a managed VM at `rate` over,
    /// derived from how the platform holds a VM's CPU rate, which the charge learner's window takes for each VM that cap
    /// holds: never at a rate it never holds a VM below, and cold, told, until what derives it has been measured.
    /// Refused, typed, naming what is missing where nothing derives it.
    fn cap_cycle(&self, rate: u64) -> Result<hex_runtime::guest::CapCycle, Refusal> {
        let _ = rate;
        Err(Refusal::Platform {
            what: "a managed VM's cap's cycle",
            platform: std::env::consts::OS,
            why: "this native owner holds no managed VM's CPU rate over a cycle it derives"
                .to_owned(),
        })
    }

    /// Joint plan step 14 (ROOT, 2026-11-13): retires what its node recorded under a profile's census cost (each
    /// backing and VM box, its boxes kept retired), run once at its node's start after native initialization, before
    /// any preparation or recovery uses them; none where it keeps no such record.
    fn retire_census(&self) -> Result<(), Refusal> {
        Ok(())
    }

    /// Joint plan step 14 (C1): the period Shards image VM `record`'s rate holds over (Shards' D247, its hold D191's
    /// governor), its run's own as its VM said it with its last rate's answer, none before its first (Shards' image
    /// channel v31): the window the charge learner caps it through its rate over, read each window, never a number of
    /// Hex's. Refused, typed, by a native owner that runs no image VM.
    fn image_rate_period(
        &self,
        record: &BoxRecord,
    ) -> Result<Option<std::time::Duration>, Refusal> {
        Err(Refusal::Invalid {
            what: "rate_period",
            why: format!(
                "box {} isn't a Shards image VM this native owner runs: only a guest's image VM holds a rate over its \
                 run's period",
                record.spec.id
            ),
        })
    }

    /// Joint plan step 39 (place 3): what node VM `run` costs its host, its run's microVM part (D241), read as an image
    /// VM's is. Refused, typed, by a native owner that runs no node VM.
    fn node_vm_host_use(&self, run: hex_api::NodeRun) -> Result<hex_api::HostUse, Refusal> {
        Err(no_node_vm(run, "host_use"))
    }

    /// Joint plan step 39 (place 3): the period node VM `run`'s governor holds its rate over (D191, D247), its run's own
    /// as its VM said it with its last rate's answer, none before its first (Shards' image channel v31). Refused, typed,
    /// by a native owner that runs no node VM.
    fn node_vm_rate_period(
        &self,
        run: hex_api::NodeRun,
    ) -> Result<Option<std::time::Duration>, Refusal> {
        Err(no_node_vm(run, "rate_period"))
    }

    /// Joint plan step 39 (place 3): holds node VM `run`'s vCPUs together to `rate` milli-CPU through its run's rate
    /// (D247), as an image VM's are held. Refused, typed, by a native owner that runs no node VM.
    fn node_vm_rate(
        &self,
        run: hex_api::NodeRun,
        rate: Option<std::num::NonZeroU64>,
    ) -> Result<Option<std::num::NonZeroU64>, Refusal> {
        let _ = rate;
        Err(no_node_vm(run, "rate"))
    }

    /// Joint plan step 39 (place 3): stops node VM `run`'s run, the charge learner's last resort (C5), its end kept in
    /// its row. Refused, typed, by a native owner that runs no node VM.
    fn stop_node_vm(&self, run: hex_api::NodeRun) -> Result<(), Refusal> {
        Err(no_node_vm(run, "stop"))
    }

    /// Joint plan step 39: tells node VM `run`'s guest its room, `room` bytes past what it holds (D188), as an image VM's
    /// guest is told it. Refused, typed, by a native owner that runs no node VM.
    fn node_vm_room(&self, run: hex_api::NodeRun, room: i64) -> Result<(), Refusal> {
        let _ = room;
        Err(no_node_vm(run, "room"))
    }

    /// Joint plan step 14 (D255): asks the governor of managed VM `record` to hold its vCPUs to `rate` milli-CPU, through
    /// the guest this owner boots it in (`GuestRuntime::managed_rate`). An owner that boots no managed guest has no
    /// governor to ask: its platform's cap is to hold the VM, told why.
    fn managed_rate(
        &self,
        record: &BoxRecord,
        rate: u64,
    ) -> Result<hex_runtime::guest::ManagedRate, Refusal> {
        let _ = (record, rate);
        Ok(hex_runtime::guest::ManagedRate::Platform(
            Refusal::Platform {
                what: "a managed VM's governor",
                platform: std::env::consts::OS,
                why: "this native owner boots no managed guest whose governor it asks".to_owned(),
            },
        ))
    }

    /// Joint plan step 14 (D255): the period managed VM `record`'s governor holds its rate over, as its VM process last
    /// answered: none before its first answer, and none from an owner that boots no managed guest.
    fn managed_rate_period(
        &self,
        record: &BoxRecord,
    ) -> Result<Option<std::time::Duration>, Refusal> {
        let _ = record;
        Ok(None)
    }

    /// Joint plan step 14 (D260): managed VM `record`'s vCPUs' guest time, as its VM process answers its use, through
    /// the guest this owner boots it in (`GuestRuntime::managed_guest_time`). An owner that boots no managed guest reads
    /// none apart, told why.
    fn managed_guest_time(
        &self,
        record: &BoxRecord,
    ) -> Result<hex_runtime::guest::ManagedGuestTime, Refusal> {
        let _ = record;
        Ok(hex_runtime::guest::ManagedGuestTime::Unheld(
            "this native owner boots no managed guest whose use it asks".to_owned(),
        ))
    }

    /// Joint plan step 14 (D260): how long managed VM `record`'s VM process has run, from its start as its record names
    /// it (`init`, on its OS's own clock) to now. Refused, typed, by an owner that holds no managed VM.
    fn managed_life(&self, record: &BoxRecord) -> Result<std::time::Duration, Refusal> {
        let _ = record;
        Err(Refusal::Platform {
            what: "a managed VM's life",
            platform: std::env::consts::OS,
            why: "this native owner holds no managed VM".to_owned(),
        })
    }

    /// Joint plan step 14 (C4; ROOT, 2026-11-13): the least share this owner holds a managed VM at, its actuator's least
    /// unit: macOS's CPU control's, Windows' job's; an owner with no cap of its own holds none below its VM's governor's
    /// ([`charges::GOVERNOR_LEAST`]).
    fn managed_least(&self) -> u64 {
        charges::GOVERNOR_LEAST.get()
    }

    /// Joint plan step 14 (C4): caps the managed VM's CPU at `rate` milli-CPU, its share as the charge learner
    /// water-fills it, where this owner caps CPU rather than weighs it; refused, typed, by an owner that holds no rate.
    fn set_managed_rate(&self, record: NativeRecord<'_>, rate: u64) -> Result<(), Refusal> {
        let _ = (record, rate);
        Err(Refusal::Platform {
            what: "a managed VM's CPU rate",
            platform: std::env::consts::OS,
            why: "this native owner holds no managed VM's CPU rate".to_owned(),
        })
    }

    /// Joint plan step 14 (C5): ends the managed VM through its receipt, the charge learner's last resort, its end then
    /// observed as any other; refused, typed, by an owner that holds no managed VM.
    fn stop_managed(&self, record: NativeRecord<'_>) -> Result<(), Refusal> {
        let _ = record;
        Err(Refusal::Platform {
            what: "a managed VM's last resort",
            platform: std::env::consts::OS,
            why: "this native owner holds no managed VM to end".to_owned(),
        })
    }

    /// Initialized native routes only; physical claims are overlaid from this owner's Store.
    fn attachment_offers(&self) -> Result<Vec<hex_api::DeviceInfo>, Refusal> {
        Ok(self
            .capacity(Class::Oci)?
            .map_or_else(Vec::new, |capacity| capacity.devices))
    }
    /// An initialized backend that enters this same Store/Turns authority itself. The outer
    /// Recorded wrapper must not write another record or acquire a reentrant method turn.
    fn owns_records(&self, spec: &BoxSpec) -> Result<bool, Refusal> {
        let _ = spec;
        Ok(false)
    }
    /// The persisted class chooses the backend before any cleanup or native operation.
    fn runtime_for(&self, spec: &BoxSpec) -> Result<&dyn Runtime, Refusal>
    where
        Self: Sized,
    {
        let _ = spec;
        Ok(self)
    }
    /// What names box `id`'s processes after the node is gone, recorded with the box once it has any (its
    /// first member started): a process and its start time, which tells it from one given its pid since.
    fn trace(&self, id: &BoxId) -> Result<Option<ProcId>, Refusal>;
    /// Takes back box `spec`, its processes the ones `trace` names where they still run.
    fn adopt(&self, spec: &BoxSpec, trace: Option<ProcId>) -> Result<(), Refusal>;
    /// Ends what still runs of what `trace` names, for a box recovery removes.
    fn end(&self, trace: Option<ProcId>) -> Result<(), Refusal>;
    fn end_for(&self, spec: &BoxSpec, trace: Option<ProcId>) -> Result<(), Refusal> {
        let _ = spec;
        self.end(trace)
    }
    /// For an `oci` box: whether its init still runs, so recovery takes it back; `None` for a class with
    /// no init. By default an `oci` box's init went with the node.
    fn alive(&self, spec: &BoxSpec) -> Result<Option<bool>, Refusal> {
        Ok(match spec.class.class() {
            Class::Oci | Class::Firecracker | Class::Shards | Class::ShardsImage => Some(false),
            Class::Process => None,
        })
    }
    /// Removes what is left of box `spec` that `trace` doesn't name, for a box recovery removes. By
    /// default there is nothing.
    fn discard(&self, spec: &BoxSpec) -> Result<(), Refusal> {
        let _ = spec;
        Ok(())
    }
    /// The boxes a live guest's node holds under names no readable record of `records` carries: orphans
    /// (T-T7.1.2). A runtime with no guest has none.
    fn orphans(&self, records: &hex_runtime::devices::Records) -> Result<Vec<BoxId>, Refusal> {
        let _ = records;
        Ok(Vec::new())
    }
    /// Deletes orphan `id` ([`Recover::orphans`]) through its guest. A runtime with no guest has none to
    /// delete, so it refuses.
    fn delete_orphan(&self, id: &BoxId) -> Result<(), Refusal> {
        Err(uncertain(
            id,
            "its delete found no record, and only a record's cleanup is proved",
        ))
    }
    /// The core's loop, for connections of its own ([`Host::attach`]). By default it has none.
    fn attach(&self, lp: Arc<dyn Loop>) -> Result<(), Refusal> {
        let _ = lp;
        Ok(())
    }
    /// A call it answers later ([`Host::forward`]). By default every call runs on the worker.
    fn forward(&self, call: Call, stdio: Stdio, reply: Reply, ended: serve::Ended) -> Forward {
        let _ = (reply, ended);
        Forward::Back(Box::new((call, stdio)))
    }
    fn forward_for(
        &self,
        spec: &BoxSpec,
        call: Call,
        stdio: Stdio,
        reply: Reply,
        ended: serve::Ended,
    ) -> Forward {
        let _ = spec;
        self.forward(call, stdio, reply, ended)
    }
}

/// A runtime whose boxes' records the node keeps.
pub struct Recorded<R> {
    runtime: R,
    books: Arc<Books>,
    /// Its lessors over its wire, where its image VMs lease through them ([`Recorded::lease_over_wire`]).
    wire_lessors: std::sync::OnceLock<Arc<crate::serve::WireLessors>>,
}

/// The node's records of its boxes, shared with the completions of calls its guest answers.
struct Books {
    store: Store,
    /// The CLI/Service's same native provider allocation; never opened by a guest backend.
    prober: Option<crate::facts::SharedProber>,
    /// Active create/publication or delete/removal methods, not a second reservation authority.
    lifecycle: Turns,
    abandoned: Abandoned,
    /// Boxes a recovery took back, which inspect reports as adopted.
    adopted: Mutex<BTreeSet<BoxId>>,
    /// The boot session the store's records were written in before this node opened it, as its marker said
    /// ([`Store::take_boot_marker`]): what tells a torn record from an earlier boot (T-T7.1.2).
    last_boot: Option<hex_core::record::BootSession>,
    /// Joint plan step 14: the charge learner of the node's VMs, where its platform caps them, and the sender its
    /// thread waits on beside its window: told each box's beginning (`Recorded::begun`), and dropped with the node.
    charges: std::sync::OnceLock<(
        Mutex<charges::Learner>,
        Mutex<hex_wait::mpsc::SyncSender<()>>,
    )>,
    /// Joint plan step 39, place 1: the last-resort ends a recovery read back from the node VM rows it let go, kept for
    /// the charge learner, which starts after it ([`Recorded::start_charges`]); at most the rows a store scans.
    node_vm_ends: Mutex<Vec<hex_api::LearnedEnd>>,
    /// Whether the machine's capacity view serves this node's machine, as its configuration says (`--capacity-view`; the
    /// review's A-10): its keeper's word is then awaited before its charge learner learns ([`Recorded::set_capacity_view`]).
    capacity_view: std::sync::atomic::AtomicBool,
    /// The machine's capacity view's latest word, what every holder has reserved through its keeper (the review's A-10),
    /// kept for the charge learner: none until its keeper speaks ([`Runtime::reserved`]).
    reserved: Mutex<Option<hex_api::Reserved>>,
}

/// Concrete shared transaction; its caller keeps the same method/device Turn on stack.
struct Transaction<'a> {
    store: &'a Store,
    owner: TransactionOwner<'a>,
    devices: &'a [DeviceId],
    native: &'a Native<'a>,
}

#[derive(Clone, Copy)]
enum TransactionOwner<'a> {
    Box(&'a BoxId),
    Handoff {
        id: &'a BoxId,
        source: &'a hex_api::BackingKey,
    },
    Pool(&'a hex_api::BackingKey),
}

type ManagedReceipt = shards_api::ManagedReceipt;
type NativeLaunch<'a> = dyn Fn(
        NativeRecord<'_>,
        &hex_runtime::devices::ManagedLaunch<'_>,
        shards_api::LaunchIo,
        Option<std::time::Duration>,
        &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>,
    ) -> Result<ManagedReceipt, Refusal>
    + 'a;
type NativeAttach<'a> =
    dyn Fn(NativeRecord<'_>, Option<&shards_api::Identity>) -> Result<ManagedReceipt, Refusal> + 'a;
type NativeFinalize<'a> =
    dyn Fn(NativeRecord<'_>, Option<&shards_api::Identity>) -> Result<(), Refusal> + 'a;
type NativeUsage<'a> = dyn Fn(NativeRecord<'_>) -> Result<Usage, Refusal> + 'a;
type NativeUnsubmittedPool<'a> = dyn Fn(NativeRecord<'_>) -> Result<(), Refusal> + 'a;

/// Borrowed callbacks into Node's or Recorded's existing native owner. They remain on the same
/// synchronous transaction stack, with no map lock, boxed transaction or second holder owner.
pub(crate) struct Native<'a> {
    pub catalog: Option<&'a hex_runtime::devices::Catalog>,
    pub launch: &'a NativeLaunch<'a>,
    pub attach: &'a NativeAttach<'a>,
    pub finalize: &'a NativeFinalize<'a>,
    pub usage: &'a NativeUsage<'a>,
    pub unsubmitted_pool: &'a NativeUnsubmittedPool<'a>,
}

/// Box `id`'s lifecycle uncertain, in the refusing site's own words (joint plan step 12).
fn uncertain(id: &BoxId, why: &str) -> Refusal {
    match id.try_clone() {
        Ok(id) => Refusal::Lifecycle {
            id,
            cause: hex_api::LifecycleCause::Uncertain,
            why: Some(why.to_owned()),
        },
        Err(error) => error,
    }
}

/// Joint plan step 14 (D260): the start of managed VM `record`'s VM process, as its record names it (`init`, on its OS's
/// own clock, Shards' `Identity::started`): refused, typed, for a record that names none.
#[cfg(any(target_os = "macos", windows))]
fn started(record: &BoxRecord) -> Result<u64, Refusal> {
    record
        .init
        .map(|init| init.start_ticks)
        .ok_or_else(|| Refusal::Invalid {
            what: "a managed VM's life",
            why: format!("box {}'s record names no VM process", record.spec.id),
        })
}

fn broken(message: &'static str) -> Refusal {
    let mut why = String::new();
    if why.try_reserve_exact(message.len()).is_err() {
        return Refusal::Exhausted {
            what: "recorded refusal bytes",
            bound: message.len(),
        };
    }
    why.push_str(message);
    Refusal::Broken(why)
}

/// A native-only component owns no VM routes. WithGuest projects those separately using the
/// same collector; a missing initialized adapter must not silently discard retained VM tuples.
#[cfg(any(target_os = "macos", windows, test))]
fn check_native_projection(
    pools: &[PoolRecord],
    introduced: Option<(hex_api::Execution, &[DeviceId])>,
) -> Result<(), Refusal> {
    if let Some(pool) = pools.first() {
        return Err(NativeRecord::Pool(pool)
            .uncertain("a native-only runtime holds a VM backing it can't project"));
    }
    if introduced.is_some() {
        return Err(broken("native runtime cannot project a VM backing"));
    }
    Ok(())
}

/// One retained route joins its SAME canonical source generation/profile. A reserved/cancelled
/// dedicated Box cannot obtain native permission from a source still Live/Draining. Adapter
/// preflight must additionally prove exact no-Intent for the unsubmitted cancellation shape.
pub(crate) fn check_backing_reference(
    record: &BoxRecord,
    pool: &PoolRecord,
) -> Result<(), Refusal> {
    let bad = |why: &str| uncertain(&record.spec.id, why);
    let route = record
        .spec
        .route
        .ok_or_else(|| bad("its record routes it on no backing"))?;
    if route.backing != Some(pool.key)
        || route.key.backend_class() != NativeRecord::Pool(pool).class()?
        || pool.phase == hex_api::BackingPhase::Prepared
    {
        return Err(bad(
            "its route names another backing, another class, or one still only prepared",
        ));
    }
    route.check_backing_parts(
        record.spec.allocation.map(|allocation| allocation.cost),
        pool.key,
        pool.cost,
        &pool.devices,
        &record.spec.devices,
    )?;
    if route.key.execution == hex_api::Execution::ShardsShared {
        return Ok(());
    }
    match pool.phase {
        hex_api::BackingPhase::Live | hex_api::BackingPhase::Draining
            if matches!(record.phase, Phase::Creating | Phase::Deleting)
                && record.init.is_none()
                && record.run.is_none()
                && record.exit.is_none() =>
        {
            Ok(())
        }
        hex_api::BackingPhase::Released if pool.allocation.is_some() && pool.exit.is_some() => {
            Ok(())
        }
        _ => Err(bad(
            "its record's phase, init, run or end disagrees with its backing's phase",
        )),
    }
}

/// Bound the sole consuming Box reference without another holder table. This is a borrowed
/// snapshot check under the existing source/Box turns, also shared by whole-owner preflight.
pub(crate) fn check_single_consumer(
    records: &hex_runtime::devices::Records,
    source: hex_api::BackingKey,
    id: &BoxId,
) -> Result<(), Refusal> {
    if records.len() > crate::store::TURNS {
        return Err(Refusal::Full(crate::store::TURNS));
    }
    for (other, row) in records {
        let row = row
            .as_ref()
            .map_err(|why| uncertain(other, &format!("a record beside it can't be read: {why}")))?;
        if row.version != hex_core::record::RECORD_VERSION || row.spec.id != *other {
            return Err(uncertain(
                other,
                "a record beside it is of another version or names another box",
            ));
        }
        row.spec.check()?;
        if other != id
            && row.spec.route.is_some_and(|route| {
                route.backing == Some(source)
                    && route.key.execution != hex_api::Execution::ShardsShared
            })
        {
            return Err(Refusal::Backing {
                key: source,
                cause: hex_api::LifecycleCause::Busy,
                why: None,
            });
        }
    }
    Ok(())
}

/// Keep the same source/Box guard while an adapter proves that a reserved/cancelled handoff
/// has no boot Intent. Revalidate the canonical rows after acquisition; no global mutex spans
/// metadata I/O or the adapter callback. Node and Recorded share this exact helper.
pub(crate) fn preflight_handoff_turns<'a>(
    turns: &'a Turns,
    store: &Store,
    record: &BoxRecord,
    catalog: Option<&hex_runtime::devices::Catalog>,
) -> Result<Option<(crate::turns::Turn<'a>, crate::turns::Turn<'a>)>, Refusal> {
    let Some(route) = record.spec.route else {
        return Ok(None);
    };
    let Some(source) = route.backing else {
        return Ok(None);
    };
    if route.key.execution == hex_api::Execution::ShardsShared {
        return Ok(None);
    }
    let pool = store
        .read_pool(&source.pool)?
        .ok_or_else(|| uncertain(&record.spec.id, "its backing has no record"))?;
    check_backing_reference(record, &pool)?;
    if !matches!(
        pool.phase,
        hex_api::BackingPhase::Live | hex_api::BackingPhase::Draining
    ) {
        return Ok(None);
    }
    let keys = catalog
        .ok_or_else(|| uncertain(&record.spec.id, "no device catalog holds its devices"))?
        .conflict_ids(&pool.devices)?;
    let held = turns.try_take_handoff(&record.spec.id, &source, &keys)?;
    if store.read_pool(&source.pool)?.as_ref() != Some(&pool)
        || store.read_lifecycle(&record.spec.id)?.as_ref() != Some(record)
    {
        return Err(uncertain(
            &record.spec.id,
            "its record or its backing changed while its handoff's turns were taken",
        ));
    }
    Ok(Some(held))
}

/// Whole retained-owner structural preflight shared by Node and Recorded. These are borrowed
/// snapshots from their SAME Store, not a second ledger. Only Catalog::holder decides conflicting
/// physical ownership, including its existing exact Box/Pool handoff rule.
pub(crate) fn preflight_records(
    records: &hex_runtime::devices::Records,
    pools: &[PoolRecord],
    catalog: Option<&hex_runtime::devices::Catalog>,
) -> Result<(), Refusal> {
    let bound = crate::store::TURNS;
    if records.len() > bound || pools.len() > bound {
        return Err(Refusal::Full(bound));
    }
    let count = records
        .len()
        .checked_add(pools.len())
        .ok_or(Refusal::Full(bound))?;
    let mut scopes = Vec::new();
    scopes
        .try_reserve_exact(count)
        .map_err(|_| Refusal::Exhausted {
            what: "retained native scope preflight",
            bound: count,
        })?;
    // A legal physical handoff has at most two retained claims per device (Box plus Pool).
    // Prepared/Released pool rows are not active grants. More is already conflicting authority.
    let claim_bound = hex_api::MAX_DEVICES
        .checked_mul(2)
        .ok_or(Refusal::Full(bound))?;
    let mut claims = Vec::new();
    claims
        .try_reserve_exact(claim_bound)
        .map_err(|_| Refusal::Exhausted {
            what: "retained physical claim preflight",
            bound: claim_bound,
        })?;
    let mut consumers = Vec::new();
    consumers
        .try_reserve_exact(records.len())
        .map_err(|_| Refusal::Exhausted {
            what: "retained consuming backing references",
            bound: records.len(),
        })?;
    let mut add_claims = |devices: &[DeviceId]| -> Result<(), Refusal> {
        if claims
            .len()
            .checked_add(devices.len())
            .is_none_or(|count| count > claim_bound)
        {
            return Err(broken(
                "retained physical claims exceed unique ownership and one handoff",
            ));
        }
        claims.extend_from_slice(devices);
        Ok(())
    };
    if pools
        .windows(2)
        .any(|pair| matches!(pair, [left, right] if left.key.pool >= right.key.pool))
    {
        return Err(broken(
            "retained backing configurations are not unique and ordered",
        ));
    }
    for pool in pools {
        pool.check()?;
        if !pool.devices.is_empty() {
            let catalog = catalog
                .ok_or_else(|| broken("retained pool selections have no initialized catalog"))?;
            if pool
                .devices
                .iter()
                .any(|id| !catalog.entries().iter().any(|entry| entry.info.id == *id))
            {
                return Err(NativeRecord::Pool(pool)
                    .uncertain("its pool selects a device its node's catalog doesn't hold"));
            }
        }
        scopes.push(*pool.scope().bytes());
        if !matches!(
            pool.phase,
            hex_api::BackingPhase::Prepared | hex_api::BackingPhase::Released
        ) {
            add_claims(&pool.devices)?;
        }
    }
    for (id, result) in records {
        let record = result
            .as_ref()
            .map_err(|why| uncertain(id, &format!("its record can't be read: {why}")))?;
        if record.version != hex_core::record::RECORD_VERSION || record.spec.id != *id {
            return Err(uncertain(
                id,
                "its record is of another version or names another box",
            ));
        }
        record.spec.check()?;
        if [record.init, record.run]
            .into_iter()
            .flatten()
            .any(|process| process.pid == 0)
        {
            return Err(uncertain(id, "its record names a process with no pid"));
        }
        // A retired box (joint plan step 14) runs nothing, holds no device and names no backing any more: its record is
        // kept for its owner's delete alone.
        if record.retired {
            if let Some(scope) = record.native_scope {
                scopes.push(*scope.bytes());
            }
            continue;
        }
        let backend = record.spec.backend_class()?;
        let shared = record.spec.route_execution() == Some(hex_api::Execution::ShardsShared);
        if matches!(backend, Class::Firecracker | Class::Shards)
            && !shared
            && (record.native_scope.is_none()
                || record.spec.allocation.is_none()
                || record.init.is_some_and(|init| init.start_ticks == 0)
                || (record.phase == Phase::Ready && record.init.is_none()))
        {
            return Err(uncertain(id, "its record's phase and its init disagree"));
        }
        if let Some(scope) = record.native_scope {
            scopes.push(*scope.bytes());
        }
        if let Some(route) = record.spec.route
            && let Some(key) = route.backing
        {
            if let Some(pool) = pools
                .binary_search_by_key(&key, |pool| pool.key)
                .ok()
                .and_then(|at| pools.get(at))
            {
                check_backing_reference(record, pool)?;
                if !shared {
                    consumers.push((key, id));
                }
            } else {
                // Every immutable route reference retains its source row through Box removal.
                return Err(uncertain(
                    id,
                    "its route names a backing with no record, which its removal would leave",
                ));
            }
        }
        add_claims(&record.spec.devices)?;
    }
    consumers.sort_unstable_by_key(|(key, _)| *key);
    if let Some(duplicate) = consumers.windows(2).find_map(|pair| match pair {
        [left, right] if left.0 == right.0 => Some(left.0),
        _ => None,
    }) {
        return Err(Refusal::Backing {
            key: duplicate,
            cause: hex_api::LifecycleCause::Uncertain,
            why: Some("two retained boxes name it as their source".to_owned()),
        });
    }
    scopes.sort_unstable();
    if scopes
        .windows(2)
        .any(|pair| matches!(pair, [left, right] if left == right))
    {
        return Err(broken("retained owners reuse a native scope incarnation"));
    }
    claims.sort_unstable();
    claims.dedup();
    if claims.is_empty() {
        return Ok(());
    }
    let catalog =
        catalog.ok_or_else(|| broken("retained physical claims have no initialized catalog"))?;
    for id in claims {
        if !catalog.entries().iter().any(|entry| entry.info.id == id) {
            return Err(broken(
                "retained physical identity is absent from the initialized catalog",
            ));
        }
        catalog.holder(id, records, pools)?;
    }
    Ok(())
}

impl Transaction<'_> {
    /// Its owner refused Uncertain, and `why` in the refusing site's own words (joint plan steps 14 and 12): a pool's
    /// backing refusal and a box's Lifecycle refusal carry them alike.
    fn uncertain(&self, why: &str) -> Refusal {
        match self.owner {
            TransactionOwner::Box(id) | TransactionOwner::Handoff { id, .. } => {
                match id.try_clone() {
                    Ok(id) => Refusal::Lifecycle {
                        id,
                        cause: hex_api::LifecycleCause::Uncertain,
                        why: Some(why.to_owned()),
                    },
                    Err(error) => error,
                }
            }
            TransactionOwner::Pool(key) => Refusal::Backing {
                key: *key,
                cause: hex_api::LifecycleCause::Uncertain,
                why: Some(why.to_owned()),
            },
        }
    }
    fn box_id(&self) -> Result<&BoxId, Refusal> {
        match self.owner {
            TransactionOwner::Box(id) | TransactionOwner::Handoff { id, .. } => Ok(id),
            TransactionOwner::Pool(_) => Err(self.uncertain("a pool's transaction has no box id")),
        }
    }
    fn owns(&self, record: NativeRecord<'_>) -> Result<(), Refusal> {
        let identity = match (self.owner, record) {
            (TransactionOwner::Box(id), NativeRecord::Box(record)) => *id == record.spec.id,
            (TransactionOwner::Handoff { id, source }, NativeRecord::Box(record)) => {
                *id == record.spec.id
                    && record.spec.route.is_some_and(|route| {
                        route.backing == Some(*source)
                            && route.key.execution != hex_api::Execution::ShardsShared
                    })
            }
            (TransactionOwner::Pool(key), NativeRecord::Pool(record)) => *key == record.key,
            _ => false,
        };
        if !identity
            || record
                .devices()
                .iter()
                .any(|device| !self.devices.contains(device))
        {
            return Err(self.uncertain("its record isn't its transaction's owner, or selects devices its transaction doesn't hold"));
        }
        Ok(())
    }
    fn with_native<T>(
        &self,
        operation: impl FnOnce(NativeRecord<'_>) -> Result<T, Refusal>,
    ) -> Result<T, Refusal> {
        match self.owner {
            TransactionOwner::Box(id) | TransactionOwner::Handoff { id, .. } => {
                let record = self.store.read_lifecycle(id)?.ok_or_else(|| {
                    self.uncertain("its box record is gone as its transaction reads it")
                })?;
                let native = NativeRecord::Box(&record);
                self.owns(native)?;
                record.spec.check()?;
                if self
                    .store
                    .check_box_backing(&record)?
                    .is_some_and(|phase| phase != hex_api::BackingPhase::Released)
                    && let Some(route) = record
                        .spec
                        .route
                        .filter(|route| route.key.execution != hex_api::Execution::ShardsShared)
                    && let Some(key) = route.backing
                {
                    return Err(Refusal::Backing {
                        key,
                        cause: hex_api::LifecycleCause::Pending,
                        why: None,
                    });
                }
                operation(native)
            }
            TransactionOwner::Pool(key) => {
                let record = self.store.read_pool(&key.pool)?.ok_or_else(|| {
                    self.uncertain("its pool record is gone as its transaction reads it")
                })?;
                let native = NativeRecord::Pool(&record);
                self.owns(native)?;
                record.check()?;
                operation(native)
            }
        }
    }
    fn launch_claims(&self, record: NativeRecord<'_>) -> Result<(), Refusal> {
        match record {
            NativeRecord::Pool(record) => {
                check_pool_claims(self.store, self.native.catalog, record)
            }
            NativeRecord::Box(record) => {
                if record.spec.devices.is_empty() {
                    return Ok(());
                }
                let catalog = self.native.catalog.ok_or_else(|| {
                    uncertain(&record.spec.id, "no device catalog holds its devices")
                })?;
                let boxes = self.store.all()?;
                let pools = self.store.all_pools()?;
                for id in &record.spec.devices {
                    match catalog.holder(*id, &boxes, &pools)? {
                        Some(hex_api::DeviceHolder::Box(id)) if id == record.spec.id => {}
                        _ => {
                            return Err(uncertain(
                                &record.spec.id,
                                "a device it claims is held by another box, or by none",
                            ));
                        }
                    }
                }
                if let Some(route) = record.spec.route
                    && let Some(key) = route.backing
                {
                    let pool = pools.iter().find(|pool| pool.key == key).ok_or_else(|| {
                        Refusal::Backing {
                            key,
                            cause: hex_api::LifecycleCause::Uncertain,
                            why: Some("a launching box's source backing has no record".to_owned()),
                        }
                    })?;
                    if pool.phase != hex_api::BackingPhase::Released || pool.exit.is_none() {
                        return Err(Refusal::Backing {
                            key,
                            cause: hex_api::LifecycleCause::Pending,
                            why: None,
                        });
                    }
                    route.check_backing_parts(
                        record.spec.allocation.map(|allocation| allocation.cost),
                        pool.key,
                        pool.cost,
                        &pool.devices,
                        &record.spec.devices,
                    )?;
                }
                Ok(())
            }
        }
    }
    fn publish(
        &self,
        scope: hex_core::record::NativeScope,
        commitment: [u8; 32],
        init: ProcId,
    ) -> Result<(), Refusal> {
        self.with_native(|current| {
            if current.scope() != Some(scope)
                || current.commitment()? != commitment
                || current.stage() != NativeStage::Creating
                || current.init().is_some()
            {
                return Err(self.uncertain("its launch was published, or its controller set, before its transaction published it"));
            }
            match current {
                NativeRecord::Box(record) => {
                    let mut current = self
                        .store
                        .read_lifecycle(&record.spec.id)?
                        .ok_or_else(|| self.uncertain("its box record is gone as its transaction publishes it"))?;
                    current.init = Some(init);
                    self.store.put_lifecycle(&current)
                }
                NativeRecord::Pool(record) => {
                    let mut current = self
                        .store
                        .read_pool(&record.key.pool)?
                        .ok_or_else(|| self.uncertain("its pool record is gone as its transaction publishes it"))?;
                    current.init = Some(init);
                    self.store.put_pool(&current)
                }
            }
        })
    }
}

impl hex_runtime::devices::Transaction for Transaction<'_> {
    fn check_unsubmitted_pool(&self) -> Result<(), Refusal> {
        if !matches!(self.owner, TransactionOwner::Pool(_)) {
            return Err(self.uncertain("a box's transaction can't prove a pool unsubmitted"));
        }
        self.with_native(|record| {
            let NativeRecord::Pool(pool) = record else {
                return Err(self.uncertain("its transaction's record isn't a pool's as it is proven unsubmitted"));
            };
            if pool.phase != hex_api::BackingPhase::Prepared
                || pool.allocation.is_some()
                || pool.init.is_some()
                || pool.exit.is_some()
            {
                return Err(self.uncertain("its pool is past its preparation, or holds an allocation, a controller or an end"));
            }
            // The caller's SAME Pool/configuration turn excludes grant/native submission.
            // This is only the native metadata half of cancellation, never a Released witness.
            (self.native.unsubmitted_pool)(record)
        })
    }
    fn managed_usage(&self) -> Result<Usage, Refusal> {
        self.with_native(|record| {
            if record.scope().is_none()
                || record.allocation().is_none()
                || !matches!(record.class()?, Class::Shards | Class::Firecracker)
            {
                return Err(
                    self.uncertain("its record isn't an allocated VM's as its usage is read")
                );
            }
            (self.native.usage)(record)
        })
    }
    fn launch_managed(
        &self,
        plan: &hex_runtime::devices::ManagedLaunch<'_>,
        io: shards_api::LaunchIo,
        patience: Option<std::time::Duration>,
    ) -> Result<ManagedReceipt, Refusal> {
        // None: a create that follows its guest, with no deadline of its caller's (the review's C-9).
        let deadline = patience
            .map(|patience| {
                std::time::Instant::now()
                    .checked_add(patience)
                    .filter(|_| patience != std::time::Duration::MAX && !patience.is_zero())
                    .ok_or_else(|| {
                        self.uncertain("its transaction's launch patience isn't a finite time")
                    })
            })
            .transpose()?;
        self.with_native(|record| {
            if record.class()? != plan.class()
                || record.allocation().is_none()
                || record.stage() != NativeStage::Creating
                || record.init().is_some()
            {
                return Err(self.uncertain("its record isn't a fresh VM creation as its transaction launches it"));
            }
            self.launch_claims(record)?;
            let deadline = hex_runtime::devices::check_managed_report(
                self.native.catalog,
                record,
                plan,
                io.gpu_report.is_some(),
                deadline,
            )?;
            let scope = record
                .scope()
                .ok_or_else(|| self.uncertain("its record names no native scope to launch"))?;
            let commitment = record.commitment()?;
            let mut introduced = false;
            let mut publish = |identity: &shards_api::Identity| {
                if introduced
                    || identity.pid == 0
                    || identity.started == 0
                    || identity.vsock != plan.vsock()
                {
                    return Err(self.uncertain("its launched controller's identity or vsock isn't its plan's"));
                }
                self.publish(
                    scope,
                    commitment,
                    ProcId {
                        pid: identity.pid,
                        start_ticks: identity.started,
                    },
                )?;
                introduced = true;
                Ok(())
            };
            let remaining = deadline
                .map(|deadline| {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        return Err(self.uncertain("its transaction's launch deadline passed before its controller was handed over"));
                    }
                    Ok(remaining)
                })
                .transpose()?;
            let receipt = (self.native.launch)(record, plan, io, remaining, &mut publish)?;
            if !introduced {
                return Err(self.uncertain("its launch's controller wasn't introduced to its record"));
            }
            Ok(receipt)
        })
    }
    fn attach_managed(
        &self,
        identity: Option<&shards_api::Identity>,
    ) -> Result<ManagedReceipt, Refusal> {
        self.with_native(|record| {
            if record.allocation().is_none()
                || record.scope().is_none()
                || !matches!(record.class()?, Class::Shards | Class::Firecracker)
            {
                return Err(self.uncertain(
                    "its record isn't an allocated VM's as its transaction attaches it",
                ));
            }
            match identity {
                Some(identity)
                    if record.init().is_some_and(|init| {
                        init.pid == identity.pid && init.start_ticks == identity.started
                    }) && identity.pid != 0
                        && identity.started != 0 => {}
                None if matches!(
                    record.stage(),
                    NativeStage::Creating | NativeStage::Deleting
                ) => {}
                _ => {
                    return Err(self.uncertain(
                        "its attached controller isn't its record's, or it isn't fresh",
                    ));
                }
            }
            (self.native.attach)(record, identity)
        })
    }
    fn finalize_managed(&self, identity: Option<&shards_api::Identity>) -> Result<(), Refusal> {
        self.with_native(|record| {
            if record.allocation().is_none()
                || record.scope().is_none()
                || !matches!(record.class()?, Class::Shards | Class::Firecracker)
                || !matches!(
                    record.stage(),
                    NativeStage::Deleting | NativeStage::Released
                )
            {
                return Err(self.uncertain("its record isn't an allocated VM's that is deleting or released as it is finalized"));
            }
            match (record.init(), identity) {
                (Some(expected), Some(actual))
                    if expected.pid == actual.pid && expected.start_ticks == actual.started => {}
                (None, Some(actual)) if actual.pid != 0 && actual.started != 0 => {}
                (None, None) => {}
                _ => return Err(self.uncertain("its finalized controller isn't its record's")),
            }
            (self.native.finalize)(record, identity)
        })
    }
    fn records(&self) -> Result<hex_runtime::devices::Records, Refusal> {
        self.store.all()
    }
    fn read(&self, id: &BoxId) -> Result<Option<BoxRecord>, Refusal> {
        if id != self.box_id()? {
            return Err(self.uncertain("its transaction reads another box than its owner"));
        }
        self.store.read_lifecycle(id)
    }
    fn put(&self, record: &BoxRecord) -> Result<(), Refusal> {
        self.owns(NativeRecord::Box(record))?;
        if self.store.read_lifecycle(&record.spec.id)?.is_none() {
            if record.spec.route.is_some_and(|route| {
                route.backing.is_some() && route.key.execution != hex_api::Execution::ShardsShared
            }) && !matches!(self.owner, TransactionOwner::Handoff { .. })
            {
                return Err(self.uncertain("a new box record names a backing outside a handoff"));
            }
            check_box_claims(self.store, self.native.catalog, &record.spec)?;
        }
        self.store.put_lifecycle(record)
    }
    fn remove(&self, id: &BoxId) -> Result<(), Refusal> {
        if id != self.box_id()? {
            return Err(self.uncertain("its transaction removes another box than its owner"));
        }
        if let Some(record) = self.store.read_lifecycle(id)? {
            self.owns(NativeRecord::Box(&record))?;
        }
        self.store.remove_lifecycle(id)
    }
    fn pool_records(&self) -> Result<Vec<PoolRecord>, Refusal> {
        self.store.all_pools()
    }
    fn read_pool(&self, key: &hex_api::BackingKey) -> Result<Option<PoolRecord>, Refusal> {
        match self.owner {
            TransactionOwner::Pool(expected) if expected == key => {}
            TransactionOwner::Handoff { source, .. } if source == key => {}
            TransactionOwner::Box(id) => {
                let current = self.store.read_lifecycle(id)?.ok_or_else(|| {
                    self.uncertain("its handoff's box record is gone as its source is read")
                })?;
                if current.spec.route.and_then(|route| route.backing) != Some(*key) {
                    return Err(self.uncertain("its handoff's box names another source"));
                }
            }
            _ => {
                return Err(self.uncertain(
                    "its transaction reads a pool that isn't its owner or handoff source",
                ));
            }
        }
        let record = self.store.read_pool(&key.pool)?;
        if let Some(record) = &record {
            record.check()?;
            if record.key != *key {
                return Err(
                    self.uncertain("its transaction's pool record names another generation")
                );
            }
            if matches!(self.owner, TransactionOwner::Pool(_)) {
                self.owns(NativeRecord::Pool(record))?;
            }
        }
        Ok(record)
    }
    fn grant_pool(&self, grant: &hex_api::BackingGrant) -> Result<PoolRecord, Refusal> {
        let record = self.read_pool(&grant.key)?.ok_or_else(|| {
            self.uncertain("its pool record is gone before its transaction's grant")
        })?;
        self.owns(NativeRecord::Pool(&record))?;
        check_pool_claims(self.store, self.native.catalog, &record)?;
        self.store.grant_pool(grant)
    }
    fn put_pool(&self, record: &PoolRecord) -> Result<(), Refusal> {
        self.owns(NativeRecord::Pool(record))?;
        // Existing cleanup authority is checked against its canonical immutable row, never a
        // fresh placement report. Draining must remain possible through a fenced handoff.
        self.store.put_pool(record)
    }
    fn remove_pool(&self, key: &hex_api::BackingKey) -> Result<(), Refusal> {
        let record = self.read_pool(key)?.ok_or_else(|| {
            self.uncertain("its pool record is gone before its transaction removed it")
        })?;
        self.owns(NativeRecord::Pool(&record))?;
        if record.phase == hex_api::BackingPhase::Released
            && record.allocation.is_none()
            && record.init.is_none()
            && record.exit.is_none()
        {
            // This is the explicit Index ACK of a never-submitted cancellation, not native
            // scope release. Recheck the SAME native metadata before erasing its only receipt.
            (self.native.unsubmitted_pool)(NativeRecord::Pool(&record))?;
        }
        self.store.remove_pool(key)
    }
}

/// Both native Node and Recorded enter their existing method/attachment turns before this call.
/// No Store turn spans native I/O; individual record writes take only their short Store turn.
pub(crate) fn transaction(
    store: &Store,
    id: &BoxId,
    devices: &[DeviceId],
    native: &Native<'_>,
    callback: &mut dyn FnMut(&dyn hex_runtime::devices::Transaction) -> Result<(), Refusal>,
) -> Result<(), Refusal> {
    callback(&Transaction {
        store,
        owner: TransactionOwner::Box(id),
        devices,
        native,
    })
}

/// SAME Box transaction, with its immutable source available read-only. The caller owns source
/// configuration then Box/full-P alias turns; no pool mutation/native callback reenters them.
pub(crate) fn handoff_transaction(
    store: &Store,
    id: &BoxId,
    source: &hex_api::BackingKey,
    full_conflicts: &[DeviceId],
    native: &Native<'_>,
    callback: &mut dyn FnMut(&dyn hex_runtime::devices::Transaction) -> Result<(), Refusal>,
) -> Result<(), Refusal> {
    let refused = |why: &str| Refusal::Backing {
        key: *source,
        cause: hex_api::LifecycleCause::Uncertain,
        why: Some(why.to_owned()),
    };
    let pool = store
        .read_pool(&source.pool)?
        .ok_or_else(|| refused("its handoff's source has no pool record"))?;
    pool.check()?;
    if pool.key != *source
        || pool.allocation.is_none()
        || !matches!(
            pool.execution,
            hex_api::Execution::ShardsDedicated | hex_api::Execution::Firecracker
        )
        || !matches!(
            pool.phase,
            hex_api::BackingPhase::Live
                | hex_api::BackingPhase::Draining
                | hex_api::BackingPhase::Released
        )
    {
        return Err(refused(
            "its handoff's source isn't an allocated dedicated or Firecracker backing that is live, draining or released",
        ));
    }
    hex_api::BackingOffer::check_selection_parts(&pool.devices, &pool.devices)?;
    let catalog = native
        .catalog
        .ok_or_else(|| refused("its node holds no device catalog for its handoff"))?;
    if catalog.conflict_ids(&pool.devices)?.as_slice() != full_conflicts {
        return Err(refused(
            "its handoff source's device conflicts aren't the ones asked",
        ));
    }
    let records = store.retained_records()?;
    check_single_consumer(&records, *source, id)?;
    if let Some(current) = records.get(id) {
        check_backing_reference(
            current
                .as_ref()
                .map_err(|why| uncertain(id, &format!("its record can't be read: {why}")))?,
            &pool,
        )?;
    }
    callback(&Transaction {
        store,
        owner: TransactionOwner::Handoff { id, source },
        devices: full_conflicts,
        native,
    })
}

/// SAME stack transaction, entered after the native owner's canonical pool/device turns.
pub(crate) fn pool_transaction(
    store: &Store,
    key: &hex_api::BackingKey,
    devices: &[DeviceId],
    native: &Native<'_>,
    callback: &mut dyn FnMut(&dyn hex_runtime::devices::Transaction) -> Result<(), Refusal>,
) -> Result<(), Refusal> {
    callback(&Transaction {
        store,
        owner: TransactionOwner::Pool(key),
        devices,
        native,
    })
}

/// Initial configuration publication has no generation yet, so its caller holds the SAME
/// configuration/device turns before entering. It performs no resource grant or native effect.
pub(crate) fn prepare_pool(
    store: &Store,
    pool: [u8; 32],
    execution: hex_api::Execution,
    cost: hex_api::RuntimeCost,
    devices: &[DeviceId],
    family: (&[u8; 32], &[DeviceId], Option<&PoolRecord>),
    validate: &mut hex_runtime::devices::PoolPreparationCheck<'_>,
) -> Result<PoolRecord, Refusal> {
    store.prepare_pool_validated(pool, execution, cost, devices, family, validate)
}

fn check_box_claims(
    store: &Store,
    catalog: Option<&hex_runtime::devices::Catalog>,
    spec: &BoxSpec,
) -> Result<(), Refusal> {
    if spec.devices.is_empty() {
        return Ok(());
    }
    let catalog =
        catalog.ok_or_else(|| uncertain(&spec.id, "no device catalog holds its devices"))?;
    catalog.prepare(spec, &store.all()?, &store.all_pools()?)?;
    Ok(())
}

fn check_pool_claims(
    store: &Store,
    catalog: Option<&hex_runtime::devices::Catalog>,
    record: &PoolRecord,
) -> Result<(), Refusal> {
    if record.devices.is_empty() {
        return Ok(());
    }
    let refused = || Refusal::Backing {
        key: record.key,
        cause: hex_api::LifecycleCause::Busy,
        why: None,
    };
    let catalog = catalog.ok_or_else(refused)?;
    let boxes = store.all()?;
    let pools = store.all_pools()?;
    for id in &record.devices {
        if !catalog.entries().iter().any(|entry| entry.info.id == *id) {
            return Err(refused());
        }
        match catalog.holder(*id, &boxes, &pools)? {
            None => {}
            Some(hex_api::DeviceHolder::Backing(key)) if key == record.key => {}
            Some(_) => return Err(refused()),
        }
    }
    Ok(())
}

fn recorded_holders(
    report: &mut hex_api::Capacity,
    records: &hex_runtime::devices::Records,
    pools: &[PoolRecord],
    catalog: Option<&hex_runtime::devices::Catalog>,
) -> Result<(), Refusal> {
    if report.devices.is_empty() {
        return report.check();
    }
    let catalog = catalog
        .ok_or_else(|| broken("initialized physical offers have no canonical device catalog"))?;
    for offer in &mut report.devices {
        if let Some(holder) = catalog.holder(offer.id, records, pools)? {
            if offer
                .holder
                .as_ref()
                .is_some_and(|native| native != &holder)
            {
                return Err(broken("native and canonical physical owners disagree"));
            }
            offer.holder = Some(holder);
        }
    }
    report.check()
}

fn native_metadata(
    report: &mut hex_api::Capacity,
    physical: Vec<hex_api::DeviceInfo>,
) -> Result<(), Refusal> {
    report.check()?;
    if physical.iter().any(|native| native.holder.is_some()) {
        return Err(broken(
            "native discovery cannot declare reservation holders",
        ));
    }
    hex_runtime::devices::normalize_capacity_devices(&mut report.devices, &physical)?;
    report.check()
}

impl<R: Recover> Recorded<R> {
    /// Its image VMs' credentials leased over its wire (joint plan step 33; docs/design/credential-grants.md C7):
    /// `lessors`, which its guest runtime's image class was built with, each box's route kept by its socket as the box is
    /// created. Given once.
    pub fn lease_over_wire(&self, lessors: Arc<crate::serve::WireLessors>) -> Result<(), Refusal> {
        self.wire_lessors
            .set(lessors)
            .map_err(|_| Refusal::Invalid {
                what: "lessors over the node's wire",
                why: "the node was given them already".to_owned(),
            })
    }

    /// The retained records past the whole-owner barrier, and apart from them the torn ones
    /// ([`Store::take_torn`]), which no barrier can check: [`Recorded::recover`] proves their boxes gone or
    /// holds them. A record in the state directory that can't be read is refused, naming its file.
    fn preflight_snapshot(
        &self,
    ) -> Result<(hex_runtime::devices::Records, crate::store::Torn), Refusal> {
        let mut records = self.books.store.retained_records()?;
        let torn = self.books.store.take_torn(&mut records)?;
        self.books.store.refuse_unreadable(&records)?;
        let pools = self.books.store.all_pools()?;
        let catalog = self.runtime.device_catalog()?;
        preflight_records(&records, &pools, catalog.as_deref())?;
        // Configuration validation is a complete second read-only pass. Native observations and
        // cleanup are deliberately deferred until every sibling and backing passes both passes.
        for (id, record) in &records {
            let record = record
                .as_ref()
                .map_err(|why| uncertain(id, &format!("its record can't be read: {why}")))?;
            let handoff = preflight_handoff_turns(
                &self.books.lifecycle,
                &self.books.store,
                record,
                catalog.as_deref(),
            )?;
            if handoff.is_some() {
                let source = record
                    .spec
                    .route
                    .and_then(|route| route.backing)
                    .ok_or_else(|| uncertain(id, "its record routes it on no backing"))?;
                let pool = pools
                    .binary_search_by_key(&source, |pool| pool.key)
                    .ok()
                    .and_then(|at| pools.get(at))
                    .ok_or_else(|| uncertain(id, "its backing has no record"))?;
                self.runtime.handoff_unsubmitted(record, pool)?;
            } else {
                self.runtime.preflight_record(record)?;
            }
        }
        for record in &pools {
            self.runtime.preflight_pool(record)?;
        }
        self.runtime.preflight_retained(&records, &pools)?;
        Ok((records, torn))
    }

    /// Startup preflight over ALL retained owners, before backend preparation/recovery or serving
    /// new calls. A refusal preserves their bytes and charge; this never probes a GPU or VM.
    pub fn preflight_recovery(&self) -> Result<(), Refusal> {
        self.preflight_snapshot().map(|_| ())
    }

    /// Initializes native owners after ALL retained owners pass the read-only barrier.
    pub fn initialize_native(&self) -> Result<(), Refusal> {
        self.preflight_recovery()?;
        self.runtime.initialize_native()
    }

    /// Joint plan step 14 (ROOT, 2026-11-13): retires, once its natives are up, every backing and VM box this node
    /// recorded under a profile's census cost ([`Recover::retire_census`]), before its preparation and recovery: the
    /// preflight passes them for it, and no exact check sees them after.
    pub fn retire_census(&self) -> Result<(), Refusal> {
        self.initialize_native()?;
        self.runtime.retire_census()
    }

    fn native_transaction(
        &self,
        owner: TransactionOwner<'_>,
        devices: &[DeviceId],
        callback: &mut dyn FnMut(&dyn hex_runtime::devices::Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        let launch =
            |record: NativeRecord<'_>,
             plan: &hex_runtime::devices::ManagedLaunch<'_>,
             io: shards_api::LaunchIo,
             patience: Option<std::time::Duration>,
             publish: &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>| {
                self.runtime
                    .launch_managed(record, plan, io, patience, publish)
            };
        let attach = |record: NativeRecord<'_>, identity: Option<&shards_api::Identity>| {
            self.runtime.attach_managed(record, identity)
        };
        let finalize = |record: NativeRecord<'_>, identity: Option<&shards_api::Identity>| {
            self.runtime.finalize_managed(record, identity)
        };
        let usage = |record: NativeRecord<'_>| self.runtime.managed_usage(record);
        let unsubmitted_pool = |record: NativeRecord<'_>| self.runtime.preflight_managed(record);
        let catalog = self.runtime.device_catalog()?;
        let native = Native {
            catalog: catalog.as_deref(),
            launch: &launch,
            attach: &attach,
            finalize: &finalize,
            usage: &usage,
            unsubmitted_pool: &unsubmitted_pool,
        };
        match owner {
            TransactionOwner::Box(id) => {
                transaction(&self.books.store, id, devices, &native, callback)
            }
            TransactionOwner::Handoff { id, source } => {
                handoff_transaction(&self.books.store, id, source, devices, &native, callback)
            }
            TransactionOwner::Pool(key) => {
                pool_transaction(&self.books.store, key, devices, &native, callback)
            }
        }
    }
}

impl<R: Recover> hex_runtime::devices::Books for Recorded<R> {
    fn records(&self) -> Result<hex_runtime::devices::Records, Refusal> {
        self.books.store.all()
    }
    fn record(&self, id: &BoxId) -> Result<Option<Result<BoxRecord, String>>, Refusal> {
        self.books.store.get(id)
    }
    fn physical_devices(&self) -> Result<Vec<hex_api::DeviceInfo>, Refusal> {
        let prober = self
            .books
            .prober
            .as_ref()
            .ok_or_else(|| broken("the recorded owner has no initialized native provider"))?;
        prober
            .lock()
            .map_err(|_| broken("the shared native provider lock was poisoned"))?
            .physical_devices()
    }
    fn physical_luid(&self, id: DeviceId) -> Result<[u8; 8], Refusal> {
        let prober = self
            .books
            .prober
            .as_ref()
            .ok_or_else(|| broken("the recorded owner has no initialized native provider"))?;
        crate::facts::physical_luid(prober, id)
    }
    fn capacity(&self) -> Result<hex_api::Capacity, Refusal> {
        // Calling WithGuest::capacity here would reenter GuestRuntime -> Books. Probe the same
        // owner's host volume directly; device routes add their initialized catalog separately.
        let mut capacity =
            crate::facts::capacity(self.books.store.directory(), self.books.authority_id()?)?;
        // This is the native resource plane, not initialized route eligibility. Guest.add_offers
        // performs its renderer/native join later, exactly once; it must not be called recursively
        // from the authority it queries. The same Prober provides all native metadata here.
        capacity.devices = hex_runtime::devices::Books::physical_devices(self)?;
        if capacity.devices.is_empty() {
            capacity.check()?;
        } else {
            recorded_holders(
                &mut capacity,
                &self.books.store.all()?,
                &self.books.store.all_pools()?,
                self.runtime.device_catalog()?.as_deref(),
            )?;
        }
        Ok(capacity)
    }
    fn with_transaction(
        &self,
        id: &BoxId,
        devices: &[DeviceId],
        callback: &mut dyn FnMut(&dyn hex_runtime::devices::Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        let _method = self.books.lifecycle.try_take_devices(id, devices)?;
        self.native_transaction(TransactionOwner::Box(id), devices, callback)
    }
    /// Waits for a turn of the box under way, following its holder (decision 9), where a method's or a native
    /// transaction's is refused the overlap: it makes no native call (`hex_runtime::devices::with_record_transaction`
    /// refuses each), so no holder waits on it.
    fn with_record_transaction(
        &self,
        id: &BoxId,
        callback: &mut dyn FnMut(&dyn hex_runtime::devices::Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        let _turn = self.books.lifecycle.take(id)?;
        self.native_transaction(TransactionOwner::Box(id), &[], callback)
    }
    fn pool_records(&self) -> Result<Vec<PoolRecord>, Refusal> {
        self.books.store.all_pools()
    }
    fn prepare_pool(
        &self,
        pool: [u8; 32],
        execution: hex_api::Execution,
        cost: hex_api::RuntimeCost,
        devices: &[DeviceId],
        family: (&[u8; 32], &[DeviceId], Option<&PoolRecord>),
        validate: &mut hex_runtime::devices::PoolPreparationCheck<'_>,
    ) -> Result<PoolRecord, Refusal> {
        let catalog = self.runtime.device_catalog()?;
        let conflicts = match catalog.as_deref() {
            Some(catalog) => catalog.conflict_ids(family.1)?,
            None if family.1.is_empty() => Vec::new(),
            None => return Err(broken("native backing preparation has no device catalog")),
        };
        let _method = self
            .books
            .lifecycle
            .try_take_pool_configuration(family.0, &conflicts)?;
        let introduced = if family.2.is_none() {
            Some((execution, devices))
        } else {
            None
        };
        let mut checked = |records: &hex_runtime::devices::Records, pools: &[PoolRecord]| {
            validate(records, pools)?;
            self.runtime.check_route_projection(pools, introduced)
        };
        prepare_pool(
            &self.books.store,
            pool,
            execution,
            cost,
            devices,
            (family.0, &conflicts, family.2),
            &mut checked,
        )
    }
    fn with_handoff_transaction(
        &self,
        id: &BoxId,
        source: &hex_api::BackingKey,
        full_conflicts: &[DeviceId],
        callback: &mut dyn FnMut(&dyn hex_runtime::devices::Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        let _turns = self
            .books
            .lifecycle
            .try_take_handoff(id, source, full_conflicts)?;
        self.native_transaction(
            TransactionOwner::Handoff { id, source },
            full_conflicts,
            callback,
        )
    }
    fn with_pool_transaction(
        &self,
        key: &hex_api::BackingKey,
        devices: &[DeviceId],
        callback: &mut dyn FnMut(&dyn hex_runtime::devices::Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        let _method = self.books.lifecycle.try_take_pool(key, devices)?;
        self.native_transaction(TransactionOwner::Pool(key), devices, callback)
    }
    fn node_vm(
        &self,
        run: &hex_api::NodeRun,
    ) -> Result<Option<hex_core::record::NodeVmRecord>, Refusal> {
        self.books.store.node_vm(run)
    }
    fn put_node_vm(&self, record: &hex_core::record::NodeVmRecord) -> Result<(), Refusal> {
        self.books.store.put_node_vm(record)?;
        // A node VM Ready is a VM to learn: a learner holding no cap reads it now, not at the next box's beginning
        // (A-9).
        if record.phase == hex_api::NodeVmPhase::Ready {
            self.begun();
        }
        Ok(())
    }
    fn all_node_vms(&self) -> Result<Vec<hex_core::record::NodeVmRecord>, Refusal> {
        self.books.store.all_node_vms()
    }
    fn mint_node_run(&self) -> Result<hex_api::NodeRun, Refusal> {
        self.books.store.mint_node_run()
    }
}

type Projection = (
    BoxId,
    Class,
    Class,
    Option<hex_api::Allocation>,
    Option<hex_api::RouteReceipt>,
);

fn projection(spec: BoxSpec) -> Result<Projection, Refusal> {
    let backend = spec.backend_class()?;
    Ok((
        spec.id,
        spec.class.class(),
        backend,
        spec.allocation,
        spec.route,
    ))
}

fn project_info(info: &mut BoxInfo, expected: &Projection) -> Result<(), Refusal> {
    let (id, class, backend, allocation, route) = expected;
    let inner = *backend == Class::Shards && info.class == Class::Oci;
    if info.id != *id
        || (info.class != *class && !inner)
        || (info.allocation != *allocation && !(inner && info.allocation.is_none()))
        || (info.route.is_some() && info.route != *route)
    {
        return Err(uncertain(
            id,
            "its runtime's view of it isn't its record's: another id, class, allocation or route",
        ));
    }
    info.class = *class;
    info.allocation = *allocation;
    info.route = *route;
    Ok(())
}

#[cfg(test)]
mod projection_tests {
    use super::*;

    fn source() -> BoxSpec {
        BoxSpec {
            id: BoxId::new("canonical-route").unwrap(),
            class: hex_api::ClassSpec::Process,
            bounds: hex_api::Bounds::default(),
            owner: None,
            devices: Vec::new(),
            allocation: None,
            route: Some(hex_api::RouteReceipt {
                key: hex_api::RouteKey {
                    class: Class::Process,
                    image: hex_api::ImagePlatform {
                        os: hex_api::ImageOs::Linux,
                        arch: hex_api::ImageArch::Amd64,
                    },
                    execution: hex_api::Execution::Native,
                    gpu: hex_api::GpuEligibility::None,
                },
                backing: None,
            }),
            band: 0,
        }
    }
    fn factual() -> BoxInfo {
        BoxInfo {
            published: Vec::new(),
            grants: Vec::new(),
            id: BoxId::new("canonical-route").unwrap(),
            class: Class::Process,
            state: hex_api::BoxState::Empty,
            bounds: hex_api::Bounds::default(),
            adopted: false,
            membership: None,
            exit: None,
            abandoned: 0,
            allocation: None,
            route: None,
            last_resort: None,
            ended: None,
            control: None,
            control_wake: None,
            live: None,
            credentials: Vec::new(),
        }
    }

    /// AC-T7.4.1: every projection preserves the chosen original receipt without a fresh report/picker.
    #[test]
    fn canonical_route_survives_native_projection() {
        let spec = source();
        let route = spec.route;
        let expected = projection(spec).unwrap();
        let mut info = factual();
        project_info(&mut info, &expected).unwrap();
        assert_eq!(info.route, route);
        assert_eq!(info.class, Class::Process);
        assert_eq!(info.allocation, None);
    }

    /// AC-T7.4.1: a reply cannot substitute another route or another box under the retained turn.
    #[test]
    fn a_foreign_reply_or_conflicting_route_is_refused() {
        let spec = source();
        let mut wrong = spec.route.unwrap();
        wrong.key.image.os = hex_api::ImageOs::Windows;
        let expected = projection(spec).unwrap();
        let mut info = factual();
        info.route = Some(wrong);
        assert!(project_info(&mut info, &expected).is_err());
        info.route = None;
        info.id = BoxId::new("foreign").unwrap();
        assert!(project_info(&mut info, &expected).is_err());
    }
}

struct ForwardTurn {
    books: Arc<Books>,
    hold: Option<Hold>,
    expected: Option<Projection>,
}

impl Drop for ForwardTurn {
    fn drop(&mut self) {
        if let Some(hold) = self.hold.take() {
            self.books.lifecycle.release(hold);
        }
    }
}

impl<R: Recover> Recorded<R> {
    /// Whether the machine's capacity view serves this node's machine, as its configuration says (`--capacity-view`; the
    /// review's A-10, CV10's configured form): where one does, the charge learner learns nothing until its keeper has
    /// spoken since the node started, its VMs booked whole meanwhile, told; where none does, no holder reserves room
    /// there. Set before the learner starts.
    pub fn set_capacity_view(&self, served: bool) {
        self.books
            .capacity_view
            .store(served, std::sync::atomic::Ordering::SeqCst);
    }

    /// Tells the charge learner a box has begun, or a node VM has booted (joint plan step 14, C1; A-9): one holding no
    /// cap runs a window at once; one waiting out its window reads it at the window's end. Nothing where no learner
    /// runs.
    fn begun(&self) {
        if let Some((_, begun)) = self.books.charges.get()
            && let Ok(begun) = begun.lock()
        {
            // Full: a beginning is told already. Gone: the learner has stopped, its failure kept.
            let _ = begun.try_send(());
        }
    }

    /// Native methods use this same owner. A backend registered with Books enters it itself;
    /// route changes between the initial read and acquisition are refused before native I/O.
    fn method_turn(&self, id: &BoxId) -> Result<Option<Turn<'_>>, Refusal> {
        let before = self.books.store.read_lifecycle(id)?;
        if let Some(record) = &before
            && self.runtime.owns_records(&record.spec)?
        {
            return Ok(None);
        }
        let devices = before
            .as_ref()
            .map_or(&[][..], |r| r.spec.devices.as_slice());
        let turn = self.books.lifecycle.try_take_devices(id, devices)?;
        let after = self.books.store.read_lifecycle(id)?;
        if before.as_ref().map(|r| &r.spec) != after.as_ref().map(|r| &r.spec) {
            return Err(uncertain(id, "its record's spec changed during its call"));
        }
        Ok(Some(turn))
    }

    fn runtime_at(&self, id: &BoxId) -> Result<&dyn Runtime, Refusal> {
        match self.books.store.read_lifecycle(id)? {
            Some(record) => self.runtime.runtime_for(&record.spec),
            None => Ok(&self.runtime),
        }
    }
    /// `runtime`'s boxes, their records in `state_dir`. Does not recover; call [`Recorded::recover`]. The records
    /// that needn't outlive the host ([`crate::store`]) are kept beside the rest, in `@records` under it (`@` is in
    /// no box id, so it is never read as a box's record): these OSes give the node no tmpfs runtime directory.
    pub fn open(runtime: R, state_dir: &Path) -> Result<Self, Refusal> {
        Self::open_using(runtime, state_dir, None)
    }

    /// The native node's single Prober/Gpus. Construct it once in the native CLI and pass this
    /// Arc before guest registration/recovery. A separate scheduler reads its flat RPC report.
    pub fn open_with_prober(
        runtime: R,
        state_dir: &Path,
        prober: crate::facts::SharedProber,
    ) -> Result<Self, Refusal> {
        Self::open_using(runtime, state_dir, Some(prober))
    }

    fn open_using(
        runtime: R,
        state_dir: &Path,
        prober: Option<crate::facts::SharedProber>,
    ) -> Result<Self, Refusal> {
        let store = Store::open(state_dir, &state_dir.join("@records"))?;
        // Before this node writes any record: so every record written from here is known as this boot's.
        let last_boot = store.take_boot_marker()?;
        Ok(Self {
            runtime,
            wire_lessors: std::sync::OnceLock::new(),
            books: Arc::new(Books {
                store,
                prober,
                lifecycle: Turns::new(
                    "changing a recorded box's lifecycle",
                    "recorded lifecycle methods",
                    crate::store::TURNS,
                    // A method's turn never waits (try_take): an overlap is refused at once, no timeout guessed. A
                    // record transaction's waits for its holder (take), bounded by the slowest hold seen and following
                    // the holder past it (decision 9); the existing Turns owner records each hold.
                    hex_core::patience::Patience::measuring("recorded.lifecycle"),
                ),
                abandoned: Abandoned::default(),
                adopted: Mutex::new(BTreeSet::new()),
                last_boot,
                charges: std::sync::OnceLock::new(),
                node_vm_ends: Mutex::new(Vec::new()),
                capacity_view: std::sync::atomic::AtomicBool::new(false),
                reserved: Mutex::new(None),
            }),
        })
    }

    /// After a restart: takes each box back or cleans it up as its record says (T-T7.1.2), by
    /// `hex_core::recovery`'s decisions, Linux's node's. A box is a record and the runtime's processes it
    /// names: a record made is the box, whatever runs in it now, so it is taken back; one being made or
    /// removed when the node stopped is removed, with what its record names ended. Safe to call again after
    /// a crash during it: ending a box tolerates its processes being gone already.
    /// After the caller's one Arc, give the native owner a weak view of this SAME authority, as
    /// [`Recorded::bind_guest_books`] does for a host beside its guest. No second owner is made.
    pub fn bind_native_books(self: &Arc<Self>) -> Result<(), Refusal>
    where
        R: 'static,
    {
        let authority: Arc<dyn hex_runtime::devices::Books> = self.clone();
        self.runtime.bind_native_books(Arc::downgrade(&authority))
    }

    pub fn recover(&self) -> Result<RecoverySummary, Refusal> {
        let (records, torn) = self.preflight_snapshot()?;
        self.runtime.initialize_native()?;
        // What a crash at a record write left: a temp file never renamed into place.
        self.books.store.remove_strays()?;
        let settled = self.books.store.settle_node_vms()?;
        // Each oci box a node VM's last-resort end ended, retired with it before recovery plans it (A-6): kept with
        // why, never removed as a box nothing runs, its node VM's run ended with an earlier node or before it.
        let retired = charges::retire_with_node_vms(
            &self.books.store,
            &records,
            &self.books.store.all_node_vms()?,
        )?;
        let mut summary = RecoverySummary {
            boot_unknown: self.books.store.boot_unread().map(str::to_owned),
            node_vms: settled.rows,
            node_vm_ends: crate::store::NodeVmEnds::of(&settled.ends)?,
            ..RecoverySummary::default()
        };
        self.books
            .node_vm_ends
            .lock()
            .map_err(|_| broken("the node VM ends' lock was poisoned"))?
            .extend(settled.ends);
        let torn = self.remove_torn_before_this_boot(torn, &mut summary)?;
        self.recover_orphans(&records, torn, &mut summary)?;
        let mut seen: BTreeMap<BoxId, Seen> = BTreeMap::new();
        for (id, record) in &records {
            // Without a readable class/identity this owner cannot choose a cleanup backend.
            // Retain the row and its claims rather than run native host cleanup by guesswork.
            let r = record.as_ref().map_err(|_| {
                uncertain(
                    id,
                    "its record can't be read, so no cleanup backend can be chosen",
                )
            })?;
            // A retired box ran nothing past its retirement and is kept for its owner's delete (joint plan step 14),
            // one retired with its node VM just now too (A-6).
            if r.retired || retired.contains(id) {
                continue;
            }
            // T-T7.1.2: a box this node keeps the record of, written in an earlier boot of the host, ran
            // nothing that outlived that boot. A backend that keeps its own records proves its own.
            let earlier_boot =
                !self.runtime.owns_records(&r.spec)? && self.books.store.earlier_boot(r);
            let alive = match record {
                // Incomplete creates and durable deletes are removed, never adopted. Asking alive
                // first would require a pending native create to look Ready before cleanup can run.
                Ok(r) if r.phase == Phase::Ready && !earlier_boot => {
                    match self.runtime.alive(&r.spec) {
                        Ok(alive) => alive,
                        Err(Refusal::Awaits {
                            needs: hex_api::Upstream::AttachAfterCrash,
                        }) => {
                            summary.held.push((id.clone(), IMAGE_HELD.to_owned()));
                            continue;
                        }
                        Err(why) => return Err(why),
                    }
                }
                Ok(_) => None,
                Err(_) => None,
            };
            seen.insert(
                id.clone(),
                Seen {
                    record: Some(record.clone()),
                    // A record is the box here, whatever runs in it now: its runtime's part stands for the
                    // cgroup Linux's node finds.
                    parts: Parts {
                        cgroup: true,
                        ..Parts::default()
                    },
                    init_alive: alive,
                    start_interrupted: false,
                    earlier_boot,
                },
            );
        }
        for (id, decision) in recovery::plan(&seen) {
            let record = records.get(&id).and_then(|r| r.as_ref().ok());
            let trace = record.and_then(|r| r.init);
            let managed = record
                .map(|r| self.runtime.owns_records(&r.spec))
                .transpose()?
                .unwrap_or(false);
            let _method = if managed {
                None
            } else {
                self.method_turn(&id)?
            };
            match decision {
                Decision::Adopt => {
                    if let Some(r) = record {
                        self.runtime.adopt(&r.spec, trace)?;
                    }
                    self.books
                        .adopted
                        .lock()
                        .map_err(|_| poisoned())?
                        .insert(id.clone());
                    summary.adopted.push(id);
                }
                Decision::Remove(why) => {
                    if managed {
                        let r = record.ok_or_else(|| {
                            uncertain(&id, "its recovery has no record to delete it by")
                        })?;
                        match self.runtime.runtime_for(&r.spec)?.delete(&id) {
                            Ok(()) => {}
                            Err(Refusal::Awaits {
                                needs: hex_api::Upstream::AttachAfterCrash,
                            }) => {
                                summary.held.push((id, IMAGE_HELD.to_owned()));
                                continue;
                            }
                            Err(why) => return Err(why),
                        }
                    } else if let Some(r) = record {
                        // An earlier boot ended what ran of it: a process named by its record now may be
                        // another's, never ended for it.
                        if !matches!(why, Why::EarlierBoot) {
                            self.runtime.end_for(&r.spec, trace)?;
                        }
                        self.runtime.discard(&r.spec)?;
                        self.books.store.remove(&id)?;
                    } else {
                        return Err(uncertain(&id, "its recovery has no record to remove it by"));
                    }
                    summary.removed.push((id, why_text(&why)));
                }
            }
        }
        Ok(summary)
    }

    /// T-T7.1.2: the records torn in the runtime place ([`Store::take_torn`]) before this boot of the host, as
    /// the boot session marker this node took as it opened says ([`Store::take_boot_marker`]): that boot's end
    /// ended everything that ran of each, host or guest, and a torn record is kept only where nothing on disk
    /// outlives a host crash, so each is removed, saying so. The rest are this boot's, returned.
    fn remove_torn_before_this_boot(
        &self,
        torn: crate::store::Torn,
        summary: &mut RecoverySummary,
    ) -> Result<crate::store::Torn, Refusal> {
        if !self
            .books
            .store
            .before_this_boot(self.books.last_boot.as_ref())
        {
            return Ok(torn);
        }
        for (id, why) in torn {
            let _turn = self.books.lifecycle.try_take_devices(&id, &[])?;
            self.books.store.remove(&id)?;
            let said = format!(
                "its record couldn't be read ({why}) and was written before the host's latest boot: {}",
                why_text(&Why::EarlierBoot)
            );
            summary.removed.push((id, said));
        }
        Ok(Vec::new())
    }

    /// T-T7.1.2: the boxes a live guest holds under names no readable record carries, and the records torn in
    /// the runtime place ([`Store::take_torn`]), whose boxes no barrier can check. This node writes a box's
    /// record before either side makes the box, and one name names one box across host and guest, so such a
    /// box is an orphan only a torn create can have left, as Linux's node finds a cgroup no record names. Each
    /// is deleted through the guest; a torn record whose name the guest held owns nothing then, and is removed
    /// as unreadable, before its box, so a crash between leaves a box no record names, which the next recovery
    /// deletes. Every other torn record is held: kept, so its name is never reused and its box stays
    /// uncertain, as no host runtime here proves a box ended without its record, and a guest that can't answer
    /// proves nothing gone. The node starts either way, saying which.
    fn recover_orphans(
        &self,
        records: &hex_runtime::devices::Records,
        torn: crate::store::Torn,
        summary: &mut RecoverySummary,
    ) -> Result<(), Refusal> {
        let orphans = self.runtime.orphans(records);
        if let Err(refused) = &orphans {
            summary.orphans_unknown = Some(refused.to_string());
        }
        for (id, why) in &torn {
            let path = self.books.store.runtime_place(id);
            let held = match &orphans {
                Ok(orphans) if orphans.contains(id) => {
                    let _turn = self.books.lifecycle.try_take_devices(id, &[])?;
                    self.books.store.remove(id)?;
                    continue;
                }
                Ok(_) => held_text(
                    &path,
                    why,
                    "no box of its guest's has its name, and no host box of it is proved ended",
                ),
                Err(refused) => held_text(
                    &path,
                    why,
                    &format!("its guest couldn't say whether it holds the box ({refused})"),
                ),
            };
            summary.held.push((id.try_clone()?, held));
        }
        for id in orphans.into_iter().flatten() {
            let _turn = self.books.lifecycle.try_take_devices(&id, &[])?;
            self.runtime.delete_orphan(&id)?;
            let why = match torn.iter().find(|(torn, _)| *torn == id) {
                Some((_, why)) => why_text(&Why::Unreadable(why.clone())),
                None => "its guest held it and no record named it".to_owned(),
            };
            summary.removed.push((id, why));
        }
        Ok(())
    }

    fn counted(&self, info: BoxInfo) -> Result<BoxInfo, Refusal> {
        self.books.counted(info)
    }

    fn forward_runtime(
        &self,
        call: Call,
        stdio: Stdio,
        reply: Reply,
        ended: serve::Ended,
    ) -> Forward {
        let id = match &call {
            Call::Inspect(id) | Call::Start(id) | Call::Usage(id) => id,
            Call::Signal { id, .. } | Call::Exec { id, .. } | Call::ExecStreams { id, .. } => id,
            _ => return self.runtime.forward(call, stdio, reply, ended),
        };
        match self.books.store.read_lifecycle(id) {
            Ok(Some(record)) => self
                .runtime
                .forward_for(&record.spec, call, stdio, reply, ended),
            Ok(None) => self.runtime.forward(call, stdio, reply, ended),
            Err(error) => Forward::Refused(error),
        }
    }
}

/// The charge learner's reads and writes of the node's managed VMs, through their native owner, and of their records.
struct Managed<'a, R> {
    runtime: &'a R,
    books: &'a Books,
}

impl<R: Recover> charges::Managed for Managed<'_, R> {
    fn usage(&self, holder: charges::Holder<'_>) -> Result<Usage, Refusal> {
        match holder {
            charges::Holder::Box(record) => self.runtime.managed_usage(NativeRecord::Box(record)),
            // A node VM is read by its run's host use alone, as an image VM is (`charges::Read::of`).
            charges::Holder::Node { .. } => Err(Refusal::Invalid {
                what: "a node VM's use",
                why: "a node VM is read by its run's host use, as an image VM is".to_owned(),
            }),
        }
    }

    fn guest_time(
        &self,
        holder: charges::Holder<'_>,
    ) -> Result<hex_runtime::guest::ManagedGuestTime, Refusal> {
        match holder {
            charges::Holder::Box(record) => self.runtime.managed_guest_time(record),
            // A node VM's vCPUs' guest time comes with its run's host use, as an image VM's does (D241).
            charges::Holder::Node { .. } => Err(Refusal::Invalid {
                what: "a node VM's guest time",
                why: "a node VM's guest time comes with its run's host use".to_owned(),
            }),
        }
    }

    fn life(&self, holder: charges::Holder<'_>) -> Result<std::time::Duration, Refusal> {
        match holder {
            charges::Holder::Box(record) => self.runtime.managed_life(record),
            // A node VM's life comes with its run's host use, its window, as an image VM's does.
            charges::Holder::Node { .. } => Err(Refusal::Invalid {
                what: "a node VM's life",
                why: "a node VM's life comes with its run's host use".to_owned(),
            }),
        }
    }

    fn cap_cycle(&self, rate: u64) -> Result<hex_runtime::guest::CapCycle, Refusal> {
        self.runtime.cap_cycle(rate)
    }

    /// An image VM's governor's least, a node VM's as an image VM's (its run's governor); a managed VM's its platform's
    /// cap's, which holds its share.
    fn least(&self, holder: charges::Holder<'_>) -> u64 {
        match holder {
            charges::Holder::Box(record) if !charges::running_image(record) => {
                self.runtime.managed_least()
            }
            charges::Holder::Box(_) | charges::Holder::Node { .. } => charges::GOVERNOR_LEAST.get(),
        }
    }

    fn host_use(&self, holder: charges::Holder<'_>) -> Result<hex_api::HostUse, Refusal> {
        match holder {
            charges::Holder::Box(record) => self.runtime.host_use(&record.spec.id),
            charges::Holder::Node { row, .. } => self.runtime.node_vm_host_use(row.run),
        }
    }

    fn taken(&self, holder: charges::Holder<'_>) -> Result<u64, Refusal> {
        match holder {
            charges::Holder::Box(record) if charges::running(record) => self
                .runtime
                .managed_usage(NativeRecord::Box(record))
                .map(|usage| usage.memory_bytes),
            charges::Holder::Box(record) => self
                .runtime
                .usage(&record.spec.id)
                .map(|usage| usage.memory_bytes),
            charges::Holder::Node { row, .. } => self
                .runtime
                .node_vm_host_use(row.run)
                .map(|host| host.footprint_bytes),
        }
    }

    fn free(&self) -> Result<hex_api::CapacityResources, Refusal> {
        Ok(crate::facts::capacity(self.books.store.directory(), self.books.authority_id()?)?.free)
    }

    fn rate_period(
        &self,
        holder: charges::Holder<'_>,
    ) -> Result<Option<std::time::Duration>, Refusal> {
        let record = match holder {
            charges::Holder::Box(record) => record,
            charges::Holder::Node { row, .. } => {
                return self.runtime.node_vm_rate_period(row.run);
            }
        };
        if charges::running_image(record) {
            return self.runtime.image_rate_period(record);
        }
        self.runtime.managed_rate_period(record)
    }

    /// D255, D260: the VM's governor holds its vCPUs to its share less its VMM's own (`Cap::governor`): an image VM's
    /// through its rate (D247), a managed VM's through the guest its node boots it in. On Windows a managed VM's job's
    /// CPU rate is set at the share beside, the hard ceiling over its VMM's own and its vCPUs' together, which binds
    /// only while its VMM's own isn't read apart; on macOS the CPU control stays at the VM's ceiling, a backstop. Where
    /// its VM process takes no rate, its platform's cap holds it at the share, told why.
    fn set_rate(
        &self,
        holder: charges::Holder<'_>,
        cap: charges::Cap<'_>,
    ) -> Result<hex_api::RateHeld, Refusal> {
        let record = match holder {
            charges::Holder::Box(record) => record,
            // A node VM is a D231 image run, held through its run's rate as an image VM is (place 3).
            charges::Holder::Node { row, .. } => {
                return self
                    .runtime
                    .node_vm_rate(row.run, Some(cap.governor()))
                    .map(|_| cap.held(false));
            }
        };
        if charges::running_image(record) {
            return self
                .runtime
                .rate(&record.spec.id, Some(cap.governor()))
                .map(|_| cap.held(false));
        }
        match self.runtime.managed_rate(record, cap.governor().get())? {
            hex_runtime::guest::ManagedRate::Governor { .. } => {
                if cfg!(windows) {
                    self.runtime
                        .set_managed_rate(NativeRecord::Box(record), cap.share)?;
                }
                Ok(cap.held(cfg!(windows)))
            }
            hex_runtime::guest::ManagedRate::Platform(why) => {
                self.runtime
                    .set_managed_rate(NativeRecord::Box(record), cap.share)?;
                Ok(hex_api::RateHeld::Platform(why.to_string()))
            }
        }
    }

    fn set_room(&self, holder: charges::Holder<'_>, room: i64) -> Result<(), Refusal> {
        match holder {
            charges::Holder::Box(record) => {
                self.runtime.room(&record.spec.id, Some(room)).map(|_| ())
            }
            // Its guest told its room through its run, as an image VM's is (D188).
            charges::Holder::Node { row, .. } => self.runtime.node_vm_room(row.run, room),
        }
    }

    fn stop(&self, holder: charges::Holder<'_>) -> Result<(), Refusal> {
        let record = match holder {
            charges::Holder::Box(record) => record,
            charges::Holder::Node { row, .. } => return self.runtime.stop_node_vm(row.run),
        };
        if charges::running_image(record) {
            return self.runtime.signal(&record.spec.id, Signal::KILL);
        }
        self.runtime.stop_managed(NativeRecord::Box(record))
    }

    fn intend(
        &self,
        id: &hex_api::LearnedHolder,
        scope: charges::Scope,
        end: &hex_api::LearnedEnd,
    ) -> Result<bool, Refusal> {
        charges::record_last_resort(&self.books.store, id, scope, Some(end))
    }
}

/// A node's charge learner as it is set ([`Recorded::start_charges`]): the receiver its thread waits on, its window, and
/// what of the rule it can't hold.
type Charges = (
    hex_wait::mpsc::Receiver<()>,
    Option<std::time::Duration>,
    Vec<Refusal>,
);

impl<R: Recover + Send + Sync + 'static> Recorded<R> {
    /// Joint plan step 14 (C1, C4, C5): starts the charge learner of the node's VMs, a thread that runs a window each
    /// time its window passes, until the node is gone, and says its window and what of the rule it can't hold, typed: a
    /// managed VM's memory budget, or, where the platform's cap derives no cycle (`Recover::cap_cycle`), why its managed
    /// VMs are learned over their governors' periods alone. Its window is the longest period of the caps it holds,
    /// re-read each window: each image VM's its run's own (`Recover::image_rate_period`), each governed managed VM's its
    /// governor's, and each VM its platform's cap holds that cap's cycle at its share. Its first window runs as it
    /// starts, so what its node holds then (the VMs its recovery took back, its node VM) is read, capped and told at
    /// once (A-9); holding no cap, it then waits for a box to begin or a node VM to boot (`charges::learn`).
    pub fn start_charges(
        self: &Arc<Self>,
    ) -> Result<(Option<std::time::Duration>, Vec<Refusal>), Refusal> {
        let (waits, window, unheld) = self.set_charges()?;
        let node = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("hex-charges".to_owned())
            .spawn(move || {
                let learned =
                    charges::learn(&waits, || node.upgrade().map(|node| node.learn_window()));
                // It can't wait for a window: it says so where its failures are kept, and stops.
                if let Err(why) = learned
                    && let Some(node) = node.upgrade()
                    && let Some((learner, _)) = node.books.charges.get()
                    && let Ok(mut learner) = learner.lock()
                {
                    learner.failed(why);
                }
            })
            .map_err(|e| Refusal::os("starting the charge learner", e))?;
        Ok((window, unheld))
    }

    /// The charge learner of the node's VMs, kept where its capacity answers read it ([`Recorded::start_charges`]),
    /// with the receiver its thread waits on, told each box's beginning ([`Recorded::begun`]), its window and what of the
    /// rule it can't hold.
    fn set_charges(&self) -> Result<Charges, Refusal> {
        // Whether its platform's cap derives a cycle at all, asked at its least.
        let managed = self.runtime.cap_cycle(self.runtime.managed_least());
        let mut learner = charges::Learner::new(managed.is_ok(), self.machine()?)?;
        // What its node's recovery read back from the node VM rows it let go (joint plan step 39, place 1).
        learner.carry_ended(std::mem::take(
            &mut *self
                .books
                .node_vm_ends
                .lock()
                .map_err(|_| broken("the node VM ends' lock was poisoned"))?,
        ))?;
        let window = learner.next_window();
        let mut unheld = Vec::new();
        unheld
            .try_reserve_exact(1)
            .map_err(|_| Refusal::Exhausted {
                what: "the charge learner's refusals",
                bound: 1,
            })?;
        unheld.push(match managed {
            Ok(_) => learner.unheld().clone(),
            Err(why) => why,
        });
        // Each box's beginning is told on it ([`Recorded::begun`]); its end is its sender's drop, with the node.
        let (begun, waits) = hex_wait::mpsc::sync_channel(1)
            .map_err(|why| Refusal::os("making the charge learner's wake", why))?;
        self.books
            .charges
            .set((Mutex::new(learner), Mutex::new(begun)))
            .map_err(|_| broken("the charge learner is started already"))?;
        Ok((waits, window, unheld))
    }

    /// The machine's resources, as its facts say them.
    fn machine(&self) -> Result<hex_api::CapacityResources, Refusal> {
        Ok(crate::facts::capacity(self.books.store.directory(), self.books.authority_id()?)?.total)
    }

    /// One window of the charge learner (joint plan step 14): the running managed VMs in the node's records, and its node
    /// VMs (step 39, place 3), within the machine less what its native boxes are capped at (C4: their bookings off the
    /// fleet's budget). Its failure is kept for the capacity answers. Answers the window it reads over next.
    fn learn_window(&self) -> Option<std::time::Duration> {
        let (shared, _) = self.books.charges.get()?;
        // Its learner is taken for each pure step and let go while the node's records and VMs are read and its VMs acted
        // on (A-2), so a capacity answer, which takes it, never waits on a record or a VM.
        let fail = |why: Refusal| {
            let mut learner = shared.lock().ok()?;
            learner.failed(why);
            learner.next_window()
        };
        // What the capacity view's holders have reserved, as the node's configuration and its keeper's word say (A-10):
        // until the keeper of a view that serves its machine has spoken since the node started, its VMs are booked whole,
        // none learned, told so, and its keeper's word wakes it.
        let reservations = match self.books.reservations() {
            Ok(reservations) => reservations,
            Err(why) => return fail(why),
        };
        let reserved = {
            let Ok(mut learner) = shared.lock() else {
                return None;
            };
            learner.told_reservations(reservations);
            match reservations {
                hex_api::LearnedReservations::Unheard => return learner.next_window(),
                hex_api::LearnedReservations::Heard { reserved, .. } => reserved,
                hex_api::LearnedReservations::Unserved => hex_api::CapacityResources::default(),
            }
        };
        // The node's records and node VM rows as its store holds them: pointers, no file read (the review's per-window
        // reads).
        let read = self.books.store.snapshot().and_then(|records| {
            let rows = self.books.store.all_node_vms()?;
            // Each oci box a node VM's last-resort end ended, retired with it once its run has ended (A-6), off the
            // learner's lock as every write into a box's record is.
            charges::retire_with_node_vms(&self.books.store, charges::entries(&records), &rows)?;
            Ok((records, rows, self.machine()?))
        });
        let (records, rows, machine) = match read {
            Ok(read) => read,
            Err(why) => return fail(why),
        };
        // What ending its node VM ends: its node's oci boxes, an unreadable record among them (place 3).
        let band = charges::node_band(charges::entries(&records));
        // Each record it can't read is counted at its machine and told, never dropped (the review's dropped records).
        let (records, unreadable) = match charges::readable(&records) {
            Ok(split) => split,
            Err(why) => return fail(why),
        };
        let io = Managed {
            runtime: &self.runtime,
            books: &self.books,
        };
        // Each managed VM it hasn't seen is asked its governor's period first, where the platform has no window (D255).
        if let Err(why) =
            charges::boxes(&records).and_then(|holders| charges::govern(shared, &io, &holders))
        {
            return fail(why);
        }
        // The VMs it learns; the rest of the node's boxes off the fleet's budget, native boxes at their caps and VMs of
        // a kind it doesn't learn whole (C4).
        let sorted = {
            let Ok(learner) = shared.lock() else {
                return None;
            };
            charges::sort(&records, &rows, |holder| learner.learns(holder))
        };
        let mut sorted = match sorted {
            Ok(sorted) => sorted,
            Err(why) => return fail(why),
        };
        // Each node VM that boots, its booking off the fleet's budget until it is Ready and learned (step 39).
        if let Err(why) = charges::booting(&rows, &band).and_then(|booting| {
            sorted
                .booked
                .try_reserve(booting.len())
                .map_err(|_| Refusal::Exhausted {
                    what: "a window's holders",
                    bound: booting.len(),
                })?;
            sorted.booked.extend(booting);
            Ok(())
        }) {
            return fail(why);
        }
        // Each node VM that runs, learned as an image VM is, over its run's period (joint plan step 39, place 3).
        let nodes = charges::nodes(&rows, &band).and_then(|nodes| {
            sorted
                .learned
                .try_reserve(nodes.len())
                .map_err(|_| Refusal::Exhausted {
                    what: "a window's VMs",
                    bound: nodes.len(),
                })?;
            Ok(nodes)
        });
        match nodes {
            Ok(nodes) => sorted.learned.extend(nodes),
            Err(why) => return fail(why),
        }
        // Its outcome is kept as the learner's fault, which the capacity answers read.
        let _ = charges::window(
            shared,
            &io,
            &sorted.learned,
            unreadable,
            |held| charges::measured(&io, machine, &sorted.booked, held, reserved),
            std::time::Instant::now,
        );
        // Each end the last resort made goes into its box's record (C5), off the learner's lock; a write that fails
        // is kept for the next window and fails this one.
        let (next, ends) = {
            let Ok(mut learner) = shared.lock() else {
                return None;
            };
            (learner.next_window(), learner.unrecorded())
        };
        let (left, failed) = charges::write_ends(&self.books.store, ends);
        if let Some(why) = failed
            && let Ok(mut learner) = shared.lock()
        {
            learner.keep_unrecorded(left);
            learner.failed(why);
        }
        next
    }
}

impl<P: Recover + 'static> Recorded<hex_runtime::guest::WithGuest<P>> {
    /// After constructing the CLI's one Arc, register a weak view of that same allocation.
    /// The guest holds no strong back-reference and no second Store/holder owner is created.
    pub fn bind_guest_books(self: &Arc<Self>) -> Result<(), Refusal> {
        let authority: Arc<dyn hex_runtime::devices::Books> = self.clone();
        let weak = Arc::downgrade(&authority);
        self.runtime.host().bind_native_books(weak.clone())?;
        self.runtime.guest().bind_books(weak)?;
        // The session this node's Store stamps its records with, so the image class judges a record's boot by that
        // stamp (T-T7.1.2): none where the Store reads none (Windows).
        self.runtime
            .guest()
            .bind_boot_session(self.books.store.boot().cloned())
    }

    /// The lanes its guest under shards-init admits at once, as this node fitted them with that guest's among its
    /// descriptors (joint plan step 39): set once, before that guest boots.
    pub fn set_guest_lanes(&self, lanes: u32) -> Result<(), Refusal> {
        self.runtime.guest().set_lanes(lanes)
    }

    /// Boots its guest now, so no create pays a boot: a guest under shards-init, which no shared backing's grant
    /// boots (joint plan step 39).
    pub fn boot_guest(&self) -> Result<(), Refusal> {
        self.runtime.guest().boot()
    }

    /// Publish only the configured shared backing after the complete retained-owner preflight.
    /// This does not boot a VM or sample GPU capacity; Index still owns its later actual grant.
    pub fn prepare_guest_backing(&self) -> Result<hex_api::BackingOffer, Refusal> {
        self.preflight_recovery()?;
        self.runtime.guest().prepare_backing()
    }

    /// Recover the already-admitted backing only after every retained sibling/configuration is
    /// known. Payload recovery follows this call and uses the same Store/turn/native owner.
    pub fn recover_guest_backing(&self) -> Result<(), Refusal> {
        self.initialize_native()?;
        self.runtime.guest().recover_backing()
    }
}

impl Books {
    /// What the charge learner takes off its fleet's budget for the capacity view's holders now (the review's A-10), as
    /// the node's configuration and its keeper's latest word say ([`charges::reservations`]).
    fn reservations(&self) -> Result<hex_api::LearnedReservations, Refusal> {
        let word = *self
            .reserved
            .lock()
            .map_err(|_| broken("the keeper's word's lock was poisoned"))?;
        Ok(charges::reservations(
            self.capacity_view.load(std::sync::atomic::Ordering::SeqCst),
            word,
        ))
    }

    /// What a VM class's capacity answer carries of the charge learner (joint plan step 14, C8), fitted to one message:
    /// nothing where no learner runs.
    fn attach_learned(
        &self,
        class: Class,
        capacity: &mut hex_api::Capacity,
        records: &crate::store::Snapshot,
    ) -> Result<(), Refusal> {
        let class = match class {
            Class::Shards => hex_sim::model::Class::Shards,
            Class::ShardsImage => hex_sim::model::Class::ShardsImage,
            Class::Firecracker => hex_sim::model::Class::Firecracker,
            Class::Process | Class::Oci => return Ok(()),
        };
        let Some((learner, _)) = self.charges.get() else {
            return Ok(());
        };
        // Its node VMs' holdings too, each by its run (joint plan step 39, place 3).
        let rows = self.store.all_node_vms()?;
        let band = charges::node_band(charges::entries(records));
        let nodes = charges::nodes(&rows, &band)?;
        // Its holders are the records it reads: a box whose record it can't read is told from its last window, counted
        // at its machine (the review's dropped records).
        learner
            .lock()
            .map_err(|_| broken("the charge learner's lock was poisoned"))?
            .answer(
                class,
                capacity,
                records
                    .values()
                    .filter_map(|record| record.as_ref().as_ref().ok())
                    .map(charges::Holder::Box)
                    .chain(nodes),
            )
    }

    /// The same Store identity backs both initialized backend capacities and the native report.
    /// It introduces no device inventory or reservation state.
    fn authority_id(&self) -> Result<String, Refusal> {
        let name = self
            .store
            .directory()
            .to_str()
            .ok_or_else(|| broken("recorded authority path is not Unicode"))?;
        let bytes = "recorded:"
            .len()
            .checked_add(name.len())
            .ok_or(Refusal::Exhausted {
                what: "recorded authority identity bytes",
                bound: usize::MAX,
            })?;
        let mut id = String::new();
        id.try_reserve_exact(bytes)
            .map_err(|_| Refusal::Exhausted {
                what: "recorded authority identity bytes",
                bound: bytes,
            })?;
        id.push_str("recorded:");
        id.push_str(name);
        Ok(id)
    }

    fn forward_turn(self: &Arc<Self>, id: &BoxId) -> Result<ForwardTurn, Refusal> {
        let record = self.store.read_lifecycle(id)?;
        let devices = record
            .as_ref()
            .map_or(&[][..], |r| r.spec.devices.as_slice());
        let hold = self.lifecycle.try_hold_devices(id, devices)?;
        let expected = match record.map(|record| projection(record.spec)).transpose() {
            Ok(expected) => expected,
            Err(error) => {
                self.lifecycle.release(hold);
                return Err(error);
            }
        };
        // This clones the existing Books allocation, not a new owner/table. Live receipts remain
        // bounded by the same TURNS admission as synchronous methods.
        Ok(ForwardTurn {
            books: Arc::clone(self),
            hold: Some(hold),
            expected,
        })
    }
    /// `info` with the abandoned programs counted for its box, recorded and not yet, and whether a recovery
    /// took it back.
    fn counted(&self, mut info: BoxInfo) -> Result<BoxInfo, Refusal> {
        let recorded = match self.store.get(&info.id)? {
            Some(Ok(record)) => {
                let abandoned = record.abandoned;
                // Why the last resort ended it, where it did (joint plan step 14, C5), across the node's restarts.
                charges::with_last_resort(&mut info, &record)?;
                project_info(&mut info, &projection(record.spec)?)?;
                abandoned
            }
            Some(Err(why)) => {
                return Err(uncertain(
                    &info.id,
                    &format!("its record can't be read: {why}"),
                ));
            }
            None => 0,
        };
        info.abandoned = recorded.saturating_add(self.abandoned.unrecorded(&info.id)?);
        info.adopted |= self
            .adopted
            .lock()
            .map_err(|_| poisoned())?
            .contains(&info.id);
        Ok(info)
    }
}

fn poisoned() -> Refusal {
    Refusal::Broken("the node's adopted boxes' lock was poisoned".to_owned())
}

impl<R: Recover> Runtime for Recorded<R> {
    /// AC-T7.4.1: the Index's grant reaches the backing's owner, whose pool transactions are this
    /// node's Books (bound before it serves). Without it the trait's default refused every grant
    /// Pending, so no pool of a node served this way (macOS, Windows) could go Live.
    fn initialize_backing(
        &self,
        grant: hex_api::BackingGrant,
    ) -> Result<hex_api::BackingOffer, Refusal> {
        grant.check()?;
        self.runtime.initialize_backing(grant)
    }

    fn release_backing(&self, key: hex_api::BackingKey) -> Result<hex_api::BackingOffer, Refusal> {
        self.runtime.release_backing(key)
    }

    fn backing_usage(&self, key: &hex_api::BackingKey) -> Result<Usage, Refusal> {
        let record = self
            .books
            .store
            .read_pool(&key.pool)?
            .filter(|record| record.key == *key)
            .ok_or_else(|| Refusal::Backing {
                key: *key,
                cause: hex_api::LifecycleCause::Uncertain,
                why: Some("its node holds no pool record of it to read usage from".to_owned()),
            })?;
        let mut result = None;
        hex_runtime::devices::Books::with_pool_transaction(
            self,
            key,
            &record.devices,
            &mut |tx| {
                result = Some(tx.managed_usage());
                Ok(())
            },
        )?;
        result.ok_or_else(|| broken("the native backing usage transaction was not invoked"))?
    }

    fn physical_report(&self) -> Result<hex_api::PhysicalReport, Refusal> {
        let prober = self
            .books
            .prober
            .as_ref()
            .ok_or_else(|| broken("the recorded owner has no initialized native provider"))?;
        let authority = self.books.authority_id()?;
        let mut report = {
            prober
                .lock()
                .map_err(|_| broken("the shared native provider lock was poisoned"))?
                .physical_report(authority)?
        }; // Native pool/query lock is released before any VM/Books/remote observation.
        crate::facts::merge_physical_observations(
            &mut report,
            Class::Shards,
            self.runtime.physical_observations(),
        )?;
        Ok(report)
    }

    fn capacity(&self, class: Class) -> Result<Option<hex_api::Capacity>, Refusal> {
        let Some(mut report) = self.runtime.capacity(class)? else {
            return Ok(None);
        };
        if !report.devices.is_empty() {
            native_metadata(
                &mut report,
                hex_runtime::devices::Books::physical_devices(self)?,
            )?;
        }
        // Its devices' holders from a copy of every record, where it offers a device; its learned holders from the
        // records as the store holds them, no copy.
        if report.devices.is_empty() {
            report.check()?;
        } else {
            recorded_holders(
                &mut report,
                &self.books.store.all()?,
                &self.books.store.all_pools()?,
                self.runtime.device_catalog()?.as_deref(),
            )?;
        }
        self.books
            .attach_learned(class, &mut report, &self.books.store.snapshot()?)?;
        Ok(Some(report))
    }

    fn classes(&self) -> Vec<Class> {
        self.runtime.classes()
    }

    /// Records the box as being made before anything is made, so a crash leaves a half-made box known as
    /// one (T-T7.1.2), then as made once the runtime has made it; a refused create leaves no record.
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        spec.check()?;
        if self.runtime.owns_records(&spec)? {
            let info = self.runtime.create(spec)?;
            self.begun();
            return self.counted(info);
        }
        let _lifecycle = self
            .books
            .lifecycle
            .try_take_devices(&spec.id, &spec.devices)?;
        let catalog = self.runtime.device_catalog()?;
        check_box_claims(&self.books.store, catalog.as_deref(), &spec)?;
        let devices = !spec.devices.is_empty();
        if devices {
            if self.books.store.get(&spec.id)?.is_some() {
                return Err(Refusal::Exists(spec.id));
            }
            let report =
                Runtime::capacity(self, spec.class.class())?.ok_or_else(|| Refusal::Platform {
                    what: "devices",
                    platform: std::env::consts::OS,
                    why: "runtime reports no native attachment catalog".into(),
                })?;
            report.check()?;
            if spec.devices.iter().any(|id| {
                !report
                    .devices
                    .iter()
                    .any(|offer| offer.id == *id && offer.holder.is_none())
            }) {
                return Err(Refusal::Invalid {
                    what: "devices",
                    why: "requested physical identity is unavailable in the native catalog".into(),
                });
            }
        }
        let record = BoxRecord::new(spec.clone(), Phase::Creating, Vec::new());
        self.books.store.begin(&record)?;
        // A returned error is not proof of physical absence for any backend. Delete/recovery owns
        // cleanup; keep Creating and its exact claims even when the backend could not publish.
        let info = self.runtime.create(spec)?;
        self.books.store.made(&record)?;
        self.begun();
        self.counted(info)
    }

    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        let _lifecycle = self.method_turn(id)?;
        self.runtime_at(id)?.start(id)?;
        self.begun();
        Ok(())
    }

    /// Its runtime's: macOS's and Windows' `process` classes run the program in a box of its own life, which a thread
    /// of the run's removes once the program ends (`hex_runtime::run`, decision 7), and an `oci` spec's run is refused
    /// everywhere. No record is written for the box: it is held in its runtime's memory alone.
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        spec.check_no_devices(std::env::consts::OS)?;
        self.runtime.run(spec, program, stdio)
    }

    /// Runs `program`, and once the box has processes, records what names them after a crash.
    fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        let _lifecycle = self.method_turn(id)?;
        let proc = self.runtime_at(id)?.exec(id, program, stdio)?;
        if _lifecycle.is_some()
            && let Some(Ok(record)) = self.books.store.get(id)?
            && record.init.is_none()
            && let Some(trace) = self.runtime.trace(id)?
        {
            self.books.store.update(id, |r| {
                r.init = Some(trace);
                true
            })?;
        }
        Ok(proc)
    }

    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        let _lifecycle = self.method_turn(id)?;
        self.runtime_at(id)?.signal(id, signal)
    }

    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.counted(self.runtime_at(id)?.inspect(id)?)
    }

    /// The box's runtime answers it, counted as its inspect is (the review's B-6): an image VM's start followed to its
    /// end, never the trait's inspect at once.
    fn started(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.counted(self.runtime_at(id)?.started(id)?)
    }

    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.runtime_at(id)?.usage(id)
    }

    fn member(&self, id: &BoxId, member: &hex_api::Member) -> Result<Box<dyn Proc>, Refusal> {
        self.runtime_at(id)?.member(id, member)
    }

    fn mcp(&self, id: &BoxId, member: &hex_api::Member) -> Result<Box<dyn Proc>, Refusal> {
        self.runtime_at(id)?.mcp(id, member)
    }

    fn exec_member(
        &self,
        id: &BoxId,
        member: &hex_api::Member,
        program: Program,
        stdio: Stdio,
    ) -> Result<Box<dyn Proc>, Refusal> {
        self.runtime_at(id)?.exec_member(id, member, program, stdio)
    }

    fn link(&self, id: &BoxId) -> Result<hex_api::ControlLink, Refusal> {
        self.runtime_at(id)?.link(id)
    }

    fn signal_member(
        &self,
        id: &BoxId,
        member: &hex_api::Member,
        signal: Signal,
    ) -> Result<(), Refusal> {
        self.runtime_at(id)?.signal_member(id, member, signal)
    }

    fn room(&self, id: &BoxId, room: Option<i64>) -> Result<Option<u64>, Refusal> {
        self.runtime_at(id)?.room(id, room)
    }

    fn rate(
        &self,
        id: &BoxId,
        rate: Option<std::num::NonZeroU64>,
    ) -> Result<Option<std::num::NonZeroU64>, Refusal> {
        self.runtime_at(id)?.rate(id, rate)
    }

    fn host_use(&self, id: &BoxId) -> Result<hex_api::HostUse, Refusal> {
        self.runtime_at(id)?.host_use(id)
    }

    /// Kiln plan P5.5b: the box's runtime pauses it, an image VM's its class's; its record is unchanged, its claim and
    /// its room held, as a pause ends no run.
    fn pause(&self, id: &BoxId) -> Result<(), Refusal> {
        self.runtime_at(id)?.pause(id)
    }

    fn resume(&self, id: &BoxId) -> Result<(), Refusal> {
        self.runtime_at(id)?.resume(id)
    }

    /// Shards' D270 slice 4: the box's runtime swaps its member's component, an image VM's its class's; its record is
    /// unchanged, as a member's start again ends no run.
    fn swap(
        &self,
        id: &BoxId,
        at: &str,
        digest: &str,
        when: hex_api::SwapWhen,
    ) -> Result<hex_api::Swapped, Refusal> {
        self.runtime_at(id)?.swap(id, at, digest, when)
    }

    fn restore(
        &self,
        id: &BoxId,
        at: &str,
        when: hex_api::SwapWhen,
    ) -> Result<hex_api::Swapped, Refusal> {
        self.runtime_at(id)?.restore(id, at, when)
    }

    fn components(&self, id: &BoxId) -> Result<Vec<hex_api::ComponentAt>, Refusal> {
        self.runtime_at(id)?.components(id)
    }

    /// Kiln plan P6b.3: the box's runtime changes its network or folders as it runs, an image VM's its class's; its
    /// record is unchanged, as a change is its run's alone (docs/design/shards-image-class.md §14).
    fn change(&self, id: &BoxId, change: hex_api::Live) -> Result<(), Refusal> {
        self.runtime_at(id)?.change(id, change)
    }

    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.runtime
            .list()?
            .into_iter()
            .map(|info| self.counted(info))
            .collect()
    }

    /// Records the box as being deleted, deletes it, then removes its record: a crash between leaves a
    /// record recovery finishes.
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.delete_by(id, |runtime| runtime.delete(id), ())
    }

    /// Deletes the box as [`Runtime::delete`] does here, its runtime answering its VM's final use (the review's B-6),
    /// never the trait's plain delete answering none, read as a run that never started; none where its runtime held no
    /// such box while its record named devices, as the delete finds it gone then.
    fn delete_with_host_use(&self, id: &BoxId) -> Result<Option<hex_api::HostUse>, Refusal> {
        self.delete_by(id, |runtime| runtime.delete_with_host_use(id), None)
    }

    /// The machine's capacity view's word, kept for the charge learner (the review's A-10): the latest keeper's, an older
    /// keeper's refused, typed; the learner then told, so one waiting for its keeper's first word runs a window now.
    fn reserved(&self, word: hex_api::Reserved) -> Result<(), Refusal> {
        charges::keep_word(
            &mut *self
                .books
                .reserved
                .lock()
                .map_err(|_| broken("the keeper's word's lock was poisoned"))?,
            word,
        )?;
        self.begun();
        Ok(())
    }
}

impl<R: Recover> Recorded<R> {
    /// [`Runtime::delete`]'s course, the box's runtime deleting it through `delete`, its answer this call's: `gone`
    /// where that runtime held no such box while its record named devices.
    fn delete_by<T>(
        &self,
        id: &BoxId,
        delete: impl FnOnce(&dyn Runtime) -> Result<T, Refusal>,
        gone: T,
    ) -> Result<T, Refusal> {
        if let Some(record) = self.books.store.read_lifecycle(id)?
            && self.runtime.owns_records(&record.spec)?
        {
            let answer = delete(self.runtime.runtime_for(&record.spec)?)?;
            self.books.abandoned.forget(id);
            if let Ok(mut adopted) = self.books.adopted.lock() {
                adopted.remove(id);
            }
            return Ok(answer);
        }
        let _lifecycle = self.method_turn(id)?;
        let mut devices = false;
        if let Some(Ok(mut record)) = self.books.store.get(id)? {
            devices = !record.spec.devices.is_empty();
            record.phase = Phase::Deleting;
            self.books.store.put(&record)?;
        }
        let answer = match delete(self.runtime_at(id)?) {
            Ok(answer) => answer,
            Err(Refusal::NoSuchBox(_)) if devices => gone,
            Err(why) => return Err(why),
        };
        self.books.store.remove(id)?;
        self.books.abandoned.forget(id);
        if let Ok(mut adopted) = self.books.adopted.lock() {
            adopted.remove(id);
        }
        Ok(answer)
    }
}

impl<R: Recover> Host for Recorded<R> {
    fn attach(&self, lp: Arc<dyn Loop>) -> Result<(), Refusal> {
        self.runtime.attach(lp)
    }

    fn lessors(&self) -> Option<Arc<crate::serve::WireLessors>> {
        self.wire_lessors.get().cloned()
    }

    /// The calls its runtime answers later go there: an inspect's answer counted as any inspect's is
    /// ([`Books::counted`]). A create or a delete, which write the box's record and stage or remove its
    /// root, runs on the worker.
    fn forward(&self, call: Call, stdio: Stdio, reply: Reply, ended: serve::Ended) -> Forward {
        let id = match &call {
            Call::Inspect(id) | Call::Start(id) | Call::Usage(id) => Some(id),
            Call::Signal { id, .. } | Call::Exec { id, .. } | Call::ExecStreams { id, .. } => {
                Some(id)
            }
            _ => None,
        };
        let reply = if let Some(id) = id {
            let turn = match self.books.forward_turn(id) {
                Ok(turn) => turn,
                Err(why) => return Forward::Refused(why),
            };
            Box::new(move |mut answer| {
                if let Some(expected) = &turn.expected
                    && let Ok(Forwarded::Answer(boxed)) = &mut answer
                    && let Answer::Info(info) = boxed.as_mut()
                    && let Err(error) = project_info(info, expected)
                {
                    answer = Err(error);
                }
                reply(answer);
                drop(turn);
            }) as Reply
        } else {
            reply
        };
        match call {
            Call::Inspect(_) => {
                let books = Arc::clone(&self.books);
                let counted: Reply = Box::new(move |got| {
                    reply(match got {
                        Ok(Forwarded::Answer(answer)) => match *answer {
                            Answer::Info(info) => books
                                .counted(info)
                                .map(|info| Forwarded::Answer(Box::new(Answer::Info(info)))),
                            answer => Ok(Forwarded::Answer(Box::new(answer))),
                        },
                        other => other,
                    })
                });
                self.forward_runtime(call, stdio, counted, ended)
            }
            Call::Start(_)
            | Call::Signal { .. }
            | Call::Usage(_)
            | Call::Exec { .. }
            | Call::ExecStreams { .. } => self.forward_runtime(call, stdio, reply, ended),
            call => Forward::Back(Box::new((call, stdio))),
        }
    }

    fn abandoned(&self, id: &BoxId) -> Result<(), Refusal> {
        self.books.abandoned.count(&self.books.store, id)
    }

    /// Network namespaces are Linux's: a node elsewhere has none to count.
    fn netns(&self) -> Result<netns::Counts, Refusal> {
        Err(Refusal::Platform {
            what: "the node's network namespace counts",
            platform: std::env::consts::OS,
            why: "network namespaces are Linux's; this node makes none".to_owned(),
        })
    }
}

/// The macOS `process` class: a box's processes are its process group, named by its leader's pid and start
/// time (`proc_pid_rusage`'s Mach absolute time, in [`ProcId::start_ticks`]).
#[cfg(target_os = "macos")]
impl Recover for hex_runtime::macos::MacRuntime {
    fn bind_native_books(
        &self,
        books: std::sync::Weak<dyn hex_runtime::devices::Books>,
    ) -> Result<(), Refusal> {
        hex_runtime::macos::MacRuntime::bind_native_books(self, books)
    }
    fn project_routes(
        &self,
        pools: &[PoolRecord],
        introduced: Option<(hex_api::Execution, &[DeviceId])>,
        projection: &mut hex_runtime::devices::RouteProjection,
    ) -> Result<(), Refusal> {
        check_native_projection(pools, introduced)?;
        // MacRuntime's actual Runtime::capacity is currently the trait's None. Do not invent
        // an explicit native prefix from classes(); WithGuest appends its real configured forms.
        let _ = projection;
        Ok(())
    }

    fn preflight_unsubmitted(&self, record: &BoxRecord) -> Result<(), Refusal> {
        hex_runtime::macos::MacRuntime::preflight_unsubmitted(self, record)
    }

    fn preflight_record(&self, record: &BoxRecord) -> Result<(), Refusal> {
        hex_runtime::macos::MacRuntime::preflight_record(self, record)
    }

    fn preflight_managed(&self, record: NativeRecord<'_>) -> Result<(), Refusal> {
        hex_runtime::macos::MacRuntime::preflight_managed(self, record)
    }

    fn preflight_retained(
        &self,
        records: &hex_runtime::devices::Records,
        pools: &[PoolRecord],
    ) -> Result<(), Refusal> {
        hex_runtime::macos::MacRuntime::preflight_retained(self, records, pools)
    }

    fn managed_usage(&self, record: NativeRecord<'_>) -> Result<Usage, Refusal> {
        hex_runtime::macos::MacRuntime::managed_usage(self, record)
    }

    fn set_managed_rate(&self, record: NativeRecord<'_>, rate: u64) -> Result<(), Refusal> {
        hex_runtime::macos::MacRuntime::set_managed_rate(self, record, rate)
    }

    fn managed_life(&self, record: &BoxRecord) -> Result<std::time::Duration, Refusal> {
        hex_runtime::macos::MacRuntime::managed_life(self, started(record)?)
    }

    fn managed_least(&self) -> u64 {
        hex_runtime::macos::MacRuntime::managed_least(self)
    }

    fn stop_managed(&self, record: NativeRecord<'_>) -> Result<(), Refusal> {
        hex_runtime::macos::MacRuntime::stop_managed(self, record)
    }

    fn cap_cycle(&self, rate: u64) -> Result<hex_runtime::guest::CapCycle, Refusal> {
        hex_runtime::macos::MacRuntime::cap_cycle(self, rate)
    }

    fn launch_managed(
        &self,
        record: NativeRecord<'_>,
        plan: &hex_runtime::devices::ManagedLaunch<'_>,
        io: shards_api::LaunchIo,
        patience: Option<std::time::Duration>,
        publish: &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        hex_runtime::macos::MacRuntime::launch_managed(self, record, plan, io, patience, publish)
    }

    fn attach_managed(
        &self,
        record: NativeRecord<'_>,
        identity: Option<&shards_api::Identity>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        hex_runtime::macos::MacRuntime::attach_managed(self, record, identity)
    }

    fn finalize_managed(
        &self,
        record: NativeRecord<'_>,
        identity: Option<&shards_api::Identity>,
    ) -> Result<(), Refusal> {
        hex_runtime::macos::MacRuntime::finalize_managed(self, record, identity)
    }

    fn trace(&self, id: &BoxId) -> Result<Option<ProcId>, Refusal> {
        Ok(self.group(id)?.and_then(|(pgid, started)| {
            Some(ProcId {
                pid: u32::try_from(pgid).ok()?,
                start_ticks: started,
            })
        }))
    }

    fn adopt(&self, spec: &BoxSpec, trace: Option<ProcId>) -> Result<(), Refusal> {
        hex_runtime::macos::MacRuntime::adopt(self, spec, group(trace))
    }

    fn end(&self, trace: Option<ProcId>) -> Result<(), Refusal> {
        hex_runtime::macos::MacRuntime::end(group(trace))
    }
}

#[cfg(target_os = "macos")]
fn group(trace: Option<ProcId>) -> Option<(i32, u64)> {
    trace.and_then(|t| Some((i32::try_from(t.pid).ok()?, t.start_ticks)))
}

/// The Windows node's boxes outlive it (native plan step 8, the decision "the Windows node, on one completion
/// port"): each box's job is named for the node and the box and held by the node's keeper as well as the
/// node, and an `oci` box's container outlives the node with it. The next node takes back each box its
/// records name that is still there, and removes the rest with what is left of them.
#[cfg(windows)]
impl Recover for hex_runtime::windows::WinRuntime {
    fn project_routes(
        &self,
        pools: &[PoolRecord],
        introduced: Option<(hex_api::Execution, &[DeviceId])>,
        projection: &mut hex_runtime::devices::RouteProjection,
    ) -> Result<(), Refusal> {
        check_native_projection(pools, introduced)?;
        hex_runtime::windows::WinRuntime::project_native_encoding_upper_bound(self, projection)
    }

    fn preflight_unsubmitted(&self, record: &BoxRecord) -> Result<(), Refusal> {
        hex_runtime::windows::WinRuntime::preflight_unsubmitted(self, record)
    }

    fn preflight_record(&self, record: &BoxRecord) -> Result<(), Refusal> {
        hex_runtime::windows::WinRuntime::preflight_record(self, record)
    }

    fn preflight_managed(&self, record: NativeRecord<'_>) -> Result<(), Refusal> {
        hex_runtime::windows::WinRuntime::preflight_managed(self, record)
    }

    fn preflight_retained(
        &self,
        records: &hex_runtime::devices::Records,
        pools: &[PoolRecord],
    ) -> Result<(), Refusal> {
        hex_runtime::windows::WinRuntime::preflight_retained(self, records, pools)
    }

    fn initialize_native(&self) -> Result<(), Refusal> {
        hex_runtime::windows::WinRuntime::initialize_native(self)
    }

    fn device_catalog(&self) -> Result<Option<hex_runtime::devices::SharedCatalog>, Refusal> {
        Ok(hex_runtime::windows::WinRuntime::device_catalog(self))
    }

    fn managed_usage(&self, record: NativeRecord<'_>) -> Result<Usage, Refusal> {
        hex_runtime::windows::WinRuntime::managed_usage(self, record)
    }

    fn set_managed_rate(&self, record: NativeRecord<'_>, rate: u64) -> Result<(), Refusal> {
        hex_runtime::windows::WinRuntime::set_managed_rate(self, record, rate)
    }

    fn managed_life(&self, record: &BoxRecord) -> Result<std::time::Duration, Refusal> {
        hex_runtime::windows::WinRuntime::managed_life(self, started(record)?)
    }

    fn managed_least(&self) -> u64 {
        hex_runtime::windows::WinRuntime::managed_least(self)
    }

    fn stop_managed(&self, record: NativeRecord<'_>) -> Result<(), Refusal> {
        hex_runtime::windows::WinRuntime::stop_managed(self, record)
    }

    /// A managed VM here is held by its governor over its governor's period (D255; every managed VM is booted
    /// `--host-managed`), the job's hard cap only its ceiling, so no job's cycle is a window: a VM whose process takes no
    /// rate is booked whole, told (ROOT, 2026-11-13).
    fn cap_cycle(&self, rate: u64) -> Result<hex_runtime::guest::CapCycle, Refusal> {
        let _ = rate;
        Err(Refusal::Platform {
            what: "a managed VM's cap's cycle",
            platform: "windows",
            why: "a managed VM is held by its governor's period; a VM with no governor is booked whole".to_owned(),
        })
    }

    fn launch_managed(
        &self,
        record: NativeRecord<'_>,
        plan: &hex_runtime::devices::ManagedLaunch<'_>,
        io: shards_api::LaunchIo,
        patience: Option<std::time::Duration>,
        publish: &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        hex_runtime::windows::WinRuntime::launch_managed(self, record, plan, io, patience, publish)
    }

    fn attach_managed(
        &self,
        record: NativeRecord<'_>,
        identity: Option<&shards_api::Identity>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        hex_runtime::windows::WinRuntime::attach_managed(self, record, identity)
    }

    fn finalize_managed(
        &self,
        record: NativeRecord<'_>,
        identity: Option<&shards_api::Identity>,
    ) -> Result<(), Refusal> {
        hex_runtime::windows::WinRuntime::finalize_managed(self, record, identity)
    }

    fn trace(&self, _id: &BoxId) -> Result<Option<ProcId>, Refusal> {
        Ok(None)
    }

    /// For an `oci` box, whether its take-back would take it: its container still held by HCS, and its
    /// init the very process its record names, still running, or not yet started
    /// ([`hex_runtime::windows::WinRuntime::alive`]); a box whose init ended while no node ran is removed,
    /// its container with it.
    fn alive(&self, spec: &BoxSpec) -> Result<Option<bool>, Refusal> {
        hex_runtime::windows::WinRuntime::alive(self, spec)
    }

    /// Takes box `spec` back on this node: a `process` box by its named job, an `oci` box with its
    /// container, scratch and init ([`hex_runtime::windows::WinRuntime::adopt`]). A box's job is named for
    /// the node and the box, so it needs no per-process trace to find.
    fn adopt(&self, spec: &BoxSpec, _trace: Option<ProcId>) -> Result<(), Refusal> {
        hex_runtime::windows::WinRuntime::adopt(self, spec)
    }

    /// Windows ends a box recovery removes by its name, in [`Recover::discard`], which has its spec; a
    /// trace names nothing of a Windows box.
    fn end(&self, _trace: Option<ProcId>) -> Result<(), Refusal> {
        Ok(())
    }

    /// Ends box `spec` a recovery removes: its named job's members, and an `oci` box's container, scratch
    /// and record ([`hex_runtime::windows::WinRuntime::end`]).
    fn discard(&self, spec: &BoxSpec) -> Result<(), Refusal> {
        hex_runtime::windows::WinRuntime::check_native_spec(self, spec)?;
        hex_runtime::windows::WinRuntime::end(self, &spec.id)
    }
}

/// A node whose Linux images are its Shards guest's: its `process` boxes are taken back through the host's
/// runtime. Its `oci` boxes live in its guest, which outlives the node's crash and which the node takes back
/// when it starts (`GuestRuntime::recorded_at`): one the guest holds is taken back as it is, and one it
/// doesn't (the guest was gone, and the node booted another) is removed (`hex_core::recovery`: its init has
/// ended). Neither has host processes to end; a box recovery removes that the guest still holds is deleted
/// there.
impl<P: Recover> Recover for hex_runtime::guest::WithGuest<P> {
    fn retire_census(&self) -> Result<(), Refusal> {
        self.guest().retire_census()
    }

    fn project_routes(
        &self,
        pools: &[PoolRecord],
        introduced: Option<(hex_api::Execution, &[DeviceId])>,
        projection: &mut hex_runtime::devices::RouteProjection,
    ) -> Result<(), Refusal> {
        if pools.len() > hex_runtime::MAX_BOXES {
            return Err(Refusal::Full(hex_runtime::MAX_BOXES));
        }
        for pool in pools {
            if !matches!(
                pool.execution,
                hex_api::Execution::ShardsShared | hex_api::Execution::ShardsDedicated
            ) {
                return Err(NativeRecord::Pool(pool).uncertain(
                    "its pool's execution isn't a Shards backing's, which its routes project",
                ));
            }
        }
        if introduced.is_some_and(|(execution, _)| {
            !matches!(
                execution,
                hex_api::Execution::ShardsShared | hex_api::Execution::ShardsDedicated
            )
        }) {
            return Err(broken(
                "new backing has no initialized guest route producer",
            ));
        }
        self.host().project_routes(&[], None, projection)?;
        self.guest().project_routes(pools, introduced, projection)
    }

    fn preflight_managed(&self, record: NativeRecord<'_>) -> Result<(), Refusal> {
        self.host().preflight_managed(record)
    }
    fn handoff_unsubmitted(&self, record: &BoxRecord, pool: &PoolRecord) -> Result<(), Refusal> {
        check_backing_reference(record, pool)?;
        self.guest().handoff_unsubmitted(record, pool)?;
        self.host().preflight_unsubmitted(record)
    }

    fn physical_observations(&self) -> Result<Vec<hex_api::PhysicalDevice>, Refusal> {
        self.guest().physical_observations()
    }

    fn preflight_record(&self, record: &BoxRecord) -> Result<(), Refusal> {
        match self.route_for(&record.spec)? {
            hex_runtime::guest::Side::Host => self.host().preflight_record(record),
            hex_runtime::guest::Side::Guest => {
                self.guest().preflight_box(record)?;
                if record.spec.route_execution() != Some(hex_api::Execution::ShardsShared) {
                    self.host().preflight_managed(NativeRecord::Box(record))?;
                }
                Ok(())
            }
        }
    }
    fn preflight_pool(&self, record: &PoolRecord) -> Result<(), Refusal> {
        self.guest().preflight_backing(record)?;
        self.host().preflight_managed(NativeRecord::Pool(record))
    }

    fn preflight_retained(
        &self,
        records: &hex_runtime::devices::Records,
        pools: &[PoolRecord],
    ) -> Result<(), Refusal> {
        self.host().preflight_retained(records, pools)?;
        // The guest's own receipts join the same cohort: one whose name no retained guest-routed
        // record carries is an orphan, its native scope nobody else's to find (AC-T7.4.1).
        for id in self.guest().receipt_ids()? {
            let record = records
                .get(&id)
                .and_then(|record| record.as_ref().ok())
                .ok_or_else(|| uncertain(&id, "its record can't be read"))?;
            if self.route_for(&record.spec)? != hex_runtime::guest::Side::Guest {
                return Err(uncertain(&id, "its record routes it outside the guest"));
            }
        }
        Ok(())
    }

    fn initialize_native(&self) -> Result<(), Refusal> {
        self.host().initialize_native()
    }

    fn device_catalog(&self) -> Result<Option<hex_runtime::devices::SharedCatalog>, Refusal> {
        let guest = self.guest().device_catalog();
        let host = self.host().device_catalog()?;
        if let (Some(guest), Some(host)) = (&guest, &host)
            && !hex_runtime::devices::SharedCatalog::ptr_eq(guest, host)
        {
            return Err(broken(
                "host and guest were initialized with different device catalogs",
            ));
        }
        Ok(guest.or(host))
    }

    fn managed_usage(&self, record: NativeRecord<'_>) -> Result<Usage, Refusal> {
        self.host().managed_usage(record)
    }

    /// D255: the box's managed VM is booted in its guest runtime, whose governor ask it is.
    fn managed_rate(
        &self,
        record: &BoxRecord,
        rate: u64,
    ) -> Result<hex_runtime::guest::ManagedRate, Refusal> {
        self.guest().managed_rate(record, rate)
    }

    fn managed_rate_period(
        &self,
        record: &BoxRecord,
    ) -> Result<Option<std::time::Duration>, Refusal> {
        self.guest().managed_rate_period(record)
    }

    /// D260: the box's managed VM is booted in its guest runtime, whose use ask it is.
    fn managed_guest_time(
        &self,
        record: &BoxRecord,
    ) -> Result<hex_runtime::guest::ManagedGuestTime, Refusal> {
        self.guest().managed_guest_time(record)
    }

    fn managed_life(&self, record: &BoxRecord) -> Result<std::time::Duration, Refusal> {
        self.host().managed_life(record)
    }

    fn managed_least(&self) -> u64 {
        self.host().managed_least()
    }

    fn set_managed_rate(&self, record: NativeRecord<'_>, rate: u64) -> Result<(), Refusal> {
        self.host().set_managed_rate(record, rate)
    }

    fn stop_managed(&self, record: NativeRecord<'_>) -> Result<(), Refusal> {
        self.host().stop_managed(record)
    }

    fn cap_cycle(&self, rate: u64) -> Result<hex_runtime::guest::CapCycle, Refusal> {
        self.host().cap_cycle(rate)
    }

    /// Joint plan step 14 (C1): an image VM's rate period is its run's, which its guest runtime reads (D247): its last
    /// rate's answer's, none before its first (Shards' image channel v31).
    fn image_rate_period(
        &self,
        record: &BoxRecord,
    ) -> Result<Option<std::time::Duration>, Refusal> {
        self.guest().rate_period(&record.spec.id)
    }

    /// Joint plan step 39 (place 3): its node VM's host use, through its guest runtime's image class (D241).
    fn node_vm_host_use(&self, run: hex_api::NodeRun) -> Result<hex_api::HostUse, Refusal> {
        self.guest().node_vm_host_use(run)
    }

    /// Joint plan step 39 (place 3): its node VM's run's rate period (D247), none before its first rate's answer.
    fn node_vm_rate_period(
        &self,
        run: hex_api::NodeRun,
    ) -> Result<Option<std::time::Duration>, Refusal> {
        self.guest().node_vm_rate_period(run)
    }

    /// Joint plan step 39 (place 3): its node VM held through its run's rate (D247).
    fn node_vm_rate(
        &self,
        run: hex_api::NodeRun,
        rate: Option<std::num::NonZeroU64>,
    ) -> Result<Option<std::num::NonZeroU64>, Refusal> {
        self.guest().node_vm_rate(run, rate)
    }

    /// Joint plan step 39 (place 3): its node VM's run stopped, its end kept in its row.
    fn stop_node_vm(&self, run: hex_api::NodeRun) -> Result<(), Refusal> {
        self.guest().stop_node_vm(run)
    }

    /// Joint plan step 39: its node VM's guest told its room through its run (D188).
    fn node_vm_room(&self, run: hex_api::NodeRun, room: i64) -> Result<(), Refusal> {
        self.guest().node_vm_room(run, room)
    }

    fn launch_managed(
        &self,
        record: NativeRecord<'_>,
        plan: &hex_runtime::devices::ManagedLaunch<'_>,
        io: shards_api::LaunchIo,
        patience: Option<std::time::Duration>,
        publish: &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        self.host()
            .launch_managed(record, plan, io, patience, publish)
    }

    fn attach_managed(
        &self,
        record: NativeRecord<'_>,
        identity: Option<&shards_api::Identity>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        self.host().attach_managed(record, identity)
    }

    fn finalize_managed(
        &self,
        record: NativeRecord<'_>,
        identity: Option<&shards_api::Identity>,
    ) -> Result<(), Refusal> {
        self.host().finalize_managed(record, identity)
    }

    fn attachment_offers(&self) -> Result<Vec<hex_api::DeviceInfo>, Refusal> {
        let mut offers = self.host().attachment_offers()?;
        let guest = self.guest().attachment_offers()?;
        offers
            .try_reserve(guest.len())
            .map_err(|_| Refusal::Exhausted {
                what: "initialized attachment offers",
                bound: hex_api::MAX_DEVICES,
            })?;
        for offer in guest {
            if let Some(existing) = offers.iter_mut().find(|existing| existing.id == offer.id) {
                if existing.model != offer.model
                    || existing.size != offer.size
                    || (existing.holder.is_some()
                        && offer.holder.is_some()
                        && existing.holder != offer.holder)
                {
                    return Err(Refusal::Broken(
                        "initialized routes disagree about one physical device".into(),
                    ));
                }
                if existing.holder.is_none() {
                    existing.holder = offer.holder;
                }
            } else {
                if offers.len() >= hex_api::MAX_DEVICES {
                    return Err(Refusal::Exhausted {
                        what: "initialized attachment offers",
                        bound: hex_api::MAX_DEVICES,
                    });
                }
                offers.push(offer);
            }
        }
        Ok(offers)
    }

    fn owns_records(&self, spec: &BoxSpec) -> Result<bool, Refusal> {
        Ok(self.route_for(spec)? == hex_runtime::guest::Side::Guest
            && self.guest().owns_record(spec))
    }

    fn runtime_for(&self, spec: &BoxSpec) -> Result<&dyn Runtime, Refusal> {
        match self.route_for(spec)? {
            hex_runtime::guest::Side::Guest => Ok(self.guest()),
            hex_runtime::guest::Side::Host => Ok(self.host()),
        }
    }

    fn trace(&self, id: &BoxId) -> Result<Option<ProcId>, Refusal> {
        match self.host().inspect(id) {
            Ok(_) => self.host().trace(id),
            Err(Refusal::NoSuchBox(_)) => Ok(None),
            Err(why) => Err(why),
        }
    }

    fn adopt(&self, spec: &BoxSpec, trace: Option<ProcId>) -> Result<(), Refusal> {
        match self.route_for(spec)? {
            // Its guest holds it already.
            hex_runtime::guest::Side::Guest => self.guest().validate_binding(spec),
            hex_runtime::guest::Side::Host => self.host().adopt(spec, trace),
        }
    }

    fn end(&self, trace: Option<ProcId>) -> Result<(), Refusal> {
        self.host().end(trace)
    }

    fn end_for(&self, spec: &BoxSpec, trace: Option<ProcId>) -> Result<(), Refusal> {
        match self.route_for(spec)? {
            hex_runtime::guest::Side::Guest => Ok(()),
            hex_runtime::guest::Side::Host => self.host().end_for(spec, trace),
        }
    }

    fn alive(&self, spec: &BoxSpec) -> Result<Option<bool>, Refusal> {
        match self.route_for(spec)? {
            hex_runtime::guest::Side::Guest => {
                self.guest().validate_binding(spec)?;
                match self.guest().inspect(&spec.id) {
                    Ok(_) => Ok(Some(true)),
                    Err(Refusal::NoSuchBox(_)) => Ok(Some(false)),
                    Err(why) => Err(why),
                }
            }
            hex_runtime::guest::Side::Host => self.host().alive(spec),
        }
    }

    fn discard(&self, spec: &BoxSpec) -> Result<(), Refusal> {
        match self.route_for(spec)? {
            hex_runtime::guest::Side::Guest => match self.guest().delete(&spec.id) {
                Ok(()) | Err(Refusal::NoSuchBox(_)) => Ok(()),
                Err(why) => Err(why),
            },
            hex_runtime::guest::Side::Host => self.host().discard(spec),
        }
    }

    fn orphans(&self, records: &hex_runtime::devices::Records) -> Result<Vec<BoxId>, Refusal> {
        self.guest().orphans(records)
    }

    fn delete_orphan(&self, id: &BoxId) -> Result<(), Refusal> {
        self.guest().delete_orphan(id)
    }

    /// Its guest's control connection, in the event form, joins the core's loop, and so does every guest's
    /// it boots or takes back after: a call through it holds no thread while the guest answers (native plan
    /// step 8 (1)).
    fn attach(&self, lp: Arc<dyn Loop>) -> Result<(), Refusal> {
        self.guest().join_loop(Arc::new(LoopJoins(lp)))
    }

    /// A call about a box its guest holds goes there through the event form's client and is answered
    /// from the loop; one about a host box runs on the worker. With no guest running the box is no box; a
    /// guest whose connection can't be joined to the loop refuses the call, naming why.
    ///
    /// A guest program can't take the caller's own pipes, and the node copies no byte between them (native
    /// plan step 8): each stream an exec carries or asks for is the guest's own stdio connection, whose end
    /// goes to the caller's client with the answer, and the caller's pipes are let go here. Its client
    /// copies between those it carried and their ends.
    fn forward(&self, call: Call, stdio: Stdio, reply: Reply, ended: serve::Ended) -> Forward {
        let id = match &call {
            Call::Inspect(id) | Call::Start(id) | Call::Usage(id) => id,
            Call::Signal { id, .. } | Call::Exec { id, .. } | Call::ExecStreams { id, .. } => id,
            _ => return Forward::Back(Box::new((call, stdio))),
        };
        match self.host().inspect(id) {
            Err(Refusal::NoSuchBox(_)) => {}
            Ok(_) | Err(_) => return Forward::Back(Box::new((call, stdio))),
        }
        forward_guest(self.guest(), call, stdio, reply, ended)
    }

    /// A box its guest runtime holds goes to that guest's node, but a Shards image VM, whose guest runs no node of
    /// Hex's: the guest runtime here answers it, as the Linux node runs an image VM's exec here (`guest_node_class`;
    /// joint plan step 12, docs/design/shards-image-class.md §2).
    fn forward_for(
        &self,
        spec: &BoxSpec,
        call: Call,
        stdio: Stdio,
        reply: Reply,
        ended: serve::Ended,
    ) -> Forward {
        match self.route_for(spec) {
            Ok(hex_runtime::guest::Side::Guest)
                if matches!(spec.class, hex_api::ClassSpec::ShardsImage(_)) =>
            {
                Forward::Back(Box::new((call, stdio)))
            }
            Ok(hex_runtime::guest::Side::Guest) => {
                forward_guest(self.guest(), call, stdio, reply, ended)
            }
            Ok(hex_runtime::guest::Side::Host) => {
                self.host().forward_for(spec, call, stdio, reply, ended)
            }
            Err(error) => Forward::Refused(error),
        }
    }
}

fn forward_guest(
    guest: &hex_runtime::guest::GuestRuntime,
    call: Call,
    stdio: Stdio,
    reply: Reply,
    ended: serve::Ended,
) -> Forward {
    let id = match &call {
        Call::Inspect(id) | Call::Start(id) | Call::Usage(id) => id,
        Call::Signal { id, .. } | Call::Exec { id, .. } | Call::ExecStreams { id, .. } => id,
        _ => return Forward::Back(Box::new((call, stdio))),
    };
    // HEX-RT-17: a Windows node hands its caller handles alone, never sockets, so it offers an exec's streams to the
    // caller to open itself, where its guest's link lends the VM's identity; a node VM's lanes lend none, and their
    // exec's streams are made here on pipes whose ends it hands on (the review's D-10).
    let offer = cfg!(windows)
        && matches!(&call, Call::Exec { .. } | Call::ExecStreams { .. })
        && (stdio.ask.stdin
            || stdio.ask.stdout
            || stdio.ask.stderr
            || stdio.stdin.is_some()
            || stdio.stdout.is_some()
            || stdio.stderr.is_some());
    let client = match guest.exec_client_for(id, offer) {
        Ok(Some(hex_runtime::guest::ExecClient::Offered(client))) => client,
        Ok(Some(hex_runtime::guest::ExecClient::Evented(client))) => {
            return forward_evented(client, call, stdio, reply, ended);
        }
        Ok(None) => return Forward::Refused(Refusal::NoSuchBox(id.clone())),
        Err(why) => return Forward::Refused(why),
    };
    let (call, stdio) = match guest_call(call, stdio) {
        Ok(prepared) => prepared,
        Err(why) => return Forward::Refused(why),
    };
    let then = started_then(&client, reply);
    let streams = hex_core::wire::Streams {
        stdin: stdio.ask.stdin,
        stdout: stdio.ask.stdout,
        stderr: stdio.ask.stderr,
    };
    let call = match call {
        Call::Exec { id, program, .. } | Call::ExecStreams { id, program, .. } => Call::Exec {
            id,
            program,
            streams,
        },
        _ => {
            return Forward::Refused(Refusal::Broken(
                "only an exec can offer guest streams".to_owned(),
            ));
        }
    };
    let pending = match client.defer(call, then, ended) {
        Ok(pending) => pending,
        Err(why) => return Forward::Refused(why),
    };
    let streams = match client.stream_offer(pending.id(), streams) {
        Ok(streams) => streams,
        Err(why) => return Forward::Refused(why),
    };
    Forward::Offered(serve::Offering {
        streams,
        ready: Box::new(move || pending.send()),
    })
}

/// A call about a box a guest holds as it goes to the guest: an exec's streams are the guest's own stdio connections,
/// asked for, whose ends go to the caller's client with the answer, and the caller's own pipes are let go here, its
/// client keeping its own (native plan step 8).
fn guest_call(call: Call, stdio: Stdio) -> Result<(Call, Stdio), Refusal> {
    Ok(match call {
        Call::Exec { id, program, .. } | Call::ExecStreams { id, program, .. } => {
            stdio.check()?;
            let ask = Ask {
                stdin: stdio.stdin.is_some() || stdio.ask.stdin,
                stdout: stdio.stdout.is_some() || stdio.ask.stdout,
                stderr: stdio.stderr.is_some() || stdio.ask.stderr,
            };
            drop(stdio);
            let exec = Call::Exec {
                id,
                program,
                streams: hex_core::wire::Streams::default(),
            };
            let asked = Stdio {
                ask,
                ..Stdio::null()
            };
            (exec, asked)
        }
        call => (call, stdio),
    })
}

/// What answers a call sent through `client`: a started program, its end to come as the guest's message, or the
/// guest's answer, to `reply`.
fn started_then(client: &hex_runtime::guest::EventedClient, reply: Reply) -> hex_client::Then {
    let started = triomphe::Arc::clone(client);
    Box::new(move |exec, got| {
        reply(match got {
            Ok(Answer::Started { pid }) => Ok(Forwarded::Started {
                pid,
                given: started.given(exec),
                program: Box::new(GuestProgram {
                    client: started,
                    exec,
                }),
            }),
            Ok(Answer::Refused(why)) | Err(why) => Err(why),
            Ok(answer) => Ok(Forwarded::Answer(Box::new(answer))),
        })
    })
}

/// Sends `call`, about a box a guest holds, through `client`, the event form's connection to that guest's node joined
/// to the core's loop (AC-T7.4.1): its answer goes to `reply` and an exec's program's end to `ended`, each as the
/// guest's message, never as a pid this host watches. Shared by [`Recorded`]'s guest and the Linux node's VM backends.
pub(crate) fn forward_evented(
    client: hex_runtime::guest::EventedClient,
    call: Call,
    stdio: Stdio,
    reply: Reply,
    ended: serve::Ended,
) -> Forward {
    let (call, stdio) = match guest_call(call, stdio) {
        Ok(prepared) => prepared,
        Err(why) => return Forward::Refused(why),
    };
    let then = started_then(&client, reply);
    // Refused before it was sent: `then` is dropped unrun, and the caller hears why.
    match client.begin(call, stdio, then, ended) {
        Ok(_) => Forward::Taken,
        Err(why) => Forward::Refused(why),
    }
}

/// The core's loop as the guest's runtime joins its connections to it: Recorded's guest's, and the Linux node's VM
/// backends'.
pub(crate) struct LoopJoins(pub(crate) Arc<dyn Loop>);

impl hex_runtime::guest::Joins for LoopJoins {
    fn reserve(&self) -> Result<(u64, hex_runtime::guest::Room), Refusal> {
        let token = self.0.token()?;
        let lp = Arc::clone(&self.0);
        Ok((token, Arc::new(move || lp.room(token))))
    }

    fn join(&self, token: u64, pump: hex_client::guest::Pump) -> Result<(), Refusal> {
        self.0.add(token, Box::new(Pumped(pump)))
    }
}

/// The guest's control connection's reading side, as a source of the core's loop.
struct Pumped(hex_client::guest::Pump);

impl serve::Source for Pumped {
    fn handle(&self) -> serve::Handle<'_> {
        self.0.handle()
    }

    fn ready(&mut self) -> serve::Want {
        match self.0.ready() {
            hex_client::guest::Pumped::Read => serve::Want::Read,
            hex_client::guest::Pumped::ReadWrite => serve::Want::ReadWrite,
            hex_client::guest::Pumped::Ended => serve::Want::Done,
        }
    }
}

/// A program in the guest, by its exec's id on the event form's connection.
struct GuestProgram {
    client: hex_runtime::guest::EventedClient,
    exec: u64,
}

impl serve::Remote for GuestProgram {
    fn kill(&self, done: serve::Killed) {
        let done = Arc::new(Mutex::new(Some(done)));
        let answered = Arc::clone(&done);
        let then: hex_client::Then = Box::new(move |_, got| {
            let sent = match got {
                Ok(Answer::Done) => Ok(()),
                Ok(Answer::Refused(why)) | Err(why) => Err(why),
                Ok(_) => Err(Refusal::Broken(
                    "the guest's node answered a kill with something but done".to_owned(),
                )),
            };
            if let Some(done) = answered.lock().ok().and_then(|mut d| d.take()) {
                done(sent);
            }
        });
        if let Err(why) = self.client.kill_then(self.exec, then)
            && let Some(done) = done.lock().ok().and_then(|mut d| d.take())
        {
            done(Err(why));
        }
    }

    fn release(&self) {
        self.client.release(self.exec);
    }
}

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod pool_tests;

#[cfg(test)]
mod report_tests;

#[cfg(test)]
mod handoff_tests;

#[cfg(test)]
mod unsubmitted_pool_tests;

#[cfg(test)]
mod route_projection_tests;

#[cfg(test)]
mod shared_outer_original_tests;

#[cfg(test)]
mod answers_tests;

#[cfg(test)]
mod image_turn_tests;

#[cfg(all(test, target_os = "macos"))]
mod macos_unsubmitted_tests {
    use super::*;
    use hex_api::{
        BackingKey, Bounds, CapacityResources, ClassSpec, DeviceKind, Execution, GpuEligibility,
        ImageArch, ImageOs, ImagePlatform, LifecycleCause, Network, OciSpec, Rootfs, RouteKey,
        RouteReceipt, RuntimeCost,
    };

    /// A launch-capable host: the row's measured cost identity (the response is a test fixture,
    /// no enforcement conclusion) and its persistent helper's private paths in `dir`. Configuring
    /// them starts and reads nothing.
    fn runtime(dir: &Path) -> hex_runtime::macos::MacRuntime {
        std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let model = hex_runtime::macos::machine_model().unwrap();
        let rates = hex_core::watch::seed(&model);
        hex_runtime::macos::MacRuntime::new(
            rates.bytes,
            rates.processes,
            hex_core::watch::lateness_seed(&model),
        )
        .unwrap()
        .with_cpu_control(hex_runtime::macos::CpuControl {
            cost_id: [0x91; 32],
            response: std::time::Duration::from_nanos(1),
        })
        .unwrap()
        .with_managed_helper(
            hex_runtime::macos::ManagedHelper::new(
                dir.join("hex"),
                dir.join("helper.sock"),
                dir.join("helper.receipt"),
            )
            .unwrap(),
        )
        .unwrap()
    }

    /// The row `preflight_handoff_turns` hands the adapter: a dedicated Box reserved on a still-held
    /// source, its scope introduced, with no init, run or exit.
    fn pending(root: &Path) -> BoxRecord {
        let fixed = CapacityResources {
            cpu_milli: 1000,
            mem_bytes: shards_api::min_memory_mib(std::env::consts::ARCH) << 20,
            disk_bytes: 0,
        };
        let cost = RuntimeCost {
            id: [0x91; 32],
            floor: fixed,
            ceiling: fixed,
            quantum: CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 1,
            },
            overhead: CapacityResources::default(),
            guest_overhead: CapacityResources::default(),
        };
        let spec = BoxSpec {
            id: BoxId::new("pending-handoff").unwrap(),
            class: ClassSpec::Shards(Box::new(OciSpec {
                rootfs: Rootfs::Dir(root.to_path_buf()),
                init: Program::of(&["/bin/true"]),
                mounts: Vec::new(),
                hostname: None,
                network: Network::Private,
                hosts: Vec::new(),
            })),
            bounds: Bounds {
                cpu_max_milli: Some(1000),
                memory_max: Some(hex_api::MIN_MEMORY),
                ..Bounds::default()
            },
            owner: None,
            devices: vec![DeviceId {
                kind: DeviceKind::Gpu,
                key: 7,
            }],
            allocation: Some(cost.allocate(CapacityResources::default()).unwrap()),
            route: Some(RouteReceipt {
                key: RouteKey {
                    class: Class::Shards,
                    image: ImagePlatform {
                        os: ImageOs::Linux,
                        arch: ImageArch::Arm64,
                    },
                    execution: Execution::ShardsDedicated,
                    gpu: GpuEligibility::Any,
                },
                backing: Some(BackingKey {
                    pool: [0x92; 32],
                    generation: [0x93; 16],
                }),
            }),
            band: 0,
        };
        spec.check().unwrap();
        let mut record = BoxRecord::new(spec, Phase::Creating, Vec::new());
        record.native_scope = Some(hex_core::record::NativeScope::from_bytes([0x94; 16]));
        record
    }

    /// AC-T7.4.1: original. Restarted with a dedicated Box's handoff pending, the macOS node's
    /// preflight asks its host, through `Recover` as `WithGuest` does, to prove it holds no launch,
    /// scope or receipt for that Box. With nothing in its native table, expect Ok for the
    /// reservation and its cancellation (Deleting); with a slot of that id held, Uncertain.
    #[test]
    fn original_macos_pending_handoff_with_no_native_slot_passes_the_host_half() {
        let root = tempfile::tempdir().unwrap();
        let native = runtime(root.path());
        let mut record = pending(root.path());
        assert!(native.list().unwrap().is_empty());
        let proved = Recover::preflight_unsubmitted(&native, &record);
        assert!(
            proved.is_ok(),
            "nothing is retained for the Box, yet the host half refused: {proved:?}"
        );
        record.phase = Phase::Deleting;
        Recover::preflight_unsubmitted(&native, &record).unwrap();
        native
            .create(BoxSpec {
                id: record.spec.id.clone(),
                class: ClassSpec::Process,
                bounds: Bounds::default(),
                owner: None,
                devices: Vec::new(),
                allocation: None,
                route: None,
                band: 0,
            })
            .unwrap();
        assert!(matches!(
            Recover::preflight_unsubmitted(&native, &record),
            Err(Refusal::Lifecycle { id, cause: LifecycleCause::Uncertain, .. }) if id == record.spec.id
        ));
        assert_eq!(native.list().unwrap().len(), 1, "the check removed nothing");
        native.delete(&record.spec.id).unwrap();
        Recover::preflight_unsubmitted(&native, &record).unwrap();
    }
}
