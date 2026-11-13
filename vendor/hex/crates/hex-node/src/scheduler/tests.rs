use super::*;
use hex_api::{Bounds, ClassSpec};
use hex_sim::model::{Ask, Demand, Kind, Resources};
use hex_sim::peer::Budget;
use std::sync::mpsc::sync_channel;

// A fixture's peer cache starts empty, so nothing in it is ever set aside.
fn no_aside(aside: cache::SetAside) {
    panic!("a fresh fixture peer cache set {aside} aside");
}

/// LB03: a reconnect republishes durable cancellation without killing that new connection.
#[test]
fn replayed_deletion_does_not_interrupt_the_reconnected_peer() {
    let dir = tempfile::tempdir().unwrap();
    let need = Resources {
        cpu_milli: u64::from(hex_api::MIN_CPU_MILLI),
        mem_bytes: hex_api::MIN_MEMORY,
        ..Resources::default()
    };
    let mut local = NodeSpec::new(NodeId(0), Resources::default());
    local.classes = Classes::NONE;
    let mut cluster =
        Cluster::single_owner(vec![local, NodeSpec::new(NodeId(1), need)], Vec::new());
    let driver = Driver::boot(
        NodeId(0),
        &cluster.nodes,
        &cluster.models,
        Budget {
            owned: hex_runtime::MAX_BOXES,
            ran: hex_runtime::MAX_BOXES,
        },
        Pick::default(),
        &dir.path().join("placements"),
        Runtimes::default(),
    )
    .unwrap();
    let (key, _) = driver
        .submit(
            Demand {
                need,
                kind: Kind::Plain,
                class: hex_sim::model::Class::Oci,
                ask: Ask::default(),
            },
            Payload {
                spec: BoxSpec {
                    route: None,
                    allocation: None,
                    devices: Vec::new(),
                    id: BoxId::new("cancel-me").unwrap(),
                    bounds: Bounds {
                        cpu_max_milli: Some(hex_api::MIN_CPU_MILLI),
                        memory_max: Some(hex_api::MIN_MEMORY),
                        ..Bounds::default()
                    },
                    class: ClassSpec::Process,
                    owner: None,
                    band: 0,
                },
                start: false,
            },
        )
        .unwrap();
    driver.delete(key).unwrap(); // Durable deletion, with its prior connection lost.
    let stopping = Arc::new(Stopping::new([1]));
    let (child, _input, _output) = stopping::tests::waiting_child();
    stopping.register(1, &child).unwrap();
    let (sender, receiver) = channel();
    let cache = cache::Cache::open(
        &dir.path().join("peers"),
        &std::collections::BTreeSet::from([1]),
        &mut cluster,
        no_aside,
    )
    .unwrap();
    let service = Service {
        id: NodeId(0),
        realm: "test".into(),
        command: Mutex::new(()),
        fault: Mutex::new(None),
        cache: Mutex::new(cache),
        _lease: std::fs::File::create(dir.path().join("lease")).unwrap(),
        order: Mutex::new(
            order::Order::open_with_peers(
                &dir.path().join("transport"),
                &std::collections::BTreeSet::from([1]),
            )
            .unwrap(),
        ),
        driver: Arc::new(driver),
        lane: None,
        runtimes: Runtimes::default(),
        cluster: Mutex::new(cluster),
        facts_dir: dir.path().to_path_buf(),
        initialized: Classes::NONE,
        physical_authority: None,
        stopping,
        queued: Arc::new(AtomicUsize::new(0)),
        control_limit: AtomicUsize::new(crate::records::MOST),
        senders: BTreeMap::from([(NodeId(1), sender)]),
        refreshing: BTreeMap::from([(NodeId(1), Arc::new(AtomicBool::new(false)))]),
    };
    service.command(dto::Command::LinkUp { peer: 1 }).unwrap();
    assert!(
        matches!(receiver.try_recv().unwrap().envelope, Some(Envelope { msg: peer::Msg::Delete { key: k, .. }, .. }) if k == key)
    );
    assert!(
        child.lock().unwrap().try_wait().unwrap().is_none(),
        "replayed Delete must keep the fresh connection alive"
    );
    // An explicit cancellation request still interrupts a stalled connection, before queue room.
    service
        .command(dto::Command::Delete { key: external(key) })
        .unwrap();
    assert!(child.lock().unwrap().try_wait().unwrap().is_some());
}

