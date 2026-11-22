//! U05, CP-T1.8 (replication.md, "The application floor, replicated", "The seam"): kiln's own
//! callers of the floor, compiled and run against Amphora. The bodies below are kiln's, copied
//! from unify/takeover's crates/kiln/src/store.rs (StoreWriterBoundary, require_same_database,
//! writer_boundary); only Store, its migrations and its deadline are stand-ins. kiln drops its
//! vendored patches 0001/0002 once these hold.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use amphora::database::Database;

mod migrations {
    pub const SCHEMA_EPOCH: u32 = 7;
    pub fn require_installed(
        _conn: &mut amphora::sql::Connection<'_>,
        _required: u32,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(30)
}
struct Inner {
    database: Database,
}
#[derive(Clone)]
struct Store {
    inner: Arc<Inner>,
}

// kiln: crates/kiln/src/store.rs, as on unify/takeover.
pub(crate) struct StoreWriterBoundary {
    store: Store,
    boundary: amphora::database::WriterBoundary,
}
#[derive(Debug, thiserror::Error)]
#[error(
    "conversation maintenance requires the original Store writer, standalone or the group's replica; this owner mode is unsupported"
)]
pub(crate) struct UnsupportedStoreWriterBoundary;
impl StoreWriterBoundary {
    pub(crate) fn require_store(&self, store: &Store) -> anyhow::Result<()> {
        self.boundary.require(&self.store.inner.database)?;
        self.boundary.require(&store.inner.database)?;
        Ok(())
    }
    pub(crate) fn version(&self) -> u32 {
        self.boundary.version()
    }
}
impl Store {
    fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        let database = Database::open(path, deadline())?;
        let database = database.for_application("kiln.host-store", migrations::SCHEMA_EPOCH)?;
        Ok(Self {
            inner: Arc::new(Inner { database }),
        })
    }
    pub(crate) fn require_same_database(&self, other: &Store) -> anyhow::Result<()> {
        self.inner
            .database
            .require_same_application(&other.inner.database)?;
        Ok(())
    }
    pub(crate) fn writer_boundary(&self, required: u32) -> anyhow::Result<StoreWriterBoundary> {
        anyhow::ensure!(
            required <= migrations::SCHEMA_EPOCH,
            "this Store does not implement the required writer contract"
        );
        let boundary =
            match self
                .inner
                .database
                .writer_floor("kiln.host-store", required, deadline())
            {
                Ok(boundary) => boundary,
                Err(amphora_api::Refusal::Invalid { what, why })
                    if what == "writer boundary"
                        && why == "original non-restoring owner required" =>
                {
                    return Err(UnsupportedStoreWriterBoundary.into());
                }
                Err(error) => return Err(error.into()),
            };
        self.inner.database.call(deadline(), move |conn| {
            migrations::require_installed(conn, required)
        })?;
        Ok(StoreWriterBoundary {
            store: self.clone(),
            boundary,
        })
    }
}

/// U05. Do: kiln's Store opens a standalone database bound to kiln.host-store at its schema
/// epoch, takes its writer boundary at that epoch, and compares itself with a clone. Expect:
/// kiln's code compiles unchanged; the boundary holds the epoch and binds both stores; the
/// clone is the same database.
#[test]
fn kilns_own_floor_callers_compile_and_hold_on_a_standalone_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("kiln.db")).unwrap();
    let boundary = store.writer_boundary(migrations::SCHEMA_EPOCH).unwrap();
    assert_eq!(boundary.version(), migrations::SCHEMA_EPOCH);
    let clone = store.clone();
    boundary.require_store(&clone).unwrap();
    store.require_same_database(&clone).unwrap();
}
