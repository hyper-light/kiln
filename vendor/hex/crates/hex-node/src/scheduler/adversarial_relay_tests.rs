use super::*;
use hex_api::{Capacity, CapacityResources, Network, OciSpec, Rootfs, RuntimeCost};

fn relay_class() -> hex_sim::model::Class {
    if cfg!(target_os = "macos") {
        hex_sim::model::Class::Shards
    } else {
        hex_sim::model::Class::Oci
    }
}

// Placement refuses a payload of another class than its placement's (placement::validate).
fn relay_payload_class(spec: OciSpec) -> ClassSpec {
    let spec = Box::new(spec);
    match relay_class() {
        hex_sim::model::Class::Oci => ClassSpec::Oci(spec),
        hex_sim::model::Class::Firecracker => ClassSpec::Firecracker(spec),
        hex_sim::model::Class::Shards => ClassSpec::Shards(spec),
    }
}

fn relay_need() -> Resources {
    Resources {
        cpu_milli: u64::from(hex_api::MIN_CPU_MILLI),
        mem_bytes: hex_api::MIN_MEMORY,
        ..Resources::default()
    }
}

// An initialized microVM class reports its verified per-box cost (capacity::profiles), and a
// runner admits only a Start carrying the receipt that cost allocates. This identity charge keeps
// each box at its demand; OCI keeps the uncosted path.
fn relay_cost() -> Option<RuntimeCost> {
    if relay_class() == hex_sim::model::Class::Oci {
        return None;
    }
    let need = hex_sim::model::capacity_resources(relay_need());
    Some(RuntimeCost {
        id: [0x72; 32],
        floor: need,
        ceiling: need,
        quantum: CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        guest_overhead: CapacityResources::default(),
        overhead: CapacityResources::default(),
    })
}

fn relay_allocation() -> Option<hex_api::Allocation> {
    relay_cost().map(|cost| {
        cost.allocate(hex_sim::model::capacity_resources(relay_need()))
            .unwrap()
    })
}

// A runner as its owner sees it: room for one relay box, charged at the runner's own cost.
fn relay_peer(id: NodeId) -> NodeSpec {
    let mut node = NodeSpec::new(id, relay_need());
    node.costs[relay_class().index()] = relay_cost();
    node
}

#[derive(Default)]
struct RelayRuntime {
    sim: hex_api::sim::SimRuntime,
    creates: AtomicUsize,
    deletes: AtomicUsize,
}

impl Runtime for RelayRuntime {
    fn physical_report(&self) -> Result<hex_api::PhysicalReport, Refusal> {
        self.sim.physical_report()
    }
    fn capacity(&self, _: hex_api::Class) -> Result<Option<Capacity>, Refusal> {
        // Room for the two relay boxes one runner holds at once.
        let need = hex_sim::model::capacity_resources(relay_need());
        let room = need.checked_add(need).unwrap();
        Ok(relay_cost().map(|cost| Capacity {
            id: "relay-fixture-pool".into(),
            total: room,
            free: room,
            devices: Vec::new(),
            cost: Some(cost),
            routes: Vec::new(),
            device_routes: Vec::new(),
            backings: Vec::new(),
            unavailable: Vec::new(),
            learned: None,
        }))
    }
    fn classes(&self) -> Vec<hex_api::Class> {
        self.sim.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        self.sim.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.start(id)
    }
    fn exec(&self, id: &BoxId, p: Program, s: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.sim.exec(id, p, s)
    }
    fn run(&self, spec: BoxSpec, p: Program, s: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.sim.run(spec, p, s)
    }
    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        self.sim.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.sim.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.sim.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.sim.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        self.sim.delete(id)
    }
}

