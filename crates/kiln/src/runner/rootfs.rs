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
    /// What it runs, as its config says (a deployment's service, W15-R02).
    pub runs: Runs,
}

/// What an image runs: its config's `Entrypoint`, `Cmd`, `WorkingDir`, `User` and `Healthcheck`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Runs {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub workdir: String,
    pub user: String,
    pub health: Option<guest::Health>,
}

/// What `cfg` (an image's config) runs. Its health check's durations are in nanoseconds, 0 for
/// Docker's defaults (30 seconds each, three retries, no start period).
pub fn runs_of(cfg: &serde_json::Value) -> Runs {
    use serde_json::Value;
    let c = cfg.get("config").cloned().unwrap_or_default();
    let list = |v: Option<&Value>| -> Vec<String> { v.and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default() };
    let text = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let secs = |v: Option<&Value>, default: u64| v.and_then(Value::as_u64).filter(|ns| *ns > 0).map_or(default, |ns| (ns / 1_000_000_000).max(1));
    let health = c.get("Healthcheck").and_then(|h| {
        let test = list(h.get("Test"));
        match test.first().map(String::as_str) {
            None | Some("NONE") => None,
            Some(_) => Some(guest::Health {
                test,
                interval_secs: secs(h.get("Interval"), 30),
                timeout_secs: secs(h.get("Timeout"), 30),
                retries: h.get("Retries").and_then(Value::as_u64).and_then(|r| u32::try_from(r).ok()).filter(|r| *r > 0).unwrap_or(3),
                start_secs: h.get("StartPeriod").and_then(Value::as_u64).map_or(0, |ns| ns / 1_000_000_000),
            }),
        }
    });
    Runs { entrypoint: list(c.get("Entrypoint")), cmd: list(c.get("Cmd")), workdir: text("WorkingDir"), user: text("User"), health }
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
    // kiln's own runner images are in no registry: built here with Docker, then saved.
    if super::docker::runner_dockerfile(image, None).is_some() {
        return saved(m, cache, kiln, image).await;
    }
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
    let blobs = cache.join("blobs");
    std::fs::create_dir_all(&blobs)?;
    let blob = |d: &str| blobs.join(d.trim_start_matches("sha256:"));
    // Its config: its environment, and what it runs.
    let config = blob(&pulled.config);
    if !config.is_file() {
        oci::blob_to(&registry, &reference.repository, &pulled.config, &config).await.context("the image's config")?;
    }
    let cfg: serde_json::Value = serde_json::from_slice(&std::fs::read(&config)?).context("the image's config doesn't read")?;
    let runs = runs_of(&cfg);
    if path.is_file()
        && let Ok(text) = std::fs::read(&about)
        && let Ok(env) = serde_json::from_slice::<Vec<(String, String)>>(&text)
    {
        return Ok(Rootfs { path, env, digest: pulled.digest, runs });
    }
    std::fs::create_dir_all(&images)?;
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
    // Compressed layers hold some three times what they weigh.
    let size = (total.saturating_mul(6) + (1 << 30)).max(2 << 30);
    let built = crate::rt::blocking(move || build(&m2, &w2, &k2, &p2, total, size, layers)).await?;
    let _ = std::fs::remove_dir_all(&work);
    if let Err(e) = built {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::write(&about, serde_json::to_vec(&env)?)?;
    std::fs::rename(&part, &path)?;
    Ok(Rootfs { path, env, digest: pulled.digest, runs })
}

/// One of kiln's runner images (`kiln/runner-*`, W13-R04) as a root file system. No registry has
/// them, so they're built here with Docker, as the `docker` executor builds them (once per
/// version), saved (`docker save`, unpacked as it streams), and their layers, plain tar, built
/// into an ext4 as a pulled image's are. Kept by Docker's id for the image, with its config.
async fn saved(m: &Machine, cache: &Path, kiln: &Path, image: &str) -> anyhow::Result<Rootfs> {
    let bin = crate::rt::blocking(super::docker::find).await?.ok_or_else(|| anyhow::anyhow!("kiln's runner image {image} is built with Docker or Podman, and this runner has neither working"))?;
    let cli = super::docker::Cli::new(&bin, cache.join("docker-config"));
    let say = |m: String| tracing::info!(image, "{m}");
    let tag = super::docker::runner_image(&cli, image, None, &say).await?.ok_or_else(|| anyhow::anyhow!("{image} isn't one of kiln's runner images"))?;
    let id = cli.run(&["image".into(), "inspect".into(), "--format".into(), "{{.Id}}".into(), tag.clone()], &std::collections::BTreeMap::new(), Duration::from_secs(120)).await?;
    let hex = id.trim().trim_start_matches("sha256:").to_ascii_lowercase();
    anyhow::ensure!(hex.len() == 64 && hex.bytes().all(|c| c.is_ascii_hexdigit()), "Docker's id for {tag} is {id:?}");
    let images = cache.join("images");
    let (path, about, config) = (images.join(format!("{hex}.ext4")), images.join(format!("{hex}.json")), images.join(format!("{hex}.config.json")));
    let _one = lock(&format!("image-{hex}")).await;
    if path.is_file()
        && let (Ok(e), Ok(c)) = (std::fs::read(&about), std::fs::read(&config))
        && let (Ok(env), Ok(cfg)) = (serde_json::from_slice::<Vec<(String, String)>>(&e), serde_json::from_slice::<serde_json::Value>(&c))
    {
        return Ok(Rootfs { path, env, digest: format!("sha256:{hex}"), runs: runs_of(&cfg) });
    }
    std::fs::create_dir_all(&images)?;
    let work = cache.join(format!("build-{}", hex.get(..12).unwrap_or("image")));
    let _ = std::fs::remove_dir_all(&work);
    let save = work.join("save");
    std::fs::create_dir_all(&save)?;
    std::fs::create_dir_all(work.join("layers"))?;
    let (c2, t2, s2) = (cli.clone(), tag.clone(), save.clone());
    crate::rt::blocking(move || c2.save_into(&t2, &s2)).await?.with_context(|| format!("saving {tag}"))?;
    let (s3, w3) = (save.clone(), work.join("layers"));
    let (cfg, layers, total, compressed) = crate::rt::blocking(move || -> anyhow::Result<Saved> {
        let read = read_saved(&s3, &w3)?;
        std::fs::remove_dir_all(&s3)?;
        Ok(read)
    })
    .await??;
    let env: Vec<(String, String)> =
        cfg.pointer("/config/Env").and_then(serde_json::Value::as_array).into_iter().flatten().filter_map(|e| e.as_str()?.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned()))).collect();
    let part = path.with_extension("part");
    let (m2, w2, k2, p2) = (m.clone(), work.clone(), kiln.to_path_buf(), part.clone());
    // Plain layers weigh what they hold: room for that and a quarter more. Docker's containerd
    // image store saves them as it keeps them, compressed: room as for a pulled image's.
    let size = if compressed { (total.saturating_mul(6) + (1 << 30)).max(2 << 30) } else { (total + total / 4 + (1 << 30)).max(2 << 30) };
    let built = crate::rt::blocking(move || build(&m2, &w2, &k2, &p2, total, size, layers)).await?;
    let _ = std::fs::remove_dir_all(&work);
    if let Err(e) = built {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::write(&about, serde_json::to_vec(&env)?)?;
    std::fs::write(&config, serde_json::to_vec(&cfg)?)?;
    std::fs::rename(&part, &path)?;
    Ok(Rootfs { path, env, digest: format!("sha256:{hex}"), runs: runs_of(&cfg) })
}

