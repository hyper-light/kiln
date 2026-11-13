use super::*;

/// AC-T7.4.1/LB03: equal start flags cannot hide contradictory immutable BoxSpecs; recovery
/// refuses before callbacks, identifies only the key/cause, and accepts the restored matching rows.
#[test]
fn equal_start_spec_disagreement_refuses_before_callbacks_without_rendering_payload() {
    const OWNER_MARKER: &str = "HEX_BOUNDS_SYNTHETIC_OWNER_ARGUMENT";
    const RUNNER_MARKER: &str = "HEX_BOUNDS_SYNTHETIC_RUNNER_ARGUMENT";
    let dir = tempfile::tempdir().unwrap();
    let other_dir = tempfile::tempdir().unwrap();
    let rt = Arc::new(Counting::default());
    let driver = try_boot_with_budget(dir.path(), rt.clone(), Budget { owned: 1, ran: 1 }).unwrap();
    let other = boot(other_dir.path(), rt.clone());
    let (d, mut payload) = request();
    payload.spec.class = ClassSpec::Oci(Box::new(OciSpec {
        rootfs: Rootfs::Dir("/private-simulated-rootfs".into()),
        init: Program::of(&["wait", OWNER_MARKER]),
        mounts: Vec::new(),
        hostname: None,
        network: Network::Private,
        hosts: Vec::new(),
    }));
    let (key, step) = driver.submit(d, payload.clone()).unwrap();
    local(&driver, step);
    let (d, p) = request();
    let (other_key, step) = other.submit(d, p).unwrap();
    local(&other, step);
    let physical = driver.physical_id(key).unwrap();
    let other_physical = other.physical_id(other_key).unwrap();
    let target_before = rt.sim.inspect(&physical).unwrap();
    let sibling_before = rt.sim.inspect(&other_physical).unwrap();
    let sibling_rows = durable_placement_files(other_dir.path());
    assert_eq!(target_before.state, BoxState::Created);
    drop(driver);

    let mut changed = payload.clone();
    let ClassSpec::Oci(oci) = &mut changed.spec.class else {
        panic!("fixture must remain an OCI spec")
    };
    oci.init.args.push(RUNNER_MARKER.to_owned());
    changed.spec.check().unwrap();
    assert_eq!(changed.start, payload.start);
    assert!(!changed.start);
    assert_ne!(changed.spec, payload.spec);
    let runner = dir.path().join(format!("{}.rec", name("ran", key)));
    let original = std::fs::read(&runner).unwrap();
    let mut corrupt: serde_json::Value = serde_json::from_slice(&original).unwrap();
    corrupt["payload"]["spec"] = serde_json::to_value(&changed.spec).unwrap();
    std::fs::write(&runner, serde_json::to_vec(&corrupt).unwrap()).unwrap();
    let before = durable_placement_files(dir.path());
    let callbacks_before = (
        rt.inspects.load(Ordering::SeqCst),
        rt.starts.load(Ordering::SeqCst),
        rt.creates.load(Ordering::SeqCst),
        rt.deletes.load(Ordering::SeqCst),
    );
    let refusal = match try_boot_with_budget(dir.path(), rt.clone(), Budget { owned: 0, ran: 0 }) {
        Ok(driver) => {
            drop(driver);
            None
        }
        Err(why) => Some(why),
    };
    let callbacks_after = (
        rt.inspects.load(Ordering::SeqCst),
        rt.starts.load(Ordering::SeqCst),
        rt.creates.load(Ordering::SeqCst),
        rt.deletes.load(Ordering::SeqCst),
    );
    let after = durable_placement_files(dir.path());
    let target_unchanged = rt.sim.inspect(&physical).unwrap() == target_before;
    let sibling_unchanged = rt.sim.inspect(&other_physical).unwrap() == sibling_before
        && durable_placement_files(other_dir.path()) == sibling_rows;
    let diagnostic = refusal
        .as_ref()
        .map(|why| format!("{why}\n{why:?}"))
        .unwrap_or_default();
    let damaged = matches!(&refusal, Some(Error::Core(Stop::Damaged(_))));
    let named_key = diagnostic.contains(&name("ran", key))
        || diagnostic.contains(&name("own", key))
        || diagnostic.contains(&format!("{key:?}"));
    let cause = diagnostic.to_ascii_lowercase();
    let named_cause = cause.contains("payload")
        && (cause.contains("disagree") || cause.contains("differ") || cause.contains("mismatch"));
    let content_hidden = [
        OWNER_MARKER,
        RUNNER_MARKER,
        "/private-simulated-rootfs",
        "caller-name",
    ]
    .iter()
    .all(|content| !diagnostic.contains(content));

    // Restore even after an erroneous successful boot; matching rows must still recover and clean up.
    std::fs::write(&runner, original).unwrap();
    let recovered =
        try_boot_with_budget(dir.path(), rt.clone(), Budget { owned: 0, ran: 0 }).unwrap();
    local(&recovered, recovered.take_recovered().unwrap());
    assert_eq!(recovered.status(key).unwrap().unwrap().physical, physical);
    assert_eq!(rt.sim.inspect(&physical).unwrap(), target_before);
    assert_eq!(rt.inspects.load(Ordering::SeqCst), callbacks_after.0 + 1);
    assert_eq!(rt.starts.load(Ordering::SeqCst), 0);
    assert_eq!(rt.creates.load(Ordering::SeqCst), 2);
    local(&recovered, recovered.delete(key).unwrap());
    assert!(matches!(
        rt.sim.inspect(&physical),
        Err(Refusal::NoSuchBox(_))
    ));
    assert_eq!(rt.sim.inspect(&other_physical).unwrap(), sibling_before);
    assert_eq!(durable_placement_files(other_dir.path()), sibling_rows);
    local(&other, other.delete(other_key).unwrap());
    assert!(rt.list().unwrap().is_empty());
    assert_eq!(durable_placement_files(dir.path()).len(), 1);
    eprintln!(
        "immutable-spec expected=Damaged/callback_delta0/key_cause_only observed_damaged={damaged} key={named_key} cause={named_cause} content_hidden={content_hidden} before={callbacks_before:?} after={callbacks_after:?} target_unchanged={target_unchanged} sibling_unchanged={sibling_unchanged} matching_recovery=true cleanup_boxes=0"
    );

    assert!(
        damaged,
        "equal-start BoxSpec disagreement was not typed Damaged"
    );
    assert_eq!(callbacks_after, callbacks_before);
    assert_eq!(after, before);
    assert!(target_unchanged && sibling_unchanged);
    assert!(
        named_key && named_cause,
        "Damaged must identify the key and payload disagreement"
    );
    assert!(content_hidden, "refusal rendered synthetic payload content");
}

