//! The Shards class on Agentfile images over a model of Shards' image boot (D231's `Images`, which has
//! no simulator of its own yet) and a record owner holding the node Store's lifecycle rules: each test
//! does a call and expects the record, the run and the refusal the design names
//! (docs/design/shards-image-class.md; joint plan step 12, AC-T7.4.1).

use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use crate::devices::{Books, Records, Transaction};
use hex_api::{
    Allocation, Bounds, Capacity, CapacityResources, DeviceInfo, NodeRun, NodeVmPhase, Runtime,
};
use hex_core::record::NodeVmRecord;
use hex_testkit::waits::follow::{self, Party};
use shards_api::image::{
    Bound, Channel, ChannelEnd, GuestTimes, HostUse, Keep, MemberEnd, MemberInfo, MemberName,
    Microvm, Receiving, StartedFrom, Unused, VcpuTime,
};

/// A record owner keeping the node Store's rules (hex-node's `Store::update_lifecycle`): a first record
/// is Creating; Creating becomes anything, Ready Ready or Deleting, Deleting Deleting alone; an exit, once
/// written, stays; only a Deleting record is removed.
struct Owner {
    records: Mutex<Records>,
    /// Its node VMs' records (joint plan step 39), apart from its boxes' as the node Store keeps them.
    node_vms: Mutex<std::collections::BTreeMap<NodeRun, NodeVmRecord>>,
    /// Its scans of every record ([`Books::records`]), each a read of the node's whole Store.
    scans: AtomicUsize,
    /// The node's state's own directory for its image boxes' diagnostics, gone with the owner.
    state: tempfile::TempDir,
    /// Its next write of a Deleting record, held once written where the test asks it, until the test lets it go: a
    /// delete's intent as slow to return as its disk's flush may be, so another call moves the box on meanwhile.
    deleting: Holding,
    /// Its next write of a box's start, Creating to Ready, held once written where the test asks it, until the test lets
    /// it go: a start's intent as slow to return as its disk's flush may be, so another call reads the box meanwhile.
    readying: Holding,
    /// Its next scan of every record ([`Books::records`]), held once read where the test asks it, until the test lets it
    /// go: a call whose read other calls' writes then outdate, as its thread's preemption may leave it.
    reading: Holding,
    /// Its next write of a run's end, the first its record has, held before it is written where the test asks it, until
    /// the test lets it go: an end as slow to be recorded as its disk's flush may be, so another call reads the record
    /// without it meanwhile.
    ending: Holding,
    /// Whether it refuses node VMs' Ready rows, as the test sets it: as a Store whose disk is full refuses a write.
    refuse_ready: AtomicBool,
    /// Whether it refuses node VMs' Ended rows, as the test sets it, as a full disk refuses them.
    refuse_ended: AtomicBool,
}

/// Where one of the owner's holds stands ([`Holding`]).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Hold {
    #[default]
    None,
    Asked,
    Holding,
    LetGo,
}

/// One of the owner's holds of a kind of call, as the test asks it: the next such call waits where it is held until the
/// test lets it go.
#[derive(Default)]
struct Holding {
    state: Mutex<Hold>,
    moved: Condvar,
}

impl Holding {
    /// Holds the next such call once it comes.
    fn ask(&self) {
        *self.state.lock().unwrap() = Hold::Asked;
    }

    /// Whether such a call is held now.
    fn holds(&self) -> bool {
        *self.state.lock().unwrap() == Hold::Holding
    }

    fn let_go(&self) {
        *self.state.lock().unwrap() = Hold::LetGo;
        self.moved.notify_all();
    }

    /// The calling thread held here where the test asked it, until the test lets it go.
    fn here(&self) {
        let mut hold = self.state.lock().unwrap();
        if *hold == Hold::Asked {
            *hold = Hold::Holding;
            drop(
                self.moved
                    .wait_while(hold, |hold| *hold == Hold::Holding)
                    .unwrap(),
            );
        }
    }
}

impl Owner {
    fn new() -> Self {
        Self {
            records: Mutex::default(),
            node_vms: Mutex::default(),
            scans: AtomicUsize::new(0),
            state: tempfile::tempdir().unwrap(),
            deleting: Holding::default(),
            readying: Holding::default(),
            reading: Holding::default(),
            ending: Holding::default(),
            refuse_ready: AtomicBool::new(false),
            refuse_ended: AtomicBool::new(false),
        }
    }

    /// Where the class keeps its runs' diagnostics ([`GuestRuntime::with_images`]).
    fn diagnostics(&self) -> std::path::PathBuf {
        self.state.path().join("image-diagnostics")
    }
}

fn broken(why: &str) -> Refusal {
    Refusal::Broken(why.to_owned())
}

impl Transaction for Owner {
    fn records(&self) -> Result<Records, Refusal> {
        Ok(self.records.lock().unwrap().clone())
    }
    fn pool_records(&self) -> Result<crate::devices::PoolRecords, Refusal> {
        Ok(Vec::new())
    }
    fn read_pool(
        &self,
        _: &hex_api::BackingKey,
    ) -> Result<Option<hex_core::record::PoolRecord>, Refusal> {
        Err(broken("an image box reads no pool"))
    }
    fn put_pool(&self, _: &hex_core::record::PoolRecord) -> Result<(), Refusal> {
        Err(broken("an image box writes no pool"))
    }
    fn grant_pool(
        &self,
        _: &hex_api::BackingGrant,
    ) -> Result<hex_core::record::PoolRecord, Refusal> {
        Err(broken("an image box grants no pool"))
    }
    fn remove_pool(&self, _: &hex_api::BackingKey) -> Result<(), Refusal> {
        Err(broken("an image box removes no pool"))
    }
    fn check_unsubmitted_pool(&self) -> Result<(), Refusal> {
        Err(broken("an image box owns no pool scope"))
    }
    fn read(&self, id: &BoxId) -> Result<Option<BoxRecord>, Refusal> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .get(id)
            .map(|row| row.as_ref().unwrap().clone()))
    }
    fn put(&self, record: &BoxRecord) -> Result<(), Refusal> {
        let ends = record.exit.is_some()
            && self
                .records
                .lock()
                .unwrap()
                .get(&record.spec.id)
                .is_some_and(|current| current.as_ref().unwrap().exit.is_none());
        if ends {
            self.ending.here();
        }
        let mut records = self.records.lock().unwrap();
        let readies = record.phase == Phase::Ready
            && records
                .get(&record.spec.id)
                .is_some_and(|current| current.as_ref().unwrap().phase == Phase::Creating);
        let allowed = match records.get(&record.spec.id) {
            None => record.phase == Phase::Creating,
            Some(current) => {
                let current = current.as_ref().unwrap();
                let phase = matches!(
                    (current.phase, record.phase),
                    (Phase::Creating, _)
                        | (Phase::Ready, Phase::Ready | Phase::Deleting)
                        | (Phase::Deleting, Phase::Deleting)
                );
                let exit = current.exit.is_none() || current.exit == record.exit;
                phase && exit && current.spec == record.spec
            }
        };
        if !allowed {
            return Err(broken("a write the Store refuses"));
        }
        records.insert(record.spec.id.clone(), Ok(record.clone()));
        drop(records);
        if record.phase == Phase::Deleting {
            self.deleting.here();
        }
        if readies {
            self.readying.here();
        }
        Ok(())
    }
    fn remove(&self, id: &BoxId) -> Result<(), Refusal> {
        let mut records = self.records.lock().unwrap();
        match records.get(id) {
            Some(Ok(record)) if record.phase == Phase::Deleting => {
                records.remove(id);
                Ok(())
            }
            _ => Err(broken("a removal of a record that isn't Deleting")),
        }
    }
    fn launch_managed(
        &self,
        _: &crate::devices::ManagedLaunch<'_>,
        _: shards_api::LaunchIo,
        _: Option<Duration>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        Err(broken("an image box launches through Shards' image boot"))
    }
    fn attach_managed(
        &self,
        _: Option<&shards_api::Identity>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        Err(broken("an image box attaches nothing"))
    }
    fn finalize_managed(&self, _: Option<&shards_api::Identity>) -> Result<(), Refusal> {
        Err(broken("an image box finalizes no native scope"))
    }
    fn managed_usage(&self) -> Result<Usage, Refusal> {
        Err(broken("an image box's usage is its run's stats"))
    }
}

impl Books for Owner {
    fn records(&self) -> Result<Records, Refusal> {
        self.scans.fetch_add(1, Ordering::SeqCst);
        let read = Transaction::records(self);
        self.reading.here();
        read
    }
    fn pool_records(&self) -> Result<crate::devices::PoolRecords, Refusal> {
        Ok(Vec::new())
    }
    fn prepare_pool(
        &self,
        _: [u8; 32],
        _: hex_api::Execution,
        _: hex_api::RuntimeCost,
        _: &[hex_api::DeviceId],
        _: (
            &[u8; 32],
            &[hex_api::DeviceId],
            Option<&hex_core::record::PoolRecord>,
        ),
        _: &mut crate::devices::PoolPreparationCheck<'_>,
    ) -> Result<hex_core::record::PoolRecord, Refusal> {
        Err(broken("an image box prepares no pool"))
    }
    /// Its host's own pool, as a node's Store answers it ([`host_pool`]).
    fn capacity(&self) -> Result<Capacity, Refusal> {
        Ok(host_pool())
    }
    fn physical_devices(&self) -> Result<Vec<DeviceInfo>, Refusal> {
        Ok(Vec::new())
    }
    fn with_transaction(
        &self,
        _: &BoxId,
        _: &[hex_api::DeviceId],
        callback: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        callback(self)
    }
    fn with_pool_transaction(
        &self,
        _: &hex_api::BackingKey,
        _: &[hex_api::DeviceId],
        _: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        Err(broken("an image box enters no pool"))
    }
    fn with_handoff_transaction(
        &self,
        _: &BoxId,
        _: &hex_api::BackingKey,
        _: &[hex_api::DeviceId],
        _: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        Err(broken("an image box hands nothing off"))
    }
    fn node_vm(&self, run: &NodeRun) -> Result<Option<NodeVmRecord>, Refusal> {
        Ok(self.node_vms.lock().unwrap().get(run).cloned())
    }
    fn put_node_vm(&self, record: &NodeVmRecord) -> Result<(), Refusal> {
        record.check()?;
        let refused = match record.phase {
            NodeVmPhase::Ready => self.refuse_ready.load(Ordering::SeqCst),
            NodeVmPhase::Ended => self.refuse_ended.load(Ordering::SeqCst),
            NodeVmPhase::Booting => false,
        };
        if refused {
            return Err(Refusal::Os {
                op: "writing a node VM's row",
                why: "no space left on the device, as the test asked".to_owned(),
            });
        }
        self.node_vms
            .lock()
            .unwrap()
            .insert(record.run, record.clone());
        Ok(())
    }
    fn all_node_vms(&self) -> Result<Vec<NodeVmRecord>, Refusal> {
        Ok(self.node_vms.lock().unwrap().values().cloned().collect())
    }
    // One past the runs it keeps, in every byte: unique while it keeps fewer than 255.
    fn mint_node_run(&self) -> Result<NodeRun, Refusal> {
        let next = u8::try_from(self.node_vms.lock().unwrap().len() + 1).unwrap();
        Ok(NodeRun::from_bytes([next; 16]))
    }
}

/// A run in the model: its end, which the test or its stop sets, its stops, and its stats.
#[derive(Default)]
struct ModelRun {
    end: Mutex<Option<RunEnd>>,
    /// What its run host said last where it exited without its run's end, as the test sets it: shards-api then reads
    /// the run's end as `Failed` in those words, and a stop of it as `Stopped` (`Session::wait_for`, `Session::stop`).
    gone: Mutex<Option<String>>,
    stops: AtomicUsize,
    stats: Mutex<Stats>,
    /// The patience each stats ask was given, in order (decision 9).
    stats_asked: Mutex<Vec<Duration>>,
    /// Each file its run host's diagnostics were told to go to (`ImageGuest::keep_diagnostics`), in order: the test
    /// writes the host's diagnostics into the last, as the run host's do.
    diagnostics: Mutex<Vec<std::fs::File>>,
    /// The bound each of those files was told to be held within, in order (the review's B-9).
    diagnostics_bounds: Mutex<Vec<u64>>,
    /// The tail its run host keeps to quote its end, as Shards' session keeps it: a bound short of it is refused
    /// `DiagnosticsBound`, as Shards refuses it; none, any bound taken.
    diagnostics_tail: Mutex<Option<u64>>,
    /// Each room (D241) and rate (D247) the run was told, in order (joint plan step 14).
    rooms: Mutex<Vec<Option<i64>>>,
    rates: Mutex<Vec<Option<std::num::NonZeroU64>>>,
    /// Each member's end, as the test sets it, by name: none until it does.
    member_ends: Mutex<std::collections::BTreeMap<String, MemberEnd>>,
    /// Each member a boot asked the stdin of, by name, with its own ends of its stdin, stdout and stderr: the
    /// test holds them as the member does.
    inside: Mutex<
        Vec<(
            String,
            std::io::PipeReader,
            std::io::PipeWriter,
            std::io::PipeWriter,
        )>,
    >,
    /// The run's lessor as its boot was given it (D242), and each lease its run host was given through it, by its
    /// lessor's name (joint plan step 33): the model's run host gives every one back as the run ends, once, as Shards'
    /// session does (`session::release`).
    lessor: Mutex<Option<shards_api::image::RunLessor>>,
    given: Mutex<Vec<String>>,
    /// Whether its stop is held, as the test says: a stop asked waits until the test lets it go.
    stop_held: Mutex<bool>,
    stop_let_go: Condvar,
    /// Whether its stops' threads end with no answer, as the test sets it: a stop asked unwinds before its run's end, as
    /// a stop whose thread failed would (D231's stop is never refused, so only its thread's end leaves it unanswered).
    abandon_stops: AtomicBool,
    /// Each plug its run host took, in order (kiln plan 01a1dd2e…, P6.2: a run's volumes as it starts).
    plugs: Mutex<Vec<shards_api::image::Plug>>,
    /// What its run host refuses the next plug with, as the test sets it: none, each plug taken.
    refuse_plug: Mutex<Option<ImageRefusal>>,
    /// What its VM tells it used in all as its run ends on its own (D260's final use), as the test sets it: none, and
    /// its end tells none ([`ended`]).
    final_use: Mutex<Option<Microvm>>,
}

impl ModelRun {
    /// `end` as the model's run host says it, with the final use the test gave it where its run ended on its own
    /// (an exit), as a VM tells it once its machine is down; else as [`ended`] says it.
    fn ended(&self, end: RunEnd) -> RunEnded {
        match (&end, self.final_use.lock().unwrap().clone()) {
            (RunEnd::Exited(_), Some(microvm)) => RunEnded {
                end,
                final_use: shards_api::image::FinalUse::Read(microvm),
                paused: Duration::ZERO,
            },
            _ => ended(end),
        }
    }

    /// Holds each stop from now on (`true`), or lets every one held go (`false`).
    fn hold_stops(&self, held: bool) {
        *self.stop_held.lock().unwrap() = held;
        self.stop_let_go.notify_all();
    }

    /// The run host's ask of credential `reference` (D242), as it relays its network process's first use of it: put to
    /// the run's lessor under no key, the lease it gives kept for the run's end.
    fn ask(
        &self,
        reference: &str,
    ) -> Result<shards_api::image::Leased, shards_api::image::LeaseRefusal> {
        let lessor = self.lessor.lock().unwrap().clone();
        let lessor = lessor.ok_or_else(|| {
            shards_api::image::LeaseRefusal::Unavailable("the run has no lessor".to_owned())
        })?;
        let leased = lessor.0.lease(reference, None)?;
        self.given.lock().unwrap().push(leased.lease.clone());
        Ok(leased)
    }

    /// The run's end as Shards' session meets it: every lease it was given, given back once.
    fn release(&self) {
        let given: Vec<String> = self.given.lock().unwrap().drain(..).collect();
        if let Some(lessor) = self.lessor.lock().unwrap().clone() {
            for lease in given {
                lessor.0.release(&lease);
            }
        }
    }
}

/// The orchestrator's ends of a member's stdin, stdout and stderr, each taken once.
type Outside = (
    Option<std::io::PipeWriter>,
    Option<std::io::PipeReader>,
    Option<std::io::PipeReader>,
);

/// A booted run in the model: its members, as the test gives the image them, and its ports, bound as its boot
/// asked them (joint plan step 15).
struct ModelGuest {
    run: Arc<ModelRun>,
    members: Vec<MemberInfo>,
    published: Vec<Bound>,
    outside: Mutex<std::collections::BTreeMap<String, Outside>>,
    /// The period its VM's governor holds a rate over, said with each rate's answer (D247's `rate_period`; Shards' image
    /// channel v31): none before its first.
    rate_period: Duration,
    /// How its microVM started (D253), as the model's run host said it at its boot.
    started_from: StartedFrom,
    /// Its run's agent-control endpoint, where its boot asked one (D252).
    control: Option<shards_api::image::ControlAt>,
    /// Whether its boot gave its command the nesting grant (D234), whose READY it then says.
    nesting: bool,
    /// Its run's grants past its microVM, as its boot's spec narrowed them (D273): the model changes none as it runs.
    egress: shards_api::image::Narrowing,
}

/// Where the model's run host binds a run's agent-control endpoint, and its microVM's key (D252).
fn model_control() -> shards_api::image::ControlAt {
    shards_api::image::ControlAt {
        address: std::net::SocketAddr::from(([127, 0, 0, 1], 49_000)),
        vm: [7; 32],
        waker: std::net::SocketAddr::from(([127, 0, 0, 1], 49_001)),
        wake: std::net::SocketAddr::from(([127, 0, 0, 1], 49_002)),
    }
}

/// The period the model's run host says its VM's governor holds a rate over (D247's `rate_period`): the model's
/// fixture, which each run says unless the test sets another for the next boot (`ModelImages::period`).
const MODEL_RATE_PERIOD: Duration = Duration::from_millis(100);

impl ImageGuest for ModelGuest {
    fn members(&self) -> &[MemberInfo] {
        &self.members
    }
    fn published(&self) -> Vec<Bound> {
        self.published.clone()
    }
    fn publish(
        &self,
        _: shards_api::image::Publish,
        _: Duration,
    ) -> Result<Vec<Bound>, ImageRefusal> {
        Err(ImageRefusal::Unsupported {
            what: "the model's live publishing",
            why: "the class publishes at its start alone".to_owned(),
        })
    }
    fn unpublish(
        &self,
        _: shards_api::image::Proto,
        _: std::net::SocketAddr,
        _: Duration,
    ) -> Result<(), ImageRefusal> {
        Err(ImageRefusal::Unsupported {
            what: "the model's live publishing",
            why: "the class publishes at its start alone".to_owned(),
        })
    }
    fn control(&self) -> Option<&shards_api::image::ControlAt> {
        self.control.as_ref()
    }
    fn rate_period(&self) -> Option<Duration> {
        if self.run.rates.lock().unwrap().is_empty() {
            None
        } else {
            Some(self.rate_period)
        }
    }
    fn started_from(&self) -> StartedFrom {
        self.started_from
    }
    // The model runs each member's first generation alone (Shards' D270: 0), all the class asks: another's streams
    // are none it holds, and its end never comes, as Shards answers a generation not started.
    fn take_output(
        &self,
        member: &MemberName,
        generation: u32,
        channel: Channel,
    ) -> Result<std::io::PipeReader, ImageRefusal> {
        let mut outside = self.outside.lock().unwrap();
        let ends = outside.get_mut(&member.name).filter(|_| generation == 0);
        let taken = match channel {
            Channel::Stdout => ends.and_then(|e| e.1.take()),
            Channel::Stderr => ends.and_then(|e| e.2.take()),
        };
        taken.ok_or_else(|| ImageRefusal::Spec {
            what: "member",
            why: format!("{}'s output was taken, or it doesn't run", member.name),
        })
    }
    fn take_input(
        &self,
        member: &MemberName,
        generation: u32,
    ) -> Result<std::io::PipeWriter, ImageRefusal> {
        let mut outside = self.outside.lock().unwrap();
        outside
            .get_mut(&member.name)
            .filter(|_| generation == 0)
            .and_then(|e| e.0.take())
            .ok_or_else(|| ImageRefusal::Spec {
                what: "member",
                why: format!("{}'s stdin was taken, or never asked for", member.name),
            })
    }
    fn wait_member(
        &self,
        member: &MemberName,
        generation: u32,
        _: Duration,
    ) -> Result<Option<MemberEnd>, ImageRefusal> {
        Ok(self
            .run
            .member_ends
            .lock()
            .unwrap()
            .get(&member.name)
            .copied()
            .filter(|_| generation == 0))
    }
    fn restart(&self, _: &MemberName, _: &[Vec<u8>], _: Duration) -> Result<u32, ImageRefusal> {
        Err(ImageRefusal::Unsupported {
            what: "the model's restarts",
            why: "it restarts no member: the class asks none".to_owned(),
        })
    }
    fn swap(
        &self,
        _: &str,
        _: &str,
        _: shards_api::image::SwapWhen,
        _: Option<&[Vec<u8>]>,
        _: Duration,
    ) -> Result<shards_api::image::Swapped, ImageRefusal> {
        Err(ImageRefusal::Unsupported {
            what: "the model's swaps",
            why: "it swaps no component: the class asks none".to_owned(),
        })
    }
    fn restore(
        &self,
        _: &str,
        _: shards_api::image::SwapWhen,
        _: Option<&[Vec<u8>]>,
        _: Duration,
    ) -> Result<shards_api::image::Swapped, ImageRefusal> {
        Err(ImageRefusal::Unsupported {
            what: "the model's swaps",
            why: "it swaps no component: the class asks none".to_owned(),
        })
    }
    fn components(&self) -> Vec<shards_api::image::ComponentAt> {
        Vec::new()
    }
    fn stats(&self, _: StatsScope, patience: Duration) -> Result<Stats, ImageRefusal> {
        self.run.stats_asked.lock().unwrap().push(patience);
        match self.run.end.lock().unwrap().clone() {
            Some(end) => Err(ImageRefusal::Ended(ended(end))),
            None => Ok(self.run.stats.lock().unwrap().clone()),
        }
    }
    // The model keeps each room (D241) and rate (D247) it is told and holds the run as told: the room within none
    // where it is short; it gives no member a terminal (D244), refused as unsupported.
    fn room(&self, room: Option<i64>, _: Duration) -> Result<Option<u64>, ImageRefusal> {
        if let Some(end) = self.run.end.lock().unwrap().clone() {
            return Err(ImageRefusal::Ended(ended(end)));
        }
        self.run.rooms.lock().unwrap().push(room);
        Ok(room.map(|room| u64::try_from(room.max(0)).unwrap()))
    }
    fn rate(
        &self,
        rate: Option<std::num::NonZeroU64>,
        _: Duration,
    ) -> Result<Option<std::num::NonZeroU64>, ImageRefusal> {
        if let Some(end) = self.run.end.lock().unwrap().clone() {
            return Err(ImageRefusal::Ended(ended(end)));
        }
        self.run.rates.lock().unwrap().push(rate);
        Ok(rate)
    }
    fn resize(&self, _: &MemberName, _: u16, _: u16, _: Duration) -> Result<(), ImageRefusal> {
        Err(ImageRefusal::Unsupported {
            what: "the model's terminals",
            why: "it gives no member a terminal".to_owned(),
        })
    }
    // Ready on time where its run gave its command the nesting grant; refused as Shards refuses a run that asks none
    // (D234). Its plugs taken and kept, or refused as the test asks (D271: a run's volumes as it starts); its unplugs
    // refused, the class asking none.
    fn wait_ready(
        &self,
        _: Option<shards_api::image::Ready>,
        _: Duration,
    ) -> Result<Option<shards_api::image::Ready>, ImageRefusal> {
        if self.nesting {
            return Ok(Some(shards_api::image::Ready::OnTime));
        }
        Err(ImageRefusal::Spec {
            what: "nesting",
            why: "the run asked no nesting grant".to_owned(),
        })
    }
    fn plug(&self, plug: &shards_api::image::Plug, _: Duration) -> Result<(), ImageRefusal> {
        if let Some(why) = self.run.refuse_plug.lock().unwrap().take() {
            return Err(why);
        }
        self.run.plugs.lock().unwrap().push(plug.clone());
        Ok(())
    }
    fn unplug(&self, _: &str, _: Duration) -> Result<(), ImageRefusal> {
        Err(ImageRefusal::Unsupported {
            what: "the model's unplugs",
            why: "the class asks none".to_owned(),
        })
    }
    fn egress(&self) -> shards_api::image::Narrowing {
        self.egress.clone()
    }
    fn grant_egress(
        &self,
        _: &shards_api::image::EgressGrant,
        _: Duration,
    ) -> Result<shards_api::image::Narrowing, ImageRefusal> {
        Err(ImageRefusal::Unsupported {
            what: "the model's grants past its microVM",
            why: "it changes none as its run runs".to_owned(),
        })
    }
    fn revoke_egress(
        &self,
        _: &shards_api::image::EgressGrant,
        _: Duration,
    ) -> Result<shards_api::image::Narrowing, ImageRefusal> {
        Err(ImageRefusal::Unsupported {
            what: "the model's grants past its microVM",
            why: "it changes none as its run runs".to_owned(),
        })
    }
    fn member_egress(
        &self,
        _: &MemberName,
        _: Option<&shards_api::image::Reach>,
        _: Duration,
    ) -> Result<shards_api::image::Narrowing, ImageRefusal> {
        Err(ImageRefusal::Unsupported {
            what: "the model's grants past its microVM",
            why: "it changes none as its run runs".to_owned(),
        })
    }
    fn pause(&self, _: Duration) -> Result<shards_api::image::Paused, ImageRefusal> {
        Err(ImageRefusal::Unsupported {
            what: "the model's pauses",
            why: "it pauses none: the class pauses no run yet (P5.5b)".to_owned(),
        })
    }
    fn resume(&self, _: Duration) -> Result<shards_api::image::Paused, ImageRefusal> {
        Err(ImageRefusal::Unsupported {
            what: "the model's resumes",
            why: "it pauses none: the class pauses no run yet (P5.5b)".to_owned(),
        })
    }
    fn save(&self, _: &std::path::Path, _: Duration) -> Result<(), ImageRefusal> {
        Err(ImageRefusal::Unsupported {
            what: "the model's saves",
            why: "it makes none".to_owned(),
        })
    }
    fn stop(&self) -> Result<(), ImageRefusal> {
        self.run.stops.fetch_add(1, Ordering::SeqCst);
        if self.run.abandon_stops.load(Ordering::SeqCst) {
            panic!("the model's stop thread ends with no answer, as the test asked");
        }
        drop(
            self.run
                .stop_let_go
                .wait_while(self.run.stop_held.lock().unwrap(), |held| *held)
                .unwrap(),
        );
        self.run.end.lock().unwrap().get_or_insert(RunEnd::Stopped);
        self.run.release();
        Ok(())
    }
    fn keep_diagnostics(&self, to: std::fs::File, bound: u64) -> Result<(), ImageRefusal> {
        if let Some(tail) = *self.run.diagnostics_tail.lock().unwrap()
            && bound < tail
        {
            return Err(ImageRefusal::DiagnosticsBound { bound, tail });
        }
        self.run.diagnostics.lock().unwrap().push(to);
        self.run.diagnostics_bounds.lock().unwrap().push(bound);
        Ok(())
    }
    fn wait_for(&self, _: Duration) -> Result<Option<RunEnded>, ImageRefusal> {
        let end = self.run.end.lock().unwrap().clone();
        Ok(end.map(|end| self.run.ended(end)).or_else(|| {
            self.run.gone.lock().unwrap().as_ref().map(|said| {
                RunEnded::unused(
                    RunEnd::Failed(format!("the run host exited without its run's end: {said}")),
                    Unused::Untold,
                )
            })
        }))
    }
    // The model tells no channel's end apart from its run's (control-01's `WithRun`), as a host's answers that end
    // without telling one do; a member that runs nothing is refused as Shards refuses it.
    fn wait_channel(
        &self,
        member: &MemberName,
        _: u32,
        _: Channel,
        _: Duration,
    ) -> Result<Option<ChannelEnd>, ImageRefusal> {
        if !self
            .members
            .iter()
            .any(|m| m.kind == member.kind && m.name == member.name && m.runs)
        {
            return Err(ImageRefusal::Image(format!(
                "{member:?} is files alone, and runs nothing"
            )));
        }
        let ended =
            self.run.end.lock().unwrap().is_some() || self.run.gone.lock().unwrap().is_some();
        Ok(if ended {
            Some(ChannelEnd::WithRun)
        } else {
            None
        })
    }
    // No run of the model asks its GPU report (D233): refused as Shards refuses a run that asked none.
    fn take_gpu_report(
        &self,
        _: Duration,
    ) -> Result<shards_api::physical::GpuReport, ImageRefusal> {
        Err(ImageRefusal::GpuProbe(
            shards_api::GpuProbeError::Unavailable,
        ))
    }
}

/// `end` as the model's run host says it (image channel v28): its microVM's final use unread, killed where its run was
/// stopped, else untold. A run the test gave a final use tells it at its own end ([`ModelRun::ended`]).
fn ended(end: RunEnd) -> RunEnded {
    let why = match end {
        RunEnd::Stopped => Unused::Killed,
        RunEnd::Exited(_) | RunEnd::Failed(_) => Unused::Untold,
    };
    RunEnded::unused(end, why)
}

/// Where the model holds a boot, as D231's boot goes: before its run host has started, when its patience
/// isn't counting yet; at its run's start, its patience counting from its host's start; or nowhere.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gate {
    Host,
    Run,
    Open,
}

/// Shards' image boot in the model: what each boot was asked and the patience it was given, a refusal
/// for the next where the test sets one, and a gate a boot waits at, open unless the test closes it.
/// A boot held at its run past its patience is refused as D231 refuses it.
struct ModelImages {
    asked: Mutex<Vec<ImageSpec>>,
    patience: Mutex<Vec<Duration>>,
    refuse: Mutex<Option<ImageRefusal>>,
    gate: Mutex<Gate>,
    opened: Condvar,
    run: Arc<ModelRun>,
    /// The members the image runs, as the test sets them: none unless it does.
    members: Mutex<Vec<MemberInfo>>,
    /// Whom each guest port its image gives coming in reaches (D231's `Bound::to`), as the test sets them: a port
    /// not named here reaches no member, so the run's own command, or is refused on a run of the members alone.
    receivers: Mutex<Vec<(u16, Vec<Receiving>)>>,
    /// Each member the image declares and what it reaches by name (D236's destinations and its remote MCP servers',
    /// `Agentfile::named`), as the test sets it: the run host holds every one to its reach of the names kept where a
    /// boot holds members apart (`Narrowing::members`, D246).
    named: Mutex<Vec<(shards_api::image::MemberKind, String, Vec<String>)>>,
    /// The period the next boot's run host says its governor holds a rate over (D247), as the test sets it.
    period: Mutex<Duration>,
    /// How the next boot's microVM starts (D253), as the test sets it: booted alone unless it does.
    started_from: Mutex<StartedFrom>,
    /// Whether the next boot's thread ends with no answer at all, as the test sets it before anything wakes that boot:
    /// held, it hears its gate alone, never its stop, and once its gate opens it unwinds past it, as a boot whose
    /// thread failed would.
    abandon: AtomicBool,
    /// How often a boot held at its gate has looked again: each wake of its wait, the test's own or one the OS gives a
    /// condition variable's waiter unasked.
    woken: AtomicUsize,
    /// The boots its stop ended (D231's `BootStop`), each `Ended(Stopped)`.
    stopped: AtomicUsize,
    /// The boots that answered with a run started.
    answered: AtomicUsize,
    /// Where the next boots' runs' relays listen, as the test sets it: a run whose boot asks an endpoint then binds a
    /// real one ([`serve_lanes`]); none, and its endpoint is the model's address alone.
    serve: Mutex<Option<std::path::PathBuf>>,
    /// Where the model's run host serves its members' own lanes on Shards' real agent-control endpoint (D238, D252):
    /// the members it says ended, and where each signal its host takes is told ([`serve_members`]); none, and it
    /// serves none.
    steered: Mutex<Option<(Vec<u32>, Taken)>>,
    /// Where the model's run host serves its members' MCP bridges (D254) on Shards' real agent-control endpoint: the
    /// members whose bridge takes no connection, each other's connection its harness echoing what it is sent
    /// ([`serve_mcp`]); none, and it serves none.
    bridged: Mutex<Option<Vec<u32>>>,
    /// Where the model's run host serves programs run in its members (D243) on Shards' real agent-control endpoint: the
    /// members that refuse them, and where each exec's member and spec is told ([`serve_execs`]); none, and it serves
    /// none.
    executed: Mutex<Option<(Vec<u32>, Execed)>>,
    /// Where the model's run host names these members' MCP bridges (D254) in its real agent-control endpoint's report
    /// and serves no lane past it ([`serve_reports`]); none, and its endpoint is another's.
    reports: Mutex<Option<Vec<u32>>>,
}

impl ModelImages {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            asked: Mutex::new(Vec::new()),
            patience: Mutex::new(Vec::new()),
            refuse: Mutex::new(None),
            gate: Mutex::new(Gate::Open),
            opened: Condvar::new(),
            run: Arc::new(ModelRun::default()),
            members: Mutex::new(Vec::new()),
            receivers: Mutex::new(Vec::new()),
            named: Mutex::new(Vec::new()),
            period: Mutex::new(MODEL_RATE_PERIOD),
            started_from: Mutex::new(StartedFrom::Booted),
            abandon: AtomicBool::new(false),
            woken: AtomicUsize::new(0),
            stopped: AtomicUsize::new(0),
            answered: AtomicUsize::new(0),
            serve: Mutex::new(None),
            steered: Mutex::new(None),
            bridged: Mutex::new(None),
            executed: Mutex::new(None),
            reports: Mutex::new(None),
        })
    }

    /// The image's member `name` of `kind` reaches each of `reaches` by name.
    fn names(&self, kind: shards_api::image::MemberKind, name: &str, reaches: &[&str]) {
        self.named.lock().unwrap().push((
            kind,
            name.to_owned(),
            reaches.iter().map(|r| (*r).to_owned()).collect(),
        ));
    }

    /// The run host's hold of each member apart, as Shards' `api_run::held_apart` makes it (D246) and its session
    /// types its causes: where the boot names members' reaches, an entry for a member the image declares not is the
    /// image's refusal and a member's second entry the request's; every member the image declares reaches, of the
    /// names kept (the destinations and remote MCP servers alike), exactly its entry's, sorted and each once (a
    /// member in no entry, nothing), else the boot is refused as a widening; each before anything starts.
    fn held_apart(&self, egress: &shards_api::image::Narrowing) -> Result<(), ImageRefusal> {
        let Some(asked) = &egress.members else {
            return Ok(());
        };
        let (Keep::Only(destinations), Keep::Only(mcp)) = (&egress.destinations, &egress.mcp)
        else {
            return Err(ImageRefusal::Widening(
                "the model keeps no name wholesale".to_owned(),
            ));
        };
        let named = self.named.lock().unwrap();
        for (i, m) in asked.iter().enumerate() {
            if !named
                .iter()
                .any(|(kind, name, _)| *kind == m.kind && *name == m.name)
            {
                return Err(ImageRefusal::Image(format!(
                    "{:?} {}: the image declares no such member",
                    m.kind, m.name
                )));
            }
            if asked[..i]
                .iter()
                .any(|o| o.kind == m.kind && o.name == m.name)
            {
                return Err(ImageRefusal::Spec {
                    what: "the request",
                    why: format!("{:?} {}: its reach is named twice", m.kind, m.name),
                });
            }
        }
        for (kind, name, reaches) in named.iter() {
            let mut reach: Vec<&String> = reaches
                .iter()
                .filter(|r| destinations.contains(r) || mcp.contains(r))
                .collect();
            reach.sort();
            reach.dedup();
            let mut lets: Vec<&String> = asked
                .iter()
                .find(|m| m.kind == *kind && m.name == *name)
                .map(|m| m.destinations.iter().collect())
                .unwrap_or_default();
            lets.sort();
            lets.dedup();
            if reach != lets {
                return Err(ImageRefusal::Widening(format!(
                    "{kind:?} {name}: of the names kept it would reach {reach:?}, where the request lets it reach {lets:?} (D246)"
                )));
            }
        }
        Ok(())
    }

    /// The image gives guest port `port` coming in to each `(declared index, member's own port)` of `to`.
    fn receive(&self, port: u16, to: &[(u32, u16)]) {
        let to = to
            .iter()
            .map(|&(member, port)| Receiving { member, port })
            .collect();
        self.receivers.lock().unwrap().push((port, to));
    }

    fn gate(&self, gate: Gate) {
        *self.gate.lock().unwrap() = gate;
        self.opened.notify_all();
    }

    /// Wakes every boot held at the gate, so one whose stop has come sees it (D231's stop kills the run host it holds,
    /// which the model has none of).
    fn poke(&self) {
        drop(self.gate.lock().unwrap());
        self.opened.notify_all();
    }
}

