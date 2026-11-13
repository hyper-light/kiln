//! The Shards class on Agentfile images over a model of Shards' image boot (D231's `Images`, which has
//! no simulator of its own yet) and a record owner holding the node Store's lifecycle rules: each test
//! does a call and expects the record, the run and the refusal the design names
//! (docs/design/shards-image-class.md; joint plan step 12, AC-T7.4.1).

use std::sync::Condvar;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::devices::{Books, Records, Transaction};
use hex_api::{Allocation, Bounds, Capacity, CapacityResources, DeviceInfo, Runtime};
use shards_api::image::{
    Bound, Channel, GuestTimes, HostUse, Keep, MemberEnd, MemberInfo, MemberName, Microvm,
    Receiving, StartedFrom, VcpuTime,
};

/// A record owner keeping the node Store's rules (hex-node's `Store::update_lifecycle`): a first record
/// is Creating; Creating becomes anything, Ready Ready or Deleting, Deleting Deleting alone; an exit, once
/// written, stays; only a Deleting record is removed.
struct Owner {
    records: Mutex<Records>,
    /// Its scans of every record ([`Books::records`]), each a read of the node's whole Store.
    scans: AtomicUsize,
    /// The node's state's own directory for its image boxes' diagnostics, gone with the owner.
    state: tempfile::TempDir,
}