/// A saved image read: its config, its layers, their bytes in all, and whether they're compressed.
type Saved = (serde_json::Value, Vec<Layer>, u64, bool);

/// What `docker save` unpacked in `save` holds: its manifest's config, and its layers in order,
/// linked into `layers` (`0000`, `0001`, …) as the builder takes them: plain tar, or gzip as
/// Docker's containerd image store keeps them (zstd the builder doesn't read). The manifest's
/// paths stay in `save`.
fn read_saved(save: &Path, layers_dir: &Path) -> anyhow::Result<Saved> {
    let inside = |rel: &str| -> anyhow::Result<PathBuf> {
        anyhow::ensure!(!rel.is_empty() && !rel.starts_with('/') && !rel.split('/').any(|c| c == ".." || c.is_empty()), "`{rel}` isn't a path in the saved image");
        Ok(save.join(rel))
    };
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(save.join("manifest.json")).context("the saved image has no manifest.json")?)?;
    let first = manifest.get(0).ok_or_else(|| anyhow::anyhow!("the saved image's manifest is empty"))?;
    let cfg: serde_json::Value = serde_json::from_slice(&std::fs::read(inside(first.get("Config").and_then(serde_json::Value::as_str).unwrap_or_default())?)?)?;
    let (mut layers, mut total, mut compressed) = (Vec::new(), 0u64, false);
    for (i, l) in first.get("Layers").and_then(serde_json::Value::as_array).into_iter().flatten().enumerate() {
        let from = inside(l.as_str().unwrap_or_default())?;
        let mut head = [0u8; 4];
        let n = std::io::Read::read(&mut std::fs::File::open(&from)?, &mut head)?;
        let media_type = match head.get(..n).unwrap_or_default() {
            [0x1f, 0x8b, ..] => "application/vnd.oci.image.layer.v1.tar+gzip",
            [0x28, 0xb5, 0x2f, 0xfd] => anyhow::bail!("layer {i} of the saved image is zstd, which a microVM's builder doesn't read yet"),
            _ => "application/vnd.oci.image.layer.v1.tar",
        };
        compressed |= media_type.ends_with("+gzip");
        let file = format!("{i:04}");
        let to = layers_dir.join(&file);
        std::fs::hard_link(&from, &to).or_else(|_| std::fs::copy(&from, &to).map(|_| ()))?;
        total += std::fs::metadata(&from)?.len();
        layers.push(Layer { file, media_type: media_type.into() });
    }
    Ok((cfg, layers, total, compressed))
}

