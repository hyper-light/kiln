//! Conflicts in the diff (docs/ci.md W26-R12). A change's latest revision is replayed onto where it
//! goes, its trunk's tip (a diff above another open one: that one's latest revision, as W14-R08
//! restacks), with `git merge-tree --write-tree` and git's zdiff3 markers, in kiln's own copy: no
//! branch moves. A replay that conflicts keeps each conflict where it falls (`kiln_ci::conflicts`):
//! a text file's between git's markers, with main's side, this side and the base, at this change's
//! own lines; a binary file, a file one side deleted, and anything else git can't merge by lines
//! named and resolved by taking a side, never shown as markers. Each is resolved on its own: main,
//! this, both, an agent's resolution or an edit. An agent's own resolution is kept as the agent's,
//! for a person to take (a repository's automatic resolution is W26-R17's ladder). Commit resolution
//! makes one revision of whoever commits it, through `revision::commit` (the path W27-R03 signs;
//! unsigned until then), its why `conflict resolved`: a diff's commit goes onto the replay's base,
//! its author and message kept; a change request's branch gets a merge commit, as GitHub's Resolve
//! conflicts makes one. Until then the gate (W26-R11) holds approving, landing and automatic merges
//! for each conflict. Replays are kept by revision and by what they went onto; replays as main
//! moves and the conflict ladder (W26-R16, R17) keep theirs the same way.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use http::StatusCode;
use kiln_ci::conflicts::{self, Kind, Merged, Side, Take, Text};
use kiln_ci::review::State;
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::Fail;
use super::diffs::{self, Diff, Version};
use super::revision;
use crate::authz::{Caller, Scope};
use crate::wire::events::{ConflictChange, ServerEvent};
use crate::gitops::{self, LS_REMOTE_TIMEOUT};
use crate::hub::Hub;

/// The why of the revision Commit resolution makes (W26-R04).
pub const WHY: &str = "conflict resolved";

fn fail(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn internal(m: impl Into<String>) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, m.into())
}

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, m.into())
}

fn refused(m: impl Into<String>) -> Fail {
    (StatusCode::CONFLICT, m.into())
}

/// Where a change's latest revision is replayed onto.
#[derive(Debug, Clone)]
struct Onto {
    /// In words: its trunk's name, or the diff below it and that diff's revision (`D40 v3`).
    name: String,
    commit: String,
    /// The trunk commit a revision made on it is based on.
    base: String,
    /// The open diff below it in its stack, when it goes onto one.
    below: Option<String>,
}

/// A replay as kept: a change's revision onto a commit, and how it went.
#[derive(Debug, Clone)]
pub struct Replay {
    pub id: String,
    pub version: u32,
    pub commit: String,
    pub onto: String,
    pub onto_name: String,
    pub tree: String,
    /// `clean`, `conflicted`, or `resolved` once Commit resolution made a revision of it.
    pub state: String,
    pub created_by: String,
    pub created_at: i64,
    pub resolved_version: Option<u32>,
    pub resolved_by: Option<String>,
    pub resolved_at: Option<i64>,
}

const REPLAY_COLUMNS: &str = "id, version, commit_sha, onto, onto_name, tree, state, created_by, created_at, resolved_version, resolved_by, resolved_at";

fn replay_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Replay> {
    Ok(Replay {
        id: r.get(0)?,
        version: r.get(1)?,
        commit: r.get(2)?,
        onto: r.get(3)?,
        onto_name: r.get(4)?,
        tree: r.get(5)?,
        state: r.get(6)?,
        created_by: r.get(7)?,
        created_at: r.get(8)?,
        resolved_version: r.get(9)?,
        resolved_by: r.get(10)?,
        resolved_at: r.get(11)?,
    })
}

/// A conflict as kept: where it falls, its sides, an agent's resolution, and the resolution taken.
#[derive(Debug, Clone)]
struct Kept {
    id: String,
    file: String,
    kind: Kind,
    hunk: Option<u32>,
    line: Option<u32>,
    end_line: Option<u32>,
    /// A text conflict's side: `{start, lines}`; else what that side has at the path, `{mode,
    /// blob}`, or null where it has nothing.
    main: Value,
    this: Value,
    base: Value,
    /// git's own words on it.
    said: Option<String>,
    agent_take: Option<Take>,
    agent_lines: Option<Vec<String>>,
    agent_by: Option<String>,
    agent_at: Option<i64>,
    take: Option<Take>,
    /// The lines a text conflict resolves to.
    lines: Option<Vec<String>>,
    resolved_by: Option<String>,
    resolved_at: Option<i64>,
}

const KEPT_COLUMNS: &str = "id, file, kind, hunk, line, end_line, main, this, base, said, agent_take, agent_lines, agent_by, agent_at, take, lines, resolved_by, resolved_at";

fn json_of(text: Option<String>) -> Value {
    text.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)
}

fn lines_of(text: Option<String>) -> Option<Vec<String>> {
    text.and_then(|t| serde_json::from_str(&t).ok())
}

fn kept_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Kept> {
    let take = |s: Option<String>| s.as_deref().and_then(Take::parse);
    Ok(Kept {
        id: r.get(0)?,
        file: r.get(1)?,
        kind: Kind::parse(&r.get::<_, String>(2)?).unwrap_or(Kind::Other),
        hunk: r.get(3)?,
        line: r.get(4)?,
        end_line: r.get(5)?,
        main: json_of(r.get(6)?),
        this: json_of(r.get(7)?),
        base: json_of(r.get(8)?),
        said: r.get(9)?,
        agent_take: take(r.get(10)?),
        agent_lines: lines_of(r.get(11)?),
        agent_by: r.get(12)?,
        agent_at: r.get(13)?,
        take: take(r.get(14)?),
        lines: lines_of(r.get(15)?),
        resolved_by: r.get(16)?,
        resolved_at: r.get(17)?,
    })
}

