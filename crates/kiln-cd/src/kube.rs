//! Reaching a cluster, the parts that need no network: kubeconfig files read the way `kubectl`
//! reads them, credential plugins' answers (`ExecCredential`), API discovery turned into the
//! table that maps a kind to its resource path, and the paths and bodies of the requests the
//! engine's actions become.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use rustc_hash::FxHashMap;
use serde_json::Value;

use crate::obj::{self, GroupKind, ResKey};

/// How a request authenticates.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Auth {
    /// No credentials.
    #[default]
    None,
    /// A bearer token.
    Token(String),
    /// A bearer token read from a file before each use (service-account tokens rotate).
    TokenFile(PathBuf),
    /// A client certificate and key, PEM.
    Certificate {
        /// Certificate chain.
        cert: Vec<u8>,
        /// Private key.
        key: Vec<u8>,
    },
    /// HTTP basic authentication.
    Basic {
        /// User.
        username: String,
        /// Password.
        password: String,
    },
    /// A credential plugin.
    Exec(Exec),
}

/// A credential plugin, as kubeconfig's `users[].user.exec` describes it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Exec {
    /// The program.
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Its environment.
    pub env: Vec<(String, String)>,
    /// The `client.authentication.k8s.io` version it speaks.
    pub api_version: String,
    /// Give it the cluster's details in `KUBERNETES_EXEC_INFO`.
    pub provide_cluster_info: bool,
}

/// Where a cluster's API server is and how to talk to it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Endpoint {
    /// The API server's URL, without a trailing slash.
    pub server: String,
    /// The CA bundle to trust, PEM; the system's roots when `None`.
    pub ca: Option<Vec<u8>>,
    /// Skip TLS verification.
    pub insecure: bool,
    /// The name to verify the server's certificate against, when not the URL's host.
    pub tls_server_name: Option<String>,
    /// Credentials.
    pub auth: Auth,
    /// The context's default namespace.
    pub namespace: Option<String>,
}

fn b64(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(s.trim()).context("base64 data")
}

fn file_or_data(m: &Value, data_key: &str, file_key: &str, base: &Path) -> Result<Option<Vec<u8>>> {
    if let Some(d) = m.get(data_key).and_then(Value::as_str).filter(|d| !d.is_empty()) {
        return Ok(Some(b64(d)?));
    }
    if let Some(f) = m.get(file_key).and_then(Value::as_str).filter(|f| !f.is_empty()) {
        let p = base.join(f);
        return Ok(Some(std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?));
    }
    Ok(None)
}

fn named<'a>(list: Option<&'a Value>, name: &str, inner: &str) -> Option<&'a Value> {
    list?.as_array()?.iter().find(|e| e.get("name").and_then(Value::as_str) == Some(name))?.get(inner)
}

/// Reads a kubeconfig's context (the current one when `context` is empty). Relative paths
/// resolve against `dir`, the file's directory.
pub fn kubeconfig(text: &str, context: &str, dir: &Path) -> Result<Endpoint> {
    let doc: Value = serde_yaml::from_str(text).context("parsing the kubeconfig")?;
    let ctx_name = if context.is_empty() {
        doc.get("current-context").and_then(Value::as_str).filter(|c| !c.is_empty()).context("the kubeconfig has no current context")?
    } else {
        context
    };
    let ctx = named(doc.get("contexts"), ctx_name, "context").with_context(|| format!("no context {ctx_name:?}"))?;
    let cluster_name = ctx.get("cluster").and_then(Value::as_str).unwrap_or("");
    let cluster = named(doc.get("clusters"), cluster_name, "cluster").with_context(|| format!("no cluster {cluster_name:?}"))?;
    let server = cluster.get("server").and_then(Value::as_str).context("the cluster has no server")?;
    let mut ep = Endpoint {
        server: server.trim_end_matches('/').to_owned(),
        ca: file_or_data(cluster, "certificate-authority-data", "certificate-authority", dir)?,
        insecure: cluster.get("insecure-skip-tls-verify").and_then(Value::as_bool).unwrap_or(false),
        tls_server_name: cluster.get("tls-server-name").and_then(Value::as_str).map(str::to_owned),
        auth: Auth::None,
        namespace: ctx.get("namespace").and_then(Value::as_str).map(str::to_owned),
    };
    let user_name = ctx.get("user").and_then(Value::as_str).unwrap_or("");
    if let Some(user) = named(doc.get("users"), user_name, "user") {
        ep.auth = user_auth(user, dir)?;
    }
    Ok(ep)
}

