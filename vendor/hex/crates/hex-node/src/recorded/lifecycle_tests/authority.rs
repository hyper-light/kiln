//! AC-T7.4.1: actual Recorded/Store/Turns, with explicitly simulated native effects.
use super::*;
use hex_api::{BoxState, DeviceId, DeviceKind, Network, OciSpec, Rootfs};
use hex_runtime::devices::{self, Books as Authority};
use std::sync::{OnceLock, Weak};

fn guest_spec(id: &str) -> BoxSpec {
    let mut spec = spec();
    spec.id = BoxId::new(id).unwrap();
    spec.class = ClassSpec::Shards(Box::new(OciSpec {
        rootfs: Rootfs::Dir("/model/root".into()),
        init: Program::of(&["sleep", "infinity"]),
        mounts: Vec::new(),
        hostname: None,
        network: Network::Private,
        hosts: Vec::new(),
    }));
    spec.check().unwrap();
    spec
}
fn info(record: &BoxRecord) -> BoxInfo {
    BoxInfo {
        published: Vec::new(),
        grants: Vec::new(),
        route: record.spec.route,
        id: record.spec.id.clone(),
        class: record.spec.class.class(),
        state: BoxState::Empty,
        bounds: record.spec.bounds.clone(),
        adopted: false,
        membership: None,
        exit: record.exit,
        abandoned: record.abandoned,
        allocation: record.spec.allocation,
        last_resort: None,
        ended: None,
        credentials: Vec::new(),
    }
}
struct Registered {
    books: OnceLock<Weak<dyn Authority>>,
    programs: Backend,
    created: AtomicUsize,
    deleted: AtomicUsize,
}
impl Registered {
    fn books(&self) -> &Weak<dyn Authority> {
        self.books.get().unwrap()
    }
}
impl Runtime for Registered {
    fn classes(&self) -> Vec<Class> {
        vec![Class::Shards]
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        devices::with_transaction(self.books(), &spec.id, &spec.devices, |tx| {
            assert!(
                tx.read(&spec.id)?.is_none(),
                "outer Recorded wrote a duplicate Creating record"
            );
            let mut row = BoxRecord::new(spec.clone(), Phase::Creating, Vec::new());
            tx.put(&row)?;
            self.created.fetch_add(1, Ordering::SeqCst);
            row.init = Some(ProcId {
                pid: 7,
                start_ticks: 11,
            });
            row.phase = Phase::Ready;
            tx.put(&row)?;
            Ok(info(&row))
        })
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        devices::with_transaction(self.books(), id, &[], |tx| {
            assert_eq!(tx.read(id)?.unwrap().phase, Phase::Ready);
            // Reentrant get/all/update use the short Store seam, not the already owned method turn.
            assert!(!tx.records()?.is_empty());
            Ok(())
        })
    }
    fn exec(&self, _: &BoxId, _: Program, _: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        Err(Refusal::Broken("test backend has no program effect".into()))
    }
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.programs.run(spec, program, stdio)
    }
    fn signal(&self, id: &BoxId, _: Signal) -> Result<(), Refusal> {
        self.start(id)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        devices::with_transaction(self.books(), id, &[], |tx| {
            tx.read(id)?
                .as_ref()
                .map(info)
                .ok_or_else(|| Refusal::NoSuchBox(id.clone()))
        })
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.inspect(id)?;
        Ok(Usage::default())
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        Ok(Vec::new())
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        devices::with_transaction(self.books(), id, &[], |tx| {
            let mut row = tx.read(id)?.ok_or_else(|| Refusal::NoSuchBox(id.clone()))?;
            row.phase = Phase::Deleting;
            tx.put(&row)?;
            self.deleted.fetch_add(1, Ordering::SeqCst);
            tx.remove(id)
        })
    }
}
struct Routed {
    host: Backend,
    guest: Registered,
}
impl Runtime for Routed {
    fn classes(&self) -> Vec<Class> {
        vec![Class::Process, Class::Shards]
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        if spec.class.class() == Class::Shards {
            self.guest.create(spec)
        } else {
            self.host.create(spec)
        }
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.host.start(id)
    }
    fn exec(&self, id: &BoxId, p: Program, s: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.host.exec(id, p, s)
    }
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        if spec.class.class() == Class::Shards {
            self.guest.run(spec, program, stdio)
        } else {
            self.host.run(spec, program, stdio)
        }
    }
    fn signal(&self, id: &BoxId, s: Signal) -> Result<(), Refusal> {
        self.host.signal(id, s)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.host.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.host.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.host.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.host.delete(id)
    }
}
impl Recover for Routed {
    // The registered guest keeps only rows in the same Store: the host's cross-check is all of it.
    fn preflight_retained(
        &self,
        records: &hex_runtime::devices::Records,
        pools: &[PoolRecord],
    ) -> Result<(), Refusal> {
        self.host.preflight_retained(records, pools)
    }
    fn owns_records(&self, spec: &BoxSpec) -> Result<bool, Refusal> {
        Ok(spec.class.class() == Class::Shards)
    }
    fn runtime_for(&self, spec: &BoxSpec) -> Result<&dyn Runtime, Refusal> {
        Ok(if spec.class.class() == Class::Shards {
            &self.guest
        } else {
            &self.host
        })
    }
    fn trace(&self, _: &BoxId) -> Result<Option<ProcId>, Refusal> {
        Ok(None)
    }
    fn adopt(&self, _: &BoxSpec, _: Option<ProcId>) -> Result<(), Refusal> {
        Ok(())
    }
    fn end(&self, _: Option<ProcId>) -> Result<(), Refusal> {
        panic!("guest cleanup reached native host end")
    }
}

