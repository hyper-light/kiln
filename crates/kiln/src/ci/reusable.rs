//! The reusable workflows a run can call (docs/ci.md W04-R18, W07-R02). The planner asks for them
//! by their `uses:` while the run goes on, since a call is expanded when its job is ready.
//! - **Local** (`./.github/workflows/x.yml`): the commit's own workflows.
//! - **Remote** (`owner/repo/.github/workflows/x.yml@ref`): pinned when the run starts,
//!   through the same mirror as actions, and read at that commit; nested calls too, up to
//!   GitHub's 10 levels.

use std::collections::HashMap;
use std::sync::Arc;

use kiln_ci::expr::Dialect;
use kiln_ci::wf::Workflow;

use super::actions::{self, Remote};
use super::load;
use crate::gitops;
use crate::hub::Hub;
use crate::store::Repo;

/// The workflows a run of `wf` at `sha` may call, by the `uses:` that names them (local ones
/// also by their path).
pub async fn files(hub: &Hub, repo: &Repo, sha: &str, wf: &Workflow) -> HashMap<String, Arc<Workflow>> {
    let mut out: HashMap<String, Arc<Workflow>> = HashMap::new();
    // A workflow that calls none (most do) needs nothing read: no git for its run to start.
    if calls(wf).is_empty() {
        return out;
    }
    match load::load(hub, repo, sha).await {
        Ok(commit) => {
            for l in commit.workflows {
                if let Some(w) = l.workflow {
                    out.insert(l.path.clone(), Arc::new(w));
                }
            }
        }
        Err(e) => tracing::warn!(repo = %repo.name, error = %format!("{e:#}"), "the commit's workflows could not be read for reusable calls"),
    }
    // Remote calls, followed through the workflows they bring.
    let mut todo: Vec<(String, usize)> = calls(wf).into_iter().map(|u| (u, 0)).collect();
    for w in out.values() {
        todo.extend(calls(w).into_iter().map(|u| (u, 0)));
    }
    while let Some((uses, depth)) = todo.pop() {
        if out.contains_key(&uses) || uses.starts_with("./") || uses.starts_with("$/") || depth >= 10 {
            continue;
        }
        let Some(r) = Remote::parse(&uses) else { continue };
        match remote(hub, &r).await {
            Ok(w) => {
                todo.extend(calls(&w).into_iter().map(|u| (u, depth + 1)));
                out.insert(uses, Arc::new(w));
            }
            Err(e) => tracing::warn!(uses = %uses, error = %format!("{e:#}"), "a reusable workflow could not be fetched"),
        }
    }
    out
}

/// The `uses:` of a workflow's jobs.
fn calls(wf: &Workflow) -> Vec<String> {
    wf.jobs.iter().filter_map(|j| j.uses.clone()).collect()
}

/// A remote reusable workflow at its pinned commit.
async fn remote(hub: &Hub, r: &Remote) -> anyhow::Result<Workflow> {
    let sha = actions::pin(hub, r).await?;
    let mirror = r.mirror(hub).to_string_lossy().to_string();
    let src = gitops::run_git(hub, &["-C", &mirror, "show", &format!("{sha}:{}", r.path)], std::time::Duration::from_secs(60)).await?;
    let text = String::from_utf8_lossy(&src).into_owned();
    let (w, diags) = kiln_ci::wf::parse_workflow(&r.path, &text, Dialect::Gha);
    if let Some(d) = diags.iter().find(|d| d.is_error()) {
        anyhow::bail!("{}@{}: {} {}", r.path, r.git_ref, d.code, d.message);
    }
    w.ok_or_else(|| anyhow::anyhow!("{} is not a workflow", r.path))
}