/// AC-T7.4.1: the largest representable structural read envelope is a count, not an eager
/// allocation or an admission grant; recovery compacts it to the actual retained population.
#[test]
fn representable_hostile_read_ceiling_compacts_without_new_physical_authority() {
    let dir = tempfile::tempdir().unwrap();
    let rt = Arc::new(Counting::default());
    let driver = try_boot_with_budget(dir.path(), rt.clone(), Budget { owned: 1, ran: 1 }).unwrap();
    let (d, p) = request();
    let (key, step) = driver.submit(d, p.clone()).unwrap();
    local(&driver, step);
    let physical = driver.physical_id(key).unwrap();
    let namespace = driver.namespace().to_owned();
    drop(driver);

    let epoch = dir.path().join("epoch.rec");
    let mut persisted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&epoch).unwrap()).unwrap();
    // 2 * (owned + ran + epoch) is usize::MAX - 1, still representable on each target.
    let huge = usize::MAX / 4;
    persisted["read_budget"] = serde_json::json!({"owned":huge,"ran":huge});
    std::fs::write(&epoch, serde_json::to_vec(&persisted).unwrap()).unwrap();
    let mut before = durable_placement_files(dir.path());
    before.remove("epoch.rec");
    let recovered =
        try_boot_with_budget(dir.path(), rt.clone(), Budget { owned: 0, ran: 0 }).unwrap();
    let compacted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&epoch).unwrap()).unwrap();
    let mut after = durable_placement_files(dir.path());
    after.remove("epoch.rec");
    local(&recovered, recovered.take_recovered().unwrap());
    assert_eq!(recovered.namespace(), namespace);
    assert_eq!(recovered.status(key).unwrap().unwrap().physical, physical);
    assert_eq!(rt.sim.inspect(&physical).unwrap().id, physical);
    let (extra, refusal) = match recovered.submit(d, p) {
        Ok((extra, step)) => {
            local(&recovered, step);
            (Some(extra), None)
        }
        Err(why) => (None, Some(why)),
    };
    let creates = rt.creates.load(Ordering::SeqCst);
    let starts = rt.starts.load(Ordering::SeqCst);
    let live = rt.list().unwrap().len();
    if let Some(extra) = extra {
        local(&recovered, recovered.delete(extra).unwrap());
    }
    local(&recovered, recovered.delete(key).unwrap());
    assert!(rt.list().unwrap().is_empty());
    assert_eq!(durable_placement_files(dir.path()).len(), 1);
    eprintln!(
        "hostile-read expected=Full0/creates1/starts0/retained1 observed={refusal:?}/creates{creates}/starts{starts}/retained{live} cleanup_boxes=0"
    );

    assert_eq!(after, before);
    assert_eq!(compacted["namespace"], persisted["namespace"]);
    assert_eq!(
        compacted["read_budget"],
        serde_json::json!({"owned":1,"ran":1})
    );
    assert_eq!(creates, 1);
    assert_eq!(starts, 0);
    assert_eq!(live, 1);
    assert!(matches!(
        refusal,
        Some(Error::Core(Stop::Full { bound: 0, .. }))
    ));
}

