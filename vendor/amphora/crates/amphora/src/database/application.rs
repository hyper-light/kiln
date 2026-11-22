//! Retained application writer boundary on the existing standalone owner (U05; replication.md,
//! "The application floor, replicated", "Today"). The database lease excludes another engine,
//! while this floor also fences old attached application writers at the actual serialized
//! transaction boundary.

use super::*;
use crate::meter::FloorRefusal;
use serde::{Deserialize, Serialize};

// Created by the first raise, inside its own session, never with the product schema: a replica
// opens that schema locally, and a member on a binary from before the floor would not create this
// table, so its pages would differ (replication.md, "The upgrade that introduces the join rows").
const FLOOR: &str = "CREATE TABLE IF NOT EXISTS __amphora_application_floor (singleton INTEGER PRIMARY KEY CHECK(singleton=1), application TEXT NOT NULL, version INTEGER NOT NULL CHECK(version>0))";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Application {
    pub(crate) name: String,
    pub(crate) version: u32,
}
impl Application {
    pub(crate) fn validate(&self) -> Result<(), Refusal> {
        // A family-shaped identifier, so its grammar bounds it: at most `api::FAMILY_MAX` bytes.
        api::family(&self.name)?;
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
            return Err(api::invalid(
                "writer boundary",
                "different original owner or application",
            ));
        }
        Ok(())
    }
    pub fn version(&self) -> u32 {
        self.floor.version
    }
}

impl Database {
    /// Identity comparison only; this does not establish a writer floor.
    pub fn require_same_application(&self, other: &Self) -> Result<(), Refusal> {
        if !Arc::ptr_eq(&self.shared, &other.shared) || self.application != other.application {
            return Err(api::invalid(
                "application database identity",
                "different retained database or application",
            ));
        }
        Ok(())
    }
    /// Bind a handle and all namespaces it creates to this application's format.
    /// No floor is installed and no authority is created by this declaration.
    pub fn for_application(&self, name: &str, version: u32) -> Result<Self, Refusal> {
        let application = Application {
            name: name.into(),
            version,
        };
        application.validate()?;
        // A replica's owner keeps this member's attestation of it (replication.md, "Attestation").
        if let Writer::Replicated { client, .. } = &self.shared.writer {
            client.handle().attest(application.clone())?;
        }
        let mut selected = self.clone();
        selected.application = Some(application);
        Ok(selected)
    }

    /// Install/observe the monotonic contract in the SAME original owner: its local writer, or
    /// its replica, where the raise is proven in its own session by every member of the
    /// configuration applied at its base (replication.md, "Raising the floor"). Return only after
    /// its normal durable callback. A failed/unknown callback yields no boundary. Remote-owner
    /// takeover needs its own application-floor proof and is refused here.
    pub fn writer_floor(
        &self,
        name: &str,
        required: u32,
        until: Instant,
    ) -> Result<WriterBoundary, Refusal> {
        let replicated = matches!(&self.shared.writer, Writer::Replicated { .. });
        if !(matches!(&self.shared.writer, Writer::Local(_)) || replicated)
            || self.shared.restoring.load(Ordering::Acquire)
        {
            return Err(api::invalid(
                "writer boundary",
                "original non-restoring owner required",
            ));
        }
        let database = self.clone();
        let application = database
            .application
            .clone()
            .ok_or_else(|| api::invalid("writer boundary", "application-bound handle required"))?;
        application.validate()?;
        if application.name != name || required == 0 || required > application.version {
            return Err(api::invalid(
                "writer boundary",
                "requested contract exceeds this application",
            ));
        }
        let wanted = Application {
            name: name.into(),
            version: required,
        };
        let outcome = database
            .call_db(until, move |db| raise(db, wanted, replicated))
            .map_err(|refusal| {
                let Writer::Replicated { client, .. } = &self.shared.writer else {
                    return refusal;
                };
                let stale = matches!(&refusal, Refusal::Invalid { what, .. } if what == "prepared transaction");
                let refusal = overtaken(refusal);
                if stale {
                    client
                        .handle()
                        .floor_refused(FloorRefusal::StaleBase, &refusal);
                }
                refusal
            })?;
        match outcome {
            Ok(floor) => Ok(WriterBoundary { database, floor }),
            Err(refused) => {
                if let Writer::Replicated { client, .. } = &self.shared.writer {
                    client
                        .handle()
                        .floor_refused(refused.reason, &refused.refusal);
                }
                Err(refused.refusal)
            }
        }
    }

