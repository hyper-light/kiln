//! `kiln/cache@v1` (docs/ci.md W08-R09): the job's caches through kiln's chunked protocol
//! (`/_kiln/cache`, `crate::ci::services::chunks`). The paths are kept in one uncompressed tar,
//! each under its index (`0/…`, `1/…`: no absolute path is ever in it), cut into the store's
//! content-defined chunks, so a cache that changed by 1 % moves about 1 % of its bytes. With
//! `local`, the runner keeps the chunks it has seen in `<work>/_cache` (least recently used go
//! first past `[agent] cache_max`, 10 GiB by default): a restore downloads only those it lacks,
//! and a save uploads only those the server's copy of the repository's caches lacks.

use std::collections::HashSet;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use kiln_ci::store::cdc::{self, Cdc};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::http::client::Client;

/// The local tier's default size.
const LOCAL_MAX: u64 = 10 << 30;

/// Where a leg reaches kiln's cache, and as whom.
pub struct Service<'a> {
    pub http: &'a Client,
    /// The results service's origin.
    pub base: String,
    pub token: &'a str,
}

/// What `kiln/cache@v1` keeps: its paths, as given and as found on this machine.
#[derive(Debug, Clone)]
pub struct Paths {
    pub given: Vec<String>,
    pub resolved: Vec<PathBuf>,
}

impl Paths {
    /// `~` is the home directory; a relative path is the workspace's.
    pub fn new(given: &[String], workspace: &Path, home: Option<&Path>) -> Paths {
        let resolved = given
            .iter()
            .map(|p| match (p.strip_prefix("~/"), home) {
                (Some(rest), Some(h)) => h.join(rest),
                _ if p == "~" => home.map(Path::to_path_buf).unwrap_or_else(|| workspace.to_path_buf()),
                _ if Path::new(p).is_absolute() => PathBuf::from(p),
                _ => workspace.join(p),
            })
            .collect();
        Paths { given: given.to_vec(), resolved }
    }

    /// The cache's version: the paths it keeps, in kiln's format (never mistaken for
    /// `actions/cache`'s).
    pub fn version(&self) -> String {
        hex::encode(Sha256::digest(format!("kiln/cache v1\n{}", self.given.join("\n")).as_bytes()))
    }
}

/// One chunk of a cache's content.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub digest: String,
    pub size: u64,
    pub offset: u64,
}

/// The local tier: `<work>/_cache/<2 hex>/<2 hex>/<digest>`.
pub struct Local {
    pub dir: PathBuf,
    pub max: u64,
}

impl Local {
    /// The tier under `work`, keeping at most `max` bytes (10 GiB when `None`).
    pub fn new(work: &Path, max: Option<u64>) -> Local {
        Local { dir: work.join("_cache"), max: max.unwrap_or(LOCAL_MAX) }
    }

    fn path(&self, digest: &str) -> PathBuf {
        self.dir.join(digest.get(0..2).unwrap_or("00")).join(digest.get(2..4).unwrap_or("00")).join(digest)
    }

    /// A chunk it holds, touched (it's the newest used now).
    fn get(&self, digest: &str) -> Option<Vec<u8>> {
        let p = self.path(digest);
        let data = std::fs::read(&p).ok()?;
        (hex::encode(Sha256::digest(&data)) == digest).then(|| {
            if let Ok(f) = std::fs::File::options().write(true).open(&p) {
                let _ = f.set_modified(std::time::SystemTime::now());
            }
            data
        })
    }

    fn put(&self, digest: &str, data: &[u8]) -> std::io::Result<()> {
        let p = self.path(digest);
        if p.is_file() {
            return Ok(());
        }
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension(format!("{}.tmp", crate::util::new_id()));
        std::fs::write(&tmp, data)?;
        std::fs::rename(tmp, p)
    }

    /// Past its size, the least recently used chunks go.
    pub fn trim(&self) {
        let mut all: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
        for a in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            for b in std::fs::read_dir(a.path()).into_iter().flatten().flatten() {
                for c in std::fs::read_dir(b.path()).into_iter().flatten().flatten() {
                    if let Ok(m) = c.metadata() {
                        all.push((m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), c.path()));
                    }
                }
            }
        }
        let mut total: u64 = all.iter().map(|(_, n, _)| n).sum();
        all.sort();
        for (_, n, p) in all {
            if total <= self.max {
                break;
            }
            if std::fs::remove_file(&p).is_ok() {
                total = total.saturating_sub(n);
            }
        }
    }
}