/// AC-T7.4.1/LB03: the persisted record/temp envelope bounds every directory entry before
/// crash cleanup; removing only an excess ignored entry restores the original cleanup authority.
#[test]
fn excess_ignored_entry_refuses_before_crash_cleanup_and_recovers_at_zero_admission() {
    let dir = tempfile::tempdir().unwrap();
    let other_dir = tempfile::tempdir().unwrap();
    let rt = Arc::new(Counting::default());
    let driver = try_boot_with_budget(dir.path(), rt.clone(), Budget { owned: 1, ran: 1 }).unwrap();
    let other = boot(other_dir.path(), rt.clone());
    let (d, p) = request();
    let (key, step) = driver.submit(d, p.clone()).unwrap();
    local(&driver, step);
    let (other_key, step) = other.submit(d, p.clone()).unwrap();
    local(&other, step);
    let physical = driver.physical_id(key).unwrap();
    let other_physical = other.physical_id(other_key).unwrap();
    let namespace = driver.namespace().to_owned();
    drop(driver);

    let durable = durable_placement_files(dir.path());
    assert_eq!(durable.len(), 3);
    let temps: Vec<_> = durable
        .keys()
        .map(|name| dir.path().join(name).with_extension("tmp"))
        .collect();
    for path in &temps {
        std::fs::write(path, b"incomplete atomic replacement").unwrap();
    }
    // Three records plus their three crash temporaries exactly fill the structural envelope.
    let excess = dir.path().join("ignored.note");
    std::fs::write(&excess, b"one extra directory entry").unwrap();
    let callbacks = (
        rt.creates.load(Ordering::SeqCst),
        rt.starts.load(Ordering::SeqCst),
        rt.inspects.load(Ordering::SeqCst),
        rt.deletes.load(Ordering::SeqCst),
    );
    let refused = try_boot_with_budget(dir.path(), rt.clone(), Budget { owned: 0, ran: 0 });
    let typed_refusal = matches!(
        &refused,
        Err(Error::Core(Stop::Full {
            what: "placement record directory entries",
            bound: 6,
        }))
    );
    let after_refusal = durable_placement_files(dir.path());
    let temps_untouched = temps
        .iter()
        .all(|path| std::fs::read(path).ok().as_deref() == Some(b"incomplete atomic replacement"));
    let callbacks_after_refusal = (
        rt.creates.load(Ordering::SeqCst),
        rt.starts.load(Ordering::SeqCst),
        rt.inspects.load(Ordering::SeqCst),
        rt.deletes.load(Ordering::SeqCst),
    );
    assert_eq!(
        std::fs::read(&excess).unwrap(),
        b"one extra directory entry"
    );
    std::fs::remove_file(excess).unwrap();

    let recovered = match refused {
        Ok(driver) => driver,
        Err(_) => {
            try_boot_with_budget(dir.path(), rt.clone(), Budget { owned: 0, ran: 0 }).unwrap()
        }
    };
    local(&recovered, recovered.take_recovered().unwrap());
    assert_eq!(recovered.namespace(), namespace);
    assert_eq!(recovered.status(key).unwrap().unwrap().physical, physical);
    assert_eq!(rt.sim.inspect(&physical).unwrap().id, physical);
    assert_eq!(rt.sim.inspect(&other_physical).unwrap().id, other_physical);
    assert!(temps.iter().all(|path| !path.exists()));
    let admission = recovered.submit(d, p);
    let admission_refused = matches!(&admission, Err(Error::Core(Stop::Full { bound: 0, .. })));
    if let Ok((extra, step)) = admission {
        local(&recovered, step);
        local(&recovered, recovered.delete(extra).unwrap());
    }
    let creates = rt.creates.load(Ordering::SeqCst);
    let ended = local(&recovered, recovered.delete(key).unwrap());
    assert!(ended.notices.contains(&Notice::Ended { key }));
    assert!(matches!(
        rt.sim.inspect(&physical),
        Err(Refusal::NoSuchBox(_))
    ));
    assert_eq!(rt.sim.inspect(&other_physical).unwrap().id, other_physical);
    local(&other, other.delete(other_key).unwrap());
    assert!(rt.list().unwrap().is_empty());
    assert_eq!(durable_placement_files(dir.path()).len(), 1);

    assert!(
        typed_refusal,
        "excess entry did not produce the exact typed refusal"
    );
    assert_eq!(after_refusal, durable);
    assert!(temps_untouched, "refused open removed crash temporaries");
    assert_eq!(callbacks_after_refusal, callbacks);
    assert!(admission_refused);
    assert_eq!(
        creates, callbacks.0,
        "recovery granted a new Runtime.create"
    );
}

