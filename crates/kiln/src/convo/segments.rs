//! U06/U15: immutable transcript content, staged in bounded calls on the host Database.
//! A record may refer to codec 3 only after its content manifest is ready. Staging never captures
//! the whole input line in a product call. Each new digest reserves its entire logical size and
//! one manifest, including abandoned staging, within a bounded immutable pool. At capacity new
//! content refuses; existing content remains readable and resumable. We never delete chunks:
//! pending imports and archived payloads can hold references outside the live record table.

use std::time::{Duration, Instant};

use amphora::database::Database;
use amphora_api::sql::{Connection, OptionalExtension as _, params};
use sha2::{Digest as _, Sha256};

/// The original recorder's maximum line size, in UTF-8 bytes.
pub const LINE_MAX: usize = 256 << 20;
const PART_BYTES: usize = 512 << 10;
const PARTS_MAX: usize = LINE_MAX / PART_BYTES;
// Eight full parts would leave no room below 4 MiB for SQL/identity metadata. Seven full
// parts keep each captured batch below that ceiling, including its small owned metadata.
const BATCH_PARTS: usize = 7;
const MARKER_BYTES: usize = 41;
const CODEC: u8 = 3;
const CALL_WAIT: Duration = Duration::from_secs(30);

// U06/U15: admission limits for the WHOLE immutable pool, published and abandoned alike.
// Full-size reservations bound partial/retried staging too. Two limits bound empty manifests
// and part/index overhead independently of payload size; this is not a physical SQL/WAL quota.
const QUOTA: Quota = Quota { bytes: 16 << 30, manifests: 65_536 };
#[derive(Clone, Copy)]
struct Quota { bytes: u64, manifests: usize }

#[derive(Clone, Copy)]
struct Manifest {
    size: usize,
    parts: usize,
    digest: [u8; 32],
}

fn until() -> anyhow::Result<Instant> {
    Instant::now()
        .checked_add(CALL_WAIT)
        .ok_or_else(|| anyhow::anyhow!("transcript staging deadline overflow"))
}

fn manifest(marker: &[u8]) -> anyhow::Result<Manifest> {
    anyhow::ensure!(marker.len() == MARKER_BYTES && marker.first() == Some(&CODEC), "invalid staged transcript marker");
    let size = marker
        .get(1..9)
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| anyhow::anyhow!("invalid staged transcript size"))?;
    let size = usize::try_from(size)?;
    anyhow::ensure!(size <= LINE_MAX, "transcript exceeds bounded line size");
    let digest = marker
        .get(9..MARKER_BYTES)
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or_else(|| anyhow::anyhow!("invalid staged transcript digest"))?;
    Ok(Manifest { size, parts: size.div_ceil(PART_BYTES), digest })
}

fn expected_part(size: usize, part: usize) -> anyhow::Result<usize> {
    let offset = part.checked_mul(PART_BYTES).ok_or_else(|| anyhow::anyhow!("transcript part offset overflow"))?;
    let left = size.checked_sub(offset).ok_or_else(|| anyhow::anyhow!("transcript part is outside the line"))?;
    Ok(left.min(PART_BYTES))
}

