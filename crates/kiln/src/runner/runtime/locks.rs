//! W29-R24: verify cache and download bytes against the accepted snapshot, every time.

use super::*;
use std::io::Read as _;

const ARCHIVE_MAX: usize = 1 << 30;

fn verified(bytes: &[u8], archive: &kiln_ci::lock::kiln::Archive) -> anyhow::Result<()> {
    anyhow::ensure!(bytes.len() < ARCHIVE_MAX, "KCI1022 {} is over 1 GiB", archive.file);
    let got = crate::util::sha256_hex(bytes);
    anyhow::ensure!(got == archive.sha256.to_ascii_lowercase(), "KCI4051 {}: SHA-256 {got}, locked as {}", archive.file, archive.sha256);
    Ok(())
}

/// Only Node's regular executable is needed. Do not unpack archive links or let an existing
/// extracted binary stand in for the archive's checked bytes.
fn executable(bytes: &[u8], archive: &kiln_ci::lock::kiln::Archive, dir: &Path) -> anyhow::Result<PathBuf> {
    verified(bytes, archive)?;
    let temporary = dir.with_extension("partial");
    let _ = std::fs::remove_dir_all(&temporary);
    std::fs::create_dir_all(temporary.join("bin"))?;
    let bin = temporary.join("bin/node");
    let mut found = false;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(bytes).take(ARCHIVE_MAX as u64));
    for (count, entry) in tar.entries()?.enumerate() {
        anyhow::ensure!(count < 100_000, "KCI1022 {} has too many archive entries", archive.file);
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let rest: PathBuf = match path.components().next() {
            Some(std::path::Component::Normal(top)) if top.to_string_lossy().starts_with("node-v") => path.components().skip(1).collect(),
            _ => path,
        };
        if rest != Path::new("bin/node") { continue; }
        anyhow::ensure!(!found && entry.header().entry_type().is_file() && entry.size() < ARCHIVE_MAX as u64, "KCI4051 {} has an invalid Node executable", archive.file);
        entry.unpack(&bin)?;
        found = true;
    }
    anyhow::ensure!(found, "KCI4051 {} has no regular bin/node", archive.file);
    let _ = std::fs::remove_dir_all(dir);
    std::fs::rename(&temporary, dir)?;
    Ok(dir.join("bin/node"))
}

impl Leg {
    pub(super) async fn locked_dockerfile(&self, uses: &str, location: &Path) -> anyhow::Result<(PathBuf, PathBuf, String)> {
        let locks = self.a.dependency_locks.as_ref().ok_or_else(|| anyhow::anyhow!("KCI4050 no Dockerfile snapshot"))?;
        locks.validate().map_err(anyhow::Error::msg)?;
        let locked = locks.dockerfiles.get(uses).ok_or_else(|| anyhow::anyhow!("KCI4050 {uses}: Dockerfile is missing from this leg's snapshot"))?.clone();
        let (root, work) = (location.to_path_buf(), self.work_dir.clone());
        crate::rt::blocking(move || materialize(&root, &work, &locked)).await?
    }

    pub(super) async fn locked_node(&self, using: &str, musl: bool) -> anyhow::Result<PathBuf> {
        let locks = self.a.dependency_locks.as_ref().ok_or_else(|| anyhow::anyhow!("KCI4050 the leg has no runtime snapshot"))?;
        locks.validate().map_err(anyhow::Error::msg)?;
        let os = std::env::consts::OS;
        let arch = match std::env::consts::ARCH { "x86_64" => "amd64", "aarch64" => "arm64", "arm" => "arm", other => other };
        let platform = format!("{os}/{arch}{}", if musl { "/musl" } else { "" });
        let archive = locks.runtime(using, &platform).map_err(anyhow::Error::msg)?.clone();
        let _guard = NODE_LOCK.turn().await;
        let cache = self.work_dir.join("_externals/_locked_archives").join(format!("{}.tar.gz", archive.sha256.to_ascii_lowercase()));
        let path = cache.clone();
        let cached = crate::rt::blocking(move || -> std::io::Result<Vec<u8>> {
            let file = std::fs::File::open(path)?;
            if file.metadata()?.len() >= ARCHIVE_MAX as u64 { return Err(std::io::Error::other("KCI1022 cached Node archive is over 1 GiB")); }
            let mut bytes = Vec::new(); file.take(ARCHIVE_MAX as u64).read_to_end(&mut bytes)?; Ok(bytes)
        }).await?;
        let bytes = match cached {
            Ok(bytes) => { verified(&bytes, &archive)?; bytes }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut response = crate::updates::get_following(&self.http, &archive.url, Duration::from_secs(600)).await?;
                anyhow::ensure!(response.is_success(), "KCI4051 downloading {}: HTTP {}", archive.file, response.status());
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await? {
                    anyhow::ensure!(bytes.len().saturating_add(chunk.len()) < ARCHIVE_MAX, "KCI1022 {} is over 1 GiB", archive.file);
                    bytes.extend_from_slice(&chunk);
                }
                verified(&bytes, &archive)?;
                let (path, copy) = (cache.clone(), bytes.clone());
                crate::rt::blocking(move || -> anyhow::Result<()> {
                    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
                    let temporary = path.with_extension("partial");
                    std::fs::write(&temporary, copy)?;
                    std::fs::rename(temporary, path)?;
                    Ok(())
                }).await??;
                bytes
            }
            Err(e) => return Err(e.into()),
        };
        let job = crate::util::sha256_hex(self.a.job_id.as_bytes());
        let dir = self.work_dir.join("_externals/_locked").join(job).join(&archive.sha256);
        crate::rt::blocking(move || executable(&bytes, &archive, &dir)).await?
    }
}