/// A text conflict's lines on one side, as kept.
fn side_lines(side: &Value) -> Vec<String> {
    side.get("lines").and_then(Value::as_array).map(|a| a.iter().filter_map(|l| l.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

impl Kept {
    /// Where the gate says it is (`expand.rs:18`), and whether it's resolved, waiting on Commit
    /// resolution.
    fn for_gate(&self) -> kiln_ci::review::Conflict {
        kiln_ci::review::Conflict { file: self.file.clone(), line: self.line, taken: self.take.is_some() }
    }

    fn place(&self) -> String {
        kiln_ci::review::place(Some(&self.file), self.line)
    }

    /// The side a conflict that isn't text takes: what it took, or the agent's choice.
    fn side(&self) -> Option<Take> {
        match self.take {
            Some(Take::Agent) => self.agent_take,
            t => t,
        }
    }

    /// As the API shows it; `of`: how many conflicts its file has.
    fn view(&self, of: usize) -> Value {
        json!({
            "id": self.id, "file": self.file, "kind": self.kind.as_str(), "hunk": self.hunk, "of": of,
            "line": self.line, "end_line": self.end_line,
            "main": self.main, "this": self.this, "base": self.base,
            "says": (!self.kind.by_lines()).then(|| self.kind.named(&self.file)),
            "git": self.said,
            "takes": Take::takes(self.kind).iter().map(|t| t.as_str()).collect::<Vec<_>>(),
            "agent": self.agent_by.as_ref().map(|by| json!({ "by": by, "take": self.agent_take.map(Take::as_str), "lines": self.agent_lines, "at": self.agent_at })),
            "resolution": self.take.map(|t| json!({ "take": t.as_str(), "lines": self.lines, "by": self.resolved_by, "at": self.resolved_at })),
        })
    }
}

/// A conflict as a replay finds it, before it's kept.
#[derive(Debug, Clone)]
struct Found {
    file: String,
    kind: Kind,
    hunk: Option<u32>,
    line: Option<u32>,
    end_line: Option<u32>,
    main: Value,
    this: Value,
    base: Value,
    said: Option<String>,
    /// What it is, whatever else moved: the same conflict in a later replay of the revision keeps
    /// what was resolved.
    fingerprint: String,
}

/// What a kept conflict carries into the next replay of its revision.
#[derive(Debug, Clone, Default)]
struct Carry {
    agent_take: Option<String>,
    agent_lines: Option<String>,
    agent_by: Option<String>,
    agent_at: Option<i64>,
    take: Option<String>,
    lines: Option<String>,
    resolved_by: Option<String>,
    resolved_at: Option<i64>,
}

/// Whether `d` may have its conflicts resolved by `caller`: it's open and not landing, and they may
/// submit to its repository (a resolution changes its code, as Apply does).
fn may_change(caller: &Caller, d: &Diff) -> Result<(), Fail> {
    if !caller.can("diffs.submit", &Scope::Repo { repo: d.repo.clone() }) {
        return Err((StatusCode::FORBIDDEN, "resolving conflicts needs `diffs.submit`".into()));
    }
    if !State::parse(&d.state).is_some_and(|s| s.is_open() && s != State::Landing) {
        return Err(refused(format!("{} is {}", d.name, d.state)));
    }
    Ok(())
}

/// Where `d`'s latest revision goes: onto the open diff below it in its stack (that diff's latest
/// revision), else onto its trunk's tip.
async fn onto_of(hub: &Hub, d: &Diff, path: &str) -> Result<Onto, Fail> {
    if d.kind == "diff" {
        let id = d.id.clone();
        let below: Option<(String, String)> = hub
            .store
            .call(move |c| Ok(c.query_row("SELECT pd.id, pd.state FROM diffs d JOIN diffs pd ON pd.id = d.parent_diff WHERE d.id = ?1", [&id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?))
            .await
            .map_err(fail)?;
        if let Some((id, _)) = below.filter(|(_, s)| State::parse(s).is_some_and(State::is_open))
            && let Some(b) = diffs::find(hub, &id).await.map_err(fail)?
            && let Some(v) = diffs::versions(hub, &b).await.map_err(fail)?.pop()
        {
            return Ok(Onto { name: format!("{} v{}", b.name, v.n), commit: v.commit, base: v.base, below: Some(id) });
        }
    }
    let spec = format!("refs/heads/{}^{{commit}}", d.trunk);
    let tip = gitops::run_git(hub, &["-C", path, "rev-parse", "--verify", "--quiet", &spec], LS_REMOTE_TIMEOUT)
        .await
        .ok()
        .map(|o| String::from_utf8_lossy(&o).trim().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| refused(format!("{} has no commits", d.trunk)))?;
    Ok(Onto { name: d.trunk.clone(), commit: tip.clone(), base: tip, below: None })
}

/// `v` replayed onto `onto` by `git merge-tree --write-tree` (zdiff3 markers, so they carry the
/// base): a diff as a cherry-pick of its commit (its parent the merge base), a change request as a
/// merge of its branch. Whether it went cleanly, and what git printed.
async fn merge_tree(hub: &Hub, path: &str, onto: &str, v: &Version, request: bool) -> Result<(bool, Merged), Fail> {
    let base = format!("--merge-base={}", v.parent);
    let mut args = vec!["-C", path, "-c", "merge.conflictStyle=zdiff3", "merge-tree", "--write-tree", "-z"];
    if !request {
        args.push(base.as_str());
    }
    args.extend([onto, v.commit.as_str()]);
    let mut cmd = gitops::git(hub);
    cmd.args(&args);
    let out = crate::proc::output(cmd, LS_REMOTE_TIMEOUT).await.map_err(fail)?;
    let said = || String::from_utf8_lossy(&out.stderr).trim().chars().take(600).collect::<String>();
    match out.status.code() {
        Some(code @ (0 | 1)) => {
            let m = conflicts::merged(&out.stdout).map_err(internal)?;
            if code == 1 && m.paths.is_empty() {
                return Err(internal(format!("git couldn't replay it and named no file: {}", said())));
            }
            Ok((code == 0, m))
        }
        _ => Err(internal(format!("git merge-tree (replays need git 2.40 or newer): {}", said()))),
    }
}

/// A blob's bytes, as git names it (`<tree>:<path>`, an id).
async fn blob(hub: &Hub, path: &str, spec: &str) -> Result<Vec<u8>, Fail> {
    gitops::run_git(hub, &["-C", path, "cat-file", "blob", spec], LS_REMOTE_TIMEOUT).await.map_err(fail)
}

/// How lines move from `text` to the blob `oid` (`git diff -U0`): where a conflict's lines in the
/// text taking a side are in that side's own file. `None` when git can't say.
async fn carried(hub: &Hub, path: &str, text: &str, oid: &str) -> Option<Vec<kiln_ci::review::Hunk>> {
    let made = diffs::git_with_input(hub, &["-C", path, "hash-object", "-w", "--stdin"], text.as_bytes().to_vec()).await.ok()?;
    let made = String::from_utf8_lossy(&made).trim().to_owned();
    if made == oid {
        return Some(Vec::new());
    }
    let d = gitops::run_git(hub, &["-C", path, "diff", "-U0", "--no-color", "--no-ext-diff", &made, oid], LS_REMOTE_TIMEOUT).await.ok()?;
    Some(kiln_ci::review::hunks(&String::from_utf8_lossy(&d)))
}

/// Each conflict a replay left, where it falls: a text file's, each between git's markers, at this
/// change's lines (and main's); any other path's, the stages git kept of it.
async fn found(hub: &Hub, path: &str, m: &Merged) -> Result<Vec<Found>, Fail> {
    let mut out = Vec::new();
    for (file, stages) in &m.paths {
        let stage = |n: u8| stages.iter().find(|s| s.stage == n);
        let said = Some(m.said(file).join("\n")).filter(|s| !s.is_empty());
        // The merged file, where both sides kept a regular file there (a submodule has no blob).
        let merged = match (stage(2), stage(3)) {
            (Some(a), Some(b)) if a.mode.starts_with("100") && b.mode.starts_with("100") => blob(hub, path, &format!("{}:{file}", m.tree)).await.unwrap_or_default(),
            _ => Vec::new(),
        };
        let kind = conflicts::kind_of(stages, m.said_binary(file) || conflicts::looks_binary(&merged));
        let text = if kind == Kind::Text { Text::parse(&String::from_utf8_lossy(&merged)).ok() } else { None };
        let Some(text) = text else {
            // Taken whole: what each side has at the path.
            let kind = if kind == Kind::Text { Kind::Other } else { kind };
            let side = |n: u8| stage(n).map_or(Value::Null, |s| json!({ "mode": s.mode, "blob": s.oid }));
            let fingerprint = crate::util::sha256_hex(json!([file, kind.as_str(), side(1), side(2), side(3)]).to_string().as_bytes());
            out.push(Found { file: file.clone(), kind, hunk: None, line: None, end_line: None, main: side(2), this: side(3), base: side(1), said, fingerprint });
            continue;
        };
        // The text taking a side differs from that side's own file only where the other side's
        // changes merged cleanly: git's diff of the two carries each conflict to its lines there.
        let (this_text, this_at) = text.with_side(Side::This);
        let (main_text, main_at) = text.with_side(Side::Main);
        let this_map = match stage(3) {
            Some(s) => carried(hub, path, &this_text, &s.oid).await,
            None => None,
        };
        let main_map = match stage(2) {
            Some(s) => carried(hub, path, &main_text, &s.oid).await,
            None => None,
        };
        for (i, ((h, (ts, tn)), (ms, mn))) in text.hunks().into_iter().zip(this_at).zip(main_at).enumerate() {
            let line = this_map.as_deref().and_then(|hs| conflicts::place(hs, ts, tn)).unwrap_or(ts);
            let main_line = main_map.as_deref().and_then(|hs| conflicts::place(hs, ms, mn)).unwrap_or(ms);
            let (main, this, base) = (conflicts::plain(&h.main), conflicts::plain(&h.this), h.base.as_deref().map(conflicts::plain));
            let fingerprint = crate::util::sha256_hex(json!([file, "text", main, this, base]).to_string().as_bytes());
            out.push(Found {
                file: file.clone(),
                kind: Kind::Text,
                hunk: Some(u32::try_from(i).unwrap_or(u32::MAX)),
                line: Some(line),
                end_line: Some(line.saturating_add(tn.max(1) - 1)),
                main: json!({ "start": main_line, "lines": main }),
                this: json!({ "start": line, "lines": this }),
                base: base.map_or(Value::Null, |b| json!({ "lines": b })),
                said: said.clone(),
                fingerprint,
            });
        }
    }
    Ok(out)
}

async fn kept_replay(hub: &Hub, diff_id: &str, version: u32, onto: &str) -> Result<Option<Replay>, Fail> {
    let (id, onto) = (diff_id.to_owned(), onto.to_owned());
    hub.store
        .call(move |c| Ok(c.query_row(&format!("SELECT {REPLAY_COLUMNS} FROM review_replays WHERE diff_id = ?1 AND version = ?2 AND onto = ?3"), params![id, version, onto], replay_row).optional()?))
        .await
        .map_err(fail)
}

/// The newest replay kept of a change.
async fn last_replay(hub: &Hub, diff_id: &str) -> Result<Option<Replay>, Fail> {
    let id = diff_id.to_owned();
    hub.store
        .call(move |c| Ok(c.query_row(&format!("SELECT {REPLAY_COLUMNS} FROM review_replays WHERE diff_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT 1"), [&id], replay_row).optional()?))
        .await
        .map_err(fail)
}

/// A replay's conflicts, in order.
async fn kept(hub: &Hub, replay: &str) -> Result<Vec<Kept>, Fail> {
    let id = replay.to_owned();
    hub.store
        .call(move |c| {
            let mut st = c.prepare(&format!("SELECT {KEPT_COLUMNS} FROM review_conflicts WHERE replay_id = ?1 ORDER BY position"))?;
            let rows = st.query_map([&id], kept_row)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(fail)
}

/// Keeps a replay of `v` onto `onto` and the conflicts it found; one kept already (made side by
/// side) stands. The same conflicts in the replay of this revision before it keep what was
/// resolved, and an agent's resolutions.
async fn record(hub: &Hub, d: &Diff, v: &Version, onto: &Onto, tree: &str, found: Vec<Found>) -> Result<Replay, Fail> {
    let (diff_id, n, commit, onto_commit, onto_name, tree) = (d.id.clone(), v.n, v.commit.clone(), onto.commit.clone(), onto.name.clone(), tree.to_owned());
    let replay = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let (id, now) = (crate::util::new_id(), crate::util::now_unix());
            let state = if found.is_empty() { "clean" } else { "conflicted" };
            let made = tx.execute(
                "INSERT OR IGNORE INTO review_replays (id, diff_id, version, commit_sha, onto, onto_name, tree, state, created_by, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'kiln', ?9)",
                params![id, diff_id, n, commit, onto_commit, onto_name, tree, state, now],
            )? > 0;
            if made {
                let mut before: HashMap<String, Carry> = HashMap::new();
                {
                    let mut st = tx.prepare(
                        "SELECT fingerprint, agent_take, agent_lines, agent_by, agent_at, take, lines, resolved_by, resolved_at FROM review_conflicts
                         WHERE replay_id = (SELECT id FROM review_replays WHERE diff_id = ?1 AND version = ?2 AND id <> ?3 AND state = 'conflicted' ORDER BY created_at DESC, rowid DESC LIMIT 1)",
                    )?;
                    let rows = st.query_map(params![diff_id, n, id], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            Carry { agent_take: r.get(1)?, agent_lines: r.get(2)?, agent_by: r.get(3)?, agent_at: r.get(4)?, take: r.get(5)?, lines: r.get(6)?, resolved_by: r.get(7)?, resolved_at: r.get(8)? },
                        ))
                    })?;
                    for row in rows {
                        let (fingerprint, carry) = row?;
                        before.entry(fingerprint).or_insert(carry);
                    }
                }
                for (i, f) in found.iter().enumerate() {
                    let c = before.remove(&f.fingerprint).unwrap_or_default();
                    tx.execute(
                        "INSERT INTO review_conflicts (id, replay_id, diff_id, position, file, kind, hunk, line, end_line, main, this, base, said, fingerprint,
                                                       agent_take, agent_lines, agent_by, agent_at, take, lines, resolved_by, resolved_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
                        params![
                            crate::util::new_id(),
                            id,
                            diff_id,
                            i64::try_from(i).unwrap_or(i64::MAX),
                            f.file,
                            f.kind.as_str(),
                            f.hunk,
                            f.line,
                            f.end_line,
                            (!f.main.is_null()).then(|| f.main.to_string()),
                            (!f.this.is_null()).then(|| f.this.to_string()),
                            (!f.base.is_null()).then(|| f.base.to_string()),
                            f.said,
                            f.fingerprint,
                            c.agent_take,
                            c.agent_lines,
                            c.agent_by,
                            c.agent_at,
                            c.take,
                            c.lines,
                            c.resolved_by,
                            c.resolved_at
                        ],
                    )?;
                }
            }
            let r = tx.query_row(&format!("SELECT {REPLAY_COLUMNS} FROM review_replays WHERE diff_id = ?1 AND version = ?2 AND onto = ?3"), params![diff_id, n, onto_commit], replay_row)?;
            tx.commit()?;
            Ok(r)
        })
        .await
        .map_err(fail)?;
    if replay.state == "conflicted" {
        hub.emit(&ServerEvent::Conflict(Box::new(ConflictChange { version: Some(replay.version), state: Some(replay.state.clone()), ..changed(d, &replay.id) })));
    }
    Ok(replay)
}

/// The replay of `d`'s latest revision onto where it goes now, made and kept when it's new. `None`
/// when there's nothing to replay: the revision is on it already, or `d` is closed or landing.
pub async fn current(hub: &Hub, d: &Diff) -> Result<Option<Replay>, Fail> {
    if !State::parse(&d.state).is_some_and(|s| s.is_open() && s != State::Landing) {
        return Ok(None);
    }
    let Some(latest) = diffs::versions(hub, d).await.map_err(fail)?.pop() else { return Ok(None) };
    let (_, path) = diffs::home(hub, d).await?;
    let onto = onto_of(hub, d, &path).await?;
    if let Some(r) = kept_replay(hub, &d.id, latest.n, &onto.commit).await? {
        return Ok(Some(r));
    }
    let request = d.kind == "change_request";
    let there = if request { gitops::run_git(hub, &["-C", &path, "merge-base", "--is-ancestor", &onto.commit, &latest.commit], LS_REMOTE_TIMEOUT).await.is_ok() } else { latest.parent == onto.commit };
    if there {
        return Ok(None);
    }
    let (clean, m) = merge_tree(hub, &path, &onto.commit, &latest, request).await?;
    let found = if clean { Vec::new() } else { found(hub, &path, &m).await? };
    record(hub, d, &latest, &onto, &m.tree, found).await.map(Some)
}

/// What the gate holds for (W26-R11): each conflict of `d`'s replay onto where it goes now, until
/// Commit resolution makes a revision of it (one resolved where it falls waits on that). When the
/// replay can't run, the last one kept of its latest revision stands.
pub async fn for_gate(hub: &Hub, d: &Diff) -> Vec<kiln_ci::review::Conflict> {
    let replay = match current(hub, d).await {
        Ok(r) => r,
        Err((_, why)) => {
            tracing::warn!(diff = %d.name, error = %why, "a change's replay onto its trunk didn't run; the gate reads the last one kept");
            let latest = diffs::versions(hub, d).await.ok().and_then(|mut v| v.pop()).map(|v| v.n);
            last_replay(hub, &d.id).await.ok().flatten().filter(|r| Some(r.version) == latest)
        }
    };
    let Some(r) = replay.filter(|r| r.state == "conflicted") else { return Vec::new() };
    kept(hub, &r.id).await.unwrap_or_default().iter().map(Kept::for_gate).collect()
}

/// A replay and its conflicts as the API shows them: what's left to resolve, and whether Commit
/// resolution may go (`ready`: each conflict resolved).
fn view(d: &Diff, latest: Option<u32>, replay: Option<&Replay>, rows: &[Kept]) -> Value {
    let mut per_file: HashMap<&str, usize> = HashMap::new();
    for c in rows {
        *per_file.entry(c.file.as_str()).or_default() += 1;
    }
    let unresolved = rows.iter().filter(|c| c.take.is_none()).count();
    json!({
        "diff": d.name, "version": latest,
        "replay": replay.map(|r| json!({
            "id": r.id, "version": r.version, "commit": r.commit, "onto": { "name": r.onto_name, "commit": r.onto }, "state": r.state, "by": r.created_by, "at": r.created_at,
            "resolved_in": r.resolved_version, "resolved_by": r.resolved_by, "resolved_at": r.resolved_at,
        })),
        "conflicts": rows.iter().map(|c| c.view(per_file.get(c.file.as_str()).copied().unwrap_or(1))).collect::<Vec<_>>(),
        "unresolved": unresolved,
        "ready": replay.is_some_and(|r| r.state == "conflicted") && unresolved == 0,
    })
}

/// `GET /api/diffs/:id/conflicts` (W26-R12): its latest revision replayed onto where it goes now,
/// and each conflict where it falls; once Commit resolution made the latest revision, the replay
/// it resolved, with what each conflict took.
pub async fn list(hub: &Hub, d: &Diff) -> Result<Value, Fail> {
    let latest = diffs::versions(hub, d).await.map_err(fail)?.pop().map(|v| v.n);
    let replay = match current(hub, d).await? {
        Some(r) => Some(r),
        None => last_replay(hub, &d.id).await?.filter(|r| r.state == "resolved" && r.resolved_version.is_some() && r.resolved_version == latest),
    };
    let rows = match &replay {
        Some(r) => kept(hub, &r.id).await?,
        None => Vec::new(),
    };
    Ok(view(d, latest, replay.as_ref(), &rows))
}

/// The event saying `d`'s conflicts in `replay` changed (W26-R12), its other parts to fill in.
fn changed(d: &Diff, replay: &str) -> ConflictChange {
    ConflictChange { repo: d.repo.clone(), diff: d.name.clone(), replay: replay.to_owned(), version: None, state: None, conflict: None, take: None, by: None, agent: None }
}

/// Conflict `id` of `d`'s replay now, with the replay: refused when there's nothing to resolve, or
/// it isn't this replay's (what it goes onto moved: it was replayed again).
async fn open_conflict(hub: &Hub, d: &Diff, id: &str) -> Result<(Replay, Kept), Fail> {
    let replay = current(hub, d).await?.filter(|r| r.state == "conflicted").ok_or_else(|| refused(format!("{} goes onto where it lands cleanly: nothing to resolve", d.name)))?;
    let c = kept(hub, &replay.id)
        .await?
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no conflict {id} in {}'s replay onto {}: it was replayed again since, look again", d.name, replay.onto_name)))?;
    Ok((replay, c))
}

/// An edit's lines, from `lines` (an array) or `text` (split at its newlines).
fn given(body: &Value) -> Result<Option<Vec<String>>, Fail> {
    if let Some(ls) = body.get("lines").and_then(Value::as_array) {
        return ls.iter().map(|l| l.as_str().map(str::to_owned).ok_or_else(|| bad("`lines` are strings"))).collect::<Result<Vec<_>, _>>().map(Some);
    }
    Ok(body.get("text").and_then(Value::as_str).map(|t| {
        let t = t.strip_suffix('\n').unwrap_or(t);
        if t.is_empty() { Vec::new() } else { t.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).to_owned()).collect() }
    }))
}

