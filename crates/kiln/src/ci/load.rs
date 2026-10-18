//! Reading a commit's workflows (docs/ci.md W02-R01, W02-R02, W02-R11): which files are
//! workflows, overlays and actions; whether `.github/workflows` run; each workflow parsed once per
//! content and kept in `ci_workflows`. A file with errors is still returned, with its
//! diagnostics, so the run it would have started shows them (`startup_failure`) instead of
//! nothing happening.

use std::collections::HashMap;

use kiln_ci::expr::Dialect;
use kiln_ci::wf::diag::StoredDiag;
use kiln_ci::wf::{self, FileKind, GithubWorkflows, Workflow};
use rusqlite::{OptionalExtension, params};

use crate::gitops::{self, LS_REMOTE_TIMEOUT};
use crate::hub::Hub;
use crate::store::Repo;

/// One workflow of a commit.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub path: String,
    pub dialect: Dialect,
    /// Whether it runs: a GitHub workflow may not (W02-R02, [`Commit::github_note`]).
    pub runs: bool,
    /// `None` when the file couldn't be read as a workflow at all.
    pub workflow: Option<Workflow>,
    pub diagnostics: Vec<StoredDiag>,
}

impl Loaded {
    pub fn has_errors(&self) -> bool {
        self.workflow.is_none() || self.diagnostics.iter().any(|d| d.severity == wf::Severity::Error)
    }
}

/// A commit's workflows, and why GitHub's run or not.
#[derive(Debug, Clone, Default)]
pub struct Commit {
    pub workflows: Vec<Loaded>,
    pub github_run: bool,
    pub github_note: String,
}

/// Is `r` a ref this module passes to git: a branch, tag, `pull/<n>/head`, `HEAD` or SHA, never
/// an option or a revision expression.
pub fn ref_ok(r: &str) -> bool {
    !r.is_empty() && r.len() <= 255 && !r.starts_with(['-', '/']) && !r.contains("..") && r.chars().all(|c| c.is_ascii_alphanumeric() || "._/+-".contains(c))
}

/// The commit `r` names ([`ref_ok`]; `HEAD` is the default branch), fetching once when the
/// mirror doesn't know it. `None` when the repository has no such ref.
pub async fn resolve(hub: &Hub, repo: &Repo, r: &str) -> anyhow::Result<Option<String>> {
    anyhow::ensure!(ref_ok(r), "`{r}` is not a branch, tag or commit");
    let path = gitops::mirror_path(hub, &repo.name);
    let p = path.to_string_lossy().to_string();
    for fetch in [false, true] {
        if fetch {
            gitops::sync_mirror(hub, repo).await?;
        }
        if !path.join("HEAD").exists() {
            continue;
        }
        if let Ok(out) = gitops::run_git(hub, &["-C", &p, "rev-parse", "--verify", "--quiet", &format!("{r}^{{commit}}")], LS_REMOTE_TIMEOUT).await {
            let sha = String::from_utf8_lossy(&out).trim().to_owned();
            if sha.len() == 40 {
                return Ok(Some(sha));
            }
        }
    }
    Ok(None)
}

/// A file's text at a commit the mirror has.
pub async fn blob(hub: &Hub, repo: &Repo, sha: &str, path: &str) -> anyhow::Result<String> {
    let mirror = gitops::ensure_commit(hub, repo, sha).await?;
    let m = mirror.to_string_lossy().to_string();
    Ok(String::from_utf8_lossy(&gitops::run_git(hub, &["-C", &m, "show", &format!("{sha}:{path}")], LS_REMOTE_TIMEOUT).await?).into_owned())
}