/// The build itself, in `work`: the layers' disk made (`total` bytes of them), an empty root file
/// system of `size` bytes at `out`, and the builder microVM run to its end.
#[allow(clippy::too_many_arguments)]
fn build(m: &Machine, work: &Path, kiln: &Path, out: &Path, total: u64, size: u64, layers: Vec<Layer>) -> anyhow::Result<()> {
    let disk = work.join("layers.ext4");
    super::firecracker::mkfs(m, &disk, total + total / 8 + (64 << 20), "layers", Some(&work.join("layers")))?;
    super::firecracker::mkfs(m, out, size, "image", None)?;
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
    /// kiln's runner images in microVMs: what `docker save` unpacked (Docker 25's layout, blobs by
    /// digest, the same layer twice) read in order, and paths out of it refused.
    #[test]
    fn w13_saved_images_read_in_order() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (save, layers) = (dir.path().join("save"), dir.path().join("layers"));
        std::fs::create_dir_all(save.join("blobs/sha256"))?;
        std::fs::create_dir_all(&layers)?;
        std::fs::write(save.join("blobs/sha256/cfg"), r#"{"config":{"Env":["PATH=/usr/bin","ImageOS=ubuntu24"],"User":""}}"#)?;
        std::fs::write(save.join("blobs/sha256/a"), b"first layer")?;
        std::fs::write(save.join("blobs/sha256/b"), b"second")?;
        let manifest = serde_json::json!([{ "Config": "blobs/sha256/cfg", "RepoTags": ["kiln/runner-ubuntu-24.04:x"], "Layers": ["blobs/sha256/a", "blobs/sha256/b", "blobs/sha256/a"] }]);
        std::fs::write(save.join("manifest.json"), manifest.to_string())?;
        let (cfg, read, total, compressed) = read_saved(&save, &layers)?;
        assert_eq!(cfg.pointer("/config/Env/1").and_then(serde_json::Value::as_str), Some("ImageOS=ubuntu24"));
        assert_eq!(read.iter().map(|l| l.file.as_str()).collect::<Vec<_>>(), ["0000", "0001", "0002"]);
        assert_eq!((total, compressed), (11 + 6 + 11, false));
        assert_eq!(std::fs::read(layers.join("0002"))?, b"first layer");
        // Docker's containerd image store saves layers gzip, as it keeps them; zstd is refused.
        std::fs::write(save.join("blobs/sha256/gz"), [0x1f, 0x8b, 8, 0, 0, 0])?;
        std::fs::write(save.join("blobs/sha256/zst"), [0x28, 0xb5, 0x2f, 0xfd, 0])?;
        std::fs::write(save.join("manifest.json"), serde_json::json!([{ "Config": "blobs/sha256/cfg", "Layers": ["blobs/sha256/gz"] }]).to_string())?;
        let gz = dir.path().join("layers-gz");
        std::fs::create_dir_all(&gz)?;
        let (_, read, _, compressed) = read_saved(&save, &gz)?;
        assert!(compressed && read.first().is_some_and(|l| l.media_type.ends_with("+gzip")));
        std::fs::write(save.join("manifest.json"), serde_json::json!([{ "Config": "blobs/sha256/cfg", "Layers": ["blobs/sha256/zst"] }]).to_string())?;
        let zst = dir.path().join("layers-zst");
        std::fs::create_dir_all(&zst)?;
        assert!(read_saved(&save, &zst).is_err_and(|e| e.to_string().contains("zstd")));
        for bad in ["../outside", "/etc/passwd", "blobs//x", ""] {
            let m = serde_json::json!([{ "Config": "blobs/sha256/cfg", "Layers": [bad] }]);
            std::fs::write(save.join("manifest.json"), m.to_string())?;
            let fresh = dir.path().join(format!("layers-{}", bad.len()));
            std::fs::create_dir_all(&fresh)?;
            assert!(read_saved(&save, &fresh).is_err(), "{bad:?}");
        }
        Ok(())
    }

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
