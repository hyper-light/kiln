//! AC-T7.4.1/LB03: deterministic lifetime adversaries at the real placement/Runtime seam.
//! Counting delegates physical effects to SimRuntime; these are not native backend tests.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
enum LifetimeCall {
    Inspect(BoxId),
    Create(BoxId),
    Delete(BoxId),
}

#[derive(Default)]
struct LifetimeRuntime {
    counting: Counting,
    calls: Mutex<Vec<LifetimeCall>>,
}

impl LifetimeRuntime {
    fn take_calls(&self) -> Vec<LifetimeCall> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

impl Runtime for LifetimeRuntime {
    fn classes(&self) -> Vec<hex_api::Class> {
        self.counting.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.calls
            .lock()
            .unwrap()
            .push(LifetimeCall::Create(spec.id.clone()));
        self.counting.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.counting.start(id)
    }
    fn exec(&self, id: &BoxId, p: Program, s: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.counting.exec(id, p, s)
    }
    fn run(&self, b: BoxSpec, p: Program, s: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.counting.run(b, p, s)
    }
    fn signal(&self, id: &BoxId, s: Signal) -> Result<(), Refusal> {
        self.counting.signal(id, s)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.calls
            .lock()
            .unwrap()
            .push(LifetimeCall::Inspect(id.clone()));
        self.counting.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.counting.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.counting.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.calls
            .lock()
            .unwrap()
            .push(LifetimeCall::Delete(id.clone()));
        self.counting.delete(id)
    }
}

// The same two-box node as try_boot_with_budget, with callback identities retained through boot.
fn lifetime_boot(dir: &Path, runtime: Arc<LifetimeRuntime>, budget: Budget) -> Driver {
    let (demand, _) = request();
    Driver::boot(
        NodeId(0),
        &[NodeSpec::new(
            NodeId(0),
            Resources {
                cpu_milli: demand.need.cpu_milli * 2,
                mem_bytes: demand.need.mem_bytes * 2,
                ..Resources::default()
            },
        )],
        &[],
        budget,
        Pick::default(),
        dir,
        Runtimes::new([(Class::Oci, runtime as Arc<dyn Runtime>)]).unwrap(),
    )
    .unwrap()
}

/// AC-T7.4.1/LB03: a captured cancellation replayed after reboot and seq=0 reuse only
/// addresses its original namespace/epoch. The equal-key sibling still executes and admits work.
#[test]
fn replayed_cancel_after_reboot_and_sequence_reuse_cannot_touch_sibling_namespace() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let rt = Arc::new(LifetimeRuntime::default());
    let budget = Budget { owned: 4, ran: 4 };
    let first = lifetime_boot(a.path(), rt.clone(), budget);
    let sibling = lifetime_boot(b.path(), rt.clone(), budget);
    let (d, p) = request();
    let (old, step) = first.submit(d, p.clone()).unwrap();
    local(&first, step);
    let (other, step) = sibling.submit(d, p.clone()).unwrap();
    local(&sibling, step);
    assert_eq!(old, other);
    let old_id = first.physical_id(old).unwrap();
    let sibling_id = sibling.physical_id(other).unwrap();
    assert_ne!(old_id, sibling_id);
    let sibling_rows = durable_placement_files(b.path());
    let namespace = first.namespace().to_owned();
    assert_eq!(
        rt.take_calls(),
        [
            LifetimeCall::Inspect(old_id.clone()),
            LifetimeCall::Create(old_id.clone()),
            LifetimeCall::Inspect(sibling_id.clone()),
            LifetimeCall::Create(sibling_id.clone()),
        ]
    );

    let deleting = first.delete(old).unwrap();
    let (_, stale_cancel) = deleting.send.first().unwrap().clone();
    // Driver::submit records the payload's runtime form in the durable demand it sends.
    let durable = Demand {
        ask: Ask {
            runtime_class: Some(hex_api::Class::Process),
            ..d.ask
        },
        ..d
    };
    assert_eq!(
        stale_cancel.msg,
        Msg::Delete {
            key: old,
            demand: durable
        }
    );
    local(&first, deleting);
    assert!(first.status(old).unwrap().is_none());
    drop(first);
    let first = lifetime_boot(a.path(), rt.clone(), budget);
    assert_eq!(first.namespace(), namespace);
    let (current, step) = first.submit(d, p.clone()).unwrap();
    let (_, repeated_start) = step.send.first().unwrap().clone();
    local(&first, step);
    let current_id = first.physical_id(current).unwrap();
    assert_eq!(old.seq, current.seq);
    assert_ne!(old.epoch, current.epoch);
    assert_ne!(old_id, current_id);