impl Images for ModelImages {
    fn backend(&self) -> Result<shards_api::Backend, ImageRefusal> {
        Ok(shards_api::Backend::Kvm)
    }
    fn boot(
        &self,
        spec: &ImageSpec,
        patience: Duration,
    ) -> Result<Box<dyn ImageGuest>, ImageRefusal> {
        self.boot_stoppable(spec, patience, &shards_api::image::BootStop::new())
    }
    /// D231's boot ended by its stop: one held before its run host's start ends `Ended(Stopped)` once stopped, as the
    /// run host Shards kills ends it, the model told it as the test wakes it ([`ModelImages::poke`]); a stop that came
    /// first boots nothing.
    fn boot_stoppable(
        &self,
        spec: &ImageSpec,
        patience: Duration,
        stop: &shards_api::image::BootStop,
    ) -> Result<Box<dyn ImageGuest>, ImageRefusal> {
        self.asked.lock().unwrap().push(spec.clone());
        self.patience.lock().unwrap().push(patience);
        *self.run.lessor.lock().unwrap() = spec.lessor.clone();
        let gate = self.gate.lock().unwrap();
        // Held at its gate until it opens, or, unless the test abandons it, until its stop has come: a wake finds only
        // what the test or the boot's stop has opened.
        let gate = self
            .opened
            .wait_while(gate, |gate| {
                self.woken.fetch_add(1, Ordering::SeqCst);
                *gate == Gate::Host && (self.abandon.load(Ordering::SeqCst) || !stop.is_stopped())
            })
            .unwrap();
        if self.abandon.swap(false, Ordering::SeqCst) {
            panic!("the model's boot thread ends with no answer, as the test asked");
        }
        if stop.is_stopped() {
            self.stopped.fetch_add(1, Ordering::SeqCst);
            return Err(ImageRefusal::Ended(RunEnded::unused(
                RunEnd::Stopped,
                Unused::NotStarted,
            )));
        }
        let (gate, _) = self
            .opened
            .wait_timeout_while(gate, patience, |gate| *gate == Gate::Run)
            .unwrap();
        if *gate == Gate::Run {
            return Err(ImageRefusal::Boot(format!(
                "the run did not start within {patience:?}"
            )));
        }
        drop(gate);
        if let Some(why) = self.refuse.lock().unwrap().take() {
            return Err(why);
        }
        self.held_apart(&spec.egress)?;
        // Each port bound as asked, one the OS picks from IANA's dynamic range (RFC 6335 §6) where asked, naming whom
        // it reaches; one no member is given refused on a run of the members alone, as Shards' run host refuses it.
        let receivers = self.receivers.lock().unwrap().clone();
        let mut published = Vec::new();
        for (publish, picked) in spec.publish.iter().zip(49152u16..) {
            let to = receivers
                .iter()
                .find(|(port, _)| *port == publish.guest_port)
                .map(|(_, to)| to.clone())
                .unwrap_or_default();
            if to.is_empty() && spec.command.is_none() {
                return Err(ImageRefusal::Widening(format!(
                    "port {}: the image gives no member this port coming in, and the run has no command of its own",
                    publish.guest_port
                )));
            }
            published.push(Bound {
                guest_port: publish.guest_port,
                proto: publish.proto,
                host: std::net::SocketAddr::new(
                    publish.host.unwrap(),
                    if publish.host_port == 0 {
                        picked
                    } else {
                        publish.host_port
                    },
                ),
                to,
            });
        }
        // Each member whose stdin the boot asked for, its three pipes: the member's ends to the test, the
        // orchestrator's to its takes.
        let mut outside = std::collections::BTreeMap::new();
        for m in spec.members.iter().filter(|m| m.stdin) {
            let (stdin_r, stdin_w) = std::io::pipe().unwrap();
            let (stdout_r, stdout_w) = std::io::pipe().unwrap();
            let (stderr_r, stderr_w) = std::io::pipe().unwrap();
            self.run.inside.lock().unwrap().push((
                m.member.name.clone(),
                stdin_r,
                stdout_w,
                stderr_w,
            ));
            outside.insert(
                m.member.name.clone(),
                (Some(stdin_w), Some(stdout_r), Some(stderr_r)),
            );
        }
        self.answered.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(ModelGuest {
            run: Arc::clone(&self.run),
            members: self.members.lock().unwrap().clone(),
            published,
            outside: Mutex::new(outside),
            rate_period: *self.period.lock().unwrap(),
            started_from: *self.started_from.lock().unwrap(),
            control: match (
                spec.control.as_ref(),
                self.serve.lock().unwrap().clone(),
                self.steered.lock().unwrap().clone(),
                self.bridged.lock().unwrap().clone(),
                self.executed.lock().unwrap().clone(),
            ) {
                (None, ..) => None,
                (Some(ask), _, _, _, Some((refused, execed))) => {
                    Some(serve_execs(ask, refused, execed))
                }
                (Some(ask), _, _, Some(refused), None) => Some(serve_mcp(ask, refused)),
                (Some(ask), _, Some((ended, taken)), None, None) => {
                    Some(serve_members(ask, ended, taken))
                }
                (Some(ask), Some(dir), None, None, None) => Some(serve_lanes(ask, dir)),
                (Some(ask), None, None, None, None) => {
                    Some(match self.reports.lock().unwrap().clone() {
                        Some(bridges) => serve_reports(ask, &bridges),
                        None => model_control(),
                    })
                }
            },
            nesting: spec.nesting,
            egress: spec.egress.clone(),
        }))
    }
}

/// The runtime's waits for the model's answers, its boot's, a stats ask's and a stop's: facts no
/// recorded operation makes, so each measures, for its fact alone (hex_testkit::waits; the lead's rule
/// of 2026-11-08), never the program-end record. The model answers each at once while its gate is open.
fn waits() -> Result<ImageWaits, Refusal> {
    Ok(ImageWaits {
        boot: Patience::measuring("test.hex_runtime.image_boot"),
        stats: Patience::measuring("test.hex_runtime.image_stats"),
        stop: Patience::measuring("test.hex_runtime.image_stop"),
        delete_boot: Patience::measuring("test.hex_runtime.image_delete_boot"),
        started: Patience::measuring("test.hex_runtime.image_started"),
    })
}

/// A guest runtime with the model's image boot and record owner (the owner kept alive by the caller).
fn model_runtime(
    images: &Arc<ModelImages>,
    waits: Result<ImageWaits, Refusal>,
) -> (GuestRuntime, Arc<Owner>) {
    let owner = Arc::new(Owner::new());
    let runtime = GuestRuntime::new(
        Arc::new(shards_api::sim::SimGuests::new(|_| {
            shards_api::End::PowerOff
        })),
        super::super::tests::config(),
    )
    .with_images(
        Ok(Arc::clone(images) as Arc<dyn Images>),
        waits,
        owner.diagnostics(),
    );
    let books: Arc<dyn Books> = owner.clone();
    runtime.bind_books(Arc::downgrade(&books)).unwrap();
    (runtime, owner)
}

fn allocation() -> Allocation {
    hex_api::sim::sim_vm_allocation().unwrap()
}

fn spec(name: &str, command: Option<Vec<String>>) -> BoxSpec {
    BoxSpec {
        id: BoxId::new(name).unwrap(),
        bounds: Bounds::default(),
        class: ClassSpec::ShardsImage(Box::new(ShardsImage {
            command,
            ..ShardsImage::new(hex_api::sim::SIM_IMAGE)
        })),
        owner: None,
        devices: Vec::new(),
        allocation: Some(allocation()),
        route: None,
        band: 0,
    }
}

fn record(owner: &Owner, id: &BoxId) -> Option<BoxRecord> {
    Transaction::read(owner, id).unwrap()
}

/// The tests' waits for a box's state, each its own fact's: a held boot answering once the model's gate
/// opens, shown Running; and a run's own end reaped through its stop, shown Empty. No recorded operation
/// makes either, so each measures, for its fact alone (hex_testkit::waits).
static LATE_BOOT_SHOWN: Patience = Patience::measuring("test.hex_runtime.image_late_boot_shown");
static REAPED_SHOWN: Patience = Patience::measuring("test.hex_runtime.image_reaped_shown");
/// A run whose host exited without its run's end, reaped through its stop and shown Empty: a fact of its own, measuring
/// alike.
static HOST_GONE_SHOWN: Patience = Patience::measuring("test.hex_runtime.image_host_gone_shown");

/// Waits on `wait`, its fact's own patience, for box `id` to show `state`, by the box's life in the runtime (the review's
/// C-5): ended once the runtime has no such box. A look: the runtime tells no event as a box's state moves, its inspect
/// read, which the party reads too rather than inspecting again (an inspect may take a boot's answer).
fn shown(runtime: &GuestRuntime, id: &BoxId, state: BoxState, wait: &Patience) -> BoxInfo {
    let last = std::cell::RefCell::new(None);
    let gone = std::cell::Cell::new(false);
    follow::until(
        wait,
        &format!("{id} shown {state:?}"),
        || {
            if gone.get() {
                Party::Ended
            } else {
                Party::Live
            }
        },
        || match runtime.inspect(id) {
            Ok(info) => {
                let reached = info.state == state;
                *last.borrow_mut() = Some(info);
                reached
            }
            Err(Refusal::NoSuchBox(_)) => {
                gone.set(true);
                false
            }
            Err(why) => panic!("inspecting {id}: {why}"),
        },
    )
    .unwrap();
    last.into_inner().unwrap()
}

/// A thread the test spawned, as the party of a wait on what it makes: ended once it has finished.
fn spawned<T>(thread: &std::thread::ScopedJoinHandle<'_, T>) -> Party {
    if thread.is_finished() {
        Party::Ended
    } else {
        Party::Live
    }
}

/// Box `id`, as the party of a wait on what the runtime's own threads make for it: live while its record is kept.
fn kept(owner: &Owner, id: &BoxId) -> Party {
    if record(owner, id).is_some() {
        Party::Live
    } else {
        Party::Ended
    }
}

/// AC-T7.4.1 (joint plan step 12): do create, then start, an image VM of two vCPUs and 256 MiB with no command.
/// Expect: create records it Creating and shows it Created, of its own class, `shards-image` (its backend still
/// Shards'; docs/design/shards-image-class.md §11, P2.2), booting nothing; start writes Ready first
/// and boots once, asking Shards for exactly its pinned image, its admitted size, no command, none of its
/// image's grants (default deny until goal 15 programs them; no destination kept, D236) and no nesting grant
/// (D234: no harness is a nested runtime); then it shows Running.
#[test]
fn a_created_vm_boots_at_start_its_admitted_size_and_no_grant() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let made = runtime.create(spec("image-boot", None)).unwrap();
    let id = made.id.clone();
    assert_eq!(
        (made.class, made.state, made.exit),
        (hex_api::Class::ShardsImage, BoxState::Created, None)
    );
    assert_eq!(record(&owner, &id).unwrap().phase, Phase::Creating);
    assert!(images.asked.lock().unwrap().is_empty());
    runtime.start(&id).unwrap();
    assert_eq!(record(&owner, &id).unwrap().phase, Phase::Ready);
    let asked = images.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    let boot = &asked[0];
    assert_eq!(
        (
            boot.image.as_str(),
            boot.vcpus,
            boot.memory_mib,
            &boot.command
        ),
        (hex_api::sim::SIM_IMAGE, 2, 256, &None)
    );
    assert_eq!(boot.egress.egress, Keep::Only(Vec::new()));
    assert_eq!(boot.egress.mcp, Keep::Only(Vec::new()));
    // No destination by name kept (D236), where Narrowing::default() keeps every one the image grants.
    assert_eq!(boot.egress.destinations, Keep::Only(Vec::new()));
    assert!(!boot.egress.dns && boot.publish.is_empty() && boot.members.is_empty());
    assert_eq!((boot.nesting, boot.ready_by), (false, None));
    assert_eq!(
        boot.binary.as_deref(),
        Some(super::super::tests::config().binary.as_path())
    );
    // Its wait measures, so Shards' boot is given no bound of its own either.
    assert_eq!(*images.patience.lock().unwrap(), vec![Duration::MAX]);
    assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Running);
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Invalid { what: "start", .. })
    ));
    assert!(runtime.list().unwrap().iter().any(|info| info.id == id));
}

/// Joint plan step 12 (D242, image channel v20): a box's spec grants its VM no credential by reference, so its boot
/// names no lessor to lease one, which Shards takes as a run that asks nothing of a lessor (and would refuse with a
/// grant). Do create and start an image VM. Expect its one boot asked no credential grant and no lessor.
#[test]
fn w12_a_vm_granted_no_credential_boots_with_no_lessor() {
    let images = ModelImages::new();
    let (runtime, _owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-no-lessor", None)).unwrap().id;
    runtime.start(&id).unwrap();
    let asked = images.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].credentials.is_empty());
    assert!(asked[0].lessor.is_none());
}

/// Joint plan step 12, Hex's stop: do signal KILL to a running VM. Expect: its run stopped once, its
/// end recorded as Signal(9) and shown Empty; a second signal NotRunning; delete then removes its record.
#[test]
fn kill_stops_the_run_and_records_signal_9() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-stop", None)).unwrap().id;
    runtime.start(&id).unwrap();
    runtime.signal(&id, Signal::KILL).unwrap();
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 1);
    assert_eq!(record(&owner, &id).unwrap().exit, Some(Exit::Signal(9)));
    let info = runtime.inspect(&id).unwrap();
    assert_eq!(
        (info.state, info.exit),
        (BoxState::Empty, Some(Exit::Signal(9)))
    );
    assert!(matches!(
        runtime.signal(&id, Signal::KILL),
        Err(Refusal::NotRunning(_, "signal"))
    ));
    runtime.delete(&id).unwrap();
    assert!(record(&owner, &id).is_none());
    assert!(matches!(runtime.inspect(&id), Err(Refusal::NoSuchBox(_))));
}

/// Joint plan step 15 (AC-T7.7.2; N6): do start an image VM and inspect it, then KILL it and inspect it again. Expect the
/// running VM's inspect to say where its run's agent-control endpoint is bound, as its run host told it (the model's, on
/// the host's loopback), with no port published; and the ended VM's to say none.
#[test]
fn an_inspect_says_where_its_runs_agent_control_endpoint_is_bound_while_it_runs() {
    let images = ModelImages::new();
    let (runtime, _owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-control", None)).unwrap().id;
    runtime.start(&id).unwrap();
    let shown = runtime.inspect(&id).unwrap();
    assert_eq!(
        (shown.state, shown.control, shown.published.len()),
        (BoxState::Running, Some(model_control().address), 0)
    );
    runtime.signal(&id, Signal::KILL).unwrap();
    let ended = runtime.inspect(&id).unwrap();
    assert_eq!((ended.state, ended.control), (BoxState::Empty, None));
}

/// Joint plan step 12: do end a running VM's run on its own, its command exiting 3. Expect: its run
/// host reaped through its stop, its end recorded as Code(3) and shown Empty.
#[test]
fn a_run_that_ends_on_its_own_is_reaped_and_its_status_recorded() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let id = runtime
        .create(spec("image-exit", Some(vec!["exit".into(), "3".into()])))
        .unwrap()
        .id;
    runtime.start(&id).unwrap();
    *images.run.end.lock().unwrap() = Some(RunEnd::Exited(3));
    let info = shown(&runtime, &id, BoxState::Empty, &REAPED_SHOWN);
    assert_eq!(info.exit, Some(Exit::Code(3)));
    assert_eq!(record(&owner, &id).unwrap().exit, Some(Exit::Code(3)));
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 1);
}

/// Joint plan steps 12, 15 and 17 (ROOT's grants-none on KVM and HVF at c8b4ce6b, 2026-11-13: a probe of what Hex asks
/// Shards, beside image_real's runs, which ran): do boot a VM of its members alone granted nothing, once with none of its
/// members driven, as image_real's cases boot, and once with its harness driven by its caller (`ShardsImage::members`),
/// as image_grants_real's check 2 boots. Expect the two boots asked alike in all but `members`: nothing kept past the
/// microVM and every member held to reaching nothing (D246's `Some([])`), with no publish, in both; the second's
/// `members` its harness alone, its stdin asked. So of what check 2 sends, its driven member alone is not image_real's.
#[test]
fn a_driven_member_changes_nothing_a_boot_asks_but_its_members() {
    use hex_api::{ImageMember, Member, MemberKind};
    // Each boot on a runtime of its own, so neither waits at a bound the other's answer taught (decision 9).
    let asked_of = |members: Vec<ImageMember>| {
        let images = ModelImages::new();
        let (runtime, _owner) = model_runtime(&images, waits());
        let mut spec = spec("image-probe", None);
        if let ClassSpec::ShardsImage(image) = &mut spec.class {
            image.members = members;
        }
        let id = runtime.create(spec).unwrap().id;
        runtime.start(&id).unwrap();
        let asked = images.asked.lock().unwrap().clone();
        let [boot] = asked.as_slice() else {
            panic!("boots asked: {asked:?}");
        };
        boot.clone()
    };
    let first = asked_of(Vec::new());
    let second = asked_of(vec![ImageMember::new(Member {
        kind: MemberKind::Harness,
        name: "probe".to_owned(),
    })]);
    assert_eq!(first.egress.egress, Keep::Only(Vec::new()));
    assert_eq!(first.egress.mcp, Keep::Only(Vec::new()));
    assert_eq!(first.egress.destinations, Keep::Only(Vec::new()));
    assert_eq!(first.egress.members, Some(Vec::new()));
    assert!(!first.egress.dns && first.publish.is_empty() && first.members.is_empty());
    let [member] = second.members.as_slice() else {
        panic!("the driven boot's members: {:?}", second.members);
    };
    assert_eq!(member.member, MemberName::harness("probe"));
    assert!(member.stdin && !member.terminal);
    // Each asks its run's agent-control endpoint of its own runtime's key alone, no socket lane (goal 2's item 2).
    for boot in [&first, &second] {
        assert!(
            boot.control
                .as_ref()
                .is_some_and(|ask| ask.sockets.is_none()),
            "{:?}",
            boot.control
        );
    }
    let mut alike = second.clone();
    alike.members.clone_from(&first.members);
    alike.ready_by = first.ready_by;
    alike.control.clone_from(&first.control);
    assert_eq!(alike, first);
}

/// Joint plan step 12 for kiln's step 17 (D256, image channel v18): a member its caller drives takes its run-time
/// arguments after its image's command, as a harness takes its session's flags on the host. Do boot a VM whose driven
/// harness is given `--resume s-1`, an empty argument and `--model claude-opus-4`, and one whose driven harness is given
/// none. Expect the first boot to ask Shards for that member with exactly those arguments, in their order, the empty
/// one kept; the second for its member with none, its image's command alone.
#[test]
fn w12_a_driven_members_run_time_arguments_ride_its_boot_in_order() {
    use hex_api::{ImageMember, Member, MemberKind};
    let args = vec![
        "--resume".to_owned(),
        "s-1".to_owned(),
        String::new(),
        "--model".to_owned(),
        "claude-opus-4".to_owned(),
    ];
    for given in [args.clone(), Vec::new()] {
        let images = ModelImages::new();
        let (runtime, _owner) = model_runtime(&images, waits());
        let mut boxed = spec("image-args", None);
        if let ClassSpec::ShardsImage(image) = &mut boxed.class {
            image.members.push(ImageMember {
                args: given.clone(),
                ..ImageMember::new(Member {
                    kind: MemberKind::Harness,
                    name: "claude".to_owned(),
                })
            });
        }
        let id = runtime.create(boxed).unwrap().id;
        runtime.start(&id).unwrap();
        let asked = images.asked.lock().unwrap().clone();
        let [boot] = asked.as_slice() else {
            panic!("boots asked: {asked:?}");
        };
        let [member] = boot.members.as_slice() else {
            panic!("the boot's members: {:?}", boot.members);
        };
        assert_eq!(member.member, MemberName::harness("claude"));
        assert_eq!(member.args, given);
    }
}

/// Joint plan step 12, Shards' D254 (AC-T7.4.1): a member's MCP bridge reaches its run's controller over the run's
/// agent-control endpoint alone, which every image VM's run asks of this class's key (goal 2's item 2), so a member
/// asking one is asked of its run as its spec names it; its connections are `Runtime::mcp`'s (item 3). Do create and
/// start a VM driving a harness that asks no bridge and an agent that asks one. Expect both made and booted, the boot
/// asking the endpoint and the agent's bridge alone.
#[test]
fn w12_a_member_asking_an_mcp_bridge_is_asked_of_its_run_with_its_endpoint() {
    use hex_api::{ImageMember, Member, MemberKind};
    let images = ModelImages::new();
    let (runtime, _owner) = model_runtime(&images, waits());
    let mut boxed = spec("image-mcp", None);
    if let ClassSpec::ShardsImage(image) = &mut boxed.class {
        image.members = vec![
            ImageMember::new(Member {
                kind: MemberKind::Harness,
                name: "claude".to_owned(),
            }),
            ImageMember {
                mcp: true,
                ..ImageMember::new(Member {
                    kind: MemberKind::Agent,
                    name: "helper".to_owned(),
                })
            },
        ];
    }
    let id = runtime.create(boxed).unwrap().id;
    runtime.start(&id).unwrap();
    let asked = images.asked.lock().unwrap().last().cloned().unwrap();
    assert!(asked.control.is_some());
    assert_eq!(
        asked
            .members
            .iter()
            .map(|member| (member.member.name.as_str(), member.mcp))
            .collect::<Vec<_>>(),
        vec![("claude", false), ("helper", true)]
    );
    runtime.delete(&id).unwrap();
}

/// Joint plan step 12, Shards' D254 (AC-T7.4.1), red first: a member's ask for an MCP bridge rides its boot's member as
/// the box's spec says it (`MemberSpec::mcp`). Do lower a member asking none and one asking one. Expect the first's
/// boot member to ask none and the second's to ask it.
#[test]
fn w12_a_members_mcp_bridge_rides_its_boot_member() {
    use hex_api::{ImageMember, Member, MemberKind};
    let claude = ImageMember::new(Member {
        kind: MemberKind::Harness,
        name: "claude".to_owned(),
    });
    assert!(!member_spec(&claude).mcp);
    assert!(
        member_spec(&ImageMember {
            mcp: true,
            ..claude
        })
        .mcp
    );
}

/// Joint plan step 12 for kiln's step 18 (D231's shares): an image VM's guest sees the host directories its caller
/// names, and only those. Do boot a VM sharing a home volume read-write and a project read-only, the project given to its
/// harness as it starts (its member scope, Shards' D277; P1.4); one granted egress and sharing none; then one whose
/// share Shards refuses. Expect the first boot to ask Shards for exactly those two shares, each its host directory,
/// where the guest sees it and whether read-only, in order, its files run from (D279's `exec`), the home the run's own
/// root's alone and the project its harness's too; the second for none, its grants widening no share; the third's
/// start refused as Invalid, in Shards' own words.
#[test]
fn w12_an_image_vm_shares_exactly_the_host_directories_its_caller_names() {
    use hex_api::{Destination, Grant, Member, MemberKind, Mount, Ports, Proto};
    let shares = vec![
        Mount {
            source: std::path::PathBuf::from("/var/lib/kiln/homes/ada/.claude"),
            destination: "/home/agent/.claude".to_owned(),
            read_only: false,
            members: Vec::new(),
        },
        Mount {
            source: std::path::PathBuf::from("/srv/projects/hex"),
            destination: "/work/hex".to_owned(),
            read_only: true,
            members: vec![Member {
                kind: MemberKind::Harness,
                name: "codex".to_owned(),
            }],
        },
    ];
    let boot_of = |shares: Vec<Mount>, grants: Vec<Grant>| {
        let images = ModelImages::new();
        images.names(
            shards_api::image::MemberKind::Harness,
            "codex",
            &["api.anthropic.com:443"],
        );
        let (runtime, _owner) = model_runtime(&images, waits());
        let mut boxed = spec("image-shares", None);
        if let ClassSpec::ShardsImage(image) = &mut boxed.class {
            image.shares = shares;
            image.grants = grants;
        }
        let id = runtime.create(boxed).unwrap().id;
        runtime.start(&id).unwrap();
        let asked = images.asked.lock().unwrap().clone();
        let [boot] = asked.as_slice() else {
            panic!("boots asked: {asked:?}");
        };
        boot.clone()
    };
    let shared = boot_of(shares.clone(), Vec::new());
    let asked: Vec<(std::path::PathBuf, String, bool)> = shared
        .shares
        .iter()
        .map(|s| (s.host.clone(), s.at.clone(), s.read_only))
        .collect();
    let named: Vec<(std::path::PathBuf, String, bool)> = shares
        .iter()
        .map(|m| (m.source.clone(), m.destination.clone(), m.read_only))
        .collect();
    assert_eq!(asked, named);
    assert_eq!(
        shared
            .shares
            .iter()
            .map(|s| (s.exec, s.members.clone()))
            .collect::<Vec<_>>(),
        vec![
            (true, None),
            (
                true,
                Some(PlugFor::Members(vec![MemberName::harness("codex")]))
            ),
        ]
    );
    let granted = boot_of(
        Vec::new(),
        vec![Grant::Egress {
            member: Member {
                kind: MemberKind::Harness,
                name: "codex".to_owned(),
            },
            to: Destination::Name("api.anthropic.com".to_owned()),
            ports: Ports {
                proto: Proto::Tcp,
                first: 443,
                last: 443,
            },
        }],
    );
    assert!(granted.shares.is_empty(), "{:?}", granted.shares);
    let images = ModelImages::new();
    let words = "/srv/projects/hex: a share's directory is an absolute UTF-8 path";
    *images.refuse.lock().unwrap() = Some(ImageRefusal::Spec {
        what: "a share",
        why: words.to_owned(),
    });
    let (runtime, _owner) = model_runtime(&images, waits());
    let mut boxed = spec("image-share-refused", None);
    if let ClassSpec::ShardsImage(image) = &mut boxed.class {
        image.shares = shares;
    }
    let id = runtime.create(boxed).unwrap().id;
    match runtime.start(&id) {
        Err(Refusal::Invalid { what, why }) => {
            assert_eq!(what, "shards image");
            assert!(why.contains("a share") && why.contains(words), "{why}");
        }
        other => panic!("{other:?}"),
    }
}

/// Joint plan steps 12 and 15 (ROOT's KVM run of image_grants_real at c8b4ce6b, 2026-11-13): do end a running VM's run
/// as Shards' run host does when it exits without its run's end, what it said last its only words (shards-api reads that
/// end as `Failed` in them, and its stop as `Stopped`). Expect: its run host reaped through its stop, and the box shown
/// Empty with an unknown exit and those words, by inspect and in its record, never a KILL Hex never sent.
#[test]
fn a_run_whose_host_exited_without_its_end_ends_in_its_hosts_words() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-host-gone", None)).unwrap().id;
    runtime.start(&id).unwrap();
    let said = "init: the microVM's last words";
    *images.run.gone.lock().unwrap() = Some(said.to_owned());
    let info = shown(&runtime, &id, BoxState::Empty, &HOST_GONE_SHOWN);
    let words = format!("the run host exited without its run's end: {said}");
    assert_eq!(
        (info.exit, info.ended.as_deref()),
        (Some(Exit::Unknown), Some(words.as_str()))
    );
    let kept = record(&owner, &id).unwrap();
    assert_eq!(
        (kept.exit, kept.ended.as_deref()),
        (Some(Exit::Unknown), Some(words.as_str()))
    );
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 1);
}

/// Joint plan step 12 (HEX-IMG-3 on KVM, ROOT 2026-11-13): do end a running VM's run as Shards' run host ends one whose
/// own command never started (D249: `Failed`, in its VM's words). Expect: its run host reaped through its stop, and the
/// box shown Empty with an unknown exit and those words, by inspect and by list, its record keeping both.
#[test]
fn a_run_that_failed_ends_saying_why_in_its_runtimes_words() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let command = vec!["/bin/sh".into(), "-c".into(), "exit 3".into()];
    let id = runtime
        .create(spec("image-never-started", Some(command)))
        .unwrap()
        .id;
    runtime.start(&id).unwrap();
    let words =
        "its command never started (status 128): exec: \"/bin/sh\": operation not permitted";
    *images.run.end.lock().unwrap() = Some(RunEnd::Failed(words.to_owned()));
    let info = shown(&runtime, &id, BoxState::Empty, &REAPED_SHOWN);
    assert_eq!(
        (info.exit, info.ended.as_deref()),
        (Some(Exit::Unknown), Some(words))
    );
    let listed = runtime.list().unwrap();
    assert!(
        listed
            .iter()
            .any(|info| info.id == id && info.ended.as_deref() == Some(words)),
        "{listed:?}"
    );
    let kept = record(&owner, &id).unwrap();
    assert_eq!(
        (kept.exit, kept.ended.as_deref()),
        (Some(Exit::Unknown), Some(words))
    );
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 1);
}

/// The review's B-9 (ROOT's ruling of 2026-11-14, shards-image-class.md §1): an image VM's diagnostics are its own output
/// on the node's disk, charged to it, so their file is held within the disk its box's allocation was admitted with
/// (Shards' bounded `keep_diagnostics`), no number picked. Do start an image VM whose box was admitted with the sim's VM
/// allocation. Expect Shards told once to keep its diagnostics, within exactly that allocation's disk.
#[test]
fn b9_an_image_vms_diagnostics_are_held_within_the_disk_its_box_was_admitted_with() {
    let images = ModelImages::new();
    let (runtime, _owner) = model_runtime(&images, waits());
    let id = runtime
        .create(spec("image-diagnostics-bound", None))
        .unwrap()
        .id;
    runtime.start(&id).unwrap();
    assert_eq!(
        *images.run.diagnostics_bounds.lock().unwrap(),
        [allocation().resources.disk_bytes]
    );
}

/// The review's B-9, its follow-up (the KVM suite at 27cdafe0c: HEX-IMG-1..5 and the node under shards-init refused, "its
/// admitted disk, 0 bytes, is short of the 12413 bytes of its run host's diagnostics Shards keeps to quote a run's
/// end"): a box admitted with less disk than the tail Shards keeps to quote its end, as an image or node VM whose root is
/// in its RAM admits none, still starts, its diagnostics held within that tail, Shards' own bound. Do start an image VM
/// whose run host keeps a tail one byte past the disk its box was admitted with. Expect it started, and its diagnostics
/// kept once, within that tail.
#[test]
fn b9_a_box_admitted_with_less_disk_than_shards_keeps_holds_that_tail() {
    let images = ModelImages::new();
    let tail = allocation().resources.disk_bytes + 1;
    *images.run.diagnostics_tail.lock().unwrap() = Some(tail);
    let (runtime, _owner) = model_runtime(&images, waits());
    let id = runtime
        .create(spec("image-diagnostics-tail", None))
        .unwrap()
        .id;
    runtime.start(&id).unwrap();
    assert_eq!(*images.run.diagnostics_bounds.lock().unwrap(), [tail]);
    assert_eq!(images.run.diagnostics.lock().unwrap().len(), 1);
}

/// An image VM's spec given `volumes` (kiln plan 01a1dd2e…, P6.2), its box admitted with the sim's VM and `disk` bytes
/// of disk, which its volumes' bounds are drawn from.
fn volumed(name: &str, disk: u64, volumes: Vec<ImageVolume>) -> BoxSpec {
    let mut cost = allocation().cost;
    cost.ceiling.disk_bytes = disk;
    let mut workload = allocation().resources;
    workload.disk_bytes = disk;
    BoxSpec {
        class: ClassSpec::ShardsImage(Box::new(ShardsImage {
            volumes,
            ..ShardsImage::new(hex_api::sim::SIM_IMAGE)
        })),
        allocation: Some(cost.allocate(workload).unwrap()),
        ..spec(name, None)
    }
}

/// A GiB, a volume's bound in these tests.
const GIB: u64 = 1 << 30;

/// A harness's 1 GiB volume, read-write at `/volumes/cache` (kiln's own place for an agent's volumes).
fn cache() -> ImageVolume {
    ImageVolume {
        name: "kiln-agent-cache".to_owned(),
        bound: NonZeroU64::new(GIB).unwrap(),
        at: "/volumes/cache".to_owned(),
        members: vec![Member {
            kind: MemberKind::Harness,
            name: "claude".to_owned(),
        }],
        read_only: false,
    }
}

/// Kiln plan 01a1dd2e…, P6.2 (AC-T7.4.1; Shards' D271, D281): an image VM's volumes are plugged into its members as its
/// run starts, beneath the plug root its run is granted, each held to its bound, and its diagnostics held within what
/// its admitted disk leaves. Do start a VM admitted with 3 GiB of disk, given a 1 GiB volume read-write into its
/// harness and a 1 GiB one read-only into its harness and an agent, both beneath `/volumes`. Expect its boot granted
/// `/volumes` alone as a plug root, each volume plugged once and in its order, by its name and bound, into its members,
/// at its place, read-only as asked and its files runnable, before its start answers; and its diagnostics held within
/// the 1 GiB its volumes leave.
#[test]
fn p6_2_an_image_vms_volumes_are_plugged_beneath_their_root_as_it_starts() {
    let images = ModelImages::new();
    let (runtime, _owner) = model_runtime(&images, waits());
    let harness = cache().members;
    let both = vec![
        harness[0].clone(),
        Member {
            kind: MemberKind::Agent,
            name: "helper".to_owned(),
        },
    ];
    let notes = ImageVolume {
        name: "kiln-agent-notes".to_owned(),
        at: "/volumes/notes".to_owned(),
        members: both.clone(),
        read_only: true,
        ..cache()
    };
    let id = runtime
        .create(volumed("image-volumes", 3 * GIB, vec![cache(), notes]))
        .unwrap()
        .id;
    runtime.start(&id).unwrap();
    assert_eq!(
        images.asked.lock().unwrap().last().unwrap().plug_roots,
        ["/volumes"]
    );
    let bound = Some(NonZeroU64::new(GIB).unwrap());
    let plug = |name: &str, members: &[Member], at: &str, read_only: bool| Plug {
        source: PlugSource::Volume {
            name: name.to_owned(),
            bound,
        },
        to: PlugFor::Members(members.iter().map(member_name).collect()),
        at: at.to_owned(),
        read_only,
        exec: true,
    };
    assert_eq!(
        *images.run.plugs.lock().unwrap(),
        [
            plug("kiln-agent-cache", &harness, "/volumes/cache", false),
            plug("kiln-agent-notes", &both, "/volumes/notes", true),
        ]
    );
    assert_eq!(*images.run.diagnostics_bounds.lock().unwrap(), [GIB]);
}

/// Kiln plan 01a1dd2e…, P6.2 (AC-T7.4.1; Shards' D281, R04): a volume its run host refuses as the run starts refuses
/// the start, typed by what holds the volume, the run stopped and the box ended in Shards' words. Do start a VM given a
/// volume its run host refuses `Bound` (the runs holding it serve it with another bound), then one given a volume
/// refused `Clearing` (a clearing holds it). Expect each start refused `Volume` with that cause in Shards' words,
/// nothing plugged, its run stopped once, and the box shown Empty, its exit unknown and its `ended` quoting them.
#[test]
fn p6_2_a_volume_its_run_host_refuses_refuses_the_start_typed() {
    for (cause, typed) in [
        (PlugRefusal::Bound, VolumeCause::Bound),
        (PlugRefusal::Clearing, VolumeCause::Clearing),
    ] {
        let images = ModelImages::new();
        let (runtime, _owner) = model_runtime(&images, waits());
        let words = format!("volume kiln-agent-cache: {cause:?}, as the test asked");
        *images.run.refuse_plug.lock().unwrap() = Some(ImageRefusal::Plug(cause, words.clone()));
        let id = runtime
            .create(volumed("image-volume-refused", GIB, vec![cache()]))
            .unwrap()
            .id;
        assert_eq!(
            runtime.start(&id),
            Err(Refusal::Volume {
                cause: typed,
                why: words.clone()
            })
        );
        assert!(images.run.plugs.lock().unwrap().is_empty());
        assert_eq!(images.run.stops.load(Ordering::SeqCst), 1);
        let info = runtime.inspect(&id).unwrap();
        assert_eq!(
            (info.state, info.exit),
            (BoxState::Empty, Some(Exit::Unknown))
        );
        assert!(
            info.ended
                .as_deref()
                .is_some_and(|ended| ended.contains(&words)),
            "{info:?}"
        );
    }
}

/// Kiln plan 01a1dd2e…, P6.2 (AC-T7.4.1): a VM's volumes are refused before anything is made where they could never be
/// given: their bounds together past the disk its box was admitted with (no box writes past its admission), or one
/// beneath a directory its guest keeps its own (Shards' `plug::root_name`). Do create a VM admitted with 1 GiB of disk
/// given two 1 GiB volumes, and one given a volume beneath `/proc`. Expect each refused Invalid saying why, with no
/// record made. Do create one admitted with 2 GiB given the same two. Expect it made.
#[test]
fn p6_2_volumes_past_the_admitted_disk_or_beneath_inits_own_are_refused_at_create() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let notes = ImageVolume {
        name: "kiln-agent-notes".to_owned(),
        at: "/volumes/notes".to_owned(),
        ..cache()
    };
    match runtime.create(volumed(
        "image-volumes-past",
        GIB,
        vec![cache(), notes.clone()],
    )) {
        Err(Refusal::Invalid { why, .. }) => assert!(why.contains("admitted"), "{why}"),
        other => panic!("volumes past the admitted disk: {other:?}"),
    }
    let proc = ImageVolume {
        at: "/proc/cache".to_owned(),
        ..cache()
    };
    match runtime.create(volumed("image-volume-proc", GIB, vec![proc])) {
        Err(Refusal::Invalid { why, .. }) => assert!(why.contains("\"/proc\""), "{why}"),
        other => panic!("a volume beneath /proc: {other:?}"),
    }
    for refused in ["image-volumes-past", "image-volume-proc"] {
        assert!(record(&owner, &BoxId::new(refused).unwrap()).is_none());
    }
    runtime
        .create(volumed("image-volumes-fit", 2 * GIB, vec![cache(), notes]))
        .unwrap();
}

/// Kiln plan 01a1dd2e…, P6.2 (AC-T7.4.1, R04): each of Shards' plug refusals typed by what refused it, a run's volumes
/// as it starts being the class's only plugs. Expect what holds the volume, `Bound` and `Clearing`, as `Volume` by its
/// retry; the spec's (`Request`, `Member`, `Outside`, `Taken`, `Volume`) as Invalid; its host's and its microVM's
/// (`Host`, `Device`) as Os; an unplug's (`Unknown`, `Busy`) as the class's own fault, Broken; each in Shards' words.
#[test]
fn p6_2_each_plug_refusal_is_typed_by_what_refused_it() {
    let id = BoxId::new("image-plug-refused").unwrap();
    let words = "Shards' words".to_owned();
    let typed = |cause| {
        refused(
            &id,
            "booting a Shards image VM",
            ImageRefusal::Plug(cause, words.clone()),
        )
    };
    assert_eq!(
        typed(PlugRefusal::Bound),
        Refusal::Volume {
            cause: VolumeCause::Bound,
            why: words.clone()
        }
    );
    assert_eq!(
        typed(PlugRefusal::Clearing),
        Refusal::Volume {
            cause: VolumeCause::Clearing,
            why: words.clone()
        }
    );
    for cause in [
        PlugRefusal::Request,
        PlugRefusal::Member,
        PlugRefusal::Outside,
        PlugRefusal::Taken,
        PlugRefusal::Volume,
    ] {
        assert!(
            matches!(typed(cause), Refusal::Invalid { what: "shards image's volume", ref why } if why.contains(&words)),
            "{cause:?}"
        );
    }
    for cause in [PlugRefusal::Host, PlugRefusal::Device] {
        assert!(
            matches!(typed(cause), Refusal::Os { op: "booting a Shards image VM", ref why } if why.contains(&words)),
            "{cause:?}"
        );
    }
    for cause in [PlugRefusal::Unknown, PlugRefusal::Busy] {
        assert!(
            matches!(typed(cause), Refusal::Broken(ref why) if why.contains(&words)),
            "{cause:?}"
        );
    }
}

/// Kiln plan 01a1dd2e…, P6.2 (AC-T7.4.1, R04): Shards' refusals of its home's volumes' listing or a clearing typed as
/// the call it answers. Expect a volume a run holds `Volume { InUse }` and one the home has none of `Volume { Unknown
/// }`, each in Shards' words; a name the store takes no volume by Invalid; and files not all removed, a `shards` that
/// didn't start, one that ended unanswered and one of another version, Os.
#[test]
fn p6_2_a_clearings_refusals_are_typed_by_what_refused_it() {
    let op = "clearing a Shards home's volume";
    assert_eq!(
        volume_refused(
            op,
            VolumeRefusal::Clear(ClearCause::InUse, "held".to_owned())
        ),
        Refusal::Volume {
            cause: VolumeCause::InUse,
            why: "held".to_owned()
        }
    );
    assert_eq!(
        volume_refused(
            op,
            VolumeRefusal::Clear(ClearCause::Unknown, "none".to_owned())
        ),
        Refusal::Volume {
            cause: VolumeCause::Unknown,
            why: "none".to_owned()
        }
    );
    assert_eq!(
        volume_refused(op, VolumeRefusal::Clear(ClearCause::Name, "bad".to_owned())),
        Refusal::Invalid {
            what: "volume",
            why: "bad".to_owned()
        }
    );
    for why in [
        VolumeRefusal::Clear(ClearCause::Store, "left".to_owned()),
        VolumeRefusal::Start("no shards there".to_owned()),
        VolumeRefusal::Ended {
            status: Some(1),
            said: "said".to_owned(),
        },
        VolumeRefusal::Version(9),
    ] {
        assert!(
            matches!(
                volume_refused(op, why.clone()),
                Refusal::Os {
                    op: "clearing a Shards home's volume",
                    ..
                }
            ),
            "{why:?}"
        );
    }
}

