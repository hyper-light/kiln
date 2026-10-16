//! How the CLI (`kiln login`, `token`, `status`, `passwd`) reaches a kiln server as its admin.
//!
//! A machine can know several (URL, admin token) pairs: explicit flags or `KILN_URL` /
//! `KILN_ADMIN_TOKEN`, the credentials the cluster bootstrap wrote, the local server's token file,
//! and the `kiln-admin` secret of a Kubernetes install. Each candidate is checked against the server
//! (`/api/me` with the token) and the first one the server accepts is used, so a link or command
//! is never signed with a token that belongs to some other installation. Nothing here creates a
//! token: that is the server's job.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Config;

/// A server and the admin token it accepted.
#[derive(Clone, Debug)]
pub struct Target {
    pub url: String,
    pub token: String,
    /// Where the token came from, for messages.
    pub source: String,
    /// The URL the server advertises for other devices (its `public_url`).
    pub public_url: Option<String>,
}

/// The cluster bootstrap's credentials file (`deploy/cluster/bootstrap.sh`).
pub fn cluster_credentials_path() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from(".local/state"));
    state.join("kiln/cluster/credentials.env")
}

/// `KEY=value` lines (shell-style, optional quotes) from an env file.
fn env_file(path: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let l = l.strip_prefix("export ").unwrap_or(l);
            if l.starts_with('#') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
            Some((k.trim().to_owned(), v.to_owned()))
        })
        .collect()
}

fn local_url(cfg: &Config) -> String {
    let addr = &cfg.server.http_addr;
    let port = addr.rsplit_once(':').map_or("7070", |(_, p)| p);
    if addr.starts_with("0.0.0.0") || addr.starts_with("127.") || addr.starts_with("[::]") || addr.starts_with("localhost") {
        format!("http://localhost:{port}")
    } else {
        cfg.server.public_url.clone()
    }
}

fn kubectl_secret() -> Option<String> {
    let has_kiln_ctx = std::process::Command::new("kubectl")
        .args(["config", "get-contexts", "-o", "name"])
        .output()
        .ok()
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l.trim() == "kiln"));
    let mut cmd = std::process::Command::new("kubectl");
    if has_kiln_ctx {
        cmd.args(["--context", "kiln"]);
    }
    cmd.args(["-n", "kiln", "get", "secret", "kiln-admin", "-o", "jsonpath={.data.token}"]);
    let out = cmd.stderr(std::process::Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD.decode(out.stdout.trim_ascii()).ok()?;
    let t = String::from_utf8(raw).ok()?.trim().to_owned();
    (!t.is_empty()).then_some(t)
}

/// Every (URL, token) pair worth trying, most specific first.
pub fn candidates(cfg: &Config, url: Option<&str>, token: Option<&str>) -> Vec<Target> {
    let mut urls: Vec<String> = Vec::new();
    let creds = env_file(&cluster_credentials_path());
    let cred = |k: &str| creds.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()).filter(|v| !v.is_empty());
    if let Some(u) = url {
        urls.push(u.trim_end_matches('/').to_owned());
    } else {
        urls.extend(cred("KILN_URL"));
        urls.push(local_url(cfg));
        urls.push("http://localhost:7070".to_owned());
    }
    urls.dedup();
    let mut tokens: Vec<(String, String)> = Vec::new();
    if let Some(t) = token.map(str::trim).filter(|t| !t.is_empty()) {
        tokens.push((t.to_owned(), "--token / KILN_ADMIN_TOKEN".to_owned()));
    } else {
        if let Some(t) = cred("KILN_ADMIN_TOKEN") {
            tokens.push((t, format!("cluster credentials ({})", cluster_credentials_path().display())));
        }
        let file = crate::server::token_path(cfg);
        if let Ok(t) = std::fs::read_to_string(&file)
            && !t.trim().is_empty()
        {
            tokens.push((t.trim().to_owned(), format!("local server token ({})", file.display())));
        }
    }
    let mut out = Vec::new();
    for u in &urls {
        for (t, src) in &tokens {
            out.push(Target { url: u.clone(), token: t.clone(), source: src.clone(), public_url: None });
        }
    }
    out
}

