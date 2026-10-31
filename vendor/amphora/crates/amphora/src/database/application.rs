//! Retained application writer boundary on the existing standalone owner (U05).
//! The database lease excludes another engine, while this floor also fences old
//! attached application writers at the actual serialized transaction boundary.

use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Application {
    pub(crate) name: String,
    pub(crate) version: u32,
}
impl Application {
    pub(crate) fn validate(&self) -> Result<(), Refusal> {
        api::family(&self.name)?;
        api::bound("application identity", self.name.len(), 128)?;
        if self.version == 0 {
            return Err(api::invalid("application contract", "zero version"));
        }
        Ok(())
    }
}

/// An actual successful floor barrier, not deserialized metadata or an attachment
/// claiming it holds the original owner. Keeping this value retains that owner,
/// its engine and its original lease through the caller's maintenance lifetime.
pub struct WriterBoundary {
    database: Database,
    floor: Application,
}
impl WriterBoundary {
    /// Identity only. The same owner's writer rechecks the persisted floor inside
    /// every application transaction, including queued pre-floor requests.
    pub fn require(&self, database: &Database) -> Result<(), Refusal> {
        if !Arc::ptr_eq(&self.database.shared, &database.shared)
            || database.application.as_ref() != self.database.application.as_ref()
        {
            return Err(api::invalid("writer boundary", "different original owner or application"));
        }
        Ok(())
    }
    pub fn version(&self) -> u32 { self.floor.version }
}

impl Database {
    /// Identity comparison only; this does not establish a writer floor.
    pub fn require_same_application(&self, other:&Self)->Result<(),Refusal> {
        if !Arc::ptr_eq(&self.shared,&other.shared) || self.application!=other.application {
            return Err(api::invalid("application database identity","different retained database or application"));
        }
        Ok(())
    }
    /// Bind a handle and all namespaces it creates to this application's format.
    /// No floor is installed and no authority is created by this declaration.
    pub fn for_application(&self, name: &str, version: u32) -> Result<Self, Refusal> {
        let application = Application { name: name.into(), version };
        application.validate()?;
        let mut selected = self.clone();
        selected.application = Some(application);
        Ok(selected)
    }

    /// Install/observe the monotonic contract in the SAME original local writer.
    /// Return only after its normal durable callback. A failed/unknown callback
    /// yields no boundary. Replication and remote-owner takeover need their own
    /// application-floor proof and are deliberately not certified by this API.
    pub fn writer_floor(&self, name: &str, required: u32, until: Instant) -> Result<WriterBoundary, Refusal> {
        if !matches!(&self.shared.writer, Writer::Local(_))
            || self.shared.restoring.load(Ordering::Acquire)
        {
            return Err(api::invalid("writer boundary", "original standalone non-restoring owner required"));
        }
        let database = self.clone();
        let application = database.application.clone()
            .ok_or_else(|| api::invalid("writer boundary", "application-bound handle required"))?;
        application.validate()?;
        if application.name != name || required == 0 || required > application.version {
            return Err(api::invalid("writer boundary", "requested contract exceeds this application"));
        }
        let wanted = Application { name: name.into(), version: required };
        let floor = database.call_db(until, move |db| {
            let current = policy(db)?;
            if let Some(current) = &current {
                anyhow::ensure!(current.name == wanted.name, "another application's writer floor is installed");
            }
            let version = current.as_ref().map_or(wanted.version, |current| current.version.max(wanted.version));
            db.query("INSERT INTO __amphora_application_floor(singleton, application, version) VALUES(1, ?1, ?2) ON CONFLICT(singleton) DO UPDATE SET version=excluded.version",
                [text(&wanted.name), EValue::from_i64(i64::from(version))], |_| Ok(true))?;
            Ok(Application { name: wanted.name, version })
        })?;
        Ok(WriterBoundary { database, floor })
    }
}

fn policy(db: &Db) -> anyhow::Result<Option<Application>> {
    let mut value = None;
    db.query("SELECT application, version FROM __amphora_application_floor WHERE singleton=1", [], |row| {
        anyhow::ensure!(value.is_none(), "duplicate application writer floor");
        value = Some(Application {
            name: row.get::<String>(0).map_err(unavailable)?,
            version: u32::try_from(row.get::<i64>(1).map_err(unavailable)?)?,
        });
        Ok(true)
    })?;
    if let Some(value) = &value { value.validate()?; }
    Ok(value)
}

pub(super) fn check(db: &Db, claim: Option<&Application>, operation: &Operation) -> anyhow::Result<()> {
    // The owner's initial durability barrier has no SQL or product effect and
    // must run before open returns an application-bindable handle.
    if matches!(operation, Operation::Mutation(Mutation::Sql(statements)) if statements.is_empty()) {
        return Ok(());
    }
    let Some(floor) = policy(db)? else { return Ok(()) };
    let claim = claim.ok_or_else(|| Refusal::Fenced("application writer contract required".into()))?;
    claim.validate()?;
    if claim.name != floor.name || claim.version < floor.version {
        return Err(Refusal::Fenced("application writer contract is older than the installed floor".into()).into());
    }
    Ok(())
}

// No member-proven application claim is carried by resolved Raft pages yet.
// Never turn a standalone floor into permission for untracked replicated writes.
pub(super) fn require_replication(db: &Db) -> anyhow::Result<()> {
    let mut present = false;
    db.query("SELECT 1 FROM sqlite_schema WHERE type='table' AND name='__amphora_application_floor'", [], |_| { present = true; Ok(false) })?;
    if present && policy(db)?.is_some() {
        return Err(Refusal::Fenced("application writer floor requires a member-proven replication contract".into()).into());
    }
    Ok(())
}