/// LB03/AC-T7.4.1: the production laptop dispatch drains the same durable messages that a peer
/// transport would send. Then the real common socket/client observes and deletes the actual box.
#[test]
fn laptop_dispatch_and_common_wire_keep_the_box_until_explicit_delete() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(hex_api::sim::SimRuntime::default());
    let need = Resources {
        cpu_milli: u64::from(hex_api::MIN_CPU_MILLI),
        mem_bytes: hex_api::MIN_MEMORY,
        ..Resources::default()
    };
    let cluster = Cluster::single_owner(vec![NodeSpec::new(NodeId(0), need)], Vec::new());
    let driver = Driver::boot(
        NodeId(0),
        &cluster.nodes,
        &cluster.models,
        Budget {
            owned: hex_runtime::MAX_BOXES,
            ran: hex_runtime::MAX_BOXES,
        },
        Pick::default(),
        &dir.path().join("placements"),
        Runtimes::new([(
            hex_sim::model::Class::Oci,
            runtime.clone() as Arc<dyn Runtime>,
        )])
        .unwrap(),
    )
    .unwrap();
    let cache = cache::Cache::open(
        &dir.path().join("peers"),
        &std::collections::BTreeSet::new(),
        &mut cluster.clone(),
        no_aside,
    )
    .unwrap();
    let service = Arc::new(Service {
        id: NodeId(0),
        realm: "test".into(),
        command: Mutex::new(()),
        fault: Mutex::new(None),
        cache: Mutex::new(cache),
        _lease: std::fs::File::create(dir.path().join("lease")).unwrap(),
        order: Mutex::new(
            order::Order::open_with_peers(
                &dir.path().join("transport"),
                &std::collections::BTreeSet::from([]),
            )
            .unwrap(),
        ),
        driver: Arc::new(driver),
        lane: None,
        runtimes: Runtimes::default(),
        cluster: Mutex::new(cluster),
        facts_dir: dir.path().to_path_buf(),
        initialized: Classes::NONE,
        physical_authority: None,
        stopping: Arc::new(Stopping::default()),
        queued: Arc::new(AtomicUsize::new(0)),
        control_limit: AtomicUsize::new(crate::records::MOST),
        senders: BTreeMap::new(),
        refreshing: BTreeMap::new(),
    });
    let demand = Demand {
        need,
        kind: Kind::Plain,
        class: hex_sim::model::Class::Oci,
        ask: Ask::default(),
    };
    let payload = Payload {
        spec: BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: BoxId::new("caller-name").unwrap(),
            bounds: Bounds {
                cpu_max_milli: Some(hex_api::MIN_CPU_MILLI),
                memory_max: Some(hex_api::MIN_MEMORY),
                ..Bounds::default()
            },
            class: ClassSpec::Process,
            owner: None,
            band: 0,
        },
        start: false,
    };
    let (key, step) = service.driver.submit(demand, payload).unwrap();
    assert!(runtime.list().unwrap().is_empty());
    let dispatch = service.dispatch(step).unwrap();
    assert!(dispatch.refusals.is_empty());
    assert!(
        dispatch
            .notices
            .iter()
            .any(|n| matches!(n, Notice::Started { key: k, .. } if *k == key))
    );
    let physical = service.driver.status(key).unwrap().unwrap().physical;
    assert_eq!(
        runtime.inspect(&physical).unwrap().state,
        hex_api::BoxState::Empty
    );

    #[cfg(target_os = "linux")]
    let platform = crate::sizes::Sizes::now().unwrap().platform();
    #[cfg(target_os = "macos")]
    let platform = crate::serve::macos::platform().unwrap();
    #[cfg(windows)]
    let platform = crate::serve::windows::platform().unwrap();
    let socket = dir.path().join("scheduler.sock");
    let server = crate::serve::Server::bind(
        &socket,
        service.clone() as Arc<dyn Host>,
        crate::serve::limits(&platform),
        platform.threads,
    )
    .unwrap();
    let client = hex_client::Client::connect(&socket).unwrap();
    // A second worker cannot publish Delete ahead of an in-progress command's Start.
    let gate = service.command.lock().unwrap();
    assert!(matches!(
        client.schedule(dto::Command::Delete { key: external(key) }),
        Err(Refusal::Exhausted {
            what: "scheduler command publication",
            ..
        })
    ));
    assert_eq!(runtime.list().unwrap().len(), 1);
    drop(gate);
    let status = client
        .schedule(dto::Command::Status { key: external(key) })
        .unwrap();
    assert!(matches!(
        client.schedule(dto::Command::Describe {
            from: 0,
            cluster: "another-cluster".into()
        }),
        Err(Refusal::Invalid {
            what: "scheduler cluster",
            ..
        })
    ));
    assert!(status.tracked);
    assert!(status.created);
    assert!(!status.ended);
    assert_eq!(status.physical, Some(physical.clone()));
    assert!(matches!(
        client.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: BoxId::new("bypass").unwrap(),
            bounds: Bounds::default(),
            class: ClassSpec::Process,
            owner: None,
            band: 0,
        }),
        Err(Refusal::Invalid {
            what: "scheduler socket",
            ..
        })
    ));
    let deleted = client
        .schedule(dto::Command::Delete { key: external(key) })
        .unwrap();
    assert!(deleted.ended);
    assert!(runtime.list().unwrap().is_empty());
    drop(client);
    drop(server);
}