fn relay_start(key: Key) -> Envelope {
    Envelope {
        msg: peer::Msg::Start {
            route: None,
            allocation: relay_allocation(),
            key,
            demand: Demand {
                need: relay_need(),
                kind: Kind::Plain,
                class: relay_class(),
                ask: Ask::default(),
            },
        },
        payload: Some(Payload {
            spec: BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: BoxId::new("relay-caller").unwrap(),
                bounds: Bounds {
                    cpu_max_milli: Some(hex_api::MIN_CPU_MILLI),
                    memory_max: Some(hex_api::MIN_MEMORY),
                    ..Bounds::default()
                },
                class: relay_payload_class(OciSpec {
                    rootfs: Rootfs::Dir(std::env::current_dir().unwrap()),
                    init: Program::of(&["wait"]),
                    mounts: Vec::new(),
                    hostname: None,
                    network: Network::Private,
                    hosts: Vec::new(),
                }),
                owner: None,
            },
            start: false,
        }),
        refusal: None,
    }
}

fn relay_receiver(dir: &Path, runtime: Arc<RelayRuntime>) -> (Arc<Service>, [Receiver<Queued>; 2]) {
    let mut owner = NodeSpec::new(NodeId(0), Resources::default());
    owner.classes = Classes::NONE;
    let mut other = NodeSpec::new(NodeId(2), Resources::default());
    other.classes = Classes::NONE;
    let cluster = Cluster::single_owner(vec![owner, relay_peer(NodeId(1)), other], Vec::new());
    let (send, receive) = channel();
    let mut service = advertise_timing_service(
        NodeId(1),
        dir,
        cluster,
        Runtimes::new([(relay_class(), runtime as Arc<dyn Runtime>)]).unwrap(),
        send,
    );
    let (other_send, other_receive) = channel();
    let owned = Arc::get_mut(&mut service).unwrap();
    owned.initialized = Classes::NONE.with(relay_class());
    owned.senders.insert(NodeId(2), other_send);
    owned
        .refreshing
        .insert(NodeId(2), Arc::new(AtomicBool::new(false)));
    owned.stopping = Arc::new(Stopping::new([0, 2]));
    owned.order = Mutex::new(
        order::Order::open_with_peers(
            &dir.join("transport"),
            &std::collections::BTreeSet::from([0, 2]),
        )
        .unwrap(),
    );
    (service, [receive, other_receive])
}

fn relay_hear(
    service: &Service,
    message: &Envelope,
    epoch: u64,
    seq: u64,
) -> Result<dto::Reply, Refusal> {
    service.command(dto::Command::Hear {
        from: 0,
        packet: packet(message, &[], epoch, seq, Vec::new(), &service.realm).unwrap(),
    })
}

fn relay_control(msg: peer::Msg) -> Envelope {
    Envelope {
        msg,
        payload: None,
        refusal: None,
    }
}

