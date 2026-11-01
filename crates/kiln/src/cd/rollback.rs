//! Rollbacks, inside the server (docs/cd.md, "Rollback"): what a rollback can go to, each with
//! its commit's message and the tags that name it; a commit or a tag turned into the deployment
//! that ran it; and auto-rollback as people set it, for an app or for every app of a repository
//! (`kiln_cd::rollback` decides, from the store's settings).
//!
//! Auto-rollback covers everything kiln goes back on by itself: an app after its rollout aborted
//! (`rollouts.rs`), kiln after its own restart didn't take (`guard.rs`), and a release that failed
//! after its commit (`deploy/flow.rs`).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http::StatusCode;
use kiln_cd::apps::{AppDef, Origin};
use kiln_cd::rollback::{self, Ask, Decision, Refused, Scope, Setting};
use kiln_cd::state::{Command, Deployment};
use serde_json::{Value, json};

use super::{Cd, SourceRefresh};
use super::ops::Fail;

/// How long a repository's tags are kept before they're read again.
const TAGS_FOR: Duration = Duration::from_secs(60);

/// The repository whose auto-rollback an app follows: the one whose `.kiln/deploy.yaml` made it,
/// else its first git source's, else its first source's.
pub fn repo_of(def: &AppDef) -> Option<String> {
    if let Origin::Deploy { repo } = &def.origin {
        return Some(repo.clone());
    }
    let git = def.sources.iter().find(|s| s.chart.is_none() && s.as_ref.is_none());
    git.or(def.sources.first()).map(|s| s.repo.clone()).filter(|r| !r.trim().is_empty())
}

/// An app's auto-rollback: whether kiln goes back on its own, who said so, and what each place
/// that can say it says.
#[derive(Clone, Debug)]
pub struct AutoRollback {
    pub decision: Decision,
    pub app: Option<Setting>,
    pub repo: Option<String>,
    pub repo_setting: Option<Setting>,
    /// The deploy file's `rollout: { auto-rollback }`, where it has a rollout.
    pub file: Option<bool>,
}

impl AutoRollback {
    /// Why it is on or off, in a few words: `set for this app by ada`.
    pub fn why(&self) -> String {
        let by = |s: &Option<Setting>| s.as_ref().and_then(|s| s.by.clone()).map(|b| format!(" by {b}")).unwrap_or_default();
        match self.decision.from {
            Scope::App => format!("set for this app{}", by(&self.app)),
            Scope::Repo => format!("set for {}{}", self.repo.as_deref().map(rollback::repo_name).unwrap_or_default(), by(&self.repo_setting)),
            Scope::DeployFile => "its deploy file's rollout says so".to_owned(),
            Scope::Default => "kiln's default".to_owned(),
        }
    }

    pub fn json(&self) -> Value {
        let setting = |s: &Option<Setting>| s.as_ref().map_or(Value::Null, |s| json!({"on": s.on, "by": s.by, "at_ms": s.at_ms}));
        json!({
            "on": self.decision.on,
            "from": self.decision.from.as_str(),
            "why": self.why(),
            "app": setting(&self.app),
            "repo": self.repo.as_deref().map(rollback::repo_name),
            "repo_setting": setting(&self.repo_setting),
            "deploy_file": self.file,
        })
    }
}

/// A setting kept in the store, if a person set it.
fn setting(cd: &Cd, key: &str) -> Option<Setting> {
    cd.store.read(|d| d.settings.get(key).cloned()).filter(|v| !v.is_null()).and_then(|v| serde_json::from_value(v).ok())
}

impl Cd {
    /// An app's auto-rollback, from its definition and what people set.
    pub(crate) fn auto_rollback_of(&self, id: &str, def: &AppDef) -> AutoRollback {
        let app = setting(self, &rollback::app_setting(id));
        let repo = repo_of(def);
        let repo_setting = repo.as_deref().and_then(|r| setting(self, &rollback::repo_setting(r)));
        let file = def.rollout.as_ref().map(|r| r.auto_rollback);
        let decision = rollback::decide(app.as_ref().map(|s| s.on), repo_setting.as_ref().map(|s| s.on), file);
        AutoRollback { decision, app, repo, repo_setting, file }
    }

