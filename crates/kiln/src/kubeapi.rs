//! A minimal Kubernetes REST client: exactly what the pod executor needs (create, get, list by
//! label, delete pods, read their logs) and restarting kiln's own Deployment (Settings →
//! Updates), over kiln's own HTTPS client on hyper-rt + hyper-tls. Object types come from
//! `k8s-openapi` (plain serde types, no runtime).
//!
//! Configuration parsing, rotating credentials and pooled connections belong to the shared
//! Kubernetes foundation (U11). This module only supplies typed pod/deployment operations.

use std::path::Path;

use anyhow::Context as _;
use k8s_openapi::api::core::v1::Pod;
use serde::Deserialize;

use crate::http::client::Method;
use crate::kubernetes::Cluster;
use kiln_cd::kube::{self, Endpoint};

pub struct KubeApi {
    cluster: Cluster,
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

fn read(path: &Path) -> anyhow::Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("read {}", path.display()))
}

impl KubeApi {
    /// Find credentials (in-cluster first, then kubeconfig). `namespace` overrides the context's.
    pub fn discover(namespace: &str) -> anyhow::Result<Self> {
        if let Some(endpoint) = kube::in_cluster() {
            return Self::from_endpoint(endpoint, namespace);
        }
        let path = std::env::var_os("KUBECONFIG")
            .and_then(|v| std::env::split_paths(&v).next())
            .unwrap_or_else(|| crate::os::home_dir().join(".kube/config"));
        let context = std::env::var("KILN_KUBE_CONTEXT").ok();
        Self::from_kubeconfig_path(&path, context.as_deref(), namespace)
    }

    fn from_endpoint(endpoint: Endpoint, namespace: &str) -> anyhow::Result<Self> {
        let namespace = if namespace.is_empty() { endpoint.namespace.clone().unwrap_or_else(|| "default".into()) } else { namespace.to_owned() };
        Ok(Self { cluster: Cluster::new("executor", endpoint)?, namespace })
    }

    /// Load a specific kubeconfig file; `context` overrides its `current-context`.
    pub fn from_kubeconfig_path(path: &Path, context: Option<&str>, namespace: &str) -> anyhow::Result<Self> {
        let text = String::from_utf8(read(path)?).context("kubeconfig is not UTF-8")?;
        let endpoint = kube::kubeconfig(&text, context.unwrap_or_default(), path.parent().unwrap_or(Path::new(".")))?;
        Self::from_endpoint(endpoint, namespace)
    }

    async fn call(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> anyhow::Result<(u16, Vec<u8>)> {
        self.call_as(method, path, body, "application/json", "application/json").await
    }

    async fn call_as(&self, method: Method, path: &str, body: Option<Vec<u8>>, content_type: &str, accept: &str) -> anyhow::Result<(u16, Vec<u8>)> {
        let method = match method {
            Method::Get => http::Method::GET, Method::Post => http::Method::POST,
            Method::Put => http::Method::PUT, Method::Patch => http::Method::PATCH,
            Method::Delete => http::Method::DELETE, Method::Head => http::Method::HEAD,
        };
        let (status, bytes) = self.cluster.request_as(method, path, Some(content_type), body.map(bytes::Bytes::from), accept).await?;
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

    /// U03/U11: return the API-assigned UID to the existing creator. A later name lookup
    /// cannot recover this ownership: the original object may already have been replaced.
    pub async fn create_pod(&self, pod: &Pod) -> anyhow::Result<Pod> {
        let name = pod.metadata.name.as_deref().filter(|n| !n.is_empty()).context("pod creation requires a name")?;
        let body = serde_json::to_vec(pod)?;
        let (code, resp) = self.call(Method::Post, &self.pods_path(), Some(body)).await?;
        if !(200..300).contains(&code) { return Err(Self::api_error(code, &resp)) }
        let made: Pod = serde_json::from_slice(&resp).context("decode created pod; its creation outcome remains unknown")?;
        anyhow::ensure!(made.metadata.name.as_deref() == Some(name) && made.metadata.namespace.as_deref() == Some(self.namespace.as_str())
            && made.metadata.uid.as_deref().is_some_and(|u| !u.is_empty() && u.len() <= 512 && !u.chars().any(char::is_control)),
            "pod create response has no exact API identity; its creation outcome remains unknown");
        Ok(made)
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

    /// U03/U11: cleanup only the API object observed by the existing owner. Missing identity
    /// refuses deletion; a same-name replacement is protected by the API's UID precondition.
    /// Acceptance or 404 is cleanup only, never proof that the pod's containers stopped.
    pub async fn delete_pod(&self, pod: &Pod) -> anyhow::Result<()> {
        anyhow::ensure!(pod.metadata.namespace.as_deref() == Some(self.namespace.as_str()), "pod deletion requires the observed namespace");
        let name = pod.metadata.name.as_deref().context("pod deletion requires the observed name")?;
        let uid = pod.metadata.uid.as_deref().context("pod deletion requires the observed UID")?;
        self.delete_pod_uid(name, uid).await
    }

    /// U03: delete only the physical pod whose stopped evidence was persisted. A replacement
    /// with the same name must survive this request; 404 remains cleanup, never stopped proof.
    pub async fn delete_pod_uid(&self, name: &str, uid: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!name.is_empty() && name.len() <= 253 && !uid.is_empty() && uid.len() <= 512 && !name.chars().chain(uid.chars()).any(char::is_control), "pod deletion requires an exact name and UID");
        let body = serde_json::to_vec(&serde_json::json!({ "apiVersion": "v1", "kind": "DeleteOptions", "propagationPolicy": "Background", "preconditions": { "uid": uid } }))?;
        let (code, resp) = self.call(Method::Delete, &format!("{}/{}", self.pods_path(), encode(name)), Some(body)).await?;
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
        assert_eq!(api.namespace, "jobs");
        assert_eq!(KubeApi::from_kubeconfig_path(&cfg, None, "override")?.namespace, "override");
        assert_eq!(encode("app.kubernetes.io/managed-by=kiln,x"), "app.kubernetes.io%2Fmanaged-by%3Dkiln%2Cx");
        Ok(())
    }
}
