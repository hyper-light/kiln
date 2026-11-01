//! A review's verdicts and comments, signed by whoever gave them, over their revision (docs/ci.md
//! W27-R04), the pure part. Each is a [`Statement`] about the version of a diff it's given on,
//! named by its commit and its verbatim patch-id (`git patch-id --verbatim`: context and
//! whitespace kept; the `--stable` one stays for finding changes). A verdict says its action and
//! the digest of its words, a comment where it is and the digest of its words. [`read`] takes one
//! back from its statement, [`asks`] holds it to what its request says, and [`holds_on`] says where
//! a verdict holds: on its revision, and on each later one whose verbatim patch-id is the one it
//! was signed over (a rebase that only moved the change: what anyone holding the statement can
//! check), never on one that changed the code. A whitespace-only change and a rebase that changed
//! the lines around a hunk both change the verbatim patch-id, so a verdict doesn't carry there
//! either: safe, and said. A replay's `same_patch` (W26-R16) is this comparison, already made.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::chain;
use super::statement::{Statement, Subject};

/// A verdict's payload type.
pub const PAYLOAD_VERDICT: &str = "application/vnd.kiln.review.verdict+json";
/// A comment's payload type.
pub const PAYLOAD_COMMENT: &str = "application/vnd.kiln.review.comment+json";
/// A revision's verbatim patch-id, in its subject's digest set.
pub const PATCH_ID: &str = "kiln-patch-id-verbatim";
/// The actions a verdict takes.
pub const ACTIONS: [&str; 4] = ["accept", "request_changes", "comment", "resign"];

/// A diff's version, as a verdict or comment is given on it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub repo: String,
    /// The diff's name (`D41`).
    pub diff: String,
    pub version: u32,
    pub commit: String,
    /// Its verbatim patch-id.
    pub patch_id: String,
}

impl Revision {
    /// What a statement about it names: the diff's version, by its commit and verbatim patch-id.
    pub fn subject(&self) -> Subject {
        Subject { name: format!("{}/{}@v{}", self.repo, self.diff, self.version), digest: BTreeMap::from([("gitCommit".to_owned(), self.commit.clone()), (PATCH_ID.to_owned(), self.patch_id.clone())]), ..Subject::default() }
    }
}

/// Where a comment is: in the thread it answers, or on lines of a file's side, or on the diff as
/// a whole (none of them).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Place {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<u32>,
}

/// What a signed verdict or comment says: its statement's body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Body {
    /// `verdict` or `comment`.
    pub kind: String,
    /// Its giver's identity: its signer.
    pub by: String,
    #[serde(flatten)]
    pub revision: Revision,
    /// A verdict's: one of [`ACTIONS`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub action: String,
    #[serde(flatten)]
    pub place: Place,
    /// The SHA-256 of its words, as given ([`said`]).
    pub said: String,
    /// The SHA-256 of a comment's suggested lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

/// The digest of a verdict's or comment's words: SHA-256, lowercase hex, of them as given.
pub fn said(words: &str) -> String {
    chain::hex(&Sha256::digest(words.as_bytes()))
}

fn statement(body: &Body) -> Statement {
    Statement { subject: vec![body.revision.subject()], ..Statement::new(serde_json::to_value(body).unwrap_or_default()) }
}

/// The statement of `by`'s verdict on `at`: its `action`, saying `words`.
pub fn verdict(by: &str, at: &Revision, action: &str, words: &str) -> Statement {
    statement(&Body { kind: "verdict".into(), by: by.to_owned(), revision: at.clone(), action: action.to_owned(), said: said(words), ..Body::default() })
}

/// The statement of `by`'s comment at `place` on `at`, saying `words`, suggesting `suggestion`.
pub fn comment(by: &str, at: &Revision, place: &Place, words: &str, suggestion: Option<&str>) -> Statement {
    statement(&Body { kind: "comment".into(), by: by.to_owned(), revision: at.clone(), place: place.clone(), said: said(words), suggestion: suggestion.map(said), ..Body::default() })
}

fn is_digest(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// What the verdict or comment `s` says, signed as `payload_type`, or why it isn't one: its kind
/// is its payload type's, it names its giver, its revision is whole and is its one subject, and a
/// verdict's action is one there is.
pub fn read(payload_type: &str, s: &Statement) -> Result<Body, String> {
    let kind = match payload_type {
        PAYLOAD_VERDICT => "verdict",
        PAYLOAD_COMMENT => "comment",
        other => return Err(format!("{other} isn't a verdict's or a comment's payload type")),
    };
    let b: Body = serde_json::from_value(s.body.clone()).map_err(|e| format!("its body isn't a {kind}'s: {e}"))?;
    if b.kind != kind {
        return Err(format!("its body says it's a {}, its payload type a {kind}", b.kind));
    }
    let r = &b.revision;
    for (what, v) in [("its giver", &b.by), ("its repository", &r.repo), ("its diff", &r.diff), ("its commit", &r.commit), ("its patch-id", &r.patch_id)] {
        if v.trim().is_empty() {
            return Err(format!("it names no {}", what.trim_start_matches("its ")));
        }
    }
    if r.version == 0 {
        return Err("its version is 0: versions count from 1".into());
    }
    if kind == "verdict" && !ACTIONS.contains(&b.action.as_str()) {
        return Err(format!("`{}` isn't a verdict: one of {}", b.action, ACTIONS.join(", ")));
    }
    if kind == "comment" && !b.action.is_empty() {
        return Err("a comment takes no action".into());
    }
    if !is_digest(&b.said) || b.suggestion.as_deref().is_some_and(|d| !is_digest(d)) {
        return Err("its words' digest isn't a SHA-256".into());
    }
    if s.subject != [r.subject()] {
        return Err(format!("its subject isn't its revision, {}", r.subject().name));
    }
    Ok(b)
}

/// What a request asks kiln to keep, to hold a signed verdict or comment to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Asked<'a> {
    /// `verdict` or `comment`.
    pub kind: &'a str,
    pub revision: Revision,
    pub action: &'a str,
    pub place: Place,
    pub words: &'a str,
    pub suggestion: Option<&'a str>,
}

