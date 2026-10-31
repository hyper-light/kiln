//! SDK attachment to the canonical owner. No database or Raft actor is created for clients.
use super::*;
use serde::{Deserialize, Serialize};

pub(super) struct Backend {
    pub(super) client: crate::transport::Client,
    path: PathBuf,
    restart: Mutex<Option<(PathBuf, api::replication::ReplicaConfig)>>,
    promoted: Mutex<Option<Database>>,
}
impl Backend {
    pub(super) fn configure(
        &self,
        replica: Option<(&Path, &api::replication::ReplicaConfig)>,
    ) -> Result<(), Refusal> {
        if let Some((path, config)) = replica {
            let mut restart = locked(&self.restart)?;
            match restart.as_ref() {
                Some((existing, current)) if existing != path || current != config => {
                    return Err(api::invalid(
                        "replica ownership",
                        "client has different reopening configuration",
                    ));
                }
                Some(_) => (),
                None => *restart = Some((path.to_path_buf(), config.clone())),
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
            Some((log, config)) => Database::open_replicated(&self.path, &log, config, until)?,
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
            weak.upgrade().map(|shared| Database { shared })
        })?);
    }
    Ok(())
}

pub(super) fn attach(
    path: &Path,
    replica: Option<(&Path, &api::replication::ReplicaConfig)>,
    until: Instant,
) -> Result<Database, Refusal> {
    let client = crate::transport::Client::connect(path, until)?;
    let identity = validate(&client, replica, until)?;
    let restart = match replica {
        Some((path, config)) => Some((path.to_path_buf(), config.clone())),
        None => identity
            .replica
            .map(|(path, config)| (PathBuf::from(path), config)),
    };
    Ok(Database {
        shared: Arc::new(Shared {
            transport: Mutex::new(None),
            path: OnceLock::new(),
            writer: Writer::Client(Backend {
                client,
                path: path.to_path_buf(),
                restart: Mutex::new(restart),
                promoted: Mutex::new(None),
            }),
            changed: Arc::new(Changed::default()),
            restoring: AtomicBool::new(false),
            admitted: Arc::new(AtomicUsize::new(0)),
            _owner: None,
        }),
    })
}

pub(super) fn validate(
    client: &crate::transport::Client,
    replica: Option<(&Path, &api::replication::ReplicaConfig)>,
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
    if let Some((path, config)) = replica {
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
        if Instant::now() >= until {
            return Err(Refusal::Timeout);
        }
        let path = canonical_path(path)?;
        let log = canonical_path(log_path)?;
        log.to_str()
            .ok_or_else(|| api::invalid("replica path", "not UTF-8"))?;
        let expected = Some((log.as_path(), &config));
        let mut owners = locked_until(OWNERS.get_or_init(Mutex::default), until)?;
        owners.retain(|_, owner| owner.shared.strong_count() > 0 || owner.lease.strong_count() > 0);
        let identity = file_identity(&path)?;
        if let Some(owner) = owners.get(&path).or_else(|| {
            identity.and_then(|id| owners.values().find(|owner| owner.identity == Some(id)))
        }) {
            if let Some(shared) = owner.shared.upgrade() {
                if shared.restoring.load(Ordering::Acquire) {
                    return Err(api::invalid("replica ownership", "restore is incomplete"));
                }
                match &shared.writer {
                    Writer::Client(backend) => match validate(&backend.client, expected, until) {
                        Ok(_) => {
                            backend.configure(expected)?;
                            return Ok(Some(Self { shared }));
                        }
                        Err(
                            Refusal::Unavailable(_) | Refusal::Timeout | Refusal::UnknownOutcome,
                        ) => (),
                        Err(error) => return Err(error),
                    },
                    Writer::Replicated {
                        log_path,
                        config: current,
                        ..
                    } if log_path == &log && current == &config => {
                        return Ok(Some(Self { shared }));
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
        if let Some(lease) = try_exclusive_file(&path)? {
            drop(lease);
            return Ok(None);
        }
        api::bound("open database owners", owners.len().saturating_add(1), 4096)?;
        let database = attach(&path, expected, until)?;
        owners.insert(
            path,
            Owner {
                shared: Arc::downgrade(&database.shared),
                lease: Weak::new(),
                identity,
            },
        );
        Ok(Some(database))
    }

    pub(super) fn resolve(&self, until: Instant) -> Result<Database, Refusal> {
        match &self.shared.writer {
            Writer::Client(backend) => backend.resolve(until),
            _ => Ok(self.clone()),
        }
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
        let replica = match &self.shared.writer {
            Writer::Replicated {
                log_path, config, ..
            } => Some((
                log_path
                    .to_str()
                    .ok_or_else(|| api::invalid("replica path", "not UTF-8"))?
                    .to_owned(),
                config.clone(),
            )),
            Writer::Local(_) => None,
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
        })
    }
    pub(super) fn remote_client(&self) -> Option<&crate::transport::Client> {
        match &self.shared.writer {
            Writer::Client(backend) => Some(&backend.client),
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
    }
}
