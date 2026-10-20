//! GitHub as a git host (docs/ci.md W14-R10, W23; docs/deploy-api.md §7): the GitHub App kiln
//! acts through.
//! - **The App.** `[ci.github]`'s id and private key, or the App GitHub's manifest flow made (its
//!   key sealed in the store). It signs itself with RS256 JWTs and acts through installation tokens
//!   limited to one repository and the permissions a call needs, kept until shortly before they
//!   expire.
//! - **The host's calls** (`super::GitHost`): reading, committing through GitHub's Git Data API
//!   (blobs, a tree, a commit, then the branch moved only as a fast-forward of the commit it was made
//!   on), pushing, pull requests, statuses, deployments, templates, push webhooks.
//! - **Check runs for KilnCI's runs** of repositories on GitHub: one per run, made when it's
//!   queued and done with its conclusion; a new attempt gets a new check run, as GitHub Actions'
//!   re-runs do.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::KeyPair;
use aws_lc_rs::signature::RSA_PKCS1_SHA256;
use base64::Engine as _;
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::{Author, ChangeRef, ChangeState, CommitError, CommitStatus, Committed, Push, Reader};
use crate::http::client::Method;
use crate::hub::Hub;
use crate::store::Repo;

const B64URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// The App.
pub struct App {
    pub id: u64,
    key: KeyPair,
    /// GitHub's REST API (`https://api.github.com`, or a GitHub Enterprise Server's `/api/v3`).
    pub api: String,
    /// GitHub's web origin.
    pub web: String,
    pub webhook_secret: Option<String>,
    pub slug: Option<String>,
    pub html_url: Option<String>,
}

/// An App's private key: PKCS#1 (`BEGIN RSA PRIVATE KEY`, as GitHub hands them out) or PKCS#8.
pub fn key_from_pem(pem: &str) -> anyhow::Result<KeyPair> {
    let body: String = pem.lines().map(str::trim).filter(|l| !l.starts_with("-----") && !l.is_empty()).collect();
    let der = B64.decode(body).context("the App's private key isn't PEM")?;
    if pem.contains("BEGIN RSA PRIVATE KEY") {
        KeyPair::from_der(&der).map_err(|_| anyhow!("the App's private key (PKCS#1) doesn't read"))
    } else {
        KeyPair::from_pkcs8(&der).map_err(|_| anyhow!("the App's private key (PKCS#8) doesn't read"))
    }
}

fn cache() -> &'static parking_lot::Mutex<Option<(Instant, Option<Arc<App>>)>> {
    static C: OnceLock<parking_lot::Mutex<Option<(Instant, Option<Arc<App>>)>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// The App kiln acts through: `[ci.github]`'s, else the one the manifest flow made; read again at
/// most every minute.
pub async fn app(hub: &Hub) -> Option<Arc<App>> {
    if let Some((at, a)) = cache().lock().as_ref()
        && at.elapsed() < Duration::from_secs(60)
    {
        return a.clone();
    }
    let a = match load(hub).await {
        Ok(a) => a.map(Arc::new),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "the GitHub App can't be read");
            None
        }
    };
    *cache().lock() = Some((Instant::now(), a.clone()));
    a
}

/// Forgets the App read last (the manifest flow made one).
pub fn forget() {
    *cache().lock() = None;
}

const KEY_AAD: &[u8] = b"github-app-key";
const SECRET_AAD: &[u8] = b"github-app-webhook-secret";
const CLIENT_AAD: &[u8] = b"github-app-client-secret";

