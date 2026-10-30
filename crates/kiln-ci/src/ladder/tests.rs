//! The ladder's rules (docs/ci.md W26-R17).

use super::{Author, Candidate, Conflict, Rung, State, Step, Switches, Tests, Tried, Whole, key};

const ALL: [Rung; 3] = Rung::LADDER;
const PASSED: Whole = Some(Tests::Passed);

fn next(conflicts: &[Conflict], author: Author, sw: Switches) -> super::Next {
    super::next(conflicts, author, sw, &ALL, PASSED)
}

fn conflict(id: &str, candidates: Vec<Candidate>) -> Conflict {
    Conflict { id: id.into(), place: format!("plan/expand.rs:{id}"), text: true, candidates, taken: false }
}

fn tried(rung: Rung, by: &str, tried: Tried) -> Candidate {
    Candidate { rung, by: by.into(), tried }
}

fn resolved(rung: Rung, by: &str, tests: Tests) -> Candidate {
    tried(rung, by, Tried::Resolved { tests })
}

fn kept(rung: Rung, why: &str) -> Candidate {
    tried(rung, "kiln", Tried::Kept { why: why.into() })
}

const AGENT: Author = Author::Agent { live: true };
const OFF: Switches = Switches { send_agent: false, automatic: false };

fn step_of(c: &Conflict, author: Author, sw: Switches) -> Step {
    next(std::slice::from_ref(c), author, sw).steps.into_iter().next().map(|(_, s)| s).unwrap_or(Step::Taken)
}

#[test]
fn w26_r17_rungs_go_in_order() {
    let mut c = conflict("31", vec![]);
    assert_eq!(step_of(&c, AGENT, OFF), Step::Try { rung: Rung::Structural });
    c.candidates.push(kept(Rung::Structural, "both sides added different code in the same place"));
    assert_eq!(step_of(&c, AGENT, OFF), Step::Try { rung: Rung::Remembered });
    c.candidates.push(kept(Rung::Remembered, "nothing remembered for this conflict"));
    assert_eq!(step_of(&c, AGENT, OFF), Step::Try { rung: Rung::Agent });
    c.candidates.push(tried(Rung::Agent, "agent:matrix for user:sam", Tried::Working));
    assert_eq!(step_of(&c, AGENT, OFF), Step::Waiting { rung: Rung::Agent });
    c.candidates.push(kept(Rung::Agent, "it couldn't tell which side is right"));
    // A rung at work still waits, whatever else it said.
    assert_eq!(step_of(&c, AGENT, OFF), Step::Waiting { rung: Rung::Agent });
    c.candidates.retain(|k| k.tried != Tried::Working);
    let n = next(std::slice::from_ref(&c), AGENT, OFF);
    assert_eq!(n.steps.first().map(|(_, s)| s.clone()), Some(Step::ToPerson));
    assert_eq!(n.state, State::Held);
    assert!(n.why.iter().any(|w| w.contains("structural merge kept it")), "{:?}", n.why);
}

#[test]
fn w26_r17_a_failed_rung_hands_on() {
    let c = conflict("18", vec![resolved(Rung::Structural, "kiln", Tests::Failed)]);
    assert_eq!(step_of(&c, AGENT, OFF), Step::Try { rung: Rung::Remembered });
    let n = next(std::slice::from_ref(&c), AGENT, OFF);
    assert!(n.why.iter().any(|w| w.contains("failed its tests")), "{:?}", n.why);
}

#[test]
fn w26_r17_untested_never_counts() {
    let c = conflict("18", vec![resolved(Rung::Structural, "kiln", Tests::Pending)]);
    assert_eq!(step_of(&c, AGENT, Switches { send_agent: true, automatic: true }), Step::Waiting { rung: Rung::Structural });
    assert_eq!(next(std::slice::from_ref(&c), AGENT, Switches { send_agent: true, automatic: true }).state, State::Waiting);
    // Nothing to test counts as passing: the resolution touches nothing with a test.
    let none = conflict("18", vec![resolved(Rung::Structural, "kiln", Tests::NoneToRun)]);
    assert_eq!(step_of(&none, AGENT, OFF), Step::Ready { rung: Rung::Structural, by: "kiln".into() });
}

// The goal's example: the structural merge resolves one hunk of two, the Matrix agent the other,
// 14 tests pass; with the switch off a person confirms.
#[test]
fn w26_r17_switch_off_waits_for_a_person() {
    let d42 = vec![
        conflict("18", vec![resolved(Rung::Structural, "kiln", Tests::Passed)]),
        conflict("31", vec![kept(Rung::Structural, "same place"), kept(Rung::Remembered, "nothing remembered"), resolved(Rung::Agent, "agent:matrix for user:sam", Tests::Passed)]),
    ];
    let n = next(&d42, AGENT, OFF);
    assert_eq!(n.state, State::Offered);
    assert_eq!(n.steps.get(1).map(|(_, s)| s.clone()), Some(Step::Ready { rung: Rung::Agent, by: "agent:matrix for user:sam".into() }));
    assert!(n.why.iter().any(|w| w.contains("a person confirms it")), "{:?}", n.why);
    // Every rung waits for a person while it's off, kiln's own merge too.
    let kilns = vec![conflict("18", vec![resolved(Rung::Structural, "kiln", Tests::Passed)])];
    assert_eq!(next(&kilns, Author::Person, Switches { send_agent: true, automatic: false }).state, State::Offered);
}

