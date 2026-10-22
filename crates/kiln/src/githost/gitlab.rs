//! GitLab as a git host (docs/ci.md W23-R04; docs/deploy-api.md §7): GitLab.com or a self-managed
//! GitLab, through its REST API (v4) with the token `[[ci.gitlab]]` gives each instance (a group or
//! project access token, or a person's).
//! - **Reading and pushing**: git over HTTPS with the token (`oauth2` as git's user).
//! - **Commits** are made here, in the repository's mirror, and pushed with a lease on the head they
//!   were made on: GitLab's commits API builds on whatever the branch holds by then.
//! - **Merge requests** (opened, found by their branch, followed, closed with a note), commit
//!   statuses, deployments, push webhooks (`X-Gitlab-Token`), new projects (empty in a group, or
//!   forked from a template and unlinked from it), and deploy tokens that read one project.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use serde_json::{Value, json};

use super::{Author, ChangeRef, ChangeState, CommitError, CommitStatus, Committed, Push, Reader};
use crate::ci::cli::enc;
use crate::http::client::Method;
use crate::hub::Hub;
use crate::store::Repo;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// A GitLab instance kiln acts on.
pub struct Gitlab {
    /// Its web origin (`https://gitlab.com`).
    pub web: String,
    /// Its REST API (`{web}/api/v4`).
    pub api: String,
    token: String,
    pub webhook_secret: Option<String>,
    /// The groups its token covers (none: every project there).
    groups: Vec<String>,
}

fn cache() -> &'static parking_lot::Mutex<Option<(Instant, Vec<Arc<Gitlab>>)>> {
    static C: OnceLock<parking_lot::Mutex<Option<(Instant, Vec<Arc<Gitlab>>)>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// The GitLab instances `[[ci.gitlab]]` names, their tokens read again at most every minute.
pub async fn hosts(hub: &Hub) -> Vec<Arc<Gitlab>> {
    if let Some((at, h)) = cache().lock().as_ref()
        && at.elapsed() < Duration::from_secs(60)
    {
        return h.clone();
    }
    let mut out = Vec::new();
    for c in &hub.cfg.ci.gitlab {
        match load(c).await {
            Ok(g) => out.push(Arc::new(g)),
            Err(e) => tracing::warn!(url = %c.url, error = %format!("{e:#}"), "a GitLab host can't be read"),
        }
    }
    *cache().lock() = Some((Instant::now(), out.clone()));
    out
}

async fn load(c: &crate::config::ci::GitlabHost) -> anyhow::Result<Gitlab> {
    let web = c.url.trim_end_matches('/').to_owned();
    let api = c.api_url.as_deref().map_or_else(|| format!("{web}/api/v4"), |a| a.trim_end_matches('/').to_owned());
    let token = super::read_secret(&c.token_file).await?.ok_or_else(|| anyhow!("{} is empty", c.token_file.display()))?;
    let webhook_secret = match &c.webhook_secret_file {
        Some(f) => super::read_secret(f).await?,
        None => None,
    };
    Ok(Gitlab { web, api, token, webhook_secret, groups: c.groups.clone() })
}

/// git's `http.<url>.extraheader` settings for every GitLab `[[ci.gitlab]]` names: its token for
/// what it covers, read as git runs.
pub fn git_headers(hub: &Hub) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for c in &hub.cfg.ci.gitlab {
        let Some(token) = std::fs::read_to_string(&c.token_file).ok().map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()) else { continue };
        let (web, basic) = (c.url.trim_end_matches('/'), format!("Authorization: Basic {}", B64.encode(format!("oauth2:{token}"))));
        if c.groups.is_empty() {
            out.push((format!("{web}/"), basic));
        } else {
            out.extend(c.groups.iter().map(|g| (format!("{web}/{}/", g.trim_matches('/')), basic.clone())));
        }
    }
    out
}

/// The GitLab a repository is on, and its project's path there.
pub async fn for_repo(hub: &Hub, repo: &Repo) -> Option<(Arc<Gitlab>, String)> {
    hosts(hub).await.into_iter().find_map(|g| g.name_of(repo).map(|n| (g, n)))
}

/// What GitLab said when a call didn't do what was asked.
pub fn said(v: &Value) -> String {
    match v.get("message").or_else(|| v.get("error")) {
        Some(Value::String(s)) => s.clone(),
        Some(m) => crate::util::truncate(&m.to_string(), 300).to_owned(),
        None => crate::util::truncate(&v.to_string(), 300).to_owned(),
    }
}