fn user_auth(user: &Value, dir: &Path) -> Result<Auth> {
    if let Some(exec) = user.get("exec").filter(|e| e.is_object()) {
        return Ok(Auth::Exec(Exec {
            command: exec.get("command").and_then(Value::as_str).context("exec has no command")?.to_owned(),
            args: exec
                .get("args")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default(),
            env: exec
                .get("env")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|e| Some((e.get("name")?.as_str()?.to_owned(), e.get("value")?.as_str().unwrap_or("").to_owned())))
                        .collect()
                })
                .unwrap_or_default(),
            api_version: exec.get("apiVersion").and_then(Value::as_str).unwrap_or("client.authentication.k8s.io/v1").to_owned(),
            provide_cluster_info: exec.get("provideClusterInfo").and_then(Value::as_bool).unwrap_or(false),
        }));
    }
    if user.get("auth-provider").is_some() {
        bail!("kubeconfig auth-provider plugins were removed from Kubernetes; use an exec plugin (gke-gcloud-auth-plugin, kubelogin)");
    }
    if let Some(t) = user.get("token").and_then(Value::as_str).filter(|t| !t.is_empty()) {
        return Ok(Auth::Token(t.to_owned()));
    }
    if let Some(f) = user.get("tokenFile").and_then(Value::as_str).filter(|f| !f.is_empty()) {
        return Ok(Auth::TokenFile(dir.join(f)));
    }
    let cert = file_or_data(user, "client-certificate-data", "client-certificate", dir)?;
    let key = file_or_data(user, "client-key-data", "client-key", dir)?;
    if let (Some(cert), Some(key)) = (cert, key) {
        return Ok(Auth::Certificate { cert, key });
    }
    if let (Some(u), Some(p)) = (user.get("username").and_then(Value::as_str), user.get("password").and_then(Value::as_str)) {
        return Ok(Auth::Basic { username: u.to_owned(), password: p.to_owned() });
    }
    Ok(Auth::None)
}

/// The cluster kiln runs in, from its pod's service account; `None` outside a pod.
pub fn in_cluster() -> Option<Endpoint> {
    let host = std::env::var("KUBERNETES_SERVICE_HOST").ok()?;
    let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".into());
    let sa = Path::new("/var/run/secrets/kubernetes.io/serviceaccount");
    let token = sa.join("token");
    if !token.exists() {
        return None;
    }
    let host = if host.contains(':') { format!("[{host}]") } else { host };
    Some(Endpoint {
        server: format!("https://{host}:{port}"),
        ca: std::fs::read(sa.join("ca.crt")).ok(),
        insecure: false,
        tls_server_name: Some("kubernetes.default.svc".into()),
        auth: Auth::TokenFile(token),
        namespace: std::fs::read_to_string(sa.join("namespace")).ok().map(|s| s.trim().to_owned()),
    })
}

/// A credential plugin's answer.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Credential {
    /// A bearer token, if it gave one.
    pub token: Option<String>,
    /// A client certificate and key, if it gave them.
    pub cert: Option<(Vec<u8>, Vec<u8>)>,
    /// When it expires (RFC 3339), if it says.
    pub expires: Option<String>,
}

/// Reads a plugin's `ExecCredential` output.
pub fn exec_credential(stdout: &str) -> Result<Credential> {
    let v: Value = serde_json::from_str(stdout.trim()).context("the credential plugin printed no ExecCredential")?;
    let status = v.get("status").context("the ExecCredential has no status")?;
    let s = |k: &str| status.get(k).and_then(Value::as_str).filter(|x| !x.is_empty()).map(str::to_owned);
    let cert = match (s("clientCertificateData"), s("clientKeyData")) {
        (Some(c), Some(k)) => Some((c.into_bytes(), k.into_bytes())),
        _ => None,
    };
    let c = Credential { token: s("token"), cert, expires: s("expirationTimestamp") };
    if c.token.is_none() && c.cert.is_none() {
        bail!("the ExecCredential has neither a token nor a client certificate");
    }
    Ok(c)
}

