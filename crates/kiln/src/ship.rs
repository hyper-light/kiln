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
//!
//! Its Deploys, every KilnCD app by project and an opened app's pane, are [`deploys`]'s (W26-R24).

pub mod deploys;

use std::collections::{HashMap, HashSet};

use http::StatusCode;
use kiln_cd::journey::{self, Attempt, Author, Build, Check, Journey, Kinds, Land, Leg, Part, Platform, Production, Replant, Review, ReviewState, Runner, RunnerReplant, Staging};
use kiln_ci::review::{Facts, Kind, Standing};
use kiln_ci::turn;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use crate::http::server::{self as srv, Response};
use crate::hub::Hub;
use crate::wire::events::{ServerEvent, ShipRows};

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
    /// It freezes a job in place (`CAP_FREEZE`, docs/ci.md W26-R22): Pause applies to its jobs.
    pub freeze: bool,
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
    /// Paused since then (W26-R22), and the runner it runs on.
    paused_at: Option<i64>,
    agent: Option<String>,
    /// The provider that runs it when that isn't KilnCI (GitHub Actions, GitLab CI, Bitbucket
    /// Pipelines): kiln keeps only its record.
    provider: Option<String>,
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
    /// The land queue's latest entry with this commit as its candidate: its state, why, and how many
    /// entries are ahead of it in its component.
    land: Option<(String, Option<String>, u32)>,
    own: Vec<DeployRec>,
    current: Vec<DeployRec>,
    releases: Vec<ReleaseRec>,
    /// A newer commit production waits on: its sha, and how far behind it this one is.
    waiting: Option<(String, i64)>,
    riders: u32,
    tags: Vec<String>,
    /// The change this commit is of, and what its review is read from (W26-R01).
    change: Option<ChangeRec>,
    /// What of it was handed to an agent, oldest first (W26-R18).
    handoffs: Vec<HandoffRec>,
}

/// A Hand off of a commit (W26-R18, migration 20261029-0100): `checks`, `threads` or `conflicts`,
/// who took it (an agent's or an attachment's name) and, for a change's, its revision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct HandoffRec {
    what: String,
    to: String,
    version: Option<u32>,
    /// Why kiln handed it, or held it (W26-R18); none for a person's.
    why: Option<String>,
}