fn stored(conn: &Connection<'_>, content: Manifest) -> anyhow::Result<(usize, usize, i64)> {
    Ok(conn.query_row(
        "SELECT size,parts,ready FROM convo_chunk_manifests WHERE digest=?1",
        params![content.digest],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?)
}

fn matches(content: Manifest, size: usize, parts: usize) -> anyhow::Result<()> {
    anyhow::ensure!(size == content.size && parts == content.parts, "staged transcript manifest conflicts with its content identity");
    Ok(())
}

fn quota_bytes(size: u64, limit: u64) -> amphora_api::Refusal {
    match (usize::try_from(size), usize::try_from(limit)) {
        (Ok(size), Ok(limit)) => amphora_api::Refusal::TooLarge {
            what: "immutable transcript reservation quota (includes abandoned staging)".into(), size, limit,
        },
        _ => amphora_api::Refusal::Unavailable(format!("immutable transcript reservation quota exhausted: {size} bytes exceeds {limit}; existing content is retained")),
    }
}

/// Runs in the writer transaction BEFORE any new content or parts are inserted. The pool's
/// one-time migration seed and every manifest/counter update are atomic; admission reads one
/// singleton row, with no manifest scan or chunk payload reads and no reset on reopen.
fn reserve(conn: &Connection<'_>, content: Manifest, quota: Quota) -> anyhow::Result<()> {
    let prior = conn.query_row(
        "SELECT size,parts,ready FROM convo_chunk_manifests WHERE digest=?1", params![content.digest],
        |row| Ok((row.get::<_,usize>(0)?,row.get::<_,usize>(1)?,row.get::<_,i64>(2)?)),
    ).optional()?;
    if let Some((size, parts, ready)) = prior {
        matches(content, size, parts)?;
        anyhow::ensure!(ready == 0 || ready == 1, "invalid staged transcript readiness");
        // A stopped importer already owns the full reservation. Lowering a budget or
        // reopening an over-limit old pool must never prevent that content from completing.
        return Ok(());
    }
    let (count, bytes) = conn.query_row(
        "SELECT manifests,bytes FROM convo_chunk_pool WHERE id=1", [],
        |row| Ok((row.get::<_,usize>(0)?,row.get::<_,u64>(1)?)),
    )?;
    if count >= quota.manifests { return Err(amphora_api::Refusal::Full { limit: quota.manifests }.into()); }
    let count = count.checked_add(1).ok_or_else(|| anyhow::anyhow!("transcript manifest quota overflow"))?;
    let bytes = bytes.checked_add(u64::try_from(content.size)?).ok_or_else(|| anyhow::anyhow!("transcript reservation size overflow"))?;
    if bytes > quota.bytes { return Err(quota_bytes(bytes, quota.bytes).into()); }
    conn.execute(
        "INSERT INTO convo_chunk_manifests(digest,size,parts,ready) VALUES (?1,?2,?3,0)",
        params![content.digest, content.size, content.parts],
    )?;
    let updated = conn.execute("UPDATE convo_chunk_pool SET bytes=?1,manifests=?2 WHERE id=1", params![bytes,count])?;
    anyhow::ensure!(updated==1, "transcript reservation counter is missing");
    Ok(())
}

fn validate_shape(conn: &Connection<'_>, content: Manifest) -> anyhow::Result<()> {
    let mut query = conn.prepare("SELECT part,length(value) FROM convo_chunks WHERE digest=?1 ORDER BY part LIMIT ?2")?;
    let rows = query.query_map(params![content.digest, PARTS_MAX.checked_add(1).ok_or_else(|| anyhow::anyhow!("transcript part limit overflow"))?], |row| Ok((row.get::<_, usize>(0)?, row.get::<_, usize>(1)?)))?;
    let mut count = 0usize;
    let mut bytes = 0usize;
    for row in rows {
        let (part, size) = row?;
        anyhow::ensure!(part == count && count < content.parts && size == expected_part(content.size, part)?, "staged transcript chunks are incomplete or malformed");
        count = count.checked_add(1).ok_or_else(|| anyhow::anyhow!("transcript part count overflow"))?;
        bytes = bytes.checked_add(size).ok_or_else(|| anyhow::anyhow!("transcript size overflow"))?;
    }
    anyhow::ensure!(count == content.parts && bytes == content.size, "staged transcript chunks are incomplete");
    Ok(())
}

/// Verifies the ready manifest inside the same SQL transaction that commits its record/cursor.
/// Recheck the bounded part shape atomically with publication, including ready manifests.
pub fn validate(conn: &Connection<'_>, marker: &[u8]) -> anyhow::Result<()> {
    let content = manifest(marker)?;
    let (size, parts, ready) = stored(conn, content)?;
    matches(content, size, parts)?;
    anyhow::ensure!(ready == 1, "staged transcript content is not ready");
    validate_shape(conn, content)
}

/// Stages one line without copying it into a Database closure. Its digest is the stable content
/// identity, so retries and concurrent staging reuse equal parts and refuse divergent bytes.
pub fn stage(database: &Database, line: &str) -> anyhow::Result<Vec<u8>> {
    stage_with_quota(database, line, QUOTA)
}

fn stage_with_quota(database: &Database, line: &str, quota: Quota) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(line.len() <= LINE_MAX, "transcript exceeds bounded line size");
    let content = Manifest { size: line.len(), parts: line.len().div_ceil(PART_BYTES), digest: Sha256::digest(line.as_bytes()).into() };
    database.call(until()?, move |conn| reserve(conn, content, quota))?;

    let mut parts = line.as_bytes().chunks(PART_BYTES).enumerate();
    // The input bound permits at most 512 parts. Only one <=3.5 MiB batch is owned at a time.
    loop {
        let batch: Vec<(usize, Vec<u8>)> = parts.by_ref().take(BATCH_PARTS).map(|(part, bytes)| (part, bytes.to_vec())).collect();
        if batch.is_empty() { break; }
        database.call(until()?, move |conn| {
            for (part, value) in batch {
                // Bound the returned bytes even if an existing row was externally corrupted.
                let prior = conn.query_row(
                    "SELECT length(value),substr(value,1,?3) FROM convo_chunks WHERE digest=?1 AND part=?2",
                    params![content.digest, part, PART_BYTES],
                    |row| Ok((row.get::<_, usize>(0)?, row.get::<_, Vec<u8>>(1)?)),
                ).optional()?;
                if let Some((size, prior)) = prior {
                    anyhow::ensure!(size == value.len() && prior == value, "staged transcript chunk conflicts with its content identity");
                } else {
                    conn.execute(
                        "INSERT OR IGNORE INTO convo_chunks(digest,part,value) VALUES (?1,?2,?3)",
                        params![content.digest, part, value],
                    )?;
                }
            }
            Ok(())
        })?;
    }

    // Only integer metadata crosses this final call; no transcript bytes are captured/read.
    database.call(until()?, move |conn| {
        let (size, parts, ready) = stored(conn, content)?;
        matches(content, size, parts)?;
        anyhow::ensure!(ready == 0 || ready == 1, "invalid staged transcript readiness");
        validate_shape(conn, content)?;
        conn.execute("UPDATE convo_chunk_manifests SET ready=1 WHERE digest=?1", params![content.digest])?;
        Ok(())
    })?;
    let mut marker = Vec::with_capacity(MARKER_BYTES);
    marker.push(CODEC);
    marker.extend_from_slice(&u64::try_from(content.size)?.to_le_bytes());
    marker.extend_from_slice(&content.digest);
    Ok(marker)
}

