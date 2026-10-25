//! The facade's repository reads (docs/ci.md W14-R11), from the server's mirror of the run's
//! repository, in GitHub's shapes:
//! - `GET /repos/{o}/{r}`;
//! - `/commits/{ref}`;
//! - `/branches/{b}`;
//! - `/git/ref/{ref}`, `/git/matching-refs/{ref}` and the older `/git/refs[/{ref}]`;
//! - `/contents/{path}?ref=`;
//! - `/tarball/{ref}` and `/zipball/{ref}`;
//! - `/compare/{base}...{head}`;
//! - `/collaborators/{username}/permission`, from what kiln grants the person there.

use std::sync::Arc;

use base64::Engine as _;
use http::StatusCode;
use serde_json::{Value, json};

use crate::ci::Fail;
use crate::gitops;
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;
use crate::store::Repo;

/// The run's repository, as the facade describes it.
pub struct Site<'a> {
    pub hub: &'a Arc<Hub>,
    pub repo: Repo,
    /// `<origin>/api/v3/repos/<owner>/<repo>`.
    pub api: String,
    /// `<origin>/<owner>/<repo>`.
    pub html: String,
    pub origin: String,
    pub mirror: String,
    pub default_branch: String,
}

impl Site<'_> {
    pub async fn new<'a>(hub: &'a Arc<Hub>, repo: Repo, origin: &str) -> Site<'a> {
        let f = crate::ci::events::facts(hub, &repo).await;
        Site { api: format!("{origin}/api/v3/repos/{}", repo.name), html: format!("{origin}/{}", repo.name), origin: origin.to_owned(), mirror: f.mirror, default_branch: f.default_branch, hub, repo }
    }

    async fn git(&self, args: &[&str]) -> anyhow::Result<Vec<u8>> {
        let mut a = vec!["-C", self.mirror.as_str()];
        a.extend_from_slice(args);
        gitops::run_git(self.hub, &a, gitops::LS_REMOTE_TIMEOUT).await
    }

    async fn text(&self, args: &[&str]) -> Option<String> {
        self.git(args).await.ok().map(|o| String::from_utf8_lossy(&o).trim().to_owned())
    }

    /// The commit a ref, a branch, a tag or a SHA names.
    pub async fn resolve(&self, r: &str) -> Option<String> {
        if !crate::ci::load::ref_ok(r) && !r.starts_with("refs/") {
            return None;
        }
        for cand in [format!("{r}^{{commit}}"), format!("refs/heads/{r}^{{commit}}"), format!("refs/tags/{r}^{{commit}}")] {
            if let Some(sha) = self.text(&["rev-parse", "--verify", "--quiet", &cand]).await.filter(|s| s.len() == 40) {
                return Some(sha);
            }
        }
        None
    }

    /// The last commit `a` and `b` share, if they share one.
    pub async fn merge_base(&self, a: &str, b: &str) -> Option<String> {
        self.text(&["merge-base", a, b]).await.filter(|s| s.len() == 40)
    }

    pub fn owner(&self) -> &str {
        self.repo.name.split_once('/').map_or(self.repo.name.as_str(), |(o, _)| o)
    }

    pub fn name(&self) -> &str {
        self.repo.name.split_once('/').map_or(self.repo.name.as_str(), |(_, n)| n)
    }

    fn owner_json(&self) -> Value {
        json!({ "login": self.owner(), "id": 0, "type": "Organization", "url": format!("{}/api/v3/users/{}", self.origin, self.owner()), "html_url": format!("{}/{}", self.origin, self.owner()) })
    }
}

fn rfc3339(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default()
}

fn not_found() -> Response {
    srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" }))
}