fn materialize(root: &Path, work: &Path, locked: &kiln_ci::lock::execution::Dockerfile) -> anyhow::Result<(PathBuf, PathBuf, String)> {
    let source = root.join(&locked.path);
    let file = std::fs::File::open(&source)?;
    anyhow::ensure!(file.metadata()?.len() <= 1 << 20, "KCI1022 action Dockerfile exceeds 1 MiB");
    let mut bytes = Vec::new(); file.take((1 << 20) + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(crate::util::sha256_hex(&bytes) == locked.source_sha256, "KCI4051 action Dockerfile differs from its captured source");
    let key = crate::util::sha256_hex(format!("{}:{}:{}", root.display(), locked.source_sha256, locked.locked).as_bytes());
    let dir = work.join("_locked_dockerfiles"); std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{key}.Dockerfile"));
    std::fs::write(&path, &locked.locked)?;
    let context = source.parent().ok_or_else(|| anyhow::anyhow!("KCI1022 action Dockerfile has no build context"))?.to_path_buf();
    Ok((path, context, format!("kiln-action/locked:{key}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive() -> anyhow::Result<(Vec<u8>, kiln_ci::lock::kiln::Archive)> {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()));
        let body = b"accepted Node bytes";
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64); header.set_mode(0o755); header.set_cksum();
        tar.append_data(&mut header, "node-v20.19.5-linux-x64/bin/node", body.as_slice())?;
        let bytes = tar.into_inner()?.finish()?;
        let archive = kiln_ci::lock::kiln::Archive { version: "v20.19.5".into(), url: "https://nodejs.org/dist/v20.19.5/node-v20.19.5-linux-x64.tar.gz".into(), file: "node-v20.19.5-linux-x64.tar.gz".into(), sha256: crate::util::sha256_hex(&bytes) };
        Ok((bytes, archive))
    }

    #[test]
    fn w29_r24_poisoned_archives_and_extracted_runtimes_cannot_satisfy_a_lock() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let dir = root.path().join("runtime");
        let (bytes, archive) = archive()?;
        let bin = executable(&bytes, &archive, &dir)?;
        assert_eq!(std::fs::read(&bin)?, b"accepted Node bytes");
        std::fs::write(&bin, b"poisoned extracted runtime")?;
        let back = executable(&bytes, &archive, &dir)?;
        assert_eq!(std::fs::read(back)?, b"accepted Node bytes");
        assert!(executable(b"poisoned cached archive", &archive, &dir).is_err());
        let mut newer = archive.clone(); newer.sha256 = "b".repeat(64);
        assert!(verified(&bytes, &newer).is_err());
        Ok(())
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn w29_r24_node_uses_the_checked_archive_cache_and_refuses_its_poisoned_bytes() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let root = tempfile::tempdir()?;
            let (bytes, archive) = archive()?;
            let locks = kiln_ci::lock::execution::Locks { runtimes: [("node20".into(), [("linux/amd64".into(), archive.clone())].into())].into(), ..Default::default() };
            let a = AssignmentV2 { job_id: "locked-node".into(), dependency_locks: Some(locks), ..Default::default() };
            let (tx, _rx) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
            let leg = Leg { a, work_dir: root.path().to_owned(), runner_name: "fixture".into(), up: Uplink(tx), cancel: Shutdown::new(), kill: Shutdown::new(), http: std::sync::Arc::new(crate::http::client::Client::public()?), warm: Default::default(), cache_max: None };
            let cache = root.path().join("_externals/_locked_archives").join(format!("{}.tar.gz", archive.sha256));
            std::fs::create_dir_all(cache.parent().ok_or_else(|| anyhow::anyhow!("no cache parent"))?)?;
            std::fs::write(&cache, bytes)?;
            let bin = leg.locked_node("node20", false).await?;
            assert_eq!(std::fs::read(&bin)?, b"accepted Node bytes");
            std::fs::write(&bin, b"poisoned extracted binary")?;
            assert_eq!(std::fs::read(leg.locked_node("node20", false).await?)?, b"accepted Node bytes");
            std::fs::write(cache, b"poisoned archive")?;
            assert!(leg.locked_node("node20", false).await.err().is_some_and(|e| e.to_string().contains("KCI4051")));
            assert!(leg.locked_node("node20", true).await.err().is_some_and(|e| e.to_string().contains("KCI4050")), "no fallback to an unlocked musl download");
            Ok(())
        })
    }

    #[test]
    fn w29_r24_locked_dockerfiles_preserve_source_and_separate_build_cache_by_pin() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("action"); std::fs::create_dir_all(&source)?;
        let text = "FROM alpine:3\nRUN echo accepted\n";
        std::fs::write(source.join("Dockerfile"), text)?;
        let mut locked = kiln_ci::lock::execution::Dockerfile { path: "Dockerfile".into(), source_sha256: crate::util::sha256_hex(text.as_bytes()), locked: format!("FROM alpine@sha256:{}\nRUN echo accepted\n", "a".repeat(64)) };
        let (path, context, tag) = materialize(&source, root.path(), &locked)?;
        assert_eq!(context, source);
        assert!(!path.starts_with(&source));
        assert_eq!(std::fs::read_to_string(&path)?, locked.locked);
        assert_eq!(std::fs::read_to_string(source.join("Dockerfile"))?, text);
        locked.locked = locked.locked.replace(&"a".repeat(64), &"b".repeat(64));
        assert_ne!(materialize(&source, root.path(), &locked)?.2, tag);
        std::fs::write(source.join("Dockerfile"), "FROM moved:latest\n")?;
        assert!(materialize(&source, root.path(), &locked).is_err());
        Ok(())
    }
}
