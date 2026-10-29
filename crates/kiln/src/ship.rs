//! Ship's read model (plan 01a175b3a21174bc8769f5e784b90ecb): each commit of a repository and where
//! it stands, from the records kiln keeps for it (the plan's "Sources by commit"): the land queue;
//! its runs, their legs and each leg's attempts; the release pipeline's legs and the images built;
//! each environment's deployments; Deploy API releases of it; and, for the commit production runs,
//! each runner's replant. Where it stands is `kiln_cd::journey`'s to say: this reads the records
//! into a journey and shapes the JSON. Every field is new, and a client must take any as missing.
//!
//! Changes are Ship items by commit (docs/ci.md W26-R01): a commit is an item when it has runs or
//! is a change's (a diff's version, a change request's commits, where a change landed; a host's
//! pull request as `githost::mirror` keeps it: its heads, its commits, where it merged). A change's
//! tip (its latest version's head, or the commit it landed as) carries its review; its other
//! commits fold under the tip (`fold_under`), an older version's head standing superseded. Each row
//! is rendered for those whose turn it is ('yours') and for everyone else, and a request picks by
//! the caller's principals (an agent counts as its person).
//!
//! - `GET /api/ship/commits?repo=&limit=&cursor=&host=&kind=&lens=`: a repository's commits, newest
//!   first, cursor-paged; `host` (kiln, github, gitlab, bitbucket) and `kind` (diff, request, commit)
//!   filter, and `lens=review` keeps the changes' commits (W26-R02).
//! - `GET /api/ship/commits/:repo/:sha`: one commit, its records and where it stands.

use std::collections::{HashMap, HashSet};

use http::StatusCode;
use kiln_cd::journey::{self, Attempt, Author, Build, Check, Journey, Kinds, Land, Leg, Part, Platform, Production, Replant, Review, ReviewState, Runner, RunnerReplant, Staging};
use kiln_ci::review::{Facts, Kind, Standing};
use kiln_ci::turn;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

pub type Fail = (StatusCode, String);

/// How long kiln's guard watches production after a deploy before it counts as held.
const GUARD_MS: i64 = 10 * 60 * 1000;

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// A runner, as the updates know it: which commit it runs, and whether its upgrade to production's
/// commit failed (and why).
#[derive(Debug, Clone, Default)]
pub struct RunnerSeen {
    pub name: String,
    pub connected: bool,
    pub commit: Option<String>,
    pub failed: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct RunRec {
    id: String,
    pipeline: String,
    branch: String,
    message: String,
    actor: String,
    created_at: i64,
}

#[derive(Debug, Clone, Default)]
struct AttemptRec {
    status: String,
    conclusion: Option<String>,
    class: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct LegRec {
    /// The job's id (`jobs.id`), the one its log streams under.
    job: String,
    run_id: String,
    /// When its job started and finished (unix seconds), as far as it has.
    started: Option<i64>,
    finished: Option<i64>,
    name: String,
    status: String,
    conclusion: Option<String>,
    reused: bool,
    error: Option<String>,
    attempts: Vec<AttemptRec>,
}

/// A KilnCD deployment of a commit, as Ship reads it: kiln deploys itself through KilnCD, not CI's
/// deployments (plan 01a1856fe81375b1bd1d1d86f270a45b, step 4). Its app's environment; the commit
/// it deployed (an image it pinned was built from it, or else its revision names it); how its sync
/// and the guard's word after it went (they share an op); and when the sync finished (unix seconds).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CdDeploy {
    pub app: String,
    pub environment: String,
    pub sha: String,
    /// It pinned an image built from the commit (else its revision named it).
    pub built: bool,
    pub op: u64,
    /// `success` or `failure`.
    pub state: String,
    pub description: String,
    pub at: i64,
}

/// A tag or a revision that names a commit: seven to forty hex digits.
fn sha_like(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One app's history as deployments of commits, oldest first: a sync and the guard's word after it
/// share an op, and either failing fails the deployment.
fn deploys_of<'a>(app: &str, environment: &str, history: impl IntoIterator<Item = &'a kiln_cd::state::Deployment>) -> Vec<CdDeploy> {
    let mut out: Vec<CdDeploy> = Vec::new();
    for h in history {
        let pinned = h.artifacts.iter().find_map(|a| a.tag.as_deref().filter(|t| sha_like(t)));
        let (sha, built) = match (pinned, h.revision.as_deref().filter(|r| sha_like(r))) {
            (Some(t), _) => (t.to_string(), true),
            (None, Some(r)) => (r.to_string(), false),
            (None, None) => continue,
        };
        match out.last_mut().filter(|d| d.op == h.op && d.sha == sha) {
            Some(d) => {
                if !h.succeeded {
                    d.state = "failure".into();
                }
                d.description = h.message.clone();
            }
            None => out.push(CdDeploy {
                app: app.to_string(),
                environment: environment.to_string(),
                sha,
                built,
                op: h.op,
                state: if h.succeeded { "success" } else { "failure" }.to_string(),
                description: h.message.clone(),
                at: i64::try_from(h.at_ms / 1000).unwrap_or(i64::MAX),
            }),
        }
    }
    out
}

/// Every KilnCD app's deployments of commits, by the environment it deploys to (none without
/// KilnCD).
pub fn cd_deploys() -> Vec<CdDeploy> {
    let Some(cd) = crate::cd::get() else { return Vec::new() };
    let apps: Vec<(String, String)> = cd.apps.read().iter().filter_map(|(id, e)| e.def.environment.clone().map(|env| (id.clone(), env))).collect();
    cd.store.read(|d| apps.iter().flat_map(|(id, env)| deploys_of(id, env, d.history.get(id).into_iter().flatten())).collect())
}

/// What KilnCD's deployments read as, to see they changed: a refresh then reads every commit again.
fn cd_mark(deploys: &[CdDeploy]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    deploys.hash(&mut h);
    h.finish()
}

/// KilnCD's deployments of a repository's commits, as Ship's deployment records. In an environment
/// where an app pinned images built from them, only those count: another app's git syncs there
/// don't say what runs.
fn cd_recs(deploys: &[CdDeploy], commits: &[(String, i64, String)]) -> Vec<DeployRec> {
    let of = |sha: &str| commits.iter().find(|(s, _, _)| s == sha || (sha.len() >= 7 && s.starts_with(sha))).map(|(s, f, _)| (s.clone(), *f));
    let mine: Vec<(&CdDeploy, String, i64)> = deploys.iter().filter_map(|d| of(&d.sha).map(|(s, f)| (d, s, f))).collect();
    mine.iter()
        .filter(|(d, _, _)| d.built || !mine.iter().any(|(x, _, _)| x.built && x.environment == d.environment))
        .map(|(d, sha, first_row)| DeployRec {
            environment: d.environment.clone(),
            production: d.environment.eq_ignore_ascii_case("production"),
            sha: sha.clone(),
            first_row: *first_row,
            state: Some(d.state.clone()),
            description: Some(d.description.clone()),
            at: d.at,
        })
        .collect()
}

/// One environment's deployment of a commit: its latest status and when it was set (unix seconds).
#[derive(Debug, Clone, Default)]
struct DeployRec {
    environment: String,
    production: bool,
    sha: String,
    first_row: i64,
    state: Option<String>,
    description: Option<String>,
    at: i64,
}

#[derive(Debug, Clone, Default)]
struct ReleaseRec {
    id: i64,
    state: String,
    message: Option<String>,
    rolled_back: bool,
}

/// Everything kiln keeps for one commit, as read.
#[derive(Debug, Clone, Default)]
struct Records {
    repo: String,
    sha: String,
    first_row: i64,
    runs: Vec<RunRec>,
    legs: Vec<LegRec>,
    images: Vec<String>,
    land: Option<(String, Option<String>)>,
    own: Vec<DeployRec>,
    current: Vec<DeployRec>,
    releases: Vec<ReleaseRec>,
    /// A newer commit production waits on: its sha, and how far behind it this one is.
    waiting: Option<(String, i64)>,
    riders: u32,
    tags: Vec<String>,
    /// The change this commit is of, and what its review is read from (W26-R01).
    change: Option<ChangeRec>,
}

/// An open thread on a change: its kind, where it is, who opened it and its first words.
#[derive(Debug, Clone, Default)]
struct ThreadRec {
    kind: String,
    file: Option<String>,
    line: Option<i64>,
    by: String,
    said: String,
}

/// A mark of the Review lane group (W26-R14), as agreed with the Ship agent (2026-10-26, NOTES.md):
/// a push, replay, comment, fix, resolution or approval, each with its time (`end` null while under
/// way, = `at` for a point), who did it (the principal as kiln writes it), its revision and its
/// signer — null until W27-R03 signs kiln's records, so the seal says unsigned, never a false one.
/// `state` is a GlyphState name (ui status-glyph-spec.ts).
#[derive(Debug, Clone, Default, PartialEq)]
struct MarkRec {
    id: String,
    kind: &'static str,
    at: i64,
    end: Option<i64>,
    by: String,
    revision: Option<u32>,
    state: &'static str,
    thread: Option<String>,
    note: Option<String>,
}

/// The change a commit is of: a kiln diff or change request (W26-R01), and the records its review
/// stands on (W26-R02, R05). Times are unix seconds.
#[derive(Debug, Clone, Default)]
struct ChangeRec {
    name: String,
    /// `diff` or `request`.
    kind: &'static str,
    host: String,
    title: String,
    author: String,
    author_for: Option<String>,
    state: String,
    landed: Option<String>,
    /// This commit's version, and whether it is that version's head (else one of the commits a
    /// change request's version brings).
    n: u32,
    head: bool,
    latest: u32,
    latest_sha: String,
    message: String,
    at: i64,
    reviewers: Vec<(String, i64)>,
    /// (reviewer, action, at, body, version reviewed)
    reviews: Vec<(String, String, i64, String, Option<u32>)>,
    /// (version, rebase only, at)
    pushes: Vec<(u32, bool, i64)>,
    threads: Vec<ThreadRec>,
    replies: Vec<(String, i64)>,
    /// The land queue's latest entry landing this change: its state and why.
    queue: Option<(String, Option<String>)>,
    /// The conflicts its latest revision's replay onto where it lands left, each holding its gate
    /// until Commit resolution makes a revision of them (W26-R12).
    conflicts: Vec<kiln_ci::review::Conflict>,
    /// The Review lane group's marks, oldest first (W26-R14).
    marks: Vec<MarkRec>,
}

impl ChangeRec {
    /// The commit that carries the change's review: where it landed, else its latest head.
    fn tip(&self) -> &str {
        self.landed.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.latest_sha)
    }

    fn is_landed(&self) -> bool {
        self.state == "landed" || self.landed.as_deref().is_some_and(|s| !s.is_empty())
    }
}

/// Which change a commit (`o.repo_id`, `o.item`) is of: the newest change naming it, its head
/// before its other commits. The facets' SQL picks the same way ([`facet_sql`]).
const CHANGE_OF: &str = "FROM ship_commit_changes x WHERE x.repo_id = o.repo_id AND x.sha = o.item ORDER BY x.created_at DESC, x.number DESC, x.diff_id DESC, x.head DESC, x.n DESC LIMIT 1";
/// A commit (`o`) is an item when it has runs or is a change's (W26-R01).
const IS_ITEM: &str = "(EXISTS (SELECT 1 FROM runs r WHERE r.repo_id = o.repo_id AND r.sha = o.item) OR EXISTS (SELECT 1 FROM ship_commit_changes x WHERE x.repo_id = o.repo_id AND x.sha = o.item))";

/// A commit's kind (`diff`, `request`, `commit`) and its change's host, as SQL over `o`.
fn facet_sql() -> (String, String) {
    (format!("COALESCE((SELECT CASE x.kind WHEN 'change_request' THEN 'request' ELSE 'diff' END {CHANGE_OF}), 'commit')"), format!("(SELECT x.host {CHANGE_OF})"))
}

fn first_words(s: &str) -> String {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    if line.chars().count() <= 40 { line.to_string() } else { format!("{}…", line.chars().take(39).collect::<String>().trim_end()) }
}

/// A principal's name as a line shows it (`user:kim` is kim).
fn shown(p: &str) -> &str {
    p.split(" for ").next().unwrap_or(p).rsplit(':').next().unwrap_or(p)
}

/// Marks of one second read in the order a revision's story takes (W26-R14): the push, the talk,
/// the fix, the replay, the resolution, the verdict.
fn mark_rank(kind: &str) -> u8 {
    match kind {
        "push" => 0,
        "comment" => 1,
        "fix" => 2,
        "replay" => 3,
        "resolution" => 4,
        _ => 5,
    }
}

/// Approval marks (W26-R14): each reviewer asked who has answered nothing yet (state `requested`,
/// no end), and each verdict given (`accept` approves, `request_changes` asks changes, `resign`
/// steps back). A review posted as comments alone is no verdict; its comments mark themselves.
fn approval_marks(marks: &mut Vec<MarkRec>, key: &str, reviewers: &[(String, i64)], reviews: &[(String, String, i64, String, Option<u32>)]) {
    for (reviewer, at) in reviewers {
        if !reviews.iter().any(|(w, a, _, _, _)| w == reviewer && a != "comment") {
            marks.push(MarkRec { id: format!("ask:{key}:{reviewer}"), kind: "approval", at: *at, end: None, by: reviewer.clone(), revision: None, state: "requested", thread: None, note: Some("asked".to_string()) });
        }
    }
    for (i, (by, action, at, body, version)) in reviews.iter().enumerate() {
        let (state, note) = match action.as_str() {
            "accept" => ("approved", None),
            "request_changes" => ("changes", None),
            "resign" => ("superseded", Some("resigned".to_string())),
            _ => continue,
        };
        let words = first_words(body);
        marks.push(MarkRec {
            id: format!("{key}:review:{i}"),
            kind: "approval",
            at: *at,
            end: Some(*at),
            by: by.clone(),
            revision: *version,
            state,
            thread: None,
            note: note.or_else(|| (!words.is_empty()).then_some(words)),
        });
    }
}

/// A mark as the detail serves it (W26-R14); `signer` is null until W27-R03 signs kiln's records.
fn mark_json(m: &MarkRec) -> Value {
    let mut v = json!({ "id": m.id, "kind": m.kind, "at": m.at, "end": m.end, "by": m.by, "signer": Value::Null, "revision": m.revision, "state": m.state });
    if let Some(o) = v.as_object_mut() {
        if let Some(t) = &m.thread {
            o.insert("thread".into(), json!(t));
        }
        if let Some(n) = &m.note {
            o.insert("note".into(), json!(n));
        }
    }
    v
}

