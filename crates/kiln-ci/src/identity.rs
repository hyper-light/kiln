//! A change's identity (docs/ci.md W26-R03), the pure part. kiln mints each change's identity
//! (`kc-` and 16 hex digits) and the commit carries it in a trailer kiln names, `Kiln-Change:`:
//! a line of the message survives every client's amend and rebase, where plain git drops a
//! header it doesn't know. Gerrit's `Change-Id` trailer (kiln wrote them before W26) and the
//! `change-id` header jj and GitButler write (GitButler's older `gitbutler-change-id` too) are
//! aliases: they find a change, never define one, and one naming another change than the trailer
//! is refused, saying why. A commit pushed with neither is matched to the change it rewrites, as
//! `git range-diff` pairs two versions of a series: by patch-id, else by how alike their patches
//! are, among the open changes' latest versions by the same author on the same branch. A split
//! keeps the change for the half most like it and makes the other a change split from it; a
//! squash keeps one change and absorbs the rest; and kiln asks only when it can't tell.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::review;

/// The trailer that carries a change's identity.
pub const TRAILER: &str = "Kiln-Change";
/// The commit headers jj and GitButler write a change's id in, newest name first.
pub const HEADERS: [&str; 2] = ["change-id", "gitbutler-change-id"];

/// Likeness (percent) at which a commit is the change it resembles, when no other is close.
pub const MATCH: u32 = 50;
/// Likeness below which a commit is a new change; between this and [`MATCH`], kiln asks.
pub const ASK: u32 = 30;
/// How much more alike the best candidate must be than the next for kiln to tell.
pub const MARGIN: u32 = 15;
/// How much of an open change must lie inside a commit for the commit to have absorbed it.
pub const INSIDE: u32 = 80;

/// Whether `id` is a change's identity as kiln writes it: `kc-` and 6 to 32 lowercase hex digits.
pub fn is_kiln_change(id: &str) -> bool {
    id.strip_prefix("kc-").is_some_and(|h| (6..=32).contains(&h.len()) && h.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)))
}

/// A new identity from `seed` (the caller's randomness).
pub fn mint(seed: u64) -> String {
    format!("kc-{seed:016x}")
}

/// The trailer lines of every paragraph after the title made only of trailers, in order: a
/// squash's message keeps each squashed commit's paragraph.
fn trailer_lines(message: &str) -> Vec<(String, String)> {
    review::paragraphs(message)
        .iter()
        .skip(1)
        .filter(|p| p.iter().all(|l| review::trailer(l).is_some()))
        .flat_map(|p| p.iter().filter_map(|l| review::trailer(l)))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

fn trailer_values(message: &str, key: &str, valid: fn(&str) -> bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (k, v) in trailer_lines(message) {
        if k.eq_ignore_ascii_case(key) && valid(&v) && !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// The identities a message's `Kiln-Change` trailers name, in order (more than one: a squash).
pub fn kiln_changes(message: &str) -> Vec<String> {
    trailer_values(message, TRAILER, is_kiln_change)
}

/// The Gerrit `Change-Id`s a message's trailers name, in order.
pub fn change_ids(message: &str) -> Vec<String> {
    trailer_values(message, "Change-Id", review::is_change_id)
}

/// A message with `Kiln-Change: <id>` among its trailers (a new trailer paragraph when it has
/// none); unchanged when it names it already.
pub fn with_kiln_change(message: &str, id: &str) -> String {
    let body = message.trim_end();
    if kiln_changes(body).iter().any(|c| c == id) {
        return format!("{body}\n");
    }
    if review::trailers(body).is_empty() { format!("{body}\n\n{TRAILER}: {id}\n") } else { format!("{body}\n{TRAILER}: {id}\n") }
}

/// A message with each of `ids` as a `Kiln-Change` trailer.
pub fn with_kiln_changes(message: &str, ids: &[String]) -> String {
    ids.iter().fold(format!("{}\n", message.trim_end()), |m, id| with_kiln_change(&m, id))
}

/// The message without kiln's own trailer lines: what two versions' messages are compared by,
/// so a version kiln restacked (adding the trailer) is still the same message.
pub fn without_kiln_change(message: &str) -> String {
    let kept: Vec<&str> = message.lines().filter(|l| !review::trailer(l).is_some_and(|(k, _)| k.eq_ignore_ascii_case(TRAILER))).collect();
    kept.join("\n").trim_end().to_owned()
}

/// Whether `v` can be a header's change id: jj's and GitButler's 32 reverse-hex characters, or
/// GitButler's older UUIDs (16 to 64 lowercase letters, digits and `-`).
pub fn is_header_id(v: &str) -> bool {
    (16..=64).contains(&v.len()) && v.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The change id a commit's headers carry: `change-id`, else `gitbutler-change-id`.
pub fn header_change_id(headers: &[(String, String)]) -> Option<String> {
    HEADERS.iter().find_map(|name| headers.iter().find(|(k, v)| k == name && is_header_id(v.trim())).map(|(_, v)| v.trim().to_owned()))
}

/// A commit as `git log --format=raw` prints it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Raw {
    pub sha: String,
    pub parents: Vec<String>,
    /// `Name <email>`.
    pub author: String,
    /// The headers other than tree, parent, author and committer (continuation lines joined).
    pub headers: Vec<(String, String)>,
    /// The message, ending in one newline (empty when it has none).
    pub message: String,
}

/// The commits of `git log --format=raw` output, in its order.
pub fn parse_raw_log(text: &str) -> Vec<Raw> {
    fn finish(out: &mut [Raw], message: &mut Vec<String>) {
        if let Some(r) = out.last_mut() {
            let m = message.join("\n");
            let m = m.trim_end();
            r.message = if m.is_empty() { String::new() } else { format!("{m}\n") };
        }
        message.clear();
    }
    let mut out: Vec<Raw> = Vec::new();
    let mut message: Vec<String> = Vec::new();
    let mut in_message = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("commit ") {
            finish(&mut out, &mut message);
            out.push(Raw { sha: rest.split_whitespace().next().unwrap_or_default().to_owned(), ..Raw::default() });
            in_message = false;
            continue;
        }
        let Some(r) = out.last_mut() else { continue };
        if in_message {
            message.push(line.strip_prefix("    ").unwrap_or(line).to_owned());
        } else if line.is_empty() {
            in_message = true;
        } else if let Some(more) = line.strip_prefix(' ') {
            if let Some((_, v)) = r.headers.last_mut() {
                v.push('\n');
                v.push_str(more);
            }
        } else {
            let (k, v) = line.split_once(' ').unwrap_or((line, ""));
            match k {
                "tree" | "committer" => {}
                "parent" => r.parents.push(v.to_owned()),
                "author" => r.author = v.rsplit_once('>').map_or(v, |(who, _)| who).trim().to_owned() + if v.contains('>') { ">" } else { "" },
                _ => r.headers.push((k.to_owned(), v.to_owned())),
            }
        }
    }
    finish(&mut out, &mut message);
    out
}

/// What a commit's patch changes, line by line (its file, a unit separator, then the line with
/// its `+` or `-`): what two versions' patches are compared by.
pub fn patch_lines(patch: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut file = "";
    let mut in_hunk = false;
    for l in patch.lines() {
        if let Some(rest) = l.strip_prefix("diff --git ") {
            in_hunk = false;
            file = rest.rsplit_once(" b/").map_or(rest, |(_, b)| b);
        } else if l.starts_with("@@") {
            in_hunk = true;
        } else if in_hunk && (l.starts_with('+') || l.starts_with('-')) {
            out.push(format!("{file}\u{1f}{l}"));
        } else if in_hunk && !l.starts_with(' ') && !l.starts_with('\\') {
            in_hunk = false;
        }
    }
    out
}

/// Each commit's patch in `git log -p` output (its default format: `commit <sha>` lines), by sha.
pub fn split_log_patches(text: &str) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut at: Option<String> = None;
    for line in text.lines() {
        if let Some(sha) = line.strip_prefix("commit ").and_then(|r| r.split_whitespace().next()).filter(|s| s.len() >= 40 && s.chars().all(|c| c.is_ascii_hexdigit())) {
            at = Some(sha.to_owned());
            out.entry(sha.to_owned()).or_default();
        } else if let Some(p) = at.as_ref().and_then(|s| out.get_mut(s)) {
            p.push_str(line);
            p.push('\n');
        }
    }
    out
}

/// What points to a change without being its identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Alias {
    /// Gerrit's `Change-Id: I…` trailer.
    ChangeId(String),
    /// The `change-id` (or `gitbutler-change-id`) header jj and GitButler write.
    Header(String),
}