/// Joint plan step 12 (D258's `keep_diagnostics`; ROOT, 2026-11-13): a box's run host's diagnostics are kept whole in one
/// file under the node's state, named in its record as its start's intent is written, and a failed run's `ended` quotes
/// their last bytes after its own words; the file goes with the box's delete. Do start an image VM, write its run host's
/// diagnostics into the file the class named, then end its run failed and inspect it; then delete it. Expect its record
/// to name the class's file for the box, Shards told once to keep the diagnostics there; the box shown Empty, its
/// `ended` its run's words, then the run host's last words from the file, its record keeping the same; after the
/// delete, the file gone with the record.
#[test]
fn w12_a_failed_runs_ended_quotes_its_run_hosts_diagnostics_and_its_delete_removes_them() {
    use std::io::Write as _;
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-diagnostics", None)).unwrap().id;
    runtime.start(&id).unwrap();
    let file = owner.diagnostics().join("image-diagnostics.log");
    assert_eq!(
        record(&owner, &id).unwrap().diagnostics.as_deref(),
        Some(file.as_path())
    );
    assert_eq!(images.run.diagnostics.lock().unwrap().len(), 1);
    let last =
        "api-run: its VM's kernel panicked: VFS: unable to mount root fs on unknown-block(0,0)";
    {
        let mut kept = images.run.diagnostics.lock().unwrap();
        let to = kept.last_mut().unwrap();
        to.write_all(b"api-run: booting its microVM\n").unwrap();
        to.write_all(format!("{last}\n").as_bytes()).unwrap();
    }
    let words = "its VM failed";
    *images.run.end.lock().unwrap() = Some(RunEnd::Failed(words.to_owned()));
    let info = shown(&runtime, &id, BoxState::Empty, &REAPED_SHOWN);
    let ended = info.ended.unwrap();
    assert!(
        ended.starts_with(words) && ended.trim_end().ends_with(last),
        "{ended}"
    );
    assert_eq!(
        record(&owner, &id).unwrap().ended.as_deref(),
        Some(ended.as_str())
    );
    runtime.delete(&id).unwrap();
    assert!(!file.exists(), "{file:?} outlived its box");
    assert!(record(&owner, &id).is_none());
}

/// Joint plan step 12 (goal 15's finding; ROOT's (b), 2026-11-13): do start a VM whose run fails as it boots, its command
/// never started (D249's `Failed`, in its VM's words), the run host's boot answering with the run up. Expect: the start
/// refused `Failed`, naming the box with those words, never answered Ok; the box then shown Empty with an unknown exit
/// and the same words, its record keeping them, its run host reaped through its stop.
#[test]
fn a_run_that_fails_as_it_boots_fails_its_start_in_its_runtimes_words() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let command = vec!["/bin/sh".into(), "-c".into(), "exit 3".into()];
    let id = runtime
        .create(spec("image-fails-as-it-boots", Some(command)))
        .unwrap()
        .id;
    let words =
        "its command never started (status 128): exec: \"/bin/sh\": operation not permitted";
    *images.run.end.lock().unwrap() = Some(RunEnd::Failed(words.to_owned()));
    assert_eq!(
        runtime.start(&id),
        Err(Refusal::Failed {
            id: id.clone(),
            why: words.to_owned(),
        })
    );
    let info = shown(&runtime, &id, BoxState::Empty, &REAPED_SHOWN);
    assert_eq!(
        (info.exit, info.ended.as_deref()),
        (Some(Exit::Unknown), Some(words))
    );
    let kept = record(&owner, &id).unwrap();
    assert_eq!(
        (kept.exit, kept.ended.as_deref()),
        (Some(Exit::Unknown), Some(words))
    );
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 1);
}

/// Joint plan step 12 (ROOT's (b), 2026-11-13): do start a VM whose command exits 3 at once, ended as the boot answers.
/// Expect: the start answered Ok, as `docker start` answers a command that started, however soon it ended; the box then
/// shown Empty with Code(3) and no words, its run host reaped through its stop.
#[test]
fn a_command_that_exits_at_once_has_started() {
    let images = ModelImages::new();
    let (runtime, _owner) = model_runtime(&images, waits());
    let command = vec!["/bin/sh".into(), "-c".into(), "exit 3".into()];
    let id = runtime
        .create(spec("image-exits-at-once", Some(command)))
        .unwrap()
        .id;
    *images.run.end.lock().unwrap() = Some(RunEnd::Exited(3));
    assert_eq!(runtime.start(&id), Ok(()));
    let info = shown(&runtime, &id, BoxState::Empty, &REAPED_SHOWN);
    assert_eq!((info.exit, info.ended), (Some(Exit::Code(3)), None));
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 1);
}

/// What a model image runs, by index: a harness and an agent that each run a program, and files that run none.
fn three_members() -> Vec<MemberInfo> {
    let member = |index, kind, name: &str, runs| MemberInfo {
        index,
        kind,
        name: name.to_owned(),
        runs,
    };
    vec![
        member(0, shards_api::image::MemberKind::Harness, "claude", true),
        member(1, shards_api::image::MemberKind::Agent, "helper", true),
        member(2, shards_api::image::MemberKind::Agent, "notes", false),
    ]
}

/// Goal 2's item 2 (D238, D252; ROOT, 2026-11-14: a signal reaches every process in the box, a VM's its members'):
/// a signal but KILL to a running VM rides each live member's own lane on its run's agent-control endpoint, which the
/// run asks of this class's key alone, its controller dialed as the signal goes; a member that has ended is skipped,
/// and one that runs no program is none a signal reaches. A program runs in a member it names (`exec_member`), so an
/// exec naming none is refused. Do: a run whose endpoint is Shards' own on loopback, its members a harness, an agent
/// that has ended and files; exec in it naming no member, then send it TERM. Expect: Invalid naming exec; TERM taken,
/// the harness's lane alone steered with 15, the box Running, no stop; the run's spec asking the endpoint of one key
/// and no socket lane.
#[test]
fn item2_term_reaches_every_live_member_and_the_vm_runs_on() {
    let images = ModelImages::new();
    *images.members.lock().unwrap() = three_members();
    let taken = Taken::default();
    *images.steered.lock().unwrap() = Some((vec![1], Arc::clone(&taken)));
    let (runtime, _owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-members", None)).unwrap().id;
    runtime.start(&id).unwrap();
    let asked = images.asked.lock().unwrap().last().cloned().unwrap();
    assert!(
        matches!(&asked.control, Some(ask) if ask.sockets.is_none()),
        "{:?}",
        asked.control
    );
    assert!(matches!(
        runtime.exec(&id, hex_api::Program::of(&["true"]), hex_api::Stdio::null()),
        Err(Refusal::Invalid { what: "exec", .. })
    ));
    assert_eq!(runtime.signal(&id, Signal::TERM), Ok(()));
    assert_eq!(*taken.lock().unwrap(), vec![(0, 15)]);
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Running);
}

/// Goal 2's item 2 (ROOT, 2026-11-14): a signal but KILL to a running VM none of whose members a signal reaches (each
/// runs no program, or has ended) is refused NotRunning, "signal: its run has no member a signal reaches", which says
/// that nothing in the run can take it, not that the VM ended: it runs on. Do: one run whose members are files alone,
/// another whose one member has ended; send each TERM. Expect: each refused so, no signal taken, each Running.
#[test]
fn item2_term_to_a_run_with_no_member_a_signal_reaches_is_refused_not_running() {
    for (name, members, ended) in [
        (
            "image-files-alone",
            vec![three_members().swap_remove(2)],
            vec![],
        ),
        (
            "image-members-ended",
            vec![three_members().swap_remove(0)],
            vec![0],
        ),
    ] {
        let images = ModelImages::new();
        *images.members.lock().unwrap() = members;
        let taken = Taken::default();
        *images.steered.lock().unwrap() = Some((ended, Arc::clone(&taken)));
        let (runtime, _owner) = model_runtime(&images, waits());
        let id = runtime.create(spec(name, None)).unwrap().id;
        runtime.start(&id).unwrap();
        assert_eq!(
            runtime.signal(&id, Signal::TERM),
            Err(Refusal::NotRunning(
                id.clone(),
                "signal: its run has no member a signal reaches"
            )),
            "{name}"
        );
        assert!(taken.lock().unwrap().is_empty(), "{name}");
        assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Running);
    }
}

/// Goal 2's item 2 (ROOT, 2026-11-14: the node's wire carries a member's signal, `Runtime::signal_member`): a signal to
/// one member of a running VM rides that member's own lane on its run's agent-control endpoint alone, KILL among them:
/// the member's, never its VM's, which runs on. Do: a run whose endpoint is Shards' own on loopback, its members a
/// harness, an agent and files; send the agent TERM, then the harness KILL. Expect: each done; the agent's lane alone
/// steered with 15, then the harness's with 9; no stop; the box Running.
#[test]
fn w2_a_members_signal_rides_its_own_lane_and_the_vm_runs_on() {
    let images = ModelImages::new();
    *images.members.lock().unwrap() = three_members();
    let taken = Taken::default();
    *images.steered.lock().unwrap() = Some((Vec::new(), Arc::clone(&taken)));
    let (runtime, _owner) = model_runtime(&images, waits());
    let id = runtime
        .create(spec("image-member-signal", None))
        .unwrap()
        .id;
    runtime.start(&id).unwrap();
    let member = |kind, name: &str| Member {
        kind,
        name: name.to_owned(),
    };
    assert_eq!(
        runtime.signal_member(&id, &member(MemberKind::Agent, "helper"), Signal::TERM),
        Ok(())
    );
    assert_eq!(
        runtime.signal_member(&id, &member(MemberKind::Harness, "claude"), Signal::KILL),
        Ok(())
    );
    assert_eq!(*taken.lock().unwrap(), vec![(1, 15), (0, 9)]);
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Running);
}

/// Goal 2's item 2: a member's own signal that reaches no program is refused, typed: NotRunning, "signal a member",
/// before its run's start; NotRunning, "signal a member: it has ended", for a member that has ended, and "signal a
/// member: it runs no program" for one of files alone; Invalid for a member its run declares not. Do: a run whose
/// endpoint is Shards' own on loopback, its agent ended; TERM to the harness before the start, then, after it, TERM to
/// the agent, the files and a harness the run declares not. Expect: each refused so, no signal taken, the box Running.
#[test]
fn w2_a_members_signal_that_reaches_no_program_is_refused_typed() {
    let images = ModelImages::new();
    *images.members.lock().unwrap() = three_members();
    let taken = Taken::default();
    *images.steered.lock().unwrap() = Some((vec![1], Arc::clone(&taken)));
    let (runtime, _owner) = model_runtime(&images, waits());
    let id = runtime
        .create(spec("image-member-unreached", None))
        .unwrap()
        .id;
    let member = |kind, name: &str| Member {
        kind,
        name: name.to_owned(),
    };
    assert_eq!(
        runtime.signal_member(&id, &member(MemberKind::Harness, "claude"), Signal::TERM),
        Err(Refusal::NotRunning(id.clone(), "signal a member"))
    );
    runtime.start(&id).unwrap();
    assert_eq!(
        runtime.signal_member(&id, &member(MemberKind::Agent, "helper"), Signal::TERM),
        Err(Refusal::NotRunning(
            id.clone(),
            "signal a member: it has ended"
        ))
    );
    assert_eq!(
        runtime.signal_member(&id, &member(MemberKind::Agent, "notes"), Signal::TERM),
        Err(Refusal::NotRunning(
            id.clone(),
            "signal a member: it runs no program"
        ))
    );
    let undeclared =
        runtime.signal_member(&id, &member(MemberKind::Harness, "codex"), Signal::TERM);
    assert!(
        matches!(&undeclared, Err(Refusal::Invalid { what: "member", .. })),
        "{undeclared:?}"
    );
    assert!(taken.lock().unwrap().is_empty());
    assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Running);
}

/// Goal 2's item 2 (D238; Shards' #72): a member's own kill rides its own lane on its run's agent-control endpoint,
/// dialed as it goes, and the member ends killed, Signal(9), never a code. Do: take a harness its caller drives, of a run
/// whose endpoint is Shards' own on loopback; kill it, its end then told as its run host tells it. Expect: the kill
/// taken, its lane steered with 9, and its end Signal(9).
#[test]
fn item2_a_members_kill_rides_its_lane_and_it_ends_killed() {
    let images = ModelImages::new();
    *images.members.lock().unwrap() = vec![three_members().swap_remove(0)];
    let taken = Taken::default();
    *images.steered.lock().unwrap() = Some((Vec::new(), Arc::clone(&taken)));
    let (runtime, _owner) = model_runtime(&images, waits());
    let claude = Member {
        kind: MemberKind::Harness,
        name: "claude".to_owned(),
    };
    let mut boxed = spec("image-member-killed", None);
    if let ClassSpec::ShardsImage(image) = &mut boxed.class {
        image.members.push(ImageMember::new(claude.clone()));
    }
    let id = runtime.create(boxed).unwrap().id;
    runtime.start(&id).unwrap();
    let mut proc = runtime.member(&id, &claude).unwrap();
    assert_eq!(proc.kill(), Ok(()));
    assert_eq!(*taken.lock().unwrap(), vec![(0, 9)]);
    images
        .run
        .member_ends
        .lock()
        .unwrap()
        .insert("claude".to_owned(), MemberEnd::Signaled(9));
    assert_eq!(
        proc.wait_for(Duration::ZERO).unwrap(),
        Some(Exit::Signal(9))
    );
}

/// Joint plan step 17, AC-T7.4.1: do create an image VM whose spec names a harness its caller drives, with a
/// variable and a working directory, take it before the start, start it, and take it. Expect: before the start
/// NotRunning; the boot asks Shards for that member's stdin with its variable and directory and no terminal; the
/// caller's ends of its three streams, given once; what the caller writes read by the member in order, and its
/// end of input at the caller's close; what the member writes on stdout and stderr read whole by the caller; no
/// end until the member's, then its status; its kill, its member having ended, answered at once; a second take
/// refused, as Shards refuses one; a member the spec doesn't name refused.
#[test]
fn a_member_its_caller_drives_gives_its_streams_once_and_its_end() {
    use std::io::{Read, Write};
    let images = ModelImages::new();
    let (runtime, _owner) = model_runtime(&images, waits());
    let claude = Member {
        kind: MemberKind::Harness,
        name: "claude".to_owned(),
    };
    let mut boxed = spec("image-driven", None);
    if let ClassSpec::ShardsImage(image) = &mut boxed.class {
        image.members.push(ImageMember {
            member: claude.clone(),
            env: vec![("TZ".to_owned(), "UTC".to_owned())],
            workdir: Some("/work/project".to_owned()),
            args: Vec::new(),
            mcp: false,
        });
    }
    let id = runtime.create(boxed).unwrap().id;
    assert!(matches!(
        runtime.member(&id, &claude),
        Err(Refusal::NotRunning(_, "member"))
    ));
    runtime.start(&id).unwrap();
    let asked = images.asked.lock().unwrap().last().cloned().unwrap();
    let [m] = asked.members.as_slice() else {
        panic!("one member asked: {:?}", asked.members);
    };
    assert_eq!(
        (&m.member, m.stdin, m.terminal, m.workdir.as_deref()),
        (
            &MemberName::harness("claude"),
            true,
            false,
            Some("/work/project")
        )
    );
    assert_eq!(m.env, vec![("TZ".to_owned(), Secret::from("UTC"))]);
    let mut proc = runtime.member(&id, &claude).unwrap();
    assert!(
        !proc.on_this_host(),
        "a member is its guest's, no process of this host"
    );
    let given = proc.streams().unwrap();
    assert!(
        proc.streams().unwrap().stdin.is_none(),
        "its streams are given once"
    );
    let mut to = hex_api::OwnedStream::from(given.stdin.unwrap());
    let mut from = hex_api::OwnedStream::from(given.stdout.unwrap());
    let mut errors = hex_api::OwnedStream::from(given.stderr.unwrap());
    let (name, mut member_in, mut member_out, mut member_err) =
        images.run.inside.lock().unwrap().pop().unwrap();
    assert_eq!(name, "claude");
    to.write_all(b"first\n").unwrap();
    to.write_all(b"second\n").unwrap();
    drop(to);
    let mut heard = String::new();
    member_in.read_to_string(&mut heard).unwrap();
    assert_eq!(heard, "first\nsecond\n", "in order, then its end of input");
    member_out.write_all(b"an answer").unwrap();
    member_err.write_all(b"a note").unwrap();
    drop((member_out, member_err));
    let mut said = String::new();
    from.read_to_string(&mut said).unwrap();
    assert_eq!(said, "an answer");
    let mut noted = String::new();
    errors.read_to_string(&mut noted).unwrap();
    assert_eq!(noted, "a note");
    assert_eq!(proc.try_wait().unwrap(), None, "no end until the member's");
    images
        .run
        .member_ends
        .lock()
        .unwrap()
        .insert("claude".to_owned(), MemberEnd::Exited(3));
    assert_eq!(proc.wait_for(Duration::ZERO).unwrap(), Some(Exit::Code(3)));
    // Its member has ended: its kill has nothing to end, and rides no lane.
    assert_eq!(proc.kill(), Ok(()));
    assert!(
        runtime.member(&id, &claude).is_err(),
        "its streams were taken"
    );
    let helper = Member {
        kind: MemberKind::Agent,
        name: "helper".to_owned(),
    };
    assert!(matches!(
        runtime.member(&id, &helper),
        Err(Refusal::Invalid { what: "member", .. })
    ));
}

/// Joint plan step 17, AC-T7.4.1: do take a member its caller drives whose run was stopped; expect its end the
/// run's, KILL (signal 9), as the box's own is.
#[test]
fn a_member_that_ends_with_its_run_ends_as_the_run_did() {
    let images = ModelImages::new();
    let (runtime, _owner) = model_runtime(&images, waits());
    let codex = Member {
        kind: MemberKind::Harness,
        name: "codex".to_owned(),
    };
    let mut boxed = spec("image-ended", None);
    if let ClassSpec::ShardsImage(image) = &mut boxed.class {
        image.members.push(ImageMember::new(codex.clone()));
    }
    let id = runtime.create(boxed).unwrap().id;
    runtime.start(&id).unwrap();
    let mut proc = runtime.member(&id, &codex).unwrap();
    images
        .run
        .member_ends
        .lock()
        .unwrap()
        .insert("codex".to_owned(), MemberEnd::WithRun);
    *images.run.end.lock().unwrap() = Some(RunEnd::Stopped);
    assert_eq!(
        proc.wait_for(Duration::ZERO).unwrap(),
        Some(Exit::Signal(Signal::KILL.number()))
    );
}

/// kiln P2.3 (AC-T7.4.1): a member its caller drives whose run failed (in its run host's or its VM's words, Shards'
/// D249) ends in those words, never an unknown exit that drops them. Do: take a driven member of a running VM; have its
/// run fail, the member ending with it. Expect its wait refused `Failed`, naming its box and the run's words.
#[test]
fn p23_a_member_whose_run_failed_ends_in_its_runs_words() {
    let images = ModelImages::new();
    let (runtime, _owner) = model_runtime(&images, waits());
    let codex = Member {
        kind: MemberKind::Harness,
        name: "codex".to_owned(),
    };
    let mut boxed = spec("image-member-failed", None);
    if let ClassSpec::ShardsImage(image) = &mut boxed.class {
        image.members.push(ImageMember::new(codex.clone()));
    }
    let id = runtime.create(boxed).unwrap().id;
    runtime.start(&id).unwrap();
    let mut proc = runtime.member(&id, &codex).unwrap();
    images
        .run
        .member_ends
        .lock()
        .unwrap()
        .insert("codex".to_owned(), MemberEnd::WithRun);
    let words = "Setup (entering its working directory): errno 2";
    *images.run.end.lock().unwrap() = Some(RunEnd::Failed(words.to_owned()));
    let ended = proc.wait_for(Duration::ZERO);
    assert!(
        matches!(&ended, Err(Refusal::Failed { id: failed, why }) if *failed == id && why == words),
        "{ended:?}"
    );
}

/// Shards' #72 (image channel v27, `MemberEnd::Signaled`): a member's end is typed, a signal that killed it its own end
/// beside its exit, never folded into the status `docker run` reports (128 plus its number), which an exit can be too.
/// Do: take four members of a running VM; have one killed by TERM, one by KILL, one exit 143, and one ended at once by
/// a restart (Shards' D270, image channel v38: `MemberEnd::Killed`, every process of it killed). Expect
/// `Exit::Signal(15)`, `Exit::Signal(9)`, `Exit::Code(143)` and `Exit::Signal(9)`, each its own.
#[test]
fn r12_a_members_end_tells_a_signal_from_its_exit() {
    let images = ModelImages::new();
    let (runtime, _owner) = model_runtime(&images, waits());
    let harness = |name: &str| Member {
        kind: MemberKind::Harness,
        name: name.to_owned(),
    };
    let ends = [
        ("termed", MemberEnd::Signaled(15), Exit::Signal(15)),
        ("killed", MemberEnd::Signaled(9), Exit::Signal(9)),
        ("exited", MemberEnd::Exited(143), Exit::Code(143)),
        ("restarted", MemberEnd::Killed, Exit::Signal(9)),
    ];
    let mut boxed = spec("image-member-ends", None);
    if let ClassSpec::ShardsImage(image) = &mut boxed.class {
        for (name, _, _) in &ends {
            image.members.push(ImageMember::new(harness(name)));
        }
    }
    let id = runtime.create(boxed).unwrap().id;
    runtime.start(&id).unwrap();
    for (name, end, exit) in ends {
        let mut proc = runtime.member(&id, &harness(name)).unwrap();
        images
            .run
            .member_ends
            .lock()
            .unwrap()
            .insert(name.to_owned(), end);
        assert_eq!(proc.wait_for(Duration::ZERO).unwrap(), Some(exit), "{name}");
    }
}

/// Shards' image channel 51 (AC-T7.4.1; CLAUDE.md §3): each refusal Shards added past 9975d1f55 is typed by what it
/// refused. Do: map a run host's budget with no room for the run's lanes (D252), a member's working directory its guest
/// couldn't enter (D277), a member's restart and a component's swap (D270, which this class asks of no run), and a
/// shared network's grant refused (D285). Expect the budget's as the host's (`Os`), the working directory's and the
/// network's as the spec's (`Invalid`), each in Shards' words, and the restart's and the swap's as the class's own
/// fault (`Broken`).
#[test]
fn v51_shards_new_refusals_are_typed_by_what_they_refused() {
    let id = BoxId::new("v51-refused").unwrap();
    let full = refused(
        &id,
        "booting a Shards image VM",
        ImageRefusal::Capacity("no room for its link".to_owned()),
    );
    assert!(
        matches!(&full, Refusal::Os { why, .. } if why.contains("no room for its link")),
        "{full:?}"
    );
    let unentered = refused(
        &id,
        "waiting for a member's end",
        ImageRefusal::Workdir("Harness codex: /w: errno 2".to_owned()),
    );
    assert!(
        matches!(&unentered, Refusal::Invalid { what: "shards image", why } if why.contains("errno 2")),
        "{unentered:?}"
    );
    for asked_of_none in [
        ImageRefusal::Restart(
            shards_api::image::RestartRefusal::Capacity,
            "no room".to_owned(),
        ),
        ImageRefusal::Swap(
            shards_api::image::SwapRefusal::Workdir,
            "errno 2".to_owned(),
        ),
    ] {
        let mapped = refused(&id, "a call", asked_of_none);
        assert!(matches!(&mapped, Refusal::Broken(_)), "{mapped:?}");
    }
    let taken = refused(
        &id,
        "a call",
        ImageRefusal::Net(shards_api::image::NetRefusal::NameTaken, "codex".to_owned()),
    );
    assert!(
        matches!(&taken, Refusal::Invalid { what: "shards image's network", why } if why.contains("NameTaken")),
        "{taken:?}"
    );
}

/// Goal 2's r13 (decision 9; Shards' pending lane handles, 155b5ee2e; AC-T7.4.1): a member lane's ask goes once, and
/// its answer is waited for within the lane wait's W*, past it on, by the run's connection's life. Do wait through
/// `followed` on a stand-in handle that answers nothing within the patience it is given unless that is `Duration::MAX`:
/// with a W* learned, then cold, then for the endpoint's refusal and for the connection's end. Expect: a learned wait
/// given its W* and then, its run-out told, `Duration::MAX` on the same ask, the answer taken and learned; a cold wait
/// given `Duration::MAX` at once; a refusal learned as a refusal's sample; the connection's end learned as none.
#[test]
fn r13_a_lanes_answer_past_its_w_star_is_followed_on_its_one_ask_and_learned() {
    let party = [Piece::Kind("the test's lane")];
    let w_star = Duration::from_nanos(1);
    let learned = Patience::seeded("hex.test.lane", w_star);
    let given = Mutex::new(Vec::new());
    assert_eq!(
        followed(&learned, &party, asked(&given, Ok(7))).unwrap(),
        Ok(7)
    );
    assert_eq!(*given.lock().unwrap(), [w_star, Duration::MAX]);
    assert!(learned.slowest() >= Some(w_star));
    assert_eq!(
        learned.deadline(),
        learned.slowest(),
        "answered, no longer following"
    );

    let cold = Patience::measuring("hex.test.lane");
    let given = Mutex::new(Vec::new());
    assert_eq!(
        followed(&cold, &party, asked(&given, Ok(7))).unwrap(),
        Ok(7)
    );
    assert_eq!(*given.lock().unwrap(), [Duration::MAX]);
    assert!(cold.slowest().is_some(), "its answer learned");

    let refused = Patience::measuring("hex.test.lane");
    let given = Mutex::new(Vec::new());
    let scope = Fault::Refused(shards_api::control::Reason::Scope);
    assert_eq!(
        followed(&refused, &party, asked(&given, Err(scope))).unwrap(),
        Err(scope)
    );
    assert!(refused.slowest().is_some(), "a refusal is an answer");

    let lost = Patience::measuring("hex.test.lane");
    let given = Mutex::new(Vec::new());
    let gone = Fault::Lost(shards_api::control::Ended::Local);
    assert_eq!(
        followed(&lost, &party, asked(&given, Err(gone))).unwrap(),
        Err(gone)
    );
    assert_eq!(
        lost.slowest(),
        None,
        "the connection's end answered nothing"
    );
}

/// A lane's handle as `followed` waits on it: each patience it is given noted in `given`, `answer` given only to a wait
/// with no bound, as an endpoint that answers later than any W* does.
fn asked(
    given: &Mutex<Vec<Duration>>,
    answer: Result<u32, Fault>,
) -> impl FnMut(Duration) -> Result<Option<u32>, Fault> + '_ {
    move |patience| {
        given.lock().unwrap().push(patience);
        if patience == Duration::MAX {
            answer.map(Some)
        } else {
            Ok(None)
        }
    }
}

/// The review's B-4 (AC-T7.4.1): an image VM's delete answers only once its run is released, its run host exited and
/// every process that host started gone, so kiln frees no room a run still holds. Do boot an image box through Shards'
/// own image boot (`ImageShards`, its run's session real) whose run host is a stand-in: it starts the run, starts a VM
/// of its own that holds its diagnostics as the VM, network and share processes hold its stderr, says the run's end once
/// asked to stop, and exits as its stdin closes, its VM left running until the test lets it go. Delete the box on a
/// thread, and let the VM go only once the run host has exited. Expect the delete to answer `Ok` only after the VM has
/// gone. Red where Shards' stop returned as the run's end was told and its host reaped what it started after (to
/// ce80cbb37: `image_real`'s release case on KVM at 45fc1605d found three processes left as the delete returned).
#[cfg(unix)]
#[test]
fn b4_an_image_vms_delete_answers_once_its_run_host_and_every_process_it_started_are_gone() {
    use shards_api::image::wire;
    use std::io::BufRead as _;
    let dir = tempfile::tempdir().unwrap();
    let (host_seen, vm_seen, vm_gate) = (
        dir.path().join("h"),
        dir.path().join("v"),
        dir.path().join("g"),
    );
    for fifo in [&host_seen, &vm_seen, &vm_gate] {
        let made = std::process::Command::new("mkfifo").arg(fifo).status();
        assert!(made.unwrap().success(), "mkfifo {fifo:?}");
    }
    let octal = |kind, payload: &[u8]| -> String {
        wire::frame(kind, payload)
            .unwrap()
            .iter()
            .map(|b| format!("\\{b:03o}"))
            .collect()
    };
    // Its start names the agent-control endpoint the class's runs ask for; the test never dials it.
    let started = wire::Started {
        members: Vec::new(),
        published: Vec::new(),
        control: Some(shards_api::image::ControlAt {
            address: std::net::SocketAddr::from(([127, 0, 0, 1], 9)),
            vm: [7; 32],
            waker: std::net::SocketAddr::from(([127, 0, 0, 1], 10)),
            wake: std::net::SocketAddr::from(([127, 0, 0, 1], 11)),
        }),
        from: StartedFrom::Booted,
        // Its grants past its microVM once made (Shards' D273, image channel v36): it makes no network.
        egress: wire::Narrowing {
            egress: wire::Keep::Only(Vec::new()),
            mcp: wire::Keep::Only(Vec::new()),
            destinations: wire::Keep::Only(Vec::new()),
            dns: false,
            members: None,
        },
    }
    .encode()
    .unwrap();
    let ended = RunEnded::unused(RunEnd::Stopped, Unused::Killed)
        .encode()
        .unwrap();
    // The run host: its start said; its VM started, holding its stderr until the gate closes; each frame its orchestrator
    // sends read whole until STOP, answered with the run's end; then its stdin read to its end, as it closes.
    let host = dir.path().join("host");
    std::fs::write(
        &host,
        format!(
            "#!/bin/sh\n\
             printf '{started}'\n\
             exec 3>'{host_seen}'\n\
             (read _ <'{vm_gate}') >'{vm_seen}' 3>&- &\n\
             echo \"$!\" >&3\n\
             while :; do\n\
             set -- $(dd bs=1 count=8 2>/dev/null | od -An -tu1)\n\
             [ \"$#\" -eq 8 ] || exit 3\n\
             len=$(( ($5 << 24) + ($6 << 16) + ($7 << 8) + $8 ))\n\
             [ \"$len\" -eq 0 ] || dd bs=1 count=\"$len\" of=/dev/null 2>/dev/null\n\
             [ \"$1\" -eq {stop} ] && break\n\
             done\n\
             printf '{ended}'\n\
             exec cat >/dev/null\n",
            started = octal(wire::to_client::STARTED, &started),
            ended = octal(wire::to_client::ENDED, &ended),
            stop = wire::to_host::STOP,
            host_seen = host_seen.display(),
            vm_seen = vm_seen.display(),
            vm_gate = vm_gate.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&host, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let owner = Arc::new(Owner::new());
    let runtime = GuestRuntime::new(
        Arc::new(shards_api::sim::SimGuests::new(|_| {
            shards_api::End::PowerOff
        })),
        crate::guest::GuestConfig {
            binary: host.clone(),
            ..super::super::tests::config()
        },
    )
    .with_images(
        Ok(Arc::new(shards_api::image::ImageShards::new()) as Arc<dyn Images>),
        waits(),
        owner.diagnostics(),
    );
    let books: Arc<dyn Books> = owner.clone();
    runtime.bind_books(Arc::downgrade(&books)).unwrap();
    // A disk that holds its run host's diagnostics' quoted tail, as a real image VM's does (the review's B-9): the sim's
    // VM allocation admits none. The fixture's own VM, with as much disk as memory, under its cost profile so widened.
    let mut admitted = spec("b4-release", None);
    let fixture = allocation();
    let vm = CapacityResources {
        disk_bytes: fixture.resources.mem_bytes,
        ..fixture.resources
    };
    admitted.allocation = Some(
        hex_api::RuntimeCost {
            ceiling: vm,
            ..fixture.cost
        }
        .allocate(vm)
        .unwrap(),
    );
    let id = runtime.create(admitted).unwrap().id;
    runtime.start(&id).unwrap();
    // Each open waits for its other end: the run host's, its VM's, then the VM's gate, each held from here.
    let mut host_end = std::io::BufReader::new(std::fs::File::open(&host_seen).unwrap());
    let mut vm_pid = String::new();
    host_end.read_line(&mut vm_pid).unwrap();
    let vm_end = std::fs::File::open(&vm_seen).unwrap();
    let gate = std::fs::OpenOptions::new()
        .write(true)
        .open(&vm_gate)
        .unwrap();
    std::thread::scope(|scope| {
        let deleting = scope.spawn(|| {
            let deleted = runtime.delete(&id);
            (deleted, writers_gone(&vm_end))
        });
        // The run host's end: its end of `host_seen` closes as it exits, its VM's closed as it started.
        let mut rest = String::new();
        assert_eq!(host_end.read_line(&mut rest).unwrap(), 0, "{rest:?}");
        drop(gate);
        let (deleted, vm_gone) = deleting.join().unwrap();
        assert_eq!(deleted, Ok(()));
        assert!(
            vm_gone,
            "the delete answered while the VM its run host started (pid {}) still ran",
            vm_pid.trim()
        );
    });
}

/// Whether every writer of the FIFO `reader` reads has closed it: a read that would wait says one still holds it.
#[cfg(unix)]
fn writers_gone(reader: &std::fs::File) -> bool {
    use std::io::Read as _;
    let flags = rustix::fs::fcntl_getfl(reader).unwrap();
    rustix::fs::fcntl_setfl(reader, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    match (&mut &*reader).read(&mut [0u8; 1]) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(e) => panic!("reading a FIFO's end: {e}"),
    }
}

/// What a run's microVM ran of its guest, as D241's run host reads it: `vcpus` vCPUs by index, vCPU `n` having run
/// `1_000 * (n + 1)` ns, and none departed.
fn guest_times(vcpus: u32) -> GuestTimes {
    GuestTimes {
        vcpus: (0..vcpus)
            .map(|index| VcpuTime {
                index,
                guest_ns: 1_000 * u64::from(index + 1),
                unread: None,
            })
            .collect(),
        departed_ns: 0,
    }
}

/// A run's microVM part (D241): its VM process's use of the host, and `guest`.
fn microvm(guest: Result<GuestTimes, String>) -> Option<Microvm> {
    Some(Microvm {
        host: Ok(HostUse {
            cpu_ns: 9_000_000,
            resident_bytes: 1 << 20,
            footprint_bytes: 1 << 20,
            charged: shards_api::image::Charged::ToThread,
        }),
        guest,
    })
}

/// Decision 9 (the person's ruling of 2026-11-13; ROOT's leftover of decision 9): a stats ask past its learned W* was
/// given that W* as Shards' own patience, so D231 refused it there though its run was answering, an R03 bound of Hex's
/// on a running VM's read. Do: read a running VM's usage once its stats wait has learned a 1 ns answer. Expect: Shards'
/// stats asked with no bound of Hex's (`Duration::MAX`: until its run answers or ends); the usage answered; and W* risen
/// to the answer that passed it.
#[test]
fn original_a_stats_ask_past_its_learned_w_star_follows_its_run() {
    let images = ModelImages::new();
    let vcpus = allocation().vm_size().unwrap().vcpus;
    *images.run.stats.lock().unwrap() = Stats {
        cpu_ns: Some(1_000),
        memory_bytes: Some(1 << 20),
        pids: Some(1),
        read_bytes: Some(0),
        write_bytes: Some(0),
        microvm: microvm(Ok(guest_times(vcpus))),
        ..Stats::default()
    };
    let learned = Duration::from_nanos(1);
    let armed = ImageWaits {
        stats: Patience::measuring("test.hex_runtime.image_stats_past"),
        ..waits().unwrap()
    };
    armed.stats.answered(learned);
    let (runtime, _owner) = model_runtime(&images, Ok(armed));
    let id = runtime.create(spec("image-stats-past", None)).unwrap().id;
    runtime.start(&id).unwrap();
    assert!(runtime.usage(&id).is_ok());
    assert_eq!(
        *images.run.stats_asked.lock().unwrap(),
        vec![Duration::MAX],
        "Shards' stats was asked with a bound of Hex's"
    );
    let stats = &runtime.images.image().unwrap().waits().unwrap().stats;
    assert!(
        stats.slowest().is_some_and(|slowest| slowest > learned),
        "W* didn't rise to the answer that passed it: {:?}",
        stats.slowest()
    );
}

/// Joint plan steps 12 and 14, HEX-IMG-4's contract (AC-T7.4.1): do read a running VM's usage, its run's stats
/// carrying what each of its vCPUs ran of its guest (D241's `Stats::microvm`); then its usage where the run host
/// read no microVM part, read no guest time, missed a vCPU, told one vCPU more than the VM's size or guest time of
/// vCPUs the VM no longer has; then where its stats lack a line. Expect: its run's CPU in microseconds, memory,
/// processes and I/O, and each vCPU's guest time by index, one for each vCPU of its size; each part not read
/// refused, naming why, never read as none (D241); a missing line refused naming it; before its start NotRunning.
#[test]
fn usage_reads_the_runs_stats_and_each_vcpus_guest_time() {
    let images = ModelImages::new();
    let vcpus = allocation().vm_size().unwrap().vcpus;
    let stats = Stats {
        cpu_ns: Some(7_000_999),
        memory_bytes: Some(64 << 20),
        pids: Some(5),
        read_bytes: Some(11),
        write_bytes: Some(13),
        microvm: microvm(Ok(guest_times(vcpus))),
        ..Stats::default()
    };
    *images.run.stats.lock().unwrap() = stats.clone();
    let (runtime, _owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-usage", None)).unwrap().id;
    assert!(matches!(
        runtime.usage(&id),
        Err(Refusal::NotRunning(_, "usage"))
    ));
    runtime.start(&id).unwrap();
    let usage = runtime.usage(&id).unwrap();
    assert_eq!(
        (
            usage.cpu_usec,
            usage.memory_bytes,
            usage.pids,
            usage.io_read_bytes,
            usage.io_write_bytes
        ),
        (7_000, 64 << 20, 5, 11, 13)
    );
    assert_eq!(
        usage.guest,
        Some(GuestTime::PerVcpuNs(
            (1..=u64::from(vcpus)).map(|n| 1_000 * n).collect()
        ))
    );
    let mut missed = guest_times(vcpus);
    missed.vcpus[0].unread = Some(shards_abi::guest_time::Unread::ThreadStat(Some(9)));
    let mut departed = guest_times(vcpus);
    departed.departed_ns = 5;
    for (microvm, says) in [
        (None, "microVM"),
        (
            microvm(Err("GUEST_TIME went unanswered".to_owned())),
            "GUEST_TIME went unanswered",
        ),
        (microvm(Ok(missed)), "vCPU 0: its thread's stat: errno 9"),
        (microvm(Ok(guest_times(vcpus + 1))), "vCPUs"),
        (microvm(Ok(departed)), "no longer has"),
    ] {
        *images.run.stats.lock().unwrap() = Stats {
            microvm,
            ..stats.clone()
        };
        assert!(
            matches!(runtime.usage(&id), Err(Refusal::Os { ref why, .. }) if why.contains(says)),
            "{says}: {:?}",
            runtime.usage(&id)
        );
    }
    *images.run.stats.lock().unwrap() = stats;
    images.run.stats.lock().unwrap().pids = None;
    assert!(matches!(
        runtime.usage(&id),
        Err(Refusal::Os { why, .. }) if why.contains("pids")
    ));
}

/// Joint plan step 12's item B (goal 54's change 3; ROOT and g121, 2026-11-13): kiln learns what a VM costs its host
/// over its window, and charges a deleted VM its final use, read with its end, never a use made up. Do read a running
/// image VM's host use twice, then delete it answering its final use; delete it again; then delete a VM whose run ended
/// on its own before its delete. Expect each read's window to run from its run's start and grow; the delete to answer the
/// VM's final use, its window the whole run and no less than the last read's, its counters its run host's; the second
/// delete refused NoSuchBox; and the delete of the ended run refused, typed (`NotRunning`), nothing read at its end and
/// none answered, which would read as a run that never started (the review's B-5), the box kept until a plain delete.
#[test]
fn w12_a_vms_host_use_spans_its_window_and_its_delete_answers_its_final_use() {
    let vcpus = allocation().vm_size().unwrap().vcpus;
    let images = ModelImages::new();
    *images.run.stats.lock().unwrap() = Stats {
        microvm: microvm(Ok(guest_times(vcpus))),
        ..Stats::default()
    };
    let (runtime, _owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-final-use", None)).unwrap().id;
    runtime.start(&id).unwrap();
    let first = runtime.host_use(&id).unwrap();
    let second = runtime.host_use(&id).unwrap();
    assert!(first.window > Duration::ZERO, "{first:?}");
    assert!(second.window >= first.window, "{first:?} then {second:?}");
    let last = runtime.delete_with_host_use(&id).unwrap().unwrap();
    assert!(last.window >= second.window, "{second:?} then {last:?}");
    assert_eq!(
        (last.cpu_ns, last.footprint_bytes, last.guest_ns.len()),
        (9_000_000, 1 << 20, usize::try_from(vcpus).unwrap())
    );
    // A delete retried once the VM is gone, as kiln's runtime of the class alone answers it (hex-node's ImageRuntime):
    // NoSuchBox, never a use made up. A node's guest runtime takes an id it has no record of as its guest's own.
    assert!(matches!(
        hex_api::Runtime::delete_with_host_use(&runtime.images, &id),
        Err(Refusal::NoSuchBox(_))
    ));
    let images = ModelImages::new();
    let (runtime, _owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-ended-use", None)).unwrap().id;
    runtime.start(&id).unwrap();
    *images.run.end.lock().unwrap() = Some(RunEnd::Exited(0));
    shown(&runtime, &id, BoxState::Empty, &REAPED_SHOWN);
    assert!(matches!(
        runtime.delete_with_host_use(&id),
        Err(Refusal::NotRunning(named, _)) if named == id
    ));
    runtime.delete(&id).unwrap();
}

/// HVF's image_real at cf535152 (ROOT's r183: HEX-IMG-1, 6 and 7, each a bare Lifecycle Uncertain where NoSuchBox was
/// due): a node whose guest keeps its boxes' records (macOS's and Windows', their guest a managed shared one) forwards a
/// call on an id its host doesn't hold to its guest (hex-node's `forward`), which found the box's guest by its record and,
/// with none, refused Lifecycle Uncertain, though no record is no box. Do ask the guest runtime for the guest of an id it
/// holds no record of, as that forward does. Expect none, which the forward answers NoSuchBox.
#[test]
fn hvf_an_id_its_guest_holds_no_record_of_has_no_guest() {
    let images = ModelImages::new();
    let (runtime, _owner) = model_runtime(&images, waits());
    let absent = BoxId::new("hex-conformance-image-life").unwrap();
    assert!(
        matches!(runtime.evented_for(&absent), Ok(None)),
        "{:?}",
        runtime.evented_for(&absent).map(|client| client.is_some())
    );
}

/// Joint plan step 12 (D253, image channel v19 and v21): a box says how its VM's microVM came up, as its run host said it
/// as the run started, in its usage and in its record. Do boot an image VM whose run host says it restored its image's
/// template, its start taking the boot's answer; then one whose run host says it was a warm host's, its start Pending
/// (its boot held past its W*) and its answer taken by a later call. Expect before each boot no start in its record,
/// then its usage saying it and its record keeping it, written as its boot's answer was taken, whichever call took it.
#[test]
fn w12_a_vms_usage_and_record_say_how_its_microvm_started() {
    let vcpus = allocation().vm_size().unwrap().vcpus;
    let stats = Stats {
        cpu_ns: Some(1_000),
        memory_bytes: Some(64 << 20),
        pids: Some(1),
        read_bytes: Some(0),
        write_bytes: Some(0),
        microvm: microvm(Ok(guest_times(vcpus))),
        ..Stats::default()
    };
    // Its start takes its boot's answer.
    let images = ModelImages::new();
    *images.started_from.lock().unwrap() = StartedFrom::Restored;
    *images.run.stats.lock().unwrap() = stats.clone();
    let (runtime, owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-restored", None)).unwrap().id;
    assert_eq!(record(&owner, &id).unwrap().started_from, None);
    runtime.start(&id).unwrap();
    assert_eq!(
        record(&owner, &id).unwrap().started_from,
        Some(hex_api::StartedFrom::Restored)
    );
    assert_eq!(
        runtime.usage(&id).unwrap().started_from,
        Some(hex_api::StartedFrom::Restored)
    );
    // A later call takes it: the start Pending, its boot held before its run host's start past a 1 ns W*.
    let images = ModelImages::new();
    *images.started_from.lock().unwrap() = StartedFrom::Warm;
    *images.run.stats.lock().unwrap() = stats;
    images.gate(Gate::Host);
    let held = ImageWaits {
        boot: Patience::seeded(
            "test.hex_runtime.image_boot_held_warm",
            Duration::from_nanos(1),
        ),
        ..waits().unwrap()
    };
    let (runtime, owner) = model_runtime(&images, Ok(held));
    let id = runtime.create(spec("image-warm", None)).unwrap().id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Lifecycle {
            cause: LifecycleCause::Pending,
            ..
        })
    ));
    assert_eq!(record(&owner, &id).unwrap().started_from, None);
    images.gate(Gate::Open);
    shown(&runtime, &id, BoxState::Running, &LATE_BOOT_SHOWN);
    assert_eq!(
        record(&owner, &id).unwrap().started_from,
        Some(hex_api::StartedFrom::Warm)
    );
    assert_eq!(
        runtime.usage(&id).unwrap().started_from,
        Some(hex_api::StartedFrom::Warm)
    );
}

/// Joint plan step 14 (C7, C4, C1; AC-T7.4.1): a running image VM is told its room, held to its rate and read for its
/// host cost through its runtime, the one call each of the node's charge learner and a caller's own runtime (goal 12's
/// item B) make. Do tell a VM rooms of 32 MiB, -8 MiB (its budget short of what it holds) and none, hold it at 250
/// milli-CPU then none, and read its host use, its run's microVM part carrying its VM process's use and each vCPU's guest
/// time; then each before its start, and its host use where its run host read no host part. Expect each room and rate
/// asked of its run as told and its answer returned; the host use its VM process's CPU, footprint and resident memory
/// and its guest time by vCPU; before its start each refused NotRunning, naming the call; the unread host part refused,
/// naming why, never read as none.
#[test]
fn w14_a_running_vm_is_told_its_room_held_to_its_rate_and_read_for_its_host_cost() {
    let images = ModelImages::new();
    let vcpus = allocation().vm_size().unwrap().vcpus;
    *images.run.stats.lock().unwrap() = Stats {
        microvm: microvm(Ok(guest_times(vcpus))),
        ..Stats::default()
    };
    let (runtime, _owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-charge", None)).unwrap().id;
    let rate = std::num::NonZeroU64::new(250);
    assert!(matches!(
        runtime.room(&id, Some(1)),
        Err(Refusal::NotRunning(_, "room"))
    ));
    assert!(matches!(
        runtime.rate(&id, rate),
        Err(Refusal::NotRunning(_, "rate"))
    ));
    assert!(matches!(
        runtime.host_use(&id),
        Err(Refusal::NotRunning(_, "host_use"))
    ));
    runtime.start(&id).unwrap();
    assert_eq!(runtime.room(&id, Some(32 << 20)).unwrap(), Some(32 << 20));
    assert_eq!(runtime.room(&id, Some(-(8 << 20))).unwrap(), Some(0));
    assert_eq!(runtime.room(&id, None).unwrap(), None);
    assert_eq!(
        *images.run.rooms.lock().unwrap(),
        [Some(32 << 20), Some(-(8 << 20)), None]
    );
    assert_eq!(runtime.rate(&id, rate).unwrap(), rate);
    assert_eq!(runtime.rate(&id, None).unwrap(), None);
    assert_eq!(*images.run.rates.lock().unwrap(), [rate, None]);
    let read = runtime.host_use(&id).unwrap();
    assert_eq!(
        read,
        hex_api::HostUse {
            cpu_ns: 9_000_000,
            footprint_bytes: 1 << 20,
            resident_bytes: 1 << 20,
            guest_ns: (1..=u64::from(vcpus)).map(|n| 1_000 * n).collect(),
            window: read.window,
            charged: hex_api::Charged::ToThread,
        }
    );
    *images.run.stats.lock().unwrap() = Stats {
        microvm: Some(Microvm {
            host: Err("its VM process's use went unread".to_owned()),
            guest: Ok(guest_times(vcpus)),
        }),
        ..Stats::default()
    };
    assert!(matches!(
        runtime.host_use(&id),
        Err(Refusal::Os { why, .. }) if why.contains("went unread")
    ));
}

/// Joint plan step 14 (C1; ROOT, 2026-11-13): a running image VM's rate period is its own run's, as its VM said it with
/// its last rate's answer (D247's `ImageGuest::rate_period`; Shards' image channel v31), the window the node's charge
/// learner caps it over, never a number of Hex's. Do start two image VMs whose VMs say 70 ms and 130 ms, each on a
/// runtime of its own (so neither boot waits at a bound the other's answer taught, decision 9), and ask each its period
/// before and after a rate is held; then ask one made and not started, and a box the runtime doesn't hold. Expect none
/// before a rate's answer, then each its own run's period; the one not started refused NotRunning, naming the call; the
/// one not held NoSuchBox.
#[test]
fn w14_a_running_vms_rate_period_is_its_own_runs() {
    let started = |name: &str, ms: u64| {
        let images = ModelImages::new();
        *images.period.lock().unwrap() = Duration::from_millis(ms);
        let (runtime, owner) = model_runtime(&images, waits());
        let id = runtime.create(spec(name, None)).unwrap().id;
        runtime.start(&id).unwrap();
        (runtime, owner, id)
    };
    let (first, _first_owner, a) = started("image-period-a", 70);
    let (second, _second_owner, b) = started("image-period-b", 130);
    assert_eq!(first.rate_period(&a).unwrap(), None, "no rate answered yet");
    let rate = std::num::NonZeroU64::new(1_000);
    assert_eq!(first.rate(&a, rate).unwrap(), rate);
    assert_eq!(second.rate(&b, rate).unwrap(), rate);
    assert_eq!(
        first.rate_period(&a).unwrap(),
        Some(Duration::from_millis(70))
    );
    assert_eq!(
        second.rate_period(&b).unwrap(),
        Some(Duration::from_millis(130))
    );
    let made = first.create(spec("image-period-c", None)).unwrap().id;
    assert!(matches!(
        first.rate_period(&made),
        Err(Refusal::NotRunning(_, "rate_period"))
    ));
    assert!(matches!(
        first.rate_period(&BoxId::new("image-period-none").unwrap()),
        Err(Refusal::NoSuchBox(_))
    ));
}

/// Joint plan step 12: do start a VM whose image Shards refuses. Expect: the refusal typed (Invalid,
/// naming the image), the box ended with an unknown exit and Shards' words for its refusal, and shown Empty,
/// its record keeping them.
#[test]
fn a_refused_boot_ends_the_box_and_says_why() {
    let images = ModelImages::new();
    *images.refuse.lock().unwrap() = Some(ImageRefusal::Image("no such image".to_owned()));
    let (runtime, owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-refused", None)).unwrap().id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Invalid { what: "shards image", why }) if why.contains("no such image")
    ));
    let kept = record(&owner, &id).unwrap();
    assert_eq!((kept.phase, kept.exit), (Phase::Ready, Some(Exit::Unknown)));
    let words = Some("the image: no such image");
    assert_eq!(kept.ended.as_deref(), words);
    let info = runtime.inspect(&id).unwrap();
    assert_eq!(
        (info.state, info.exit, info.ended.as_deref()),
        (BoxState::Empty, Some(Exit::Unknown), words)
    );
    runtime.delete(&id).unwrap();
}

/// Decision 9, joint plan step 12: do start a VM whose boot passes its learned patience before its run host
/// has started (Shards' own patience counting from that start, after the caller's). Expect: start refused
/// Lifecycle Pending, the box Created while its boot runs; then, the boot answering, the box Running with no second
/// boot. (A delete meanwhile follows the boot: kiln's choice (b), below.)
#[test]
fn a_boot_past_its_patience_is_pending_and_its_answer_lands_after() {
    let images = ModelImages::new();
    images.gate(Gate::Host);
    // The model holds the boot before its run host's start, which no bound outlasts, so a 1 ns record runs
    // the start's wait out at once. The stats and stop waits measure, as waits() says.
    let held = ImageWaits {
        boot: Patience::seeded("test.hex_runtime.image_boot_held", Duration::from_nanos(1)),
        ..waits().unwrap()
    };
    let (runtime, _owner) = model_runtime(&images, Ok(held));
    let id = runtime.create(spec("image-late", None)).unwrap().id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Lifecycle {
            cause: LifecycleCause::Pending,
            ..
        })
    ));
    assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Created);
    images.gate(Gate::Open);
    shown(&runtime, &id, BoxState::Running, &LATE_BOOT_SHOWN);
    assert_eq!(images.asked.lock().unwrap().len(), 1);
}

