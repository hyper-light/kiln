//! The Shards class on Agentfile images over a model of Shards' image boot (D231's `Images`, which has
//! no simulator of its own yet) and a record owner holding the node Store's lifecycle rules: each test
//! does a call and expects the record, the run and the refusal the design names
//! (docs/design/shards-image-class.md; joint plan step 12, AC-T7.4.1).

use std::sync::Condvar;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::devices::{Books, Records, Transaction};
use hex_api::{Allocation, Bounds, Capacity, CapacityResources, DeviceInfo, Runtime};
use shards_api::image::{Bound, Channel, Keep, MemberEnd, MemberInfo, MemberName, Receiving};

/// A record owner keeping the node Store's rules (hex-node's `Store::update_lifecycle`): a first record
/// is Creating; Creating becomes anything, Ready Ready or Deleting, Deleting Deleting alone; an exit, once
/// written, stays; only a Deleting record is removed.
#[derive(Default)]
struct Owner {
    records: Mutex<Records>,
    /// Its scans of every record ([`Books::records`]), each a read of the node's whole Store.
    scans: AtomicUsize,
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
    stops: AtomicUsize,
    stats: Mutex<Stats>,
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
}

impl ImageGuest for ModelGuest {
    fn members(&self) -> &[MemberInfo] {
        &self.members
    }
    fn published(&self) -> &[Bound] {
        &self.published
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
    fn stats(&self, _: StatsScope, _: Duration) -> Result<Stats, ImageRefusal> {
        match self.run.end.lock().unwrap().clone() {
            Some(end) => Err(ImageRefusal::Ended(end)),
            None => Ok(*self.run.stats.lock().unwrap()),
        }
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
        Ok(())
    }
    fn wait_for(&self, _: Duration) -> Result<Option<RunEnd>, ImageRefusal> {
        Ok(self.run.end.lock().unwrap().clone())
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
    /// What each member the image declares reaches by name (D236), as the test sets it: the run host holds every
    /// one to its reach of the destinations kept where a boot holds members apart (`Narrowing::members`).
    named: Mutex<Vec<(shards_api::image::MemberKind, String, Vec<String>)>>,
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

    /// The run host's hold of each member apart, as Hex's recorded Shards change makes it (`api_run`'s
    /// `held_apart`): every member the image declares reaches exactly what the boot lets it of the destinations
    /// kept, else the boot is refused as a widening before anything starts.
    fn held_apart(&self, egress: &shards_api::image::Narrowing) -> Result<(), ImageRefusal> {
        let Some(asked) = &egress.members else {
            return Ok(());
        };
        let Keep::Only(kept) = &egress.destinations else {
            return Err(ImageRefusal::Widening(
                "the model keeps no destination wholesale".to_owned(),
            ));
        };
        for (kind, name, reaches) in self.named.lock().unwrap().iter() {
            let mut reach: Vec<&String> = reaches.iter().filter(|r| kept.contains(r)).collect();
            reach.sort();
            let mut lets: Vec<&String> = asked
                .iter()
                .find(|m| m.kind == *kind && m.name == *name)
                .map(|m| m.destinations.iter().collect())
                .unwrap_or_default();
            lets.sort();
            if reach != lets {
                return Err(ImageRefusal::Widening(format!(
                    "{kind:?} {name}: of the destinations kept it would reach {reach:?}, where the request lets it reach {lets:?}"
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
    let owner = Arc::new(Owner::default());
    let runtime = GuestRuntime::new(
        Arc::new(shards_api::sim::SimGuests::new(|_| {
            shards_api::End::PowerOff
        })),
        super::super::tests::config(),
    )
    .with_images(Ok(Arc::clone(images) as Arc<dyn Images>), waits);
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
/// The model's boot asked, and a box its refused boot ended shown Empty: facts no recorded operation makes either.
static BOOT_ASKED: Patience = Patience::measuring("test.hex_runtime.image_boot_asked");
static REFUSED_SHOWN: Patience = Patience::measuring("test.hex_runtime.image_refused_shown");

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

/// Joint plan steps 12 and 14: do read a running VM's usage, then one whose stats lack a line. Expect:
/// its run's CPU in microseconds, memory, processes and I/O, guest time per vCPU Awaits
/// (GuestCpuTimePerVcpu), never a number; a missing line refused naming it; before its start
/// NotRunning.
#[test]
fn usage_reads_the_runs_stats_and_guest_time_awaits_shards() {
    let images = ModelImages::new();
    *images.run.stats.lock().unwrap() = Stats {
        cpu_ns: Some(7_000_999),
        memory_bytes: Some(64 << 20),
        pids: Some(5),
        read_bytes: Some(11),
        write_bytes: Some(13),
        ..Stats::default()
    };
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
        Some(GuestTime::Awaits(Upstream::GuestCpuTimePerVcpu))
    );
    images.run.stats.lock().unwrap().pids = None;
    assert!(matches!(
        runtime.usage(&id),
        Err(Refusal::Os { why, .. }) if why.contains("pids")
    ));
}

/// Joint plan step 12: do start a VM whose image Shards refuses. Expect: the refusal typed (Invalid,
/// naming the image), the box ended with an unknown exit and shown Empty, its record keeping both.
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
    let info = runtime.inspect(&id).unwrap();
    assert_eq!(
        (info.state, info.exit),
        (BoxState::Empty, Some(Exit::Unknown))
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

/// Decision 9, joint plan step 12 (CLAUDE.md §2: every wait has a bound): do start a VM whose run host starts but
/// whose run doesn't, its boot past its learned patience. Expect: Shards' boot given that patience as its own; start
/// refused Lifecycle Pending; then Shards' refusal at its patience ends the box (Exit::Unknown), shown Empty, its
/// record keeping it, with no second boot.
#[test]
fn a_boot_is_given_its_learned_patience_and_ends_the_box_past_it() {
    let images = ModelImages::new();
    images.gate(Gate::Run);
    // A 1 ns record arms the boot's bound at once; the model's twin of it says what that bound is.
    let seed = Duration::from_nanos(1);
    let bound = Patience::seeded("test.hex_runtime.image_boot_bounded_twin", seed)
        .deadline()
        .unwrap();
    let bounded = ImageWaits {
        boot: Patience::seeded("test.hex_runtime.image_boot_bounded", seed),
        ..waits().unwrap()
    };
    let (runtime, owner) = model_runtime(&images, Ok(bounded));
    let id = runtime.create(spec("image-bounded", None)).unwrap().id;
    assert!(matches!(
        runtime.start(&id),
        Err(Refusal::Lifecycle {
            cause: LifecycleCause::Pending,
            ..
        })
    ));
    hex_testkit::waits::until(&BOOT_ASKED, "the model's boot asked", || {
        !images.patience.lock().unwrap().is_empty()
    })
    .unwrap();
    assert_eq!(*images.patience.lock().unwrap(), vec![bound]);
    let info = shown(&runtime, &id, BoxState::Empty, &REFUSED_SHOWN);
    assert_eq!(info.exit, Some(Exit::Unknown));
    assert_eq!(record(&owner, &id).unwrap().exit, Some(Exit::Unknown));
    assert_eq!(images.asked.lock().unwrap().len(), 1);
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
    let owner = Arc::new(Owner::default());
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
    let owner = Arc::new(Owner::default());
    let runtime = GuestRuntime::new(
        Arc::new(shards_api::sim::SimGuests::new(|_| {
            shards_api::End::PowerOff
        })),
        super::super::tests::config(),
    )
    .with_images(host_images(), waits());
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
        Some(vec![shards_api::image::wire::MemberReach {
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

/// Decision 9, joint plan step 12: do make the image waits from records of all three measures, from none
/// outside the waits census, and from none while it runs. Expect: each armed at twice its record; refused
/// naming the first record to take; and each measuring, armed at nothing, under its own name, so the census
/// takes its first record.
#[test]
fn the_image_waits_take_their_records_or_measure_only_while_the_census_runs() {
    let ms = Duration::from_millis;
    let recorded = |measure: &str| match measure {
        "image_boot" => Ok(ms(3)),
        "image_stats" => Ok(ms(5)),
        "image_stop" => Ok(ms(7)),
        other => panic!("no such measure {other}"),
    };
    let armed = ImageWaits::from_records(recorded, false).unwrap();
    assert_eq!(
        (
            armed.boot.deadline(),
            armed.stats.deadline(),
            armed.stop.deadline()
        ),
        (Some(ms(6)), Some(ms(10)), Some(ms(14)))
    );
    let none = |measure: &str| {
        Err(Refusal::Platform {
            what: "the oci class's guest",
            platform: "test",
            why: format!("no {measure} record for any host class yet"),
        })
    };
    assert!(matches!(
        ImageWaits::from_records(none, false),
        Err(Refusal::Platform { why, .. }) if why.contains("image_boot")
    ));
    let census = ImageWaits::from_records(none, true).unwrap();
    assert_eq!(
        (
            census.boot.deadline(),
            census.stats.deadline(),
            census.stop.deadline()
        ),
        (None, None, None)
    );
    assert_eq!(
        (census.boot.name(), census.stats.name(), census.stop.name()),
        ("hex.image.boot", "hex.image.stats", "hex.image.stop")
    );
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