/// What a kind is served as.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Resource {
    /// Group.
    pub group: String,
    /// The version kiln asks for (the group's preferred one).
    pub version: String,
    /// The resource's plural name in paths (`deployments`).
    pub plural: String,
    /// Whether objects live in namespaces.
    pub namespaced: bool,
    /// Whether it can be watched.
    pub watch: bool,
}

/// What discovery found: every kind a cluster serves.
#[derive(Clone, Debug, Default)]
pub struct Discovery {
    /// By group and kind.
    pub kinds: FxHashMap<GroupKind, Resource>,
    /// Every `group/version` and `group/version/Kind` served, for Helm's `--api-versions`.
    pub api_versions: Vec<String>,
}

impl Discovery {
    /// Reads aggregated discovery (`/api` and `/apis` with
    /// `Accept: application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList`).
    pub fn add_aggregated(&mut self, doc: &Value) {
        for group in doc.get("items").and_then(Value::as_array).into_iter().flatten() {
            let name = group.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("");
            // Versions come in preference order: the first is the group's preferred one.
            for (i, version) in group.get("versions").and_then(Value::as_array).into_iter().flatten().enumerate() {
                let v = version.get("version").and_then(Value::as_str).unwrap_or("");
                let gv = if name.is_empty() { v.to_owned() } else { format!("{name}/{v}") };
                self.api_versions.push(gv.clone());
                for r in version.get("resources").and_then(Value::as_array).into_iter().flatten() {
                    let Some(kind) = r.pointer("/responseKind/kind").and_then(Value::as_str) else { continue };
                    let plural = r.get("resource").and_then(Value::as_str).unwrap_or("");
                    if plural.contains('/') {
                        continue;
                    }
                    self.api_versions.push(format!("{gv}/{kind}"));
                    let gk = GroupKind::new(name, kind);
                    if i > 0 && self.kinds.contains_key(&gk) {
                        continue;
                    }
                    let verbs: Vec<&str> = r.get("verbs").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
                    self.kinds.insert(gk, Resource {
                        group: name.to_owned(),
                        version: v.to_owned(),
                        plural: plural.to_owned(),
                        namespaced: r.get("scope").and_then(Value::as_str) == Some("Namespaced"),
                        watch: verbs.contains(&"watch"),
                    });
                }
            }
        }
        self.api_versions.sort();
        self.api_versions.dedup();
    }

    /// The resource of a kind.
    pub fn get(&self, group: &str, kind: &str) -> Option<&Resource> {
        self.kinds.get(&GroupKind::new(group, kind))
    }
}

/// The path of a kind's collection (in `namespace`, if namespaced and given).
pub fn collection_path(r: &Resource, namespace: Option<&str>) -> String {
    let base = if r.group.is_empty() { format!("/api/{}", r.version) } else { format!("/apis/{}/{}", r.group, r.version) };
    match namespace.filter(|ns| r.namespaced && !ns.is_empty()) {
        Some(ns) => format!("{base}/namespaces/{}/{}", enc(ns), r.plural),
        None => format!("{base}/{}", r.plural),
    }
}

/// The path of one object.
pub fn object_path(r: &Resource, key: &ResKey) -> String {
    format!("{}/{}", collection_path(r, Some(&key.namespace)), enc(&key.name))
}

/// Percent-encodes a path segment or query value.
pub fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The query of a server-side apply (`PATCH`, `application/apply-patch+yaml`).
pub fn apply_query(manager: &str, force: bool, dry_run: bool) -> String {
    let mut q = format!("fieldManager={}", enc(manager));
    if force {
        q.push_str("&force=true");
    }
    if dry_run {
        q.push_str("&dryRun=All");
    }
    q
}

/// One event of a watch stream.
#[derive(Clone, Debug, PartialEq)]
pub enum WatchEvent {
    /// An object was added or modified.
    Upsert(Value),
    /// An object was deleted.
    Delete(Value),
    /// A bookmark: everything up to this version has been sent.
    Bookmark(String),
    /// The watch's version is too old (410 Gone): list again.
    Expired,
    /// An error status.
    Error(String),
}

