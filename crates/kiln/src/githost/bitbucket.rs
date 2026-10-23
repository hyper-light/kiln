//! Bitbucket Cloud as a git host (docs/ci.md W23-R05; docs/deploy-api.md §7), through its REST API
//! (2.0) with what `[ci.bitbucket]` gives: an access token (a workspace's, project's or
//! repository's), or a user and an app password or API token.
//! - **Reading and pushing**: git over HTTPS (`x-token-auth` with an access token; with an API
//!   token, whose user is an email, `x-bitbucket-api-token-auth`; else the user).
//! - **Commits** are made here, in the repository's mirror, and pushed with a lease on the head they
//!   were made on.
//! - **Pull requests** (opened, found by their branch, followed, declined with a comment), build
//!   statuses (a release's too: Bitbucket has no deployments API outside Pipelines), push webhooks
//!   (`X-Hub-Signature`), new repositories (empty, or with a template's history).
//! - **Deploy keys**: an SSH key that only reads one repository, made for a reader of kiln's
//!   (Argo CD): Bitbucket's API makes no token that only reads.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use serde_json::{Value, json};

use super::{Author, ChangeRef, ChangeState, CommitError, CommitStatus, Committed, Push, Reader};
use crate::http::client::Method;
use crate::hub::Hub;
use crate::store::Repo;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// How kiln signs in to Bitbucket.
enum Cred {
    /// An access token: `Bearer` for the API, `x-token-auth` for git.
    Token(String),
    /// A user and an app password or API token.
    Basic { user: String, password: String },
}

/// Bitbucket Cloud, as kiln acts on it.
pub struct Bitbucket {
    /// Its web origin (`https://bitbucket.org`).
    pub web: String,
    /// Its REST API (`https://api.bitbucket.org/2.0`).
    pub api: String,
    cred: Cred,
    pub webhook_secret: Option<String>,
    /// The workspaces it covers (none: every one).
    workspaces: Vec<String>,
    /// Where git reads over SSH (`ssh://git@bitbucket.org`).
    ssh: String,
}

fn cache() -> &'static parking_lot::Mutex<Option<(Instant, Option<Arc<Bitbucket>>)>> {
    static C: OnceLock<parking_lot::Mutex<Option<(Instant, Option<Arc<Bitbucket>>)>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// Bitbucket as `[ci.bitbucket]` sets it up, read again at most every minute.
pub async fn host(hub: &Hub) -> Option<Arc<Bitbucket>> {
    if let Some((at, b)) = cache().lock().as_ref()
        && at.elapsed() < Duration::from_secs(60)
    {
        return b.clone();
    }
    let b = match &hub.cfg.ci.bitbucket {
        Some(c) => match load(c).await {
            Ok(b) => Some(Arc::new(b)),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "Bitbucket's settings can't be read");
                None
            }
        },
        None => None,
    };
    *cache().lock() = Some((Instant::now(), b.clone()));
    b
}

async fn load(c: &crate::config::ci::BitbucketHost) -> anyhow::Result<Bitbucket> {
    let cred = match (&c.token_file, &c.username, &c.password_file) {
        (Some(f), _, _) => Cred::Token(super::read_secret(f).await?.ok_or_else(|| anyhow!("{} is empty", f.display()))?),
        (None, Some(user), Some(f)) => Cred::Basic { user: user.clone(), password: super::read_secret(f).await?.ok_or_else(|| anyhow!("{} is empty", f.display()))? },
        _ => bail!("[ci.bitbucket] needs token_file, or username and password_file"),
    };
    let webhook_secret = match &c.webhook_secret_file {
        Some(f) => super::read_secret(f).await?,
        None => None,
    };
    let web = c.web_url.trim_end_matches('/').to_owned();
    let ssh = c.ssh_url.as_deref().map_or_else(|| format!("ssh://git@{}", super::host_of(&web)), |u| u.trim_end_matches(['/', ':']).to_owned());
    Ok(Bitbucket { web, api: c.api_url.trim_end_matches('/').to_owned(), cred, webhook_secret, workspaces: c.workspaces.clone(), ssh })
}