/// Each test's delete under way, its intent written or its answer given: these tests' facts alone.
static DELETE_FOLLOWS_UNSTARTED: Patience =
    Patience::measuring("test.hex_runtime.image_delete_follows_unstarted");
static DELETE_FOLLOWS_STARTING: Patience =
    Patience::measuring("test.hex_runtime.image_delete_follows_starting");
static DELETE_FOLLOWS_STOP: Patience =
    Patience::measuring("test.hex_runtime.image_delete_follows_stop");
static DELETE_FOLLOWS_ABANDONED: Patience =
    Patience::measuring("test.hex_runtime.image_delete_follows_abandoned");

/// Kiln's choice (b) (ROOT, 2026-11-13; joint plan step 12, decision 9): a delete of an image VM still booting answers
/// once the box is gone, never Pending, so its caller needs no retry. D231's boot gives no stop for a boot under way
/// (`Images::boot` blocks until its run starts or its host ends), so the delete follows the boot to its answer. Do:
/// start a VM whose boot is held before its run host's start past a 1 ns W*, its start Pending; delete it with its
/// host use as the boot is held; then let the boot end refused, its run never started. Expect: the delete's intent
/// written as it follows the boot, never Pending; its answer no use (no run, nothing to charge); the box gone with its
/// run host's diagnostics; one boot asked; and a delete retried then NoSuchBox.
#[test]
fn w12_a_delete_over_a_boot_follows_it_and_answers_no_use_where_its_run_never_started() {
    let images = ModelImages::new();
    images.gate(Gate::Host);
    let held = ImageWaits {
        boot: Patience::seeded(
            "test.hex_runtime.image_boot_deleted_unstarted",
            Duration::from_nanos(1),
        ),
        ..waits().unwrap()
    };
    let (runtime, owner) = model_runtime(&images, Ok(held));
    let id = runtime
        .create(spec("image-deleted-unstarted", None))
        .unwrap()
        .id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Lifecycle {
            cause: LifecycleCause::Pending,
            ..
        })
    ));
    let diagnostics = record(&owner, &id).unwrap().diagnostics.unwrap();
    let answered = std::thread::scope(|scope| {
        let deleting = scope.spawn(|| runtime.delete_with_host_use(&id));
        follow::until(
            &DELETE_FOLLOWS_UNSTARTED,
            "the delete following the boot",
            || spawned(&deleting),
            || {
                deleting.is_finished()
                    || record(&owner, &id).is_some_and(|r| r.phase == Phase::Deleting)
            },
        )
        .unwrap();
        *images.refuse.lock().unwrap() = Some(ImageRefusal::Boot(
            "its run host ended before its run started".to_owned(),
        ));
        images.gate(Gate::Open);
        deleting.join().unwrap()
    });
    assert_eq!(answered, Ok(None));
    assert!(record(&owner, &id).is_none());
    assert!(!diagnostics.exists(), "{diagnostics:?} outlived its box");
    assert_eq!(images.asked.lock().unwrap().len(), 1);
    assert!(matches!(
        Runtime::delete_with_host_use(&runtime.images, &id),
        Err(Refusal::NoSuchBox(_))
    ));
}

/// The delete over a boot held forever under way: this test's fact alone.
static DELETE_STOPS_BOOT: Patience =
    Patience::measuring("test.hex_runtime.image_delete_stops_boot");

/// Kiln's choice (b), the real thing (ROOT, 2026-11-13; Shards' D231 `BootStop` at ed1136305): a delete of a box still
/// booting stops its boot, its run host killed mid-boot, and follows the boot to its end, `Ended(Stopped)`, never Pending.
/// Do: start a VM whose boot is held before its run host's start forever, past a 1 ns W*, its start Pending; delete it
/// with its host use, its own wait past a 1 ns W* too, so it follows the boot once it has asked its stop; then wake
/// the held boot, and, where no stop came, let it go so the test ends. Expect: the boot ended by its stop, the delete's
/// answer no use (the run never started), the box gone, one boot.
#[test]
fn w12_a_delete_over_a_boot_held_forever_stops_it_and_answers_no_use() {
    let images = ModelImages::new();
    images.gate(Gate::Host);
    let held = ImageWaits {
        boot: Patience::seeded(
            "test.hex_runtime.image_boot_delete_stops",
            Duration::from_nanos(1),
        ),
        delete_boot: Patience::seeded(
            "test.hex_runtime.image_delete_boot_stops",
            Duration::from_nanos(1),
        ),
        ..waits().unwrap()
    };
    let (runtime, owner) = model_runtime(&images, Ok(held));
    let id = runtime
        .create(spec("image-deleted-held-forever", None))
        .unwrap()
        .id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Lifecycle {
            cause: LifecycleCause::Pending,
            ..
        })
    ));
    let answered = std::thread::scope(|scope| {
        let deleting = scope.spawn(|| runtime.delete_with_host_use(&id));
        follow::until(
            &DELETE_STOPS_BOOT,
            "the delete following the boot",
            || spawned(&deleting),
            || deleting.is_finished() || image_waits(&runtime).delete_boot.deadline().is_none(),
        )
        .unwrap();
        images.poke();
        if images.stopped.load(Ordering::SeqCst) == 0 && !deleting.is_finished() {
            // No stop came: let the boot go, so the test ends either way.
            images.gate(Gate::Open);
        }
        deleting.join().unwrap()
    });
    assert_eq!(images.stopped.load(Ordering::SeqCst), 1, "{answered:?}");
    assert_eq!(answered, Ok(None));
    assert!(record(&owner, &id).is_none());
    assert_eq!(images.asked.lock().unwrap().len(), 1);
}

/// Kiln's choice (b), as above: a stop that comes after the boot has answered is no longer the boot's (D231's
/// `BootStop`), so a delete of a box whose run started before it, its answer untaken, stops it as a run. Do: start a VM
/// whose boot is held past a 1 ns W*, its start Pending; let its run start, its answer taken by no call; delete it with
/// its host use. Expect: the boot not stopped, the run stopped instead, the delete's answer its final use of its host,
/// read as it ran, before its stop; the box gone; one boot, one stop.
#[test]
fn w12_a_delete_of_a_box_whose_boot_answered_stops_its_run_and_answers_its_final_use() {
    let vcpus = allocation().vm_size().unwrap().vcpus;
    let images = ModelImages::new();
    *images.run.stats.lock().unwrap() = Stats {
        microvm: microvm(Ok(guest_times(vcpus))),
        ..Stats::default()
    };
    images.gate(Gate::Host);
    let held = ImageWaits {
        boot: Patience::seeded(
            "test.hex_runtime.image_boot_deleted_starting",
            Duration::from_nanos(1),
        ),
        ..waits().unwrap()
    };
    let (runtime, owner) = model_runtime(&images, Ok(held));
    let id = runtime
        .create(spec("image-deleted-starting", None))
        .unwrap()
        .id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Lifecycle {
            cause: LifecycleCause::Pending,
            ..
        })
    ));
    images.gate(Gate::Open);
    follow::until(
        &DELETE_FOLLOWS_STARTING,
        "the boot answered",
        || kept(&owner, &id),
        || images.answered.load(Ordering::SeqCst) == 1,
    )
    .unwrap();
    let last = runtime.delete_with_host_use(&id).unwrap().unwrap();
    assert_eq!(images.stopped.load(Ordering::SeqCst), 0);
    assert_eq!(
        (last.cpu_ns, last.footprint_bytes, last.guest_ns.len()),
        (9_000_000, 1 << 20, usize::try_from(vcpus).unwrap())
    );
    assert!(record(&owner, &id).is_none());
    assert_eq!(images.asked.lock().unwrap().len(), 1);
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 1);
}

/// Kiln's choice (b): a delete never answers Pending, so one whose stop runs past the stop's W* follows the stop to its
/// end. Do: start a VM, its stop's W* one a 1 ns stop taught; delete it with its stop held. Expect: the delete waiting
/// on its stop, never Pending; once the stop is let go, its answer, the box gone, one stop.
#[test]
fn w12_a_delete_follows_its_stop_past_the_stops_w_star() {
    let images = ModelImages::new();
    let slow = ImageWaits {
        stop: Patience::seeded(
            "test.hex_runtime.image_stop_deleted",
            Duration::from_nanos(1),
        ),
        ..waits().unwrap()
    };
    let (runtime, owner) = model_runtime(&images, Ok(slow));
    let id = runtime
        .create(spec("image-deleted-stopping", None))
        .unwrap()
        .id;
    runtime.start(&id).unwrap();
    images.run.hold_stops(true);
    let answered = std::thread::scope(|scope| {
        let deleting = scope.spawn(|| runtime.delete(&id));
        follow::until(
            &DELETE_FOLLOWS_STOP,
            "the delete's stop asked",
            || spawned(&deleting),
            || deleting.is_finished() || images.run.stops.load(Ordering::SeqCst) == 1,
        )
        .unwrap();
        images.run.hold_stops(false);
        deleting.join().unwrap()
    });
    assert_eq!(answered, Ok(()));
    assert!(record(&owner, &id).is_none());
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 1);
}

/// Kiln's choice (b) (ROOT, 2026-11-13: the delete's wait follows the boot until its party's end, and says so there):
/// a delete following a boot is refused only where the boot's thread ends with no answer, so neither its run's start
/// nor its run host's end is known. Do: start a VM whose boot is held past a 1 ns W*, its start Pending; delete it as
/// the boot is held, its own wait past a 1 ns W* too, so it follows the boot once it has asked its stop; wake the held
/// boot as the OS may wake a condition variable's waiter unasked (a signal's EINTR, the step-39 agent's finding of
/// 2026-11-13); then let the boot's thread end with no answer. Expect: the delete never Pending, refused as the node's
/// broken state in its own words, the boot it ends still under way; the box's record kept, Deleting.
#[test]
fn w12_a_delete_whose_boot_ends_with_no_answer_says_the_boot_is_still_under_way() {
    let images = ModelImages::new();
    images.gate(Gate::Host);
    let held = ImageWaits {
        boot: Patience::seeded(
            "test.hex_runtime.image_boot_deleted_abandoned",
            Duration::from_nanos(1),
        ),
        delete_boot: Patience::seeded(
            "test.hex_runtime.image_delete_boot_abandoned",
            Duration::from_nanos(1),
        ),
        ..waits().unwrap()
    };
    let (runtime, owner) = model_runtime(&images, Ok(held));
    let id = runtime
        .create(spec("image-deleted-abandoned", None))
        .unwrap()
        .id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Lifecycle {
            cause: LifecycleCause::Pending,
            ..
        })
    ));
    images.abandon.store(true, Ordering::SeqCst);
    let answered = std::thread::scope(|scope| {
        let deleting = scope.spawn(|| runtime.delete(&id));
        follow::until(
            &DELETE_FOLLOWS_ABANDONED,
            "the delete following the boot",
            || spawned(&deleting),
            || deleting.is_finished() || image_waits(&runtime).delete_boot.deadline().is_none(),
        )
        .unwrap();
        // The wake the test never opened, its stop come: the held boot looks again and stays held, the delete following.
        let looked = images.woken.load(Ordering::SeqCst);
        images.poke();
        follow::until(
            &DELETE_FOLLOWS_ABANDONED,
            "the held boot looking again",
            || spawned(&deleting),
            || images.woken.load(Ordering::SeqCst) > looked,
        )
        .unwrap();
        assert_eq!(images.stopped.load(Ordering::SeqCst), 0);
        assert!(!deleting.is_finished());
        images.gate(Gate::Open);
        deleting.join().unwrap()
    });
    assert!(
        matches!(&answered, Err(Refusal::Broken(why)) if why.contains("the boot it ends is still under way")),
        "{answered:?}"
    );
    assert_eq!(record(&owner, &id).unwrap().phase, Phase::Deleting);
}

/// Decision 9 (original; the person's ruling of 2026-11-13: W* is a bound only while answers come within it), joint
/// plan step 12: a boot past its learned W* was given that W* as Shards' own patience, so D231 killed its run host
/// there and the box ended, though its run was starting. Do: start a VM whose run host starts but whose run doesn't
/// yet, its boot's W* one a 1 ns boot taught it; once Shards' boot is asked, let the run start. Expect: Shards' boot
/// given no bound of Hex's (`Duration::MAX`: until the run starts or its host ends); start refused Lifecycle Pending,
/// the run-out told (the boot's next wait follows its party); the box Running once its run starts, with no second
/// boot; and W* risen to the boot that passed it, its answer taken by the later call.
#[test]
fn original_a_boot_past_its_learned_w_star_goes_on_to_its_run() {
    /// The model's boot asked, and the box shown Running once its run started: this test's facts alone.
    static ASKED: Patience = Patience::measuring("test.hex_runtime.image_boot_past_asked");
    static SHOWN: Patience = Patience::measuring("test.hex_runtime.image_boot_past_shown");
    let images = ModelImages::new();
    images.gate(Gate::Run);
    let learned = Duration::from_nanos(1);
    let armed = ImageWaits {
        boot: Patience::measuring("test.hex_runtime.image_boot_past"),
        ..waits().unwrap()
    };
    armed.boot.answered(learned);
    let (runtime, owner) = model_runtime(&images, Ok(armed));
    let id = runtime.create(spec("image-past", None)).unwrap().id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Lifecycle {
            cause: LifecycleCause::Pending,
            ..
        })
    ));
    follow::until(
        &ASKED,
        "the model's boot asked",
        || kept(&owner, &id),
        || !images.patience.lock().unwrap().is_empty(),
    )
    .unwrap();
    assert_eq!(
        *images.patience.lock().unwrap(),
        vec![Duration::MAX],
        "Shards' boot was given a bound of Hex's"
    );
    let boot = &runtime.images.image().unwrap().waits().unwrap().boot;
    assert_eq!(boot.deadline(), None, "the run-out wasn't told");
    assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Created);
    assert_eq!(
        record(&owner, &id).unwrap().exit,
        None,
        "the box ended at W*"
    );
    images.gate(Gate::Open);
    shown(&runtime, &id, BoxState::Running, &SHOWN);
    assert_eq!(images.asked.lock().unwrap().len(), 1);
    assert!(
        boot.slowest().is_some_and(|slowest| slowest > learned),
        "W* didn't rise to the boot that passed it: {:?}",
        boot.slowest()
    );
}

/// Joint plan step 12, the owner's crash (ROOT, 2026-11-12): do give the runtime a Ready record with no
/// exit and no run of this process, as a node restart leaves. Expect: every call and recovery's
/// adoption refused Awaits(AttachAfterCrash), the record and its claim kept; then, the host's boot
/// session bound as another than the record's, the box ended (Exit::Unknown) and deletable.
#[test]
fn an_earlier_node_processes_run_is_held_until_a_reboot_proves_it_ended() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let spec = spec("image-held", None);
    let id = spec.id.clone();
    let mut left = BoxRecord::new(spec.clone(), Phase::Creating, Vec::new());
    Transaction::put(owner.as_ref(), &left).unwrap();
    left.phase = Phase::Ready;
    left.boot = Some(BootSession::new("1111-before").unwrap());
    Transaction::put(owner.as_ref(), &left).unwrap();
    let held = Err(Refusal::Awaits {
        needs: Upstream::AttachAfterCrash,
    });
    assert_eq!(runtime.inspect(&id).map(|_| ()), held);
    assert_eq!(runtime.start(&id), held);
    assert_eq!(runtime.signal(&id, Signal::KILL), held);
    assert_eq!(runtime.delete(&id), held);
    assert_eq!(runtime.validate_binding(&spec), held);
    assert_eq!(runtime.recover_managed(&spec), held);
    assert!(record(&owner, &id).is_some());
    let listed = runtime.list().unwrap();
    assert!(
        listed
            .iter()
            .any(|info| info.id == id && info.state == BoxState::Running)
    );
    runtime
        .bind_boot_session(Some(BootSession::new("2222-after").unwrap()))
        .unwrap();
    runtime.validate_binding(&spec).unwrap();
    let info = runtime.inspect(&id).unwrap();
    assert_eq!(
        (info.state, info.exit),
        (BoxState::Empty, Some(Exit::Unknown))
    );
    runtime.delete(&id).unwrap();
    assert!(record(&owner, &id).is_none());
    assert!(images.asked.lock().unwrap().is_empty());
}

/// Decision 9, joint plan step 12: do create with no recorded waits, and with a VM size Shards can't
/// boot (255 MiB, odd). Expect: each refused before any record, the first with the waits' own refusal,
/// the second naming the allocation.
#[test]
fn a_create_is_refused_before_any_record_without_its_waits_or_a_bootable_size() {
    let images = ModelImages::new();
    let unrecorded = Refusal::Platform {
        what: "the oci class's guest",
        platform: "test",
        why: "no image_boot record for any host class yet".to_owned(),
    };
    let (runtime, owner) = model_runtime(&images, Err(unrecorded.clone()));
    assert_eq!(
        runtime.create(spec("image-unrecorded", None)).map(|_| ()),
        Err(unrecorded)
    );
    assert!(Transaction::records(owner.as_ref()).unwrap().is_empty());
    let (runtime, owner) = model_runtime(&images, waits());
    let odd_vm = CapacityResources {
        cpu_milli: 1_000,
        mem_bytes: 255 << 20,
        disk_bytes: 0,
    };
    let cost = hex_api::RuntimeCost {
        id: [3; 32],
        floor: CapacityResources::default(),
        ceiling: odd_vm,
        quantum: CapacityResources {
            cpu_milli: 1_000,
            mem_bytes: 1 << 20,
            disk_bytes: 1,
        },
        guest_overhead: CapacityResources::default(),
        overhead: CapacityResources::default(),
    };
    let mut unbootable = spec("image-odd", None);
    unbootable.allocation = Some(cost.allocate(odd_vm).unwrap());
    assert!(matches!(
        runtime.create(unbootable),
        Err(Refusal::Invalid { what: "shards image", why }) if why.contains("allocation")
    ));
    assert!(Transaction::records(owner.as_ref()).unwrap().is_empty());
    assert!(images.asked.lock().unwrap().is_empty());
}

/// Joint plan step 12: do create on a node configured with no image boot. Expect: refused naming that, before
/// any record.
#[test]
fn a_create_is_refused_before_any_record_on_a_node_without_an_image_boot() {
    let owner = Arc::new(Owner::new());
    let runtime = GuestRuntime::new(
        Arc::new(shards_api::sim::SimGuests::new(|_| {
            shards_api::End::PowerOff
        })),
        super::super::tests::config(),
    );
    let books: Arc<dyn Books> = owner.clone();
    runtime.bind_books(Arc::downgrade(&books)).unwrap();
    assert_eq!(
        runtime.create(spec("image-no-host", None)).map(|_| ()),
        Err(Refusal::Platform {
            what: "Shards image VMs",
            platform: std::env::consts::OS,
            why: "this node configures no Shards image boot".to_owned(),
        })
    );
    assert!(Transaction::records(owner.as_ref()).unwrap().is_empty());
}

/// The variable naming the case the guest pair's check runs, in a test process of its own whose environment holds
/// that case's SHARDS_KERNEL and SHARDS_INIT.
const GUEST_PAIR_CASE: &str = "HEX_IMAGE_GUEST_PAIR_CASE";

/// A path this host calls absolute (`Path::is_absolute`), as D231's run host asks of SHARDS_KERNEL and SHARDS_INIT.
fn absolute(name: &str) -> String {
    if cfg!(windows) {
        format!("C:\\shards\\{name}")
    } else {
        format!("/shards/{name}")
    }
}

/// Joint plan step 12, CLAUDE.md §12 (Ada, 2026-11-12: the class works on Windows and macOS too; ROOT, 2026-11-12:
/// the guest's pair checked as the node starts): D231's run host boots the guest its environment names, which it
/// inherits from the node: SHARDS_KERNEL and SHARDS_INIT, each taken only as an absolute path, both or neither
/// (Shards 66113379: api_run.rs `as_run`, one alone refused "SHARDS_KERNEL and SHARDS_INIT go together"). A node
/// whose pair is broken would have every image VM's start refused. Do start a node's image boot (`host_images`,
/// which `hex node` calls once as it starts) and create an image VM, each case in a test process of its own whose
/// environment holds the case's pair. Expect: with neither, or both absolute, created and recorded on every host,
/// Linux, macOS and Windows, its run host started holding only its stdio (vendor/patches/shards 0001), so nothing
/// awaits it; with one alone, or one the run host would drop (a relative path), refused before any record, naming
/// both variables.
#[test]
fn image_vms_are_served_only_with_shards_kernel_and_init_both_or_neither() {
    let Some(case) = std::env::var_os(GUEST_PAIR_CASE) else {
        for (case, kernel, init) in [
            ("neither", None, None),
            (
                "both",
                Some(absolute("vmlinux")),
                Some(absolute("shards-init")),
            ),
            ("kernel-alone", Some(absolute("vmlinux")), None),
            ("init-alone", None, Some(absolute("shards-init"))),
            (
                "kernel-relative",
                Some("vmlinux".to_owned()),
                Some(absolute("shards-init")),
            ),
            (
                "both-relative",
                Some("vmlinux".to_owned()),
                Some("shards-init".to_owned()),
            ),
        ] {
            let mut alone = std::process::Command::new(std::env::current_exe().unwrap());
            alone
                .args([
                    "--exact",
                    "guest::image::tests::image_vms_are_served_only_with_shards_kernel_and_init_both_or_neither",
                    "--test-threads=1",
                ])
                .env(GUEST_PAIR_CASE, case);
            for (name, value) in [("SHARDS_KERNEL", kernel), ("SHARDS_INIT", init)] {
                match value {
                    Some(value) => alone.env(name, value),
                    None => alone.env_remove(name),
                };
            }
            let ran = alone.output().unwrap();
            assert!(
                ran.status.success(),
                "{case}: {}{}",
                String::from_utf8_lossy(&ran.stdout),
                String::from_utf8_lossy(&ran.stderr)
            );
        }
        return;
    };
    let owner = Arc::new(Owner::new());
    let runtime = GuestRuntime::new(
        Arc::new(shards_api::sim::SimGuests::new(|_| {
            shards_api::End::PowerOff
        })),
        super::super::tests::config(),
    )
    // A host Shards' check says runs microVMs: this test's fact is the guest pair alone.
    .with_images(
        checked_images(|| Ok(shards_api::HostCheck::Usable(shards_api::Backend::Kvm))),
        waits(),
        owner.diagnostics(),
    );
    let books: Arc<dyn Books> = owner.clone();
    runtime.bind_books(Arc::downgrade(&books)).unwrap();
    let made = runtime
        .create(spec("image-pair", None))
        .map(|info| info.state);
    if matches!(case.to_str(), Some("neither" | "both")) {
        assert_eq!(made, Ok(BoxState::Created));
        assert!(record(&owner, &BoxId::new("image-pair").unwrap()).is_some());
        return;
    }
    match made {
        Err(Refusal::Platform {
            what: "Shards image VMs",
            why,
            ..
        }) => assert!(
            why.contains("SHARDS_KERNEL") && why.contains("SHARDS_INIT"),
            "{case:?}: {why}"
        ),
        other => panic!("{case:?}: {other:?}"),
    }
    assert!(Transaction::records(owner.as_ref()).unwrap().is_empty());
}

/// Joint plan step 12: do read each SHARDS_KERNEL and SHARDS_INIT pair a run host might inherit, as D231's run host
/// reads it. Expect: an empty value is none; both absolute, or neither, taken; one alone, or a value the run host
/// would drop (relative, or on Unix not UTF-8), refused naming both variables.
#[test]
fn the_guest_pair_is_read_as_the_run_host_reads_it() {
    let (kernel, init) = (absolute("vmlinux"), absolute("shards-init"));
    let (kernel, init) = (OsStr::new(&kernel), OsStr::new(&init));
    let (empty, relative) = (OsStr::new(""), OsStr::new("vmlinux"));
    for (k, i) in [
        (None, None),
        (Some(empty), Some(empty)),
        (Some(kernel), Some(init)),
    ] {
        assert_eq!(guest_pair(k, i), Ok(()), "{k:?} {i:?}");
    }
    #[cfg(unix)]
    let not_utf8 = <OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b"/shards/\xffvmlinux");
    #[cfg(not(unix))]
    let not_utf8 = relative;
    for (k, i) in [
        (Some(kernel), None),
        (Some(kernel), Some(empty)),
        (None, Some(init)),
        (Some(relative), Some(init)),
        (Some(relative), Some(relative)),
        (Some(not_utf8), Some(init)),
    ] {
        let why = guest_pair(k, i).unwrap_err().to_string();
        assert!(
            why.contains("SHARDS_KERNEL") && why.contains("SHARDS_INIT"),
            "{k:?} {i:?}: {why}"
        );
    }
}

/// AC-T7.4.1 (kiln plan 01a1dd2e…, P2.2): Shards' check of the host (`shards_api::host_check`, D262) decides whether a
/// node boots image VMs, each of its answers mapped. Do: map a usable host, hosts Shards says run no microVM, and each way
/// the check fails to be made or read. Expect: the usable host served; each unusable one refused Platform in Shards' own
/// words; a check that didn't start, that the OS failed or that ended without its answer refused Os, its words kept; an
/// answer of another Shards' refused Platform, naming it.
#[test]
fn p2_2_each_host_check_answer_maps_to_the_class_or_its_refusal() {
    use shards_api::{Backend, CheckRefusal, HostCheck, Unusable};
    assert_eq!(
        runs_microvms(Ok(HostCheck::Usable(Backend::HypervisorFramework))),
        Ok(())
    );
    for why in [
        Unusable::KvmMissing,
        Unusable::KvmDenied,
        Unusable::HvSupportOff,
        Unusable::WhpOff,
        Unusable::NoNested,
        Unusable::Unsupported {
            os: "macos".into(),
            arch: "x86_64".into(),
        },
    ] {
        assert_eq!(
            runs_microvms(Ok(HostCheck::Unusable {
                backend: Some(Backend::Kvm),
                why: why.clone(),
            })),
            Err(Refusal::Platform {
                what: "Shards image VMs",
                platform: std::env::consts::OS,
                why: why.to_string(),
            })
        );
    }
    assert!(matches!(
        runs_microvms(Err(CheckRefusal::Start("no such file".into()))),
        Err(Refusal::Os { op: "starting shards api-check", why }) if why == "no such file"
    ));
    assert!(matches!(
        runs_microvms(Err(CheckRefusal::Os {
            what: "making the check's pipe",
            kind: std::io::ErrorKind::OutOfMemory,
        })),
        Err(Refusal::Os {
            op: "making the check's pipe",
            ..
        })
    ));
    assert!(matches!(
        runs_microvms(Err(CheckRefusal::Ended {
            status: Some(3),
            said: "shards: no such command: api-check".into(),
        })),
        Err(Refusal::Os { why, .. }) if why.contains("no such command: api-check")
    ));
    assert!(matches!(
        runs_microvms(Err(CheckRefusal::Version(2))),
        Err(Refusal::Platform { what: "Shards image VMs", why, .. }) if why.contains("version 2")
    ));
}