    for _ in 0..2 {
        let replay = first.hear(NodeId(0), stale_cancel.clone()).unwrap();
        local(&first, replay);
        let replay = first.hear(NodeId(0), repeated_start.clone()).unwrap();
        local(&first, replay);
    }
    let status = first.poll_status().unwrap();
    assert!(matches!(
        status.notices.as_slice(),
        [Notice::Status { key, info }] if *key == current && info.id == current_id
    ));
    assert!(matches!(
        first.delete(old),
        Err(Error::Runtime(Refusal::NoSuchBox(id))) if id == old_id
    ));
    let calls = rt.take_calls();
    let rows_after_replay = durable_placement_files(b.path());
    let live_after_replay = rt.list().unwrap();
    let current_status = first.status(current).unwrap().unwrap();
    let sibling_status = sibling.status(other).unwrap().unwrap();

    let mut progress = rt
        .exec(&sibling_id, Program::of(&["fail", "7"]), Stdio::null())
        .unwrap();
    let sibling_exit = progress.try_wait().unwrap();
    let (next, step) = sibling.submit(d, p).unwrap();
    let progressed = local(&sibling, step);
    let next_id = sibling.physical_id(next).unwrap();
    local(&first, first.delete(current).unwrap());
    local(&sibling, sibling.delete(other).unwrap());
    local(&sibling, sibling.delete(next).unwrap());
    assert!(rt.list().unwrap().is_empty());
    assert_eq!(durable_placement_files(a.path()).len(), 1);
    assert_eq!(durable_placement_files(b.path()).len(), 1);

    assert_eq!(
        calls,
        [
            LifetimeCall::Delete(old_id.clone()),
            LifetimeCall::Inspect(current_id.clone()),
            LifetimeCall::Create(current_id.clone()),
            LifetimeCall::Delete(old_id.clone()),
            LifetimeCall::Delete(old_id),
            LifetimeCall::Inspect(current_id.clone()),
        ]
    );
    assert_eq!(rows_after_replay, sibling_rows);
    assert_eq!(live_after_replay.len(), 2);
    assert!(live_after_replay.iter().any(|info| info.id == sibling_id));
    assert!(live_after_replay.iter().any(|info| info.id == current_id));
    assert!(current_status.created && !current_status.deleting && !current_status.ended);
    assert!(sibling_status.created && !sibling_status.deleting && !sibling_status.ended);
    assert_eq!(sibling_exit, Some(hex_api::Exit::Code(7)));
    assert!(progressed.notices.contains(&Notice::Started {
        key: next,
        node: NodeId(0),
    }));
    assert_eq!(rt.counting.creates.load(Ordering::SeqCst), 4);
    assert_eq!(
        rt.take_calls(),
        [
            LifetimeCall::Inspect(next_id.clone()),
            LifetimeCall::Create(next_id.clone()),
            LifetimeCall::Delete(current_id),
            LifetimeCall::Delete(sibling_id),
            LifetimeCall::Delete(next_id),
        ]
    );
}