/// `POST /api/diffs/:id/conflicts/:conflict/resolution` (W26-R12): `{ take: main | this | both |
/// agent | edit, lines? | text? }`. A person's resolves it (what it takes, by whom, when; the
/// revision comes with Commit resolution); an agent's is kept as the agent's resolution, for a
/// person to take (`take: agent`). A conflict that isn't text takes a side, or the agent's choice;
/// an edit holds no conflict markers. The replay comes back as `GET …/conflicts` shows it.
pub async fn resolve(hub: &Hub, d: &Diff, id: &str, caller: &Caller, body: &Value) -> Result<Value, Fail> {
    may_change(caller, d)?;
    let (me, agent) = super::review::who(caller);
    let take = body.get("take").and_then(Value::as_str).and_then(Take::parse).ok_or_else(|| bad("`take`: main, this, both, agent or edit"))?;
    let (replay, c) = open_conflict(hub, d, id).await?;
    let place = c.place();
    if !Take::takes(c.kind).contains(&take) {
        return Err(bad(c.kind.named(&c.file)));
    }
    if agent && take == Take::Agent {
        return Err(bad(format!("{place}: an agent resolves it its own way: main, this, both or an edit")));
    }
    // The lines a text conflict resolves to; a conflict that isn't text keeps only what it takes.
    let lines: Option<Vec<String>> = match take {
        Take::Edit => {
            let ls = given(body)?.ok_or_else(|| bad(format!("{place}: an edit gives its `lines` (or `text`)")))?;
            if conflicts::has_markers(&ls) {
                return Err(bad(format!("{place}: the edit still has conflict markers")));
            }
            Some(ls)
        }
        Take::Agent if c.agent_by.is_none() => return Err(refused(format!("{place}: no agent has resolved it"))),
        Take::Agent => c.agent_lines.clone().filter(|_| c.kind.by_lines()),
        Take::Main if c.kind.by_lines() => Some(side_lines(&c.main)),
        Take::This if c.kind.by_lines() => Some(side_lines(&c.this)),
        Take::Both => Some(side_lines(&c.main).into_iter().chain(side_lines(&c.this)).collect()),
        Take::Main | Take::This => None,
    };
    let (cid, me2, taken, lines_s) = (c.id.clone(), me.clone(), take.as_str(), lines.as_ref().map(|l| json!(l).to_string()));
    hub.store
        .call(move |db| {
            let now = crate::util::now_unix();
            let sql = if agent {
                "UPDATE review_conflicts SET agent_take = ?2, agent_lines = ?3, agent_by = ?4, agent_at = ?5 WHERE id = ?1"
            } else {
                "UPDATE review_conflicts SET take = ?2, lines = ?3, resolved_by = ?4, resolved_at = ?5 WHERE id = ?1"
            };
            db.execute(sql, params![cid, taken, lines_s, me2, now])?;
            Ok(())
        })
        .await
        .map_err(fail)?;
    let whose = if agent { "an agent's resolution" } else { "resolved" };
    hub.store.audit(caller.name.clone(), "diffs.conflict.resolve".into(), format!("{} {} v{} ({place}): {whose}, taking {taken}", d.repo, d.name, replay.version)).await;
    hub.emit(&ServerEvent::Conflict(Box::new(ConflictChange { conflict: Some(c.id.clone()), take: Some(taken.to_owned()), by: Some(me.clone()), agent: Some(agent), ..changed(d, &replay.id) })));
    let rows = kept(hub, &replay.id).await?;
    Ok(view(d, Some(replay.version), Some(&replay), &rows))
}