impl Alias {
    /// How kiln's records name its kind.
    pub fn kind(&self) -> &'static str {
        match self {
            Alias::ChangeId(_) => "change-id",
            Alias::Header(_) => "header",
        }
    }

    pub fn value(&self) -> &str {
        match self {
            Alias::ChangeId(v) | Alias::Header(v) => v,
        }
    }

    /// From its kind and value as kiln's records keep them.
    pub fn parse(kind: &str, value: &str) -> Option<Alias> {
        match kind {
            "change-id" => Some(Alias::ChangeId(value.to_owned())),
            "header" => Some(Alias::Header(value.to_owned())),
            _ => None,
        }
    }

    fn said(&self) -> String {
        match self {
            Alias::ChangeId(v) => format!("its Change-Id {v}"),
            Alias::Header(v) => format!("its change-id header {v}"),
        }
    }
}

/// A commit of a push, bottom of the stack first, as identity reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pushed {
    pub sha: String,
    pub message: String,
    /// The change id its headers carry ([`header_change_id`]).
    pub header: Option<String>,
    /// `git patch-id --stable`.
    pub patch_id: String,
    /// [`patch_lines`] of its patch.
    pub lines: Vec<String>,
}

impl Pushed {
    /// The aliases it carries: its Change-Ids, then its header's id.
    pub fn aliases(&self) -> Vec<Alias> {
        change_ids(&self.message).into_iter().map(Alias::ChangeId).chain(self.header.clone().map(Alias::Header)).collect()
    }

    fn title(&self) -> String {
        review::title_and_summary(&self.message).0
    }
}

/// An open change's latest version, by the pusher, on the branch pushed to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Open {
    /// Its identity.
    pub change: String,
    /// `D41`.
    pub name: String,
    pub title: String,
    pub patch_id: String,
    pub lines: Vec<String>,
}

/// A change kiln has on the branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// `D41`, `CR4`.
    pub name: String,
    pub open: bool,
}

/// What kiln's records say of the branch's changes: each by identity, and each alias's change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Known {
    pub changes: BTreeMap<String, Change>,
    pub aliases: BTreeMap<Alias, String>,
}

/// What a person said a commit is, when kiln asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// An open change, by name (`D41`).
    Change(String),
    New,
}

/// An answer: `<sha>:<D41|new>` (the push option `change=`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// The commit's sha or a prefix of it (4 hex digits at least).
    pub commit: String,
    pub choice: Choice,
}

