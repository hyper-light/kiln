//! Runners' warm workspaces, as placement takes them (docs/ci.md W24-R18). A runner says what it
//! holds warm (`AgentMsg::WorkState`, `kiln_ci::work::slots::WorkState`); this measures how far
//! each lane's freshest checkout is behind the newest commit of its repository's default branch
//! in the server's mirror (commits, and files changed between the two), and makes of it what the
//! scheduler takes (`Input::Warm`, through `kiln_ci::work::slots::held`). A distance git can't
//! give (a commit the mirror lacks) counts as none.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use kiln_ci::sched::warm::Held;
use kiln_ci::work::slots::{self, WorkState};

use crate::hub::Hub;

/// How long one git command over a mirror may take.
const GIT: Duration = Duration::from_secs(10);

/// The most distances kept: past it they're measured again.
const KEPT: usize = 4096;

/// Distances measured, by repository, checkout and the commit measured against: one checkout's
/// distance from one commit never changes.
static MEASURED: LazyLock<parking_lot::Mutex<HashMap<(String, String, String), (u32, u32)>>> = LazyLock::new(Default::default);

/// A full commit SHA, as git takes it without reading it for an option: what a runner says it
/// checked out is nothing else.
fn is_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// What git printed in `repo`'s mirror for `args`, trimmed; `None` where it failed.
async fn git(hub: &Hub, repo: &str, args: &[&str]) -> Option<String> {
    let mirror = crate::gitops::mirror_path(hub, repo).to_string_lossy().into_owned();
    let mut all = vec!["-C", mirror.as_str()];
    all.extend_from_slice(args);
    let out = crate::gitops::run_git(hub, &all, GIT).await.ok()?;
    Some(String::from_utf8_lossy(&out).trim().to_owned())
}

/// The newest commit of `repo`'s default branch in the server's mirror.
async fn head(hub: &Hub, repo: &str) -> Option<String> {
    git(hub, repo, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]).await.filter(|h| is_sha(h))
}

/// How far `base` is behind `head` in `repo`'s mirror: commits, and files changed between them.
async fn distance(hub: &Hub, repo: &str, base: &str, head: &str) -> Option<(u32, u32)> {
    if base == head {
        return Some((0, 0));
    }
    let at = (repo.to_owned(), base.to_owned(), head.to_owned());
    if let Some(d) = MEASURED.lock().get(&at) {
        return Some(*d);
    }
    let range = format!("{base}..{head}");
    let behind = git(hub, repo, &["rev-list", "--count", range.as_str()]).await?.parse::<u32>().ok()?;
    let files = git(hub, repo, &["diff", "--name-only", base, head]).await?.lines().filter(|l| !l.is_empty()).count();
    let d = (behind, u32::try_from(files).unwrap_or(u32::MAX));
    let mut kept = MEASURED.lock();
    if kept.len() >= KEPT {
        kept.clear();
    }
    kept.insert(at, d);
    Some(d)
}

/// A runner's report of what it holds warm, as placement takes it: each lane's freshest checkout
/// measured against its repository's newest commit (W24-R18).
pub async fn held(hub: &Hub, state: &WorkState) -> Held {
    let mut heads: HashMap<String, Option<String>> = HashMap::new();
    let mut known: HashMap<(String, String), (u32, u32)> = HashMap::new();
    for l in &state.lanes {
        let Some(base) = l.freshest().and_then(|s| s.base.clone()).filter(|b| is_sha(b)) else { continue };
        let repo = l.lane.repo.clone();
        let newest = match heads.get(&repo) {
            Some(h) => h.clone(),
            None => {
                let h = head(hub, &repo).await;
                heads.insert(repo.clone(), h.clone());
                h
            }
        };
        let Some(newest) = newest else { continue };
        if let Some(d) = distance(hub, &repo, &base, &newest).await {
            known.insert((repo, base), d);
        }
    }
    slots::held(state, &|repo, base| known.get(&(repo.to_owned(), base.to_owned())).copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a runner says it checked out reaches git only as a full SHA: never as an option.
    #[test]
    fn w24_r18_only_a_full_sha_reaches_git() {
        assert!(is_sha(&"a".repeat(40)) && is_sha(&"0f".repeat(32)));
        for bad in ["--output=/tmp/x", "HEAD", "abc123", &"g".repeat(40), &format!("{}..HEAD", "a".repeat(40))] {
            assert!(!is_sha(bad), "{bad}");
        }
    }
}
