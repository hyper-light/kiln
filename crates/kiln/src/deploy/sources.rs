//! Fleet changes in apps' source repositories (docs/deploy-api.md §3.3): one change, a kiln-base
//! version or values set, made in each selected app's own chart and proposed as one pull request
//! per repository (a merge request on GitLab); on kiln's own git server, which has none, as a
//! branch and its diff, which kiln merges when asked. Followed together: the fleet change says
//! how each stands, and is done once none is open.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::flow::{Fail, FleetAsk, Who, audit, author, bad, conflict, internal, missing};
use super::host::{Change, CommitError, Committed, Merged};
use super::model::{App, BITBUCKET_HOST, GITHUB_HOST, GITLAB_HOST, KILN_HOST, RepoRef};
use super::{Deploy, remote, store};
use crate::githost::{ChangeState, GitHost};

/// What a fleet change makes in each app's source repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceChange {
    /// kiln-base at this version in each app's chart (its `Chart.yaml`'s dependency).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Values set in each app's values file, by path (`image.pullPolicy`), as a fleet's `set`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Where each app's chart is in its repository, `{app}` its name: `charts/{app}` by default,
    /// as `kiln app init` lays it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chart: Option<String>,
    /// Its values file: `<chart>/values.yaml` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<String>,
}

impl SourceChange {
    fn chart_of(&self, app: &str) -> String {
        self.chart.as_deref().unwrap_or("charts/{app}").replace("{app}", app).trim_matches('/').to_owned()
    }

    fn values_of(&self, app: &str) -> String {
        match &self.values {
            Some(v) => v.replace("{app}", app).trim_matches('/').to_owned(),
            None => format!("{}/values.yaml", self.chart_of(app)),
        }
    }

    /// What it is, in a line: `kiln-base 0.1.4`, `replicaCount=2, image.pullPolicy=Always`.
    fn what(&self) -> String {
        match &self.base {
            Some(v) => format!("kiln-base {v}"),
            None => self.set.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", "),
        }
    }
}

/// One source repository's part of a fleet change, as kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub fleet: String,
    /// kiln's name for the repository (the apps' `source`).
    pub repo: String,
    /// Where it is: `kiln`, `github`, `gitlab` or `bitbucket`, and its name there.
    pub host: String,
    pub name: String,
    /// The branch the change goes to, and the one it's proposed on.
    pub into: String,
    pub branch: String,
    /// The apps it changes, and the files.
    pub apps: Vec<String>,
    pub files: Vec<String>,
    /// The pull request (another host's), its number and page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// On kiln's own, the change's commit and the one it's made on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// `open`, `merged`, `closed`, `unchanged` (it holds the change already) or `failed`.
    pub state: String,
    /// Why it failed, or what else it says (an app of it that can't take the change).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub by: String,
    pub at: i64,
}

impl Proposal {
    fn open(&self) -> bool {
        self.state == "open"
    }
}

/// The files `s` changes in `app`'s chart, from the files as the repository has them (`have`, by
/// path; `None`: not there). Only files that change.
pub fn changes_for(app: &str, s: &SourceChange, have: &BTreeMap<String, Option<String>>) -> Result<Vec<Change>, String> {
    let file = |path: &str| have.get(path).cloned().flatten().ok_or_else(|| format!("{app}: there's no {path} (name the chart's path: `chart`)"));
    let mut out = Vec::new();
    if let Some(version) = &s.base {
        let at = format!("{}/Chart.yaml", s.chart_of(app));
        let text = file(&at)?;
        let (moved, vendored) = super::init::with_base(&text, version).ok_or_else(|| format!("{app}: {at} doesn't depend on kiln-base"))?;
        if moved != text {
            out.push(Change { path: at, content: Some(moved.into_bytes()) });
        }
        // Vendored: its files come with it, which this kiln has at the version it ships alone.
        if vendored {
            let ships = super::init::base_version();
            if version != ships {
                return Err(format!("{app}: its chart vendors kiln-base, and this kiln vendors only the version it ships ({ships}): depend on kiln's registry to move to others (kiln app init --base)"));
            }
            for (path, text) in super::init::vendored_in(Path::new(&s.chart_of(app))) {
                let path = path.to_string_lossy().into_owned();
                if have.get(&path).cloned().flatten().as_deref() != Some(text.as_str()) {
                    out.push(Change { path, content: Some(text.into_bytes()) });
                }
            }
        }
    }
    if !s.set.is_empty() {
        let at = s.values_of(app);
        let text = file(&at)?;
        let merged = super::flow::merge_values(&text, &s.set).map_err(|e| format!("{app}: {at}: {e}"))?;
        if merged != text {
            out.push(Change { path: at, content: Some(merged.into_bytes()) });
        }
    }
    Ok(out)
}