/// The answer `given` says, if it is one.
pub fn answer(given: &str) -> Option<Answer> {
    let (sha, what) = given.trim().split_once(':')?;
    let sha = sha.trim().to_ascii_lowercase();
    if sha.len() < 4 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let what = what.trim();
    let choice = if what.eq_ignore_ascii_case("new") {
        Choice::New
    } else {
        let n = what.strip_prefix(['D', 'd'])?;
        (!n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).then(|| Choice::Change(format!("D{n}")))?
    };
    Some(Answer { commit: sha, choice })
}

/// How a commit's change was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum How {
    /// Its `Kiln-Change` trailer.
    Trailer,
    /// An alias kiln knew.
    Alias(Alias),
    /// The same patch as the change's latest version (a rebase).
    PatchId,
    /// Its patch is this alike (percent) to the change's latest version.
    Alike(u32),
    /// Someone said which.
    Answered,
    /// None: a new change.
    New,
}

/// What a pushed commit is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identified {
    pub sha: String,
    /// Its change's identity; `None`: a new change, whose identity kiln mints.
    pub change: Option<String>,
    pub how: How,
    /// The change a new one was split from.
    pub split_from: Option<String>,
    /// Open changes this commit absorbed (a squash), by identity.
    pub absorbed: Vec<String>,
    /// The aliases it carries, kept for its change.
    pub aliases: Vec<Alias>,
}

/// A commit kiln can't tell the change of: the candidates and how alike each is (percent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub sha: String,
    pub title: String,
    pub candidates: Vec<(String, u32)>,
}

/// Why a push's commits can't be identified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// A trailer and an alias (or two aliases) naming different changes.
    Disagree(String),
    /// An answer that can't be taken.
    Answer(String),
    /// Commits kiln can't tell the change of.
    Ask(Vec<Question>),
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Disagree(s) | Refused::Answer(s) => f.write_str(s),
            Refused::Ask(qs) => {
                let lines: Vec<String> = qs
                    .iter()
                    .map(|q| {
                        let s = short(&q.sha);
                        let names: Vec<String> = q.candidates.iter().map(|(n, p)| format!("{n} ({p}% alike)")).collect();
                        let first = q.candidates.first().map_or("D1", |(n, _)| n.as_str());
                        format!(
                            "kiln can't tell which change {s} (\"{}\") rewrites: {}. Say which and push again (`git push -o change={s}:{first} …`, or `change={s}:new` for a new change), or put that change's `{TRAILER}:` line back in its message",
                            q.title,
                            names.join(" or ")
                        )
                    })
                    .collect();
                f.write_str(&lines.join("\n"))
            }
        }
    }
}

/// A multiset of a commit's changed lines and its title, for likeness.
struct Bag(BTreeMap<String, u32>);

impl Bag {
    fn of(lines: &[String], title: &str) -> Bag {
        let mut m: BTreeMap<String, u32> = BTreeMap::new();
        for l in lines.iter().cloned().chain((!title.trim().is_empty()).then(|| format!("\u{1f}title\u{1f}{}", title.trim()))) {
            *m.entry(l).or_default() += 1;
        }
        Bag(m)
    }

    fn size(&self) -> u32 {
        self.0.values().sum()
    }

    fn common(&self, other: &Bag) -> u32 {
        self.0.iter().map(|(k, n)| other.0.get(k).map_or(0, |m| (*n).min(*m))).sum()
    }

    /// How alike two patches are, in percent (twice the shared lines over both sizes).
    fn alike(&self, other: &Bag) -> u32 {
        (200 * self.common(other)).checked_div(self.size() + other.size()).unwrap_or(0)
    }

    /// How much of `self` lies inside `other`, in percent.
    fn inside(&self, other: &Bag) -> u32 {
        (100 * self.common(other)).checked_div(self.size()).unwrap_or(0)
    }
}

