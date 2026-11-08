//! The hand-off's pieces, one at a time (LB03, W05-R25): ranks, ballots, a member's promises, a
//! claim's decision, an owner's members, and the words on the wire.

use super::*;
use crate::place::ask::Ask;
use crate::place::msg::{ToOwner, ToRunner};
use crate::sched::state::Fence;

fn h(run: &str) -> Handle {
    Handle::new("mac", 3, run)
}

fn waiting(run: &str) -> Held {
    Held::Waiting { ask: Ask { run: run.into(), ..Ask::default() } }
}

/// Rendezvous hashing (Thaler and Ravishankar, 1998): every node ranks alike, the order is total,
/// and a node gone moves only the runs it was first for.
#[test]
fn lb03_rendezvous_ranks_alike_everywhere_and_a_gone_node_moves_only_its_runs() {
    let nodes = ["a", "b", "c", "d"];
    let mut moved = 0;
    for i in 0..400 {
        let run = h(&format!("r{i}"));
        let all = ranked(&run, nodes);
        assert_eq!(all, ranked(&run, nodes.iter().rev().copied()), "the order is the nodes', not their listing's");
        assert_eq!(all.len(), 4);
        let without_c = ranked(&run, nodes.into_iter().filter(|n| *n != "c"));
        if all.first() == Some(&"c") {
            moved += 1;
            assert_eq!(without_c.first(), all.get(1), "the next in line takes it");
        } else {
            assert_eq!(without_c.first(), all.first(), "{run:?} isn't c's: it stays");
        }
    }
    // About a quarter are c's; none of the others moved.
    assert!((60..140).contains(&moved), "{moved}");
    // The weight is a fixed function: the same on every platform and build.
    assert_eq!(rank(&h("r1"), "a"), rank(&Handle::new("mac", 3, "r1"), "a"));
    assert_ne!(rank(&h("r1"), "a"), rank(&h("r1"), "b"));
}

/// Ballots: the first owner's is below every claim, a round outranks any rank, and an adopted id
/// names its run's first handle.
#[test]
fn lb03_ballots_order_round_then_rank_and_an_adopted_id_names_its_handle() {
    let run = h("r1");
    let a = Ballot::of(1, &run, "a");
    let b = Ballot::of(1, &run, "b");
    assert!(Ballot::FIRST < a && Ballot::FIRST < b);
    assert_eq!(a < b, a.rank < b.rank);
    let a2 = Ballot::above(Some(&a.clone().max(b.clone())), &run, "a");
    assert!(a2 > a && a2 > b && a2.round == 2);
    assert_eq!(Ballot::above(None, &run, "a"), a);
    let id = run.adopted_id();
    assert_eq!(Handle::of_adopted(&id), Some(run.clone()));
    assert_eq!(Handle::of("linux", 9, &id), run, "an adopted run moved again keeps its first handle");
    assert_eq!(Handle::of("mac", 3, "r1"), run);
    assert_eq!(Handle::of_adopted("r1"), None);
}

/// A member promises only above what it promised, refuses the first owner's word once it did,
/// takes the owner's word that it owns the run only under the ballot promised, and keeps its
/// promises across its process starting again (Paxos' acceptors keep theirs).
#[test]
fn lb03_a_member_promises_only_higher_refuses_the_first_owner_after_and_keeps_its_promise() -> Result<(), String> {
    let run = h("r1");
    let mut m = Acceptor::default();
    assert_eq!(m.gate(&run, &Ballot::FIRST), Gate::Pass);
    m.probed(&run, ["mac".to_owned(), "linux".to_owned()]);
    assert_eq!(m.claim(&run, &Ballot::FIRST, None, &[]), Err(Ballot::FIRST), "the first owner's ballot claims nothing");
    let (a, b) = (Ballot::of(1, &run, "a"), Ballot::of(1, &run, "b"));
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    let p = m.claim(&run, &hi, Some(waiting("r1")), &["studio".to_owned()]).map_err(|e| format!("{e:?}"))?;
    assert_eq!(p.members, vec!["linux".to_owned(), "mac".to_owned(), "studio".to_owned()], "what it knows and what the claim said");
    assert_eq!(m.claim(&run, &lo, None, &[]), Err(hi.clone()));
    assert_eq!(m.claim(&run, &hi, None, &[]).map(|p| p.ballot), Ok(hi.clone()), "its claimant asking again is answered again");
    assert_eq!(m.gate(&run, &Ballot::FIRST), Gate::Refuse(hi.clone()), "the first owner's later word is refused");
    assert_eq!(m.gate(&run, &hi), Gate::Refuse(hi.clone()), "nor its claimant's, until it owns it");
    assert_eq!(m.extend(&run, &Ballot::FIRST, &["fedora".to_owned()]), Err(hi.clone()), "its members don't grow past a promise");
    assert!(!m.own(&run, &lo) && m.own(&run, &hi));
    assert_eq!(m.gate(&run, &hi), Gate::Pass);
    assert_eq!(m.gate(&run, &lo), Gate::Refuse(hi.clone()), "by ballot: a lower one is refused, whoever sends it");
    assert_eq!(m.owned_by(&hi.by), vec![run.clone()]);
    let kept: Vec<(Handle, Row)> = m.changes().into_iter().filter_map(|(k, r)| r.map(|r| (k, r))).collect();
    assert_eq!(kept.len(), 1);
    let again = Acceptor::restore(kept);
    assert_eq!(again.gate(&run, &Ballot::FIRST), Gate::Refuse(hi.clone()), "kept across a restart");
    assert_eq!(again.row(&run).and_then(|r| r.held.clone()), Some(waiting("r1")));
    Ok(())
}