/// An open thread on a change: its kind, where it is, who opened it and its first words.
#[derive(Debug, Clone, Default)]
struct ThreadRec {
    /// Its id, and the revision it was opened on (W26-R18: Hand off names each).
    id: String,
    version: u32,
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
    /// Its revisions that fix: an agent's fix or a suggestion applied, by why or by the thread
    /// they answer (W26-R04, R18).
    fixes: Vec<u32>,
    threads: Vec<ThreadRec>,
    replies: Vec<(String, i64)>,
    /// The land queue's latest entry landing this change: its state, why, and how many entries are
    /// ahead of it in its component (docs/ci.md W26-R19).
    queue: Option<(String, Option<String>, u32)>,
    /// The conflicts its latest revision's replay onto where it lands left, each holding its gate
    /// until Commit resolution makes a revision of them (W26-R12).
    conflicts: Vec<kiln_ci::review::Conflict>,
    /// The Review lane group's marks, oldest first (W26-R14).
    marks: Vec<MarkRec>,
    /// A host's pull request kiln opened for an agent (`agents.change`): that agent's session
    /// (W26-R18 drafts). A kiln change names its agent as its author.
    by_agent: Option<String>,
    /// How fresh it is on its trunk, from its latest version (W26-R16): a replay of kiln's says what
    /// it went onto. None when its latest isn't a replay.
    fresh: Option<String>,
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
    let trunk: String = c.query_row("SELECT trunk FROM diffs WHERE id = ?1", [&id], |r| r.get(0)).unwrap_or_default();
    let mut latest_replay: Option<(Option<String>, String)> = None;
    let mut st = c.prepare("SELECT n, commit_sha, rebase_only, created_at, message, pushed_by, why, thread, base FROM diff_versions WHERE diff_id = ?1 ORDER BY n")?;
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
            r.get::<_, String>(8)?,
        ))
    })? {
        let (v, commit, rebase_only, at, message, pushed_by, why, thread, base) = row?;
        latest_replay = Some((why.clone(), base));
        if v == ch.n {
            (ch.message, ch.at) = (message, at);
        }
        ch.pushes.push((v, rebase_only, at));
        (ch.latest, ch.latest_sha) = (v, commit);
        // A revision is the lane's push, the fix answering a thread (an agent's fix, a suggestion
        // applied), the resolution of a replay's conflicts, or kiln's replay as its trunk moved
        // (W26-R16), by which `why` it was made (W26-R14). A replay kept with its conflicts is its
        // replay's mark, below.
        let kind = match why.as_deref() {
            Some("conflict resolved") => "resolution",
            Some(w) if w == kiln_ci::fresh::WHY => "replay",
            Some(w) if w == kiln_ci::fresh::WHY_CONFLICTED => "",
            Some(_) => "fix",
            None if thread.is_some() => "fix",
            None => "push",
        };
        if kind == "fix" {
            ch.fixes.push(v);
        }
        if !kind.is_empty() {
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
    }
    // How fresh it is (W26-R16), as the change page says it, from what the store has: a replay of
    // kiln's names the trunk tip it went onto (its base).
    let on = |base: &str| format!("{trunk}@{}", base.get(..6).unwrap_or(base));
    ch.fresh = match latest_replay {
        Some((Some(w), base)) if w == kiln_ci::fresh::WHY => Some(format!("replays clean on {}", on(&base))),
        Some((Some(w), base)) if w == kiln_ci::fresh::WHY_CONFLICTED => Some(format!("conflicts on {}", on(&base))),
        _ => None,
    };
    let mut st = c.prepare("SELECT reviewer, added_at FROM diff_reviewers WHERE diff_id = ?1 ORDER BY added_at, reviewer")?;
    ch.reviewers = st.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
    let mut st = c.prepare("SELECT reviewer, action, created_at, body, version FROM reviews WHERE diff_id = ?1 ORDER BY created_at, rowid")?;
    ch.reviews = st
        .query_map([&id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, Some(r.get::<_, u32>(4)?))))?
        .collect::<Result<_, _>>()?;
    approval_marks(&mut marks, &id, &ch.reviewers, &ch.reviews);
    let mut st = c.prepare(
        "SELECT t.kind, t.file, t.start_line, t.created_by, (SELECT c.body FROM review_comments c WHERE c.thread_id = t.id AND c.published = 1 ORDER BY c.created_at, c.rowid LIMIT 1), t.id, t.version
         FROM review_threads t WHERE t.diff_id = ?1 AND t.state = 'open' AND EXISTS (SELECT 1 FROM review_comments c WHERE c.thread_id = t.id AND c.published = 1) ORDER BY t.created_at, t.id",
    )?;
    ch.threads = st
        .query_map([&id], |r| Ok(ThreadRec { kind: r.get(0)?, file: r.get(1)?, line: r.get(2)?, by: r.get(3)?, said: r.get::<_, Option<String>>(4)?.unwrap_or_default(), id: r.get(5)?, version: r.get(6)? }))?
        .collect::<Result<_, _>>()?;
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
            "SELECT l.state, l.reason, MAX(COALESCE(l.position, 1) - 1, 0) FROM land_queue l WHERE l.repo_id = ?1 AND EXISTS (SELECT 1 FROM json_each(CASE WHEN json_valid(l.diffs) THEN l.diffs ELSE '[]' END) j WHERE j.value = ?2) ORDER BY l.updated_at DESC, l.rowid DESC LIMIT 1",
            params![repo_id, id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
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
    // The agent kiln opened it for, as its change record names it (agents.change, W26-R18).
    let by_agent: Option<String> = c
        .query_row(
            "SELECT json_extract(v, '$.agent') FROM kv WHERE k LIKE 'agents/change/%' AND json_extract(v, '$.provider') = ?2 AND json_extract(v, '$.change') = ?3
               AND json_extract(v, '$.repo') = (SELECT name FROM repos WHERE id = ?1) LIMIT 1",
            params![repo_id, host, format!("#{number}")],
            |r| r.get(0),
        )
        .optional()?;
    // How fresh it is (W26-R16): its latest head's latest replay onto its base, in kiln's mirror.
    let fresh = c
        .query_row(
            "SELECT r.state, r.onto, p.base FROM review_pull_replays r JOIN host_pulls p ON p.repo_id = r.repo_id AND p.host = r.host AND p.number = r.number
              WHERE r.repo_id = ?1 AND r.host = ?2 AND r.number = ?3 AND r.head_sha = ?4 ORDER BY r.k DESC LIMIT 1",
            params![repo_id, host, number, latest_sha],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
        )
        .optional()?
        .map(|(state, onto, base)| {
            let at = format!("{base}@{}", onto.get(..6).unwrap_or(&onto));
            if state == "clean" { format!("replays clean on {at}") } else { format!("conflicts on {at}") }
        });
    Ok(ChangeRec {
        fresh,
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
        // Its host keeps no why for a head, so none reads as a fix.
        fixes: Vec::new(),
        threads,
        replies: Vec::new(),
        // The entry that lands it, kiln's or its host's queue's (docs/ci.md W26-R19, W28-R109).
        queue: c
            .query_row(
                "SELECT l.state, l.reason, MAX(COALESCE(l.position, 1) - 1, 0) FROM land_queue l WHERE l.repo_id = ?1 AND l.host IS NOT NULL
                 AND EXISTS (SELECT 1 FROM json_each(CASE WHEN json_valid(l.host) THEN l.host ELSE '{}' END, '$.changes') j WHERE json_extract(j.value, '$.number') = ?2)
                 ORDER BY l.updated_at DESC, l.rowid DESC LIMIT 1",
                params![repo_id, number],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?,
        // A host's pull request is replayed on its host, not here.
        conflicts: Vec::new(),
        marks,
        by_agent,
    })
}

/// A change's review as its tip carries it (W26-R02): whose turn it is folded from its records
/// (`kiln_ci::turn`, W26-R05), its state read from its facts (`kiln_ci::review::state`). An older
/// version's head stands superseded; a change request's other commits, and an abandoned change's,
/// carry none.
/// The review of the change `ch` on its commit `sha`; `handed`, its threads or conflicts handed to
/// its author's agent at this revision (W26-R18), which reads fixing while the revision is live;
/// `held`, agents' feedback kiln kept from its ended agent at this revision, whose why the review
/// says.
fn review_of(ch: &ChangeRec, sha: &str, handed: Option<&HandoffRec>, held: Option<&HandoffRec>) -> Option<Review> {
    if !ch.head || ch.state == "abandoned" {
        return None;
    }
    // Handed off at this revision and replaced by a fix since (W26-R18): which version fixed it.
    let fixed_in = handed.filter(|_| sha != ch.tip()).and_then(|_| ch.fixes.iter().copied().filter(|v| *v > ch.n).min()).map(|v| format!("v{v}"));
    let handed = handed.filter(|_| sha == ch.tip());
    let held = held.filter(|_| sha == ch.tip()).and_then(|h| h.why.clone());
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
        fixing: handed.is_some(),
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
    // What its agent was handed: the file in conflict, or whose thread (how many, when several).
    let handed_about = || match (handed.map(|h| h.what.as_str()), ch.threads.as_slice()) {
        (Some("conflicts"), _) => ch.conflicts.first().map(|k| kiln_ci::review::place(Some(&k.file), k.line)).unwrap_or_else(|| "its conflicts".to_string()),
        (_, [one]) => format!("{}'s thread", shown(&one.by)),
        (_, many) => format!("{} threads", many.len()),
    };
    let about = match state {
        ReviewState::Blocked => ch.threads.iter().rev().find(|t| Kind::read(&t.kind) == Kind::Blocking).map(|t| match (&t.file, t.line) {
            (Some(f), Some(l)) => format!("{}:{l}", f.rsplit('/').next().unwrap_or(f)),
            (Some(f), None) => f.rsplit('/').next().unwrap_or(f).to_string(),
            _ => said(t),
        }),
        ReviewState::Changes => ch.reviews.iter().rev().find(|r| r.1 == "request_changes").map(|r| first_words(&r.3)),
        ReviewState::Conflict => ch.conflicts.first().map(|k| kiln_ci::review::place(Some(&k.file), k.line)),
        ReviewState::Requested => ch.threads.last().map(said),
        ReviewState::Fixing => Some(handed_about()),
        // A draft stays a draft (W26-R02), and says who is at work on it (W26-AC10).
        ReviewState::Draft => handed.map(|h| format!("{} fixing · {}", h.to, handed_about())),
        _ => None,
    }
    .unwrap_or_default();
    // What kiln held, and why: the fix waits for a person (W26-R18).
    let about = held.filter(|_| state != ReviewState::Fixing).unwrap_or(about);
    let tip = ch.tip();
    let superseded_by = (sha != tip).then(|| if ch.n < ch.latest { format!("v{}", ch.latest) } else { short(tip) });
    Some(Review { state, turn: whose, author, open, conflicts: gate.conflicts, fixing: handed.map(|h| h.to.clone()), about, superseded_by, fixed_in, fresh: ch.fresh.clone() })
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
        "SELECT j.id, j.run_id, COALESCE(NULLIF(j.display, ''), j.key), j.status, j.conclusion, j.reused_from IS NOT NULL, j.error, j.started_at, j.finished_at, j.paused_at, j.agent,
                NULLIF(r.provider, 'kiln')
         FROM jobs j JOIN runs r ON r.id = j.run_id WHERE r.repo_id = ?1 AND r.sha = ?2 AND (j.job_id IS NULL OR j.run_attempt = r.attempt) ORDER BY j.rowid",
    )?;
    let legs: Vec<(String, LegRec)> = st
        .query_map(params![repo_id, sha], |r| {
            Ok((
                r.get::<_, String>(0)?,
                LegRec {
                    job: r.get(0)?,
                    run_id: r.get(1)?,
                    name: r.get(2)?,
                    status: r.get(3)?,
                    conclusion: r.get(4)?,
                    reused: r.get(5)?,
                    error: r.get(6)?,
                    started: r.get(7)?,
                    finished: r.get(8)?,
                    attempts: Vec::new(),
                    paused_at: r.get(9)?,
                    agent: r.get::<_, Option<String>>(10)?.filter(|a| !a.is_empty() && a != "-"),
                    provider: r.get(11)?,
                },
            ))
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
        .query_row("SELECT state, reason, MAX(COALESCE(position, 1) - 1, 0) FROM land_queue WHERE repo_id = ?1 AND candidate = ?2 ORDER BY updated_at DESC LIMIT 1", params![repo_id, sha], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
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
    let mut st = c.prepare("SELECT what, to_name, version, why FROM ship_handoffs WHERE repo_id = ?1 AND sha = ?2 ORDER BY at, id")?;
    rec.handoffs = st.query_map(params![repo_id, sha], |r| Ok(HandoffRec { what: r.get(0)?, to: r.get(1)?, version: r.get(2)?, why: r.get(3)? }))?.collect::<Result<_, _>>()?;
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
    let land = match queue.map(|(s, why, ahead)| (s.as_str(), why.clone().unwrap_or_default(), *ahead)) {
        Some(("testing", ..)) => Land::Testing,
        Some(("queued", _, ahead)) => Land::Queued { ahead },
        Some(("ejected", why, _)) => Land::Ejected { why },
        // Waiting on Review in the queue, or back with it on a conflict (docs/ci.md W26-R19, W28-R107).
        Some(("held", why, _)) => Land::Held { why },
        Some(("returned", why, _)) => Land::Returned { why },
        _ if change.is_some_and(|ch| !ch.is_landed()) => Land::NotYet,
        _ => Land::Landed,
    };
    let run_pipeline: HashMap<&str, &str> = rec.runs.iter().map(|r| (r.id.as_str(), r.pipeline.as_str())).collect();
    let run_made: HashMap<&str, i64> = rec.runs.iter().map(|r| (r.id.as_str(), r.created_at)).collect();
    let mut checks: Vec<Check> = Vec::new();
    // Each platform once, as the newest release run that builds it has it (a release run again
    // replaces the one before, as a leg tried again keeps its lane), beside when that run was made.
    let mut releases: Vec<(i64, Platform)> = Vec::new();
    let mut image_parts: Vec<Part> = Vec::new();
    for leg in &rec.legs {
        let pipeline = run_pipeline.get(leg.run_id.as_str()).copied().unwrap_or_default();
        // A leg names its job and run once it has run (Output streams that job's log).
        let ran = !matches!(leg.status.as_str(), "queued" | "waiting" | "pending" | "blocked" | "skipped");
        if is_release(pipeline) {
            let made = run_made.get(leg.run_id.as_str()).copied().unwrap_or_default();
            let p = Platform {
                name: leg.name.clone(),
                state: part_of(&leg.status, leg.conclusion.as_deref()),
                job: ran.then(|| leg.job.clone()),
                run: ran.then(|| leg.run_id.clone()),
                start: leg.started,
                end: leg.finished,
            };
            match releases.iter_mut().find(|(_, q)| q.name == p.name) {
                Some(slot) if slot.0 <= made => *slot = (made, p),
                Some(_) => {}
                None => releases.push((made, p)),
            }
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
        let l = Leg { name: leg.name.clone(), attempts, note: leg.error.clone().unwrap_or_default(), job: ran.then(|| leg.job.clone()), run: ran.then(|| leg.run_id.clone()), start: leg.started, end: leg.finished };
        match checks.iter_mut().find(|c| c.pipeline == pipeline) {
            Some(c) => c.legs.push(l),
            None => checks.push(Check { pipeline: pipeline.to_string(), legs: vec![l] }),
        }
    }
    let platforms: Vec<Platform> = releases.into_iter().map(|(_, p)| p).collect();
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

    // Handed off (W26-R18): its failure to whoever took it; its review to its author's agent, at this revision.
    let handled_by = rec.handoffs.iter().rev().find(|h| h.what == "checks").map(|h| h.to.clone());
    // The newest at this revision counts: kiln holding agents' feedback for a person, or a hand-off.
    let live = change.and_then(|ch| rec.handoffs.iter().rev().find(|h| h.what != "checks" && h.version.is_none_or(|v| v == ch.n)));
    let (handed, held) = match live {
        Some(h) if h.what == HELD => (None, Some(h)),
        h => (h, None),
    };
    let review = change.and_then(|ch| review_of(ch, &rec.sha, handed, held));
    Journey { author, land, checks, build, staging, production, replant, in_production, handled_by, fixed_by: None, review }
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
/// API's approval); hand a failed check to an agent ([`hand_off`]: while no one has it) or retry it
/// (the run's rerun, its failed legs only); retry a runner that didn't replant (its upgrade,
/// again). A change whose turn is yours: hand its threads or conflicts to its author's agent, or
/// review it; while that agent fixes it, neither.
fn actions(rec: &Records, j: &Journey, yours: bool) -> Vec<Value> {
    let act = |name: &str, method: &str, path: String, body: Value| json!({ "name": name, "method": method, "path": path, "body": body });
    let hand = || act("hand_off", "POST", "/api/ship/hand-off".to_string(), json!({ "repo": rec.repo, "sha": rec.sha }));
    let mut out = Vec::new();
    if let Some(r) = rec.releases.iter().find(|r| r.state == "waiting") {
        out.push(act("approve", "POST", format!("/api/deploy/releases/{}/approve", r.id), json!({})));
        out.push(act("hold", "POST", format!("/api/deploy/releases/{}/reject", r.id), json!({})));
    }
    if let Some(leg) = failed_leg(rec) {
        if j.handled_by.is_none() {
            out.push(hand());
        }
        out.push(act("retry", "POST", format!("/api/ci/runs/{}/rerun", leg.run_id), json!({ "mode": "failed" })));
    }
    if let (true, Some(r)) = (j.in_production, j.replant.as_ref()) {
        for x in r.runners.iter().filter(|x| x.state == Runner::Failed) {
            out.push(act("retry_runner", "POST", format!("/api/updates/runners/{}", x.name), Value::Null));
        }
    }
    // W26-R02's Hand off and Review pair, for the one whose turn it is.
    if let (true, Some(ch), Some(r)) = (yours, rec.change.as_ref(), j.review.as_ref())
        && r.superseded_by.is_none()
        && r.state != ReviewState::Fixing
    {
        if made_by(rec).agent.is_some() && (r.conflicts > 0 || !ch.threads.is_empty()) && !out.iter().any(|a| a.get("name").and_then(Value::as_str) == Some("hand_off")) {
            out.push(hand());
        }
        out.push(act("review", "GET", format!("/api/diffs/{}", ch.name), Value::Null));
    }
    // An agent's draft, marked ready for review by its person or one who may land (W26-R18): a draft
    // has no turn, so it's on the row for whoever looks, and the endpoint says who may.
    if let (Some(ch), Some(r)) = (rec.change.as_ref(), j.review.as_ref())
        && r.superseded_by.is_none()
        && r.state == ReviewState::Draft
        && (authors_agent(ch).is_some() || ch.by_agent.is_some())
    {
        out.push(act("ready", "POST", "/api/ship/ready".to_string(), json!({ "repo": rec.repo, "sha": rec.sha })));
    }
    out
}

/// The commit's first failed check (not a release's platform): what Retry and Hand off are about.
fn failed_leg(rec: &Records) -> Option<&LegRec> {
    rec.legs.iter().find(|l| matches!(attempt_of(&l.status, l.conclusion.as_deref(), None), Attempt::Failed) && !is_release(rec.runs.iter().find(|r| r.id == l.run_id).map(|r| r.pipeline.as_str()).unwrap_or_default()))
}

/// The agent that wrote a change (`agent:<id>`, as its agent token pushed it), if an agent did.
fn authors_agent(ch: &ChangeRec) -> Option<&str> {
    ch.author.strip_prefix("agent:").and_then(|s| s.split(' ').next()).filter(|s| !s.is_empty())
}

/// Who made a commit, as far as kiln knows: the agent session that made it, and the person it traces
/// back to (a kiln principal, `user:ada`). Ship asks this one place: Hand off goes to `agent`
/// (W26-R18), Files' ask asks it (the UI reads `made_by`), and a commit with neither is a person's
/// or no one's kiln knows. Today it's a kiln change's author and author_for; for a lone commit,
/// production's runs record only git's author name, so nothing. Chain of Ownership (Ada,
/// 2026-10-29: any conversation or action traced back to its human initiator) fills it for every
/// commit, and what reads it follows unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MadeBy {
    agent: Option<String>,
    person: Option<String>,
}

fn made_by(rec: &Records) -> MadeBy {
    match rec.change.as_ref() {
        Some(ch) => match authors_agent(ch) {
            Some(agent) => MadeBy { agent: Some(agent.to_owned()), person: ch.author_for.clone().filter(|p| !p.is_empty()) },
            None => MadeBy { agent: None, person: Some(ch.author.clone()).filter(|p| !p.is_empty()) },
        },
        None => MadeBy::default(),
    }
}

impl MadeBy {
    /// Who made one of the conflict ladder's tries (docs/ci.md W26-R17), from its actor's
    /// principal: an agent's (`agent:<id> for user:kim`) names the agent and the person it acts for;
    /// a person's (`user:kim`) the person; kiln's own (its structural merge, a remembered resolution)
    /// neither, its rung saying who. A try isn't a commit until it's accepted; the revision it
    /// becomes then reads its maker through [`made_by`], as every commit does.
    fn of_actor(principal: &str) -> MadeBy {
        if let Some(rest) = principal.strip_prefix("agent:") {
            let mut words = rest.split(' ');
            let agent = words.next().filter(|a| !a.is_empty()).map(str::to_owned);
            let person = match (words.next(), words.next()) {
                (Some("for"), Some(p)) if !p.is_empty() => Some(p.to_owned()),
                _ => None,
            };
            return MadeBy { agent, person };
        }
        if principal.starts_with("user:") {
            return MadeBy { agent: None, person: Some(principal.to_owned()) };
        }
        MadeBy::default()
    }

    /// `{agent?, person?}` (optional fields, W26-R18), or null when kiln knows neither.
    fn json(&self) -> Value {
        if self.agent.is_none() && self.person.is_none() {
            return Value::Null;
        }
        let mut v = serde_json::Map::new();
        if let Some(a) = &self.agent {
            v.insert("agent".into(), json!(a));
        }
        if let Some(p) = &self.person {
            v.insert("person".into(), json!(p));
        }
        Value::Object(v)
    }
}

/// [`MadeBy::of_actor`] as the API shows it: `{agent?, person?}`, or null.
pub(crate) fn made_by_actor(principal: &str) -> Value {
    MadeBy::of_actor(principal).json()
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
    // When its jobs ran, the first one's start to the last one's end (`to` null while one runs), and
    // the machines they ran on in the order they started: each commit's line in a repository's
    // commits says them (docs/ci.md W26-R29). Times, not a running count, so the row's JSON (and its
    // version) holds still while a job runs.
    let running = rec.legs.iter().any(|l| l.started.is_some() && l.finished.is_none());
    let ran = rec.legs.iter().filter_map(|l| l.started).min().map(|from| json!({ "from": from, "to": if running { None } else { rec.legs.iter().filter_map(|l| l.finished).max() } }));
    let mut by_start: Vec<&LegRec> = rec.legs.iter().filter(|l| l.agent.is_some()).collect();
    by_start.sort_by_key(|l| l.started.unwrap_or(i64::MAX));
    let mut machines: Vec<&str> = Vec::new();
    for a in by_start.iter().filter_map(|l| l.agent.as_deref()) {
        if !machines.contains(&a) {
            machines.push(a);
        }
    }
    let row = json!({
        "repo": rec.repo, "sha": rec.sha, "short": short(&rec.sha),
        "ran": ran, "machines": machines,
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
        "made_by": made_by(rec).json(),
        "actions": actions(rec, j, yours),
    });
    with_version(row)
}

/// The row with `v`: a hash of the JSON it shows (FNV-1a, 64 bits, in hex), the same from memory
/// and from the store, so the page can keep a row that hasn't changed as it is (W26-R06).
fn with_version(mut row: Value) -> Value {
    let v = version_of(&row.to_string());
    if let Some(o) = row.as_object_mut() {
        o.insert("v".into(), json!(v));
    }
    row
}

/// FNV-1a, 64 bits, in hex, of `text`: a row's `v` and a detail's `dv`.
fn version_of(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// A commit rendered once for everyone and once for those whose turn it is (when anyone's).
struct Rendered {
    others: Value,
    yours: Option<Value>,
    whose: Vec<String>,
    /// Where it stands, and the agent behind it when one is: what the pace line counts.
    stand: journey::Stand,
    agent: Option<String>,
    /// Its detail's parts and their `dv`, when asked for (`track`): someone reads that detail.
    tracked: Option<Tracked>,
}

/// What a commit's detail shows beyond its row, and its version (`dv`), held for a commit whose
/// detail a page read (W21-R15): the next refresh says how its parts changed from these.
#[derive(Debug, Clone, PartialEq)]
struct Tracked {
    dv: String,
    parts: serde_json::Map<String, Value>,
}

impl Tracked {
    fn of(parts: serde_json::Map<String, Value>) -> Self {
        Self { dv: parts_version(&parts), parts }
    }
}

fn render(rec: &Records, runners: &[RunnerSeen], now: i64, track: bool) -> Rendered {
    let j = journey_of(rec, runners, now);
    let whose = whose(&j);
    let yours = (!whose.is_empty()).then(|| summary_of(rec, &j, now, true));
    let stand = journey::standing(&j, now).stand;
    let agent = match &j.author {
        Author::Agent { name, .. } => Some(name.clone()),
        Author::Person(_) => None,
    };
    let tracked = track.then(|| Tracked::of(detail_parts(rec, &j, runners)));
    Rendered { others: summary_of(rec, &j, now, false), yours, whose, stand, agent, tracked }
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

/// A platform of the build as the detail shows it, as a leg is: `job`, `run` and its times only
/// once it has them (optional new fields; its Output and its lane, W26-R23).
fn platform_json(p: &journey::Platform, state: &str) -> Value {
    let mut v = json!({ "name": p.name, "state": state });
    if let Some(o) = v.as_object_mut() {
        if let Some(job) = &p.job {
            o.insert("job".into(), json!(job));
        }
        if let Some(run) = &p.run {
            o.insert("run".into(), json!(run));
        }
        if let Some(start) = p.start {
            o.insert("start".into(), json!(start));
        }
        if let Some(end) = p.end {
            o.insert("end".into(), json!(end));
        }
    }
    v
}

/// What a person can do to one of a commit's jobs from its row (docs/ci.md W26-R21, R22), through
/// `POST /api/ship/jobs`: each verb that applies now, and why each other doesn't, as that endpoint
/// would answer. Another provider's job is that provider's: none applies here.
fn job_verbs(leg: &LegRec, runners: &[RunnerSeen]) -> (Vec<&'static str>, serde_json::Map<String, Value>) {
    if let Some(p) = &leg.provider {
        let why = Value::String(format!("{} runs it, not kiln", crate::ci::retry::provider_says(p)));
        return (Vec::new(), ["retry", "cancel", "pause", "resume"].into_iter().map(|v| (v.to_owned(), why.clone())).collect());
    }
    let status = crate::store::Status::parse(&leg.status);
    let paused = leg.paused_at.is_some();
    let ended = status.is_terminal() || matches!(leg.status.as_str(), "completed");
    let running = status == crate::store::Status::Running && !ended;
    let freezes = leg.agent.as_deref().is_none_or(|a| runners.iter().any(|r| r.name == a && r.freeze));
    let mut can = Vec::new();
    let mut why = serde_json::Map::new();
    let mut say = |verb: &'static str, no: Option<String>| match no {
        None => can.push(verb),
        Some(w) => {
            why.insert(verb.to_owned(), Value::String(w));
        }
    };
    let finished = || Some("it has finished".to_owned());
    say("retry", if !ended { Some("it hasn't finished".to_owned()) } else if leg.status == "skipped" { Some("it was skipped: retry what it needs".to_owned()) } else { None });
    say("cancel", if ended { finished() } else { None });
    let pause = if ended {
        finished()
    } else if paused {
        Some("it's paused".to_owned())
    } else if running && !freezes {
        Some(format!("its runner {} can't freeze a job in place", leg.agent.as_deref().unwrap_or_default()))
    } else {
        None
    };
    say("pause", pause);
    say("resume", if ended { finished() } else if !paused { Some("it isn't paused".to_owned()) } else { None });
    (can, why)
}

/// Every job of a commit, either engine's, as the jobs pane acts on them (W26-R21, R22): its id
/// and run, its pipeline and name (a lane's), the runner it runs on, whether it's paused, and the
/// verbs that apply.
fn jobs_json(rec: &Records, runners: &[RunnerSeen]) -> Vec<Value> {
    let pipeline: HashMap<&str, &str> = rec.runs.iter().map(|r| (r.id.as_str(), r.pipeline.as_str())).collect();
    rec.legs
        .iter()
        .map(|l| {
            let (can, why) = job_verbs(l, runners);
            let mut v = json!({ "id": l.job, "run": l.run_id, "pipeline": pipeline.get(l.run_id.as_str()).copied().unwrap_or_default(), "name": l.name, "verbs": can, "why_not": why });
            if let Some(o) = v.as_object_mut() {
                if let Some(a) = &l.agent {
                    o.insert("runner".into(), json!(a));
                }
                if let Some(at) = l.paused_at {
                    o.insert("paused_at".into(), json!(at));
                }
            }
            v
        })
        .collect()
}

/// A commit with every record its stand came from, as `viewer` sees it: its row, what the detail
/// shows beyond it, and `dv`, that part's version (W21-R15: a `ship` frame of the same `dv` changes
/// only the row's part).
fn detail(rec: &Records, runners: &[RunnerSeen], now: i64, viewer: Option<&HashSet<String>>) -> Value {
    detail_tracked(rec, runners, now, viewer).0
}

/// [`detail`], and what it shows beyond its row as [`Tracked`].
fn detail_tracked(rec: &Records, runners: &[RunnerSeen], now: i64, viewer: Option<&HashSet<String>>) -> (Value, Tracked) {
    let j = journey_of(rec, runners, now);
    let mut v = summary_of(rec, &j, now, holds(&whose(&j), viewer));
    let t = Tracked::of(detail_parts(rec, &j, runners));
    if let Some(o) = v.as_object_mut() {
        o.extend(t.parts.clone());
        o.insert("dv".into(), json!(t.dv));
    }
    (v, t)
}

/// How a detail's parts went from `old` to `new` (W21-R15): the parts that changed, each whole, or,
/// for a list (its jobs, its checks), its length and the entries that changed, whichever is
/// shorter. A page holding `old` that lays them over it holds `new`. None when a part went: only
/// loading it again says that.
fn parts_delta(old: &serde_json::Map<String, Value>, new: &serde_json::Map<String, Value>) -> Option<(std::collections::BTreeMap<String, Value>, std::collections::BTreeMap<String, crate::wire::events::ShipItems>)> {
    use crate::wire::events::{ShipItem, ShipItems};
    if old.keys().any(|k| !new.contains_key(k)) {
        return None;
    }
    let (mut parts, mut items) = (std::collections::BTreeMap::new(), std::collections::BTreeMap::new());
    for (k, v) in new {
        let was = old.get(k);
        if was == Some(v) {
            continue;
        }
        if let (Some(Value::Array(a)), Value::Array(b)) = (was, v) {
            let at: Vec<ShipItem> = b.iter().enumerate().filter(|(i, x)| a.get(*i) != Some(*x)).map(|(i, x)| ShipItem { i, v: x.clone() }).collect();
            let some = ShipItems { len: b.len(), at };
            let shorter = serde_json::to_string(&some).map(|t| t.len()).unwrap_or(usize::MAX) < serde_json::to_string(v).map(|t| t.len()).unwrap_or(0);
            if shorter {
                items.insert(k.clone(), some);
                continue;
            }
        }
        parts.insert(k.clone(), v.clone());
    }
    Some((parts, items))
}

/// The version of what a detail shows beyond its row: the same parts, the same `dv`, for every viewer.
fn parts_version(parts: &serde_json::Map<String, Value>) -> String {
    version_of(&serde_json::to_string(parts).unwrap_or_default())
}

/// What a commit's detail shows beyond its row: its review marks, checks, build, deployments,
/// releases, replant and jobs.
fn detail_parts(rec: &Records, j: &Journey, runners: &[RunnerSeen]) -> serde_json::Map<String, Value> {
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
        "platforms": j.build.platforms.iter().map(|p| platform_json(p, part(p.state))).collect::<Vec<_>>(),
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
    let mut o = serde_json::Map::new();
    o.insert("review".into(), json!(marks.iter().map(mark_json).collect::<Vec<_>>()));
    o.insert("checks".into(), json!(checks));
    o.insert("build".into(), build);
    o.insert("deployments".into(), json!(deployments));
    o.insert("releases".into(), json!(rec.releases.iter().map(|r| json!({ "state": r.state, "message": r.message, "rolled_back": r.rolled_back })).collect::<Vec<_>>()));
    o.insert("replant".into(), json!(replant));
    o.insert("jobs".into(), json!(jobs_json(rec, runners)));
    o
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
    // The page's rows drawn on the compute pool, a commit a unit (goal 3, plan 01a1a59c2a8e step 5),
    // not on the request's shard.
    let viewer = viewer.cloned();
    let page: Vec<Value> = crate::rt::compute_map(recs.into_iter().take(usize::try_from(limit).unwrap_or(30)).collect(), move |r| {
        let x = render(&r, &runners, now, false);
        if holds(&x.whose, viewer.as_ref()) { x.yours.unwrap_or(x.others) } else { x.others }
    })
    .await
    .map_err(internal)?;
    let next = if more { page.last().and_then(|c| c.get("cursor")).cloned() } else { None };
    Ok(json!({ "commits": page, "next_cursor": next }))
}

/// Each repository's newest commit in view, newest first, from the store (`by=repo`, when memory
/// can't say: docs/ci.md W26-R29). Each is the first of its repository's own list, so it's the
/// commit that list shows on top, with the same facets.
pub async fn heads_in(store: &crate::store::Store, q: &HashMap<String, String>, only: Option<String>, viewer: Option<&HashSet<String>>, runners: Vec<RunnerSeen>, deploys: &[CdDeploy], now: i64) -> Result<Value, Fail> {
    let names: Option<Vec<String>> = only.as_deref().map(|o| serde_json::from_str(o).unwrap_or_default());
    let repos = store.repos().await.map_err(internal)?;
    let mut heads: Vec<Value> = Vec::new();
    for r in repos.iter().filter(|r| names.as_ref().is_none_or(|n| n.contains(&r.name))) {
        let mut one = q.clone();
        one.insert("repo".into(), r.name.clone());
        one.insert("limit".into(), "1".into());
        one.remove("cursor");
        let page = list_in(store, &one, only.clone(), viewer, runners.clone(), deploys, now).await?;
        if let Some(c) = page.get("commits").and_then(Value::as_array).and_then(|a| a.first()) {
            heads.push(c.clone());
        }
    }
    heads.sort_by_key(|c| std::cmp::Reverse(c.get("cursor").and_then(Value::as_i64).unwrap_or(0)));
    Ok(json!({ "commits": heads, "next_cursor": null }))
}

/// A page's JSON as the list sends it: its commits' JSON as held, and where the next page starts.
fn page_body(page: &[(i64, &str)], next: Option<i64>) -> String {
    let mut out = String::with_capacity(page.iter().map(|(_, j)| j.len() + 1).sum::<usize>() + 48);
    out.push_str("{\"commits\":[");
    for (i, (_, json)) in page.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(json);
    }
    out.push_str("],\"next_cursor\":");
    match next {
        Some(f) => out.push_str(&f.to_string()),
        None => out.push_str("null"),
    }
    out.push('}');
    out
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
    stand: journey::Stand,
    agent: Option<String>,
    /// What time moves without a record changing: its guard or its soak runs down. Drawn again at
    /// each refresh, as production's commit is.
    moves: bool,
    /// Its detail's parts and `dv`, once someone read that detail from memory (W21-R15): from then
    /// on each row drawn for the commit works them out too, and a `ship` frame says how they
    /// changed. Unread details cost nothing.
    tracked: std::sync::OnceLock<Tracked>,
}

impl Row {
    fn of(rec: Records, runners: &[RunnerSeen], now: i64, track: bool) -> Result<Self, Fail> {
        let x = render(&rec, runners, now, track);
        let text = |v: &Value| serde_json::to_string(v).map_err(|e| internal(e.into()));
        let (others, yours) = (text(&x.others)?, x.yours.as_ref().map(text).transpose()?);
        let (kind, host) = (kind_of(&rec), rec.change.as_ref().map(|c| c.host.clone()));
        let moves = others.contains("\"guard\"") || others.contains("\"soaking\"");
        let tracked = std::sync::OnceLock::new();
        if let Some(t) = x.tracked {
            let _ = tracked.set(t);
        }
        Ok(Self { rec, others, yours, whose: x.whose, kind, host, stand: x.stand, agent: x.agent, moves, tracked })
    }

    /// Each commit's row of `items`, drawn on the compute pool, a commit a unit of each piece (goal 3,
    /// plan 01a1a59c2a8e step 5): a render and its JSON each, which drawn on the request's shard held
    /// every other request on that shard for as long as the drawing took. Each says whether its
    /// detail's `dv` is wanted too.
    async fn draw<K: Send + 'static>(items: Vec<(K, Records, bool)>, runners: &[RunnerSeen], now: i64) -> Result<Vec<(K, Row)>, Fail> {
        let runners = runners.to_vec();
        crate::rt::compute_map(items, move |(key, rec, track)| Row::of(rec, &runners, now, track).map(|row| (key, row))).await.map_err(internal)?.into_iter().collect()
    }

    /// Whether `new`, drawn for the same commit, says anything `self` didn't: its row for anyone,
    /// or its detail beyond the row where that was followed.
    fn differs(&self, new: &Row) -> bool {
        self.others != new.others || self.yours != new.yours || self.whose != new.whose || self.tracked.get().is_some_and(|t| new.tracked.get().map(|n| &n.dv) != Some(&t.dv))
    }

    /// The row as a `ship` frame carries it, drawn after `was` (the same commit's row before, if
    /// held): where both followed its detail and that moved, how its parts changed from `was`'s.
    fn frame(&self, was: Option<&Row>) -> crate::wire::events::ShipRow {
        let value = |t: &str| serde_json::from_str::<Value>(t).unwrap_or(Value::Null);
        let mut r = crate::wire::events::ShipRow { row: value(&self.others), yours: self.yours.as_deref().map(value), whose: self.whose.clone(), dv: self.tracked.get().map(|t| t.dv.clone()), ..Default::default() };
        if let (Some(old), Some(new)) = (was.and_then(|w| w.tracked.get()), self.tracked.get())
            && old.dv != new.dv
            && let Some((parts, items)) = parts_delta(&old.parts, &new.parts)
        {
            r.base = Some(old.dv.clone());
            r.parts = parts;
            r.items = items;
        }
        r
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
    /// What the pace line counts of each repository (by id), as last drawn ([`pace_mark`]).
    pace: HashMap<String, String>,
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

/// The frame of repository `name` among `said`, made the first time.
fn said_of<'a>(said: &'a mut HashMap<String, ShipRows>, name: &str) -> &'a mut ShipRows {
    said.entry(name.to_owned()).or_insert_with(|| ShipRows { repo: name.to_owned(), ..ShipRows::default() })
}

/// What the pace line counts of one repository ([`ShipCache::pace`]), as text: the line of every
/// repository in view is made of these, so when none moved, no pace line did.
fn pace_mark(name: &str, r: &HeldRepo, runners: &[RunnerSeen]) -> String {
    let (flight, agents, envs, on) = pace_parts(&[(name, r)], runners);
    json!({ "in_flight": flight, "agents": agents, "envs": envs, "runners": on }).to_string()
}

/// The pace line's parts for `repos` (by name, in order): the commits in flight, the agents behind
/// them, what each environment runs, and how many connected runners run the first production commit
/// any runner runs.
fn pace_parts<'a>(repos: &[(&str, &'a HeldRepo)], runners: &[RunnerSeen]) -> (usize, std::collections::BTreeSet<&'a str>, Vec<Value>, Value) {
    let flight: Vec<&Row> = repos.iter().flat_map(|(_, r)| r.rows.values()).filter(|row| matches!(row.stand, journey::Stand::Landing | journey::Stand::Checking | journey::Stand::Building)).collect();
    let agents: std::collections::BTreeSet<&str> = flight.iter().filter_map(|row| row.agent.as_deref()).collect();
    let runs = |r: &RunnerSeen, sha: &str| r.commit.as_deref().is_some_and(|c| sha.starts_with(c) || c.starts_with(sha));
    let mut envs = Vec::new();
    let mut on = Value::Null;
    for (name, r) in repos {
        // Staging before production, as a commit goes.
        let mut current: Vec<&DeployRec> = r.ctx.current.iter().collect();
        current.sort_by_key(|d| d.production);
        for d in current {
            let behind = d.production.then(|| r.ctx.commits.iter().filter(|(_, f, _)| *f > d.first_row).count());
            envs.push(json!({ "repo": name, "environment": d.environment, "short": short(&d.sha), "behind": behind }));
            if d.production && on.is_null() && runners.iter().any(|x| runs(x, &d.sha)) {
                let live: Vec<&RunnerSeen> = runners.iter().filter(|x| x.connected).collect();
                on = json!({ "on": live.iter().filter(|x| runs(x, &d.sha)).count(), "of": live.len() });
            }
        }
    }
    (flight.len(), agents, envs, on)
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
    ///
    /// What it changed, a [`ShipRows`] for each repository with rows that read otherwise than
    /// before (W21-R15): the `ship` frames that let a page take each change in place. Nothing the
    /// first time (no page has rows from before). None from a refresh that returned at once: the
    /// one running may have read the store before the change its caller heard of, so a caller that
    /// must say every change ([`rows_loop`]) tries again.
    pub async fn refresh(&self, store: &crate::store::Store, runners: &[RunnerSeen], deploys: &[CdDeploy], now: i64) -> Result<Option<Vec<ShipRows>>, Fail> {
        use std::sync::atomic::Ordering;
        if self.refreshing.swap(true, Ordering::AcqRel) {
            return Ok(None);
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
        // The commits a page has read the detail of: their rows work out its `dv` too.
        let (waiting, tracked): (HashMap<String, Vec<String>>, HashSet<(String, String)>) = {
            let h = self.held.read().map_err(|_| poisoned())?;
            let waiting = h.repos.values().map(|r| (r.ctx.id.clone(), r.rows.iter().filter(|(_, row)| row.rec.releases.iter().any(|x| x.state == "waiting")).map(|(s, _)| s.clone()).collect())).collect();
            let tracked = h.repos.values().flat_map(|r| r.rows.iter().filter(|(_, row)| row.tracked.get().is_some()).map(|(s, _)| (r.ctx.id.clone(), s.clone()))).collect();
            (waiting, tracked)
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
        // Each loaded commit's row drawn on the compute pool, before the model's lock is taken (a
        // render and its JSON each): every page that reads the model waits on its write lock, on
        // every shard, and the drawing on the request's shard held up every request on it.
        let mut items = Vec::new();
        let mut loaded = Vec::with_capacity(read.loaded.len());
        for (i, (ctx, recs, all, drop)) in read.loaded.into_iter().enumerate() {
            items.extend(recs.into_iter().map(|rec| {
                let track = tracked.contains(&(ctx.id.clone(), rec.sha.clone()));
                (i, rec, track)
            }));
            loaded.push((ctx, Vec::new(), all, drop));
        }
        for (i, row) in Row::draw(items, runners, now).await? {
            if let Some((_, rows, _, _)) = loaded.get_mut(i) {
                rows.push((row.rec.sha.clone(), row));
            }
        }
        // What time and the runners move without a record changing: the guard and the soak run
        // down, and the commit production runs replants. Those few are drawn again, on the pool too,
        // from their records as held: the rows a repository's reload doesn't draw afresh.
        let again = {
            let h = self.held.read().map_err(|_| poisoned())?;
            let mut again: Vec<((String, String), Records, bool)> = Vec::new();
            for held in h.repos.values() {
                let fresh = loaded.iter().find(|(ctx, _, _, _)| ctx.id == held.ctx.id);
                if fresh.is_some_and(|(_, _, all, _)| *all) {
                    continue;
                }
                let ctx = fresh.map_or(&held.ctx, |(ctx, _, _, _)| ctx);
                let prod = ctx.current.iter().find(|d| d.production).map(|d| d.sha.as_str());
                for (sha, row) in &held.rows {
                    if fresh.is_some_and(|(_, rows, _, _)| rows.iter().any(|(s, _)| s == sha)) {
                        continue;
                    }
                    if prod == Some(sha.as_str()) || row.moves {
                        again.push(((held.ctx.id.clone(), sha.clone()), row.rec.clone(), row.tracked.get().is_some()));
                    }
                }
            }
            again
        };
        let again = Row::draw(again, runners, now).await?;
        let mut h = self.held.write().map_err(|_| poisoned())?;
        if read.top < h.seq {
            return Ok(Some(Vec::new()));
        }
        // Each repository's frame as it fills, by its name; none the first time.
        let first = !h.built;
        let mut said: HashMap<String, ShipRows> = HashMap::new();
        let gone: Vec<String> = h.repos.iter().filter(|(_, r)| read.gone_repos.contains(&r.ctx.id)).map(|(name, _)| name.clone()).collect();
        for name in gone {
            h.repos.remove(&name);
            said_of(&mut said, &name).whole = true;
        }
        for (ctx, rows, all, drop) in loaded {
            // A repository renamed: its commits move with it.
            let old = h.repos.iter().find(|(name, r)| r.ctx.id == ctx.id && **name != ctx.name).map(|(name, _)| name.clone());
            if let Some(old) = old
                && let Some(r) = h.repos.remove(&old)
            {
                h.repos.insert(ctx.name.clone(), r);
                said_of(&mut said, &old).whole = true;
                said_of(&mut said, &ctx.name).whole = true;
            }
            let held = h.repos.entry(ctx.name.clone()).or_default();
            // Read again whole: what it held is what's compared, and what isn't read again is gone.
            let mut before = if all { std::mem::take(&mut held.rows) } else { HashMap::new() };
            let mut changed: Vec<crate::wire::events::ShipRow> = Vec::new();
            let mut went: Vec<String> = Vec::new();
            for sha in drop {
                if held.rows.remove(&sha).is_some() {
                    went.push(sha);
                }
            }
            for (sha, row) in rows {
                let was = if all { before.remove(&sha) } else { held.rows.remove(&sha) };
                // The first build sends nothing: no frame is made (each reads its row's JSON again).
                if !first && was.as_ref().is_none_or(|w| w.differs(&row)) {
                    changed.push(row.frame(was.as_ref()));
                }
                held.rows.insert(sha, row);
            }
            went.extend(before.into_keys());
            let newest: Vec<(i64, String)> = ctx.commits.iter().take(cap).map(|(s, f, _)| (*f, s.clone())).collect();
            {
                let keep: std::collections::HashSet<&str> = newest.iter().map(|(_, s)| s.as_str()).collect();
                let out: Vec<String> = held.rows.keys().filter(|sha| !keep.contains(sha.as_str())).cloned().collect();
                for sha in out {
                    held.rows.remove(&sha);
                    went.push(sha);
                }
            }
            held.order = newest.into_iter().filter(|(_, s)| held.rows.contains_key(s)).collect();
            held.total = ctx.commits.len();
            held.ctx = ctx;
            if !first && (!changed.is_empty() || !went.is_empty()) {
                let name = held.ctx.name.clone();
                let f = said_of(&mut said, &name);
                f.rows.extend(changed);
                went.sort();
                went.dedup();
                f.gone.extend(went);
            }
        }
        // The rows drawn again, where their commit is still held.
        for ((id, sha), row) in again {
            if let Some((name, r)) = h.repos.iter_mut().find(|(_, r)| r.ctx.id == id)
                && let Some(slot) = r.rows.get_mut(&sha)
            {
                if !first && slot.differs(&row) {
                    said_of(&mut said, name).rows.push(row.frame(Some(slot)));
                }
                *slot = row;
            }
        }
        // What the pace line counts of each repository: a frame says when it moved.
        let marks: HashMap<String, (String, String)> = h.repos.iter().map(|(name, r)| (r.ctx.id.clone(), (name.clone(), pace_mark(name, r, runners)))).collect();
        for (id, (name, mark)) in &marks {
            if !first && h.pace.get(id) != Some(mark) {
                said_of(&mut said, name).pace = true;
            }
        }
        h.pace = marks.into_iter().map(|(id, (_, mark))| (id, mark)).collect();
        h.seq = read.top;
        h.built = true;
        h.at_ms = now;
        h.cd = cd;
        let mut out: Vec<ShipRows> = said.into_values().collect();
        out.sort_by(|a, b| a.repo.cmp(&b.repo));
        Ok(Some(out))
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
        Ok(Some(page_body(page, page.last().filter(|_| more).map(|(f, _)| *f))))
    }

    /// Each repository's newest commit in view, newest first: Ship's rows are repositories, each
    /// showing its newest commit (docs/ci.md W26-R29). None until the model is built, or when a
    /// repository in view holds none that matches though it has commits memory doesn't hold: the
    /// store says then.
    pub fn heads(&self, only: Option<&[String]>, facets: &Facets, viewer: Option<&HashSet<String>>) -> Result<Option<String>, Fail> {
        let h = self.held.read().map_err(|_| poisoned())?;
        if !h.built {
            return Ok(None);
        }
        let mut picked: Vec<(i64, &str)> = Vec::new();
        for (_, r) in h.repos.iter().filter(|(name, _)| only.is_none_or(|o| o.iter().any(|n| n == *name))) {
            match r.order.iter().find_map(|(f, sha)| r.rows.get(sha).filter(|row| facets.matches(row.kind, row.host.as_deref())).map(|row| (*f, row.json(viewer)))) {
                Some(p) => picked.push(p),
                None if r.order.len() < r.total => return Ok(None),
                None => {}
            }
        }
        picked.sort_unstable_by_key(|p| std::cmp::Reverse(p.0));
        Ok(Some(page_body(&picked, None)))
    }

    /// The list's pace line for the repositories in view, from memory (plan
    /// 01a1856fe81375b1bd1d1d86f270a45b, step 5): the commits in flight (landing, checking,
    /// building) and the agents behind them; what each environment of each repository runs now,
    /// and how many commits production is behind; and how many of the connected runners run
    /// production's commit. None until the model is built.
    pub fn pace(&self, repo: Option<&str>, only: Option<&[String]>, runners: &[RunnerSeen]) -> Option<Value> {
        let h = self.held.read().ok()?;
        if !h.built {
            return None;
        }
        let mut repos: Vec<(&str, &HeldRepo)> = h.repos.iter().filter(|(name, r)| repo.is_none_or(|x| x == name.as_str() || x == r.ctx.id) && only.is_none_or(|o| o.iter().any(|n| n == *name))).map(|(n, r)| (n.as_str(), r)).collect();
        repos.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let (flight, agents, envs, on) = pace_parts(&repos, runners);
        Some(json!({ "in_flight": flight, "agents": agents.len(), "envs": envs, "runners": on }))
    }

    /// One commit's detail from memory (`sha`: whole, or a prefix of seven or more), if held.
    pub fn detail(&self, repo: &str, sha: &str, runners: &[RunnerSeen], now: i64, viewer: Option<&HashSet<String>>) -> Option<Value> {
        let h = self.held.read().ok()?;
        if !h.built {
            return None;
        }
        let r = h.repos.iter().find(|(name, r)| name.as_str() == repo || r.ctx.id == repo).map(|(_, r)| r)?;
        let row = r.rows.get(sha).or_else(|| if sha.len() >= 7 { r.rows.iter().find(|(s, _)| s.starts_with(sha)).map(|(_, row)| row) } else { None })?;
        let (v, t) = detail_tracked(&row.rec, runners, now, viewer);
        // Read: from now on its rows hold its parts, so a `ship` frame says how they change (W21-R15).
        let _ = row.tracked.set(t);
        Some(v)
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
            let freeze = hub.instances.lock().newest_wide_caps(&r.name) & crate::proto::CAP_FREEZE != 0;
            RunnerSeen { name: r.name, connected: r.connected, commit: r.commit, failed, freeze }
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
    if c.stale(now) {
        let _ = bring_up(hub, c, &runners, &deploys, now).await;
    }
    let limit = q.get("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(30).clamp(1, 1000);
    let cursor = q.get("cursor").and_then(|c| c.parse::<i64>().ok()).unwrap_or(i64::MAX);
    let viewer = match caller {
        Some(c) => Some(principals(hub, c).await),
        None => None,
    };
    let repo = q.get("repo").map(String::as_str).filter(|r| !r.is_empty());
    // `by=repo`: Ship's rows, each repository's newest commit (docs/ci.md W26-R29); a repository
    // chosen narrows them to it.
    let heads = q.get("by").is_some_and(|b| b == "repo");
    let in_view: Option<Vec<String>> = match (repo, only.as_ref()) {
        (Some(r), Some(o)) => Some(o.iter().filter(|n| n.as_str() == r).cloned().collect()),
        (Some(r), None) => Some(vec![r.to_owned()]),
        (None, o) => o.cloned(),
    };
    let held = if heads { c.heads(in_view.as_deref(), &Facets::from_query(q), viewer.as_ref())? } else { c.page(repo, only.as_deref(), &Facets::from_query(q), viewer.as_ref(), cursor, limit)? };
    match held {
        Some(mut body) => {
            let seen = viewer_json(viewer.as_ref());
            // The pace line's words go with the page (an optional field), and whom its rows were
            // picked for.
            if body.ends_with('}') {
                body.pop();
                if let Some(pace) = c.pace(repo, only.as_deref(), &runners).and_then(|p| serde_json::to_string(&p).ok()) {
                    body.push_str(",\"pace\":");
                    body.push_str(&pace);
                }
                body.push_str(",\"viewer\":");
                body.push_str(&seen.to_string());
                body.push('}');
            }
            Ok(srv::response(StatusCode::OK, "application/json", srv::full(body)))
        }
        None if heads => {
            let in_view = in_view.map(|names| serde_json::to_string(&names).unwrap_or_else(|_| "[]".into()));
            // From the store: no `viewer`, so the page loads it again on the next `ship` frame
            // rather than lay frames, which say what changed of memory, over it.
            Ok(srv::json(StatusCode::OK, &heads_in(&hub.store, q, in_view, viewer.as_ref(), runners, &deploys, now).await?))
        }
        None => {
            let only = only.map(|names| serde_json::to_string(&names).unwrap_or_else(|_| "[]".into()));
            Ok(srv::json(StatusCode::OK, &list_in(&hub.store, q, only, viewer.as_ref(), runners, &deploys, now).await?))
        }
    }
}

/// Whom a page's rows were picked for (`viewer`, an optional field; W21-R15): the principals a
/// `ship` frame's `whose` is held against, so the page takes `yours` where the list would. Only on
/// what memory answered, whose changes the frames say: it marks a page that can take them in
/// place; a page without it (from the store, or a server that sends no frames) loads again.
fn viewer_json(viewer: Option<&HashSet<String>>) -> Value {
    let mut v: Vec<&str> = viewer.into_iter().flatten().map(String::as_str).collect();
    v.sort_unstable();
    json!(v)
}

fn with_viewer(mut v: Value, seen: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.insert("viewer".into(), seen);
    }
    v
}

/// Brings the read model up to date, and sends what changed down the events sockets (W21-R15).
/// False when another refresh was running ([`ShipCache::refresh`]'s None).
async fn bring_up(hub: &Hub, c: &ShipCache, runners: &[RunnerSeen], deploys: &[CdDeploy], now: i64) -> bool {
    match c.refresh(&hub.store, runners, deploys, now).await {
        Ok(Some(changed)) => {
            for rows in changed {
                hub.emit(&ServerEvent::Ship(Box::new(rows)));
            }
            true
        }
        Ok(None) => false,
        Err((_, e)) => {
            tracing::warn!("ship: the read model kept its last good state: {e}");
            true
        }
    }
}

/// The events a Ship page used to load its lists again on (`ui/src/views/ship`): each says a
/// record a row shows may have changed, so the read model is brought up to date and what changed
/// goes down as `ship` frames.
const MOVES_ROWS: [&str; 9] = ["run", "job", "review", "comment", "conflict", "cd_apps", "cd_rollout", "updates", "update_job"];

/// Whether an events frame is one of [`MOVES_ROWS`]: each frame's text begins with its tag.
fn moves_rows(text: &str) -> bool {
    text.strip_prefix("{\"type\":\"").and_then(|t| t.split_once('"')).is_some_and(|(tag, _)| MOVES_ROWS.contains(&tag))
}

/// Ship's rows as they change (W21-R15): after a frame that may move one, once the model has been
/// built (a page read it) and while any page listens, the model is brought up to date and its
/// changes sent. A burst is one refresh: what comes within [`FRESH_MS`] of the first frame goes
/// with it, the freshness a read already promises, and the window the page applies frames in
/// (`APPLY_MS` in `ui/src/lib/resource.ts`).
pub async fn rows_loop(hub: std::sync::Arc<Hub>) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    let mut rx = hub.events.subscribe();
    let window = std::time::Duration::from_millis(u64::try_from(FRESH_MS).unwrap_or(250));
    loop {
        let heard = futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            m = rx.recv().fuse() => match m {
                Ok(text) => moves_rows(&text),
                Err(async_broadcast::RecvError::Overflowed(_)) => true,
                Err(async_broadcast::RecvError::Closed) => return Ok(()),
            },
        };
        if !heard {
            continue;
        }
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            () = crate::rt::sleep(window).fuse() => {}
        }
        // What came meanwhile is in the refresh already.
        loop {
            match rx.try_recv() {
                Ok(_) | Err(async_broadcast::TryRecvError::Overflowed(_)) => {}
                Err(async_broadcast::TryRecvError::Empty) => break,
                Err(async_broadcast::TryRecvError::Closed) => return Ok(()),
            }
        }
        let c = cache();
        if !c.built() || crate::metrics::global().ws_clients.get() <= 0 {
            continue;
        }
        // Another refresh running may have read the store before what was heard: once it's done
        // (it takes no longer than a page's read waits for it), this one, so no change goes unsaid.
        loop {
            let runners = runners_seen(&hub).await;
            if bring_up(&hub, c, &runners, &cd_deploys(), crate::util::now_ms()).await {
                break;
            }
            futures::select_biased! {
                () = hub.shutdown.cancelled().fuse() => return Ok(()),
                () = crate::rt::sleep(window).fuse() => {}
            }
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
    if c.stale(now) {
        let _ = bring_up(hub, c, &runners, &deploys, now).await;
    }
    let viewer = match caller {
        Some(c) => Some(principals(hub, c).await),
        None => None,
    };
    // From memory, its `viewer` (W21-R15: `ship` frames say how it changes); from the store, none.
    let v = match c.detail(repo, sha, &runners, now, viewer.as_ref()) {
        Some(v) => with_viewer(v, viewer_json(viewer.as_ref())),
        None => get_in(&hub.store, repo, sha, viewer.as_ref(), runners, &deploys, now).await?,
    };
    if !visible(&v, only.as_deref()) {
        return Err((StatusCode::NOT_FOUND, "no such commit".to_string()));
    }
    Ok(srv::json(StatusCode::OK, &v))
}

/// `POST /api/ship/jobs` `{repo, sha, verb, jobs?, scope?}` (docs/ci.md W26-R21, R22): a commit's
/// jobs acted on together, across its runs: run again (`retry`), paused, resumed or cancelled.
/// Those named (`jobs`), or those a word picks (`scope`): `failed`, `running`, `waiting` or `all`.
/// Each run they belong to gets its own call underneath (`runs/:id/retry`, `cancel`, `pause`,
/// `resume`, as the caller), and each job answers: done, or why not. As the caller, who needs
/// `runs.rerun` (retry), `runs.cancel` (cancel) or `runs.pause` (pause, resume) on the repository.
pub async fn jobs(hub: &std::sync::Arc<Hub>, caller: &crate::authz::Caller, body: &Value) -> Result<Response, Fail> {
    jobs_in(hub, caller, body).await.map(|v| srv::json(StatusCode::OK, &v))
}

async fn jobs_in(hub: &std::sync::Arc<Hub>, caller: &crate::authz::Caller, body: &Value) -> Result<Value, Fail> {
    let field = |k: &str| body.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let (Some(repo), Some(sha), Some(verb)) = (field("repo"), field("sha"), field("verb")) else {
        return Err((StatusCode::BAD_REQUEST, "`repo`, `sha` and `verb`: the commit, and what to do to its jobs".to_string()));
    };
    let (cap, word) = match verb.as_str() {
        "retry" => ("runs.rerun", "again"),
        "cancel" => ("runs.cancel", "cancelled"),
        "pause" => ("runs.pause", "paused"),
        "resume" => ("runs.pause", "resumed"),
        _ => return Err((StatusCode::BAD_REQUEST, format!("`verb` {verb}: retry, pause, resume or cancel"))),
    };
    if !caller.can(cap, &crate::authz::Scope::Repo { repo: repo.clone() }) {
        return Err((StatusCode::FORBIDDEN, format!("{verb} needs `{cap}` on {repo}")));
    }
    let named: Option<Vec<String>> = body.get("jobs").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect());
    let scope = field("scope");
    if named.as_ref().is_none_or(Vec::is_empty) && scope.is_none() {
        return Err((StatusCode::BAD_REQUEST, "name the `jobs`, or a `scope`: failed, running, waiting or all".to_string()));
    }
    let (r, s, deploys) = (repo.clone(), sha.clone(), cd_deploys());
    let rec = hub.store.call(move |c| load(c, &r, &s, &deploys)).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, "no such commit".to_string()))?;
    let mut out: Vec<Value> = Vec::new();
    // The jobs it's for, by run, in the commit's order.
    let picked: Vec<&LegRec> = match (named.filter(|n| !n.is_empty()), scope.as_deref()) {
        (Some(ids), _) => {
            for id in ids.iter().filter(|id| !rec.legs.iter().any(|l| &l.job == *id)) {
                out.push(json!({ "job": id, "done": false, "why": "not a job of this commit" }));
            }
            rec.legs.iter().filter(|l| ids.contains(&l.job)).collect()
        }
        (None, Some(scope)) => {
            let status = |l: &LegRec| crate::store::Status::parse(&l.status);
            let ended = |l: &LegRec| status(l).is_terminal() || l.status == "completed";
            let pick = |l: &LegRec| match scope {
                "failed" => part_of(&l.status, l.conclusion.as_deref()) == Part::Failed,
                "running" => !ended(l) && status(l) == crate::store::Status::Running,
                "waiting" => !ended(l) && status(l) != crate::store::Status::Running,
                _ => true,
            };
            if !matches!(scope, "failed" | "running" | "waiting" | "all") {
                return Err((StatusCode::BAD_REQUEST, format!("`scope` {scope}: failed, running, waiting or all")));
            }
            rec.legs.iter().filter(|l| pick(l)).collect()
        }
        (None, None) => Vec::new(),
    };
    let mut runs: Vec<(String, Vec<String>)> = Vec::new();
    for l in picked {
        match runs.iter_mut().find(|(r, _)| *r == l.run_id) {
            Some((_, ids)) => ids.push(l.job.clone()),
            None => runs.push((l.run_id.clone(), vec![l.job.clone()])),
        }
    }
    for (run, ids) in runs {
        let said = match verb.as_str() {
            "retry" => crate::ci::retry::retry_set(hub, &run, &ids, &caller.name).await,
            "cancel" => crate::ci::retry::cancel_set(hub, &run, ids.clone(), &caller.name).await,
            _ => crate::ci::retry::pause_set(hub, &run, ids.clone(), verb == "pause", &caller.name).await,
        };
        match said {
            Ok(v) => {
                for j in v.get("jobs").and_then(Value::as_array).into_iter().flatten() {
                    let mut one = json!({ "job": j.get("job"), "run": run, "done": j.get(word).and_then(Value::as_bool).unwrap_or(false) });
                    if let (Some(why), Some(o)) = (j.get("why"), one.as_object_mut()) {
                        o.insert("why".into(), why.clone());
                    }
                    out.push(one);
                }
            }
            Err((_, why)) => out.extend(ids.iter().map(|id| json!({ "job": id, "run": run, "done": false, "why": why }))),
        }
    }
    Ok(json!({ "repo": repo, "sha": rec.sha, "verb": verb, "jobs": out }))
}

