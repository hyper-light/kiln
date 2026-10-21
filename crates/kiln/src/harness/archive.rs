//! Ended agents' transcripts in kiln's store of bytes (docs/blobs.md, CP-T10.1). The harness's
//! own record of a conversation (Claude Code's transcript, Codex's rollouts) is what the
//! conversation store holds most of (docs/conversations.md, "What is kept"), and it lets an ended
//! agent's go after `[conversations] keep_closed_days`. Once a conversation closes, its record is
//! written here too, in pieces never replaced: lines `first` to `last` of a source at
//! `transcripts/<session>/<source>/<first>-<last>.jsonl.br` (brotli, a line a line), each a row of
//! `transcript_segments` (its range, codec, lines and bytes). A conversation that closes again adds
//! a piece of its lines since, so the archive never holds less than the store did. It stays, on an
//! S3 when kiln keeps its bytes on one, so a lost server disk doesn't lose it.
//!
//! The record is read through the conversation store's public reads alone (`Store::marks`,
//! `Store::record`), a page of lines at a time: the store's rows, feeds and pruning are its own. An
//! archive is due [`AFTER`] a close, so the record's last lines, still on their way from the
//! session's machine, are in; what's due is kept in kiln's own store (`transcript_archives`), so a
//! restart doesn't lose it. Until it's done, the conversation store lets go of no record of a
//! session that ended since (`Store::hold_prune`): a record goes only once it's archived. A piece
//! holds at most [`PIECE_LINES`] lines, and [`PIECE_BYTES`] bytes of them but for its last line;
//! one is read back unpacked to at most [`UNPACKED_MAX`].

use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::time::Duration;

use kiln_seams::blobs::{Cond, Put, Refusal};
use rusqlite::params;
use serde::Serialize;

use crate::hub::Hub;

/// How long after a close a conversation's record is archived.
pub const AFTER: Duration = Duration::from_secs(300);
/// How often what's due is looked for.
const EVERY: Duration = Duration::from_secs(60);
/// How long an archive that failed waits to be tried again.
const RETRY_MS: i64 = 3_600_000;
/// Archives one look takes, at most.
const BATCH: i64 = 8;
/// Lines read from the conversation store at a time.
const PAGE: usize = 2000;
/// The most lines, and bytes of them, a piece holds.
pub const PIECE_LINES: u64 = 50_000;
pub const PIECE_BYTES: u64 = 32 << 20;
/// The most a piece is unpacked to when it's read back: its lines, and a last one however long.
pub const UNPACKED_MAX: u64 = 1 << 30;
/// brotli's quality, as the conversation store keeps lines (`crate::convo`).
const QUALITY: u32 = 5;
/// The codec a piece is written in.
const CODEC: &str = "jsonl+br";
/// The conversation store notes a close a moment before kiln does: held back from that much earlier.
const HOLD_SLACK_MS: i64 = 60_000;

/// A name as it goes in a key: itself when it's a plain name, else its SHA-256.
fn plain(name: &str) -> String {
    if !name.is_empty() && name.len() <= 128 && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) && name != "." && name != ".." {
        name.to_owned()
    } else {
        crate::util::sha256_hex(name.as_bytes())
    }
}

/// Where lines `first` to `last` of a session's record from `source` are kept.
pub fn key(session: &str, source: &str, first: u64, last: u64) -> String {
    format!("transcripts/{}/{}/{first}-{last}.jsonl.br", plain(session), plain(source))
}

/// An archived piece of a record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Piece {
    pub source: String,
    pub first: u64,
    pub last: u64,
    #[serde(skip)]
    pub key: String,
    pub lines: u64,
    /// As kept: compressed.
    pub bytes: u64,
}

/// A conversation closed: its record is archived [`AFTER`] from now (noted in the background).
pub fn closed(hub: &Hub, session: &str) {
    if hub.harness.convo.is_none() {
        return;
    }
    let (store, s) = (hub.store.clone(), session.to_owned());
    crate::rt::spawn(async move { due(&store, &s).await });
}

async fn due(store: &crate::store::Store, session: &str) {
    let now = crate::util::now_ms();
    let (s, at) = (session.to_owned(), now.saturating_add(i64::try_from(AFTER.as_millis()).unwrap_or(i64::MAX)));
    let r = store
        .call(move |c| {
            c.execute(
                "INSERT INTO transcript_archives (session, closed_ms, due_ms, done_ms) VALUES (?1, ?2, ?3, NULL)
                 ON CONFLICT (session) DO UPDATE SET closed_ms = MIN(closed_ms, excluded.closed_ms), due_ms = excluded.due_ms, done_ms = NULL",
                params![s, now, at],
            )?;
            Ok(())
        })
        .await;
    if let Err(e) = r {
        tracing::warn!(target: "kiln.blobs", event = "put", session, error = %format!("{e:#}"), "a closed conversation's record won't be archived");
    }
}