/// Whether the server at `t.url` accepts `t.token`; fills in its advertised public URL.
async fn check(client: &reqwest::Client, t: &mut Target) -> Result<bool, String> {
    let resp = client
        .get(format!("{}/api/me", t.url))
        .bearer_auth(&t.token)
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .map_err(|e| format!("unreachable ({e})"))?;
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("not a kiln server ({e})"))?;
    t.public_url = v.get("public_url").and_then(|u| u.as_str()).map(str::to_owned);
    Ok(v.get("authenticated").and_then(serde_json::Value::as_bool).unwrap_or(false))
}

/// The first candidate the server accepts, or an error that lists what was tried.
pub async fn resolve(cfg: &Config, url: Option<&str>, token: Option<&str>) -> anyhow::Result<Target> {
    let client = reqwest::Client::new();
    let mut tried = Vec::new();
    for mut t in candidates(cfg, url, token) {
        match check(&client, &mut t).await {
            Ok(true) => return Ok(t),
            Ok(false) => tried.push(format!("  {} with {}: token rejected", t.url, t.source)),
            Err(why) => tried.push(format!("  {} with {}: {why}", t.url, t.source)),
        }
    }
    // Last resort: a Kubernetes install's secret (slow, so only when nothing local worked).
    if token.is_none()
        && let Some(secret) = kubectl_secret()
    {
        let urls: Vec<String> = match url {
            Some(u) => vec![u.trim_end_matches('/').to_owned()],
            None => vec!["http://localhost:7070".to_owned(), local_url(cfg)],
        };
        for u in urls {
            let mut t = Target { url: u, token: secret.clone(), source: "kubectl secret kiln/kiln-admin".to_owned(), public_url: None };
            match check(&client, &mut t).await {
                Ok(true) => return Ok(t),
                Ok(false) => tried.push(format!("  {} with {}: token rejected", t.url, t.source)),
                Err(why) => tried.push(format!("  {} with {}: {why}", t.url, t.source)),
            }
        }
    }
    if tried.is_empty() {
        anyhow::bail!(
            "no admin token found on this machine. Pass --url and --token (or KILN_URL / KILN_ADMIN_TOKEN), \
             or sign in with your password in the browser"
        );
    }
    anyhow::bail!(
        "no kiln server accepted an admin token from this machine. Tried:\n{}\n\
         Pass --url and --token (or KILN_URL / KILN_ADMIN_TOKEN) for another server.",
        tried.join("\n")
    )
}

/// Whether a URL is only reachable from this machine.
pub fn is_loopback(url: &str) -> bool {
    let host = url.split("://").nth(1).unwrap_or(url).split(['/', ':']).next().unwrap_or("");
    matches!(host, "localhost" | "127.0.0.1" | "[" | "" | "0.0.0.0") || host.starts_with("127.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_files_parse() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let p = dir.path().join("c.env");
        std::fs::write(&p, "# comment\nKILN_URL=http://localhost:7070\nexport KILN_ADMIN_TOKEN=\"abc\"\nEMPTY=\n")?;
        let kv = env_file(&p);
        assert!(kv.contains(&("KILN_URL".to_owned(), "http://localhost:7070".to_owned())));
        assert!(kv.contains(&("KILN_ADMIN_TOKEN".to_owned(), "abc".to_owned())));
        Ok(())
    }

    #[test]
    fn loopback() {
        assert!(is_loopback("http://localhost:7070"));
        assert!(is_loopback("http://127.0.0.1:7070/x"));
        assert!(!is_loopback("http://192.168.1.77:7070"));
        assert!(!is_loopback("https://ci.example"));
    }
}
