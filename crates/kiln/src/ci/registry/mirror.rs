//! Pull-through caches (docs/ci.md W12-R02, R06): `[[ci.registry.mirrors]]` names an upstream
//! (Docker Hub, GHCR, Quay, `gcr.io` and Artifact Registry, ECR Public, `mcr.microsoft.com`,
//! `registry.k8s.io`, any other) that kiln's registry pulls through:
//! - under a prefix (`/v2/docker.io/library/alpine/manifests/3.20`), as containerd's `hosts.toml`
//!   (`override_path`) and kiln's runners address it, and at the root of the mirror's own `listen`
//!   address, as Docker's `registry-mirrors` does (Docker Hub);
//! - a manifest by digest, or a blob, comes from the upstream once and from kiln after;
//! - a tag is asked of the upstream again after `tag_ttl` (five minutes);
//! - when the upstream can't be reached, what kiln has is served, with a `Warning` saying so (R06).

use std::path::PathBuf;

use http::{StatusCode, header};
use rusqlite::{OptionalExtension as _, params};
use sha2::{Digest as _, Sha256};

use super::{Error, err, internal};
use crate::ci::integrations::oci;
use crate::config::ci::Mirror;
use crate::http::client::Method;
use crate::hub::Hub;

/// The mirror a name is under, and the upstream's own name for it (`library/alpine`).
pub fn of<'a>(hub: &'a Hub, name: &'a str) -> Option<(&'a Mirror, &'a str)> {
    hub.cfg.ci.registry.mirrors.iter().find_map(|m| {
        let prefix = m.prefix();
        name.strip_prefix(prefix.as_str()).and_then(|rest| rest.strip_prefix('/')).filter(|rest| !rest.is_empty()).map(|rest| (m, rest))
    })
}

/// The mirror a `listen` address serves at its root.
pub fn at_root(hub: &Hub, port: u16) -> Option<&Mirror> {
    hub.cfg.ci.registry.mirrors.iter().find(|m| m.listen.as_deref().and_then(|l| l.rsplit_once(':')).and_then(|(_, p)| p.parse::<u16>().ok()) == Some(port))
}

fn registry(m: &Mirror) -> oci::Registry {
    let credentials = match (&m.username, &m.password_file) {
        (Some(u), Some(f)) => std::fs::read_to_string(f).ok().map(|p| (u.clone(), p.trim().to_owned())).filter(|(_, p)| !p.is_empty()),
        _ => None,
    };
    let mut r = oci::Registry::for_name(&m.upstream, credentials, false);
    if let Some(url) = &m.url {
        r.base = url.trim_end_matches('/').to_owned();
    }
    r
}

/// What a pull through the mirror found: the digest and whether it came from kiln's copy because
/// the upstream was out of reach.
pub struct Fetched {
    pub digest: String,
    pub stale: bool,
}

