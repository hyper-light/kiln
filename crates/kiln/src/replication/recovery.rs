//! U15 host recovery scenario. The nemesis only loses transport messages; the product actor owns
//! all elections and commits. Kiln's real Records/inbox, conversation and CD adapters remain in use.
//! This partitions a logical replica while the carrier relay remains available. It does not
//! claim service availability after losing the hub that relays the surviving members' traffic.
use std::collections::BTreeMap;
use std::sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use amphora::database::Database;
use amphora::replication::ReplicaHandle;
use crate::convo::store::Engine as _;

// U15: local observations from the existing fixture relay, returned through its own join.
// These are processing observations, never application/durability acknowledgements.
#[derive(Debug, Default)]
struct TrafficCounts { drained: u64, admitted: u64, completed: u64 }
#[derive(Debug, Default)]
struct NetworkSummary {
    origins: BTreeMap<u64, TrafficCounts>,
    iterations: u64,
    max_loop_gap: Duration,
    max_receipt_age: Duration,
    finishing_at: Option<Duration>,
    // At most RECEIPTS existing identities, sampled once before finishing polls them:
    // (from, to, message, admitted, until). This vector is a snapshot, not an ingress queue.
    pending_at_finish: Vec<(u64, u64, u64, Instant, Instant)>,
}