/// AC-T7.4.1/LB03: the first remote-created feedback meets publication Busy before any successful
/// outgoing feedback RTT. Release publication by event, not elapsed sleep, then require this same
/// worker to persist/ack creation without an administrator Retry/LinkUp or another queued message.
#[test]
fn cold_remote_creation_feedback_survives_owner_publication_busy() {
    let dir = tempfile::tempdir().unwrap();
    let need = Resources {
        cpu_milli: u64::from(hex_api::MIN_CPU_MILLI),
        mem_bytes: hex_api::MIN_MEMORY,
        ..Resources::default()
    };
    let mut local = NodeSpec::new(NodeId(0), Resources::default());
    local.classes = Classes::NONE;
    let mut cluster =
        Cluster::single_owner(vec![local, NodeSpec::new(NodeId(1), need)], Vec::new());
    let owner_driver = Driver::boot(
        NodeId(0),
        &cluster.nodes,
        &cluster.models,
        Budget {
            owned: hex_runtime::MAX_BOXES,
            ran: hex_runtime::MAX_BOXES,
        },
        Pick::default(),
        &dir.path().join("owner-placements"),
        Runtimes::default(),
    )
    .unwrap();
    let runtime = Arc::new(hex_api::sim::SimRuntime::default());
    let receiver = Driver::boot(
        NodeId(1),
        &cluster.nodes,
        &cluster.models,
        Budget {
            owned: hex_runtime::MAX_BOXES,
            ran: hex_runtime::MAX_BOXES,
        },
        Pick::default(),
        &dir.path().join("receiver-placements"),
        Runtimes::new([(
            hex_sim::model::Class::Oci,
            runtime.clone() as Arc<dyn Runtime>,
        )])
        .unwrap(),
    )
    .unwrap();
    let (owner_send, owner_receive) = channel();
    let owner = Arc::new(Service {
        id: NodeId(0),
        realm: "cold-feedback".into(),
        command: Mutex::new(()),
        fault: Mutex::new(None),
        cache: Mutex::new(
            cache::Cache::open(
                &dir.path().join("peers"),
                &std::collections::BTreeSet::from([1]),
                &mut cluster,
                no_aside,
            )
            .unwrap(),
        ),
        _lease: std::fs::File::create(dir.path().join("lease")).unwrap(),
        order: Mutex::new(
            order::Order::open_with_peers(
                &dir.path().join("transport"),
                &std::collections::BTreeSet::from([1]),
            )
            .unwrap(),
        ),
        driver: Arc::new(owner_driver),
        lane: None,
        runtimes: Runtimes::default(),
        cluster: Mutex::new(cluster),
        facts_dir: dir.path().to_path_buf(),
        initialized: Classes::NONE,
        physical_authority: None,
        stopping: Arc::new(Stopping::new([1])),
        queued: Arc::new(AtomicUsize::new(0)),
        control_limit: AtomicUsize::new(crate::records::MOST),
        senders: BTreeMap::from([(NodeId(1), owner_send)]),
        refreshing: BTreeMap::from([(NodeId(1), Arc::new(AtomicBool::new(false)))]),
    });
    let (key, submitted) = owner
        .driver
        .submit(
            Demand {
                need,
                kind: Kind::Plain,
                class: hex_sim::model::Class::Oci,
                ask: Ask::default(),
            },
            Payload {
                spec: BoxSpec {
                    route: None,
                    allocation: None,
                    devices: Vec::new(),
                    id: BoxId::new("first-cold-feedback").unwrap(),
                    bounds: Bounds {
                        cpu_max_milli: Some(hex_api::MIN_CPU_MILLI),
                        memory_max: Some(hex_api::MIN_MEMORY),
                        ..Bounds::default()
                    },
                    class: ClassSpec::Process,
                    owner: None,
                    band: 0,
                },
                start: false,
            },
        )
        .unwrap();
    owner.dispatch(submitted).unwrap();
    let start = owner_receive.recv().unwrap().envelope.unwrap();
    assert!(matches!(start.msg, peer::Msg::Start { key: k, .. } if k == key));
    let created = receiver.hear(NodeId(0), start).unwrap();
    assert_eq!(
        runtime.list().unwrap().len(),
        1,
        "receiver physically created its box"
    );
    assert!(receiver.status(key).unwrap().unwrap().created);
    assert!(!owner.driver.status(key).unwrap().unwrap().created);
    let feedback = created
        .send
        .into_iter()
        .find_map(|(to, message)| if to == NodeId(0) { Some(message) } else { None })
        .unwrap();
    assert!(matches!(feedback.msg, peer::Msg::Started { key: k, .. } if k == key));
    assert!(!dir.path().join("transport/peer-1.rec").exists());

    let successes = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::new(AtomicUsize::new(0));
    let (busy, saw_busy) = sync_channel(0);
    let (release, released) = sync_channel(0);
    let publication = owner.command.lock().unwrap();
    let (send, receive) = sync_channel(1);
    let pending = Arc::new(AtomicUsize::new(1));
    send.send(Queued {
        envelope: Some(feedback),
        observations: Vec::new(),
        _permit: Permit(Arc::clone(&pending)),
        _refresh: None,
    })
    .unwrap();
    drop(send); // Exactly one feedback publication; no external retry or link-up is supplied.
    let lane = PeerLane {
        peer: Peer {
            node: 0,
            host: "unused-mock-peer".into(),
            command: "unused-mock-command".into(),
        },
        id: NodeId(1),
        realm: "cold-feedback".into(),
        models: Vec::new(),
        epoch: 1,
        stopping: Arc::new(Stopping::new([0])),
        report: Arc::new(|_, _| {}),
        source: Arc::new(Mutex::new(Weak::new())),
    };
    let worker = {
        let owner = Arc::clone(&owner);
        let successes = Arc::clone(&successes);
        let attempts = Arc::clone(&attempts);
        std::thread::spawn(move || {
            let mut first_busy = Some((busy, released));
            run_peer_with(
                lane,
                receive,
                |_, _| Ok(()),
                |_, command| {
                    let feedback = matches!(command, dto::Command::Hear { .. });
                    let reply = owner.command(command);
                    match &reply {
                        Ok(_) => {
                            successes.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(Refusal::Exhausted {
                            what: "scheduler command publication",
                            ..
                        }) => {
                            assert_eq!(
                                successes.load(Ordering::SeqCst),
                                0,
                                "cold feedback has no prior successful response"
                            );
                            if feedback {
                                attempts.fetch_add(1, Ordering::SeqCst);
                                if let Some((busy, released)) = first_busy.take() {
                                    busy.send(()).unwrap();
                                    released.recv().unwrap();
                                }
                            }
                        }
                        Err(why) => panic!("unexpected mock peer refusal: {why}"),
                    }
                    Ok(reply)
                },
            );
        })
    };
    saw_busy.recv().unwrap();
    assert_eq!(successes.load(Ordering::SeqCst), 0);
    assert!(!owner.driver.status(key).unwrap().unwrap().created);
    assert!(!dir.path().join("transport/peer-1.rec").exists());
    drop(publication);
    release.send(()).unwrap();
    worker.join().unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(successes.load(Ordering::SeqCst), 1);
    assert_eq!(pending.load(Ordering::SeqCst), 0);
    assert!(owner.driver.status(key).unwrap().unwrap().created);
    assert!(dir.path().join("transport/peer-1.rec").exists());
    let physical = receiver.status(key).unwrap().unwrap().physical;
    runtime.delete(&physical).unwrap();
}

fn advertise_timing_service(
    id: NodeId,
    dir: &Path,
    cluster: Cluster,
    runtimes: Runtimes,
    sender: Sender<Queued>,
) -> Arc<Service> {
    advertise_service_with_budget(
        id,
        dir,
        cluster,
        runtimes,
        sender,
        Budget {
            owned: hex_runtime::MAX_BOXES,
            ran: hex_runtime::MAX_BOXES,
        },
    )
}

pub(super) fn advertise_service_with_budget(
    id: NodeId,
    dir: &Path,
    mut cluster: Cluster,
    runtimes: Runtimes,
    sender: Sender<Queued>,
    budget: Budget,
) -> Arc<Service> {
    std::fs::create_dir_all(dir).unwrap();
    let other = cluster.nodes.iter().find(|node| node.id != id).unwrap().id;
    let driver = Driver::boot(
        id,
        &cluster.nodes,
        &cluster.models,
        budget,
        Pick::default(),
        &dir.join("placements"),
        runtimes.clone(),
    )
    .unwrap();
    let initialized = cluster
        .nodes
        .iter()
        .find(|node| node.id == id)
        .unwrap()
        .classes;
    let physical_authority = runtimes
        .physical_report()
        .unwrap()
        .map(|report| report.authority);
    Arc::new(Service {
        id,
        realm: "advertise-time".into(),
        command: Mutex::new(()),
        fault: Mutex::new(None),
        cache: Mutex::new(
            cache::Cache::open(
                &dir.join("peers"),
                &std::collections::BTreeSet::from([other.0]),
                &mut cluster,
                no_aside,
            )
            .unwrap(),
        ),
        _lease: std::fs::File::create(dir.join("lease")).unwrap(),
        order: Mutex::new(
            order::Order::open_with_peers(
                &dir.join("transport"),
                &std::collections::BTreeSet::from([other.0]),
            )
            .unwrap(),
        ),
        driver: Arc::new(driver),
        lane: None,
        runtimes,
        cluster: Mutex::new(cluster),
        facts_dir: dir.to_path_buf(),
        initialized,
        physical_authority,
        stopping: Arc::new(Stopping::new([other.0])),
        queued: Arc::new(AtomicUsize::new(0)),
        control_limit: AtomicUsize::new(crate::records::MOST),
        senders: BTreeMap::from([(other, sender)]),
        refreshing: BTreeMap::from([(other, Arc::new(AtomicBool::new(false)))]),
    })
}

fn capacity_budget_request(boxes: u32) -> (Demand, Payload) {
    let cpu = hex_api::MIN_CPU_MILLI * boxes;
    let memory = hex_api::MIN_MEMORY * u64::from(boxes);
    (
        Demand {
            need: Resources {
                cpu_milli: u64::from(cpu),
                mem_bytes: memory,
                ..Resources::default()
            },
            kind: Kind::Plain,
            class: hex_sim::model::Class::Oci,
            ask: Ask::default(),
        },
        Payload {
            spec: BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: BoxId::new("capacity-budget").unwrap(),
                bounds: Bounds {
                    cpu_max_milli: Some(cpu),
                    memory_max: Some(memory),
                    ..Bounds::default()
                },
                class: ClassSpec::Process,
                owner: None,
                band: 0,
            },
            start: false,
        },
    )
}

