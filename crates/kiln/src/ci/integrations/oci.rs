//! OCI registries (docs/cd-targets.md §7.1): any registry that speaks the OCI Distribution
//! Specification, with Docker's token authentication. Docker Hub, GHCR, Quay, GitLab, Harbor,
//! ECR, Artifact Registry, ACR, JFrog, a self-hosted `distribution`: each is reached the same way.
//!
//! - **References** are normalized as Docker does: `alpine` is `docker.io/library/alpine:latest`;
//!   a first component with a dot or a port, or `localhost`, is a registry.
//! - **Authentication**: a request is answered `401` with a `WWW-Authenticate` challenge. `Bearer`
//!   challenges are met with a token from the realm (with the integration's username and token
//!   as basic credentials, when it has them), cached until it expires; `Basic` ones with the
//!   credentials themselves.
//! - **Resolution**: a tag becomes a digest from a `HEAD` on its manifest (the
//!   `Docker-Content-Digest` header), or from the manifest's own bytes when a registry leaves the
//!   header out. An index lists the platforms an image is built for.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use base64::Engine as _;
use parking_lot::Mutex;

use crate::http::client::{Client, Method, Roots};

/// The media types a manifest is asked for in, most specific first.
const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json";

/// Docker Hub's names: what references say, where its API is, and its key in a Docker config.
pub const DOCKER_HUB: &str = "docker.io";
const DOCKER_HUB_API: &str = "registry-1.docker.io";
const DOCKER_HUB_AUTH_KEY: &str = "https://index.docker.io/v1/";

/// The largest manifest read whole.
const MANIFEST_LIMIT: usize = 4 << 20;

/// An image reference, normalized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    /// The registry as references name it: `docker.io`, `ghcr.io`, `localhost:5000`.
    pub registry: String,
    /// The repository: `library/alpine`, `acme/kiln`.
    pub repository: String,
    /// The tag, if the reference has one (`latest` when it has neither a tag nor a digest).
    pub tag: Option<String>,
    /// The digest, if the reference pins one.
    pub digest: Option<String>,
}

impl Reference {
    /// `registry/repository`, as a pull names it.
    pub fn name(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
    }

    /// The reference pinned to `digest`: `registry/repository@sha256:…`.
    pub fn pinned(&self, digest: &str) -> String {
        format!("{}@{digest}", self.name())
    }

    /// What follows `manifests/` in the API: the digest when pinned, else the tag.
    fn target(&self) -> &str {
        self.digest.as_deref().or(self.tag.as_deref()).unwrap_or("latest")
    }
}

impl std::fmt::Display for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())?;
        if let Some(t) = &self.tag {
            write!(f, ":{t}")?;
        }
        if let Some(d) = &self.digest {
            write!(f, "@{d}")?;
        }
        Ok(())
    }
}

/// Parses an image reference as Docker normalizes it.
pub fn parse_reference(s: &str) -> anyhow::Result<Reference> {
    let s = s.trim();
    anyhow::ensure!(!s.is_empty(), "an empty image reference");
    anyhow::ensure!(!s.contains("://"), "`{s}`: an image reference has no scheme (ghcr.io/org/app:tag)");
    let (rest, digest) = match s.split_once('@') {
        Some((r, d)) => {
            anyhow::ensure!(valid_digest(d), "`{s}`: `{d}` is not a digest (sha256:<64 hex>)");
            (r, Some(d.to_owned()))
        }
        None => (s, None),
    };
    // A tag follows the last `:` after the last `/` (a registry's port comes before a `/`).
    let slash = rest.rfind('/').map_or(0, |i| i + 1);
    let (name, tag) = match rest.get(slash..).and_then(|last| last.rfind(':')).map(|i| slash + i) {
        Some(i) => (rest.get(..i).unwrap_or(rest), rest.get(i + 1..).map(str::to_owned)),
        None => (rest, None),
    };
    if let Some(t) = &tag {
        anyhow::ensure!(valid_tag(t), "`{s}`: `{t}` is not a tag");
    }
    let (registry, repository) = match name.split_once('/') {
        Some((first, path)) if first.contains('.') || first.contains(':') || first == "localhost" => (first.to_ascii_lowercase(), path.to_owned()),
        _ => (DOCKER_HUB.to_owned(), name.to_owned()),
    };
    let registry = if registry == "index.docker.io" || registry == DOCKER_HUB_API { DOCKER_HUB.to_owned() } else { registry };
    // Docker Hub's official images live under `library/`.
    let repository = if registry == DOCKER_HUB && !repository.contains('/') { format!("library/{repository}") } else { repository };
    anyhow::ensure!(
        !repository.is_empty() && repository.split('/').all(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))),
        "`{s}`: `{repository}` is not a repository name (lower case, digits, `.`, `_`, `-`)"
    );
    let tag = if tag.is_none() && digest.is_none() { Some("latest".to_owned()) } else { tag };
    Ok(Reference { registry, repository, tag, digest })
}