    /// An app's auto-rollback by its id, with `file` for what its rollout was compiled with
    /// (`None`: its definition's now). An app kiln doesn't hold goes back, as it always has.
    pub(crate) fn auto_rollback(&self, id: &str, file: Option<bool>) -> AutoRollback {
        let def = self.apps.read().get(id).map(|e| e.def.clone());
        match def {
            Some(mut def) => {
                if let (Some(on), Some(r)) = (file, def.rollout.as_mut()) {
                    r.auto_rollback = on;
                }
                self.auto_rollback_of(id, &def)
            }
            None => AutoRollback { decision: rollback::decide(setting(self, &rollback::app_setting(id)).map(|s| s.on), None, file), app: None, repo: None, repo_setting: None, file },
        }
    }

    /// Sets (or, with `None`, clears) auto-rollback for an app or a repository.
    pub(crate) async fn set_auto_rollback(&self, key: String, on: Option<bool>, by: &str) -> Result<(), Fail> {
        let value = on.map_or(Value::Null, |on| json!(Setting { on, by: Some(by.to_owned()), at_ms: super::now_ms() }));
        match self.store.submit(Command::PutSetting(key, value)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err((StatusCode::BAD_REQUEST, e)),
            Err(_) => Err((StatusCode::SERVICE_UNAVAILABLE, "the deployments store stopped".into())),
        }
    }

    /// The repositories people set auto-rollback for, each with its setting.
    pub(crate) fn repo_auto_rollbacks(&self) -> Vec<(String, Setting)> {
        self.store.read(|d| {
            d.settings
                .iter()
                .filter_map(|(k, v)| Some((k.strip_prefix(rollback::REPO_SETTING)?.to_owned(), serde_json::from_value::<Setting>(v.clone()).ok()?)))
                .collect()
        })
    }

    /// A repository's tags and the commits they name, from KilnCD's mirror of it; read again
    /// once a minute. None where the mirror can't be read.
    pub(crate) async fn tags(self: &Arc<Self>, url: &str) -> Arc<Vec<(String, String)>> {
        if let Some((at, t)) = self.live.tags.lock().get(url)
            && at.elapsed() < TAGS_FOR
        {
            return t.clone();
        }
        let source = self.read_context(url);
        let got = crate::rt::blocking_in(crate::rt::SHARE_GIT_MIRROR_IO, move || source.open()?.tags()).await.ok().and_then(Result::ok).unwrap_or_default();
        let got = Arc::new(got);
        let mut tags = self.live.tags.lock();
        if tags.len() > 256 {
            tags.clear();
        }
        tags.insert(url.to_owned(), (Instant::now(), got.clone()));
        got
    }

    /// The source of `def` whose mirror has commit `rev` (7 or more of its first hex digits, or a
    /// whole one), and that commit whole: one a rollback can go back to though no deployment ran
    /// it (docs/ci.md W26-R25). Git sources only; none where `rev` isn't a commit's.
    fn find_commit(&self, sources: &mut SourceRefresh, def: &AppDef, rev: &str) -> Option<(usize, String)> {
        let rev = rev.trim().to_ascii_lowercase();
        if !(7..=64).contains(&rev.len()) || !rev.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        for (i, s) in def.sources.iter().enumerate().filter(|(_, s)| s.chart.is_none()) {
            // U08: read through this operation's captured reader, as its tag is.
            let Ok(mirror) = sources.open_url(self, &s.repo) else { continue };
            if let Some(c) = mirror.resolve(&rev).ok().filter(|c| c.starts_with(&rev)) {
                return Some((i, c));
            }
        }
        None
    }

    /// The commit a tag names in one of an app's git sources.
    fn tag_commit(&self, sources: &mut SourceRefresh, def: &AppDef, name: &str) -> Option<String> {
        for s in def.sources.iter().filter(|s| s.chart.is_none()) {
            // U08: a mutation's tag choice belongs to this operation's captured reader.
            // The URL-only UI hint cache cannot select a deployment for another credential.
            let Ok(mirror) = sources.open_url(self, &s.repo) else { continue };
            let Ok(tags) = mirror.tags() else { continue };
            if let Some((_, c)) = tags.iter().find(|(n, _)| n == name) {
                return Some(c.clone());
            }
        }
        None
    }
}