/// `DELETE /api/diffs/:id/conflicts/:conflict/resolution`: a person's resolution taken back (an
/// agent takes back its own).
pub async fn undo(hub: &Hub, d: &Diff, id: &str, caller: &Caller) -> Result<Value, Fail> {
    may_change(caller, d)?;
    let (me, agent) = super::review::who(caller);
    let (replay, c) = open_conflict(hub, d, id).await?;
    let cid = c.id.clone();
    hub.store
        .call(move |db| {
            let sql = if agent {
                "UPDATE review_conflicts SET agent_take = NULL, agent_lines = NULL, agent_by = NULL, agent_at = NULL WHERE id = ?1"
            } else {
                "UPDATE review_conflicts SET take = NULL, lines = NULL, resolved_by = NULL, resolved_at = NULL WHERE id = ?1"
            };
            db.execute(sql, [&cid])?;
            Ok(())
        })
        .await
        .map_err(fail)?;
    hub.store.audit(caller.name.clone(), "diffs.conflict.undo".into(), format!("{} {} v{} ({})", d.repo, d.name, replay.version, c.place())).await;
    hub.emit(&ServerEvent::Conflict(Box::new(ConflictChange { conflict: Some(c.id.clone()), by: Some(me.clone()), agent: Some(agent), ..changed(d, &replay.id) })));
    let rows = kept(hub, &replay.id).await?;
    Ok(view(d, Some(replay.version), Some(&replay), &rows))
}

