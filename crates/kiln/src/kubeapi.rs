//! A minimal Kubernetes REST client: exactly what the pod executor needs (create, get, list by
//! label, delete pods, read their logs) and restarting kiln's own Deployment (Settings →
//! Updates), over kiln's own HTTPS client on hyper-rt + hyper-tls. Object types come from
//! `k8s-openapi` (plain serde types, no runtime).
//!
//! Credentials, in order:
//! 1. **In-cluster**: `KUBERNETES_SERVICE_HOST`/`_PORT` and the mounted service-account token
//!    (re-read on every request, since bound tokens rotate) and CA.
//! 2. **kubeconfig**: `$KUBECONFIG` (first entry) or `~/.kube/config`, the context named by
//!    `KILN_KUBE_CONTEXT` or `current-context`. Supports CA by file or data, client certificates by
//!    file or data, and bearer tokens or token files. `exec` credential plugins are not supported
//!    and are reported as such rather than silently ignored.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use k8s_openapi::api::core::v1::Pod;
use serde::Deserialize;

use crate::http::client::{Client, Identity, Method, Roots};

const SA_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

enum Token {
    None,
    Static(String),
    File(PathBuf),
}

pub struct KubeApi {
    client: Client,
    server: String,
    token: Token,
    pub namespace: String,
}

/// An error the API server returned (`Status` object), kept typed so callers can react to
/// e.g. quota rejections.
#[derive(Debug, thiserror::Error)]
#[error("kubernetes API {code} {reason}: {message}")]
pub struct ApiError {
    pub code: u16,
    pub reason: String,
    pub message: String,
}

#[derive(Deserialize, Default)]
struct StatusBody {
    #[serde(default)]
    reason: String,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct PodList {
    #[serde(default)]
    items: Vec<Pod>,
}

// ---------------------------------------------------------------- kubeconfig

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct KubeConfig {
    #[serde(default)]
    current_context: String,
    #[serde(default)]
    clusters: Vec<Named<Cluster>>,
    #[serde(default)]
    users: Vec<Named<User>>,
    #[serde(default)]
    contexts: Vec<Named<Ctx>>,
}

#[derive(Deserialize)]
struct Named<T> {
    name: String,
    #[serde(alias = "cluster", alias = "user", alias = "context")]
    item: T,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct Cluster {
    server: String,
    certificate_authority: Option<PathBuf>,
    certificate_authority_data: Option<String>,
    #[serde(default)]
    insecure_skip_tls_verify: bool,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct User {
    client_certificate: Option<PathBuf>,
    client_certificate_data: Option<String>,
    client_key: Option<PathBuf>,
    client_key_data: Option<String>,
    token: Option<String>,
    token_file: Option<PathBuf>,
    exec: Option<serde_yaml::Value>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct Ctx {
    cluster: String,
    user: String,
    namespace: Option<String>,
}

fn b64(s: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .context("invalid base64 in kubeconfig")
}

/// Relative paths in a kubeconfig are relative to the file itself.
fn rel(base: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() { p.to_path_buf() } else { base.parent().unwrap_or(Path::new(".")).join(p) }
}

fn read(path: &Path) -> anyhow::Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("read {}", path.display()))
}

impl KubeApi {
    /// Find credentials (in-cluster first, then kubeconfig). `namespace` overrides the context's.
    pub fn discover(namespace: &str) -> anyhow::Result<Self> {
        if let (Ok(host), Ok(port)) =
            (std::env::var("KUBERNETES_SERVICE_HOST"), std::env::var("KUBERNETES_SERVICE_PORT"))
        {
            let token = Path::new(SA_DIR).join("token");
            if token.exists() {
                let ca = read(&Path::new(SA_DIR).join("ca.crt"))?;
                let host = if host.contains(':') { format!("[{host}]") } else { host };
                return Ok(Self {
                    client: Client::new(Roots::Pem(ca), None)?,
                    server: format!("https://{host}:{port}"),
                    token: Token::File(token),
                    namespace: namespace.to_owned(),
                });
            }
        }
        Self::from_kubeconfig(namespace)
    }

    fn from_kubeconfig(namespace: &str) -> anyhow::Result<Self> {
        let path = std::env::var_os("KUBECONFIG")
            .and_then(|v| std::env::split_paths(&v).next())
            .or_else(|| Some(crate::os::home_dir().join(".kube").join("config")))
            .ok_or_else(|| anyhow::anyhow!("no in-cluster credentials and no kubeconfig"))?;
        let context = std::env::var("KILN_KUBE_CONTEXT").ok();
        Self::from_kubeconfig_path(&path, context.as_deref(), namespace)
    }

