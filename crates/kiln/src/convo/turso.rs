//! The store's engine: Turso, embedded (`docs/conversations.md`, "The store").

use std::path::Path;

use super::rows::{EventRow, Row};
use super::store::{Engine, Kept, Query};

/// A Turso database file.
pub struct Turso {
    _private: (),
}

impl Turso {
    /// The database at `path`, made and migrated as needed.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        anyhow::bail!("the Turso engine isn't built yet ({})", path.display())
    }
}

impl Engine for Turso {
    fn apply(&mut self, _rows: &[Row]) -> anyhow::Result<()> {
        anyhow::bail!("not built yet")
    }
    fn kept(&mut self, _session: &str, _last: usize) -> anyhow::Result<Option<Kept>> {
        anyhow::bail!("not built yet")
    }
    fn events(&mut self, _session: &str, _before: Option<u64>, _limit: usize) -> anyhow::Result<Vec<EventRow>> {
        anyhow::bail!("not built yet")
    }
    fn search(&mut self, _q: &Query) -> anyhow::Result<Vec<EventRow>> {
        anyhow::bail!("not built yet")
    }
}