/// Failure lines the agent a failure is handed to gets with it.
const HANDED_LINES: usize = 40;

/// `POST /api/ship/ready` `{repo, sha}`: the row's Ready (W26-R18, drafts): an agent's draft marked
/// ready for review by its agent's person (the change's `for`) or one who may land on the
/// repository (`diffs.land`): kiln's own as kiln does it for them, a host's through its API
/// (`GitHost::ready_change`, docs/ci.md W29-R12). The agent's change then says it's no draft.
pub async fn ready(hub: &std::sync::Arc<Hub>, caller: &crate::authz::Caller, body: &Value) -> Result<Response, Fail> {
    ready_in(hub, caller, body).await.map(|v| srv::json(StatusCode::OK, &v))
}

async fn ready_in(hub: &std::sync::Arc<Hub>, caller: &crate::authz::Caller, body: &Value) -> Result<Value, Fail> {
    let field = |k: &str| body.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let (Some(repo), Some(sha)) = (field("repo"), field("sha")) else {
        return Err((StatusCode::BAD_REQUEST, "`repo` and `sha`: the draft to mark ready".to_string()));
    };
    let (r, s, deploys) = (repo.clone(), sha.clone(), cd_deploys());
    let rec = hub.store.call(move |c| load(c, &r, &s, &deploys)).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, "no such commit".to_string()))?;
    let j = journey_of(&rec, &runners_seen(hub).await, crate::util::now_ms());
    let draft = j.review.as_ref().is_some_and(|r| r.superseded_by.is_none() && r.state == ReviewState::Draft);
    let Some(ch) = rec.change.as_ref().filter(|_| draft) else {
        return Err((StatusCode::CONFLICT, format!("{} isn't a draft", rec.change.as_ref().map_or_else(|| short(&rec.sha), |ch| ch.name.clone()))));
    };
    let Some(agent) = authors_agent(ch).map(str::to_owned).or_else(|| ch.by_agent.clone()) else {
        return Err((StatusCode::CONFLICT, format!("{} is its author's own draft: they mark it ready", ch.name)));
    };
    let person = ch.author_for.clone().filter(|p| !p.is_empty());
    let theirs = person.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(&crate::ci::diffs::author_of(&caller.name, Some(caller.kind)).0));
    if !theirs && !caller.can("diffs.land", &crate::authz::Scope::Repo { repo: repo.clone() }) {
        let whose = person.as_deref().map(|p| format!(" ({})", shown(p))).unwrap_or_default();
        return Err((StatusCode::FORBIDDEN, format!("marking {} ready is for its agent's person{whose} or one who can land on {repo} (diffs.land)", ch.name)));
    }
    let repos = hub.store.repos().await.map_err(internal)?;
    let target = repos.iter().find(|x| x.name == repo).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository {repo}")))?;
    let host = match ch.host.as_str() {
        "kiln" => crate::githost::GitHost::Kiln,
        _ => crate::githost::for_repo(hub, target).await.ok_or_else(|| (StatusCode::CONFLICT, format!("kiln can't reach {repo}'s host to mark {} ready", ch.name)))?,
    };
    crate::workspace::change::ready_where_it_is(hub, target, &host, &ch.name).await?;
    crate::workspace::change::readied(hub, &agent, &repo, &ch.name).await;
    hub.store.audit(caller.name.clone(), "ship.ready".into(), format!("{repo} {} marked ready for review", ch.name)).await;
    Ok(json!({ "ready": { "change": ch.name } }))
}

