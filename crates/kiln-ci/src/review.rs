//! Diffs, versions and stacks (docs/ci.md W14-R03), the pure part: what a commit message says
//! (its title, summary and `Change-Id`), a push's options, a diff's states, and what a submitted
//! stack of commits does to the diffs there are. The server reads the commits and keeps the
//! diffs.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

/// A diff's state (review.md): `draft` and `needs-review` to `landed`, or `abandoned`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Draft,
    NeedsReview,
    ChangesRequested,
    Accepted,
    Landing,
    Landed,
    Abandoned,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Draft => "draft",
            State::NeedsReview => "needs-review",
            State::ChangesRequested => "changes-requested",
            State::Accepted => "accepted",
            State::Landing => "landing",
            State::Landed => "landed",
            State::Abandoned => "abandoned",
        }
    }

    pub fn parse(s: &str) -> Option<State> {
        Some(match s {
            "draft" => State::Draft,
            "needs-review" => State::NeedsReview,
            "changes-requested" => State::ChangesRequested,
            "accepted" => State::Accepted,
            "landing" => State::Landing,
            "landed" => State::Landed,
            "abandoned" => State::Abandoned,
            _ => return None,
        })
    }

    /// Still under review: a new version may come.
    pub fn is_open(self) -> bool {
        !matches!(self, State::Landed | State::Abandoned)
    }
}

/// Whether `line` is a git trailer (`Key: value`, the key letters, digits and `-`).
fn trailer(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once(':')?;
    (!k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && v.starts_with(' ')).then(|| (k, v.trim()))
}

/// The message's paragraphs, without trailing blank lines.
fn paragraphs(message: &str) -> Vec<Vec<&str>> {
    let mut out: Vec<Vec<&str>> = vec![Vec::new()];
    for line in message.trim_end().lines() {
        if line.trim().is_empty() {
            if out.last().is_some_and(|p| !p.is_empty()) {
                out.push(Vec::new());
            }
        } else if let Some(p) = out.last_mut() {
            p.push(line);
        }
    }
    out.retain(|p| !p.is_empty());
    out
}

/// The trailers of a message: its last paragraph, when every line of it is one (and it isn't
/// the title).
pub fn trailers(message: &str) -> Vec<(String, String)> {
    let ps = paragraphs(message);
    match ps.split_last() {
        Some((last, rest)) if !rest.is_empty() && last.iter().all(|l| trailer(l).is_some()) => last.iter().filter_map(|l| trailer(l)).map(|(k, v)| (k.to_owned(), v.to_owned())).collect(),
        _ => Vec::new(),
    }
}

/// Whether `id` is a Change-Id: `I` and 40 lowercase hex digits (Gerrit's).
pub fn is_change_id(id: &str) -> bool {
    id.len() == 41 && id.starts_with('I') && id.get(1..).is_some_and(|h| h.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)))
}

/// The commit's `Change-Id` trailer (the last one, as Gerrit reads it).
pub fn change_id(message: &str) -> Option<String> {
    trailers(message).into_iter().rev().find(|(k, v)| k.eq_ignore_ascii_case("Change-Id") && is_change_id(v)).map(|(_, v)| v)
}

/// A message with `Change-Id: <id>` among its trailers (a new trailer paragraph when it has none).
pub fn with_change_id(message: &str, id: &str) -> String {
    let body = message.trim_end();
    if change_id(body).is_some() {
        return format!("{body}\n");
    }
    if trailers(body).is_empty() { format!("{body}\n\nChange-Id: {id}\n") } else { format!("{body}\nChange-Id: {id}\n") }
}

/// A commit message's title (its first line) and summary (the rest, without its trailers).
pub fn title_and_summary(message: &str) -> (String, String) {
    let ps = paragraphs(message);
    let title = ps.first().and_then(|p| p.first()).map(|l| l.trim().to_owned()).unwrap_or_default();
    let has_trailers = !trailers(message).is_empty();
    let end = if has_trailers { ps.len().saturating_sub(1) } else { ps.len() };
    let mut body: Vec<String> = Vec::new();
    for (i, p) in ps.iter().enumerate().take(end) {
        let lines: Vec<&str> = if i == 0 { p.iter().skip(1).copied().collect() } else { p.clone() };
        if !lines.is_empty() {
            body.push(lines.join("\n"));
        }
    }
    (title, body.join("\n\n"))
}