    /// Load a specific kubeconfig file; `context` overrides its `current-context`.
    pub fn from_kubeconfig_path(path: &Path, context: Option<&str>, namespace: &str) -> anyhow::Result<Self> {
        let path = path.to_path_buf();
        let cfg: KubeConfig = serde_yaml::from_slice(&read(&path)?)
            .with_context(|| format!("parse {}", path.display()))?;
        let want = context.map_or_else(|| cfg.current_context.clone(), str::to_owned);
        let ctx = cfg
            .contexts
            .iter()
            .find(|c| c.name == want)
            .ok_or_else(|| anyhow::anyhow!("context `{want}` not found in {}", path.display()))?;
        let cluster = cfg
            .clusters
            .iter()
            .find(|c| c.name == ctx.item.cluster)
            .ok_or_else(|| anyhow::anyhow!("cluster `{}` not found", ctx.item.cluster))?;
        let user = cfg.users.iter().find(|u| u.name == ctx.item.user).map(|u| &u.item);
        if cluster.item.insecure_skip_tls_verify {
            anyhow::bail!("kubeconfig asks to skip TLS verification; kiln refuses unverified clusters");
        }
        let ca = match (&cluster.item.certificate_authority_data, &cluster.item.certificate_authority) {
            (Some(d), _) => Some(b64(d)?),
            (None, Some(p)) => Some(read(&rel(&path, p))?),
            (None, None) => None,
        };
        let mut identity = None;
        let mut token = Token::None;
        if let Some(u) = user {
            if u.exec.is_some() {
                anyhow::bail!("kubeconfig user `{}` uses an exec credential plugin, which kiln does not run", ctx.item.user);
            }
            let cert = match (&u.client_certificate_data, &u.client_certificate) {
                (Some(d), _) => Some(b64(d)?),
                (None, Some(p)) => Some(read(&rel(&path, p))?),
                _ => None,
            };
            let key = match (&u.client_key_data, &u.client_key) {
                (Some(d), _) => Some(b64(d)?),
                (None, Some(p)) => Some(read(&rel(&path, p))?),
                _ => None,
            };
            if let (Some(cert_pem), Some(key_pem)) = (cert, key) {
                identity = Some(Identity { cert_pem, key_pem });
            }
            token = match (&u.token, &u.token_file) {
                (Some(t), _) => Token::Static(t.clone()),
                (None, Some(f)) => Token::File(rel(&path, f)),
                _ => Token::None,
            };
        }
        let roots = match ca {
            Some(pem) => Roots::Pem(pem),
            None => Roots::WebPki,
        };
        let ns = if namespace.is_empty() { ctx.item.namespace.clone().unwrap_or_else(|| "default".into()) } else { namespace.to_owned() };
        Ok(Self {
            client: Client::new(roots, identity)?,
            server: cluster.item.server.trim_end_matches('/').to_owned(),
            token,
            namespace: ns,
        })
    }

    fn bearer(&self) -> anyhow::Result<Option<String>> {
        Ok(match &self.token {
            Token::None => None,
            Token::Static(t) => Some(t.clone()),
            Token::File(p) => Some(String::from_utf8_lossy(&read(p)?).trim().to_owned()),
        })
    }