async fn load(hub: &Hub) -> anyhow::Result<Option<App>> {
    let c = &hub.cfg.ci.github;
    let (api, web) = (c.api_url.trim_end_matches('/').to_owned(), c.web_url.trim_end_matches('/').to_owned());
    if let (Some(id), Some(file)) = (c.app_id, c.private_key_file.clone()) {
        let pem = crate::rt::blocking(move || std::fs::read_to_string(&file).with_context(|| format!("{}", file.display()))).await??;
        let secret = match c.webhook_secret_file.clone() {
            Some(f) => crate::rt::blocking(move || std::fs::read_to_string(f).ok()).await?.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()),
            None => None,
        };
        return Ok(Some(App { id, key: key_from_pem(&pem)?, api, web, webhook_secret: secret, slug: None, html_url: None }));
    }
    #[allow(clippy::type_complexity)]
    let row: Option<(i64, Option<String>, Option<String>, Vec<u8>, Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>)> = hub
        .store
        .call(|c| Ok(c.query_row("SELECT app_id, slug, html_url, key_sealed, key_nonce, secret_sealed, secret_nonce FROM github_app WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))).optional()?))
        .await?;
    let Some((id, slug, html_url, ks, kn, ss, sn)) = row else { return Ok(None) };
    let pem = String::from_utf8(crate::ci::secrets::open_master(hub, &ks, &kn, KEY_AAD)?)?;
    let webhook_secret = match (ss, sn) {
        (Some(s), Some(n)) => Some(String::from_utf8(crate::ci::secrets::open_master(hub, &s, &n, SECRET_AAD)?)?),
        _ => None,
    };
    Ok(Some(App { id: u64::try_from(id)?, key: key_from_pem(&pem)?, api, web, webhook_secret, slug, html_url }))
}

/// Keeps an App GitHub's manifest flow made (`/app-manifests/{code}/conversions`' answer).
pub async fn keep(hub: &Hub, made: &Value) -> anyhow::Result<u64> {
    let id = made.get("id").and_then(Value::as_u64).ok_or_else(|| anyhow!("GitHub's answer has no App id"))?;
    let pem = made.get("pem").and_then(Value::as_str).ok_or_else(|| anyhow!("GitHub's answer has no private key"))?;
    key_from_pem(pem)?;
    let (ks, kn) = crate::ci::secrets::seal_master(hub, pem.as_bytes(), KEY_AAD)?;
    let secret = made.get("webhook_secret").and_then(Value::as_str).filter(|s| !s.is_empty());
    let (ss, sn) = match secret {
        Some(s) => {
            let (a, b) = crate::ci::secrets::seal_master(hub, s.as_bytes(), SECRET_AAD)?;
            (Some(a), Some(b))
        }
        None => (None, None),
    };
    let (slug, html, client) = (made.get("slug").and_then(Value::as_str).map(str::to_owned), made.get("html_url").and_then(Value::as_str).map(str::to_owned), made.get("client_id").and_then(Value::as_str).map(str::to_owned));
    // Its OAuth client's secret, for signing people in with GitHub (W23-R08).
    let (cs, cn) = match made.get("client_secret").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        Some(s) => {
            let (a, b) = crate::ci::secrets::seal_master(hub, s.as_bytes(), CLIENT_AAD)?;
            (Some(a), Some(b))
        }
        None => (None, None),
    };
    let idi = i64::try_from(id)?;
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT OR REPLACE INTO github_app (id, app_id, slug, client_id, html_url, key_sealed, key_nonce, secret_sealed, secret_nonce, client_secret_sealed, client_secret_nonce, created_at) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![idi, slug, client, html, ks, kn, ss, sn, cs, cn, crate::util::now_unix()],
            )?;
            Ok(())
        })
        .await?;
    forget();
    Ok(id)
}

/// How a call is made.
pub enum Auth<'a> {
    /// As the App itself (its JWT).
    App,
    Token(&'a str),
    None,
}

/// What GitHub said when a call didn't do what was asked.
pub fn said(v: &Value) -> String {
    v.get("message").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| crate::util::truncate(&v.to_string(), 300).to_owned())
}

