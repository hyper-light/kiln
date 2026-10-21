//! Ended agents' transcripts in kiln's store of bytes (docs/blobs.md, CP-T10.1). The harness's
//! own record of a conversation (Claude Code's transcript, Codex's rollouts) is what the
//! conversation store holds most of (docs/conversations.md, "What is kept"), and it lets an ended
//! agent's go after `[conversations] keep_closed_days`. Once a conversation closes, its record is
//! written here as well, `transcripts/<session>/<source>.jsonl.br` (brotli, a line a line), where
//! it stays, on an S3 when kiln keeps its bytes on one, so a lost server disk doesn't lose it.
//!
//! The record is read through the conversation store's public reads alone (`Store::marks`,
//! `Store::record`), a page of lines at a time: the store's rows, feeds and pruning are its own.
//! An archive is due [`AFTER`] a close, so the record's last lines, still on their way from the
//! session's machine, are in; what's due is kept in kiln's own store (`transcript_archives`), so a
//! restart doesn't lose it. Each archive holds at most [`LINES_MAX`] lines of a source, and
//! [`BYTES_MAX`] bytes of them.

use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{OptionalExtension as _, params};

use crate::hub::Hub;

/// How long after a close a conversation's record is archived.
pub const AFTER: Duration = Duration::from_secs(300);
/// How often what's due is looked for.
const EVERY: Duration = Duration::from_secs(60);
/// Archives one look takes, at most.
const BATCH: i64 = 8;
/// Lines read from the conversation store at a time.
const PAGE: usize = 2000;
/// The most lines, and bytes of them, an archive of one source holds.
pub const LINES_MAX: u64 = 10_000_000;
pub const BYTES_MAX: u64 = 8 << 30;
/// brotli's quality, as the conversation store keeps lines (`crate::convo`).
const QUALITY: u32 = 5;

/// A name as it goes in a key: itself when it's a plain name, else its SHA-256.
fn plain(name: &str) -> String {
    if !name.is_empty() && name.len() <= 128 && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) && name != "." && name != ".." {
        name.to_owned()
    } else {
        crate::util::sha256_hex(name.as_bytes())
    }
}

/// Where a session's archived record from `source` is kept.
pub fn key(session: &str, source: &str) -> String {
    format!("transcripts/{}/{}.jsonl.br", plain(session), plain(source))
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
    let (s, at) = (session.to_owned(), crate::util::now_ms().saturating_add(i64::try_from(AFTER.as_millis()).unwrap_or(i64::MAX)));
    let r = store
        .call(move |c| {
            c.execute(
                "INSERT INTO transcript_archives (session, due_ms, done_ms, sources) VALUES (?1, ?2, NULL, '[]')
                 ON CONFLICT (session) DO UPDATE SET due_ms = excluded.due_ms, done_ms = NULL",
                params![s, at],
            )?;
            Ok(())
        })
        .await;
    if let Err(e) = r {
        tracing::warn!(target: "kiln.blobs", event = "put", session, error = %format!("{e:#}"), "a closed conversation's record won't be archived");
    }
}

/// The sources archived of a session (none: not archived, or not yet).
pub async fn archived(hub: &Hub, session: &str) -> anyhow::Result<Vec<String>> {
    let s = session.to_owned();
    let list: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT sources FROM transcript_archives WHERE session = ?1 AND done_ms IS NOT NULL", [&s], |r| r.get(0)).optional()?)).await?;
    Ok(list.and_then(|l| serde_json::from_str(&l).ok()).unwrap_or_default())
}

/// Archives what's due, every minute, for as long as the server runs (with a conversation store).
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    loop {
        crate::rt::sleep(EVERY).await;
        let Some(convo) = hub.harness.convo.clone() else { continue };
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
            match archive(&hub, &convo, &session).await {
                Ok(sources) => {
                    let (s, list) = (session.clone(), serde_json::to_string(&sources)?);
                    hub.store.call(move |c| Ok(c.execute("UPDATE transcript_archives SET done_ms = ?2, sources = ?3 WHERE session = ?1", params![s, crate::util::now_ms(), list])?)).await?;
                }
                Err(e) => {
                    // Tried again at the next look, an hour on.
                    tracing::warn!(target: "kiln.blobs", event = "put", session = %session, error = %format!("{e:#}"), "a closed conversation's record couldn't be archived");
                    let (s, at) = (session.clone(), crate::util::now_ms().saturating_add(3_600_000));
                    hub.store.call(move |c| Ok(c.execute("UPDATE transcript_archives SET due_ms = ?2 WHERE session = ?1", params![s, at])?)).await?;
                }
            }
        }
    }
}