struct Network {
    isolated: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<(anyhow::Result<()>, NetworkSummary)>>,
}
impl Network {
    fn start(nodes: &BTreeMap<u64, Database>) -> anyhow::Result<Self> {
        let handles: BTreeMap<u64, ReplicaHandle> = nodes.iter().map(|(id, db)| Ok((*id, db.replica().context("missing product replica")?))).collect::<anyhow::Result<_>>()?;
        let isolated = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (cut, done) = (isolated.clone(), stop.clone());
        let join = std::thread::Builder::new().name("u15-host-transport".into()).spawn(move || {
            const RECEIPTS: usize = 64;
            struct PendingReceive {
                from: u64,
                to: u64,
                message: u64,
                admitted: Instant,
                until: Instant,
                receipt: amphora::replication::ReceiveReceipt,
            }
            fn record(first: &mut Option<anyhow::Error>, error: anyhow::Error) {
                tracing::warn!(target: "kiln.replication", event = "test_transport_refused", error = %format!("{error:#}"), "U15 test transport recorded a refusal without an application acknowledgement");
                if first.is_none() { *first = Some(error); }
            }
            let mut incoming: BTreeMap<u64, super::Incoming> = handles.keys().map(|id| (*id, super::Incoming::default())).collect();
            let mut receipts: std::collections::VecDeque<PendingReceive> = std::collections::VecDeque::new();
            let mut next = 0u64;
            let started = Instant::now();
            let mut observed = NetworkSummary { origins: handles.keys().map(|id| (*id, TrafficCounts::default())).collect(), ..Default::default() };
            let mut last_loop = started;
            let mut first_error = None;
            let mut terminal = None;
            let mut finishing = None;
            loop {
                let now = Instant::now();
                observed.iterations = observed.iterations.saturating_add(1);
                observed.max_loop_gap = observed.max_loop_gap.max(now.saturating_duration_since(last_loop));
                last_loop = now;
                if done.load(Ordering::SeqCst) { finishing.get_or_insert_with(|| Instant::now() + Duration::from_secs(3)); }
                if finishing.is_some() && observed.finishing_at.is_none() {
                    observed.finishing_at = Some(started.elapsed());
                    observed.pending_at_finish = receipts.iter().map(|p| (p.from, p.to, p.message, p.admitted, p.until)).collect();
                }
                // U15: poll every admitted record, not only the queue head. One busy actor
                // cannot stop another node's egress or hide its late validation/fencing result.
                for _ in 0..receipts.len() {
                    let Some(mut pending) = receipts.pop_front() else { break; };
                    observed.max_receipt_age = observed.max_receipt_age.max(pending.admitted.elapsed());
                    match pending.receipt.try_result() {
                        Ok(None) => receipts.push_back(pending),
                        Ok(Some(())) => {
                            if let Some(counts) = observed.origins.get_mut(&pending.from) { counts.completed = counts.completed.saturating_add(1); }
                        },
                        Err(error) => {
                            let fenced = matches!(&error, amphora_api::Refusal::Fenced(_));
                            let error = anyhow::Error::new(error).context(format!("U15 test transport message {} {}->{} processing failed after {:?} (transport elapsed {:?}, deadline {:?})", pending.message, pending.from, pending.to, pending.admitted.elapsed(), started.elapsed(), pending.until));
                            if fenced {
                                finishing.get_or_insert_with(|| Instant::now() + Duration::from_secs(3));
                                if first_error.is_some() && terminal.is_none() { terminal = Some(format!("{error:#}")); }
                            }
                            record(&mut first_error, error);
                        }
                    }
                }
                if let Some(until) = finishing {
                    // No new admissions after this cutoff. Observe already-owned packets to
                    // their typed result/deadline, including after an early driver failure.
                    if receipts.is_empty() { break; }
                    if Instant::now() >= until {
                        while let Some(pending) = receipts.pop_front() {
                            record(&mut first_error, anyhow::Error::new(amphora_api::Refusal::Timeout).context(format!("U15 test transport finish could not observe message {} {}->{} before its drain deadline after {:?}", pending.message, pending.from, pending.to, started.elapsed())));
                        }
                        break;
                    }
                } else {
                    let step = (|| -> anyhow::Result<()> {
                        for (from, replica) in &handles {
                            for outgoing in replica.drain(16).with_context(|| format!("U15 test transport drain from {from} failed after {:?}", started.elapsed()))? {
                                if let Some(counts) = observed.origins.get_mut(from) { counts.drained = counts.drained.saturating_add(1); }
                                if *from == cut.load(Ordering::SeqCst) || outgoing.to == cut.load(Ordering::SeqCst) { continue; }
                                let to = outgoing.to;
                                next = next.checked_add(1).context("transport id exhausted")?;
                                for frame in super::frames(*from, to, next, outgoing.bytes)? {
                                    if let Some(bytes) = incoming.get_mut(&to).context("unknown product destination")?.receive(frame, Instant::now()).with_context(|| format!("U15 test transport reassembly message {next} {from}->{to} failed"))? {
                                        if receipts.len() >= RECEIPTS {
                                            return Err(anyhow::Error::new(amphora_api::Refusal::Full { limit: RECEIPTS }).context(format!("U15 test transport receipt admission exhausted for message {next} {from}->{to}")));
                                        }
                                        let admitted = Instant::now();
                                        let until = admitted + Duration::from_secs(2);
                                        let receipt = handles.get(&to).context("missing product destination")?.admit_receive(*from, bytes, until)
                                            .with_context(|| format!("U15 test transport admission message {next} {from}->{to} failed after {:?}", started.elapsed()))?;
                                        if let Some(counts) = observed.origins.get_mut(from) { counts.admitted = counts.admitted.saturating_add(1); }
                                        receipts.push_back(PendingReceive { from: *from, to, message: next, admitted, until, receipt });
                                    }
                                }
                            }
                        }
                        Ok(())
                    })();
                    if let Err(error) = step {
                        finishing.get_or_insert_with(|| Instant::now() + Duration::from_secs(3));
                        if first_error.is_some() && terminal.is_none() { terminal = Some(format!("{error:#}")); }
                        record(&mut first_error, error);
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let result = match first_error {
                Some(error) => Err(match terminal { Some(terminal) => error.context(format!("U15 test transport also terminated after: {terminal}")), None => error }),
                None => Ok(()),
            };
            (result, observed)
        })?;
        Ok(Self { isolated, stop, join: Some(join) })
    }
    fn finish_report(&mut self) -> anyhow::Result<(anyhow::Result<()>, NetworkSummary)> {
        self.stop.store(true, Ordering::SeqCst);
        self.join.take().context("missing transport thread")?.join().map_err(|_| anyhow::anyhow!("transport thread panicked"))
    }
    fn finish(mut self) -> anyhow::Result<()> {
        let (result, _) = self.finish_report()?;
        result
    }
    fn finish_after_bootstrap_failure(mut self, error: anyhow::Error) -> anyhow::Error {
        // U09/U15: cleanup already joins in Drop. Preserve its first transport error as context
        // instead of discarding it, while retaining the original bootstrap error as the source.
        match self.finish_report() {
            Ok((Ok(()), observed)) => error.context(format!("U09/U15 bootstrap failed; test transport finished without a driver error; relay observations: {observed:?}")),
            Ok((Err(transport), observed)) => error.context(format!("U09/U15 bootstrap failed; first test transport failure: {transport:#}; relay observations: {observed:?}")),
            Err(transport) => error.context(format!("U09/U15 bootstrap failed; first test transport failure: {transport:#}")),
        }
    }
}
impl Drop for Network {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() { let _ = join.join(); }
    }
}

fn leader(nodes: &BTreeMap<u64, Database>, excluded: u64) -> anyhow::Result<u64> {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        for (id, database) in nodes {
            if *id == excluded { continue; }
            let replica = database.replica().context("missing product replica")?;
            let status = replica.status()?;
            if status.leader == *id && status.fenced.is_none() { return Ok(*id); }
        }
        anyhow::ensure!(Instant::now() < until, "product did not elect a reachable leader");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// U15: roles can change while the earlier fixture writes commit. Select a follower at the
/// boundary that needs one, rather than retaining the first election's role assignments.
fn follower(nodes: &BTreeMap<u64, Database>) -> anyhow::Result<Database> {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        for database in nodes.values() {
            let status = database.replica().context("missing product replica")?.status()?;
            if status.leader != 0 && status.leader != status.node_id && !status.bootstrapping && status.fenced.is_none() {
                return Ok(database.clone());
            }
        }
        anyhow::ensure!(Instant::now() < until, "product did not expose a ready follower");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn open_nodes(directory: &std::path::Path) -> anyhow::Result<BTreeMap<u64, Database>> {
    let mut nodes = BTreeMap::new();
    for id in 1..=3 {
        let folder = directory.join(id.to_string());
        std::fs::create_dir_all(&folder)?;
        let config = super::Config { id, hub: 1, voters: vec![1, 2, 3], learners: Vec::new(), peers: BTreeMap::from([("runner-a".into(), 2), ("runner-b".into(), 3)]), bootstrap: None };
        nodes.insert(id, super::open_database(&folder.join("amphora.db"), &folder.join("replica"), &config)?);
    }
    Ok(nodes)
}

/// U09/U15: extend the real product-replica fixture, with separate physical key paths per host.
/// Refused initialization returns no Master and cannot overwrite the canonical verifier.
fn secret_bootstrap(nodes: &BTreeMap<u64, Database>, directory: &std::path::Path, provision: bool) -> anyhow::Result<()> {
    use crate::secrets::crypto;
    use crate::sql::OptionalExtension as _;
    const CELL: &str = "secrets/bootstrap-master";
    const SEALED: &str = "u09/test-sealed";
    let phase = if provision { "initial" } else { "restart" };
    let key = [41u8; 32];
    let pin = crate::util::sha256_hex(&key);
    let mut ready = BTreeMap::new();
    for (id, database) in nodes {
        let started = Instant::now();
        let store = crate::store::Store::from_database(database.clone()).map_err(|error| {
            let elapsed = started.elapsed();
            // Cached status is independent of the actor's SQL queue. Keep every result,
            // including a missing handle or a fenced actor, without replacing the first error.
            let statuses: Vec<_> = nodes.iter().map(|(node, database)| (*node, database.replica().map(|replica| replica.status()))).collect();
            error.context(format!("U09/U15 bootstrap {phase} node {id}: open canonical Store and apply migrations failed after {elapsed:?}; all cached replica statuses: {statuses:?}"))
        })?;
        let mut cfg = crate::config::Config::default();
        cfg.server.data_dir = directory.join(id.to_string()).join("secret-bootstrap");
        let path = cfg.data("secrets").join("master.key");
        let before: Option<String> = store.now(|c| Ok(c.query_row("SELECT v FROM kv WHERE k=?1", [CELL], |r| r.get(0)).optional()?)).with_context(|| format!("U09/U15 bootstrap {phase} node {id}: read verifier before refusal checks"))?;
        if provision {
            // No local replica config: the gate must still recognize the actual product owner.
            assert!(crypto::initialize(&cfg, &store).is_err(), "replicated SDK frontend without a pin must not become ready");
            cfg.ci.secrets.trusted_master_sha256 = Some(pin.clone());
            assert!(crypto::initialize(&cfg, &store).is_err(), "replicated master must be preprovisioned");
            assert!(!path.exists(), "missing master was regenerated");
            std::fs::write(&path, [42u8; 32]).with_context(|| format!("U09/U15 bootstrap {phase} node {id}: provision wrong-key fixture"))?;
            assert!(crypto::initialize(&cfg, &store).is_err(), "wrong local master must fail before readiness");
            assert_eq!(store.now(|c| Ok(c.query_row("SELECT v FROM kv WHERE k=?1", [CELL], |r| r.get::<_, String>(0)).optional()?)).with_context(|| format!("U09/U15 bootstrap {phase} node {id}: verify refused initialization left verifier unchanged"))?, before);
            std::fs::write(&path, key).with_context(|| format!("U09/U15 bootstrap {phase} node {id}: provision trusted fixture master"))?;
        } else {
            cfg.ci.secrets.trusted_master_sha256 = Some(pin.clone());
        }
        let cached = crypto::initialize(&cfg, &store).with_context(|| format!("U09/U15 bootstrap {phase} node {id}: initialize trusted master and bind or verify immutable identity"))?;
        let bound: String = store.now(|c| Ok(c.query_row("SELECT v FROM kv WHERE k=?1", [CELL], |r| r.get(0))?)).with_context(|| format!("U09/U15 bootstrap {phase} node {id}: read acknowledged verifier"))?;
        if provision {
            std::fs::write(&path, [43u8; 32]).with_context(|| format!("U09/U15 bootstrap {phase} node {id}: provision rebind-refusal fixture"))?;
            cfg.ci.secrets.trusted_master_sha256 = Some(crate::util::sha256_hex(&[43u8; 32]));
            assert!(crypto::initialize(&cfg, &store).is_err(), "a new self-consistent local pin must not rebind canonical identity");
            assert_eq!(store.now(|c| Ok(c.query_row("SELECT v FROM kv WHERE k=?1", [CELL], |r| r.get::<_, String>(0))?)).with_context(|| format!("U09/U15 bootstrap {phase} node {id}: verify refused rebind left immutable identity unchanged"))?, bound);
            std::fs::write(&path, key).with_context(|| format!("U09/U15 bootstrap {phase} node {id}: restore trusted fixture master"))?;
        }
        ready.insert(*id, (store, cached));
    }
    let elected = leader(nodes, 0).with_context(|| format!("U09/U15 bootstrap {phase}: select current product leader for follower-origin seal"))?;
    let (source_id, (source, master)) = ready.iter().find(|(id, _)| **id != elected).context("U09/U15 bootstrap: missing crypto follower")?;
    if provision {
        let sealed = crypto::seal(&master.bytes(), b"acknowledged cross-node secret", b"u09 fixture").with_context(|| format!("U09/U15 bootstrap {phase} node {source_id}: seal cross-node fixture"))?;
        let value = serde_json::to_string(&sealed).context("U09/U15 bootstrap: encode sealed fixture")?;
        source.now(move |c| { c.execute("INSERT INTO kv(k,v) VALUES (?1,?2)", crate::sql::params![SEALED, value])?; Ok(()) }).with_context(|| format!("U09/U15 bootstrap {phase} node {source_id}: commit follower-origin sealed fixture"))?;
    }
    for (id, (store, master)) in &ready {
        let json: String = store.now(|c| Ok(c.query_row("SELECT v FROM kv WHERE k=?1", [SEALED], |r| r.get(0))?)).with_context(|| format!("U09/U15 bootstrap {phase} node {id}: read replicated sealed fixture"))?;
        let (sealed, nonce): (Vec<u8>, Vec<u8>) = serde_json::from_str(&json).with_context(|| format!("U09/U15 bootstrap {phase} node {id}: decode replicated sealed fixture"))?;
        assert_eq!(crypto::open(&master.bytes(), &sealed, &nonce, b"u09 fixture").with_context(|| format!("U09/U15 bootstrap {phase} node {id}: open cross-node sealed fixture"))?, b"acknowledged cross-node secret", "cross-node sealing and restart must retain the same master");
    }
    Ok(())
}

fn cd_state(database: Database, directory: std::path::PathBuf, write: bool) -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        use kiln_cd::state::{Command, Deployment};
        let status = database.replica().context("missing CD product replica")?.status()?;
        let legacy = directory.join(status.node_id.to_string()).join("cd-legacy");
        if write {
            anyhow::ensure!(status.leader != 0 && status.leader != status.node_id && !status.bootstrapping && status.fenced.is_none(), "CD commands must originate on a ready product follower: {status:?}");
        }
        let bus: Arc<dyn kiln_seams::Bus> = Arc::new(crate::bus::QuiverBus::open(&Default::default(), crate::rt::clock())?);
        let shutdown = crate::rt::Shutdown::new();
        let source = crate::cd::store::SourceConfig::Replica { member: status.node_id, source: None };
        let store = crate::cd::store::Store::open_from(database, &legacy, bus, shutdown.clone(), source).await?;
        let setting = serde_json::json!({ "revision": "acknowledged-revision", "enabled": true });
        let deployment = Deployment { app: "recovery-app".into(), op: 7, revision: Some("acknowledged-revision".into()), trigger: "manual".into(), succeeded: true, message: "acknowledged deployment".into(), applied: 2, at_ms: 3, ..Default::default() };
        let result = async {
            if write {
                store.submit(Command::PutSetting("recovery-setting".into(), setting.clone())).await?.map_err(anyhow::Error::msg)?;
                store.submit(Command::Deployed { deployment: Box::new(deployment.clone()), limit: 10 }).await?.map_err(anyhow::Error::msg)?;
            }
            anyhow::ensure!(store.read(|data| data.settings.get("recovery-setting").cloned()) == Some(setting), "CD lost an acknowledged setting");
            let history = store.read(|data| data.history.get("recovery-app").map(|entries| entries.iter().cloned().collect::<Vec<_>>()));
            anyhow::ensure!(history == Some(vec![Deployment { id: 1, ..deployment }]), "CD lost or duplicated acknowledged deployment history");
            Ok::<(), anyhow::Error>(())
        }.await;
        shutdown.cancel();
        drop(store);
        result
    })
}

fn wait_for_product_drain(directory: &std::path::Path) -> anyhow::Result<()> {
    let until = Instant::now() + Duration::from_secs(10);
    for id in 1..=3 {
        // U15: this public inspection takes the product's exclusive lease. A live CD watch
        // or draining actor must release it before a successful probe can prove disk recovery.
        let path = directory.join(id.to_string()).join("amphora.db");
        loop {
            match amphora::replication::bootstrap::is_replica(&path) {
                Ok(marked) => {
                    anyhow::ensure!(marked, "replica {id} lost its durable product identity");
                    break;
                }
                Err(error) if matches!(&error, amphora_api::Refusal::Unavailable(_) | amphora_api::Refusal::Timeout) => {
                    anyhow::ensure!(Instant::now() < until, "replica {id} did not finish product-owned drain: {error}");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

#[test]
fn u15_acknowledged_conversation_and_inbox_survive_product_leader_loss_and_restart_without_duplicate_send() -> anyhow::Result<()> {
    let cache = std::path::PathBuf::from(std::env::var_os("HOME").context("HOME")?).join(".cache");
    std::fs::create_dir_all(&cache)?;
    let directory = tempfile::tempdir_in(cache)?;
    let nodes = open_nodes(directory.path())?;
    let network = Network::start(&nodes)?;
    // Originate actual adaptive SQL and Records calls on a follower. Product-owned ReadIndex
    // and proposal forwarding must carry both the captured answer and durable application.
    let database = follower(&nodes)?;
    let shutdown = crate::rt::Shutdown::new();
    let waiting = database.clone();
    crate::rt::block_on(async move { crate::rt::timeout(Duration::from_secs(10), super::wait_for_writer(&waiting, &shutdown)).await? })?;
    if let Err(error) = secret_bootstrap(&nodes, directory.path(), true) {
        return Err(network.finish_after_bootstrap_failure(error));
    }
    let mut conversation = crate::convo::amphora::Amphora::from_database(database.clone())?;
    conversation.apply(&[
        crate::convo::rows::Row::Session { session: "session".into(), info: Some(r#"{"id":"session"}"#.into()), meta: None, state: None, changes: None, epoch: Some(1), open: true, at: 1 },
        crate::convo::rows::Row::Event(Box::new(crate::convo::rows::EventRow { session: "session".into(), id: "event".into(), seq: 1, at: 2, parent: None, by: None, kind: "user".into(), level: None, tool: None, status: None, text: "acknowledged conversation".into(), body: "{}".into() })),
    ])?;
    let records = Arc::new(crate::records::from_database(&database, "host")?);
    let inbox = crate::harness::inbox::Inbox::new(records, Default::default());
    let accepted = crate::rt::block_on(async move {
        inbox.open("session", 1).await?;
        Ok(inbox.accept("session", "message", "digest", Some(1), 3).await?)
    })?;
    assert!(!accepted.repeat);
    // U15/B2: these CD commands originate on a current follower through the actual adapter;
    // their ACKs must represent the same product quorum as conversation and inbox writes.
    // Each logical host has its own legacy directory. Canonical provenance must allow
    // another replica to consume completed state without revalidating a foreign path.
    let legacy = directory.path().to_path_buf();
    cd_state(follower(&nodes)?, legacy.clone(), true)?;
    drop(conversation);
    // Lose the leader that owns the product now, after all acknowledged fixture writes.
    let original = leader(&nodes, 0)?;
    let former_leader = nodes.get(&original).context("missing leader")?.clone();
    network.isolated.store(original, Ordering::SeqCst);
    let successor = leader(&nodes, original)?;
    let next = nodes.get(&successor).context("missing successor")?.clone();
    cd_state(next.clone(), legacy.clone(), false)?;
    let mut recovered = crate::convo::amphora::Amphora::from_database(next.clone())?;
    let events = recovered.events("session", None, 10)?;
    assert_eq!(events.len(), 1);
    assert_eq!(events.first().map(|e| e.text.as_str()), Some("acknowledged conversation"));
    let inbox = crate::harness::inbox::Inbox::new(Arc::new(crate::records::from_database(&next, "host")?), Default::default());
    let expected = accepted.clone();
    crate::rt::block_on(async move {
        inbox.open("session", 1).await?;
        let repeated = inbox.accept("session", "message", "digest", Some(1), 4).await?;
        anyhow::ensure!(repeated.repeat && repeated.seq == expected.seq && repeated.at == expected.at, "a retry lost the first durable send acknowledgement");
        anyhow::ensure!(inbox.taken("session", 10).await? == vec!["message".to_owned()], "acknowledged send duplicated after leader loss");
        Ok::<(), anyhow::Error>(())
    })?;
    // An isolated former leader cannot acknowledge a divergent SQL session.
    assert!(former_leader.call(Instant::now() + Duration::from_millis(250), |c| { c.execute_batch("CREATE TABLE divergent(x)")?; Ok(()) }).is_err());
    let divergent = next.call(crate::store::deadline(), |c| Ok(c.query_row("SELECT count(*) FROM sqlite_schema WHERE name='divergent'", [], |r| r.get::<_, i64>(0))?))?;
    assert_eq!(divergent, 0);
    drop(recovered);
    drop(next);
    drop(database);
    drop(former_leader);
    network.finish()?;
    drop(nodes);
    wait_for_product_drain(directory.path())?;
    // Reopen every durable log and state machine; adapters reload canonical product state.
    let nodes = open_nodes(directory.path())?;
    let network = Network::start(&nodes)?;
    let writer = leader(&nodes, 0)?;
    if let Err(error) = secret_bootstrap(&nodes, directory.path(), false) {
        return Err(network.finish_after_bootstrap_failure(error));
    }
    let database = nodes.get(&writer).context("missing restarted leader")?.clone();
    cd_state(database.clone(), legacy, false)?;
    let mut conversation = crate::convo::amphora::Amphora::from_database(database.clone())?;
    assert_eq!(conversation.events("session", None, 10)?.len(), 1);
    let inbox = crate::harness::inbox::Inbox::new(Arc::new(crate::records::from_database(&database, "host")?), Default::default());
    crate::rt::block_on(async move {
        inbox.open("session", 1).await?;
        let repeated = inbox.accept("session", "message", "digest", Some(1), 5).await?;
        anyhow::ensure!(repeated.repeat && repeated.seq == accepted.seq && repeated.at == accepted.at, "restart lost the durable send acknowledgement");
        anyhow::ensure!(inbox.taken("session", 10).await? == vec!["message".to_owned()], "restart duplicated an acknowledged send");
        Ok::<(), anyhow::Error>(())
    })?;
    drop(conversation);
    drop(database);
    network.finish()?;
    drop(nodes);
    wait_for_product_drain(directory.path())?;
    Ok(())
}