impl App {
    /// A JWT that is the App itself (GitHub takes ten minutes at most).
    pub fn jwt(&self) -> anyhow::Result<String> {
        let now = crate::util::now_unix();
        let head = B64URL.encode(serde_json::to_vec(&json!({ "alg": "RS256", "typ": "JWT" }))?);
        let body = B64URL.encode(serde_json::to_vec(&json!({ "iat": now - 60, "exp": now + 540, "iss": self.id }))?);
        let signing = format!("{head}.{body}");
        let mut sig = vec![0u8; self.key.public_modulus_len()];
        self.key.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), signing.as_bytes(), &mut sig).map_err(|_| anyhow!("signing the App's JWT failed"))?;
        Ok(format!("{signing}.{}", B64URL.encode(sig)))
    }

    /// The `owner/name` on GitHub of a repository whose clone URL is on this App's GitHub.
    pub fn name_of(&self, repo: &Repo) -> Option<String> {
        let (host, path) = super::split_url(&repo.clone_url)?;
        (host == super::host_of(&self.web)).then_some(path)
    }

    fn name(&self, repo: &Repo) -> anyhow::Result<String> {
        self.name_of(repo).ok_or_else(|| anyhow!("{} isn't on {}", repo.name, self.web))
    }

    /// One call to GitHub's API: its status and JSON (`null` when none).
    pub async fn call(&self, method: Method, path: &str, auth: Auth<'_>, body: Option<&Value>) -> anyhow::Result<(u16, Value)> {
        let client = crate::http::client::github()?;
        let url = if path.starts_with("http://") || path.starts_with("https://") { path.to_owned() } else { format!("{}{path}", self.api) };
        let mut req = client.request(method, &url).header("Accept", "application/vnd.github+json").header("X-GitHub-Api-Version", "2022-11-28").header("User-Agent", "kiln").timeout(Duration::from_secs(30));
        req = match auth {
            Auth::App => req.bearer(&self.jwt()?),
            Auth::Token(t) => req.bearer(t),
            Auth::None => req,
        };
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.with_context(|| format!("GitHub: {url}"))?;
        let status = resp.status();
        let bytes = resp.bytes(32 << 20).await?;
        let v = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())) };
        Ok((status, v))
    }

    /// The App's installation for `owner/name`, or for an owner (`name` without a `/`).
    pub async fn installation(&self, name: &str) -> anyhow::Result<u64> {
        static C: OnceLock<parking_lot::Mutex<HashMap<String, (Instant, u64)>>> = OnceLock::new();
        let cache = C.get_or_init(Default::default);
        if let Some((at, id)) = cache.lock().get(name)
            && at.elapsed() < Duration::from_secs(600)
        {
            return Ok(*id);
        }
        let path = match name.split_once('/') {
            Some(_) => format!("/repos/{name}/installation"),
            None => format!("/users/{name}/installation"),
        };
        let (st, v) = self.call(Method::Get, &path, Auth::App, None).await?;
        if st != 200 {
            let install = self.html_url.as_deref().map(|u| format!(": install it at {u}/installations/new")).unwrap_or_default();
            bail!("the GitHub App isn't installed for {name} ({st}){install}");
        }
        let id = v.get("id").and_then(Value::as_u64).ok_or_else(|| anyhow!("GitHub's installation has no id"))?;
        cache.lock().insert(name.to_owned(), (Instant::now(), id));
        Ok(id)
    }

    /// A token of the installation for `name` (one repository, `owner/repo`, or every one an
    /// owner's installation covers) with `permissions` (GitHub's names, `read` or `write`), kept
    /// until five minutes before it expires.
    pub async fn token(&self, name: &str, permissions: &BTreeMap<String, String>) -> anyhow::Result<String> {
        static C: OnceLock<parking_lot::Mutex<HashMap<String, (i64, String)>>> = OnceLock::new();
        let cache = C.get_or_init(Default::default);
        let key = format!("{}|{name}|{}", self.id, serde_json::to_string(permissions).unwrap_or_default());
        let now = crate::util::now_unix();
        if let Some((until, t)) = cache.lock().get(&key)
            && *until > now + 300
        {
            return Ok(t.clone());
        }
        let inst = self.installation(name).await?;
        let mut body = json!({ "permissions": permissions });
        if let (Some((_, repo)), Some(o)) = (name.split_once('/'), body.as_object_mut()) {
            o.insert("repositories".into(), json!([repo]));
        }
        let (st, v) = self.call(Method::Post, &format!("/app/installations/{inst}/access_tokens"), Auth::App, Some(&body)).await?;
        if st != 201 {
            bail!("GitHub made no token for {name} ({st}): {}", said(&v));
        }
        let token = v.get("token").and_then(Value::as_str).ok_or_else(|| anyhow!("GitHub's token answer has no token"))?.to_owned();
        let until = v.get("expires_at").and_then(Value::as_str).and_then(|s| time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()).map_or(now + 3000, time::OffsetDateTime::unix_timestamp);
        cache.lock().insert(key, (until, token.clone()));
        Ok(token)
    }

    async fn token_for(&self, repo: &Repo, perms: &[(&str, &str)]) -> anyhow::Result<(String, String)> {
        let name = self.name(repo)?;
        let p: BTreeMap<String, String> = perms.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        let t = self.token(&name, &p).await?;
        Ok((name, t))
    }

    /// How git reads it: GitHub's URL and the installation's token as git's password.
    pub async fn reader(&self, repo: &Repo) -> anyhow::Result<Reader> {
        let (name, t) = self.token_for(repo, &[("contents", "read"), ("metadata", "read")]).await?;
        let basic = B64.encode(format!("x-access-token:{t}"));
        Ok(Reader { url: format!("{}/{name}.git", self.web), header: Some(("Authorization".into(), format!("Basic {basic}"))) })
    }

    /// The URL git pushes to, its token in it (for a push that leaves no config behind).
    pub async fn push_url(&self, repo: &Repo) -> anyhow::Result<String> {
        let (name, t) = self.token_for(repo, &[("contents", "write"), ("metadata", "read")]).await?;
        let rest = self.web.split_once("://").map_or(self.web.as_str(), |(_, r)| r);
        let scheme = self.web.split_once("://").map_or("https", |(s, _)| s);
        Ok(format!("{scheme}://x-access-token:{t}@{rest}/{name}.git"))
    }

    /// Commits through the Git Data API: blobs, a tree on `expected`'s, a commit, and the branch
    /// moved only as a fast-forward (so never past someone else's commit).
    pub async fn commit(&self, repo: &Repo, branch: &str, expected: Option<&str>, files: &[super::Change], message: &str, author: &Author) -> Result<Committed, CommitError> {
        self.commit_on(repo, branch, expected, expected, files, message, author).await
    }

    /// [`App::commit`], the commit made on `base` (none: an empty tree) and the branch at
    /// `expected` (none: made by it), as a new branch off another's head is. A workflow file is
    /// written only with the `workflows` permission, as GitHub refuses it otherwise.
    #[allow(clippy::too_many_arguments)]
    pub async fn commit_on(&self, repo: &Repo, branch: &str, base: Option<&str>, expected: Option<&str>, files: &[super::Change], message: &str, author: &Author) -> Result<Committed, CommitError> {
        let mut perms = vec![("contents", "write"), ("metadata", "read")];
        if files.iter().any(|f| f.path.starts_with(".github/workflows/")) {
            perms.push(("workflows", "write"));
        }
        let (name, t) = self.token_for(repo, &perms).await?;
        let base_tree = match base {
            Some(e) => {
                let (st, v) = self.call(Method::Get, &format!("/repos/{name}/git/commits/{e}"), Auth::Token(&t), None).await?;
                if st != 200 {
                    return Err(CommitError::Other(anyhow!("{e}: {}", said(&v))));
                }
                v.pointer("/tree/sha").and_then(Value::as_str).map(str::to_owned)
            }
            None => None,
        };
        let mut entries = Vec::new();
        for f in files {
            let sha = match &f.content {
                Some(bytes) => {
                    let (st, v) = self.call(Method::Post, &format!("/repos/{name}/git/blobs"), Auth::Token(&t), Some(&json!({ "content": B64.encode(bytes), "encoding": "base64" }))).await?;
                    if st != 201 {
                        return Err(CommitError::Other(anyhow!("{}: {}", f.path, said(&v))));
                    }
                    v.get("sha").cloned().unwrap_or(Value::Null)
                }
                None => Value::Null,
            };
            entries.push(json!({ "path": f.path, "mode": "100644", "type": "blob", "sha": sha }));
        }
        let mut tree_body = json!({ "tree": entries });
        if let (Some(b), Some(o)) = (&base_tree, tree_body.as_object_mut()) {
            o.insert("base_tree".into(), json!(b));
        }
        let (st, v) = self.call(Method::Post, &format!("/repos/{name}/git/trees"), Auth::Token(&t), Some(&tree_body)).await?;
        if st != 201 {
            return Err(CommitError::Other(anyhow!("the tree: {}", said(&v))));
        }
        let tree = v.get("sha").and_then(Value::as_str).unwrap_or_default().to_owned();
        if let (Some(e), Some(b)) = (base, &base_tree)
            && *b == tree
        {
            return Ok(Committed::Unchanged(e.to_owned()));
        }
        let parents: Vec<&str> = base.into_iter().collect();
        let who = json!({ "name": author.name, "email": author.email });
        let (st, v) = self.call(Method::Post, &format!("/repos/{name}/git/commits"), Auth::Token(&t), Some(&json!({ "message": message, "tree": tree, "parents": parents, "author": who, "committer": who }))).await?;
        if st != 201 {
            return Err(CommitError::Other(anyhow!("the commit: {}", said(&v))));
        }
        let sha = v.get("sha").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (st, v) = match expected {
            Some(_) => self.call(Method::Patch, &format!("/repos/{name}/git/refs/heads/{branch}"), Auth::Token(&t), Some(&json!({ "sha": sha, "force": false }))).await?,
            None => self.call(Method::Post, &format!("/repos/{name}/git/refs"), Auth::Token(&t), Some(&json!({ "ref": format!("refs/heads/{branch}"), "sha": sha }))).await?,
        };
        match st {
            200 | 201 => Ok(Committed::New(sha)),
            // Not a fast-forward of what it was made on, or the branch is there already.
            409 | 422 => Err(CommitError::Moved),
            _ => Err(CommitError::Other(anyhow!("{branch}: {}", said(&v)))),
        }
    }

    pub async fn open_change(&self, repo: &Repo, head: &str, base: &str, title: &str, body: &str) -> anyhow::Result<ChangeRef> {
        let (name, t) = self.token_for(repo, &[("pull_requests", "write"), ("contents", "read"), ("metadata", "read")]).await?;
        let (st, v) = self.call(Method::Post, &format!("/repos/{name}/pulls"), Auth::Token(&t), Some(&json!({ "title": title, "head": head, "base": base, "body": body }))).await?;
        if st != 201 {
            bail!("GitHub opened no pull request ({st}): {}", said(&v));
        }
        Ok(ChangeRef { number: v.get("number").and_then(Value::as_u64).unwrap_or(0), url: v.get("html_url").and_then(Value::as_str).unwrap_or_default().to_owned() })
    }

    pub async fn change(&self, repo: &Repo, number: u64) -> anyhow::Result<ChangeState> {
        let (name, t) = self.token_for(repo, &[("pull_requests", "read"), ("metadata", "read")]).await?;
        let (st, v) = self.call(Method::Get, &format!("/repos/{name}/pulls/{number}"), Auth::Token(&t), None).await?;
        if st != 200 {
            bail!("pull request {number} ({st}): {}", said(&v));
        }
        Ok(if v.get("merged").and_then(Value::as_bool) == Some(true) || v.get("merged_at").is_some_and(|m| !m.is_null()) {
            ChangeState::Merged { commit: v.get("merge_commit_sha").and_then(Value::as_str).map(str::to_owned) }
        } else if v.get("state").and_then(Value::as_str) == Some("closed") {
            ChangeState::Closed
        } else {
            ChangeState::Open
        })
    }

    pub async fn status(&self, repo: &Repo, sha: &str, s: &CommitStatus) -> anyhow::Result<()> {
        let (name, t) = self.token_for(repo, &[("statuses", "write"), ("metadata", "read")]).await?;
        let (st, v) = self.call(Method::Post, &format!("/repos/{name}/statuses/{sha}"), Auth::Token(&t), Some(&json!({ "state": s.state, "context": s.context, "description": crate::util::truncate(&s.description, 140), "target_url": s.url }))).await?;
        anyhow::ensure!(st == 201, "GitHub took no status ({st}): {}", said(&v));
        Ok(())
    }

    /// A deployment of `sha` to `environment` (the latest one there is, else a new one), and its state.
    pub async fn deployment(&self, repo: &Repo, sha: &str, environment: &str, state: &str, url: Option<&str>) -> anyhow::Result<()> {
        let (name, t) = self.token_for(repo, &[("deployments", "write"), ("metadata", "read")]).await?;
        let (st, list) = self.call(Method::Get, &format!("/repos/{name}/deployments?sha={sha}&environment={}", crate::ci::cli::enc(environment)), Auth::Token(&t), None).await?;
        let found = (st == 200).then(|| list.as_array().and_then(|l| l.first()).and_then(|d| d.get("id")).and_then(Value::as_u64)).flatten();
        let id = match found {
            Some(id) => id,
            None => {
                let (st, v) = self.call(Method::Post, &format!("/repos/{name}/deployments"), Auth::Token(&t), Some(&json!({ "ref": sha, "environment": environment, "auto_merge": false, "required_contexts": [], "description": "kiln" }))).await?;
                anyhow::ensure!(st == 201, "GitHub made no deployment ({st}): {}", said(&v));
                v.get("id").and_then(Value::as_u64).ok_or_else(|| anyhow!("GitHub's deployment has no id"))?
            }
        };
        let mut body = json!({ "state": state, "environment": environment, "description": "kiln" });
        if let (Some(u), Some(o)) = (url, body.as_object_mut()) {
            o.insert("environment_url".into(), json!(u));
        }
        let (st, v) = self.call(Method::Post, &format!("/repos/{name}/deployments/{id}/statuses"), Auth::Token(&t), Some(&body)).await?;
        anyhow::ensure!(st == 201, "GitHub took no deployment status ({st}): {}", said(&v));
        Ok(())
    }

    /// `owner/name` made from the template repository `template` (`owner/name`).
    pub async fn create_from_template(&self, template: &str, name: &str, private: bool) -> anyhow::Result<()> {
        let (owner, repo) = name.split_once('/').ok_or_else(|| anyhow!("`{name}`: owner/name"))?;
        let p: BTreeMap<String, String> = [("administration".to_owned(), "write".to_owned()), ("contents".to_owned(), "read".to_owned()), ("metadata".to_owned(), "read".to_owned())].into();
        let t = self.token(owner, &p).await?;
        let (st, v) = self.call(Method::Post, &format!("/repos/{template}/generate"), Auth::Token(&t), Some(&json!({ "owner": owner, "name": repo, "private": private }))).await?;
        anyhow::ensure!(st == 201, "GitHub made no repository ({st}): {}", said(&v));
        Ok(())
    }
}