/// `10G` (or `10GB`, `10GiB`), `512M`, `1048576`.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let s = s.strip_suffix("iB").or_else(|| s.strip_suffix("ib")).or_else(|| s.strip_suffix(['B', 'b'])).unwrap_or(s);
    let (n, mult) = match s.char_indices().last()? {
        (i, 'G' | 'g') => (s.get(..i)?, 1u64 << 30),
        (i, 'M' | 'm') => (s.get(..i)?, 1 << 20),
        (i, 'K' | 'k') => (s.get(..i)?, 1 << 10),
        _ => (s, 1),
    };
    n.trim().parse::<u64>().ok()?.checked_mul(mult)
}

/// The paths in one tar, each under its index; those that don't exist are left out.
pub fn archive(paths: &Paths, out: &Path) -> anyhow::Result<usize> {
    let mut b = tar::Builder::new(std::fs::File::create(out)?);
    b.follow_symlinks(false);
    let mut kept = 0;
    for (i, p) in paths.resolved.iter().enumerate() {
        if p.is_dir() {
            append_sorted(&mut b, Path::new(&i.to_string()), p)?;
            kept += 1;
        } else if p.is_file() {
            b.append_path_with_name(p, i.to_string())?;
            kept += 1;
        }
    }
    b.finish()?;
    Ok(kept)
}

/// `dir` into the tar as `name`, its entries in name order: the same files make the same archive,
/// and so the same chunks, whatever order the file system lists them in. Files, directories and
/// links are kept; sockets and devices aren't.
fn append_sorted(b: &mut tar::Builder<std::fs::File>, name: &Path, dir: &Path) -> anyhow::Result<()> {
    b.append_dir(name, dir)?;
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for e in entries {
        let (from, to, kind) = (e.path(), name.join(e.file_name()), e.file_type()?);
        if kind.is_dir() {
            append_sorted(b, &to, &from)?;
        } else if kind.is_file() || kind.is_symlink() {
            b.append_path_with_name(&from, &to)?;
        }
    }
    Ok(())
}

/// A tar back where its paths were named: unpacked in `staging` first (nothing in it can land
/// outside), then each index moved into place over what's there.
pub fn unpack(archive: &Path, paths: &Paths, staging: &Path) -> anyhow::Result<()> {
    let _ = std::fs::remove_dir_all(staging);
    std::fs::create_dir_all(staging)?;
    tar::Archive::new(std::fs::File::open(archive)?).unpack(staging)?;
    for (i, to) in paths.resolved.iter().enumerate() {
        let from = staging.join(i.to_string());
        if from.symlink_metadata().is_ok() {
            place(&from, to)?;
        }
    }
    let _ = std::fs::remove_dir_all(staging);
    Ok(())
}

/// `from` at `to`: renamed when it can be, else copied; a directory merged into one that's there.
fn place(from: &Path, to: &Path) -> anyhow::Result<()> {
    let meta = from.symlink_metadata()?;
    if meta.is_dir() && to.is_dir() {
        for e in std::fs::read_dir(from)? {
            let e = e?;
            place(&e.path(), &to.join(e.file_name()))?;
        }
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if to.symlink_metadata().is_ok() && !to.is_dir() {
        std::fs::remove_file(to)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    // Another file system: copied.
    if meta.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            place(&e.path(), &to.join(e.file_name()))?;
        }
    } else if meta.file_type().is_symlink() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(std::fs::read_link(from)?, to)?;
    } else {
        std::fs::copy(from, to)?;
    }
    Ok(())
}

/// A file's content-defined chunks (the store's cuts), and its SHA-256; each kept in `local`.
pub fn chunks(path: &Path, local: Option<&Local>) -> anyhow::Result<(String, u64, Vec<Chunk>)> {
    let mut f = std::fs::File::open(path)?;
    let mut whole = Sha256::new();
    let mut cut = Cdc::new();
    let mut pending: Vec<u8> = Vec::with_capacity(cdc::MAX * 2);
    let (mut start, mut eof, mut size, mut offset) = (0usize, false, 0u64, 0u64);
    let mut buf = vec![0u8; 4 << 20];
    let mut out = Vec::new();
    loop {
        while !eof && pending.len() - start < cdc::MAX {
            if start > 0 {
                pending.drain(..start);
                start = 0;
            }
            let n = f.read(&mut buf)?;
            if n == 0 {
                eof = true;
            } else {
                let got = buf.get(..n).unwrap_or_default();
                whole.update(got);
                pending.extend_from_slice(got);
                size += n as u64;
            }
        }
        let rest = pending.get(start..).unwrap_or_default();
        match cut.next(rest, eof) {
            Some(n) if n > 0 => {
                let data = rest.get(..n).unwrap_or_default();
                let digest = hex::encode(Sha256::digest(data));
                if let Some(l) = local {
                    l.put(&digest, data)?;
                }
                out.push(Chunk { digest, size: n as u64, offset });
                offset += n as u64;
                start += n;
            }
            _ => break,
        }
    }
    Ok((hex::encode(whole.finalize()), size, out))
}