/// git's `http.<url>.extraheader` settings for Bitbucket: `[ci.bitbucket]`'s credentials for what
/// they cover, read as git runs.
pub fn git_headers(hub: &Hub) -> Vec<(String, String)> {
    let Some(c) = &hub.cfg.ci.bitbucket else { return Vec::new() };
    let read = |f: &std::path::PathBuf| std::fs::read_to_string(f).ok().map(|t| t.trim().to_owned()).filter(|t| !t.is_empty());
    let cred = match (&c.token_file, &c.username, &c.password_file) {
        (Some(f), _, _) => read(f).map(Cred::Token),
        (None, Some(user), Some(f)) => read(f).map(|password| Cred::Basic { user: user.clone(), password }),
        _ => None,
    };
    let Some(cred) = cred else { return Vec::new() };
    let (u, p) = login_of(&cred);
    let (web, basic) = (c.web_url.trim_end_matches('/'), format!("Authorization: Basic {}", B64.encode(format!("{u}:{p}"))));
    if c.workspaces.is_empty() {
        return vec![(format!("{web}/"), basic)];
    }
    c.workspaces.iter().map(|w| (format!("{web}/{w}/"), basic.clone())).collect()
}

/// git's user and password for these credentials.
fn login_of(cred: &Cred) -> (String, String) {
    match cred {
        Cred::Token(t) => ("x-token-auth".into(), t.clone()),
        // An Atlassian API token signs in to the API with an email, and to git with this name.
        Cred::Basic { user, password } if user.contains('@') => ("x-bitbucket-api-token-auth".into(), password.clone()),
        Cred::Basic { user, password } => (user.clone(), password.clone()),
    }
}

/// Bitbucket, if a repository is on it, and the repository's `workspace/slug`.
pub async fn for_repo(hub: &Hub, repo: &Repo) -> Option<(Arc<Bitbucket>, String)> {
    let b = host(hub).await?;
    let name = b.name_of(repo)?;
    Some((b, name))
}

/// What Bitbucket said when a call didn't do what was asked.
pub fn said(v: &Value) -> String {
    v.pointer("/error/message")
        .and_then(Value::as_str)
        .map(|m| match v.pointer("/error/detail").and_then(Value::as_str) {
            Some(d) => format!("{m}: {d}"),
            None => m.to_owned(),
        })
        .unwrap_or_else(|| crate::util::truncate(&v.to_string(), 300).to_owned())
}

/// A path segment: Bitbucket's `{uuid}`s, encoded.
pub fn seg(s: &str) -> String {
    crate::ci::cli::enc(s)
}

impl Bitbucket {
    /// The `workspace/slug` of a repository on Bitbucket that kiln's credentials cover.
    pub fn name_of(&self, repo: &Repo) -> Option<String> {
        let (host, path) = super::split_url(&repo.clone_url)?;
        if host != super::host_of(&self.web) || path.split('/').count() != 2 {
            return None;
        }
        let workspace = path.split('/').next().unwrap_or_default();
        (self.workspaces.is_empty() || self.workspaces.iter().any(|w| w.eq_ignore_ascii_case(workspace))).then_some(path)
    }

    fn name(&self, repo: &Repo) -> anyhow::Result<String> {
        self.name_of(repo).ok_or_else(|| anyhow!("{} isn't on {}", repo.name, self.web))
    }

    fn api_auth(&self) -> String {
        match &self.cred {
            Cred::Token(t) => format!("Bearer {t}"),
            Cred::Basic { user, password } => format!("Basic {}", B64.encode(format!("{user}:{password}"))),
        }
    }

    /// git's user and password.
    fn git_login(&self) -> (String, String) {
        login_of(&self.cred)
    }

    /// One call to Bitbucket's API as kiln: its status and JSON (`null` when none).
    pub async fn call(&self, method: Method, path: &str, body: Option<&Value>) -> anyhow::Result<(u16, Value)> {
        let auth = self.api_auth();
        self.call_with(&auth, method, path, body).await
    }

    /// One call with someone's own token (W23-R08: what they do from kiln is theirs).
    pub async fn call_as(&self, token: &str, method: Method, path: &str, body: Option<&Value>) -> anyhow::Result<(u16, Value)> {
        self.call_with(&format!("Bearer {token}"), method, path, body).await
    }