/// A push webhook's ref update.
pub fn push_event(headers: &http::HeaderMap, body: &[u8]) -> Option<Push> {
    if headers.get("x-github-event").and_then(|v| v.to_str().ok()) != Some("push") {
        return None;
    }
    let v = super::json_body(body)?;
    let s = |p: &str| v.pointer(p).and_then(Value::as_str).map(str::to_owned);
    Some(Push { repo: s("/repository/full_name")?, refname: s("/ref")?, before: s("/before")?, after: s("/after")? })
}

/// kiln's job permissions as a GitHub App's (W14-R12): the same names, `_` for `-`; `id-token` is
/// kiln's own.
pub fn permissions_of(p: &BTreeMap<String, kiln_ci::plan::perms::Level>) -> BTreeMap<String, String> {
    use kiln_ci::plan::perms::Level;
    let mut out: BTreeMap<String, String> = p
        .iter()
        .filter(|(k, _)| k.as_str() != "id-token")
        .filter_map(|(k, l)| {
            let level = match l {
                Level::Read => "read",
                Level::Write => "write",
                Level::None => return None,
            };
            Some((k.replace('-', "_"), level.to_owned()))
        })
        .collect();
    out.entry("metadata".into()).or_insert_with(|| "read".into());
    out
}

// ------------------------------------------------------------------ check runs

