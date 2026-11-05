//! Asking about code no agent made (docs/ci.md W26-R07's Files, components/code/about.tsx): in
//! Ship, a person's change, a lone commit or a host's pull request has no agent to ask, so kiln
//! starts a helper for the person who asked, as the conflict ladder starts its agents (W26-R17).
//! It gets the repository at that commit in a folder of its own, reads and searches it there, and
//! changes nothing. It's the card's, as an agent's helper is (`crate::harness::about`): ended once
//! no card reads it.

use std::sync::Arc;

use http::StatusCode;
use serde_json::{Value, json};

use super::Fail;
use crate::authz::{Caller, Scope};
use crate::harness::lineage::{self, Ask, Starter};
use crate::harness::ops::{Tapped, about_prompt};
use crate::hub::Hub;
use crate::store::Repo;

/// The harness a helper runs in, and its mode: Claude Code's classifier lets it fetch the
/// repository into its own folder without a person there to approve each command.
const KIND: &str = "claude";
const MODE: &str = "auto";

/// Whether `s` reads as a commit's id: 7 to 64 hex digits.
fn is_commit(s: &str) -> bool {
    (7..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// How the helper gets `r` at `commit` into `dir`: kiln's own copy of a repository it hosts (its
/// git serves any commit), else the repository's own clone URL, which serves a host's pull
/// request's head by its commit too.
pub fn fetch_brief(public_url: &str, r: &Repo, commit: &str, dir: &str) -> String {
    let url = if r.mode == "native" { format!("{}/git/{}.git", public_url.trim_end_matches('/'), r.name) } else { r.clone_url.clone() };
    format!(
        "First get the repository at that commit, in a folder of your own:\n\n    git clone --filter=blob:none --no-checkout {url} {dir}\n    git -C {dir} fetch --filter=blob:none origin {commit}\n    git -C {dir} checkout --detach {commit}\n\nRead and search it there, and nowhere else. If you can't get it, answer from the code below alone, and say so under \"Worth knowing\"."
    )
}

/// `POST /api/repos/:repo/about` (`ci.repos.about`): what a person tapped in a change no agent
/// made, at `commit`, asked of a helper kiln starts for them. Answered as `agents.about` answers:
/// the helper, which the card reads with `agents.result`.
pub async fn start(hub: &Arc<Hub>, repo: &str, caller: &Caller, body: &Value) -> Result<Value, Fail> {
    let fail = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let r = hub.store.repos().await.map_err(fail)?.into_iter().find(|r| r.name == repo).ok_or((StatusCode::NOT_FOUND, format!("no repository {repo}")))?;
    if !caller.can("diffs.read", &Scope::Repo { repo: r.name.clone() }) {
        return Err((StatusCode::FORBIDDEN, format!("asking about {}'s code needs `diffs.read` there", r.name)));
    }
    let s = |k: &str| body.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
    let commit = s("commit").filter(|c| is_commit(c)).ok_or((StatusCode::BAD_REQUEST, "`commit`: the commit the code is at".to_owned()))?;
    let path = s("path").ok_or((StatusCode::BAD_REQUEST, "`path`: the file the code is in".to_owned()))?;
    let code = s("code").ok_or((StatusCode::BAD_REQUEST, "`code`: the code around what was tapped".to_owned()))?;
    let selection = s("selection");
    let tapped = Tapped {
        path,
        line: body.get("line").and_then(Value::as_u64),
        end: body.get("end_line").and_then(Value::as_u64),
        selection,
        change: body.get("change").and_then(Value::as_bool).unwrap_or(false),
        question: s("question"),
    };
    let dir = format!("~/.cache/kiln/about/{}", crate::util::new_id());
    let prompt = format!("{}\n\n{}", fetch_brief(&hub.cfg.server.public_url, &r, commit, &dir), about_prompt(&tapped, code));
    let by = Starter { parent: None, opener: caller.opener(), actor: caller.name.clone(), plan_run: None, asker: Some(caller.clone()) };
    let name = format!("About {}", selection.unwrap_or(path).chars().take(48).collect::<String>());
    let ask = Ask { kind: KIND.into(), prompt: Some(prompt), mode: Some(MODE.into()), name: Some(name), count: Some(1), helper: Some(crate::harness::about::ABOUT.to_owned()), ..Ask::default() };
    let infos = lineage::start(hub, &by, ask).await?;
    // It's the card's: ended once no card reads it.
    for i in &infos {
        crate::harness::about::started(hub.clone(), i.id.clone(), caller.name.clone());
    }
    Ok(json!({"agents": infos.iter().map(crate::harness::wait::brief).collect::<Vec<_>>(), "itself": false}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(mode: &str) -> Repo {
        Repo {
            id: "r1".into(),
            name: "acme/app".into(),
            clone_url: "https://github.com/acme/app.git".into(),
            branches: vec!["main".into()],
            poll_seconds: 60,
            enabled: true,
            created_at: 0,
            last_polled_at: None,
            last_error: None,
            mode: mode.into(),
            default_branch: None,
            mirror: None,
        }
    }

    /// docs/ci.md W26-R07: a helper gets a repository kiln hosts from kiln's own git, and one kiln
    /// follows from where it lives, at the commit the code is at, in a folder of its own.
    #[test]
    fn w26_r07_a_helper_fetches_the_commit_from_where_the_repository_lives() {
        let native = fetch_brief("https://kiln.example/", &repo("native"), "abc1234", "~/.cache/kiln/about/x");
        assert!(native.contains("git clone --filter=blob:none --no-checkout https://kiln.example/git/acme/app.git ~/.cache/kiln/about/x"), "{native}");
        assert!(native.contains("fetch --filter=blob:none origin abc1234") && native.contains("checkout --detach abc1234"), "{native}");
        let upstream = fetch_brief("https://kiln.example", &repo("upstream"), "abc1234", "~/x");
        assert!(upstream.contains("--no-checkout https://github.com/acme/app.git ~/x"), "{upstream}");
        assert!(is_commit("abc1234") && is_commit(&"f".repeat(40)) && !is_commit("abc12") && !is_commit("main") && !is_commit(&"f".repeat(65)));
    }
}