fn advertise_capacity(service: &Service, peer: &NodeSpec) -> Result<dto::Reply, Refusal> {
    service.command(dto::Command::Advertise {
        from: peer.id.0,
        cluster: service.realm.clone(),
        offer: offer::from_local(peer, &[]).unwrap(),
        observations: Vec::new(),
    })
}

fn capacity_budget_service(
    dir: &Path,
) -> (
    Arc<Service>,
    Arc<hex_api::sim::SimRuntime>,
    Receiver<Queued>,
    NodeSpec,
) {
    let (request, _) = capacity_budget_request(1);
    // An advertised VM class must carry its immutable runtime cost (offer::checked_shape); these
    // OCI admission tests advertise an OCI-only peer.
    let mut peer = NodeSpec::new(NodeId(1), request.need);
    peer.classes = Classes::NONE.with(hex_sim::model::Class::Oci);
    let cluster = Cluster::single_owner(
        vec![NodeSpec::new(NodeId(0), request.need), peer.clone()],
        Vec::new(),
    );
    let budget = crate::placement::budget::runtime_budget(NodeId(0), &cluster.nodes).unwrap();
    let runtime = Arc::new(hex_api::sim::SimRuntime::default());
    let (sender, receiver) = channel();
    let service = advertise_service_with_budget(
        NodeId(0),
        dir,
        cluster,
        Runtimes::new([(
            hex_sim::model::Class::Oci,
            runtime.clone() as Arc<dyn Runtime>,
        )])
        .unwrap(),
        sender,
        budget,
    );
    // Initialize admission and control through the same incoming-facts command as the updates.
    assert!(
        advertise_capacity(&service, &peer)
            .unwrap()
            .refusals
            .is_empty()
    );
    (service, runtime, receiver, peer)
}