/// The paths [`changes_for`] reads of `app`.
fn reads(app: &str, s: &SourceChange) -> Vec<String> {
    let mut out = Vec::new();
    if s.base.is_some() {
        out.push(format!("{}/Chart.yaml", s.chart_of(app)));
        out.extend(super::init::vendored_in(Path::new(&s.chart_of(app))).into_iter().map(|(p, _)| p.to_string_lossy().into_owned()));
    }
    if !s.set.is_empty() {
        out.push(s.values_of(app));
    }
    out
}

/// A source change is an app's maintenance: `environments.manage` on the server.
fn may_maintain(who: &Who) -> Result<(), Fail> {
    match &who.caller {
        Some(c) if !c.can("environments.manage", &crate::authz::Scope::Server) => {
            Err((StatusCode::FORBIDDEN, format!("{} may not change apps' sources: that needs environments.manage", who.name)))
        }
        _ => Ok(()),
    }
}

/// A fleet change's id as its branch takes it: the idempotency key, if it is one a branch name can
/// hold, or a new one.
fn fleet_id(f: &FleetAsk) -> Result<String, Fail> {
    match f.idempotency_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        Some(k) if k.len() <= 64 && k.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) && !k.starts_with('.') && !k.ends_with(".lock") => Ok(k.to_owned()),
        Some(k) => Err(bad(format!("{k:?}: a source change's key names its branch: at most 64 letters, digits, `-`, `_` and `.`"))),
        None => Ok(crate::util::new_id()),
    }
}

/// Proposes `s` in the source repository of each app `f` selects: one proposal per repository,
/// kept, and how they stand. Asked again with the same key, the same proposals.
pub async fn propose(d: &Deploy, who: &Who, f: &FleetAsk, s: &SourceChange) -> Result<Value, Fail> {
    if s.base.is_some() == !s.set.is_empty() {
        return Err(bad("a change in apps' sources is a kiln-base version (`base`) or values to `set`: one of them"));
    }
    if let Some(v) = &s.base
        && (v.is_empty() || !v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')))
    {
        return Err(bad(format!("{v:?} isn't a chart version")));
    }
    may_maintain(who)?;
    let id = fleet_id(f)?;
    if !store::proposals(&*d.records, &id).await.map_err(internal)?.is_empty() {
        return status(d, &id).await?.ok_or_else(|| missing(format!("no fleet change {id}")));
    }
    let apps = super::flow::selected(d, f).await?;
    let mut by_repo: BTreeMap<String, Vec<App>> = BTreeMap::new();
    let mut skipped = Vec::new();
    for a in apps {
        match a.source.clone() {
            Some(src) => by_repo.entry(src).or_default().push(a),
            None => skipped.push(json!({ "app": a.name, "error": "it has no source repository" })),
        }
    }
    if by_repo.is_empty() {
        return Err(bad("none of the apps selected has a source repository"));
    }
    let rows = d.hub.store.repos().await.map_err(internal)?;
    for (repo, apps) in &by_repo {
        let row = rows.iter().find(|r| r.name.eq_ignore_ascii_case(repo));
        let p = propose_in(d, who, &id, repo, row, apps, s).await;
        store::put_proposal(&*d.records, &p).await.map_err(internal)?;
    }
    audit(d, &who.name, "deploy.fleet", format!("{id}: {} in the sources of {}", s.what(), by_repo.values().flatten().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", "))).await;
    let mut v = status(d, &id).await?.ok_or_else(|| missing(format!("no fleet change {id}")))?;
    if let (false, Some(m)) = (skipped.is_empty(), v.as_object_mut()) {
        m.insert("skipped".into(), Value::Array(skipped));
    }
    Ok(v)
}