    /// The application floor and each member of the group against it (replication.md, "The
    /// seam"), read without a claim, so an application below the floor can see it and stand
    /// down. A replica answers what its owner published at its last applied index, without
    /// waiting; a standalone database reads its floor on a reader by `until`, and names no
    /// members, being no group.
    pub fn floor_status(
        &self,
        until: Instant,
    ) -> Result<amphora_api::replication::FloorStatus, Refusal> {
        let store = match &self.shared.writer {
            Writer::Replicated { client, .. } => return client.handle().floor_status(),
            Writer::Local(store) => store,
            Writer::Client(_) => {
                return Err(api::invalid("floor status", "the owner's database"));
            }
        };
        let read = |db: &mut Db| -> anyhow::Result<amphora_api::replication::FloorStatus> {
            let floor = policy(db)?;
            Ok(amphora_api::replication::FloorStatus {
                application: floor.as_ref().map(|floor| floor.name.clone()),
                version: floor.map_or(0, |floor| floor.version),
                members: Vec::new(),
            })
        };
        store
            .read_committed(
                until,
                |reader| read(reader),
                move |engine| read(&mut engine.db),
            )
            .map_err(|e| refused(&e))
    }
}

/// A request the floor refused, and why: counted under its reason where it is refused, then
/// answered as its refusal.
#[derive(Debug)]
pub(crate) struct FloorRefused {
    pub(crate) reason: FloorRefusal,
    pub(crate) refusal: Refusal,
}

impl FloorRefused {
    fn new(reason: FloorRefusal, refusal: Refusal) -> Self {
        Self { reason, refusal }
    }
}

/// The raise's session: the floor becomes `wanted`'s version, or stays above it, for `wanted`'s
/// application; on a replica only on the members' proof (replication.md, "Raising the floor"). A
/// refusal is its value, typed, so its caller counts it; it writes nothing.
pub(crate) fn raise(
    db: &mut Db,
    wanted: Application,
    replicated: bool,
) -> anyhow::Result<Result<Application, FloorRefused>> {
    let current = policy(db)?;
    if let Some(current) = &current
        && current.name != wanted.name
    {
        return Ok(Err(FloorRefused::new(
            FloorRefusal::OtherApplication,
            Refusal::Fenced("another application's writer floor is installed".into()),
        )));
    }
    // The floor never falls: a raise to it or below answers the floor in force at once, with
    // nothing to prove and nothing written (a caller asking for an older contract is served by
    // the newer one in force, as kiln's writer_boundary asks).
    if let Some(current) = current
        && current.version >= wanted.version
    {
        return Ok(Ok(current));
    }
    let version = wanted.version;
    if replicated && let Some(refused) = prove(db, &wanted.name, version)? {
        return Ok(Err(refused));
    }
    db.exec(FLOOR)?;
    db.query(
        "INSERT INTO __amphora_application_floor(singleton, application, version) \
             VALUES(1, ?1, ?2) ON CONFLICT(singleton) DO UPDATE SET version=excluded.version",
        [text(&wanted.name), EValue::from_i64(i64::from(version))],
        |_| Ok(true),
    )?;
    Ok(Ok(Application {
        name: wanted.name,
        version,
    }))
}

/// What a replicated raise answers when the stale-base rule refuses it: the group changed
/// between its proof and its index, so it is to be asked again, `Unavailable`, the class for
/// "retry later" (amphora-seam-owner's ruling on step 4, a). Every other answer is its own: a
/// displaced entry stays UnknownOutcome.
pub(super) fn overtaken(refusal: Refusal) -> Refusal {
    match refusal {
        Refusal::Invalid { what, why }
            if what == "prepared transaction"
                && why == "the database advanced since preparation" =>
        {
            Refusal::Unavailable("the database advanced since preparation: the group changed while the raise was proven; raise again".into())
        }
        other => other,
    }
}