/// A push's options (`git push -o …`): `draft`, `ready`, `reviewer=ada,agent:review`
/// (repeatable), `message=…` (a note on the new versions), `topic=…`, `title=…` (a change
/// request's); unknown ones are said back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub draft: bool,
    pub ready: bool,
    pub reviewers: Vec<String>,
    pub message: Option<String>,
    pub topic: Option<String>,
    /// A change request's title.
    pub title: Option<String>,
    pub unknown: Vec<String>,
}

pub fn options<S: AsRef<str>>(given: &[S]) -> Options {
    let mut o = Options::default();
    for opt in given {
        let opt = opt.as_ref().trim();
        let (k, v) = opt.split_once('=').map_or((opt, None), |(k, v)| (k, Some(v.trim())));
        match (k.trim(), v) {
            ("draft" | "wip", None) => o.draft = true,
            ("ready", None) => o.ready = true,
            ("reviewer" | "reviewers" | "r", Some(v)) => o.reviewers.extend(v.split(',').map(str::trim).filter(|r| !r.is_empty()).map(str::to_owned)),
            ("message" | "m", Some(v)) => o.message = Some(v.to_owned()),
            ("topic", Some(v)) if !v.is_empty() => o.topic = Some(v.to_owned()),
            ("title", Some(v)) if !v.is_empty() => o.title = Some(v.to_owned()),
            _ => o.unknown.push(opt.to_owned()),
        }
    }
    o.reviewers.dedup();
    o
}

/// A commit a push submits, bottom of the stack first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub parents: Vec<String>,
    pub message: String,
    /// `git patch-id --stable`: the same patch on another base has the same one.
    pub patch_id: String,
}

/// An open or closed diff with this Change-Id, as the server keeps it: its latest version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Known {
    pub number: u32,
    pub state: State,
    pub version: u32,
    pub commit: String,
    pub parent: String,
    /// The trunk commit the version was based on.
    pub base: String,
    /// The Change-Id of the diff below it in its stack (`None`: it sat on the trunk).
    pub below: Option<String>,
    pub patch_id: String,
    pub message: String,
}

/// What a submitted commit does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// A new diff, version 1.
    New,
    /// A new version of diff `number`; `rebase_only`: the same patch and message, on a new base
    /// or in a new place; `reordered`: the diff below it isn't the one that was.
    Version { number: u32, n: u32, rebase_only: bool, reordered: bool },
    /// Diff `number`'s latest version is this commit, or the same patch and message in the same
    /// place on the same base (only the diffs below it changed): no new version.
    Unchanged { number: u32, n: u32 },
}

/// One commit of a submission: what it does, and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub sha: String,
    pub parent: String,
    pub change_id: String,
    pub title: String,
    pub summary: String,
    pub step: Step,
}

/// The most commits one push submits.
pub const MAX_STACK: usize = 100;