/// A claim waits for every member it knows, learns members from each promise and claims there
/// too, yields to a higher ballot (and claims again a round up once that one is gone), and
/// decides by what the members hold: follow it where it started, close it out where it never did.
#[test]
fn lb03_a_claim_decides_only_once_every_member_promised() {
    let run = h("r1");
    let me = Ballot::of(1, &run, "a");
    let mut c = Claim::new(run.clone(), me.clone(), ["mac".to_owned(), "linux".to_owned()], None);
    assert_eq!(c.decide(), Decision::Waiting(vec!["linux".to_owned(), "mac".to_owned()]));
    assert_eq!(c.promised("linux", &me, Some(waiting("r1")), &["linux".to_owned(), "studio".to_owned()]), vec!["studio".to_owned()]);
    assert_eq!(c.promised("mac", &Ballot::of(2, &run, "a"), None, &[]), Vec::<String>::new(), "another ballot's promise is let go");
    assert_eq!(c.decide(), Decision::Waiting(vec!["mac".to_owned(), "studio".to_owned()]));
    c.promised("mac", &me, None, &[]);
    c.promised("studio", &me, Some(Held::Running { fence: Fence { epoch: 4, n: 1 }, ask: None }), &[]);
    assert_eq!(c.decide(), Decision::Runs { at: "studio".into(), fence: Fence { epoch: 4, n: 1 }, ended: None, ask: None });
    let mut w = Claim::new(run.clone(), me.clone(), ["linux".to_owned()], None);
    w.promised("linux", &me, Some(waiting("r1")), &[]);
    assert!(matches!(w.decide(), Decision::NotStarted { ask: Some(a) } if a.run == "r1"));
    let mut g = Claim::new(run.clone(), me.clone(), ["linux".to_owned()], None);
    g.promised("linux", &me, None, &[]);
    assert_eq!(g.decide(), Decision::Gone);
    let higher = Ballot::of(3, &run, "b");
    g.refused(higher.clone());
    assert_eq!(g.decide(), Decision::Yield(higher.clone()));
    let next = g.again().map(|c| c.ballot);
    assert!(next.as_ref().is_some_and(|b| *b > higher && b.by == "a"), "{next:?}");
}

/// An owner's members: its first probes make them; a runner past them waits until every member
/// that may hold the run took it; a member that can't hold it isn't waited for; with none that may,
/// the run goes anywhere at once. A probe says the members, those being added among them.
#[test]
fn lb03_an_owner_adds_runners_to_a_run_only_with_every_holding_members_word() {
    let mut m = Members::default();
    assert_eq!(m.write("r1", "linux", true), Verdict::Pass(vec!["linux".into()]));
    assert_eq!(m.write("r1", "studio", true), Verdict::Pass(vec!["linux".into(), "studio".into()]));
    assert_eq!(m.write("r1", "linux", false), Verdict::Pass(vec!["linux".into(), "studio".into()]), "a member again: as it is");
    assert_eq!(m.write("r1", "fedora", false), Verdict::Ask { members: vec!["linux".into(), "studio".into()], add: "fedora".into() });
    assert_eq!(m.write("r1", "fedora", false), Verdict::Wait);
    assert_eq!(m.members("r1"), vec!["fedora".to_owned(), "linux".into(), "studio".into()], "one being added is said with probes");
    assert_eq!(m.extended("r1", "linux", "fedora"), Released { to: Vec::new(), members: m.members("r1") });
    assert_eq!(m.asking("r1"), vec![("studio".to_owned(), "fedora".to_owned())]);
    // studio can't hold it after all: it isn't waited for.
    let r = m.heard("r1", "studio", false);
    assert_eq!(r.to, vec!["fedora".to_owned()]);
    // None may hold it any more: anywhere, at once.
    m.heard("r1", "linux", false);
    m.heard("r1", "fedora", false);
    assert!(matches!(m.write("r1", "mini", false), Verdict::Pass(_)));
}

/// The words are JSON tagged by op, none of them one of placement's own: a node from before reads
/// them as words it doesn't know, and lets them go.
#[test]
fn lb03_handoff_words_are_ops_of_their_own() -> Result<(), serde_json::Error> {
    let run = h("r1");
    let words = [
        Word::Orphan { handle: run.clone(), members: vec!["linux".into()], held: waiting("r1"), ballot: None, why: "mac".into() },
        Word::Adopting { handle: run.clone(), ballot: Some(Ballot::of(1, &run, "a")), why: None },
        Word::Claim { handle: run.clone(), ballot: Ballot::of(1, &run, "a"), members: vec!["linux".into()] },
        Word::Promise { handle: run.clone(), ballot: Ballot::of(1, &run, "a"), held: Some(Held::Ended { fence: Fence { epoch: 1, n: 2 }, ok: true, ask: None }), members: Vec::new() },
        Word::Refused { handle: run.clone(), ballot: Ballot::of(1, &run, "a") },
        Word::Own { handle: run.clone(), ballot: Ballot::of(1, &run, "a") },
        Word::Extend { handle: run.clone(), add: "fedora".into(), ballot: Ballot::FIRST },
        Word::Extended { handle: run.clone(), add: "fedora".into() },
    ];
    for w in words {
        let s = serde_json::to_string(&w)?;
        assert_eq!(serde_json::from_str::<Word>(&s)?, w);
        assert!(serde_json::from_str::<ToRunner>(&s).is_err() && serde_json::from_str::<ToOwner>(&s).is_err(), "{s}");
        assert_eq!(w.handle(), &run);
    }
    let probe = serde_json::to_string(&ToRunner::Withdraw { run: "r1".into() })?;
    assert!(serde_json::from_str::<Word>(&probe).is_err(), "placement's own words aren't these");
    Ok(())
}