#[test]
fn registered_backend_uses_same_arc_store_and_turns_without_outer_record_or_reentrant_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (gate, _, _) = fixture(0);
    let node = Arc::new(
        Recorded::open(
            Routed {
                host: Backend(Arc::clone(&gate)),
                guest: Registered {
                    books: OnceLock::new(),
                    programs: Backend(Arc::clone(&gate)),
                    created: AtomicUsize::new(0),
                    deleted: AtomicUsize::new(0),
                },
            },
            dir.path(),
        )
        .unwrap(),
    );
    let authority: Arc<dyn Authority> = node.clone();
    node.runtime
        .guest
        .books
        .set(Arc::downgrade(&authority))
        .ok()
        .unwrap();
    drop(authority);
    let expected = guest_spec("routed");
    node.create(expected.clone()).unwrap();
    let records = Authority::records(node.as_ref()).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records.get(&expected.id).unwrap().as_ref().unwrap().spec,
        expected
    );
    node.start(&expected.id).unwrap();
    node.signal(&expected.id, Signal::TERM).unwrap();
    assert_eq!(node.inspect(&expected.id).unwrap().class, Class::Shards);
    let mut foreign = spec();
    foreign.id = expected.id.clone();
    gate.sim.create(foreign.clone()).unwrap();
    node.delete(&expected.id).unwrap();
    assert!(Authority::records(node.as_ref()).unwrap().is_empty());
    assert!(
        gate.sim.inspect(&foreign.id).is_ok(),
        "persisted guest route deleted host's same-named box"
    );
    assert_eq!(node.runtime.guest.created.load(Ordering::SeqCst), 1);
    assert_eq!(node.runtime.guest.deleted.load(Ordering::SeqCst), 1);
    gate.sim.delete(&foreign.id).unwrap();
    let weak = node.runtime.guest.books.get().unwrap().clone();
    drop(node);
    assert!(
        weak.upgrade().is_none(),
        "registered guest retained a strong owner cycle"
    );
}

#[test]
fn shared_transactions_refuse_forged_claims_cross_identity_and_retained_device_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let (gate, _, _) = fixture(0);
    // A first device claim is checked against the canonical catalog (check_box_claims): GPU 7 is
    // in it, so each refusal below is the claim's own, never a missing catalog.
    let catalog = devices::Catalog::parse(
        b"hex-devices-1\ngpu\t7\tfixture-gpu\t4096\thcs\tPCIROOT(0)#PCI(0700)\t\n",
    )
    .unwrap();
    assert!(
        gate.catalog
            .set(devices::SharedCatalog::try_new(catalog).unwrap())
            .is_ok()
    );
    let node = Recorded::open(Backend(gate), dir.path()).unwrap();
    let device = DeviceId {
        kind: DeviceKind::Gpu,
        key: 7,
    };
    let mut a = BoxRecord::new(guest_spec("a"), Phase::Creating, Vec::new());
    a.spec.devices.push(device);
    a.spec.check().unwrap();
    assert!(Authority::with_transaction(&node, &a.spec.id, &[], &mut |tx| tx.put(&a)).is_err());
    assert!(Authority::records(&node).unwrap().is_empty());
    Authority::with_transaction(&node, &a.spec.id, &[device], &mut |tx| {
        assert!(tx.read(&BoxId::new("foreign").unwrap()).is_err());
        tx.put(&a)
    })
    .unwrap();
    let mut b = a.clone();
    b.spec.id = BoxId::new("b").unwrap();
    assert!(
        Authority::with_transaction(&node, &b.spec.id, &[device], &mut |tx| tx.put(&b)).is_err()
    );
    Authority::with_transaction(&node, &a.spec.id, &[device], &mut |tx| {
        let mut row = tx.read(&a.spec.id)?.unwrap();
        row.phase = Phase::Deleting;
        tx.put(&row)
    })
    .unwrap();
    assert!(
        Authority::with_transaction(&node, &b.spec.id, &[device], &mut |tx| tx.put(&b)).is_err()
    );
    assert!(
        Authority::with_transaction(&node, &a.spec.id, &[], &mut |tx| tx.remove(&a.spec.id))
            .is_err()
    );
    // This fixture has no native effects; the test owns the positive terminal proof.
    Authority::with_transaction(&node, &a.spec.id, &[device], &mut |tx| {
        tx.remove(&a.spec.id)
    })
    .unwrap();
    Authority::with_transaction(&node, &b.spec.id, &[device], &mut |tx| tx.put(&b)).unwrap();
    assert_eq!(Authority::records(&node).unwrap().len(), 1);
}