/// What submitting `commits` (bottom first, everything between the trunk and the pushed commit,
/// based on trunk commit `base`) does to the diffs there are (`known`, by Change-Id); refused
/// whole, saying why, when any commit can't be a diff. Only the diffs that changed get versions
/// (review.md): a diff restacked on a changed one below keeps its version; one whose patch only
/// moved to a new base, or to a new place in the stack, gets a rebase-only version.
pub fn plan(commits: &[Commit], known: &BTreeMap<String, Known>, base: &str) -> Result<Vec<Planned>, String> {
    if commits.is_empty() {
        return Err("nothing to submit: the commit is already on the trunk".into());
    }
    if commits.len() > MAX_STACK {
        return Err(format!("{} commits: a push submits at most {MAX_STACK}; land or squash some first", commits.len()));
    }
    let mut seen = HashSet::new();
    let mut out: Vec<Planned> = Vec::with_capacity(commits.len());
    for c in commits {
        let below = out.last().map(|p| p.change_id.clone());
        let short = c.sha.get(..12).unwrap_or(&c.sha);
        let [parent] = c.parents.as_slice() else {
            return Err(format!("{short} is a merge: a diff is one commit on another (a branch with merges is a change request, refs/for/<trunk>/branch/<name>)"));
        };
        let Some(id) = change_id(&c.message) else {
            return Err(format!("{short} has no Change-Id: install kiln's commit-msg hook (`kiln clone`, or `kiln hook install`) and amend, or submit with `kiln submit`, which adds them"));
        };
        if !seen.insert(id.clone()) {
            return Err(format!("{short}: two commits of this push have Change-Id {id}; squash them, or give one a new Change-Id"));
        }
        let (title, summary) = title_and_summary(&c.message);
        if title.is_empty() {
            return Err(format!("{short} has no message"));
        }
        let step = match known.get(&id) {
            None => Step::New,
            Some(k) if k.state == State::Landed => return Err(format!("{short}: Change-Id {id} is D{}'s, which landed; give this commit a new Change-Id", k.number)),
            Some(k) if k.state == State::Abandoned => return Err(format!("{short}: D{} is abandoned; reclaim it first (`kiln diff reclaim D{}`)", k.number, k.number)),
            Some(k) if k.commit == c.sha => Step::Unchanged { number: k.number, n: k.version },
            Some(k) => {
                let same = !k.patch_id.is_empty() && k.patch_id == c.patch_id && k.message.trim_end() == c.message.trim_end();
                let reordered = k.below != below;
                if same && !reordered && k.base == base {
                    Step::Unchanged { number: k.number, n: k.version }
                } else {
                    Step::Version { number: k.number, n: k.version + 1, rebase_only: same, reordered }
                }
            }
        };
        out.push(Planned { sha: c.sha.clone(), parent: parent.clone(), change_id: id, title, summary, step });
    }
    Ok(out)
}

/// A patch of a series submitted without git (W14-R03's `POST /api/repos/:repo/submit`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Patch {
    /// `Name <email>`; the submitter when absent.
    #[serde(default)]
    pub author: Option<String>,
    /// As git takes it (`GIT_AUTHOR_DATE`).
    #[serde(default)]
    pub date: Option<String>,
    pub message: String,
    /// A unified diff (`git diff` or `diff -u` with `a/` and `b/` paths).
    pub diff: String,
}

/// The patches of `git format-patch` output (one file, or many concatenated), in order.
pub fn parse_mbox(text: &str) -> Vec<Patch> {
    let mut out = Vec::new();
    let mut chunks: Vec<Vec<&str>> = Vec::new();
    for line in text.lines() {
        if line.starts_with("From ") && line.split_whitespace().nth(1).is_some_and(|w| w.len() >= 40 && w.chars().all(|c| c.is_ascii_hexdigit())) {
            chunks.push(Vec::new());
            continue;
        }
        if let Some(c) = chunks.last_mut() {
            c.push(line);
        }
    }
    for c in chunks {
        let (mut author, mut date, mut subject) = (None, None, String::new());
        let mut i = 0;
        // Headers, with continuation lines, up to the first blank line.
        let mut last: Option<&str> = None;
        while let Some(line) = c.get(i) {
            i += 1;
            if line.is_empty() {
                break;
            }
            if line.starts_with([' ', '\t']) {
                if last == Some("subject") {
                    subject.push_str(line);
                }
                continue;
            }
            let Some((k, v)) = line.split_once(':') else { continue };
            last = None;
            match k.to_ascii_lowercase().as_str() {
                "from" => author = Some(v.trim().to_owned()),
                "date" => date = Some(v.trim().to_owned()),
                "subject" => {
                    subject = v.trim().to_owned();
                    last = Some("subject");
                }
                _ => {}
            }
        }
        // `[PATCH 2/3] title` → `title`.
        let title = match subject.strip_prefix('[') {
            Some(rest) if rest.split_once(']').is_some_and(|(tag, _)| tag.to_ascii_uppercase().contains("PATCH")) => rest.split_once(']').map_or(rest, |(_, t)| t).trim().to_owned(),
            _ => subject.trim().to_owned(),
        };
        // The body to `---`, then the diff to the signature (`-- ` and git's version).
        let rest: Vec<&str> = c.get(i..).unwrap_or_default().to_vec();
        let cut = rest.iter().position(|l| *l == "---").unwrap_or(rest.len());
        let body = rest.get(..cut).unwrap_or_default().join("\n");
        let after = rest.get(cut + 1..).unwrap_or_default();
        let start = after.iter().position(|l| l.starts_with("diff --git ") || l.starts_with("--- ")).unwrap_or(after.len());
        let end = after.iter().rposition(|l| *l == "-- ").unwrap_or(after.len());
        let diff = after.get(start..end.max(start)).unwrap_or_default().join("\n");
        let message = if body.trim().is_empty() { format!("{title}\n") } else { format!("{title}\n\n{}\n", body.trim_end()) };
        out.push(Patch { author, date, message, diff: format!("{}\n", diff.trim_end()) });
    }
    out
}

