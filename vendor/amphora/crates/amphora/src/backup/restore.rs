//! The shell that restores a second from the archive (docs/design/backup.md): the base's pages
//! written as the file, then each generation of log after it written as the log's file and
//! replayed by the engine's own recovery, the last one up to the second's last commit.

use std::io::Write as _;
use std::path::Path;
use std::time::Instant;

use amphora_api::blobs::Blobs;

use super::Mark;
use super::object::{self, Segment};
use super::plan::{self, BaseKey, SegmentKey, ThroughKey};
use super::ship::{FRAME_BYTES, list_all};
use crate::db::Db;

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    /// The generation and frame of the last commit it kept.
    pub cut: (u64, u64),
    pub base: u64,
    pub bytes: u64,
    /// The bytes of log it fetched ([`super::check_due`]'s measure).
    pub log_bytes: u64,
}

fn wal_path(path: &Path) -> std::path::PathBuf {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    std::path::PathBuf::from(wal)
}

/// The through marks under `prefix` ([`plan::ThroughKey`]).
pub(super) fn through_marks(
    blobs: &dyn Blobs,
    prefix: &str,
    until: Instant,
) -> anyhow::Result<Vec<(ThroughKey, String)>> {
    let mut marks = Vec::new();
    for (key, _) in list_all(blobs, &format!("{prefix}/through/"), until)? {
        marks.push((ThroughKey::parse(prefix, &key)?, key));
    }
    Ok(marks)
}

/// Restores the database as it stood at the end of `second` (every commit stamped at or before
/// it) from the archive under `prefix`, into a new file at `path`.
pub fn restore(
    blobs: &dyn Blobs,
    prefix: &str,
    second: u64,
    path: &Path,
    until: Instant,
) -> anyhow::Result<Restored> {
    if path.try_exists()? || wal_path(path).try_exists()? {
        anyhow::bail!("{} exists: a restore makes a new database", path.display());
    }
    let mut keys = Vec::new();
    for (key, _) in list_all(blobs, &format!("{prefix}/log/"), until)? {
        keys.push((SegmentKey::parse(prefix, &key)?, key));
    }
    let mut segments: Vec<SegmentKey> = keys.iter().map(|(s, _)| s.clone()).collect();
    let marks: Vec<ThroughKey> = through_marks(blobs, prefix, until)?
        .into_iter()
        .map(|(m, _)| m)
        .collect();
    plan::raise_through(&mut segments, &marks);
    let mut bases = Vec::new();
    for (key, _) in list_all(blobs, &format!("{prefix}/base/"), until)? {
        bases.push(BaseKey::parse(prefix, &key)?);
    }
    let at = plan::locate(&segments, second)?;
    let at_key = keys
        .get(at)
        .map(|(_, k)| k.clone())
        .ok_or_else(|| anyhow::anyhow!("segment {at} is gone"))?;
    let located: Segment = object::decode(&at_key, &blobs.get(&at_key, until)?)?;
    let marks: Vec<Mark> = located
        .marks
        .iter()
        .map(|m| Mark {
            seq: located.identity.seq,
            frame: m.frame,
            ns: m.ns,
            unknown_from: m.unknown_from,
        })
        .collect();
    let frame = plan::cut_in(&marks, second)?;
    let plan = plan::chain(&segments, &bases, at, frame, second)?;
    let mut bytes = 0u64;
    let mut log_bytes = 0u64;

    // The base's pages, as the file.
    let manifest_key = plan.base.key(prefix);
    let manifest = object::manifest(&manifest_key, &blobs.get(&manifest_key, until)?)?;
    {
        let mut file = std::fs::File::create_new(path)?;
        // An incremental base keeps an earlier base's chunks where the log changed none of their
        // pages (docs/design/backup.md, "Incremental bases").
        for (chunk, base) in (0u64..).zip(manifest.chunk_bases()?) {
            let key = plan::chunk_key(prefix, base, chunk);
            let sealed = blobs.get(&key, until)?;
            let pages = object::opened(&key, &sealed)?;
            file.write_all(pages)?;
            bytes = bytes.saturating_add(u64::try_from(sealed.len())?);
        }
        file.sync_all()?;
    }

    // Each generation's log, replayed whole but the last, which ends at the cut.
    let mut index = 0usize;
    while let Some(first) = plan.segments.get(index) {
        let generation = segments.get(*first).map_or(0, |s| s.generation);
        // The first generation's log is new (checked above); each later one takes the place of the
        // log the fold before it emptied but left (a truncating checkpoint keeps its file).
        let mut log = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(wal_path(path))?;
        let mut written = 0u64;
        let mut header_written = false;
        while let Some(i) = plan.segments.get(index) {
            let Some((key, name)) = keys.get(*i) else {
                anyhow::bail!("segment {i} is gone");
            };
            if key.generation != generation {
                break;
            }
            index = index.saturating_add(1);
            let body: Segment = if *i == at {
                log_bytes = log_bytes.saturating_add(u64::try_from(located.frames.len())?);
                located.clone()
            } else {
                let got = blobs.get(name, until)?;
                bytes = bytes.saturating_add(u64::try_from(got.len())?);
                log_bytes = log_bytes.saturating_add(u64::try_from(got.len())?);
                object::decode(name, &got)?
            };
            if body.first != written.saturating_add(1) {
                anyhow::bail!("segment {name} doesn't follow frame {written}");
            }
            if !header_written {
                log.write_all(&body.header)?;
                header_written = true;
            }
            let end = if generation == plan.cut.0 {
                key.last.min(plan.cut.1)
            } else {
                key.last
            };
            let take = end.saturating_sub(written).saturating_mul(FRAME_BYTES);
            let frames = body
                .frames
                .get(..usize::try_from(take)?)
                .ok_or_else(|| anyhow::anyhow!("segment {name} holds fewer frames than it says"))?;
            log.write_all(frames)?;
            written = end;
            if generation == plan.cut.0 && written >= plan.cut.1 {
                break;
            }
        }
        log.sync_all()?;
        drop(log);
        let mut db = Db::open(path)?;
        db.fold_log()?;
        drop(db);
        if generation == plan.cut.0 {
            if written != plan.cut.1 {
                anyhow::bail!(
                    "the archive's log ends at frame {written}, before the cut at {}",
                    plan.cut.1
                );
            }
            break;
        }
    }
    Ok(Restored {
        cut: plan.cut,
        base: plan.base.id,
        bytes,
        log_bytes,
    })
}