fn refused(r: Refused) -> Fail {
    match r {
        Refused::Missing(m) => (StatusCode::NOT_FOUND, m),
        Refused::Unusable(m) => (StatusCode::BAD_REQUEST, m),
    }
}

/// Where a rollback of an app goes, as the server resolved it: a deployment of its history, or a
/// commit of one of its sources none of them ran (docs/ci.md W26-R25); with the tag, when a tag
/// named it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Going {
    Deployment { id: u64, tag: Option<String> },
    Commit { source: usize, commit: String, tag: Option<String> },
}

/// What a rollback's `revision` is in an app's sources, looked up in git before a rollback is
/// chosen: the source with that commit (and the commit whole), and the tag of that name (and the
/// commit it names, with the source that has it).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Looked {
    pub commit: Option<(usize, String)>,
    pub tag: Option<(String, Option<(usize, String)>)>,
}

/// Where a rollback of `id` goes: `to` by its id, `revision` by what it ran (a commit, a revision as
/// recorded, or a tag of one of the app's git sources), else the one before what runs. A commit (or
/// a tag's) that no deployment that can be gone back to ran is gone back to all the same, rendered
/// anew, when one of the app's git sources has it. Git is read through this operation's own
/// sources (U08), which go on to render it.
pub(super) async fn target(cd: &Arc<Cd>, sources: SourceRefresh, id: &str, def: &AppDef, history: &VecDeque<Deployment>, to: Option<u64>, revision: Option<&str>) -> Result<(SourceRefresh, Going), Fail> {
    let count = def.sources.len();
    // Git is asked only for what the history doesn't know.
    let mut sources = sources;
    let mut looked = Looked::default();
    if let Some(name) = revision.map(str::trim).filter(|r| to.is_none() && !r.is_empty())
        && rollback::pick(id, history, Ask::Revision(name), count).is_err()
    {
        let (cd2, def2, name2) = (cd.clone(), def.clone(), name.to_owned());
        let (back, found) = crate::rt::blocking_in(crate::rt::SHARE_GIT_MIRROR_IO, move || {
            let mut found = Looked { commit: cd2.find_commit(&mut sources, &def2, &name2), tag: None };
            if found.commit.is_none()
                && let Some(c) = cd2.tag_commit(&mut sources, &def2, &name2)
            {
                let commit = cd2.find_commit(&mut sources, &def2, &c).or(Some((usize::MAX, c)));
                found.tag = Some((name2, commit));
            }
            (sources, found)
        })
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("git couldn't be read: {e:#}")))?;
        sources = back;
        looked = found;
    }
    choose(id, history, count, to, revision, looked).map(|going| (sources, going))
}