/// A hunk of `git diff -U0`: `@@ -old_start,old_len +new_start,new_len @@`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
}

/// The hunks of a `git diff -U0` of one file.
pub fn hunks(diff: &str) -> Vec<Hunk> {
    let range = |r: &str| -> Option<(u32, u32)> {
        let (a, b) = r.split_once(',').unwrap_or((r, "1"));
        Some((a.parse().ok()?, b.parse().ok()?))
    };
    diff.lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("@@ -")?;
            let (old, rest) = rest.split_once(" +")?;
            let new = rest.split_once(" @@").map_or(rest, |(n, _)| n);
            let ((old_start, old_len), (new_start, new_len)) = (range(old)?, range(new)?);
            Some(Hunk { old_start, old_len, new_start, new_len })
        })
        .collect()
}

/// Where lines `start..=end` of a file are after the changes `hunks` make to it: moved by the
/// hunks before them; `None` when a hunk touches them (a comment there is outdated).
pub fn follow(hunks: &[Hunk], start: u32, end: u32) -> Option<(u32, u32)> {
    let (s, e) = (i64::from(start), i64::from(end.max(start)));
    let mut delta: i64 = 0;
    for h in hunks {
        let (a, b, d) = (i64::from(h.old_start), i64::from(h.old_len), i64::from(h.new_len));
        if b > 0 {
            if a + b - 1 < s {
                delta += d - b;
                continue;
            }
            if a > e {
                break;
            }
            return None;
        }
        // Lines inserted after old line `a`.
        if a < s {
            delta += d;
            continue;
        }
        if a >= e {
            break;
        }
        return None;
    }
    Some((u32::try_from(s + delta).ok()?, u32::try_from(e + delta).ok()?))
}

/// Open threads on a change, by kind (W26-R11).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Open {
    pub question: u32,
    pub nit: u32,
    pub issue: u32,
    pub blocking: u32,
}

/// What a change's review state is read from (W26-R02).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Facts {
    pub draft: bool,
    /// A reviewer asked for changes on the latest revision and hasn't approved since.
    pub changes_asked: bool,
    pub open: Open,
    /// Conflicts unresolved in its latest replay onto trunk.
    pub conflicts: u32,
    /// An agent is at work on its threads or conflicts.
    pub fixing: bool,
    /// The approvals trunk's policy asks for are given.
    pub policy_met: bool,
}

/// Where a change's review stands, as Ship draws it (`kiln_cd::journey::ReviewState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Standing {
    Draft,
    Requested,
    Changes,
    Blocked,
    Fixing,
    Conflict,
    Approved,
}