/// `tree` with `puts` (mode, blob, path) and `removes` written to the repository, without a working
/// tree.
async fn tree_with(hub: &Hub, path: &str, tree: &str, puts: &[(String, String, String)], removes: &[String]) -> Result<String, Fail> {
    let index = std::env::temp_dir().join(format!("kiln-index-{}", crate::util::new_id()));
    let idx = index.to_string_lossy().into_owned();
    let env = [("GIT_INDEX_FILE", idx.as_str())];
    let zero = "0".repeat(tree.len());
    let result = async {
        let said = |r: &diffs::Ran| internal(String::from_utf8_lossy(&r.err).trim().to_owned());
        let r = diffs::git_run(hub, &["-C", path, "read-tree", tree], &env, Vec::new()).await.map_err(fail)?;
        if !r.ok {
            return Err(said(&r));
        }
        let mut info = Vec::new();
        for (mode, oid, p) in puts {
            info.extend(format!("{mode} {oid}\t{p}\0").into_bytes());
        }
        for p in removes {
            info.extend(format!("0 {zero}\t{p}\0").into_bytes());
        }
        let r = diffs::git_run(hub, &["-C", path, "update-index", "-z", "--index-info"], &env, info).await.map_err(fail)?;
        if !r.ok {
            return Err(said(&r));
        }
        let r = diffs::git_run(hub, &["-C", path, "write-tree"], &env, Vec::new()).await.map_err(fail)?;
        if !r.ok {
            return Err(said(&r));
        }
        Ok(String::from_utf8_lossy(&r.out).trim().to_owned())
    }
    .await;
    let _ = std::fs::remove_file(&index);
    result
}