/// [`target`]'s choice, from the history and what git said ([`Looked`]): pure.
pub(super) fn choose(id: &str, history: &VecDeque<Deployment>, sources: usize, to: Option<u64>, revision: Option<&str>, looked: Looked) -> Result<Going, Fail> {
    let ask = match (to, revision) {
        (Some(_), Some(_)) => return Err((StatusCode::BAD_REQUEST, "name the deployment (`to`) or what it ran (`revision`), not both".into())),
        (Some(n), None) => Ask::Deployment(n),
        (None, Some(r)) => Ask::Revision(r),
        (None, None) => Ask::Previous,
    };
    let going = |t: rollback::Target<'_>, tag: Option<String>| match t {
        rollback::Target::Deployment(d) => Going::Deployment { id: d.id, tag },
        rollback::Target::Commit { source, commit } => Going::Commit { source, commit, tag },
    };
    let why = match rollback::pick(id, history, ask, sources) {
        Ok(d) => return Ok(Going::Deployment { id: d.id, tag: None }),
        Err(r) if revision.is_none() => return Err(refused(r)),
        Err(r) => r,
    };
    let name = revision.unwrap_or_default().trim();
    // A commit of one of its sources, whole or by its first hex digits.
    if let Some(found) = looked.commit {
        return rollback::pick_any(id, history, Ask::Revision(name), sources, Some(found)).map(|t| going(t, None)).map_err(refused);
    }
    // A tag, then, of a commit: the deployment that ran it, else the commit anew.
    let Some((tag, found)) = looked.tag else {
        return Err(match why {
            Refused::Missing(m) => (StatusCode::NOT_FOUND, format!("{m}; nor is {name} a commit or a tag of its sources")),
            r => refused(r),
        });
    };
    let commit = found.as_ref().map(|(_, c)| c.clone()).unwrap_or_default();
    rollback::pick_any(id, history, Ask::Revision(&commit), sources, found.filter(|(s, _)| *s < sources)).map(|t| going(t, Some(tag.clone()))).map_err(|r| match r {
        Refused::Missing(_) => (StatusCode::NOT_FOUND, format!("tag {tag} is commit {}, which none of {id}'s sources has", short(&commit))),
        r => refused(r),
    })
}

/// How many of an app's source's newest commits a rollback offers beside its deployments.
const COMMITS: usize = 20;