/// AC-T7.4.1 (kiln plan 01a1dd2e…, P2.2): a node's guest runtime lists the Shards image class where it boots image VMs,
/// the class's capacity its host's plane; one whose host Shards' check says runs no microVM lists none, and refuses an
/// image VM's create and the class's capacity alike, in Shards' words. Do: a guest runtime over the model's image boot,
/// and one given the refusal `host_images` makes of a host whose user may not open `/dev/kvm`. Expect: the first's
/// classes hold shards-image, its capacity of the class the owner's plane; the second's hold none of it, its create and
/// its capacity of the class each refused Platform naming `/dev/kvm`, and no boot asked of either.
#[test]
fn p2_2_a_guest_runtime_lists_the_image_class_only_where_it_boots_image_vms() {
    let images = ModelImages::new();
    let (serving, _owner) = model_runtime(&images, waits());
    assert!(
        serving.classes().contains(&hex_api::Class::ShardsImage),
        "{:?}",
        serving.classes()
    );
    // Its host's pool, with the class's one route at its per-box cost on that pool (§11).
    assert_eq!(
        serving.capacity(hex_api::Class::ShardsImage),
        Ok(Some(Capacity {
            routes: vec![hex_api::RouteOffer {
                key: route_key().unwrap(),
                cost: Some(cost(host_pool().total).unwrap()),
                backing: None,
            }],
            ..host_pool()
        }))
    );
    let refused = runs_microvms(Ok(shards_api::HostCheck::Unusable {
        backend: Some(shards_api::Backend::Kvm),
        why: shards_api::Unusable::KvmDenied,
    }))
    .unwrap_err();
    assert!(
        matches!(&refused, Refusal::Platform { what: "Shards image VMs", why, .. } if why.contains("/dev/kvm")),
        "{refused:?}"
    );
    let owner = Arc::new(Owner::new());
    let unserved = GuestRuntime::new(
        Arc::new(shards_api::sim::SimGuests::new(|_| {
            shards_api::End::PowerOff
        })),
        super::super::tests::config(),
    )
    .with_images(Err(refused.clone()), waits(), owner.diagnostics());
    let books: Arc<dyn Books> = owner.clone();
    unserved.bind_books(Arc::downgrade(&books)).unwrap();
    assert!(!unserved.classes().contains(&hex_api::Class::ShardsImage));
    assert_eq!(
        unserved.create(spec("unserved", None)),
        Err(refused.clone())
    );
    assert_eq!(unserved.capacity(hex_api::Class::ShardsImage), Err(refused));
    assert!(record(&owner, &BoxId::new("unserved").unwrap()).is_none());
    assert!(images.asked.lock().unwrap().is_empty());
}

/// AC-T7.4.1 (kiln plan 01a1dd2e…, P2.2; docs/design/shards-image-class.md §11): the class's per-box cost is Shards'
/// envelope on its node's plane. Do derive it for a plane twice and for another plane; allocate under it VMs asked at
/// nothing, at a unit of each, and past Shards' least by less than a unit; and derive it for planes short of Shards'
/// least VM. Expect its floor Shards' least VM, its ceiling the plane, no allowance of its own, one identity for one
/// plane and another for another; each allocation one Shards boots as asked, covering what was asked; a VM past the
/// plane refused; and the short planes refused.
#[test]
fn p2_2_the_image_class_cost_is_shards_envelope_on_its_nodes_plane() {
    let total = host_pool().total;
    let profile = cost(total).unwrap();
    let (vcpus, least) = shards_api::envelope(std::env::consts::ARCH, 0, 0).unwrap();
    assert_eq!(
        profile.floor,
        CapacityResources {
            cpu_milli: u64::from(vcpus) * 1_000,
            mem_bytes: least << 20,
            disk_bytes: 0,
        }
    );
    assert_eq!(profile.ceiling, total);
    assert_eq!(profile.guest_overhead, CapacityResources::default());
    assert_eq!(profile.overhead, CapacityResources::default());
    assert_eq!(cost(total), Ok(profile));
    let larger = CapacityResources {
        mem_bytes: total.mem_bytes * 2,
        ..total
    };
    assert_ne!(cost(larger).unwrap().id, profile.id);
    for asked in [
        CapacityResources::default(),
        CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        CapacityResources {
            cpu_milli: 1_500,
            mem_bytes: (least << 20) + 1,
            disk_bytes: 4_097,
        },
        CapacityResources {
            cpu_milli: 3_000,
            mem_bytes: 300 << 20,
            disk_bytes: 0,
        },
    ] {
        let allocation = profile.allocate(asked).unwrap();
        assert!(admitted_size(&allocation).is_ok(), "{asked:?}");
        assert_eq!(allocation.covers(asked), Ok(()), "{asked:?}");
    }
    assert!(
        profile
            .allocate(CapacityResources {
                cpu_milli: total.cpu_milli + 1,
                ..CapacityResources::default()
            })
            .is_err()
    );
    for short in [
        CapacityResources {
            cpu_milli: 500,
            ..total
        },
        CapacityResources {
            mem_bytes: 1 << 20,
            ..total
        },
    ] {
        assert!(cost(short).is_err(), "{short:?}");
    }
}

/// AC-T7.4.1 (kiln plan 01a1dd2e…, P2.2; docs/design/shards-image-class.md §11): a node's preflight holds an image
/// VM's record to the image class's own checks, never a dedicated guest's. Do preflight the record of a VM its scheduler
/// routed and sized under the class's cost; of one made with no route, sized by its caller; of one whose route names a
/// backing; and of one sized at memory Shards boots otherwise. Expect the first two to pass; the third refused
/// Uncertain, naming its route; the fourth refused Invalid, naming its allocation.
#[test]
fn p2_2_a_node_preflights_an_image_record_as_its_class_admitted_it() {
    let images = ModelImages::new();
    let (runtime, _owner) = model_runtime(&images, waits());
    let ready = |spec: BoxSpec| BoxRecord::new(spec, Phase::Ready, Vec::new());
    let mut admitted = spec("image-admitted", None);
    admitted.bounds = Bounds {
        cpu_max_milli: Some(2_000),
        memory_max: Some(256 << 20),
        ..Bounds::default()
    };
    admitted.allocation = Some(
        cost(host_pool().total)
            .unwrap()
            .allocate(CapacityResources {
                cpu_milli: 2_000,
                mem_bytes: 256 << 20,
                disk_bytes: 0,
            })
            .unwrap(),
    );
    admitted.route = Some(hex_api::RouteReceipt {
        key: route_key().unwrap(),
        backing: None,
    });
    assert_eq!(runtime.preflight_box(&ready(admitted.clone())), Ok(()));
    assert_eq!(
        runtime.preflight_box(&ready(spec("image-own-size", None))),
        Ok(())
    );
    let mut backed = admitted;
    backed.route = Some(hex_api::RouteReceipt {
        key: route_key().unwrap(),
        backing: Some(hex_api::BackingKey {
            pool: [1; 32],
            generation: [2; 16],
        }),
    });
    let refused = runtime.preflight_box(&ready(backed)).unwrap_err();
    assert!(
        matches!(&refused, Refusal::Lifecycle { cause: LifecycleCause::Uncertain, why: Some(why), .. } if why.contains("route")),
        "{refused:?}"
    );
    let mut odd = spec("image-odd", None);
    odd.allocation = Some(
        hex_api::RuntimeCost {
            id: [5; 32],
            floor: CapacityResources::default(),
            ceiling: CapacityResources {
                cpu_milli: 1_000,
                mem_bytes: 1 << 30,
                disk_bytes: 0,
            },
            quantum: CapacityResources {
                cpu_milli: 1_000,
                mem_bytes: 1 << 20,
                disk_bytes: 1,
            },
            guest_overhead: CapacityResources::default(),
            overhead: CapacityResources::default(),
        }
        .allocate(CapacityResources {
            cpu_milli: 1_000,
            mem_bytes: 129 << 20,
            disk_bytes: 0,
        })
        .unwrap(),
    );
    let refused = runtime.preflight_box(&ready(odd)).unwrap_err();
    assert!(
        matches!(&refused, Refusal::Invalid { what: "shards image", why } if why.contains("allocation")),
        "{refused:?}"
    );
}

/// AC-T7.4.1 (kiln plan 01a1dd2e…, P2.2): a node counts its image class's route among the routes it offers where it
/// boots image VMs, so its offers stay within their eligibility word. Do project the routes of a node's guest runtime
/// that boots image VMs and of one that boots none. Expect the first's projection to hold the image route beside the
/// dedicated guest's, two, and the second's the dedicated guest's alone, one.
#[test]
fn p2_2_a_node_counts_its_image_route_where_it_boots_image_vms() {
    let images = ModelImages::new();
    let config = super::super::tests::config();
    let owner = Arc::new(Owner::new());
    let unusable = runs_microvms(Ok(shards_api::HostCheck::Unusable {
        backend: Some(shards_api::Backend::Kvm),
        why: shards_api::Unusable::KvmDenied,
    }))
    .unwrap_err();
    for (boot, count) in [
        (Ok(Arc::clone(&images) as Arc<dyn Images>), 2),
        (Err(unusable), 1),
    ] {
        let runtime = GuestRuntime::new(
            Arc::new(shards_api::sim::SimGuests::new(|_| {
                shards_api::End::PowerOff
            })),
            config.clone(),
        )
        .with_images(boot, waits(), owner.diagnostics())
        .with_runtime_cost(fixed_cost(&config))
        .unwrap();
        let books: Arc<dyn Books> = owner.clone();
        runtime.bind_books(Arc::downgrade(&books)).unwrap();
        let mut projection = crate::devices::RouteProjection::new();
        runtime.project_routes(&[], None, &mut projection).unwrap();
        assert_eq!(projection.count().unwrap(), count);
    }
}

/// Joint plan step 12: do signal KILL to a VM never started. Expect: no boot, its end recorded
/// Signal(9) and shown Empty, and its start then NotRunning.
#[test]
fn kill_before_start_ends_the_vm_without_a_boot() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-unstarted", None)).unwrap().id;
    runtime.signal(&id, Signal::KILL).unwrap();
    let kept = record(&owner, &id).unwrap();
    assert_eq!(
        (kept.phase, kept.exit),
        (Phase::Ready, Some(Exit::Signal(9)))
    );
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::NotRunning(_, "start"))
    ));
    assert!(images.asked.lock().unwrap().is_empty());
}

/// Joint plan step 15 (AC-T7.7.2; N4): do create image VMs granted a provider by name by UDP and kiln by an IPv6
/// address. Expect each refused naming what Shards' destinations (D236) lack, with no record made and no boot asked.
/// Do: create one of the members alone granted a provider by name and kiln on its host, each by TCP, and a publish
/// on the host's loopback, and start it, its image's harness reaching that provider and the host by name and given
/// the publish's port. Expect: its boot keeps exactly those two destinations, holds the harness to them, and asks for
/// exactly that publish; and its inspect shows its every grant, as its record holds them, and the publish as the host
/// bound it, its receiver the harness Shards names, at its own port.
#[test]
fn a_vms_grants_are_programmed_into_its_boot_or_refused_before_its_record() {
    use hex_api::{Destination, Grant, Member, MemberKind, Ports, Proto};
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let codex = Member {
        kind: MemberKind::Harness,
        name: "codex".to_owned(),
    };
    let tcp = |port| Ports {
        proto: Proto::Tcp,
        first: port,
        last: port,
    };
    let granted = |name: &str, grants: Vec<Grant>| {
        let mut spec = spec(name, None);
        if let ClassSpec::ShardsImage(image) = &mut spec.class {
            image.grants = grants;
        }
        spec
    };
    for (grant, needs) in [
        (
            Grant::Egress {
                member: codex.clone(),
                to: Destination::Name("api.openai.com".to_owned()),
                ports: Ports {
                    proto: Proto::Udp,
                    ..tcp(443)
                },
            },
            Upstream::MemberGrantsByName,
        ),
        (
            Grant::Egress {
                member: codex.clone(),
                to: Destination::Address("2001:db8::5".parse().unwrap()),
                ports: tcp(7070),
            },
            Upstream::MemberGrantsByAddress,
        ),
    ] {
        assert_eq!(
            runtime.create(granted("image-unheld", vec![grant])),
            Err(Refusal::Awaits { needs })
        );
        assert!(Transaction::records(owner.as_ref()).unwrap().is_empty());
    }
    assert!(images.asked.lock().unwrap().is_empty());
    let provider = Grant::Egress {
        member: codex.clone(),
        to: Destination::Name("api.openai.com".to_owned()),
        ports: tcp(443),
    };
    let kiln = Grant::Host {
        member: codex.clone(),
        ports: tcp(7070),
    };
    let publish = Grant::Publish {
        member: codex,
        proto: Proto::Tcp,
        port: 1455,
        host: "127.0.0.1".parse().unwrap(),
        host_port: None,
    };
    let grants = vec![provider, kiln, publish];
    let id = runtime
        .create(granted("image-published", grants.clone()))
        .unwrap()
        .id;
    *images.members.lock().unwrap() = vec![MemberInfo {
        index: 0,
        kind: shards_api::image::MemberKind::Harness,
        name: "codex".to_owned(),
        runs: true,
    }];
    images.receive(1455, &[(0, 1455)]);
    images.names(
        shards_api::image::MemberKind::Harness,
        "codex",
        &["api.openai.com:443", "host.docker.internal:7070"],
    );
    runtime.start(&id).unwrap();
    let asked = images.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    let boot = &asked[0];
    assert_eq!(
        boot.publish,
        vec![shards_api::image::Publish {
            guest_port: 1455,
            proto: shards_api::image::Proto::Tcp,
            host: Some("127.0.0.1".parse().unwrap()),
            host_port: 0,
        }]
    );
    assert_eq!(boot.egress.egress, Keep::Only(Vec::new()));
    assert_eq!(boot.egress.mcp, Keep::Only(Vec::new()));
    assert_eq!(
        boot.egress.destinations,
        Keep::Only(vec![
            "api.openai.com:443".to_owned(),
            "host.docker.internal:7070".to_owned()
        ])
    );
    assert_eq!(
        boot.egress.members,
        Some(vec![shards_api::image::MemberReach {
            kind: shards_api::image::MemberKind::Harness,
            name: "codex".to_owned(),
            destinations: vec![
                "api.openai.com:443".to_owned(),
                "host.docker.internal:7070".to_owned()
            ],
        }])
    );
    assert!(!boot.egress.dns);
    let shown = runtime.inspect(&id).unwrap();
    assert_eq!(shown.state, BoxState::Running);
    assert_eq!(shown.grants, grants);
    assert_eq!(
        shown.published,
        vec![hex_api::Published {
            grant: 2,
            at: "127.0.0.1:49152".parse().unwrap(),
            receiver: hex_api::Receiver::Named { port: 1455 },
        }]
    );
}

/// Joint plan step 15 (AC-T7.7.2; N4): do start an image VM granted a provider for its harness alone, where its image
/// also lets an agent reach that provider by name. Expect: the start refused Invalid, past the image's grants, the
/// run host having refused the boot before anything started, and the box ended; so no member reaches a destination
/// granted only to another.
#[test]
fn a_destination_another_member_would_reach_too_refuses_the_boot() {
    use hex_api::{Destination, Grant, Member, MemberKind, Ports, Proto};
    let images = ModelImages::new();
    images.names(
        shards_api::image::MemberKind::Harness,
        "codex",
        &["api.openai.com:443"],
    );
    images.names(
        shards_api::image::MemberKind::Agent,
        "helper",
        &["api.openai.com:443"],
    );
    let (runtime, owner) = model_runtime(&images, waits());
    let mut spec = spec("image-shared-name", None);
    if let ClassSpec::ShardsImage(image) = &mut spec.class {
        image.grants = vec![Grant::Egress {
            member: Member {
                kind: MemberKind::Harness,
                name: "codex".to_owned(),
            },
            to: Destination::Name("api.openai.com".to_owned()),
            ports: Ports {
                proto: Proto::Tcp,
                first: 443,
                last: 443,
            },
        }];
    }
    let id = runtime.create(spec).unwrap().id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Invalid { what: "shards image", why })
            if why.contains("past the image's grants") && why.contains("helper")
    ));
    assert_eq!(images.asked.lock().unwrap().len(), 1);
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 0);
    let shown = runtime.inspect(&id).unwrap();
    assert_eq!(
        (shown.state, shown.exit),
        (BoxState::Empty, Some(Exit::Unknown))
    );
    assert_eq!(record(&owner, &id).unwrap().exit, Some(Exit::Unknown));
}

/// Joint plan step 15 (AC-T7.7.2; N4, D246): do start an image VM granting a provider to a harness its image doesn't
/// declare, beside the image's own harness, which reaches nothing by name. Expect: the start refused Invalid naming
/// the image (the run host's `Cause::Image`: it declares no such member), the run host having refused the boot before
/// anything started, and the box ended; so no grant is kept for a member the image doesn't have.
#[test]
fn a_destination_for_a_member_its_image_doesnt_declare_refuses_the_boot() {
    use hex_api::{Destination, Grant, Member, MemberKind, Ports, Proto};
    let images = ModelImages::new();
    images.names(shards_api::image::MemberKind::Harness, "codex", &[]);
    let (runtime, owner) = model_runtime(&images, waits());
    let mut spec = spec("image-undeclared", None);
    if let ClassSpec::ShardsImage(image) = &mut spec.class {
        image.grants = vec![Grant::Egress {
            member: Member {
                kind: MemberKind::Harness,
                name: "ghost".to_owned(),
            },
            to: Destination::Name("api.openai.com".to_owned()),
            ports: Ports {
                proto: Proto::Tcp,
                first: 443,
                last: 443,
            },
        }];
    }
    let id = runtime.create(spec).unwrap().id;
    let refused = runtime.start(&id);
    assert!(
        matches!(&refused, Err(Refusal::Invalid { what: "shards image", why })
            if why.starts_with("the image:") && why.contains("ghost") && why.contains("declares no such member")),
        "{refused:?}"
    );
    assert_eq!(images.asked.lock().unwrap().len(), 1);
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 0);
    let shown = runtime.inspect(&id).unwrap();
    assert_eq!(
        (shown.state, shown.exit),
        (BoxState::Empty, Some(Exit::Unknown))
    );
    assert_eq!(record(&owner, &id).unwrap().exit, Some(Exit::Unknown));
}

/// Joint plan step 15 (N6): do create, then start, an image VM whose run has a command of its own, granted a publish
/// of its harness's port, first where its image gives that port to the harness (at the harness's own port 8443),
/// then where it gives it no member, so it reaches the run's own command (D122). Expect: each created, its boot
/// asked for exactly that publish; the first runs, its inspect showing the harness Shards names as the port's
/// receiver at 8443; the second's start refused Invalid naming the grant and the run's own command, its run stopped
/// and the box ended with KILL's signal, so no port stays published for anyone its grant doesn't name.
#[test]
fn a_publish_on_a_run_with_its_own_command_is_held_to_its_receiver() {
    use hex_api::{Grant, Member, MemberKind, Proto};
    let publish = Grant::Publish {
        member: Member {
            kind: MemberKind::Harness,
            name: "codex".to_owned(),
        },
        proto: Proto::Tcp,
        port: 1455,
        host: "127.0.0.1".parse().unwrap(),
        host_port: None,
    };
    let commanded = |name: &str| {
        let mut spec = spec(name, Some(vec!["serve".to_owned()]));
        if let ClassSpec::ShardsImage(image) = &mut spec.class {
            image.grants = vec![publish.clone()];
        }
        spec
    };
    let codex = MemberInfo {
        index: 0,
        kind: shards_api::image::MemberKind::Harness,
        name: "codex".to_owned(),
        runs: true,
    };
    let images = ModelImages::new();
    *images.members.lock().unwrap() = vec![codex.clone()];
    images.receive(1455, &[(0, 8443)]);
    let (runtime, _owner) = model_runtime(&images, waits());
    let id = runtime.create(commanded("image-cmd-member")).unwrap().id;
    runtime.start(&id).unwrap();
    assert_eq!(images.asked.lock().unwrap()[0].publish.len(), 1);
    let shown = runtime.inspect(&id).unwrap();
    assert_eq!(shown.state, BoxState::Running);
    assert_eq!(
        shown.published,
        vec![hex_api::Published {
            grant: 0,
            at: "127.0.0.1:49152".parse().unwrap(),
            receiver: hex_api::Receiver::Named { port: 8443 },
        }]
    );

    let images = ModelImages::new();
    *images.members.lock().unwrap() = vec![codex];
    let (runtime, owner) = model_runtime(&images, waits());
    let id = runtime.create(commanded("image-cmd-command")).unwrap().id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Invalid { what: "grants", why })
            if why.starts_with("grants[0]") && why.contains("the run's own command")
    ));
    assert_eq!(images.asked.lock().unwrap().len(), 1);
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 1);
    let shown = runtime.inspect(&id).unwrap();
    assert_eq!(
        (shown.state, shown.exit),
        (BoxState::Empty, Some(Exit::Signal(9)))
    );
    assert!(shown.published.is_empty());
    assert_eq!(record(&owner, &id).unwrap().exit, Some(Exit::Signal(9)));
}

/// Joint plan step 15 (N6): do start an image VM granted a publish to a harness its image doesn't run, then one
/// whose image runs it only as files. Expect: each start refused Invalid naming the grant's member, its run
/// stopped and the box ended with KILL's signal, so no port stays published for a member no grant names.
#[test]
fn a_publish_to_a_member_the_image_doesnt_run_stops_the_vm() {
    use hex_api::{Grant, Member, MemberKind, Proto};
    for (name, members) in [
        ("image-nobody", Vec::new()),
        (
            "image-files",
            vec![MemberInfo {
                index: 0,
                kind: shards_api::image::MemberKind::Harness,
                name: "codex".to_owned(),
                runs: false,
            }],
        ),
    ] {
        let images = ModelImages::new();
        *images.members.lock().unwrap() = members;
        images.receive(1455, &[(0, 1455)]);
        let (runtime, owner) = model_runtime(&images, waits());
        let mut spec = spec(name, None);
        if let ClassSpec::ShardsImage(image) = &mut spec.class {
            image.grants = vec![Grant::Publish {
                member: Member {
                    kind: MemberKind::Harness,
                    name: "codex".to_owned(),
                },
                proto: Proto::Tcp,
                port: 1455,
                host: "127.0.0.1".parse().unwrap(),
                host_port: None,
            }];
        }
        let id = runtime.create(spec).unwrap().id;
        assert!(matches!(
            runtime.start(&id),
            Err(Refusal::Invalid { what: "grants", why }) if why.contains("grants[0].member harness codex")
        ));
        assert_eq!(images.run.stops.load(Ordering::SeqCst), 1);
        // The start's own stop waited for the run's end and recorded it before the start returned, so the box
        // shows it with no wait of the test's.
        let shown = runtime.inspect(&id).unwrap();
        assert_eq!(shown.state, BoxState::Empty);
        assert_eq!(shown.exit, Some(Exit::Signal(9)));
        assert!(shown.published.is_empty());
        assert_eq!(record(&owner, &id).unwrap().exit, Some(Exit::Signal(9)));
        // Why it was stopped outlives the start that said it: the box's end keeps the grants' refusal.
        assert!(
            shown
                .ended
                .as_deref()
                .is_some_and(|why| why.contains("grants[0].member harness codex")),
            "{shown:?}"
        );
    }
}

/// The image class's waits as the guest runtime holds them, so a test can see where a wait of its own stands.
fn image_waits(runtime: &GuestRuntime) -> &ImageWaits {
    runtime
        .images
        .image
        .as_ref()
        .unwrap()
        .waits
        .as_ref()
        .unwrap()
}

/// `started` asked of `id` on its own thread, and the model's gate opened once that call has returned or its own wait
/// has run out (its W* a 1 ns one taught it, so it follows the boot from then on): either way the call has looked at
/// the box with its boot still held. Answers what `started` answered.
fn started_over_a_held_boot(
    runtime: &GuestRuntime,
    images: &ModelImages,
    id: &BoxId,
    wait: &Patience,
) -> Result<BoxInfo, Refusal> {
    std::thread::scope(|scope| {
        let asking = scope.spawn(|| runtime.started(id));
        follow::until(
            wait,
            "started following the boot",
            || spawned(&asking),
            || asking.is_finished() || image_waits(runtime).started.deadline().is_none(),
        )
        .unwrap();
        images.gate(Gate::Open);
        asking.join().unwrap()
    })
}

/// Each test's `started` call under way: these tests' facts alone.
static STARTED_FOLLOWS_RUN: Patience =
    Patience::measuring("test.hex_runtime.image_started_follows_run");
static STARTED_FOLLOWS_FAILURE: Patience =
    Patience::measuring("test.hex_runtime.image_started_follows_failure");
static STARTED_FOLLOWS_ABANDONED: Patience =
    Patience::measuring("test.hex_runtime.image_started_follows_abandoned");

/// Waits whose boot and `started` call each run out at once (a 1 ns W*), so a start answers Pending and `started`
/// follows the boot from its first look.
fn started_waits(name: &'static str, started: &'static str) -> ImageWaits {
    ImageWaits {
        boot: Patience::seeded(name, Duration::from_nanos(1)),
        started: Patience::seeded(started, Duration::from_nanos(1)),
        ..waits().unwrap()
    }
}

/// Kiln's HexVms adapter (ROOT, 2026-11-13; joint plan step 12, decision 9): a start past its boot's W* answers
/// Pending, and its caller waits for the box's run with `started` rather than polling. Do: start a VM whose boot is held
/// past a 1 ns W*, its start Pending; ask `started` as the boot is held, then let its run start. Expect: `started`
/// following the boot until it answers, then the box Running; one boot.
#[test]
fn w12_started_follows_a_held_boot_to_its_run() {
    let images = ModelImages::new();
    images.gate(Gate::Host);
    let (runtime, _owner) = model_runtime(
        &images,
        Ok(started_waits(
            "test.hex_runtime.image_boot_started_run",
            "test.hex_runtime.image_started_run",
        )),
    );
    let id = runtime.create(spec("image-started-run", None)).unwrap().id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Lifecycle {
            cause: LifecycleCause::Pending,
            ..
        })
    ));
    let info = started_over_a_held_boot(&runtime, &images, &id, &STARTED_FOLLOWS_RUN).unwrap();
    assert_eq!(info.state, BoxState::Running);
    assert_eq!(images.asked.lock().unwrap().len(), 1);
}

/// As above, its boot then refused, its run never started. Expect: `started` answering the run's own failure in its
/// runtime's words, `Refusal::Failed`, as the box's `ended` keeps them.
#[test]
fn w12_started_answers_a_boot_that_fails_in_its_words() {
    let images = ModelImages::new();
    images.gate(Gate::Host);
    let (runtime, _owner) = model_runtime(
        &images,
        Ok(started_waits(
            "test.hex_runtime.image_boot_started_failure",
            "test.hex_runtime.image_started_failure",
        )),
    );
    let id = runtime
        .create(spec("image-started-failure", None))
        .unwrap()
        .id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Lifecycle {
            cause: LifecycleCause::Pending,
            ..
        })
    ));
    *images.refuse.lock().unwrap() = Some(ImageRefusal::Boot(
        "its image's members never started".to_owned(),
    ));
    let answered = started_over_a_held_boot(&runtime, &images, &id, &STARTED_FOLLOWS_FAILURE);
    assert!(
        matches!(&answered, Err(Refusal::Failed { id: failed, why }) if failed == &id && why.contains("its image's members never started")),
        "{answered:?}"
    );
    assert!(
        runtime
            .inspect(&id)
            .unwrap()
            .ended
            .is_some_and(|why| why.contains("its image's members never started"))
    );
}

/// As above, its boot's thread then ending with no answer. Expect: `started` refused only there, as the node's broken
/// state in its own words, the boot it follows still under way.
#[test]
fn w12_started_whose_boot_ends_with_no_answer_says_the_boot_is_still_under_way() {
    let images = ModelImages::new();
    images.gate(Gate::Host);
    let (runtime, _owner) = model_runtime(
        &images,
        Ok(started_waits(
            "test.hex_runtime.image_boot_started_abandoned",
            "test.hex_runtime.image_started_abandoned",
        )),
    );
    let id = runtime
        .create(spec("image-started-abandoned", None))
        .unwrap()
        .id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Lifecycle {
            cause: LifecycleCause::Pending,
            ..
        })
    ));
    images.abandon.store(true, Ordering::SeqCst);
    let answered = started_over_a_held_boot(&runtime, &images, &id, &STARTED_FOLLOWS_ABANDONED);
    assert!(
        matches!(&answered, Err(Refusal::Broken(why)) if why.contains("the boot it follows is still under way")),
        "{answered:?}"
    );
}

/// Kiln's HexVms adapter: a box not booting is answered at once, as inspect answers it. Do: create a VM, ask `started`,
/// start it, ask again. Expect: `Created` with nothing booted, then `Running`, each inspect's answer.
#[test]
fn w12_started_of_a_box_not_booting_answers_as_inspect_at_once() {
    let images = ModelImages::new();
    let (runtime, _owner) = model_runtime(&images, waits());
    let id = runtime
        .create(spec("image-started-at-once", None))
        .unwrap()
        .id;
    assert_eq!(runtime.started(&id).unwrap(), runtime.inspect(&id).unwrap());
    assert_eq!(runtime.started(&id).unwrap().state, BoxState::Created);
    assert!(images.asked.lock().unwrap().is_empty());
    runtime.start(&id).unwrap();
    assert_eq!(runtime.started(&id).unwrap().state, BoxState::Running);
    // An id the class holds no record of is no box of its (a node's guest runtime takes such an id as its guest's).
    assert!(matches!(
        Runtime::started(&runtime.images, &BoxId::new("image-started-none").unwrap()),
        Err(Refusal::NoSuchBox(_))
    ));
}

/// A box granted a publish of port 1455 to harness codex, which the model's image doesn't run, its port given to
/// member 0 (none), so the run its boot starts breaks its grants (N6).
fn granted_codex(name: &str) -> BoxSpec {
    use hex_api::{Grant, Member, MemberKind, Proto};
    let mut spec = spec(name, None);
    if let ClassSpec::ShardsImage(image) = &mut spec.class {
        image.grants = vec![Grant::Publish {
            member: Member {
                kind: MemberKind::Harness,
                name: "codex".to_owned(),
            },
            proto: Proto::Tcp,
            port: 1455,
            host: "127.0.0.1".parse().unwrap(),
            host_port: None,
        }];
    }
    spec
}

/// Each test's call under way over a run that breaks its grants: these tests' facts alone.
static STARTED_HOLDS_GRANTS: Patience =
    Patience::measuring("test.hex_runtime.image_started_holds_grants");
static INSPECT_HOLDS_GRANTS: Patience =
    Patience::measuring("test.hex_runtime.image_inspect_holds_grants");

/// Joint plan step 15 (N6), a start past its boot's W* (the person's ruling of 2026-11-13): do start a VM granted a
/// publish to a harness its image doesn't run, its boot held past a 1 ns W* so its start answers Pending, and ask
/// `started` as the boot is held, its run then starting. Expect: `started` refused Invalid naming the grant's member,
/// as a start answers such a run; its run stopped once; and the box ended with KILL's signal, nothing published, its
/// end keeping why, so no port stays published for a member no grant names whichever call took the boot's answer.
#[test]
fn n6_started_over_a_pending_start_stops_a_run_that_breaks_its_grants() {
    let images = ModelImages::new();
    images.receive(1455, &[(0, 1455)]);
    images.gate(Gate::Host);
    let (runtime, owner) = model_runtime(
        &images,
        Ok(started_waits(
            "test.hex_runtime.image_boot_n6_started",
            "test.hex_runtime.image_started_n6",
        )),
    );
    let id = runtime
        .create(granted_codex("image-n6-started"))
        .unwrap()
        .id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Lifecycle {
            cause: LifecycleCause::Pending,
            ..
        })
    ));
    let refused = started_over_a_held_boot(&runtime, &images, &id, &STARTED_HOLDS_GRANTS);
    assert!(
        matches!(&refused, Err(Refusal::Invalid { what: "grants", why }) if why.contains("grants[0].member harness codex")),
        "{refused:?}"
    );
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 1);
    let shown = runtime.inspect(&id).unwrap();
    assert_eq!(
        (shown.state, shown.exit),
        (BoxState::Empty, Some(Exit::Signal(9)))
    );
    assert!(shown.published.is_empty());
    assert!(
        shown
            .ended
            .as_deref()
            .is_some_and(|why| why.contains("grants[0].member harness codex")),
        "{shown:?}"
    );
    assert_eq!(record(&owner, &id).unwrap().exit, Some(Exit::Signal(9)));
}

/// As above, the boot's answer taken by an inspect, a call that follows nothing. Expect: no look refused or showing it
/// running free: it shows Running while its stop goes, then Empty with KILL's signal and nothing published, its end
/// keeping why; its run stopped once; and a `started` asked after it answers its failure in those words.
#[test]
fn n6_an_inspect_that_takes_a_pending_boots_answer_stops_a_run_that_breaks_its_grants() {
    let images = ModelImages::new();
    images.receive(1455, &[(0, 1455)]);
    images.gate(Gate::Host);
    let held = ImageWaits {
        boot: Patience::seeded(
            "test.hex_runtime.image_boot_n6_inspect",
            Duration::from_nanos(1),
        ),
        ..waits().unwrap()
    };
    let (runtime, _owner) = model_runtime(&images, Ok(held));
    let id = runtime
        .create(granted_codex("image-n6-inspect"))
        .unwrap()
        .id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Lifecycle {
            cause: LifecycleCause::Pending,
            ..
        })
    ));
    images.gate(Gate::Open);
    let ended = shown(&runtime, &id, BoxState::Empty, &INSPECT_HOLDS_GRANTS);
    assert_eq!(ended.exit, Some(Exit::Signal(9)));
    assert!(ended.published.is_empty());
    assert!(
        ended
            .ended
            .as_deref()
            .is_some_and(|why| why.contains("grants[0].member harness codex")),
        "{ended:?}"
    );
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 1);
    let late = runtime.started(&id);
    assert!(
        matches!(&late, Err(Refusal::Failed { why, .. }) if why.contains("grants[0].member harness codex")),
        "{late:?}"
    );
}

/// Decision 9 (no record is read), joint plan step 12: do make the image waits. Expect: each following its party (no
/// bound) until its first answer, under its own name, and no refusal for want of a record: an image box needs none.
#[test]
fn the_image_waits_follow_their_parties_until_their_first_answers() {
    let waits = ImageWaits::new();
    assert_eq!(
        (
            waits.boot.deadline(),
            waits.stats.deadline(),
            waits.stop.deadline(),
            waits.delete_boot.deadline(),
            waits.started.deadline()
        ),
        (None, None, None, None, None)
    );
    assert_eq!(
        (
            waits.boot.name(),
            waits.stats.name(),
            waits.stop.name(),
            waits.delete_boot.name(),
            waits.started.name()
        ),
        (
            "hex.image.boot",
            "hex.image.stats",
            "hex.image.stop",
            "hex.image.delete_boot",
            "hex.image.started"
        )
    );
    waits.boot.answered(Duration::from_millis(3));
    assert_eq!(waits.boot.deadline(), Some(Duration::from_millis(3)));
}

/// AC-T7.4.1, joint plan step 12: a call on the guest's path reads the node's records once, the image class's routing
/// taking the one read the call carries on (ROOT, 2026-11-12: it had made every such call read them twice, and usage
/// three times). Do: record an ordinary oci box, as a macOS or Windows node records one in its guest, then, with no
/// guest running, start, exec, signal, inspect, read the usage of, and delete it. Expect: each refused, its box in no
/// guest, and each one scan of the records.
#[test]
fn a_guest_path_call_scans_the_records_once() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let oci = super::super::tests::oci(hex_api::Rootfs::Dir("/rootfs".into()), Vec::new());
    let id = oci.id.clone();
    Transaction::put(
        owner.as_ref(),
        &BoxRecord::new(oci, Phase::Creating, Vec::new()),
    )
    .unwrap();
    let scans = || owner.scans.swap(0, Ordering::SeqCst);
    scans();
    let calls: [(&str, &dyn Fn() -> bool); 6] = [
        ("start", &|| runtime.start(&id).is_err()),
        ("exec", &|| {
            runtime
                .exec(&id, hex_api::Program::of(&["true"]), hex_api::Stdio::null())
                .is_err()
        }),
        ("signal", &|| runtime.signal(&id, Signal::TERM).is_err()),
        ("inspect", &|| runtime.inspect(&id).is_err()),
        ("usage", &|| runtime.usage(&id).is_err()),
        ("delete", &|| runtime.delete(&id).is_err()),
    ];
    let counted: Vec<(&str, bool, usize)> = calls
        .iter()
        .map(|(call, refused)| (*call, refused(), scans()))
        .collect();
    assert!(
        counted.iter().all(|(_, refused, n)| *refused && *n == 1),
        "each call: refused, and its scans of the records: {counted:?}"
    );
}

/// The placeholder kiln's broker gives Claude Code's harness for its account's credential (Shards' D242): a visible
/// marker, then 256 random bits in hex.
const CLAUDE_PLACEHOLDER: &str =
    "kiln-placeholder-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Claude Code's harness as kiln boots it (joint plan step 33's example): its providers by name, by TCP, and its
/// account's credential toward both; and the model's image naming what the harness reaches.
fn credentialed(images: &ModelImages, name: &str) -> (BoxSpec, Vec<hex_api::CredentialGrant>) {
    use hex_api::{CredentialGrant, Destination, Grant, Member, MemberKind, Ports, Proto};
    images.names(
        shards_api::image::MemberKind::Harness,
        "claude",
        &["api.anthropic.com:443", "platform.claude.com:443"],
    );
    let claude = Member {
        kind: MemberKind::Harness,
        name: "claude".to_owned(),
    };
    let egress = |host: &str| Grant::Egress {
        member: claude.clone(),
        to: Destination::Name(host.to_owned()),
        ports: Ports {
            proto: Proto::Tcp,
            first: 443,
            last: 443,
        },
    };
    let credentials = vec![CredentialGrant {
        member: claude.clone(),
        reference: "anthropic/ada".to_owned(),
        toward: vec![
            "api.anthropic.com".to_owned(),
            "platform.claude.com".to_owned(),
        ],
        placeholder: CLAUDE_PLACEHOLDER.to_owned(),
    }];
    let mut spec = spec(name, None);
    if let ClassSpec::ShardsImage(image) = &mut spec.class {
        image.grants = vec![egress("api.anthropic.com"), egress("platform.claude.com")];
        image.credentials = credentials.clone();
    }
    (spec, credentials)
}