/// The proposal in one repository: what the apps of it change, committed on a branch of the
/// change and proposed from it; what kept it from being made, kept as `failed`.
async fn propose_in(d: &Deploy, who: &Who, id: &str, repo: &str, row: Option<&crate::store::Repo>, apps: &[App], s: &SourceChange) -> Proposal {
    let mut p = Proposal {
        fleet: id.to_owned(),
        repo: repo.to_owned(),
        host: String::new(),
        name: repo.to_owned(),
        into: row.and_then(|r| r.default_branch.clone()).unwrap_or_else(|| "main".to_owned()),
        branch: format!("kiln/fleet/{id}"),
        apps: apps.iter().map(|a| a.name.clone()).collect(),
        files: Vec::new(),
        number: None,
        url: None,
        commit: None,
        parent: None,
        state: "failed".to_owned(),
        note: None,
        by: who.name.clone(),
        at: crate::util::now_unix(),
    };
    if let Err(why) = make(d, who, &mut p, row, apps, s).await {
        p.state = "failed".to_owned();
        p.note = Some(why);
    }
    p
}

/// Where a repository kiln has is: kiln's own, or the host whose credentials cover it, with its
/// name there.
async fn where_is(d: &Deploy, row: &crate::store::Repo) -> Result<(&'static str, String), String> {
    match crate::githost::for_repo(&d.hub, row).await {
        Some(GitHost::Kiln) => Ok((KILN_HOST, row.name.clone())),
        Some(GitHost::Github(app)) => app.name_of(row).map(|n| (GITHUB_HOST, n)).ok_or_else(|| format!("{} isn't on kiln's GitHub", row.name)),
        Some(GitHost::Gitlab(g)) => g.name_of(row).map(|n| (GITLAB_HOST, n)).ok_or_else(|| format!("{} isn't on a GitLab of kiln's", row.name)),
        Some(GitHost::Bitbucket(b)) => b.name_of(row).map(|n| (BITBUCKET_HOST, n)).ok_or_else(|| format!("{} isn't on kiln's Bitbucket", row.name)),
        None => Err(format!("{} is on a host kiln has no credentials for ({})", row.name, row.clone_url)),
    }
}

async fn make(d: &Deploy, who: &Who, p: &mut Proposal, row: Option<&crate::store::Repo>, apps: &[App], s: &SourceChange) -> Result<(), String> {
    let row = row.ok_or_else(|| format!("kiln has no repository {}", p.repo))?;
    let (host, name) = where_is(d, row).await?;
    (p.host, p.name) = (host.to_owned(), name);
    let e = |e: anyhow::Error| format!("{e:#}");
    // What each app changes, from the files at the branch's head.
    let (kiln, there) = if host == KILN_HOST { (Some(d.hosted.clone()), None) } else { (None, Some(remote::open(d, &RepoRef { host: host.to_owned(), repo: p.name.clone(), branch: p.into.clone(), root: String::new() }).await?)) };
    let head = match (&kiln, &there) {
        (Some(h), _) => {
            let (h, r, b) = (h.clone(), p.name.clone(), p.into.clone());
            crate::rt::blocking(move || h.head(&r, &b)).await.map_err(e)?.map_err(e)?
        }
        (_, Some(g)) => g.head(Duration::ZERO).await.map_err(e)?,
        _ => None,
    }
    .ok_or_else(|| format!("{} has no {} yet", p.repo, p.into))?;
    let mut changes = Vec::new();
    let mut notes = Vec::new();
    for a in apps {
        let mut have = BTreeMap::new();
        for path in reads(&a.name, s) {
            let text = match (&kiln, &there) {
                (Some(h), _) => {
                    let (h, r, c, at) = (h.clone(), p.name.clone(), head.clone(), path.clone());
                    crate::rt::blocking(move || h.read(&r, &c, &at)).await.map_err(e)?.map_err(e)?
                }
                (_, Some(g)) => g.read(&head, &path).await.map_err(e)?,
                _ => None,
            };
            have.insert(path, text.map(|b| String::from_utf8_lossy(&b).into_owned()));
        }
        match changes_for(&a.name, s, &have) {
            Ok(c) => changes.extend(c),
            Err(why) => notes.push(why),
        }
    }
    p.files = changes.iter().map(|c| c.path.clone()).collect();
    p.note = (!notes.is_empty()).then(|| notes.join("; "));
    if changes.is_empty() {
        p.state = if notes.is_empty() { "unchanged" } else { "failed" }.to_owned();
        return Ok(());
    }
    let message = format!("{} for {}\n\nkiln's fleet change {}, asked for by {}.\n\nKiln-Fleet: {}\n", s.what(), p.apps.join(", "), p.fleet, who.name, p.fleet);
    match (&kiln, &there) {
        (Some(h), _) => {
            let (h, r, b, parent, m, by) = (h.clone(), p.name.clone(), p.branch.clone(), head.clone(), message, author(&who.name));
            let made = crate::rt::blocking(move || h.branch_from(&r, &b, &parent, &changes, &m, &by)).await.map_err(e)?.map_err(e)?;
            if let Committed::New(sha) = made {
                p.commit = Some(sha);
            }
            p.parent = Some(head);
        }
        (_, Some(g)) => {
            let body = format!(
                "kiln's fleet change `{}`, asked for by {}: {} in {}.\n\nMerging this pull request makes the change in the apps' source; closing it leaves them as they are. `kiln deploy fleet --show {}` follows it with the fleet change's others.\n",
                p.fleet,
                who.name,
                s.what(),
                p.apps.join(", "),
                p.fleet
            );
            let made = g.propose(&p.branch, changes, message, author(&who.name), &body).await.map_err(e)?;
            (p.number, p.url) = (Some(made.number), Some(made.url));
        }
        _ => {}
    }
    p.state = "open".to_owned();
    Ok(())
}