/// `POST /api/ship/hand-off` `{repo, sha}`: the row's Hand off, as review-ship.html draws it
/// (W26-R18, R02). What the commit has open goes to one agent. Its change's author's agent, while
/// it runs, gets the change's conflicts, its open threads and its failed checks in one message, the
/// way its comments reach it (docs/agentic.md C3). A failure with no agent of its own goes to the
/// agent attached to what failed, as the failure itself wakes it (an incident, W20-R08). Each is
/// kept (`ship_handoffs`), so the row says who is on it; with nothing open, or no agent to take it,
/// it says why. As the caller, with `runs.rerun` on the repository, as Retry asks. The person's side
/// is signed (docs/ci.md W27-R08): their own hand-on beside it (`hand_on`), else kiln's take for
/// them, saying why it isn't theirs (`unsigned`: their browser's reason).
pub async fn hand_off(hub: &std::sync::Arc<Hub>, caller: &crate::authz::Caller, body: &Value) -> Result<Response, Fail> {
    hand_off_in(hub, caller, body).await.map(|v| srv::json(StatusCode::OK, &v))
}

/// `POST /api/ship/hand-off/statement` `{repo, sha}`: what a person signs to hand the commit off from
/// a browser, with the browser's own key (docs/ci.md W27-R08; `ui/src/lib/signing.ts`), said as a
/// verdict's is. It's their hand-on of what's open of it to kiln's server, which passes it on
/// unchanged to the agent that takes it, since kiln knows who that is only as it hands it
/// ([`hand_off`], with the signed hand-on as `hand_on`). Refused as Hand off would be.
pub async fn hand_off_statement(hub: &std::sync::Arc<Hub>, caller: &crate::authz::Caller, body: &Value) -> Result<Response, Fail> {
    hand_off_statement_in(hub, caller, body).await.map(|v| srv::json(StatusCode::OK, &v))
}

async fn hand_off_statement_in(hub: &std::sync::Arc<Hub>, caller: &crate::authz::Caller, body: &Value) -> Result<Value, Fail> {
    use kiln_ci::identity::custody::{Body, Kind, to_value};
    use kiln_ci::identity::statement::Statement;
    let h = handing(hub, caller, body).await?;
    let org = hub.pki.organization();
    let person = crate::identities::person_identity(&hub.store, &org, caller).await.map_err(|e| (StatusCode::FORBIDDEN, e.to_string()))?;
    let what = h.what().join(", ");
    let b = Body { to: Some(crate::custody::server_uri(&org)), why: handed_why(&h.rec, &what), ..Body::new(Kind::HandedOn, &person) };
    let s = Statement { subject: vec![handed_work(&h.rec, &what)], ..Statement::new(to_value(&b)) };
    crate::custody::to_sign(&s).map_err(internal)
}

/// A commit's Hand off as kiln decides it, before anything is handed: the commit, its journey, and
/// its change's author's agent while that runs (its id and name), which takes all that's open;
/// without one, its failure goes to the agent attached to what failed.
struct Handing {
    rec: Records,
    j: Journey,
    running: Option<(String, String)>,
}

impl Handing {
    /// What's open: a change's conflicts and threads, at its live revision only, and a failure
    /// while no one has it.
    fn open(&self) -> (&[kiln_ci::review::Conflict], &[ThreadRec], Option<&LegRec>) {
        let live = self.rec.change.as_ref().filter(|_| self.j.review.as_ref().is_some_and(|r| r.superseded_by.is_none()));
        let (conflicts, threads) = live.map(|ch| (ch.conflicts.as_slice(), ch.threads.as_slice())).unwrap_or_default();
        (conflicts, threads, failed_leg(&self.rec).filter(|_| self.j.handled_by.is_none()))
    }

    /// What its taker is handed: all that's open, to the author's agent; else the failed checks.
    fn what(&self) -> Vec<&'static str> {
        if self.running.is_none() || self.rec.change.is_none() {
            return vec!["checks"];
        }
        let (conflicts, threads, failed) = self.open();
        [(!conflicts.is_empty(), "conflicts"), (!threads.is_empty(), "threads"), (failed.is_some(), "checks")].into_iter().filter_map(|(y, w)| y.then_some(w)).collect()
    }
}

/// `body`'s commit's Hand off, decided ([`Handing`]): refused when the caller can't hand it off,
/// when nothing is open, or when no one could take it.
async fn handing(hub: &std::sync::Arc<Hub>, caller: &crate::authz::Caller, body: &Value) -> Result<Handing, Fail> {
    let field = |k: &str| body.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let (Some(repo), Some(sha)) = (field("repo"), field("sha")) else {
        return Err((StatusCode::BAD_REQUEST, "`repo` and `sha`: the commit to hand off".to_string()));
    };
    if !caller.can("runs.rerun", &crate::authz::Scope::Repo { repo: repo.clone() }) {
        return Err((StatusCode::FORBIDDEN, format!("handing off needs `runs.rerun` on {repo}")));
    }
    let deploys = cd_deploys();
    let rec = hub.store.call(move |c| load(c, &repo, &sha, &deploys)).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, "no such commit".to_string()))?;
    let j = journey_of(&rec, &runners_seen(hub).await, crate::util::now_ms());
    // Its maker's agent, from the one place Ship reads it (made_by: Chain of Ownership fills it in time).
    let author = made_by(&rec).agent;
    let running = match author.as_deref() {
        Some(id) => {
            let agent = crate::workspace::change::of(hub, id).await.map_or_else(|| id.to_owned(), |r| r.agent);
            hub.harness.info(&agent).filter(|i| i.status != crate::harness::model::Status::Exited).map(|i| (i.id, i.name))
        }
        None => None,
    };
    let h = Handing { rec, j, running };
    let (nothing, failure) = {
        let (conflicts, threads, failed) = h.open();
        (conflicts.is_empty() && threads.is_empty() && failed.is_none(), failed.is_some())
    };
    let at = short(&h.rec.sha);
    if nothing {
        return Err((StatusCode::CONFLICT, format!("{at} has nothing open to hand off")));
    }
    // No agent of its own and nothing failed: a change's review has no one to go to.
    if (h.running.is_none() || h.rec.change.is_none()) && !failure {
        let name = h.rec.change.as_ref().map_or(at, |ch| ch.name.clone());
        return Err((
            StatusCode::CONFLICT,
            match author {
                Some(_) => format!("{name}'s agent isn't running now: start an agent on it from Mission control"),
                None => format!("{name} is its author's own: no agent of its author to hand it to"),
            },
        ));
    }
    Ok(h)
}

async fn hand_off_in(hub: &std::sync::Arc<Hub>, caller: &crate::authz::Caller, body: &Value) -> Result<Value, Fail> {
    let h = handing(hub, caller, body).await?;
    let (rec, what) = (&h.rec, h.what());
    let (conflicts, threads, failed) = h.open();
    let at = short(&rec.sha);
    let base = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    if let (Some((id, name)), Some(ch)) = (h.running.as_ref(), rec.change.as_ref()) {
        let excerpt = match failed {
            Some(leg) => crate::ci::logsearch::failure(hub, &leg.job, HANDED_LINES).await.ok(),
            None => None,
        };
        let text = handed_text(rec, ch, conflicts, threads, failed.map(|l| (l, excerpt.as_ref())), &base, &caller.name);
        if !crate::workspace::follow::deliver(hub, id, text).await {
            return Err((StatusCode::BAD_GATEWAY, format!("{name} didn't get it: kiln couldn't reach it")));
        }
        keep(hub, rec, &what, name, Some(id), None, Some(ch.n), &caller.name).await?;
        let custody = hand_off_signed(hub, caller, rec, &what, id, body).await;
        return Ok(json!({ "handed": { "to": name, "agent": id, "what": what }, "custody": custody }));
    }
    // No agent of its own: its failure goes to what looks after it ([`handing`] saw one).
    let Some(leg) = failed else {
        return Err((StatusCode::CONFLICT, format!("{at} has nothing open to hand off")));
    };
    let Some(notice) = crate::ci::notify::failed_notice(hub, &leg.run_id).await else {
        return Err((StatusCode::NOT_FOUND, format!("no run {} to hand off", leg.run_id)));
    };
    let t = crate::ci::attach::failure_of(hub, &notice, "failure").await;
    let Some(a) = crate::ci::attach::all(hub).await.into_iter().find(|a| crate::ci::attach::looks_after(a, &t.about)) else {
        return Err((StatusCode::CONFLICT, format!("No agent looks after {}: attach one on its page in Repos (an agent woken by failures), and Hand off gives it {at}", rec.repo)));
    };
    let Some(i) = crate::ci::attach::wake(hub, &a, &t).await else {
        return Err((StatusCode::CONFLICT, format!("{} didn't take {at}", a.name)));
    };
    let Some(agent) = i.agent.clone() else {
        return Err((StatusCode::CONFLICT, format!("{} didn't take {at}: {}", a.name, i.why.unwrap_or_else(|| "its incident waits on a person".to_string()))));
    };
    keep(hub, rec, &what, &a.name, Some(&agent), Some(&i.id), None, &caller.name).await?;
    let custody = hand_off_signed(hub, caller, rec, &what, &agent, body).await;
    Ok(json!({ "handed": { "to": a.name, "agent": agent, "incident": i.id, "what": what }, "custody": custody }))
}

/// The work a hand-off of `rec`'s commit hands (`what`: its checks, threads or conflicts), by its
/// repository, commit and what (`kiln-work-v1`).
fn handed_work(rec: &Records, what: &str) -> kiln_ci::identity::statement::Subject {
    crate::custody::work(&format!("{}@{} {what}", rec.repo, short(&rec.sha)), &[&rec.repo, &rec.sha, what])
}

/// Why a hand-off's records say it changed hands.
fn handed_why(rec: &Records, what: &str) -> String {
    format!("handed off: {what} of {}@{}", rec.repo, short(&rec.sha))
}

/// What `caller` handed agent session `agent` of `rec`'s commit (`what`: its checks, threads or
/// conflicts) changes hands signed (docs/ci.md W27-R08; the Transfers plan's step 6). The person's
/// own hand-on when they sent one (`hand_on` in `body`, signed on their device), of that work to the
/// agent, or, from a browser, which signs before kiln knows who takes it, to kiln's server, which
/// passes it on unchanged to the agent (a relay's record). Else kiln's take for them on their
/// signed-in session, saying their own signature is missing and why: their browser's reason
/// (`unsigned`), the one they sent refused, or no key on the device that asked. The agent signs its
/// receipt as it takes the work up, at its next turn. What the answer says of it: the hand-on, what
/// kiln passed on, and whether kiln signed it for them.
async fn hand_off_signed(hub: &Hub, caller: &crate::authz::Caller, rec: &Records, what: &[&str], agent: &str, body: &Value) -> Value {
    let org = hub.pki.organization();
    let what = what.join(", ");
    let thing = handed_work(rec, &what);
    let person = crate::identities::person_identity(&hub.store, &org, caller).await.unwrap_or_else(|_| crate::custody::person_uri(&org, &crate::identities::person_of(&caller.name)));
    let to = match crate::identities::agent_at(hub, agent, hub.harness.epoch_of(agent)).await {
        Ok(to) => to,
        Err(e) => {
            tracing::warn!(target: "kiln.custody", %agent, error = %format!("{e:#}"), "a hand-off went unsigned: kiln can't name the agent");
            return Value::Null;
        }
    };
    let why = handed_why(rec, &what);
    let mut missing = body.get("unsigned").and_then(Value::as_str).map(|m| m.chars().take(300).collect::<String>());
    // Their own, signed on their device: kept if it's theirs, of this work, to this agent or to kiln.
    if let Some(bundle) = body.get("hand_on") {
        let server = crate::custody::server_uri(&org);
        let via_kiln = crate::custody::hand_on_to(bundle).is_some_and(|t| t == server);
        let kept = crate::custody::person_hand_on(hub, &person, if via_kiln { &server } else { &to }, &thing, bundle, &format!("api:{}", caller.name)).await;
        match kept {
            Ok(Ok(digest)) if via_kiln => {
                let passed = crate::custody::pass_on(hub, vec![thing], &digest, &person, &to, "handed off from Ship, passed on unchanged to the agent that took it").await;
                return match passed {
                    Ok((passed, bundle)) => {
                        crate::custody::ask_receipt(hub, agent, &bundle).await;
                        json!({ "hand_on": digest, "passed_on": passed, "kiln_signed_for_you": false })
                    }
                    Err(e) => {
                        tracing::warn!(target: "kiln.custody", %agent, error = %format!("{e:#}"), "a person's hand-off to kiln wasn't passed on to its agent");
                        json!({ "hand_on": digest, "kiln_signed_for_you": false })
                    }
                };
            }
            Ok(Ok(digest)) => {
                crate::custody::ask_receipt(hub, agent, bundle).await;
                return json!({ "hand_on": digest, "kiln_signed_for_you": false });
            }
            Ok(Err(why)) => {
                tracing::info!(target: "kiln.custody", %agent, %why, "a person's own hand-off refused: kiln signs it for them");
                missing = Some(format!("the one they sent was refused ({why})"));
            }
            Err(e) => tracing::warn!(target: "kiln.custody", %agent, error = %format!("{e:#}"), "a person's own hand-off couldn't be checked"),
        }
    }
    let missing = missing.unwrap_or_else(|| "no key on the device that asked".to_owned());
    let why = format!("{why}: kiln's take for them, on their signed-in session; their own signature is missing: {missing}");
    match crate::custody::hand_work(hub, thing, &to, &why, crate::custody::Giver::For(person)).await {
        Ok((digest, bundle)) => {
            crate::custody::ask_receipt(hub, agent, &bundle).await;
            json!({ "hand_on": digest, "kiln_signed_for_you": true })
        }
        Err(e) => {
            tracing::warn!(target: "kiln.custody", %agent, error = %format!("{e:#}"), "a hand-off's take wasn't kept");
            Value::Null
        }
    }
}