/// One commit's change, if it is a change's.
fn load_change(c: &rusqlite::Connection, repo_id: &str, sha: &str) -> anyhow::Result<Option<ChangeRec>> {
    let found: Option<(String, Option<u32>, bool, String)> = c
        .query_row(&format!("SELECT x.diff_id, x.n, x.head, x.host {}", CHANGE_OF.replace("o.repo_id", "?1").replace("o.item", "?2")), params![repo_id, sha], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .optional()?;
    let Some((id, n, head, host)) = found else { return Ok(None) };
    if let Some(number) = id.strip_prefix(&format!("pull:{host}:")).and_then(|n| n.parse::<i64>().ok()) {
        return load_pull(c, repo_id, &host, number, n.unwrap_or(0), head).map(Some);
    }
    let mut ch = c.query_row("SELECT number, kind, title, author, author_for, state, landed_commit FROM diffs WHERE id = ?1", [&id], |r| {
        let (number, kind): (i64, String) = (r.get(0)?, r.get(1)?);
        let request = kind == "change_request";
        Ok(ChangeRec {
            name: format!("{}{number}", if request { "CR" } else { "D" }),
            kind: if request { "request" } else { "diff" },
            host: host.clone(),
            title: r.get(2)?,
            author: r.get(3)?,
            author_for: r.get(4)?,
            state: r.get(5)?,
            landed: r.get(6)?,
            n: n.unwrap_or(0),
            head,
            ..ChangeRec::default()
        })
    })?;
    let mut marks: Vec<MarkRec> = Vec::new();
    let mut st = c.prepare("SELECT n, commit_sha, rebase_only, created_at, message, pushed_by, why, thread FROM diff_versions WHERE diff_id = ?1 ORDER BY n")?;
    for row in st.query_map([&id], |r| {
        Ok((
            r.get::<_, u32>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, bool>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, Option<String>>(7)?,
        ))
    })? {
        let (v, commit, rebase_only, at, message, pushed_by, why, thread) = row?;
        if v == ch.n {
            (ch.message, ch.at) = (message, at);
        }
        ch.pushes.push((v, rebase_only, at));
        (ch.latest, ch.latest_sha) = (v, commit);
        // A revision is the lane's push, the fix answering a thread (an agent's fix, a suggestion
        // applied) or the resolution of a replay's conflicts, by which `why` it was made (W26-R14).
        let kind = match why.as_deref() {
            Some("conflict resolved") => "resolution",
            Some(_) => "fix",
            None if thread.is_some() => "fix",
            None => "push",
        };
        marks.push(MarkRec {
            id: format!("{id}:v{v}"),
            kind,
            at,
            end: Some(at),
            by: pushed_by,
            revision: Some(v),
            state: "passed",
            thread,
            note: why.or_else(|| rebase_only.then(|| "rebase only".to_string())),
        });
    }
    let mut st = c.prepare("SELECT reviewer, added_at FROM diff_reviewers WHERE diff_id = ?1 ORDER BY added_at, reviewer")?;
    ch.reviewers = st.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
    let mut st = c.prepare("SELECT reviewer, action, created_at, body, version FROM reviews WHERE diff_id = ?1 ORDER BY created_at, rowid")?;
    ch.reviews = st
        .query_map([&id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, Some(r.get::<_, u32>(4)?))))?
        .collect::<Result<_, _>>()?;
    approval_marks(&mut marks, &id, &ch.reviewers, &ch.reviews);
    let mut st = c.prepare(
        "SELECT t.kind, t.file, t.start_line, t.created_by, (SELECT c.body FROM review_comments c WHERE c.thread_id = t.id AND c.published = 1 ORDER BY c.created_at, c.rowid LIMIT 1)
         FROM review_threads t WHERE t.diff_id = ?1 AND t.state = 'open' AND EXISTS (SELECT 1 FROM review_comments c WHERE c.thread_id = t.id AND c.published = 1) ORDER BY t.created_at, t.id",
    )?;
    ch.threads = st.query_map([&id], |r| Ok(ThreadRec { kind: r.get(0)?, file: r.get(1)?, line: r.get(2)?, by: r.get(3)?, said: r.get::<_, Option<String>>(4)?.unwrap_or_default() }))?.collect::<Result<_, _>>()?;
    // Each thread's published comments, oldest first (the first is its opening one).
    let mut by_thread: HashMap<String, Vec<(String, String, i64)>> = HashMap::new();
    let mut st = c.prepare("SELECT c.thread_id, c.id, c.author, c.created_at FROM review_comments c JOIN review_threads t ON t.id = c.thread_id WHERE t.diff_id = ?1 AND c.published = 1 ORDER BY c.created_at, c.rowid")?;
    for row in st.query_map([&id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)))? {
        let (tid, cid, author, at) = row?;
        ch.replies.push((author.clone(), at));
        by_thread.entry(tid).or_default().push((cid, author, at));
    }
    // Each published thread is a comment mark as it opens (a blocking one holds, any other waits),
    // each published comment after its thread's first is its answer, each resolution its point.
    let mut st = c.prepare(
        "SELECT t.id, t.kind, t.file, t.start_line, t.state, t.created_by, t.created_at, t.resolved_by, t.resolved_at, t.resolution
         FROM review_threads t WHERE t.diff_id = ?1 AND EXISTS (SELECT 1 FROM review_comments c WHERE c.thread_id = t.id AND c.published = 1) ORDER BY t.created_at, t.id",
    )?;
    for row in st.query_map([&id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, Option<String>>(7)?,
            r.get::<_, Option<i64>>(8)?,
            r.get::<_, Option<String>>(9)?,
        ))
    })? {
        let (tid, kind, file, line, state, by, at, resolved_by, resolved_at, resolution) = row?;
        let open = state != "resolved";
        let place = kiln_ci::review::place(file.as_deref(), line.and_then(|l| u32::try_from(l).ok()));
        marks.push(MarkRec {
            id: format!("{tid}:open"),
            kind: "comment",
            at,
            end: resolved_at,
            by,
            revision: None,
            state: if !open { "passed" } else if kind == "blocking" { "blocked" } else { "needs" },
            thread: Some(tid.clone()),
            note: Some(if kind == "question" { place } else { format!("{kind} · {place}") }),
        });
        for (cid, author, at) in by_thread.get(&tid).into_iter().flatten().skip(1) {
            marks.push(MarkRec { id: format!("comment:{cid}"), kind: "comment", at: *at, end: Some(*at), by: author.clone(), revision: None, state: "passed", thread: Some(tid.clone()), note: Some("answered".to_string()) });
        }
        if let (Some(by), Some(at)) = (resolved_by, resolved_at) {
            marks.push(MarkRec {
                id: format!("{tid}:resolved"),
                kind: "comment",
                at,
                end: Some(at),
                by,
                revision: None,
                state: "passed",
                thread: Some(tid),
                note: resolution.filter(|r| !r.is_empty()).or_else(|| Some("resolved".to_string())),
            });
        }
    }
    // Each replay of a revision onto where it lands: a conflicted one is under way until resolved.
    let mut st = c.prepare("SELECT id, version, onto_name, state, created_by, created_at, resolved_at FROM review_replays WHERE diff_id = ?1 ORDER BY created_at, rowid")?;
    for row in st.query_map([&id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, i64>(5)?, r.get::<_, Option<i64>>(6)?))
    })? {
        let (rid, version, onto, state, by, at, resolved_at) = row?;
        let conflicted = state == "conflicted";
        marks.push(MarkRec {
            id: format!("replay:{rid}"),
            kind: "replay",
            at,
            end: if conflicted { resolved_at } else { Some(at) },
            by,
            revision: Some(version),
            state: if conflicted && resolved_at.is_none() { "conflict" } else { "passed" },
            thread: None,
            note: Some(format!("onto {onto}")),
        });
    }
    // Stable: marks of one second keep the order they were written here.
    marks.sort_by_key(|m| (m.at, mark_rank(m.kind)));
    ch.marks = marks;
    let mut st = c.prepare(
        "SELECT k.file, k.line, k.take IS NOT NULL FROM review_conflicts k JOIN review_replays r ON r.id = k.replay_id
         WHERE r.id = (SELECT p.id FROM review_replays p WHERE p.diff_id = ?1 ORDER BY p.created_at DESC, p.rowid DESC LIMIT 1)
           AND r.state = 'conflicted' AND r.version = (SELECT MAX(v.n) FROM diff_versions v WHERE v.diff_id = ?1)
         ORDER BY k.position",
    )?;
    ch.conflicts = st.query_map([&id], |r| Ok(kiln_ci::review::Conflict { file: r.get(0)?, line: r.get(1)?, taken: r.get(2)? }))?.collect::<Result<_, _>>()?;
    ch.queue = c
        .query_row(
            "SELECT l.state, l.reason FROM land_queue l WHERE l.repo_id = ?1 AND EXISTS (SELECT 1 FROM json_each(CASE WHEN json_valid(l.diffs) THEN l.diffs ELSE '[]' END) j WHERE j.value = ?2) ORDER BY l.updated_at DESC, l.rowid DESC LIMIT 1",
            params![repo_id, id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(Some(ch))
}

/// A host's pull request as a change (W26-R01, R20): from the mirror (`githost::mirror`). Its
/// review state reads its reviews as kiln's would (changes asked by anyone's latest review, else
/// approved by one), its asked reviewers join its turn when it's first seen.
fn load_pull(c: &rusqlite::Connection, repo_id: &str, host: &str, number: i64, n: u32, head: bool) -> anyhow::Result<ChangeRec> {
    let (title, author, state, reviews, threads, said_by, said, merged, first_seen, requested): (String, String, String, String, i64, String, String, String, i64, String) = c.query_row(
        "SELECT title, author, state, reviews, threads, said_by, said, merged_sha, first_seen, requested FROM host_pulls WHERE repo_id = ?1 AND host = ?2 AND number = ?3",
        params![repo_id, host, number],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)),
    )?;
    let reviews: Vec<(String, String, i64, String, Option<u32>)> = serde_json::from_str::<Vec<Value>>(&reviews)
        .unwrap_or_default()
        .iter()
        .map(|r| {
            let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
            let action = match s("verdict").as_str() {
                "approve" => "accept".to_string(),
                v => v.to_string(),
            };
            (s("by"), action, r.get("at").and_then(Value::as_i64).unwrap_or(first_seen), String::new(), None)
        })
        .collect();
    // Each reviewer's latest word decides, as hosts read them.
    let mut latest: HashMap<&str, &str> = HashMap::new();
    for (by, action, _, _, _) in &reviews {
        if action != "comment" {
            latest.insert(by.as_str(), action.as_str());
        }
    }
    let review_state = match state.as_str() {
        "draft" => "draft",
        "merged" => "landed",
        "closed" => "abandoned",
        _ if latest.values().any(|a| *a == "request_changes") => "changes-requested",
        _ if latest.values().any(|a| *a == "accept") => "accepted",
        _ => "needs-review",
    };
    let mut st = c.prepare("SELECT n, sha, seen_at FROM host_pull_heads WHERE repo_id = ?1 AND host = ?2 AND number = ?3 ORDER BY n")?;
    let heads: Vec<(u32, String, i64)> = st.query_map(params![repo_id, host, number], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
    let (latest_n, latest_sha) = heads.last().map(|(k, s, _)| (*k, s.clone())).unwrap_or_default();
    let at = heads.iter().find(|(k, _, _)| *k == n).map_or(first_seen, |(_, _, t)| *t);
    let count = usize::try_from(threads).unwrap_or(0);
    let threads: Vec<ThreadRec> = (0..count).map(|i| ThreadRec { kind: "question".into(), by: if i + 1 == count { said_by.clone() } else { String::new() }, said: if i + 1 == count { said.clone() } else { String::new() }, ..ThreadRec::default() }).collect();
    // A host's pull request's lane marks: a push per head, the asks and the verdicts (the mirror
    // keeps no per-comment times, so no comment marks).
    let mut marks: Vec<MarkRec> = Vec::new();
    let key = format!("pull:{host}:{number}");
    for (k, _, t) in &heads {
        marks.push(MarkRec { id: format!("{key}:v{k}"), kind: "push", at: *t, end: Some(*t), by: author.clone(), revision: Some(*k), state: "passed", thread: None, note: None });
    }
    let asked: Vec<(String, i64)> = serde_json::from_str::<Vec<String>>(&requested).unwrap_or_default().into_iter().map(|r| (r, first_seen)).collect();
    approval_marks(&mut marks, &key, &asked, &reviews);
    marks.sort_by_key(|m| (m.at, mark_rank(m.kind)));
    Ok(ChangeRec {
        name: format!("{}{number}", if host == "gitlab" { "!" } else { "#" }),
        kind: "request",
        host: host.to_string(),
        message: title.clone(),
        title,
        author,
        author_for: None,
        state: review_state.to_string(),
        landed: Some(merged).filter(|m| !m.is_empty()),
        n,
        head,
        latest: latest_n,
        latest_sha,
        at,
        reviewers: asked,
        reviews,
        pushes: heads.iter().map(|(k, _, t)| (*k, false, *t)).collect(),
        threads,
        replies: Vec::new(),
        queue: None,
        // A host's pull request is replayed on its host, not here.
        conflicts: Vec::new(),
        marks,
    })
}

/// A change's review as its tip carries it (W26-R02): whose turn it is folded from its records
/// (`kiln_ci::turn`, W26-R05), its state read from its facts (`kiln_ci::review::state`). An older
/// version's head stands superseded; a change request's other commits, and an abandoned change's,
/// carry none.
fn review_of(ch: &ChangeRec, sha: &str) -> Option<Review> {
    if !ch.head || ch.state == "abandoned" {
        return None;
    }
    let mut ev: Vec<(i64, u8, turn::Event)> = Vec::new();
    ev.extend(ch.reviewers.iter().map(|(w, at)| (*at, 0, turn::Event::Asked(w.clone()))));
    ev.extend(ch.pushes.iter().filter(|(v, _, _)| *v > 1).map(|(_, rebase_only, at)| (*at, 1, turn::Event::Pushed { rebase_only: *rebase_only })));
    ev.extend(ch.replies.iter().map(|(w, at)| (*at, 2, turn::Event::Replied(w.clone()))));
    ev.extend(ch.reviews.iter().map(|(w, action, at, _, _)| {
        let e = match action.as_str() {
            "accept" => turn::Event::Approved(w.clone()),
            "resign" => turn::Event::Removed(w.clone()),
            _ => turn::Event::Replied(w.clone()),
        };
        (*at, 3, e)
    }));
    ev.sort_by_key(|(at, order, _)| (*at, *order));
    let mut events: Vec<turn::Event> = ev.into_iter().map(|(_, _, e)| e).collect();
    match ch.state.as_str() {
        "draft" => events.push(turn::Event::Draft),
        "landed" => events.push(turn::Event::Closed),
        _ => {}
    }
    // An agent's change is its person's to answer.
    let author = ch.author_for.clone().unwrap_or_else(|| ch.author.clone());
    let mut whose: Vec<String> = turn::fold(&ch.author, &events).turn.into_iter().map(|p| if p == ch.author { author.clone() } else { p }).collect();
    whose.sort();
    whose.dedup();
    // The gate (W26-R11), as merging asks it: what's open by kind, and what holds the change, read
    // as accepting and the land queue read them.
    let threads: Vec<kiln_ci::review::Thread> = ch
        .threads
        .iter()
        .map(|t| kiln_ci::review::Thread { kind: Kind::read(&t.kind), by: t.by.clone(), file: t.file.clone(), line: t.line.and_then(|l| u32::try_from(l).ok()), ..kiln_ci::review::Thread::default() })
        .collect();
    let gate = kiln_ci::review::gate(&threads, &ch.conflicts, None);
    let open = Kinds { question: gate.open.question, nit: gate.open.nit, issue: gate.open.issue, blocking: gate.open.blocking };
    let facts = Facts {
        draft: ch.state == "draft",
        changes_asked: ch.state == "changes-requested",
        open: gate.open,
        conflicts: gate.conflicts,
        fixing: false,
        policy_met: matches!(ch.state.as_str(), "accepted" | "landing" | "landed"),
    };
    let state = match kiln_ci::review::state(&facts) {
        Standing::Draft => ReviewState::Draft,
        Standing::Requested => ReviewState::Requested,
        Standing::Changes => ReviewState::Changes,
        Standing::Blocked => ReviewState::Blocked,
        Standing::Fixing => ReviewState::Fixing,
        Standing::Conflict => ReviewState::Conflict,
        Standing::Approved => ReviewState::Approved,
    };
    let said = |t: &ThreadRec| format!("{}: {}", shown(&t.by), first_words(&t.said));
    let about = match state {
        ReviewState::Blocked => ch.threads.iter().rev().find(|t| Kind::read(&t.kind) == Kind::Blocking).map(|t| match (&t.file, t.line) {
            (Some(f), Some(l)) => format!("{}:{l}", f.rsplit('/').next().unwrap_or(f)),
            (Some(f), None) => f.rsplit('/').next().unwrap_or(f).to_string(),
            _ => said(t),
        }),
        ReviewState::Changes => ch.reviews.iter().rev().find(|r| r.1 == "request_changes").map(|r| first_words(&r.3)),
        ReviewState::Conflict => ch.conflicts.first().map(|k| kiln_ci::review::place(Some(&k.file), k.line)),
        ReviewState::Requested => ch.threads.last().map(said),
        _ => None,
    }
    .unwrap_or_default();
    let tip = ch.tip();
    let superseded_by = (sha != tip).then(|| if ch.n < ch.latest { format!("v{}", ch.latest) } else { short(tip) });
    Some(Review { state, turn: whose, author, open, conflicts: gate.conflicts, fixing: None, about, superseded_by })
}

fn short(sha: &str) -> String {
    sha.get(..7).unwrap_or(sha).to_string()
}

fn is_release(pipeline: &str) -> bool {
    pipeline.to_ascii_lowercase().contains("release")
}

/// A pipeline that builds the commit's image (kiln's own `image`): Build's image, not a check.
fn is_image(pipeline: &str) -> bool {
    pipeline.to_ascii_lowercase().contains("image")
}

/// kiln's own failures (a runner lost, the infrastructure, an image pull, a full disk): not the code's.
fn kilns_own(class: Option<&str>) -> bool {
    matches!(class, Some("runner-lost" | "infra" | "image-pull" | "disk-full"))
}

fn attempt_of(status: &str, conclusion: Option<&str>, class: Option<&str>) -> Attempt {
    match (status, conclusion) {
        ("queued" | "waiting" | "pending" | "blocked", _) => Attempt::Queued,
        ("running" | "in_progress", _) => Attempt::Running,
        (_, Some("success" | "neutral" | "skipped")) => Attempt::Passed,
        (_, Some("cancelled")) => Attempt::Cancelled,
        (_, Some(_)) if kilns_own(class) => Attempt::KilnError,
        (_, Some(_)) => Attempt::Failed,
        // v1 says how a job ended in its status, with no conclusion. A job it skipped didn't run
        // (a dependency failed): it's no pass.
        ("succeeded", None) => Attempt::Passed,
        ("failed" | "timed_out", None) => Attempt::Failed,
        ("cancelled" | "skipped", None) => Attempt::Cancelled,
        _ => Attempt::Queued,
    }
}

fn part_of(status: &str, conclusion: Option<&str>) -> Part {
    match attempt_of(status, conclusion, None) {
        Attempt::Queued => Part::Queued,
        Attempt::Running => Part::Running,
        Attempt::Passed | Attempt::Reused => Part::Passed,
        Attempt::Cancelled => Part::Superseded,
        Attempt::Failed | Attempt::KilnError => Part::Failed,
    }
}

const DEPLOY_COLS: &str = "d.environment, (d.production = 1 OR lower(d.environment) = 'production'), d.sha,
     (SELECT o.seq FROM ship_order o WHERE o.repo_id = d.repo_id AND o.item = d.sha),
     (SELECT s.state FROM ci_deployment_statuses s WHERE s.deployment_id = d.id ORDER BY s.id DESC LIMIT 1),
     (SELECT s.description FROM ci_deployment_statuses s WHERE s.deployment_id = d.id ORDER BY s.id DESC LIMIT 1),
     (SELECT s.created_at FROM ci_deployment_statuses s WHERE s.deployment_id = d.id ORDER BY s.id DESC LIMIT 1)";

fn deploy_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeployRec> {
    Ok(DeployRec {
        environment: r.get(0)?,
        production: r.get(1)?,
        sha: r.get(2)?,
        first_row: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
        state: r.get(4)?,
        description: r.get(5)?,
        at: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
    })
}

/// What a repository's commits share, read once per refresh rather than once per commit: what each
/// environment runs now, the newer commit production waits on, and every commit's first run (its
/// place in time) and branch, newest first.
#[derive(Debug, Clone, Default)]
struct RepoCtx {
    id: String,
    name: String,
    current: Vec<DeployRec>,
    waiting: Option<(String, i64)>,
    commits: Vec<(String, i64, String)>,
    /// KilnCD's deployments of its commits, oldest first.
    cd: Vec<DeployRec>,
}