/// AC-T7.4.1/LB03: transport fencing precedes driver effects. A wrong-owner refusal cannot
/// create a box, block another configured peer, or block the sender's next valid stamp.
/// The earlier unchanged-order candidate and its exact result remain separately at dc29.
#[test]
fn wrong_owner_start_has_no_effect_and_other_peer_and_later_stamp_progress() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(RelayRuntime::default());
    let (service, feedback) = relay_receiver(dir.path(), Arc::clone(&runtime));
    let key = Key {
        owner: 0,
        epoch: 1,
        seq: 1,
    };
    let start = relay_start(key);
    relay_hear(&service, &relay_control(peer::Msg::Holds { key }), 1, 1).unwrap();
    let row = dir.path().join("transport/peer-0.rec");
    let before = std::fs::read(&row).unwrap();
    let rejected = relay_hear(&service, &relay_start(Key { owner: 1, ..key }), 2, 1);
    let after = std::fs::read(&row).unwrap();
    let effects_after_refusal = (
        runtime.creates.load(Ordering::SeqCst),
        runtime.deletes.load(Ordering::SeqCst),
        runtime.list().unwrap().len(),
    );
    let corrected = relay_hear(&service, &start, 2, 1);
    let other_key = Key { owner: 2, ..key };
    let other_start = relay_start(other_key);
    let other_reply = service.command(dto::Command::Hear {
        from: 2,
        packet: packet(&other_start, &[], 1, 1, Vec::new(), &service.realm).unwrap(),
    });
    let other_created = service
        .driver
        .status(other_key)
        .unwrap()
        .is_some_and(|s| s.created);
    let other_row = dir.path().join("transport/peer-2.rec");
    let other_order = std::fs::read(&other_row).unwrap();
    relay_hear(&service, &start, 2, 2).unwrap();
    let created = service.driver.status(key).unwrap().unwrap().created;
    let peer::Msg::Start { demand, .. } = start.msg else {
        panic!("fixture constructed a non-Start message")
    };
    relay_hear(
        &service,
        &relay_control(peer::Msg::Delete { key, demand }),
        2,
        3,
    )
    .unwrap();
    relay_hear(&service, &relay_control(peer::Msg::Forget { key }), 2, 4).unwrap();
    let other_order_unchanged = std::fs::read(&other_row).unwrap() == other_order;
    for (seq, msg) in [
        (
            2,
            peer::Msg::Delete {
                key: other_key,
                demand,
            },
        ),
        (3, peer::Msg::Forget { key: other_key }),
    ] {
        service
            .command(dto::Command::Hear {
                from: 2,
                packet: packet(&relay_control(msg), &[], 1, seq, Vec::new(), &service.realm)
                    .unwrap(),
            })
            .unwrap();
    }
    drop(feedback);
    eprintln!(
        "wrong-owner refused={rejected:?}; order_unchanged={}; corrected={corrected:?}; other_reply={other_reply:?}; other_created={other_created}; later_created={created}; effects_after_refusal={effects_after_refusal:?}; cleanup_boxes={}; cleanup_rows=({},{}); permits={}",
        before == after,
        runtime.list().unwrap().len(),
        service.driver.status(key).unwrap().is_some(),
        service.driver.status(other_key).unwrap().is_some(),
        service.queued.load(Ordering::Acquire)
    );
    assert!(matches!(
        rejected,
        Err(Refusal::Invalid {
            what: "placement",
            ..
        })
    ));
    assert_eq!(effects_after_refusal, (0, 0, 0));
    assert!(created && other_created && other_reply.is_ok() && other_order_unchanged);
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.deletes.load(Ordering::SeqCst), 2);
    assert!(runtime.list().unwrap().is_empty());
    assert!(service.driver.status(key).unwrap().is_none());
    assert!(service.driver.status(other_key).unwrap().is_none());
    assert_eq!(service.queued.load(Ordering::Acquire), 0);
    assert_ne!(
        after, before,
        "transport stamp precedes logical driver refusal"
    );
    assert!(
        matches!(corrected, Err(Refusal::Broken(_))),
        "changed bytes at a consumed stamp must refuse"
    );
}