/// What a change's author's agent is handed: where its change stands open, and what to do about
/// each (`excerpt`: the failed check's failure, as `ci::logsearch::failure` gives it).
fn handed_text(rec: &Records, ch: &ChangeRec, conflicts: &[kiln_ci::review::Conflict], threads: &[ThreadRec], failed: Option<(&LegRec, Option<&Value>)>, base: &str, by: &str) -> String {
    let mut text = format!("{} handed you your change {} (version {}, commit {}) from Ship.", shown(by), ch.name, ch.n, short(&rec.sha));
    if !conflicts.is_empty() {
        let places: Vec<String> = conflicts.iter().map(|k| kiln_ci::review::place(Some(&k.file), k.line)).collect();
        text.push_str(&format!("\n\nIt conflicts with where it lands: {}. Resolve them in a new version (agents.change).", places.join(", ")));
    }
    if !threads.is_empty() {
        // Thread by thread (W26-R18): each one's id, where it is and the revision it's on, and
        // whether an agent opened it (agents' feedback a person sends on).
        text.push_str(&format!("\n\nIts open threads ({}):", threads.len()));
        for t in threads {
            let place = match (&t.file, t.line) {
                (Some(f), Some(l)) => format!("{f}:{l}"),
                (Some(f), None) => f.clone(),
                _ => "the conversation".to_string(),
            };
            let agents = if t.by.starts_with("agent:") { " (an agent's)" } else { "" };
            text.push_str(&format!("\n- thread {}, {place} on version {} ({}), {}{agents}: {}", t.id, t.version, t.kind, shown(&t.by), first_words(&t.said)));
        }
        let ids: Vec<String> = threads.iter().map(|t| format!("\"{}\"", t.id)).collect();
        text.push_str(&format!(
            "\n\nAnswer each in its thread: ci.diffs.reply {{\"diff\": \"{}\", \"thread\": <its id>, \"body\": …}}. Make what they ask in a new version that fixes them: agents.change {{\"threads\": [{}]}}.",
            ch.name,
            ids.join(", ")
        ));
    }
    if let Some((leg, excerpt)) = failed {
        let run_of = |l: &LegRec| rec.runs.iter().find(|r| r.id == l.run_id).map(|r| r.pipeline.as_str()).unwrap_or_default();
        let names: Vec<&str> = rec.legs.iter().filter(|l| matches!(attempt_of(&l.status, l.conclusion.as_deref(), None), Attempt::Failed) && !is_release(run_of(l))).map(|l| l.name.as_str()).collect();
        text.push_str(&format!("\n\nIts checks failed: {}.", names.join(", ")));
        let lines: Vec<&str> = excerpt.and_then(|f| f.get("lines")).and_then(Value::as_array).into_iter().flatten().filter_map(|l| l.get("text").and_then(Value::as_str)).collect();
        if !lines.is_empty() {
            let step = excerpt.and_then(|f| f.get("step_name")).and_then(Value::as_str).map(|n| format!(", step \"{n}\"")).unwrap_or_default();
            text.push_str(&format!("\n\nWhere {} failed{step}:\n```\n{}\n```", leg.name, lines.join("\n")));
        }
        text.push_str(&format!("\n\nThe whole log: ci.legs.logs (leg {}); its failure: ci.legs.failure_excerpt. Fix it in a new version (agents.change).", leg.job));
    }
    text.push_str(&format!(" It's at {base}/diffs/{}", ch.name));
    text
}

/// What kiln keeps, as a hand-off's `what`, when it holds agents' feedback for a person (W26-R18).
pub const HELD: &str = "held";

/// What kiln itself did with agents' feedback on a change (W26-R18), kept as Ship reads hand-offs:
/// at revision `version` of the change, its commit `sha` in repository `repo` (its name or id), the change's
/// threads handed to agent `agent` (`to`, its name; `what` `threads`), which reads fixing until the
/// next revision, or held for a person (`what` [`HELD`]), which the item says; `why`, in words.
/// Audited, as kiln's own doing.
#[allow(clippy::too_many_arguments)]
pub async fn kiln_kept(hub: &Hub, repo: &str, sha: &str, version: u32, what: &str, to: &str, agent: &str, why: &str) -> anyhow::Result<()> {
    let (repo2, sha, what, to, agent, why2, at) = (repo.to_owned(), sha.to_owned(), what.to_owned(), to.to_owned(), agent.to_owned(), why.to_owned(), crate::util::now_unix());
    let audit = format!("{repo}@{} {what} to {to}: {why}", short(&sha));
    hub.store
        .call(move |c| {
            let repo_id: String = c.query_row("SELECT id FROM repos WHERE name = ?1 OR id = ?1", [&repo2], |r| r.get(0))?;
            c.execute(
                "INSERT INTO ship_handoffs (repo_id, sha, what, to_name, agent, incident, version, by, at, why) VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, 'kiln', ?7, ?8)",
                params![repo_id, sha, what, to, agent, version, at, why2],
            )?;
            Ok(())
        })
        .await?;
    hub.store.audit("kiln".into(), "ship.hand_off".into(), audit).await;
    Ok(())
}

/// Keeps a Hand off of `rec`'s commit, one row for each of `what` (W26-R18): to whom (`to`, its
/// agent and the incident it opened), the change's revision for its review, by whom, now.
/// A change's conflicts the conflict ladder handed to an agent (docs/ci.md W26-R17), kept as Hand
/// off keeps them (W26-R18), so its row reads "<to> fixing" until a new revision replaces it.
pub(crate) async fn conflicts_handed(hub: &Hub, repo: &str, sha: &str, to: &str, agent: &str, version: u32) -> Result<(), Fail> {
    let (repo, sha, to, agent, at) = (repo.to_owned(), sha.to_owned(), to.to_owned(), agent.to_owned(), crate::util::now_unix());
    hub.store
        .call(move |c| {
            let repo_id: String = c.query_row("SELECT id FROM repos WHERE name = ?1 OR id = ?1", [&repo], |r| r.get(0))?;
            c.execute("INSERT INTO ship_handoffs (repo_id, sha, what, to_name, agent, incident, version, by, at) VALUES (?1, ?2, 'conflicts', ?3, ?4, NULL, ?5, 'kiln', ?6)", params![repo_id, sha, to, agent, version, at])?;
            Ok(())
        })
        .await
        .map_err(internal)
}