impl Service<'_> {
    async fn post(&self, what: &str, body: &Value) -> anyhow::Result<(u16, Value)> {
        let url = format!("{}/_kiln/cache/{what}", self.base.trim_end_matches('/'));
        let resp = self.http.post(&url).bearer(self.token).header("content-type", "application/json").body(serde_json::to_vec(body)?).timeout(Duration::from_secs(300)).send().await?;
        let status = resp.status();
        let v: Value = if status == 204 { Value::Null } else { resp.json(64 << 20).await.unwrap_or_default() };
        Ok((status, v))
    }
}

/// What a restore found.
pub struct Found {
    pub key: String,
    pub downloaded: u64,
    pub local_bytes: u64,
}

/// The first entry `keys` name (the first key, then each restore key, as GitHub looks), its
/// chunks from the local tier or the server, into `archive`, checked whole. `None` on a miss.
pub async fn restore(svc: &Service<'_>, keys: &[String], version: &str, scope: &str, local: Option<&Local>, archive: &Path, look_only: bool) -> anyhow::Result<Option<Found>> {
    let (status, v) = svc.post("lookup", &json!({ "keys": keys, "version": version, "scope": scope })).await?;
    if status == 204 {
        return Ok(None);
    }
    anyhow::ensure!((200..300).contains(&status), "{}", v.get("message").and_then(Value::as_str).unwrap_or("the cache lookup failed"));
    let id = v.get("id").and_then(Value::as_i64).unwrap_or_default();
    let key = v.get("key").and_then(Value::as_str).unwrap_or_default().to_owned();
    if look_only {
        return Ok(Some(Found { key, downloaded: 0, local_bytes: 0 }));
    }
    let want = v.pointer("/object/digest").and_then(Value::as_str).unwrap_or_default().to_owned();
    let list: Vec<(String, u64)> = v.pointer("/object/chunks").cloned().map(serde_json::from_value).transpose()?.unwrap_or_default();
    let mut f = std::fs::File::create(archive)?;
    let mut whole = Sha256::new();
    let (mut downloaded, mut local_bytes) = (0u64, 0u64);
    for (digest, size) in &list {
        let data = match local.and_then(|l| l.get(digest)) {
            Some(d) => {
                local_bytes += d.len() as u64;
                d
            }
            None => {
                let url = format!("{}/_kiln/cache/entries/{id}/chunks/{digest}", svc.base.trim_end_matches('/'));
                let resp = svc.http.get(&url).bearer(svc.token).timeout(Duration::from_secs(600)).send().await?;
                anyhow::ensure!(resp.is_success(), "chunk {digest}: HTTP {}", resp.status());
                let d = resp.bytes(cdc::MAX + 1).await?.to_vec();
                anyhow::ensure!(hex::encode(Sha256::digest(&d)) == *digest, "chunk {digest} arrived corrupted");
                if let Some(l) = local {
                    l.put(digest, &d)?;
                }
                downloaded += d.len() as u64;
                d
            }
        };
        anyhow::ensure!(data.len() as u64 == *size, "chunk {digest} is {} bytes, not {size}", data.len());
        whole.update(&data);
        f.write_all(&data)?;
    }
    f.flush()?;
    let got = hex::encode(whole.finalize());
    anyhow::ensure!(got == want, "the cache's content hashes to sha256:{got}, not sha256:{want}");
    Ok(Some(Found { key, downloaded, local_bytes }))
}

/// What a save sent.
pub struct Saved {
    pub chunks: usize,
    pub uploaded: u64,
    pub size: u64,
}