/// AC-T7.4.1/LB03: retained receive order fences an old Start after physical Delete, Forget,
/// receiver restart and a newer sender epoch. Refused identity/retry changes must not consume it.
#[test]
fn delayed_start_stays_fenced_through_forget_restart_and_new_sender_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(RelayRuntime::default());
    let (service, feedback) = relay_receiver(dir.path(), Arc::clone(&runtime));
    let key = Key {
        owner: 0,
        epoch: 1,
        seq: 1,
    };
    let start = relay_start(key);
    relay_hear(&service, &start, 1, 1).unwrap();
    let row = dir.path().join("transport/peer-0.rec");
    let first = std::fs::read(&row).unwrap();
    relay_hear(&service, &start, 1, 1).unwrap();
    let repeated = std::fs::read(&row).unwrap();
    let mut changed = start.clone();
    changed.payload.as_mut().unwrap().spec.id = BoxId::new("changed-at-same-stamp").unwrap();
    let changed_reply = relay_hear(&service, &changed, 1, 1);
    let after_changed = std::fs::read(&row).unwrap();
    let peer::Msg::Start { demand, .. } = start.msg else {
        panic!("fixture constructed a non-Start message")
    };
    relay_hear(
        &service,
        &relay_control(peer::Msg::Delete { key, demand }),
        1,
        2,
    )
    .unwrap();
    relay_hear(&service, &start, 1, 1).unwrap();
    let ended_after_delayed = service.driver.status(key).unwrap().unwrap().ended;
    relay_hear(&service, &relay_control(peer::Msg::Forget { key }), 1, 3).unwrap();
    let forgotten = service.driver.status(key).unwrap().is_none();
    let retained = std::fs::read(&row).unwrap();
    let receiver_epoch = service.order.lock().unwrap().epoch;
    drop(feedback);
    drop(service);

    let (service, feedback) = relay_receiver(dir.path(), Arc::clone(&runtime));
    let restarted = service.order.lock().unwrap().epoch > receiver_epoch;
    relay_hear(&service, &start, 1, 1).unwrap();
    let after_restart = std::fs::read(&row).unwrap();
    let old_key_absent = service.driver.status(key).unwrap().is_none();
    let next_key = Key { seq: 2, ..key };
    let next_start = relay_start(next_key);
    let mut foreign_realm = packet(&next_start, &[], 2, 1, Vec::new(), &service.realm).unwrap();
    foreign_realm.cluster = "foreign-realm".into();
    let realm_reply = service.command(dto::Command::Hear {
        from: 0,
        packet: foreign_realm,
    });
    let sender_reply = service.command(dto::Command::Hear {
        from: 3,
        packet: packet(&next_start, &[], 2, 1, Vec::new(), &service.realm).unwrap(),
    });
    let after_refusals = std::fs::read(&row).unwrap();
    let unknown_sender_absent = !dir.path().join("transport/peer-3.rec").exists();
    relay_hear(&service, &relay_control(peer::Msg::Holds { key }), 2, 1).unwrap();
    let new_epoch = std::fs::read(&row).unwrap();
    relay_hear(&service, &next_start, 1, u64::MAX).unwrap();
    let after_old_epoch = std::fs::read(&row).unwrap();
    let fenced_effects = (
        runtime.creates.load(Ordering::SeqCst),
        runtime.deletes.load(Ordering::SeqCst),
        runtime.list().unwrap().len(),
    );
    relay_hear(&service, &next_start, 2, 2).unwrap();
    let later_created = service.driver.status(next_key).unwrap().unwrap().created;
    relay_hear(
        &service,
        &relay_control(peer::Msg::Delete {
            key: next_key,
            demand,
        }),
        2,
        3,
    )
    .unwrap();
    relay_hear(
        &service,
        &relay_control(peer::Msg::Forget { key: next_key }),
        2,
        4,
    )
    .unwrap();
    // No external resources exist: only this simulator's boxes and owned temporary record files.
    let leftovers = runtime.list().unwrap();
    for info in &leftovers {
        runtime.delete(&info.id).unwrap();
    }
    drop(feedback);
    eprintln!(
        "delayed-start changed={changed_reply:?}; realm={realm_reply:?}; sender={sender_reply:?}; restarted={restarted}; old_key_absent={old_key_absent}; fenced_effects={fenced_effects:?}; later_created={later_created}; cleanup_leftovers={}; permits={}",
        leftovers.len(),
        service.queued.load(Ordering::Acquire)
    );
    assert!(matches!(changed_reply, Err(Refusal::Broken(_))));
    assert_eq!(first, repeated);
    assert_eq!(first, after_changed);
    assert!(ended_after_delayed && forgotten && restarted && old_key_absent);
    assert_eq!(retained, after_restart);
    assert!(matches!(
        realm_reply,
        Err(Refusal::Invalid {
            what: "scheduler cluster",
            ..
        })
    ));
    assert!(matches!(
        sender_reply,
        Err(Refusal::Invalid {
            what: "scheduler sender",
            ..
        })
    ));
    assert!(unknown_sender_absent);
    assert_eq!(retained, after_refusals);
    assert_eq!(new_epoch, after_old_epoch);
    assert_eq!(fenced_effects, (1, 1, 0));
    assert!(later_created);
    assert!(leftovers.is_empty());
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.deletes.load(Ordering::SeqCst), 2);
    assert!(service.driver.status(next_key).unwrap().is_none());
    assert_eq!(service.queued.load(Ordering::Acquire), 0);
}

