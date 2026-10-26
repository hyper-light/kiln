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

use super::Cd;
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
        let (dir, auth, u) = (self.mirror_dir(url), self.git_auth(url), url.to_owned());
        let got = crate::rt::blocking(move || kiln_cd::git::Mirror::open(&dir, &u, auth)?.tags()).await.ok().and_then(Result::ok).unwrap_or_default();
        let got = Arc::new(got);
        let mut tags = self.live.tags.lock();
        if tags.len() > 256 {
            tags.clear();
        }
        tags.insert(url.to_owned(), (Instant::now(), got.clone()));
        got
    }

    /// The commit a tag names in one of an app's git sources.
    async fn tag_commit(self: &Arc<Self>, def: &AppDef, name: &str) -> Option<String> {
        for s in def.sources.iter().filter(|s| s.chart.is_none()) {
            if let Some((_, c)) = self.tags(&s.repo).await.iter().find(|(n, _)| n == name) {
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

/// The deployment a rollback of `id` goes to: `to` by its id, `revision` by what it ran (a commit,
/// a revision as recorded, or a tag of one of the app's git sources), else the one before what
/// runs. With the tag, when a tag found it.
pub(super) async fn target(cd: &Arc<Cd>, id: &str, def: &AppDef, history: &VecDeque<Deployment>, to: Option<u64>, revision: Option<&str>) -> Result<(u64, Option<String>), Fail> {
    let sources = def.sources.len();
    let ask = match (to, revision) {
        (Some(_), Some(_)) => return Err((StatusCode::BAD_REQUEST, "name the deployment (`to`) or what it ran (`revision`), not both".into())),
        (Some(n), None) => Ask::Deployment(n),
        (None, Some(r)) => Ask::Revision(r),
        (None, None) => Ask::Previous,
    };
    match rollback::pick(id, history, ask, sources) {
        Ok(d) => Ok((d.id, None)),
        // Not a commit or revision it ran: a tag, then, of the commit it ran.
        Err(Refused::Missing(why)) if revision.is_some() => {
            let name = revision.unwrap_or_default().trim();
            let Some(commit) = cd.tag_commit(def, name).await else { return Err((StatusCode::NOT_FOUND, format!("{why}; nor is {name} a tag of its sources"))) };
            match rollback::pick(id, history, Ask::Revision(&commit), sources) {
                Ok(d) => Ok((d.id, Some(name.to_owned()))),
                Err(Refused::Missing(_)) => Err((StatusCode::NOT_FOUND, format!("tag {name} is commit {}, which no deployment of {id} ran: kiln rolls back only to what it deployed", short(&commit)))),
                Err(r) => Err(refused(r)),
            }
        }
        Err(r) => Err(refused(r)),
    }
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
