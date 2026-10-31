//! CP-T1.5 / U06/U15: revision-preserving import into an unused Records namespace.
//! Database encloses each bounded page and its cursor in one rollback-on-error transaction.
//! Ordinary namespace operations remain hidden until the validated final marker is durable.
use super::*;
use amphora_api::import::{ImportPage, ImportProgress};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS __amphora_imports (
 namespace TEXT PRIMARY KEY, source TEXT NOT NULL, cursor BLOB NOT NULL,
 head INTEGER NOT NULL, horizon INTEGER NOT NULL, feed INTEGER NOT NULL,
 done INTEGER NOT NULL, last_page BLOB NOT NULL,
 live INTEGER NOT NULL, families INTEGER NOT NULL, missing INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS __amphora_import_families (
 namespace TEXT NOT NULL, family TEXT NOT NULL, newest INTEGER NOT NULL,
 PRIMARY KEY(namespace,family)
);";
const CURSOR_MAX: usize = 4096;
const SOURCE_MAX: usize = 1024;
// Charge a 128-byte publication slot per family (maximum 64-byte name plus map entry and
// ownership allowance). Refuse before another slot would exceed the product result budget.
const PUBLICATION_FAMILIES: usize = api::RESULT_MAX / 128;

pub(super) struct Imported {
    pub done: bool,
    /// Only the first finalization publishes. A replay after normal writes must not rewind
    /// the durable wait cache to the historical source head.
    pub publication: Option<ImportedState>,
}
pub(super) struct ImportedState {
    pub head: Rev,
    pub feed: Rev,
    pub newest: BTreeMap<String, Rev>,
}
#[derive(Default)]
struct Counts {
    live: u64,
    families: u64,
    missing: u64,
}
fn counts(db: &Db, namespace: &str) -> anyhow::Result<Counts> {
    let mut result = None;
    db.query(
        "SELECT live,families,missing FROM __amphora_imports WHERE namespace=?1",
        [text(namespace)],
        |r| {
            result = Some(Counts {
                live: rev(r, 0)?,
                families: rev(r, 1)?,
                missing: rev(r, 2)?,
            });
            Ok(false)
        },
    )?;
    result.ok_or_else(|| api::invalid("import state", "missing counters").into())
}
fn family_head(
    db: &Db,
    namespace: &str,
    family: &str,
    declared: bool,
) -> anyhow::Result<Option<u64>> {
    let query = if declared {
        "SELECT newest FROM __amphora_import_families WHERE namespace=?1 AND family=?2"
    } else {
        "SELECT newest FROM __amphora_families WHERE namespace=?1 AND family=?2"
    };
    let mut result = None;
    db.query(query, [text(namespace), text(family)], |r| {
        result = Some(rev(r, 0)?);
        Ok(false)
    })?;
    Ok(result)
}
fn another_family(counts: &mut Counts) -> anyhow::Result<()> {
    counts.families = counts
        .families
        .checked_add(1)
        .ok_or_else(|| api::invalid("import families", "count overflow"))?;
    api::bound(
        "import family publication",
        usize::try_from(counts.families)?,
        PUBLICATION_FAMILIES,
    )?;
    Ok(())
}
pub(super) fn progress(db: &Db, namespace: &str) -> anyhow::Result<Option<ImportProgress>> {
    let mut result = None;
    db.query(
        "SELECT source,cursor,done FROM __amphora_imports WHERE namespace=?1",
        [text(namespace)],
        |r| {
            result = Some(ImportProgress {
                source: r.get::<String>(0).map_err(unavailable)?,
                cursor: bytes(r, 1)?.ok_or_else(|| api::invalid("import cursor", "null"))?,
                done: r.get::<i64>(2).map_err(unavailable)? != 0,
            });
            Ok(false)
        },
    )?;
    Ok(result)
}
pub(super) fn incomplete(db: &Db, namespace: &str) -> anyhow::Result<bool> {
    Ok(progress(db, namespace)?.is_some_and(|p| !p.done))
}

pub(super) fn validate(page: &ImportPage) -> Result<(), Refusal> {
    if page.source.is_empty() {
        return Err(api::invalid(
            "import source",
            "an immutable snapshot identity is required",
        ));
    }
    api::bound("import source", page.source.len(), SOURCE_MAX)?;
    api::bound("import cursor", page.expected_cursor.len(), CURSOR_MAX)?;
    api::bound("import cursor", page.next_cursor.len(), CURSOR_MAX)?;
    if page.next_cursor.is_empty() || page.expected_cursor == page.next_cursor {
        return Err(api::invalid(
            "import cursor",
            "each page must advance to a nonempty position",
        ));
    }
    api::bound("import versions", page.versions.len(), api::TX_MAX)?;
    api::bound("import families", page.state.newest.len(), api::TX_MAX)?;
    let state = &page.state;
    if i64::try_from(state.head).is_err()
        || state.horizon > state.head
        || state.feed > state.head
        || state.feed.saturating_add(1) < state.horizon
    {
        return Err(api::invalid(
            "import state",
            "head, snapshot horizon and feed horizon are inconsistent",
        ));
    }
    let mut size = page
        .source
        .len()
        .checked_add(page.expected_cursor.len())
        .and_then(|n| n.checked_add(page.next_cursor.len()))
        .ok_or_else(|| api::invalid("import page", "size overflow"))?;
    let sequences = u64::try_from(api::TX_MAX)
        .map_err(|_| api::invalid("import sequence", "limit conversion failed"))?;
    for v in &page.versions {
        api::family(&v.family)?;
        api::key(&v.key)?;
        // Source commits use zero-based write positions below TX_MAX. Preserving that bound
        // prevents an imported revision that no bounded change-feed page could ever deliver.
        if v.rev == 0 || v.rev > state.head || v.seq >= sequences {
            return Err(api::invalid(
                "import version",
                "revision or sequence is outside the source state",
            ));
        }
        if let Some(value) = &v.value {
            api::bound("value", value.len(), api::VALUE_MAX)?;
        }
        size = size
            .checked_add(v.family.len())
            .and_then(|n| n.checked_add(v.key.len()))
            .and_then(|n| n.checked_add(v.value.as_ref().map_or(0, Vec::len)))
            .and_then(|n| n.checked_add(32))
            .ok_or_else(|| api::invalid("import page", "size overflow"))?;
    }
    for (family, newest) in &state.newest {
        api::family(family)?;
        if *newest > state.head {
            return Err(api::invalid(
                "import family",
                "newest is ahead of the source head",
            ));
        }
        size = size
            .checked_add(family.len())
            .and_then(|n| n.checked_add(16))
            .ok_or_else(|| api::invalid("import page", "size overflow"))?;
    }
    api::bound("import page bytes", size, api::REQUEST_MAX)
}
fn occupied(db: &Db, namespace: &str) -> anyhow::Result<bool> {
    let mut found = false;
    for query in [
        "SELECT 1 FROM __amphora_namespaces WHERE name=?1 AND (head!=0 OR horizon!=0 OR feed!=0 OR live!=0) LIMIT 1",
        "SELECT 1 FROM __amphora_versions WHERE namespace=?1 LIMIT 1",
        "SELECT 1 FROM __amphora_families WHERE namespace=?1 LIMIT 1",
    ] {
        db.query(query, [text(namespace)], |_| {
            found = true;
            Ok(false)
        })?;
        if found {
            break;
        }
    }
    Ok(found)
}
/// Errors MUST escape Database's transaction: converting an error after an insert into a
/// successful answer would retain a partial page. This function owns no transaction or callback.
pub(super) fn apply_page(db: &Db, namespace: &str, page: &ImportPage) -> anyhow::Result<Imported> {
    api::family(namespace)?;
    validate(page)?;
    let encoded = postcard::to_allocvec(page)?;
    api::bound("encoded import page", encoded.len(), api::REQUEST_MAX)?;
    if let Some(prior) = progress(db, namespace)? {
        if prior.source != page.source {
            return Err(
                api::invalid("import source", "the namespace belongs to another snapshot").into(),
            );
        }
        let mut metadata = None;
        db.query(
            "SELECT head,horizon,feed,last_page FROM __amphora_imports WHERE namespace=?1",
            [text(namespace)],
            |r| {
                metadata = Some((
                    rev(r, 0)?,
                    rev(r, 1)?,
                    rev(r, 2)?,
                    bytes(r, 3)?.ok_or_else(|| api::invalid("import replay", "missing page"))?,
                ));
                Ok(false)
            },
        )?;
        let (head, horizon, feed, last) =
            metadata.ok_or_else(|| api::invalid("import state", "missing metadata"))?;
        if (head, horizon, feed) != (page.state.head, page.state.horizon, page.state.feed) {
            return Err(
                api::invalid("import state", "source metadata changed between pages").into(),
            );
        }
        if prior.cursor == page.next_cursor && last == encoded {
            return Ok(Imported {
                done: prior.done,
                publication: None,
            });
        }
        if prior.done {
            return Err(api::invalid("import", "the namespace import is already complete").into());
        }
        if prior.cursor != page.expected_cursor {
            return Err(api::invalid(
                "import cursor",
                "the expected position does not match the committed page",
            )
            .into());
        }
    } else {
        if !page.expected_cursor.is_empty() {
            return Err(api::invalid("import cursor", "the initial cursor must be empty").into());
        }
        if occupied(db, namespace)? {
            return Err(
                api::invalid("import namespace", "it already contains Records state").into(),
            );
        }
        db.query("INSERT INTO __amphora_imports(namespace,source,cursor,head,horizon,feed,done,last_page,live,families,missing) VALUES (?1,?2,?3,?4,?5,?6,0,?7,0,0,0)",[text(namespace),text(&page.source),blob(&[])?,integer(page.state.head)?,integer(page.state.horizon)?,integer(page.state.feed)?,blob(&[])?],|_|Ok(true))?;
        db.query("INSERT OR IGNORE INTO __amphora_namespaces(name,head,horizon,feed,live) VALUES (?1,0,0,0,0)",[text(namespace)],|_|Ok(true))?;
    }
    let mut counts = counts(db, namespace)?;
    for version in &page.versions {
        let mut exists = false;
        db.query("SELECT 1 FROM __amphora_versions WHERE namespace=?1 AND family=?2 AND key=?3 AND rev=?4",[text(namespace),text(&version.family),blob(&version.key)?,integer(version.rev)?],|_|{exists=true;Ok(false)})?;
        if exists {
            return Err(api::invalid(
                "import version",
                "the same family/key/revision appears more than once",
            )
            .into());
        }
        let mut sequence_used = false;
        // The existing feed index bounds this lookup to one family/commit's TX_MAX slots.
        db.query("SELECT 1 FROM __amphora_versions WHERE namespace=?1 AND family=?2 AND rev=?3 AND seq=?4 LIMIT 1",[text(namespace),text(&version.family),integer(version.rev)?,integer(version.seq)?],|_|{sequence_used=true;Ok(false)})?;
        if sequence_used {
            return Err(api::invalid(
                "import sequence",
                "two changes occupy one family/commit sequence",
            )
            .into());
        }
        let derived = family_head(db, namespace, &version.family, false)?;
        let declared = family_head(db, namespace, &version.family, true)?;
        if declared.is_some_and(|head| head < version.rev) {
            return Err(api::invalid(
                "import family",
                "a version is ahead of its declared source family head",
            )
            .into());
        }
        if derived.is_none() {
            another_family(&mut counts)?;
            if declared.is_none() {
                counts.missing = counts
                    .missing
                    .checked_add(1)
                    .ok_or_else(|| api::invalid("import families", "count overflow"))?;
            }
        }
        let mut latest = None;
        db.query("SELECT rev,value IS NOT NULL FROM __amphora_versions WHERE namespace=?1 AND family=?2 AND key=?3 ORDER BY rev DESC LIMIT 1",[text(namespace),text(&version.family),blob(&version.key)?],|r|{latest=Some((rev(r,0)?,r.get::<i64>(1).map_err(unavailable)?!=0));Ok(false)})?;
        if latest.is_none_or(|(rev, _)| version.rev > rev) {
            match (
                latest.is_some_and(|(_, live)| live),
                version.value.is_some(),
            ) {
                (false, true) => {
                    counts.live = counts
                        .live
                        .checked_add(1)
                        .ok_or_else(|| api::invalid("import live rows", "count overflow"))?
                }
                (true, false) => {
                    counts.live = counts
                        .live
                        .checked_sub(1)
                        .ok_or_else(|| api::invalid("import live rows", "count underflow"))?
                }
                _ => {}
            }
        }
        db.query("INSERT INTO __amphora_versions(namespace,family,key,rev,seq,value) VALUES (?1,?2,?3,?4,?5,?6)",[text(namespace),text(&version.family),blob(&version.key)?,integer(version.rev)?,integer(version.seq)?,version.value.as_ref().map(|v|blob(v)).transpose()?.unwrap_or(EValue::Null)],|_|Ok(true))?;
        db.query("INSERT INTO __amphora_families(namespace,family,newest) VALUES (?1,?2,?3) ON CONFLICT(namespace,family) DO UPDATE SET newest=MAX(__amphora_families.newest,excluded.newest)",[text(namespace),text(&version.family),integer(version.rev)?],|_|Ok(true))?;
    }
    for (family, newest) in &page.state.newest {
        let derived = family_head(db, namespace, family, false)?;
        let declared = family_head(db, namespace, family, true)?;
        if declared.is_some_and(|old| old != *newest) {
            return Err(api::invalid(
                "import family",
                "a source family head changed between pages",
            )
            .into());
        }
        if derived.is_some_and(|head| head > *newest) {
            return Err(api::invalid(
                "import family",
                "a declared family head precedes a retained version",
            )
            .into());
        }
        if derived.is_none() {
            another_family(&mut counts)?;
        } else if declared.is_none() {
            counts.missing = counts
                .missing
                .checked_sub(1)
                .ok_or_else(|| api::invalid("import families", "missing count underflow"))?;
        }
        db.query("INSERT OR IGNORE INTO __amphora_import_families(namespace,family,newest) VALUES (?1,?2,?3)",[text(namespace),text(family),integer(*newest)?],|_|Ok(true))?;
        db.query("INSERT INTO __amphora_families(namespace,family,newest) VALUES (?1,?2,?3) ON CONFLICT(namespace,family) DO UPDATE SET newest=MAX(__amphora_families.newest,excluded.newest)",[text(namespace),text(family),integer(*newest)?],|_|Ok(true))?;
    }
    let publication = if page.done {
        Some(finalize(db, namespace, page, &counts)?)
    } else {
        None
    };
    db.query("UPDATE __amphora_imports SET cursor=?2,done=?3,last_page=?4,live=?5,families=?6,missing=?7 WHERE namespace=?1",[text(namespace),blob(&page.next_cursor)?,integer(u64::from(page.done))?,blob(&encoded)?,integer(counts.live)?,integer(counts.families)?,integer(counts.missing)?],|_|Ok(true))?;
    Ok(Imported {
        done: page.done,
        publication,
    })
}
fn finalize(
    db: &Db,
    namespace: &str,
    page: &ImportPage,
    counts: &Counts,
) -> anyhow::Result<ImportedState> {
    // Counters were advanced with each page. Finalization never rescans version history.
    if counts.missing != 0 {
        return Err(api::invalid("import families", "source family metadata is incomplete").into());
    }
    if counts.live > LIVE_MAX {
        return Err(Refusal::Full {
            limit: usize::try_from(LIVE_MAX)?,
        }
        .into());
    }
    let mut newest = BTreeMap::new();
    db.query(
        "SELECT family,newest FROM __amphora_families WHERE namespace=?1 ORDER BY family",
        [text(namespace)],
        |r| {
            api::bound(
                "import family publication",
                newest
                    .len()
                    .checked_add(1)
                    .ok_or_else(|| api::invalid("import families", "count overflow"))?,
                PUBLICATION_FAMILIES,
            )?;
            let head = rev(r, 1)?;
            if head > page.state.head {
                return Err(api::invalid(
                    "import family",
                    "a family head is ahead of the namespace",
                )
                .into());
            }
            newest.insert(r.get::<String>(0).map_err(unavailable)?, head);
            Ok(true)
        },
    )?;
    if u64::try_from(newest.len())? != counts.families {
        return Err(
            api::invalid("import families", "source family counters are inconsistent").into(),
        );
    }
    db.query(
        "UPDATE __amphora_namespaces SET head=?2,horizon=?3,feed=?4,live=?5 WHERE name=?1",
        [
            text(namespace),
            integer(page.state.head)?,
            integer(page.state.horizon)?,
            integer(page.state.feed)?,
            integer(counts.live)?,
        ],
        |_| Ok(true),
    )?;
    Ok(ImportedState {
        head: page.state.head,
        feed: page.state.feed,
        newest,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use amphora_api::import::{ImportState, ImportVersion};
    fn version(key: &[u8], rev: Rev, seq: u64, value: Option<&[u8]>) -> ImportVersion {
        ImportVersion {
            family: "items".into(),
            key: key.to_vec(),
            rev,
            seq,
            value: value.map(<[u8]>::to_vec),
        }
    }
    fn first() -> ImportPage {
        ImportPage {
            source: "immutable-source-sha256".into(),
            expected_cursor: vec![],
            next_cursor: vec![1],
            versions: vec![
                version(b"a", 1, 0, Some(b"old")),
                version(b"b", 2, 0, Some(b"before delete")),
            ],
            state: ImportState {
                head: 5,
                horizon: 2,
                feed: 1,
                newest: vec![],
            },
            done: false,
        }
    }
    fn last() -> ImportPage {
        ImportPage {
            source: "immutable-source-sha256".into(),
            expected_cursor: vec![1],
            next_cursor: vec![2],
            versions: vec![version(b"a", 3, 0, Some(b"new")), version(b"b", 3, 1, None)],
            state: ImportState {
                head: 5,
                horizon: 2,
                feed: 1,
                newest: vec![("items".into(), 3), ("compacted".into(), 4)],
            },
            done: true,
        }
    }
    fn open(path: &Path) -> anyhow::Result<Db> {
        let db = Db::open(path)?;
        db.exec(super::super::SCHEMA)?;
        db.exec(super::SCHEMA)?;
        Ok(db)
    }
    #[test]
    fn u06_import_reopens_preserving_baselines_tombstones_and_family_heads() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("import.amphora");
        {
            let mut db = open(&path)?;
            let imported = db.transaction(|db| apply_page(db, "host", &first()))?;
            assert!(!imported.done);
            assert!(imported.publication.is_none());
            assert!(incomplete(&db, "host")?);
            assert_eq!(progress(&db, "host")?.map(|p| p.cursor), Some(vec![1]));
        }
        let mut db = open(&path)?;
        assert!(incomplete(&db, "host")?);
        let imported = db.transaction(|db| apply_page(db, "host", &last()))?;
        assert!(imported.done);
        let published = imported
            .publication
            .ok_or_else(|| anyhow::anyhow!("missing final publication"))?;
        assert_eq!((published.head, published.feed), (5, 1));
        assert_eq!(published.newest.get("compacted"), Some(&4));
        assert!(!incomplete(&db, "host")?);
        assert_eq!(
            get(&db, "host", "items", b"a", 2)?.map(|r| (r.rev, r.value)),
            Some((1, b"old".to_vec()))
        );
        assert_eq!(
            get(&db, "host", "items", b"a", 5)?.map(|r| (r.rev, r.value)),
            Some((3, b"new".to_vec()))
        );
        assert!(get(&db, "host", "items", b"b", 5)?.is_none());
        let mut deletes = Vec::new();
        db.query("SELECT seq,value FROM __amphora_versions WHERE namespace='host' AND rev=3 ORDER BY seq",[],|r| {deletes.push((rev(r,0)?,bytes(r,1)?));Ok(true)})?;
        assert_eq!(deletes, vec![(0, Some(b"new".to_vec())), (1, None)]);
        // U06: the boundary deletion stays observable, then obsolete history is reclaimed.
        for before in [3, 4] {
            // Force one visited version per physical transaction. A tombstone becomes
            // eligible only after its older value is gone; reopening must retain that cursor.
            loop {
                let (progress, _) = db.compaction_attempt(100, false, |db| {
                    super::super::compaction::step(db, "host", before, 1)
                })?;
                assert!(progress.visited <= 1);
                assert!(get(&db, "host", "items", b"b", 5)?.is_none());
                if !progress.more {
                    break;
                }
                drop(db);
                db = open(&path)?;
            }
            if before == 3 {
                let answer =
                    super::super::apply(&db, "host", &Operation::Changes("items".into(), 2, 10))??;
                let Answer::Changes(page) = answer else {
                    anyhow::bail!("missing boundary changes")
                };
                assert_eq!(page.changes.len(), 2);
                assert!(
                    page.changes
                        .iter()
                        .any(|change| change.key == b"b" && change.value.is_none())
                );
            }
        }
        let mut retained = 0;
        db.query(
            "SELECT COUNT(*) FROM __amphora_versions WHERE namespace = 'host'",
            [],
            |row| {
                retained = rev(row, 0)?;
                Ok(false)
            },
        )?;
        assert_eq!(
            retained, 1,
            "compaction retains one live baseline, not deleted-key history"
        );

        // U06/U15: actual prepared-page pressure must shrink the GC prefix, commit progress,
        // and resume it after recovery. Derive the small allowance from a real one-visit image;
        // the full image must exceed it, so this cannot pass without adaptive page admission.
        use hyper_durable::{EntryRef, StateMachine};
        use hyper_raft::proto::{ConfState, EntryType};
        let config = ConfState {
            voters: vec![1],
            ..ConfState::default()
        };
        let bounded_path = dir.path().join("bounded.db");
        let mut machine = crate::replication::Machine::open(&bounded_path, config.clone())?;
        let (prepared, cutoff) = machine.prepare(|connection| {
            for value in [vec![7; 16384], b"current".to_vec()] {
                connection.records_commit(
                    "bounded",
                    Tx {
                        conds: vec![],
                        writes: (0..64u32)
                            .map(|key| Write::Put {
                                family: "items".into(),
                                key: key.to_be_bytes().to_vec(),
                                value: value.clone(),
                            })
                            .collect(),
                    },
                )?;
            }
            Ok(2)
        })?;
        let apply = |machine: &mut crate::replication::Machine,
                     index: u64,
                     bytes: &[u8]|
         -> anyhow::Result<()> {
            let mut answers = Vec::new();
            machine.apply(
                &EntryRef {
                    index,
                    term: 1,
                    kind: EntryType::EntryNormal,
                    context: b"bounded-compaction",
                    data: bytes,
                },
                &mut answers,
            )?;
            assert_eq!(answers.len(), 1);
            assert!(answers[0].result.is_ok());
            Ok(())
        };
        apply(&mut machine, 1, &prepared.encode()?)?;
        let (one, _) =
            machine.prepare_db(|db| super::super::compaction::step(db, "bounded", cutoff, 1))?;
        let one_encoded = one.encode()?.len();
        let pages = one_encoded.div_ceil(4096);
        let room = 81 + pages * 4103;
        let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let (full, whole) =
            machine.prepare_compaction("bounded", cutoff, until, api::REQUEST_MAX)?;
        assert!(
            full.encode()?.len() > room,
            "fixture must exceed the actual small entry allowance"
        );
        let (prefix, progress) = machine.prepare_compaction("bounded", cutoff, until, room)?;
        assert!(progress.visited > 0 && progress.visited < whole.visited);
        assert!(progress.deleted > 0 && progress.more);
        assert!(prefix.encode()?.len() <= room);
        let mut visited = progress.visited;
        let mut deleted = progress.deleted;
        apply(&mut machine, 2, &prefix.encode()?)?;
        drop(machine);
        let mut machine = crate::replication::Machine::open(&bounded_path, config)?;
        let mut completed = progress.completed;
        for index in 3..131 {
            let (prefix, progress) = machine.prepare_compaction("bounded", cutoff, until, room)?;
            assert!(prefix.encode()?.len() <= room);
            visited += progress.visited;
            deleted += progress.deleted;
            apply(&mut machine, index, &prefix.encode()?)?;
            completed = progress.completed;
            if !progress.more {
                break;
            }
        }
        assert_eq!(completed, cutoff);
        assert_eq!(
            visited, whole.visited,
            "recovery must not revisit the committed cursor prefix"
        );
        assert_eq!(deleted, 64);
        machine.read(|connection| {
            for key in 0..64u32 {
                assert_eq!(
                    connection
                        .records_get("bounded", "items", &key.to_be_bytes())?
                        .map(|row| row.value),
                    Some(b"current".to_vec())
                );
            }
            Ok(())
        })?;

        drop(db);
        let db = open(&path)?;
        assert!(get(&db, "host", "items", b"b", 5)?.is_none());
        assert_eq!(
            get(&db, "host", "items", b"a", 5)?.map(|row| row.rev),
            Some(3)
        );
        Ok(())
    }
    #[test]
    fn u06_import_fences_source_cursor_and_rolls_back_failed_finalization() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = open(&dir.path().join("import.amphora"))?;
        db.transaction(|db| apply_page(db, "host", &first()))?;
        // Lost page acknowledgement: replay is exact and changes no imported revision.
        db.transaction(|db| apply_page(db, "host", &first()))?;
        for bad in [
            {
                let mut p = last();
                p.source = "different-snapshot".into();
                p
            },
            {
                let mut p = last();
                p.expected_cursor = vec![99];
                p
            },
            {
                let mut p = last();
                p.state.head = 6;
                p
            },
            // Fails after its versions have been inserted: none may remain and cursor stays 1.
            {
                let mut p = last();
                p.state.newest = vec![("items".into(), 1)];
                p
            },
            {
                let mut p = last();
                p.state.newest.clear();
                p
            },
            {
                let mut p = last();
                if let Some(v) = p.versions.last_mut() {
                    v.seq = 1000;
                }
                p
            },
            {
                let mut p = last();
                if let Some(v) = p.versions.last_mut() {
                    v.seq = 0;
                }
                p
            },
        ] {
            assert!(db.transaction(|db| apply_page(db, "host", &bad)).is_err());
            assert_eq!(progress(&db, "host")?.map(|p| p.cursor), Some(vec![1]));
            assert!(get(&db, "host", "items", b"a", 3)?.is_some_and(|r| r.rev == 1));
            assert!(incomplete(&db, "host")?);
        }
        db.transaction(|db| apply_page(db, "host", &last()))?;
        let outcome = db.transaction(|db| {
            commit(
                db,
                "host",
                &Tx {
                    conds: vec![],
                    writes: vec![Write::Put {
                        family: "items".into(),
                        key: b"c".to_vec(),
                        value: b"later".to_vec(),
                    }],
                },
                5,
                1,
            )
        })?;
        assert!(matches!(outcome, Ok(Answer::Rev(6))));
        let replay = db.transaction(|db| apply_page(db, "host", &last()))?;
        assert!(replay.done && replay.publication.is_none());
        let mut head = 0;
        db.query(
            "SELECT head FROM __amphora_namespaces WHERE name='host'",
            [],
            |r| {
                head = rev(r, 0)?;
                Ok(false)
            },
        )?;
        assert_eq!(head, 6);
        let mut divergent = last();
        divergent.state.newest.clear();
        assert!(
            db.transaction(|db| apply_page(db, "host", &divergent))
                .is_err()
        );
        Ok(())
    }
    /// T-T1.5.2 / U06: source pages may deliver declarations before values and old baselines
    /// after newer tombstones. The final visible live state must still match that history.
    #[test]
    fn u06_import_arbitrary_page_order_preserves_live_count_and_baselines() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = open(&dir.path().join("import.amphora"))?;
        let mut declared = first();
        declared.versions.clear();
        declared.state.newest = last().state.newest;
        db.transaction(|db| apply_page(db, "host", &declared))?;
        let mut recent = last();
        // Repeating equal source declarations is idempotent across pages.
        recent.state.newest = last().state.newest;
        recent.done = false;
        db.transaction(|db| apply_page(db, "host", &recent))?;
        let mut baseline = first();
        baseline.expected_cursor = vec![2];
        baseline.next_cursor = vec![3];
        baseline.done = true;
        db.transaction(|db| apply_page(db, "host", &baseline))?;
        let mut live = 0;
        db.query(
            "SELECT live FROM __amphora_namespaces WHERE name='host'",
            [],
            |r| {
                live = rev(r, 0)?;
                Ok(false)
            },
        )?;
        assert_eq!(live, 1);
        assert!(get(&db, "host", "items", b"b", 5)?.is_none());
        assert_eq!(
            get(&db, "host", "items", b"b", 2)?.map(|r| r.value),
            Some(b"before delete".to_vec())
        );
        assert_eq!(
            get(&db, "host", "items", b"a", 5)?.map(|r| r.value),
            Some(b"new".to_vec())
        );
        Ok(())
    }
}