/// `archive` as the entry `key`: its chunks cut, those the repository's caches lack uploaded
/// (from the local tier or the archive itself), then the entry committed. `None` when another
/// save of the key got there first.
pub async fn save(svc: &Service<'_>, key: &str, version: &str, scope: &str, local: Option<&Local>, archive: &Path) -> anyhow::Result<Option<Saved>> {
    let a = archive.to_path_buf();
    let keep = local.map(|l| Local { dir: l.dir.clone(), max: l.max });
    let (digest, size, list) = crate::rt::blocking(move || chunks(&a, keep.as_ref())).await??;
    let digests: Vec<&String> = list.iter().map(|c| &c.digest).collect();
    let (status, v) = svc.post("missing", &json!({ "chunks": digests, "scope": scope })).await?;
    anyhow::ensure!((200..300).contains(&status), "{}", v.get("message").and_then(Value::as_str).unwrap_or("the cache refused the save"));
    let missing: HashSet<String> = v.get("missing").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    let mut f = std::fs::File::open(archive)?;
    let mut uploaded = 0u64;
    let mut sent: HashSet<&str> = HashSet::new();
    for c in list.iter().filter(|c| missing.contains(&c.digest)) {
        if !sent.insert(&c.digest) {
            continue;
        }
        let data = match local.and_then(|l| l.get(&c.digest)) {
            Some(d) => d,
            None => {
                let mut d = vec![0u8; usize::try_from(c.size).unwrap_or(0)];
                f.seek(SeekFrom::Start(c.offset))?;
                f.read_exact(&mut d)?;
                d
            }
        };
        let url = format!("{}/_kiln/cache/chunks/{}", svc.base.trim_end_matches('/'), c.digest);
        let resp = svc.http.put(&url).bearer(svc.token).body(data).timeout(Duration::from_secs(600)).send().await?;
        anyhow::ensure!(resp.is_success(), "chunk {}: HTTP {}", c.digest, resp.status());
        uploaded += c.size;
    }
    let pairs: Vec<(String, u64)> = list.iter().map(|c| (c.digest.clone(), c.size)).collect();
    let (status, v) = svc.post("entries", &json!({ "key": key, "version": version, "scope": scope, "object": { "digest": digest, "size": size, "chunks": pairs } })).await?;
    if status == 409 {
        return Ok(None);
    }
    anyhow::ensure!((200..300).contains(&status), "{}", v.get("message").and_then(Value::as_str).unwrap_or("the cache refused the save"));
    Ok(Some(Saved { chunks: list.len(), uploaded, size }))
}

/// Where this runner keeps one of a job's v1 `cache:` folders (docs/ci.md W02-R06, W24-R11):
/// `<work>/_kept/<repository>/<job>/<folder>`, each name hashed.
pub fn kept_at(work: &Path, repo: &str, job: &str, dir: &str) -> PathBuf {
    let h = |s: &str| -> String { crate::util::sha256_hex(s.as_bytes()).chars().take(16).collect() };
    work.join("_kept").join(h(repo)).join(h(job)).join(h(dir))
}

/// Puts a kept folder in place for a leg. A trusted leg's is what the job's last leg on this
/// runner kept, moved rather than copied; an untrusted leg's starts empty. Says which.
pub fn put_kept(kept: &Path, at: &Path, trusted: bool) -> anyhow::Result<&'static str> {
    use anyhow::Context as _;
    if let Some(parent) = at.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if trusted && kept.is_dir() {
        if std::fs::symlink_metadata(at).is_ok() {
            remove_any(at).with_context(|| format!("clear {}", at.display()))?;
        }
        std::fs::rename(kept, at).with_context(|| format!("move {} into place", kept.display()))?;
        return Ok("restored from this runner");
    }
    std::fs::create_dir_all(at).with_context(|| format!("create {}", at.display()))?;
    Ok(if trusted { "empty: nothing kept here yet" } else { "empty: an untrusted leg gets no kept folders" })
}

/// Keeps a trusted leg's folder for the job's next leg on this runner, in place of what was kept.
/// `false` when the leg left no folder there.
pub fn keep(at: &Path, kept: &Path) -> anyhow::Result<bool> {
    use anyhow::Context as _;
    if !std::fs::symlink_metadata(at).is_ok_and(|m| m.is_dir()) {
        return Ok(false);
    }
    if let Some(parent) = kept.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if std::fs::symlink_metadata(kept).is_ok() {
        remove_any(kept).with_context(|| format!("clear {}", kept.display()))?;
    }
    std::fs::rename(at, kept).with_context(|| format!("keep {}", at.display()))?;
    Ok(true)
}