/// AC-T7.4.1/LB03: an owner that durably requested create-only cannot acquire init execution
/// through a contradictory runner payload during recovery, even inside the retained read floor.
#[test]
fn contradictory_recovered_start_intent_refuses_without_executing_init() {
    let dir = tempfile::tempdir().unwrap();
    let other_dir = tempfile::tempdir().unwrap();
    let rt = Arc::new(Counting::default());
    let driver = try_boot_with_budget(dir.path(), rt.clone(), Budget { owned: 1, ran: 1 }).unwrap();
    let other = boot(other_dir.path(), rt.clone());
    let (d, mut p) = request();
    p.spec.class = ClassSpec::Oci(Box::new(OciSpec {
        rootfs: Rootfs::Dir("/private-simulated-rootfs".into()),
        init: Program::of(&["wait"]),
        mounts: Vec::new(),
        hostname: None,
        network: Network::Private,
        hosts: Vec::new(),
    }));
    assert!(!p.start);
    let (key, step) = driver.submit(d, p).unwrap();
    local(&driver, step);
    let (d, p) = request();
    let (other_key, step) = other.submit(d, p).unwrap();
    local(&other, step);
    let physical = driver.physical_id(key).unwrap();
    let other_physical = other.physical_id(other_key).unwrap();
    assert_eq!(rt.sim.inspect(&physical).unwrap().state, BoxState::Created);
    assert_eq!(rt.starts.load(Ordering::SeqCst), 0);
    drop(driver);

    let runner = dir.path().join(format!("{}.rec", name("ran", key)));
    let original = std::fs::read(&runner).unwrap();
    let mut corrupt: serde_json::Value = serde_json::from_slice(&original).unwrap();
    corrupt["payload"]["start"] = serde_json::json!(true);
    std::fs::write(&runner, serde_json::to_vec(&corrupt).unwrap()).unwrap();
    let owner: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join(format!("{}.rec", name("own", key)))).unwrap(),
    )
    .unwrap();
    assert_eq!(owner["payload"]["start"], false);
    let before = durable_placement_files(dir.path());
    let (recovered, refusal) =
        match try_boot_with_budget(dir.path(), rt.clone(), Budget { owned: 0, ran: 0 }) {
            Ok(driver) => (Some(driver), None),
            Err(why) => (None, Some(why)),
        };
    let starts = rt.starts.load(Ordering::SeqCst);
    let creates = rt.creates.load(Ordering::SeqCst);
    let deletes_before_cleanup = rt.deletes.load(Ordering::SeqCst);
    let observed = rt.sim.inspect(&physical).unwrap();
    let sibling = rt.sim.inspect(&other_physical).unwrap();
    let after = durable_placement_files(dir.path());

    // Retain the observed outcome, then clean up even when the forbidden start occurred.
    let recovered = recovered.unwrap_or_else(|| {
        std::fs::write(&runner, original).unwrap();
        try_boot_with_budget(dir.path(), rt.clone(), Budget { owned: 0, ran: 0 }).unwrap()
    });
    local(&recovered, recovered.delete(key).unwrap());
    assert!(matches!(
        rt.sim.inspect(&physical),
        Err(Refusal::NoSuchBox(_))
    ));
    assert_eq!(rt.sim.inspect(&other_physical).unwrap(), sibling);
    local(&other, other.delete(other_key).unwrap());
    assert!(rt.list().unwrap().is_empty());
    assert_eq!(durable_placement_files(dir.path()).len(), 1);
    eprintln!(
        "contradictory-start expected=Damaged/starts0/Created observed={refusal:?}/starts{starts}/{:?} creates={creates} deletes_before_cleanup={deletes_before_cleanup} cleanup_boxes=0",
        observed.state
    );

    assert_eq!(
        starts, 0,
        "contradictory recovery executed init; state={:?}",
        observed.state
    );
    assert_eq!(observed.state, BoxState::Created);
    assert_eq!(creates, 2);
    assert_eq!(deletes_before_cleanup, 0);
    assert_eq!(after, before);
    assert!(matches!(refusal, Some(Error::Core(Stop::Damaged(_)))));
}