/// `GET /repos/{o}/{r}`.
pub fn repository(s: &Site<'_>) -> Response {
    let r = &s.repo;
    srv::json(
        StatusCode::OK,
        &json!({
            "id": 0, "node_id": format!("kiln-repo-{}", r.id), "name": s.name(), "full_name": r.name, "private": true, "visibility": "private",
            "owner": s.owner_json(), "html_url": s.html, "description": null, "fork": false, "url": s.api,
            "clone_url": format!("{}.git", s.html), "git_url": format!("{}.git", s.html), "ssh_url": null,
            "default_branch": s.default_branch, "master_branch": s.default_branch, "archived": false, "disabled": false,
            "topics": [], "license": null, "homepage": null, "language": null, "size": 0,
            "created_at": rfc3339(r.created_at), "updated_at": rfc3339(r.created_at), "pushed_at": rfc3339(r.last_polled_at.unwrap_or(r.created_at)),
            "permissions": { "admin": false, "maintain": false, "push": false, "triage": false, "pull": true },
            "tarball_url": format!("{}/tarball/{{ref}}", s.api), "zipball_url": format!("{}/zipball/{{ref}}", s.api),
        }),
    )
}

/// A commit in GitHub's shape, with its files when asked.
pub async fn commit_json(s: &Site<'_>, sha: &str, files: bool) -> Option<Value> {
    let fmt = "--format=%H%x1f%T%x1f%P%x1f%an%x1f%ae%x1f%aI%x1f%cn%x1f%ce%x1f%cI%x1f%B";
    let out = s.text(&["show", "-s", fmt, sha]).await?;
    let f: Vec<&str> = out.splitn(10, '\x1f').collect();
    let get = |i: usize| f.get(i).copied().unwrap_or_default();
    let full = get(0).to_owned();
    if full.len() != 40 {
        return None;
    }
    let parents: Vec<Value> = get(2).split_whitespace().map(|p| json!({ "sha": p, "url": format!("{}/commits/{p}", s.api), "html_url": format!("{}/commit/{p}", s.html) })).collect();
    let mut v = json!({
        "sha": full, "node_id": format!("kiln-commit-{full}"),
        "commit": {
            "author": { "name": get(3), "email": get(4), "date": get(5) },
            "committer": { "name": get(6), "email": get(7), "date": get(8) },
            "message": get(9).trim_end(), "tree": { "sha": get(1), "url": format!("{}/git/trees/{}", s.api, get(1)) },
            "url": format!("{}/git/commits/{full}", s.api), "comment_count": 0,
            "verification": { "verified": false, "reason": "unsigned", "signature": null, "payload": null },
        },
        "url": format!("{}/commits/{full}", s.api), "html_url": format!("{}/commit/{full}", s.html),
        "author": null, "committer": null, "parents": parents,
    });
    if files {
        let base = get(2).split_whitespace().next().map(str::to_owned);
        let (list, stats) = diff_files(s, base.as_deref(), &full).await;
        if let Some(o) = v.as_object_mut() {
            o.insert("files".into(), Value::Array(list));
            o.insert("stats".into(), stats);
        }
    }
    Some(v)
}

/// Changed files between two commits (`None`: from the empty tree), GitHub's `files`.
pub(crate) async fn diff_files(s: &Site<'_>, base: Option<&str>, head: &str) -> (Vec<Value>, Value) {
    let empty = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let from = base.unwrap_or(empty);
    let names = s.text(&["diff", "--name-status", "-M", "-z", from, head]).await.unwrap_or_default();
    let nums = s.text(&["diff", "--numstat", "-M", from, head]).await.unwrap_or_default();
    let mut counts = std::collections::HashMap::new();
    for line in nums.lines() {
        let mut p = line.splitn(3, '\t');
        let (a, d, name) = (p.next().unwrap_or("0"), p.next().unwrap_or("0"), p.next().unwrap_or_default());
        // A rename prints as `old => new` or `dir/{old => new}`; the new name is what counts.
        let name = name.rsplit(" => ").next().unwrap_or(name).replace(['{', '}'], "");
        counts.insert(name, (a.parse::<i64>().unwrap_or(0), d.parse::<i64>().unwrap_or(0)));
    }
    let mut files = Vec::new();
    let (mut adds, mut dels) = (0i64, 0i64);
    let mut parts = names.split('\0').filter(|x| !x.is_empty());
    while let Some(st) = parts.next() {
        let (status, prev) = match st.chars().next() {
            Some('A') => ("added", None),
            Some('D') => ("removed", None),
            Some('R') => ("renamed", parts.next()),
            Some('C') => ("copied", parts.next()),
            _ => ("modified", None),
        };
        let Some(name) = parts.next() else { break };
        let (a, d) = counts.get(name).copied().unwrap_or((0, 0));
        adds += a;
        dels += d;
        let mut f = json!({ "filename": name, "status": status, "additions": a, "deletions": d, "changes": a + d, "sha": null,
                            "blob_url": format!("{}/blob/{head}/{name}", s.html), "raw_url": format!("{}/raw/{head}/{name}", s.html), "contents_url": format!("{}/contents/{name}?ref={head}", s.api) });
        if let (Some(p), Some(o)) = (prev, f.as_object_mut()) {
            o.insert("previous_filename".into(), json!(p));
        }
        files.push(f);
    }
    (files, json!({ "total": adds + dels, "additions": adds, "deletions": dels }))
}