/// The check-run state a run is in: (GitHub's `status`, its `conclusion` when completed).
fn check_state(status: &str, conclusion: Option<&str>) -> (&'static str, Option<String>) {
    match status {
        "pending" | "queued" => ("queued", None),
        "running" => ("in_progress", None),
        _ => {
            let c = match conclusion.unwrap_or(status) {
                "success" | "succeeded" => "success",
                "failure" | "failed" | "startup_failure" => "failure",
                "cancelled" => "cancelled",
                "skipped" => "skipped",
                "timed_out" => "timed_out",
                "action_required" => "action_required",
                _ => "neutral",
            };
            ("completed", Some(c.to_owned()))
        }
    }
}

/// Brings GitHub's check run for KilnCI run `run_id` to where the run is.
pub async fn report(hub: &Hub, run_id: &str) -> anyhow::Result<()> {
    let Some(app) = app(hub).await else { return Ok(()) };
    let id = run_id.to_owned();
    #[allow(clippy::type_complexity)]
    let row: Option<(String, String, String, String, String, Option<String>, Option<String>, i64)> = hub
        .store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT repo_id, sha, pipeline, COALESCE(run_name, pipeline), status, conclusion, workflow_id, attempt FROM runs WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
            )
            .optional()?)
        })
        .await?;
    // KilnCI's runs (v1's report statuses with their token, as they did).
    let Some((repo_id, sha, path, title, status, conclusion, Some(_workflow), attempt)) = row else { return Ok(()) };
    let repos = hub.store.repos().await?;
    let Some(repo) = repos.into_iter().find(|r| r.id == repo_id) else { return Ok(()) };
    let Some(name) = app.name_of(&repo) else { return Ok(()) };
    let (want, conclusion) = check_state(&status, conclusion.as_deref());
    let id = run_id.to_owned();
    let known: Option<(i64, String, i64)> = hub.store.call(move |c| Ok(c.query_row("SELECT check_id, state, attempt FROM github_checks WHERE run_id = ?1", [&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?)).await?;
    if let Some((_, state, at)) = &known
        && state == want
        && *at == attempt
    {
        return Ok(());
    }
    let t = app.token(&name, &[("checks".to_owned(), "write".to_owned()), ("metadata".to_owned(), "read".to_owned())].into()).await?;
    let details = format!("{}/runs/{run_id}", hub.cfg.server.public_url.trim_end_matches('/'));
    let label = if title.is_empty() { path.clone() } else { title.clone() };
    let mut body = json!({ "name": label, "head_sha": sha, "status": want, "details_url": details, "external_id": run_id });
    if let (Some(c), Some(o)) = (&conclusion, body.as_object_mut()) {
        o.insert("conclusion".into(), json!(c));
        o.insert("completed_at".into(), json!(rfc3339(crate::util::now_unix())));
        o.insert("output".into(), json!({ "title": format!("{label}: {c}"), "summary": format!("[The run on kiln]({details}), attempt {attempt}.") }));
    }
    // A new attempt of a finished run gets a check run of its own, as GitHub Actions' re-runs do.
    let fresh = match &known {
        None => true,
        Some((_, state, at)) => *at != attempt || (state == "completed" && want != "completed"),
    };
    let check = match (&known, fresh) {
        (Some((cid, _, _)), false) => {
            let (st, v) = app.call(Method::Patch, &format!("/repos/{name}/check-runs/{cid}"), Auth::Token(&t), Some(&body)).await?;
            anyhow::ensure!(st == 200, "GitHub took no check run update ({st}): {}", said(&v));
            *cid
        }
        _ => {
            let (st, v) = app.call(Method::Post, &format!("/repos/{name}/check-runs"), Auth::Token(&t), Some(&body)).await?;
            anyhow::ensure!(st == 201, "GitHub made no check run ({st}): {}", said(&v));
            v.get("id").and_then(Value::as_i64).ok_or_else(|| anyhow!("GitHub's check run has no id"))?
        }
    };
    let (id, repo_name, state) = (run_id.to_owned(), name, want.to_owned());
    hub.store
        .call(move |c| {
            c.execute("INSERT OR REPLACE INTO github_checks (run_id, repo, check_id, state, attempt, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![id, repo_name, check, state, attempt, crate::util::now_unix()])?;
            Ok(())
        })
        .await?;
    Ok(())
}

fn rfc3339(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default()
}

/// Keeps GitHub's check runs with KilnCI's runs, for as long as the server runs: each run's change
/// as it's broadcast, and every minute the ones GitHub may have missed.
pub async fn checks_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    let mut rx = hub.events.subscribe();
    loop {
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            m = rx.recv().fuse() => match m {
                Ok(text) => {
                    if let Some(id) = run_of_event(&text)
                        && app(&hub).await.is_some()
                        && let Err(e) = report(&hub, &id).await
                    {
                        tracing::warn!(run = %id, error = %format!("{e:#}"), "GitHub's check run didn't follow the run");
                    }
                }
                Err(async_broadcast::RecvError::Overflowed(_)) => reconcile(&hub).await,
                Err(async_broadcast::RecvError::Closed) => return Ok(()),
            },
            () = crate::rt::sleep(Duration::from_secs(60)).fuse() => reconcile(&hub).await,
        }
    }
}