fn capacity_budget_rows(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir.join("placements"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rec"))
        .map(|path| {
            (
                path.file_name().unwrap().to_str().unwrap().to_owned(),
                std::fs::read(path).unwrap(),
            )
        })
        .collect()
}

/// AC-T7.4.1: incoming facts change admission immediately, preserving live/pending durable rows
/// and already-held control permits even when the current ceiling falls below retained intents.
#[test]
fn service_advertise_growth_and_shrink_preserve_intents_and_control() {
    let dir = tempfile::tempdir().unwrap();
    let (service, runtime, receiver, mut peer) = capacity_budget_service(dir.path());
    let (demand, payload) = capacity_budget_request(1);
    let (live, step) = service.driver.submit(demand, payload).unwrap();
    assert!(service.dispatch(step).unwrap().refusals.is_empty());
    let status = service.driver.status(live).unwrap().unwrap();
    assert!(status.local && status.created);
    assert_eq!(runtime.list().unwrap().len(), 1);
    let (pending, payload) = capacity_budget_request(4);
    let (first, step) = service.driver.submit(pending, payload.clone()).unwrap();
    assert!(service.dispatch(step).unwrap().refusals.is_empty());
    assert!(matches!(
        service
            .driver
            .submit(pending, payload.clone())
            .map_err(failed),
        Err(Refusal::Exhausted {
            what: "owned records",
            bound: 2
        })
    ));

    peer.capacity = capacity_budget_request(2).0.need;
    assert!(
        advertise_capacity(&service, &peer)
            .unwrap()
            .refusals
            .is_empty()
    );
    let (second, step) = service.driver.submit(pending, payload.clone()).unwrap();
    assert!(service.dispatch(step).unwrap().refusals.is_empty());
    assert!(matches!(
        service
            .driver
            .submit(pending, payload.clone())
            .map_err(failed),
        Err(Refusal::Exhausted {
            what: "owned records",
            bound: 3
        })
    ));
    service.refresh_peers().unwrap();
    let held_control = receiver.try_recv().unwrap();
    assert!(held_control.envelope.is_none());
    assert_eq!(service.queued.load(Ordering::Acquire), 1);
    let before = capacity_budget_rows(dir.path());
    peer.capacity = demand.need;
    assert!(
        advertise_capacity(&service, &peer)
            .unwrap()
            .refusals
            .is_empty()
    );
    assert_eq!(capacity_budget_rows(dir.path()), before);
    assert_eq!(service.queued.load(Ordering::Acquire), 1);
    assert!(matches!(
        service.driver.submit(pending, payload).map_err(failed),
        Err(Refusal::Exhausted {
            what: "owned records",
            bound: 2
        })
    ));
    assert_eq!(service.driver.status(live).unwrap().unwrap(), status);
    assert_eq!(
        runtime.inspect(&status.physical).unwrap().id,
        status.physical
    );
    for key in [first, second] {
        let status = service.driver.status(key).unwrap().unwrap();
        assert!(status.node.is_none() && !status.created && !status.ended);
    }
    drop(held_control);
    assert_eq!(service.queued.load(Ordering::Acquire), 0);
    assert!(!service.refreshing[&NodeId(1)].load(Ordering::Acquire));
    // A smaller admission ceiling must still permit the real lifecycle cleanup path.
    for key in [live, first, second] {
        assert!(
            service
                .command(dto::Command::Delete { key: external(key) })
                .unwrap()
                .refusals
                .is_empty()
        );
    }
    assert!(runtime.list().unwrap().is_empty());
    assert_eq!(capacity_budget_rows(dir.path()).len(), 1); // epoch only, no abandoned authority.
}

/// AC-T7.4.1: a peer's larger facts do not authorize extra records before their recovery ceiling
/// is durable. Remove the write fault before admission checks so it cannot mask an enlarged cap.
#[test]
fn service_advertise_failed_budget_growth_keeps_old_admission_and_rows() {
    let dir = tempfile::tempdir().unwrap();
    let (service, runtime, receiver, mut peer) = capacity_budget_service(dir.path());
    let (demand, payload) = capacity_budget_request(1);
    let (live, step) = service.driver.submit(demand, payload).unwrap();
    service.dispatch(step).unwrap();
    let physical = service.driver.status(live).unwrap().unwrap().physical;
    let (pending, payload) = capacity_budget_request(4);
    let (intent, step) = service.driver.submit(pending, payload.clone()).unwrap();
    service.dispatch(step).unwrap();
    service.refresh_peers().unwrap();
    let held_control = receiver.try_recv().unwrap();
    let before = capacity_budget_rows(dir.path());
    let control_limit = service.control_limit.load(Ordering::Acquire);
    let blocked = dir.path().join("placements/epoch.tmp");
    std::fs::create_dir(&blocked).unwrap();
    peer.capacity = capacity_budget_request(2).0.need;
    let growth = advertise_capacity(&service, &peer);
    std::fs::remove_dir(blocked).unwrap();
    assert!(matches!(growth, Err(Refusal::Broken(_))));
    assert_eq!(service.cluster.lock().unwrap().nodes[1], peer);
    assert_eq!(service.control_limit.load(Ordering::Acquire), control_limit);
    assert!(matches!(
        service.driver.submit(pending, payload).map_err(failed),
        Err(Refusal::Exhausted {
            what: "owned records",
            bound: 2
        })
    ));
    assert_eq!(capacity_budget_rows(dir.path()), before);
    assert_eq!(runtime.list().unwrap().len(), 1);
    assert_eq!(runtime.inspect(&physical).unwrap().id, physical);
    assert_eq!(service.queued.load(Ordering::Acquire), 1);
    drop(held_control);
    assert_eq!(service.queued.load(Ordering::Acquire), 0);
    for key in [live, intent] {
        assert!(
            service
                .command(dto::Command::Delete { key: external(key) })
                .unwrap()
                .refusals
                .is_empty()
        );
    }
    assert!(runtime.list().unwrap().is_empty());
    assert_eq!(capacity_budget_rows(dir.path()).len(), 1);
}

/// AC-T7.4.1/r423: successful SSH Describe is followed by local source Advertise Busy. Its own
/// pauses must not become learned operation cost. Virtual time has one fixed Describe cost; only
/// requested waits advance it thereafter. Three Busy retries are gated before publication opens.
#[test]
fn local_advertise_retry_pace_does_not_learn_its_own_pauses() {
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let need = Resources {
        cpu_milli: u64::from(hex_api::MIN_CPU_MILLI),
        mem_bytes: hex_api::MIN_MEMORY,
        ..Resources::default()
    };
    let mut local = NodeSpec::new(NodeId(0), Resources::default());
    local.classes = Classes::NONE;
    let cluster = Cluster::single_owner(vec![local, NodeSpec::new(NodeId(1), need)], Vec::new());
    let (owner_send, owner_receive) = channel();
    let owner = advertise_timing_service(
        NodeId(0),
        &dir.path().join("owner"),
        cluster.clone(),
        Runtimes::default(),
        owner_send,
    );
    let runtime = Arc::new(hex_api::sim::SimRuntime::default());
    let (source_send, source_receive) = channel();
    let source = advertise_timing_service(
        NodeId(1),
        &dir.path().join("source"),
        cluster,
        Runtimes::new([(
            hex_sim::model::Class::Oci,
            runtime.clone() as Arc<dyn Runtime>,
        )])
        .unwrap(),
        source_send,
    );
    assert_ne!(source.id, owner.id);
    let (key, submitted) = owner
        .driver
        .submit(
            Demand {
                need,
                kind: Kind::Plain,
                class: hex_sim::model::Class::Oci,
                ask: Ask::default(),
            },
            Payload {
                spec: BoxSpec {
                    route: None,
                    allocation: None,
                    devices: Vec::new(),
                    id: BoxId::new("local-advertise-timing").unwrap(),
                    bounds: Bounds {
                        cpu_max_milli: Some(hex_api::MIN_CPU_MILLI),
                        memory_max: Some(hex_api::MIN_MEMORY),
                        ..Bounds::default()
                    },
                    class: ClassSpec::Process,
                    owner: None,
                    band: 0,
                },
                start: false,
            },
        )
        .unwrap();
    owner.dispatch(submitted).unwrap();
    let start = owner_receive.recv().unwrap().envelope.unwrap();
    let created = source.driver.hear(NodeId(0), start).unwrap();
    source.dispatch(created).unwrap();
    assert_eq!(runtime.list().unwrap().len(), 1);
    assert!(!owner.driver.status(key).unwrap().unwrap().created);
    let publication = source.command.lock().unwrap();
    let (waited, waits) = sync_channel(0);
    let (release, released) = sync_channel(0);
    let (applied, was_applied) = sync_channel(0);
    let described = Arc::new(AtomicBool::new(false));
    let pauses = Arc::new(Mutex::new(Duration::ZERO));
    // One virtual nanosecond is the test's fixed work unit, never a production wait/seed.
    let work = Duration::from_nanos(1);
    let began = Instant::now();
    let lane = PeerLane {
        peer: Peer {
            node: 0,
            host: "unused-mock-peer".into(),
            command: "unused-mock-command".into(),
        },
        id: source.id,
        realm: source.realm.clone(),
        models: Vec::new(),
        epoch: 1,
        stopping: Arc::clone(&source.stopping),
        report: Arc::new(|_, why| panic!("unexpected peer failure: {why}")),
        source: Arc::new(Mutex::new(Arc::downgrade(&source))),
    };
    let worker = {
        let owner = Arc::clone(&owner);
        let described_for_call = Arc::clone(&described);
        let pauses_for_wait = Arc::clone(&pauses);
        std::thread::spawn(move || {
            let mut applied = Some(applied);
            let mut released = Some(released);
            let mut attempts = 0;
            run_peer_with_time(
                lane,
                source_receive,
                |_, _| Ok(()),
                |_, command| {
                    let describe = matches!(&command, dto::Command::Describe { .. });
                    let result = owner.command(command);
                    if describe {
                        assert!(
                            result.as_ref().unwrap().offer.is_some(),
                            "first Describe succeeds with an actual owner offer"
                        );
                        described_for_call.store(true, Ordering::SeqCst);
                    } else if result.is_ok()
                        && owner.driver.status(key).unwrap().unwrap().created
                        && let Some(applied) = applied.take()
                    {
                        applied.send(()).unwrap();
                    }
                    Ok(result)
                },
                || {
                    began
                        .checked_add(
                            *pauses.lock().unwrap()
                                + if described.load(Ordering::SeqCst) {
                                    work
                                } else {
                                    Duration::ZERO
                                },
                        )
                        .unwrap()
                },
                |_, pace| {
                    assert!(described_for_call.load(Ordering::SeqCst));
                    attempts += 1;
                    waited.send(pace).unwrap();
                    *pauses_for_wait.lock().unwrap() += pace;
                    if attempts == 3 {
                        released.take().unwrap().recv().unwrap();
                    }
                    Ok(false)
                },
            );
        })
    };
    let observed = [
        waits.recv().unwrap(),
        waits.recv().unwrap(),
        waits.recv().unwrap(),
    ];
    drop(publication);
    release.send(()).unwrap();
    was_applied.recv().unwrap();
    assert!(owner.driver.status(key).unwrap().unwrap().created);
    let physical = source.driver.status(key).unwrap().unwrap().physical;
    drop(source); // Close the source queue after its publication/feedback complete.
    worker.join().unwrap();
    runtime.delete(&physical).unwrap();
    // Clean up before the RED assertion so failing timing evidence never strands a worker.
    assert_eq!(
        observed, [work; 3],
        "only fixed operation work may determine retry pace"
    );
}

/// AC-T7.4.1: all peer queues and a worker's held word share one admission count.
/// Lowering that count preserves outstanding work; disconnect/drop returns both permits.
#[test]
fn queued_control_limit_is_global_and_survives_shrink_and_disconnect() {
    let dir = tempfile::tempdir().unwrap();
    let cluster = Cluster::single_owner(
        (0..3)
            .map(|id| NodeSpec::new(NodeId(id), Resources::default()))
            .collect(),
        Vec::new(),
    );
    let (first_send, first_receive) = channel();
    let mut service = advertise_timing_service(
        NodeId(0),
        dir.path(),
        cluster,
        Runtimes::default(),
        first_send,
    );
    let (second_send, second_receive) = channel();
    let owned = Arc::get_mut(&mut service).unwrap();
    owned.senders.insert(NodeId(2), second_send);
    owned
        .refreshing
        .insert(NodeId(2), Arc::new(AtomicBool::new(false)));
    owned.control_limit.store(2, Ordering::Release);
    service.refresh_peers().unwrap();
    assert_eq!(service.queued.load(Ordering::Acquire), 2);
    let in_flight = first_receive.try_recv().unwrap();
    // Receiving alone does not release the worker's admission; it owns the word until completion.
    assert_eq!(service.queued.load(Ordering::Acquire), 2);
    service.control_limit.store(1, Ordering::Release);
    service.refresh_peers().unwrap(); // Existing per-peer refreshes coalesce even above the limit.
    assert_eq!(service.queued.load(Ordering::Acquire), 2);
    drop(in_flight);
    assert_eq!(service.queued.load(Ordering::Acquire), 1);
    assert!(matches!(
        service.refresh_peers(),
        Err(Refusal::Exhausted { bound: 1, .. })
    ));
    assert!(!service.refreshing[&NodeId(1)].load(Ordering::Acquire));
    drop(second_receive); // Drops a queued word as well as that endpoint.
    assert_eq!(service.queued.load(Ordering::Acquire), 0);
    service.control_limit.store(2, Ordering::Release);
    assert!(matches!(service.refresh_peers(), Err(Refusal::Broken(_))));
    // Peer1's successful send remains admitted; peer2's failed send returns its permits.
    assert_eq!(service.queued.load(Ordering::Acquire), 1);
    assert!(!service.refreshing[&NodeId(2)].load(Ordering::Acquire));
    drop(first_receive);
    assert_eq!(service.queued.load(Ordering::Acquire), 0);
    assert!(!service.refreshing[&NodeId(1)].load(Ordering::Acquire));
}

/// AC-T7.4.1: durable placement words cannot bypass the same global queue authority.
#[test]
fn dispatch_control_refuses_at_global_limit_and_returns_failed_send_permits() {
    let dir = tempfile::tempdir().unwrap();
    let cluster = Cluster::single_owner(
        (0..2)
            .map(|id| NodeSpec::new(NodeId(id), Resources::default()))
            .collect(),
        Vec::new(),
    );
    let (sender, receiver) = channel();
    let service =
        advertise_timing_service(NodeId(0), dir.path(), cluster, Runtimes::default(), sender);
    service.control_limit.store(1, Ordering::Release);
    let step = || Step {
        send: vec![(
            NodeId(1),
            Envelope {
                msg: peer::Msg::Holds {
                    key: Key {
                        owner: 0,
                        epoch: 1,
                        seq: 0,
                    },
                },
                payload: None,
                refusal: None,
            },
        )],
        notices: Vec::new(),
    };
    assert!(service.dispatch(step()).unwrap().refusals.is_empty());
    assert_eq!(service.queued.load(Ordering::Acquire), 1);
    let refused = service.dispatch(step()).unwrap();
    assert_eq!(refused.refusals.len(), 1);
    assert_eq!(
        refused.refusals[0],
        encoded_refusal(&Refusal::Exhausted {
            what: "queued peer messages",
            bound: 1,
        })
        .unwrap()
    );
    assert_eq!(service.queued.load(Ordering::Acquire), 1);
    let word = receiver.try_recv().unwrap();
    assert_eq!(service.queued.load(Ordering::Acquire), 1);
    drop(word);
    assert_eq!(service.queued.load(Ordering::Acquire), 0);
    drop(receiver);
    let refused = service.dispatch(step()).unwrap();
    assert_eq!(refused.refusals.len(), 1);
    assert_eq!(
        refused.refusals[0],
        encoded_refusal(&Refusal::Broken(
            "scheduler peer worker disconnected".into(),
        ))
        .unwrap()
    );
    assert_eq!(service.queued.load(Ordering::Acquire), 0);
}

/// AC-T7.4.1/LB03, a cache is not authority: do start the scheduler over a peer cache holding
/// peer 1's offer as a build before 516a370b cached it (every class, no runtime cost) beside peer
/// 2's valid offer. Expect it to start with peer 1 uncached, its record set aside and reported
/// against it with why, and peer 2's cached facts restored.
#[test]
fn a_cached_offer_todays_rules_refuse_is_set_aside_and_the_scheduler_starts() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let peers = state.join("peers");
    std::fs::create_dir_all(&peers).unwrap();
    let room = Resources {
        cpu_milli: 1_000,
        mem_bytes: 1 << 30,
        ..Resources::default()
    };
    let stale = dto::Offer {
        spec: serde_json::to_string(&NodeSpec::new(NodeId(1), room)).unwrap(),
        models: Vec::new(),
    };
    std::fs::write(
        peers.join("peer-1.rec"),
        serde_json::to_string(&stale).unwrap(),
    )
    .unwrap();
    let mut valid = NodeSpec::new(NodeId(2), room);
    valid.classes = Classes::NONE.with(hex_sim::model::Class::Oci);
    let valid_record = serde_json::to_string(&offer::from_local(&valid, &[]).unwrap()).unwrap();
    std::fs::write(peers.join("peer-2.rec"), &valid_record).unwrap();
    let config = Config {
        cluster: "cache-start".into(),
        node: 0,
        state,
        socket: dir.path().join("scheduler.sock"),
        nodes: Vec::new(),
        models: Vec::new(),
        backends: Vec::new(),
        // An empty host is refused before any SSH process starts.
        peers: [1, 2]
            .map(|node| Peer {
                node,
                host: String::new(),
                command: String::new(),
            })
            .into(),
        capacity: None,
    };
    let reports = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&reports);
    let report: Report =
        Arc::new(move |node, why| sink.lock().unwrap().push((node, why.to_owned())));
    let service = Service::open(&config, report).unwrap();
    service.shutdown().unwrap();
    let nodes = service.cluster.lock().unwrap().nodes.clone();
    let mut placeholder = NodeSpec::new(NodeId(1), Resources::default());
    placeholder.classes = Classes::NONE;
    assert_eq!(nodes[1], placeholder);
    assert_eq!(nodes[2], valid);
    let reports = reports.lock().unwrap();
    assert!(
        reports.iter().any(|(node, why)| *node == NodeId(1)
            && why.contains("initialized VM peer offer has no immutable runtime cost")),
        "{reports:?}"
    );
    assert!(!peers.join("peer-1.rec").exists());
    assert_eq!(
        std::fs::read_to_string(peers.join("peer-2.rec")).unwrap(),
        valid_record
    );
}