fn valid_digest(d: &str) -> bool {
    d.split_once(':').is_some_and(|(alg, hex)| alg == "sha256" && hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn valid_tag(t: &str) -> bool {
    !t.is_empty() && t.len() <= 128 && t.bytes().enumerate().all(|(i, b)| b.is_ascii_alphanumeric() || b == b'_' || (i > 0 && matches!(b, b'.' | b'-')))
}

/// How a registry is reached: its API's base URL and the credentials an integration gives.
#[derive(Clone, Debug)]
pub struct Registry {
    /// As references name it.
    pub name: String,
    /// `https://registry-1.docker.io`, `https://ghcr.io`, `http://localhost:5000`.
    pub base: String,
    pub credentials: Option<(String, String)>,
}

impl Registry {
    /// The registry a reference names, with `credentials` (a username and a token or password).
    /// `insecure` speaks plain HTTP (a registry on the local network or in a test).
    pub fn for_name(name: &str, credentials: Option<(String, String)>, insecure: bool) -> Self {
        let host = if name == DOCKER_HUB { DOCKER_HUB_API } else { name };
        let scheme = if insecure { "http" } else { "https" };
        Self { name: name.to_owned(), base: format!("{scheme}://{host}"), credentials }
    }

    /// The key a Docker config uses for this registry.
    fn auth_key(&self) -> String {
        if self.name == DOCKER_HUB { DOCKER_HUB_AUTH_KEY.to_owned() } else { self.name.clone() }
    }
}

/// What a tag stands for now.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Resolved {
    pub digest: String,
    pub media_type: String,
    /// The platforms an index lists (`linux/amd64`, `linux/arm64/v8`); empty for one manifest.
    pub platforms: Vec<String>,
}

pub(crate) fn client() -> anyhow::Result<&'static Client> {
    static C: OnceLock<Client> = OnceLock::new();
    if let Some(c) = C.get() {
        return Ok(c);
    }
    let c = Client::new(Roots::WebPki, None)?;
    Ok(C.get_or_init(|| c))
}

/// Tokens from registries' realms, by (realm, service, scope, credentials), until they expire.
fn tokens() -> &'static Mutex<HashMap<(String, String, String, String), (String, Instant)>> {
    static T: OnceLock<Mutex<HashMap<(String, String, String, String), (String, Instant)>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A `WWW-Authenticate` challenge: its scheme and parameters.
#[derive(Debug, PartialEq, Eq)]
pub struct Challenge {
    pub scheme: String,
    pub params: HashMap<String, String>,
}

/// Parses `Bearer realm="…",service="…",scope="…"` (or `Basic realm="…"`).
pub fn parse_challenge(h: &str) -> Option<Challenge> {
    let h = h.trim();
    let (scheme, rest) = h.split_once(' ').unwrap_or((h, ""));
    let mut params = HashMap::new();
    let mut rest = rest.trim();
    while !rest.is_empty() {
        let (k, after) = rest.split_once('=')?;
        let k = k.trim().to_ascii_lowercase();
        let after = after.trim_start();
        let (v, tail) = if let Some(q) = after.strip_prefix('"') {
            let end = q.find('"')?;
            (q.get(..end)?.to_owned(), q.get(end + 1..).unwrap_or(""))
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (after.get(..end)?.trim().to_owned(), after.get(end..).unwrap_or(""))
        };
        params.insert(k, v);
        rest = tail.trim_start().trim_start_matches(',').trim_start();
    }
    Some(Challenge { scheme: scheme.to_ascii_lowercase(), params })
}

fn basic(user: &str, secret: &str) -> String {
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{secret}")))
}