/// What each conflicted path of a fresh replay becomes by what its conflicts took: a text file
/// with each conflict resolved where it falls (its mode as merged), or the side a path taken whole
/// takes (gone where that side has nothing). Refused when the replay isn't the one resolved.
async fn resolutions(hub: &Hub, path: &str, m: &Merged, rows: &[Kept]) -> Result<(Vec<(String, String, String)>, Vec<String>), Fail> {
    let (mut puts, mut removes) = (Vec::new(), Vec::new());
    let modes: HashMap<String, String> = {
        let mut args = vec!["-C", path, "ls-tree", "-z", m.tree.as_str(), "--"];
        args.extend(m.paths.iter().map(|(p, _)| p.as_str()));
        let out = gitops::run_git(hub, &args, LS_REMOTE_TIMEOUT).await.map_err(fail)?;
        String::from_utf8_lossy(&out).split('\0').filter_map(|e| e.split_once('\t').and_then(|(head, p)| head.split(' ').next().map(|mode| (p.to_owned(), mode.to_owned())))).collect()
    };
    let again = |file: &str| refused(format!("{file}: the replay came out otherwise this time; look again"));
    for (file, stages) in &m.paths {
        let mine: Vec<&Kept> = rows.iter().filter(|c| c.file == *file).collect();
        let stage = |n: u8| stages.iter().find(|s| s.stage == n);
        let Some(first) = mine.first() else { return Err(again(file)) };
        if !first.kind.by_lines() {
            let side = first.side().ok_or_else(|| refused(format!("{}: resolve this conflict first", first.place())))?;
            match stage(if side == Take::Main { 2 } else { 3 }) {
                Some(s) => puts.push((s.mode.clone(), s.oid.clone(), file.clone())),
                None => removes.push(file.clone()),
            }
            continue;
        }
        let text = Text::parse(&String::from_utf8_lossy(&blob(hub, path, &format!("{}:{file}", m.tree)).await?)).map_err(|_| again(file))?;
        let hunks = text.hunks();
        let same = hunks.len() == mine.len() && hunks.iter().zip(&mine).all(|(h, c)| conflicts::plain(&h.main) == side_lines(&c.main) && conflicts::plain(&h.this) == side_lines(&c.this));
        if !same {
            return Err(again(file));
        }
        let mut picks = Vec::with_capacity(mine.len());
        for (i, c) in mine.iter().enumerate() {
            let take = c.take.ok_or_else(|| refused(format!("{}: resolve this conflict first", c.place())))?;
            let given = match take {
                Take::Agent | Take::Edit => c.lines.as_deref(),
                _ => None,
            };
            picks.push(text.pick(i, take, given).map_err(internal)?);
        }
        let mut content = text.resolved(&picks).map_err(internal)?;
        // A file whose last lines are a side's that ended without a newline ends without one again.
        if text.ends_in_conflict()
            && let Some(last) = mine.last()
        {
            let from = match last.take {
                Some(Take::Main) => stage(2),
                Some(Take::This) => stage(3),
                Some(Take::Both) if side_lines(&last.this).is_empty() => stage(2),
                Some(Take::Both) => stage(3),
                _ => None,
            };
            if let Some(s) = from
                && blob(hub, path, &s.oid).await?.last().is_some_and(|b| *b != b'\n')
                && content.ends_with('\n')
            {
                content.pop();
            }
        }
        let oid = diffs::git_with_input(hub, &["-C", path, "hash-object", "-w", "--stdin"], content.into_bytes()).await.map_err(fail)?;
        let mode = modes.get(file).cloned().or_else(|| stage(2).or_else(|| stage(3)).map(|s| s.mode.clone())).unwrap_or_else(|| "100644".into());
        puts.push((mode, String::from_utf8_lossy(&oid).trim().to_owned(), file.clone()));
    }
    if rows.iter().any(|c| !m.paths.iter().any(|(p, _)| *p == c.file)) {
        return Err(refused("the replay came out otherwise this time; look again"));
    }
    Ok((puts, removes))
}