/// The members' proof of a raise to `version` on a replica, read at this session's base; the
/// stale-base rule makes it hold where it applies (replication.md, "Raising the floor").
fn prove(db: &Db, application: &str, version: u32) -> anyhow::Result<Option<FloorRefused>> {
    use crate::replication::floor;
    if floor::tracked(db)?.is_none() {
        return Ok(Some(FloorRefused::new(
            FloorRefusal::Unproven,
            Refusal::Unavailable("joins aren't tracked yet; every member must attest first".into()),
        )));
    }
    if !floor::configuration(db)?.voters_outgoing.is_empty() {
        return Ok(Some(FloorRefused::new(
            FloorRefusal::Busy,
            Refusal::Unavailable(
                "the group is leaving a joint configuration; raise the floor once it has".into(),
            ),
        )));
    }
    let short = floor::short_of(db, application, version)?;
    if !short.is_empty() {
        return Ok(Some(FloorRefused::new(
            FloorRefusal::Unproven,
            Refusal::Unavailable(format!(
                "members {short:?} have not attested {application} at version {version} or above"
            )),
        )));
    }
    Ok(None)
}

/// The floor in force at this session's base, if one is installed.
pub(super) fn policy(db: &Db) -> anyhow::Result<Option<Application>> {
    let mut present = false;
    db.query(
        "SELECT 1 FROM sqlite_schema WHERE type='table' AND name='__amphora_application_floor'",
        [],
        |_| {
            present = true;
            Ok(false)
        },
    )?;
    if !present {
        return Ok(None);
    }
    let mut value = None;
    db.query(
        "SELECT application, version FROM __amphora_application_floor WHERE singleton=1",
        [],
        |row| {
            anyhow::ensure!(value.is_none(), "duplicate application writer floor");
            value = Some(Application {
                name: row.get::<String>(0).map_err(unavailable)?,
                version: u32::try_from(row.get::<i64>(1).map_err(unavailable)?)?,
            });
            Ok(true)
        },
    )?;
    if let Some(value) = &value {
        value.validate()?;
    }
    Ok(value)
}

pub(super) fn check(
    db: &Db,
    claim: Option<&Application>,
    operation: &Operation,
) -> anyhow::Result<()> {
    match claim_refused(db, claim, operation)? {
        Some(refused) => Err(refused.refusal.into()),
        None => Ok(()),
    }
}

/// Whether `claim` holds against the floor installed in `db`: reads and writes alike, in every
/// mode, so a handle's answer doesn't depend on where its database runs (CLAUDE.md section 8).
pub(super) fn claim_holds(db: &Db, claim: Option<&Application>) -> anyhow::Result<()> {
    match claim_refusal(db, claim)? {
        Some(refused) => Err(refused.refusal.into()),
        None => Ok(()),
    }
}

/// The floor's refusal of `operation` from a handle claiming `claim`, typed, if it refuses it.
pub(super) fn claim_refused(
    db: &Db,
    claim: Option<&Application>,
    operation: &Operation,
) -> anyhow::Result<Option<FloorRefused>> {
    // The owner's initial durability barrier has no SQL or product effect and
    // must run before open returns an application-bindable handle.
    if matches!(operation, Operation::Mutation(Mutation::Sql(statements)) if statements.is_empty())
    {
        return Ok(None);
    }
    claim_refusal(db, claim)
}

fn claim_refusal(db: &Db, claim: Option<&Application>) -> anyhow::Result<Option<FloorRefused>> {
    let Some(floor) = policy(db)? else {
        return Ok(None);
    };
    let Some(claim) = claim else {
        return Ok(Some(FloorRefused::new(
            FloorRefusal::NoClaim,
            Refusal::Fenced("application writer contract required".into()),
        )));
    };
    claim.validate()?;
    if claim.name != floor.name {
        return Ok(Some(FloorRefused::new(
            FloorRefusal::OtherApplication,
            Refusal::Fenced(
                "application writer contract is for another application than the installed floor"
                    .into(),
            ),
        )));
    }
    if claim.version < floor.version {
        return Ok(Some(FloorRefused::new(
            FloorRefusal::BelowFloor,
            Refusal::Fenced("application writer contract is older than the installed floor".into()),
        )));
    }
    Ok(None)
}