#[allow(clippy::too_many_arguments)]
async fn keep(hub: &Hub, rec: &Records, what: &[&str], to: &str, agent: Option<&str>, incident: Option<&str>, version: Option<u32>, by: &str) -> Result<(), Fail> {
    let (repo, sha, what) = (rec.repo.clone(), rec.sha.clone(), what.iter().map(|w| (*w).to_owned()).collect::<Vec<String>>());
    let (to, agent, incident, who, at) = (to.to_owned(), agent.map(str::to_owned), incident.map(str::to_owned), by.to_owned(), crate::util::now_unix());
    let audit = format!("{repo}@{} {} to {to}", short(&sha), what.join(", "));
    hub.store
        .call(move |c| {
            let repo_id: String = c.query_row("SELECT id FROM repos WHERE name = ?1 OR id = ?1", [&repo], |r| r.get(0))?;
            for w in &what {
                let v = if w == "checks" { None } else { version };
                c.execute("INSERT INTO ship_handoffs (repo_id, sha, what, to_name, agent, incident, version, by, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)", params![repo_id, sha, w, to, agent, incident, v, who, at])?;
            }
            Ok(())
        })
        .await
        .map_err(internal)?;
    hub.store.audit(by.to_owned(), "ship.hand_off".into(), audit).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W26-R17: who made each of the conflict ladder's tries reads back in MadeBy's shape: an agent's
    /// offer names the agent and its person, a person's edit the person, kiln's own merge neither.
    #[test]
    fn w26_r17_a_ladder_trys_made_by() {
        assert_eq!(made_by_actor("agent:01a18b4228f7 for user:kim"), json!({ "agent": "01a18b4228f7", "person": "user:kim" }));
        assert_eq!(made_by_actor("agent:resolver"), json!({ "agent": "resolver" }));
        assert_eq!(made_by_actor("user:kim"), json!({ "person": "user:kim" }));
        assert_eq!(made_by_actor("kiln"), Value::Null);
    }

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
            RunnerSeen { name: "metal-ryzen-01".into(), connected: true, commit: Some(short(C1)), failed: None, freeze: true },
            RunnerSeen { name: "metal-epyc-01".into(), connected: true, commit: Some(C1.into()), failed: None, freeze: false },
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

    /// W26-R23: a release run again (production's 4ecf691f7e had three) leaves each platform one
    /// lane, as the newest run that builds it has it: the job and run its Output streams once it
    /// has run, and the times its lane draws. A platform the newer run doesn't build stays as the
    /// older run built it.
    #[test]
    fn w26_r23_a_platform_is_one_lane_as_its_newest_release_run_has_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let mut sql = production_fixture();
            sql += &format!(
                "INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, message, author, status, created_at) VALUES ('release2', 'kiln', 1430, 'release', 'push', 'main', '{C1}', 'again', 'user.email', 'running', {});",
                NOW_S + 3000
            );
            sql += &format!(
                "INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, started_at) VALUES ('r2a', 'release2', 'linux', 'Linux (x86-64, ARM64)', 'running', '[]', '[]', '{{}}', {});",
                NOW_S + 3100
            );
            sql += "INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec) VALUES ('r2b', 'release2', 'macos', 'macOS (ARM64), on a Mac', 'queued', '[]', '[]', '{}');";
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let one = get_in(&store, "hyper-light/kiln", C1, None, Vec::new(), &[], (NOW_S + 4000) * 1000).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let platforms = field(&one, &["build", "platforms"]).and_then(Value::as_array).cloned().unwrap_or_default();
            assert_eq!(
                platforms,
                vec![
                    json!({ "name": "Linux (x86-64, ARM64)", "state": "running", "job": "r2a", "run": "release2", "start": NOW_S + 3100 }),
                    json!({ "name": "macOS (ARM64), on a Mac", "state": "queued" }),
                    json!({ "name": "Windows (x86-64, cross-built)", "state": "passed", "job": "j9", "run": "release", "start": NOW_S + 292, "end": NOW_S + 855 }),
                ]
            );
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
            // Step 5's pace line: nothing in flight, production on C1 one commit behind, both runners on it.
            assert_eq!(
                cache.pace(Some("hyper-light/kiln"), None, &runners()),
                Some(json!({ "in_flight": 0, "agents": 0, "envs": [{ "repo": "hyper-light/kiln", "environment": "production", "short": short(C1), "behind": 1 }], "runners": { "on": 2, "of": 2 } }))
            );
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
            // Production on C2, behind by nothing; no runner on it yet, so no runners' count.
            assert_eq!(
                cache.pace(None, None, &runners()),
                Some(json!({ "in_flight": 0, "agents": 0, "envs": [{ "repo": "hyper-light/kiln", "environment": "production", "short": short(C2), "behind": 0 }], "runners": null }))
            );
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
            // Hand off first on a failed check, as the mock draws it (W26-R18).
            assert_eq!(names, vec!["hand_off", "retry", "approve", "hold", "retry_runner"]);
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
                    // Ship's rows (W26-R29): each repository's newest commit, through the same facets.
                    if limit == 2 {
                        let mut hq: HashMap<String, String> = fq.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                        hq.insert("by".into(), "repo".into());
                        let want = heads_in(store, &hq, None, viewer, runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
                        if let Some(got) = cache.heads(None, &Facets::from_query(&hq), viewer).map_err(|(_, m)| anyhow::anyhow!(m))? {
                            assert_eq!(serde_json::from_str::<Value>(&got)?, want, "round {round}: the repositories' newest commits {fq:?} for {viewer:?} from memory aren't the store's");
                            compared += 1;
                        }
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

    /// W21-R15: the `ship` frames a refresh sends carry every change a page shows. Through random
    /// changes to every table the model reads, a page held from before with each frame's rows put in
    /// place (by sha, newest first; `yours` where the viewer holds `whose`) and its `gone` taken out
    /// is the page memory serves after, for a viewer in a change's turn, one out of it and none; the
    /// pace line moved only where a frame says so; and a detail read before is the detail after
    /// with its frame's row laid over it when the frame's `dv` is its own, else a frame names it.
    #[test]
    fn w21_r15_ship_frames_carry_every_change_a_page_shows() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let sql = fixture();
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let cache = ShipCache::new();
            let mut now = NOW_S * 1000;
            let first = cache.refresh(&store, &runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!(first, Some(Vec::new()), "the first build sends nothing: no page has rows from before it");
            const REPO: &str = "hyper-light/kiln";
            let (kim, sam): (HashSet<String>, HashSet<String>) = (["user:kim".to_string(), "github:kim".to_string()].into(), ["user:sam".to_string()].into());
            let viewers = [None, Some(&kim), Some(&sam)];
            let rows_of = |cache: &ShipCache, viewer: Option<&HashSet<String>>| -> anyhow::Result<Vec<Value>> {
                let page = cache.page(Some(REPO), None, &Facets::default(), viewer, i64::MAX, 1000).map_err(|(_, m)| anyhow::anyhow!(m))?.ok_or_else(|| anyhow::anyhow!("memory holds every commit"))?;
                Ok(serde_json::from_str::<Value>(&page)?.get("commits").and_then(Value::as_array).cloned().unwrap_or_default())
            };
            let sha_of = |v: &Value| v.get("sha").and_then(Value::as_str).unwrap_or_default().to_owned();
            let pick = |r: &crate::wire::events::ShipRow, viewer: Option<&HashSet<String>>| match &r.yours {
                Some(y) if holds(&r.whose, viewer) => y.clone(),
                _ => r.row.clone(),
            };
            let details = |cache: &ShipCache, rows: &[Value], viewer: Option<&HashSet<String>>, now: i64| -> HashMap<String, Value> { rows.iter().filter_map(|c| cache.detail(REPO, &sha_of(c), &runners(), now, viewer).map(|d| (sha_of(c), d))).collect() };
            let mut held: Vec<(Vec<Value>, HashMap<String, Value>)> = Vec::new();
            for v in viewers {
                let rows = rows_of(&cache, v)?;
                let d = details(&cache, &rows, v, now);
                held.push((rows, d));
            }
            let mut pace = cache.pace(Some(REPO), None, &runners());
            let shas = [C1, C2, C3, "4444444ddddddddddddddddddddddddddddddddd", "5555555eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee", "6666666fffffffffffffffffffffffffffffffff", "8888888999999999999999999999999999999999"];
            let mut seed: u64 = 0x00dd_ba11;
            let mut next = move |n: u64| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed % n.max(1)
            };
            let (mut sent, mut patched, mut moved, mut reread) = (0usize, 0usize, 0usize, 0usize);
            for round in 0..80u64 {
                for k in 0..3 {
                    let sha = shas.get(usize::try_from(next(7)).unwrap_or(0)).copied().unwrap_or(C1);
                    let sql = ops(1000 + round * 3 + k, next(33), sha);
                    store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
                }
                now += FRESH_MS;
                let frames = cache.refresh(&store, &runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?.ok_or_else(|| anyhow::anyhow!("one refresh at a time: none was running"))?;
                let frame = frames.iter().find(|f| f.repo == REPO);
                assert!(frames.iter().all(|f| f.repo == REPO && !f.whole), "round {round}: one repository, never renamed: {frames:?}");
                sent += frame.map_or(0, |f| f.rows.len());
                for f in &frames {
                    let mut seen = HashSet::new();
                    assert!(f.rows.iter().all(|r| seen.insert(sha_of(&r.row))), "round {round}: a row twice in one frame");
                }
                let now_pace = cache.pace(Some(REPO), None, &runners());
                assert!(now_pace == pace || frame.is_some_and(|f| f.pace), "round {round}: the pace line moved without a frame saying so: {pace:?} → {now_pace:?}");
                pace = now_pace;
                for (v, (rows, ds)) in viewers.iter().zip(held.iter_mut()) {
                    let mut got: Vec<Value> = rows.clone();
                    if let Some(f) = frame {
                        got.retain(|c| !f.gone.contains(&sha_of(c)));
                        for r in &f.rows {
                            let row = pick(r, *v);
                            match got.iter_mut().find(|c| sha_of(c) == sha_of(&row)) {
                                Some(slot) => *slot = row,
                                None => got.push(row),
                            }
                        }
                        got.sort_by_key(|c| std::cmp::Reverse(c.get("cursor").and_then(Value::as_i64).unwrap_or(0)));
                    }
                    let want = rows_of(&cache, *v)?;
                    assert_eq!(got, want, "round {round}: the page held, with the frames put in place, isn't the page after, for {v:?}");
                    *rows = want;
                    // Each detail read before: unchanged, or its frame laid over it (its row, and its
                    // parts where they moved from its `dv`), or named to be read again.
                    for (sha, before) in ds.iter() {
                        let Some(after) = cache.detail(REPO, sha, &runners(), now, *v) else { continue };
                        let named = frame.and_then(|f| f.rows.iter().find(|r| sha_of(&r.row) == *sha));
                        let was = before.get("dv").and_then(Value::as_str);
                        match named {
                            None => assert_eq!(&after, before, "round {round}: {sha}'s detail changed with no frame naming it, for {v:?}"),
                            Some(r) if r.dv.is_some() && (r.dv.as_deref() == was || r.base.as_deref() == was) => {
                                let mut laid = before.clone();
                                if let (Some(o), Value::Object(row)) = (laid.as_object_mut(), pick(r, *v)) {
                                    o.extend(row);
                                    if r.dv.as_deref() != was {
                                        moved += 1;
                                        o.extend(r.parts.clone());
                                        for (k, it) in &r.items {
                                            let mut list = o.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
                                            list.resize(it.len, Value::Null);
                                            for x in &it.at {
                                                if let Some(slot) = list.get_mut(x.i) {
                                                    *slot = x.v.clone();
                                                }
                                            }
                                            o.insert(k.clone(), Value::Array(list));
                                        }
                                        o.insert("dv".into(), json!(r.dv));
                                    }
                                }
                                assert_eq!(after, laid, "round {round}: {sha}'s detail isn't its frame laid over the detail before, for {v:?}");
                                patched += 1;
                            }
                            Some(_) => reread += 1,
                        }
                    }
                    *ds = details(&cache, rows, *v, now);
                }
            }
            println!("ship frames: {sent} rows sent; details patched {patched} times ({moved} with their parts), read again {reread}");
            assert!(sent >= 40 && moved > 0 && patched > reread, "the changes reached too little: {sent} rows, {patched} patched ({moved} with parts), {reread} read again");
            Ok(())
        })
    }

    /// W21-R15: a commit gone from memory is in its frame's `gone`, and a row that reads as it did
    /// isn't sent: a refresh with nothing changed sends nothing.
    #[test]
    fn w21_r15_ship_frames_say_what_went_and_skip_what_holds() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let sql = fixture();
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let cache = ShipCache::with_cap(3);
            let now = NOW_S * 1000;
            cache.refresh(&store, &runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let quiet = cache.refresh(&store, &runners(), &[], now + FRESH_MS).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!(quiet, Some(Vec::new()), "nothing changed, nothing sent");
            // A newer commit: memory holds three, so the oldest goes.
            let sql = format!("INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, message, author, status, created_at, attempt) VALUES ('r9', 'kiln', 9, 'ci', 'push', 'main', '9999999{z}', 'newest', 'ada', 'queued', {NOW_S}, 1);", z = "a".repeat(33));
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let frames = cache.refresh(&store, &runners(), &[], now + 2 * FRESH_MS).await.map_err(|(_, m)| anyhow::anyhow!(m))?.unwrap_or_default();
            let f = frames.first().ok_or_else(|| anyhow::anyhow!("a frame for the new commit"))?;
            assert_eq!(f.repo, "hyper-light/kiln");
            assert!(f.rows.iter().any(|r| r.row.get("message").and_then(Value::as_str) == Some("newest")), "{f:?}");
            assert_eq!(f.gone, vec![C1.to_string()], "the oldest of the four is no longer held");
            // Its event frame, as the socket sends it.
            let text = ServerEvent::Ship(Box::new(f.clone())).text();
            assert!(text.starts_with("{\"type\":\"ship\",\"repo\":\"hyper-light/kiln\""), "{text}");
            assert!(moves_rows("{\"type\":\"job\",\"job\":{}}") && moves_rows("{\"type\":\"cd_apps\",\"apps\":[]}") && !moves_rows(&text) && !moves_rows("{\"type\":\"harness\"}"));
            Ok(())
        })
    }

    /// W21-R15: a detail's parts that moved go as the entries of a list that changed, or the part
    /// whole where that's shorter; a part gone can't be said, so it isn't.
    #[test]
    fn w21_r15_parts_delta_says_entries_or_wholes() -> anyhow::Result<()> {
        let map = |v: Value| v.as_object().cloned().unwrap_or_default();
        let jobs = |n: usize, moved: usize| (0..n).map(|i| json!({ "id": format!("job-{i}"), "verbs": if i == moved { json!(["cancel"]) } else { json!(["retry"]) } })).collect::<Vec<_>>();
        let old = map(json!({ "jobs": jobs(12, 99), "build": { "image": "a" }, "review": [] }));
        let new = map(json!({ "jobs": jobs(13, 4), "build": { "image": "b" }, "review": [] }));
        let (parts, items) = parts_delta(&old, &new).ok_or_else(|| anyhow::anyhow!("every part is still there"))?;
        assert_eq!(parts.keys().collect::<Vec<_>>(), ["build"], "an object goes whole; what didn't move doesn't go");
        let it = items.get("jobs");
        assert_eq!(it.map(|i| (i.len, i.at.iter().map(|x| x.i).collect::<Vec<_>>())), Some((13, vec![4, 12])), "the entry that moved and the one that came");
        // Every entry moved: the list whole is shorter than its entries with their places.
        let all = map(json!({ "jobs": (0..12).map(|i| json!(i)).collect::<Vec<_>>() }));
        let shifted = map(json!({ "jobs": (1..13).map(|i| json!(i)).collect::<Vec<_>>() }));
        assert!(parts_delta(&all, &shifted).is_some_and(|(p, i)| p.contains_key("jobs") && i.is_empty()));
        assert!(parts_delta(&old, &map(json!({ "jobs": [] }))).is_none(), "a part gone");
        Ok(())
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

    /// A future's longest poll: how long one of its polls held its thread at the most.
    struct Longest<F> {
        inner: std::pin::Pin<Box<F>>,
        most: std::time::Duration,
    }

    impl<F: std::future::Future> std::future::Future for Longest<F> {
        type Output = (F::Output, std::time::Duration);
        fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
            let t = std::time::Instant::now();
            let polled = self.inner.as_mut().poll(cx);
            self.most = self.most.max(t.elapsed());
            polled.map(|v| (v, self.most))
        }
    }

    /// Goal 3, plan 01a1a59c2a8e step 5 (perf/api-pass's leftover): the ship page's rows are drawn on
    /// the compute pool, not on the shard of the request that brings the model up to date. Building
    /// the model of 1,000 commits (4 runs of 2 legs each), no poll of the refresh holds its thread
    /// for a fifth of what drawing the rows takes; drawn on the shard, the drawing was one poll.
    #[test]
    fn g3_s5_the_ship_pages_rows_are_drawn_off_the_requests_shard() -> anyhow::Result<()> {
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
                    for commit in 0..1000i64 {
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
            let (built, longest) = Longest { inner: Box::pin(cache.refresh(&store, &runners(), &[], now)), most: std::time::Duration::ZERO }.await;
            built.map_err(|(_, m)| anyhow::anyhow!(m))?;
            // What drawing every row takes, on this thread.
            let recs: Vec<Records> = cache.held.read().map_err(|_| anyhow::anyhow!("the model's lock was poisoned"))?.repos.values().flat_map(|r| r.rows.values().map(|row| row.rec.clone())).collect();
            anyhow::ensure!(recs.len() == 1000, "{} commits held", recs.len());
            let t = std::time::Instant::now();
            for rec in recs {
                Row::of(rec, &runners(), now, false).map_err(|(_, m)| anyhow::anyhow!(m))?;
            }
            let draw = t.elapsed();
            anyhow::ensure!(longest * 5 < draw, "the refresh held its thread {longest:?} in one poll; drawing the rows takes {draw:?}");
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

    /// D41, as `w26_ac01`'s: agent:matrix's diff (for sam) with kim's open question on its version 1.
    const D41: &str = "4141414aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn d41(author: &str) -> String {
        let t = NOW_S - 600;
        format!(
            "INSERT INTO diffs (id, repo_id, number, kind, title, author, author_kind, author_for, state, trunk, created_at, updated_at) VALUES ('d41', 'kiln', 41, 'diff', 'pipeline: dynamic matrices', '{author}', 'agent', 'user:sam', 'needs-review', 'main', {t}, {t});
             INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, message, pushed_by, created_at) VALUES ('d41', 1, '{D41}', 'p', 'b', 'pipeline: dynamic matrices', '{author}', {t});
             INSERT INTO review_threads (id, diff_id, version, file, start_line, state, created_by, created_at, kind) VALUES ('th1', 'd41', 1, 'crates/kiln-ci/src/plan/matrix.rs', 42, 'open', 'user:kim', {t}, 'question');
             INSERT INTO review_comments (id, thread_id, author, body, created_at, updated_at, published) VALUES ('c1', 'th1', 'user:kim', 'empty matrix meant?', {t}, {t}, 1);"
        )
    }

    /// A page's row for the commit `short`: its stand, its line and its actions' names.
    fn stand_of(page: &Value, short: &str) -> (String, String, Vec<String>) {
        let s = |c: &Value, k: &str| c.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        page.get("commits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|c| s(c, "short") == short)
            .map(|c| (s(c, "stand"), s(c, "line"), c.get("actions").and_then(Value::as_array).into_iter().flatten().filter_map(|a| a.get("name").and_then(Value::as_str)).map(str::to_string).collect()))
            .unwrap_or_default()
    }

    fn row(stand: &str, line: &str, actions: &[&str]) -> (String, String, Vec<String>) {
        (stand.to_string(), line.to_string(), actions.iter().map(|a| a.to_string()).collect())
    }

    /// W26-R18: what was handed off says who is on it. A person's failure handed to an agent reads
    /// "<who> fixing", needs no one and offers Retry alone; a change's threads handed to its
    /// author's agent read fixing to everyone, with no Hand off and Review pair. From the store,
    /// and from memory once a refresh reads what the hand-off noted.
    #[test]
    fn w26_r18_what_was_handed_off_says_who_is_on_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let sql = fixture() + &d41("agent:matrix");
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let now = NOW_S * 1000;
            let sam: HashSet<String> = ["user:sam".to_string()].into();
            let from_store = |viewer: Option<HashSet<String>>| {
                let store = store.clone();
                async move { list_in(&store, &HashMap::new(), None, viewer.as_ref(), runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m)) }
            };
            let cache = ShipCache::new();
            let from_memory = |cache: &ShipCache, viewer: Option<&HashSet<String>>| -> Value { cache.page(None, None, &Facets::default(), viewer, i64::MAX, 20).ok().flatten().and_then(|p| serde_json::from_str(&p).ok()).unwrap_or_default() };
            cache.refresh(&store, &runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m))?;

            // Before: each needs its person, with Hand off first (the mock's order).
            let before = from_store(Some(sam.clone())).await?;
            assert_eq!(stand_of(&before, "3333333"), row("needs_you", "rust lint + test · assertion failed in sync::reconnect", &["hand_off", "retry"]));
            assert_eq!(stand_of(&before, "4141414"), row("needs_you", "kim: empty matrix meant? · 1 thread", &["hand_off", "review"]));
            let hand = before.get("commits").and_then(Value::as_array).into_iter().flatten().filter_map(|c| c.get("actions").and_then(Value::as_array)).flatten().find(|a| a.get("name") == Some(&json!("hand_off"))).cloned().unwrap_or_default();
            assert_eq!((hand.get("method"), hand.get("path")), (Some(&json!("POST")), Some(&json!("/api/ship/hand-off"))), "one endpoint, which decides: {hand}");

            // Handed off: C3's failure to ci-doctor, D41's thread to its agent at its revision.
            let sql = format!(
                "INSERT INTO ship_handoffs (repo_id, sha, what, to_name, agent, incident, version, by, at) VALUES ('kiln', '{C3}', 'checks', 'ci-doctor', 'a1', 'i1', NULL, 'user:mei', {NOW_S});
                 INSERT INTO ship_handoffs (repo_id, sha, what, to_name, agent, incident, version, by, at) VALUES ('kiln', '{D41}', 'threads', 'Matrix agent', 'matrix', NULL, 1, 'user:sam', {NOW_S});"
            );
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let failing = row("failing", "ci-doctor fixing · assertion failed in sync::reconnect", &["retry"]);
            let fixing = row("checking", "Matrix agent fixing · kim's thread", &[]);
            for viewer in [Some(sam.clone()), Some(["user:kim".to_string()].into()), None] {
                let after = from_store(viewer).await?;
                assert_eq!((stand_of(&after, "3333333"), stand_of(&after, "4141414")), (failing.clone(), fixing.clone()));
            }
            cache.refresh(&store, &runners(), &[], now + FRESH_MS).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let held = from_memory(&cache, Some(&sam));
            assert_eq!((stand_of(&held, "3333333"), stand_of(&held, "4141414")), (failing, fixing), "the hand-off's trigger brought them into memory");
            Ok(())
        })
    }

    /// W26-AC10: what kiln did with agents' feedback, as the item reads it. Held for a person (its
    /// agent ended), the review says why at that revision, and the item doesn't read fixing; handed
    /// by kiln after that, the newest counts and it reads the agent fixing.
    #[test]
    fn w26_ac10_agents_feedback_kiln_held_says_why_and_kilns_hand_off_reads_fixing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let sql = fixture() + &d41("agent:matrix");
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let now = NOW_S * 1000;
            let sam: HashSet<String> = ["user:sam".to_string()].into();
            let page = || {
                let (store, sam) = (store.clone(), sam.clone());
                async move { list_in(&store, &HashMap::new(), None, Some(&sam), runners(), &[], now).await.map_err(|(_, m)| anyhow::anyhow!(m)) }
            };
            let sql = format!("INSERT INTO ship_handoffs (repo_id, sha, what, to_name, agent, incident, version, by, at, why) VALUES ('kiln', '{D41}', 'held', 'Matrix agent', 'matrix', NULL, 1, 'kiln', {NOW_S}, 'The fix waits for a person to start or resume Matrix agent');");
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            assert_eq!(stand_of(&page().await?, "4141414"), row("needs_you", "The fix waits for a person to start or resume Matrix agent · 1 thread", &["hand_off", "review"]));
            let sql = format!("INSERT INTO ship_handoffs (repo_id, sha, what, to_name, agent, incident, version, by, at, why) VALUES ('kiln', '{D41}', 'threads', 'Matrix agent', 'matrix', NULL, 1, 'kiln', {}, 'kiln/kiln lets agents fix code from agents'' feedback');", NOW_S + 1);
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            assert_eq!(stand_of(&page().await?, "4141414"), row("checking", "Matrix agent fixing · kim's thread", &[]));
            // A draft stays a draft (W26-R02), naming who is at work on it.
            store.call(|c| Ok(c.execute("UPDATE diffs SET state = 'draft' WHERE id = 'd41'", []).map(|_| ())?)).await?;
            let (stand, line, _) = stand_of(&page().await?, "4141414");
            assert_eq!((stand.as_str(), line.as_str()), ("waiting", "Draft · Matrix agent fixing · kim's thread"));
            Ok(())
        })
    }

    /// Who made each commit, from the one place Ship reads it (made_by, W26-R18; Chain of Ownership
    /// fills it for every commit in time): an agent's change says its agent and the person it acts
    /// for; a person's change, its person; a lone commit, nothing yet (git's author name isn't a
    /// kiln principal).
    #[test]
    fn w26_r18_made_by_says_who_made_each_commit() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let sql = fixture() + &d41("agent:matrix");
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let page = list_in(&store, &HashMap::new(), None, None, runners(), &[], NOW_S * 1000).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let made = |short: &str| page.get("commits").and_then(Value::as_array).into_iter().flatten().find(|c| c.get("short") == Some(&json!(short))).and_then(|c| c.get("made_by").cloned());
            assert_eq!(made("4141414"), Some(json!({ "agent": "matrix", "person": "user:sam" })), "an agent's change: its agent, for its person");
            assert_eq!(made("3333333"), Some(Value::Null), "a lone commit: git's author name names no one kiln knows");
            let sql = fixture_person_change();
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let page = list_in(&store, &HashMap::new(), None, None, runners(), &[], NOW_S * 1000).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let lee = page.get("commits").and_then(Value::as_array).into_iter().flatten().find(|c| c.get("short") == Some(&json!("5151515"))).and_then(|c| c.get("made_by").cloned());
            assert_eq!(lee, Some(json!({ "person": "user:lee" })), "a person's change: its person, and no agent");
            Ok(())
        })
    }

    /// W26-R16 on Ship's row: a change kiln replayed as its trunk moved says so at the end of its
    /// line, the replay's base naming the tip it went onto; before the replay, the line says nothing
    /// of it. The row's line changes, so its cache must take the new version (diff_versions'
    /// triggers note the change).
    #[test]
    fn w26_r16_a_replay_ends_the_rows_line_with_how_fresh_it_is() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        crate::rt::block_on(async move {
            let sql = fixture() + &fixture_person_change();
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let line = |page: &Value, short: &str| page.get("commits").and_then(Value::as_array).into_iter().flatten().find(|c| c.get("short") == Some(&json!(short))).and_then(|c| c.get("line").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
            let page = list_in(&store, &HashMap::new(), None, None, runners(), &[], NOW_S * 1000).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let before = line(&page, "5151515");
            assert!(!before.contains("replays clean"), "{before}");
            let seq = |store: crate::store::Store| async move { store.call(|c| Ok(c.query_row("SELECT n FROM ship_seq", [], |r| r.get::<_, i64>(0))?)).await };
            let was = seq(store.clone()).await?;
            let t = NOW_S - 60;
            let replay = "5252525aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
            let sql = format!(
                "INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, message, pushed_by, created_at, rebase_only, why) VALUES ('d51', 2, '{replay}', '4c1e2a9f00', '4c1e2a9f00', 'sched: lee', 'kiln', {t}, 1, 'replayed');"
            );
            store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            assert!(seq(store.clone()).await? > was, "the replay notes the change for Ship's cache");
            let page = list_in(&store, &HashMap::new(), None, None, runners(), &[], NOW_S * 1000).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let after = line(&page, "5252525");
            assert!(after.ends_with(" · replays clean on main@4c1e2a") || after == "replays clean on main@4c1e2a", "{after}");
            Ok(())
        })
    }

    /// D51: lee's own diff, a person's change.
    fn fixture_person_change() -> String {
        let t = NOW_S - 300;
        let sha = "5151515aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        format!(
            "INSERT INTO diffs (id, repo_id, number, kind, title, author, author_kind, state, trunk, created_at, updated_at) VALUES ('d51', 'kiln', 51, 'diff', 'sched: lee', 'user:lee', 'human', 'needs-review', 'main', {t}, {t});
             INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, message, pushed_by, created_at) VALUES ('d51', 1, '{sha}', 'p', 'b', 'sched: lee', 'user:lee', {t});"
        )
    }

    /// W26-R18: Hand off gives a person's failure to the agent attached to what failed, as the
    /// failure itself would (an incident with its agent), and keeps who took it; with no one
    /// attached, nothing open, or no right to rerun, it says why and keeps nothing.
    #[test]
    fn w26_r18_hand_off_gives_a_failure_to_the_agent_attached_to_it() -> anyhow::Result<()> {
        within(async {
            let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
            let h = std::sync::Arc::clone(&hub);
            crate::rt::spawn(async move {
                let _ = crate::harness::registry::Registry::run_local(h).await;
            });
            hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
            let sql = fixture();
            hub.store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let owner = crate::authz::Caller::owner();
            let c3 = json!({ "repo": "hyper-light/kiln", "sha": C3 });
            let refused = |r: Result<Value, Fail>| r.err().map(|(c, m)| (c.as_u16(), m)).unwrap_or_default();
            let kept = |hub: std::sync::Arc<Hub>| async move { hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM ship_handoffs", [], |r| r.get::<_, i64>(0))?)).await };

            let (code, why) = refused(hand_off_in(&hub, &owner, &c3).await);
            anyhow::ensure!(code == 409 && why.contains("No agent looks after hyper-light/kiln"), "{code}: {why}");
            let (code, why) = refused(hand_off_in(&hub, &owner, &json!({ "repo": "hyper-light/kiln", "sha": C1 })).await);
            anyhow::ensure!(code == 409 && why.contains("nothing open"), "{code}: {why}");
            let (code, _) = refused(hand_off_in(&hub, &owner, &json!({ "repo": "hyper-light/kiln" })).await);
            anyhow::ensure!(code == 400, "no commit named: {code}");
            let (code, _) = refused(hand_off_in(&hub, &crate::authz::Caller::peer("user:ro", "viewer"), &c3).await);
            anyhow::ensure!(code == 403, "no right to rerun: {code}");
            anyhow::ensure!(kept(std::sync::Arc::clone(&hub)).await? == 0, "nothing kept while refused");

            let folder = tempfile::tempdir()?;
            let cwd = folder.path().to_string_lossy().into_owned();
            let cx = crate::ops::Cx { hub: std::sync::Arc::clone(&hub), caller: owner.clone() };
            crate::ops::call(cx, "attachments.save", json!({ "name": "ci-doctor", "on": "repo:hyper-light/kiln", "harness": { "kind": "claude", "on": "local", "cwd": cwd }, "wake": ["failure"] }))
                .await
                .map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let handed = hand_off_in(&hub, &owner, &c3).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            anyhow::ensure!(handed.pointer("/handed/to") == Some(&json!("ci-doctor")) && handed.pointer("/handed/what") == Some(&json!(["checks"])), "{handed}");
            let id = handed.pointer("/handed/incident").and_then(Value::as_str).unwrap_or_default();
            let incident = crate::ci::attach::incident(&hub, id).await.ok_or_else(|| anyhow::anyhow!("no incident {id}"))?;
            anyhow::ensure!(incident.agent.is_some() && incident.seen.first().and_then(|s| s.run.as_deref()) == Some("r5") && incident.title.contains("failed on main"), "{incident:?}");
            anyhow::ensure!(kept(std::sync::Arc::clone(&hub)).await? == 1, "kept once");

            // Its row says so; asked again, there's nothing more to hand.
            let page = list_in(&hub.store, &HashMap::new(), None, None, runners(), &[], NOW_S * 1000).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            anyhow::ensure!(stand_of(&page, "3333333") == row("failing", "ci-doctor fixing · assertion failed in sync::reconnect", &["retry"]), "{:?}", stand_of(&page, "3333333"));
            let (code, why) = refused(hand_off_in(&hub, &owner, &c3).await);
            anyhow::ensure!(code == 409 && why.contains("nothing open"), "{code}: {why}");
            Ok(())
        })
    }

    /// W27-R08 (the Transfers plan's step 6): a Hand off changes hands signed. From a device with
    /// no key, it's kiln's take for the person on their signed-in session, saying their own
    /// signature is missing, to the agent the failure goes to; the agent's receipt answers it, and
    /// the chain walks unbroken, its first link kiln's for the person, never the person's.
    #[test]
    fn w27_r08_a_hand_off_changes_hands_signed() -> anyhow::Result<()> {
        within(async {
            let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
            let h = std::sync::Arc::clone(&hub);
            crate::rt::spawn(async move {
                let _ = crate::harness::registry::Registry::run_local(h).await;
            });
            hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
            crate::identities::own_identities(&hub.store, &hub.pki).await?;
            let sql = fixture();
            hub.store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let owner = crate::authz::Caller::owner();
            let folder = tempfile::tempdir()?;
            let cwd = folder.path().to_string_lossy().into_owned();
            let cx = crate::ops::Cx { hub: std::sync::Arc::clone(&hub), caller: owner.clone() };
            crate::ops::call(cx, "attachments.save", json!({ "name": "ci-doctor", "on": "repo:hyper-light/kiln", "harness": { "kind": "claude", "on": "local", "cwd": cwd }, "wake": ["failure"] }))
                .await
                .map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let handed = hand_off_in(&hub, &owner, &json!({ "repo": "hyper-light/kiln", "sha": C3 })).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            anyhow::ensure!(handed.pointer("/custody/kiln_signed_for_you") == Some(&json!(true)), "{handed}");
            let hand_on = handed.pointer("/custody/hand_on").and_then(Value::as_str).unwrap_or_default().to_owned();
            let agent = handed.pointer("/handed/agent").and_then(Value::as_str).unwrap_or_default().to_owned();
            // The agent's receipt, signed with a key certified for its identity.
            let to = crate::identities::agent_at(&hub, &agent, hub.harness.epoch_of(&agent)).await?;
            let key = crate::pki::new_key_pem()?;
            let csr = crate::pki::make_csr_with_key("kiln-identity", &key)?;
            let pem = hub.pki.issue_identity(&csr, &kiln_ci::identity::chain::Identity::parse(&to)?, &[], time::Duration::hours(1))?;
            let org = hub.pki.organization();
            let (person, server) = (crate::custody::person_uri(&org, "admin"), crate::custody::server_uri(&org));
            let thing = crate::custody::work("hyper-light/kiln@3333333 checks", &["hyper-light/kiln", C3, "checks"]);
            let body = kiln_ci::identity::custody::Body { from: Some(person.clone()), why: "handed off".into(), ..kiln_ci::identity::custody::Body::new(kiln_ci::identity::custody::Kind::Received, &to) };
            let st = kiln_ci::identity::statement::Statement { subject: vec![thing], follows: vec![hand_on], ..kiln_ci::identity::statement::Statement::new(kiln_ci::identity::custody::to_value(&body)) };
            let receipt = {
                use rcgen::SigningKey as _;
                use kiln_ci::identity::statement::{PAYLOAD_CUSTODY, bundle, payload, to_sign};
                let p = payload(&st)?;
                bundle(PAYLOAD_CUSTODY, &p, &rcgen::KeyPair::from_pem(&key)?.sign(&to_sign(PAYLOAD_CUSTODY, &p))?, &crate::pki::der_of(&pem)?)
            };
            let now = crate::util::now_unix();
            let got = crate::custody::take(&hub.store, &hub.pki.ca_der, &org, &receipt, "runner local", now).await?.map_err(|r| anyhow::anyhow!("{r}"))?;
            let v = crate::custody::chain(&hub.store, &hub.pki.ca_der, &org, &got.digest, now).await?.ok_or_else(|| anyhow::anyhow!("no chain"))?;
            anyhow::ensure!(v.get("unbroken") == Some(&json!(true)), "{v}");
            let taken = v.pointer("/hops/1").cloned().unwrap_or_default();
            anyhow::ensure!(taken.get("kind") == Some(&json!("taken")) && taken.get("custodian") == Some(&json!(server)) && taken.get("taken_for") == Some(&json!(person)), "kiln for the person: {taken}");
            anyhow::ensure!(taken.get("why").and_then(Value::as_str).is_some_and(|w| w.contains("no key on the device that asked")), "{taken}");
            Ok(())
        })
    }

    /// W27-R08, from a browser with its own key: the browser signs what kiln says
    /// (`hand_off_statement`), the person's hand-on to kiln's server, since kiln doesn't know yet who
    /// takes it. kiln passes it on unchanged to the agent the failure goes to, whose receipt follows,
    /// and the chain walks back to the person's own signature, no link of it kiln's for them. From a
    /// browser that can't sign, kiln's take for them gives the browser's reason.
    #[test]
    fn w27_r08_a_hand_off_from_a_browser_is_its_persons_own() -> anyhow::Result<()> {
        use base64::Engine as _;
        use kiln_ci::identity::custody::{Body, Kind, to_value};
        use kiln_ci::identity::statement::{PAYLOAD_CUSTODY, Statement, bundle, payload, to_sign};
        use rcgen::SigningKey as _;
        within(async {
            for signs in [true, false] {
                let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
                let h = std::sync::Arc::clone(&hub);
                crate::rt::spawn(async move {
                    let _ = crate::harness::registry::Registry::run_local(h).await;
                });
                hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
                crate::identities::own_identities(&hub.store, &hub.pki).await?;
                let sql = fixture();
                hub.store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
                let owner = crate::authz::Caller::owner();
                let folder = tempfile::tempdir()?;
                let cwd = folder.path().to_string_lossy().into_owned();
                let cx = crate::ops::Cx { hub: std::sync::Arc::clone(&hub), caller: owner.clone() };
                crate::ops::call(cx, "attachments.save", json!({ "name": "ci-doctor", "on": "repo:hyper-light/kiln", "harness": { "kind": "claude", "on": "local", "cwd": cwd }, "wake": ["failure"] }))
                    .await
                    .map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
                let org = hub.pki.organization();
                let (person, server) = (crate::custody::person_uri(&org, "admin"), crate::custody::server_uri(&org));
                let commit = json!({ "repo": "hyper-light/kiln", "sha": C3 });
                let ask = if signs {
                    // What kiln says to sign, signed with the browser's key, certified as the person.
                    let said = hand_off_statement_in(&hub, &owner, &commit).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
                    anyhow::ensure!(said.pointer("/statement/body/by") == Some(&json!(person)) && said.pointer("/statement/body/to") == Some(&json!(server)), "{said}");
                    let key = crate::pki::new_key_pem()?;
                    let pem = hub.pki.issue_identity(&crate::pki::make_csr_with_key("kiln-identity", &key)?, &kiln_ci::identity::chain::Identity::parse(&person)?, &[], time::Duration::hours(1))?;
                    let b64 = base64::engine::general_purpose::STANDARD;
                    let part = |k: &str| b64.decode(said.get(k).and_then(Value::as_str).unwrap_or_default());
                    let (p, pae) = (part("payload")?, part("pae")?);
                    let hand_on = bundle(PAYLOAD_CUSTODY, &p, &rcgen::KeyPair::from_pem(&key)?.sign(&pae)?, &crate::pki::der_of(&pem)?);
                    json!({ "repo": "hyper-light/kiln", "sha": C3, "hand_on": hand_on })
                } else {
                    json!({ "repo": "hyper-light/kiln", "sha": C3, "unsigned": "this page isn't secure (https), so this browser has no key" })
                };
                let handed = hand_off_in(&hub, &owner, &ask).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
                anyhow::ensure!(handed.pointer("/custody/kiln_signed_for_you") == Some(&json!(!signs)), "{handed}");
                // The agent's receipt of what kiln handed it: what kiln passed on, else its take.
                let agent = handed.pointer("/handed/agent").and_then(Value::as_str).unwrap_or_default().to_owned();
                let to = crate::identities::agent_at(&hub, &agent, hub.harness.epoch_of(&agent)).await?;
                let follows = handed.pointer(if signs { "/custody/passed_on" } else { "/custody/hand_on" }).and_then(Value::as_str).unwrap_or_default().to_owned();
                let key = crate::pki::new_key_pem()?;
                let pem = hub.pki.issue_identity(&crate::pki::make_csr_with_key("kiln-identity", &key)?, &kiln_ci::identity::chain::Identity::parse(&to)?, &[], time::Duration::hours(1))?;
                let thing = crate::custody::work("hyper-light/kiln@3333333 checks", &["hyper-light/kiln", C3, "checks"]);
                let body = Body { from: Some(if signs { server.clone() } else { person.clone() }), why: "handed off".into(), ..Body::new(Kind::Received, &to) };
                let st = Statement { subject: vec![thing], follows: vec![follows], ..Statement::new(to_value(&body)) };
                let p = payload(&st)?;
                let receipt = bundle(PAYLOAD_CUSTODY, &p, &rcgen::KeyPair::from_pem(&key)?.sign(&to_sign(PAYLOAD_CUSTODY, &p))?, &crate::pki::der_of(&pem)?);
                let now = crate::util::now_unix();
                let got = crate::custody::take(&hub.store, &hub.pki.ca_der, &org, &receipt, "runner local", now).await?.map_err(|r| anyhow::anyhow!("{r}"))?;
                let v = crate::custody::chain(&hub.store, &hub.pki.ca_der, &org, &got.digest, now).await?.ok_or_else(|| anyhow::anyhow!("no chain"))?;
                anyhow::ensure!(v.get("unbroken") == Some(&json!(true)), "{v}");
                let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
                let kinds: Vec<&str> = hops.iter().filter_map(|h| h.get("kind").and_then(Value::as_str)).collect();
                if signs {
                    anyhow::ensure!(kinds == ["received", "passed-on", "handed-on"], "the person's own hand-on, passed on: {v}");
                    anyhow::ensure!(hops.get(2).and_then(|h| h.get("custodian")) == Some(&json!(person)), "{v}");
                    anyhow::ensure!(hops.iter().all(|h| h.get("taken_for").is_none_or(Value::is_null)), "nothing of it kiln's for them: {v}");
                } else {
                    anyhow::ensure!(kinds == ["received", "taken"], "{v}");
                    let taken = hops.get(1).cloned().unwrap_or_default();
                    anyhow::ensure!(taken.get("taken_for") == Some(&json!(person)), "kiln for the person: {taken}");
                    anyhow::ensure!(taken.get("why").and_then(Value::as_str).is_some_and(|w| w.contains("this page isn't secure (https), so this browser has no key")), "{taken}");
                }
            }
            Ok(())
        })
    }

    /// W26-R18: a change's Hand off goes to its author's agent while it runs: its open threads, in
    /// one message queued after its turn, and the change reads fixing; once that agent has ended,
    /// Hand off says to start one.
    #[cfg(unix)]
    #[test]
    fn w26_r18_hand_off_gives_a_changes_threads_to_its_authors_agent() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        within(async {
            let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
            let h = std::sync::Arc::clone(&hub);
            crate::rt::spawn(async move {
                let _ = crate::harness::registry::Registry::run_local(h).await;
            });
            // An agent that stays up and says nothing, keeping what it's given.
            let dir = tempfile::tempdir()?;
            let (program, heard) = (dir.path().join("quiet-claude"), dir.path().join("heard.jsonl"));
            std::fs::write(&program, format!("#!/bin/sh\nexec cat >> '{}'\n", heard.display()))?;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
            let spec = crate::harness::session::Spec { kind: "claude".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), program: Some(program.to_string_lossy().into_owned()), ..Default::default() };
            let opened = hub.harness.open(&hub, "local", spec, 1).await.map_err(|e| anyhow::anyhow!("{e:#}"))?;
            let agent = opened.into_iter().next().map(|i| i.id).ok_or_else(|| anyhow::anyhow!("nothing opened"))?;
            let sql = fixture() + &d41(&format!("agent:{agent}"));
            hub.store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let owner = crate::authz::Caller::owner();
            let body = json!({ "repo": "hyper-light/kiln", "sha": D41 });

            // What it's told: the change, each open thread where it is and what it says, how to answer.
            let rec = hub.store.call(|c| load(c, "hyper-light/kiln", D41, &[])).await?.ok_or_else(|| anyhow::anyhow!("no D41"))?;
            let ch = rec.change.clone().ok_or_else(|| anyhow::anyhow!("D41 has no change"))?;
            let base = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
            let told = handed_text(&rec, &ch, &ch.conflicts, &ch.threads, None, &base, &owner.name);
            anyhow::ensure!(
                told.contains("your change D41 (version 1, commit 4141414)")
                    && told.contains("- thread th1, crates/kiln-ci/src/plan/matrix.rs:42 on version 1 (question), kim: empty matrix meant?")
                    && told.contains("ci.diffs.reply")
                    && told.contains("agents.change {\"threads\": [\"th1\"]}")
                    && told.ends_with("/diffs/D41"),
                "{told}"
            );
            let handed = hand_off_in(&hub, &owner, &body).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            anyhow::ensure!(handed.pointer("/handed/agent") == Some(&json!(agent)) && handed.pointer("/handed/what") == Some(&json!(["threads"])), "{handed}");
            // Those very words reach it (its turn isn't under way, so at once).
            let mut got = String::new();
            for _ in 0..250 {
                got = std::fs::read_to_string(&heard).unwrap_or_default();
                if got.contains("your change D41 (version 1, commit 4141414)") {
                    break;
                }
                crate::rt::sleep(std::time::Duration::from_millis(20)).await;
            }
            anyhow::ensure!(got.contains("your change D41 (version 1, commit 4141414)") && got.contains("matrix.rs:42 on version 1 (question), kim: empty matrix meant?"), "it heard: {got}");
            let page = list_in(&hub.store, &HashMap::new(), None, None, runners(), &[], NOW_S * 1000).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let (stand, line, _) = stand_of(&page, "4141414");
            anyhow::ensure!(stand == "checking" && line.ends_with("fixing · kim's thread"), "{stand}: {line}");

            // Its agent gone, Hand off has no one to give it to.
            let cx = crate::ops::Cx { hub: std::sync::Arc::clone(&hub), caller: owner.clone() };
            crate::ops::call(cx, "agents.stop", json!({ "agent": agent })).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            for _ in 0..500 {
                if hub.harness.info(&agent).is_some_and(|i| i.status == crate::harness::model::Status::Exited) {
                    break;
                }
                crate::rt::sleep(std::time::Duration::from_millis(20)).await;
            }
            hub.store.call(|c| Ok(c.execute("DELETE FROM ship_handoffs", []).map(|_| ())?)).await?;
            let why = hand_off_in(&hub, &owner, &body).await.err().map(|(c, m)| (c.as_u16(), m)).unwrap_or_default();
            anyhow::ensure!(why.0 == 409 && why.1.contains("D41's agent isn't running now"), "{why:?}");
            Ok(())
        })
    }

    /// W26-R18, Hand off thread by thread, on a stacked diff (D41), a branch change request (CR13)
    /// and a draft one (CR14), each its author's agent's and each with two open threads, kim's and
    /// a review agent's. Handed off, its agent is told each thread's id, place and revision (oldest
    /// first, then by id), the agent's marked as an agent's, and how to answer them; its row reads that agent fixing (a
    /// draft stays a draft, W26-R02). Once the agent's fix comes as v2, the handed revision's row
    /// says it was fixed in v2.
    #[cfg(unix)]
    #[test]
    fn w26_r18_hand_off_names_each_thread_marks_agents_and_says_fixed_in() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        within(async {
            let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
            let h = std::sync::Arc::clone(&hub);
            crate::rt::spawn(async move {
                let _ = crate::harness::registry::Registry::run_local(h).await;
            });
            // (diff id, number, kind, state, branch, its v1 commit, its v2 commit, its name)
            let changes = [
                ("d41", 41, "diff", "needs-review", "NULL", "4141414aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "4242424aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "D41"),
                ("cr13", 13, "change_request", "needs-review", "'b13'", "5151515aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "5252525aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "CR13"),
                ("cr14", 14, "change_request", "draft", "'b14'", "6161616aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "6262626aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "CR14"),
            ];
            // Each change's agent, which stays up and says nothing, keeping what it's given: it never
            // ends the turn a hand-off starts, so one agent would be sent only the first.
            let dir = tempfile::tempdir()?;
            let mut agents = Vec::new();
            for (.., name) in &changes {
                let (program, heard) = (dir.path().join(format!("quiet-claude-{name}")), dir.path().join(format!("heard-{name}.jsonl")));
                std::fs::write(&program, format!("#!/bin/sh\nexec cat >> '{}'\n", heard.display()))?;
                std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
                let spec = crate::harness::session::Spec { kind: "claude".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), program: Some(program.to_string_lossy().into_owned()), ..Default::default() };
                let agent = hub.harness.open(&hub, "local", spec, 1).await.map_err(|e| anyhow::anyhow!("{e:#}"))?.into_iter().next().map(|i| i.id).ok_or_else(|| anyhow::anyhow!("nothing opened"))?;
                agents.push((agent, heard));
            }
            let t = NOW_S - 600;
            let mut sql = fixture();
            for ((id, n, kind, state, branch, v1, _, _), (agent, _)) in changes.iter().zip(&agents) {
                sql.push_str(&format!(
                    "INSERT INTO diffs (id, repo_id, number, kind, title, author, author_kind, author_for, state, trunk, branch, created_at, updated_at) VALUES ('{id}', 'kiln', {n}, '{kind}', 'legs {n}', 'agent:{agent}', 'agent', 'user:sam', '{state}', 'main', {branch}, {t}, {t});
                     INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, message, pushed_by, created_at) VALUES ('{id}', 1, '{v1}', 'p', 'b', 'legs {n}', 'agent:{agent}', {t});
                     INSERT INTO review_threads (id, diff_id, version, file, start_line, state, created_by, created_at, kind) VALUES ('{id}k', '{id}', 1, 'src/a.rs', 3, 'open', 'user:kim', {t}, 'question'), ('{id}a', '{id}', 1, 'src/a.rs', 9, 'open', 'agent:review@box-a', {t}, 'issue');
                     INSERT INTO review_comments (id, thread_id, author, body, created_at, updated_at, published) VALUES ('{id}kc', '{id}k', 'user:kim', 'why three?', {t}, {t}, 1), ('{id}ac', '{id}a', 'agent:review@box-a', 'an empty matrix should mean no legs', {t}, {t}, 1);"
                ));
            }
            hub.store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let owner = crate::authz::Caller::owner();
            let page = || {
                let store = hub.store.clone();
                async move { list_in(&store, &HashMap::new(), None, None, runners(), &[], NOW_S * 1000).await.map_err(|(_, m)| anyhow::anyhow!(m)) }
            };
            for ((id, _, _, state, _, v1, _, name), (_, heard)) in changes.iter().zip(&agents) {
                let handed = hand_off_in(&hub, &owner, &json!({ "repo": "hyper-light/kiln", "sha": v1 })).await.map_err(|(c, m)| anyhow::anyhow!("{name}: {c}: {m}"))?;
                anyhow::ensure!(handed.pointer("/handed/what") == Some(&json!(["threads"])), "{name}: {handed}");
                let mut got = String::new();
                for _ in 0..250 {
                    got = std::fs::read_to_string(heard).unwrap_or_default();
                    if got.contains(&format!("your change {name} (version 1")) {
                        break;
                    }
                    crate::rt::sleep(std::time::Duration::from_millis(20)).await;
                }
                anyhow::ensure!(
                    got.contains(&format!("- thread {id}k, src/a.rs:3 on version 1 (question), kim: why three?"))
                        && got.contains(&format!("- thread {id}a, src/a.rs:9 on version 1 (issue), "))
                        && got.contains("(an agent's): an empty matrix should mean no legs")
                        && got.contains(&format!("agents.change {{\\\"threads\\\": [\\\"{id}a\\\", \\\"{id}k\\\"]}}")),
                    "{name}: each thread, the agent's marked, and how to answer them: {got}"
                );
                let (stand, line, _) = stand_of(&page().await?, v1.get(..7).unwrap_or_default());
                let (want, start) = if *state == "draft" { ("waiting", "Draft · ") } else { ("checking", "") };
                anyhow::ensure!(stand == want && line.starts_with(start) && line.ends_with(" fixing · 2 threads"), "{name} reads its agent fixing: {stand}: {line}");
            }
            // The agents' fixes come, each v2 answering both threads.
            let mut sql = String::new();
            for ((id, _, _, _, _, _, v2, _), (agent, _)) in changes.iter().zip(&agents) {
                sql.push_str(&format!(
                    "INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, message, pushed_by, created_at, why, thread, note) VALUES ('{id}', 2, '{v2}', 'p', 'b', 'no legs', 'agent:{agent}', {}, 'agent''s fix', '{id}k', 'answers {id}k, {id}a');",
                    t + 60
                ));
            }
            hub.store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let after = page().await?;
            for (_, _, _, _, _, v1, _, name) in &changes {
                let (stand, line, _) = stand_of(&after, v1.get(..7).unwrap_or_default());
                anyhow::ensure!((stand.as_str(), line.as_str()) == ("superseded", "Fixed in v2"), "{name}'s handed revision says which version fixed it: {stand}: {line}");
            }
            Ok(())
        })
    }

    /// W26-R18, drafts (Ada, 2026-11-01): the Matrix agent's draft change request, made for sam,
    /// offers Ready on its row. lee, neither its person nor able to land, is refused, saying why.
    /// sam's Ready marks it ready for review: the row reads its review state with no Ready, and the
    /// agent's change says it's no draft. Asked again, it's no draft to mark.
    #[test]
    fn w26_r18_an_agents_draft_is_marked_ready_from_ship_by_its_person() -> anyhow::Result<()> {
        within(async {
            const CR15: &str = "5151515aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
            let hub = crate::hub::tests::scratch_hub().await?;
            let t = NOW_S - 600;
            let sql = fixture()
                + &format!(
                    "INSERT INTO diffs (id, repo_id, number, kind, title, author, author_kind, author_for, state, trunk, branch, created_at, updated_at) VALUES ('cr15', 'kiln', 15, 'change_request', 'cap a share', 'agent:matrix-session', 'agent', 'user:sam', 'draft', 'main', 'b15', {t}, {t});
                     INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, message, pushed_by, created_at) VALUES ('cr15', 1, '{CR15}', 'p', 'b', 'cap a share', 'agent:matrix-session', {t});"
                );
            hub.store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
            let record = crate::workspace::change::Record { agent: "matrix-session".into(), repo: Some("hyper-light/kiln".into()), provider: "kiln".into(), change: Some("CR15".into()), review: Some("draft".into()), draft: Some(true), ..Default::default() };
            crate::workspace::change::save(&hub, &record).await;
            let page = || {
                let store = hub.store.clone();
                async move { list_in(&store, &HashMap::new(), None, None, runners(), &[], NOW_S * 1000).await.map_err(|(_, m)| anyhow::anyhow!(m)) }
            };
            let (stand, line, actions) = stand_of(&page().await?, "5151515");
            anyhow::ensure!(line.starts_with("Draft") && actions.iter().any(|a| a == "ready"), "the draft offers Ready: {stand}: {line}: {actions:?}");
            let body = json!({ "repo": "hyper-light/kiln", "sha": CR15 });
            let refused = |r: Result<Value, Fail>| r.err().unwrap_or((StatusCode::OK, String::new()));
            let (code, why) = refused(ready_in(&hub, &crate::authz::Caller::peer("user:lee", "viewer"), &body).await);
            anyhow::ensure!(code == StatusCode::FORBIDDEN && why.contains("is for its agent's person (sam) or one who can land on hyper-light/kiln"), "lee: {code}: {why}");
            let sam = crate::authz::Caller::peer("user:sam", "viewer");
            ready_in(&hub, &sam, &body).await.map_err(|(c, m)| anyhow::anyhow!("sam: {c}: {m}"))?;
            let state = crate::ci::diffs::find(&hub, "CR15").await?.map(|d| d.state).unwrap_or_default();
            let (stand, line, actions) = stand_of(&page().await?, "5151515");
            anyhow::ensure!(state == "needs-review" && !line.starts_with("Draft") && !actions.iter().any(|a| a == "ready"), "ready for review: {state}; the row: {stand}: {line}: {actions:?}");
            let r = crate::workspace::change::of(&hub, "matrix-session").await.ok_or_else(|| anyhow::anyhow!("the agent's change went missing"))?;
            anyhow::ensure!((r.draft, r.review.as_deref()) == (Some(false), Some("needs-review")), "the agent's change says it's no draft: {r:?}");
            let (code, why) = refused(ready_in(&hub, &sam, &body).await);
            anyhow::ensure!(code == StatusCode::CONFLICT && why == "CR15 isn't a draft", "again: {code}: {why}");
            Ok(())
        })
    }

    /// A future run to its end on a thread of its own, within two minutes.
    /// W26-R21, R22: a commit's jobs acted on together across its two runs (`POST /api/ship/jobs`):
    /// paused by a word (`waiting`) and resumed by name, cancelled by a word (`running`), run again
    /// (`failed`). A running job whose runner can't freeze it says why, as do a job that isn't the
    /// commit's, a verb or scope that isn't one, a caller without the verb's right, and a commit
    /// that isn't. The commit's `jobs` say what each can do, and why not.
    #[test]
    fn w26_r21_r22_one_endpoint_acts_on_a_commits_jobs_across_its_runs() -> anyhow::Result<()> {
        within(async {
            let hub = crate::hub::tests::scratch_hub_with(|_| {}).await?;
            let repo = hub
                .store
                .upsert_repo(crate::store::Repo { id: "r-two".into(), name: "acme/two".into(), clone_url: "https://example/two".into(), branches: vec![], poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: None, mirror: None })
                .await?;
            let sha = "abcabcabcabcabcabcabcabcabcabcabcabcabca";
            // Two v1 runs of the commit: ci (a fails, b needs it) and lint (c runs on ag1, d waits).
            let mut ids: HashMap<String, String> = HashMap::new();
            for (n, (id, yaml)) in [("r1", "name: ci\njobs:\n  a: {steps: [{run: x}]}\n  b: {needs: [a], steps: [{run: y}]}\n"), ("r2", "name: lint\njobs:\n  c: {steps: [{run: z}]}\n  d: {steps: [{run: w}]}\n")].into_iter().enumerate() {
                let p = crate::pipeline::parse(yaml)?;
                let planned = crate::pipeline::plan(&p, &crate::pipeline::Context::default())?;
                let run = crate::store::Run {
                    id: id.into(),
                    repo_id: repo.id.clone(),
                    repo_name: repo.name.clone(),
                    number: i64::try_from(n)? + 1,
                    pipeline: p.name.clone(),
                    event: "push".into(),
                    branch: "main".into(),
                    sha: sha.into(),
                    message: "two runs".into(),
                    author: "ada".into(),
                    status: crate::store::Status::Queued,
                    created_at: NOW_S,
                    started_at: None,
                    finished_at: None,
                    error: None,
                    concurrency_group: None,
                };
                hub.store.create_run(run, planned).await?;
                ids.extend(hub.store.jobs(id.into(), false).await?.into_iter().map(|j| (j.key, j.id)));
            }
            let id = |k: &str| ids.get(k).cloned().unwrap_or_default();
            let (a, b, c, d) = (id("a"), id("b"), id("c"), id("d"));
            anyhow::ensure!(hub.store.claim_job(a.clone(), "ag1".into(), "baremetal".into()).await?);
            hub.store.finish_job(a.clone(), crate::store::Status::Failed, Some(1), None, None, None).await?;
            anyhow::ensure!(hub.store.claim_job(c.clone(), "ag1".into(), "baremetal".into()).await?);
            let owner = crate::authz::Caller::owner();
            let act = |body: Value| {
                let (hub, owner) = (std::sync::Arc::clone(&hub), owner.clone());
                async move { jobs_in(&hub, &owner, &body).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}")) }
            };
            let each = |v: &Value| -> HashMap<String, (bool, String)> {
                v.get("jobs").and_then(Value::as_array).into_iter().flatten().map(|j| (j.get("job").and_then(Value::as_str).unwrap_or_default().to_owned(), (j.get("done").and_then(Value::as_bool).unwrap_or(false), j.get("why").and_then(Value::as_str).unwrap_or_default().to_owned()))).collect()
            };
            let ask = |verb: &str, extra: Value| {
                let mut body = json!({ "repo": "acme/two", "sha": sha, "verb": verb });
                if let (Some(o), Some(x)) = (body.as_object_mut(), extra.as_object()) {
                    o.extend(x.clone());
                }
                body
            };

            // Paused by a word: the one waiting, d (b, skipped, has ended).
            let v = act(ask("pause", json!({ "scope": "waiting" }))).await?;
            anyhow::ensure!(each(&v) == HashMap::from([(d.clone(), (true, String::new()))]), "{v}");
            // c runs on ag1, which can't freeze it: refused, saying so; nothing was sent to stop it.
            let v = act(ask("pause", json!({ "jobs": [c] }))).await?;
            anyhow::ensure!(each(&v).get(&c).is_some_and(|(done, why)| !done && why.contains("its runner ag1 can't freeze a job in place")), "{v}");
            // What the commit says each can do.
            let seen = vec![RunnerSeen { name: "ag1".into(), connected: true, ..RunnerSeen::default() }];
            let detail = get_in(&hub.store, "acme/two", sha, None, seen, &[], NOW_S * 1000).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let job = |id: &str| detail.get("jobs").and_then(Value::as_array).and_then(|a| a.iter().find(|j| j.get("id").and_then(Value::as_str) == Some(id))).cloned().unwrap_or_default();
            let verbs = |j: &Value| j.get("verbs").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>()).unwrap_or_default();
            let (jd, jc, ja) = (job(&d), job(&c), job(&a));
            anyhow::ensure!(jd.get("paused_at").is_some() && verbs(&jd) == ["cancel", "resume"] && jd.pointer("/why_not/pause") == Some(&json!("it's paused")), "d: {jd}");
            anyhow::ensure!(verbs(&jc) == ["cancel"] && jc.pointer("/why_not/pause").and_then(Value::as_str).is_some_and(|w| w.contains("can't freeze")) && jc.get("runner") == Some(&json!("ag1")), "c: {jc}");
            anyhow::ensure!(verbs(&ja) == ["retry"] && ja.get("pipeline") == Some(&json!("ci")) && ja.get("run") == Some(&json!("r1")), "a: {ja}");
            // Resumed by name.
            let v = act(ask("resume", json!({ "jobs": [d] }))).await?;
            anyhow::ensure!(each(&v).get(&d) == Some(&(true, String::new())), "{v}");
            anyhow::ensure!(hub.store.job(d.clone()).await?.and_then(|j| j.paused_at).is_none(), "d goes on");
            // Cancelled by a word: what runs, c; d waits on.
            let v = act(ask("cancel", json!({ "scope": "running" }))).await?;
            anyhow::ensure!(each(&v) == HashMap::from([(c.clone(), (true, String::new()))]), "{v}");
            anyhow::ensure!(hub.store.job(c.clone()).await?.map(|j| j.status) == Some(crate::store::Status::Cancelled), "c stopped");
            // Run again by a word: what failed, a, with b back to wait on it.
            let v = act(ask("retry", json!({ "scope": "failed" }))).await?;
            anyhow::ensure!(each(&v).get(&a).is_some_and(|(done, _)| *done) && each(&v).len() == 1, "{v}");
            anyhow::ensure!(hub.store.job(b.clone()).await?.map(|j| j.status) == Some(crate::store::Status::Pending), "b waits on a again");
            // Not the commit's, not a verb, not a scope, nothing named, no right, no commit.
            let v = act(ask("cancel", json!({ "jobs": ["nope"] }))).await?;
            anyhow::ensure!(each(&v).get("nope") == Some(&(false, "not a job of this commit".to_owned())), "{v}");
            let code = |r: Result<Value, Fail>| r.err().map(|(c, _)| c.as_u16()).unwrap_or(200);
            anyhow::ensure!(code(jobs_in(&hub, &owner, &ask("explode", json!({ "scope": "all" }))).await) == 400);
            anyhow::ensure!(code(jobs_in(&hub, &owner, &ask("retry", json!({ "scope": "everything" }))).await) == 400);
            anyhow::ensure!(code(jobs_in(&hub, &owner, &ask("retry", json!({}))).await) == 400);
            anyhow::ensure!(code(jobs_in(&hub, &crate::authz::Caller::peer("user:ro", "viewer"), &ask("pause", json!({ "scope": "all" }))).await) == 403);
            anyhow::ensure!(code(jobs_in(&hub, &owner, &json!({ "repo": "acme/two", "sha": "f00d", "verb": "retry", "scope": "all" })).await) == 404);
            Ok(())
        })
    }

    /// W26-R29: Ship's rows are repositories, each its newest commit, newest first (`by=repo`); and
    /// each commit says when its jobs ran, the first one's start to the last one's end (open while
    /// one runs), and the machines they ran on, in the order they started.
    #[test]
    fn w26_r29_rows_are_repositories_and_a_commit_says_when_and_where_it_ran() -> anyhow::Result<()> {
        within(async {
            let hub = crate::hub::tests::scratch_hub_with(|_| {}).await?;
            let p = crate::pipeline::parse("name: ci\njobs:\n  a: {steps: [{run: x}]}\n  b: {steps: [{run: y}]}\n")?;
            let older = "1111111aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
            let other = "2222222bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
            let newer = "3333333ccccccccccccccccccccccccccccccccc";
            let mut jobs: HashMap<String, Vec<String>> = HashMap::new();
            for (n, (repo_id, repo, sha)) in [("r-one", "acme/one", older), ("r-two", "acme/two", other), ("r-one", "acme/one", newer)].into_iter().enumerate() {
                let r = hub
                    .store
                    .upsert_repo(crate::store::Repo { id: repo_id.into(), name: repo.into(), clone_url: format!("https://example/{repo}"), branches: vec![], poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: None, mirror: None })
                    .await?;
                let run = crate::store::Run {
                    id: format!("run-{n}"),
                    repo_id: r.id.clone(),
                    repo_name: r.name.clone(),
                    number: i64::try_from(n)? + 1,
                    pipeline: p.name.clone(),
                    event: "push".into(),
                    branch: "main".into(),
                    sha: sha.into(),
                    message: format!("commit {n}"),
                    author: "ada".into(),
                    status: crate::store::Status::Queued,
                    created_at: NOW_S + i64::try_from(n)?,
                    started_at: None,
                    finished_at: None,
                    error: None,
                    concurrency_group: None,
                };
                hub.store.create_run(run, crate::pipeline::plan(&p, &crate::pipeline::Context::default())?).await?;
                jobs.insert(sha.into(), hub.store.jobs(format!("run-{n}"), false).await?.into_iter().map(|j| j.id).collect());
            }
            let of = |sha: &str, i: usize| jobs.get(sha).and_then(|j| j.get(i)).cloned().unwrap_or_default();
            // The newer commit: a on ag2, then b on ag1, both ended. The other repository's: a, still running.
            for (job, agent) in [(of(newer, 0), "ag2"), (of(newer, 1), "ag1"), (of(other, 0), "ag1")] {
                anyhow::ensure!(hub.store.claim_job(job, agent.into(), "baremetal".into()).await?, "claimed");
            }
            for job in [of(newer, 0), of(newer, 1)] {
                hub.store.finish_job(job, crate::store::Status::Succeeded, Some(0), None, None, None).await?;
            }
            let heads = heads_in(&hub.store, &HashMap::from([("by".to_string(), "repo".to_string())]), None, None, Vec::new(), &[], NOW_S * 1000).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let rows = heads.get("commits").and_then(Value::as_array).cloned().unwrap_or_default();
            let shas: Vec<&str> = rows.iter().filter_map(|c| c.get("sha").and_then(Value::as_str)).collect();
            anyhow::ensure!(shas == [newer, other], "each repository's newest, newest first: {shas:?}");
            let row = |sha: &str| rows.iter().find(|c| c.get("sha").and_then(Value::as_str) == Some(sha)).cloned().unwrap_or_default();
            let (n, o) = (row(newer), row(other));
            let (from, to) = (n.pointer("/ran/from").and_then(Value::as_i64), n.pointer("/ran/to").and_then(Value::as_i64));
            anyhow::ensure!(from.is_some() && to.is_some() && from <= to, "the newer ran from its first start to its last end: {n}");
            anyhow::ensure!(n.get("machines") == Some(&json!(["ag2", "ag1"])), "its machines in the order they started: {n}");
            anyhow::ensure!(o.pointer("/ran/from").is_some_and(Value::is_i64) && o.pointer("/ran/to") == Some(&Value::Null), "a commit still running has no end yet: {o}");
            anyhow::ensure!(o.get("machines") == Some(&json!(["ag1"])), "{o}");
            // A commit none of whose jobs started says neither.
            let d = get_in(&hub.store, "acme/one", older, None, Vec::new(), &[], NOW_S * 1000).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            anyhow::ensure!(d.get("ran") == Some(&Value::Null) && d.get("machines") == Some(&json!([])), "{d}");
            Ok(())
        })
    }

    /// W26-R21, R22: a run another provider keeps (GitHub Actions here) is that provider's. Each
    /// verb, by name and by a word, is refused for its jobs saying so, and none of them changes in
    /// kiln's tables; the commit offers none of the verbs for them, saying why.
    #[test]
    fn w26_r21_r22_a_providers_jobs_are_that_providers_to_act_on() -> anyhow::Result<()> {
        within(async {
            let hub = crate::hub::tests::scratch_hub_with(|_| {}).await?;
            let repo = hub
                .store
                .upsert_repo(crate::store::Repo { id: "r-gh".into(), name: "acme/gh".into(), clone_url: "https://example/gh".into(), branches: vec![], poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: None, mirror: None })
                .await?;
            let sha = "fedfedfedfedfedfedfedfedfedfedfedfedfedf";
            let p = crate::pipeline::parse("name: build\njobs:\n  a: {steps: [{run: x}]}\n  b: {steps: [{run: y}]}\n")?;
            let planned = crate::pipeline::plan(&p, &crate::pipeline::Context::default())?;
            let run = crate::store::Run {
                id: "g1".into(),
                repo_id: repo.id.clone(),
                repo_name: repo.name.clone(),
                number: 1,
                pipeline: p.name.clone(),
                event: "push".into(),
                branch: "main".into(),
                sha: sha.into(),
                message: "on GitHub".into(),
                author: "ada".into(),
                status: crate::store::Status::Queued,
                created_at: NOW_S,
                started_at: None,
                finished_at: None,
                error: None,
                concurrency_group: None,
            };
            hub.store.create_run(run, planned).await?;
            // What kiln keeps of a GitHub Actions run (`ci::providers::record`): its provider, and its id there.
            hub.store.call(|c| Ok(c.execute("UPDATE runs SET provider = 'github-actions', external_id = '4242' WHERE id = 'g1'", [])?)).await?;
            let ids: Vec<String> = hub.store.jobs("g1".into(), false).await?.into_iter().map(|j| j.id).collect();
            anyhow::ensure!(ids.len() == 2, "{ids:?}");
            let kept = |hub: std::sync::Arc<crate::hub::Hub>, ids: Vec<String>| async move {
                let mut out = Vec::new();
                for id in ids {
                    out.push(hub.store.job(id).await?.map(|j| (j.status, j.attempt, j.paused_at)));
                }
                anyhow::Ok(out)
            };
            let before = kept(std::sync::Arc::clone(&hub), ids.clone()).await?;
            let owner = crate::authz::Caller::owner();
            let why = "GitHub Actions runs it, not kiln";
            for (verb, pick) in [("retry", json!({ "jobs": [ids.first()] })), ("cancel", json!({ "scope": "all" })), ("pause", json!({ "scope": "waiting" })), ("resume", json!({ "jobs": ids }))] {
                let mut body = json!({ "repo": "acme/gh", "sha": sha, "verb": verb });
                if let (Some(o), Some(x)) = (body.as_object_mut(), pick.as_object()) {
                    o.extend(x.clone());
                }
                let v = jobs_in(&hub, &owner, &body).await.map_err(|(c, m)| anyhow::anyhow!("{verb}: {c}: {m}"))?;
                let said = v.get("jobs").and_then(Value::as_array).cloned().unwrap_or_default();
                anyhow::ensure!(!said.is_empty() && said.iter().all(|j| j.get("done") == Some(&json!(false)) && j.get("why") == Some(&json!(why))), "{verb}: {v}");
            }
            anyhow::ensure!(kept(std::sync::Arc::clone(&hub), ids.clone()).await? == before, "nothing changed in kiln's tables");
            let detail = get_in(&hub.store, "acme/gh", sha, None, Vec::new(), &[], NOW_S * 1000).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let jobs = detail.get("jobs").and_then(Value::as_array).cloned().unwrap_or_default();
            anyhow::ensure!(jobs.len() == 2, "{detail}");
            for j in &jobs {
                anyhow::ensure!(j.get("verbs") == Some(&json!([])), "{j}");
                for verb in ["retry", "cancel", "pause", "resume"] {
                    anyhow::ensure!(j.pointer(&format!("/why_not/{verb}")) == Some(&json!(why)), "{verb}: {j}");
                }
            }
            Ok(())
        })
    }

    fn within(f: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static) -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::rt::block_on(f));
        });
        rx.recv_timeout(std::time::Duration::from_secs(120)).unwrap_or_else(|_| Err(anyhow::anyhow!("not done within two minutes")))
    }
}