/// Reads one line of a watch stream.
pub fn watch_event(line: &[u8]) -> Option<WatchEvent> {
    let v: Value = serde_json::from_slice(line).ok()?;
    let ty = v.get("type").and_then(Value::as_str)?.to_owned();
    let object = v.get("object").cloned().unwrap_or(Value::Null);
    Some(match ty.as_str() {
        "ADDED" | "MODIFIED" => WatchEvent::Upsert(object),
        "DELETED" => WatchEvent::Delete(object),
        "BOOKMARK" => WatchEvent::Bookmark(obj::str_at(&object, &["metadata", "resourceVersion"]).unwrap_or("").to_owned()),
        "ERROR" => {
            if obj::i64_at(&object, &["code"]) == Some(410) {
                WatchEvent::Expired
            } else {
                WatchEvent::Error(obj::str_at(&object, &["message"]).unwrap_or("watch error").to_owned())
            }
        }
        _ => return None,
    })
}

/// Splits a buffer of watch stream bytes into complete lines, keeping the rest.
pub fn take_lines(buf: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
        let rest = buf.split_off(pos + 1);
        let mut line = std::mem::replace(buf, rest);
        line.pop();
        if !line.is_empty() {
            out.push(line);
        }
    }
    out
}

/// A Kubernetes `Status` error, read from a failed response.
pub fn status_message(status: u16, body: &[u8]) -> String {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let msg = obj::str_at(&v, &["message"]).map(str::to_owned).unwrap_or_else(|| String::from_utf8_lossy(body).chars().take(300).collect());
    let reason = obj::str_at(&v, &["reason"]).unwrap_or("");
    if reason.is_empty() { format!("HTTP {status}: {msg}") } else { format!("HTTP {status} {reason}: {msg}") }
}

/// Labels kiln's watches select on.
pub fn kiln_selector() -> &'static str {
    "kiln.dev/app"
}

/// The environment a credential plugin gets (`KUBERNETES_EXEC_INFO` when it asks for the
/// cluster's details).
pub fn exec_env(exec: &Exec, ep: &Endpoint) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = exec.env.iter().cloned().collect();
    let mut info = serde_json::json!({"apiVersion": exec.api_version, "kind": "ExecCredential", "spec": {"interactive": false}});
    if exec.provide_cluster_info
        && let Some(spec) = info.get_mut("spec").and_then(Value::as_object_mut) {
            spec.insert(
                "cluster".into(),
                serde_json::json!({"server": ep.server, "certificate-authority-data": ep.ca.as_ref().map(|c| base64::engine::general_purpose::STANDARD.encode(c)), "insecure-skip-tls-verify": ep.insecure}),
            );
        }
    env.insert("KUBERNETES_EXEC_INFO".into(), info.to_string());
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const KUBECONFIG: &str = r#"
apiVersion: v1
kind: Config
current-context: kiln
clusters:
- name: kiln
  cluster:
    server: https://192.168.49.2:8443/
    certificate-authority-data: Q0E=
- name: eks
  cluster:
    server: https://ABC.gr7.us-east-1.eks.amazonaws.com
    certificate-authority: ca.pem
contexts:
- name: kiln
  context: { cluster: kiln, user: kiln, namespace: default }
- name: prod
  context: { cluster: eks, user: aws }
users:
- name: kiln
  user:
    client-certificate-data: Q0VSVA==
    client-key-data: S0VZ
- name: aws
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1beta1
      command: aws
      args: [eks, get-token, --cluster-name, prod]
      env: [{ name: AWS_PROFILE, value: prod }]