/// An app's source's newest commits as a rollback offers them (docs/ci.md W26-R25): its first git
/// source of its own, at the revision it follows, newest first, each who, when and what, with the
/// deployment that ran it and the newest that rendered what it renders (by its inputs' address,
/// worked out from git without rendering). None where git can't be read.
pub(crate) async fn commits(cd: &Arc<Cd>, def: &AppDef, history: &VecDeque<Deployment>) -> Vec<Value> {
    let Some((i, src)) = def.sources.iter().enumerate().find(|(_, s)| s.chart.is_none() && s.as_ref.is_none()) else { return Vec::new() };
    // A Kubernetes cluster's version and APIs shape some renders' addresses; not connected, theirs
    // are read without them (and may match no deployment's).
    let cluster = cd.clusters.read().get(&def.destination.cluster).cloned();
    let (kube_version, api_versions) = match cluster.filter(|_| !kiln_cd::providers::is_target(&def.destination.cluster)) {
        Some(c) => (c.version.read().clone(), c.discovery.read().api_versions.clone()),
        None => (None, Vec::new()),
    };
    let (cd2, d2, url, rev, n) = (cd.clone(), def.clone(), src.repo.clone(), src.revision.clone(), def.sources.len());
    let rows = crate::rt::blocking_in(crate::rt::SHARE_GIT_MIRROR_IO, move || -> anyhow::Result<Vec<(kiln_cd::git::Commit, Option<u64>)>> {
        let log = cd2.read_context(&url).open()?.log(&rev, COMMITS)?;
        Ok(log
            .into_iter()
            .map(|c| {
                let key = cd2.inputs_of(&d2, kube_version.as_deref(), &api_versions, Some(&rollback::held_commits(n, i, &c.sha))).ok().map(|r| r.key);
                (c, key)
            })
            .collect())
    })
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or_default();
    rows.into_iter()
        .map(|(c, key)| {
            let ran = rollback::ran_by(history, &c.sha).map(|d| d.id);
            let same = key.and_then(|k| rollback::same_as(history, k)).map(|d| d.id);
            json!({"sha": c.sha, "author": c.author, "at": c.at, "subject": c.subject, "deployment": ran, "same_as": same})
        })
        .collect()
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// The repository a deployment's commits come from as people read them, and those commits: its
/// first git source's (one that only provides files, such as a chart's values, when that's all
/// it has). The revision first (git's head when it synced, which the history shows), then the
/// commit its render was made from, when they differ (a commit that left the app's files alone).
fn deployed_commits(def: &AppDef, d: &Deployment) -> Option<(String, Vec<String>)> {
    let git = |own: bool| def.sources.iter().enumerate().find(|(_, s)| s.chart.is_none() && (!own || s.as_ref.is_none()));
    let (i, src) = git(true).or_else(|| git(false))?;
    let hex = |r: &String| r.len() >= 7 && r.bytes().all(|b| b.is_ascii_hexdigit());
    let mut shas: Vec<String> = d.revision.clone().filter(hex).into_iter().collect();
    if let Some(c) = d.commits.get(i).cloned().flatten().filter(|c| !shas.contains(c)) {
        shas.push(c);
    }
    (!shas.is_empty()).then(|| (src.repo.clone(), shas))
}

/// A deployment as a rollback offers it: its record, its commit (who, when, the whole message) and
/// the tags that name what it ran.
pub(super) async fn row(cd: &Arc<Cd>, def: &AppDef, d: &Deployment) -> Value {
    let mut row = serde_json::to_value(d).unwrap_or(Value::Null);
    let Some((url, shas)) = deployed_commits(def, d) else { return row };
    let Some(sha) = shas.first() else { return row };
    let commit = cd.live.commit(cd, &url, sha).await;
    let all = cd.tags(&url).await;
    let mut tags: Vec<String> = Vec::new();
    for s in &shas {
        let full = match commit.as_ref().and_then(|c| c.get("sha")).and_then(Value::as_str) {
            Some(f) if f.starts_with(s.as_str()) => f,
            _ => s.as_str(),
        };
        for t in rollback::tags_of(&all, full) {
            if !tags.iter().any(|x| x == t) {
                tags.push(t.to_owned());
            }
        }
    }
    if let Some(m) = row.as_object_mut() {
        if let Some(c) = commit {
            m.insert("commit".into(), c);
        }
        m.insert("tags".into(), json!(tags));
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(id: u64, rev: &str, key: u64, ok: bool) -> Deployment {
        Deployment { app: "default/web".into(), id, revision: Some(rev.into()), key: Some(key), succeeded: ok, commits: vec![Some(rev.into())], ..Deployment::default() }
    }

    /// W26-R25: a rollback goes to a deployment by its id, by what it ran or by a tag of that, and
    /// else to a commit git found in one of the app's sources, or a tag's commit, rendered anew;
    /// what's neither says so.
    #[test]
    fn w26_r25_a_rollback_goes_to_a_deployment_or_any_commit_git_has() {
        let c = |n: u32| format!("{n:07}bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let h: VecDeque<Deployment> = vec![dep(1, &c(1), 10, true), dep(2, &c(2), 20, true)].into();
        let at = |to, rev: Option<&str>, looked| choose("default/web", &h, 1, to, rev, looked);
        assert_eq!(at(None, None, Looked::default()), Ok(Going::Deployment { id: 1, tag: None }), "the one before what runs");
        assert_eq!(at(Some(2), None, Looked::default()), Ok(Going::Deployment { id: 2, tag: None }));
        assert_eq!(at(None, Some(&c(1)[..8]), Looked::default()), Ok(Going::Deployment { id: 1, tag: None }), "what a deployment ran");
        // A commit none ran, which git has in source 0.
        let found = Looked { commit: Some((0, c(5))), tag: None };
        assert_eq!(at(None, Some(&c(5)[..9]), found), Ok(Going::Commit { source: 0, commit: c(5), tag: None }));
        // A tag of a commit a deployment ran, and of one none ran.
        let tagged = |commit: String| Looked { commit: None, tag: Some(("v1".into(), Some((0, commit)))) };
        assert_eq!(at(None, Some("v1"), tagged(c(1))), Ok(Going::Deployment { id: 1, tag: Some("v1".into()) }));
        assert_eq!(at(None, Some("v1"), tagged(c(6))), Ok(Going::Commit { source: 0, commit: c(6), tag: Some("v1".into()) }));
        // Neither a commit nor a tag of its sources: 404, saying so.
        let none = at(None, Some("nowhere"), Looked::default());
        assert!(matches!(&none, Err((StatusCode::NOT_FOUND, m)) if m.contains("nor is nowhere a commit or a tag")), "{none:?}");
        assert!(matches!(at(Some(1), Some("v1"), Looked::default()), Err((StatusCode::BAD_REQUEST, _))));
    }
}