/// Deletes what a retention of `retention_ns` lets go at `now_ns` ([`plan::expired`]): how many
/// objects.
pub fn expire(
    blobs: &dyn Blobs,
    prefix: &str,
    now_ns: u64,
    retention_ns: u64,
    until: Instant,
) -> anyhow::Result<usize> {
    let mut segments = Vec::new();
    for (key, _) in list_all(blobs, &format!("{prefix}/log/"), until)? {
        segments.push(SegmentKey::parse(prefix, &key)?);
    }
    let marks: Vec<ThroughKey> = through_marks(blobs, prefix, until)?
        .into_iter()
        .map(|(m, _)| m)
        .collect();
    plan::raise_through(&mut segments, &marks);
    let mut bases = Vec::new();
    for (key, _) in list_all(blobs, &format!("{prefix}/base/"), until)? {
        bases.push(BaseKey::parse(prefix, &key)?);
    }
    let mut chunks = Vec::new();
    for (key, _) in list_all(blobs, &format!("{prefix}/pages/"), until)? {
        chunks.push((plan::chunk_id(prefix, &key)?, key));
    }
    let mut gone = plan::expired(prefix, &segments, &bases, &chunks, now_ns, retention_ns);
    // A chunk a kept base reuses stays, whichever base first stored it.
    let mut reused = std::collections::BTreeSet::new();
    for base in &bases {
        let key = base.key(prefix);
        if gone.contains(&key) {
            continue;
        }
        let manifest = object::manifest(&key, &blobs.get(&key, until)?)?;
        for (chunk, from) in (0u64..).zip(manifest.chunk_bases()?) {
            if from != manifest.id {
                reused.insert(plan::chunk_key(prefix, from, chunk));
            }
        }
    }
    gone.retain(|key| !reused.contains(key));
    for key in &gone {
        blobs.delete(key, until)?;
    }
    Ok(gone.len())
}