/// The pieces archived of a session's record, by source and line.
pub async fn archived(hub: &Hub, session: &str) -> anyhow::Result<Vec<Piece>> {
    let s = session.to_owned();
    hub.store
        .call(move |c| {
            let mut q = c.prepare("SELECT source, first, last, key, lines, bytes FROM transcript_segments WHERE session = ?1 ORDER BY source, first")?;
            let rows = q.query_map([&s], |r| {
                let n = |i: usize| r.get::<_, i64>(i).map(|v| u64::try_from(v).unwrap_or(0));
                Ok(Piece { source: r.get(0)?, first: n(1)?, last: n(2)?, key: r.get(3)?, lines: n(4)?, bytes: n(5)? })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
}

/// Holds the conversation store's letting go of records back to the oldest close not archived yet
/// (nothing held back once every one is).
async fn hold(hub: &Hub, convo: &crate::convo::store::Store) -> anyhow::Result<()> {
    let oldest: Option<i64> = hub.store.call(|c| Ok(c.query_row("SELECT MIN(closed_ms) FROM transcript_archives WHERE done_ms IS NULL", [], |r| r.get(0))?)).await?;
    convo.hold_prune(oldest.map_or(i64::MAX, |t| t.saturating_sub(HOLD_SLACK_MS)));
    Ok(())
}

/// Archives what's due, every minute, for as long as the server runs (with a conversation store).
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    loop {
        if let Some(convo) = hub.harness.convo.clone() {
            pass(&hub, &convo).await?;
        }
        crate::rt::sleep(EVERY).await;
    }
}

/// One look: what's due archived, the hold on the conversation store moved on as far as it may.
async fn pass(hub: &Hub, convo: &crate::convo::store::Store) -> anyhow::Result<()> {
    hold(hub, convo).await?;
    let now = crate::util::now_ms();
    let due: Vec<String> = hub
        .store
        .call(move |c| {
            let mut q = c.prepare("SELECT session FROM transcript_archives WHERE done_ms IS NULL AND due_ms <= ?1 ORDER BY due_ms LIMIT ?2")?;
            let rows = q.query_map(params![now, BATCH], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await?;
    for session in due {
        match archive(hub, convo, &session).await {
            Ok(_) => {
                let s = session.clone();
                hub.store.call(move |c| Ok(c.execute("UPDATE transcript_archives SET done_ms = ?2 WHERE session = ?1", params![s, crate::util::now_ms()])?)).await?;
            }
            Err(e) => {
                tracing::warn!(target: "kiln.blobs", event = "put", session = %session, error = %format!("{e:#}"), "a closed conversation's record couldn't be archived: the conversation store keeps it, and it's tried again in an hour");
                let (s, at) = (session.clone(), crate::util::now_ms().saturating_add(RETRY_MS));
                hub.store.call(move |c| Ok(c.execute("UPDATE transcript_archives SET due_ms = ?2 WHERE session = ?1", params![s, at])?)).await?;
            }
        }
    }
    hold(hub, convo).await
}

/// Archives what's new of a session's record since its last piece, each source on from the line
/// after its last piece's: the pieces written.
pub async fn archive(hub: &Hub, convo: &crate::convo::store::Store, session: &str) -> anyhow::Result<usize> {
    let (c, s) = (convo.clone(), session.to_owned());
    let marks = crate::rt::blocking(move || c.marks(&s)).await??;
    let mut written = 0usize;
    for mark in marks {
        let (s, src) = (session.to_owned(), mark.source.clone());
        let last: Option<i64> = hub.store.call(move |c| Ok(c.query_row("SELECT MAX(last) FROM transcript_segments WHERE session = ?1 AND source = ?2", params![s, src], |r| r.get(0))?)).await?;
        let from = last.map_or(0, |l| u64::try_from(l).unwrap_or(0).saturating_add(1));
        written = written.saturating_add(pieces(hub, convo, session, &mark.source, from).await?);
    }
    Ok(written)
}

/// A piece being made: its first and last lines, and its lines compressed so far.
struct Making {
    first: u64,
    last: u64,
    lines: u64,
    raw: u64,
    enc: brotli::CompressorWriter<Vec<u8>>,
}

impl Making {
    fn new(first: u64) -> Self {
        Self { first, last: first, lines: 0, raw: 0, enc: brotli::CompressorWriter::new(Vec::new(), 1 << 16, QUALITY, 22) }
    }
}

/// Writes one source's lines from `from` on as pieces: how many.
async fn pieces(hub: &Hub, convo: &crate::convo::store::Store, session: &str, source: &str, from: u64) -> anyhow::Result<usize> {
    let (mut from, mut written) = (from, 0usize);
    let mut making: Option<Making> = None;
    loop {
        let (c, s, src) = (convo.clone(), session.to_owned(), source.to_owned());
        let page = crate::rt::blocking(move || c.record(&s, &src, from, PAGE)).await??;
        let full = page.len() >= PAGE;
        let Some(next) = page.last().map(|(n, _)| n.saturating_add(1)) else { break };
        from = next;
        // Each line into the piece being made; a full one is written and another begun.
        let mut rest = page.into_iter().peekable();
        while rest.peek().is_some() {
            let m = making.take().unwrap_or_else(|| Making::new(rest.peek().map_or(from, |(n, _)| *n)));
            let (m, more) = crate::rt::blocking(move || -> std::io::Result<(Making, Vec<(u64, String)>)> {
                let mut m = m;
                let mut more = Vec::new();
                for (n, line) in rest.by_ref() {
                    if m.lines >= PIECE_LINES || m.raw >= PIECE_BYTES {
                        more.push((n, line));
                        more.extend(rest.by_ref());
                        break;
                    }
                    m.enc.write_all(line.as_bytes())?;
                    m.enc.write_all(b"\n")?;
                    (m.last, m.lines, m.raw) = (n, m.lines.saturating_add(1), m.raw.saturating_add(line.len() as u64).saturating_add(1));
                }
                Ok((m, more))
            })
            .await??;
            if more.is_empty() {
                making = Some(m);
                break;
            }
            write_piece(hub, session, source, m).await?;
            written = written.saturating_add(1);
            rest = more.into_iter().peekable();
        }
        if !full {
            break;
        }
    }
    if let Some(m) = making.filter(|m| m.lines > 0) {
        write_piece(hub, session, source, m).await?;
        written = written.saturating_add(1);
    }
    Ok(written)
}

/// Puts a piece where nothing is (one a cut-short try put already is taken as it is) and records it.
async fn write_piece(hub: &Hub, session: &str, source: &str, m: Making) -> anyhow::Result<()> {
    let k = key(session, source, m.first, m.last);
    let Making { first, last, lines, enc, .. } = m;
    let bytes = crate::rt::blocking(move || enc.into_inner()).await?;
    let size = match hub.blobs.blobs.put(&k, bytes, Put { cond: Cond::IfNoneMatch, sha256: None }).await {
        Ok(meta) => meta.size,
        Err(Refusal::PreconditionFailed(_)) => hub.blobs.blobs.head(&k).await.map_err(|e| anyhow::anyhow!("{k}: {e}"))?.size,
        Err(e) => anyhow::bail!("{k}: {e}"),
    };
    let (s, src, kk, now) = (session.to_owned(), source.to_owned(), k.clone(), crate::util::now_ms());
    let i = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT INTO transcript_segments (session, source, first, last, key, codec, lines, bytes, written_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (session, source, first) DO NOTHING",
                params![s, src, i(first), i(last), kk, CODEC, i(lines), i(size), now],
            )?;
            Ok(())
        })
        .await?;
    tracing::info!(target: "kiln.blobs", event = "put", session, source, first, last, lines, "a piece of a closed conversation's record archived");
    Ok(())
}

/// A source's archived lines, in order, as JSON lines: each piece read and unpacked in turn.
pub fn lines(hub: &Arc<Hub>, pieces: Vec<Piece>) -> async_channel::Receiver<bytes::Bytes> {
    let (tx, rx) = async_channel::bounded(4);
    let hub = Arc::clone(hub);
    crate::rt::spawn(async move {
        for p in pieces {
            let packed = match crate::blobs::io::get_all(hub.blobs.blobs.as_ref(), &p.key, p.bytes).await {
                Ok((_, b)) => b,
                Err(e) => {
                    tracing::warn!(target: "kiln.blobs", event = "get", key = %p.key, error = %e, "an archived record ends short");
                    return;
                }
            };
            let text = crate::rt::blocking(move || -> std::io::Result<Vec<u8>> {
                let mut out = Vec::new();
                brotli::Decompressor::new(packed.as_slice(), 1 << 16).take(UNPACKED_MAX).read_to_end(&mut out)?;
                Ok(out)
            })
            .await;
            let Ok(Ok(text)) = text else {
                tracing::warn!(target: "kiln.blobs", event = "get", key = %p.key, "an archived piece couldn't be unpacked; the record ends short");
                return;
            };
            if tx.send(bytes::Bytes::from(text)).await.is_err() {
                return;
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(session: &str, lines: std::ops::Range<u64>) -> Vec<crate::convo::rows::Row> {
        lines.map(|n| crate::convo::rows::Row::Record(Box::new(crate::convo::rows::RecordRow { session: session.into(), source: "cc".into(), n, end: n + 1, line: format!("{{\"n\":{n},\"text\":\"{}\"}}", "word ".repeat(20)), kept: None }))).collect()
    }

    async fn read_back(hub: &Arc<Hub>, session: &str) -> anyhow::Result<Vec<String>> {
        let rx = lines(hub, archived(hub, session).await?);
        let mut text = Vec::new();
        while let Ok(b) = rx.recv().await {
            text.extend_from_slice(&b);
        }
        Ok(String::from_utf8(text)?.lines().map(str::to_owned).collect())
    }

    /// CP-T10.1: a closed conversation's record, read from the conversation store, is in kiln's
    /// store of bytes: every line, in order, in brotli-compressed pieces. Closed again after more
    /// lines, only those are archived, as a piece of their own; none is ever replaced, so the
    /// archive holds every line even once the store has let the first ones go.
    #[test]
    fn a_closed_conversations_record_is_archived_in_pieces_never_replaced() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let (_dir, convo) = crate::convo::store::scratch()?;
            let c = convo.clone();
            let first = PIECE_LINES + 1234;
            assert!(crate::rt::blocking(move || c.put_wait(record("s1", 0..first))).await?);
            assert_eq!(archive(&hub, &convo, "s1").await?, 2, "two pieces: one full, one the rest");
            let pieces = archived(&hub, "s1").await?;
            assert_eq!(pieces.iter().map(|p| (p.first, p.last)).collect::<Vec<_>>(), [(0, PIECE_LINES - 1), (PIECE_LINES, first - 1)]);
            let (packed, raw) = (pieces.iter().map(|p| p.bytes).sum::<u64>(), first * 120);
            assert!(packed > 0 && packed < raw / 4, "{packed} bytes packed");
            let expect: Vec<String> = record("s1", 0..first).into_iter().filter_map(|r| if let crate::convo::rows::Row::Record(r) = r { Some(r.line) } else { None }).collect();
            assert!(read_back(&hub, "s1").await? == expect);
            // Nothing new: nothing written.
            assert_eq!(archive(&hub, &convo, "s1").await?, 0);
            // It resumed and closed again: its new lines are a piece of their own.
            let c = convo.clone();
            assert!(crate::rt::blocking(move || c.put_wait(record("s1", first..first + 10))).await?);
            assert_eq!(archive(&hub, &convo, "s1").await?, 1);
            let all: Vec<String> = record("s1", 0..first + 10).into_iter().filter_map(|r| if let crate::convo::rows::Row::Record(r) = r { Some(r.line) } else { None }).collect();
            assert!(read_back(&hub, "s1").await? == all);
            // A piece a cut-short try put already is taken as it is, never replaced.
            let k = key("s2", "cc", 0, 2);
            hub.blobs.blobs.put(&k, b"kept before".to_vec(), Put::default()).await?;
            let c = convo.clone();
            assert!(crate::rt::blocking(move || c.put_wait(record("s2", 0..3))).await?);
            assert_eq!(archive(&hub, &convo, "s2").await?, 1);
            assert_eq!(hub.blobs.blobs.get(&k, None).await?.1, b"kept before".to_vec());
            Ok(())
        })
    }

    /// CP-T10.1: until a closed conversation's record is archived, the conversation store keeps the
    /// records of every session that ended since; once it is, the hold goes.
    #[test]
    fn the_conversation_store_holds_records_until_they_are_archived() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let convo = hub.harness.convo.clone().ok_or_else(|| anyhow::anyhow!("no conversation store"))?;
            let c = convo.clone();
            assert!(crate::rt::blocking(move || c.put_wait(record("s1", 0..3))).await?);
            due(&hub.store, "s1").await;
            let closed: i64 = hub.store.call(|c| Ok(c.query_row("SELECT closed_ms FROM transcript_archives WHERE session = 's1'", [], |r| r.get(0))?)).await?;
            hold(&hub, &convo).await?;
            assert_eq!(convo.held(), Some(closed - HOLD_SLACK_MS));
            hub.store.call(|c| Ok(c.execute("UPDATE transcript_archives SET due_ms = 0", [])?)).await?;
            pass(&hub, &convo).await?;
            assert_eq!(archived(&hub, "s1").await?.len(), 1);
            assert_eq!(convo.held(), None, "archived: nothing held back");
            Ok(())
        })
    }

    /// Names that aren't plain go into keys as their SHA-256.
    #[test]
    fn keys_hold_plain_names_or_their_hash() {
        assert_eq!(key("s-1", "cc", 0, 9), "transcripts/s-1/cc/0-9.jsonl.br");
        assert_eq!(key("a/b", "..", 3, 4), format!("transcripts/{}/{}/3-4.jsonl.br", crate::util::sha256_hex(b"a/b"), crate::util::sha256_hex(b"..")));
    }
}
