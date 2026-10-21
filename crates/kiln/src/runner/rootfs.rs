//! A leg's OCI image as a microVM's root file system (docs/ci.md W13-R05): the image's manifest for
//! this machine's platform, pulled through kiln when kiln mirrors its registry (W12-R02); its
//! layers downloaded once each and checked by digest; and an ext4 of them built in a microVM of
//! its own, where `kiln guest build` writes the files as root, so they keep the owners, modes and
//! device files the image has, with no root here. Kept by the manifest's digest, read-only, for
//! every leg of that image.

use std::collections::HashMap;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Context as _;

use super::firecracker::{Drive, Machine};
use super::guest::{self, FromGuest, Layer, ToGuest};
use crate::ci::integrations::oci;

/// How long a build may take: a big image's layers are a few gigabytes to write.
const BUILD: Duration = Duration::from_secs(1800);

/// An image, ready to boot from.
#[derive(Debug, Clone)]
pub struct Rootfs {
    pub path: PathBuf,
    /// Its config's `Env`.
    pub env: Vec<(String, String)>,
    pub digest: String,
}

/// One at a time for `key` in this process (an image's build, kiln's disk).
pub async fn lock(key: &str) -> futures::lock::OwnedMutexGuard<()> {
    static LOCKS: OnceLock<parking_lot::Mutex<HashMap<String, Arc<futures::lock::Mutex<()>>>>> = OnceLock::new();
    let m = Arc::clone(LOCKS.get_or_init(Default::default).lock().entry(key.to_owned()).or_default());
    m.lock_owned().await
}

/// What an image is pulled with: kiln's origin, a token for its registry, and the registries kiln
/// mirrors (`(upstream, prefix)`).
#[derive(Debug, Clone, Default)]
pub struct Pull {
    pub origin: String,
    pub token: String,
    pub mirrors: Vec<(String, String)>,
}

impl Pull {
    /// A leg's: its runtime's registry, its job's token.
    pub fn of(a: &kiln_ci::wire::AssignmentV2) -> Pull {
        let origin = if a.runtime.registry_url.is_empty() { &a.runtime.facade_url } else { &a.runtime.registry_url };
        Pull { origin: origin.clone(), token: a.runtime.github_token.clone(), mirrors: a.runtime.mirrors.clone() }
    }
}

/// Where to pull `image` from: through kiln when kiln mirrors its registry, signed in with the
/// token at kiln's own registry, and anonymously anywhere else.
fn source(image: &str, pull: &Pull) -> anyhow::Result<(oci::Registry, oci::Reference)> {
    let r = oci::parse_reference(image)?;
    let origin = &pull.origin;
    let own = crate::githost::host_of(origin);
    let own_plain = origin.starts_with("http://");
    let job = Some(("kiln".to_owned(), pull.token.clone())).filter(|(_, t)| !t.is_empty());
    if let Some((_, prefix)) = pull.mirrors.iter().find(|(up, _)| *up == r.registry).filter(|_| !own.is_empty()) {
        let through = oci::Reference { registry: own.clone(), repository: format!("{prefix}/{}", r.repository), ..r };
        return Ok((oci::Registry::for_name(&own, job, own_plain), through));
    }
    if !own.is_empty() && r.registry == own {
        return Ok((oci::Registry::for_name(&own, job, own_plain), r));
    }
    let host = r.registry.rsplit_once(':').map_or(r.registry.as_str(), |(h, _)| h);
    let local = matches!(host, "localhost" | "127.0.0.1" | "[::1]");
    Ok((oci::Registry::for_name(&r.registry, None, local), r))
}

/// `image` as a root file system: kept already, or pulled and built now.
pub async fn prepare(m: &Machine, cache: &Path, kiln: &Path, image: &str, pull: &Pull) -> anyhow::Result<Rootfs> {
    let (registry, reference) = source(image, pull)?;
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    let pulled = oci::manifest_for(&registry, &reference, "linux", arch).await?;
    let hex = pulled.digest.trim_start_matches("sha256:").to_owned();
    let images = cache.join("images");
    let (path, about) = (images.join(format!("{hex}.ext4")), images.join(format!("{hex}.json")));
    let _one = lock(&format!("image-{hex}")).await;
    if path.is_file()
        && let Ok(text) = std::fs::read(&about)
        && let Ok(env) = serde_json::from_slice::<Vec<(String, String)>>(&text)
    {
        return Ok(Rootfs { path, env, digest: pulled.digest });
    }
    std::fs::create_dir_all(&images)?;
    let blobs = cache.join("blobs");
    std::fs::create_dir_all(&blobs)?;
    let blob = |d: &str| blobs.join(d.trim_start_matches("sha256:"));
    // Its config, for its environment.
    let config = blob(&pulled.config);
    if !config.is_file() {
        oci::blob_to(&registry, &reference.repository, &pulled.config, &config).await.context("the image's config")?;
    }
    let cfg: serde_json::Value = serde_json::from_slice(&std::fs::read(&config)?).context("the image's config doesn't read")?;
    let env: Vec<(String, String)> =
        cfg.pointer("/config/Env").and_then(serde_json::Value::as_array).into_iter().flatten().filter_map(|e| e.as_str()?.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned()))).collect();
    // Its layers, each once.
    let mut total = 0u64;
    for (digest, _, _) in &pulled.layers {
        let p = blob(digest);
        total += if p.is_file() { std::fs::metadata(&p)?.len() } else { oci::blob_to(&registry, &reference.repository, digest, &p).await.with_context(|| format!("layer {digest}"))? };
    }
    // Built in a microVM: the layers on one disk, the new root file system on another.
    let work = cache.join(format!("build-{}", hex.get(..12).unwrap_or("image")));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(work.join("layers"))?;
    let mut layers = Vec::new();
    for (i, (digest, media, _)) in pulled.layers.iter().enumerate() {
        let file = format!("{i:04}");
        let to = work.join("layers").join(&file);
        if std::fs::hard_link(blob(digest), &to).is_err() {
            std::fs::copy(blob(digest), &to)?;
        }
        layers.push(Layer { file, media_type: media.clone() });
    }
    let part = path.with_extension("part");
    let (m2, w2, k2, p2) = (m.clone(), work.clone(), kiln.to_path_buf(), part.clone());
    let built = crate::rt::blocking(move || build(&m2, &w2, &k2, &p2, total, layers)).await?;
    let _ = std::fs::remove_dir_all(&work);
    if let Err(e) = built {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::write(&about, serde_json::to_vec(&env)?)?;
    std::fs::rename(&part, &path)?;
    Ok(Rootfs { path, env, digest: pulled.digest })
}