/// Whether `b`, signed by `signer`, is what `asked` asks: given by its signer, on the revision
/// asked about, saying what's asked. Else what differs.
pub fn asks(b: &Body, signer: &str, asked: &Asked<'_>) -> Result<(), String> {
    if b.by != signer {
        return Err(format!("it says it's {}'s, and {signer} signed it", b.by));
    }
    if b.kind != asked.kind {
        return Err(format!("it's a signed {}, and this is a {}", b.kind, asked.kind));
    }
    let (r, a) = (&b.revision, &asked.revision);
    if (&r.repo, &r.diff) != (&a.repo, &a.diff) {
        return Err(format!("it's signed on {} {}, and this is {} {}", r.repo, r.diff, a.repo, a.diff));
    }
    if r.version != a.version || r.commit != a.commit {
        return Err(format!("it's signed on v{} ({}), and {}'s latest is v{} ({})", r.version, short(&r.commit), a.diff, a.version, short(&a.commit)));
    }
    if r.patch_id != a.patch_id {
        return Err(format!("its patch-id isn't v{}'s verbatim one", a.version));
    }
    if b.action != asked.action {
        return Err(format!("it signs `{}`, and this asks `{}`", b.action, asked.action));
    }
    if b.place != asked.place {
        return Err("it's signed for another place on the diff".into());
    }
    if b.said != said(asked.words) || b.suggestion != asked.suggestion.map(said) {
        return Err("its words aren't the ones signed".into());
    }
    Ok(())
}

fn short(sha: &str) -> &str {
    sha.get(..10).unwrap_or(sha)
}

/// A version of a diff as kiln keeps it: its number, its commit, and its verbatim patch-id (none:
/// kiln couldn't read one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kept {
    pub n: u32,
    pub commit: String,
    pub patch_id: Option<String>,
}

/// Where a signed verdict stands on one version of its diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Holds {
    /// It was given on that very version.
    Given,
    /// Carried there from the version it was given on: the same verbatim patch-id.
    Carried { from: u32 },
    /// It doesn't hold there: why.
    Not(String),
}

impl Holds {
    pub fn holds(&self) -> bool {
        !matches!(self, Self::Not(_))
    }
}