fn repo_ctx(c: &rusqlite::Connection, repo: &str, deploys: &[CdDeploy]) -> anyhow::Result<Option<RepoCtx>> {
    let found: Option<(String, String)> = c.query_row("SELECT id, name FROM repos WHERE name = ?1 OR id = ?1", [repo], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    let Some((id, name)) = found else { return Ok(None) };
    let mut st = c.prepare(&format!(
        "SELECT {DEPLOY_COLS} FROM ci_deployments d WHERE d.repo_id = ?1 AND d.id = (SELECT MAX(e.id) FROM ci_deployments e WHERE e.repo_id = d.repo_id AND e.environment = d.environment
           AND (SELECT s.state FROM ci_deployment_statuses s WHERE s.deployment_id = e.id ORDER BY s.id DESC LIMIT 1) = 'success')"
    ))?;
    let mut current: Vec<DeployRec> = st.query_map([&id], deploy_row)?.collect::<Result<_, _>>()?;
    let waiting = c
        .query_row(
            "SELECT o.item, o.seq FROM deploy_releases d JOIN ship_order o ON o.repo_id = ?1 AND o.item = json_extract(d.spec, '$.version')
             WHERE d.state = 'waiting' AND lower(d.environment) = 'production' AND EXISTS (SELECT 1 FROM runs r WHERE r.repo_id = o.repo_id AND r.sha = o.item)
             ORDER BY o.seq DESC LIMIT 1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let mut st = c.prepare(&format!(
        "SELECT o.item, o.seq, COALESCE((SELECT MIN(r.branch) FROM runs r WHERE r.repo_id = o.repo_id AND r.sha = o.item), '') FROM ship_order o WHERE o.repo_id = ?1 AND {IS_ITEM} ORDER BY o.seq DESC"
    ))?;
    let commits: Vec<(String, i64, String)> = st.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
    // What each environment runs now: its newest successful deployment, CI's or KilnCD's.
    let cd = cd_recs(deploys, &commits);
    for d in cd.iter().filter(|d| d.state.as_deref() == Some("success")) {
        match current.iter_mut().find(|x| x.environment.eq_ignore_ascii_case(&d.environment)) {
            Some(x) if x.at > d.at => {}
            Some(x) => *x = d.clone(),
            None => current.push(d.clone()),
        }
    }
    Ok(Some(RepoCtx { id, name, current, waiting, commits, cd }))
}

/// Reads one commit's records (`sha`: the whole sha, or any unique prefix of seven or more).
fn load(c: &rusqlite::Connection, repo: &str, sha: &str, deploys: &[CdDeploy]) -> anyhow::Result<Option<Records>> {
    let Some(ctx) = repo_ctx(c, repo, deploys)? else { return Ok(None) };
    let found = ctx.commits.iter().find(|(s, _, _)| s == sha || (sha.len() >= 7 && s.starts_with(sha))).map(|(s, f, _)| (s.clone(), *f));
    match found {
        Some((sha, first_row)) => Ok(Some(load_with(c, &ctx, &sha, first_row)?)),
        None => Ok(None),
    }
}

/// One commit's own records; what it shares with its repository comes from `ctx`.
fn load_with(c: &rusqlite::Connection, ctx: &RepoCtx, sha: &str, first_row: i64) -> anyhow::Result<Records> {
    let (repo_id, sha) = (ctx.id.clone(), sha.to_string());
    let mut rec = Records { repo: ctx.name.clone(), sha: sha.clone(), first_row, ..Records::default() };
    let mut st = c.prepare("SELECT id, pipeline, branch, message, author, created_at FROM runs WHERE repo_id = ?1 AND sha = ?2 ORDER BY rowid")?;
    rec.runs = st
        .query_map(params![repo_id, sha], |r| Ok(RunRec { id: r.get(0)?, pipeline: r.get(1)?, branch: r.get(2)?, message: r.get(3)?, actor: r.get(4)?, created_at: r.get(5)? }))?
        .collect::<Result<_, _>>()?;

    // The engine's legs are its run's latest attempt's; v1's jobs (no `job_id`, no `run_attempt`:
    // production's runs, 2026-10-28) are their run's, retried in place.
    let mut st = c.prepare(
        "SELECT j.id, j.run_id, COALESCE(NULLIF(j.display, ''), j.key), j.status, j.conclusion, j.reused_from IS NOT NULL, j.error, j.started_at, j.finished_at
         FROM jobs j JOIN runs r ON r.id = j.run_id WHERE r.repo_id = ?1 AND r.sha = ?2 AND (j.job_id IS NULL OR j.run_attempt = r.attempt) ORDER BY j.rowid",
    )?;
    let legs: Vec<(String, LegRec)> = st
        .query_map(params![repo_id, sha], |r| {
            Ok((r.get::<_, String>(0)?, LegRec { job: r.get(0)?, run_id: r.get(1)?, name: r.get(2)?, status: r.get(3)?, conclusion: r.get(4)?, reused: r.get(5)?, error: r.get(6)?, started: r.get(7)?, finished: r.get(8)?, attempts: Vec::new() }))
        })?
        .collect::<Result<_, _>>()?;
    let mut at = c.prepare("SELECT status, conclusion, failure_class FROM ci_attempts WHERE leg_id = ?1 ORDER BY number")?;
    for (id, mut leg) in legs {
        leg.attempts = at.query_map([&id], |r| Ok(AttemptRec { status: r.get(0)?, conclusion: r.get(1)?, class: r.get(2)? }))?.collect::<Result<_, _>>()?;
        rec.legs.push(leg);
    }

    let mut st = c.prepare("SELECT i.digest FROM ci_images i JOIN runs r ON r.id = i.run_id WHERE r.repo_id = ?1 AND r.sha = ?2 ORDER BY i.id")?;
    rec.images = st.query_map(params![repo_id, sha], |r| r.get(0))?.collect::<Result<_, _>>()?;

    rec.land = c
        .query_row("SELECT state, reason FROM land_queue WHERE repo_id = ?1 AND candidate = ?2 ORDER BY updated_at DESC LIMIT 1", params![repo_id, sha], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;

    // This commit's latest deployment in each environment; what each runs now is the repository's.
    let mut st = c.prepare(&format!("SELECT {DEPLOY_COLS} FROM ci_deployments d WHERE d.repo_id = ?1 AND d.sha = ?2 AND d.id = (SELECT MAX(e.id) FROM ci_deployments e WHERE e.repo_id = d.repo_id AND e.environment = d.environment AND e.sha = d.sha)"))?;
    rec.own = st.query_map(params![repo_id, sha], deploy_row)?.collect::<Result<_, _>>()?;
    for d in ctx.cd.iter().filter(|d| d.sha == sha) {
        match rec.own.iter_mut().find(|x| x.environment.eq_ignore_ascii_case(&d.environment)) {
            Some(x) if x.at > d.at => {}
            Some(x) => *x = d.clone(),
            None => rec.own.push(d.clone()),
        }
    }
    rec.current = ctx.current.clone();
    // Deploy API releases of this commit (by version), and a newer one production waits on.
    let mut st = c.prepare(
        "SELECT id, state, json_extract(spec, '$.message'), json_extract(spec, '$.rolled_back_by') IS NOT NULL FROM deploy_releases
         WHERE lower(environment) = 'production' AND (json_extract(spec, '$.version') = ?1 OR (length(json_extract(spec, '$.version')) >= 7 AND ?1 LIKE json_extract(spec, '$.version') || '%')) ORDER BY id",
    )?;
    rec.releases = st.query_map([&sha], |r| Ok(ReleaseRec { id: r.get(0)?, state: r.get(1)?, message: r.get(2)?, rolled_back: r.get(3)? }))?.collect::<Result<_, _>>()?;
    rec.waiting = ctx.waiting.clone();
    // The commits a production approval would ship: those newer than what production runs, up to this one.
    let prod_row = rec.current.iter().find(|d| d.production).map(|d| d.first_row).unwrap_or(0);
    let branch = rec.runs.first().map(|r| r.branch.clone()).unwrap_or_default();
    rec.riders = u32::try_from(ctx.commits.iter().filter(|(_, f, b)| *b == branch && *f > prod_row && *f <= first_row).count()).unwrap_or(u32::MAX);
    let mut st = c.prepare("SELECT tag_name FROM ci_releases WHERE repo_id = ?1 AND target_commitish = ?2 ORDER BY id")?;
    rec.tags = st.query_map(params![repo_id, sha], |r| r.get(0))?.collect::<Result<_, _>>()?;
    rec.change = load_change(c, &repo_id, &sha)?;
    Ok(rec)
}

/// A commit's records as `kiln_cd::journey` reads them, `now` in ms.
fn journey_of(rec: &Records, runners: &[RunnerSeen], now: i64) -> Journey {
    let first = rec.runs.first();
    let change = rec.change.as_ref();
    let author = match (first, change) {
        (None, Some(ch)) => match &ch.author_for {
            Some(person) => Author::Agent { name: ch.author.clone(), for_person: person.clone() },
            None => Author::Person(ch.author.clone()),
        },
        _ => Author::Person(first.map(|r| r.actor.clone()).unwrap_or_default()),
    };
    // A change's commit is in the queue when an entry lands its change, and not landed until the change is.
    let queue = rec.land.as_ref().or(change.and_then(|ch| ch.queue.as_ref()));
    let land = match queue.map(|(s, why)| (s.as_str(), why)) {
        Some(("testing", _)) => Land::Testing,
        Some(("queued", _)) => Land::Queued { ahead: 0 },
        Some(("ejected", why)) => Land::Ejected { why: why.clone().unwrap_or_default() },
        _ if change.is_some_and(|ch| !ch.is_landed()) => Land::NotYet,
        _ => Land::Landed,
    };
    let run_pipeline: HashMap<&str, &str> = rec.runs.iter().map(|r| (r.id.as_str(), r.pipeline.as_str())).collect();
    let mut checks: Vec<Check> = Vec::new();
    let mut platforms: Vec<Platform> = Vec::new();
    let mut image_parts: Vec<Part> = Vec::new();
    for leg in &rec.legs {
        let pipeline = run_pipeline.get(leg.run_id.as_str()).copied().unwrap_or_default();
        if is_release(pipeline) {
            platforms.push(Platform { name: leg.name.clone(), state: part_of(&leg.status, leg.conclusion.as_deref()) });
            continue;
        }
        if is_image(pipeline) {
            image_parts.push(part_of(&leg.status, leg.conclusion.as_deref()));
            continue;
        }
        let attempts: Vec<Attempt> = if leg.reused {
            vec![Attempt::Reused]
        } else if leg.attempts.is_empty() {
            vec![attempt_of(&leg.status, leg.conclusion.as_deref(), None)]
        } else {
            leg.attempts.iter().map(|a| attempt_of(&a.status, a.conclusion.as_deref(), a.class.as_deref())).collect()
        };
        // A leg names its job and run once it has run (Output streams that job's log).
        let ran = !matches!(leg.status.as_str(), "queued" | "waiting" | "pending" | "blocked" | "skipped");
        let l = Leg { name: leg.name.clone(), attempts, note: leg.error.clone().unwrap_or_default(), job: ran.then(|| leg.job.clone()), run: ran.then(|| leg.run_id.clone()), start: leg.started, end: leg.finished };
        match checks.iter_mut().find(|c| c.pipeline == pipeline) {
            Some(c) => c.legs.push(l),
            None => checks.push(Check { pipeline: pipeline.to_string(), legs: vec![l] }),
        }
    }
    // The image: an image the commit's runs pushed; else what its image pipeline's job says; else
    // under way while a release (the engine's, which pushes it) builds; else nothing built it.
    let release_running = platforms.iter().any(|p| matches!(p.state, Part::Queued | Part::Running));
    let image = if !rec.images.is_empty() {
        Part::Passed
    } else if let Some(p) = [Part::Failed, Part::Running, Part::Queued, Part::Superseded].into_iter().find(|p| image_parts.contains(p)) {
        p
    } else if !image_parts.is_empty() {
        Part::Passed
    } else if release_running {
        Part::Running
    } else {
        Part::NotYet
    };
    let build = Build { platforms, image };

    let checks_ok = !matches!(journey::pips(&Journey { checks: checks.clone(), ..empty() }, now).into_iter().find(|p| p.stage == journey::Stage::Checks).map(|p| p.state), Some(journey::Glyph::Failed));
    let newer = |d: &DeployRec| d.first_row > rec.first_row;
    let own = |prod: bool| rec.own.iter().find(|d| d.production == prod && (prod || lower_is(&d.environment, "staging") || !rec.own.iter().any(|x| !x.production && lower_is(&x.environment, "staging"))));
    let current = |prod: bool| rec.current.iter().find(|d| d.production == prod && (prod || lower_is(&d.environment, "staging") || !rec.current.iter().any(|x| !x.production && lower_is(&x.environment, "staging"))));

    let staging = match (own(false), current(false)) {
        _ if !checks_ok => Staging::Skipped,
        (Some(d), cur) => match d.state.as_deref() {
            Some("success") if cur.map(|c| c.sha == rec.sha).unwrap_or(false) => Staging::Healthy,
            Some("success" | "inactive") => Staging::Moved,
            Some("queued" | "pending" | "in_progress") => Staging::Syncing,
            _ => Staging::NotYet,
        },
        (None, Some(cur)) if newer(cur) => Staging::Rode { with: short(&cur.sha) },
        _ => Staging::NotYet,
    };

    let cur_prod = current(true);
    let in_production = cur_prod.map(|c| c.sha == rec.sha).unwrap_or(false);
    let waiting_release = rec.releases.iter().any(|r| r.state == "waiting");
    let rolled = rec.releases.iter().rev().find(|r| r.rolled_back || r.state == "failed");
    let production = if waiting_release {
        Production::WaitsForApproval { commits: rec.riders.max(1) }
    } else if let Some(r) = rolled.filter(|r| r.rolled_back) {
        Production::RolledBack { to: cur_prod.map(|c| short(&c.sha)).unwrap_or_default(), why: r.message.clone().unwrap_or_default(), acknowledged: false }
    } else if let Some(d) = own(true) {
        let at = d.at.saturating_mul(1000);
        match d.state.as_deref() {
            Some("queued" | "pending" | "in_progress") => Production::Delivering,
            Some("success") if in_production && now < at.saturating_add(GUARD_MS) => Production::Guard { since: at, until: at.saturating_add(GUARD_MS) },
            Some("success") if in_production => Production::Runs,
            Some("success" | "inactive") => Production::Moved,
            Some("failure" | "error") => Production::RolledBack { to: cur_prod.map(|c| short(&c.sha)).unwrap_or_default(), why: d.description.clone().unwrap_or_default(), acknowledged: false },
            _ => Production::NotYet,
        }
    } else {
        match (cur_prod, &rec.waiting) {
            (Some(cur), _) if newer(cur) && checks_ok => Production::Rode { with: short(&cur.sha) },
            (_, Some((sha, row))) if *row > rec.first_row && checks_ok && cur_prod.map(|c| c.first_row < rec.first_row).unwrap_or(true) => Production::Rides { with: short(sha) },
            _ => Production::NotYet,
        }
    };

    let replant = in_production.then(|| Replant {
        server: Part::Passed,
        runners: runners
            .iter()
            .map(|r| {
                let on = r.commit.as_deref().map(|c| rec.sha.starts_with(c) || c.starts_with(&rec.sha)).unwrap_or(false);
                let state = if r.failed.is_some() {
                    Runner::Failed
                } else if !r.connected {
                    Runner::Offline
                } else if on {
                    Runner::On
                } else {
                    Runner::Waiting
                };
                RunnerReplant { name: r.name.clone(), state, why: r.failed.clone().unwrap_or_default() }
            })
            .collect(),
    });

    let review = change.and_then(|ch| review_of(ch, &rec.sha));
    Journey { author, land, checks, build, staging, production, replant, in_production, handled_by: None, fixed_by: None, review }
}

fn lower_is(s: &str, want: &str) -> bool {
    s.eq_ignore_ascii_case(want)
}

fn empty() -> Journey {
    Journey {
        author: Author::Person(String::new()),
        land: Land::Landed,
        checks: Vec::new(),
        build: Build { platforms: Vec::new(), image: Part::Passed },
        staging: Staging::NotYet,
        production: Production::NotYet,
        replant: None,
        in_production: false,
        handled_by: None,
        fixed_by: None,
        review: None,
    }
}

/// What a person can do about a commit from its row, each through the endpoint kiln has for it
/// (nothing new decides): approve or hold the production release waiting on them (the Deploy
/// API's approval); retry a failed check (the run's rerun, its failed legs only) or hand it to an
/// agent (an attachment: an agent looking after the run); retry a runner that didn't replant (its
/// upgrade, again).
fn actions(rec: &Records, j: &Journey, yours: bool) -> Vec<Value> {
    let act = |name: &str, method: &str, path: String, body: Value| json!({ "name": name, "method": method, "path": path, "body": body });
    let mut out = Vec::new();
    if let Some(r) = rec.releases.iter().find(|r| r.state == "waiting") {
        out.push(act("approve", "POST", format!("/api/deploy/releases/{}/approve", r.id), json!({})));
        out.push(act("hold", "POST", format!("/api/deploy/releases/{}/reject", r.id), json!({})));
    }
    let failed = rec.legs.iter().find(|l| matches!(attempt_of(&l.status, l.conclusion.as_deref(), None), Attempt::Failed) && !is_release(rec.runs.iter().find(|r| r.id == l.run_id).map(|r| r.pipeline.as_str()).unwrap_or_default()));
    if let Some(leg) = failed {
        out.push(act("retry", "POST", format!("/api/ci/runs/{}/rerun", leg.run_id), json!({ "mode": "failed" })));
        out.push(act("hand_off", "POST", "/api/attachments".to_string(), json!({ "repo": rec.repo, "run": leg.run_id })));
    }
    if let (true, Some(r)) = (j.in_production, j.replant.as_ref()) {
        for x in r.runners.iter().filter(|x| x.state == Runner::Failed) {
            out.push(act("retry_runner", "POST", format!("/api/updates/runners/{}", x.name), Value::Null));
        }
    }
    // A change whose turn is yours: hand it to an agent, or review it (W26-R02's Hand off and Review pair).
    if let (true, Some(ch), Some(r)) = (yours, rec.change.as_ref(), j.review.as_ref())
        && r.superseded_by.is_none()
    {
        if !out.iter().any(|a| a.get("name").and_then(Value::as_str) == Some("hand_off")) {
            out.push(act("hand_off", "POST", "/api/attachments".to_string(), json!({ "repo": rec.repo, "diff": ch.name })));
        }
        out.push(act("review", "GET", format!("/api/diffs/{}", ch.name), Value::Null));
    }
    out
}

/// A commit's kind for the facets: its change's (`diff`, `request`), else `commit`.
fn kind_of(rec: &Records) -> &'static str {
    rec.change.as_ref().map_or("commit", |ch| ch.kind)
}

/// Who a commit under review is theirs to act on (`journey::in_turn_of`): its turn, and its author
/// when it's theirs to answer (changes asked, a blocking thread, a conflict, its checks failed).
fn whose(j: &Journey) -> Vec<String> {
    let Some(r) = j.review.as_ref().filter(|r| r.superseded_by.is_none()) else { return Vec::new() };
    let mut out: Vec<String> = r.turn.iter().chain(std::iter::once(&r.author)).filter(|p| journey::in_turn_of(j, p)).cloned().collect();
    out.sort();
    out.dedup();
    out
}

/// The facets a list is filtered by (W26-R02): the change's host, the kind, the Review lens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facets {
    pub host: Option<String>,
    pub kind: Option<String>,
    pub review: bool,
}