#[cfg(test)]
mod quota_tests;

/// Reads one bounded immutable part per query, then verifies the exact byte length, SHA-256
/// identity and UTF-8 before returning a transcript. No query materializes the complete line.
pub fn read(conn: &Connection<'_>, marker: &[u8]) -> anyhow::Result<String> {
    let content = manifest(marker)?;
    validate(conn, marker)?;
    let mut line = Vec::with_capacity(content.size);
    let mut hash = Sha256::new();
    for part in 0..content.parts {
        let (size, bytes) = conn.query_row(
            "SELECT length(value),substr(value,1,?3) FROM convo_chunks WHERE digest=?1 AND part=?2",
            params![content.digest, part, PART_BYTES],
            |row| Ok((row.get::<_, usize>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )?;
        anyhow::ensure!(size == bytes.len() && size == expected_part(content.size, part)?, "staged transcript chunk is incomplete or exceeds its bound");
        hash.update(&bytes);
        line.extend_from_slice(&bytes);
    }
    anyhow::ensure!(line.len() == content.size, "staged transcript has an incorrect byte size");
    let digest: [u8; 32] = hash.finalize().into();
    anyhow::ensure!(digest == content.digest, "staged transcript digest mismatch");
    Ok(String::from_utf8(line)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u06_u15_staging_resumes_partial_chunks_and_verifies_shared_content() -> anyhow::Result<()> {
        let database = Database::in_memory(until()?)?;
        let _engine=crate::convo::amphora::Amphora::from_database(database.clone())?;
        // More than a call's input budget, including UTF-8 crossing several part boundaries.
        let line = "λ".repeat((2 << 20) + 1);
        let content = Manifest { size: line.len(), parts: line.len().div_ceil(PART_BYTES), digest: Sha256::digest(line.as_bytes()).into() };
        let first = line.as_bytes().get(..PART_BYTES).ok_or_else(|| anyhow::anyhow!("missing fixture part"))?.to_vec();
        // A process stopped after one bounded staging transaction; no record can use it yet.
        database.call(until()?, move |conn| {
            reserve(conn,content,QUOTA)?;
            conn.execute("INSERT INTO convo_chunks VALUES (?1,0,?2)", params![content.digest,first])?;
            Ok(())
        })?;
        let mut pending = vec![CODEC];
        pending.extend_from_slice(&u64::try_from(content.size)?.to_le_bytes());
        pending.extend_from_slice(&content.digest);
        assert!(database.call(until()?, move |conn| validate(conn, &pending)).is_err());
        let marker = stage(&database, &line)?;
        assert_eq!(marker.len(), MARKER_BYTES);
        assert_eq!(stage(&database, &line)?, marker);
        let saved = marker.clone();
        let read = database.call(until()?, move |conn| read(conn, &saved))?;
        assert_eq!(read, line);
        let (parts, largest) = database.call(until()?, |conn| Ok(conn.query_row("SELECT count(*),max(length(value)) FROM convo_chunks", [], |row| Ok((row.get::<_,usize>(0)?,row.get::<_,usize>(1)?)))?))?;
        assert_eq!(parts, content.parts);
        assert_eq!(largest, PART_BYTES);
        // External damage with an unchanged length must fail digest validation and cannot be
        // overwritten by retrying staging: content identities always remain immutable.
        database.call(until()?, move |conn| {
            conn.execute("UPDATE convo_chunks SET value=?1 WHERE digest=?2 AND part=0", params![vec![b'x';PART_BYTES],content.digest])?;
            Ok(())
        })?;
        assert!(database.call(until()?, move |conn| super::read(conn, &marker)).is_err());
        assert!(stage(&database, &line).is_err());
        Ok(())
    }
}
