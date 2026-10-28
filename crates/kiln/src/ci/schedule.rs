//! Schedules (docs/ci.md W03-R10): every minute, the `schedule` entries of the workflows on each
//! repository's default branch that are due start runs on that branch's latest commit, with the
//! cron that fired in `github.event.schedule`.
//!
//! - **GitHub-exact:** five-field cron in UTC, one run per entry that fires. **Deviation:** no
//!   five-minute minimum, and no disabling after inactivity.
//! - **kiln's additions, opt-in per entry:**
//!   - `timezone`, with DST: a skipped local time runs at the next minute, a repeated one once;
//!   - `jitter`, the same delay every time for the same entry;
//!   - `branch`: run on another branch's latest commit;
//!   - `inputs`;
//!   - `skip-if-unchanged`: skip when the last successful scheduled run had this commit;
//!   - `catch-up`: one run for times missed while the server was down.
//!
//! Each fire is recorded (`ci_schedule_fires`), so a minute fires once whatever restarts.

use std::collections::HashMap;
use std::sync::Arc;

use kiln_ci::trigger::cron::{self, Cron};
use kiln_ci::trigger::{Event, Source, context};
use kiln_ci::wf::ir::{Schedule, YVal};
use rusqlite::{OptionalExtension, params};

use super::events;
use super::load;
use crate::hub::Hub;
use crate::store::Repo;

/// The commits' workflows read so far, by `(repository, commit)`; kept while they are heads.
#[derive(Default)]
pub struct Cache {
    commits: HashMap<(String, String), Arc<load::Commit>>,
}

impl Cache {
    async fn get(&mut self, hub: &Hub, repo: &Repo, sha: &str) -> anyhow::Result<Arc<load::Commit>> {
        let key = (repo.id.clone(), sha.to_owned());
        if let Some(c) = self.commits.get(&key) {
            return Ok(Arc::clone(c));
        }
        let c = Arc::new(load::load(hub, repo, sha).await?);
        self.commits.insert(key, Arc::clone(&c));
        Ok(c)
    }

    /// Forget commits that are no longer a branch head the schedules read.
    fn keep(&mut self, live: &[(String, String)]) {
        self.commits.retain(|k, _| live.contains(k));
    }
}

/// Starts what is due at the minute beginning at `minute` (unix seconds).
pub async fn tick(hub: &Hub, minute: i64, cache: &mut Cache) {
    if !hub.ci.enabled() {
        return;
    }
    let repos = match hub.store.repos().await {
        Ok(r) => r,
        Err(e) => return tracing::warn!(error = %format!("{e:#}"), "schedules: the repositories could not be read"),
    };
    let mut live = Vec::new();
    for repo in repos {
        match repo_tick(hub, &repo, minute, cache, &mut live).await {
            Ok(()) => {}
            Err(e) => tracing::warn!(repo = %repo.name, error = %format!("{e:#}"), "schedules"),
        }
    }
    cache.keep(&live);
}

async fn repo_tick(hub: &Hub, repo: &Repo, minute: i64, cache: &mut Cache, live: &mut Vec<(String, String)>) -> anyhow::Result<()> {
    let refs: HashMap<String, String> = hub.store.refs(repo.id.clone()).await?.into_iter().collect();
    let Some(head) = refs.get("HEAD->").cloned() else { return Ok(()) };
    let Some(sha) = refs.get(&head).cloned() else { return Ok(()) };
    live.push((repo.id.clone(), sha.clone()));
    let commit = cache.get(hub, repo, &sha).await?;
    for (l, i, s) in entries(&commit, hub.cfg.ci.kiln_workflows) {
        if let Err(e) = entry(hub, repo, &refs, &head, &sha, l, i, s, minute, cache, live).await {
            tracing::warn!(repo = %repo.name, workflow = %l.path, cron = %s.cron, error = %format!("{e:#}"), "a schedule could not run");
        }
    }
    Ok(())
}

/// The schedule entries of a commit's workflows the engine starts, each with its index in its
/// workflow: GitHub's, and the kiln dialect's only once `[ci] kiln_workflows` takes them off v1,
/// as for every other event (`events::on_trigger`). v1's scheduler fires them otherwise: with the
/// engine on and `kiln_workflows` off, both did, and each `.kiln/` schedule ran twice (W28 step 3's
/// replay: wire-lab's and sim's every night).
fn entries(commit: &load::Commit, kiln_workflows: bool) -> Vec<(&load::Loaded, usize, &Schedule)> {
    commit
        .workflows
        .iter()
        .filter(|l| l.runs && !l.has_errors() && (l.dialect == kiln_ci::expr::Dialect::Gha || kiln_workflows))
        .filter_map(|l| l.workflow.as_ref().map(|w| (l, w)))
        .flat_map(|(l, w)| w.on.iter().filter(|t| t.event == "schedule").flat_map(|t| t.schedules.iter()).enumerate().map(move |(i, s)| (l, i, s)))
        .collect()
}