/// The `Authorization` that meets `challenge` for `registry`, fetching (or reusing) a token.
async fn answer(registry: &Registry, challenge: &Challenge, scope: &str) -> anyhow::Result<String> {
    match challenge.scheme.as_str() {
        "basic" => {
            let (u, p) = registry.credentials.as_ref().context("the registry asks for a username and password, and this integration has none")?;
            Ok(basic(u, p))
        }
        "bearer" => {
            let realm = challenge.params.get("realm").context("a Bearer challenge without a realm")?;
            let service = challenge.params.get("service").cloned().unwrap_or_default();
            let scope = challenge.params.get("scope").cloned().unwrap_or_else(|| scope.to_owned());
            // Keyed by who asks (the user and a hash of the secret), so new credentials never reuse
            // a token the old ones got.
            let who = registry.credentials.as_ref().map(|(u, p)| format!("{u}:{}", crate::util::sha256_hex(p.as_bytes()))).unwrap_or_default();
            let key = (realm.clone(), service.clone(), scope.clone(), who);
            if let Some((t, until)) = tokens().lock().get(&key)
                && Instant::now() < *until
            {
                return Ok(format!("Bearer {t}"));
            }
            let mut url = format!("{realm}?service={}", urlencode(&service));
            if !scope.is_empty() {
                url.push_str(&format!("&scope={}", urlencode(&scope)));
            }
            let mut req = client()?.get(&url);
            if let Some((u, p)) = &registry.credentials {
                req = req.header("authorization", &basic(u, p));
            }
            let resp = req.send().await.with_context(|| format!("token from {realm}"))?;
            anyhow::ensure!(resp.is_success(), "{realm} refused a token ({}): check the integration's username and token", resp.status());
            let body: serde_json::Value = resp.json(1 << 20).await?;
            let token = body.get("token").or_else(|| body.get("access_token")).and_then(|v| v.as_str()).context("the token answer holds no token")?.to_owned();
            // Docker's default lifetime is 60 s; renew a little early.
            let secs = body.get("expires_in").and_then(serde_json::Value::as_u64).unwrap_or(60).max(30);
            tokens().lock().insert(key, (token.clone(), Instant::now() + Duration::from_secs(secs.saturating_sub(10))));
            Ok(format!("Bearer {token}"))
        }
        other => anyhow::bail!("the registry asks for `{other}` authentication, which kiln doesn't speak"),
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One request to the registry's API, answering its challenge once.
pub(crate) async fn call(registry: &Registry, method: Method, path: &str, scope: &str) -> anyhow::Result<crate::http::client::Response> {
    let url = format!("{}{path}", registry.base);
    let first = client()?.request(method, &url).header("accept", ACCEPT).send().await.with_context(|| format!("{} {url}", method_name(method)))?;
    if first.status() != 401 {
        return Ok(first);
    }
    let challenge = first.header("www-authenticate").and_then(parse_challenge).context("the registry answered 401 without a challenge")?;
    let auth = answer(registry, &challenge, scope).await?;
    client()?.request(method, &url).header("accept", ACCEPT).header("authorization", &auth).send().await.with_context(|| format!("{} {url}", method_name(method)))
}

/// A request with a body (a blob, a manifest), answering the registry's challenge once.
async fn send(registry: &Registry, method: Method, url: &str, scope: &str, content_type: &str, body: &[u8]) -> anyhow::Result<crate::http::client::Response> {
    let build = |auth: Option<&str>| -> anyhow::Result<crate::http::client::RequestBuilder<'static>> {
        let mut r = client()?.request(method, url).header("content-type", content_type).body(body.to_vec());
        if let Some(a) = auth {
            r = r.header("authorization", a);
        }
        Ok(r)
    };
    let first = build(None)?.send().await.with_context(|| format!("{} {url}", method_name(method)))?;
    if first.status() != 401 {
        return Ok(first);
    }
    let challenge = first.header("www-authenticate").and_then(parse_challenge).context("the registry answered 401 without a challenge")?;
    let auth = answer(registry, &challenge, scope).await?;
    build(Some(&auth))?.send().await.with_context(|| format!("{} {url}", method_name(method)))
}

/// `location` (an upload's), absolute.
fn absolute(registry: &Registry, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") { location.to_owned() } else { format!("{}{location}", registry.base) }
}

/// A blob pushed into `repo` (or found there already): its digest and size.
async fn push_blob(registry: &Registry, repo: &str, bytes: &[u8]) -> anyhow::Result<(String, u64)> {
    let digest = format!("sha256:{}", crate::util::sha256_hex(bytes));
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let scope = format!("repository:{repo}:pull,push");
    if call(registry, Method::Head, &format!("/v2/{repo}/blobs/{digest}"), &scope).await?.is_success() {
        return Ok((digest, size));
    }
    // A monolithic upload: started, then the blob in one `PUT`.
    let start = send(registry, Method::Post, &format!("{}/v2/{repo}/blobs/uploads/", registry.base), &scope, "application/octet-stream", &[]).await?;
    anyhow::ensure!(start.status() == 202, "{} didn't start an upload into {repo} ({})", registry.name, start.status());
    let at = absolute(registry, start.header("location").context("an upload without a location")?);
    let url = format!("{at}{}digest={}", if at.contains('?') { "&" } else { "?" }, urlencode(&digest));
    let put = send(registry, Method::Put, &url, &scope, "application/octet-stream", bytes).await?;
    anyhow::ensure!(put.status() == 201, "{} didn't take a blob into {repo} ({})", registry.name, put.status());
    Ok((digest, size))
}

/// kiln's signed attestation (a Sigstore bundle) attached to `subject` in another registry, as
/// in kiln's own (docs/ci.md W18-R02): an OCI 1.1 artifact manifest whose `subject` is the image,
/// and where the registry has no referrers API (its answer to the push has no `OCI-Subject`),
/// listed in the referrers tag schema's index (`sha256-<hex>`) too. Its digest.
pub async fn attach(registry: &Registry, repo: &str, subject: &str, bundle: &[u8], predicate_type: &str) -> anyhow::Result<String> {
    const BUNDLE_TYPE: &str = "application/vnd.dev.sigstore.bundle.v0.3+json";
    const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
    let scope = format!("repository:{repo}:pull,push");
    // The subject as the registry has it: its media type and size.
    let head = call(registry, Method::Head, &format!("/v2/{repo}/manifests/{subject}"), &scope).await?;
    anyhow::ensure!(head.is_success(), "{} has no {repo}@{subject} ({})", registry.name, head.status());
    let media_type = head.header("content-type").map(|t| t.split(';').next().unwrap_or(t).trim().to_owned()).unwrap_or_else(|| OCI_MANIFEST.to_owned());
    let size: u64 = head.header("content-length").and_then(|l| l.trim().parse().ok()).context("the subject's size")?;
    let (cd, cs) = push_blob(registry, repo, b"{}").await?;
    let (bd, bs) = push_blob(registry, repo, bundle).await?;
    let annotations = serde_json::json!({ "dev.sigstore.bundle.content": "dsse-envelope", "dev.sigstore.bundle.predicateType": predicate_type });
    let manifest = serde_json::json!({
        "schemaVersion": 2, "mediaType": OCI_MANIFEST, "artifactType": BUNDLE_TYPE,
        "config": { "mediaType": "application/vnd.oci.empty.v1+json", "digest": cd, "size": cs },
        "layers": [{ "mediaType": BUNDLE_TYPE, "digest": bd, "size": bs }],
        "subject": { "mediaType": media_type, "digest": subject, "size": size },
        "annotations": annotations,
    });
    let body = serde_json::to_vec(&manifest)?;
    let digest = format!("sha256:{}", crate::util::sha256_hex(&body));
    let put = send(registry, Method::Put, &format!("{}/v2/{repo}/manifests/{digest}", registry.base), &scope, OCI_MANIFEST, &body).await?;
    anyhow::ensure!(put.status() == 201, "{} didn't take the attestation's manifest ({})", registry.name, put.status());
    if put.header("oci-subject").is_none() {
        // The referrers tag schema: the subject's referrers in an index at `sha256-<hex>`.
        let tag = subject.replace(':', "-");
        let have = call(registry, Method::Get, &format!("/v2/{repo}/manifests/{tag}"), &scope).await?;
        let mut index = if have.is_success() { have.json::<serde_json::Value>(4 << 20).await.unwrap_or_default() } else { serde_json::Value::Null };
        if index.get("manifests").and_then(serde_json::Value::as_array).is_none() {
            index = serde_json::json!({ "schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [] });
        }
        let entry = serde_json::json!({ "mediaType": OCI_MANIFEST, "digest": digest, "size": body.len(), "artifactType": BUNDLE_TYPE, "annotations": annotations });
        if let Some(list) = index.get_mut("manifests").and_then(serde_json::Value::as_array_mut)
            && !list.iter().any(|m| m.get("digest") == entry.get("digest"))
        {
            list.push(entry);
        }
        let text = serde_json::to_vec(&index)?;
        let put = send(registry, Method::Put, &format!("{}/v2/{repo}/manifests/{tag}", registry.base), &scope, "application/vnd.oci.image.index.v1+json", &text).await?;
        anyhow::ensure!(put.status() == 201, "{} didn't take the referrers index at {tag} ({})", registry.name, put.status());
    }
    Ok(digest)
}

fn method_name(m: Method) -> &'static str {
    match m {
        Method::Get => "GET",
        Method::Head => "HEAD",
        Method::Post => "POST",
        Method::Put => "PUT",
        Method::Patch => "PATCH",
        Method::Delete => "DELETE",
    }
}

/// What `reference` stands for now in `registry`: its digest (the pinned one, checked to exist,
/// or the tag's), its media type and, for an index, its platforms.
pub async fn resolve(registry: &Registry, reference: &Reference) -> anyhow::Result<Option<Resolved>> {
    let path = format!("/v2/{}/manifests/{}", reference.repository, reference.target());
    let scope = format!("repository:{}:pull", reference.repository);
    let head = call(registry, Method::Head, &path, &scope).await?;
    match head.status() {
        404 => return Ok(None),
        401 | 403 => anyhow::bail!("{} refused {reference} ({}): it's private, or the integration can't pull it", registry.name, head.status()),
        s if !(200..300).contains(&s) => anyhow::bail!("{} answered {s} for {reference}", registry.name),
        _ => {}
    }
    let media_type = head.header("content-type").unwrap_or_default().split(';').next().unwrap_or_default().trim().to_owned();
    let digest = head.header("docker-content-digest").map(str::to_owned);
    let is_index = media_type.contains("index") || media_type.contains("manifest.list");
    // The manifest itself: for a digest the registry left out, or an index's platforms.
    if digest.is_some() && !is_index {
        return Ok(digest.map(|digest| Resolved { digest, media_type, platforms: Vec::new() }));
    }
    let get = call(registry, Method::Get, &path, &scope).await?;
    anyhow::ensure!(get.is_success(), "{} answered {} for {reference}'s manifest", registry.name, get.status());
    let media_type = get.header("content-type").map(|c| c.split(';').next().unwrap_or_default().trim().to_owned()).unwrap_or(media_type);
    let header_digest = get.header("docker-content-digest").map(str::to_owned);
    let bytes = get.bytes(MANIFEST_LIMIT).await?;
    let digest = digest.or(header_digest).unwrap_or_else(|| format!("sha256:{}", crate::util::sha256_hex(&bytes)));
    let platforms = if is_index { platforms_of(&bytes) } else { Vec::new() };
    if let Some(pinned) = &reference.digest {
        anyhow::ensure!(*pinned == digest, "{} answered {digest} for {reference}", registry.name);
    }
    Ok(Some(Resolved { digest, media_type, platforms }))
}

/// A repository's tags, as far as ten pages of a thousand go.
pub async fn tags(registry: &Registry, repository: &str) -> anyhow::Result<Vec<String>> {
    let scope = format!("repository:{repository}:pull");
    let mut path = format!("/v2/{repository}/tags/list?n=1000");
    let mut all = Vec::new();
    for _ in 0..10 {
        let resp = call(registry, Method::Get, &path, &scope).await?;
        anyhow::ensure!(resp.is_success(), "{} answered {} for {repository}'s tags", registry.name, resp.status());
        let next = resp.header("link").and_then(|l| next_page(l, &registry.base));
        let v: serde_json::Value = serde_json::from_slice(&resp.bytes(MANIFEST_LIMIT).await?).context("the tag list doesn't read")?;
        all.extend(v.get("tags").and_then(serde_json::Value::as_array).into_iter().flatten().filter_map(serde_json::Value::as_str).map(str::to_owned));
        match next {
            Some(n) => path = n,
            None => break,
        }
    }
    Ok(all)
}

/// The next page a `Link` header points at (`</v2/acme/web/tags/list?last=x&n=1000>; rel="next"`),
/// as a path on the registry.
fn next_page(link: &str, base: &str) -> Option<String> {
    let target = link.split(',').find(|l| l.contains("rel=\"next\""))?.split(';').next()?.trim().strip_prefix('<')?.strip_suffix('>')?;
    let path = target.strip_prefix(base).unwrap_or(target);
    path.starts_with('/').then(|| path.to_owned())
}

/// The platforms an index lists, skipping attestation manifests (`unknown/unknown`).
pub fn platforms_of(index: &[u8]) -> Vec<String> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(index) else { return Vec::new() };
    let mut out: Vec<String> = v
        .get("manifests")
        .and_then(|m| m.as_array())
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let p = m.get("platform")?;
            let os = p.get("os")?.as_str()?;
            let arch = p.get("architecture")?.as_str()?;
            if os == "unknown" {
                return None;
            }
            Some(match p.get("variant").and_then(|v| v.as_str()) {
                Some(var) => format!("{os}/{arch}/{var}"),
                None => format!("{os}/{arch}"),
            })
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Whether the integration's credentials are good: a token for pulling from `repository` (or
/// the registry's `/v2/` answered with them, for registries that don't scope tokens).
pub async fn verify(registry: &Registry, repository: Option<&str>) -> anyhow::Result<String> {
    let probe = call(registry, Method::Get, "/v2/", &repository.map(|r| format!("repository:{r}:pull")).unwrap_or_default()).await?;
    match probe.status() {
        200 => Ok(match &registry.credentials {
            Some((u, _)) => format!("{} accepted {u}'s credentials", registry.name),
            None => format!("{} answers without credentials", registry.name),
        }),
        401 | 403 => anyhow::bail!("{} refused the credentials ({})", registry.name, probe.status()),
        s => anyhow::bail!("{} answered {s} at /v2/", registry.name),
    }
}

/// A Docker config (`.dockerconfigjson`) holding `registry`'s credentials, for a Kubernetes
/// `imagePullSecrets` Secret.
pub fn docker_config(registry: &Registry) -> Option<String> {
    let (u, p) = registry.credentials.as_ref()?;
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"));
    Some(serde_json::json!({ "auths": { registry.auth_key(): { "username": u, "password": p, "auth": auth } } }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_list_s_next_page_is_a_path_on_the_registry() {
        let base = "https://ghcr.io";
        assert_eq!(next_page(r#"</v2/acme/web/tags/list?last=1.2&n=1000>; rel="next""#, base).as_deref(), Some("/v2/acme/web/tags/list?last=1.2&n=1000"));
        assert_eq!(next_page(r#"<https://ghcr.io/v2/acme/web/tags/list?last=1.2>; rel="next""#, base).as_deref(), Some("/v2/acme/web/tags/list?last=1.2"));
        assert_eq!(next_page(r#"<https://elsewhere.example/v2/x>; rel="next""#, base), None, "never another host");
        assert_eq!(next_page(r#"</v2/acme/web/tags/list?last=1.2>; rel="prev""#, base), None);
    }

    #[test]
    fn references_normalize_as_docker_does() -> anyhow::Result<()> {
        let r = parse_reference("alpine")?;
        assert_eq!((r.registry.as_str(), r.repository.as_str(), r.tag.as_deref(), r.digest.as_deref()), ("docker.io", "library/alpine", Some("latest"), None));
        let r = parse_reference("acme/kiln:3f2a9c1")?;
        assert_eq!((r.registry.as_str(), r.repository.as_str(), r.tag.as_deref()), ("docker.io", "acme/kiln", Some("3f2a9c1")));
        let r = parse_reference("ghcr.io/hyper-light/kiln:v1.2")?;
        assert_eq!((r.registry.as_str(), r.repository.as_str()), ("ghcr.io", "hyper-light/kiln"));
        let r = parse_reference("localhost:5000/team/app")?;
        assert_eq!((r.registry.as_str(), r.repository.as_str(), r.tag.as_deref()), ("localhost:5000", "team/app", Some("latest")));
        let d = format!("sha256:{}", "a".repeat(64));
        let r = parse_reference(&format!("registry.example.com:8443/app@{d}"))?;
        assert_eq!((r.registry.as_str(), r.tag.as_deref(), r.digest.as_deref()), ("registry.example.com:8443", None, Some(d.as_str())));
        assert_eq!(r.pinned(&d), format!("registry.example.com:8443/app@{d}"));
        assert_eq!(parse_reference("index.docker.io/acme/kiln")?.registry, "docker.io");
        assert!(parse_reference("https://ghcr.io/x").is_err());
        assert!(parse_reference("Acme/Kiln").is_err());
        assert!(parse_reference("app@sha256:abc").is_err());
        assert_eq!(parse_reference("acme/kiln:1.4")?.to_string(), "docker.io/acme/kiln:1.4");
        Ok(())
    }

    #[test]
    fn challenges_parse() {
        let c = parse_challenge(r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#);
        let c = c.map(|c| (c.scheme, c.params.get("realm").cloned(), c.params.get("service").cloned(), c.params.get("scope").cloned()));
        assert_eq!(
            c,
            Some(("bearer".to_owned(), Some("https://auth.docker.io/token".to_owned()), Some("registry.docker.io".to_owned()), Some("repository:library/alpine:pull".to_owned())))
        );
        assert_eq!(parse_challenge(r#"Basic realm="Registry""#).map(|c| c.scheme), Some("basic".to_owned()));
    }

    #[test]
    fn indexes_list_their_platforms() {
        let index = br#"{"manifests":[
            {"digest":"sha256:1","platform":{"os":"linux","architecture":"amd64"}},
            {"digest":"sha256:2","platform":{"os":"linux","architecture":"arm64","variant":"v8"}},
            {"digest":"sha256:3","platform":{"os":"unknown","architecture":"unknown"}}]}"#;
        assert_eq!(platforms_of(index), ["linux/amd64", "linux/arm64/v8"]);
    }

    #[test]
    fn docker_configs_carry_the_credentials_under_the_registrys_key() {
        let hub = Registry::for_name(DOCKER_HUB, Some(("acme".into(), "dckr_pat_x".into())), false);
        let cfg = docker_config(&hub).unwrap_or_default();
        assert!(cfg.contains("https://index.docker.io/v1/") && cfg.contains("\"username\":\"acme\""), "{cfg}");
        assert!(docker_config(&Registry::for_name("ghcr.io", None, false)).is_none());
        assert_eq!(Registry::for_name(DOCKER_HUB, None, false).base, "https://registry-1.docker.io");
    }

    /// Docker Hub itself, anonymously: an official image's tag resolves to a digest, and its
    /// index lists its platforms. Needs the network: `cargo test -- --ignored docker_hub`.
    #[test]
    #[ignore]
    fn docker_hub_resolves_an_official_image() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let r = parse_reference("alpine:3.20")?;
            let got = resolve(&Registry::for_name(DOCKER_HUB, None, false), &r).await?.ok_or_else(|| anyhow::anyhow!("alpine:3.20 not found"))?;
            assert!(got.digest.starts_with("sha256:") && got.digest.len() == 71, "{got:?}");
            assert!(got.platforms.iter().any(|p| p == "linux/amd64") && got.platforms.iter().any(|p| p.starts_with("linux/arm64")), "{got:?}");
            // Pinned to that digest: the same answer.
            let pinned = parse_reference(&r.pinned(&got.digest))?;
            assert_eq!(resolve(&Registry::for_name(DOCKER_HUB, None, false), &pinned).await?.map(|p| p.digest), Some(got.digest.clone()));
            println!("alpine:3.20 = {} ({})", got.digest, got.platforms.join(", "));
            Ok(())
        })
    }

    /// A registry that wants a bearer token from its realm, as Docker Hub does: resolution gets a
    /// token with the integration's credentials, then reads the tag's digest.
    #[test]
    fn a_tag_resolves_through_token_authentication() -> anyhow::Result<()> {
        use std::sync::Arc;

        use crate::http::server::{self as srv, Request, Response};

        struct Fake {
            base: parking_lot::Mutex<String>,
        }
        impl srv::Handler for Fake {
            async fn handle(self: Arc<Self>, req: Request) -> Response {
                let path = req.uri().path().to_owned();
                let auth = srv::header_str(&req, &http::header::AUTHORIZATION).unwrap_or("").to_owned();
                if path == "/token" {
                    let want = basic("acme", "s3cret");
                    if auth != want {
                        return srv::text(http::StatusCode::UNAUTHORIZED, "bad credentials");
                    }
                    return srv::json(http::StatusCode::OK, &serde_json::json!({ "token": "tok123", "expires_in": 300 }));
                }
                if auth != "Bearer tok123" {
                    let realm = format!("{}/token", self.base.lock());
                    let c = format!(r#"Bearer realm="{realm}",service="fake",scope="repository:acme/kiln:pull""#);
                    return srv::with_header(srv::text(http::StatusCode::UNAUTHORIZED, "auth"), http::header::WWW_AUTHENTICATE, &c);
                }
                match path.as_str() {
                    "/v2/" => srv::status(http::StatusCode::OK),
                    "/v2/acme/kiln/manifests/abc1234" => {
                        let r = srv::with_header(srv::status(http::StatusCode::OK), http::header::CONTENT_TYPE, "application/vnd.oci.image.manifest.v1+json");
                        srv::with_header(r, http::header::HeaderName::from_static("docker-content-digest"), &format!("sha256:{}", "b".repeat(64)))
                    }
                    _ => srv::status(http::StatusCode::NOT_FOUND),
                }
            }
        }

        crate::rt::block_on(async {
            let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
            let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
            let fake = Arc::new(Fake { base: parking_lot::Mutex::new(format!("http://{addr}")) });
            crate::rt::spawn(async move {
                while let Ok(s) = tcp.accept().await {
                    let (h, r) = (Arc::clone(&fake), s.peer_addr().ok());
                    crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
                }
            });
            let name = format!("{addr}");
            let registry = Registry::for_name(&name, Some(("acme".into(), "s3cret".into())), true);
            let reference = parse_reference(&format!("{name}/acme/kiln:abc1234"))?;
            let got = resolve(&registry, &reference).await?;
            assert_eq!(got.map(|r| r.digest), Some(format!("sha256:{}", "b".repeat(64))));
            // A tag that isn't there: none, not an error.
            assert_eq!(resolve(&registry, &parse_reference(&format!("{name}/acme/kiln:nope"))?).await?, None);
            assert!(verify(&registry, Some("acme/kiln")).await?.contains("accepted acme"));
            // Wrong credentials: refused, with what to check.
            let wrong = Registry::for_name(&name, Some(("acme".into(), "wrong".into())), true);
            let err = resolve(&wrong, &parse_reference(&format!("{name}/acme/kiln:other"))?).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(err.contains("refused a token"), "{err}");
            Ok(())
        })
    }

    /// W18-R02 in another registry: an attestation pushed as a referrer of its image there (its
    /// blobs, then its manifest with the image as its `subject`), and, the registry having no
    /// referrers API, listed in the referrers tag schema's index too; pushed again, not twice.
    #[test]
    fn w18_r02_an_attestation_attached_in_another_registry() -> anyhow::Result<()> {
        use std::collections::BTreeMap;
        use std::sync::Arc;

        use http_body_util::BodyExt as _;

        use crate::http::server::{self as srv, Request, Response};

        #[derive(Default)]
        struct Fake {
            /// Blobs and manifests by digest (and tag), as pushed.
            blobs: parking_lot::Mutex<BTreeMap<String, Vec<u8>>>,
            manifests: parking_lot::Mutex<BTreeMap<String, (String, Vec<u8>)>>,
        }
        impl srv::Handler for Fake {
            async fn handle(self: Arc<Self>, req: Request) -> Response {
                let (method, path, query) = (req.method().clone(), req.uri().path().to_owned(), req.uri().query().unwrap_or_default().to_owned());
                let ct = srv::header_str(&req, &http::header::CONTENT_TYPE).unwrap_or_default().to_owned();
                let body = req.into_body().collect().await.map(|b| b.to_bytes().to_vec()).unwrap_or_default();
                let subject = format!("sha256:{}", "c".repeat(64));
                match (method.as_str(), path.as_str()) {
                    ("HEAD", p) if p == format!("/v2/acme/app/manifests/{subject}") => {
                        let r = srv::with_header(srv::status(http::StatusCode::OK), http::header::CONTENT_TYPE, "application/vnd.oci.image.manifest.v1+json");
                        srv::with_header(r, http::header::CONTENT_LENGTH, "1234")
                    }
                    ("HEAD", p) if p.starts_with("/v2/acme/app/blobs/") => {
                        let d = p.trim_start_matches("/v2/acme/app/blobs/");
                        srv::status(if self.blobs.lock().contains_key(d) { http::StatusCode::OK } else { http::StatusCode::NOT_FOUND })
                    }
                    ("POST", "/v2/acme/app/blobs/uploads/") => srv::with_header(srv::status(http::StatusCode::ACCEPTED), http::header::LOCATION, "/v2/acme/app/blobs/uploads/u1"),
                    ("PUT", "/v2/acme/app/blobs/uploads/u1") => {
                        let d = query.strip_prefix("digest=").map(|d| d.replace("%3A", ":")).unwrap_or_default();
                        self.blobs.lock().insert(d, body);
                        srv::status(http::StatusCode::CREATED)
                    }
                    ("GET", p) if p.starts_with("/v2/acme/app/manifests/") => match self.manifests.lock().get(p.trim_start_matches("/v2/acme/app/manifests/")) {
                        Some((t, b)) => srv::response(http::StatusCode::OK, t, srv::full(b.clone())),
                        None => srv::status(http::StatusCode::NOT_FOUND),
                    },
                    // No referrers API: no `OCI-Subject` in its answer.
                    ("PUT", p) if p.starts_with("/v2/acme/app/manifests/") => {
                        self.manifests.lock().insert(p.trim_start_matches("/v2/acme/app/manifests/").to_owned(), (ct, body));
                        srv::status(http::StatusCode::CREATED)
                    }
                    _ => srv::status(http::StatusCode::NOT_FOUND),
                }
            }
        }

        crate::rt::block_on(async {
            let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
            let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
            let fake = Arc::new(Fake::default());
            let serving = Arc::clone(&fake);
            crate::rt::spawn(async move {
                while let Ok(s) = tcp.accept().await {
                    let (h, r) = (Arc::clone(&serving), s.peer_addr().ok());
                    crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
                }
            });
            let registry = Registry::for_name(&format!("{addr}"), None, true);
            let subject = format!("sha256:{}", "c".repeat(64));
            let bundle = br#"{"mediaType":"application/vnd.dev.sigstore.bundle.v0.3+json"}"#;
            let digest = attach(&registry, "acme/app", &subject, bundle, "https://slsa.dev/provenance/v1").await?;
            let manifests = fake.manifests.lock().clone();
            let (ct, body) = manifests.get(&digest).cloned().unwrap_or_default();
            let m: serde_json::Value = serde_json::from_slice(&body)?;
            assert_eq!(ct, "application/vnd.oci.image.manifest.v1+json");
            assert_eq!((m.pointer("/subject/digest"), m.pointer("/subject/size")), (Some(&serde_json::json!(subject)), Some(&serde_json::json!(1234))), "{m}");
            assert_eq!(m.get("artifactType"), Some(&serde_json::json!("application/vnd.dev.sigstore.bundle.v0.3+json")));
            let layer = m.pointer("/layers/0/digest").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
            assert_eq!(fake.blobs.lock().get(&layer).map(Vec::as_slice), Some(&bundle[..]), "the bundle, as a blob");
            let tag = subject.replace(':', "-");
            let index: serde_json::Value = serde_json::from_slice(&manifests.get(&tag).map(|(_, b)| b.clone()).unwrap_or_default())?;
            assert_eq!(index.pointer("/manifests/0/digest"), Some(&serde_json::json!(digest)), "the referrers tag schema's index: {index}");
            // Again: the same manifest, listed once.
            assert_eq!(attach(&registry, "acme/app", &subject, bundle, "https://slsa.dev/provenance/v1").await?, digest);
            let index: serde_json::Value = serde_json::from_slice(&fake.manifests.lock().get(&tag).map(|(_, b)| b.clone()).unwrap_or_default())?;
            assert_eq!(index.get("manifests").and_then(serde_json::Value::as_array).map(Vec::len), Some(1));
            Ok(())
        })
    }
}
