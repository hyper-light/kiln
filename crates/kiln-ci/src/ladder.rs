//! The conflict ladder (docs/ci.md W26-R17), the pure part: for one conflicted replay, what each
//! of its conflicts tries next, and where the replay as a whole stands. The server tries each rung
//! (`ci::ladder`), runs the affected tests on what it made, and asks again.
//!
//! - The rungs, in order ([`Rung`]): kiln's structural merge, a resolution remembered from the
//!   same hunk, an agent (the change's author's, or, where the repository turns it on, one kiln
//!   sends to a runner that has the repository), then the person, who takes a side or edits.
//! - A rung's resolution counts only once the affected tests passed on it ([`Tests::Passed`], or
//!   [`Tests::NoneToRun`] where nothing the resolution touches has a test): a failed one is kept
//!   and said, and the next rung is tried; one whose tests haven't finished waits.
//! - Each resolution is tested with the conflicts it doesn't resolve taking main's side; once
//!   every conflict has one, they're tested together ([`Whole`]) before any is offered.
//! - Who decides (Ada's decision on the Review design's page 4): with automatic resolution off, a
//!   repository's default, a person confirms every resolution before it lands, whichever rung made
//!   it; on, a resolution whose tests passed goes on by itself, saying which rung made it.
//! - A remembered resolution is found by its hunk's [`key`]: git rerere's way, the hunk's two
//!   sides normalised (no marker labels, no base, line endings and trailing space dropped), in a
//!   fixed order so main's and this side's swapped are the same hunk, with the file's language.
//!
//! Every answer says why, in words, for the change page and the audit.

use serde::{Deserialize, Serialize};
use sha2::Digest as _;

/// A rung of the ladder, in the order they're tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rung {
    /// kiln's structural merge (`kiln_search::merge`).
    Structural,
    /// A resolution a person accepted before, of the same hunk on any revision.
    Remembered,
    /// The change's author's agent, or an agent kiln sends where the repository turns it on.
    Agent,
    /// The person: they take a side or edit.
    Person,
}

impl Rung {
    pub const LADDER: [Rung; 3] = [Rung::Structural, Rung::Remembered, Rung::Agent];

    pub fn as_str(self) -> &'static str {
        match self {
            Rung::Structural => "structural",
            Rung::Remembered => "remembered",
            Rung::Agent => "agent",
            Rung::Person => "person",
        }
    }

    pub fn parse(s: &str) -> Option<Rung> {
        [Rung::Structural, Rung::Remembered, Rung::Agent, Rung::Person].into_iter().find(|r| r.as_str() == s)
    }

    /// Who made a resolution of this rung, in words.
    pub fn named(self) -> &'static str {
        match self {
            Rung::Structural => "kiln's structural merge",
            Rung::Remembered => "a resolution remembered from the same conflict",
            Rung::Agent => "an agent",
            Rung::Person => "a person",
        }
    }
}

/// The affected tests of a resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tests {
    /// Started, not finished.
    Pending,
    Passed,
    Failed,
    /// Nothing the resolution touches has a test.
    NoneToRun,
}

impl Tests {
    pub fn as_str(self) -> &'static str {
        match self {
            Tests::Pending => "pending",
            Tests::Passed => "passed",
            Tests::Failed => "failed",
            Tests::NoneToRun => "none_to_run",
        }
    }

    pub fn parse(s: &str) -> Option<Tests> {
        [Tests::Pending, Tests::Passed, Tests::Failed, Tests::NoneToRun].into_iter().find(|t| t.as_str() == s)
    }

    /// Whether a resolution with these tests counts.
    pub fn count(self) -> bool {
        matches!(self, Tests::Passed | Tests::NoneToRun)
    }
}

/// What a rung made of a conflict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Tried {
    /// It's at work: an agent resolving.
    Working,
    /// It made nothing, and why (the structural merge kept the markers; nothing is remembered).
    Kept { why: String },
    /// It made a resolution, and its affected tests are this far.
    Resolved { tests: Tests },
}

/// A rung's try at one conflict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub rung: Rung,
    /// Who made it: `kiln`, an agent's principal.
    pub by: String,
    pub tried: Tried,
}

/// One conflict as the ladder sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub id: String,
    /// Where it is, as the gate says it (`plan/expand.rs:31`).
    pub place: String,
    /// A text conflict (the structural merge and a remembered resolution only read those).
    pub text: bool,
    /// What each rung made of it so far.
    pub candidates: Vec<Candidate>,
    /// A person resolved it themselves (took a side or edited).
    pub taken: bool,
}

/// Who wrote the change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Author {
    Person,
    /// An agent session; `live`: it can still be reached.
    Agent { live: bool },
}

/// The repository's switches, both off by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Switches {
    /// Send an agent to a runner that has the repository where the author's can't resolve it.
    pub send_agent: bool,
    /// Let a resolution whose tests passed go on without a person.
    pub automatic: bool,
}

/// Where one conflict is on the ladder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Step {
    /// Try this rung next.
    Try { rung: Rung },
    /// A rung is at work or its tests are running.
    Waiting { rung: Rung },
    /// Resolved by this rung, its tests passed.
    Ready { rung: Rung, by: String },
    /// Resolved by a person.
    Taken,
    /// Every rung is spent: a person takes a side or edits.
    ToPerson,
}

/// Where the replay as a whole is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Some conflict has a rung to try.
    Climbing,
    /// Nothing to try, something still at work or under test.
    Waiting,
    /// Some conflict waits for a person to take a side or edit.
    Held,
    /// Every conflict has a resolution whose tests passed: they're to be tested together.
    TestWhole,
    /// Every conflict is resolved and tested, together too; a person confirms.
    Offered,
    /// Every conflict is resolved and tested, and may go on without a person.
    GoesOn,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Climbing => "climbing",
            State::Waiting => "waiting",
            State::Held => "held",
            State::TestWhole => "test_whole",
            State::Offered => "offered",
            State::GoesOn => "goes_on",
        }
    }
}

