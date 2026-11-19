//! The verdicts of kiln's Slates fleet under injected faults (docs/agentic.md W7, "The multi-runner test with injected
//! faults"; kiln goal 95101ef32ffb8b099942fe076619c310, part 60178ceb; Slates' A-580), each from what an agent or a
//! person can see. Pure, so each is tested on every host; the test (`tests/slates_fleet_faults.rs`) asks them of what
//! its runners answered after each fault.
//!
//! - **No acknowledged change lost** ([`lost`]): every file a turn wrote, and Slates acknowledged, before the fault is
//!   served after it byte for byte, by whichever node serves the volume then.
//! - **A move completes or refuses by kind** ([`judge_move`]): kiln's word on a move, once it settles, is the agent
//!   working on its target with its workspace, or a refusal naming one of the kinds a move is refused by
//!   ([`super::slates_move::KINDS`], `[kind: x]`) with the agent working where it was; never neither.
//! - **The fleet re-forms** ([`not_reformed`]): every runner `[slates_fleet]` lists is a member again, serving (its
//!   node's member id said).

use std::collections::BTreeMap;

use super::slates_fleet::Member;

/// What a move came to, as kiln said it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveOutcome {
    /// The agent works on `to`, its workspace with it.
    Moved { to: String },
    /// Refused by `kind`, one a move is refused by; the agent works where it was.
    Refused { kind: String },
}

/// What kiln said of a move once it settled: the runner it was from and to, where the agent works now, and the
/// refusal kiln gave, if any, in its words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveSaid<'a> {
    pub from: &'a str,
    pub to: &'a str,
    pub runs_on: Option<&'a str>,
    pub refused: Option<&'a str>,
}

/// Why a move's end is none W7 allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unjudged {
    /// Refused in words naming no kind.
    NoKind { said: String },
    /// Refused by a kind no move is refused by.
    UnknownKind { kind: String },
    /// Refused, yet the agent isn't working where it was.
    RefusedElsewhere { runs_on: Option<String> },
    /// Not refused, and the agent isn't working on the target.
    NeitherMovedNorRefused { runs_on: Option<String> },
}

/// The kind a refusal names (`... [kind: x]`), if it names one.
pub fn refusal_kind(_said: &str) -> Option<&str> {
    None
}

/// A move's end, judged ([`MoveSaid`]): moved, or refused by a kind a move is refused by with the agent where it was,
/// or why it's neither.
pub fn judge_move(said: &MoveSaid<'_>) -> Result<MoveOutcome, Unjudged> {
    Ok(MoveOutcome::Moved {
        to: said.to.to_owned(),
    })
}

/// The paths of `acknowledged` (each file's bytes as its turn wrote them and Slates acknowledged them) that `served`
/// doesn't hold byte for byte: an acknowledged change lost. A file served besides them is no loss.
pub fn lost(_acknowledged: &BTreeMap<String, Vec<u8>>, _served: &BTreeMap<String, Vec<u8>>) -> Vec<String> {
    Vec::new()
}

/// The runners of `listed` (by machine) that `members` doesn't hold as serving members (a member id said): the fleet
/// hasn't re-formed while any is left.
pub fn not_reformed<'a>(_listed: &[&'a str], _members: &[Member]) -> Vec<&'a str> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(machine: &str, member: Option<u64>) -> Member {
        Member { machine: machine.to_owned(), address: "127.0.0.1:17300".to_owned(), domain: 1, certificate: String::new(), member }
    }

    /// A-580. Do: judge a move kiln answered with the agent on its target; one refused by a kind with the agent on its
    /// source; one refused in words naming no kind; one refused by a kind no move is refused by; one refused with the
    /// agent on neither runner; one neither refused nor on its target. Expect: moved; refused by its kind; and each of
    /// the rest unjudged, saying why: a move ends moved or refused by kind, never anything else.
    #[test]
    fn a_move_ends_moved_or_refused_by_a_kind_and_nothing_else() {
        let said = |runs_on: Option<&'static str>, refused: Option<&'static str>| MoveSaid { from: "a", to: "b", runs_on, refused };
        assert_eq!(judge_move(&said(Some("b"), None)), Ok(MoveOutcome::Moved { to: "b".to_owned() }));
        assert_eq!(
            judge_move(&said(Some("a"), Some("b's Slates hasn't said it serves yet [kind: target_not_serving]"))),
            Ok(MoveOutcome::Refused { kind: "target_not_serving".to_owned() })
        );
        assert_eq!(
            judge_move(&said(Some("a"), Some("it couldn't move"))),
            Err(Unjudged::NoKind { said: "it couldn't move".to_owned() })
        );
        assert_eq!(
            judge_move(&said(Some("a"), Some("it couldn't move [kind: vanished]"))),
            Err(Unjudged::UnknownKind { kind: "vanished".to_owned() })
        );
        assert_eq!(
            judge_move(&said(None, Some("refused [kind: unplaced]"))),
            Err(Unjudged::RefusedElsewhere { runs_on: None })
        );
        assert_eq!(
            judge_move(&said(Some("a"), None)),
            Err(Unjudged::NeitherMovedNorRefused { runs_on: Some("a".to_owned()) })
        );
        assert_eq!(refusal_kind("words [kind: attached]"), Some("attached"));
        assert_eq!(refusal_kind("words [kind: attached"), None, "an unclosed kind names none");
    }

    /// A-580. Do: compare the files a turn wrote and Slates acknowledged with what is served after a fault: all of them
    /// byte for byte with one more file; one changed; one missing. Expect nothing lost; then the changed one; then the
    /// missing one.
    #[test]
    fn an_acknowledged_file_not_served_byte_for_byte_is_lost() {
        let files = |pairs: &[(&str, &str)]| -> BTreeMap<String, Vec<u8>> {
            pairs.iter().map(|(path, bytes)| ((*path).to_owned(), bytes.as_bytes().to_vec())).collect()
        };
        let acknowledged = files(&[("a.txt", "one"), ("b.txt", "two")]);
        assert!(lost(&acknowledged, &files(&[("a.txt", "one"), ("b.txt", "two"), ("c.txt", "three")])).is_empty());
        assert_eq!(lost(&acknowledged, &files(&[("a.txt", "one"), ("b.txt", "TWO")])), vec!["b.txt".to_owned()]);
        assert_eq!(lost(&acknowledged, &files(&[("b.txt", "two")])), vec!["a.txt".to_owned()]);
    }

    /// A-580. Do: judge the fleet's members against three listed runners: all three members with their member ids; one
    /// whose node has said no member id yet (it doesn't serve); one absent. Expect re-formed; then the one not serving;
    /// then the absent one.
    #[test]
    fn the_fleet_reforms_once_every_listed_runner_serves_as_a_member() {
        let listed = ["a", "b", "c"];
        assert!(not_reformed(&listed, &[member("a", Some(1)), member("b", Some(2)), member("c", Some(3))]).is_empty());
        assert_eq!(not_reformed(&listed, &[member("a", Some(1)), member("b", Some(2)), member("c", None)]), vec!["c"]);
        assert_eq!(not_reformed(&listed, &[member("a", Some(1)), member("c", Some(3))]), vec!["b"]);
    }
}
