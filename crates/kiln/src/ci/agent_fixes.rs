//! A repository's switch that lets agents fix code from agents' feedback (docs/ci.md W26-R18):
//! set on its page in Repos, off until turned on, kept with who set it and when, and audited.
//! While it's off, an agent's comment on an agent's change starts nothing; on, it goes to that
//! change's agent, which pushes a fix and answers in the thread. What reaches an agent is decided
//! by `kiln_ci::review::agents`; this keeps the switch.

use http::StatusCode;
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::Fail;
use crate::authz::{Caller, Scope};
use crate::hub::Hub;

/// What the switch does, as the repository's page says it.
pub const SAYS: &str = "On: an agent's comment on an agent's change goes to that change's agent, which pushes a fix and answers in the thread. Expect more versions and CI runs. Off: agents' comments wait for a person, who can Hand off.";

/// The capability that changes it, as changes the conflict ladder's automatic resolution
/// (W26-R17) and the land queue.
const CHANGES: &str = "diffs.land";

/// Whether repository `repo_id` lets agents fix code from agents' feedback, and who set it and
/// when; off, with no one, until someone turns it on.
pub async fn of(hub: &Hub, repo_id: &str) -> (bool, Option<(String, i64)>) {
    let id = repo_id.to_owned();
    let row = hub
        .store
        .call(move |c| Ok(c.query_row("SELECT agents_fix, set_by, set_at FROM review_agent_fixes WHERE repo_id = ?1", [&id], |r| Ok((r.get::<_, bool>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))).optional()?))
        .await
        .ok()
        .flatten();
    row.map_or((false, None), |(on, by, at)| (on, Some((by, at))))
}

/// Whether repository `repo_id` lets agents fix code from agents' feedback.
pub async fn on(hub: &Hub, repo_id: &str) -> bool {
    of(hub, repo_id).await.0
}

/// The switch of the repository named `repo` (`owner/name`), as [`of`] answers it.
pub async fn of_named(hub: &Hub, repo: &str) -> (bool, Option<(String, i64)>) {
    let name = repo.to_owned();
    let row = hub
        .store
        .call(move |c| {
            Ok(c.query_row("SELECT f.agents_fix, f.set_by, f.set_at FROM review_agent_fixes f JOIN repos r ON r.id = f.repo_id WHERE r.name = ?1", [&name], |r| Ok((r.get::<_, bool>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))
                .optional()?)
        })
        .await
        .ok()
        .flatten();
    row.map_or((false, None), |(on, by, at)| (on, Some((by, at))))
}

/// Who set a switch and when, in words: `turned on by ada, 31 Oct 2026`.
pub fn set_words(on: bool, set: Option<&(String, i64)>) -> String {
    let state = if on { "on" } else { "off" };
    let Some((by, at)) = set else { return format!("{state} until someone turns it on") };
    let day = time::OffsetDateTime::from_unix_timestamp(*at).ok().and_then(|t| t.format(&time::macros::format_description!("[day padding:none] [month repr:short] [year]")).ok());
    format!("turned {state} by {}{}", kiln_ci::review::named(by), day.map(|d| format!(", {d}")).unwrap_or_default())
}

/// `GET` or `PUT /api/repos/:repo/agent-fixes` (`{agents_fix}`): the switch, who set it and when,
/// what it does, and whether the caller may change it (`diffs.land` on the repository). A change is
/// audited.
pub async fn route(hub: &Hub, repo: &str, caller: &Caller, body: Option<&Value>) -> Result<Value, Fail> {
    let fail = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let r = hub.store.repos().await.map_err(fail)?.into_iter().find(|r| r.name == repo).ok_or((StatusCode::NOT_FOUND, format!("no repository {repo}")))?;
    let scope = Scope::Repo { repo: r.name.clone() };
    if !caller.can("diffs.read", &scope) {
        return Err((StatusCode::FORBIDDEN, "reading a repository's agent fixes needs `diffs.read`".into()));
    }
    let may = caller.can(CHANGES, &scope);
    if let Some(body) = body {
        if !may {
            return Err((StatusCode::FORBIDDEN, format!("turning agent fixes on or off needs `{CHANGES}` on {}", r.name)));
        }
        let on = body.get("agents_fix").and_then(Value::as_bool).ok_or_else(|| (StatusCode::BAD_REQUEST, "`agents_fix`: true or false".to_owned()))?;
        let (id, by, now) = (r.id.clone(), caller.name.clone(), crate::util::now_unix());
        hub.store
            .call(move |c| {
                c.execute(
                    "INSERT INTO review_agent_fixes (repo_id, agents_fix, set_by, set_at) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (repo_id) DO UPDATE SET agents_fix = excluded.agents_fix, set_by = excluded.set_by, set_at = excluded.set_at",
                    params![id, on, by, now],
                )?;
                Ok(())
            })
            .await
            .map_err(fail)?;
        hub.store.audit(caller.name.clone(), "repo.agent_fixes".into(), format!("{}: agents fix from agents' feedback {}", r.name, if on { "on" } else { "off" })).await;
    }
    let (on, set) = of(hub, &r.id).await;
    let (set_by, set_at) = set.map_or((None, None), |(by, at)| (Some(by), Some(at)));
    Ok(json!({ "repo": r.name, "agents_fix": on, "set_by": set_by, "set_at": set_at, "says": SAYS, "may_change": may }))
}