#[test]
fn w26_r17_switch_on_goes_when_tests_pass() {
    let on = Switches { send_agent: false, automatic: true };
    let passed = vec![conflict("31", vec![kept(Rung::Structural, "same place"), kept(Rung::Remembered, "none"), resolved(Rung::Agent, "agent:matrix for user:sam", Tests::Passed)])];
    let n = next(&passed, AGENT, on);
    assert_eq!(n.state, State::GoesOn);
    assert!(n.why.iter().any(|w| w.contains("goes on")), "{:?}", n.why);
    let failed = vec![conflict("31", vec![kept(Rung::Structural, "same place"), kept(Rung::Remembered, "none"), resolved(Rung::Agent, "agent:matrix for user:sam", Tests::Failed)])];
    assert_eq!(next(&failed, AGENT, on).state, State::Held);
    // A person's own resolution needs no one else.
    let mut taken = conflict("31", vec![]);
    taken.taken = true;
    assert_eq!(next(&[taken], AGENT, OFF).state, State::GoesOn);
}

#[test]
fn w26_r17_sent_agent_only_when_turned_on() {
    let spent = conflict("31", vec![kept(Rung::Structural, "same place"), kept(Rung::Remembered, "none")]);
    assert_eq!(step_of(&spent, Author::Person, OFF), Step::ToPerson);
    assert_eq!(step_of(&spent, Author::Agent { live: false }, OFF), Step::ToPerson);
    let send = Switches { send_agent: true, automatic: false };
    assert_eq!(step_of(&spent, Author::Person, send), Step::Try { rung: Rung::Agent });
    assert_eq!(step_of(&spent, Author::Agent { live: false }, send), Step::Try { rung: Rung::Agent });
    // A conflict that isn't text skips the structural merge and the remembered resolution.
    let binary = Conflict { text: false, ..conflict("logo.png", vec![]) };
    assert_eq!(step_of(&binary, AGENT, OFF), Step::Try { rung: Rung::Agent });
}

#[test]
fn w26_r17_remembered_key_ignores_labels_base_and_order() {
    let lines = |s: &[&str]| s.iter().map(|l| (*l).to_owned()).collect::<Vec<_>>();
    let main = lines(&["    legs.dedup_by_key(|l| l.key());"]);
    let this = lines(&["    let legs = apply(m.include, m.exclude, legs);"]);
    let k = key("rust", &main, &this);
    assert_eq!(k, key("rust", &this, &main));
    assert_eq!(k, key("rust", &lines(&["    legs.dedup_by_key(|l| l.key());  \r"]), &this));
    assert_ne!(k, key("python", &main, &this));
    assert_ne!(k, key("rust", &main, &lines(&["    let legs = apply(m.include, legs);"])));
    assert_eq!(k.len(), 64);
}

#[test]
fn w26_r17_resolutions_are_tested_together_before_theyre_offered() {
    let d42 = vec![conflict("18", vec![resolved(Rung::Structural, "kiln", Tests::Passed)]), conflict("31", vec![kept(Rung::Structural, "same place"), resolved(Rung::Remembered, "kiln", Tests::Passed)])];
    assert_eq!(super::next(&d42, AGENT, OFF, &ALL, None).state, State::TestWhole);
    assert_eq!(super::next(&d42, AGENT, OFF, &ALL, Some(Tests::Pending)).state, State::Waiting);
    let failed = super::next(&d42, AGENT, Switches { send_agent: false, automatic: true }, &ALL, Some(Tests::Failed));
    assert_eq!(failed.state, State::Held);
    assert!(failed.why.iter().any(|w| w.contains("together they failed")), "{:?}", failed.why);
    assert_eq!(super::next(&d42, AGENT, OFF, &ALL, PASSED).state, State::Offered);
}

#[test]
fn w26_r17_a_rung_the_server_doesnt_run_is_passed_over() {
    let c = conflict("31", vec![]);
    let n = super::next(std::slice::from_ref(&c), AGENT, OFF, &[Rung::Remembered], None);
    assert_eq!(n.steps.first().map(|(_, s)| s.clone()), Some(Step::Try { rung: Rung::Remembered }));
    assert!(n.why.iter().any(|w| w.contains("isn't on this kiln's ladder")), "{:?}", n.why);
}

#[test]
fn w26_r17_names_round_trip() {
    for r in [Rung::Structural, Rung::Remembered, Rung::Agent, Rung::Person] {
        assert_eq!(Rung::parse(r.as_str()), Some(r));
    }
    for t in [Tests::Pending, Tests::Passed, Tests::Failed, Tests::NoneToRun] {
        assert_eq!(Tests::parse(t.as_str()), Some(t));
    }
}