impl Facets {
    pub fn from_query(q: &HashMap<String, String>) -> Self {
        let word = |k: &str| q.get(k).map(|v| v.trim().to_ascii_lowercase()).filter(|v| !v.is_empty());
        Self { host: word("host"), kind: word("kind"), review: word("lens").as_deref() == Some("review") }
    }

    fn matches(&self, kind: &str, host: Option<&str>) -> bool {
        self.kind.as_deref().is_none_or(|k| k == kind) && self.host.as_deref().is_none_or(|h| host == Some(h)) && (!self.review || kind != "commit")
    }
}

/// Whether a viewer (their principals) holds a row's turn.
fn holds(whose: &[String], viewer: Option<&HashSet<String>>) -> bool {
    viewer.is_some_and(|v| whose.iter().any(|p| v.contains(p)))
}

/// A commit as the list shows it, for whoever holds its turn (`yours`) or anyone else.
fn summary_of(rec: &Records, j: &Journey, now: i64, yours: bool) -> Value {
    let s = journey::standing_as(j, now, yours);
    let first = rec.runs.first();
    let ch = rec.change.as_ref();
    let envs: Vec<Value> = rec.current.iter().filter(|d| d.sha == rec.sha).map(|d| json!({ "environment": d.environment, "production": d.production })).collect();
    let message = match (first, ch) {
        (Some(r), _) => r.message.lines().next().unwrap_or_default().to_string(),
        (None, Some(ch)) if ch.head && ch.kind == "request" => ch.title.clone(),
        (None, Some(ch)) => ch.message.lines().next().filter(|l| !l.is_empty()).map_or_else(|| ch.title.clone(), str::to_string),
        (None, None) => String::new(),
    };
    let change = match (ch, j.review.as_ref()) {
        (Some(ch), r) => json!({
            "name": ch.name, "kind": ch.kind, "host": ch.host, "title": ch.title, "revision": ch.n, "latest": ch.latest,
            "state": r.map(|r| r.state), "turn": r.map(|r| r.turn.clone()).unwrap_or_default(),
            "open": r.map(|r| json!({ "question": r.open.question, "nit": r.open.nit, "issue": r.open.issue, "blocking": r.open.blocking })),
            "superseded_by": r.and_then(|r| r.superseded_by.clone()),
        }),
        (None, _) => Value::Null,
    };
    let row = json!({
        "repo": rec.repo, "sha": rec.sha, "short": short(&rec.sha),
        "message": message,
        "author": first.map(|r| r.actor.clone()).or_else(|| ch.map(|c| c.author.clone())).unwrap_or_default(),
        "branch": first.map(|r| r.branch.clone()).unwrap_or_default(),
        "at": first.map(|r| r.created_at).or_else(|| ch.map(|c| c.at)).unwrap_or_default(),
        "stand": s.stand, "line": s.line, "pips": s.pips,
        "environments": envs, "tags": rec.tags,
        "runs": rec.runs.iter().map(|r| json!({ "id": r.id, "pipeline": r.pipeline })).collect::<Vec<_>>(),
        "cursor": rec.first_row,
        "kind": kind_of(rec), "host": ch.map(|c| c.host.clone()), "change": change,
        "fold_under": ch.map(ChangeRec::tip).filter(|tip| *tip != rec.sha),
        "actions": actions(rec, j, yours),
    });
    with_version(row)
}

/// The row with `v`: a hash of the JSON it shows (FNV-1a, 64 bits, in hex), the same from memory
/// and from the store, so the page can keep a row that hasn't changed as it is (W26-R06).
fn with_version(mut row: Value) -> Value {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in row.to_string().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    if let Some(o) = row.as_object_mut() {
        o.insert("v".into(), json!(format!("{h:016x}")));
    }
    row
}

/// A commit rendered once for everyone and once for those whose turn it is (when anyone's).
struct Rendered {
    others: Value,
    yours: Option<Value>,
    whose: Vec<String>,
}

fn render(rec: &Records, runners: &[RunnerSeen], now: i64) -> Rendered {
    let j = journey_of(rec, runners, now);
    let whose = whose(&j);
    let yours = (!whose.is_empty()).then(|| summary_of(rec, &j, now, true));
    Rendered { others: summary_of(rec, &j, now, false), yours, whose }
}

/// A leg as the detail shows it; `job` and `run` only once it has run (optional new fields).
fn leg_json(l: &journey::Leg) -> Value {
    let mut v = json!({ "name": l.name, "state": journey::leg_glyph(l), "attempts": l.attempts.len(), "note": l.note });
    if let Some(o) = v.as_object_mut() {
        if let Some(job) = &l.job {
            o.insert("job".into(), json!(job));
        }
        if let Some(run) = &l.run {
            o.insert("run".into(), json!(run));
        }
        if let Some(start) = l.start {
            o.insert("start".into(), json!(start));
        }
        if let Some(end) = l.end {
            o.insert("end".into(), json!(end));
        }
    }
    v
}

/// A commit with every record its stand came from, as `viewer` sees it.
fn detail(rec: &Records, runners: &[RunnerSeen], now: i64, viewer: Option<&HashSet<String>>) -> Value {
    let j = journey_of(rec, runners, now);
    let mut v = summary_of(rec, &j, now, holds(&whose(&j), viewer));
    let checks: Vec<Value> = j
        .checks
        .iter()
        .map(|c| json!({ "pipeline": c.pipeline, "legs": c.legs.iter().map(leg_json).collect::<Vec<_>>() }))
        .collect();
    let part = |p: Part| match p {
        Part::Queued => "queued",
        Part::Running => "running",
        Part::Passed => "passed",
        Part::Failed => "failed",
        Part::Superseded => "superseded",
        Part::NotYet => "none",
    };
    let build = json!({
        "platforms": j.build.platforms.iter().map(|p| json!({ "name": p.name, "state": part(p.state) })).collect::<Vec<_>>(),
        "image": { "state": part(j.build.image), "digest": rec.images.last() },
    });
    let deployments: Vec<Value> = rec.own.iter().map(|d| json!({ "environment": d.environment, "production": d.production, "state": d.state, "description": d.description, "at": d.at })).collect();
    let replant: Vec<Value> = j.replant.as_ref().map(|r| r.runners.iter().map(|x| json!({ "name": x.name, "state": format!("{:?}", x.state).to_lowercase(), "why": x.why })).collect()).unwrap_or_default();
    // The Review lane group's marks (W26-R14), oldest first; a commit with no change has the one:
    // its push.
    let mut marks = rec.change.as_ref().map(|c| c.marks.clone()).unwrap_or_default();
    if rec.change.is_none() {
        let at = rec.runs.first().map(|r| r.created_at).unwrap_or_default();
        marks.push(MarkRec { id: format!("push:{}", rec.sha), kind: "push", at, end: Some(at), by: rec.runs.first().map(|r| r.actor.clone()).unwrap_or_default(), revision: None, state: "passed", thread: None, note: None });
    }
    if let Some(o) = v.as_object_mut() {
        o.insert("review".into(), json!(marks.iter().map(mark_json).collect::<Vec<_>>()));
        o.insert("checks".into(), json!(checks));
        o.insert("build".into(), build);
        o.insert("deployments".into(), json!(deployments));
        o.insert("releases".into(), json!(rec.releases.iter().map(|r| json!({ "state": r.state, "message": r.message, "rolled_back": r.rolled_back })).collect::<Vec<_>>()));
        o.insert("replant".into(), json!(replant));
    }
    v
}

/// A repository's commits, newest first, a page at a time (`cursor`: the last page's last
/// `cursor`), of the repositories named in `only` (JSON, those the caller may read), or all.
pub async fn list_in(store: &crate::store::Store, q: &HashMap<String, String>, only: Option<String>, viewer: Option<&HashSet<String>>, runners: Vec<RunnerSeen>, deploys: &[CdDeploy], now: i64) -> Result<Value, Fail> {
    let deploys = deploys.to_vec();
    let limit = q.get("limit").and_then(|l| l.parse::<i64>().ok()).unwrap_or(30).clamp(1, 100);
    let cursor = q.get("cursor").and_then(|c| c.parse::<i64>().ok()).unwrap_or(i64::MAX);
    let repo = q.get("repo").filter(|v| !v.is_empty()).cloned();
    let f = Facets::from_query(q);
    let recs = store
        .call(move |c| {
            let (kind, host) = facet_sql();
            let mut st = c.prepare(&format!(
                "SELECT p.name, o.item, o.seq FROM ship_order o JOIN repos p ON p.id = o.repo_id
                 WHERE (?1 IS NULL OR p.name = ?1 OR p.id = ?1) AND (?4 IS NULL OR p.name IN (SELECT value FROM json_each(?4)))
                   AND o.seq < ?2 AND {IS_ITEM}
                   AND (?5 IS NULL OR {kind} = ?5) AND (?6 IS NULL OR {host} = ?6) AND (?7 = 0 OR {kind} <> 'commit')
                 ORDER BY o.seq DESC LIMIT ?3",
            ))?;
            let keys: Vec<(String, String)> = st.query_map(params![repo, cursor, limit + 1, only, f.kind, f.host, f.review], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
            let mut out = Vec::new();
            for (repo, sha) in keys {
                if let Some(rec) = load(c, &repo, &sha, &deploys)? {
                    out.push(rec);
                }
            }
            Ok(out)
        })
        .await
        .map_err(internal)?;
    let more = recs.len() > usize::try_from(limit).unwrap_or(usize::MAX);
    let page: Vec<Value> = recs
        .iter()
        .take(usize::try_from(limit).unwrap_or(30))
        .map(|r| {
            let x = render(r, &runners, now);
            if holds(&x.whose, viewer) { x.yours.unwrap_or(x.others) } else { x.others }
        })
        .collect();
    let next = if more { page.last().and_then(|c| c.get("cursor")).cloned() } else { None };
    Ok(json!({ "commits": page, "next_cursor": next }))
}

/// One commit of a repository, with the records its stand came from.
pub async fn get_in(store: &crate::store::Store, repo: &str, sha: &str, viewer: Option<&HashSet<String>>, runners: Vec<RunnerSeen>, deploys: &[CdDeploy], now: i64) -> Result<Value, Fail> {
    let (repo, sha, deploys) = (repo.to_owned(), sha.to_owned(), deploys.to_vec());
    let rec = store.call(move |c| load(c, &repo, &sha, &deploys)).await.map_err(internal)?;
    rec.map(|r| detail(&r, &runners, now, viewer)).ok_or_else(|| (StatusCode::NOT_FOUND, "no such commit".to_string()))
}


// ---- In memory: a request reads memory only (plan step 5) ---------------------------------------
//
// The speed costs nothing in correctness: what memory serves is what the store says (a refresh
// reloads every commit whose records changed, deleted ones included, and drops the commits and
// repositories gone); no change is lost (the triggers note inserts, updates and deletes of every
// table read, one row per commit with its seq moving forward, and a row is pruned only once a
// refresh that read it is applied); refreshes run one at a time and the applied seq never goes
// back; memory holds at most `cap` commits per repository, older pages coming from the store; and
// a failed refresh keeps the last good model, retried on the next read.

/// A repository's newest commits held in memory: each one's records and its list JSON already
/// rendered, their order newest first (each one's place in ship_order, item), and how many it has.
#[derive(Debug, Default)]
struct HeldRepo {
    ctx: RepoCtx,
    order: Vec<(i64, String)>,
    rows: HashMap<String, Row>,
    total: usize,
}

/// A commit held: its records, its list JSON for everyone and for those whose turn it is, who
/// they are, and its facets.
#[derive(Debug)]
struct Row {
    rec: Records,
    others: String,
    yours: Option<String>,
    whose: Vec<String>,
    kind: &'static str,
    host: Option<String>,
}

impl Row {
    fn of(rec: Records, runners: &[RunnerSeen], now: i64) -> Result<Self, Fail> {
        let x = render(&rec, runners, now);
        let text = |v: &Value| serde_json::to_string(v).map_err(|e| internal(e.into()));
        let (others, yours) = (text(&x.others)?, x.yours.as_ref().map(text).transpose()?);
        let (kind, host) = (kind_of(&rec), rec.change.as_ref().map(|c| c.host.clone()));
        Ok(Self { rec, others, yours, whose: x.whose, kind, host })
    }

    fn json(&self, viewer: Option<&HashSet<String>>) -> &str {
        match &self.yours {
            Some(y) if holds(&self.whose, viewer) => y,
            _ => &self.others,
        }
    }
}

#[derive(Debug, Default)]
struct Held {
    seq: i64,
    built: bool,
    at_ms: i64,
    repos: HashMap<String, HeldRepo>,
    /// KilnCD's deployments as last read ([`cd_mark`]).
    cd: u64,
}

/// What a refresh read from the store, in one snapshot.
struct Read {
    top: i64,
    loaded: Vec<(RepoCtx, Vec<Records>, bool, Vec<String>)>,
    gone_repos: Vec<String>,
}

/// Ship's read model in memory.
#[derive(Debug)]
pub struct ShipCache {
    held: std::sync::RwLock<Held>,
    refreshing: std::sync::atomic::AtomicBool,
    cap: usize,
}

impl Default for ShipCache {
    fn default() -> Self {
        Self::with_cap(CAP)
    }
}

/// How old the model may be before a read brings it up to date.
const FRESH_MS: i64 = 250;
/// The most commits held per repository; older pages come from the store.
const CAP: usize = 5000;

fn poisoned() -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, "ship's read model lock was poisoned".to_string())
}

/// Lets the next refresh start when this one ends, however it ends.
struct Flight<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