/// What each pushed commit is (W26-R03): its trailer's change; else the change its aliases point
/// to (aliases kiln has never seen say it is new); else, among the open changes no other commit
/// claimed (`open`: the latest versions of the
/// pusher's open changes on the branch), the one with the same patch-id, else the one its patch
/// is clearly most like; else a new change. Two commits claiming one change are a split (the more
/// alike keeps it); one holding two open changes whole is a squash (it keeps the most alike and
/// absorbs the rest). Refused when a trailer and an alias disagree, or when kiln can't tell and
/// `answers` doesn't say.
pub fn identify(pushed: &[Pushed], open: &[Open], known: &Known, answers: &[Answer]) -> Result<Vec<Identified>, Refused> {
    let name = |kc: &str| known.changes.get(kc).map_or_else(|| kc.to_owned(), |c| c.name.clone());
    let is_open = |kc: &str| known.changes.get(kc).is_some_and(|c| c.open);
    let mut out: Vec<Identified> = Vec::with_capacity(pushed.len());
    let mut answered: BTreeSet<usize> = BTreeSet::new();
    // Commits whose aliases kiln has never seen: they say they are new (a fresh Change-Id, the
    // half of a `jj split` given a new change-id), so they aren't matched to an open change.
    let mut said_new: BTreeSet<usize> = BTreeSet::new();

    // What each commit says it is.
    for (i, p) in pushed.iter().enumerate() {
        let s = short(&p.sha);
        let aliases = p.aliases();
        let resolved: Vec<(&Alias, &String)> = aliases.iter().filter_map(|a| known.aliases.get(a).map(|c| (a, c))).collect();
        let trailers = kiln_changes(&p.message);
        let (claims, how): (Vec<String>, How) = if let Some(first) = trailers.first() {
            if let Some((a, c)) = resolved.iter().find(|(_, c)| !trailers.contains(c)) {
                return Err(Refused::Disagree(format!(
                    "{s}: its {TRAILER} line names {first} ({}), but {} is {}'s ({c}). The trailer is a change's identity and an alias only points to one, so they can't name two changes: take one of the lines out (`git commit --amend`) and push again",
                    name(first),
                    a.said(),
                    name(c)
                )));
            }
            (trailers.clone(), How::Trailer)
        } else if let Some((a0, _)) = resolved.first() {
            let ids: Vec<(&Alias, &String)> = resolved.iter().filter(|(a, _)| matches!(a, Alias::ChangeId(_))).copied().collect();
            let heads: Vec<(&Alias, &String)> = resolved.iter().filter(|(a, _)| matches!(a, Alias::Header(_))).copied().collect();
            if let (Some((ia, ic)), Some((ha, hc))) = (ids.first(), heads.first())
                && !ids.iter().any(|(_, c)| heads.iter().any(|(_, d)| c == d))
            {
                return Err(Refused::Disagree(format!(
                    "{s}: {} is {}'s ({ic}), but {} is {}'s ({hc}), and kiln can't take both: take the Change-Id line out (`git commit --amend`), or put the right change's `{TRAILER}:` line in, and push again",
                    ia.said(),
                    name(ic),
                    ha.said(),
                    name(hc)
                )));
            }
            let mut claims: Vec<String> = Vec::new();
            for (_, c) in &resolved {
                if !claims.contains(c) {
                    claims.push((*c).clone());
                }
            }
            ((claims), How::Alias((*a0).clone()))
        } else {
            if !aliases.is_empty() {
                said_new.insert(i);
            }
            (Vec::new(), How::New)
        };
        let mut x = Identified { sha: p.sha.clone(), change: None, how, split_from: None, absorbed: Vec::new(), aliases };
        if let Some(a) = answers.iter().find(|a| p.sha.starts_with(&a.commit)) {
            if let Some(c) = claims.first() {
                return Err(Refused::Answer(format!("{s} says which change it is ({}), so `change={}:…` has nothing to answer", name(c), a.commit)));
            }
            answered.insert(i);
            x.how = How::Answered;
            if let Choice::Change(n) = &a.choice {
                let kc = known.changes.iter().find(|(_, c)| c.open && c.name.eq_ignore_ascii_case(n)).map(|(k, _)| k.clone());
                x.change = Some(kc.ok_or_else(|| Refused::Answer(format!("`change={}:{n}`: {n} isn't an open change on this branch", a.commit)))?);
            }
        } else {
            let mut it = claims.into_iter();
            x.change = it.next();
            x.absorbed = it.filter(|c| is_open(c)).collect();
        }
        out.push(x);
    }

    // Commits claiming one change: a split. The one most like the change keeps it; the others
    // are matched as commits that say nothing are (the change split from it last time, still
    // carrying its trailer, is found again), else are new changes split from it.
    let mut split_hint: BTreeMap<usize, String> = BTreeMap::new();
    let open_by: BTreeMap<&str, &Open> = open.iter().map(|o| (o.change.as_str(), o)).collect();
    let mut by_change: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, x) in out.iter().enumerate() {
        if let Some(c) = &x.change {
            by_change.entry(c.clone()).or_default().push(i);
        }
    }
    for (kc, idx) in by_change.iter().filter(|(_, v)| v.len() > 1) {
        let keeper = match open_by.get(kc.as_str()) {
            Some(o) => {
                let ob = Bag::of(&o.lines, &o.title);
                idx.iter().copied().max_by_key(|i| (pushed.get(*i).map_or(0, |p| Bag::of(&p.lines, &p.title()).alike(&ob)), Reverse(*i)))
            }
            None => idx.first().copied(),
        };
        for i in idx.iter().copied().filter(|i| Some(*i) != keeper) {
            if let Some(x) = out.get_mut(i) {
                x.change = None;
                x.how = How::New;
                x.absorbed.clear();
                split_hint.insert(i, kc.clone());
            }
        }
    }

    // Commits that say nothing: matched among the open changes no commit claimed.
    let taken: BTreeSet<String> = out.iter().flat_map(|x| x.change.iter().chain(x.absorbed.iter()).cloned()).collect();
    let free: Vec<&Open> = open.iter().filter(|o| !taken.contains(&o.change)).collect();
    let free_bags: Vec<Bag> = free.iter().map(|o| Bag::of(&o.lines, &o.title)).collect();
    // Their changed lines alone, for what lies inside what.
    let free_lines: Vec<Bag> = free.iter().map(|o| Bag::of(&o.lines, "")).collect();
    let mut tentative: Vec<(usize, usize, u32, How)> = Vec::new();
    let mut squashed: Vec<(usize, Vec<usize>)> = Vec::new();
    let mut questions: Vec<Question> = Vec::new();
    for (i, p) in pushed.iter().enumerate() {
        let unclaimed = out.get(i).is_some_and(|x| x.change.is_none() && x.how == How::New) && !answered.contains(&i) && !said_new.contains(&i);
        if !unclaimed {
            continue;
        }
        let same: Vec<usize> = free.iter().enumerate().filter(|(_, o)| !p.patch_id.is_empty() && o.patch_id == p.patch_id).map(|(j, _)| j).collect();
        if let [j] = same.as_slice() {
            tentative.push((i, *j, 100, How::PatchId));
            continue;
        }
        let pb = Bag::of(&p.lines, &p.title());
        let pl = Bag::of(&p.lines, "");
        let alike = |j: usize| free_bags.get(j).map_or(0, |b| pb.alike(b));
        // A squash holds its changes side by side: together they fit in it (two changes alike
        // to each other, both inside it, are a question, not a squash).
        let inside: Vec<usize> = free_lines.iter().enumerate().filter(|(_, b)| b.size() > 0 && b.inside(&pl) >= INSIDE).map(|(j, _)| j).collect();
        let covered: u32 = inside.iter().filter_map(|j| free_lines.get(*j)).map(|b| b.common(&pl)).sum();
        if inside.len() >= 2
            && covered <= pl.size() + pl.size() / 10
            && let Some(best) = inside.iter().copied().max_by_key(|j| (alike(*j), Reverse(*j)))
        {
            tentative.push((i, best, alike(best), How::Alike(alike(best))));
            squashed.push((i, inside.into_iter().filter(|j| *j != best).collect()));
            continue;
        }
        let mut scored: Vec<(u32, usize)> = (0..free.len()).map(|j| (alike(j), j)).filter(|(s, _)| *s >= ASK).collect();
        scored.sort_by_key(|(s, j)| (Reverse(*s), *j));
        // The one open change its changed lines all lie inside: a part of it (a split's smaller
        // half, or the change with lines taken out), however unlike the whole.
        let holders: Vec<usize> = free_lines.iter().enumerate().filter(|(_, b)| pl.size() > 0 && pl.inside(b) >= INSIDE).map(|(j, _)| j).collect();
        match (scored.as_slice(), holders.as_slice()) {
            ([(s, j), rest @ ..], _) if *s >= MATCH && rest.first().is_none_or(|(s2, _)| s - s2 >= MARGIN) => tentative.push((i, *j, *s, How::Alike(*s))),
            (_, [j]) => tentative.push((i, *j, alike(*j), How::Alike(alike(*j)))),
            ([], _) => {}
            _ => questions.push(Question { sha: p.sha.clone(), title: p.title(), candidates: scored.iter().filter_map(|(s, j)| free.get(*j).map(|o| (o.name.clone(), *s))).collect() }),
        }
    }
    if !questions.is_empty() {
        return Err(Refused::Ask(questions));
    }
    // One commit per change: the most alike keeps it; another as alike is a half split from it.
    let mut winners: BTreeMap<usize, (usize, u32)> = BTreeMap::new();
    for (i, j, s, _) in &tentative {
        match winners.get(j) {
            Some((wi, ws)) if (*ws, Reverse(*wi)) >= (*s, Reverse(*i)) => {}
            _ => {
                winners.insert(*j, (*i, *s));
            }
        }
    }
    for (i, j, _, how) in tentative {
        let Some(o) = free.get(j) else { continue };
        let Some(x) = out.get_mut(i) else { continue };
        if winners.get(&j).is_some_and(|(wi, _)| *wi == i) {
            x.change = Some(o.change.clone());
            x.how = how;
        } else {
            x.split_from = Some(o.change.clone());
        }
    }
    for (i, js) in squashed {
        let absorbed: Vec<String> = js.into_iter().filter(|j| !winners.contains_key(j)).filter_map(|j| free.get(j).map(|o| o.change.clone())).collect();
        if let Some(x) = out.get_mut(i) {
            x.absorbed.extend(absorbed);
        }
    }
    for (i, kc) in split_hint {
        if let Some(x) = out.get_mut(i).filter(|x| x.change.is_none() && x.split_from.is_none()) {
            x.split_from = Some(kc);
        }
    }
    // A change kept by one commit is never absorbed by another.
    let kept: BTreeSet<String> = out.iter().filter_map(|x| x.change.clone()).collect();
    for x in &mut out {
        x.absorbed.retain(|c| !kept.contains(c));
        x.absorbed.dedup();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KA: &str = "kc-00000000000000aa";
    const KB: &str = "kc-00000000000000bb";
    const KC: &str = "kc-00000000000000cc";
    const IA: &str = "I1111111111111111111111111111111111111111";
    const IB: &str = "I2222222222222222222222222222222222222222";
    const JJ: &str = "mlqnqnkrxpuvuuxzlzoltostwlwyskpx";

    fn lines(file: &str, n: std::ops::Range<u32>) -> Vec<String> {
        n.map(|k| format!("{file}\u{1f}+line {k}")).collect()
    }

    fn pushed(sha: &str, message: &str, patch_id: &str, lines: Vec<String>) -> Pushed {
        Pushed { sha: sha.repeat(40).get(..40).unwrap_or_default().to_owned(), message: message.to_owned(), header: None, patch_id: patch_id.to_owned(), lines }
    }

    fn open(change: &str, name: &str, title: &str, patch_id: &str, lines: Vec<String>) -> Open {
        Open { change: change.into(), name: name.into(), title: title.into(), patch_id: patch_id.into(), lines }
    }

    /// D40–D42, open: a.rs, b.rs, c.rs, ten lines each.
    fn stack() -> (Vec<Open>, Known) {
        let o = vec![open(KA, "D40", "one", "pa", lines("a.rs", 0..10)), open(KB, "D41", "two", "pb", lines("b.rs", 0..10)), open(KC, "D42", "three", "pc", lines("c.rs", 0..10))];
        let changes = o.iter().map(|x| (x.change.clone(), Change { name: x.name.clone(), open: true })).collect();
        (o, Known { changes, aliases: BTreeMap::new() })
    }

    fn changes(r: &[Identified]) -> Vec<Option<&str>> {
        r.iter().map(|x| x.change.as_deref()).collect()
    }

    #[test]
    fn w26_r03_trailer() {
        assert!(is_kiln_change(&mint(0x7f3a9e)) && is_kiln_change("kc-7f3a9e") && !is_kiln_change("kc-7F3A9E") && !is_kiln_change("kc-12") && !is_kiln_change(IA));
        assert_eq!(mint(1), "kc-0000000000000001");
        let m = format!("fix: x\n\nbody\n\nSigned-off-by: Ada <a@x>\n{TRAILER}: {KA}\n");
        assert_eq!(kiln_changes(&m), [KA]);
        assert_eq!(kiln_changes(&format!("{TRAILER}: {KA}\n")), Vec::<String>::new(), "a title isn't a trailer");
        assert_eq!(kiln_changes(&format!("t\n\nsee {TRAILER}: {KA} here\n")), Vec::<String>::new(), "prose isn't a trailer");
        // Adding one: into the trailers, or as their paragraph; once.
        assert_eq!(with_kiln_change("fix: x\n", KA), format!("fix: x\n\n{TRAILER}: {KA}\n"));
        assert_eq!(with_kiln_change("fix: x\n\nSigned-off-by: Ada <a@x>", KA), format!("fix: x\n\nSigned-off-by: Ada <a@x>\n{TRAILER}: {KA}\n"));
        assert_eq!(with_kiln_change(&m, KA), m);
        assert_eq!(kiln_changes(&with_kiln_changes("land\n", &[KA.into(), KB.into()])), [KA, KB]);
        // A squash's message keeps both commits' paragraphs: both are read, in order.
        let squash = format!("one\n\n{TRAILER}: {KA}\nChange-Id: {IA}\n\ntwo\n\n{TRAILER}: {KB}\n");
        assert_eq!(kiln_changes(&squash), [KA, KB]);
        assert_eq!(change_ids(&squash), [IA]);
        // Messages compare without kiln's lines.
        assert_eq!(without_kiln_change(&with_kiln_change("t\n\nbody\n", KA)), "t\n\nbody");
        assert_eq!(without_kiln_change(&m), "fix: x\n\nbody\n\nSigned-off-by: Ada <a@x>");
    }

    #[test]
    fn w26_r03_headers() {
        let log = format!(
            "commit {a}\ntree t\nparent {p}\nauthor Ada Lovelace <ada@x> 1700000000 +0000\ncommitter Ada <ada@x> 1700000000 +0000\ngpgsig -----BEGIN SSH SIGNATURE-----\n abc\n -----END SSH SIGNATURE-----\nchange-id {JJ}\n\n    jj: one\n    \n    body\n\ncommit {b}\ntree t\nparent {a}\nauthor Bo <bo@x> 1700000000 +0000\ncommitter Bo <bo@x> 1700000000 +0000\ngitbutler-change-id 96420be4-a3ed-4bea-b534-ff2160cbb848\n\n    gb: two\n",
            a = "a".repeat(40),
            b = "b".repeat(40),
            p = "p".repeat(40)
        );
        let r = parse_raw_log(&log);
        assert_eq!(r.len(), 2);
        let (one, two) = (r.first().cloned().unwrap_or_default(), r.get(1).cloned().unwrap_or_default());
        assert_eq!((one.sha.as_str(), one.parents.as_slice(), one.author.as_str()), ("a".repeat(40).as_str(), ["p".repeat(40)].as_slice(), "Ada Lovelace <ada@x>"));
        assert_eq!(one.message, "jj: one\n\nbody\n");
        assert!(one.headers.iter().any(|(k, v)| k == "gpgsig" && v.contains("END SSH")), "{:?}", one.headers);
        assert_eq!(header_change_id(&one.headers).as_deref(), Some(JJ));
        assert_eq!(header_change_id(&two.headers).as_deref(), Some("96420be4-a3ed-4bea-b534-ff2160cbb848"), "GitButler's older header");
        assert_eq!(two.message, "gb: two\n");
        assert_eq!(header_change_id(&[("change-id".into(), "NOT AN ID".into())]), None);
    }

    #[test]
    fn w26_r03_patch_lines() {
        let log = format!(
            "commit {a}\nAuthor: A <a@x>\n\n    one\n\ndiff --git a/x.rs b/x.rs\nindex 1..2 100644\n--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,2 @@\n ctx\n--- removed dashes\n+added\n\\ No newline at end of file\ncommit {b}\nAuthor: A <a@x>\n\n    two\n\ndiff --git a/y.rs b/y.rs\nnew file mode 100644\n--- /dev/null\n+++ b/y.rs\n@@ -0,0 +1 @@\n+y\n",
            a = "a".repeat(40),
            b = "b".repeat(40)
        );
        let p = split_log_patches(&log);
        assert_eq!(p.len(), 2);
        assert_eq!(patch_lines(p.get(&"a".repeat(40)).map_or("", String::as_str)), ["x.rs\u{1f}--- removed dashes", "x.rs\u{1f}+added"]);
        assert_eq!(patch_lines(p.get(&"b".repeat(40)).map_or("", String::as_str)), ["y.rs\u{1f}+y"]);
    }

    /// D40–D42 rebased with plain git and no hook: the same patches; then D41 amended.
    #[test]
    fn w26_r03_amend_and_rebase() -> Result<(), Refused> {
        let (o, k) = stack();
        let rebased = [pushed("1", "one\n", "pa", lines("a.rs", 0..10)), pushed("2", "two\n", "pb", lines("b.rs", 0..10)), pushed("3", "three\n", "pc", lines("c.rs", 0..10))];
        let r = identify(&rebased, &o, &k, &[])?;
        assert_eq!(changes(&r), [Some(KA), Some(KB), Some(KC)]);
        assert!(r.iter().all(|x| x.how == How::PatchId));
        // D41 amended: two of its ten lines changed, a new patch-id.
        let mut amended = lines("b.rs", 0..8);
        amended.extend(["b.rs\u{1f}+new 1".to_owned(), "b.rs\u{1f}+new 2".to_owned()]);
        let r = identify(&[pushed("1", "one\n", "pa", lines("a.rs", 0..10)), pushed("4", "two, amended\n", "pb2", amended), pushed("5", "three\n", "pc", lines("c.rs", 0..10))], &o, &k, &[])?;
        assert_eq!(changes(&r), [Some(KA), Some(KB), Some(KC)]);
        assert!(matches!(r.get(1).map(|x| &x.how), Some(How::Alike(p)) if *p >= MATCH), "{r:?}");
        // A new commit on top, alike to nothing: a new change.
        let r = identify(&[pushed("6", "four\n", "pd", lines("d.rs", 0..5))], &o, &k, &[])?;
        assert_eq!((changes(&r), r.first().map(|x| &x.how)), (vec![None], Some(&How::New)));
        Ok(())
    }

    /// D41 split in two: with its trailer on both halves (`git commit -c`), and without one.
    #[test]
    fn w26_r03_split() -> Result<(), Refused> {
        let (o, k) = stack();
        let msg = format!("two\n\n{TRAILER}: {KB}\n");
        // Seven of its lines in one half, three in the other: the bigger half keeps D41.
        let r = identify(&[pushed("1", &msg, "x", lines("b.rs", 0..3)), pushed("2", &msg, "y", lines("b.rs", 3..10))], &o, &k, &[])?;
        assert_eq!(changes(&r), [None, Some(KB)]);
        assert_eq!(r.first().and_then(|x| x.split_from.as_deref()), Some(KB), "the other half says it was split from D41");
        // Pushed again (D41 now the bigger half, D43 the smaller): each finds its own, though
        // both still carry D41's trailer.
        const KD: &str = "kc-00000000000000dd";
        let (mut o2, mut k2) = (o.clone(), k.clone());
        if let Some(d41) = o2.iter_mut().find(|x| x.change == KB) {
            d41.lines = lines("b.rs", 3..10);
            d41.patch_id = "y".into();
        }
        o2.push(open(KD, "D43", "two", "x", lines("b.rs", 0..3)));
        k2.changes.insert(KD.into(), Change { name: "D43".into(), open: true });
        let r = identify(&[pushed("1", &msg, "x", lines("b.rs", 0..3)), pushed("2", &msg, "y", lines("b.rs", 3..10))], &o2, &k2, &[])?;
        assert_eq!((changes(&r), r.first().and_then(|x| x.split_from.clone())), (vec![Some(KD), Some(KB)], None));
        // Without the trailer: both halves are like D41; the more alike keeps it.
        let r = identify(&[pushed("1", "two\n", "x", lines("b.rs", 0..6)), pushed("2", "two, part 2\n", "y", lines("b.rs", 6..10))], &o, &k, &[])?;
        assert_eq!(changes(&r), [Some(KB), None]);
        assert_eq!(r.get(1).and_then(|x| x.split_from.as_deref()), Some(KB));
        // A small half with a title of its own is unlike the whole, but lies inside it: still a split.
        let r = identify(&[pushed("1", "two\n", "x", lines("b.rs", 0..8)), pushed("2", "tail of two\n", "y", lines("b.rs", 8..10))], &o, &k, &[])?;
        assert_eq!((changes(&r), r.get(1).and_then(|x| x.split_from.clone())), (vec![Some(KB), None], Some(KB.to_owned())));
        Ok(())
    }

    /// D41 and D42 squashed into one commit: with both trailers (`git rebase -i` squash), and
    /// without (a fixup, or no trailers at all).
    #[test]
    fn w26_r03_squash() -> Result<(), Refused> {
        let (o, k) = stack();
        let mut both = lines("b.rs", 0..10);
        both.extend(lines("c.rs", 0..10));
        let msg = format!("two\n\n{TRAILER}: {KB}\n\nthree\n\n{TRAILER}: {KC}\n");
        let r = identify(&[pushed("1", "one\n", "pa", lines("a.rs", 0..10)), pushed("2", &msg, "z", both.clone())], &o, &k, &[])?;
        assert_eq!(changes(&r), [Some(KA), Some(KB)]);
        assert_eq!(r.get(1).map(|x| x.absorbed.clone()), Some(vec![KC.to_owned()]));
        let r = identify(&[pushed("1", "one\n", "pa", lines("a.rs", 0..10)), pushed("2", "two\n", "z", both)], &o, &k, &[])?;
        assert_eq!(changes(&r), [Some(KA), Some(KB)], "the title tips it to D41");
        assert_eq!(r.get(1).map(|x| x.absorbed.clone()), Some(vec![KC.to_owned()]));
        Ok(())
    }

    /// Change-Ids (kiln's history) and jj's and GitButler's headers find their changes; an
    /// unknown alias is a new change that keeps it.
    #[test]
    fn w26_r03_aliases() -> Result<(), Refused> {
        let (o, mut k) = stack();
        k.aliases.insert(Alias::ChangeId(IA.into()), KA.into());
        k.aliases.insert(Alias::Header(JJ.into()), KB.into());
        let jj = Pushed { header: Some(JJ.into()), ..pushed("2", "two, rewritten\n", "q", lines("z.rs", 0..3)) };
        let r = identify(&[pushed("1", &format!("one, rewritten\n\nChange-Id: {IA}\n"), "p", lines("y.rs", 0..3)), jj], &o, &k, &[])?;
        assert_eq!(changes(&r), [Some(KA), Some(KB)]);
        assert_eq!(r.first().map(|x| &x.how), Some(&How::Alias(Alias::ChangeId(IA.into()))));
        assert_eq!(r.get(1).map(|x| &x.how), Some(&How::Alias(Alias::Header(JJ.into()))));
        // A header kiln hasn't seen: a new change, with the alias kept for it.
        let fresh = Pushed { header: Some("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".into()), ..pushed("3", "new\n", "n", lines("n.rs", 0..3)) };
        let r = identify(&[fresh], &o, &k, &[])?;
        assert_eq!(r.first().map(|x| (x.change.clone(), x.aliases.clone())), Some((None, vec![Alias::Header("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".into())])));
        // A fresh Change-Id says the commit is new, however like an open change it is.
        let r = identify(&[pushed("5", &format!("three\n\nChange-Id: {IB}\n"), "other", lines("c.rs", 0..10))], &o, &k, &[])?;
        assert_eq!((changes(&r), r.first().map(|x| &x.how)), (vec![None], Some(&How::New)));
        // The trailer and an alias agreeing: the trailer.
        let r = identify(&[pushed("4", &format!("one\n\n{TRAILER}: {KA}\nChange-Id: {IA}\n"), "p", vec![])], &o, &k, &[])?;
        assert_eq!((changes(&r), r.first().map(|x| &x.how)), (vec![Some(KA)], Some(&How::Trailer)));
        Ok(())
    }

    fn unexpected(e: Result<Vec<Identified>, Refused>) -> Refused {
        Refused::Answer(format!("unexpected: {e:?}"))
    }

    #[test]
    fn w26_r03_disagreement() -> Result<(), Refused> {
        let (o, mut k) = stack();
        k.aliases.insert(Alias::ChangeId(IB.into()), KB.into());
        k.aliases.insert(Alias::Header(JJ.into()), KC.into());
        let e = identify(&[pushed("1", &format!("one\n\n{TRAILER}: {KA}\nChange-Id: {IB}\n"), "p", vec![])], &o, &k, &[]);
        let Err(Refused::Disagree(why)) = e else { return Err(unexpected(e)) };
        assert!(why.contains("1111111") && why.contains(KA) && why.contains("D40") && why.contains(IB) && why.contains("D41") && why.contains("can't name two changes"), "{why}");
        let both = Pushed { header: Some(JJ.into()), ..pushed("2", &format!("two\n\nChange-Id: {IB}\n"), "p", vec![]) };
        let e = identify(&[both], &o, &k, &[]);
        let Err(Refused::Disagree(why)) = e else { return Err(unexpected(e)) };
        assert!(why.contains("D41") && why.contains("D42") && why.contains(JJ), "{why}");
        Ok(())
    }

    /// Two open changes about as alike to a commit: kiln asks, and an answer settles it.
    #[test]
    fn w26_r03_asks() -> Result<(), Refused> {
        let (mut o, k) = stack();
        if let Some(c) = o.get_mut(2) {
            c.lines = lines("b.rs", 0..10);
            c.title = "two".into();
        }
        let p = [pushed("9", "two\n", "new", lines("b.rs", 0..9))];
        let e = identify(&p, &o, &k, &[]);
        let Err(Refused::Ask(qs)) = e else { return Err(unexpected(e)) };
        let q = qs.first().cloned().unwrap_or(Question { sha: String::new(), title: String::new(), candidates: vec![] });
        assert_eq!(q.candidates.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["D41", "D42"]);
        let said = Refused::Ask(qs).to_string();
        assert!(said.contains("can't tell which change 9999999") && said.contains("change=9999999:D41") && said.contains("change=9999999:new"), "{said}");
        let r = identify(&p, &o, &k, &[answer("9999999:d42").ok_or(Refused::Answer("parse".into()))?])?;
        assert_eq!((changes(&r), r.first().map(|x| &x.how)), (vec![Some(KC)], Some(&How::Answered)));
        let r = identify(&p, &o, &k, &[answer("99999:new").ok_or(Refused::Answer("parse".into()))?])?;
        assert_eq!(changes(&r), [None]);
        // A middling likeness asks too.
        let mut half = lines("a.rs", 0..4);
        half.extend(lines("q.rs", 0..8));
        assert!(matches!(identify(&[pushed("8", "other\n", "h", half)], &o, &k, &[]), Err(Refused::Ask(_))));
        // Answers that can't be taken.
        assert!(matches!(identify(&p, &o, &k, &[Answer { commit: "9999".into(), choice: Choice::Change("D7".into()) }]), Err(Refused::Answer(w)) if w.contains("D7 isn't an open change")));
        let claimed = [pushed("7", &format!("t\n\n{TRAILER}: {KA}\n"), "x", vec![])];
        assert!(matches!(identify(&claimed, &o, &k, &[Answer { commit: "7777".into(), choice: Choice::New }]), Err(Refused::Answer(_))));
        assert_eq!(answer("abc"), None);
        assert_eq!(answer("abcd:D"), None);
        assert_eq!(answer("abcd:x41"), None);
        Ok(())
    }

    /// The same inputs, the same answer.
    #[test]
    fn w26_r03_deterministic() -> Result<(), Refused> {
        let (o, k) = stack();
        let p = [pushed("1", "two\n", "x", lines("b.rs", 0..6)), pushed("2", "two\n", "y", lines("b.rs", 4..10))];
        assert_eq!(identify(&p, &o, &k, &[])?, identify(&p, &o, &k, &[])?);
        Ok(())
    }
}