/// A fleet change in sources as it stands (`None`: it has no proposals): each repository's,
/// looked at again while open (a pull request merged or closed on its host; kiln's own branch
/// merged into its branch, or gone), with the diff of kiln's own still open.
pub async fn status(d: &Deploy, id: &str) -> Result<Option<Value>, Fail> {
    let mut list = store::proposals(&*d.records, id).await.map_err(internal)?;
    if list.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::new();
    for p in &mut list {
        if p.open()
            && let Some(state) = look(d, p).await
            && state != p.state
        {
            p.state = state;
            store::put_proposal(&*d.records, p).await.map_err(internal)?;
        }
        let mut v = serde_json::to_value(&*p).map_err(|e| internal(e.into()))?;
        if p.open()
            && p.host == KILN_HOST
            && let (Some(parent), Some(commit), Some(m)) = (p.parent.clone(), p.commit.clone(), v.as_object_mut())
        {
            let (h, r) = (d.hosted.clone(), p.name.clone());
            let diff = crate::rt::blocking(move || h.diff(&r, &parent, &commit)).await.map_err(internal)?.map_err(internal)?;
            m.insert("diff".into(), Value::String(cut(diff, 64 << 10)));
        }
        out.push(v);
    }
    let mut states: BTreeMap<&str, u32> = BTreeMap::new();
    for p in &list {
        *states.entry(p.state.as_str()).or_default() += 1;
    }
    Ok(Some(json!({ "fleet": id, "source": true, "done": !list.iter().any(Proposal::open), "states": states, "proposals": out })))
}

/// At most `max` bytes of `text`, cut at a line.
fn cut(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut at = max;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    let keep = text.get(..at).and_then(|t| t.rfind('\n')).unwrap_or(at);
    format!("{}\n… ({} bytes in all)\n", text.get(..keep).unwrap_or_default(), text.len())
}

/// Where an open proposal is now: `merged`, `closed` or `open` (`None`: it can't be looked at).
async fn look(d: &Deploy, p: &Proposal) -> Option<String> {
    if p.host == KILN_HOST {
        let (h, r, into, branch, commit) = (d.hosted.clone(), p.name.clone(), p.into.clone(), p.branch.clone(), p.commit.clone()?);
        return crate::rt::blocking(move || -> anyhow::Result<String> {
            Ok(if h.contains(&r, &into, &commit)? {
                "merged"
            } else if h.head(&r, &branch)?.is_none() {
                "closed"
            } else {
                "open"
            }
            .to_owned())
        })
        .await
        .ok()?
        .ok();
    }
    let g = remote::open(d, &RepoRef { host: p.host.clone(), repo: p.name.clone(), branch: p.into.clone(), root: String::new() }).await.ok()?;
    match g.proposal(p.number?).await {
        Ok(ChangeState::Open) => Some("open".to_owned()),
        Ok(ChangeState::Merged { .. }) => {
            g.drop_branch(&p.branch).await;
            Some("merged".to_owned())
        }
        Ok(ChangeState::Closed) => {
            g.drop_branch(&p.branch).await;
            Some("closed".to_owned())
        }
        Err(e) => {
            tracing::debug!(fleet = %p.fleet, repo = %p.repo, error = %format!("{e:#}"), "deploy: a fleet change's pull request can't be looked at");
            None
        }
    }
}