    async fn call_with(&self, auth: &str, method: Method, path: &str, body: Option<&Value>) -> anyhow::Result<(u16, Value)> {
        let (status, bytes) = self.raw(auth, method, path, body, 32 << 20).await?;
        let v = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())) };
        Ok((status, v))
    }

    /// A call's bytes (a step's log), a redirect followed without kiln's credentials.
    pub async fn bytes(&self, path: &str, limit: usize) -> anyhow::Result<(u16, bytes::Bytes)> {
        let auth = self.api_auth();
        self.raw(&auth, Method::Get, path, None, limit).await
    }

    async fn raw(&self, auth: &str, method: Method, path: &str, body: Option<&Value>, limit: usize) -> anyhow::Result<(u16, bytes::Bytes)> {
        let client = crate::http::client::github()?;
        let url = if path.starts_with("http://") || path.starts_with("https://") { path.to_owned() } else { format!("{}{path}", self.api) };
        let mut req = client.request(method, &url).header("Authorization", auth).header("Accept", "application/json").timeout(Duration::from_secs(30));
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.with_context(|| format!("Bitbucket: {url}"))?;
        let status = resp.status();
        if (300..400).contains(&status)
            && let Some(to) = resp.header("location").map(str::to_owned)
        {
            let r = client.request(Method::Get, &to).timeout(Duration::from_secs(60)).send().await.with_context(|| format!("Bitbucket's redirect: {to}"))?;
            let st = r.status();
            return Ok((st, r.bytes(limit).await?));
        }
        Ok((status, resp.bytes(limit).await?))
    }

    /// How git reads it.
    pub fn reader(&self, repo: &Repo) -> anyhow::Result<Reader> {
        let name = self.name(repo)?;
        let (u, p) = self.git_login();
        Ok(Reader { url: format!("{}/{name}.git", self.web), header: Some(("Authorization".into(), format!("Basic {}", B64.encode(format!("{u}:{p}"))))) })
    }

    /// The URL git pushes to, the credentials in it.
    pub fn push_url(&self, repo: &Repo) -> anyhow::Result<String> {
        let (u, p) = self.git_login();
        Ok(super::with_credentials(&self.web, &u, &p, &self.name(repo)?))
    }

    /// Commits `files` onto `branch`, made here on `expected` and pushed with a lease on it.
    #[allow(clippy::too_many_arguments)]
    pub async fn commit(&self, hub: &Hub, repo: &Repo, branch: &str, expected: Option<&str>, files: &[super::Change], message: &str, author: &Author) -> Result<Committed, CommitError> {
        let url = self.push_url(repo)?;
        super::remote_commit(hub, repo, &url, branch, expected, files, message, author).await
    }

    pub async fn open_change(&self, repo: &Repo, head: &str, base: &str, title: &str, body: &str) -> anyhow::Result<ChangeRef> {
        let name = self.name(repo)?;
        let ask = json!({ "title": title, "description": body, "source": { "branch": { "name": head } }, "destination": { "branch": { "name": base } } });
        let (st, v) = self.call(Method::Post, &format!("/repositories/{name}/pullrequests"), Some(&ask)).await?;
        if st != 201 {
            bail!("Bitbucket opened no pull request ({st}): {}", said(&v));
        }
        Ok(ChangeRef { number: v.get("id").and_then(Value::as_u64).unwrap_or(0), url: v.pointer("/links/html/href").and_then(Value::as_str).unwrap_or_default().to_owned() })
    }

    pub async fn change(&self, repo: &Repo, number: u64) -> anyhow::Result<ChangeState> {
        let name = self.name(repo)?;
        let (st, v) = self.call(Method::Get, &format!("/repositories/{name}/pullrequests/{number}"), None).await?;
        if st != 200 {
            bail!("pull request #{number} ({st}): {}", said(&v));
        }
        Ok(pr_state(&v))
    }

    /// The pull request from the branch `head` of the repository, open or not, if there is one
    /// (the newest).
    pub async fn find_change(&self, repo: &Repo, head: &str) -> anyhow::Result<Option<(ChangeRef, ChangeState)>> {
        let name = self.name(repo)?;
        let q = crate::ci::cli::enc(&format!("source.branch.name=\"{head}\""));
        let (st, v) = self.call(Method::Get, &format!("/repositories/{name}/pullrequests?q={q}&state=OPEN&state=MERGED&state=DECLINED&state=SUPERSEDED"), None).await?;
        if st != 200 {
            bail!("the pull requests from {head} ({st}): {}", said(&v));
        }
        let newest = v.get("values").and_then(Value::as_array).and_then(|l| l.iter().max_by_key(|p| p.get("id").and_then(Value::as_u64).unwrap_or(0)));
        Ok(newest.map(|p| (ChangeRef { number: p.get("id").and_then(Value::as_u64).unwrap_or(0), url: p.pointer("/links/html/href").and_then(Value::as_str).unwrap_or_default().to_owned() }, pr_state(p))))
    }

    /// Declines pull request `number`, saying why (a comment) first.
    pub async fn close_change(&self, repo: &Repo, number: u64, why: &str) -> anyhow::Result<()> {
        let name = self.name(repo)?;
        if !why.is_empty() {
            let (st, v) = self.call(Method::Post, &format!("/repositories/{name}/pullrequests/{number}/comments"), Some(&json!({ "content": { "raw": why } }))).await?;
            if st != 201 {
                tracing::debug!(pull = number, status = st, said = %said(&v), "Bitbucket took no comment on a pull request kiln declines");
            }
        }
        let (st, v) = self.call(Method::Post, &format!("/repositories/{name}/pullrequests/{number}/decline"), None).await?;
        anyhow::ensure!(st == 200, "Bitbucket didn't decline pull request #{number} ({st}): {}", said(&v));
        Ok(())
    }

    /// Deletes a branch (gone already is fine). Its name's `/`s stay as they are in the path, as
    /// Bitbucket reads it.
    pub async fn delete_branch(&self, repo: &Repo, branch: &str) -> anyhow::Result<()> {
        let name = self.name(repo)?;
        let path: Vec<String> = branch.split('/').map(seg).collect();
        let (st, v) = self.call(Method::Delete, &format!("/repositories/{name}/refs/branches/{}", path.join("/")), None).await?;
        anyhow::ensure!(matches!(st, 204 | 404), "Bitbucket didn't delete {branch} ({st}): {}", said(&v));
        Ok(())
    }

    /// The repository `workspace/slug` as Bitbucket shows it to kiln (`None`: there's none it sees).
    pub async fn repository(&self, name: &str) -> anyhow::Result<Option<Value>> {
        let (st, v) = self.call(Method::Get, &format!("/repositories/{name}"), None).await?;
        match st {
            200 => Ok(Some(v)),
            404 => Ok(None),
            _ => bail!("Bitbucket didn't say whether {name} is there ({st}): {}", said(&v)),
        }
    }

    /// Makes `workspace/slug`, private and empty.
    pub async fn create_repository(&self, name: &str, description: &str) -> anyhow::Result<()> {
        anyhow::ensure!(name.split('/').count() == 2, "`{name}`: workspace/slug");
        let (st, v) = self.call(Method::Post, &format!("/repositories/{name}"), Some(&json!({ "scm": "git", "is_private": true, "description": description }))).await?;
        anyhow::ensure!(st == 200 || st == 201, "Bitbucket made no repository {name} ({st}): {}", said(&v));
        Ok(())
    }

    /// The workspaces kiln's credentials cover (none: every one).
    pub fn workspaces(&self) -> &[String] {
        &self.workspaces
    }

    /// Where git reads `workspace/slug` over SSH, as a deploy key does.
    pub fn ssh_url(&self, name: &str) -> String {
        format!("{}/{name}.git", self.ssh)
    }

    /// A deploy key of the repository `workspace/slug` (an SSH key that only reads it), for one
    /// reader: its id on Bitbucket, and its private key (OpenSSH's format), which kiln keeps
    /// nowhere. Making one takes the repository's admin.
    pub async fn deploy_key(&self, name: &str, label: &str) -> anyhow::Result<(u64, String)> {
        let (public, private) = ssh_key("kiln")?;
        let (st, v) = self.call(Method::Post, &format!("/repositories/{name}/deploy-keys"), Some(&json!({ "key": public, "label": label }))).await?;
        anyhow::ensure!(st == 200 || st == 201, "Bitbucket made no deploy key for {name} ({st}): {}", said(&v));
        let id = v.get("id").and_then(Value::as_u64).ok_or_else(|| anyhow!("Bitbucket's deploy key for {name} has no id"))?;
        Ok((id, private))
    }

    /// Deletes a deploy key (gone already is fine).
    pub async fn revoke_deploy_key(&self, name: &str, id: u64) -> anyhow::Result<()> {
        let (st, v) = self.call(Method::Delete, &format!("/repositories/{name}/deploy-keys/{id}"), None).await?;
        anyhow::ensure!(matches!(st, 204 | 404), "Bitbucket didn't delete deploy key {id} of {name} ({st}): {}", said(&v));
        Ok(())
    }

    /// A build status, in Bitbucket's words (`INPROGRESS`, `SUCCESSFUL`, `FAILED`, `STOPPED`).
    pub async fn status(&self, hub: &Hub, repo: &Repo, sha: &str, s: &CommitStatus) -> anyhow::Result<()> {
        let name = self.name(repo)?;
        let state = match s.state.as_str() {
            "success" => "SUCCESSFUL",
            "failure" | "error" | "failed" => "FAILED",
            "cancelled" | "canceled" | "inactive" => "STOPPED",
            _ => "INPROGRESS",
        };
        // Bitbucket needs a link; kiln's own pages, when none is given.
        let url = s.url.clone().unwrap_or_else(|| hub.cfg.server.public_url.trim_end_matches('/').to_owned());
        let body = json!({ "key": crate::util::truncate(&s.context, 40), "name": s.context, "state": state, "description": crate::util::truncate(&s.description, 255), "url": url });
        let (st, v) = self.call(Method::Post, &format!("/repositories/{name}/commit/{sha}/statuses/build"), Some(&body)).await?;
        anyhow::ensure!(st == 201 || st == 200, "Bitbucket took no status ({st}): {}", said(&v));
        Ok(())
    }

    /// A release reported as a build status (`deploy/<environment>`).
    pub async fn deployment(&self, hub: &Hub, repo: &Repo, sha: &str, environment: &str, state: &str, url: Option<&str>) -> anyhow::Result<()> {
        let s = CommitStatus { context: format!("deploy/{environment}"), state: state.to_owned(), description: format!("kiln: {state} to {environment}"), url: url.map(str::to_owned) };
        self.status(hub, repo, sha, &s).await
    }

    /// `workspace/slug` made with the history of `template` (`workspace/slug`): a new repository,
    /// and the template's branches and tags pushed to it.
    pub async fn create_from_template(&self, hub: &Hub, template: &str, name: &str, private: bool) -> anyhow::Result<()> {
        anyhow::ensure!(name.split('/').count() == 2, "`{name}`: workspace/slug");
        let (st, v) = self.call(Method::Post, &format!("/repositories/{name}"), Some(&json!({ "scm": "git", "is_private": private }))).await?;
        anyhow::ensure!(st == 200 || st == 201, "Bitbucket made no repository ({st}): {}", said(&v));
        let (u, p) = self.git_login();
        let (from, to) = (super::with_credentials(&self.web, &u, &p, template), super::with_credentials(&self.web, &u, &p, name));
        let scratch = std::env::temp_dir().join(format!("kiln-template-{}", crate::util::new_id()));
        let s = scratch.to_string_lossy().into_owned();
        let copied = async {
            crate::gitops::run_git(hub, &["clone", "--bare", "--quiet", &from, &s], Duration::from_secs(600)).await?;
            crate::gitops::run_git(hub, &["-C", &s, "push", "--quiet", &to, "refs/heads/*:refs/heads/*", "refs/tags/*:refs/tags/*"], Duration::from_secs(600)).await
        }
        .await;
        let _ = crate::rt::blocking(move || std::fs::remove_dir_all(scratch)).await;
        copied.map(|_| ()).with_context(|| format!("{template}'s history into {name}"))
    }
}