/// A change's review state (W26-R02): a draft is a draft; an agent at work shows as that; then a
/// conflict, a blocking thread and changes asked, each the author's to answer; approved once its gate
/// is clear (no blocking thread, no conflict, no changes asked, the policy's approvals given); else
/// waiting on its reviewers.
pub fn state(f: &Facts) -> Standing {
    if f.draft {
        Standing::Draft
    } else if f.fixing {
        Standing::Fixing
    } else if f.conflicts > 0 {
        Standing::Conflict
    } else if f.open.blocking > 0 {
        Standing::Blocked
    } else if f.changes_asked {
        Standing::Changes
    } else if f.policy_met {
        Standing::Approved
    } else {
        Standing::Requested
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "I0123456789abcdef0123456789abcdef01234567";
    const B: &str = "Ifedcba9876543210fedcba9876543210fedcba98";

    #[test]
    fn w14_r03_messages() {
        let m = format!("pipeline: job outputs\n\nOutputs flow to `needs`.\n\nSecond paragraph.\n\nSigned-off-by: Ada <a@x>\nChange-Id: {A}\n");
        assert_eq!(change_id(&m).as_deref(), Some(A));
        assert_eq!(title_and_summary(&m), ("pipeline: job outputs".to_owned(), "Outputs flow to `needs`.\n\nSecond paragraph.".to_owned()));
        // Not a trailer block: prose with a colon, or the title itself.
        assert_eq!(change_id(&format!("Change-Id: {A}")), None, "a title isn't a trailer");
        assert_eq!(change_id(&format!("t\n\nnote: Change-Id: {A} here\n")), None);
        assert_eq!(change_id("t\n\nChange-Id: Inothex\n"), None);
        // Adding one: into the trailers, or as their paragraph.
        assert_eq!(with_change_id("fix: x\n", A), format!("fix: x\n\nChange-Id: {A}\n"));
        assert_eq!(with_change_id("fix: x\n\nbody\n\nSigned-off-by: Ada <a@x>\n", A), format!("fix: x\n\nbody\n\nSigned-off-by: Ada <a@x>\nChange-Id: {A}\n"));
        assert_eq!(with_change_id(&m, B), m, "one is enough");
    }

    #[test]
    fn w14_r03_mbox() {
        let mbox = "From 1111111111111111111111111111111111111111 Mon Sep 17 00:00:00 2001\nFrom: Ada <ada@example.com>\nDate: Sun, 18 Oct 2026 12:00:00 +0000\nSubject: [PATCH 1/2] one: a\n\nWhy one.\n---\n a.txt | 1 +\n\ndiff --git a/a.txt b/a.txt\nnew file mode 100644\n--- /dev/null\n+++ b/a.txt\n@@ -0,0 +1 @@\n+one\n-- \n2.47.0\n\nFrom 2222222222222222222222222222222222222222 Mon Sep 17 00:00:00 2001\nFrom: Ada <ada@example.com>\nSubject: [PATCH 2/2] two: a\n continued\n\n---\ndiff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n-- \n2.47.0\n";
        let p = parse_mbox(mbox);
        assert_eq!(p.len(), 2);
        let (a, b) = (p.first().cloned().unwrap_or_default(), p.get(1).cloned().unwrap_or_default());
        assert_eq!((a.author.as_deref(), a.message.as_str()), (Some("Ada <ada@example.com>"), "one: a\n\nWhy one.\n"));
        assert!(a.diff.starts_with("diff --git a/a.txt b/a.txt\n") && a.diff.ends_with("+one\n"), "{:?}", a.diff);
        assert_eq!(b.message, "two: a continued\n");
        assert!(b.diff.contains("+two") && !b.diff.contains("2.47"), "{:?}", b.diff);
    }

    #[test]
    fn w14_r05_follow() {
        let h = hunks("diff --git a/f b/f\n@@ -2 +2,3 @@\n-x\n+a\n+b\n+c\n@@ -10,2 +12,0 @@\n-y\n-z\n@@ -20,0 +21 @@\n+w\n");
        assert_eq!(h, [Hunk { old_start: 2, old_len: 1, new_start: 2, new_len: 3 }, Hunk { old_start: 10, old_len: 2, new_start: 12, new_len: 0 }, Hunk { old_start: 20, old_len: 0, new_start: 21, new_len: 1 }]);
        assert_eq!(follow(&h, 1, 1), Some((1, 1)), "before every change");
        assert_eq!(follow(&h, 5, 7), Some((7, 9)), "after line 2 grew by two");
        assert_eq!(follow(&h, 2, 2), None, "its line changed");
        assert_eq!(follow(&h, 9, 11), None, "its lines were removed");
        assert_eq!(follow(&h, 15, 16), Some((15, 16)), "+2 then -2");
        assert_eq!(follow(&h, 19, 21), None, "a line inserted inside it");
        assert_eq!(follow(&h, 21, 22), Some((22, 23)), "a line inserted before it");
        assert_eq!(follow(&[], 3, 4), Some((3, 4)));
    }

    #[test]
    fn w14_r03_options() {
        let o = options(&["draft", "reviewer=ada,agent:review", "r=bob", "message=addressed comments", "topic=outputs", "nonsense=1"]);
        assert!(o.draft && !o.ready);
        assert_eq!(o.reviewers, ["ada", "agent:review", "bob"]);
        assert_eq!((o.message.as_deref(), o.topic.as_deref()), (Some("addressed comments"), Some("outputs")));
        assert_eq!(o.unknown, ["nonsense=1"]);
    }

    fn commit(sha: &str, parent: &str, id: &str, title: &str, patch: &str) -> Commit {
        Commit { sha: sha.repeat(40).get(..40).unwrap_or_default().to_owned(), parents: vec![parent.repeat(40).get(..40).unwrap_or_default().to_owned()], message: format!("{title}\n\nChange-Id: {id}\n"), patch_id: patch.into() }
    }

    #[test]
    fn w14_r03_plan() -> Result<(), String> {
        const C: &str = "I1111111111111111111111111111111111111111";
        let (d1, d2, d3) = (commit("a", "t", A, "one", "p1"), commit("b", "a", B, "two", "p2"), commit("c", "b", C, "three", "p3"));
        // W14-AC01: three commits, three new diffs, bottom first, each on the one below.
        let p = plan(&[d1.clone(), d2.clone(), d3.clone()], &BTreeMap::new(), "t")?;
        assert_eq!(p.iter().map(|x| &x.step).collect::<Vec<_>>(), [&Step::New, &Step::New, &Step::New]);
        assert_eq!((p.first().map(|x| x.title.as_str()), p.get(1).map(|x| x.parent.as_str())), (Some("one"), Some(d1.sha.as_str())));
        let known = |number, c: &Commit, below: Option<&str>, state| Known {
            number,
            state,
            version: 1,
            commit: c.sha.clone(),
            parent: c.parents.first().cloned().unwrap_or_default(),
            base: "t".into(),
            below: below.map(str::to_owned),
            patch_id: c.patch_id.clone(),
            message: c.message.clone(),
        };
        let k: BTreeMap<String, Known> = [(A.to_owned(), known(1, &d1, None, State::NeedsReview)), (B.to_owned(), known(2, &d2, Some(A), State::NeedsReview)), (C.to_owned(), known(3, &d3, Some(B), State::NeedsReview))].into();
        let steps = |p: Vec<Planned>| p.into_iter().map(|x| x.step).collect::<Vec<_>>();
        // Amending D2 adds D2 v2 only: D3, restacked on it, keeps its version.
        let d2b = Commit { sha: "d".repeat(40), patch_id: "p2-amended".into(), ..d2.clone() };
        let d3b = Commit { sha: "e".repeat(40), parents: vec![d2b.sha.clone()], ..d3.clone() };
        assert_eq!(steps(plan(&[d1.clone(), d2b, d3b], &k, "t")?), [Step::Unchanged { number: 1, n: 1 }, Step::Version { number: 2, n: 2, rebase_only: false, reordered: false }, Step::Unchanged { number: 3, n: 1 }]);
        // The trunk moved and the stack with it: rebase-only versions, each marked.
        let r1 = Commit { sha: "f".repeat(40), parents: vec!["u".repeat(40)], ..d1.clone() };
        let r2 = Commit { sha: "9".repeat(40), parents: vec![r1.sha.clone()], ..d2.clone() };
        let rebased = Step::Version { number: 1, n: 2, rebase_only: true, reordered: false };
        assert_eq!(steps(plan(&[r1.clone(), r2], &k, "u")?), [rebased, Step::Version { number: 2, n: 2, rebase_only: true, reordered: false }]);
        // Reordering is seen: D3 below D2 now.
        let o3 = Commit { sha: "8".repeat(40), parents: vec![d1.sha.clone()], ..d3.clone() };
        let o2 = Commit { sha: "7".repeat(40), parents: vec![o3.sha.clone()], ..d2.clone() };
        assert_eq!(
            steps(plan(&[d1.clone(), o3, o2], &k, "t")?),
            [Step::Unchanged { number: 1, n: 1 }, Step::Version { number: 3, n: 2, rebase_only: true, reordered: true }, Step::Version { number: 2, n: 2, rebase_only: true, reordered: true }]
        );
        // A new message isn't rebase-only.
        let m1 = Commit { sha: "6".repeat(40), message: format!("one, reworded\n\nChange-Id: {A}\n"), ..d1.clone() };
        assert_eq!(steps(plan(&[m1], &k, "t")?), [Step::Version { number: 1, n: 2, rebase_only: false, reordered: false }]);
        // Refusals: no Change-Id, a merge, the same Change-Id twice, landed and abandoned diffs.
        let bare = Commit { message: "no id\n".into(), ..d1.clone() };
        assert!(plan(&[bare], &k, "t").is_err_and(|e| e.contains("no Change-Id")));
        let merge = Commit { parents: vec!["t".repeat(40), "s".repeat(40)], ..d1.clone() };
        assert!(plan(&[merge], &k, "t").is_err_and(|e| e.contains("merge")));
        assert!(plan(&[d1.clone(), Commit { sha: "5".repeat(40), ..d1.clone() }], &k, "t").is_err_and(|e| e.contains("two commits")));
        let landed: BTreeMap<String, Known> = [(A.to_owned(), known(1, &d1, None, State::Landed))].into();
        assert!(plan(&[Commit { sha: "4".repeat(40), ..d1.clone() }], &landed, "t").is_err_and(|e| e.contains("landed")));
        let gone: BTreeMap<String, Known> = [(A.to_owned(), known(1, &d1, None, State::Abandoned))].into();
        assert!(plan(&[Commit { sha: "4".repeat(40), ..d1.clone() }], &gone, "t").is_err_and(|e| e.contains("reclaim")));
        assert!(plan(&[], &k, "t").is_err());
        Ok(())
    }

    #[test]
    fn w26_r02_a_review_stands_as_its_records_say() {
        let open = |blocking, question| Open { blocking, question, ..Open::default() };
        let approved = Facts { policy_met: true, ..Facts::default() };
        assert_eq!(state(&Facts::default()), Standing::Requested, "waiting on its reviewers");
        assert_eq!(state(&approved), Standing::Approved);
        assert_eq!(state(&Facts { open: open(0, 2), ..approved }), Standing::Approved, "questions don't hold the gate");
        assert_eq!(state(&Facts { open: open(1, 0), ..approved }), Standing::Blocked, "a blocking thread holds an approval");
        assert_eq!(state(&Facts { changes_asked: true, ..approved }), Standing::Changes);
        assert_eq!(state(&Facts { conflicts: 2, open: open(1, 0), ..approved }), Standing::Conflict, "a conflict comes before a thread");
        assert_eq!(state(&Facts { fixing: true, conflicts: 2, ..approved }), Standing::Fixing, "an agent at work shows as that");
        assert_eq!(state(&Facts { draft: true, fixing: true, ..approved }), Standing::Draft);
    }
}