/// Merges a fleet change's proposal on kiln's own git server (`repo`: one of them; none: each
/// one open): kiln's own has no pull requests, so kiln merges the branch when asked, as git would.
/// Another host's are merged there.
pub async fn merge(d: &Deploy, who: &Who, id: &str, repo: Option<&str>) -> Result<Value, Fail> {
    may_maintain(who)?;
    let list = store::proposals(&*d.records, id).await.map_err(internal)?;
    if list.is_empty() {
        return Err(missing(format!("no fleet change {id} in apps' sources")));
    }
    let picked: Vec<Proposal> = list.into_iter().filter(|p| repo.is_none_or(|r| p.repo.eq_ignore_ascii_case(r))).collect();
    if picked.is_empty() {
        return Err(missing(format!("fleet change {id} proposes nothing in {}", repo.unwrap_or_default())));
    }
    for mut p in picked {
        if !p.open() {
            continue;
        }
        if p.host != KILN_HOST {
            if repo.is_some() {
                return Err(bad(format!("{} is on {}: merge its pull request there ({})", p.repo, remote::host_label(&p.host), p.url.as_deref().unwrap_or_default())));
            }
            continue;
        }
        let commit = p.commit.clone().ok_or_else(|| internal(anyhow::anyhow!("{}'s proposal has no commit", p.repo)))?;
        let message = format!("Merge kiln's fleet change {} into {}\n\nMerged by {} through kiln.\n\nKiln-Fleet: {}\n", p.fleet, p.into, who.name, p.fleet);
        let (h, r, into, by) = (d.hosted.clone(), p.name.clone(), p.into.clone(), author(&who.name));
        let merged = crate::rt::blocking(move || h.merge_into(&r, &into, &commit, &message, &by)).await.map_err(internal)?;
        match merged {
            Ok(Merged::Conflict(files)) => return Err(conflict(format!("{}'s change conflicts with its {} in {files}: merge {} by hand", p.repo, p.into, p.branch))),
            Ok(_) => {}
            Err(CommitError::Moved) => return Err(conflict(format!("{}'s {} moved while merging: ask again", p.repo, p.into))),
            Err(CommitError::Other(e)) => return Err(internal(e)),
        }
        let (h, r, b) = (d.hosted.clone(), p.name.clone(), p.branch.clone());
        crate::rt::blocking(move || h.delete_branch(&r, &b)).await.map_err(internal)?.map_err(internal)?;
        p.state = "merged".to_owned();
        store::put_proposal(&*d.records, &p).await.map_err(internal)?;
        audit(d, &who.name, "deploy.fleet.merge", format!("{id} into {}'s {}", p.repo, p.into)).await;
        // What a push of it starts (its CI) starts.
        crate::gitops::want_named(&d.hub, &p.repo).await;
    }
    status(d, id).await?.ok_or_else(|| missing(format!("no fleet change {id}")))
}