/// AC-T7.7.2, joint plan step 33 (its step 6; Shards' D242; docs/design/credential-grants.md C3, C4): do create and
/// start a harness VM granting its credential toward its two providers, on a node whose orchestrator gave it lessors
/// over a simulated broker; then ask the credential as Shards' run host relays its egress's first use, and stop the VM.
/// Expect: the boot asked exactly the box's credential, one grant toward both hosts with its placeholder and a lessor;
/// inspect showing every credential grant as the record holds it; the ask answered with the broker's value, held
/// there; a host the grant doesn't name, or a request without its placeholder, carrying no credential (Shards' decision
/// passes it bare); and the lease given back as the run ends.
#[test]
fn w33_a_harness_vms_credentials_reach_its_boot_and_record_and_lease_from_its_lessors() {
    use hex_api::lease::sim::SimLessors;
    let images = ModelImages::new();
    let broker = SimLessors::new();
    broker.hold("anthropic/ada", b"sk-ant-api03-real").unwrap();
    let (runtime, owner) = model_runtime(&images, waits());
    let runtime = runtime.with_lessors(Arc::new(broker.clone()));
    let (spec, credentials) = credentialed(&images, "image-credentials");
    let id = runtime.create(spec).unwrap().id;
    runtime.start(&id).unwrap();
    let asked = images.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        asked[0].credentials,
        vec![shards_api::image::CredentialGrant {
            reference: "anthropic/ada".to_owned(),
            toward: vec![
                "api.anthropic.com".to_owned(),
                "platform.claude.com".to_owned()
            ],
            placeholder: CLAUDE_PLACEHOLDER.to_owned(),
        }]
    );
    assert!(asked[0].lessor.is_some());
    assert_eq!(runtime.inspect(&id).unwrap().credentials, credentials);
    match &record(&owner, &id).unwrap().spec.class {
        ClassSpec::ShardsImage(image) => assert_eq!(image.credentials, credentials),
        other => panic!("a Shards image VM recorded as {other:?}"),
    }
    assert_eq!(
        shards_api::image::decide(
            &asked[0].credentials,
            "api.anthropic.com",
            "api.anthropic.com",
            Some(CLAUDE_PLACEHOLDER)
        ),
        shards_api::image::Fill::With("anthropic/ada".to_owned())
    );
    assert_eq!(
        shards_api::image::decide(
            &asked[0].credentials,
            "evil.example",
            "evil.example",
            Some(CLAUDE_PLACEHOLDER)
        ),
        shards_api::image::Fill::Pass
    );
    assert_eq!(
        shards_api::image::decide(
            &asked[0].credentials,
            "api.anthropic.com",
            "api.anthropic.com",
            None
        ),
        shards_api::image::Fill::Pass
    );
    let leased = images.run.ask("anthropic/ada").unwrap();
    assert_eq!(leased.value.0, b"sk-ant-api03-real");
    assert!(broker.holds(&leased.lease).unwrap());
    assert!(matches!(
        images.run.ask("anthropic/none"),
        Err(shards_api::image::LeaseRefusal::Denied(_))
    ));
    runtime.signal(&id, Signal::KILL).unwrap();
    assert!(
        !broker.holds(&leased.lease).unwrap(),
        "the run's lease outlived its end"
    );
    assert_eq!(broker.given_back().unwrap(), 1);
}

/// AC-T7.7.2, joint plan step 33 (C2, C3): do create a harness VM granting a credential on a node its orchestrator gave
/// no lessors, and, on one it did, a VM granting a credential toward a host its egress grants don't reach (beyond what
/// its image is let reach). Expect each refused before any record: the first as the node's configuration
/// (`Unconfigured`), the second as invalid credentials; and nothing booted.
#[test]
fn w33_a_credential_no_one_here_can_lease_or_beyond_its_egress_is_refused_before_any_record() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let (spec, _) = credentialed(&images, "image-unleased");
    let id = spec.id.clone();
    assert!(matches!(
        runtime.create(spec),
        Err(Refusal::Unconfigured { what, .. }) if what.contains("credentials")
    ));
    assert!(record(&owner, &id).is_none());
    let runtime = runtime.with_lessors(Arc::new(hex_api::lease::sim::SimLessors::new()));
    let (mut spec, _) = credentialed(&images, "image-beyond");
    if let ClassSpec::ShardsImage(image) = &mut spec.class {
        image.credentials[0]
            .toward
            .push("api.github.com".to_owned());
    }
    let id = spec.id.clone();
    assert!(matches!(
        runtime.create(spec),
        Err(Refusal::Invalid { what: "credentials", why }) if why.contains("api.github.com")
    ));
    assert!(record(&owner, &id).is_none());
    assert!(images.asked.lock().unwrap().is_empty());
}

/// Shards' D286 (kiln goal 01a1ec34…; docs/design/shards-image-class.md §2b): do create a VM given a share another
/// server serves on a node bound no server of them; bind one serving `project`, then create and start a VM given
/// `project` writable to its harness, and create and start one given a source the server serves none of. Expect the
/// first refused `Unconfigured` before any record, nothing booted; the second's boot asked of Shards with one served
/// share at its place, writable, its files run from, given to its harness as it starts, its stream the server's one
/// stream, made for that box, writable; the third's start refused `Invalid` naming its source before any effect, its
/// record still Creating, nothing more booted and no stream made.
#[test]
fn d286_a_served_share_is_asked_of_the_nodes_server_as_its_box_boots() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let harness = hex_api::Member {
        kind: hex_api::MemberKind::Harness,
        name: "claude".to_owned(),
    };
    let served = |name: &str, source: &str| {
        let mut spec = spec(name, None);
        if let ClassSpec::ShardsImage(image) = &mut spec.class {
            image.served = vec![hex_api::ServedMount {
                source: source.to_owned(),
                destination: "/srv/project".to_owned(),
                read_only: false,
                members: vec![harness.clone()],
            }];
        }
        spec
    };
    let unserved = served("image-unserved", "project");
    let id = unserved.id.clone();
    assert!(matches!(
        runtime.create(unserved),
        Err(Refusal::Unconfigured { what, .. }) if what.contains("served shares")
    ));
    assert!(record(&owner, &id).is_none());
    assert!(images.asked.lock().unwrap().is_empty());

    let servers = hex_api::serve::sim::SimServers::new();
    servers.serve("project").unwrap();
    runtime
        .images
        .bind_servers(Arc::new(servers.clone()))
        .unwrap();
    let id = runtime
        .create(served("image-served", "project"))
        .unwrap()
        .id;
    runtime.start(&id).unwrap();
    let asked = images.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    let [share] = asked[0].served.as_slice() else {
        panic!("{} served shares asked", asked[0].served.len());
    };
    assert_eq!(
        (
            share.at.as_str(),
            share.read_only,
            share.exec,
            &share.members
        ),
        (
            "/srv/project",
            false,
            true,
            &Some(PlugFor::Members(vec![MemberName::harness("claude")]))
        )
    );
    assert_eq!(servers.made().unwrap(), 1);
    let probed = servers
        .probe(&share.stream.as_fd().try_clone_to_owned().unwrap())
        .unwrap();
    assert_eq!(
        (probed.id.as_str(), probed.source.as_str(), probed.read_only),
        ("image-served", "project", false)
    );

    let id = runtime
        .create(served("image-elsewhere", "elsewhere"))
        .unwrap()
        .id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Invalid { why, .. }) if why.contains("elsewhere")
    ));
    assert_eq!(record(&owner, &id).unwrap().phase, Phase::Creating);
    assert_eq!(
        images.asked.lock().unwrap().len(),
        1,
        "a boot asked past a refused stream"
    );
    assert_eq!(servers.made().unwrap(), 1);
}

/// The node VM these tests boot (joint plan step 39): run 1's, Hex's node image, the model's admitted size, one share
/// where Hex's init finds it, its node's key, and its lanes as its node fitted them (a fixture: six, one place's).
fn node_vm() -> NodeVm {
    NodeVm {
        run: NodeRun::from_bytes([1; 16]),
        image: hex_api::sim::SIM_IMAGE.to_owned(),
        allocation: allocation(),
        shares: vec![ImageShare {
            host: std::path::PathBuf::from("/srv/node-state"),
            at: "/run/hex/shares/0".to_owned(),
            read_only: false,
            exec: true,
            members: None,
        }],
        controller: [9; 32],
        lanes: 6,
    }
}

/// Joint plan step 39 (AC-T7.4.1; ROOT, 2026-11-13: the hosted node VM is an image VM, one code path): do boot a node
/// VM through the image class. Expect its boot asked of Shards as Hex's own init under the nesting grant (`/init`, no
/// member, no published port, no grant past the VM, no credential, no ready-by of Hex's), at its admitted size, with its
/// shares, and with its run's agent-control endpoint taking its node's key alone and admitting its lanes to the relay's
/// sockets, each a guest frame wide; its endpoint and READY answered; its record a node VM's, Ready, named by its run;
/// and no box call seeing it: no box record, nothing listed.
#[test]
fn w39_a_node_vm_boots_under_shards_init_through_the_image_class_and_is_never_a_box() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let vm = node_vm();
    let booted = runtime.images.boot_node(&vm).unwrap();
    let asked = images.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    let spec = &asked[0];
    let size = vm.allocation.vm_size().unwrap();
    assert_eq!(
        (spec.image.as_str(), spec.vcpus, spec.memory_mib),
        (vm.image.as_str(), size.vcpus, size.memory_mib)
    );
    assert_eq!(spec.command, Some(vec!["/init".to_owned()]));
    assert!(spec.nesting && spec.ready_by.is_none());
    assert!(spec.members.is_empty() && spec.publish.is_empty() && spec.credentials.is_empty());
    assert_eq!(
        spec.egress,
        shards_api::image::Narrowing {
            egress: Keep::Only(Vec::new()),
            mcp: Keep::Only(Vec::new()),
            destinations: Keep::Only(Vec::new()),
            dns: false,
            members: None,
        }
    );
    assert_eq!(spec.shares, vm.shares);
    assert_eq!(
        spec.control,
        Some(shards_api::image::ControlAsk {
            controller: vm.controller,
            sockets: Some(shards_api::image::SocketGrant {
                dir: hex_client::guest::lanes::DIR.to_owned(),
                lanes: vm.lanes,
                window: u32::try_from(hex_core::stream::MAX_FRAME).unwrap(),
            }),
            stdio: false,
        })
    );
    assert!(spec.egress_ceiling.is_empty());
    assert_eq!(
        (booted.run, booted.control, booted.ready),
        (vm.run, model_control(), shards_api::image::Ready::OnTime)
    );
    let record = owner
        .node_vms
        .lock()
        .unwrap()
        .get(&vm.run)
        .cloned()
        .unwrap();
    assert_eq!(
        (
            record.phase,
            record.image.as_str(),
            record.allocation,
            record.exit
        ),
        (NodeVmPhase::Ready, vm.image.as_str(), vm.allocation, None)
    );
    assert!(owner.records.lock().unwrap().is_empty());
    assert!(runtime.list().unwrap().is_empty());
}

/// Joint plan step 39, item 4 (AC-T7.4.1; ROOT, 2026-11-13: a node VM's starts told cold or restored, and its READY an
/// event line): do boot a node VM whose run host says it restored its image's template, then form its `node_vm_ready`
/// line, and one for a run that saved its template and said READY late. Expect the boot to answer its run restored and
/// its READY on time, and each line one line: its run's name, how it started and how its READY came, each a JSON
/// string, and its READY's time in whole nanoseconds.
#[test]
fn w39_a_node_vms_boot_says_how_its_run_started_and_its_ready_line_says_it() {
    let images = ModelImages::new();
    *images.started_from.lock().unwrap() = StartedFrom::Restored;
    let (runtime, _owner) = model_runtime(&images, waits());
    let vm = node_vm();
    let booted = runtime.images.boot_node(&vm).unwrap();
    assert_eq!(
        (booted.started_from, booted.ready),
        (
            hex_api::StartedFrom::Restored,
            shards_api::image::Ready::OnTime
        )
    );
    let run = "01".repeat(16);
    assert_eq!(
        node::ready_line(
            &vm.run,
            booted.started_from,
            booted.ready,
            Duration::from_nanos(1_234)
        ),
        format!(
            "hex.guest event=node_vm_ready run=\"{run}\" started_from=\"restored\" ready=\"on_time\" took_ns=1234"
        )
    );
    assert_eq!(
        node::ready_line(
            &vm.run,
            hex_api::StartedFrom::Saved,
            shards_api::image::Ready::Late(5),
            Duration::from_secs(2)
        ),
        format!(
            "hex.guest event=node_vm_ready run=\"{run}\" started_from=\"saved\" ready=\"late\" took_ns=2000000000"
        )
    );
}

/// Joint plan step 39, place 3 (AC-T7.4.1; ROOT, 2026-11-13: the node VM is held through its run's rate, D247): do boot a
/// node VM whose run says its governor holds a rate over 70 ms, hold it to 4 CPUs, read its period, then stop it and
/// hold it again. Expect its run held at 4,000 milli-CPU as an image box's is, its period its run's 70 ms, and once its
/// run has ended the hold refused, its run no longer running.
#[test]
fn w39_a_node_vm_is_held_to_its_rate_over_its_runs_period() {
    let images = ModelImages::new();
    *images.period.lock().unwrap() = Duration::from_millis(70);
    let (runtime, _owner) = model_runtime(&images, waits());
    let vm = node_vm();
    runtime.images.boot_node(&vm).unwrap();
    let rate = std::num::NonZeroU64::new(4_000);
    assert_eq!(runtime.images.rate_node(vm.run, rate).unwrap(), rate);
    assert_eq!(*images.run.rates.lock().unwrap(), [rate]);
    assert_eq!(
        runtime.images.rate_period_node(vm.run).unwrap(),
        Some(Duration::from_millis(70))
    );
    runtime.images.stop_node(vm.run).unwrap();
    let refused = runtime.images.rate_node(vm.run, rate).unwrap_err();
    assert!(
        matches!(&refused, Refusal::Invalid { why, .. } if why.contains("isn't running here")),
        "{refused:?}"
    );
}

/// Joint plan step 39 (AC-T7.4.1; ROOT, 2026-11-13: a node VM's guest is told its room like any guest's, D188): do boot a
/// node VM, tell its guest a room of 1 GiB, then stop it and tell it again. Expect its run told the 1 GiB as an image box's
/// is, and once its run has ended the room refused, its run no longer running.
#[test]
fn w39_a_node_vms_guest_is_told_its_room_through_its_run() {
    let images = ModelImages::new();
    let (runtime, _owner) = model_runtime(&images, waits());
    let vm = node_vm();
    runtime.images.boot_node(&vm).unwrap();
    let room = Some(1_i64 << 30);
    assert_eq!(
        runtime.images.room_node(vm.run, room).unwrap(),
        Some(1 << 30)
    );
    assert_eq!(*images.run.rooms.lock().unwrap(), [room]);
    runtime.images.stop_node(vm.run).unwrap();
    let refused = runtime.images.room_node(vm.run, room).unwrap_err();
    assert!(
        matches!(&refused, Refusal::Invalid { why, .. } if why.contains("isn't running here")),
        "{refused:?}"
    );
}

/// Joint plan step 39 (AC-T7.4.1; ROOT, 2026-11-13: its HostUse readable, as an image VM's): do boot a node VM, read
/// what it costs its host, then stop it. Expect its use its run's microVM part, through the same stats as an image
/// box's; its stop its run's end, `KILL`, kept in its record, Ended; and its use no longer read, its run gone.
#[test]
fn w39_a_node_vms_host_use_is_its_runs_and_its_stop_ends_its_record() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let vm = node_vm();
    runtime.images.boot_node(&vm).unwrap();
    let vcpus = vm.allocation.vm_size().unwrap().vcpus;
    images.run.stats.lock().unwrap().microvm = microvm(Ok(guest_times(vcpus)));
    let used = runtime.images.host_use_node(vm.run).unwrap();
    assert_eq!(
        (used.cpu_ns, used.resident_bytes, used.footprint_bytes),
        (9_000_000, 1 << 20, 1 << 20)
    );
    assert_eq!(
        used.guest_ns,
        (1..=u64::from(vcpus))
            .map(|i| 1_000 * i)
            .collect::<Vec<_>>()
    );
    let exit = runtime.images.stop_node(vm.run).unwrap();
    assert_eq!(exit, Exit::Signal(Signal::KILL.number()));
    let record = owner
        .node_vms
        .lock()
        .unwrap()
        .get(&vm.run)
        .cloned()
        .unwrap();
    assert_eq!(
        (record.phase, record.exit),
        (NodeVmPhase::Ended, Some(exit))
    );
    assert!(runtime.images.host_use_node(vm.run).is_err());
}

/// Joint plan step 39 (AC-T7.4.1): do boot a node VM that Shards refuses to start. Expect the boot refused in Shards'
/// words, typed by what failed, and its record Ended, its exit unknown: no run of it held.
#[test]
fn w39_a_node_vm_whose_boot_is_refused_ends_its_record_in_shards_words() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    *images.refuse.lock().unwrap() = Some(ImageRefusal::Boot("no room for the VM".to_owned()));
    let vm = node_vm();
    let refused = runtime.images.boot_node(&vm).unwrap_err();
    assert!(
        matches!(&refused, Refusal::Os { why, .. } if why.contains("no room for the VM")),
        "{refused:?}"
    );
    let record = owner
        .node_vms
        .lock()
        .unwrap()
        .get(&vm.run)
        .cloned()
        .unwrap();
    assert_eq!(
        (record.phase, record.exit),
        (NodeVmPhase::Ended, Some(Exit::Unknown))
    );
    assert!(runtime.images.host_use_node(vm.run).is_err());
}

/// Each signal the model's member host took, with its member's index, in order.
type Taken = Arc<Mutex<Vec<(u32, u32)>>>;

/// A run's real agent-control endpoint (D238, D252) on loopback, Shards' own, its microVM's key the model's, taking
/// `ask`'s controller alone and admitting its members' lanes; its host serves each lane as a run host serves a member's:
/// a member in `ended` said ended, `Exited(0)`; any other running, the signal it is steered with taken, told to `taken`
/// with its member's index, and the member then killed by it, `Signaled(n)`, as a command that keeps no handler is. A
/// model run's endpoint and host live while the test does.
fn serve_members(
    ask: &shards_api::image::ControlAsk,
    ended: Vec<u32>,
    taken: Taken,
) -> shards_api::image::ControlAt {
    use shards_api::control::exec;
    let vm = shards_api::control::Identity::generate().unwrap();
    let (endpoint, served) = shards_remote::control::Endpoint::bind(
        &vm,
        ask.controller,
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)),
        shards_remote::control::Streams {
            outputs: &[],
            inputs: &[],
            execs: Some(shards_api::control::exec_admissions()),
            sockets: None,
            bridges: &[],
        },
    )
    .unwrap();
    let (address, waker, wake) = (endpoint.address, endpoint.waker, endpoint.wake);
    std::thread::spawn(move || endpoint.run());
    let lanes = served.members.unwrap();
    let revoke = served.revoke;
    std::thread::spawn(move || {
        // Held for the endpoint's life: let go, it would end every lane.
        let _revoke = revoke;
        for lane in lanes {
            let member = lane.ask.member;
            if ended.contains(&member) {
                lane.replies.tell(exec::Reply::Exited(0));
                continue;
            }
            lane.replies.tell(exec::Reply::Running);
            if let Ok(exec::Steer::Signal(signal)) = lane.steer.recv() {
                taken.lock().unwrap().push((member, signal));
                lane.replies.tell(exec::Reply::Steered(Ok(())));
                lane.replies.tell(exec::Reply::Signaled(signal));
            }
        }
    });
    shards_api::image::ControlAt {
        address,
        vm: vm.fingerprint(),
        waker,
        wake,
    }
}

/// A run's real agent-control endpoint (D238, D252) on loopback, Shards' own, its microVM's key the model's, taking
/// `ask`'s controller alone and admitting its execs; its host serves each MCP ask as a run host serves a member's bridge
/// (D254): one for a member in `refused` its run asked no bridge of, `Unstarted(Refused)`; any other, a connection as its
/// harness makes one, started, the harness echoing every byte it is sent until what it is sent ends, then finishing its
/// output and saying it ended, `Exited(0)` (Shards' own stand-in, `control.rs`' `mcp_stand_in`). A model run's endpoint
/// and host live while the test does.
fn serve_mcp(
    ask: &shards_api::image::ControlAsk,
    refused: Vec<u32>,
) -> shards_api::image::ControlAt {
    use shards_api::control::exec;
    let vm = shards_api::control::Identity::generate().unwrap();
    let (endpoint, served) = shards_remote::control::Endpoint::bind(
        &vm,
        ask.controller,
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)),
        shards_remote::control::Streams {
            outputs: &[],
            inputs: &[],
            execs: Some(shards_api::control::exec_admissions()),
            sockets: None,
            bridges: &[],
        },
    )
    .unwrap();
    let (address, waker, wake) = (endpoint.address, endpoint.waker, endpoint.wake);
    std::thread::spawn(move || endpoint.run());
    let starts = served.execs.unwrap();
    let revoke = served.revoke;
    std::thread::spawn(move || {
        // Held for the endpoint's life: let go, it would end every lane.
        let _revoke = revoke;
        for start in starts {
            assert_eq!(start.ask.kind, exec::Kind::Mcp);
            if refused.contains(&start.ask.member) {
                start
                    .replies
                    .tell(exec::Reply::Unstarted(exec::Unstarted::Refused));
                continue;
            }
            std::thread::spawn(move || echo_harness(start));
        }
    });
    shards_api::image::ControlAt {
        address,
        vm: vm.fingerprint(),
        waker,
        wake,
    }
}

/// An MCP connection's host (D254) as Shards' stand-in makes one: says it started, its harness echoing what it is sent;
/// as that ends, finishes its output and says it ended, keeping its streams' controls until its steering closes.
fn echo_harness(start: shards_api::control::exec::Start) {
    use shards_remote::managed::input::Next;
    let shards_api::control::exec::Start {
        stdout,
        stdin,
        steer,
        replies,
        ..
    } = start;
    replies.tell(shards_api::control::exec::Reply::Started);
    let (relay, input) = stdin.unwrap();
    let mut written = 0u64;
    loop {
        match relay.next() {
            Next::Record { bytes, .. } => {
                written += bytes.len() as u64;
                stdout.0.wait_capacity().unwrap();
                let mut record = stdout.0.take_buffer().unwrap().unwrap();
                record.extend_from_slice(&bytes);
                stdout.0.submit(record).unwrap();
                relay.delivered(bytes);
            }
            Next::End => {
                input.ended(written, shards_abi::run::managed::InputEnd::Closed);
                break;
            }
            Next::Abort(errno) => {
                input.ended(written, shards_abi::run::managed::InputEnd::Aborted(errno));
                break;
            }
            Next::Gone => break,
        }
    }
    stdout.1.finish().unwrap();
    replies.tell(shards_api::control::exec::Reply::Exited(0));
    while steer.recv().is_ok() {}
    drop((stdout, input));
}

/// Each program the model's member host was asked to run (D243), with its member's index and its spec, in order.
type Execed = Arc<Mutex<Vec<(u32, shards_abi::run::Spec)>>>;

/// A run's real agent-control endpoint (D238, D252) on loopback, Shards' own, its microVM's key the model's, taking
/// `ask`'s controller alone and admitting its execs; its host serves each exec as a run host serves a program run in a
/// member (D243), each ask told to `execed` with its member's index: one in a member in `refused`, as one that has
/// ended, `Unstarted(Refused)`; any other started ([`model_program`]). A model run's endpoint and host live while the
/// test does.
fn serve_execs(
    ask: &shards_api::image::ControlAsk,
    refused: Vec<u32>,
    execed: Execed,
) -> shards_api::image::ControlAt {
    use shards_api::control::exec;
    let vm = shards_api::control::Identity::generate().unwrap();
    let (endpoint, served) = shards_remote::control::Endpoint::bind(
        &vm,
        ask.controller,
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)),
        shards_remote::control::Streams {
            outputs: &[],
            inputs: &[],
            execs: Some(shards_api::control::exec_admissions()),
            sockets: None,
            bridges: &[],
        },
    )
    .unwrap();
    let (address, waker, wake) = (endpoint.address, endpoint.waker, endpoint.wake);
    std::thread::spawn(move || endpoint.run());
    let starts = served.execs.unwrap();
    let revoke = served.revoke;
    std::thread::spawn(move || {
        // Held for the endpoint's life: let go, it would end every lane.
        let _revoke = revoke;
        for start in starts {
            let exec::Kind::Exec(spec) = &start.ask.kind else {
                panic!("an ask of no program: {:?}", start.ask.kind);
            };
            execed
                .lock()
                .unwrap()
                .push((start.ask.member, spec.as_ref().clone()));
            if refused.contains(&start.ask.member) {
                start
                    .replies
                    .tell(exec::Reply::Unstarted(exec::Unstarted::Refused));
                continue;
            }
            std::thread::spawn(move || model_program(start));
        }
    });
    shards_api::image::ControlAt {
        address,
        vm: vm.fingerprint(),
        waker,
        wake,
    }
}

/// `bytes` as one record of the output `to` produces, once it has room.
fn produce(to: &shards_remote::managed::Producer, bytes: &[u8]) {
    to.wait_capacity().unwrap();
    let mut record = to.take_buffer().unwrap().unwrap();
    record.extend_from_slice(bytes);
    to.submit(record).unwrap();
}

/// A program run in a member as the model's host runs one (D243): it says it started, writes its arguments, joined by
/// spaces, as a line to its stdout and `said` to its stderr; then `cat` echoes what it is sent to its stdout until that
/// ends, `sleep` runs until a signal ends it, and `exit N` exits N, any other 0. It finishes both outputs before it says
/// how it ended, as Shards' exec host does, keeping its streams' controls until its steering closes.
fn model_program(start: shards_api::control::exec::Start) {
    use shards_api::control::exec::{Kind, Reply, Steer};
    use shards_remote::managed::input::Next;
    let shards_api::control::exec::Start {
        ask,
        stdout,
        stderr,
        stdin,
        steer,
        replies,
        ..
    } = start;
    let Kind::Exec(spec) = ask.kind else {
        panic!("an ask of no program");
    };
    let argv: Vec<String> = spec
        .argv
        .iter()
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    replies.tell(Reply::Started);
    produce(&stdout.0, format!("{}\n", argv.join(" ")).as_bytes());
    if let Some((producer, _)) = &stderr {
        produce(producer, b"said\n");
    }
    let mut end = Reply::Exited(0);
    match argv.first().map(String::as_str) {
        Some("cat") => {
            let (relay, input) = stdin.as_ref().unwrap();
            let mut written = 0u64;
            loop {
                match relay.next() {
                    Next::Record { bytes, .. } => {
                        written += bytes.len() as u64;
                        produce(&stdout.0, &bytes);
                        relay.delivered(bytes);
                    }
                    Next::End => {
                        input.ended(written, shards_abi::run::managed::InputEnd::Closed);
                        break;
                    }
                    Next::Abort(errno) => {
                        input.ended(written, shards_abi::run::managed::InputEnd::Aborted(errno));
                        break;
                    }
                    Next::Gone => break,
                }
            }
        }
        Some("sleep") => {
            while let Ok(steered) = steer.recv() {
                replies.took();
                if let Steer::Signal(signal) = steered {
                    end = Reply::Signaled(signal);
                    break;
                }
            }
        }
        Some("exit") => end = Reply::Exited(argv[1].parse().unwrap()),
        _ => {}
    }
    stdout.1.finish().unwrap();
    if let Some((_, control)) = &stderr {
        control.finish().unwrap();
    }
    replies.tell(end);
    while steer.recv().is_ok() {}
    drop((stdout, stderr, stdin));
}

/// A run's real agent-control endpoint (D252) on loopback, its microVM's key the model's own, taking `ask`'s controller
/// alone and admitting the lanes it grants; its host connects each lane to the socket it names in `dir`, as
/// shards-init's relay host does (D251), and answers with its run host's end of it ([`lane_to`]). A model run's
/// endpoint and host live while the test does.
fn serve_lanes(
    ask: &shards_api::image::ControlAsk,
    dir: std::path::PathBuf,
) -> shards_api::image::ControlAt {
    let grant = ask.sockets.as_ref().unwrap();
    let vm = shards_api::control::Identity::generate().unwrap();
    let (endpoint, served) = shards_remote::control::Endpoint::bind(
        &vm,
        ask.controller,
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)),
        shards_remote::control::Streams {
            outputs: &[],
            inputs: &[],
            execs: None,
            sockets: Some(shards_api::control::SocketAdmission {
                lanes: grant.lanes,
                window: grant.window,
            }),
            bridges: &[],
        },
    )
    .unwrap();
    let (address, waker, wake) = (endpoint.address, endpoint.waker, endpoint.wake);
    std::thread::spawn(move || endpoint.run());
    let asks = served.sockets.unwrap();
    let revoke = served.revoke;
    std::thread::spawn(move || {
        // Held for the endpoint's life: let go, it would end every lane.
        let _revoke = revoke;
        for ask in asks {
            let reply = match lane_to(&dir.join(ask.name())) {
                Ok(socket) => shards_remote::control::unix::Reply::Connected(socket),
                // Linux's ECONNREFUSED: the run host is the guest's.
                Err(_) => shards_remote::control::unix::Reply::Unreached(
                    std::num::NonZeroU16::new(111).unwrap(),
                ),
            };
            let _ = ask.reply(reply);
        }
    });
    shards_api::image::ControlAt {
        address,
        vm: vm.fingerprint(),
        waker,
        wake,
    }
}

/// A connection to the relay's socket, as the relay listens in `/run/hex/channel` (its `--unix` form): on Unix an
/// AF_UNIX stream socket, std's.
#[cfg(unix)]
type Relayed = std::os::unix::net::UnixStream;

/// The run host's end of a lane to the relay's socket at `path` (D251): on Unix the socket itself, as shards-init's
/// relay host connects it.
#[cfg(unix)]
fn lane_to(path: &Path) -> std::io::Result<std::os::unix::net::UnixStream> {
    std::os::unix::net::UnixStream::connect(path)
}

/// The relay's socket at `path`, listening once this returns, its first connection served by `serve` on a thread of
/// its own.
#[cfg(unix)]
fn relay<T: Send + 'static>(
    path: &Path,
    serve: impl FnOnce(Relayed) -> T + Send + 'static,
) -> std::thread::JoinHandle<T> {
    let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
    std::thread::spawn(move || serve(listener.accept().unwrap().0))
}

/// A lane's end on Windows as Shards' run host makes it there (`crates/shards/src/api_run.rs`'s `open_lane` and
/// `handed_pair`, at 097ec0047): a plain pipe pair, whose ends each say the end of their writes as a socket's
/// half-close does, the run host's end answering the lane and the other its VM's, which carries the lane to the relay's
/// socket in the guest. The relay here reads and writes the VM's end itself: the guest's AF_UNIX socket is a Linux
/// one, which no Windows host carries, and afunix.sys, which would stand in for it, now and then misses waking a read
/// with its peer's end (HEX-SOCK-3, beaf22ee), the end this relay waits for.
#[cfg(windows)]
type Relayed = shards_ipc::Stream;

/// The relays listening in the model runs' socket directories, each by its socket's path: the guest's directory, as
/// one map of this process's on Windows.
#[cfg(windows)]
static RELAYS: Mutex<BTreeMap<PathBuf, std::sync::mpsc::Sender<Relayed>>> =
    Mutex::new(BTreeMap::new());

/// The run host's end of a lane to the relay's socket at `path` (D251), the VM's end handed to that relay: a path no
/// relay listens at is not found, as its connect would be.
#[cfg(windows)]
fn lane_to(path: &Path) -> std::io::Result<shards_ipc::Stream> {
    let relays = RELAYS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let relay = relays.get(path).ok_or(std::io::ErrorKind::NotFound)?;
    let (ours, theirs) = shards_ipc::Stream::plain_pair()?;
    relay
        .send(theirs)
        .map_err(|_| std::io::ErrorKind::ConnectionRefused)?;
    Ok(ours)
}

/// The relay's socket at `path`, listening once this returns, its first connection served by `serve` on a thread of
/// its own; gone as `serve` returns, as a listener is.
#[cfg(windows)]
fn relay<T: Send + 'static>(
    path: &Path,
    serve: impl FnOnce(Relayed) -> T + Send + 'static,
) -> std::thread::JoinHandle<T> {
    let (listening, connections) = std::sync::mpsc::channel();
    RELAYS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(path.to_owned(), listening);
    let path = path.to_owned();
    std::thread::spawn(move || {
        let served = serve(connections.recv().unwrap());
        RELAYS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&path);
        served
    })
}

/// The node's relay in the guest under shards-init, as far as its first connection needs it: its control socket in
/// `dir`, one connection's token, then its node's hello answered, and each capacity ask after it with `pool`, its
/// guest's own, where one is given; held until the client goes. Says whether it greeted.
fn greeting_relay(dir: &std::path::Path, pool: Option<Capacity>) -> std::thread::JoinHandle<bool> {
    use std::io::{Read, Write};
    let path = dir.join(hex_core::stream::socket_name(hex_core::stream::CONTROL_PORT).unwrap());
    relay(&path, move |mut conn| {
        conn.write_all(&7u64.to_le_bytes()).unwrap();
        let mut d = hex_core::stream::Deframer::new(hex_core::wire::MAX_MESSAGE);
        let mut byte = [0u8; 1];
        let mut greeted = false;
        loop {
            let message = loop {
                // Held until the client goes, as a relay's connection is.
                if !matches!(conn.read(&mut byte), Ok(1)) {
                    return greeted;
                }
                if let (_, Some(m)) = d.take(&byte).unwrap() {
                    break m;
                }
            };
            let (h, request) =
                hex_core::wire::read_request_apart(&message, hex_core::wire::MAX_MESSAGE).unwrap();
            let answer = match (request, &pool) {
                (hex_core::wire::Request::Hello { .. }, _) => {
                    greeted = true;
                    hex_core::wire::Answer::Hello(hex_core::wire::Hello {
                        build: hex_core::wire::build(),
                        classes: vec![hex_api::Class::Oci],
                        limits: hex_core::wire::Limits {
                            connections: 1,
                            message_bytes: hex_core::wire::MAX_MESSAGE,
                            in_flight: 1,
                            programs: 1,
                        },
                        budgets: hex_core::wire::Budgets {
                            in_flight: 1,
                            programs: 1,
                        },
                    })
                }
                (hex_core::wire::Request::Call(hex_core::wire::Call::Capacity(_)), Some(pool)) => {
                    hex_core::wire::Answer::Capacity(Some(pool.try_clone().unwrap()))
                }
                (other, _) => panic!("the relay was asked {other:?}"),
            };
            let bytes = hex_core::wire::answer(h.id, &answer, hex_core::wire::MAX_MESSAGE).unwrap();
            conn.write_all(&hex_core::stream::frame(&bytes, hex_core::wire::MAX_MESSAGE).unwrap())
                .unwrap();
        }
    })
}

/// A fixed VM's cost profile for `config`'s guest: its vCPUs and memory, floor and ceiling alike, as a node's guest is
/// charged (`GuestConfig::checked_runtime_allocation`).
fn fixed_cost(config: &crate::guest::GuestConfig) -> hex_api::RuntimeCost {
    let vm = crate::guest::selected::resources(config).unwrap();
    hex_api::RuntimeCost {
        id: *b"hex-runtime node VM test profile",
        floor: vm,
        ceiling: vm,
        quantum: CapacityResources {
            cpu_milli: 1_000,
            mem_bytes: 1 << 20,
            disk_bytes: 1,
        },
        guest_overhead: CapacityResources::default(),
        overhead: CapacityResources::default(),
    }
}

/// A node's guest runtime of the image form, its node VM booted through `images`, its records `owner`'s, its node
/// having fitted it six lanes: with the configuration it was made from.
fn image_form(
    images: &Arc<ModelImages>,
    owner: &Arc<Owner>,
) -> (GuestRuntime, crate::guest::GuestConfig) {
    let config = crate::guest::GuestConfig {
        boot: crate::guest::Boot::Image(hex_api::sim::SIM_IMAGE.to_owned()),
        ..super::super::tests::config()
    };
    let runtime = GuestRuntime::new(
        Arc::new(shards_api::sim::SimGuests::new(|_| {
            shards_api::End::PowerOff
        })),
        config.clone(),
    )
    .with_images(
        Ok(Arc::clone(images) as Arc<dyn Images>),
        waits(),
        owner.diagnostics(),
    )
    .with_runtime_cost(fixed_cost(&config))
    .unwrap();
    runtime.set_lanes(6).unwrap();
    let books: Arc<dyn Books> = owner.clone();
    runtime.bind_books(Arc::downgrade(&books)).unwrap();
    (runtime, config)
}

/// Joint plan step 39 (AC-T7.4.1; docs/design/node-under-shards-init.md §4, §5; ROOT, 2026-11-13: one code path): do
/// boot a node's guest under shards-init. Expect it booted through the image class as a node VM, its run's endpoint
/// taking this runtime's key and admitting the lanes its node fitted; its record a node VM's, Ready, charged at the
/// configured VM; its relay dialed through a lane of that endpoint and its node's hello the answer; and the runtime's
/// next ask the same guest, its run still running, no second boot.
#[test]
fn w39_a_guest_under_shards_init_boots_through_the_image_class_and_greets_its_node_over_its_runs_lanes()
 {
    let images = ModelImages::new();
    let sockets = super::super::tests::socket_fixture();
    *images.serve.lock().unwrap() = Some(sockets.path().to_owned());
    let relay = greeting_relay(sockets.path(), None);
    let owner = Arc::new(Owner::new());
    let (runtime, config) = image_form(&images, &owner);
    runtime.boot().unwrap();
    let asked = images.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    let spec = &asked[0];
    assert!(spec.nesting && spec.command == Some(vec!["/init".to_owned()]));
    let control = spec.control.as_ref().unwrap();
    assert_ne!(control.controller, [0; 32]);
    assert_eq!(control.sockets.as_ref().map(|grant| grant.lanes), Some(6));
    let records = owner.node_vms.lock().unwrap().clone();
    assert_eq!(records.len(), 1);
    let record = records.into_values().next().unwrap();
    assert_eq!(record.phase, NodeVmPhase::Ready);
    assert_eq!(
        record.allocation,
        config
            .checked_runtime_allocation(fixed_cost(&config))
            .unwrap()
    );
    // The same guest again: its run still runs.
    runtime.boot().unwrap();
    assert_eq!(images.asked.lock().unwrap().len(), 1);
    drop(runtime);
    assert!(relay.join().unwrap());
}

/// The review's C-4 (AC-T7.4.1; joint plan step 39): a node VM whose run ended on its own, its end not yet kept in its
/// row, is no reason to boot another beside it while that row stays Ready, locked and booked. Do boot a node's guest
/// under shards-init, end its run on its own, refuse its row's Ended write as a full disk refuses it, and ask the guest
/// again. Expect the ask refused in the write's words, no second boot asked of Shards, and the row still Ready.
#[test]
fn c4_a_node_vm_whose_end_cant_be_kept_refuses_the_next_ask_rather_than_boot_beside_it() {
    let images = ModelImages::new();
    let sockets = super::super::tests::socket_fixture();
    *images.serve.lock().unwrap() = Some(sockets.path().to_owned());
    let relay = greeting_relay(sockets.path(), None);
    let owner = Arc::new(Owner::new());
    let (runtime, _) = image_form(&images, &owner);
    runtime.boot().unwrap();
    *images.run.end.lock().unwrap() = Some(RunEnd::Exited(0));
    owner.refuse_ended.store(true, Ordering::SeqCst);
    let asked = runtime.boot();
    assert!(
        matches!(
            &asked,
            Err(Refusal::Os {
                op: "writing a node VM's row",
                ..
            })
        ),
        "{asked:?}"
    );
    assert_eq!(
        images.asked.lock().unwrap().len(),
        1,
        "a second node VM was booted beside the first"
    );
    let phases: Vec<NodeVmPhase> = owner
        .node_vms
        .lock()
        .unwrap()
        .values()
        .map(|row| row.phase)
        .collect();
    assert_eq!(phases, [NodeVmPhase::Ready]);
    drop(runtime);
    assert!(relay.join().unwrap());
}

/// A node's loop with no room for one more connection: it refuses every guest connection's join.
struct FullLoop;