fn run_of_event(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    (v.get("type").and_then(Value::as_str) == Some("run")).then(|| v.pointer("/run/id").and_then(Value::as_str).map(str::to_owned)).flatten()
}

/// The runs GitHub was told about that changed since.
async fn reconcile(hub: &Hub) {
    if app(hub).await.is_none() {
        return;
    }
    let ids: Vec<String> = hub
        .store
        .call(|c| {
            let mut st = c.prepare("SELECT g.run_id FROM github_checks g JOIN runs r ON r.id = g.run_id WHERE g.state != 'completed' OR g.attempt != r.attempt LIMIT 200")?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?;
            Ok(rows.filter_map(Result::ok).collect())
        })
        .await
        .unwrap_or_default();
    for id in ids {
        if let Err(e) = report(hub, &id).await {
            tracing::debug!(run = %id, error = %format!("{e:#}"), "a check run's catch-up");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::signature::KeyPair as _;

    #[test]
    fn w14_r10_jwt_and_permissions() -> anyhow::Result<()> {
        let kp = KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).map_err(|_| anyhow!("keygen"))?;
        let der = aws_lc_rs::encoding::AsDer::<aws_lc_rs::encoding::Pkcs8V1Der>::as_der(&kp).map_err(|_| anyhow!("der"))?;
        let pem = format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", B64.encode(der.as_ref()));
        let app = App { id: 42, key: key_from_pem(&pem)?, api: "https://api.github.com".into(), web: "https://github.com".into(), webhook_secret: None, slug: None, html_url: None };
        let jwt = app.jwt()?;
        let mut parts = jwt.split('.');
        let (h, b, s) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
        let claims: Value = serde_json::from_slice(&B64URL.decode(b)?)?;
        assert_eq!(claims.get("iss"), Some(&json!(42)));
        let pubkey = aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256, kp.public_key().as_ref().to_vec());
        pubkey.verify(format!("{h}.{b}").as_bytes(), &B64URL.decode(s)?).map_err(|_| anyhow!("the JWT's signature doesn't verify"))?;
        let repo = |url: &str| Repo { id: "r".into(), name: "acme/app".into(), clone_url: url.into(), branches: vec![], poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: None, mirror: None };
        assert_eq!(app.name_of(&repo("https://github.com/acme/app.git")).as_deref(), Some("acme/app"));
        assert_eq!(app.name_of(&repo("https://gitlab.com/acme/app.git")), None);
        use kiln_ci::plan::perms::Level;
        let p = permissions_of(&[("contents".to_owned(), Level::Write), ("pull-requests".to_owned(), Level::Read), ("id-token".to_owned(), Level::Write), ("issues".to_owned(), Level::None)].into());
        assert_eq!(p, [("contents".to_owned(), "write".to_owned()), ("metadata".to_owned(), "read".to_owned()), ("pull_requests".to_owned(), "read".to_owned())].into());
        assert_eq!(check_state("failed", Some("failure")), ("completed", Some("failure".into())));
        assert_eq!(check_state("running", None), ("in_progress", None));
        Ok(())
    }
}