/// A project's `:id` in GitLab's API: its path, encoded.
pub fn id(name: &str) -> String {
    enc(name)
}

impl Gitlab {
    /// The path (`group/subgroup/name`) of a repository on this GitLab that its token covers.
    pub fn name_of(&self, repo: &Repo) -> Option<String> {
        let (host, path) = super::split_url(&repo.clone_url)?;
        if host != super::host_of(&self.web) {
            return None;
        }
        // A GitLab served under a path (`https://corp.example/gitlab`).
        let root = self.web.split_once("://").map_or(self.web.as_str(), |(_, r)| r).split_once('/').map(|(_, p)| p.trim_matches('/')).unwrap_or_default();
        let path = if root.is_empty() { path } else { path.strip_prefix(root)?.trim_start_matches('/').to_owned() };
        let covered = self.groups.is_empty() || self.groups.iter().any(|g| path.strip_prefix(g.trim_matches('/')).is_some_and(|rest| rest.starts_with('/')));
        (covered && !path.is_empty()).then_some(path)
    }

    fn name(&self, repo: &Repo) -> anyhow::Result<String> {
        self.name_of(repo).ok_or_else(|| anyhow!("{} isn't on {}", repo.name, self.web))
    }

    /// The groups its token covers (none: every project there).
    pub fn groups(&self) -> &[String] {
        &self.groups
    }

    /// One call to GitLab's API as kiln: its status and JSON (`null` when none).
    pub async fn call(&self, method: Method, path: &str, body: Option<&Value>) -> anyhow::Result<(u16, Value)> {
        self.call_as(&self.token, method, path, body).await
    }