/// AC-T7.4.1/LB03: a create that committed physically but failed to report, followed by
/// failed cleanup, retains owner intent and runner charge through a smaller-budget reboot.
#[test]
fn failed_cleanup_and_delete_intent_survive_smaller_budget_restart_until_exact_delete() {
    let dir = tempfile::tempdir().unwrap();
    let rt = Arc::new(LifetimeRuntime::default());
    let driver = lifetime_boot(dir.path(), rt.clone(), Budget { owned: 2, ran: 2 });
    let (d, p) = request();
    rt.counting.fail_create.store(true, Ordering::SeqCst);
    rt.counting.fail_delete.store(true, Ordering::SeqCst);
    let (uncertain, step) = driver.submit(d, p.clone()).unwrap();
    let unknown = local(&driver, step);
    assert!(matches!(
        unknown.notices.as_slice(),
        [Notice::Unknown {
            key,
            refusal: Refusal::Os { op: "test create", .. },
            cleanup: Some(Refusal::Os { op: "test delete", .. }),
        }] if *key == uncertain
    ));
    let uncertain_id = driver.physical_id(uncertain).unwrap();
    assert_eq!(
        rt.counting.sim.inspect(&uncertain_id).unwrap().state,
        BoxState::Empty
    );
    let pending = driver.status(uncertain).unwrap().unwrap();
    assert!(pending.local && !pending.created && !pending.ended);

    rt.counting.fail_create.store(false, Ordering::SeqCst);
    let (survivor, step) = driver.submit(d, p.clone()).unwrap();
    local(&driver, step);
    let survivor_id = driver.physical_id(survivor).unwrap();
    let before_restart = local(&driver, driver.delete(uncertain).unwrap());
    assert!(matches!(
        before_restart.notices.as_slice(),
        [Notice::Unknown {
            key,
            refusal: Refusal::Os { op: "test delete", .. },
            cleanup: None,
        }] if *key == uncertain
    ));
    assert_eq!(
        rt.take_calls(),
        [
            LifetimeCall::Inspect(uncertain_id.clone()),
            LifetimeCall::Create(uncertain_id.clone()),
            LifetimeCall::Delete(uncertain_id.clone()),
            LifetimeCall::Inspect(survivor_id.clone()),
            LifetimeCall::Create(survivor_id.clone()),
            LifetimeCall::Delete(uncertain_id.clone()),
        ]
    );
    assert_eq!(driver.owned().unwrap().len(), 2);
    assert_eq!(
        driver.state.lock().unwrap().node.admitted(),
        [d.need.cpu_milli * 2, d.need.mem_bytes * 2, 0, 0]
    );
    let namespace = driver.namespace().to_owned();
    drop(driver);

    let driver = lifetime_boot(dir.path(), rt.clone(), Budget { owned: 1, ran: 1 });
    let recovered = local(&driver, driver.take_recovered().unwrap());
    assert!(matches!(
        recovered.notices.as_slice(),
        [Notice::Unknown {
            key,
            refusal: Refusal::Os { op: "test delete", .. },
            cleanup: None,
        }, Notice::Started { key: other, node: NodeId(0) }]
        if *key == uncertain && *other == survivor
    ));
    assert_eq!(driver.namespace(), namespace);
    assert_eq!(driver.control_bound().unwrap(), 5);
    assert_eq!(
        rt.take_calls(),
        [
            LifetimeCall::Inspect(uncertain_id.clone()),
            LifetimeCall::Inspect(survivor_id.clone()),
            LifetimeCall::Delete(uncertain_id.clone()),
        ]
    );
    let rows = durable_placement_files(dir.path());
    assert_eq!(rows.len(), 5);
    // A ran row names its owner and its explicit device binding: none, for this CPU-only box.
    for (kind, prefix) in [("own", "deleting 0 "), ("ran", "bindings1:0:0 running ")] {
        let value: serde_json::Value =
            serde_json::from_slice(rows.get(&format!("{}.rec", name(kind, uncertain))).unwrap())
                .unwrap();
        assert!(value["row"].as_str().unwrap().starts_with(prefix));
    }
    let pending = driver.status(uncertain).unwrap().unwrap();
    assert!(pending.local && pending.created && pending.deleting && !pending.ended);
    assert_eq!(pending.physical, uncertain_id);
    assert_eq!(
        driver.status(survivor).unwrap().unwrap().physical,
        survivor_id
    );
    assert_eq!(
        driver.state.lock().unwrap().node.admitted(),
        [d.need.cpu_milli * 2, d.need.mem_bytes * 2, 0, 0]
    );
    let still_unknown = local(&driver, driver.delete(uncertain).unwrap());
    assert!(matches!(
        still_unknown.notices.as_slice(),
        [Notice::Unknown { key, cleanup: None, .. }] if *key == uncertain
    ));
    assert!(matches!(
        driver.submit(d, p.clone()),
        Err(Error::Core(Stop::Full {
            what: "owned records",
            bound: 1
        }))
    ));
    assert_eq!(durable_placement_files(dir.path()), rows);
    assert_eq!(
        rt.counting.sim.inspect(&uncertain_id).unwrap().id,
        uncertain_id
    );
    assert_eq!(
        rt.counting.sim.inspect(&survivor_id).unwrap().id,
        survivor_id
    );
    assert_eq!(rt.list().unwrap().len(), 2);
    assert_eq!(rt.counting.creates.load(Ordering::SeqCst), 2);
    assert_eq!(
        rt.take_calls(),
        [LifetimeCall::Delete(uncertain_id.clone())]
    );

    rt.counting.fail_delete.store(false, Ordering::SeqCst);
    let ended = local(&driver, driver.delete(uncertain).unwrap());
    assert!(ended.notices.contains(&Notice::Ended { key: uncertain }));
    assert!(matches!(
        rt.counting.sim.inspect(&uncertain_id),
        Err(Refusal::NoSuchBox(id)) if id == uncertain_id
    ));
    assert!(driver.status(uncertain).unwrap().is_none());
    assert_eq!(driver.owned().unwrap(), [(survivor, Some(NodeId(0)))]);
    assert_eq!(durable_placement_files(dir.path()).len(), 3);
    assert_eq!(
        driver.state.lock().unwrap().node.admitted(),
        [d.need.cpu_milli, d.need.mem_bytes, 0, 0]
    );

    // Drive an accidental read-floor admission to Runtime.create before checking it.
    let (extra, refusal) = match driver.submit(d, p.clone()) {
        Ok((extra, step)) => {
            local(&driver, step);
            (Some(extra), None)
        }
        Err(why) => (None, Some(why)),
    };
    let creates_at_equality = rt.counting.creates.load(Ordering::SeqCst);
    let live_at_equality = rt.list().unwrap().len();
    if let Some(extra) = extra {
        local(&driver, driver.delete(extra).unwrap());
    }
    let mut progress = rt
        .exec(&survivor_id, Program::of(&["fail", "7"]), Stdio::null())
        .unwrap();
    let survivor_exit = progress.try_wait().unwrap();
    local(&driver, driver.delete(survivor).unwrap());
    assert!(rt.list().unwrap().is_empty());
    let (fresh, step) = driver.submit(d, p).unwrap();
    let admitted = local(&driver, step);
    let fresh_id = driver.physical_id(fresh).unwrap();
    assert_eq!(rt.counting.sim.inspect(&fresh_id).unwrap().id, fresh_id);
    local(&driver, driver.delete(fresh).unwrap());
    assert!(rt.list().unwrap().is_empty());
    assert!(driver.owned().unwrap().is_empty());
    assert_eq!(driver.state.lock().unwrap().node.admitted(), [0, 0, 0, 0]);
    assert_eq!(durable_placement_files(dir.path()).len(), 1);

    assert_eq!(
        creates_at_equality, 2,
        "recovery read floor admitted a physical replacement"
    );
    assert_eq!(live_at_equality, 1);
    assert!(matches!(
        refusal,
        Some(Error::Core(Stop::Full {
            what: "owned records",
            bound: 1
        }))
    ));
    assert_eq!(survivor_exit, Some(hex_api::Exit::Code(7)));
    assert!(admitted.notices.contains(&Notice::Started {
        key: fresh,
        node: NodeId(0),
    }));
    assert_eq!(rt.counting.creates.load(Ordering::SeqCst), 3);
    assert_eq!(rt.counting.deletes.load(Ordering::SeqCst), 7);
    assert_eq!(
        rt.take_calls(),
        [
            LifetimeCall::Delete(uncertain_id),
            LifetimeCall::Delete(survivor_id),
            LifetimeCall::Inspect(fresh_id.clone()),
            LifetimeCall::Create(fresh_id.clone()),
            LifetimeCall::Delete(fresh_id),
        ]
    );
}
