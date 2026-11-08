//! W29-R25: whole-source review facts, bounded before collection. Git reports raw object
//! changes; blobs are immutable bytes, including symlinks. Attributes cannot run a diff driver
//! or hide changes with textconv. Only our recorded child is terminated on overflow/timeout.

use super::*;
use dep::{CodeChange, CodeFile, Difference};

async fn bounded(cmd: std::process::Command) -> Result<Vec<u8>, Refusal> {
    let mut child = crate::proc::spawn_duplex_in_pool(cmd, None).await.map_err(|_| Refusal::Unavailable("code difference"))?;
    child.stdin.close();
    let result = crate::rt::timeout(WITHIN, async {
        let out = async {
            let mut bytes = Vec::new();
            while let Ok(chunk) = child.stdout.recv().await {
                if bytes.len().saturating_add(chunk.len()) > dep::DIFFERENCE_MAX { return Err(Refusal::TooLarge { what: "code difference", limit: dep::DIFFERENCE_MAX }); }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        };
        // Git diagnostics may contain a credential-bearing source URL. Drain and discard them
        // independently; neither retained error bytes nor a blocked pipe can defeat the bound.
        let err = async { while child.stderr.recv().await.is_ok() {} Ok::<_, Refusal>(()) };
        let (bytes, ()) = futures::try_join!(out, err)?;
        let code = (&mut child.exit).await.ok().flatten();
        Ok::<_, Refusal>((bytes, code))
    }).await;
    match result {
        Ok(Ok((bytes, Some(0)))) => Ok(bytes),
        Ok(Ok(_)) => Err(Refusal::Unavailable("code difference")),
        failure => {
            let error = match failure { Ok(Err(error)) => error, _ => Refusal::Unavailable("code difference") };
            child.terminate(Duration::ZERO);
            child.stdout.close();
            child.stderr.close();
            let _ = crate::rt::timeout(Duration::from_secs(5), &mut child.exit).await;
            Err(error)
        }
    }
}

struct Object { mode: u32, id: String }
struct Change { path: String, before: Option<Object>, after: Option<Object> }

fn object(mode: &str, id: &str) -> Result<Option<Object>, Refusal> {
    if !dep::commit_valid(id) { return Err(Refusal::Invalid("comparison object")); }
    let mode = u32::from_str_radix(mode, 8).map_err(|_| Refusal::Invalid("comparison mode"))?;
    if mode == 0 && id.bytes().all(|b| b == b'0') { return Ok(None); }
    if !matches!(mode, 0o100644 | 0o100755 | 0o120000 | 0o160000) { return Err(Refusal::Invalid("comparison mode")); }
    Ok(Some(Object { mode, id: id.into() }))
}

fn changes(raw: &[u8]) -> Result<Vec<Change>, Refusal> {
    if !raw.is_empty() && raw.last() != Some(&0) { return Err(Refusal::Invalid("comparison record")); }
    let mut parts = raw.split(|b| *b == 0);
    let mut changes = Vec::new();
    while let Some(header) = parts.next() {
        if header.is_empty() {
            if parts.next().is_some() { return Err(Refusal::Invalid("comparison record")); }
            break;
        }
        let header = std::str::from_utf8(header).map_err(|_| Refusal::Invalid("comparison record"))?;
        let fields: Vec<_> = header.strip_prefix(':').ok_or(Refusal::Invalid("comparison record"))?.split_ascii_whitespace().collect();
        let [old_mode, new_mode, old_id, new_id, status] = fields.as_slice() else { return Err(Refusal::Invalid("comparison record")) };
        if !matches!(*status, "A" | "D" | "M" | "T") { return Err(Refusal::Invalid("comparison status")); }
        let path = std::str::from_utf8(parts.next().ok_or(Refusal::Invalid("comparison path"))?).map_err(|_| Refusal::Invalid("comparison path"))?;
        if !dep::path_valid(path) { return Err(Refusal::Invalid("comparison path")); }
        if changes.len() >= dep::DIFFERENCE_FILES { return Err(Refusal::Full { what: "changed files", limit: dep::DIFFERENCE_FILES }); }
        changes.push(Change { path: path.into(), before: object(old_mode, old_id)?, after: object(new_mode, new_id)? });
    }
    Ok(changes)
}

impl<H: Deref<Target = Hub> + Send + Sync> Sources<H> {
    async fn code_file(&self, remote: &Remote, object: Option<Object>, size: &mut usize) -> Result<Option<CodeFile>, Refusal> {
        let Some(object) = object else { return Ok(None) };
        let bytes = if object.mode == 0o160000 { object.id.into_bytes() } else {
            let kind = self.bytes(remote, &["cat-file", "-t", &object.id], "code object").await?;
            if kind != b"blob\n" { return Err(Refusal::Invalid("code object")); }
            let length = self.bytes(remote, &["cat-file", "-s", &object.id], "code object").await?;
            let length = String::from_utf8_lossy(&length).trim().parse::<usize>().map_err(|_| Refusal::Invalid("code object size"))?;
            if size.saturating_add(length) > dep::DIFFERENCE_MAX { return Err(Refusal::TooLarge { what: "code difference", limit: dep::DIFFERENCE_MAX }); }
            let bytes = self.bytes(remote, &["cat-file", "blob", &object.id], "code object").await?;
            if bytes.len() != length { return Err(Refusal::Integrity("code object")); }
            bytes
        };
        *size = size.saturating_add(bytes.len());
        if *size > dep::DIFFERENCE_MAX { return Err(Refusal::TooLarge { what: "code difference", limit: dep::DIFFERENCE_MAX }); }
        Ok(Some(CodeFile { mode: object.mode, bytes }))
    }
}

pub(super) async fn compare<H: Deref<Target = Hub> + Send + Sync>(source: &Sources<H>, before: Revision, after: Revision) -> Result<Difference, Refusal> {
    let mut difference = Difference { before, after, files: Vec::new() };
    difference.validate()?;
    let remote = source.checked(&difference.before).await?;
    source.checked(&difference.after).await?;
    let _guard = actions::source_lock(&remote).map_err(|_| Refusal::Unavailable("mirror lock"))?.read().await;
    // Each captured commit belongs to its captured source. Their relationship is not an
    // approval: a trusted old capture may precede an upstream's rewritten history.
    source.bytes(&remote, &["cat-file", "-e", &format!("{}^{{commit}}", difference.before.commit)], "comparison commit").await?;
    source.bytes(&remote, &["cat-file", "-e", &format!("{}^{{commit}}", difference.after.commit)], "comparison commit").await?;
    let mut cmd = actions::source_command(&source.hub, &remote).await;
    cmd.env("GIT_NO_REPLACE_OBJECTS", "1").arg("-C").arg(remote.mirror(&source.hub)).args([
        "diff-tree", "-r", "--no-commit-id", "--raw", "--no-abbrev", "-z", "--no-renames", "--no-ext-diff", "--no-textconv", "--ignore-submodules=none",
        &difference.before.commit, &difference.after.commit, "--",
    ]);
    let raw = bounded(cmd).await?;
    let mut size = 0usize;
    for change in changes(&raw)? {
        size = size.saturating_add(change.path.len());
        let before = source.code_file(&remote, change.before, &mut size).await?;
        let after = source.code_file(&remote, change.after, &mut size).await?;
        difference.files.push(CodeChange { path: change.path, before, after });
    }
    difference.validate()?;
    Ok(difference)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w29_r25_raw_code_changes_preserve_modes_and_gitlinks_without_path_quoting() -> Result<(), Refusal> {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let zero = "0".repeat(40);
        let raw = format!(":100644 100755 {a} {a} M\0src/run script\0:000000 160000 {zero} {b} A\0modules/lib\0:120000 000000 {a} {zero} D\0old-link\0");
        let records = changes(raw.as_bytes())?;
        assert_eq!(records.len(), 3);
        assert!(records.first().is_some_and(|r| r.path == "src/run script" && r.before.as_ref().is_some_and(|o| o.mode == 0o100644) && r.after.as_ref().is_some_and(|o| o.mode == 0o100755)));
        assert!(records.get(1).is_some_and(|r| r.before.is_none() && r.after.as_ref().is_some_and(|o| o.mode == 0o160000 && o.id == b)));
        assert!(records.get(2).is_some_and(|r| r.before.as_ref().is_some_and(|o| o.mode == 0o120000) && r.after.is_none()));
        assert!(changes(format!(":100644 100644 {a} {b} M\0../outside\0").as_bytes()).is_err());
        assert!(changes(format!(":100644 100644 {a} {b} M\0truncated").as_bytes()).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn w29_r25_comparison_output_is_bounded_and_stderr_is_drained_privately() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", "head -c 5000000 /dev/zero >&2; printf review-bytes"]);
            assert_eq!(bounded(cmd).await?, b"review-bytes");
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", "head -c 4194305 /dev/zero; sleep 30"]);
            let started = std::time::Instant::now();
            assert_eq!(bounded(cmd).await, Err(Refusal::TooLarge { what: "code difference", limit: dep::DIFFERENCE_MAX }));
            assert!(started.elapsed() < Duration::from_secs(10));
            Ok(())
        })
    }
}