/// Withdraws a fleet change's open proposals: each pull request closed, saying so, and each
/// branch of kiln's own deleted.
pub async fn withdraw(d: &Deploy, who: &Who, id: &str) -> Result<Value, Fail> {
    may_maintain(who)?;
    let list = store::proposals(&*d.records, id).await.map_err(internal)?;
    if list.is_empty() {
        return Err(missing(format!("no fleet change {id} in apps' sources")));
    }
    for mut p in list.into_iter().filter(Proposal::open) {
        if p.host == KILN_HOST {
            let (h, r, b) = (d.hosted.clone(), p.name.clone(), p.branch.clone());
            crate::rt::blocking(move || h.delete_branch(&r, &b)).await.map_err(internal)?.map_err(internal)?;
        } else if let Some(n) = p.number {
            let g = remote::open(d, &RepoRef { host: p.host.clone(), repo: p.name.clone(), branch: p.into.clone(), root: String::new() }).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
            g.withdraw(n, &p.branch, &format!("kiln's fleet change {id} was withdrawn by {}: this {} goes with it.", who.name, remote::change_name(&p.host, n))).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
        }
        p.state = "closed".to_owned();
        store::put_proposal(&*d.records, &p).await.map_err(internal)?;
    }
    audit(d, &who.name, "deploy.fleet.withdraw", id.to_owned()).await;
    status(d, id).await?.ok_or_else(|| missing(format!("no fleet change {id}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn have(files: &[(&str, &str)]) -> BTreeMap<String, Option<String>> {
        files.iter().map(|(p, t)| ((*p).to_owned(), Some((*t).to_owned()))).collect()
    }

    #[test]
    fn a_source_change_moves_kiln_base_or_sets_values_in_each_chart() -> Result<(), String> {
        let chart = "apiVersion: v2\nname: web\nversion: 0.1.0\ndependencies:\n  # kiln's base chart.\n  - name: kiln-base\n    version: 0.1.2\n    repository: oci://kiln.example.com/kiln/charts\n";
        let base = SourceChange { base: Some("0.1.4".into()), ..SourceChange::default() };
        let c = changes_for("web", &base, &have(&[("charts/web/Chart.yaml", chart)]))?;
        let text = c.first().and_then(|c| c.content.clone()).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        assert_eq!(text, chart.replace("version: 0.1.2", "version: 0.1.4"), "only its version line changes");
        assert!(changes_for("web", &base, &have(&[("charts/web/Chart.yaml", &chart.replace("0.1.2", "0.1.4"))]))?.is_empty(), "there already");
        assert!(changes_for("web", &base, &have(&[])).is_err_and(|e| e.contains("no charts/web/Chart.yaml")));
        assert!(changes_for("web", &base, &have(&[("charts/web/Chart.yaml", "apiVersion: v2\nname: web\n")])).is_err_and(|e| e.contains("doesn't depend on kiln-base")));
        // Vendored: only to the version this kiln ships, its files with it.
        let vendored = "apiVersion: v2\nname: web\ndependencies:\n  - name: kiln-base\n    version: 0.0.9\n";
        assert!(changes_for("web", &base, &have(&[("charts/web/Chart.yaml", vendored)])).is_err_and(|e| e.contains("vendors")));
        let ships = SourceChange { base: Some(super::super::init::base_version().into()), ..SourceChange::default() };
        let c = changes_for("web", &ships, &have(&[("charts/web/Chart.yaml", vendored)]))?;
        assert!(c.iter().any(|c| c.path == "charts/web/charts/kiln-base/Chart.yaml") && c.iter().any(|c| c.path == "charts/web/Chart.yaml"), "{:?}", c.iter().map(|c| &c.path).collect::<Vec<_>>());
        // Values, in the chart's values file, or one named.
        let set = SourceChange { set: [("replicaCount".to_owned(), "2".to_owned())].into(), chart: Some("deploy/{app}".into()), ..SourceChange::default() };
        let c = changes_for("api", &set, &have(&[("deploy/api/values.yaml", "replicaCount: 1\nimage:\n  tag: v1\n")]))?;
        assert_eq!(c.first().map(|c| c.path.as_str()), Some("deploy/api/values.yaml"));
        assert!(c.first().and_then(|c| c.content.as_ref()).is_some_and(|b| String::from_utf8_lossy(b).contains("replicaCount: 2")));
        assert_eq!(reads("api", &set), ["deploy/api/values.yaml"]);
        Ok(())
    }

    #[test]
    fn a_fleet_s_key_names_its_branch() {
        let ask = |k: &str| FleetAsk { idempotency_key: Some(k.into()), ..FleetAsk::default() };
        assert_eq!(fleet_id(&ask("base-0.1.4")).ok().as_deref(), Some("base-0.1.4"));
        assert!(fleet_id(&ask("../main")).is_err() && fleet_id(&ask("a b")).is_err() && fleet_id(&ask("x.lock")).is_err());
        assert!(fleet_id(&FleetAsk::default()).is_ok_and(|id| !id.is_empty()));
    }
}