fn relay_owner(dir: &Path, cluster: Cluster) -> (Arc<Service>, [Receiver<Queued>; 2]) {
    let (send, receive) = channel();
    let mut service = advertise_timing_service(NodeId(0), dir, cluster, Runtimes::default(), send);
    let (other_send, other_receive) = channel();
    let owned = Arc::get_mut(&mut service).unwrap();
    owned.senders.insert(NodeId(2), other_send);
    owned
        .refreshing
        .insert(NodeId(2), Arc::new(AtomicBool::new(false)));
    owned.stopping = Arc::new(Stopping::new([1, 2]));
    owned.order = Mutex::new(
        order::Order::open_with_peers(
            &dir.join("transport"),
            &std::collections::BTreeSet::from([1, 2]),
        )
        .unwrap(),
    );
    (service, [receive, other_receive])
}

fn relay_take(receive: &Receiver<Queued>) -> Envelope {
    let word = receive.try_recv().unwrap();
    word.envelope.clone().unwrap()
}

fn relay_feedback(owner: &Service, from: u32, message: &Envelope, seq: u64) {
    owner
        .command(dto::Command::Hear {
            from,
            packet: packet(message, &[], 1, seq, Vec::new(), &owner.realm).unwrap(),
        })
        .unwrap();
}

/// AC-T7.4.1/LB03: two peer queues share permits even while Start's reply is held/lost. A full
/// queue refuses immediate Delete publication, but durable cancellation reconciles on reconnect.
/// Drop returns both permits; reboot retains both rows, and each peer can finish exact cleanup.
#[test]
fn cancellation_reconciles_lost_start_reply_without_losing_either_queued_lane() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(RelayRuntime::default());
    let (runner, [feedback, unused_feedback]) =
        relay_receiver(&dir.path().join("runner"), Arc::clone(&runtime));
    let template = relay_start(Key {
        owner: 0,
        epoch: 1,
        seq: 1,
    });
    let peer::Msg::Start { demand, .. } = template.msg else {
        panic!("fixture constructed a non-Start message")
    };
    let runtime_class = template.payload.as_ref().map(|p| p.spec.class.class());
    let mut local = NodeSpec::new(NodeId(0), Resources::default());
    local.classes = Classes::NONE;
    let cluster = Cluster::single_owner(
        vec![local, relay_peer(NodeId(1)), relay_peer(NodeId(2))],
        Vec::new(),
    );
    let owner_dir = dir.path().join("owner");
    let (owner, [first_receive, second_receive]) = relay_owner(&owner_dir, cluster.clone());
    // Exactly two real owner placements occupy the two lanes; this is not a fabricated permit.
    owner.control_limit.store(2, Ordering::Release);
    let (first, first_step) = owner
        .driver
        .submit(demand, template.payload.clone().unwrap())
        .unwrap();
    let (second, second_step) = owner
        .driver
        .submit(demand, template.payload.unwrap())
        .unwrap();
    owner.dispatch(first_step).unwrap();
    owner.dispatch(second_step).unwrap();
    let pending = Arc::clone(&owner.queued);
    let epoch = owner.order.lock().unwrap().epoch;
    let trace = Arc::new(Mutex::new(Vec::new()));
    let failures = Arc::new(Mutex::new(Vec::new()));
    let (started, saw_start) = sync_channel(0);
    let (release_start, start_released) = sync_channel(0);
    let (deleted, saw_delete) = sync_channel(0);
    let (release_delete, delete_released) = sync_channel(0);
    let lane = PeerLane {
        peer: Peer {
            node: 1,
            host: "unused-owned-channel".into(),
            command: "unused-owned-command".into(),
        },
        id: NodeId(0),
        realm: owner.realm.clone(),
        models: Vec::new(),
        epoch,
        stopping: Arc::clone(&owner.stopping),
        report: {
            let failures = Arc::clone(&failures);
            Arc::new(move |_, why| failures.lock().unwrap().push(why.to_owned()))
        },
        source: Arc::new(Mutex::new(Arc::downgrade(&owner))),
    };
    let worker = {
        let runner = Arc::clone(&runner);
        let trace = Arc::clone(&trace);
        std::thread::spawn(move || {
            let mut opens = 0;
            let now = std::time::Instant::now();
            run_peer_with_time(
                lane,
                first_receive,
                |_, _| {
                    opens += 1;
                    Ok(opens)
                },
                |connection, command| {
                    let (from, incoming) = match command {
                        dto::Command::Hear { from, packet } => (from, packet),
                        dto::Command::Describe { from, cluster } => {
                            assert_eq!(from, 0);
                            assert_eq!(cluster, runner.realm);
                            return Ok(Ok(dto::Reply::default()));
                        }
                        _ => panic!("unexpected owned worker command"),
                    };
                    let message =
                        peer::decode_message(&incoming.message, &[], wire::MAX_MESSAGE).unwrap();
                    trace.lock().unwrap().push((*connection, message));
                    let result = runner.command(dto::Command::Hear {
                        from,
                        packet: incoming,
                    });
                    match message {
                        peer::Msg::Start { .. } => {
                            started.send(()).unwrap();
                            start_released.recv().unwrap();
                            // The peer actually applied Start, but its response is lost.
                            Err(Refusal::Broken("owned fixture lost Start reply".into()))
                        }
                        peer::Msg::Delete { .. } => {
                            deleted.send(()).unwrap();
                            delete_released.recv().unwrap();
                            Ok(result)
                        }
                        _ => panic!("unexpected owned worker message"),
                    }
                },
                || now,
                |_, _| Ok(false),
            );
        })
    };
    saw_start.recv().unwrap();
    let in_flight = pending.load(Ordering::Acquire);
    let started_effects = runtime.creates.load(Ordering::SeqCst);
    let lost_started = relay_take(&feedback);
    let cancellation = owner
        .command(dto::Command::Delete {
            key: external(first),
        })
        .unwrap();
    let deleting = owner.driver.status(first).unwrap().unwrap().deleting;
    let after_refused_publication = pending.load(Ordering::Acquire);
    release_start.send(()).unwrap();
    saw_delete.recv().unwrap();
    let after_reconnect = pending.load(Ordering::Acquire);
    let ended = relay_take(&feedback);
    let runner_ended = runner.driver.status(first).unwrap().unwrap().ended;
    let second_before = owner.driver.status(second).unwrap().unwrap();
    let second_row = owner_dir.join(format!(
        "placements/own-{}-{}-{}.rec",
        second.owner, second.epoch, second.seq
    ));
    let second_bytes = std::fs::read(&second_row).unwrap();
    drop(owner); // Close only these senders while the worker owns its in-flight word.
    release_delete.send(()).unwrap();
    worker.join().unwrap();
    let after_worker_drop = pending.load(Ordering::Acquire);
    let second_start = relay_take(&second_receive);
    drop(second_receive);
    let after_both_drops = pending.load(Ordering::Acquire);

    let (owner, [out_first, out_second]) = relay_owner(&owner_dir, cluster.clone());
    owner.control_limit.store(2, Ordering::Release);
    let recovered_first = owner.driver.status(first).unwrap().unwrap().deleting;
    let recovered_second = owner.driver.status(second).unwrap().unwrap();
    let second_bytes_retained = std::fs::read(&second_row).unwrap() == second_bytes;
    let new_epoch = owner.order.lock().unwrap().epoch;
    owner.command(dto::Command::LinkUp { peer: 1 }).unwrap();
    let replayed_delete = relay_take(&out_first);
    relay_hear(&runner, &replayed_delete, new_epoch, 1).unwrap();
    let replayed_ended = relay_take(&feedback);
    relay_feedback(&owner, 1, &replayed_ended, 1);
    let forget = relay_take(&out_first);
    relay_hear(&runner, &forget, new_epoch, 2).unwrap();

    // The other real placement survived queue drop/reboot and can still progress independently.
    let other_runtime = Arc::new(RelayRuntime::default());
    let (other_send, other_feedback) = channel();
    let mut other = advertise_timing_service(
        NodeId(2),
        &dir.path().join("other"),
        cluster,
        Runtimes::new([(
            relay_class(),
            Arc::clone(&other_runtime) as Arc<dyn Runtime>,
        )])
        .unwrap(),
        other_send,
    );
    Arc::get_mut(&mut other).unwrap().initialized = Classes::NONE.with(relay_class());
    relay_hear(&other, &second_start, new_epoch, 1).unwrap();
    relay_feedback(&owner, 2, &relay_take(&other_feedback), 1);
    let second_created = owner.driver.status(second).unwrap().unwrap().created;
    owner
        .command(dto::Command::Delete {
            key: external(second),
        })
        .unwrap();
    relay_hear(&other, &relay_take(&out_second), new_epoch, 2).unwrap();
    relay_feedback(&owner, 2, &relay_take(&other_feedback), 2);
    relay_hear(&other, &relay_take(&out_second), new_epoch, 3).unwrap();
    drop(feedback);
    drop(unused_feedback);
    drop(other_feedback);
    drop(out_first);
    drop(out_second);
    let trace = trace.lock().unwrap().clone();
    eprintln!(
        "worker permits={in_flight}/{after_refused_publication}/{after_reconnect}/{after_worker_drop}/{after_both_drops}; deleting={deleting}; recovered=({recovered_first},{second_bytes_retained}); second_created={second_created}; trace={trace:?}; final_permits={}/{}/{}; boxes={}/{}",
        owner.queued.load(Ordering::Acquire),
        runner.queued.load(Ordering::Acquire),
        other.queued.load(Ordering::Acquire),
        runtime.list().unwrap().len(),
        other_runtime.list().unwrap().len()
    );
    assert_eq!(
        (
            in_flight,
            after_refused_publication,
            after_reconnect,
            after_worker_drop,
            after_both_drops
        ),
        (2, 2, 2, 1, 0)
    );
    assert_eq!(started_effects, 1);
    assert!(matches!(lost_started.msg, peer::Msg::Started { key } if key == first));
    assert_eq!(
        cancellation.refusals,
        vec![
            encoded_refusal(&Refusal::Exhausted {
                what: "queued peer messages",
                bound: 2
            })
            .unwrap()
        ]
    );
    assert!(deleting && runner_ended && recovered_first && second_bytes_retained && second_created);
    assert_eq!(second_before.node, Some(NodeId(2)));
    assert_eq!(recovered_second, second_before);
    assert!(matches!(ended.msg, peer::Msg::Ended { key } if key == first));
    // Driver::submit records the payload's runtime form in the durable demand the owner sends.
    let durable = Demand {
        ask: Ask {
            runtime_class,
            ..demand.ask
        },
        ..demand
    };
    assert_eq!(
        replayed_delete.msg,
        peer::Msg::Delete {
            key: first,
            demand: durable
        }
    );
    assert_eq!(
        trace,
        vec![
            (
                1,
                peer::Msg::Start {
                    route: None,
                    allocation: relay_allocation(),
                    key: first,
                    demand: durable
                }
            ),
            (
                2,
                peer::Msg::Delete {
                    key: first,
                    demand: durable
                }
            )
        ]
    );
    assert_eq!(
        *failures.lock().unwrap(),
        vec![Refusal::Broken("owned fixture lost Start reply".into()).to_string()]
    );
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.deletes.load(Ordering::SeqCst), 1);
    assert_eq!(other_runtime.creates.load(Ordering::SeqCst), 1);
    assert_eq!(other_runtime.deletes.load(Ordering::SeqCst), 1);
    assert!(runtime.list().unwrap().is_empty() && other_runtime.list().unwrap().is_empty());
    assert!(
        owner.driver.status(first).unwrap().is_none()
            && owner.driver.status(second).unwrap().is_none()
    );
    assert!(
        runner.driver.status(first).unwrap().is_none()
            && other.driver.status(second).unwrap().is_none()
    );
    assert_eq!(
        (
            owner.queued.load(Ordering::Acquire),
            runner.queued.load(Ordering::Acquire),
            other.queued.load(Ordering::Acquire)
        ),
        (0, 0, 0)
    );
}