/// Writes each source of a session's record to the store: the sources archived.
pub async fn archive(hub: &Hub, convo: &crate::convo::store::Store, session: &str) -> anyhow::Result<Vec<String>> {
    let (c, s) = (convo.clone(), session.to_owned());
    let marks = crate::rt::blocking(move || c.marks(&s)).await??;
    let mut sources = Vec::new();
    for mark in marks {
        let k = key(session, &mark.source);
        let mut writer = crate::blobs::io::Writer::new(hub.blobs.blobs.as_ref(), &k);
        let written = copy(convo, session, &mark.source, &mut writer).await;
        match written {
            Ok(lines) => {
                writer.finish().await.map_err(|e| anyhow::anyhow!("{k}: {e}"))?;
                tracing::info!(target: "kiln.blobs", event = "put", session, source = %mark.source, lines, "a closed conversation's record archived");
                sources.push(mark.source);
            }
            Err(e) => {
                writer.abort().await;
                return Err(e.context(format!("archiving {k}")));
            }
        }
    }
    Ok(sources)
}

/// Copies one source's lines into `writer`, brotli-compressed a page at a time: how many.
async fn copy(convo: &crate::convo::store::Store, session: &str, source: &str, writer: &mut crate::blobs::io::Writer<'_>) -> anyhow::Result<u64> {
    let mut enc = Some(brotli::CompressorWriter::new(Vec::new(), 1 << 16, QUALITY, 22));
    let (mut from, mut lines, mut bytes) = (0u64, 0u64, 0u64);
    loop {
        let (c, s, src) = (convo.clone(), session.to_owned(), source.to_owned());
        let page = crate::rt::blocking(move || c.record(&s, &src, from, PAGE)).await??;
        let Some(last) = page.last().map(|(n, _)| *n) else { break };
        let full = page.len() >= PAGE;
        lines = lines.saturating_add(page.len() as u64);
        bytes = page.iter().fold(bytes, |b, (_, l)| b.saturating_add(l.len() as u64).saturating_add(1));
        anyhow::ensure!(lines <= LINES_MAX && bytes <= BYTES_MAX, "the record is over {LINES_MAX} lines or {} GiB", BYTES_MAX >> 30);
        let mut e = enc.take().ok_or_else(|| anyhow::anyhow!("the compressor is gone"))?;
        let (e, out) = crate::rt::blocking(move || -> std::io::Result<_> {
            for (_, line) in &page {
                e.write_all(line.as_bytes())?;
                e.write_all(b"\n")?;
            }
            e.flush()?;
            let mut e = e;
            let out = std::mem::take(e.get_mut());
            Ok((e, out))
        })
        .await??;
        enc = Some(e);
        writer.write(&out).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        from = last.saturating_add(1);
        if !full {
            break;
        }
    }
    let e = enc.ok_or_else(|| anyhow::anyhow!("the compressor is gone"))?;
    let rest = crate::rt::blocking(move || e.into_inner()).await?;
    writer.write(&rest).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    /// CP-T10.1: a closed conversation's record, read from the conversation store, is in kiln's
    /// store of bytes: every line, in order, brotli-compressed.
    #[test]
    fn a_closed_conversations_record_is_archived_whole() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let (_dir, convo) = crate::convo::store::scratch()?;
            let lines: Vec<String> = (0..5000u64).map(|n| format!("{{\"n\":{n},\"text\":\"{}\"}}", "word ".repeat(20))).collect();
            let rows = lines.iter().enumerate().map(|(n, l)| crate::convo::rows::Row::Record(Box::new(crate::convo::rows::RecordRow { session: "s1".into(), source: "cc".into(), n: n as u64, end: n as u64 + 1, line: l.clone(), kept: None }))).collect();
            let c = convo.clone();
            assert!(crate::rt::blocking(move || c.put_wait(rows)).await?);
            let sources = archive(&hub, &convo, "s1").await?;
            assert_eq!(sources, ["cc"]);
            let (meta, packed) = hub.blobs.blobs.get(&key("s1", "cc"), None).await?;
            assert!(meta.size > 0 && (meta.size as usize) < lines.iter().map(String::len).sum::<usize>() / 4, "{} bytes packed", meta.size);
            let mut text = String::new();
            brotli::Decompressor::new(packed.as_slice(), 4096).read_to_string(&mut text)?;
            assert_eq!(text.lines().map(str::to_owned).collect::<Vec<_>>(), lines);
            Ok(())
        })
    }

    /// Names that aren't plain go into keys as their SHA-256.
    #[test]
    fn keys_hold_plain_names_or_their_hash() {
        assert_eq!(key("s-1", "cc"), "transcripts/s-1/cc.jsonl.br");
        assert_eq!(key("a/b", ".."), format!("transcripts/{}/{}.jsonl.br", crate::util::sha256_hex(b"a/b"), crate::util::sha256_hex(b"..")));
    }
}