/// An Ed25519 key for SSH: its public key as `authorized_keys` has it (`ssh-ed25519 AAAA… <comment>`),
/// and its private key in OpenSSH's format (unencrypted).
fn ssh_key(comment: &str) -> anyhow::Result<(String, String)> {
    use aws_lc_rs::rand::SecureRandom as _;
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair as _};
    fn string(out: &mut Vec<u8>, b: &[u8]) {
        out.extend_from_slice(&u32::try_from(b.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(b);
    }
    let rng = aws_lc_rs::rand::SystemRandom::new();
    let (mut seed, mut check) = ([0u8; 32], [0u8; 4]);
    rng.fill(&mut seed).map_err(|_| anyhow!("no SSH key could be made"))?;
    rng.fill(&mut check).map_err(|_| anyhow!("no SSH key could be made"))?;
    let pair = Ed25519KeyPair::from_seed_unchecked(&seed).map_err(|_| anyhow!("no SSH key could be made"))?;
    let public = pair.public_key().as_ref();
    let mut blob = Vec::new();
    string(&mut blob, b"ssh-ed25519");
    string(&mut blob, public);
    // The private section: a check number twice, the key (its seed, then its public half), the
    // comment, padded to 8 bytes with 1, 2, 3 …
    let mut private = Vec::new();
    private.extend_from_slice(&check);
    private.extend_from_slice(&check);
    string(&mut private, b"ssh-ed25519");
    string(&mut private, public);
    string(&mut private, &[seed.as_slice(), public].concat());
    string(&mut private, comment.as_bytes());
    for pad in 1..=7u8 {
        if private.len().is_multiple_of(8) {
            break;
        }
        private.push(pad);
    }
    let mut key = b"openssh-key-v1\0".to_vec();
    string(&mut key, b"none");
    string(&mut key, b"none");
    string(&mut key, b"");
    key.extend_from_slice(&1u32.to_be_bytes());
    string(&mut key, &blob);
    string(&mut key, &private);
    let text = B64.encode(&key);
    let lines: Vec<&str> = text.as_bytes().chunks(70).filter_map(|c| std::str::from_utf8(c).ok()).collect();
    Ok((format!("ssh-ed25519 {} {comment}", B64.encode(&blob)), format!("-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n", lines.join("\n"))))
}

/// Where a pull request (Bitbucket's JSON for one) is.
fn pr_state(v: &Value) -> ChangeState {
    match v.get("state").and_then(Value::as_str).unwrap_or_default() {
        "MERGED" => ChangeState::Merged { commit: v.pointer("/merge_commit/hash").and_then(Value::as_str).map(str::to_owned), by: v.pointer("/closed_by/nickname").and_then(Value::as_str).map(str::to_owned) },
        "DECLINED" | "SUPERSEDED" => ChangeState::Closed,
        _ => ChangeState::Open,
    }
}

/// A push webhook's ref update (`repo:push`; its first change).
pub fn push_event(headers: &http::HeaderMap, body: &[u8]) -> Option<Push> {
    if headers.get("x-event-key").and_then(|v| v.to_str().ok()) != Some("repo:push") {
        return None;
    }
    let v = super::json_body(body)?;
    let change = v.pointer("/push/changes/0")?;
    let new = change.get("new").filter(|n| !n.is_null());
    let old = change.get("old").filter(|o| !o.is_null());
    let kind = new.or(old).and_then(|n| n.get("type")).and_then(Value::as_str).unwrap_or("branch");
    let short = new.or(old).and_then(|n| n.get("name")).and_then(Value::as_str)?;
    let refname = if kind == "tag" { format!("refs/tags/{short}") } else { format!("refs/heads/{short}") };
    let hash = |n: Option<&Value>| n.and_then(|n| n.pointer("/target/hash")).and_then(Value::as_str).map_or_else(|| super::NO_COMMIT.to_owned(), str::to_owned);
    Some(Push { repo: v.pointer("/repository/full_name").and_then(Value::as_str)?.to_owned(), refname, before: hash(old), after: hash(new) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(url: &str) -> Repo {
        Repo { id: "r".into(), name: "acme/app".into(), clone_url: url.into(), branches: vec![], poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: None, mirror: None }
    }

    #[test]
    fn w23_r05_names_and_logins() {
        let b = Bitbucket { web: "https://bitbucket.org".into(), api: "https://api.bitbucket.org/2.0".into(), cred: Cred::Token("t".into()), webhook_secret: None, workspaces: vec!["acme".into()], ssh: "ssh://git@bitbucket.org".into() };
        assert_eq!(b.ssh_url("acme/releases"), "ssh://git@bitbucket.org/acme/releases.git");
        assert_eq!(b.name_of(&repo("https://bitbucket.org/acme/app.git")).as_deref(), Some("acme/app"));
        assert_eq!(b.name_of(&repo("git@bitbucket.org:acme/app.git")).as_deref(), Some("acme/app"));
        assert_eq!(b.name_of(&repo("https://bitbucket.org/other/app.git")), None);
        assert_eq!(b.git_login().0, "x-token-auth");
        let api = Bitbucket { cred: Cred::Basic { user: "ada@example.com".into(), password: "p".into() }, ..b };
        assert_eq!(api.git_login().0, "x-bitbucket-api-token-auth");
        assert!(api.api_auth().starts_with("Basic "));
        assert_eq!(seg("{0f0f}"), "%7B0f0f%7D");
    }

    #[test]
    fn deploy_keys_are_openssh_ed25519() -> anyhow::Result<()> {
        let (public, private) = ssh_key("kiln")?;
        let mut parts = public.split(' ');
        assert_eq!(parts.next(), Some("ssh-ed25519"));
        let blob = B64.decode(parts.next().unwrap_or_default())?;
        assert_eq!(parts.next(), Some("kiln"));
        // `string "ssh-ed25519"`, then the 32-byte key.
        assert_eq!(blob.len(), 4 + 11 + 4 + 32);
        assert_eq!(blob.get(4..15), Some(b"ssh-ed25519".as_slice()));
        let body: String = private.lines().filter(|l| !l.starts_with("-----")).collect();
        assert!(private.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----\n") && private.ends_with("-----END OPENSSH PRIVATE KEY-----\n"));
        assert!(private.lines().all(|l| l.len() <= 70));
        let key = B64.decode(body)?;
        assert!(key.starts_with(b"openssh-key-v1\0"));
        // The public key in it is the one Bitbucket gets.
        assert!(key.windows(blob.len()).any(|w| w == blob.as_slice()));
        assert_ne!(ssh_key("kiln")?.0, public, "a new key each time");
        // OpenSSH reads it, where it's here to: the public key it makes from it is the same.
        #[cfg(unix)]
        if let Ok(dir) = tempfile::tempdir() {
            use std::os::unix::fs::PermissionsExt as _;
            let file = dir.path().join("key");
            std::fs::write(&file, &private)?;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))?;
            if let Ok(out) = std::process::Command::new("ssh-keygen").arg("-y").arg("-f").arg(&file).output() {
                assert!(out.status.success(), "ssh-keygen: {}", String::from_utf8_lossy(&out.stderr));
                let derived = String::from_utf8_lossy(&out.stdout);
                assert_eq!(derived.split(' ').take(2).collect::<Vec<_>>(), public.split(' ').take(2).collect::<Vec<_>>());
            }
        }
        Ok(())
    }

    #[test]
    fn w23_r05_push_hook() {
        let mut h = http::HeaderMap::new();
        h.insert("x-event-key", http::HeaderValue::from_static("repo:push"));
        let body = br#"{"repository":{"full_name":"acme/app"},"push":{"changes":[{"new":{"type":"branch","name":"main","target":{"hash":"b"}},"old":{"type":"branch","name":"main","target":{"hash":"a"}}}]}}"#;
        assert_eq!(push_event(&h, body), Some(Push { repo: "acme/app".into(), refname: "refs/heads/main".into(), before: "a".into(), after: "b".into() }));
        let created = br#"{"repository":{"full_name":"acme/app"},"push":{"changes":[{"new":{"type":"tag","name":"v1","target":{"hash":"c"}},"old":null}]}}"#;
        assert_eq!(push_event(&h, created).map(|p| (p.refname, p.before)), Some(("refs/tags/v1".into(), super::super::NO_COMMIT.into())));
    }
}