"#;

    #[test]
    fn kubeconfigs_read_like_kubectl() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        std::fs::write(tmp.path().join("ca.pem"), "EKS CA")?;
        let ep = kubeconfig(KUBECONFIG, "", tmp.path())?;
        assert_eq!(ep.server, "https://192.168.49.2:8443");
        assert_eq!(ep.ca.as_deref(), Some(&b"CA"[..]));
        assert_eq!(ep.auth, Auth::Certificate { cert: b"CERT".to_vec(), key: b"KEY".to_vec() });
        assert_eq!(ep.namespace.as_deref(), Some("default"));
        let prod = kubeconfig(KUBECONFIG, "prod", tmp.path())?;
        assert_eq!(prod.ca.as_deref(), Some(&b"EKS CA"[..]));
        match prod.auth {
            Auth::Exec(e) => {
                assert_eq!(e.command, "aws");
                assert_eq!(e.args, ["eks", "get-token", "--cluster-name", "prod"]);
                assert_eq!(e.env, [("AWS_PROFILE".to_owned(), "prod".to_owned())]);
            }
            other => bail!("expected exec, got {other:?}"),
        }
        assert!(kubeconfig(KUBECONFIG, "nope", tmp.path()).is_err());
        Ok(())
    }

    #[test]
    fn exec_credentials_give_tokens_or_certificates() -> Result<()> {
        let c = exec_credential(r#"{"kind":"ExecCredential","status":{"token":"t0k","expirationTimestamp":"2026-10-17T00:00:00Z"}}"#)?;
        assert_eq!(c.token.as_deref(), Some("t0k"));
        assert_eq!(c.expires.as_deref(), Some("2026-10-17T00:00:00Z"));
        assert!(exec_credential(r#"{"status":{}}"#).is_err());
        Ok(())
    }

    #[test]
    fn discovery_maps_kinds_to_paths() {
        let doc = json!({"items": [
            {"metadata": {"name": ""}, "versions": [{"version": "v1", "resources": [
                {"resource": "configmaps", "responseKind": {"kind": "ConfigMap"}, "scope": "Namespaced", "verbs": ["get", "list", "watch"]},
                {"resource": "namespaces", "responseKind": {"kind": "Namespace"}, "scope": "Cluster", "verbs": ["get", "list", "watch"]},
                {"resource": "pods/log", "responseKind": {"kind": "Pod"}, "scope": "Namespaced", "verbs": ["get"]}]}]},
            {"metadata": {"name": "apps"}, "versions": [
                {"version": "v1", "resources": [{"resource": "deployments", "responseKind": {"kind": "Deployment"}, "scope": "Namespaced", "verbs": ["watch"]}]},
                {"version": "v1beta2", "resources": [{"resource": "deployments", "responseKind": {"kind": "Deployment"}, "scope": "Namespaced", "verbs": ["watch"]}]}]}
        ]});
        let mut d = Discovery::default();
        d.add_aggregated(&doc);
        let dep = d.get("apps", "Deployment").cloned();
        assert_eq!(dep.as_ref().map(|r| r.version.as_str()), Some("v1"));
        let key = ResKey::new("apps", "Deployment", "web", "api");
        assert_eq!(dep.map(|r| object_path(&r, &key)), Some("/apis/apps/v1/namespaces/web/deployments/api".into()));
        let ns = d.get("", "Namespace").cloned();
        assert_eq!(ns.map(|r| collection_path(&r, Some("ignored"))), Some("/api/v1/namespaces".into()));
        assert!(d.api_versions.contains(&"apps/v1/Deployment".to_owned()));
        assert!(d.api_versions.contains(&"v1".to_owned()));
    }

    #[test]
    fn watch_streams_split_into_events() {
        let mut buf = b"{\"type\":\"ADDED\",\"object\":{\"kind\":\"ConfigMap\"}}\n{\"type\":\"BOOKMARK\",\"object\":{\"metadata\":{\"resourceVersion\":\"42\"}}}\n{\"type\":\"ERR".to_vec();
        let lines = take_lines(&mut buf);
        assert_eq!(lines.len(), 2);
        assert_eq!(buf, b"{\"type\":\"ERR");
        assert_eq!(lines.first().and_then(|l| watch_event(l)), Some(WatchEvent::Upsert(json!({"kind": "ConfigMap"}))));
        assert_eq!(lines.get(1).and_then(|l| watch_event(l)), Some(WatchEvent::Bookmark("42".into())));
        assert_eq!(watch_event(br#"{"type":"ERROR","object":{"code":410}}"#), Some(WatchEvent::Expired));
    }
}