#[path = "adversarial_relay_tests.rs"]
mod adversarial_relay_tests;

#[path = "device_offer_tests.rs"]
mod device_offer_tests;

/// AC-T7.4.1: cleanup intent survives unavailable renderer telemetry; it is never a new boot
/// grant. This uses the actual Service action parser over canonical full-set receipts.
#[test]
fn canonical_draining_action_is_not_filtered_by_unavailable_fresh_gpu_observation() {
    let fixed = hex_api::CapacityResources {
        cpu_milli: 1_000,
        mem_bytes: 64 << 20,
        disk_bytes: 0,
    };
    let cost = hex_api::RuntimeCost {
        id: [71; 32],
        floor: fixed,
        ceiling: fixed,
        quantum: hex_api::CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        overhead: hex_api::CapacityResources::default(),
        guest_overhead: hex_api::CapacityResources::default(),
    };
    let key = hex_api::BackingKey {
        pool: [72; 32],
        generation: [73; 16],
    };
    let backing = hex_api::BackingOffer {
        guest: Default::default(),
        key,
        cost,
        phase: hex_api::BackingPhase::Draining,
        devices: vec![hex_api::DeviceId {
            kind: hex_api::DeviceKind::Gpu,
            key: 7,
        }],
    };
    let report = hex_api::Capacity {
        id: "same-owner".into(),
        total: fixed,
        free: fixed,
        cost: None,
        devices: Vec::new(),
        device_routes: Vec::new(),
        backings: vec![backing],
        routes: vec![hex_api::RouteOffer {
            key: hex_api::RouteKey {
                class: hex_api::Class::Firecracker,
                image: hex_api::ImagePlatform {
                    os: hex_api::ImageOs::Linux,
                    arch: hex_api::ImageArch::Amd64,
                },
                execution: hex_api::Execution::Firecracker,
                gpu: hex_api::GpuEligibility::Required,
            },
            cost: Some(cost),
            backing: Some(key),
        }],
        unavailable: Vec::new(),
        learned: None,
    };
    report.check().unwrap();
    let reports = vec![(hex_sim::model::Class::Firecracker, Some(report))];
    let mut actions = BackingActions::from_reports(&reports).unwrap();
    let unavailable = hex_api::BackendUnavailable::from_refusal(
        hex_api::Class::Firecracker,
        Refusal::Backing {
            key,
            cause: hex_api::LifecycleCause::Uncertain,
        },
    )
    .unwrap();
    actions.only_available(&reports, &[unavailable]).unwrap();
    assert!(actions.prepared.is_empty() && actions.released.is_empty());
    assert_eq!(actions.draining.len(), 1);
    assert_eq!(actions.draining[0].0, hex_sim::model::Class::Firecracker);
    assert_eq!(actions.draining[0].1.key, key);
    assert_eq!(
        actions.draining[0].1.devices,
        vec![hex_api::DeviceId {
            kind: hex_api::DeviceKind::Gpu,
            key: 7
        }]
    );
}