/// `GET /repos/{o}/{r}/commits/{ref}`.
pub async fn commit(s: &Site<'_>, r: &str) -> Response {
    match s.resolve(r).await {
        Some(sha) => match commit_json(s, &sha, true).await {
            Some(v) => srv::json(StatusCode::OK, &v),
            None => not_found(),
        },
        None => srv::json(StatusCode::UNPROCESSABLE_ENTITY, &json!({ "message": format!("No commit found for SHA: {r}") })),
    }
}

/// `GET /repos/{o}/{r}/branches/{b}`.
pub async fn branch(s: &Site<'_>, b: &str) -> Response {
    let Some(sha) = s.text(&["rev-parse", "--verify", "--quiet", &format!("refs/heads/{b}^{{commit}}")]).await.filter(|x| x.len() == 40) else { return srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Branch not found" })) };
    let c = commit_json(s, &sha, false).await.unwrap_or(Value::Null);
    srv::json(StatusCode::OK, &json!({ "name": b, "commit": c, "protected": false, "_links": { "self": format!("{}/branches/{b}", s.api), "html": format!("{}/tree/{b}", s.html) } }))
}

fn ref_json(s: &Site<'_>, name: &str, sha: &str, kind: &str) -> Value {
    json!({ "ref": name, "node_id": format!("kiln-ref-{name}"), "url": format!("{}/git/{name}", s.api), "object": { "sha": sha, "type": kind, "url": format!("{}/git/{kind}s/{sha}", s.api) } })
}

/// `GET /repos/{o}/{r}/git/ref/{ref}` (one) and `/git/matching-refs/{prefix}` (all that start so).
pub async fn git_refs(s: &Site<'_>, r: &str, one: bool) -> Response {
    let full = format!("refs/{}", r.trim_start_matches("refs/"));
    let out = s.text(&["for-each-ref", "--format=%(refname)%09%(objectname)%09%(objecttype)", &full]).await.unwrap_or_default();
    let all = s.text(&["for-each-ref", "--format=%(refname)%09%(objectname)%09%(objecttype)"]).await.unwrap_or_default();
    let pick = |text: &str| -> Vec<Value> {
        text.lines()
            .filter_map(|l| {
                let mut p = l.split('\t');
                let (n, sha, kind) = (p.next()?, p.next()?, p.next()?);
                Some((n.to_owned(), sha.to_owned(), kind.to_owned()))
            })
            .filter(|(n, _, _)| if one { n == &full } else { n.starts_with(&full) })
            .map(|(n, sha, kind)| ref_json(s, &n, &sha, &kind))
            .collect()
    };
    let found = { let x = pick(&out); if x.is_empty() { pick(&all) } else { x } };
    if one {
        match found.into_iter().next() {
            Some(v) => srv::json(StatusCode::OK, &v),
            None => not_found(),
        }
    } else {
        srv::json(StatusCode::OK, &Value::Array(found))
    }
}

/// `GET /git/refs[/{ref}]`, GitHub's older form: the ref when one is named exactly, else the
/// refs it begins (every ref, with none).
pub async fn git_refs_older(s: &Site<'_>, r: &str) -> Response {
    let full = format!("refs/{}", r.trim_start_matches("refs/"));
    let exact = !r.is_empty() && s.text(&["for-each-ref", "--format=%(refname)", &full]).await.is_some_and(|out| out.lines().any(|l| l == full));
    git_refs(s, r, exact).await
}

/// `GET /repos/{o}/{r}/contents/{path}?ref=`: a file (base64, or raw when asked) or a
/// directory's entries.
pub async fn contents(s: &Site<'_>, path: &str, r: Option<&str>, raw: bool) -> Response {
    let reference = r.filter(|x| !x.is_empty()).map(str::to_owned).unwrap_or_else(|| s.default_branch.clone());
    let Some(sha) = s.resolve(&reference).await else { return not_found() };
    let path = path.trim_matches('/');
    if path.split('/').any(|p| p == "..") {
        return not_found();
    }
    let spec = format!("{sha}:{path}");
    let kind = s.text(&["cat-file", "-t", &spec]).await.unwrap_or_default();
    let item = |name: &str, p: &str, t: &str, size: i64, blob: &str| {
        let kind = match t {
            "tree" => "dir",
            "commit" => "submodule",
            _ => "file",
        };
        json!({ "type": kind, "name": name, "path": p, "sha": blob, "size": size, "url": format!("{}/contents/{p}?ref={reference}", s.api),
                "html_url": format!("{}/blob/{reference}/{p}", s.html), "git_url": format!("{}/git/blobs/{blob}", s.api),
                "download_url": if kind == "file" { Value::String(format!("{}/raw/{reference}/{p}", s.html)) } else { Value::Null } })
    };
    match kind.as_str() {
        "blob" => {
            let Ok(bytes) = s.git(&["cat-file", "blob", &spec]).await else { return not_found() };
            if raw {
                return srv::response(StatusCode::OK, "application/octet-stream", srv::full(bytes));
            }
            let blob = s.text(&["rev-parse", &spec]).await.unwrap_or_default();
            let name = path.rsplit('/').next().unwrap_or(path);
            let size = i64::try_from(bytes.len()).unwrap_or(0);
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            // GitHub wraps the base64 at 60 characters.
            let wrapped: String = b64.as_bytes().chunks(60).map(|c| format!("{}\n", String::from_utf8_lossy(c))).collect();
            let mut v = item(name, path, "blob", size, &blob);
            if let Some(o) = v.as_object_mut() {
                o.insert("encoding".into(), json!("base64"));
                o.insert("content".into(), json!(wrapped));
            }
            srv::json(StatusCode::OK, &v)
        }
        "tree" => {
            let out = s.text(&["ls-tree", "-l", "-z", &spec]).await.unwrap_or_default();
            let list: Vec<Value> = out
                .split('\0')
                .filter(|l| !l.is_empty())
                .filter_map(|l| {
                    let (meta, name) = l.split_once('\t')?;
                    let mut m = meta.split_whitespace();
                    let (_mode, t, blob, size) = (m.next()?, m.next()?, m.next()?, m.next().unwrap_or("0"));
                    let p = if path.is_empty() { name.to_owned() } else { format!("{path}/{name}") };
                    Some(item(name, &p, t, size.parse().unwrap_or(0), blob))
                })
                .collect();
            srv::json(StatusCode::OK, &Value::Array(list))
        }
        _ => not_found(),
    }
}

/// `GET /repos/{o}/{r}/tarball/{ref}` and `/zipball/{ref}`: the commit's tree under one top-level
/// directory, as GitHub's archives are (what `actions/checkout` unpacks when git is missing).
pub async fn archive(s: &Site<'_>, r: &str, zip: bool) -> Result<Response, Fail> {
    let reference = if r.is_empty() { s.default_branch.clone() } else { r.to_owned() };
    let Some(sha) = s.resolve(&reference).await else { return Ok(not_found()) };
    let prefix = format!("{}-{}-{}/", s.owner(), s.name(), sha.get(..7).unwrap_or(&sha));
    let fmt = if zip { "--format=zip" } else { "--format=tar.gz" };
    let bytes = s.git(&["archive", fmt, &format!("--prefix={prefix}"), &sha]).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let name = format!("{}-{}-{}.{}", s.owner(), s.name(), sha.get(..7).unwrap_or(&sha), if zip { "zip" } else { "tar.gz" });
    let r = srv::response(StatusCode::OK, if zip { "application/zip" } else { "application/x-gzip" }, srv::full(bytes));
    Ok(srv::with_header(r, http::header::CONTENT_DISPOSITION, &format!("attachment; filename={name}")))
}

/// `GET /repos/{o}/{r}/compare/{base}...{head}` (also `..`).
pub async fn compare(s: &Site<'_>, basehead: &str) -> Response {
    let (base, head) = match basehead.split_once("...").or_else(|| basehead.split_once("..")) {
        Some(x) => x,
        None => return not_found(),
    };
    let (Some(b), Some(h)) = (s.resolve(base).await, s.resolve(head).await) else { return not_found() };
    let mb = s.text(&["merge-base", &b, &h]).await.unwrap_or_default();
    let ahead: i64 = s.text(&["rev-list", "--count", &format!("{b}..{h}")]).await.and_then(|x| x.parse().ok()).unwrap_or(0);
    let behind: i64 = s.text(&["rev-list", "--count", &format!("{h}..{b}")]).await.and_then(|x| x.parse().ok()).unwrap_or(0);
    let status = match (ahead, behind) {
        (0, 0) => "identical",
        (_, 0) => "ahead",
        (0, _) => "behind",
        _ => "diverged",
    };
    let list = s.text(&["rev-list", "--reverse", "--max-count=250", &format!("{b}..{h}")]).await.unwrap_or_default();
    let mut commits = Vec::new();
    for c in list.lines() {
        if let Some(v) = commit_json(s, c, false).await {
            commits.push(v);
        }
    }
    let (files, _) = diff_files(s, Some(if mb.is_empty() { &b } else { &mb }), &h).await;
    srv::json(
        StatusCode::OK,
        &json!({
            "url": format!("{}/compare/{basehead}", s.api), "html_url": format!("{}/compare/{basehead}", s.html),
            "status": status, "ahead_by": ahead, "behind_by": behind, "total_commits": ahead,
            "base_commit": commit_json(s, &b, false).await, "merge_base_commit": commit_json(s, if mb.is_empty() { &b } else { &mb }, false).await,
            "commits": commits, "files": files,
        }),
    )
}

/// `GET /user`: a job token is GitHub's Actions bot.
pub fn user(origin: &str) -> Response {
    srv::json(StatusCode::OK, &json!({ "login": "github-actions[bot]", "id": 41898282, "node_id": "MDM6Qm90NDE4OTgyODI=", "type": "Bot", "site_admin": false, "url": format!("{origin}/api/v3/users/github-actions[bot]"), "html_url": format!("{origin}/apps/github-actions") }))
}

/// `GET /rate_limit`: the facade doesn't limit jobs.
pub fn rate_limit() -> Response {
    let reset = crate::util::now_unix() + 3600;
    let r = json!({ "limit": 5000, "used": 0, "remaining": 5000, "reset": reset });
    srv::json(StatusCode::OK, &json!({ "resources": { "core": r, "search": r, "graphql": r }, "rate": r }))
}

/// `GET /repos/{o}/{r}/collaborators/{username}/permission`: a person's access to the repository
/// as GitHub says it, from what kiln grants them there, as its git serves them (the server's owner
/// is its admin; then `settings.manage`, an admin; `diffs.land`, a maintainer, who moves trunks;
/// `diffs.submit`, a writer, who pushes branches; `diffs.read` or `git.read`, a reader, who fetches;
/// else none), and 404 for no such person. openai/codex-action asks it of the run's actor before it
/// runs (ruff's security review, corpus pass 1025a).
pub async fn collaborator_permission(s: &Site<'_>, login: &str) -> Result<Response, Fail> {
    let role = if login.eq_ignore_ascii_case(&crate::authz::Caller::owner().name) {
        "admin"
    } else {
        let name = login.to_owned();
        let id: Option<String> = s
            .hub
            .store
            .call(move |c| {
                use rusqlite::OptionalExtension as _;
                Ok(c.query_row("SELECT id FROM users WHERE name = ?1 COLLATE NOCASE AND disabled_at IS NULL", [&name], |r| r.get(0)).optional()?)
            })
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        let Some(id) = id else { return Ok(srv::json(StatusCode::NOT_FOUND, &json!({ "message": "Not Found" }))) };
        let here = crate::authz::Scope::Repo { repo: s.repo.name.clone() };
        let mut role = "none";
        for (capability, held) in [("settings.manage", "admin"), ("diffs.land", "maintain"), ("diffs.submit", "write"), ("diffs.read", "read"), ("git.read", "read")] {
            if crate::authz::gate::user_may(s.hub, &id, capability, &here).await {
                role = held;
                break;
            }
        }
        role
    };
    Ok(srv::json(StatusCode::OK, &permission(&s.origin, login, role)))
}

/// GitHub's answer for `login` holding `role`: the legacy base permission beside the role
/// (maintain is write, triage read), and the collaborator with what the role lets them do.
fn permission(origin: &str, login: &str, role: &str) -> Value {
    let base = match role {
        "maintain" => "write",
        "triage" => "read",
        r => r,
    };
    let rank = ["none", "read", "triage", "write", "maintain", "admin"].iter().position(|r| *r == role).unwrap_or(0);
    let url = format!("{origin}/api/v3/users/{login}");
    json!({
        "permission": base,
        "role_name": role,
        "user": {
            "login": login, "id": 0, "node_id": "", "avatar_url": "", "gravatar_id": "", "type": "User", "site_admin": false,
            "url": url, "html_url": format!("{origin}/{login}"), "followers_url": format!("{url}/followers"), "following_url": format!("{url}/following{{/other_user}}"),
            "gists_url": format!("{url}/gists{{/gist_id}}"), "starred_url": format!("{url}/starred{{/owner}}{{/repo}}"), "subscriptions_url": format!("{url}/subscriptions"),
            "organizations_url": format!("{url}/orgs"), "repos_url": format!("{url}/repos"), "events_url": format!("{url}/events{{/privacy}}"), "received_events_url": format!("{url}/received_events"),
            "permissions": { "pull": rank >= 1, "triage": rank >= 2, "push": rank >= 3, "maintain": rank >= 4, "admin": rank >= 5 },
            "role_name": role,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W14-R11: a person's permission as GitHub's API gives it: the legacy base (maintain is
    /// write, triage read), the role, and what it lets them do.
    #[test]
    fn w14_r11_a_collaborators_permission_in_githubs_shape() {
        let at = |v: &Value, p: &str| v.pointer(p).cloned().unwrap_or(Value::Null);
        let p = permission("http://k", "dev", "maintain");
        assert_eq!((at(&p, "/permission"), at(&p, "/role_name"), at(&p, "/user/login")), (json!("write"), json!("maintain"), json!("dev")));
        assert_eq!(at(&p, "/user/permissions"), json!({ "pull": true, "triage": true, "push": true, "maintain": true, "admin": false }));
        assert_eq!(at(&p, "/user/url"), json!("http://k/api/v3/users/dev"));
        let none = permission("http://k", "stranger", "none");
        assert_eq!((at(&none, "/permission"), at(&none, "/user/permissions/pull")), (json!("none"), json!(false)));
        assert_eq!(at(&permission("http://k", "admin", "admin"), "/user/permissions/admin"), json!(true));
        assert_eq!(at(&permission("http://k", "r", "read"), "/permission"), json!("read"));
    }
}