    /// One call with someone's own token (W23-R08: what they do from kiln is theirs).
    pub async fn call_as(&self, token: &str, method: Method, path: &str, body: Option<&Value>) -> anyhow::Result<(u16, Value)> {
        let (status, bytes) = self.raw(token, method, path, body, 32 << 20).await?;
        let v = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())) };
        Ok((status, v))
    }

    /// A call's bytes (a job's trace, an artifacts archive), a redirect followed without the token.
    pub async fn bytes(&self, path: &str, limit: usize) -> anyhow::Result<(u16, bytes::Bytes)> {
        self.raw(&self.token, Method::Get, path, None, limit).await
    }

    async fn raw(&self, token: &str, method: Method, path: &str, body: Option<&Value>, limit: usize) -> anyhow::Result<(u16, bytes::Bytes)> {
        let client = crate::http::client::github()?;
        let url = if path.starts_with("http://") || path.starts_with("https://") { path.to_owned() } else { format!("{}{path}", self.api) };
        let mut req = client.request(method, &url).bearer(token).header("Accept", "application/json").timeout(Duration::from_secs(30));
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.with_context(|| format!("GitLab: {url}"))?;
        let status = resp.status();
        if (300..400).contains(&status)
            && let Some(to) = resp.header("location").map(str::to_owned)
        {
            let r = client.request(Method::Get, &to).timeout(Duration::from_secs(60)).send().await.with_context(|| format!("GitLab's redirect: {to}"))?;
            let st = r.status();
            return Ok((st, r.bytes(limit).await?));
        }
        Ok((status, resp.bytes(limit).await?))
    }

    /// How git reads it: the project's URL and the token as git's password.
    pub fn reader(&self, repo: &Repo) -> anyhow::Result<Reader> {
        let name = self.name(repo)?;
        let basic = B64.encode(format!("oauth2:{}", self.token));
        Ok(Reader { url: format!("{}/{name}.git", self.web), header: Some(("Authorization".into(), format!("Basic {basic}"))) })
    }

    /// The URL git pushes to, the token in it.
    pub fn push_url(&self, repo: &Repo) -> anyhow::Result<String> {
        Ok(super::with_credentials(&self.web, "oauth2", &self.token, &self.name(repo)?))
    }

    /// Commits `files` onto `branch`, made here on `expected` and pushed with a lease on it.
    #[allow(clippy::too_many_arguments)]
    pub async fn commit(&self, hub: &Hub, repo: &Repo, branch: &str, expected: Option<&str>, files: &[super::Change], message: &str, author: &Author) -> Result<Committed, CommitError> {
        let url = self.push_url(repo)?;
        super::remote_commit(hub, repo, &url, branch, expected, files, message, author).await
    }

    pub async fn open_change(&self, repo: &Repo, head: &str, base: &str, title: &str, body: &str) -> anyhow::Result<ChangeRef> {
        let name = self.name(repo)?;
        let (st, v) = self.call(Method::Post, &format!("/projects/{}/merge_requests", id(&name)), Some(&json!({ "source_branch": head, "target_branch": base, "title": title, "description": body }))).await?;
        if st != 201 {
            bail!("GitLab opened no merge request ({st}): {}", said(&v));
        }
        Ok(ChangeRef { number: v.get("iid").and_then(Value::as_u64).unwrap_or(0), url: v.get("web_url").and_then(Value::as_str).unwrap_or_default().to_owned() })
    }

    pub async fn change(&self, repo: &Repo, number: u64) -> anyhow::Result<ChangeState> {
        let name = self.name(repo)?;
        let (st, v) = self.call(Method::Get, &format!("/projects/{}/merge_requests/{number}", id(&name)), None).await?;
        if st != 200 {
            bail!("merge request !{number} ({st}): {}", said(&v));
        }
        Ok(mr_state(&v))
    }

    /// The merge request from the branch `head` of the project, open or not, if there is one (the
    /// newest).
    pub async fn find_change(&self, repo: &Repo, head: &str) -> anyhow::Result<Option<(ChangeRef, ChangeState)>> {
        let name = self.name(repo)?;
        let (st, v) = self.call(Method::Get, &format!("/projects/{}/merge_requests?source_branch={}&state=all&per_page=5", id(&name), enc(head)), None).await?;
        if st != 200 {
            bail!("the merge requests from {head} ({st}): {}", said(&v));
        }
        let newest = v.as_array().and_then(|l| l.iter().max_by_key(|m| m.get("iid").and_then(Value::as_u64).unwrap_or(0)));
        Ok(newest.map(|m| (ChangeRef { number: m.get("iid").and_then(Value::as_u64).unwrap_or(0), url: m.get("web_url").and_then(Value::as_str).unwrap_or_default().to_owned() }, mr_state(m))))
    }

    /// Closes merge request `number` unmerged, saying why (a note) first.
    pub async fn close_change(&self, repo: &Repo, number: u64, why: &str) -> anyhow::Result<()> {
        let name = self.name(repo)?;
        if !why.is_empty() {
            let (st, v) = self.call(Method::Post, &format!("/projects/{}/merge_requests/{number}/notes", id(&name)), Some(&json!({ "body": why }))).await?;
            if st != 201 {
                tracing::debug!(merge_request = number, status = st, said = %said(&v), "GitLab took no note on a merge request kiln closes");
            }
        }
        let (st, v) = self.call(Method::Put, &format!("/projects/{}/merge_requests/{number}", id(&name)), Some(&json!({ "state_event": "close" }))).await?;
        anyhow::ensure!(st == 200, "GitLab didn't close merge request !{number} ({st}): {}", said(&v));
        Ok(())
    }

    /// Deletes a branch (gone already is fine).
    pub async fn delete_branch(&self, repo: &Repo, branch: &str) -> anyhow::Result<()> {
        let name = self.name(repo)?;
        let (st, v) = self.call(Method::Delete, &format!("/projects/{}/repository/branches/{}", id(&name), enc(branch)), None).await?;
        anyhow::ensure!(matches!(st, 204 | 404), "GitLab didn't delete {branch} ({st}): {}", said(&v));
        Ok(())
    }

    /// The project `group/…/name` as GitLab shows it to kiln's token (`None`: there's none it sees).
    pub async fn project(&self, name: &str) -> anyhow::Result<Option<Value>> {
        let (st, v) = self.call(Method::Get, &format!("/projects/{}", id(name)), None).await?;
        match st {
            200 => Ok(Some(v)),
            404 => Ok(None),
            _ => bail!("GitLab didn't say whether {name} is there ({st}): {}", said(&v)),
        }
    }

    /// Makes `group/…/name`, private and empty, in its group.
    pub async fn create_project(&self, name: &str, description: &str) -> anyhow::Result<()> {
        let (ns, path) = name.rsplit_once('/').ok_or_else(|| anyhow!("`{name}`: group/name"))?;
        let (st, v) = self.call(Method::Get, &format!("/namespaces/{}", id(ns)), None).await?;
        anyhow::ensure!(st == 200, "kiln's GitLab token reaches no group {ns} ({st}): {}", said(&v));
        let namespace = v.get("id").and_then(Value::as_u64).ok_or_else(|| anyhow!("GitLab's group {ns} has no id"))?;
        let body = json!({ "name": path, "path": path, "namespace_id": namespace, "visibility": "private", "description": description, "initialize_with_readme": false });
        let (st, v) = self.call(Method::Post, "/projects", Some(&body)).await?;
        anyhow::ensure!(st == 201, "GitLab made no project {name} ({st}): {}", said(&v));
        Ok(())
    }

    /// A deploy token of the project `name` that reads its repository and nothing else: its id,
    /// its username and the token (GitLab shows a token once, as it's made).
    pub async fn deploy_token(&self, name: &str, label: &str) -> anyhow::Result<(u64, String, String)> {
        let (st, v) = self.call(Method::Post, &format!("/projects/{}/deploy_tokens", id(name)), Some(&json!({ "name": label, "scopes": ["read_repository"] }))).await?;
        anyhow::ensure!(st == 201, "GitLab made no deploy token for {name} ({st}): {} (it takes a Maintainer of the project)", said(&v));
        let token_id = v.get("id").and_then(Value::as_u64).ok_or_else(|| anyhow!("GitLab's deploy token has no id"))?;
        let user = v.get("username").and_then(Value::as_str).ok_or_else(|| anyhow!("GitLab's deploy token has no username"))?.to_owned();
        let token = v.get("token").and_then(Value::as_str).ok_or_else(|| anyhow!("GitLab's deploy token has no token"))?.to_owned();
        Ok((token_id, user, token))
    }

    /// A deploy token revoked (gone already is fine).
    pub async fn revoke_deploy_token(&self, name: &str, token_id: u64) -> anyhow::Result<()> {
        let (st, v) = self.call(Method::Delete, &format!("/projects/{}/deploy_tokens/{token_id}", id(name)), None).await?;
        anyhow::ensure!(matches!(st, 204 | 404), "GitLab didn't revoke deploy token {token_id} ({st}): {}", said(&v));
        Ok(())
    }

    /// A commit status, in GitLab's words (`pending`, `running`, `success`, `failed`, `canceled`).
    pub async fn status(&self, repo: &Repo, sha: &str, s: &CommitStatus) -> anyhow::Result<()> {
        let name = self.name(repo)?;
        let state = match s.state.as_str() {
            "success" => "success",
            "failure" | "error" | "failed" => "failed",
            "running" | "in_progress" => "running",
            "cancelled" | "canceled" => "canceled",
            _ => "pending",
        };
        let body = json!({ "state": state, "name": s.context, "description": crate::util::truncate(&s.description, 255), "target_url": s.url });
        let (st, v) = self.call(Method::Post, &format!("/projects/{}/statuses/{sha}", id(&name)), Some(&body)).await?;
        // GitLab refuses a status the commit already has ("Cannot transition status via :run from :running").
        if st == 400 && said(&v).contains("Cannot transition status") {
            return Ok(());
        }
        anyhow::ensure!(st == 201 || st == 200, "GitLab took no status ({st}): {}", said(&v));
        Ok(())
    }

    /// A deployment of `sha` to `environment` (the latest of that commit there, else a new one), in
    /// GitLab's words (`running`, `success`, `failed`, `canceled`); GitLab retires older ones itself.
    pub async fn deployment(&self, repo: &Repo, sha: &str, environment: &str, state: &str, url: Option<&str>) -> anyhow::Result<()> {
        let _ = url;
        let status = match state {
            "queued" | "pending" | "in_progress" => "running",
            "success" => "success",
            "failure" | "error" => "failed",
            "inactive" => return Ok(()),
            _ => "canceled",
        };
        let name = self.name(repo)?;
        let (st, list) = self.call(Method::Get, &format!("/projects/{}/deployments?environment={}&order_by=id&sort=desc&per_page=50", id(&name), enc(environment)), None).await?;
        let found = (st == 200).then(|| list.as_array().into_iter().flatten().find(|d| d.get("sha").and_then(Value::as_str) == Some(sha)).and_then(|d| d.get("id")).and_then(Value::as_u64)).flatten();
        let (st, v) = match found {
            Some(d) => self.call(Method::Put, &format!("/projects/{}/deployments/{d}", id(&name)), Some(&json!({ "status": status }))).await?,
            None => {
                let git_ref = match repo.default_branch.clone() {
                    Some(b) => b,
                    None => {
                        let (_, p) = self.call(Method::Get, &format!("/projects/{}", id(&name)), None).await?;
                        p.get("default_branch").and_then(Value::as_str).unwrap_or("main").to_owned()
                    }
                };
                self.call(Method::Post, &format!("/projects/{}/deployments", id(&name)), Some(&json!({ "environment": environment, "sha": sha, "ref": git_ref, "tag": false, "status": status }))).await?
            }
        };
        anyhow::ensure!(st == 200 || st == 201, "GitLab took no deployment ({st}): {}", said(&v));
        Ok(())
    }

    /// `group/name` made from the project `template`: a fork of it, then unlinked, so it is a
    /// project of its own with the template's history (forks need no paid tier, custom project
    /// templates do).
    pub async fn create_from_template(&self, template: &str, name: &str, private: bool) -> anyhow::Result<()> {
        let (ns, path) = name.rsplit_once('/').ok_or_else(|| anyhow!("`{name}`: group/name"))?;
        let body = json!({ "namespace_path": ns, "path": path, "name": path, "visibility": if private { "private" } else { "public" } });
        let (st, v) = self.call(Method::Post, &format!("/projects/{}/fork", id(template)), Some(&body)).await?;
        anyhow::ensure!(st == 201 || st == 202, "GitLab made no project ({st}): {}", said(&v));
        let made = v.get("id").and_then(Value::as_u64).ok_or_else(|| anyhow!("GitLab's new project has no id"))?;
        let (st, v) = self.call(Method::Delete, &format!("/projects/{made}/fork"), None).await?;
        if !(200..300).contains(&st) && st != 304 {
            tracing::warn!(project = name, status = st, said = %said(&v), "the new project is still linked to its template");
        }
        Ok(())
    }
}