/// The resolutions together: not yet tested, or their tests this far.
pub type Whole = Option<Tests>;

/// What the ladder says of a replay: each conflict's step (by id, in order), the replay's state,
/// and why, in words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Next {
    pub steps: Vec<(String, Step)>,
    pub state: State,
    pub why: Vec<String>,
}

/// Whether a rung can try a conflict at all, and if not, why. `can`: the rungs the server runs.
fn reaches(rung: Rung, c: &Conflict, author: Author, sw: Switches, can: &[Rung]) -> Result<(), String> {
    if !can.contains(&rung) {
        return Err(format!("{}: {} isn't on this kiln's ladder", c.place, rung.named()));
    }
    match rung {
        Rung::Structural | Rung::Remembered if !c.text => Err(format!("{}: {} reads only text", c.place, rung.named())),
        Rung::Agent => match author {
            Author::Agent { live: true } => Ok(()),
            _ if sw.send_agent => Ok(()),
            Author::Agent { live: false } => Err(format!("{}: the change's agent is gone, and its trunk sends no agent (.kiln/review.yaml: fresh: conflict-agents)", c.place)),
            Author::Person => Err(format!("{}: the change is a person's, and its trunk sends no agent (.kiln/review.yaml: fresh: conflict-agents)", c.place)),
        },
        _ => Ok(()),
    }
}

/// One conflict's step: the first rung whose resolution counts; else one at work; else the next
/// rung not yet tried that reaches it; else the person.
fn step(c: &Conflict, author: Author, sw: Switches, can: &[Rung], why: &mut Vec<String>) -> Step {
    if c.taken {
        return Step::Taken;
    }
    for rung in Rung::LADDER {
        let tried: Vec<&Candidate> = c.candidates.iter().filter(|k| k.rung == rung).collect();
        if let Some(k) = tried.iter().find(|k| matches!(k.tried, Tried::Resolved { tests } if tests.count())) {
            return Step::Ready { rung, by: k.by.clone() };
        }
        if tried.iter().any(|k| matches!(k.tried, Tried::Working | Tried::Resolved { tests: Tests::Pending })) {
            return Step::Waiting { rung };
        }
        if tried.is_empty() {
            match reaches(rung, c, author, sw, can) {
                Ok(()) => return Step::Try { rung },
                Err(w) => why.push(w),
            }
            continue;
        }
        for k in tried {
            match &k.tried {
                Tried::Kept { why: w } => why.push(format!("{}: {} kept it: {w}", c.place, rung.named())),
                Tried::Resolved { tests: Tests::Failed } => why.push(format!("{}: {}'s resolution failed its tests", c.place, rung.named())),
                _ => {}
            }
        }
    }
    Step::ToPerson
}

/// What happens next for a replay whose conflicts are `conflicts`: `can`, the rungs the server
/// runs; `whole`, how the resolutions fared tested together.
pub fn next(conflicts: &[Conflict], author: Author, sw: Switches, can: &[Rung], whole: Whole) -> Next {
    let mut why = Vec::new();
    let steps: Vec<(String, Step)> = conflicts.iter().map(|c| (c.id.clone(), step(c, author, sw, can, &mut why))).collect();
    let any = |f: fn(&Step) -> bool| steps.iter().any(|(_, s)| f(s));
    let state = if any(|s| matches!(s, Step::Try { .. })) {
        State::Climbing
    } else if any(|s| matches!(s, Step::Waiting { .. })) {
        State::Waiting
    } else if any(|s| matches!(s, Step::ToPerson)) {
        why.push("a person takes a side or edits where every rung is spent".to_owned());
        State::Held
    } else if steps.is_empty() {
        State::GoesOn
    } else {
        for (id, s) in &steps {
            if let (Step::Ready { rung, by }, Some(c)) = (s, conflicts.iter().find(|c| &c.id == id)) {
                why.push(format!("{}: resolved by {} ({by}), its tests passed", c.place, rung.named()));
            }
        }
        let made = steps.iter().any(|(_, s)| matches!(s, Step::Ready { .. }));
        match whole {
            None => return Next { steps, state: State::TestWhole, why },
            Some(Tests::Pending) => return Next { steps, state: State::Waiting, why },
            Some(Tests::Failed) => {
                why.push("each resolution passed its tests, but together they failed: a person takes a side or edits".to_owned());
                return Next { steps, state: State::Held, why };
            }
            Some(Tests::Passed | Tests::NoneToRun) => {}
        }
        if made && !sw.automatic {
            why.push("a person confirms it: the repository's automatic resolution is off".to_owned());
            State::Offered
        } else {
            if made {
                why.push("it goes on: the repository turned automatic resolution on, and its tests passed".to_owned());
            }
            State::GoesOn
        }
    };
    Next { steps, state, why }
}

/// A hunk's key, for remembering its resolution: SHA-256 over its language and its two sides,
/// each line without its ending and trailing space, the sides in a fixed order (git rerere's
/// normalisation: no marker labels, no base).
pub fn key(language: &str, main: &[String], this: &[String]) -> String {
    let side = |lines: &[String]| lines.iter().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n");
    let (a, b) = (side(main), side(this));
    let (first, second) = if a <= b { (a, b) } else { (b, a) };
    let mut h = sha2::Sha256::new();
    for part in [language, "\0", first.as_str(), "\0", second.as_str()] {
        h.update(part.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests;