impl crate::guest::Joins for FullLoop {
    fn reserve(&self) -> Result<(u64, crate::guest::Room), Refusal> {
        Err(Refusal::Exhausted {
            what: "the node loop's connections",
            bound: 0,
        })
    }
    fn join(&self, _: u64, _: hex_client::guest::Pump) -> Result<(), Refusal> {
        Err(Refusal::Exhausted {
            what: "the node loop's connections",
            bound: 0,
        })
    }
}

/// Joint plan step 39 (AC-T7.4.1; ROOT, 2026-11-13): do boot a node's guest under shards-init whose connection its
/// node's loop can't join, though its node greeted it. Expect: the boot refused as the loop refused it, and its node
/// VM's run stopped first: its record Ended with its exit, so no run is left beside the next boot's.
#[test]
fn w39_a_guest_under_shards_init_its_nodes_loop_cant_join_has_its_node_vm_stopped() {
    let images = ModelImages::new();
    let sockets = super::super::tests::socket_fixture();
    *images.serve.lock().unwrap() = Some(sockets.path().to_owned());
    let relay = greeting_relay(sockets.path(), None);
    let owner = Arc::new(Owner::new());
    let (runtime, _) = image_form(&images, &owner);
    runtime.join_loop(Arc::new(FullLoop)).unwrap();
    let refused = runtime.boot().unwrap_err();
    assert!(
        matches!(
            refused,
            Refusal::Exhausted {
                what: "the node loop's connections",
                ..
            }
        ),
        "{refused:?}"
    );
    let records = owner.node_vms.lock().unwrap().clone();
    assert_eq!(records.len(), 1);
    let record = records.into_values().next().unwrap();
    assert_eq!(
        (record.phase, record.exit.is_some()),
        (NodeVmPhase::Ended, true),
        "{record:?}"
    );
    drop(runtime);
    assert!(relay.join().unwrap());
}

/// The guest node's own pool as its relay answers it: two CPUs and 4 GiB, all of them free.
fn guest_pool() -> Capacity {
    let room = CapacityResources {
        cpu_milli: 2_000,
        mem_bytes: 4 << 30,
        disk_bytes: 0,
    };
    Capacity {
        id: "guest-node".to_owned(),
        total: room,
        free: room,
        devices: Vec::new(),
        cost: None,
        routes: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        unavailable: Vec::new(),
        learned: None,
        own: Default::default(),
        node_vms: Vec::new(),
        unlearned: None,
    }
}

/// A node's host's own pool, as its Store answers it: 8 CPUs and a GiB of memory and of disk, half of each free.
fn host_pool() -> Capacity {
    Capacity {
        own: Default::default(),
        id: "image-host".into(),
        total: CapacityResources {
            cpu_milli: 8_000,
            mem_bytes: 1 << 30,
            disk_bytes: 1 << 30,
        },
        free: CapacityResources {
            cpu_milli: 4_000,
            mem_bytes: 1 << 29,
            disk_bytes: 1 << 29,
        },
        devices: Vec::new(),
        cost: None,
        routes: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        unavailable: Vec::new(),
        learned: None,
        node_vms: Vec::new(),
        unlearned: None,
    }
}

/// The route an oci box takes into its node's node VM (V2): a Linux image of its host's architecture, no GPU.
fn node_vm_route() -> hex_api::RouteOffer {
    hex_api::RouteOffer {
        key: hex_api::RouteKey {
            class: hex_api::Class::Oci,
            image: hex_api::ImagePlatform {
                os: hex_api::ImageOs::Linux,
                arch: super::super::route::host_platform().unwrap().arch,
            },
            execution: hex_api::Execution::NodeVm,
            gpu: hex_api::GpuEligibility::None,
        },
        cost: None,
        backing: None,
    }
}

/// The receipt the node VM `owner` keeps the one record of answers (joint plan step 39, V2): its run, what its
/// allocation books, Ready, and what its guest is admitted at.
fn node_vm_receipt(owner: &Owner) -> hex_api::NodeVmReceipt {
    let record = owner
        .node_vms
        .lock()
        .unwrap()
        .values()
        .next()
        .cloned()
        .unwrap();
    hex_api::NodeVmReceipt {
        run: record.run,
        booking: record.allocation.booking(),
        phase: NodeVmPhase::Ready,
        guest: record.allocation.guest().unwrap(),
    }
}

/// V2, macOS's defect (ROOT, 2026-11-13: in the image form the oci answer was the node VM guest's own pool, so the
/// scheduler took the guest's plane for its whole node's, its host boxes held to the guest's room): a node whose guest
/// is a node VM under shards-init answers its oci class on its host's plane, as a node whose guest is shared does, its
/// one route its node VM's, and carries the node VM's receipt with what its guest is admitted at (joint plan step 39).
/// Do ask the oci class's capacity of such a node, its host reporting no oci pool of its own (macOS's). Expect the
/// host's pool, its id and its room; one route, NodeVm, for a Linux image of the host's architecture and no GPU; and
/// the node VM's receipt as its record keeps it.
#[test]
fn v2_the_image_forms_oci_answer_is_its_hosts_pool_routed_through_its_node_vm() {
    let images = ModelImages::new();
    let sockets = super::super::tests::socket_fixture();
    *images.serve.lock().unwrap() = Some(sockets.path().to_owned());
    let relay = greeting_relay(sockets.path(), Some(guest_pool()));
    let owner = Arc::new(Owner::new());
    let (runtime, _) = image_form(&images, &owner);
    let capacity = runtime.capacity(hex_api::Class::Oci).unwrap().unwrap();
    let host = host_pool();
    assert_eq!(
        (capacity.id.as_str(), capacity.total, capacity.free),
        (host.id.as_str(), host.total, host.free)
    );
    assert_eq!(capacity.routes, [node_vm_route()]);
    assert_eq!(capacity.node_vms, [node_vm_receipt(&owner)]);
    capacity.check().unwrap();
    drop(runtime);
    assert!(relay.join().unwrap());
}

/// V2, Windows' defect (ROOT, 2026-11-13: where its host runs oci images of its own, the node answered its host's native
/// oci pool and left its node VM out, so the scheduler never saw it): a node whose host has an oci pool of its own and
/// whose guest is a node VM answers its oci class with its host's routes and its node VM's beside them, and the node
/// VM's receipt. Do ask the oci class's capacity of such a node, its host reporting a native Windows oci route. Expect
/// the host's pool, its native routes first and then the NodeVm route, and the node VM's receipt.
#[test]
fn v2_a_host_with_an_oci_pool_of_its_own_answers_its_node_vm_beside_it() {
    let images = ModelImages::new();
    let sockets = super::super::tests::socket_fixture();
    *images.serve.lock().unwrap() = Some(sockets.path().to_owned());
    let relay = greeting_relay(sockets.path(), Some(guest_pool()));
    let owner = Arc::new(Owner::new());
    let (runtime, _) = image_form(&images, &owner);
    runtime.boot().unwrap();
    let native_route = hex_api::RouteOffer {
        key: hex_api::RouteKey {
            class: hex_api::Class::Oci,
            image: hex_api::ImagePlatform {
                os: hex_api::ImageOs::Windows,
                arch: super::super::route::host_platform().unwrap().arch,
            },
            execution: hex_api::Execution::Native,
            gpu: hex_api::GpuEligibility::None,
        },
        cost: None,
        backing: None,
    };
    let native = Capacity {
        routes: vec![native_route],
        ..host_pool()
    };
    let node = super::super::WithGuest::configured(
        super::super::tests::native_reporting(native.try_clone().unwrap()),
        runtime,
        |_host, _class| Ok(super::super::Side::Host),
    );
    let capacity = node.capacity(hex_api::Class::Oci).unwrap().unwrap();
    assert_eq!(
        (capacity.id.as_str(), capacity.total),
        (native.id.as_str(), native.total)
    );
    assert_eq!(capacity.routes, [native_route, node_vm_route()]);
    assert_eq!(capacity.node_vms, [node_vm_receipt(&owner)]);
    capacity.check().unwrap();
    drop(node);
    assert!(relay.join().unwrap());
}

/// Joint plan step 33's sign-ins (Shards' D242, step 8; docs/design/credential-grants.md C8), red first: do create and
/// start a harness VM whose CLI signs in, on a node whose orchestrator gave it lessors. Expect the boot asked exactly the
/// box's sign-in, run-wide as Shards keeps it (its name, its CLI, its hosts, an earlier capture's placeholders), beside a
/// lessor that answers it; and the record and inspect keeping the grant.
#[test]
fn s33_a_harness_vms_sign_in_reaches_its_boot_beside_its_lessor() {
    use hex_api::lease::sim::SimLessors;
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let runtime = runtime.with_lessors(Arc::new(SimLessors::new()));
    let (mut spec, _) = credentialed(&images, "image-sign-in");
    let captured = hex_api::Captured {
        access: "kiln/sign-ins/claude/access".to_owned(),
        access_placeholder: "kiln-placeholder-61636365737321".to_owned(),
        refresh_placeholder: Some("kiln-placeholder-7265667265736821".to_owned()),
    };
    let sign_in = hex_api::SignInGrant {
        member: hex_api::Member {
            kind: hex_api::MemberKind::Harness,
            name: "claude".to_owned(),
        },
        sign_in: "claude/ada".to_owned(),
        cli: hex_api::SignInCli::ClaudeCode,
        toward: vec!["api.anthropic.com".to_owned()],
        captured: Some(captured),
    };
    if let ClassSpec::ShardsImage(image) = &mut spec.class {
        image.credentials.clear();
        image.sign_ins = vec![sign_in.clone()];
    }
    let id = runtime.create(spec).unwrap().id;
    runtime.start(&id).unwrap();
    let asked = images.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        asked[0].sign_ins,
        vec![shards_api::image::SignInGrant {
            sign_in: "claude/ada".to_owned(),
            cli: shards_api::image::Cli::ClaudeCode,
            toward: vec!["api.anthropic.com".to_owned()],
            captured: Some(shards_api::image::Captured {
                access: "kiln/sign-ins/claude/access".to_owned(),
                access_placeholder: "kiln-placeholder-61636365737321".to_owned(),
                refresh_placeholder: Some("kiln-placeholder-7265667265736821".to_owned()),
            }),
        }]
    );
    assert!(
        asked[0].lessor.is_some(),
        "a run with a sign-in has a lessor"
    );
    match &record(&owner, &id).unwrap().spec.class {
        ClassSpec::ShardsImage(image) => assert_eq!(image.sign_ins, vec![sign_in]),
        other => panic!("a Shards image VM recorded as {other:?}"),
    }
    runtime.delete(&id).unwrap();
}

/// The stop under way of box `id` as this process's runs hold it, where its run is stopping: its slot, the answer its
/// stop's thread gives and whether that thread has gone.
fn stopping(runtime: &GuestRuntime, id: &BoxId) -> Option<Arc<Slot<RunEnd>>> {
    match runtime
        .images
        .image()
        .unwrap()
        .runs
        .lock()
        .unwrap()
        .get(Key::Box(id))
    {
        Some(Run::Stopping(slot, _)) => Some(Arc::clone(slot)),
        _ => None,
    }
}

/// The facts the waits on a slot's answer are tested by, each its own (hex_testkit::waits): a delete's intent held at
/// its write, a stop's answer taken by an inspect (the box shown Empty), a stop's thread gone with no answer, a boot
/// asked.
static B1_DELETE_HELD: Patience = Patience::measuring("test.hex_runtime.b1_delete_held");
static B1_STOP_TAKEN: Patience = Patience::measuring("test.hex_runtime.b1_stop_taken");
static B1_STOP_GONE: Patience = Patience::measuring("test.hex_runtime.b1_stop_gone");
static B1_BOOT_ASKED: Patience = Patience::measuring("test.hex_runtime.b1_boot_asked");

/// B-1 (Hex audit, 2026-11-14; decision 9; joint plan step 12; AC-T7.4.1): a call's wait for the answer of a stop's
/// thread follows that thread, as a wait for its party does: an answer given is seen given though another call has
/// taken it since. Do: end a running VM's run on its own, its stop held, so an inspect reaps it through its stop; delete
/// the VM, its intent's write held once written; let the stop answer, and an inspect take its answer (the box Empty), as
/// the charge learner's window, a list or a `started` does, its time the stop's W*; then let the delete go on. Expect:
/// the delete answers once the box is gone, never Pending, and its wait for the stop tells no run-out, W* the stop's
/// own answer as the inspect took it; one stop.
#[test]
fn b1_a_deletes_wait_for_a_stop_whose_answer_another_call_took_sees_it_given() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let command = Some(vec!["exit".into(), "3".into()]);
    let id = runtime.create(spec("image-b1-taken", command)).unwrap().id;
    runtime.start(&id).unwrap();
    images.run.hold_stops(true);
    *images.run.end.lock().unwrap() = Some(RunEnd::Exited(3));
    assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Running);
    assert!(
        stopping(&runtime, &id).is_some(),
        "the inspect reaped no run"
    );
    owner.deleting.ask();
    let (answered, taught) = std::thread::scope(|scope| {
        let deleting = scope.spawn(|| runtime.delete(&id));
        follow::until(
            &B1_DELETE_HELD,
            "the delete's intent held",
            || spawned(&deleting),
            || deleting.is_finished() || owner.deleting.holds(),
        )
        .unwrap();
        images.run.hold_stops(false);
        shown(&runtime, &id, BoxState::Empty, &B1_STOP_TAKEN);
        let taught = image_waits(&runtime).stop.deadline();
        owner.deleting.let_go();
        (deleting.join().unwrap(), taught)
    });
    assert_eq!(answered, Ok(()));
    assert!(
        taught.is_some(),
        "the inspect's take taught the stop's W* nothing"
    );
    assert_eq!(
        image_waits(&runtime).stop.deadline(),
        taught,
        "the delete told a run-out of a stop whose answer another call had taken"
    );
    assert!(record(&owner, &id).is_none());
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 1);
}

/// B-1, as above: a stop's thread that ends with no answer leaves nothing under way, so a call that finds its stop is
/// refused there, as the node's broken state in its own words, never Pending. Do: teach the stop's W* by a VM's KILL;
/// end another VM's run on its own, so an inspect reaps it through its stop, whose thread then ends with no answer;
/// KILL that VM. Expect: the KILL refused, the stop's thread named as ended with no answer; no run-out told, W* the
/// stop it was taught; the box's record kept, Ready with no end, its run's end unknown.
#[test]
fn b1_a_kill_whose_stops_thread_ended_with_no_answer_is_refused_never_pending() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let teacher = runtime.create(spec("image-b1-teacher", None)).unwrap().id;
    runtime.start(&teacher).unwrap();
    runtime.signal(&teacher, Signal::KILL).unwrap();
    let taught = image_waits(&runtime).stop.deadline();
    assert!(taught.is_some(), "the teacher's stop taught its W* nothing");
    // The model's next run starts afresh, and each stop's thread from now on ends with no answer.
    *images.run.end.lock().unwrap() = None;
    images.run.abandon_stops.store(true, Ordering::SeqCst);
    let command = Some(vec!["exit".into(), "3".into()]);
    let id = runtime
        .create(spec("image-b1-stop-gone", command))
        .unwrap()
        .id;
    runtime.start(&id).unwrap();
    *images.run.end.lock().unwrap() = Some(RunEnd::Exited(3));
    assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Running);
    let slot = stopping(&runtime, &id).unwrap();
    follow::until(
        &B1_STOP_GONE,
        "the stop's thread gone",
        || kept(&owner, &id),
        || slot.gone.load(Ordering::Acquire),
    )
    .unwrap();
    let killed = runtime.signal(&id, Signal::KILL);
    assert!(
        matches!(&killed, Err(Refusal::Broken(why)) if why.contains("its stop's thread ended with no answer")),
        "{killed:?}"
    );
    assert_eq!(
        image_waits(&runtime).stop.deadline(),
        taught,
        "a run-out told of a stop whose thread had ended"
    );
    let kept = record(&owner, &id).unwrap();
    assert_eq!((kept.phase, kept.exit), (Phase::Ready, None));
}

/// B-1, a start's own wait: before the boot's first answer it follows the boot's thread (decision 9's cold start), so
/// a boot whose thread ends with no answer refuses its start there, as the node's broken state in its own words. Do:
/// start a VM with no boot answer seen, its boot held before its run host's start; let its boot's thread end with no
/// answer. Expect: the start refused, its boot's thread named as ended with no answer; the box's record kept, Ready
/// with no end, neither its run's start nor its run host's end known.
#[test]
fn b1_a_cold_start_whose_boots_thread_ends_with_no_answer_is_refused() {
    let images = ModelImages::new();
    images.gate(Gate::Host);
    let (runtime, owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-b1-boot-gone", None)).unwrap().id;
    images.abandon.store(true, Ordering::SeqCst);
    let started = std::thread::scope(|scope| {
        let starting = scope.spawn(|| runtime.start(&id));
        follow::until(
            &B1_BOOT_ASKED,
            "the boot asked",
            || spawned(&starting),
            || starting.is_finished() || !images.asked.lock().unwrap().is_empty(),
        )
        .unwrap();
        images.gate(Gate::Open);
        starting.join().unwrap()
    });
    assert!(
        matches!(&started, Err(Refusal::Broken(why)) if why.contains("its boot's thread ended with no answer")),
        "{started:?}"
    );
    let kept = record(&owner, &id).unwrap();
    assert_eq!((kept.phase, kept.exit), (Phase::Ready, None));
}

/// C-3 (Hex audit, 2026-11-14; decision 9; joint plan step 39; AC-T7.4.1): a node VM's stop follows its stop's thread,
/// as B-1's waits do, so one whose thread ends with no answer is refused there, never waited for forever (its node's
/// guest runtime stops its node VM under its boot lock, and as it drops). Do: boot a node VM with no stop answer seen;
/// stop it, its stop's thread ending with no answer. Expect: the stop refused, its stop's thread named as ended with no
/// answer; its record kept, Ready with no end, its run's end unknown.
#[test]
fn c3_a_node_vms_cold_stop_whose_thread_ends_with_no_answer_is_refused() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let vm = node_vm();
    runtime.images.boot_node(&vm).unwrap();
    images.run.abandon_stops.store(true, Ordering::SeqCst);
    let stopped = runtime.images.stop_node(vm.run);
    assert!(
        matches!(&stopped, Err(Refusal::Broken(why)) if why.contains("its stop's thread ended with no answer")),
        "{stopped:?}"
    );
    let row = owner
        .node_vms
        .lock()
        .unwrap()
        .get(&vm.run)
        .cloned()
        .unwrap();
    assert_eq!((row.phase, row.exit), (NodeVmPhase::Ready, None));
}

/// V2 (ROOT, 2026-11-13: when the inner node refuses what the host-side pool admitted, tell it, with both numbers): a
/// node VM's guest refusing room for a box its host's pool admitted is told with the pool's numbers beside the guest's
/// own, never as the guest's words alone. Do tell a guest's Full and Exhausted refusals, its pool holding 1.5 CPUs and
/// 3 GiB, 2 CPUs and 4 GiB booked with the box, the guest saying 1 CPU and 2 GiB free; then one of a guest whose free
/// room can't be read. Expect each refusal typed Invalid, naming the pool's room, what it booked and the guest's free
/// room, and the guest's own words; and the unread room said so.
#[test]
fn v2_a_node_vms_guest_refusing_what_its_hosts_pool_admitted_is_told_with_both_numbers() {
    let gib = 1u64 << 30;
    let pool = CapacityResources {
        cpu_milli: 1_500,
        mem_bytes: 3 * gib,
        disk_bytes: 0,
    };
    let booked = CapacityResources {
        cpu_milli: 2_000,
        mem_bytes: 4 * gib,
        disk_bytes: 0,
    };
    let free = CapacityResources {
        cpu_milli: 1_000,
        mem_bytes: 2 * gib,
        disk_bytes: 0,
    };
    for refusal in [
        Refusal::Full(7),
        Refusal::Exhausted {
            what: "boxes",
            bound: 7,
        },
    ] {
        let words = refusal.to_string();
        let told = GuestRuntime::guest_room_refusal(refusal, pool, booked, Some(free));
        let Refusal::Invalid { why, .. } = &told else {
            panic!("{told:?}");
        };
        for said in [
            "1500".to_owned(),
            (3 * gib).to_string(),
            "2000".to_owned(),
            (4 * gib).to_string(),
            "1000".to_owned(),
            (2 * gib).to_string(),
            words,
        ] {
            assert!(why.contains(&said), "{said} in {why}");
        }
    }
    let told = GuestRuntime::guest_room_refusal(Refusal::Full(7), pool, booked, None);
    assert!(
        matches!(&told, Refusal::Invalid { why, .. } if why.contains("unread")),
        "{told:?}"
    );
}

/// V2 (ROOT, 2026-11-13: on macOS, process boxes got no route once any class reported routes, its process class
/// answering no pool of its own): a node whose host's process class answers no pool of its own answers it on its host's
/// plane, the oci answer's, with its native process route, so its process boxes stay placeable beside its node VM's
/// route. Do ask the process class's capacity of a node whose process side answers none and whose guest is a node VM.
/// Expect the host's pool, the same plane its oci answer is on, its one route a native process route for its host's
/// platform with no GPU.
#[test]
fn v2_a_host_whose_process_class_answers_no_pool_answers_its_hosts_plane_with_its_process_route() {
    let images = ModelImages::new();
    let sockets = super::super::tests::socket_fixture();
    *images.serve.lock().unwrap() = Some(sockets.path().to_owned());
    let relay = greeting_relay(sockets.path(), Some(guest_pool()));
    let owner = Arc::new(Owner::new());
    let (runtime, _) = image_form(&images, &owner);
    runtime.boot().unwrap();
    let node = super::super::WithGuest::configured(
        hex_api::sim::SimRuntime::new(),
        runtime,
        |_host, _class| Ok(super::super::Side::Host),
    );
    let process = node.capacity(hex_api::Class::Process).unwrap().unwrap();
    let oci = node.capacity(hex_api::Class::Oci).unwrap().unwrap();
    assert_eq!(
        (process.id.as_str(), process.total, process.free),
        (oci.id.as_str(), oci.total, oci.free)
    );
    assert_eq!(
        process.routes,
        [hex_api::RouteOffer {
            key: hex_api::RouteKey {
                class: hex_api::Class::Process,
                image: super::super::route::host_platform().unwrap(),
                execution: hex_api::Execution::Native,
                gpu: hex_api::GpuEligibility::None,
            },
            cost: None,
            backing: None,
        }]
    );
    process.check().unwrap();
    drop(node);
    assert!(relay.join().unwrap());
}

/// What one of the review's B-3, B-5 and B-6 tests waits for: the boot it held, answered once let go.
static B3_HELD_BOOT_ANSWERED: Patience =
    Patience::measuring("test.hex_runtime.b3_held_boot_answered");

/// A run's own words where its command never started, as Shards' run host says them (D249).
const NEVER_STARTED: &str =
    "its command never started (status 128): exec: \"/bin/sh\": operation not permitted";

/// The review's B-3 (D249; kiln's HexVms adapter, ROOT 2026-11-13): `started` answers a run that failed as it booted, its
/// command never started, as `start` answers it, `Refusal::Failed` in its runtime's words, never the box Running. Do:
/// teach the boot's wait its W* by a first VM's boot, answered at once (its cold start); hold a second VM's boot past
/// it, its start Pending; have its run fail as it boots; let the boot answer, then ask `started`, the call that takes
/// the boot's answer. Expect: `Failed`, in those words.
#[test]
fn b3_started_over_a_run_that_failed_as_it_booted_answers_its_failure() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let first = runtime.create(spec("b3-first", None)).unwrap().id;
    runtime.start(&first).unwrap();
    runtime.delete(&first).unwrap();
    images.gate(Gate::Host);
    let command = vec!["/bin/sh".into(), "-c".into(), "exit 3".into()];
    let id = runtime
        .create(spec("b3-failed-as-it-booted", Some(command)))
        .unwrap()
        .id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Lifecycle {
            cause: LifecycleCause::Pending,
            ..
        })
    ));
    *images.run.end.lock().unwrap() = Some(RunEnd::Failed(NEVER_STARTED.to_owned()));
    images.gate(Gate::Open);
    follow::until(
        &B3_HELD_BOOT_ANSWERED,
        "the held boot answered",
        || kept(&owner, &id),
        || images.answered.load(Ordering::SeqCst) == 2,
    )
    .unwrap();
    let started = runtime.started(&id);
    assert!(
        matches!(&started, Err(Refusal::Failed { why, .. }) if why == NEVER_STARTED),
        "started answered {:?} for a run whose command never started",
        started.map(|info| info.state)
    );
}

/// The review's B-5 (docs/design/shards-image-class.md §3; goal 54's change 3): a delete answering its VM's final use
/// answers none only where its run never started, so kiln charges nothing. A VM whose command ran and ended before its
/// delete, its VM telling no final use with that end (D260's `Unused::Untold`), has no use left to read (Shards' stats
/// answer only a running run), so its delete is refused, typed, the box kept as it was, never answered none. Do: start a
/// VM, read its use as it runs, have its command exit; ask its delete with its host use; then delete it. Expect:
/// `NotRunning`, naming its final use, its record kept; then the plain delete removing it.
#[test]
fn b5_a_delete_with_host_use_of_a_vm_whose_command_ended_first_is_refused_and_keeps_the_box() {
    let vcpus = allocation().vm_size().unwrap().vcpus;
    let images = ModelImages::new();
    *images.run.stats.lock().unwrap() = Stats {
        microvm: microvm(Ok(guest_times(vcpus))),
        ..Stats::default()
    };
    let (runtime, owner) = model_runtime(&images, waits());
    let command = vec!["/bin/sh".into(), "-c".into(), "exit 0".into()];
    let id = runtime
        .create(spec("b5-command-ended-first", Some(command)))
        .unwrap()
        .id;
    runtime.start(&id).unwrap();
    runtime.host_use(&id).unwrap();
    *images.run.end.lock().unwrap() = Some(RunEnd::Exited(0));
    let refused = runtime.delete_with_host_use(&id);
    assert!(
        matches!(&refused, Err(Refusal::NotRunning(named, why)) if *named == id && why.contains("ended on its own")),
        "{refused:?}"
    );
    assert!(
        record(&owner, &id).is_some(),
        "the refused delete removed the box"
    );
    runtime.delete(&id).unwrap();
    assert!(record(&owner, &id).is_none());
}

/// Kiln's P7.1b ask (lane replay, kiln 107a4da17; Shards' D260; AC-T7.4.1): a run that ends on its own keeps the final
/// use its VM tells with that end, so its owner needn't hold it running to be charged what it used. Do: a VM whose
/// command ends on its own, its VM telling its final use; see it ended; read its record; delete it with its host use.
/// Expect: its record keeping its exit and that use (its VM's counters, each vCPU's guest time by index, its window no
/// longer than the run took to be seen ended); the delete answering that use and removing the box.
#[test]
fn p7_a_run_that_ends_on_its_own_keeps_its_final_use_for_its_delete() {
    let vcpus = allocation().vm_size().unwrap().vcpus;
    let images = ModelImages::new();
    *images.run.final_use.lock().unwrap() = microvm(Ok(guest_times(vcpus)));
    let (runtime, owner) = model_runtime(&images, waits());
    let command = vec!["/bin/sh".into(), "/kiln-replay/replay.sh".into()];
    let id = runtime
        .create(spec("p7-final-use", Some(command)))
        .unwrap()
        .id;
    let before = Instant::now();
    runtime.start(&id).unwrap();
    *images.run.end.lock().unwrap() = Some(RunEnd::Exited(0));
    shown(&runtime, &id, BoxState::Empty, &REAPED_SHOWN);
    let seen = before.elapsed();
    let kept = record(&owner, &id).unwrap();
    assert_eq!(kept.exit, Some(Exit::Code(0)));
    let used = kept.final_use.unwrap();
    assert_eq!(
        (used.cpu_ns, used.footprint_bytes, used.resident_bytes),
        (9_000_000, 1 << 20, 1 << 20)
    );
    assert_eq!(
        used.guest_ns,
        (1..=u64::from(vcpus))
            .map(|n| 1_000 * n)
            .collect::<Vec<_>>()
    );
    assert_eq!(used.charged, hex_api::Charged::ToThread);
    assert!(
        used.window <= seen,
        "{used:?} past the {seen:?} its run took to be seen ended"
    );
    assert_eq!(runtime.delete_with_host_use(&id), Ok(Some(used)));
    assert!(record(&owner, &id).is_none());
}

/// Kiln's P7.1b (Shards' D260; the review's B-5 as it stands with a final use told; AC-T7.4.1): a delete with its host
/// use that is the first call to see its run's own end answers the use its VM told with it, where before it was
/// refused, nothing left to read. Do: start a VM whose VM tells its final use, read its use as it runs, have its
/// command exit; ask its delete with its host use. Expect: that final use, its window no shorter than the read's, and
/// the box gone.
#[test]
fn p7_a_delete_that_first_sees_its_runs_own_end_answers_its_final_use() {
    let vcpus = allocation().vm_size().unwrap().vcpus;
    let images = ModelImages::new();
    *images.run.stats.lock().unwrap() = Stats {
        microvm: microvm(Ok(guest_times(vcpus))),
        ..Stats::default()
    };
    *images.run.final_use.lock().unwrap() = microvm(Ok(guest_times(vcpus)));
    let (runtime, owner) = model_runtime(&images, waits());
    let command = vec!["/bin/sh".into(), "-c".into(), "exit 0".into()];
    let id = runtime
        .create(spec("p7-final-use-first-seen", Some(command)))
        .unwrap()
        .id;
    runtime.start(&id).unwrap();
    let read = runtime.host_use(&id).unwrap();
    *images.run.end.lock().unwrap() = Some(RunEnd::Exited(0));
    let used = runtime.delete_with_host_use(&id).unwrap().unwrap();
    assert_eq!(
        (used.cpu_ns, used.guest_ns.len()),
        (9_000_000, usize::try_from(vcpus).unwrap())
    );
    assert!(used.window >= read.window, "{read:?} then {used:?}");
    assert!(record(&owner, &id).is_none());
}

/// The review's B-5: a VM running at its delete whose final read is refused (its run host told nothing of its microVM)
/// has its delete with its host use refused in that read's words, the box kept running, never answered none. Do: start a
/// VM whose stats carry no microVM part; ask its delete with its host use; then delete it. Expect: the read's refusal, no
/// stop asked, the box running; then the plain delete removing it.
#[test]
fn b5_a_delete_with_host_use_whose_final_read_is_refused_says_so_and_keeps_the_box_running() {
    let images = ModelImages::new();
    *images.run.stats.lock().unwrap() = Stats::default();
    let (runtime, owner) = model_runtime(&images, waits());
    let id = runtime
        .create(spec("b5-final-read-refused", None))
        .unwrap()
        .id;
    runtime.start(&id).unwrap();
    let refused = runtime.delete_with_host_use(&id);
    assert!(
        matches!(
            &refused,
            Err(Refusal::Os {
                op: "reading a Shards image VM's use of its host",
                ..
            })
        ),
        "{refused:?}"
    );
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Running);
    runtime.delete(&id).unwrap();
    assert!(record(&owner, &id).is_none());
}

/// The review's B-6 (kiln's HexVms adapter; goal 54's change 3): a node's own runtime on macOS and Windows, `WithGuest`
/// over its host's and the guest's, passes `started` and `delete_with_host_use` of an image VM to the guest runtime, as
/// it passes `host_use`, never the trait's defaults (`started` an inspect, a delete answering none). Do, through
/// `WithGuest`: start a VM and ask its delete with its host use; start another whose run fails as it boots and ask
/// `started` of it. Expect: the first's final use, its record gone; the second's start its failure, or Pending where its
/// boot took longer than the first's answer, its W* (decision 9); `started` its failure in its runtime's words either
/// way, as the guest runtime answers it.
#[test]
fn b6_with_guest_passes_started_and_delete_with_host_use_to_its_guest() {
    let vcpus = allocation().vm_size().unwrap().vcpus;
    let images = ModelImages::new();
    *images.run.stats.lock().unwrap() = Stats {
        microvm: microvm(Ok(guest_times(vcpus))),
        ..Stats::default()
    };
    let (guest, owner) = model_runtime(&images, waits());
    let node = crate::guest::WithGuest::new(hex_api::sim::SimRuntime::new(), guest).unwrap();
    let id = node.create(spec("b6-with-guest-running", None)).unwrap().id;
    node.start(&id).unwrap();
    let last = node.delete_with_host_use(&id);
    assert!(
        matches!(&last, Ok(Some(used)) if used.cpu_ns == 9_000_000),
        "{last:?}"
    );
    assert!(record(&owner, &id).is_none());
    *images.run.end.lock().unwrap() = Some(RunEnd::Failed(NEVER_STARTED.to_owned()));
    let command = vec!["/bin/sh".into(), "-c".into(), "exit 3".into()];
    let failed = node
        .create(spec("b6-with-guest-failed", Some(command)))
        .unwrap()
        .id;
    assert!(matches!(
        node.start(&failed),
        Err(Refusal::Failed { .. }
            | Refusal::Lifecycle {
                cause: LifecycleCause::Pending,
                ..
            })
    ));
    let started = node.started(&failed);
    assert!(
        matches!(&started, Err(Refusal::Failed { why, .. }) if why == NEVER_STARTED),
        "{:?}",
        started.map(|info| info.state)
    );
}

/// C-2 (Hex audit, 2026-11-14; joint plan step 39; AC-T7.4.1): a node VM whose boot fails once its run has started has
/// that run stopped, so none runs held by no one, its row unwritten, beside the one the node's next boot starts. Do:
/// boot a node VM whose Ready row the record owner refuses, as a full disk would. Expect: the boot refused in the owner's
/// words; its run stopped once, its end `Stopped`; no run of it held here; its row Ended with its stop's exit.
#[test]
fn c2_a_node_vm_whose_ready_row_is_refused_has_its_run_stopped_and_its_row_ended() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    owner.refuse_ready.store(true, Ordering::SeqCst);
    let vm = node_vm();
    let refused = runtime.images.boot_node(&vm).unwrap_err();
    assert!(
        matches!(&refused, Refusal::Os { why, .. } if why.contains("no space left on the device")),
        "{refused:?}"
    );
    assert_eq!(
        images.run.stops.load(Ordering::SeqCst),
        1,
        "its run wasn't stopped"
    );
    assert_eq!(*images.run.end.lock().unwrap(), Some(RunEnd::Stopped));
    assert!(
        !runtime.images.node_runs(vm.run).unwrap(),
        "its run is still held"
    );
    let row = owner
        .node_vms
        .lock()
        .unwrap()
        .get(&vm.run)
        .cloned()
        .unwrap();
    assert_eq!(
        (row.phase, row.exit),
        (
            NodeVmPhase::Ended,
            Some(Exit::Signal(Signal::KILL.number()))
        )
    );
}

/// C-2, before its boot: a node VM whose run this process can't hold, its runs at their bound (`MAX_BOXES`, the most VMs
/// a node holds), is refused with its row Ended, never left Booting and booked for as long as its node runs. Do: fill
/// this process's runs with ended ones, then boot a node VM. Expect: the boot refused Full at that bound, no boot asked,
/// its row Ended, its exit unknown.
#[test]
fn c2_a_node_vm_whose_run_cant_be_held_is_refused_with_its_row_ended() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let runs = &runtime.images.image().unwrap().runs;
    let vm = node_vm();
    for n in 0..crate::MAX_BOXES {
        let mut run = [0xff; 16];
        run[..8].copy_from_slice(&u64::try_from(n).unwrap().to_le_bytes());
        assert_ne!(NodeRun::from_bytes(run), vm.run);
        runs.insert(NodeRun::from_bytes(run), Run::Ended(End::unknown(None)))
            .unwrap();
    }
    assert_eq!(
        runtime.images.boot_node(&vm),
        Err(Refusal::Full(crate::MAX_BOXES))
    );
    assert!(images.asked.lock().unwrap().is_empty(), "a boot was asked");
    let row = owner
        .node_vms
        .lock()
        .unwrap()
        .get(&vm.run)
        .cloned()
        .unwrap();
    assert_eq!(
        (row.phase, row.exit),
        (NodeVmPhase::Ended, Some(Exit::Unknown))
    );
}

/// Whether box `id`'s boot under way, as this process's runs hold it, has been asked to stop (D231's `BootStop`).
fn boot_stopped(runtime: &GuestRuntime, id: &BoxId) -> bool {
    matches!(
        runtime
            .images
            .image()
            .unwrap()
            .runs
            .lock()
            .unwrap()
            .get(Key::Box(id)),
        Some(Run::Booting(_, stop)) if stop.is_stopped()
    )
}

/// The facts B-2's tests wait on, each its own (hex_testkit::waits): a call's scan of the records held once read, the
/// run's end recorded by another call (the box shown Empty), a delete's intent written, a boot asked and its stop
/// asked, and a delete's scan and a start's write of its run's end both held.
static B2_INSPECT_HELD: Patience = Patience::measuring("test.hex_runtime.b2_inspect_held");
static B2_INSPECT_ENDED: Patience = Patience::measuring("test.hex_runtime.b2_inspect_ended");
static B2_STOP_INTENT: Patience = Patience::measuring("test.hex_runtime.b2_stop_intent");
static B2_STOP_HELD: Patience = Patience::measuring("test.hex_runtime.b2_stop_held");
static B2_STOP_ENDED: Patience = Patience::measuring("test.hex_runtime.b2_stop_ended");
static B2_BOOT_ASKED: Patience = Patience::measuring("test.hex_runtime.b2_boot_asked");
static B2_BOOT_STOP: Patience = Patience::measuring("test.hex_runtime.b2_boot_stop");
static B2_BOOT_HELD: Patience = Patience::measuring("test.hex_runtime.b2_boot_held");

/// B-2 (Hex audit, 2026-11-14; joint plan step 12; AC-T7.4.1): a box this process ran is never taken for an earlier
/// node process's, held, because its call read the record before another call recorded the run's end and let go of
/// the run. Do: end a running VM's run on its own, its stop held, so an inspect reaps it through its stop; inspect it
/// again, that call held once it has read the records; let the stop answer, and a third inspect record the end (the box
/// Empty); then let the held inspect go on. Expect: it answers the box Empty with its run's exit, as recorded, never
/// Awaits(AttachAfterCrash).
#[test]
fn b2_an_inspect_whose_read_another_call_outdated_answers_the_recorded_end() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let command = Some(vec!["exit".into(), "3".into()]);
    let id = runtime
        .create(spec("image-b2-inspect", command))
        .unwrap()
        .id;
    runtime.start(&id).unwrap();
    images.run.hold_stops(true);
    *images.run.end.lock().unwrap() = Some(RunEnd::Exited(3));
    assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Running);
    assert!(
        stopping(&runtime, &id).is_some(),
        "the inspect reaped no run"
    );
    owner.reading.ask();
    let late = std::thread::scope(|scope| {
        let inspecting = scope.spawn(|| runtime.inspect(&id));
        follow::until(
            &B2_INSPECT_HELD,
            "the inspect's read held",
            || spawned(&inspecting),
            || inspecting.is_finished() || owner.reading.holds(),
        )
        .unwrap();
        images.run.hold_stops(false);
        shown(&runtime, &id, BoxState::Empty, &B2_INSPECT_ENDED);
        owner.reading.let_go();
        inspecting.join().unwrap()
    });
    assert_eq!(
        late.map(|info| (info.state, info.exit)),
        Ok((BoxState::Empty, Some(Exit::Code(3))))
    );
}