/// Where [`load_from`] reads a commit and keeps what it parsed: the server's mirror and store
/// ([`load`]), or a plain repository and memory (tests).
pub trait Source: Sync {
    /// What `git ls-tree -r <sha> -- .github/workflows .kiln` prints.
    fn tree(&self, sha: &str) -> impl Future<Output = anyhow::Result<String>> + Send;
    fn blob(&self, oid: &str) -> impl Future<Output = anyhow::Result<String>> + Send;
    /// What [`Source::remember`] kept for this path and content.
    fn cached(&self, path: &str, key: &str) -> impl Future<Output = anyhow::Result<Option<Loaded>>> + Send;
    fn remember(&self, key: &str, l: &Loaded) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// Every workflow file at `sha`, with its overlay applied and the W02-R02 precedence decided.
/// GitHub workflows that don't run are read too (`runs: false`), so pages can show them.
pub async fn load(hub: &Hub, repo: &Repo, sha: &str) -> anyhow::Result<Commit> {
    let mirror = gitops::ensure_commit(hub, repo, sha).await?;
    let src = Mirror { hub, repo, dir: mirror.to_string_lossy().into_owned() };
    // The repository's own setting, else the server's (W02-R02).
    let setting = super::repo_config::load(hub, repo).await.github_workflows.unwrap_or(hub.cfg.ci.github_workflows);
    load_from(&src, sha, setting).await
}

/// [`load`], reading through `src`.
pub async fn load_from(src: &impl Source, sha: &str, setting: GithubWorkflows) -> anyhow::Result<Commit> {
    // "<mode> blob <oid>\t<path>"
    let mut files: Vec<(String, String, FileKind)> = Vec::new();
    for line in src.tree(sha).await?.lines() {
        let Some((meta, path)) = line.split_once('\t') else { continue };
        let Some(oid) = meta.split_whitespace().nth(2) else { continue };
        if let Some(kind) = wf::file_kind(path) {
            files.push((path.to_owned(), oid.to_owned(), kind));
        }
    }
    let has_kiln = files.iter().any(|(_, _, k)| *k == FileKind::KilnWorkflow);
    let (run_github, note) = wf::github_workflows_run(setting, has_kiln);
    let overlays: HashMap<String, (String, String)> =
        files.iter().filter_map(|(p, oid, k)| if let FileKind::Overlay { stem } = k { Some((stem.clone(), (p.clone(), oid.clone()))) } else { None }).collect();
    let mut out = Commit { workflows: Vec::new(), github_run: run_github, github_note: note.to_owned() };
    for (path, oid, kind) in &files {
        let (dialect, runs) = match kind {
            FileKind::KilnWorkflow => (Dialect::Kiln, true),
            FileKind::GhaWorkflow => (Dialect::Gha, run_github),
            _ => continue,
        };
        let stem = path.rsplit('/').next().and_then(|n| n.strip_suffix(".yml").or_else(|| n.strip_suffix(".yaml"))).unwrap_or_default().to_owned();
        let overlay = if dialect == Dialect::Gha { overlays.get(&stem).cloned() } else { None };
        let content_key = match &overlay {
            Some((_, ooid)) => format!("{oid}+{ooid}"),
            None => oid.clone(),
        };
        if let Some(hit) = src.cached(path, &content_key).await? {
            out.workflows.push(Loaded { runs, ..hit });
            continue;
        }
        let text = src.blob(oid).await?;
        let p = path.clone();
        let (mut workflow, diags) = crate::rt::blocking(move || wf::parse_workflow(&p, &text, dialect)).await?;
        let mut all = diags;
        if let (Some(w), Some((opath, ooid))) = (workflow.as_mut(), &overlay) {
            let (ov, mut odiags) = wf::parse_overlay(&src.blob(ooid).await?);
            for d in &mut odiags {
                d.path = format!("{opath}: {}", d.path);
            }
            all.extend(odiags);
            if let Some(ov) = ov {
                all.extend(wf::apply_overlay(w, &ov));
            }
        }
        let loaded = Loaded { path: path.clone(), dialect, runs, workflow, diagnostics: stored(&all) };
        src.remember(&content_key, &loaded).await?;
        out.workflows.push(loaded);
    }
    Ok(out)
}

/// The server's [`Source`]: the repository's mirror, and `ci_workflows`.
struct Mirror<'a> {
    hub: &'a Hub,
    repo: &'a Repo,
    dir: String,
}

impl Source for Mirror<'_> {
    async fn tree(&self, sha: &str) -> anyhow::Result<String> {
        let out = gitops::run_git(self.hub, &["-C", &self.dir, "ls-tree", "-r", sha, "--", ".github/workflows", ".kiln"], LS_REMOTE_TIMEOUT).await?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    async fn blob(&self, oid: &str) -> anyhow::Result<String> {
        Ok(String::from_utf8_lossy(&gitops::run_git(self.hub, &["-C", &self.dir, "cat-file", "blob", oid], LS_REMOTE_TIMEOUT).await?).into_owned())
    }

    async fn cached(&self, path: &str, key: &str) -> anyhow::Result<Option<Loaded>> {
        cached(self.hub, &self.repo.id, path, key).await
    }

    async fn remember(&self, key: &str, l: &Loaded) -> anyhow::Result<()> {
        remember(self.hub, &self.repo.id, key, l).await
    }
}

/// Diagnostics as they are kept and served.
pub fn stored(d: &[wf::Diag]) -> Vec<StoredDiag> {
    serde_json::to_value(d).ok().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
}

async fn cached(hub: &Hub, repo: &str, path: &str, key: &str) -> anyhow::Result<Option<Loaded>> {
    let (repo, path, key) = (repo.to_owned(), path.to_owned(), key.to_owned());
    hub.store
        .call(move |c| {
            let row: Option<(String, Option<String>, String)> = c
                .query_row("SELECT dialect, def, diagnostics FROM ci_workflows WHERE repo_id = ?1 AND path = ?2 AND content_key = ?3", params![repo, path, key], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()?;
            Ok(row.map(|(dialect, def, diags)| Loaded {
                path: path.clone(),
                dialect: if dialect == "gha" { Dialect::Gha } else { Dialect::Kiln },
                runs: true,
                workflow: def.and_then(|d| serde_json::from_str(&d).ok()),
                diagnostics: serde_json::from_str(&diags).unwrap_or_default(),
            }))
        })
        .await
}

async fn remember(hub: &Hub, repo: &str, key: &str, l: &Loaded) -> anyhow::Result<()> {
    let (repo, key) = (repo.to_owned(), key.to_owned());
    let path = l.path.clone();
    let dialect = if l.dialect == Dialect::Gha { "gha" } else { "kiln" };
    let name = l.workflow.as_ref().and_then(|w| w.name.clone()).unwrap_or_else(|| path.clone());
    let def = l.workflow.as_ref().and_then(|w| serde_json::to_string(w).ok());
    let diags = serde_json::to_string(&l.diagnostics).unwrap_or_else(|_| "[]".into());
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT OR IGNORE INTO ci_workflows (id, repo_id, path, content_key, dialect, name, def, diagnostics, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![crate::util::new_id(), repo, path, key, dialect, name, def, diags, crate::util::now_unix()],
            )?;
            Ok(())
        })
        .await
}
