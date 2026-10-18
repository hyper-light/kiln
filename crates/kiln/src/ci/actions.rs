//! Actions on the server (docs/ci.md W07-R02, W07-R03): every remote `uses:` a run may reach is
//! resolved to a commit when the run starts (through composite actions, recursively), mirrored,
//! and served to the run's legs as a tarball of that commit, so every leg and every rerun of the
//! attempt runs exactly the same code, and a source that goes away later doesn't stop them.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use http::StatusCode;
use kiln_ci::wf::Workflow;
use kiln_ci::wf::ir::Step;
use kiln_ci::wire::ActionRef;

use super::{Fail, token};
use crate::gitops;
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

const GIT_TIMEOUT: Duration = Duration::from_secs(300);

/// A remote action: `owner/repo[/path]@ref`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub owner: String,
    pub repo: String,
    pub path: String,
    pub git_ref: String,
}

impl Remote {
    pub fn parse(uses: &str) -> Option<Remote> {
        if uses.starts_with("./") || uses.starts_with("$/") || uses.starts_with("docker://") || uses.starts_with("kiln/") || uses.starts_with("kiln://") {
            return None;
        }
        let (name, git_ref) = uses.split_once('@')?;
        let mut parts = name.splitn(3, '/');
        let (owner, repo) = (parts.next()?, parts.next()?);
        if owner.is_empty() || repo.is_empty() || git_ref.is_empty() {
            return None;
        }
        let safe = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) && !s.starts_with('.');
        if !safe(owner) || !safe(repo) || git_ref.starts_with('-') || git_ref.contains("..") {
            return None;
        }
        Some(Remote { owner: owner.into(), repo: repo.into(), path: parts.next().unwrap_or_default().trim_matches('/').into(), git_ref: git_ref.into() })
    }

    fn full(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

pub(crate) fn mirror(hub: &Hub, owner: &str, repo: &str) -> PathBuf {
    hub.cfg.data("actions").join(format!("{owner}__{repo}.git"))
}

fn tarball_path(hub: &Hub, owner: &str, repo: &str, sha: &str) -> PathBuf {
    hub.cfg.data("actions").join("tar").join(format!("{owner}__{repo}")).join(format!("{sha}.tar.gz"))
}

fn steps_uses(steps: &[Step], out: &mut Vec<String>) {
    for s in steps {
        if let Some(u) = &s.uses {
            out.push(u.clone());
        }
        steps_uses(&s.parallel, out);
    }
}

/// Every remote action `wf` uses, resolved: by `uses`, the commit and where to download it.
pub async fn resolve(hub: &Hub, wf: &Workflow) -> Result<BTreeMap<String, ActionRef>, String> {
    let mut todo: Vec<(String, usize)> = Vec::new();
    for j in &wf.jobs {
        let mut u = Vec::new();
        steps_uses(&j.steps, &mut u);
        todo.extend(u.into_iter().map(|x| (x, 0)));
    }
    let mut out: BTreeMap<String, ActionRef> = BTreeMap::new();
    let mut shas: BTreeMap<(String, String), String> = BTreeMap::new();
    while let Some((uses, depth)) = todo.pop() {
        if out.contains_key(&uses) {
            continue;
        }
        let Some(r) = Remote::parse(&uses) else { continue };
        if depth > 9 {
            return Err(format!("`{uses}`: composite actions nest at most 10 deep"));
        }
        let key = (r.full(), r.git_ref.clone());
        let sha = match shas.get(&key) {
            Some(s) => s.clone(),
            None => {
                let s = pin(hub, &r).await.map_err(|e| format!("`{uses}` could not be resolved: {e:#}"))?;
                shas.insert(key, s.clone());
                s
            }
        };
        // A composite action's own actions are the run's too.
        if let Some(meta) = metadata(hub, &r, &sha).await
            && let (Some(action), _) = kiln_ci::wf::parse_action(&meta)
            && action.runs.using == "composite"
        {
            let mut inner = Vec::new();
            steps_uses(&action.runs.steps, &mut inner);
            todo.extend(inner.into_iter().map(|x| (x, depth + 1)));
        }
        out.insert(uses.clone(), ActionRef { repo: r.full(), sha: sha.clone(), path: r.path.clone(), url: format!("/_kiln/actions/{}/{}/{sha}.tar.gz", r.owner, r.repo) });
    }
    Ok(out)
}

/// The commit `ref` names now, fetched into the mirror (W07-R02, W07-R03): a tag (peeled), a
/// branch, or a full SHA. When the source can't be reached, a ref the mirror knows still works.
pub(crate) async fn pin(hub: &Hub, r: &Remote) -> anyhow::Result<String> {
    let dir = mirror(hub, &r.owner, &r.repo);
    let d = dir.to_string_lossy().to_string();
    let url = format!("https://github.com/{}/{}.git", r.owner, r.repo);
    if !dir.join("HEAD").exists() {
        if let Some(parent) = dir.parent() {
            let p = parent.to_path_buf();
            crate::rt::blocking(move || std::fs::create_dir_all(p)).await??;
        }
        gitops::run_git(hub, &["clone", "--bare", "--quiet", &url, &d], GIT_TIMEOUT).await?;
    }
    let full_sha = r.git_ref.len() == 40 && r.git_ref.bytes().all(|b| b.is_ascii_hexdigit());
    let fetched = if full_sha {
        gitops::run_git(hub, &["-C", &d, "fetch", "--quiet", "origin", &r.git_ref], GIT_TIMEOUT).await.or(Ok::<_, anyhow::Error>(Vec::new())).map(|_| ())
    } else {
        gitops::run_git(hub, &["-C", &d, "fetch", "--quiet", "--force", "--prune", "origin", "+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"], GIT_TIMEOUT).await.map(|_| ())
    };
    if let Err(e) = fetched {
        tracing::warn!(action = %r.full(), error = %format!("{e:#}"), "action source unreachable; using the mirror");
    }
    for candidate in [format!("refs/tags/{}^{{commit}}", r.git_ref), format!("refs/heads/{}^{{commit}}", r.git_ref), format!("{}^{{commit}}", r.git_ref)] {
        if let Ok(o) = gitops::run_git(hub, &["-C", &d, "rev-parse", "--verify", "--quiet", &candidate], GIT_TIMEOUT).await {
            let sha = String::from_utf8_lossy(&o).trim().to_owned();
            if sha.len() == 40 {
                return Ok(sha);
            }
        }
    }
    anyhow::bail!("no tag, branch or commit `{}` in {} (or the action source is unreachable and it was never mirrored)", r.git_ref, r.full())
}

/// `action.yml` (or `.yaml`) of an action at a commit.
async fn metadata(hub: &Hub, r: &Remote, sha: &str) -> Option<String> {
    let d = mirror(hub, &r.owner, &r.repo).to_string_lossy().to_string();
    let base = if r.path.is_empty() { String::new() } else { format!("{}/", r.path) };
    for name in ["action.yml", "action.yaml"] {
        if let Ok(o) = gitops::run_git(hub, &["-C", &d, "show", &format!("{sha}:{base}{name}")], GIT_TIMEOUT).await {
            return Some(String::from_utf8_lossy(&o).into_owned());
        }
    }
    None
}

/// `GET /_kiln/actions/<owner>/<repo>/<sha>.tar.gz`, for a leg's token (W07-R03).
pub async fn tarball(hub: &Arc<Hub>, rest: &[&str], req: &Request) -> Result<Response, Fail> {
    let [owner, repo, file] = rest else { return Err((StatusCode::NOT_FOUND, "no such action".into())) };
    let sha = file.trim_end_matches(".tar.gz");
    let ok_name = |s: &str| !s.is_empty() && !s.starts_with('.') && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if !ok_name(owner) || !ok_name(repo) || sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err((StatusCode::NOT_FOUND, "no such action".into()));
    }
    let leg = token::from_headers(req.headers()).and_then(|t| token::verify(hub, &t));
    if leg.is_none() {
        return Err((StatusCode::UNAUTHORIZED, "a leg's token is needed".into()));
    }
    let path = tarball_path(hub, owner, repo, sha);
    if !path.exists() {
        let d = mirror(hub, owner, repo).to_string_lossy().to_string();
        let out = gitops::run_git(hub, &["-C", &d, "archive", "--format=tar.gz", sha], GIT_TIMEOUT).await.map_err(|e| (StatusCode::NOT_FOUND, format!("action not mirrored: {e:#}")))?;
        let p = path.clone();
        crate::rt::blocking(move || -> std::io::Result<()> {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = p.with_extension("partial");
            std::fs::write(&tmp, out)?;
            std::fs::rename(tmp, p)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    let rx = crate::rt::file_source(path).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(srv::response(StatusCode::OK, "application/gzip", srv::channel_body(rx)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms() {
        assert_eq!(Remote::parse("actions/checkout@v4"), Some(Remote { owner: "actions".into(), repo: "checkout".into(), path: String::new(), git_ref: "v4".into() }));
        assert_eq!(Remote::parse("github/codeql-action/init@v3").map(|r| r.path), Some("init".into()));
        for local in ["./.github/actions/x", "$/actions/y", "docker://alpine:3", "kiln/cache@v1", "nope", "a/b@", "../x/y@v1", "a/b@-x"] {
            assert!(Remote::parse(local).is_none(), "{local}");
        }
    }
}
