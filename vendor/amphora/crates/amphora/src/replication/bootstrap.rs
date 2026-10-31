//! Bounded full-image adoption before any resolved page delta (U15 / CP-T1.8).
use crate::db::Db;
use amphora_api::{self as api, Refusal, replication::Bootstrap};
use amphora_engine::Value;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const TABLE: &str = "CREATE TABLE IF NOT EXISTS __amphora_baseline (id INTEGER PRIMARY KEY CHECK(id = 1), source TEXT NOT NULL, identity BLOB NOT NULL, complete INTEGER NOT NULL, received TEXT NOT NULL, total TEXT NOT NULL, image BLOB NOT NULL)";
const PREFIX: &[u8] = b"amphora.seed.v1\0";
// Existing snapshot encoding permits this much framing beyond the page-image budget.
const IMAGE_BYTES: usize = super::SNAPSHOT_BYTES.saturating_add(1 << 20);
const FRAMING: usize = 1024;

/// Inspect ownership before a startup importer opens the local writer. A pending baseline also
/// counts: a crash can occur after retaining the source but before the replica table is created.
/// Callers must still let the local open recheck under its own lease before writing.
pub fn is_replica(path: &std::path::Path) -> Result<bool, Refusal> {
    if !path.try_exists().map_err(unavailable)? {
        return Ok(false);
    }
    let owner = std::sync::Arc::new(crate::database::exclusive_file(path)?);
    let mut db = Db::open_readonly(path).map_err(unavailable)?;
    db.hold_owner(owner);
    let mut marked = false;
    db.query("SELECT 1 FROM sqlite_schema WHERE name IN ('__amphora_replica', '__amphora_baseline') LIMIT 1", [], |_| { marked = true; Ok(false) }).map_err(unavailable)?;
    Ok(marked)
}

/// Inspect the explicitly chosen source under the product lease. Persist the returned declaration
/// for all initial members; this function never selects a source by connection or election order.
pub fn inspect_source(path: &std::path::Path, source_node: u64) -> Result<Bootstrap, Refusal> {
    if source_node == 0 {
        return Err(api::invalid("baseline source", "node ID must be positive"));
    }
    let owner = std::sync::Arc::new(crate::database::exclusive_file(path)?);
    let mut db = Db::open_readonly(path).map_err(unavailable)?;
    db.hold_owner(owner);
    if table_exists(&db)? {
        let mut saved = None;
        db.query(
            "SELECT source, identity FROM __amphora_baseline WHERE id = 1",
            [],
            |row| {
                let source_node = row.get::<String>(0)?.parse::<u64>()?;
                let source_id: [u8; 32] = row
                    .get_value(1)
                    .to_blob()
                    .ok_or_else(|| anyhow::anyhow!("baseline identity is not a blob"))?
                    .try_into()?;
                saved = Some(Bootstrap {
                    source_node,
                    source_id,
                });
                Ok(false)
            },
        )
        .map_err(unavailable)?;
        let declaration = saved.ok_or_else(|| conflict("durable declaration is missing"))?;
        if declaration.source_node != source_node {
            return Err(conflict("inspection requested a different source node"));
        }
        return Ok(declaration);
    }
    let mut replica = false;
    db.query(
        "SELECT 1 FROM sqlite_schema WHERE name = '__amphora_replica' LIMIT 1",
        [],
        |_| {
            replica = true;
            Ok(false)
        },
    )
    .map_err(unavailable)?;
    if replica {
        return Err(conflict(
            "an existing replica cannot be declared as a new source",
        ));
    }
    let image = capture(&mut db)?;
    Ok(Bootstrap {
        source_node,
        source_id: identity(&image),
    })
}