fn remove_any(p: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(p)?.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w24_r11_v1_dirs_are_kept_for_trusted_legs_only() -> anyhow::Result<()> {
        let d = tempfile::tempdir()?;
        let kept = kept_at(d.path(), "acme/app", "build", "target");
        let (one, two, three) = (d.path().join("w/1/target"), d.path().join("w/2/target"), d.path().join("w/3/target"));
        // The job's first leg starts empty and writes; its folder is kept after its steps.
        assert_eq!(put_kept(&kept, &one, true)?, "empty: nothing kept here yet");
        std::fs::write(one.join("f"), "first")?;
        assert!(keep(&one, &kept)?);
        // The job's next trusted leg reads what the first wrote, and keeps it again.
        assert_eq!(put_kept(&kept, &two, true)?, "restored from this runner");
        assert_eq!(std::fs::read_to_string(two.join("f"))?, "first");
        assert!(keep(&two, &kept)?);
        // An untrusted leg sees none of it, and what it writes never reaches the kept folder.
        assert_eq!(put_kept(&kept, &three, false)?, "empty: an untrusted leg gets no kept folders");
        assert!(!three.join("f").exists());
        std::fs::write(three.join("poison"), "x")?;
        assert!(kept.join("f").exists() && !kept.join("poison").exists());
        // Each repository, job and folder has its own.
        assert_ne!(kept, kept_at(d.path(), "acme/other", "build", "target"));
        assert_ne!(kept, kept_at(d.path(), "acme/app", "test", "target"));
        assert_ne!(kept, kept_at(d.path(), "acme/app", "build", "node_modules"));
        Ok(())
    }

    #[test]
    fn w08_r09_archive_round_trip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let ws = dir.path().join("ws");
        let home = dir.path().join("home");
        std::fs::create_dir_all(ws.join("target/debug"))?;
        std::fs::create_dir_all(home.join(".cargo/registry"))?;
        std::fs::write(ws.join("target/debug/app"), "binary")?;
        std::fs::write(home.join(".cargo/registry/index"), "crates")?;
        std::fs::write(ws.join("lock.txt"), "one file")?;
        let paths = Paths::new(&["target".into(), "~/.cargo/registry".into(), "lock.txt".into(), "missing".into()], &ws, Some(&home));
        for name in ["zeta", "alpha", "mid"] {
            std::fs::write(ws.join("target").join(name), name)?;
        }
        let tar = dir.path().join("c.tar");
        assert_eq!(archive(&paths, &tar)?, 3, "what doesn't exist is left out");
        // Each directory's entries in name order, whatever order the file system keeps.
        let names: Vec<String> = tar::Archive::new(std::fs::File::open(&tar)?).entries()?.filter_map(|e| e.ok()?.path().ok().map(|p| p.to_string_lossy().trim_end_matches('/').to_owned())).filter(|n| n.starts_with("0/")).collect();
        assert_eq!(names, ["0/alpha", "0/debug", "0/debug/app", "0/mid", "0/zeta"], "{names:?}");
        // Gone, then back where each was.
        std::fs::remove_dir_all(ws.join("target"))?;
        std::fs::remove_dir_all(home.join(".cargo"))?;
        std::fs::remove_file(ws.join("lock.txt"))?;
        unpack(&tar, &paths, &dir.path().join("staging"))?;
        assert_eq!(std::fs::read_to_string(ws.join("target/debug/app"))?, "binary");
        assert_eq!(std::fs::read_to_string(home.join(".cargo/registry/index"))?, "crates");
        assert_eq!(std::fs::read_to_string(ws.join("lock.txt"))?, "one file");
        // The version names the paths, and isn't actions/cache's.
        assert_ne!(paths.version(), Paths::new(&["target".into()], &ws, None).version());
        Ok(())
    }

    #[test]
    fn w08_r09_chunks_and_the_local_tier() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let f = dir.path().join("blob");
        // 6 MiB of noise: several chunks, the same cuts as the store's.
        let data: Vec<u8> = (0..6u32 << 20).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        std::fs::write(&f, &data)?;
        let local = Local { dir: dir.path().join("_cache"), max: 1 << 30 };
        let (digest, size, list) = chunks(&f, Some(&local))?;
        assert_eq!((digest, size), (hex::encode(Sha256::digest(&data)), data.len() as u64));
        let cuts: Vec<u64> = cdc::cuts(&data).into_iter().map(|n| n as u64).collect();
        assert_eq!(list.iter().map(|c| c.size).collect::<Vec<_>>(), cuts);
        assert!(list.windows(2).all(|w| w.get(1).map(|b| b.offset) == w.first().map(|a| a.offset + a.size)));
        assert!(list.iter().all(|c| local.get(&c.digest).is_some()), "kept in the local tier");
        // Past its size, the oldest go.
        let small = Local { dir: local.dir.clone(), max: 1 };
        small.trim();
        assert!(list.iter().all(|c| local.get(&c.digest).is_none()));
        assert_eq!((parse_size("10G"), parse_size("512M"), parse_size("42")), (Some(10 << 30), Some(512 << 20), Some(42)));
        assert_eq!((parse_size("20GB"), parse_size("20GiB"), parse_size("1 k"), parse_size("lots")), (Some(20 << 30), Some(20 << 30), Some(1 << 10), None));
        Ok(())
    }
}