/// Makes sure kiln has `reference` (a tag or a digest) of `name` (the mirror's prefixed name):
/// asked of the upstream when kiln hasn't got it, or the tag is older than its TTL. `None`: the
/// upstream hasn't got it either.
pub(super) async fn manifest(hub: &Hub, m: &Mirror, name: &str, upstream_name: &str, reference: &str) -> Result<Option<Fetched>, Error> {
    let by_digest = reference.contains(':');
    let (n, r) = (name.to_owned(), reference.to_owned());
    let kept: Option<(String, i64)> = hub
        .store
        .call(move |c| {
            Ok(if by_digest {
                c.query_row("SELECT digest, created_at FROM registry_manifests WHERE repo = ?1 AND digest = ?2", params![n, r], |x| Ok((x.get(0)?, x.get(1)?))).optional()?
            } else {
                c.query_row("SELECT digest, updated_at FROM registry_tags WHERE repo = ?1 AND tag = ?2", params![n, r], |x| Ok((x.get(0)?, x.get(1)?))).optional()?
            })
        })
        .await
        .map_err(internal)?;
    let fresh = |at: i64| by_digest || crate::util::now_unix() - at < i64::try_from(m.tag_ttl.unwrap_or(300)).unwrap_or(300);
    if let Some((digest, at)) = &kept
        && fresh(*at)
    {
        return Ok(Some(Fetched { digest: digest.clone(), stale: false }));
    }
    let reg = registry(m);
    let scope = format!("repository:{upstream_name}:pull");
    let got = oci::call(&reg, Method::Get, &format!("/v2/{upstream_name}/manifests/{reference}"), &scope).await;
    let resp = match got {
        Ok(r) if r.status() == 404 => return Ok(None),
        Ok(r) if r.is_success() => r,
        // Out of reach, or refusing: what kiln has, saying so (W12-R06).
        Ok(r) => return stale(kept, &format!("{} answered {}", m.upstream, r.status())),
        Err(e) => return stale(kept, &format!("{e:#}")),
    };
    let media_type = resp.header("content-type").unwrap_or(super::OCI_MANIFEST).split(';').next().unwrap_or(super::OCI_MANIFEST).trim().to_owned();
    let body = resp.bytes(super::MAX_MANIFEST).await.map_err(internal)?;
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(&body)));
    if by_digest && digest != reference {
        return Err(err(StatusCode::BAD_GATEWAY, "MANIFEST_INVALID", format!("{} sent a manifest that is {digest}, not {reference}", m.upstream)));
    }
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    let (n, d, mt, bytes, tag) = (name.to_owned(), digest.clone(), v.get("mediaType").and_then(serde_json::Value::as_str).map_or(media_type, str::to_owned), body.to_vec(), (!by_digest).then(|| reference.to_owned()));
    let subject = v.pointer("/subject/digest").and_then(serde_json::Value::as_str).map(str::to_owned);
    let size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    hub.store
        .call(move |c| {
            let tx = c.transaction()?;
            let now = crate::util::now_unix();
            tx.execute("INSERT INTO registry_repos (name, created_at) VALUES (?1, ?2) ON CONFLICT (name) DO NOTHING", params![n, now])?;
            tx.execute(
                "INSERT INTO registry_manifests (repo, digest, media_type, subject, body, size, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT (repo, digest) DO NOTHING",
                params![n, d, mt, subject, bytes, size, now],
            )?;
            if let Some(t) = tag {
                tx.execute(
                    "INSERT INTO registry_tags (repo, tag, digest, updated_at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (repo, tag) DO UPDATE SET digest = excluded.digest, updated_at = excluded.updated_at",
                    params![n, t, d, now],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(internal)?;
    Ok(Some(Fetched { digest, stale: false }))
}

fn stale(kept: Option<(String, i64)>, why: &str) -> Result<Option<Fetched>, Error> {
    match kept {
        Some((digest, _)) => {
            tracing::warn!(why, "a mirror's upstream is out of reach; serving what kiln has");
            Ok(Some(Fetched { digest, stale: true }))
        }
        None => Err(err(StatusCode::BAD_GATEWAY, "UNAVAILABLE", format!("the upstream is out of reach and kiln has no copy: {why}"))),
    }
}

/// Makes sure kiln has blob `digest` of `name`: from the upstream once (its redirect followed
/// without kiln's credentials), checked against its digest, then kept in the store.
pub(super) async fn blob(hub: &Hub, m: &Mirror, name: &str, upstream_name: &str, digest: &str) -> Result<bool, Error> {
    if super::linked(hub, name, digest).await?.is_some() {
        return Ok(true);
    }
    let hex = super::digest_ok(digest)?.to_owned();
    // Another mirror repository, or anyone, may have it already: linked here at once.
    if let Some(obj) = crate::ci::services::cas::object(hub, &hex).await.map_err(internal)? {
        super::link(hub, name, digest, obj.size).await?;
        return Ok(true);
    }
    let reg = registry(m);
    let scope = format!("repository:{upstream_name}:pull");
    let first = oci::call(&reg, Method::Get, &format!("/v2/{upstream_name}/blobs/{digest}"), &scope).await.map_err(|e| err(StatusCode::BAD_GATEWAY, "UNAVAILABLE", format!("{}: {e:#}", m.upstream)))?;
    let mut resp = match first.status() {
        404 => return Ok(false),
        301 | 302 | 303 | 307 | 308 => {
            let to = first.header("location").ok_or_else(|| err(StatusCode::BAD_GATEWAY, "UNAVAILABLE", format!("{}'s redirect has no location", m.upstream)))?.to_owned();
            oci::client().map_err(internal)?.request(Method::Get, &to).timeout(std::time::Duration::from_secs(60)).send().await.map_err(|e| err(StatusCode::BAD_GATEWAY, "UNAVAILABLE", format!("{e:#}")))?
        }
        s if (200..300).contains(&s) => first,
        s => return Err(err(StatusCode::BAD_GATEWAY, "UNAVAILABLE", format!("{} answered {s} for {digest}", m.upstream))),
    };
    if !resp.is_success() {
        return Err(err(StatusCode::BAD_GATEWAY, "UNAVAILABLE", format!("{digest} from {}: {}", m.upstream, resp.status())));
    }
    let path: PathBuf = crate::ci::services::cas::staging(hub).join(format!("mirror-{}", crate::util::new_id()));
    let (sink, done) = crate::rt::file_sink(path.clone(), hub.cfg.ci.registry.max_blob_bytes()).map_err(internal)?;
    while let Some(chunk) = resp.chunk().await.map_err(|e| err(StatusCode::BAD_GATEWAY, "UNAVAILABLE", format!("{digest} from {}: {e:#}", m.upstream)))? {
        if sink.send(chunk).await.is_err() {
            break;
        }
    }
    drop(sink);
    let kept = async {
        done.await.map_err(internal)?;
        let obj = crate::ci::services::cas::put_file(hub, &path).await.map_err(internal)?;
        if obj.digest != hex {
            return Err(err(StatusCode::BAD_GATEWAY, "DIGEST_INVALID", format!("{} sent sha256:{} for {digest}", m.upstream, obj.digest)));
        }
        super::link(hub, name, digest, obj.size).await
    }
    .await;
    let p = path.clone();
    let _ = crate::rt::blocking(move || std::fs::remove_file(p)).await;
    kept.map(|()| true)
}

/// Docker's `daemon.json` and containerd's `hosts.toml` for kiln's mirrors at `origin` (what a
/// machine that pulls through kiln is given).
pub fn client_config(hub: &Hub, origin: &str) -> serde_json::Value {
    let origin = origin.trim_end_matches('/');
    let host_of = |o: &str| o.split_once("://").map_or(o, |(_, r)| r).to_owned();
    let mut docker = Vec::new();
    let mut containerd = serde_json::Map::new();
    for m in &hub.cfg.ci.registry.mirrors {
        if m.upstream == oci::DOCKER_HUB
            && let Some(l) = &m.listen
        {
            let port = l.rsplit_once(':').map_or("", |(_, p)| p);
            let host = host_of(origin);
            let host = host.rsplit_once(':').map_or(host.as_str(), |(h, _)| h).to_owned();
            docker.push(format!("{}://{host}:{port}", origin.split_once("://").map_or("https", |(s, _)| s)));
        }
        let server = if m.upstream == oci::DOCKER_HUB { "https://registry-1.docker.io".to_owned() } else { format!("https://{}", m.upstream) };
        let toml = format!("server = \"{server}\"\n\n[host.\"{origin}/v2/{}\"]\n  capabilities = [\"pull\", \"resolve\"]\n  override_path = true\n", m.prefix());
        containerd.insert(format!("/etc/containerd/certs.d/{}/hosts.toml", m.upstream), serde_json::Value::String(toml));
    }
    serde_json::json!({ "docker": { "registry-mirrors": docker }, "containerd": containerd })
}

/// `GET /api/ci/registry/mirrors` (`ci.registry.mirrors`): the registries kiln pulls through, and
/// what a machine that pulls through kiln is given: Docker's `registry-mirrors`, and each
/// mirror's containerd `hosts.toml`, as a cluster's nodes read them under `config_path`
/// (`kiln images mirrors --containerd /etc/containerd/certs.d` writes them). A mirror that isn't
/// `anonymous` asks a node to sign in, which containerd's files can't: it's said so.
pub async fn mirrors(hub: &Hub) -> Result<crate::http::server::Response, crate::ci::Fail> {
    let list: Vec<serde_json::Value> =
        hub.cfg.ci.registry.mirrors.iter().map(|m| serde_json::json!({ "upstream": m.upstream, "prefix": m.prefix(), "listen": m.listen, "anonymous": m.anonymous })).collect();
    let notes: Vec<String> = hub
        .cfg
        .ci
        .registry
        .mirrors
        .iter()
        .filter(|m| !m.anonymous)
        .map(|m| format!("{} isn't `anonymous`: kiln asks whoever pulls through it to sign in, which a node pulling with containerd's hosts.toml doesn't", m.upstream))
        .collect();
    let mut out = client_config(hub, &hub.cfg.server.public_url);
    if let Some(o) = out.as_object_mut() {
        o.insert("mirrors".into(), serde_json::Value::Array(list));
        o.insert("notes".into(), serde_json::json!(notes));
    }
    Ok(crate::http::server::json(StatusCode::OK, &out))
}

/// Whether `reference` names an image kiln mirrors: the reference to pull it through kiln
/// (`kiln.example:443/docker.io/library/alpine:3.20`), for runners' image pulls.
pub fn through(hub: &Hub, reference: &str, registry_host: &str) -> Option<String> {
    let r = oci::parse_reference(reference).ok()?;
    let m = hub.cfg.ci.registry.mirrors.iter().find(|m| m.upstream == r.registry)?;
    let mut out = format!("{registry_host}/{}/{}", m.prefix(), r.repository);
    if let Some(t) = &r.tag {
        out.push(':');
        out.push_str(t);
    }
    if let Some(d) = &r.digest {
        out.push('@');
        out.push_str(d);
    }
    Some(out)
}

/// A response's header, to say kiln served its own copy (W12-R06).
pub fn warn_stale(r: crate::http::server::Response, upstream: &str) -> crate::http::server::Response {
    crate::http::server::with_header(r, header::WARNING, &format!("110 kiln \"served from kiln's copy: {upstream} is out of reach\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W12-R02: what a machine pulling through kiln is given: Docker's `registry-mirrors` for
    /// Docker Hub's own `listen`, and each mirror's containerd `hosts.toml` (its upstream's address,
    /// then kiln's under its prefix, `override_path`, as a cluster's nodes read them).
    #[test]
    fn w12_r02_what_a_node_pulls_through() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub_with(|c| {
                c.ci.registry.mirrors = vec![
                    Mirror { upstream: "docker.io".into(), listen: Some("0.0.0.0:5001".into()), anonymous: true, ..Mirror::default() },
                    Mirror { upstream: "ghcr.io".into(), prefix: Some("gh".into()), ..Mirror::default() },
                ];
            })
            .await?;
            let c = client_config(&hub, "https://kiln.example/");
            assert_eq!(c.pointer("/docker/registry-mirrors"), Some(&serde_json::json!(["https://kiln.example:5001"])), "{c}");
            let file = |k: &str| c.get("containerd").and_then(|m| m.get(k)).and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
            assert_eq!(
                file("/etc/containerd/certs.d/docker.io/hosts.toml"),
                "server = \"https://registry-1.docker.io\"\n\n[host.\"https://kiln.example/v2/docker.io\"]\n  capabilities = [\"pull\", \"resolve\"]\n  override_path = true\n"
            );
            assert!(file("/etc/containerd/certs.d/ghcr.io/hosts.toml").contains("server = \"https://ghcr.io\"\n\n[host.\"https://kiln.example/v2/gh\"]"), "{c}");
            Ok(())
        })
    }
}