/// B-2, kiln's choice (b): a delete answers once the box is gone, never refused Uncertain because its read of the
/// record after its stop answered was outdated by another call that recorded the run's end and let go of the run. Do:
/// end a running VM's run on its own, its stop held, so an inspect reaps it; delete it, and once its intent is written,
/// hold its next scan of the records once read; let the stop answer, so the delete reads, and an inspect record the end
/// (the box Empty); then let the delete go on. Expect: the delete answers Ok, the box gone.
#[test]
fn b2_a_delete_whose_read_another_call_outdated_answers_once_the_box_is_gone() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let command = Some(vec!["exit".into(), "3".into()]);
    let id = runtime
        .create(spec("image-b2-stopping", command))
        .unwrap()
        .id;
    runtime.start(&id).unwrap();
    images.run.hold_stops(true);
    *images.run.end.lock().unwrap() = Some(RunEnd::Exited(3));
    assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Running);
    let answered = std::thread::scope(|scope| {
        let deleting = scope.spawn(|| runtime.delete(&id));
        follow::until(
            &B2_STOP_INTENT,
            "the delete's intent written",
            || spawned(&deleting),
            || {
                deleting.is_finished()
                    || record(&owner, &id).is_some_and(|r| r.phase == Phase::Deleting)
            },
        )
        .unwrap();
        owner.reading.ask();
        images.run.hold_stops(false);
        follow::until(
            &B2_STOP_HELD,
            "the delete's read held",
            || spawned(&deleting),
            || deleting.is_finished() || owner.reading.holds(),
        )
        .unwrap();
        shown(&runtime, &id, BoxState::Empty, &B2_STOP_ENDED);
        owner.reading.let_go();
        deleting.join().unwrap()
    });
    assert_eq!(answered, Ok(()));
    assert!(record(&owner, &id).is_none());
}

/// B-2, as the review found it: a delete of a booting box answers once the box is gone, never refused Uncertain because
/// its read of the record was outdated by the box's start, which took the stopped boot's answer, recorded the run's end
/// and let go of the run. Do: start a VM with no boot answer seen, its boot held before its run host's start; delete
/// it, and once it has asked the boot to stop, hold the next scan of the records once read and the next write of a
/// run's end before it is written; let the boot see its stop, so the delete reads the records and the start writes the
/// end; let the end's write go, then the delete. Expect: one boot stopped and the start refused; the delete answers Ok,
/// the box gone.
#[test]
fn b2_a_delete_of_a_booting_box_whose_read_its_start_outdated_answers_once_it_is_gone() {
    let images = ModelImages::new();
    images.gate(Gate::Host);
    let (runtime, owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-b2-booting", None)).unwrap().id;
    let (started, deleted) = std::thread::scope(|scope| {
        let starting = scope.spawn(|| runtime.start(&id));
        follow::until(
            &B2_BOOT_ASKED,
            "the boot asked",
            || spawned(&starting),
            || starting.is_finished() || !images.asked.lock().unwrap().is_empty(),
        )
        .unwrap();
        let deleting = scope.spawn(|| runtime.delete(&id));
        follow::until(
            &B2_BOOT_STOP,
            "the boot's stop asked",
            || spawned(&deleting),
            || deleting.is_finished() || boot_stopped(&runtime, &id),
        )
        .unwrap();
        owner.reading.ask();
        owner.ending.ask();
        images.poke();
        // Each held by the other's: the delete's read by the end the start writes, so either thread's end ends it.
        follow::until(
            &B2_BOOT_HELD,
            "the delete's read and the start's end held",
            || match (spawned(&deleting), spawned(&starting)) {
                (Party::Live, Party::Live) => Party::Live,
                _ => Party::Ended,
            },
            || {
                deleting.is_finished()
                    || starting.is_finished()
                    || (owner.reading.holds() && owner.ending.holds())
            },
        )
        .unwrap();
        owner.ending.let_go();
        let started = starting.join().unwrap();
        owner.reading.let_go();
        (started, deleting.join().unwrap())
    });
    assert_eq!(images.stopped.load(Ordering::SeqCst), 1);
    assert!(
        started.is_err(),
        "a start whose boot was stopped: {started:?}"
    );
    assert_eq!(deleted, Ok(()));
    assert!(record(&owner, &id).is_none());
}

/// The review's B-7 (AC-T7.4.1): a boot its delete stopped mid-boot (D231's `BootStop`) answers `Ended(Stopped)`, and
/// where the start that asked it takes that answer (`Runs::booted`) it is no boot's answer: the time to the stop is no
/// sample of the boot's W* (a boot hung ten minutes and then stopped would make W* ten minutes), and its run ends as the
/// stop ended it, KILL, as `Runs::settle` records the same answer. Do put a stopped boot's answer in a booting run's
/// slot and take it as its start does. Expect the stop given back, the boot's wait with no sample, and the run ended by
/// signal 9.
#[test]
fn b7_a_stopped_boot_its_start_takes_is_no_sample_and_ends_killed() {
    let runs = Runs::new();
    let id = BoxId::new("b7-stopped-boot").unwrap();
    let slot = Arc::new(Slot::<Booted>::new());
    runs.insert(
        &id,
        Run::Booting(Arc::clone(&slot), Arc::new(BootStop::new())),
    )
    .unwrap();
    slot.put(Err(ImageRefusal::Ended(RunEnded::unused(
        RunEnd::Stopped,
        Unused::NotStarted,
    ))));
    let waits = waits().unwrap();
    let taken = runs.booted(&id, &slot, &waits).unwrap();
    assert!(
        matches!(
            taken,
            Err(ImageRefusal::Ended(RunEnded {
                end: RunEnd::Stopped,
                ..
            }))
        ),
        "{taken:?}"
    );
    assert_eq!(
        waits.boot.slowest(),
        None,
        "the stop taken for a boot's answer"
    );
    match runs.lock().unwrap().get(Key::Box(&id)) {
        Some(Run::Ended(end)) => assert_eq!(end.exit, hex_api::Exit::Signal(9)),
        _ => panic!("the stopped boot's run isn't ended"),
    }
}

/// A box whose spec asks `bridged` a bridge (D254), each a member its caller drives.
fn bridged_spec(name: &str, bridged: &[&Member]) -> BoxSpec {
    let mut boxed = spec(name, None);
    if let ClassSpec::ShardsImage(image) = &mut boxed.class {
        image.members = bridged
            .iter()
            .map(|member| ImageMember {
                mcp: true,
                ..ImageMember::new((*member).clone())
            })
            .collect();
    }
    boxed
}

fn agent(name: &str) -> Member {
    Member {
        kind: MemberKind::Agent,
        name: name.to_owned(),
    }
}

/// An MCP connection's end once its output has finished: no recorded operation makes it, so measuring, for its fact
/// alone (hex_testkit::waits).
static MCP_END: Patience = Patience::measuring("test.hex_runtime.image_mcp_end");

/// `proc`'s end as an MCP connection's comes, as [`MCP_END`] waits, by the test's own run, which lives while it does.
fn mcp_end(proc: &mut Box<dyn Proc>) -> Exit {
    follow::within(
        &MCP_END,
        "an MCP connection's end",
        || Party::Live,
        |by| {
            let wait = by.map_or(Duration::MAX, |at| {
                at.saturating_duration_since(Instant::now())
            });
            proc.wait_for(wait).unwrap()
        },
    )
    .unwrap()
}

/// Goal 2's item 3 (D254; ROOT, 2026-11-14; AC-T7.4.1): a member's MCP connection is its caller's program. Do: a run
/// whose agent-control endpoint is Shards' own on loopback, its spec asking the agent `helper` a bridge; take the next
/// connection its harness makes, write a request on its stdin and end it, then read its stdout to its end. Expect: a
/// program of no host process, its stdin and stdout given once and no stderr; the request echoed back whole, then the
/// end; its end the connection's, `Exit::Code(0)`; the box Running, its VM never stopped.
#[test]
fn w2_mcp_a_members_bridge_connection_is_its_callers_program() {
    use std::io::{Read, Write};
    let images = ModelImages::new();
    *images.members.lock().unwrap() = three_members();
    *images.bridged.lock().unwrap() = Some(Vec::new());
    let (runtime, _owner) = model_runtime(&images, waits());
    let helper = agent("helper");
    let id = runtime
        .create(bridged_spec("image-mcp-connection", &[&helper]))
        .unwrap()
        .id;
    runtime.start(&id).unwrap();
    let mut proc = runtime.mcp(&id, &helper).unwrap();
    assert!(!proc.on_this_host(), "an MCP connection is its guest's");
    let given = proc.streams().unwrap();
    assert!(given.stderr.is_none(), "an MCP connection has no stderr");
    assert!(
        proc.streams().unwrap().stdin.is_none(),
        "its streams are given once"
    );
    let mut to = hex_api::OwnedStream::from(given.stdin.unwrap());
    let mut from = hex_api::OwnedStream::from(given.stdout.unwrap());
    let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n";
    to.write_all(request).unwrap();
    drop(to);
    let mut heard = Vec::new();
    from.read_to_end(&mut heard).unwrap();
    assert_eq!(
        heard, request,
        "what its harness sends, whole, then its end"
    );
    assert_eq!(mcp_end(&mut proc), Exit::Code(0));
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Running);
}

/// Goal 2's item 3 (ROOT's mapping, 2026-11-14; AC-T7.4.1): an MCP connection that can't be taken is refused, typed.
/// Do: a box whose spec asks the agents `helper`, `notes` and `ghost` a bridge, its run declaring no `ghost` and its
/// host refusing `notes`'s bridge (as Shards refuses one a run asked none of); ask for `helper`'s before its start;
/// after it, ask for the harness `claude`'s (its spec asks none), `ghost`'s and `notes`'s; then stop the VM and ask for
/// `helper`'s. Expect: `NotRunning(id, "mcp")` before the start; `Invalid` naming the spec for `claude`, the run for
/// `ghost`; `NotRunning(id, "mcp: its member takes no MCP connection")` for `notes`; and `NotRunning(id, "mcp")` once the
/// VM has stopped.
#[test]
fn w2_mcp_a_connection_that_cant_be_taken_is_refused_typed() {
    let images = ModelImages::new();
    *images.members.lock().unwrap() = three_members();
    *images.bridged.lock().unwrap() = Some(vec![2]);
    let (runtime, _owner) = model_runtime(&images, waits());
    let (helper, notes, ghost) = (agent("helper"), agent("notes"), agent("ghost"));
    let claude = Member {
        kind: MemberKind::Harness,
        name: "claude".to_owned(),
    };
    let id = runtime
        .create(bridged_spec(
            "image-mcp-refused",
            &[&helper, &notes, &ghost],
        ))
        .unwrap()
        .id;
    let refused = |member: &Member| runtime.mcp(&id, member).map(drop);
    assert_eq!(
        refused(&helper),
        Err(Refusal::NotRunning(id.clone(), "mcp"))
    );
    runtime.start(&id).unwrap();
    assert_eq!(
        refused(&claude),
        Err(Refusal::Invalid {
            what: "mcp",
            why: format!("box {id}'s spec asks no MCP bridge of harness claude"),
        })
    );
    assert_eq!(
        refused(&ghost),
        Err(Refusal::Invalid {
            what: "member",
            why: format!("box {id}'s run declares no agent ghost"),
        })
    );
    assert_eq!(
        refused(&notes),
        Err(Refusal::NotRunning(
            id.clone(),
            "mcp: its member takes no MCP connection"
        ))
    );
    runtime.signal(&id, Signal::KILL).unwrap();
    assert_eq!(
        refused(&helper),
        Err(Refusal::NotRunning(id.clone(), "mcp"))
    );
}

/// Goal 2's item 3 (ROOT's mapping, 2026-11-14; AC-T7.4.1): a member's MCP connection's kill resets it. Do: take
/// `helper`'s next connection, write a request and read its echo, then kill it and read its stdout to its end. Expect:
/// the echo; the kill done; its stdout's end; its end `Exit::Signal(9)`, the box Running, its VM never stopped; and its
/// kill again, once it has ended, done at once.
#[test]
fn w2_mcp_a_connections_kill_resets_it_and_its_end_reads_killed() {
    use std::io::{Read, Write};
    let images = ModelImages::new();
    *images.members.lock().unwrap() = three_members();
    *images.bridged.lock().unwrap() = Some(Vec::new());
    let (runtime, _owner) = model_runtime(&images, waits());
    let helper = agent("helper");
    let id = runtime
        .create(bridged_spec("image-mcp-kill", &[&helper]))
        .unwrap()
        .id;
    runtime.start(&id).unwrap();
    let mut proc = runtime.mcp(&id, &helper).unwrap();
    let given = proc.streams().unwrap();
    let mut to = hex_api::OwnedStream::from(given.stdin.unwrap());
    let mut from = hex_api::OwnedStream::from(given.stdout.unwrap());
    to.write_all(b"ping\n").unwrap();
    let mut echo = [0u8; 5];
    from.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"ping\n");
    assert_eq!(proc.kill(), Ok(()));
    let mut rest = Vec::new();
    from.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty(), "{rest:?}");
    assert_eq!(mcp_end(&mut proc), Exit::Signal(9));
    assert_eq!(proc.kill(), Ok(()));
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Running);
}

/// A program's end in a member once its outputs have finished: no recorded operation makes it, so measuring, for its
/// fact alone (hex_testkit::waits).
static PROGRAM_END: Patience = Patience::measuring("test.hex_runtime.image_program_end");

/// `proc`'s end as a program's in a member comes, as [`PROGRAM_END`] waits, by the test's own run, which lives while it
/// does.
fn program_end(proc: &mut Box<dyn Proc>) -> Exit {
    follow::within(
        &PROGRAM_END,
        "a member's program's end",
        || Party::Live,
        |by| {
            let wait = by.map_or(Duration::MAX, |at| {
                at.saturating_duration_since(Instant::now())
            });
            proc.wait_for(wait).unwrap()
        },
    )
    .unwrap()
}

fn harness(name: &str) -> Member {
    Member {
        kind: MemberKind::Harness,
        name: name.to_owned(),
    }
}

/// Kiln's P7.1b ask (lane replay, kiln 107a4da17; Shards' D243 on D238's exec lane; AC-T7.4.1): a program runs in a
/// running image VM's member through its run's agent-control endpoint, as its caller's program, no member of it driven.
/// Do: a run whose endpoint is Shards' own on loopback, its members a harness, an agent and files; run `cat -` in the
/// harness, a variable, a working directory and its three streams asked for; write a line to its stdin and end it, then
/// read its stdout and stderr to their ends. Expect: a program of no host process, its streams given once; its stdout
/// its arguments' line then what it was sent, its stderr its own; its end `Exit::Code(0)`; one lane asked, of the
/// harness's index, carrying its arguments, its variable as `NAME=VALUE`, its working directory and its stdin, no
/// terminal and no user; the box Running, its VM never stopped.
#[test]
fn p7_a_program_runs_in_a_member_as_its_callers_program() {
    use std::io::{Read, Write};
    let images = ModelImages::new();
    *images.members.lock().unwrap() = three_members();
    let execed = Execed::default();
    *images.executed.lock().unwrap() = Some((Vec::new(), Arc::clone(&execed)));
    let (runtime, _owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-exec", None)).unwrap().id;
    runtime.start(&id).unwrap();
    let program = Program {
        args: vec!["cat".to_owned(), "-".to_owned()],
        env: vec![("KILN_REPLAY".to_owned(), "1".to_owned())],
        cwd: Some("/work/app".to_owned()),
        user: None,
    };
    let all = hex_api::Ask {
        stdin: true,
        stdout: true,
        stderr: true,
    };
    let mut proc = runtime
        .exec_member(
            &id,
            &harness("claude"),
            program,
            Stdio {
                ask: all,
                ..Stdio::null()
            },
        )
        .unwrap();
    assert!(!proc.on_this_host(), "a program in a member is its guest's");
    let given = proc.streams().unwrap();
    assert!(
        proc.streams().unwrap().stdout.is_none(),
        "its streams are given once"
    );
    let mut to = hex_api::OwnedStream::from(given.stdin.unwrap());
    let mut from = hex_api::OwnedStream::from(given.stdout.unwrap());
    let mut errors = hex_api::OwnedStream::from(given.stderr.unwrap());
    to.write_all(b"step 1\n").unwrap();
    drop(to);
    let mut heard = String::new();
    from.read_to_string(&mut heard).unwrap();
    assert_eq!(heard, "cat -\nstep 1\n");
    let mut said = String::new();
    errors.read_to_string(&mut said).unwrap();
    assert_eq!(said, "said\n");
    assert_eq!(program_end(&mut proc), Exit::Code(0));
    let asked = execed.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "one lane asked");
    let (member, ran) = &asked[0];
    assert_eq!(*member, 0, "the harness's index");
    assert_eq!(ran.argv, vec![b"cat".to_vec(), b"-".to_vec()]);
    assert_eq!(ran.env, vec![b"KILN_REPLAY=1".to_vec()]);
    assert_eq!(ran.cwd, b"/work/app".to_vec());
    assert!(
        ran.stdin && ran.tty.is_none() && ran.user.is_empty(),
        "{ran:?}"
    );
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Running);
}

/// Kiln's P7.1b (Shards' D243; AC-T7.4.1): a program that can't run in a member is refused, typed, its program checked
/// before its run. Do: a box whose run's members are a harness, an agent its host refuses programs in (as one that has
/// ended) and files; run a program in the harness before its start; after it, one naming a user, then programs in the
/// agent `ghost` its run declares not, in the files, in the agent, and one naming no member through `exec`; then stop
/// the VM and run one in the harness. Expect: `NotRunning(id, "exec in a member")` before its start; `Invalid` naming
/// the program for the user, and the member for `ghost`; `NotRunning(id, "exec in a member: it runs no program")` for
/// the files; `NotRunning(id, "exec in a member: its member refused it, ended or confined")` for the agent; `Invalid`
/// naming exec; and `NotRunning(id, "exec in a member")` once the VM has stopped. Only the agent's lane was asked.
#[test]
fn p7_a_program_that_cant_run_in_a_member_is_refused_typed() {
    let images = ModelImages::new();
    *images.members.lock().unwrap() = three_members();
    let execed = Execed::default();
    *images.executed.lock().unwrap() = Some((vec![1], Arc::clone(&execed)));
    let (runtime, _owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-exec-refused", None)).unwrap().id;
    let ran = |member: &Member, program: Program| {
        runtime
            .exec_member(&id, member, program, Stdio::null())
            .map(drop)
    };
    let claude = harness("claude");
    assert_eq!(
        ran(&claude, Program::of(&["true"])),
        Err(Refusal::NotRunning(id.clone(), "exec in a member"))
    );
    runtime.start(&id).unwrap();
    let as_root = Program {
        user: Some(hex_api::User::default()),
        ..Program::of(&["true"])
    };
    assert!(matches!(
        ran(&claude, as_root),
        Err(Refusal::Invalid {
            what: "program",
            ..
        })
    ));
    assert_eq!(
        ran(&agent("ghost"), Program::of(&["true"])),
        Err(Refusal::Invalid {
            what: "member",
            why: format!("box {id}'s run declares no agent ghost"),
        })
    );
    assert_eq!(
        ran(&agent("notes"), Program::of(&["true"])),
        Err(Refusal::NotRunning(
            id.clone(),
            "exec in a member: it runs no program"
        ))
    );
    assert_eq!(
        ran(&agent("helper"), Program::of(&["true"])),
        Err(Refusal::NotRunning(
            id.clone(),
            "exec in a member: its member refused it, ended or confined"
        ))
    );
    assert!(matches!(
        runtime.exec(&id, Program::of(&["true"]), Stdio::null()),
        Err(Refusal::Invalid { what: "exec", .. })
    ));
    runtime.signal(&id, Signal::KILL).unwrap();
    assert_eq!(
        ran(&claude, Program::of(&["true"])),
        Err(Refusal::NotRunning(id.clone(), "exec in a member"))
    );
    let asked: Vec<u32> = execed
        .lock()
        .unwrap()
        .iter()
        .map(|(member, _)| *member)
        .collect();
    assert_eq!(asked, vec![1], "only the agent's lane was asked");
}

/// Kiln's P7.1b (Shards' D243's `EXEC_SIGNAL`; AC-T7.4.1): a program's kill rides its own lane while its outputs are
/// open. Do: run `sleep` in the harness, read its first line, then kill it and read its stdout to its end. Expect: the
/// line; the kill done; its stdout's end; its end `Exit::Signal(9)`, its host having taken signal 9; its kill again,
/// once it has ended, done at once; the box Running, its VM never stopped.
#[test]
fn p7_a_programs_kill_rides_its_lane_and_its_end_reads_killed() {
    use std::io::{BufRead, Read};
    let images = ModelImages::new();
    *images.members.lock().unwrap() = three_members();
    *images.executed.lock().unwrap() = Some((Vec::new(), Execed::default()));
    let (runtime, _owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-exec-kill", None)).unwrap().id;
    runtime.start(&id).unwrap();
    let out = hex_api::Ask {
        stdout: true,
        ..hex_api::Ask::default()
    };
    let mut proc = runtime
        .exec_member(
            &id,
            &harness("claude"),
            Program::of(&["sleep"]),
            Stdio {
                ask: out,
                ..Stdio::null()
            },
        )
        .unwrap();
    let given = proc.streams().unwrap();
    let mut from = std::io::BufReader::new(hex_api::OwnedStream::from(given.stdout.unwrap()));
    let mut line = String::new();
    from.read_line(&mut line).unwrap();
    assert_eq!(line, "sleep\n");
    assert_eq!(proc.kill(), Ok(()));
    let mut rest = Vec::new();
    from.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty(), "{rest:?}");
    assert_eq!(program_end(&mut proc), Exit::Signal(9));
    assert_eq!(proc.kill(), Ok(()));
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Running);
}

/// Kiln's P7.1b (Shards' `ExecControl` at 9975d1f55; AC-T7.4.1): once a program's outputs have finished its control
/// waits for its exit, and steers nothing beside that wait, so its kill can't ride its lane then. Do: kill a program
/// whose control its end's thread has taken, its end not yet told; then tell its end and kill it again. Expect:
/// `Awaits(ExecSignalAfterOutput)`, nothing marked killed; then done at once.
#[test]
fn p7_a_kill_once_a_programs_outputs_have_finished_awaits_shards() {
    let control = Mutex::new(None);
    let killed = AtomicBool::new(false);
    let end: ExecEnd = Arc::new(Slot::new());
    assert_eq!(
        kill_exec(&control, &killed, &end),
        Err(Refusal::Awaits {
            needs: Upstream::ExecSignalAfterOutput
        })
    );
    assert!(!killed.load(Ordering::SeqCst));
    end.put(Ok(Exit::Code(0)));
    assert_eq!(kill_exec(&control, &killed, &end), Ok(()));
}

/// Set in the child [`c10_a_cold_ready_wait_is_told_as_it_starts`] starts of itself: there it boots a node VM cold.
const C10_CHILD: &str = "HEX_TEST_C10_COLD_READY_CHILD";

/// The review's C-10 (AC-T7.4.1; decision 9; ROOT, 2026-11-14): a node VM's READY wait at its process's first boot has no
/// W* to bound it, so an init hanging alive would hold its node's start for good, told to no one. Do, in a process of
/// its own (so no other boot of this binary has taught READY its W*), boot a node VM through the model. Expect its stderr
/// to tell once, as the wait starts, that it follows its party with no W*: `hex.wait event=following
/// wait=guest.hosted.ready party=<a JSON string>`.
#[test]
fn c10_a_cold_ready_wait_is_told_as_it_starts() {
    if std::env::var_os(C10_CHILD).is_some() {
        let images = ModelImages::new();
        let (runtime, _owner) = model_runtime(&images, waits());
        runtime.images.boot_node(&node_vm()).unwrap();
        return;
    }
    let module = module_path!().split_once("::").unwrap().1;
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            &format!("{module}::c10_a_cold_ready_wait_is_told_as_it_starts"),
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(C10_CHILD, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains("test result: ok. 1 passed"),
        "{stdout}\n{stderr}"
    );
    let told: Vec<&str> = stderr
        .lines()
        .filter(|line| {
            line.starts_with("hex.wait event=following wait=guest.hosted.ready party=\"")
        })
        .collect();
    assert_eq!(told.len(), 1, "the cold READY wait told once: {stderr}");
}

/// The start held at its Ready write, as this case's fact: no recorded operation makes it.
static READYING_HELD: Patience = Patience::measuring("test.hex_runtime.image_readying_held");

/// The review's F4 side finding (1) (AC-T7.4.1; T-T7.1.2): a start puts its boot under way in this process's runs before
/// it writes its box Ready, so a call that reads the box Ready finds the boot this process runs, never a run of an
/// earlier process held. Do create an image VM and start it, holding the start once its Ready is written; inspect the
/// box meanwhile; then let the start go. Expect the inspect not refused as a held run (`Awaits(AttachAfterCrash)`), and
/// the start to boot the box.
#[test]
fn f4s1_a_box_whose_start_has_written_it_ready_is_never_held() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("ready-window", None)).unwrap().id;
    owner.readying.ask();
    std::thread::scope(|scope| {
        let start = scope.spawn(|| runtime.start(&id));
        // A look: the owner tells no event as it holds a write; its party is the start's thread.
        follow::until(
            &READYING_HELD,
            "the start held at its Ready write",
            || spawned(&start),
            || owner.readying.holds(),
        )
        .unwrap();
        let seen = runtime.inspect(&id);
        owner.readying.let_go();
        assert!(
            !matches!(
                seen,
                Err(Refusal::Awaits {
                    needs: Upstream::AttachAfterCrash
                })
            ),
            "its own start's box read as held: {seen:?}"
        );
        start.join().unwrap().unwrap();
    });
    assert_eq!(images.asked.lock().unwrap().len(), 1);
}

/// A node VM's run as its record and its lines name it: its 16 bytes in lowercase hex.
fn run_hex(run: &NodeRun) -> String {
    run.bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The review's F4 side finding (3) (AC-T7.4.1; CLAUDE.md §3): a guest runtime that ends while its node VM's stop is
/// refused says so, never dropping the refusal, as its drop can return none. Do boot a node's guest under shards-init,
/// refuse its row's Ended write as a full disk refuses it, and drop the runtime. Expect a `hex.guest
/// event=node_vm_unstopped` line naming its run and the write's refusal.
#[test]
fn f4s3_a_node_vm_whose_stop_is_refused_as_its_runtime_ends_is_told() {
    let images = ModelImages::new();
    let sockets = super::super::tests::socket_fixture();
    *images.serve.lock().unwrap() = Some(sockets.path().to_owned());
    let relay = greeting_relay(sockets.path(), None);
    let owner = Arc::new(Owner::new());
    let (runtime, _) = image_form(&images, &owner);
    runtime.boot().unwrap();
    let run = *owner.node_vms.lock().unwrap().keys().next().unwrap();
    owner.refuse_ended.store(true, Ordering::SeqCst);
    node::TOLD.with(|told| told.borrow_mut().clear());
    drop(runtime);
    let told = node::TOLD.with(|told| told.borrow().clone());
    assert!(
        told.iter()
            .any(|line| line.contains("event=node_vm_unstopped")
                && line.contains(&run_hex(&run))
                && line.contains("no space left on the device")),
        "{told:?}"
    );
    assert!(relay.join().unwrap());
}

/// The review's F4 side finding (3) (AC-T7.4.1): a node VM's boot whose wait for its answer fails is stopped and its row
/// Ended, never left under way, held by no one and booked. Do boot a node VM whose boot's wait is refused (the test's
/// fault: its slot's next wait refused, as a poisoned lock or the OS refuses one). Expect the boot refused in the wait's
/// words, and its row Ended.
#[test]
fn f4s3_a_node_vm_boot_whose_wait_fails_is_stopped_and_its_row_ended() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let vm = node_vm();
    WAIT_REFUSED.with(|refused| refused.set(true));
    let booted = runtime.images.boot_node(&vm);
    assert!(
        matches!(&booted, Err(Refusal::Os { why, .. }) if why.contains("as the test asked")),
        "{booted:?}"
    );
    let row = owner
        .node_vms
        .lock()
        .unwrap()
        .get(&vm.run)
        .cloned()
        .unwrap();
    assert_eq!(
        (row.phase, row.exit.is_some()),
        (NodeVmPhase::Ended, true),
        "{row:?}"
    );
}

/// The review's B-12 (AC-T7.4.1): a call on an image VM whose record can't be read is refused saying why it can't. Do
/// keep an image box's record unreadable in its owner, with why, and inspect the box through the image class. Expect it
/// refused as broken, in the record's own words.
#[test]
fn b12_an_image_vm_whose_record_cant_be_read_is_refused_in_its_words() {
    let images = ModelImages::new();
    let (runtime, owner) = model_runtime(&images, waits());
    let id = BoxId::new("unreadable").unwrap();
    owner.records.lock().unwrap().insert(
        id.try_clone().unwrap(),
        Err("its bytes end mid-field".to_owned()),
    );
    match Runtime::inspect(&runtime.images, &id) {
        Err(Refusal::Broken(why)) => assert!(why.contains("its bytes end mid-field"), "{why}"),
        other => panic!("an unreadable record inspected: {other:?}"),
    }
}

/// A warm room counting its claims, each refused, and its gives (kiln plan `01a1dd2e313f72f69c6d93061143cc2e`, P3.1): a
/// pool's host claims its room before it starts, so a room refusing every claim starts none.
#[derive(Default)]
struct CountedRoom {
    claims: AtomicUsize,
    gives: AtomicUsize,
}

impl WarmRoom for CountedRoom {
    fn claim(&self, _: &WarmHost) -> bool {
        self.claims.fetch_add(1, Ordering::SeqCst);
        false
    }

    fn give(&self, _: &WarmHost, _: Gave) {
        self.gives.fetch_add(1, Ordering::SeqCst);
    }
}

/// The image class alone over the model's boot, given warm room `room` where there is one, its owner kept by the caller.
fn pooling_class(room: Option<Arc<dyn WarmRoom>>) -> (ImageClass, Owner) {
    let owner = Owner::new();
    let class = ImageClass::new(super::super::tests::config().binary).with_boot(
        Ok(ModelImages::new() as Arc<dyn Images>),
        waits(),
        owner.diagnostics(),
    );
    if let Some(room) = room {
        class.bind_room(room).unwrap();
    }
    (class, owner)
}

/// HEX-IMG-9, the warm pools' conformance case.
fn pools_case() -> &'static hex_api::conformance::Case {
    hex_api::conformance::IMAGE_CASES
        .iter()
        .find(|case| case.id == "HEX-IMG-9")
        .unwrap()
}

/// The pool of the model's boxes: their image and admitted size.
fn sim_pool() -> WarmPool {
    WarmPool {
        image: hex_api::sim::SIM_IMAGE.to_owned(),
        size: allocation().vm_size().unwrap(),
    }
}

/// Kiln plan `01a1dd2e313f72f69c6d93061143cc2e`, P3.1 (CP-T7.2; HEX-IMG-9): do hold the class given a warm room to the
/// pools' case. Expect: it passes, each pool kept once however often asked, listed with no host warm or starting, until
/// it's let go; and no keep claimed its room, as no box of a pool started.
#[test]
fn p3_1_the_class_given_a_room_keeps_lists_and_lets_go_of_warm_pools() {
    let room = Arc::new(CountedRoom::default());
    let (class, _owner) = pooling_class(Some(Arc::clone(&room) as Arc<dyn WarmRoom>));
    pools_case()
        .check(&class, &hex_api::sim::SimFixture::ShardsImage)
        .unwrap();
    assert_eq!(room.claims.load(Ordering::SeqCst), 0, "a keep claimed room");
    assert_eq!(room.gives.load(Ordering::SeqCst), 0, "a let-go gave room");
}

/// P3.1 (HEX-IMG-9's other branch): do ask a class given no warm room to keep, and to let go of, a pool. Expect: each
/// refused `Unconfigured` naming the warm room, which kiln maps to its typed NO_POOLS, and none listed.
#[test]
fn p3_1_a_class_given_no_room_keeps_no_pool() {
    let (class, _owner) = pooling_class(None);
    let pool = sim_pool();
    assert_eq!(
        Runtime::keep_pool(&class, &pool),
        Err(Refusal::no_warm_room())
    );
    assert_eq!(
        Runtime::let_go_pool(&class, &pool),
        Err(Refusal::no_warm_room())
    );
    assert_eq!(Runtime::pools(&class), Ok(Vec::new()));
    pools_case()
        .check(&class, &hex_api::sim::SimFixture::ShardsImage)
        .unwrap();
}

/// P3.1: do ask for a pool of a size Shards boots otherwise (one MiB of memory, short of its least). Expect: refused
/// `Invalid` naming the warm pool, as a box of that size is refused at its create, and kept by none.
#[test]
fn p3_1_a_pool_of_a_size_shards_boots_otherwise_is_refused() {
    let (class, _owner) = pooling_class(Some(Arc::new(CountedRoom::default())));
    let pool = WarmPool {
        size: VmSize {
            vcpus: 1,
            memory_mib: 1,
        },
        ..sim_pool()
    };
    assert!(
        matches!(
            Runtime::keep_pool(&class, &pool),
            Err(Refusal::Invalid {
                what: "warm pool",
                ..
            })
        ),
        "a pool Shards would boot at another size"
    );
    assert_eq!(Runtime::pools(&class), Ok(Vec::new()));
}

/// P3.1: do bind a class's warm room, then the same room again, then another. Expect: the same taken, another refused
/// `Broken`, as its record authority is bound once.
#[test]
fn p3_1_a_classs_warm_room_is_bound_once() {
    let room: Arc<dyn WarmRoom> = Arc::new(CountedRoom::default());
    let (class, _owner) = pooling_class(Some(Arc::clone(&room)));
    assert_eq!(class.bind_room(room), Ok(()));
    assert!(matches!(
        class.bind_room(Arc::new(CountedRoom::default())),
        Err(Refusal::Broken(_))
    ));
}

/// P3.1: do keep a pool of another image at a box's size, start the box; then keep the box's own pool and start a box of
/// it. Expect: the first boots on the class's own boot, no kept pool being its; the second never reaches that boot, its
/// pool's taking it (Shards' own run hosts, which this test has none of, so its start is refused; its room refuses every
/// host its pool would start warm).
#[test]
fn p3_1_a_box_boots_through_the_pool_kept_for_its_image_and_size_alone() {
    let images = ModelImages::new();
    let (runtime, _owner) = model_runtime(&images, waits());
    let room = Arc::new(CountedRoom::default());
    runtime
        .images
        .bind_room(Arc::clone(&room) as Arc<dyn WarmRoom>)
        .unwrap();
    let other = WarmPool {
        image: format!("hex-other@sha256:{}", "0".repeat(64)),
        ..sim_pool()
    };
    Runtime::keep_pool(&runtime.images, &other).unwrap();
    let unpooled = runtime.create(spec("image-unpooled", None)).unwrap().id;
    runtime.start(&unpooled).unwrap();
    assert_eq!(images.asked.lock().unwrap().len(), 1);
    Runtime::keep_pool(&runtime.images, &sim_pool()).unwrap();
    let pooled = runtime.create(spec("image-pooled", None)).unwrap().id;
    assert!(runtime.start(&pooled).is_err(), "no Shards run host here");
    assert_eq!(
        images.asked.lock().unwrap().len(),
        1,
        "a box of a kept pool booted on the class's own boot"
    );
}

/// A run's real agent-control endpoint (D238, D252) on loopback, Shards' own, its microVM's key the model's, taking
/// `ask`'s controller alone and admitting its execs, its report naming `bridges` as the members whose MCP bridge its run
/// serves (D254); its host starts nothing a controller asks, each ask refused as one its run asked none of, so what a
/// controller reads of it is its report. A model run's endpoint and host live while the test does.
fn serve_reports(
    ask: &shards_api::image::ControlAsk,
    bridges: &[u32],
) -> shards_api::image::ControlAt {
    use shards_api::control::exec;
    let vm = shards_api::control::Identity::generate().unwrap();
    let (endpoint, served) = shards_remote::control::Endpoint::bind(
        &vm,
        ask.controller,
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)),
        shards_remote::control::Streams {
            outputs: &[],
            inputs: &[],
            execs: Some(shards_api::control::exec_admissions()),
            sockets: None,
            bridges,
        },
    )
    .unwrap();
    let (address, waker, wake) = (endpoint.address, endpoint.waker, endpoint.wake);
    std::thread::spawn(move || endpoint.run());
    let starts = served.execs.unwrap();
    let revoke = served.revoke;
    std::thread::spawn(move || {
        // Held for the endpoint's life: let go, it would end every lane.
        let _revoke = revoke;
        for start in starts {
            start
                .replies
                .tell(exec::Reply::Unstarted(exec::Unstarted::Refused));
        }
    });
    shards_api::image::ControlAt {
        address,
        vm: vm.fingerprint(),
        waker,
        wake,
    }
}

/// Kiln plan step 35's `/runs/control` (D238, D239, D254; AC-T7.4.1): a running image VM's agent-control link, as both
/// its ends say it. Do: a box whose spec asks the agent `helper` (its run's member 1) a bridge, its run's endpoint
/// Shards' own on loopback, its report naming member 1's bridge; ask its link before its start, then twice once it
/// runs, then once its VM is killed. Expect: `NotRunning(id, "link")` before the start; then both ends' word on one TLS
/// 1.3 handshake, TLS13_AES_256_GCM_SHA384 (0x1302, D239) and one key exchange group, each named, and the lanes its
/// controller was granted, by Shards' names: its control lane, its execs and the run's log they read, then member 1's
/// bridge (`mcp:1`); the second answer the first's; the box Running, its VM never stopped; and `NotRunning(id, "link")`
/// once its VM is killed.
#[test]
fn s35_a_running_image_vms_link_says_both_ends_tls_and_its_lanes() {
    let images = ModelImages::new();
    *images.members.lock().unwrap() = three_members();
    *images.reports.lock().unwrap() = Some(vec![1]);
    let (runtime, _owner) = model_runtime(&images, waits());
    let helper = agent("helper");
    let id = runtime
        .create(bridged_spec("image-link", &[&helper]))
        .unwrap()
        .id;
    assert_eq!(
        runtime.link(&id),
        Err(Refusal::NotRunning(id.clone(), "link"))
    );
    runtime.start(&id).unwrap();
    let link = runtime.link(&id).unwrap();
    assert_eq!(
        link.controller, link.endpoint,
        "both ends' word on one handshake"
    );
    assert_eq!(
        (link.controller.suite, link.controller.suite_name),
        (0x1302, Some("TLS13_AES_256_GCM_SHA384"))
    );
    assert!(
        link.controller.group_name.is_some(),
        "{:?}",
        link.controller
    );
    assert_eq!(link.lanes, ["control", "exec", "log:run", "mcp:1"]);
    assert_eq!(runtime.link(&id).unwrap(), link, "asked again, the same");
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Running);
    runtime.signal(&id, Signal::KILL).unwrap();
    assert_eq!(
        runtime.link(&id),
        Err(Refusal::NotRunning(id.clone(), "link"))
    );
}

/// Kiln plan step 35 (AC-T7.4.1): a link whose endpoint never reported is refused as `Runtime::link` says. Do: refuse a
/// box's link for its connection's end, this side's own close among them, then for the endpoint's reset of its control
/// lane. Expect: `NotRunning(id, "link: its run ended")` for the end; `Os` for the reset, in Shards' words, naming the box.
#[test]
fn s35_a_link_with_no_report_is_refused_typed() {
    let id = BoxId::new("image-link-refused").unwrap();
    assert_eq!(
        link_refused(&id, Fault::Lost(shards_api::control::Ended::Local)),
        Refusal::NotRunning(id.clone(), "link: its run ended")
    );
    let reset = link_refused(&id, Fault::Revoked);
    assert!(
        matches!(&reset, Refusal::Os { why, .. } if why.starts_with("box image-link-refused: ")),
        "{reset:?}"
    );
}