/// Whether the entry is due at `minute`: its own, or (with `catch-up`) a missed one.
fn due(c: &Cron, tz: &cron::TimeZone, s: &Schedule, seed: &str, minute: i64, last: Option<i64>) -> bool {
    let shift = s.jitter.as_deref().and_then(cron::seconds).map_or(0, |max| cron::jitter(seed, max) / 60 * 60);
    let at = minute - i64::try_from(shift).unwrap_or(0);
    if c.fires_at(at, tz) {
        return true;
    }
    // Catch-up: one run when the times since the last fire include missed ones.
    match last.filter(|_| s.catch_up) {
        Some(last) if last < at - 60 => !c.fires_in(last, at - 60, tz, 1).is_empty(),
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
async fn entry(hub: &Hub, repo: &Repo, refs: &HashMap<String, String>, head: &str, head_sha: &str, l: &load::Loaded, i: usize, s: &Schedule, minute: i64, cache: &mut Cache, live: &mut Vec<(String, String)>) -> anyhow::Result<()> {
    let c = Cron::parse(&s.cron).map_err(anyhow::Error::msg)?;
    let tz = cron::zone(s.timezone.as_deref()).map_err(anyhow::Error::msg)?;
    let (repo_id, path) = (repo.id.clone(), l.path.clone());
    let last: Option<i64> = hub
        .store
        .call(move |db| Ok(db.query_row("SELECT MAX(minute) FROM ci_schedule_fires WHERE repo_id = ?1 AND path = ?2 AND entry = ?3", params![repo_id, path, i64::try_from(i).unwrap_or(0)], |r| r.get(0)).optional()?.flatten()))
        .await?;
    if !due(&c, &tz, s, &format!("{}/{}/{i}", repo.name, l.path), minute, last) {
        return Ok(());
    }
    // Once per minute and entry, whatever restarts or races.
    let (repo_id, path, cron_text) = (repo.id.clone(), l.path.clone(), s.cron.clone());
    let fresh = hub
        .store
        .call(move |db| Ok(db.execute("INSERT OR IGNORE INTO ci_schedule_fires (repo_id, path, entry, minute, cron) VALUES (?1, ?2, ?3, ?4, ?5)", params![repo_id, path, i64::try_from(i).unwrap_or(0), minute, cron_text])? == 1))
        .await?;
    if !fresh {
        return Ok(());
    }
    // `branch:` runs another branch's head, with that commit's version of the workflow.
    let (git_ref, sha, loaded) = match &s.branch {
        Some(b) => {
            let r = format!("refs/heads/{b}");
            let Some(sha) = refs.get(&r).cloned() else { anyhow::bail!("no branch `{b}`") };
            live.push((repo.id.clone(), sha.clone()));
            let commit = cache.get(hub, repo, &sha).await?;
            let Some(l2) = commit.workflows.iter().find(|x| x.path == l.path).cloned() else { anyhow::bail!("`{b}` has no {}", l.path) };
            (r, sha, l2)
        }
        None => (head.to_owned(), head_sha.to_owned(), l.clone()),
    };
    if s.skip_if_unchanged && last_success_sha(hub, repo, &l.path).await?.as_deref() == Some(sha.as_str()) {
        tracing::info!(repo = %repo.name, workflow = %l.path, cron = %s.cron, "scheduled run skipped: nothing changed since the last successful one");
        return Ok(());
    }
    let f = events::facts(hub, repo).await;
    let (message, _) = crate::gitops::commit_info(hub, repo, &sha).await;
    let actor = last_author(hub, &f.mirror, &sha, &l.path).await;
    let mut payload = context::schedule(&s.cron, &f.repo);
    if !s.inputs.is_empty()
        && let Some(o) = payload.as_object_mut()
    {
        o.insert("inputs".into(), serde_json::Value::Object(s.inputs.iter().map(|(k, v)| (k.clone(), yval_json(v))).collect()));
    }
    let e = Event { name: "schedule".into(), action: s.cron.clone(), repo: repo.name.clone(), git_ref, sha, actor, trusted: true, source: Source::Schedule, payload, message: Some(message.clone()), ..Event::default() };
    events::start(hub, repo, &e, &loaded, &message, &e.actor, serde_json::Value::Null).await?;
    Ok(())
}

fn yval_json(y: &YVal) -> serde_json::Value {
    match y {
        YVal::Null => serde_json::Value::Null,
        YVal::Bool(b) => serde_json::Value::Bool(*b),
        YVal::Num(n) => serde_json::Number::from_f64(*n).map_or(serde_json::Value::Null, serde_json::Value::Number),
        YVal::Str(t) => serde_json::Value::String(t.src.clone()),
        YVal::Seq(s) => serde_json::Value::Array(s.iter().map(yval_json).collect()),
        YVal::Map(m) => serde_json::Value::Object(m.iter().map(|(k, v)| (k.clone(), yval_json(v))).collect()),
    }
}

/// The commit of the workflow's last successful scheduled run.
async fn last_success_sha(hub: &Hub, repo: &Repo, path: &str) -> anyhow::Result<Option<String>> {
    let (repo_id, path) = (repo.id.clone(), path.to_owned());
    hub.store
        .call(move |c| {
            Ok(c.query_row("SELECT sha FROM runs WHERE repo_id = ?1 AND workflow_id = ?2 AND event = 'schedule' AND conclusion = 'success' ORDER BY rowid DESC LIMIT 1", params![repo_id, path], |r| r.get(0))
                .optional()?)
        })
        .await
}

/// Who last changed the workflow: `github.actor` for scheduled runs, as on GitHub.
async fn last_author(hub: &Hub, mirror: &str, sha: &str, path: &str) -> String {
    crate::gitops::run_git(hub, &["-C", mirror, "log", "-1", "--format=%an", sha, "--", path], crate::gitops::LS_REMOTE_TIMEOUT)
        .await
        .map(|o| String::from_utf8_lossy(&o).trim().to_owned())
        .ok()
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| "kiln".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sched(cron: &str) -> Schedule {
        Schedule { cron: cron.into(), ..Schedule::default() }
    }

    #[test]
    fn w03_r10_due_now_with_jitter_and_catch_up() -> Result<(), String> {
        let c = Cron::parse("0 6 * * *")?;
        let tz = cron::zone(None)?;
        // 2026-10-17T06:00:00Z.
        let six = 1_792_216_800;
        assert!(due(&c, &tz, &sched("0 6 * * *"), "s", six, None));
        assert!(!due(&c, &tz, &sched("0 6 * * *"), "s", six + 60, None));
        // A jittered entry fires the same few minutes later, every day.
        let j = Schedule { jitter: Some("10m".into()), ..sched("0 6 * * *") };
        let delay = i64::try_from(cron::jitter("s", 600) / 60 * 60).map_err(|e| e.to_string())?;
        assert!(due(&c, &tz, &j, "s", six + delay, None));
        assert!(delay == 0 || !due(&c, &tz, &j, "s", six, None));
        // After three hours down across 06:00, catch-up fires once at the next minute; without it,
        // the missed time is dropped, as on GitHub.
        let up = six + 2 * 3600;
        let last = Some(six - 24 * 3600);
        assert!(!due(&c, &tz, &sched("0 6 * * *"), "s", up, last));
        let catch = Schedule { catch_up: true, ..sched("0 6 * * *") };
        assert!(due(&c, &tz, &catch, "s", up, last));
        // Nothing was missed since the last fire: no catch-up.
        assert!(!due(&c, &tz, &catch, "s", up, Some(six)));
        Ok(())
    }

    /// W03-R10, found by W28 step 3's replay: a `.kiln/` workflow's schedule is the engine's only
    /// once `[ci] kiln_workflows` takes the kiln dialect off v1, whose scheduler fires it before
    /// that; GitHub's schedules are the engine's either way.
    #[test]
    fn w03_r10_a_kiln_schedule_is_v1s_until_kiln_workflows() -> Result<(), String> {
        use kiln_ci::expr::Dialect;
        let read = |path: &str, text: &str, dialect: Dialect| {
            let (workflow, _) = kiln_ci::wf::parse_workflow(path, text, dialect);
            load::Loaded { path: path.into(), dialect, runs: true, workflow, diagnostics: Vec::new() }
        };
        let commit = load::Commit {
            workflows: vec![
                read(".kiln/wire-lab.yaml", "name: wire-lab\non:\n  schedule: [\"17 3 * * *\"]\n  manual: true\njobs:\n  seeds:\n    runs-on: [baremetal, linux]\n    steps:\n      - run: echo seeds\n", Dialect::Kiln),
                read(".github/workflows/nightly.yml", "on:\n  schedule:\n    - cron: '0 4 * * *'\n    - cron: '0 5 * * *'\njobs:\n  n:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo n\n", Dialect::Gha),
            ],
            ..load::Commit::default()
        };
        if let Some(l) = commit.workflows.iter().find(|l| l.has_errors()) {
            return Err(format!("{} didn't read", l.path));
        }
        let fired = |kiln_workflows: bool| entries(&commit, kiln_workflows).into_iter().map(|(l, i, s)| (l.path.clone(), i, s.cron.clone())).collect::<Vec<_>>();
        let nightly = |i: usize, cron: &str| (".github/workflows/nightly.yml".to_owned(), i, cron.to_owned());
        assert_eq!(fired(false), vec![nightly(0, "0 4 * * *"), nightly(1, "0 5 * * *")], "the kiln dialect's stays v1's");
        assert_eq!(fired(true), vec![(".kiln/wire-lab.yaml".to_owned(), 0, "17 3 * * *".to_owned()), nightly(0, "0 4 * * *"), nightly(1, "0 5 * * *")]);
        Ok(())
    }
}