/// Where a merge request (GitLab's JSON for one) is.
fn mr_state(v: &Value) -> ChangeState {
    match v.get("state").and_then(Value::as_str).unwrap_or_default() {
        "merged" => ChangeState::Merged {
            commit: v.get("merge_commit_sha").and_then(Value::as_str).or_else(|| v.get("squash_commit_sha").and_then(Value::as_str)).map(str::to_owned),
            by: v.pointer("/merge_user/username").or_else(|| v.pointer("/merged_by/username")).and_then(Value::as_str).map(str::to_owned),
        },
        "closed" => ChangeState::Closed,
        _ => ChangeState::Open,
    }
}

/// A push webhook's ref update (`Push Hook`, `Tag Push Hook`).
pub fn push_event(headers: &http::HeaderMap, body: &[u8]) -> Option<Push> {
    let kind = headers.get("x-gitlab-event").and_then(|v| v.to_str().ok())?;
    if kind != "Push Hook" && kind != "Tag Push Hook" {
        return None;
    }
    let v = super::json_body(body)?;
    let s = |p: &str| v.pointer(p).and_then(Value::as_str).map(str::to_owned);
    Some(Push { repo: s("/project/path_with_namespace")?, refname: s("/ref")?, before: s("/before")?, after: s("/after")? })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(url: &str) -> Repo {
        Repo { id: "r".into(), name: "acme/app".into(), clone_url: url.into(), branches: vec![], poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: None, mirror: None }
    }

    #[test]
    fn w23_r04_names() {
        let g = Gitlab { web: "https://gitlab.com".into(), api: "https://gitlab.com/api/v4".into(), token: "t".into(), webhook_secret: None, groups: vec!["acme".into()] };
        assert_eq!(g.name_of(&repo("https://gitlab.com/acme/platform/app.git")).as_deref(), Some("acme/platform/app"));
        assert_eq!(g.name_of(&repo("git@gitlab.com:acme/app.git")).as_deref(), Some("acme/app"));
        assert_eq!(g.name_of(&repo("https://gitlab.com/acmeco/app.git")), None, "another group");
        assert_eq!(g.name_of(&repo("https://github.com/acme/app.git")), None);
        let corp = Gitlab { web: "https://corp.example/gitlab".into(), api: String::new(), token: "t".into(), webhook_secret: None, groups: vec![] };
        assert_eq!(corp.name_of(&repo("https://corp.example/gitlab/infra/app.git")).as_deref(), Some("infra/app"));
        assert_eq!(id("acme/platform/app"), "acme%2Fplatform%2Fapp");
    }

    #[test]
    fn w23_r04_push_hook() {
        let mut h = http::HeaderMap::new();
        h.insert("x-gitlab-event", http::HeaderValue::from_static("Push Hook"));
        let body = br#"{"ref":"refs/heads/main","before":"a","after":"b","project":{"path_with_namespace":"acme/app"}}"#;
        assert_eq!(push_event(&h, body), Some(Push { repo: "acme/app".into(), refname: "refs/heads/main".into(), before: "a".into(), after: "b".into() }));
        h.insert("x-gitlab-event", http::HeaderValue::from_static("Pipeline Hook"));
        assert_eq!(push_event(&h, body), None);
    }
}