/// Where a verdict given on `given` stands on version `n` of its diff, `versions` as kiln keeps
/// them: on its own version (its commit), and on a later one whose verbatim patch-id is the one
/// it was signed over.
pub fn holds_on(given: &Revision, versions: &[Kept], n: u32) -> Holds {
    let kept = |k: u32| versions.iter().find(|v| v.n == k);
    let Some(own) = kept(given.version) else { return Holds::Not(format!("v{} isn't one of {}'s versions", given.version, given.diff)) };
    if own.commit != given.commit {
        return Holds::Not(format!("v{} is {}, not the {} it was signed on", given.version, short(&own.commit), short(&given.commit)));
    }
    if n == given.version {
        return Holds::Given;
    }
    if n < given.version {
        return Holds::Not(format!("it was given on v{}, after v{n}", given.version));
    }
    match kept(n).map(|v| v.patch_id.as_deref()) {
        None => Holds::Not(format!("v{n} isn't one of {}'s versions", given.diff)),
        Some(Some(p)) if p == given.patch_id => Holds::Carried { from: given.version },
        Some(Some(_)) => Holds::Not(format!("v{n} changed the code: its verbatim patch-id isn't v{}'s", given.version)),
        Some(None) => Holds::Not(format!("kiln can't read v{n}'s verbatim patch-id")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d41(version: u32, commit: &str) -> Revision {
        Revision { repo: "hyper-light/kiln".into(), diff: "D41".into(), version, commit: commit.into(), patch_id: "9f3c".repeat(10) }
    }

    /// Version `n` at `commit`, its verbatim patch-id the signed one's (`same`) or another.
    fn kept(n: u32, commit: &str, same: bool) -> Kept {
        Kept { n, commit: commit.into(), patch_id: Some(if same { "9f3c".repeat(10) } else { format!("{n}e{commit}") }) }
    }

    const KIM: &str = "kiln://hyper-light/person/kim";

    /// W27-R04: a verdict's statement names its revision by commit and verbatim patch-id, and
    /// reads back as it was given; one whose subject or kind was changed doesn't.
    #[test]
    fn w27_r04_a_verdict_reads_back_from_its_statement() -> Result<(), String> {
        let at = d41(2, "c2");
        let s = verdict(KIM, &at, "accept", "looks right");
        let b = read(PAYLOAD_VERDICT, &s)?;
        assert_eq!((b.kind.as_str(), b.by.as_str(), &b.revision, b.action.as_str()), ("verdict", KIM, &at, "accept"));
        assert_eq!(b.said, said("looks right"));
        assert_eq!(s.subject.first().map(|x| (x.name.as_str(), x.digest.get(PATCH_ID).cloned())), Some(("hyper-light/kiln/D41@v2", Some(at.patch_id.clone()))));
        assert!(read(PAYLOAD_COMMENT, &s).is_err_and(|e| e.contains("payload type")));
        let mut other = s.clone();
        other.subject = vec![d41(3, "c3").subject()];
        assert!(read(PAYLOAD_VERDICT, &other).is_err_and(|e| e.contains("subject")));
        assert!(read(PAYLOAD_VERDICT, &verdict(KIM, &at, "lgtm", "")).is_err_and(|e| e.contains("isn't a verdict")));
        let c = comment(KIM, &at, &Place { file: Some("a.py".into()), side: Some("new".into()), start: Some(3), end: Some(4), ..Place::default() }, "why?", None);
        assert_eq!(read(PAYLOAD_COMMENT, &c)?.place.file.as_deref(), Some("a.py"));
        Ok(())
    }

    /// W27-R04: a signed verdict is taken only for what its request asks: its giver, its diff's
    /// latest revision, its action and its words; each difference is said.
    #[test]
    fn w27_r04_a_signed_verdict_is_held_to_what_its_request_asks() -> Result<(), String> {
        let at = d41(2, "c2");
        let b = read(PAYLOAD_VERDICT, &verdict(KIM, &at, "accept", "looks right"))?;
        let asked = Asked { kind: "verdict", revision: at.clone(), action: "accept", words: "looks right", ..Asked::default() };
        asks(&b, KIM, &asked)?;
        let refused = |a: &Asked<'_>, signer: &str| asks(&b, signer, a).err().unwrap_or_default();
        assert!(refused(&asked, "kiln://hyper-light/person/sam").contains("sam signed it"));
        assert!(refused(&Asked { action: "request_changes", ..asked.clone() }, KIM).contains("asks `request_changes`"));
        assert!(refused(&Asked { words: "looks wrong", ..asked.clone() }, KIM).contains("words"));
        assert!(refused(&Asked { revision: d41(3, "c3"), ..asked.clone() }, KIM).contains("latest is v3"));
        assert!(refused(&Asked { revision: Revision { patch_id: "e1".repeat(20), ..at.clone() }, ..asked.clone() }, KIM).contains("verbatim"));
        assert!(refused(&Asked { kind: "comment", ..asked.clone() }, KIM).contains("this is a comment"));
        Ok(())
    }

    /// W27-AC04: Kim's approval of D41 signed over v2 holds on v5, a plain rebase with v2's
    /// verbatim patch-id; not on v6, which changed the code; not on v6ws, a whitespace-only change;
    /// not on v5ctx, a rebase that changed the lines around the hunk (each changes the verbatim
    /// patch-id, though the --stable one may be the same); nor before it.
    #[test]
    fn w27_ac04_a_verdict_holds_on_its_revision_and_on_rebases_of_its_patch() {
        let at = d41(2, "c2");
        let rebased = [kept(1, "c1", false), kept(2, "c2", true), kept(3, "c3", true), kept(4, "c4", true), kept(5, "c5", true)];
        assert_eq!(holds_on(&at, &rebased, 2), Holds::Given);
        assert_eq!(holds_on(&at, &rebased, 5), Holds::Carried { from: 2 });
        let then = |last: Kept| rebased.iter().cloned().chain(std::iter::once(last)).collect::<Vec<_>>();
        for v6 in [kept(6, "c6", false), kept(6, "c6ws", false)] {
            assert!(matches!(holds_on(&at, &then(v6.clone()), 6), Holds::Not(why) if why.contains("v6 changed the code")), "{v6:?}");
        }
        let ctx = [kept(1, "c1", false), kept(2, "c2", true), kept(5, "c5ctx", false)];
        assert!(matches!(holds_on(&at, &ctx, 5), Holds::Not(why) if why.contains("v5 changed the code")), "v5ctx");
        assert!(matches!(holds_on(&at, &then(Kept { n: 6, commit: "c6".into(), patch_id: None }), 6), Holds::Not(why) if why.contains("can't read v6")));
        assert!(matches!(holds_on(&at, &rebased, 1), Holds::Not(why) if why.contains("after v1")));
        assert!(matches!(holds_on(&d41(2, "c9"), &rebased, 2), Holds::Not(why) if why.contains("not the c9")));
        assert!(!holds_on(&at, &rebased, 7).holds(), "a version kiln doesn't keep");
    }
}