impl ShipCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cap(cap: usize) -> Self {
        Self { held: std::sync::RwLock::new(Held::default()), refreshing: std::sync::atomic::AtomicBool::new(false), cap: cap.max(1) }
    }

    /// Whether the model is older than [`FRESH_MS`] (or not built yet).
    pub fn stale(&self, now: i64) -> bool {
        self.held.read().map(|h| !h.built || now - h.at_ms >= FRESH_MS).unwrap_or(true)
    }

    pub fn built(&self) -> bool {
        self.held.read().map(|h| h.built).unwrap_or(false)
    }

    /// The last change applied (tests: it never goes back).
    pub fn applied(&self) -> i64 {
        self.held.read().map(|h| h.seq).unwrap_or(0)
    }

    /// Brings the model up to date: every commit the first time, then those whose records
    /// changed since, the gone dropped. One at a time: while one runs, another returns at once
    /// (its reader gets the model as it is, at most [`FRESH_MS`] old). A failure leaves the last
    /// good model, and the next read tries again.
    pub async fn refresh(&self, store: &crate::store::Store, runners: &[RunnerSeen], deploys: &[CdDeploy], now: i64) -> Result<(), Fail> {
        use std::sync::atomic::Ordering;
        if self.refreshing.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let _flight = Flight(&self.refreshing);
        // KilnCD's deployments aren't the store's: when they changed, every commit is read again.
        let cd = cd_mark(deploys);
        let (built, seq) = {
            let h = self.held.read().map_err(|_| poisoned())?;
            (h.built && h.cd == cd, h.seq)
        };
        let deploys = deploys.to_vec();
        let cap = self.cap;
        let waiting: HashMap<String, Vec<String>> = {
            let h = self.held.read().map_err(|_| poisoned())?;
            h.repos.values().map(|r| (r.ctx.id.clone(), r.rows.iter().filter(|(_, row)| row.rec.releases.iter().any(|x| x.state == "waiting")).map(|(s, _)| s.clone()).collect())).collect()
        };
        let read = store
            .call(move |c| -> anyhow::Result<Read> {
                // One snapshot: the seq read and every record loaded agree.
                let tx = c.savepoint()?;
                let top: i64 = tx.query_row("SELECT n FROM ship_seq", [], |r| r.get(0))?;
                let mut want: HashMap<String, (bool, std::collections::HashSet<String>)> = HashMap::new();
                if !built {
                    let mut st = tx.prepare("SELECT id FROM repos")?;
                    for id in st.query_map([], |r| r.get::<_, String>(0))? {
                        want.insert(id?, (true, Default::default()));
                    }
                } else {
                    let mut st = tx.prepare("SELECT repo_id, item FROM ship_changes WHERE seq > ?1 AND seq <= ?2")?;
                    for row in st.query_map(params![seq, top], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
                        let (repo, sha) = row?;
                        let e = want.entry(repo).or_default();
                        if sha.is_empty() {
                            e.0 = true;
                        } else {
                            e.1.insert(sha);
                        }
                    }
                }
                let mut read = Read { top, loaded: Vec::new(), gone_repos: Vec::new() };
                for (repo, (mut all, mut shas)) in want {
                    let Some(ctx) = repo_ctx(&tx, &repo, &deploys)? else {
                        read.gone_repos.push(repo);
                        continue;
                    };
                    // What other commits are read against (what each environment runs, the
                    // commit production waits on) changing: every commit of the repository.
                    all |= shas.iter().any(|s| ctx.current.iter().any(|d| &d.sha == s) || ctx.waiting.as_ref().is_some_and(|(w, _)| w == s));
                    // A commit waiting on approval counts the commits riding with it.
                    shas.extend(waiting.get(&ctx.id).into_iter().flatten().cloned());
                    let newest: std::collections::HashSet<&str> = ctx.commits.iter().take(cap).map(|(s, _, _)| s.as_str()).collect();
                    let mut recs = Vec::new();
                    for (sha, first_row, _) in ctx.commits.iter().take(cap).filter(|(s, _, _)| all || shas.contains(s)) {
                        recs.push(load_with(&tx, &ctx, sha, *first_row)?);
                    }
                    // A commit noted but no longer among the newest held (or gone): dropped.
                    let drop: Vec<String> = shas.into_iter().filter(|s| !newest.contains(s.as_str())).collect();
                    read.loaded.push((ctx, recs, all, drop));
                }
                tx.commit()?;
                // What this read has seen may go: a row changed since has a later seq and stays.
                c.execute("DELETE FROM ship_changes WHERE seq <= ?1", [top])?;
                Ok(read)
            })
            .await
            .map_err(internal)?;
        let mut h = self.held.write().map_err(|_| poisoned())?;
        if read.top < h.seq {
            return Ok(());
        }
        h.repos.retain(|_, r| !read.gone_repos.contains(&r.ctx.id));
        for (ctx, recs, all, drop) in read.loaded {
            // A repository renamed: its commits move with it.
            let old = h.repos.iter().find(|(name, r)| r.ctx.id == ctx.id && **name != ctx.name).map(|(name, _)| name.clone());
            if let Some(r) = old.and_then(|old| h.repos.remove(&old)) {
                h.repos.insert(ctx.name.clone(), r);
            }
            let held = h.repos.entry(ctx.name.clone()).or_default();
            if all {
                held.rows.clear();
            }
            for sha in drop {
                held.rows.remove(&sha);
            }
            for rec in recs {
                held.rows.insert(rec.sha.clone(), Row::of(rec, runners, now)?);
            }
            let newest: Vec<(i64, String)> = ctx.commits.iter().take(cap).map(|(s, f, _)| (*f, s.clone())).collect();
            held.rows.retain(|sha, _| newest.iter().any(|(_, s)| s == sha));
            held.order = newest.into_iter().filter(|(_, s)| held.rows.contains_key(s)).collect();
            held.total = ctx.commits.len();
            held.ctx = ctx;
        }
        // What time and the runners move without a record changing: the guard and the soak run
        // down, and the commit production runs replants. Those few are drawn again.
        for held in h.repos.values_mut() {
            let prod = held.ctx.current.iter().find(|d| d.production).map(|d| d.sha.clone());
            for (sha, row) in held.rows.iter_mut() {
                if prod.as_deref() == Some(sha.as_str()) || row.others.contains("\"guard\"") || row.others.contains("\"soaking\"") {
                    *row = Row::of(std::mem::take(&mut row.rec), runners, now)?;
                }
            }
        }
        h.seq = read.top;
        h.built = true;
        h.at_ms = now;
        h.cd = cd;
        Ok(())
    }

    /// A page of commits, newest first, from memory: of `repo` (name or id) or every repository,
    /// of those named in `only`, matching `facets`, before `cursor`, at most `limit`, each as
    /// `viewer` sees it. None when memory can't answer it whole (not built, or the page reaches
    /// past the commits held): the store answers then.
    pub fn page(&self, repo: Option<&str>, only: Option<&[String]>, facets: &Facets, viewer: Option<&HashSet<String>>, cursor: i64, limit: usize) -> Result<Option<String>, Fail> {
        let h = self.held.read().map_err(|_| poisoned())?;
        if !h.built {
            return Ok(None);
        }
        let mut picked: Vec<(i64, &str)> = Vec::new();
        let repos: Vec<(&String, &HeldRepo)> = h.repos.iter().filter(|(name, r)| repo.is_none_or(|x| x == name.as_str() || x == r.ctx.id) && only.is_none_or(|o| o.iter().any(|n| n == *name))).collect();
        for (_, r) in &repos {
            // Newest first already: skip to the cursor, take what a page needs.
            let from = r.order.partition_point(|(f, _)| *f >= cursor);
            let before = picked.len();
            for (f, sha) in r.order.iter().skip(from) {
                if picked.len() - before > limit {
                    break;
                }
                if let Some(row) = r.rows.get(sha).filter(|row| facets.matches(row.kind, row.host.as_deref())) {
                    picked.push((*f, row.json(viewer)));
                }
            }
            // Fewer than a page held past the cursor, and the repository has more: the store's.
            if picked.len() - before < limit + 1 && r.order.len() < r.total {
                return Ok(None);
            }
        }
        if repos.len() > 1 {
            picked.sort_unstable_by_key(|p| std::cmp::Reverse(p.0));
        }
        let more = picked.len() > limit;
        let page = picked.get(..limit.min(picked.len())).unwrap_or_default();
        let mut out = String::with_capacity(page.iter().map(|(_, j)| j.len() + 1).sum::<usize>() + 48);
        out.push_str("{\"commits\":[");
        for (i, (_, json)) in page.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(json);
        }
        out.push_str("],\"next_cursor\":");
        match page.last().filter(|_| more) {
            Some((f, _)) => out.push_str(&f.to_string()),
            None => out.push_str("null"),
        }
        out.push('}');
        Ok(Some(out))
    }

    /// One commit's detail from memory (`sha`: whole, or a prefix of seven or more), if held.
    pub fn detail(&self, repo: &str, sha: &str, runners: &[RunnerSeen], now: i64, viewer: Option<&HashSet<String>>) -> Option<Value> {
        let h = self.held.read().ok()?;
        if !h.built {
            return None;
        }
        let r = h.repos.iter().find(|(name, r)| name.as_str() == repo || r.ctx.id == repo).map(|(_, r)| r)?;
        let rec = r.rows.get(sha).map(|row| &row.rec).or_else(|| if sha.len() >= 7 { r.rows.iter().find(|(s, _)| s.starts_with(sha)).map(|(_, row)| &row.rec) } else { None })?;
        Some(detail(rec, runners, now, viewer))
    }
}

static CACHE: std::sync::OnceLock<ShipCache> = std::sync::OnceLock::new();

fn cache() -> &'static ShipCache {
    CACHE.get_or_init(ShipCache::new)
}

/// The runners as the updates know them, for the commit production runs.
async fn runners_seen(hub: &std::sync::Arc<Hub>) -> Vec<RunnerSeen> {
    let jobs = hub.updates.jobs();
    crate::updates::runners(hub)
        .await
        .into_iter()
        .map(|r| {
            let failed = jobs
                .iter()
                .rev()
                .find(|j| j.kind == "runner" && j.target == r.name)
                .filter(|j| matches!(j.state, crate::updates::JobState::Failed))
                .map(|j| j.error.clone().unwrap_or_else(|| "the upgrade failed".to_string()));
            RunnerSeen { name: r.name, connected: r.connected, commit: r.commit, failed }
        })
        .collect()
}