#[derive(Debug)]
pub(super) struct Baseline {
    declaration: Option<Bootstrap>,
    source: bool,
    complete: bool,
    received: usize,
    total: usize,
    // Source: original complete image. Follower: exactly the committed prefix.
    image: Vec<u8>,
}
#[derive(Serialize, Deserialize)]
struct Chunk {
    declaration: Bootstrap,
    offset: u64,
    total: u64,
    image: Vec<u8>,
}
fn unavailable(error: impl std::fmt::Display) -> Refusal {
    Refusal::Unavailable(error.to_string())
}
fn conflict(why: &str) -> Refusal {
    Refusal::Fenced(format!("baseline import: {why}"))
}
fn table_exists(db: &Db) -> Result<bool, Refusal> {
    let mut found = false;
    db.query(
        "SELECT name FROM sqlite_schema WHERE name = '__amphora_baseline'",
        [],
        |_| {
            found = true;
            Ok(false)
        },
    )
    .map_err(unavailable)?;
    Ok(found)
}
fn nonempty(db: &Db) -> Result<bool, Refusal> {
    let mut found = false;
    db.query(
        "SELECT name FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' LIMIT 1",
        [],
        |_| {
            found = true;
            Ok(false)
        },
    )
    .map_err(unavailable)?;
    Ok(found)
}
fn identity(image: &[u8]) -> [u8; 32] {
    Sha256::digest(image).into()
}
fn capture(db: &mut Db) -> Result<Vec<u8>, Refusal> {
    let pages = db.image_pages(super::SNAPSHOT_BYTES).map_err(unavailable)?;
    if pages.is_empty() {
        return Err(api::invalid(
            "baseline source",
            "source has no database image; fresh empty groups use no bootstrap declaration",
        ));
    }
    let image = postcard::to_allocvec(&pages).map_err(unavailable)?;
    api::bound("baseline image", image.len(), IMAGE_BYTES)?;
    Ok(image)
}
impl Baseline {
    /// Runs before bootstrap_db or replica metadata modifies the original source.
    pub(super) fn open(
        db: &mut Db,
        node: u64,
        declaration: Option<Bootstrap>,
        marked: bool,
    ) -> Result<Self, Refusal> {
        if declaration.as_ref().is_some_and(|d| d.source_node == 0) {
            return Err(api::invalid("baseline source", "node ID must be positive"));
        }
        if table_exists(db)? {
            let mut saved = None;
            db.query("SELECT source, identity, complete, received, total, image FROM __amphora_baseline WHERE id = 1", [], |row| {
                let source_node = row.get::<String>(0)?.parse::<u64>()?;
                let source_id: [u8; 32] = row.get_value(1).to_blob().ok_or_else(|| anyhow::anyhow!("baseline identity is not a blob"))?.try_into()?;
                let complete = match row.get::<i64>(2)? { 0 => false, 1 => true, _ => anyhow::bail!("invalid baseline completion flag") };
                let received = row.get::<String>(3)?.parse::<usize>()?;
                let total = row.get::<String>(4)?.parse::<usize>()?;
                api::bound("baseline image", total, IMAGE_BYTES)?;
                let image = row.get_value(5).to_blob().ok_or_else(|| anyhow::anyhow!("baseline image is not a blob"))?;
                api::bound("retained baseline image", image.len(), IMAGE_BYTES)?;
                saved = Some(Self { declaration: Some(Bootstrap { source_node, source_id }), source: node == source_node, complete, received, total, image: image.to_vec() });
                Ok(false)
            }).map_err(unavailable)?;
            let saved = saved.ok_or_else(|| conflict("durable declaration is missing"))?;
            if saved.declaration != declaration {
                return Err(conflict(
                    "configured source differs from durable declaration",
                ));
            }
            if saved.received > saved.total
                || (saved.complete && (saved.received != saved.total || !saved.image.is_empty()))
                || (!saved.complete
                    && saved.image.len()
                        != if saved.source {
                            saved.total
                        } else {
                            saved.received
                        })
            {
                return Err(conflict("durable seed cursor or image length differs"));
            }
            if !saved.complete
                && saved.source
                && saved
                    .declaration
                    .as_ref()
                    .is_none_or(|d| identity(&saved.image) != d.source_id)
            {
                return Err(conflict("retained source image identity differs"));
            }
            return Ok(saved);
        }
        if marked && declaration.is_some() {
            return Err(conflict(
                "existing replica has no matching baseline declaration",
            ));
        }
        let has_data = nonempty(db)?;
        let Some(ref declared) = declaration else {
            if !marked && has_data {
                return Err(conflict(
                    "nonempty local database requires an explicit source declaration",
                ));
            }
            return Ok(Self {
                declaration: None,
                source: false,
                complete: true,
                received: 0,
                total: 0,
                image: Vec::new(),
            });
        };
        let source = node == declared.source_node;
        let image = if source {
            let image = capture(db)?;
            if identity(&image) != declared.source_id {
                return Err(conflict(
                    "source database identity differs from declaration",
                ));
            }
            image
        } else {
            if has_data {
                return Err(conflict("nonempty follower cannot adopt another source"));
            }
            Vec::new()
        };
        let baseline = Self {
            declaration,
            source,
            complete: false,
            received: 0,
            total: image.len(),
            image,
        };
        db.transaction(|db| baseline.marker(db))
            .map_err(unavailable)?;
        db.flusher()().map_err(unavailable)?;
        Ok(baseline)
    }
    pub(super) fn declaration(&self) -> Option<&Bootstrap> {
        self.declaration.as_ref()
    }
    pub(super) fn completed_marker(&self, db: &Db) -> anyhow::Result<()> {
        self.write_marker(db, true, self.total, self.total, &[])
    }
    pub(super) fn completed(&mut self) {
        self.complete = true;
        self.received = self.total;
        self.image.clear();
    }
    pub(super) fn pending(&self) -> bool {
        !self.complete
    }
    pub(super) fn source(&self) -> Option<u64> {
        if self.complete {
            None
        } else {
            self.declaration.as_ref().map(|d| d.source_node)
        }
    }
    /// Caller subtracts GroupStore framing and proposal context from actual log.entry_room().
    pub(super) fn proposal(&self, entry_room: usize) -> Result<Option<Vec<u8>>, Refusal> {
        if self.complete {
            return Ok(None);
        }
        if !self.source {
            return Err(Refusal::NotLeader {
                leader: self.source(),
            });
        }
        let declaration = self
            .declaration
            .as_ref()
            .ok_or_else(|| conflict("pending declaration is missing"))?;
        let limit = entry_room.min(super::COMMAND_BYTES);
        let chunk_bytes = limit
            .checked_sub(FRAMING)
            .filter(|n| *n > 0)
            .ok_or_else(|| {
                api::invalid(
                    "baseline entry room",
                    "no room for image bytes after framing",
                )
            })?;
        let end = self
            .received
            .checked_add(chunk_bytes)
            .ok_or_else(|| conflict("seed offset overflow"))?
            .min(self.total);
        let image = self
            .image
            .get(self.received..end)
            .filter(|image| !image.is_empty())
            .ok_or_else(|| conflict("source image or cursor is missing"))?
            .to_vec();
        let chunk = Chunk {
            declaration: declaration.clone(),
            offset: u64::try_from(self.received).map_err(unavailable)?,
            total: u64::try_from(self.total).map_err(unavailable)?,
            image,
        };
        let mut bytes = PREFIX.to_vec();
        bytes.extend(postcard::to_allocvec(&chunk).map_err(unavailable)?);
        api::bound("baseline log entry", bytes.len(), limit)?;
        Ok(Some(bytes))
    }
    /// Handles only seed records. Each prefix, cursor and applied point share a WAL commit. The
    /// final prefix installs the complete checked image and completion marker atomically.
    /// `finish` recreates product schemas and writes the actual replica applied point.
    pub(super) fn apply_seed(
        &mut self,
        db: &mut Db,
        bytes: &[u8],
        finish: impl FnOnce(&mut Db) -> anyhow::Result<()>,
    ) -> Result<bool, Refusal> {
        let Some(payload) = bytes.strip_prefix(PREFIX) else {
            return Ok(false);
        };
        api::bound("baseline command", bytes.len(), super::COMMAND_BYTES)?;
        let chunk: Chunk = postcard::from_bytes(payload).map_err(unavailable)?;
        if self.declaration.as_ref() != Some(&chunk.declaration) {
            return Err(conflict("committed seed differs from durable declaration"));
        }
        let offset = usize::try_from(chunk.offset).map_err(unavailable)?;
        let total = usize::try_from(chunk.total).map_err(unavailable)?;
        api::bound("baseline image", total, IMAGE_BYTES)?;
        let end = offset
            .checked_add(chunk.image.len())
            .ok_or_else(|| conflict("chunk offset overflow"))?;
        if chunk.image.is_empty() || end > total || total == 0 {
            return Err(conflict("invalid seed chunk bounds"));
        }
        // A retry after completion must never overwrite later committed deltas.
        if self.complete {
            db.apply_pages(&[], finish).map_err(unavailable)?;
            db.flusher()().map_err(unavailable)?;
            return Ok(true);
        }
        if self.total != 0 && self.total != total {
            return Err(conflict("seed total changed"));
        }
        if offset < self.received {
            if end > self.received || self.image.get(offset..end) != Some(chunk.image.as_slice()) {
                return Err(conflict("conflicting repeated seed prefix"));
            }
            db.apply_pages(&[], finish).map_err(unavailable)?;
            db.flusher()().map_err(unavailable)?;
            return Ok(true);
        }
        if offset != self.received {
            return Err(conflict("seed chunk does not follow committed prefix"));
        }
        let follower_image = if self.source {
            if self.image.get(offset..end) != Some(chunk.image.as_slice()) {
                return Err(conflict("seed chunk differs from retained source"));
            }
            None
        } else {
            let mut image = Vec::with_capacity(end);
            image.extend_from_slice(&self.image);
            image.extend_from_slice(&chunk.image);
            Some(image)
        };
        let image = follower_image.as_deref().unwrap_or(&self.image);
        let complete = end == total;
        let pages = if complete {
            if identity(image) != chunk.declaration.source_id {
                return Err(conflict("completed image differs from declared identity"));
            }
            let pages: Vec<(u32, Vec<u8>)> = postcard::from_bytes(image).map_err(unavailable)?;
            super::validate_pages(&pages, super::SNAPSHOT_BYTES)?;
            let mut expected = 1u32;
            for (id, _) in &pages {
                if *id != expected {
                    return Err(conflict("image pages are not complete and contiguous"));
                }
                expected = expected
                    .checked_add(1)
                    .ok_or_else(|| conflict("page number overflow"))?;
            }
            if pages.is_empty() {
                return Err(conflict("image lacks a database header"));
            }
            pages
        } else {
            Vec::new()
        };
        db.apply_pages(&pages, |db| {
            finish(db)?;
            self.write_marker(db, complete, end, total, if complete { &[] } else { image })
        })
        .map_err(unavailable)?;
        db.flusher()().map_err(unavailable)?;
        self.received = end;
        self.total = total;
        self.complete = complete;
        if complete {
            self.image.clear();
        } else if let Some(image) = follower_image {
            self.image = image;
        }
        Ok(true)
    }
    pub(super) fn marker(&self, db: &Db) -> anyhow::Result<()> {
        self.write_marker(db, self.complete, self.received, self.total, &self.image)
    }
    fn write_marker(
        &self,
        db: &Db,
        complete: bool,
        received: usize,
        total: usize,
        image: &[u8],
    ) -> anyhow::Result<()> {
        let Some(declaration) = &self.declaration else {
            return Ok(());
        };
        db.exec(TABLE)?;
        db.query("INSERT INTO __amphora_baseline(id,source,identity,complete,received,total,image) VALUES(1,?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET source=excluded.source,identity=excluded.identity,complete=excluded.complete,received=excluded.received,total=excluded.total,image=excluded.image", [Value::from_text(declaration.source_node.to_string()), Value::from_slice(&declaration.source_id)?, Value::from_i64(i64::from(complete)), Value::from_text(received.to_string()), Value::from_text(total.to_string()), Value::from_slice(image)?], |_| Ok(true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source(db: &mut Db) -> Bootstrap {
        db.exec("CREATE TABLE retained(value TEXT); INSERT INTO retained VALUES('unchanged base')")
            .unwrap();
        Bootstrap {
            source_node: 1,
            source_id: identity(&capture(db).unwrap()),
        }
    }
    /// U15/CP-T1.8: pre-import inspection cannot create or alter the source database or WAL.
    #[test]
    fn source_inspection_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inspect.db");
        assert!(!is_replica(&path).unwrap());
        assert!(!path.exists());
        let mut db = Db::open(&path).unwrap();
        source(&mut db);
        drop(db);
        let wal = dir.path().join("inspect.db-wal");
        let before = std::fs::read(&path).unwrap();
        let before_wal = std::fs::read(&wal).ok();
        assert!(!is_replica(&path).unwrap());
        let declaration = inspect_source(&path, 1).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read(&wal).ok(), before_wal);
        let mut db = Db::open(&path).unwrap();
        Baseline::open(&mut db, 1, Some(declaration), false).unwrap();
        drop(db);
        let before = std::fs::read(&path).unwrap();
        let before_wal = std::fs::read(&wal).ok();
        assert!(is_replica(&path).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read(&wal).ok(), before_wal);
    }

    /// U15/CP-T1.8: explicit identity, nonempty follower refusal and deterministic restart retry.
    #[test]
    fn source_identity_and_restart_fences() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.db");
        let mut db = Db::open(&path).unwrap();
        let declaration = source(&mut db);
        assert!(matches!(
            Baseline::open(&mut db, 1, None, false),
            Err(Refusal::Fenced(_))
        ));
        assert!(matches!(
            Baseline::open(&mut db, 2, Some(declaration.clone()), false),
            Err(Refusal::Fenced(_))
        ));
        let baseline = Baseline::open(&mut db, 1, Some(declaration.clone()), false).unwrap();
        let proposal = baseline.proposal(4096).unwrap().unwrap();
        drop(db);
        let mut db = Db::open(&path).unwrap();
        let reopened = Baseline::open(&mut db, 1, Some(declaration), false).unwrap();
        assert_eq!(proposal, reopened.proposal(4096).unwrap().unwrap());
        assert!(matches!(
            Baseline::open(
                &mut db,
                1,
                Some(Bootstrap {
                    source_node: 1,
                    source_id: [0; 32]
                }),
                false
            ),
            Err(Refusal::Fenced(_))
        ));
    }
    /// U15/CP-T1.8: three identical complete bases, restart during prefix, duplicate seed after a delta.
    #[test]
    fn three_members_install_all_base_pages_before_completion() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("source.db")).unwrap();
        let declaration = source(&mut db);
        let mut source = Baseline::open(&mut db, 1, Some(declaration.clone()), false).unwrap();
        let mut followers = Vec::new();
        for node in [2, 3] {
            let path = dir.path().join(format!("{node}.db"));
            let mut db = Db::open(&path).unwrap();
            let state = Baseline::open(&mut db, node, Some(declaration.clone()), false).unwrap();
            followers.push((node, path, db, state));
        }
        let mut proposals = 0;
        let mut last = Vec::new();
        while source.pending() {
            assert!(proposals < 128);
            let bytes = source.proposal(4096).unwrap().unwrap();
            for (node, path, db, state) in &mut followers {
                state.apply_seed(db, &bytes, |_| Ok(())).unwrap();
                // Reopen the durable cursor rather than infer it from a lost acknowledgement.
                *state = Baseline::open(db, *node, Some(declaration.clone()), true).unwrap();
                assert_eq!(
                    state.pending(),
                    source.received.checked_add(3072).unwrap() < source.total
                );
                let _ = path;
            }
            source.apply_seed(&mut db, &bytes, |_| Ok(())).unwrap();
            last = bytes;
            proposals += 1;
        }
        assert!(proposals > 1);
        for (_, _, db, state) in &mut followers {
            let mut value = String::new();
            db.query("SELECT value FROM retained", [], |row| {
                value = row.get(0)?;
                Ok(false)
            })
            .unwrap();
            assert_eq!(value, "unchanged base");
            db.exec("UPDATE retained SET value='later delta'").unwrap();
            state.apply_seed(db, &last, |_| Ok(())).unwrap();
            db.query("SELECT value FROM retained", [], |row| {
                value = row.get(0)?;
                Ok(false)
            })
            .unwrap();
            assert_eq!(value, "later delta");
        }
    }
    /// U15/CP-T1.8: seed finalization recreates product metadata, gates SQL, fences images and
    /// deltas, and survives a restart between committed chunks on all three machines.
    #[test]
    fn machine_seed_gates_sql_and_snapshot_identity() {
        use hyper_durable::{EntryRef, StateMachine};
        use hyper_raft::proto::{ConfState, EntryType};
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("machine-1.db");
        let mut db = Db::open(&source_path).unwrap();
        source(&mut db);
        drop(db);
        let declaration = inspect_source(&source_path, 1).unwrap();
        let conf = ConfState {
            voters: vec![1, 2, 3],
            ..ConfState::default()
        };
        let open = |node| {
            let path = dir.path().join(format!("machine-{node}.db"));
            let owner = std::sync::Arc::new(crate::database::exclusive_file(&path).unwrap());
            super::super::Machine::open_with_owner(
                &path,
                conf.clone(),
                owner,
                node,
                Some(declaration.clone()),
            )
            .unwrap()
        };
        let mut machines: Vec<_> = (1..=3).map(open).collect();
        let mut index = 1;
        while machines[0].bootstrap_pending() {
            assert!(index < 128);
            let mut evaluated = false;
            assert!(
                machines[0]
                    .prepare(|_| {
                        evaluated = true;
                        Ok(())
                    })
                    .is_err()
            );
            assert!(!evaluated);
            let bytes = machines[0].prepare_bootstrap(4096).unwrap().unwrap();
            for machine in &mut machines {
                let mut answers = Vec::new();
                machine
                    .apply(
                        &EntryRef {
                            index,
                            term: 1,
                            kind: EntryType::EntryNormal,
                            context: b"seed",
                            data: &bytes,
                        },
                        &mut answers,
                    )
                    .unwrap();
                assert!(answers[0].result.is_ok());
            }
            if index == 1 {
                drop(machines);
                machines = (1..=3).map(open).collect();
                assert!(machines.iter().all(|m| m.bootstrap_pending()));
            }
            index += 1;
        }
        let (delta, _) = machines[0]
            .prepare(|sql| {
                sql.execute_batch(
                    "CREATE TABLE later(value TEXT); INSERT INTO later VALUES('delta')",
                )?;
                Ok(())
            })
            .unwrap();
        let bytes = delta.encode().unwrap();
        for machine in &mut machines {
            machine
                .apply(
                    &EntryRef {
                        index,
                        term: 1,
                        kind: EntryType::EntryNormal,
                        context: b"delta",
                        data: &bytes,
                    },
                    &mut Vec::new(),
                )
                .unwrap();
            let value: String = machine
                .read(|sql| {
                    Ok(sql.query_row("SELECT value FROM retained", [], |row| row.get(0usize))?)
                })
                .unwrap();
            assert_eq!(value, "unchanged base");
        }
        let mut image = Vec::new();
        let (point, configuration) = machines[0].image(&mut image).unwrap();
        let path = dir.path().join("unrelated.db");
        let mut unrelated = super::super::Machine::open(&path, conf).unwrap();
        assert!(unrelated.install(&image, point, &configuration).is_err());
        assert!(
            unrelated
                .apply(
                    &EntryRef {
                        index,
                        term: 1,
                        kind: EntryType::EntryNormal,
                        context: b"wrong-base",
                        data: &bytes
                    },
                    &mut Vec::new()
                )
                .is_err()
        );
        assert_eq!(unrelated.durable().index, 0);
    }
}