#[test]
fn forwarded_info_uses_captured_box_class_and_refuses_foreign_reply() {
    let dir = tempfile::tempdir().unwrap();
    let (gate, _, _) = fixture(0);
    let node = Recorded::open(Backend(Arc::clone(&gate)), dir.path()).unwrap();
    let expected = guest_spec("forwarded");
    node.books
        .store
        .put(&BoxRecord::new(expected.clone(), Phase::Ready, Vec::new()))
        .unwrap();
    for foreign in [false, true] {
        let (tx, rx) = mpsc::sync_channel(1);
        assert!(matches!(
            Host::forward(
                &node,
                Call::Inspect(expected.id.clone()),
                Stdio::null(),
                Box::new(move |a| tx.send(a).unwrap()),
                Box::new(|_| {})
            ),
            Forward::Taken
        ));
        assert!(node.books.lifecycle.try_take(&expected.id).is_err());
        let mut reply = info(&BoxRecord::new(expected.clone(), Phase::Ready, Vec::new()));
        reply.class = Class::Oci;
        if foreign {
            reply.id = BoxId::new("foreign").unwrap();
        }
        gate.forwarded.lock().unwrap().take().unwrap()(Ok(Forwarded::Answer(Box::new(
            Answer::Info(reply),
        ))));
        let answer = rx.recv().unwrap();
        if foreign {
            assert!(answer.is_err());
        } else {
            match answer.unwrap() {
                Forwarded::Answer(answer) => match *answer {
                    Answer::Info(info) => {
                        assert_eq!(info.class, Class::Shards);
                        assert_eq!(info.id, expected.id);
                    }
                    _ => panic!("wrong answer kind"),
                },
                _ => panic!("wrong forwarded kind"),
            }
        }
        assert!(node.books.lifecycle.try_take(&expected.id).is_ok());
    }
}

#[test]
fn native_offers_require_same_provider_identity_and_use_actual_metadata_without_holders() {
    use hex_api::{Capacity, CapacityResources, DeviceInfo};
    let id = DeviceId {
        kind: DeviceKind::Gpu,
        key: 7,
    };
    let native = DeviceInfo {
        id,
        model: "actual native GPU".into(),
        size: 4096,
        holder: None,
    };
    let capacity = || Capacity {
        routes: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        id: "same-node".into(),
        total: CapacityResources::default(),
        free: CapacityResources::default(),
        cost: None,
        devices: vec![DeviceInfo {
            id,
            model: "catalog metadata is not native proof".into(),
            size: 1,
            holder: None,
        }],
        unavailable: Vec::new(),
        learned: None,
    };
    let mut report = capacity();
    native_metadata(&mut report, vec![native.try_clone().unwrap()]).unwrap();
    assert_eq!(report.devices, vec![native.try_clone().unwrap()]);
    assert!(native_metadata(&mut capacity(), vec![]).is_err());
    assert!(
        native_metadata(
            &mut capacity(),
            vec![native.try_clone().unwrap(), native.try_clone().unwrap()]
        )
        .is_err()
    );
    let mut forged = native.try_clone().unwrap();
    forged.id.key = 8;
    assert!(native_metadata(&mut capacity(), vec![forged]).is_err());
    let mut holder = native;
    holder.holder = Some(hex_api::DeviceHolder::Box(
        BoxId::new("invented-holder").unwrap(),
    ));
    assert!(native_metadata(&mut capacity(), vec![holder]).is_err());
}