    async fn call(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> anyhow::Result<(u16, Vec<u8>)> {
        self.call_as(method, path, body, "application/json", "application/json").await
    }

    /// [`Self::call`] naming the body's content type (patches say which kind they are) and what
    /// to answer with (logs are text).
    async fn call_as(&self, method: Method, path: &str, body: Option<Vec<u8>>, content_type: &str, accept: &str) -> anyhow::Result<(u16, Vec<u8>)> {
        let url = format!("{}{path}", self.server);
        let mut req = self.client.request(method, &url).header("Accept", accept);
        if let Some(t) = self.bearer()? {
            req = req.bearer(&t);
        }
        if let Some(b) = body {
            req = req.header("Content-Type", content_type).body(b);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let bytes = resp.bytes(64 << 20).await?;
        Ok((status, bytes.to_vec()))
    }

    fn api_error(code: u16, body: &[u8]) -> anyhow::Error {
        let s: StatusBody = serde_json::from_slice(body).unwrap_or_default();
        let message = if s.message.is_empty() { String::from_utf8_lossy(body).chars().take(400).collect() } else { s.message };
        anyhow::Error::new(ApiError { code, reason: s.reason, message })
    }

    fn pods_path(&self) -> String {
        format!("/api/v1/namespaces/{}/pods", self.namespace)
    }

    pub async fn create_pod(&self, pod: &Pod) -> anyhow::Result<()> {
        let body = serde_json::to_vec(pod)?;
        let (code, resp) = self.call(Method::Post, &self.pods_path(), Some(body)).await?;
        if (200..300).contains(&code) { Ok(()) } else { Err(Self::api_error(code, &resp)) }
    }

    pub async fn get_pod(&self, name: &str) -> anyhow::Result<Option<Pod>> {
        let (code, resp) = self.call(Method::Get, &format!("{}/{}", self.pods_path(), encode(name)), None).await?;
        match code {
            200 => Ok(Some(serde_json::from_slice(&resp).context("decode pod")?)),
            404 => Ok(None),
            _ => Err(Self::api_error(code, &resp)),
        }
    }

    pub async fn list_pods(&self, label_selector: &str) -> anyhow::Result<Vec<Pod>> {
        let path = format!("{}?labelSelector={}", self.pods_path(), encode(label_selector));
        let (code, resp) = self.call(Method::Get, &path, None).await?;
        if code != 200 {
            return Err(Self::api_error(code, &resp));
        }
        Ok(serde_json::from_slice::<PodList>(&resp).context("decode pod list")?.items)
    }

    /// A pod's log (its only container's), as text.
    pub async fn pod_log(&self, name: &str) -> anyhow::Result<String> {
        let (code, resp) = self.call_as(Method::Get, &format!("{}/{}/log", self.pods_path(), encode(name)), None, "application/json", "*/*").await?;
        if code != 200 {
            return Err(Self::api_error(code, &resp));
        }
        Ok(String::from_utf8_lossy(&resp).into_owned())
    }

    /// Restart a Deployment's pods the way `kubectl rollout restart` (and ArgoCD's restart
    /// action) does: a new pod-template annotation, so its rollout strategy replaces them.
    pub async fn restart_deployment(&self, name: &str) -> anyhow::Result<()> {
        let path = format!("/apis/apps/v1/namespaces/{}/deployments/{}", encode(&self.namespace), encode(name));
        let patch = serde_json::json!({
            "spec": { "template": { "metadata": { "annotations": { "kubectl.kubernetes.io/restartedAt": crate::util::now_rfc3339() } } } }
        });
        let (code, resp) = self.call_as(Method::Patch, &path, Some(serde_json::to_vec(&patch)?), "application/strategic-merge-patch+json", "application/json").await?;
        if (200..300).contains(&code) { Ok(()) } else { Err(Self::api_error(code, &resp)) }
    }

    /// Delete a pod in the background (its containers stop on their grace period). A pod that is
    /// already gone is not an error.
    pub async fn delete_pod(&self, name: &str) -> anyhow::Result<()> {
        let path = format!("{}/{}?propagationPolicy=Background", self.pods_path(), encode(name));
        let (code, resp) = self.call(Method::Delete, &path, None).await?;
        if (200..300).contains(&code) || code == 404 { Ok(()) } else { Err(Self::api_error(code, &resp)) }
    }
}

/// Percent-encode a path segment or query value (RFC 3986 unreserved characters pass through).
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kubeconfig_resolves_context_and_files() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let ca = crate::pki::Pki::open(dir.path())?.ca_pem;
        std::fs::write(dir.path().join("ca.crt"), &ca)?;
        let cfg = dir.path().join("config");
        std::fs::write(
            &cfg,
            r#"
apiVersion: v1
kind: Config
current-context: dev
clusters:
- name: c1
  cluster: { server: "https://192.168.49.2:8443/", certificate-authority: ca.crt }
users:
- name: u1
  user: { token: abc }
contexts:
- name: dev
  context: { cluster: c1, user: u1, namespace: jobs }
"#,
        )?;
        let api = KubeApi::from_kubeconfig_path(&cfg, None, "")?;
        assert_eq!(api.server, "https://192.168.49.2:8443");
        assert_eq!(api.namespace, "jobs");
        assert_eq!(api.bearer()?, Some("abc".into()));
        assert_eq!(encode("app.kubernetes.io/managed-by=kiln,x"), "app.kubernetes.io%2Fmanaged-by%3Dkiln%2Cx");
        Ok(())
    }
}