impl Owner {
    fn new() -> Self {
        Self {
            records: Mutex::default(),
            scans: AtomicUsize::new(0),
            state: tempfile::tempdir().unwrap(),
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
        let mut records = self.records.lock().unwrap();
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
        _: Duration,
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
        Transaction::records(self)
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
    fn capacity(&self) -> Result<Capacity, Refusal> {
        Err(broken("the image tests read no capacity"))
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
}

impl ModelRun {
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
    /// How its microVM started (D253), as the model's run host said it at its boot.
    started_from: StartedFrom,
}

/// The period the model's run host says its VM's governor holds a rate over (D247's `rate_period`): the model's
/// fixture, which its runs all say.
const MODEL_RATE_PERIOD: Duration = Duration::from_millis(100);

impl ImageGuest for ModelGuest {
    fn members(&self) -> &[MemberInfo] {
        &self.members
    }
    fn published(&self) -> &[Bound] {
        &self.published
    }
    // No agent-control endpoint: the image class asks for none yet (D252).
    fn control(&self) -> Option<&shards_api::image::ControlAt> {
        None
    }
    fn rate_period(&self) -> Duration {
        MODEL_RATE_PERIOD
    }
    fn started_from(&self) -> StartedFrom {
        self.started_from
    }
    fn take_output(
        &self,
        member: &MemberName,
        channel: Channel,
    ) -> Result<std::io::PipeReader, ImageRefusal> {
        let mut outside = self.outside.lock().unwrap();
        let ends = outside.get_mut(&member.name);
        let taken = match channel {
            Channel::Stdout => ends.and_then(|e| e.1.take()),
            Channel::Stderr => ends.and_then(|e| e.2.take()),
        };
        taken.ok_or_else(|| ImageRefusal::Spec {
            what: "member",
            why: format!("{}'s output was taken, or it doesn't run", member.name),
        })
    }
    fn take_input(&self, member: &MemberName) -> Result<std::io::PipeWriter, ImageRefusal> {
        let mut outside = self.outside.lock().unwrap();
        outside
            .get_mut(&member.name)
            .and_then(|e| e.0.take())
            .ok_or_else(|| ImageRefusal::Spec {
                what: "member",
                why: format!("{}'s stdin was taken, or never asked for", member.name),
            })
    }
    fn wait_member(
        &self,
        member: &MemberName,
        _: Duration,
    ) -> Result<Option<MemberEnd>, ImageRefusal> {
        Ok(self
            .run
            .member_ends
            .lock()
            .unwrap()
            .get(&member.name)
            .copied())
    }
    fn stats(&self, _: StatsScope, patience: Duration) -> Result<Stats, ImageRefusal> {
        self.run.stats_asked.lock().unwrap().push(patience);
        match self.run.end.lock().unwrap().clone() {
            Some(end) => Err(ImageRefusal::Ended(end)),
            None => Ok(self.run.stats.lock().unwrap().clone()),
        }
    }
    // The model keeps each room (D241) and rate (D247) it is told and holds the run as told: the room within none
    // where it is short; it gives no member a terminal (D244), refused as unsupported.
    fn room(&self, room: Option<i64>, _: Duration) -> Result<Option<u64>, ImageRefusal> {
        if let Some(end) = self.run.end.lock().unwrap().clone() {
            return Err(ImageRefusal::Ended(end));
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
            return Err(ImageRefusal::Ended(end));
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
    // As Shards refuses them for a run that asks no nesting grant (D234) and of a model with no plugs (D235).
    fn wait_ready(
        &self,
        _: Option<shards_api::image::Ready>,
        _: Duration,
    ) -> Result<Option<shards_api::image::Ready>, ImageRefusal> {
        Err(ImageRefusal::Spec {
            what: "nesting",
            why: "the model's runs ask no nesting grant".to_owned(),
        })
    }
    fn plug(
        &self,
        _: &MemberName,
        _: &std::path::Path,
        _: &str,
        _: bool,
        _: Duration,
    ) -> Result<(), ImageRefusal> {
        Err(ImageRefusal::Unsupported {
            what: "the model's plugs",
            why: "it has none".to_owned(),
        })
    }
    fn unplug(&self, _: &str, _: Duration) -> Result<(), ImageRefusal> {
        Err(ImageRefusal::Unsupported {
            what: "the model's plugs",
            why: "it has none".to_owned(),
        })
    }
    fn stop(&self) -> Result<(), ImageRefusal> {
        self.run.stops.fetch_add(1, Ordering::SeqCst);
        self.run.end.lock().unwrap().get_or_insert(RunEnd::Stopped);
        self.run.release();
        Ok(())
    }
    fn keep_diagnostics(&self, to: std::fs::File) -> Result<(), ImageRefusal> {
        self.run.diagnostics.lock().unwrap().push(to);
        Ok(())
    }
    fn wait_for(&self, _: Duration) -> Result<Option<RunEnd>, ImageRefusal> {
        let end = self.run.end.lock().unwrap().clone();
        Ok(end.or_else(|| {
            self.run.gone.lock().unwrap().as_ref().map(|said| {
                RunEnd::Failed(format!("the run host exited without its run's end: {said}"))
            })
        }))
    }
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
    /// How the next boot's microVM starts (D253), as the test sets it: booted alone unless it does.
    started_from: Mutex<StartedFrom>,
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
            started_from: Mutex::new(StartedFrom::Booted),
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
        self.asked.lock().unwrap().push(spec.clone());
        self.patience.lock().unwrap().push(patience);
        *self.run.lessor.lock().unwrap() = spec.lessor.clone();
        let gate = self.gate.lock().unwrap();
        let gate = self
            .opened
            .wait_while(gate, |gate| *gate == Gate::Host)
            .unwrap();
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
        Ok(Box::new(ModelGuest {
            run: Arc::clone(&self.run),
            members: self.members.lock().unwrap().clone(),
            published,
            outside: Mutex::new(outside),
            started_from: *self.started_from.lock().unwrap(),
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

/// Waits on `wait`, its fact's own patience, for box `id` to show `state`.
fn shown(runtime: &GuestRuntime, id: &BoxId, state: BoxState, wait: &Patience) -> BoxInfo {
    let mut last = None;
    hex_testkit::waits::until(wait, &format!("{id} shown {state:?}"), || {
        let info = runtime.inspect(id).unwrap();
        let reached = info.state == state;
        last = Some(info);
        reached
    })
    .unwrap();
    last.unwrap()
}

/// Joint plan step 12: do create, then start, an image VM of two vCPUs and 256 MiB with no command.
/// Expect: create records it Creating and shows it Created, booting nothing; start writes Ready first
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
        (hex_api::Class::Shards, BoxState::Created, None)
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
    let mut alike = second.clone();
    alike.members.clone_from(&first.members);
    alike.ready_by = first.ready_by;
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

/// Joint plan step 12 for kiln's step 18 (D231's shares): an image VM's guest sees the host directories its caller
/// names, and only those. Do boot a VM sharing a home volume read-write and a project read-only; one granted egress and
/// sharing none; then one whose share Shards refuses. Expect the first boot to ask Shards for exactly those two shares,
/// each its host directory, where the guest sees it and whether read-only, in order; the second for none, its grants
/// widening no share; the third's start refused as Invalid, in Shards' own words.
#[test]
fn w12_an_image_vm_shares_exactly_the_host_directories_its_caller_names() {
    use hex_api::{Destination, Grant, Member, MemberKind, Mount, Ports, Proto};
    let shares = vec![
        Mount {
            source: std::path::PathBuf::from("/var/lib/kiln/homes/ada/.claude"),
            destination: "/home/agent/.claude".to_owned(),
            read_only: false,
        },
        Mount {
            source: std::path::PathBuf::from("/srv/projects/hex"),
            destination: "/work/hex".to_owned(),
            read_only: true,
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

/// Joint plan step 12, steps 11's boundary: do exec a member's program and send TERM to a running VM.
/// Expect: Awaits(MemberExec) and Awaits(MemberSignal), no stop, and the VM still Running.
#[test]
fn member_exec_and_signals_await_the_agent_control_endpoint() {
    let images = ModelImages::new();
    let (runtime, _owner) = model_runtime(&images, waits());
    let id = runtime.create(spec("image-members", None)).unwrap().id;
    runtime.start(&id).unwrap();
    assert!(matches!(
        runtime.exec(&id, hex_api::Program::of(&["true"]), hex_api::Stdio::null()),
        Err(Refusal::Awaits {
            needs: Upstream::MemberExec
        })
    ));
    assert_eq!(
        runtime.signal(&id, Signal::TERM),
        Err(Refusal::Awaits {
            needs: Upstream::MemberSignal
        })
    );
    assert_eq!(images.run.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.inspect(&id).unwrap().state, BoxState::Running);
}

/// Joint plan step 17, AC-T7.4.1: do create an image VM whose spec names a harness its caller drives, with a
/// variable and a working directory, take it before the start, start it, and take it. Expect: before the start
/// NotRunning; the boot asks Shards for that member's stdin with its variable and directory and no terminal; the
/// caller's ends of its three streams, given once; what the caller writes read by the member in order, and its
/// end of input at the caller's close; what the member writes on stdout and stderr read whole by the caller; no
/// end until the member's, then its status; its own kill awaiting the agent-control endpoint; a second take
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
    assert_eq!(
        proc.kill(),
        Err(Refusal::Awaits {
            needs: Upstream::MemberSignal
        })
    );
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
/// delete refused NoSuchBox; and the delete of the ended run to answer none, nothing read at its end.
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
    assert_eq!(runtime.delete_with_host_use(&id).unwrap(), None);
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
/// Lifecycle Pending, the box Created while its boot runs, delete Pending; then, the boot answering, the box
/// Running with no second boot.
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
    assert!(matches!(
        runtime.delete(&id),
        Err(Refusal::Lifecycle {
            cause: LifecycleCause::Pending,
            ..
        })
    ));
    images.gate(Gate::Open);
    shown(&runtime, &id, BoxState::Running, &LATE_BOOT_SHOWN);
    assert_eq!(images.asked.lock().unwrap().len(), 1);
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
    hex_testkit::waits::until(&ASKED, "the model's boot asked", || {
        !images.patience.lock().unwrap().is_empty()
    })
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
    .with_images(host_images(), waits(), owner.diagnostics());
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
    }
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
            waits.stop.deadline()
        ),
        (None, None, None)
    );
    assert_eq!(
        (waits.boot.name(), waits.stats.name(), waits.stop.name()),
        ("hex.image.boot", "hex.image.stats", "hex.image.stop")
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
/// Expect: the boot asked exactly the box's credential, one grant toward both hosts and a lessor; inspect showing every
/// credential grant as the record holds it; the ask answered with the broker's value, held there; a host the grant
/// doesn't name carrying no credential (Shards' decision passes it bare); and the lease given back as the run ends.
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
            "api.anthropic.com"
        ),
        shards_api::image::Fill::With("anthropic/ada".to_owned())
    );
    assert_eq!(
        shards_api::image::decide(&asked[0].credentials, "evil.example", "evil.example"),
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