/// `POST /api/diffs/:id/conflicts/commit` (W26-R12): Commit resolution. Once each conflict of the
/// replay now is resolved, one revision of whoever commits it, its why `conflict resolved`: a
/// diff's commit onto where it goes (its author and message kept, the committer them), a change
/// request's merge commit of where it goes into its branch. Agents whose resolutions were taken
/// are its co-authors. Unsigned until W27-R03 signs `revision::commit`'s commits. A person commits it:
/// an agent's resolution waits on one.
pub async fn commit(hub: &Arc<Hub>, d: &Diff, caller: &Caller) -> Result<Value, Fail> {
    may_change(caller, d)?;
    let (me, agent) = super::review::who(caller);
    if agent {
        return Err((StatusCode::FORBIDDEN, format!("{}: a person commits a resolution; an agent's waits for one to take it", d.name)));
    }
    let replay = current(hub, d).await?.filter(|r| r.state == "conflicted").ok_or_else(|| refused(format!("{} goes onto where it lands cleanly: nothing to resolve", d.name)))?;
    let rows = kept(hub, &replay.id).await?;
    let open: Vec<String> = rows.iter().filter(|c| c.take.is_none()).map(|c| format!("{}: resolve this conflict first", c.place())).collect();
    if !open.is_empty() {
        return Err(refused(open.join("; ")));
    }
    let (repo, path) = diffs::home(hub, d).await?;
    let latest = diffs::versions(hub, d).await.map_err(fail)?.pop().filter(|v| v.n == replay.version).ok_or_else(|| refused(format!("{} has a newer revision: look again", d.name)))?;
    let onto = onto_of(hub, d, &path).await?;
    if onto.commit != replay.onto {
        return Err(refused(format!("{} moved: look again", onto.name)));
    }
    let (_, m) = merge_tree(hub, &path, &onto.commit, &latest, d.kind == "change_request").await?;
    if m.tree != replay.tree {
        return Err(refused("the replay came out otherwise this time; look again"));
    }
    let (puts, removes) = resolutions(hub, &path, &m, &rows).await?;
    let tree = tree_with(hub, &path, &m.tree, &puts, &removes).await?;
    // An agent's resolution taken makes the agent a co-author.
    let agents: Vec<&str> = rows.iter().filter(|c| c.take == Some(Take::Agent)).filter_map(|c| c.agent_by.as_deref()).collect::<BTreeSet<_>>().into_iter().collect();
    let files: BTreeSet<&str> = rows.iter().map(|c| c.file.as_str()).collect();
    let (n, made) = match &d.branch {
        // A change request: where it goes merged into its branch, the merge resolved.
        Some(branch) => {
            let listed: Vec<String> = files.iter().map(|f| format!("\t{f}")).collect();
            let message = format!("Merge {} into {branch}\n\nConflicts resolved:\n{}\n", onto.name, listed.join("\n"));
            let made = revision::commit(hub, &path, &revision::Commit { tree: &tree, parents: &[latest.commit.as_str(), onto.commit.as_str()], author: (me.as_str(), None), by: &caller.name, co_authors: &agents, message: &message })
                .await
                .map_err(internal)?;
            gitops::run_git(hub, &["-C", &path, "update-ref", &format!("refs/heads/{branch}"), &made.sha, &latest.commit], LS_REMOTE_TIMEOUT).await.map_err(|_| refused(format!("{branch} moved: push or pull, then try again")))?;
            let by = super::cr::By { pushed_by: caller.name.clone(), author: diffs::author_of(&caller.name, Some(caller.kind)), started_by: caller.started_by() };
            let out = super::cr::submit(hub, &repo, &path, &d.trunk, branch, &made.sha, &by, &kiln_ci::review::Options::default()).await.map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
            (out.version, made)
        }
        // A diff: its commit onto where it goes, its author and message kept.
        None => {
            let message = gitops::run_git(hub, &["-C", &path, "log", "-1", "--format=%B", &latest.commit], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default();
            let who_wrote = gitops::run_git(hub, &["-C", &path, "log", "-1", "--format=%an <%ae>%x00%ad", "--date=raw", &latest.commit], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_default();
            let (author, date) = who_wrote.split_once('\0').unwrap_or((&who_wrote, ""));
            let made = revision::commit(hub, &path, &revision::Commit { tree: &tree, parents: &[onto.commit.as_str()], author: (author, Some(date)), by: &caller.name, co_authors: &agents, message: &message }).await.map_err(internal)?;
            let patch = diffs::git_run(hub, &["-C", &path, "show", "--no-color", &made.sha], &[], Vec::new()).await.ok().map(|r| r.out).unwrap_or_default();
            let patch_id = diffs::git_with_input(hub, &["-C", &path, "patch-id", "--stable"], patch).await.ok().and_then(|o| String::from_utf8_lossy(&o).split_whitespace().next().map(str::to_owned)).unwrap_or_default();
            let nv = diffs::NewVersion { commit: made.sha.clone(), parent: onto.commit.clone(), base: onto.base.clone(), patch_id, message: made.message.clone(), note: None, rebase_only: false, pushed_by: caller.name.clone(), parent_diff: onto.below.clone() };
            (diffs::add_version(hub, &repo, &path, d, nv).await.map_err(fail)?, made)
        }
    };
    // Why the revision exists (W26-R04).
    revision::record(hub, &d.id, n, &made.sha, &revision::Why { why: WHY, thread: None, comment: None, note: Some(WHY) }).await.map_err(fail)?;
    let (rid, me2) = (replay.id.clone(), me.clone());
    hub.store
        .call(move |c| Ok(c.execute("UPDATE review_replays SET state = 'resolved', resolved_version = ?2, resolved_by = ?3, resolved_at = ?4 WHERE id = ?1", params![rid, n, me2, crate::util::now_unix()]).map(|_| ())?))
        .await
        .map_err(fail)?;
    hub.store.audit(caller.name.clone(), "diffs.conflicts.commit".into(), format!("{} {} v{n}: {} conflicts resolved onto {} ({})", d.repo, d.name, rows.len(), onto.name, made.sha)).await;
    // What waits on the gate asks it again: an automatic merge goes once it clears.
    super::land::kick();
    hub.emit(&ServerEvent::Conflict(Box::new(ConflictChange { version: Some(n), state: Some("resolved".to_owned()), by: Some(me.clone()), ..changed(d, &replay.id) })));
    Ok(json!({ "diff": d.name, "version": n, "why": WHY, "commit": made.sha, "by": me, "signer": made.signer, "replay": replay.id, "conflicts": rows.len() }))
}
