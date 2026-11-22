//! SDK attachment to the canonical owner. No database or Raft actor is created for clients.
use super::*;
use serde::{Deserialize, Serialize};

pub(super) struct Backend {
    pub(super) client: crate::transport::Client,
    /// The descriptor its calls' socket holds, charged to the process's descriptor budget while
    /// this client lives (local-owner-transport.md, "One descriptor budget", item 6).
    _descriptor: crate::descriptors::Held,
    path: PathBuf,
    restart: Mutex<
        Option<(
            PathBuf,
            api::replication::ReplicaConfig,
            api::replication::Durability,
        )>,
    >,
    promoted: Mutex<Option<Database>>,
}
impl Backend {
    pub(super) fn configure(
        &self,
        replica: Option<(
            &Path,
            &api::replication::ReplicaConfig,
            &api::replication::Durability,
        )>,
    ) -> Result<(), Refusal> {
        if let Some((path, config, durability)) = replica {
            let mut restart = locked(&self.restart)?;
            match restart.as_ref() {
                Some((existing, current, acknowledged))
                    if existing != path || current != config || acknowledged != durability =>
                {
                    return Err(api::invalid(
                        "replica ownership",
                        "client has different reopening configuration",
                    ));
                }
                Some(_) => (),
                None => *restart = Some((path.to_path_buf(), config.clone(), durability.clone())),
            }
        }
        Ok(())
    }
    fn resolve(&self, until: Instant) -> Result<Database, Refusal> {
        let mut promoted = locked_until(&self.promoted, until)?;
        if let Some(database) = promoted.as_ref() {
            return Ok(database.clone());
        }
        // Selection sends only authentication and identity probes. No application request or
        // FnOnce is submitted until selection finishes, so this cannot replay an unknown write.
        let restart = locked(&self.restart)?.clone();
        let database = match restart {
            Some((log, config, durability)) => {
                Database::open_replicated_with(&self.path, &log, config, durability, until)?
            }
            None => Database::open(&self.path, until)?,
        };
        if !matches!(&database.shared.writer, Writer::Client(_)) {
            *promoted = Some(database.clone());
        }
        Ok(database)
    }
    pub(super) fn replica(&self) -> Option<crate::replication::ReplicaHandle> {
        let promoted = self.promoted.try_lock().ok()?;
        match &promoted.as_ref()?.shared.writer {
            Writer::Replicated { client, .. } => Some(client.handle()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Identity {
    pub restoring: bool,
    pub replica: Option<(String, api::replication::ReplicaConfig)>,
    /// A replicated owner's durability, its fates in member order (replication.md, "Fates and
    /// durability checked"); added at wire::VERSION 4.
    pub durability: Option<api::replication::Durability>,
}

pub(super) fn publish(database: &Database, path: &Path) -> Result<(), Refusal> {
    match database.shared.path.set(path.to_path_buf()) {
        Ok(()) => (),
        Err(path) if database.shared.path.get() == Some(&path) => (),
        Err(_) => {
            return Err(api::invalid(
                "database owner",
                "canonical endpoint path changed",
            ));
        }
    }
    if database.shared.restoring.load(Ordering::Acquire) {
        return Ok(());
    }
    if matches!(database.shared.writer, Writer::Client(_)) {
        return Err(api::invalid(
            "database owner",
            "a client cannot publish another owner",
        ));
    }
    let mut service = locked(&database.shared.transport)?;
    if service.is_none() {
        let weak = Arc::downgrade(&database.shared);
        *service = Some(crate::transport::Owner::start(path, move || {
            weak.upgrade().map(|shared| Database {
                shared,
                application: None,
            })
        })?);
    }
    Ok(())
}

pub(super) fn attach(
    path: &Path,
    replica: Option<(
        &Path,
        &api::replication::ReplicaConfig,
        &api::replication::Durability,
    )>,
    descriptor: crate::descriptors::Held,
    until: Instant,
) -> Result<Database, Refusal> {
    let client = crate::transport::Client::connect(path, until)?;
    let identity = validate(&client, replica, until)?;
    // A client that stated no replica reopens one in the class its owner names.
    let restart = match replica {
        Some((path, config, durability)) => {
            Some((path.to_path_buf(), config.clone(), durability.clone()))
        }
        None => match (identity.replica, identity.durability) {
            (Some((path, config)), Some(durability)) => {
                Some((PathBuf::from(path), config, durability))
            }
            (Some(_), None) => {
                return Err(api::invalid(
                    "replica ownership",
                    "the active owner names no durability",
                ));
            }
            (None, _) => None,
        },
    };
    Ok(Database {
        application: None,
        shared: Arc::new(Shared {
            transport: Mutex::new(None),
            path: OnceLock::new(),
            writer: Writer::Client(Backend {
                client,
                _descriptor: descriptor,
                path: path.to_path_buf(),
                restart: Mutex::new(restart),
                promoted: Mutex::new(None),
            }),
            changed: Arc::new(Changed::default()),
            restoring: AtomicBool::new(false),
            admitted: Arc::new(AtomicUsize::new(0)),
            stamps: std::sync::OnceLock::new(),
            _owner: None,
        }),
    })
}

pub(super) fn validate(
    client: &crate::transport::Client,
    replica: Option<(
        &Path,
        &api::replication::ReplicaConfig,
        &api::replication::Durability,
    )>,
    until: Instant,
) -> Result<Identity, Refusal> {
    let identity = client
        .identity(until)
        .map_err(|error| match Refusal::from(error) {
            Refusal::UnknownOutcome => unavailable("owner identity probe interrupted"),
            other => other,
        })?;
    if identity.restoring {
        return Err(api::invalid("database owner", "restore is incomplete"));
    }
    if let Some((path, config, durability)) = replica {
        let matches = identity
            .replica
            .as_ref()
            .is_some_and(|(actual, current)| Path::new(actual) == path && current == config);
        if !matches {
            return Err(api::invalid(
                "replica ownership",
                "the active owner has different replication configuration",
            ));
        }
        if identity.durability.as_ref() != Some(durability) {
            return Err(api::invalid(
                "replica ownership",
                "the active owner acknowledges writes under a different durability",
            ));
        }
    }
    Ok(identity)
}

impl Database {
    /// Reads the current owner's replication readiness without SQL or ownership promotion.
    /// None identifies a standalone owner. An unreachable configured owner returns a typed
    /// refusal, so a client cannot mistake an unavailable or bootstrapping replica for readiness.
    pub fn replica_status(
        &self,
        until: Instant,
    ) -> Result<Option<api::replication::ReplicaStatus>, Refusal> {
        if Instant::now() >= until {
            return Err(Refusal::Timeout);
        }
        let status = match &self.shared.writer {
            Writer::Local(_) => None,
            Writer::Replicated { client, .. } => Some(client.handle().status()?),
            Writer::Client(backend) => {
                let configured = locked_until(&backend.restart, until)?.is_some();
                let status = backend.client.replica_status(until).map_err(|error| {
                    match Refusal::from(error) {
                        Refusal::UnknownOutcome => unavailable("owner status probe interrupted"),
                        other => other,
                    }
                })?;
                if configured && status.is_none() {
                    return Err(unavailable(
                        "configured owner replication status unavailable",
                    ));
                }
                status
            }
        };
        if Instant::now() >= until {
            return Err(Refusal::Timeout);
        }
        Ok(status)
    }

    /// Attaches to an already-running configured owner without opening or inspecting its engine.
    /// None means no owner holds the lease at this instant; `open_replicated` rechecks ownership
    /// after the host prepares an unowned database. A client retains this full reopening config.
    pub fn attach_replicated(
        path: &Path,
        log_path: &Path,
        config: api::replication::ReplicaConfig,
        until: Instant,
    ) -> Result<Option<Self>, Refusal> {
        Self::attach_owner(
            path,
            log_path,
            config,
            api::replication::Durability::Persisted,
            None,
            until,
        )
    }

    /// [`Database::attach_replicated`], reopening as `durability` says if this client takes the
    /// owner's place.
    pub fn attach_replicated_with(
        path: &Path,
        log_path: &Path,
        config: api::replication::ReplicaConfig,
        durability: api::replication::Durability,
        until: Instant,
    ) -> Result<Option<Self>, Refusal> {
        Self::attach_owner(path, log_path, config, durability, None, until)
    }

    /// [`Database::attach_replicated`] for a host that owns with `guards`
    /// ([`Database::open_replicated_guarded`]): it makes no directory and never attaches as a
    /// client. It answers this process's live owner opened at this file with guards of the same
    /// key and this configuration, or None when no owner holds the lease; an unguarded owner,
    /// other guards, another file at the path, or another process's owner is refused.
    pub fn attach_replicated_guarded(
        path: &Path,
        log_path: &Path,
        config: api::replication::ReplicaConfig,
        guards: Guards,
        until: Instant,
    ) -> Result<Option<Self>, Refusal> {
        Self::attach_owner(
            path,
            log_path,
            config,
            api::replication::Durability::Persisted,
            Some(guards),
            until,
        )
    }

    fn attach_owner(
        path: &Path,
        log_path: &Path,
        config: api::replication::ReplicaConfig,
        durability: api::replication::Durability,
        guards: Option<Guards>,
        until: Instant,
    ) -> Result<Option<Self>, Refusal> {
        if Instant::now() >= until {
            return Err(Refusal::Timeout);
        }
        let (path, log) = match guards {
            Some(_) => (existing_path(path)?, existing_path(log_path)?),
            None => (resolved_path(path)?, resolved_path(log_path)?),
        };
        log.to_str()
            .ok_or_else(|| api::invalid("replica path", "not UTF-8"))?;
        let durability = canonical(durability);
        let expected = Some((log.as_path(), &config, &durability));
        let mut owners = locked_until(OWNERS.get_or_init(Mutex::default), until)?;
        forget_drained(&mut owners)?;
        let identity = file_identity(&path)?;
        if let Some(owner) = owners.get(&path).or_else(|| {
            identity.and_then(|id| owners.values().find(|owner| owner.identity == Some(id)))
        }) {
            if let Some(shared) = owner.shared.upgrade() {
                if let Some(guards) = &guards {
                    admit_guarded(owner, &shared, identity, guards)?;
                }
                if shared.restoring.load(Ordering::Acquire) {
                    return Err(api::invalid("replica ownership", "restore is incomplete"));
                }
                match &shared.writer {
                    Writer::Client(backend) => match validate(&backend.client, expected, until) {
                        Ok(_) => {
                            backend.configure(expected)?;
                            return Ok(Some(Self {
                                shared,
                                application: None,
                            }));
                        }
                        Err(
                            Refusal::Unavailable(_) | Refusal::Timeout | Refusal::UnknownOutcome,
                        ) => (),
                        Err(error) => return Err(error),
                    },
                    Writer::Replicated {
                        log_path,
                        config: current,
                        durability: acknowledged,
                        ..
                    } if log_path == &log && current == &config && acknowledged == &durability => {
                        return Ok(Some(Self {
                            shared,
                            application: None,
                        }));
                    }
                    _ => {
                        return Err(api::invalid(
                            "replica ownership",
                            "a different live owner must drain",
                        ));
                    }
                }
            }
            if owner.lease.strong_count() > 0 {
                return Err(unavailable("previous database owner is draining"));
            }
        }
        // Admission before the probe: a probe makes the endpoint directory and takes the lease,
        // which a spent descriptor budget would refuse to anyone who went on to open. The charge
        // is the probe's lease, then the attached client's socket (local-owner-transport.md, "One
        // descriptor budget", item 6).
        let descriptor = crate::descriptors::process()?.take("open descriptors", 1)?;
        // No owner holds a lease under a directory that doesn't exist; attach makes no directory.
        if !parent(&path).try_exists().map_err(unavailable)? {
            return Ok(None);
        }
        let lease = match guards {
            // The path is canonical and its parent exists: the probe makes no directory above.
            Some(_) => crate::transport::try_lease(&path)?,
            None => try_exclusive_file(&path)?,
        };
        if let Some(lease) = lease {
            drop(lease);
            return Ok(None);
        }
        if guards.is_some() {
            if !drained_here(&path)? {
                return Err(remote_owner());
            }
            // This process drained the owner here: a child it forked may share the lease until its
            // exec. Wait for it to the deadline; once free, no owner holds it.
            drop(owners);
            let (since, mut pause) = (Instant::now(), None);
            let mut probed = since;
            loop {
                if Instant::now() >= until {
                    lease_held_since_drain(&path);
                    return Err(Refusal::Timeout);
                }
                pause_for_lease(&mut pause, probed, since, until);
                probed = Instant::now();
                if let Some(lease) = crate::transport::try_lease(&path)? {
                    drop(lease);
                    drain_window_taken(since);
                    return Ok(None);
                }
            }
        }
        let database = attach(&path, expected, descriptor, until)?;
        owners.insert(
            path,
            Owner {
                shared: Arc::downgrade(&database.shared),
                lease: Weak::new(),
                identity,
                key: None,
            },
        );
        Ok(Some(database))
    }

    pub(super) fn resolve(&self, until: Instant) -> Result<Database, Refusal> {
        let mut database = match &self.shared.writer {
            Writer::Client(backend) => backend.resolve(until),
            _ => Ok(self.clone()),
        }?;
        database.application = self.application.clone();
        Ok(database)
    }
    /// Selects the current host owner before carrying replication messages. A standby returns
    /// None; after its predecessor releases ownership this handle starts the same configured
    /// product actor and returns its carrier handle. Submitted application calls are never retried.
    pub fn ensure_owner(
        &self,
        until: Instant,
    ) -> Result<Option<crate::replication::ReplicaHandle>, Refusal> {
        let database = self.resolve(until)?;
        Ok(match &database.shared.writer {
            Writer::Replicated { client, .. } => Some(client.handle()),
            Writer::Local(_) | Writer::Client(_) => None,
        })
    }
    pub(crate) fn transport_identity(&self) -> Result<Identity, Refusal> {
        let (replica, durability) = match &self.shared.writer {
            Writer::Replicated {
                log_path,
                config,
                durability,
                ..
            } => (
                Some((
                    log_path
                        .to_str()
                        .ok_or_else(|| api::invalid("replica path", "not UTF-8"))?
                        .to_owned(),
                    config.clone(),
                )),
                Some(durability.clone()),
            ),
            Writer::Local(_) => (None, None),
            Writer::Client(_) => {
                return Err(api::invalid(
                    "database owner",
                    "client handle cannot serve another client",
                ));
            }
        };
        Ok(Identity {
            restoring: self.shared.restoring.load(Ordering::Acquire),
            replica,
            durability,
        })
    }
    pub(super) fn remote_client(&self) -> Option<crate::transport::Client> {
        match &self.shared.writer {
            Writer::Client(backend) => {
                Some(backend.client.for_application(self.application.clone()))
            }
            _ => None,
        }
    }
}
impl Namespace {
    pub(super) fn remote(&self) -> Result<Option<crate::transport::Namespace>, Refusal> {
        self.database
            .remote_client()
            .map(|client| client.namespace(&self.name))
            .transpose()
    }
}

pub(super) fn submit(
    client: &crate::transport::Client,
    namespace: &str,
    operation: Operation,
    until: Instant,
) -> Result<Answer, Refusal> {
    match operation {
        Operation::Mutation(Mutation::Sql(statements)) => {
            return client
                .execute(statements, until)
                .map(Answer::Sql)
                .map_err(Refusal::from);
        }
        Operation::Mutation(Mutation::Migrate(migration)) => {
            return client
                .migrate(migration, until)
                .map(Answer::Migrated)
                .map_err(Refusal::from);
        }
        Operation::Call(_) => {
            return Err(api::invalid(
                "SQL session",
                "client calls must use the public SQL session before capturing the closure",
            ));
        }
        _ => (),
    }
    let records = client.namespace(namespace)?;
    match operation {
        Operation::Mutation(Mutation::Records { tx, .. }) => {
            records.commit(tx, until).map(Answer::Rev)
        }
        Operation::Mutation(Mutation::Compact { before, .. }) => {
            records.compact(before, until)?;
            Ok(Answer::Done)
        }
        Operation::Mutation(Mutation::CompactStep { before, .. }) => {
            records.compact_step(before, until).map(Answer::Compacted)
        }
        Operation::Get(family, key, at) => records.get(&family, &key, at, until).map(Answer::Row),
        Operation::Scan(family, scan) => records.scan(&family, scan, until).map(Answer::Page),
        Operation::Changes(family, after, limit) => records
            .changes(&family, after, limit, until)
            .map(Answer::Changes),
        Operation::Head => records.head(until).map(Answer::Rev),
        Operation::Import(page) => {
            records.import_page(page, until)?;
            Ok(Answer::Done)
        }
        Operation::ImportProgress => records.import_state(until).map(Answer::ImportProgress),
        Operation::Call(_) | Operation::Mutation(Mutation::Sql(_) | Mutation::Migrate(_)) => {
            Err(api::invalid("client operation", "unexpected dispatch"))
        }
        // A group's holding is its own engine's (CP-T1.9), never a remote client's to send.
        Operation::Mutation(Mutation::Holding(_)) => Err(api::invalid(
            "holding",
            "a group's holding is kept by the group's own engine",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn until() -> Instant {
        Instant::now() + crate::transport::HANG
    }

    fn config() -> api::replication::ReplicaConfig {
        api::replication::ReplicaConfig {
            node_id: 1,
            voters: vec![1],
            learners: vec![],
            bootstrap: None,
        }
    }

    /// The entries of `dir`, to compare before and after an open.
    fn entries(dir: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn refused_replicated_opens_leave_no_directory_they_made() {
        // CP-T1.5 (U05), local-owner-transport.md "Owner guards": an unguarded open_replicated
        // or attach_replicated refused by the owner table or by its configuration makes no
        // directory first, and an attach that finds no owner makes none either.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let owner =
            Database::open_replicated(&path, &dir.path().join("log"), config(), until()).unwrap();
        let before = entries(dir.path());
        let elsewhere = dir.path().join("new").join("log");
        let refused = Database::open_replicated(&path, &elsewhere, config(), until());
        assert!(
            matches!(refused, Err(Refusal::Invalid { .. })),
            "{:?}",
            refused.err()
        );
        let refused = Database::attach_replicated(&path, &elsewhere, config(), until());
        assert!(
            matches!(refused, Err(Refusal::Invalid { .. })),
            "{:?}",
            refused.err()
        );
        let invalid = api::replication::ReplicaConfig {
            node_id: 0,
            ..config()
        };
        let (missing_db, missing_log) = (
            dir.path().join("a").join("db"),
            dir.path().join("b").join("log"),
        );
        let refused = Database::open_replicated(&missing_db, &missing_log, invalid, until());
        assert!(
            matches!(refused, Err(Refusal::Invalid { .. })),
            "{:?}",
            refused.err()
        );
        let attached = Database::attach_replicated(&missing_db, &missing_log, config(), until());
        assert!(
            matches!(attached, Ok(None)),
            "{:?}",
            attached.map(|d| d.is_some())
        );
        assert_eq!(
            entries(dir.path()),
            before,
            "a refused open made a directory"
        );
        drop(owner);
    }

    #[test]
    fn a_replicated_open_refused_for_its_durability_makes_no_directory() {
        // CP-T1.5 (U05) and CP-T1.8, local-owner-transport.md "Owner guards": a durability the
        // configuration can't take (one fate asked, below the two a held quorum needs) is refused
        // with the configuration's other refusals, before the log's lease makes a directory.
        let dir = tempfile::tempdir().unwrap();
        let before = entries(dir.path());
        let one_fate = api::replication::Durability::Replicated(api::replication::Fates {
            of: vec![(1, 1)],
            spanning: 1,
        });
        let refused = Database::open_replicated_with(
            &dir.path().join("a").join("db"),
            &dir.path().join("b").join("log"),
            config(),
            one_fate,
            until(),
        );
        assert!(
            matches!(&refused, Err(Refusal::Invalid { what, .. }) if what == "replicated class"),
            "{:?}",
            refused.err()
        );
        assert_eq!(
            entries(dir.path()),
            before,
            "a refused open made a directory"
        );
    }

    /// CP-T1.5 (U05), AC-FD.1.4 (local-owner-transport.md, "One descriptor budget", item 6): with
    /// the descriptor budget spent, open, open_replicated and attach_replicated under directories
    /// that don't exist are refused at the budget and make none of them. Do: a budget of 2, both
    /// held; the three opens. Expect: each refused TooLarge at 2; no directory made.
    #[test]
    fn opens_refused_at_the_descriptor_budget_make_no_directory() {
        let budget = crate::descriptors::tests::budget(2);
        let _spent = budget.take("open descriptors", 2).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let before = entries(dir.path());
        let (db, log) = (
            dir.path().join("a").join("db"),
            dir.path().join("b").join("log"),
        );
        let full = |refused: Result<(), Refusal>| {
            assert!(
                matches!(refused, Err(Refusal::TooLarge { limit: 2, .. })),
                "{refused:?}"
            );
        };
        full(Database::open(&db, until()).map(drop));
        full(Database::open_replicated(&db, &log, config(), until()).map(drop));
        full(Database::attach_replicated(&db, &log, config(), until()).map(drop));
        assert_eq!(
            entries(dir.path()),
            before,
            "a refused open made a directory"
        );
    }

    /// CP-T1.5 (U05), AC-FD.1.4 (local-owner-transport.md, "One descriptor budget", item 6):
    /// attach with the descriptor budget spent, on a database no process owns, is refused at the
    /// budget and leaves no endpoint directory or lease behind. Do: a budget of 2, both held;
    /// attach_replicated. Expect: refused TooLarge at 2; no endpoint directory.
    #[test]
    fn attach_refuses_at_the_descriptor_budget_before_it_touches_the_lease() {
        let budget = crate::descriptors::tests::budget(2);
        let _spent = budget.take("open descriptors", 2).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let attached =
            Database::attach_replicated(&path, &dir.path().join("log"), config(), until());
        assert!(
            matches!(attached, Err(Refusal::TooLarge { limit: 2, .. })),
            "attach at the budget: {:?}",
            attached.map(|d| d.is_some())
        );
        let mut sidecar = path.into_os_string();
        sidecar.push(".client");
        assert!(
            !Path::new(&sidecar).exists(),
            "the refused attach made the endpoint directory"
        );
    }

    /// AC-FD.1.4, T-FD.1.4 (local-owner-transport.md, "One descriptor budget", item 6): an owner
    /// charges its lease to the process's descriptor budget, not to a count of owners, and gives
    /// it back once drained. Do: a budget of 1; open one database; open a second; drop the first
    /// and, once its lease's descriptor is given back, open the second again. Expect: the first
    /// open holds 1; the second refused TooLarge at 1; then served.
    #[test]
    fn an_owner_charges_its_lease_to_the_descriptor_budget() {
        let budget = crate::descriptors::tests::budget(1);
        let dir = tempfile::tempdir().unwrap();
        let first = Database::open(&dir.path().join("first"), until()).unwrap();
        assert_eq!(budget.held(), 1);
        let second = dir.path().join("second");
        let refused = Database::open(&second, until()).map(drop);
        assert!(
            matches!(refused, Err(Refusal::TooLarge { limit: 1, .. })),
            "{refused:?}"
        );
        drop(first);
        // The owner drains on its own threads; the lease's descriptor is given back after.
        let deadline = until();
        while budget.held() > 0 {
            assert!(
                Instant::now() < deadline,
                "the drained owner's lease was kept"
            );
            std::thread::park_timeout(std::time::Duration::from_millis(1));
        }
        drop(Database::open(&second, until()).unwrap());
    }

    #[test]
    fn a_guarded_owner_refuses_a_replicated_open_or_attach_in_this_process() {
        // CP-T1.8 (AC-T1.8.3), replication.md "What it guarantees": guarded opens are persisted,
        // so an open or attach stating the replicated class at a guarded owner in this process is
        // refused, typed, as a different live owner, never answered persisted in its place. A
        // persisted attach reuses the owner.
        let dir = tempfile::tempdir().unwrap();
        let (path, log) = (dir.path().join("db"), dir.path().join("log"));
        let config = api::replication::ReplicaConfig {
            voters: vec![1, 2],
            ..config()
        };
        let owner = Database::open_replicated_guarded(
            &path,
            &log,
            config.clone(),
            Guards::new([1; 32], Arc::new(())),
            until(),
        )
        .unwrap();
        // Two members of two fates: a class the configuration can span.
        let replicated = api::replication::Durability::Replicated(api::replication::Fates {
            of: vec![(1, 1), (2, 2)],
            spanning: 2,
        });
        let different = |refused: Result<(), Refusal>| {
            assert!(
                matches!(
                    &refused,
                    Err(Refusal::Invalid { what, why })
                        if what == "replica ownership" && why.contains("different live owner")
                ),
                "{refused:?}"
            );
        };
        different(
            Database::attach_replicated_with(
                &path,
                &log,
                config.clone(),
                replicated.clone(),
                until(),
            )
            .map(drop),
        );
        different(
            Database::open_replicated_with(&path, &log, config.clone(), replicated, until())
                .map(drop),
        );
        let attached = Database::attach_replicated(&path, &log, config, until()).unwrap();
        assert!(attached.is_some(), "the persisted attach found no owner");
        drop(owner);
    }

    /// The owner's identity at wire::VERSION 3, before it named a durability.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Before {
        restoring: bool,
        replica: Option<(String, api::replication::ReplicaConfig)>,
    }

    /// CP-T1.5 and CP-T1.8 (AC-T1.8.5), why the identity's durability is wire::VERSION 4 (replication.md, "Fates and
    /// durability checked"; local-owner-transport.md, "Versions"). Do: encode an identity naming a
    /// durability and read it as version 3's; encode one of version 3 and read it as today's.
    /// Expect: version 3's reader takes the frame without its durability, silently, so the two
    /// can't share a version and the greeting refuses the other; today's refuses the shorter frame
    /// rather than reading a durability into it.
    #[test]
    fn the_identitys_durability_is_a_new_version() {
        let config = api::replication::ReplicaConfig {
            node_id: 1,
            voters: vec![1, 2, 3],
            learners: Vec::new(),
            bootstrap: None,
        };
        let replica = Some(("raft.log".to_owned(), config));
        let named = Identity {
            restoring: false,
            replica: replica.clone(),
            durability: Some(api::replication::Durability::Replicated(
                api::replication::Fates {
                    of: vec![(1, 10), (2, 20), (3, 30)],
                    spanning: 2,
                },
            )),
        };
        let bytes = postcard::to_allocvec(&named).unwrap();
        assert_eq!(
            postcard::from_bytes::<Before>(&bytes).unwrap(),
            Before {
                restoring: false,
                replica: replica.clone(),
            }
        );
        let before = postcard::to_allocvec(&Before {
            restoring: false,
            replica,
        })
        .unwrap();
        assert!(postcard::from_bytes::<Identity>(&before).is_err());
    }
}
