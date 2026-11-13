//! AC-T7.4.1/LB03: the consistency guard must preserve authorized matching-payload recovery.

use super::*;

/// An init whose authorized start failed remains Created until recovery retries that same intent.
/// The existing contradictory-start regression also checks create-only recovery after repair.
#[test]
fn matching_recovered_payload_resumes_authorized_init_at_zero_admission() {
    let dir = tempfile::tempdir().unwrap();
    let rt = Arc::new(Counting::default());
    let driver = try_boot_with_budget(dir.path(), rt.clone(), Budget { owned: 1, ran: 1 }).unwrap();
    let (d, mut p) = request();
    p.spec.class = ClassSpec::Oci(Box::new(OciSpec {
        rootfs: Rootfs::Dir("/private-simulated-rootfs".into()),
        init: Program::of(&["wait"]),
        mounts: Vec::new(),
        hostname: None,
        network: Network::Private,
        hosts: Vec::new(),
    }));
    p.start = true;
    rt.fail_start.store(true, Ordering::SeqCst);
    rt.fail_delete.store(true, Ordering::SeqCst);
    let (key, step) = driver.submit(d, p.clone()).unwrap();
    let failed = local(&driver, step);
    assert!(matches!(
        failed.notices.as_slice(),
        [Notice::Unknown {
            key: failed_key,
            refusal: Refusal::Os { op: "test start", .. },
            cleanup: Some(Refusal::Os { op: "test delete", .. }),
        }] if *failed_key == key
    ));
    let physical = driver.physical_id(key).unwrap();
    let namespace = driver.namespace().to_owned();
    assert_eq!(rt.sim.inspect(&physical).unwrap().state, BoxState::Created);
    assert_eq!(rt.starts.load(Ordering::SeqCst), 1);
    drop(driver);

    rt.fail_start.store(false, Ordering::SeqCst);
    rt.fail_delete.store(false, Ordering::SeqCst);
    let recovered =
        try_boot_with_budget(dir.path(), rt.clone(), Budget { owned: 0, ran: 0 }).unwrap();
    let ready = local(&recovered, recovered.take_recovered().unwrap());
    let observed = rt.sim.inspect(&physical).unwrap();
    let starts = rt.starts.load(Ordering::SeqCst);
    let creates = rt.creates.load(Ordering::SeqCst);
    let owner = recovered
        .state
        .lock()
        .unwrap()
        .rows
        .payload("own", key)
        .unwrap();
    let runner = recovered
        .state
        .lock()
        .unwrap()
        .rows
        .payload("ran", key)
        .unwrap();
    let refused = recovered.submit(d, p.clone());
    let admission_refused = matches!(refused, Err(Error::Core(Stop::Full { bound: 0, .. })));
    local(&recovered, recovered.delete(key).unwrap());
    assert!(rt.list().unwrap().is_empty());
    assert!(recovered.owned().unwrap().is_empty());
    assert_eq!(durable_placement_files(dir.path()).len(), 1);
    assert_eq!(
        recovered.state.lock().unwrap().node.admitted(),
        [0, 0, 0, 0]
    );
    eprintln!(
        "matching-payload expected=Running/starts2/creates1 observed={:?}/starts{starts}/creates{creates} admission_refused={admission_refused} cleanup_boxes=0",
        observed.state
    );

    assert_eq!(recovered.namespace(), namespace);
    assert_eq!(observed.id, physical);
    assert_eq!(observed.state, BoxState::Running);
    assert_eq!(starts, 2);
    assert_eq!(creates, 1);
    assert_eq!(owner, p);
    assert_eq!(runner, p);
    assert!(admission_refused);
    assert!(ready.notices.contains(&Notice::Started {
        key,
        node: NodeId(0)
    }));
}