/// The build itself, in `work`: the layers' disk made, an empty root file system at `out` sized
/// for what `total` bytes of compressed layers can hold, and the builder microVM run to its end.
fn build(m: &Machine, work: &Path, kiln: &Path, out: &Path, total: u64, layers: Vec<Layer>) -> anyhow::Result<()> {
    let disk = work.join("layers.ext4");
    super::firecracker::mkfs(m, &disk, total + total / 8 + (64 << 20), "layers", Some(&work.join("layers")))?;
    super::firecracker::mkfs(m, out, (total.saturating_mul(6) + (1 << 30)).max(2 << 30), "image", None)?;
    let key = work.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let socks = super::firecracker::Socks::new(&key)?;
    let control = UnixListener::bind(socks.port(guest::CONTROL_PORT))?;
    let mut vm = super::firecracker::boot(
        m,
        work,
        &socks,
        &[Drive { id: "kiln", path: kiln, read_only: true }, Drive { id: "layers", path: &disk, read_only: true }, Drive { id: "image", path: out, read_only: false }],
        "build",
        2,
        1024,
    )?;
    let mut conn = vm.accept(&control, Duration::from_secs(120))?;
    conn.set_read_timeout(Some(BUILD))?;
    anyhow::ensure!(matches!(guest::recv::<FromGuest>(&mut conn)?, Some(FromGuest::Ready)), "the builder didn't say it was ready:\n{}", super::firecracker::tail(&vm.console));
    guest::send(&mut conn, &ToGuest::Build(layers))?;
    let said = guest::recv::<FromGuest>(&mut conn);
    let console = vm.console.clone();
    vm.finish(Duration::from_secs(30));
    match said {
        Ok(Some(FromGuest::Built(None))) => Ok(()),
        Ok(Some(FromGuest::Built(Some(why)))) => anyhow::bail!("building its root file system: {why}"),
        Ok(_) => anyhow::bail!("the builder ended without saying how it went:\n{}", super::firecracker::tail(&console)),
        Err(e) => anyhow::bail!("the builder: {e:#}\n{}", super::firecracker::tail(&console)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(registry_url: &str, mirrors: &[(&str, &str)]) -> Pull {
        let mut a = kiln_ci::wire::AssignmentV2::default();
        a.runtime.registry_url = registry_url.into();
        a.runtime.github_token = "job-token".into();
        a.runtime.mirrors = mirrors.iter().map(|(u, p)| ((*u).to_owned(), (*p).to_owned())).collect();
        Pull::of(&a)
    }

    /// W13-R05 with W12-R02: an image from a registry kiln mirrors is pulled through kiln, signed
    /// in with the job's token, as kiln's own images are; others straight from their registry.
    #[test]
    fn w13_r05_images_come_through_kiln_when_it_mirrors_them() -> anyhow::Result<()> {
        let a = leg("https://kiln.example.com", &[("docker.io", "hub")]);
        let (r, through) = source("alpine:3.20", &a)?;
        assert_eq!((r.base.as_str(), through.repository.as_str(), through.tag.as_deref()), ("https://kiln.example.com", "hub/library/alpine", Some("3.20")));
        assert_eq!(r.credentials, Some(("kiln".into(), "job-token".into())));
        let (r, own) = source("kiln.example.com/acme/runner:1", &a)?;
        assert_eq!((r.base.as_str(), own.repository.as_str()), ("https://kiln.example.com", "acme/runner"));
        let (r, other) = source("ghcr.io/acme/tool@sha256:0000000000000000000000000000000000000000000000000000000000000000", &a)?;
        assert_eq!((r.base.as_str(), r.credentials.is_none(), other.digest.is_some()), ("https://ghcr.io", true, true));
        let (r, _) = source("127.0.0.1:5000/test/shell:1", &leg("http://127.0.0.1:7070", &[]))?;
        assert_eq!(r.base, "http://127.0.0.1:5000", "a registry on loopback speaks plain HTTP");
        Ok(())
    }
}