/// `GET /api/ship/commits?…`: from memory, brought up to date first when it's older than
/// [`FRESH_MS`]; from the store when memory can't answer the page whole.
pub async fn list(hub: &std::sync::Arc<Hub>, q: &HashMap<String, String>, caller: Option<&crate::authz::Caller>) -> Result<Response, Fail> {
    let only: Option<Vec<String>> = match caller {
        Some(c) => crate::authz::gate::readable(hub, c, "runs.read").await,
        None => None,
    };
    let now = crate::util::now_ms();
    let c = cache();
    let runners = runners_seen(hub).await;
    let deploys = cd_deploys();
    if c.stale(now)
        && let Err((_, e)) = c.refresh(&hub.store, &runners, &deploys, now).await
    {
        tracing::warn!("ship: the read model kept its last good state: {e}");
    }
    let limit = q.get("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(30).clamp(1, 1000);
    let cursor = q.get("cursor").and_then(|c| c.parse::<i64>().ok()).unwrap_or(i64::MAX);
    let viewer = match caller {
        Some(c) => Some(principals(hub, c).await),
        None => None,
    };
    match c.page(q.get("repo").map(String::as_str).filter(|r| !r.is_empty()), only.as_deref(), &Facets::from_query(q), viewer.as_ref(), cursor, limit)? {
        Some(body) => Ok(srv::response(StatusCode::OK, "application/json", srv::full(body))),
        None => {
            let only = only.map(|names| serde_json::to_string(&names).unwrap_or_else(|_| "[]".into()));
            Ok(srv::json(StatusCode::OK, &list_in(&hub.store, q, only, viewer.as_ref(), runners, &deploys, now).await?))
        }
    }
}

/// Who a caller is, as a change's turn names principals: itself, the person an agent acts for (an
/// agent counts as its person), and the hosts' accounts its person linked (`github:<login>`).
async fn principals(hub: &Hub, c: &crate::authz::Caller) -> HashSet<String> {
    let (name, _) = crate::ci::diffs::author_of(&c.name, Some(c.kind));
    let mut out: HashSet<String> = [Some(c.name.clone()), Some(name), crate::ci::diffs::acting_for(&c.name)].into_iter().flatten().filter(|p| !p.is_empty()).collect();
    if let Some(user) = c.user.clone() {
        let linked: Vec<String> = hub
            .store
            .call(move |db| {
                let mut q = db.prepare("SELECT provider, login FROM user_identities WHERE user_id = ?1")?;
                let rows = q.query_map([&user], |r| Ok(format!("{}:{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .await
            .unwrap_or_default();
        out.extend(linked);
    }
    out
}

/// Whether a commit's JSON is of a repository the caller may read (`only`: the names it may, or
/// None for every one): one it may not is as missing as one that isn't there.
fn visible(v: &Value, only: Option<&[String]>) -> bool {
    only.is_none_or(|names| v.get("repo").and_then(Value::as_str).is_some_and(|repo| names.iter().any(|n| n == repo)))
}

/// `GET /api/ship/commits/:repo/:sha`: from memory, else from the store; of a repository the
/// caller may read, else not found.
pub async fn get(hub: &std::sync::Arc<Hub>, repo: &str, sha: &str, caller: Option<&crate::authz::Caller>) -> Result<Response, Fail> {
    let only: Option<Vec<String>> = match caller {
        Some(c) => crate::authz::gate::readable(hub, c, "runs.read").await,
        None => None,
    };
    let now = crate::util::now_ms();
    let c = cache();
    let runners = runners_seen(hub).await;
    let deploys = cd_deploys();
    if c.stale(now)
        && let Err((_, e)) = c.refresh(&hub.store, &runners, &deploys, now).await
    {
        tracing::warn!("ship: the read model kept its last good state: {e}");
    }
    let viewer = match caller {
        Some(c) => Some(principals(hub, c).await),
        None => None,
    };
    let v = match c.detail(repo, sha, &runners, now, viewer.as_ref()) {
        Some(v) => v,
        None => get_in(&hub.store, repo, sha, viewer.as_ref(), runners, &deploys, now).await?,
    };
    if !visible(&v, only.as_deref()) {
        return Err((StatusCode::NOT_FOUND, "no such commit".to_string()));
    }
    Ok(srv::json(StatusCode::OK, &v))
}

#[cfg(test)]
mod tests {
    use super::*;

    const C1: &str = "1111111aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const C2: &str = "2222222bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C3: &str = "3333333ccccccccccccccccccccccccccccccccc";
    const NOW_S: i64 = 1_800_000_000;

    /// kiln's own repository, three commits on main: the oldest in production with every runner
    /// replanted onto it; the next a flaky pass healthy in staging, production waiting on a person
    /// for it; the newest a person's, failing its checks.
    fn fixture() -> String {
        let leg = |id: &str, run: &str, name: &str, status: &str, conclusion: &str| {
            // A leg that ran has its start, and its finish once completed (W26-R14's lanes).
            let started = if status == "queued" { "NULL" } else { "1700000000" };
            let finished = if status == "completed" { "1700000300" } else { "NULL" };
            format!("INSERT INTO jobs (id, run_id, key, display, status, conclusion, labels, needs, spec, job_id, run_attempt, started_at, finished_at) VALUES ('{id}', '{run}', '{name}', '{name}', '{status}', '{conclusion}', '[]', '[]', '{{}}', '{name}', 1, {started}, {finished});")
        };
        let run = |id: &str, n: i64, pipeline: &str, sha: &str, author: &str, msg: &str| {
            format!("INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, message, author, status, created_at, attempt) VALUES ('{id}', 'kiln', {n}, '{pipeline}', 'push', 'main', '{sha}', '{msg}', '{author}', 'completed', {NOW_S}, 1);")
        };
        let mut s = String::from("INSERT INTO repos (id, name, clone_url, created_at) VALUES ('kiln', 'hyper-light/kiln', 'https://example.invalid/kiln.git', 1);");
        s += &run("r1", 1, "ci", C1, "ada", "deploy: a rollback holds");
        s += &leg("l1", "r1", "rust lint + test", "completed", "success");
        s += &run("r2", 2, "release", C1, "ada", "deploy: a rollback holds");
        for (i, p) in ["Linux", "Windows", "macOS"].iter().enumerate() {
            s += &leg(&format!("p1{i}"), "r2", p, "completed", "success");
        }
        s += &run("r3", 3, "ci", C2, "Kiln Distro", "docs: the train notes");
        s += &leg("l2", "r3", "playwright", "completed", "success");
        s += "INSERT INTO ci_attempts (id, leg_id, number, status, conclusion, failure_class) VALUES ('a1', 'l2', 1, 'done', 'failure', 'failure'), ('a2', 'l2', 2, 'done', 'success', NULL);";
        s += &run("r4", 4, "release", C2, "Kiln Distro", "docs: the train notes");
        s += &leg("p2", "r4", "Linux", "completed", "success");
        s += &format!("INSERT INTO ci_images (repo_id, name, reference, digest, run_id, created_at) VALUES ('kiln', 'kiln', 'kiln:2222222', 'sha256:abc', 'r4', {NOW_S});");
        s += &run("r5", 5, "ci", C3, "mei", "sync: a move carries its homes");
        s += &leg("l3", "r5", "rust lint + test", "completed", "failure");
        s += "UPDATE jobs SET error = 'assertion failed in sync::reconnect' WHERE id = 'l3';";
        let deploy = |id: i64, env: &str, sha: &str, prod: i64, at: i64| {
            format!("INSERT INTO ci_deployments (id, repo_id, environment, sha, git_ref, creator, production, created_at, updated_at) VALUES ({id}, 'kiln', '{env}', '{sha}', 'main', 'kiln', {prod}, {at}, {at});
                     INSERT INTO ci_deployment_statuses (deployment_id, state, creator, created_at) VALUES ({id}, 'success', 'kiln', {at});")
        };
        s += &deploy(1, "staging", C1, 0, NOW_S - 7200);
        s += &deploy(2, "production", C1, 1, NOW_S - 3600);
        s += &deploy(3, "staging", C2, 0, NOW_S - 600);
        s += &format!("INSERT INTO deploy_releases (app, environment, state, spec, created_at) VALUES ('kiln', 'production', 'waiting', '{{\"version\": \"{C2}\"}}', {NOW_S});");
        s
    }

    fn runners() -> Vec<RunnerSeen> {
        vec![
            RunnerSeen { name: "metal-ryzen-01".into(), connected: true, commit: Some(short(C1)), failed: None },
            RunnerSeen { name: "metal-epyc-01".into(), connected: true, commit: Some(C1.into()), failed: None },
        ]
    }

    fn field<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
        path.iter().try_fold(v, |v, k| v.get(k))
    }

    /// a946ea5 as production keeps it (2026-10-28): v1's runs, whose jobs have no `job_id` and no
    /// `run_attempt` and say how they ended in their status alone; and C2, its checks passed and
    /// nothing built.
    fn production_fixture() -> String {
        let run = |id: &str, n: i64, pipeline: &str, sha: &str, status: &str| {
            format!("INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, message, author, status, created_at) VALUES ('{id}', 'kiln', {n}, '{pipeline}', 'push', 'main', '{sha}', 'Mission control: an offline machine', 'user.email', '{status}', {NOW_S});")
        };
        let job = |id: &str, run: &str, key: &str, display: &str, status: &str, error: &str, started: Option<i64>, finished: i64| {
            let started = started.map_or_else(|| "NULL".to_string(), |s| (NOW_S + s).to_string());
            let display = display.replace('\'', "''");
            format!("INSERT INTO jobs (id, run_id, key, display, status, error, labels, needs, spec, started_at, finished_at) VALUES ('{id}', '{run}', '{key}', '{display}', '{status}', NULLIF('{error}', ''), '[]', '[]', '{{}}', {started}, {});", NOW_S + finished)
        };
        let mut s = String::from("INSERT INTO repos (id, name, clone_url, created_at) VALUES ('kiln', 'hyper-light/kiln', 'https://example.invalid/kiln.git', 1);");
        s += &run("ci", 1426, "ci", C1, "failed");
        s += &job("j1", "ci", "release-binary", "release build (bare metal)", "skipped", "dependency failed", None, 1660);
        s += &job("j2", "ci", "rust", "rust lint + test", "failed", "step 4 exited with Some(101)", Some(1583), 3719);
        s += &job("j3", "ci", "ui", "ui typecheck + lint + build", "failed", "step 2 exited with Some(1)", Some(1598), 1660);
        s += &job("j4", "ci", "ui-checks", "ui checks (Chromium and WebKit, desktop and phone)", "skipped", "dependency failed", None, 1660);
        s += &job("j5", "ci", "vendor", "vendored projects (upstream plus kiln's recorded changes)", "failed", "step 3 exited with Some(1)", Some(1586), 1601);
        s += &run("image", 1427, "image", C1, "succeeded");
        s += &job("j6", "image", "image", "build and publish kiln's image", "succeeded", "", Some(358), 822);
        s += &run("release", 1428, "release", C1, "succeeded");
        s += &job("j7", "release", "linux", "Linux (x86-64, ARM64)", "succeeded", "", Some(292), 1361);
        s += &job("j8", "release", "macos", "macOS (ARM64), on a Mac", "succeeded", "", Some(292), 949);
        s += &job("j9", "release", "windows", "Windows (x86-64, cross-built)", "succeeded", "", Some(292), 855);
        s += &run("ci2", 1429, "ci", C2, "succeeded");
        s += &job("k1", "ci2", "rust", "rust lint + test", "succeeded", "", Some(10), 900);
        s
    }

    /// Plan 01a1856fe81375b1bd1d1d86f270a45b, steps 2 and 3: production's own runs report. Each of
    /// a946ea5's v1 jobs is a leg as it ended, with its times; its Build is its image pipeline's
    /// job and its release's platforms. A commit no build ran for isn't built.
    #[test]
    fn ship_reports_productions_v1_runs_as_they_ended() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let sql = production_fixture();
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let now = (NOW_S + 4000) * 1000;
            let one = get_in(&store, "hyper-light/kiln", C1, None, Vec::new(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
            let pip = |v: &Value, stage: &str| v.get("pips").and_then(Value::as_array).and_then(|ps| ps.iter().find(|p| p.get("stage").and_then(Value::as_str) == Some(stage))).map(|p| text(p, "state")).unwrap_or_default();
            let checks = field(&one, &["checks"]).and_then(Value::as_array).cloned().unwrap_or_default();
            assert_eq!(checks.iter().map(|c| text(c, "pipeline")).collect::<Vec<_>>(), vec!["ci"], "the image and release pipelines aren't checks");
            let legs: Vec<(String, String, String, Option<i64>, Option<i64>, bool)> = checks
                .iter()
                .flat_map(|c| c.get("legs").and_then(Value::as_array).cloned().unwrap_or_default())
                .map(|l| (text(&l, "name"), text(&l, "state"), text(&l, "note"), l.get("start").and_then(Value::as_i64), l.get("end").and_then(Value::as_i64), l.get("job").is_some()))
                .collect();
            let at = |s: i64| Some(NOW_S + s);
            assert_eq!(
                legs,
                vec![
                    ("release build (bare metal)".into(), "superseded".into(), "dependency failed".into(), None, at(1660), false),
                    ("rust lint + test".into(), "failed".into(), "step 4 exited with Some(101)".into(), at(1583), at(3719), true),
                    ("ui typecheck + lint + build".into(), "failed".into(), "step 2 exited with Some(1)".into(), at(1598), at(1660), true),
                    ("ui checks (Chromium and WebKit, desktop and phone)".into(), "superseded".into(), "dependency failed".into(), None, at(1660), false),
                    ("vendored projects (upstream plus kiln's recorded changes)".into(), "failed".into(), "step 3 exited with Some(1)".into(), at(1586), at(1601), true),
                ]
            );
            assert_eq!((pip(&one, "checks"), text(&one, "stand"), text(&one, "line")), ("failed".into(), "needs_you".into(), "rust lint + test · step 4 exited with Some(101)".into()));
            // Build: the image its image pipeline built, the platforms its release built.
            assert_eq!(field(&one, &["build", "image", "state"]).and_then(Value::as_str), Some("passed"));
            let platforms: Vec<(String, String)> =
                field(&one, &["build", "platforms"]).and_then(Value::as_array).into_iter().flatten().map(|p| (text(p, "name"), text(p, "state"))).collect();
            assert_eq!(
                platforms,
                vec![("Linux (x86-64, ARM64)".into(), "passed".into()), ("macOS (ARM64), on a Mac".into(), "passed".into()), ("Windows (x86-64, cross-built)".into(), "passed".into())]
            );
            assert_eq!(pip(&one, "build"), "passed");
            // A commit no build ran for: not built, its checks passed.
            let two = get_in(&store, "hyper-light/kiln", C2, None, Vec::new(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!((pip(&two, "checks"), pip(&two, "build"), field(&two, &["build", "image", "state"]).and_then(Value::as_str)), ("passed".into(), "none".into(), Some("none")));
            assert_eq!((text(&two, "stand"), text(&two, "line")), ("waiting".into(), "Checks passed".into()));
            Ok(())
        })
    }

    /// Three commits of kiln's own, oldest first, each checked and its image built: C3, C1, C2.
    fn cd_fixture() -> String {
        let run = |id: &str, n: i64, pipeline: &str, sha: &str| {
            format!("INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, message, author, status, created_at) VALUES ('{id}', 'kiln', {n}, '{pipeline}', 'push', 'main', '{sha}', 'a commit', 'user.email', 'succeeded', {NOW_S});")
        };
        let job = |id: &str, run: &str, key: &str| {
            format!("INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, started_at, finished_at) VALUES ('{id}', '{run}', '{key}', '{key}', 'succeeded', '[]', '[]', '{{}}', {NOW_S}, {});", NOW_S + 60)
        };
        let mut s = String::from("INSERT INTO repos (id, name, clone_url, created_at) VALUES ('kiln', 'hyper-light/kiln', 'https://example.invalid/kiln.git', 1);");
        for (sha, n) in [(C3, 1), (C1, 3), (C2, 5)] {
            s += &run(&format!("ci{n}"), n, "ci", sha);
            s += &job(&format!("c{n}"), &format!("ci{n}"), "rust lint + test");
            s += &run(&format!("im{n}"), n + 1, "image", sha);
            s += &job(&format!("i{n}"), &format!("im{n}"), "image");
        }
        s
    }

    /// KilnCD's history as kiln-production keeps it (2026-10-28): C3 deployed and held, then C1 at
    /// `t1`, each sync followed by its guard's word on the same op (the engine numbers its syncs
    /// afresh when it restarts); and another app in production, its namespaces, syncing C2 from git
    /// with no image.
    fn cd_history(t1: i64) -> Vec<CdDeploy> {
        let entry = |app: &str, op: u64, sha: &str, at: i64, message: &str, image: bool| kiln_cd::state::Deployment {
            app: app.into(),
            op,
            revision: Some(sha.into()),
            succeeded: true,
            message: message.into(),
            at_ms: u64::try_from(at.saturating_mul(1000)).unwrap_or(0),
            artifacts: if image {
                vec![kiln_cd::apps::ArtifactRef { name: "image".into(), reference: format!("kiln:{sha}"), repository: "kiln".into(), tag: Some(sha.into()), digest: String::new() }]
            } else {
                Vec::new()
            },
            ..Default::default()
        };
        let (k, t0) = ("default/kiln-production", t1 - 7200);
        let kiln = [
            entry(k, 1, C3, t0, "successfully synced (all tasks run)", true),
            entry(k, 1, C3, t0 + 600, "kiln restarted into kiln:3333333, and stayed healthy for 10m", true),
            entry(k, 1, C1, t1, "successfully synced (all tasks run)", true),
            entry(k, 1, C1, t1 + 600, "kiln restarted into kiln:1111111, and stayed healthy for 10m", true),
        ];
        let mut out = deploys_of(k, "production", kiln.iter());
        let namespaces = [entry("default/namespaces", 2, C2, t1 + 900, "successfully synced (all tasks run)", false)];
        out.extend(deploys_of("default/namespaces", "production", namespaces.iter()));
        out
    }

    /// Plan 01a1856fe81375b1bd1d1d86f270a45b, step 4: production as KilnCD keeps it. The commit
    /// kiln-production runs reads in production (the guard watching for its first ten minutes) and
    /// replants; the one it replaced reads superseded; the newer one built and waiting. Another
    /// app's git sync in production says nothing of what runs, and with no staging nothing reads
    /// in staging. KilnCD's word is read into memory when it changes, though the store didn't.
    #[test]
    fn ship_reads_production_from_kilncd() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let sql = cd_fixture();
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let t1 = NOW_S - 3600;
            let deploys = cd_history(t1);
            assert_eq!(deploys.iter().map(|d| (short(&d.sha), d.built, d.state.as_str())).collect::<Vec<_>>(), vec![
                (short(C3), true, "success"),
                (short(C1), true, "success"),
                (short(C2), false, "success"),
            ]);
            let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
            let pip = |v: &Value, stage: &str| v.get("pips").and_then(Value::as_array).and_then(|ps| ps.iter().find(|p| p.get("stage").and_then(Value::as_str) == Some(stage))).map(|p| text(p, "state")).unwrap_or_default();
            let now = NOW_S * 1000;
            let one = get_in(&store, "hyper-light/kiln", C1, None, runners(), &deploys, now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!((pip(&one, "production"), text(&one, "stand"), text(&one, "line")), ("healthy".into(), "shipped".into(), "In production · runners 2/2".into()));
            assert_eq!(field(&one, &["replant"]).and_then(Value::as_array).map(Vec::len), Some(2));
            let old = get_in(&store, "hyper-light/kiln", C3, None, runners(), &deploys, now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!((pip(&old, "production"), text(&old, "stand"), text(&old, "line")), ("passed".into(), "shipped".into(), "Superseded".into()));
            let newer = get_in(&store, "hyper-light/kiln", C2, None, runners(), &deploys, now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!(
                (pip(&newer, "production"), pip(&newer, "staging"), text(&newer, "stand"), text(&newer, "line")),
                ("none".into(), "none".into(), "waiting".into(), "Built".into())
            );
            // In the guard's ten minutes after the sync: shipping, the guard watching.
            let watching = get_in(&store, "hyper-light/kiln", C1, None, runners(), &deploys, (t1 + 300) * 1000).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!((pip(&watching, "production"), text(&watching, "stand"), text(&watching, "line")), ("guard".into(), "shipping".into(), "Guard watching".into()));
            // From memory the same; then KilnCD deploys C2, and the next refresh reads it in.
            let cache = ShipCache::new();
            let stands = |cache: &ShipCache| -> Vec<(String, String, String)> {
                let page: Value = cache.page(None, None, &Facets::default(), None, i64::MAX, 10).ok().flatten().and_then(|p| serde_json::from_str(&p).ok()).unwrap_or_default();
                page.get("commits").and_then(Value::as_array).into_iter().flatten().map(|c| (text(c, "short"), text(c, "stand"), text(c, "line"))).collect()
            };
            cache.refresh(&store, &runners(), &deploys, now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!(stands(&cache), vec![
                (short(C2), "waiting".into(), "Built".into()),
                (short(C1), "shipped".into(), "In production · runners 2/2".into()),
                (short(C3), "shipped".into(), "Superseded".into()),
            ]);
            let mut later = deploys.clone();
            later.push(CdDeploy { app: "default/kiln-production".into(), environment: "production".into(), sha: C2.into(), built: true, op: 2, state: "success".into(), description: "successfully synced (all tasks run)".into(), at: NOW_S });
            cache.refresh(&store, &runners(), &later, now + FRESH_MS).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!(stands(&cache), vec![
                (short(C2), "shipping".into(), "Guard watching".into()),
                (short(C1), "shipped".into(), "Superseded".into()),
                (short(C3), "shipped".into(), "Superseded".into()),
            ]);
            Ok(())
        })
    }

    #[test]
    fn ship_commits_of_kilns_own_repository_from_its_records() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let sql = fixture();
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let now = NOW_S * 1000;
            let list = list_in(&store, &HashMap::from([("repo".to_string(), "hyper-light/kiln".to_string())]), None, None, runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let commits = list.get("commits").and_then(Value::as_array).cloned().unwrap_or_default();
            let got: Vec<(String, String, String)> = commits
                .iter()
                .map(|c| (field(c, &["short"]).and_then(Value::as_str).unwrap_or_default().to_string(), field(c, &["stand"]).and_then(Value::as_str).unwrap_or_default().to_string(), field(c, &["line"]).and_then(Value::as_str).unwrap_or_default().to_string()))
                .collect();
            assert_eq!(
                got,
                vec![
                    ("3333333".to_string(), "needs_you".to_string(), "rust lint + test · assertion failed in sync::reconnect".to_string()),
                    ("2222222".to_string(), "needs_you".to_string(), "Approve for production · 1 commit".to_string()),
                    ("1111111".to_string(), "shipped".to_string(), "In production · runners 2/2".to_string()),
                ]
            );

            let one = get_in(&store, "hyper-light/kiln", "2222222", None, runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let pip = |stage: &str| one.get("pips").and_then(Value::as_array).and_then(|ps| ps.iter().find(|p| p.get("stage").and_then(Value::as_str) == Some(stage))).and_then(|p| p.get("state")).and_then(Value::as_str).map(str::to_string);
            assert_eq!(pip("checks").as_deref(), Some("flaky"));
            assert_eq!(pip("staging").as_deref(), Some("healthy"));
            assert_eq!(pip("production").as_deref(), Some("needs"));
            assert_eq!(field(&one, &["build", "image", "digest"]).and_then(Value::as_str), Some("sha256:abc"));
            assert_eq!(field(&one, &["checks"]).and_then(Value::as_array).map(Vec::len), Some(1));
            // A leg that ran names its job and its run, for Output to stream.
            let leg = field(&one, &["checks"]).and_then(Value::as_array).and_then(|c| c.first()).and_then(|c| c.get("legs")).and_then(Value::as_array).and_then(|l| l.first()).cloned().unwrap_or_default();
            assert!(leg.get("job").and_then(Value::as_str).is_some_and(|j| !j.is_empty()), "a leg that ran names its job: {leg}");
            assert!(leg.get("run").and_then(Value::as_str).is_some_and(|r| !r.is_empty()), "a leg that ran names its run: {leg}");
            // It says when it started (W26-R14), and the row carries its version `v` (W26-R06).
            assert!(leg.get("start").and_then(Value::as_i64).is_some(), "a leg that ran says when it started: {leg}");
            assert!(one.get("v").and_then(Value::as_str).is_some_and(|v| v.len() == 16), "a row carries its version: {one}");

            let shipped = get_in(&store, "hyper-light/kiln", C1, None, runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!(field(&shipped, &["replant"]).and_then(Value::as_array).map(Vec::len), Some(2));
            // Every action names an endpoint kiln has: a route in its table, not the unnamed default.
            let mut failed = runners();
            if let Some(r) = failed.get_mut(1) {
                r.failed = Some("upgrade timed out: no report within 15 minutes".into());
            }
            let with_failed = list_in(&store, &HashMap::new(), None, None, failed, &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let acts: Vec<(String, String, String)> = with_failed
                .get("commits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|c| c.get("actions").and_then(Value::as_array).cloned().unwrap_or_default())
                .map(|a| (a.get("name").and_then(Value::as_str).unwrap_or_default().to_string(), a.get("method").and_then(Value::as_str).unwrap_or_default().to_string(), a.get("path").and_then(Value::as_str).unwrap_or_default().to_string()))
                .collect();
            let names: Vec<&str> = acts.iter().map(|(n, _, _)| n.as_str()).collect();
            assert_eq!(names, vec!["retry", "hand_off", "approve", "hold", "retry_runner"]);
            let named = |method: &str, path: &str| -> anyhow::Result<bool> {
                let m = http::Method::from_bytes(method.as_bytes())?;
                let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
                Ok(crate::authz::rule(&m, &segs).pattern != crate::authz::UNNAMED.pattern)
            };
            assert!(!named("POST", "/api/ship/no/such/route")?, "the check tells a route kiln lacks");
            for (name, method, path) in &acts {
                assert!(named(method, path)?, "{name}: {method} {path} is no route kiln has");
            }
            let missing = get_in(&store, "hyper-light/kiln", "deadbee", None, runners(), &[], now).await;
            assert!(matches!(missing, Err((StatusCode::NOT_FOUND, _))));
            Ok(())
        })
    }

    /// W26-AC01 (kiln's part): a diff and a change request are Ship items by commit, ordered as
    /// Ship orders, the tip carrying the review for each viewer, the rest folded under it; they
    /// carry on as the same item once landed; host and kind filter, and the Review lens keeps them.
    #[test]
    fn w26_ac01_a_kiln_diff_and_a_change_request_are_ship_items() -> anyhow::Result<()> {
        const D41: &str = "4141414aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const D41V2: &str = "4242424aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const LANDED: &str = "4343434aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const CR1: &str = "1212121ccccccccccccccccccccccccccccccccc";
        const CR2: &str = "1313131ccccccccccccccccccccccccccccccccc";
        const CRH: &str = "1414141ccccccccccccccccccccccccccccccccc";
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let t = NOW_S - 600;
            let sql = fixture()
                + &format!(
                    "INSERT INTO diffs (id, repo_id, number, kind, title, author, author_kind, author_for, state, trunk, created_at, updated_at) VALUES ('d41', 'kiln', 41, 'diff', 'pipeline: dynamic matrices', 'agent:matrix', 'agent', 'user:sam', 'needs-review', 'main', {t}, {t});
                     INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, message, pushed_by, created_at) VALUES ('d41', 1, '{D41}', 'p', 'b', 'pipeline: dynamic matrices', 'agent:matrix', {t});
                     INSERT INTO diff_reviewers (diff_id, reviewer, state, added_at) VALUES ('d41', 'user:kim', 'not-looked', {t});
                     INSERT INTO review_threads (id, diff_id, version, file, start_line, state, created_by, created_at, kind) VALUES ('th1', 'd41', 1, 'crates/kiln-ci/src/plan/matrix.rs', 42, 'open', 'user:kim', {}, 'question');
                     INSERT INTO review_comments (id, thread_id, author, body, created_at, updated_at, published) VALUES ('c1', 'th1', 'user:kim', 'empty matrix meant?\n\nGitHub skips the job when `matrix` is empty.', {}, {}, 1);
                     INSERT INTO diffs (id, repo_id, number, kind, title, author, author_kind, state, trunk, branch, created_at, updated_at) VALUES ('cr12', 'kiln', 12, 'change_request', 'sched: fair shares', 'user:lee', 'human', 'needs-review', 'main', 'fair', {}, {});
                     INSERT INTO diff_version_commits (diff_id, n, sha, position) VALUES ('cr12', 1, '{CR1}', 0), ('cr12', 1, '{CR2}', 1);
                     INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, pushed_by, created_at) VALUES ('cr12', 1, '{CRH}', 'b', 'b', 'user:lee', {});",
                    t + 60, t + 60, t + 60, t + 120, t + 120, t + 120
                );
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let now = NOW_S * 1000;
            let (kim, sam): (HashSet<String>, HashSet<String>) = (["user:kim".to_string()].into(), ["user:sam".to_string()].into());
            let list = |q: &[(&str, &str)], viewer: Option<HashSet<String>>| {
                let (store, q) = (store.clone(), q.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>());
                async move { list_in(&store, &q, None, viewer.as_ref(), runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m)) }
            };
            let rows = |v: &Value| -> Vec<(String, String, String, String)> {
                let s = |c: &Value, k: &str| c.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                v.get("commits").and_then(Value::as_array).into_iter().flatten().map(|c| (s(c, "short"), s(c, "stand"), s(c, "line"), s(c, "fold_under").get(..7).unwrap_or_default().to_string())).collect()
            };
            let row = |r: (&str, &str, &str, &str)| (r.0.to_string(), r.1.to_string(), r.2.to_string(), r.3.to_string());

            // Newest first on Ship's one scale: the request's head over its commits, then D41, then
            // the fixture's commits. kim asked a question, so it's sam's (the agent's person) to answer.
            let as_sam = list(&[], Some(sam.clone())).await?;
            assert_eq!(
                rows(&as_sam),
                vec![
                    row(("1414141", "waiting", "Waiting on review", "")),
                    row(("1313131", "waiting", "Not landed", "1414141")),
                    row(("1212121", "waiting", "Not landed", "1414141")),
                    row(("4141414", "needs_you", "kim: empty matrix meant? · 1 thread", "")),
                    row(("3333333", "needs_you", "rust lint + test · assertion failed in sync::reconnect", "")),
                    row(("2222222", "needs_you", "Approve for production · 1 commit", "")),
                    row(("1111111", "shipped", "In production · runners 2/2", "")),
                ]
            );
            let d41 = as_sam.get("commits").and_then(Value::as_array).and_then(|c| c.get(3)).cloned().unwrap_or_default();
            assert_eq!((field(&d41, &["kind"]), field(&d41, &["host"]), field(&d41, &["change", "name"])), (Some(&json!("diff")), Some(&json!("kiln")), Some(&json!("D41"))));
            let pip = |c: &Value, stage: &str| c.get("pips").and_then(Value::as_array).and_then(|ps| ps.iter().find(|p| p.get("stage").and_then(Value::as_str) == Some(stage))).and_then(|p| p.get("state")).and_then(Value::as_str).map(str::to_string);
            assert_eq!((pip(&d41, "review").as_deref(), pip(&d41, "land").as_deref()), (Some("requested"), Some("none")), "its strip lit at Review");
            let names: Vec<&str> = d41.get("actions").and_then(Value::as_array).into_iter().flatten().filter_map(|a| a.get("name").and_then(Value::as_str)).collect();
            assert_eq!(names, vec!["hand_off", "review"], "Ship's Hand off and Review pair, for the one whose turn it is");
            // Everyone else: waiting, and no pair.
            let as_kim = list(&[], Some(kim.clone())).await?;
            let d41k = as_kim.get("commits").and_then(Value::as_array).and_then(|c| c.get(3)).cloned().unwrap_or_default();
            assert_eq!((field(&d41k, &["stand"]), field(&d41k, &["actions"])), (Some(&json!("waiting")), Some(&json!([]))));
            assert_eq!(list(&[], None).await?, as_kim, "no viewer sees what everyone else does");

            // The facets.
            let shorts = |v: &Value| rows(v).into_iter().map(|r| r.0).collect::<Vec<_>>();
            assert_eq!(shorts(&list(&[("kind", "diff")], None).await?), vec!["4141414"]);
            assert_eq!(shorts(&list(&[("kind", "request")], None).await?), vec!["1414141", "1313131", "1212121"]);
            assert_eq!(shorts(&list(&[("kind", "commit")], None).await?), vec!["3333333", "2222222", "1111111"]);
            assert_eq!(shorts(&list(&[("lens", "review"), ("host", "kiln")], None).await?), vec!["1414141", "1313131", "1212121", "4141414"]);
            assert!(shorts(&list(&[("host", "github")], None).await?).is_empty());

            // v2 supersedes v1, which folds under it; then D41 lands and carries on as the commit it
            // landed as.
            let sql = format!(
                "INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, message, pushed_by, created_at) VALUES ('d41', 2, '{D41V2}', 'p', 'b', 'pipeline: dynamic matrices', 'agent:matrix', {});
                 INSERT INTO reviews (id, diff_id, version, reviewer, reviewer_kind, action, body, created_at) VALUES ('rv1', 'd41', 2, 'user:kim', 'human', 'accept', '', {});
                 UPDATE review_threads SET state = 'resolved' WHERE id = 'th1';
                 UPDATE diffs SET state = 'accepted' WHERE id = 'd41';",
                t + 200,
                t + 300
            );
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let after = rows(&list(&[("kind", "diff")], Some(sam.clone())).await?);
            assert_eq!(after, vec![row(("4242424", "landing", "Approved · joining the queue", "")), row(("4141414", "superseded", "Replaced by v2", "4242424"))]);
            let sql = format!(
                "INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, message, author, status, created_at, attempt) VALUES ('r41', 'kiln', 41, 'ci', 'push', 'main', '{LANDED}', 'pipeline: dynamic matrices', 'user:sam', 'completed', {NOW_S}, 1);
                 INSERT INTO jobs (id, run_id, key, display, status, conclusion, labels, needs, spec, job_id, run_attempt) VALUES ('l41', 'r41', 'rust lint + test', 'rust lint + test', 'completed', 'success', '[]', '[]', '{{}}', 'rust lint + test', 1);
                 UPDATE diffs SET state = 'landed', landed_commit = '{LANDED}', landed_at = {NOW_S} WHERE id = 'd41';"
            );
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let landed = list(&[("kind", "diff")], Some(sam.clone())).await?;
            let r = rows(&landed);
            assert_eq!(r.iter().map(|x| (x.0.as_str(), x.3.as_str())).collect::<Vec<_>>(), vec![("4343434", ""), ("4242424", "4343434"), ("4141414", "4343434")]);
            let tip = landed.get("commits").and_then(Value::as_array).and_then(|c| c.first()).cloned().unwrap_or_default();
            assert_eq!((pip(&tip, "review").as_deref(), pip(&tip, "land").as_deref()), (Some("approved"), Some("passed")), "landed, its review still lit");
            assert_eq!(field(&tip, &["change", "name"]), Some(&json!("D41")));
            assert_eq!(r.get(1).map(|x| x.2.as_str()), Some("Replaced by 4343434"));
            Ok(())
        })
    }

    fn ops(n: u64, op: u64, sha: &str) -> String {
        let t = NOW_S - 3600 + i64::try_from(n).unwrap_or(0);
        let env = if n.is_multiple_of(2) { "staging" } else { "production" };
        let state = ["success", "in_progress", "failure"].get(usize::try_from(n % 3).unwrap_or(0)).copied().unwrap_or("success");
        match op {
            0 => format!("INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, message, author, status, created_at, attempt) VALUES ('x{n}', 'kiln', {}, '{}', 'push', 'main', '{sha}', 'change {n}', 'ada', 'completed', {t}, 1);", 1000 + n, if n.is_multiple_of(3) { "release" } else { "ci" }),
            1 => format!("INSERT INTO jobs (id, run_id, key, display, status, conclusion, labels, needs, spec, job_id, run_attempt) SELECT 'j{n}', id, 'leg {n}', 'leg {n}', 'completed', 'success', '[]', '[]', '{{}}', 'leg {n}', 1 FROM runs WHERE sha = '{sha}' LIMIT 1;"),
            2 => format!("UPDATE jobs SET conclusion = CASE conclusion WHEN 'success' THEN 'failure' ELSE 'success' END WHERE id = (SELECT j.id FROM jobs j JOIN runs r ON r.id = j.run_id WHERE r.sha = '{sha}' LIMIT 1);"),
            3 => format!("INSERT INTO ci_attempts (id, leg_id, number, status, conclusion, failure_class) SELECT 'a{n}', j.id, {n}, 'done', 'failure', 'runner-lost' FROM jobs j JOIN runs r ON r.id = j.run_id WHERE r.sha = '{sha}' LIMIT 1;"),
            4 => format!("DELETE FROM runs WHERE id = (SELECT id FROM runs WHERE sha = '{sha}' ORDER BY rowid DESC LIMIT 1);"),
            5 => format!("DELETE FROM jobs WHERE id = (SELECT j.id FROM jobs j JOIN runs r ON r.id = j.run_id WHERE r.sha = '{sha}' LIMIT 1);"),
            6 => format!("INSERT INTO ci_images (repo_id, name, reference, digest, run_id, created_at) SELECT 'kiln', 'kiln', 'kiln:x', 'sha256:{n}', id, 1 FROM runs WHERE sha = '{sha}' LIMIT 1;"),
            7 => format!("INSERT INTO ci_deployments (id, repo_id, environment, sha, git_ref, creator, production, created_at, updated_at) VALUES ({}, 'kiln', '{env}', '{sha}', 'main', 'kiln', {}, {t}, {t}); INSERT INTO ci_deployment_statuses (deployment_id, state, creator, created_at) VALUES ({}, '{state}', 'kiln', {t});", 100 + n, u8::from(env == "production"), 100 + n),
            8 => format!("INSERT INTO deploy_releases (app, environment, state, spec, created_at) VALUES ('kiln', 'production', 'waiting', '{{\"version\": \"{sha}\"}}', 1);"),
            9 => "UPDATE deploy_releases SET state = 'committing' WHERE state = 'waiting';".to_string(),
            10 => "DELETE FROM deploy_releases WHERE id = (SELECT MIN(id) FROM deploy_releases);".to_string(),
            11 => format!("INSERT OR IGNORE INTO ci_releases (repo_id, tag_name, target_commitish, author, created_at) VALUES ('kiln', 'v0.{n}', '{sha}', 'ada', 1);"),
            12 => format!("DELETE FROM ci_releases WHERE id = (SELECT MIN(id) FROM ci_releases WHERE target_commitish = '{sha}');"),
            13 => format!("UPDATE ci_deployment_statuses SET state = '{state}' WHERE id = (SELECT MAX(id) FROM ci_deployment_statuses);"),
            14 => format!("DELETE FROM ci_deployments WHERE id = (SELECT MIN(id) FROM ci_deployments WHERE sha = '{sha}');"),
            _ => review_op(n, op, sha, t),
        }
    }

    /// A random change to review's records (W26-R01): a diff and its versions, reviewers, reviews,
    /// threads of each kind and replies, a land, a change request with its commits, the land queue,
    /// drafts.
    fn review_op(n: u64, op: u64, sha: &str, t: i64) -> String {
        // One of the diffs there are, by turns.
        let d = format!("(SELECT id FROM diffs ORDER BY rowid LIMIT 1 OFFSET ({n} % MAX(1, (SELECT COUNT(*) FROM diffs))))");
        let th = format!("(SELECT id FROM review_threads ORDER BY rowid LIMIT 1 OFFSET ({n} % MAX(1, (SELECT COUNT(*) FROM review_threads))))");
        let pick = |xs: &[&'static str]| xs.get(usize::try_from(n).unwrap_or(0) % xs.len().max(1)).copied().unwrap_or_default();
        match op {
            15 => format!(
                "INSERT INTO diffs (id, repo_id, number, kind, title, author, author_kind, state, trunk, created_at, updated_at) VALUES ('d{n}', 'kiln', {n}, 'diff', 'change {n}', 'user:sam', 'human', 'needs-review', 'main', {t}, {t});
                 INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, message, pushed_by, created_at) VALUES ('d{n}', 1, '{sha}', 'p', 'b', 'change {n}', 'user:sam', {t});"
            ),
            16 => format!("INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, rebase_only, pushed_by, created_at) SELECT d.id, (SELECT MAX(v.n) + 1 FROM diff_versions v WHERE v.diff_id = d.id), '{sha}', 'p', 'b', {}, 'user:sam', {t} FROM diffs d WHERE d.id = {d};", n % 2),
            17 => format!("INSERT OR IGNORE INTO diff_reviewers (diff_id, reviewer, state, added_at) SELECT {d}, '{}', 'not-looked', {t} WHERE {d} IS NOT NULL;", pick(&["user:kim", "user:lee"])),
            18 => {
                let (action, state) = [("accept", "accepted"), ("request_changes", "changes-requested"), ("comment", "needs-review")].get(usize::try_from(n % 3).unwrap_or(0)).copied().unwrap_or(("comment", "needs-review"));
                format!(
                    "INSERT INTO reviews (id, diff_id, version, reviewer, reviewer_kind, action, body, created_at) SELECT 'rv{n}', {d}, 1, 'user:kim', 'human', '{action}', 'outputs stay JSON', {t} WHERE {d} IS NOT NULL;
                     UPDATE diffs SET state = '{state}' WHERE id = {d} AND state NOT IN ('landed', 'draft');"
                )
            }
            19 => format!(
                "INSERT INTO review_threads (id, diff_id, version, file, start_line, state, created_by, created_at, kind) SELECT 'th{n}', {d}, 1, 'src/expand.rs', 22, 'open', 'user:kim', {t}, '{}' WHERE {d} IS NOT NULL;
                 INSERT INTO review_comments (id, thread_id, author, body, created_at, updated_at, published) SELECT 'c{n}', 'th{n}', 'user:kim', 'empty matrix meant?', {t}, {t}, {} WHERE EXISTS (SELECT 1 FROM review_threads WHERE id = 'th{n}');",
                pick(&["question", "nit", "issue", "blocking"]),
                u8::from(!n.is_multiple_of(5))
            ),
            20 => format!("UPDATE review_threads SET state = CASE state WHEN 'open' THEN 'resolved' ELSE 'open' END WHERE id = {th};"),
            21 => format!("INSERT INTO review_comments (id, thread_id, author, body, created_at, updated_at, published) SELECT 'c{n}', {th}, 'user:sam', 'it is', {t}, {t}, 1 WHERE {th} IS NOT NULL;"),
            22 => format!("UPDATE diffs SET state = 'landed', landed_commit = '{sha}' WHERE id = {d};"),
            23 => format!(
                "INSERT INTO diffs (id, repo_id, number, kind, title, author, author_kind, state, trunk, branch, created_at, updated_at) VALUES ('cr{n}', 'kiln', {n}, 'change_request', 'request {n}', 'agent:matrix', 'agent', 'needs-review', 'main', 'b{n}', {t}, {t});
                 UPDATE diffs SET author_for = 'user:sam' WHERE id = 'cr{n}';
                 INSERT INTO diff_version_commits (diff_id, n, sha, position) VALUES ('cr{n}', 1, '7777777{n:033}', 0);
                 INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, pushed_by, created_at) VALUES ('cr{n}', 1, '{sha}', 'b', 'b', 'agent:matrix', {t});"
            ),
            24 => format!(
                "INSERT INTO land_queue (id, repo_id, trunk, diffs, strategy, state, enqueued_by, created_at, updated_at) SELECT 'lq{n}', 'kiln', 'main', json_array({d}), 'rebase', '{}', 'user:sam', {t}, {t} WHERE {d} IS NOT NULL;",
                pick(&["queued", "testing", "ejected"])
            ),
            25 => format!("UPDATE diffs SET state = CASE state WHEN 'draft' THEN 'needs-review' ELSE 'draft' END WHERE id = {d} AND state <> 'landed';"),
            26 => format!("DELETE FROM land_queue WHERE id = (SELECT MIN(id) FROM land_queue); DELETE FROM reviews WHERE id = (SELECT MIN(id) FROM reviews WHERE diff_id = {d});"),
            // A host's pull request, mirrored (W26-R20): opened with a commit besides its head.
            27 => format!(
                "INSERT INTO host_pulls (repo_id, host, number, title, author, state, requested, first_seen) VALUES ('kiln', '{}', {n}, 'pull {n}', 'github:ada', 'open', '[\"github:kim\"]', {t});
                 INSERT INTO host_pull_commits (repo_id, host, number, sha, position) VALUES ('kiln', '{}', {n}, '9999999{n:033}', 0);
                 INSERT INTO host_pull_heads (repo_id, host, number, n, sha, seen_at) VALUES ('kiln', '{}', {n}, 1, '{sha}', {t});",
                pick(&["github", "gitlab", "bitbucket"]),
                pick(&["github", "gitlab", "bitbucket"]),
                pick(&["github", "gitlab", "bitbucket"])
            ),
            28 => format!(
                "INSERT INTO host_pull_heads (repo_id, host, number, n, sha, seen_at) SELECT p.repo_id, p.host, p.number, (SELECT MAX(h.n) + 1 FROM host_pull_heads h WHERE h.repo_id = p.repo_id AND h.host = p.host AND h.number = p.number), '{sha}', {t}
                 FROM host_pulls p WHERE p.rowid = (SELECT rowid FROM host_pulls ORDER BY rowid LIMIT 1 OFFSET ({n} % MAX(1, (SELECT COUNT(*) FROM host_pulls)))) AND EXISTS (SELECT 1 FROM host_pull_heads h WHERE h.repo_id = p.repo_id AND h.host = p.host AND h.number = p.number);"
            ),
            29 => format!(
                "UPDATE host_pulls SET reviews = '[{{\"by\": \"github:kim\", \"verdict\": \"{}\", \"at\": {t}}}]', threads = {}, said_by = 'github:kim', said = 'empty matrix meant?', state = '{}', merged_sha = '{}'
                 WHERE rowid = (SELECT rowid FROM host_pulls ORDER BY rowid LIMIT 1 OFFSET ({n} % MAX(1, (SELECT COUNT(*) FROM host_pulls))));",
                pick(&["approve", "request_changes", "comment"]),
                n % 3,
                pick(&["open", "open", "draft", "merged"]),
                if n % 4 == 3 { sha } else { "" }
            ),
            // A change's latest revision replayed onto main (W26-R12): conflicted (a text conflict and
            // a binary file, one of them resolved), or clean.
            30 => format!(
                "INSERT INTO review_replays (id, diff_id, version, commit_sha, onto, onto_name, tree, state, created_by, created_at)
                 SELECT 'rp{n}', d.id, (SELECT MAX(v.n) FROM diff_versions v WHERE v.diff_id = d.id), '{sha}', 'onto{n}', 'main', 'tree{n}', '{}', 'kiln', {t} FROM diffs d WHERE d.id = {d} AND EXISTS (SELECT 1 FROM diff_versions v WHERE v.diff_id = d.id);
                 INSERT INTO review_conflicts (id, replay_id, diff_id, position, file, kind, hunk, line, end_line, fingerprint) SELECT 'ka{n}', id, diff_id, 0, 'src/expand.rs', 'text', 0, 18, 24, 'fa{n}' FROM review_replays WHERE id = 'rp{n}' AND state = 'conflicted';
                 INSERT INTO review_conflicts (id, replay_id, diff_id, position, file, kind, fingerprint, take) SELECT 'kb{n}', id, diff_id, 1, 'assets/logo.png', 'binary', 'fb{n}', '{}' FROM review_replays WHERE id = 'rp{n}' AND state = 'conflicted';",
                pick(&["conflicted", "conflicted", "clean"]),
                pick(&["main", "this"])
            ),
            // A conflict resolved or taken back, and a replay's resolution committed.
            31 => format!(
                "UPDATE review_conflicts SET take = CASE WHEN take IS NULL THEN 'both' ELSE NULL END WHERE id = (SELECT id FROM review_conflicts ORDER BY rowid LIMIT 1 OFFSET ({n} % MAX(1, (SELECT COUNT(*) FROM review_conflicts))));
                 UPDATE review_replays SET state = 'resolved', resolved_version = version + 1 WHERE {} AND id = (SELECT id FROM review_replays WHERE state = 'conflicted' ORDER BY rowid LIMIT 1);",
                u8::from(n.is_multiple_of(3))
            ),
            _ => "DELETE FROM host_pulls WHERE rowid = (SELECT MIN(rowid) FROM host_pulls);".to_string(),
        }
    }

    /// Memory against the store: how many answers memory gave, each equal to the store's, for a
    /// viewer in a change's turn, one out of it and none, and through each facet (W26-R02).
    async fn same(store: &crate::store::Store, cache: &ShipCache, now: i64, round: usize) -> anyhow::Result<usize> {
        let mut compared = 0;
        let (kim, sam): (HashSet<String>, HashSet<String>) = (["user:kim".to_string(), "github:kim".to_string()].into(), ["user:sam".to_string()].into());
        let facets: [&[(&str, &str)]; 6] = [&[], &[("kind", "diff")], &[("kind", "request")], &[("kind", "commit")], &[("lens", "review"), ("host", "kiln")], &[("host", "github")]];
        for limit in [2usize, 1000] {
            for viewer in [None, Some(&kim), Some(&sam)] {
                for fq in facets {
                    let mut q = HashMap::from([("repo".to_string(), "hyper-light/kiln".to_string()), ("limit".to_string(), limit.to_string())]);
                    q.extend(fq.iter().map(|(k, v)| (k.to_string(), v.to_string())));
                    let want = list_in(store, &q, None, viewer, runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
                    if let Some(got) = cache.page(Some("hyper-light/kiln"), None, &Facets::from_query(&q), viewer, i64::MAX, limit).map_err(|(_, m)| anyhow::anyhow!(m))? {
                        assert_eq!(serde_json::from_str::<Value>(&got)?, want, "round {round}: the list of {limit} {fq:?} for {viewer:?} from memory isn't the store's");
                        compared += 1;
                    }
                    if !fq.is_empty() {
                        continue;
                    }
                    for c in want.get("commits").and_then(Value::as_array).into_iter().flatten() {
                        let sha = c.get("sha").and_then(Value::as_str).unwrap_or_default();
                        if let Some(got) = cache.detail("hyper-light/kiln", sha, &runners(), now, viewer) {
                            let want = get_in(store, "hyper-light/kiln", sha, viewer, runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
                            assert_eq!(got, want, "round {round}: {sha}'s detail for {viewer:?} from memory isn't the store's");
                            compared += 1;
                        }
                    }
                }
            }
        }
        Ok(compared)
    }

    /// What memory serves is what the store says: random inserts, updates and deletes across every
    /// table the model reads (runs, legs, attempts, images, deployments and their statuses, Deploy
    /// API releases, tags; and review's: diffs, versions, a change request's commits, reviewers,
    /// reviews, threads, comments, the land queue, the hosts' mirrored pull requests, replays and
    /// their conflicts), the list (as three viewers, through each facet)
    /// and every commit's detail compared after each refresh; a cap of 3 so pages past memory come
    /// from the store.
    #[test]
    fn ship_memory_is_what_the_store_says_through_random_changes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let sql = fixture();
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let cache = ShipCache::with_cap(3);
            // The last two have no runs: they are items only as a change's commits.
            let shas = [C1, C2, C3, "4444444ddddddddddddddddddddddddddddddddd", "5555555eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee", "6666666fffffffffffffffffffffffffffffffff", "8888888999999999999999999999999999999999"];
            let mut seed: u64 = 0x5eed_cafe;
            let mut next = move |n: u64| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed % n.max(1)
            };
            let mut now = NOW_S * 1000;
            let mut n = 0u64;
            let mut compared = 0;
            for round in 0..80 {
                for _ in 0..4 {
                    n += 1;
                    let sha = shas.get(usize::try_from(next(7)).unwrap_or(0)).copied().unwrap_or(C1);
                    let sql = ops(n, next(33), sha);
                    store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
                }
                now += FRESH_MS;
                let before = cache.applied();
                cache.refresh(&store, &runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
                assert!(cache.applied() >= before, "the change applied went back");
                compared += same(&store, &cache, now, round).await?;
            }
            assert!(compared >= 400, "memory answered only {compared} times");
            // Nothing noted is left once applied: one row per commit at most while it waits.
            let left: i64 = store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM ship_changes", [], |r| r.get(0))?)).await?;
            assert_eq!(left, 0);
            Ok(())
        })
    }

    /// Refreshes run one at a time and the change applied never goes back, with writes between
    /// and refreshes side by side; memory still says what the store says.
    #[test]
    fn ship_refreshes_side_by_side_lose_nothing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let sql = fixture();
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let cache = ShipCache::new();
            let mut now = NOW_S * 1000;
            cache.refresh(&store, &runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            for n in 0..40u64 {
                let sql = ops(500 + n, n % 31, if n.is_multiple_of(2) { C2 } else { C3 });
                let write = store.call(move |c| Ok(c.execute_batch(&sql)?));
                now += FRESH_MS;
                let before = cache.applied();
                let (ra, rb) = (runners(), runners());
                let (w, a, b) = futures::join!(write, cache.refresh(&store, &ra, &[], now), cache.refresh(&store, &rb, &[], now));
                w?;
                a.map_err(|(_, m)| anyhow::anyhow!(m))?;
                b.map_err(|(_, m)| anyhow::anyhow!(m))?;
                assert!(cache.applied() >= before, "the change applied went back");
                // Whatever a refresh beside the write missed, the next one brings in.
                now += FRESH_MS;
                cache.refresh(&store, &runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
                same(&store, &cache, now, usize::try_from(n).unwrap_or(0)).await?;
            }
            Ok(())
        })
    }

    /// Bounded: a commit changed a hundred times is one row waiting; memory holds at most its cap
    /// and the store answers past it; a failed refresh keeps serving the last good model and the
    /// next read tries again.
    #[test]
    fn ship_stays_bounded_and_a_failed_refresh_keeps_the_last_good_model() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let sql = fixture();
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let cache = ShipCache::with_cap(2);
            let now = NOW_S * 1000;
            cache.refresh(&store, &runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            for _ in 0..100 {
                store.call(|c| Ok(c.execute("UPDATE jobs SET conclusion = CASE conclusion WHEN 'success' THEN 'failure' ELSE 'success' END WHERE id = 'l3'", []).map(|_| ())?)).await?;
            }
            let rows: i64 = store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM ship_changes", [], |r| r.get(0))?)).await?;
            assert_eq!(rows, 1, "a commit changed a hundred times is one row");
            // Two held of three: a page of one comes from memory, a page of ten from the store.
            assert!(cache.page(None, None, &Facets::default(), None, i64::MAX, 1).map_err(|(_, m)| anyhow::anyhow!(m))?.is_some());
            assert!(cache.page(None, None, &Facets::default(), None, i64::MAX, 10).map_err(|(_, m)| anyhow::anyhow!(m))?.is_none());
            let held = cache.page(None, None, &Facets::default(), None, i64::MAX, 1).map_err(|(_, m)| anyhow::anyhow!(m))?;
            // The store failing: the refresh says so, memory keeps what it had, and stays stale.
            store.call(|c| Ok(c.execute_batch("ALTER TABLE ship_changes RENAME TO ship_changes_away")?)).await?;
            assert!(cache.refresh(&store, &runners(), &[], now + FRESH_MS).await.is_err());
            assert_eq!(cache.page(None, None, &Facets::default(), None, i64::MAX, 1).map_err(|(_, m)| anyhow::anyhow!(m))?, held);
            assert!(cache.stale(now + FRESH_MS));
            // Back: the next read brings everything in.
            store.call(|c| Ok(c.execute_batch("ALTER TABLE ship_changes_away RENAME TO ship_changes")?)).await?;
            cache.refresh(&store, &runners(), &[], now + FRESH_MS).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            same(&store, &cache, now + FRESH_MS, 0).await?;
            Ok(())
        })
    }

    /// Callers see only the repositories they may read: memory's pages hold only theirs, and a
    /// commit of another is as missing as one that isn't there.
    #[test]
    fn ship_callers_see_only_the_repositories_they_may_read() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let sql = fixture();
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let cache = ShipCache::new();
            let now = NOW_S * 1000;
            cache.refresh(&store, &runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let count = |page: Option<String>| -> usize { page.and_then(|p| serde_json::from_str::<Value>(&p).ok()).and_then(|v| v.get("commits").and_then(Value::as_array).map(Vec::len)).unwrap_or(usize::MAX) };
            let other = ["someone/else".to_string()];
            let mine = ["hyper-light/kiln".to_string()];
            assert_eq!(count(cache.page(None, Some(&other), &Facets::default(), None, i64::MAX, 10).map_err(|(_, m)| anyhow::anyhow!(m))?), 0);
            assert_eq!(count(cache.page(Some("hyper-light/kiln"), Some(&other), &Facets::default(), None, i64::MAX, 10).map_err(|(_, m)| anyhow::anyhow!(m))?), 0);
            assert_eq!(count(cache.page(None, Some(&mine), &Facets::default(), None, i64::MAX, 10).map_err(|(_, m)| anyhow::anyhow!(m))?), 3);
            let one = get_in(&store, "hyper-light/kiln", C1, None, runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert!(visible(&one, None));
            assert!(visible(&one, Some(&mine)));
            assert!(!visible(&one, Some(&other)));
            assert!(!visible(&one, Some(&[])));
            Ok(())
        })
    }

    /// A change shows within 250 ms: the model is fresh for FRESH_MS, and the first read after
    /// brings in just what the triggers noted.
    #[test]
    fn ship_a_change_shows_within_250ms() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let sql = fixture();
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let cache = ShipCache::new();
            let now = NOW_S * 1000;
            cache.refresh(&store, &runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let stand = |cache: &ShipCache| -> Option<String> {
                let page: Value = serde_json::from_str(&cache.page(None, None, &Facets::default(), None, i64::MAX, 10).ok()??).ok()?;
                page.get("commits")?.as_array()?.iter().find(|c| c.get("short").and_then(Value::as_str) == Some("2222222"))?.get("stand")?.as_str().map(str::to_string)
            };
            assert_eq!(stand(&cache).as_deref(), Some("needs_you"));
            // A person approves 2222222's production release.
            store.call(|c| Ok(c.execute("UPDATE deploy_releases SET state = 'committing' WHERE id = 1", []).map(|_| ())?)).await?;
            assert!(!cache.stale(now + FRESH_MS - 1));
            assert!(cache.stale(now + FRESH_MS));
            let t = std::time::Instant::now();
            cache.refresh(&store, &runners(), &[], now + FRESH_MS).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let took = t.elapsed();
            println!("ship: a change brought in in {took:?}");
            assert_eq!(stand(&cache).as_deref(), Some("in_staging"));
            assert!(took.as_millis() < 250, "bringing a change in took {took:?}");
            Ok(())
        })
    }

    /// Plan step 5's measure: 10,000 runs (2,500 commits of 4 runs, 2 legs each), the model built,
    /// then pages of 1,000 and 100 commits and one commit's detail read from memory. Run in release:
    /// `cargo test --release -p kiln --lib ship_bench -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn ship_bench_ten_thousand_runs_sub_millisecond() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            store
                .call(|c| {
                    let tx = c.savepoint()?;
                    tx.execute("INSERT INTO repos (id, name, clone_url, created_at) VALUES ('kiln', 'hyper-light/kiln', 'https://example.invalid/kiln.git', 1)", [])?;
                    let mut run = tx.prepare("INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, message, author, status, created_at, attempt) VALUES (?1, 'kiln', ?2, ?3, 'push', 'main', ?4, 'a change', 'ada', 'completed', ?5, 1)")?;
                    let mut leg = tx.prepare("INSERT INTO jobs (id, run_id, key, display, status, conclusion, labels, needs, spec, job_id, run_attempt) VALUES (?1, ?2, ?3, ?3, 'completed', 'success', '[]', '[]', '{}', ?3, 1)")?;
                    let mut n = 0i64;
                    for commit in 0..2500i64 {
                        let sha = format!("{commit:07x}{}", "a".repeat(33));
                        for pipeline in ["ci", "e2e", "sim", "release"] {
                            n += 1;
                            let id = format!("r{n}");
                            run.execute(params![id, n, pipeline, sha, NOW_S + n])?;
                            for k in 0..2 {
                                leg.execute(params![format!("{id}l{k}"), id, format!("{pipeline} leg {k}")])?;
                            }
                        }
                    }
                    drop((run, leg));
                    tx.commit()?;
                    Ok(())
                })
                .await?;
            let cache = ShipCache::new();
            let now = NOW_S * 1000;
            let t = std::time::Instant::now();
            cache.refresh(&store, &runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let build = t.elapsed();
            let time = |f: &dyn Fn() -> usize| -> (f64, f64, f64, usize) {
                let mut us: Vec<f64> = Vec::new();
                let mut bytes = 0;
                for _ in 0..500 {
                    let t = std::time::Instant::now();
                    bytes = f();
                    us.push(t.elapsed().as_secs_f64() * 1e6);
                }
                us.sort_by(|a, b| a.total_cmp(b));
                let at = |q: f64| us.get(((us.len() as f64 * q) as usize).min(us.len().saturating_sub(1))).copied().unwrap_or(f64::MAX);
                (at(0.5), at(0.95), at(0.99), bytes)
            };
            let p1000 = time(&|| cache.page(Some("hyper-light/kiln"), None, &Facets::default(), None, i64::MAX, 1000).ok().flatten().map(|s| s.len()).unwrap_or(0));
            let p100 = time(&|| cache.page(Some("hyper-light/kiln"), None, &Facets::default(), None, i64::MAX, 100).ok().flatten().map(|s| s.len()).unwrap_or(0));
            let one = time(&|| cache.detail("hyper-light/kiln", "00004d2", &runners(), now, None).map(|v| v.to_string().len()).unwrap_or(0));
            println!("ship bench: built 2,500 commits from 10,000 runs in {build:?}");
            for (name, (p50, p95, p99, bytes)) in [("limit=1000", p1000), ("limit=100", p100), ("detail", one)] {
                println!("ship bench: {name}: p50 {p50:.1} µs, p95 {p95:.1} µs, p99 {p99:.1} µs, {bytes} bytes");
            }
            assert!(p1000.3 > 0 && p100.3 > 0 && one.3 > 0, "every read answered");
            if !cfg!(debug_assertions) {
                assert!(p1000.2 < 1000.0, "limit=1000 p99 {:.1} µs is 1 ms or more", p1000.2);
                assert!(p100.2 < 200.0, "limit=100 p99 {:.1} µs is 0.2 ms or more", p100.2);
                assert!(one.2 < 200.0, "detail p99 {:.1} µs is 0.2 ms or more", one.2);
            }
            Ok(())
        })
    }

    /// W26-R14: a commit with no change carries the Review lane group's one mark — its push,
    /// revision null, signer null until W27-R03.
    #[test]
    fn w26_r14_a_commit_with_no_change_carries_its_push() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let sql = fixture();
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let one = get_in(&store, "hyper-light/kiln", "2222222", None, runners(), &[], NOW_S * 1000).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let marks = field(&one, &["review"]).and_then(Value::as_array).cloned().unwrap_or_default();
            assert_eq!(marks.len(), 1, "a commit with no change has the one mark: {marks:?}");
            let m = marks.first().cloned().unwrap_or_default();
            assert_eq!((m.get("kind"), m.get("state"), m.get("revision"), m.get("signer")), (Some(&json!("push")), Some(&json!("passed")), Some(&Value::Null), Some(&Value::Null)), "{m}");
            assert!(m.get("end") == m.get("at"), "its push is a point: {m}");
            Ok(())
        })
    }
}
