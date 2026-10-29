//! Whose turn it is on a change (docs/ci.md W26-R05), the pure part: Gerrit's attention set, as kiln
//! keeps it. An asked reviewer (a person or an agent) joins the turn; a reply hands it back (the
//! author's reply to the reviewers still to approve, a reviewer's to the author); a push hands it to
//! the reviewers; an approval takes its giver out, and once every reviewer has approved the turn is
//! empty; a land or an abandon empties it. A draft has no turn. The server folds a change's events
//! in order and orders Ship's needs-you-first by the result.

use std::collections::BTreeSet;

/// Something that happened on a change, as it bears on whose turn it is. Principals are written as
/// kiln writes them (`user:kim`, `agent:…`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A reviewer (a person or an agent) was asked.
    Asked(String),
    /// A reviewer was taken off the change.
    Removed(String),
    /// Someone commented, answered a thread or asked for changes.
    Replied(String),
    /// A new revision: pushed, applied or replayed. A rebase-only one keeps its approvals.
    Pushed { rebase_only: bool },
    /// Someone approved the latest revision.
    Approved(String),
    /// Pushed as a draft, or turned back into one.
    Draft,
    /// Out of draft: asking for review.
    Ready,
    /// Landed or abandoned.
    Closed,
}

/// Where a change's turn stands after its events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Turn {
    pub reviewers: BTreeSet<String>,
    /// Who approved the latest revision (or one it only rebases).
    pub approved: BTreeSet<String>,
    /// Whose turn it is.
    pub turn: BTreeSet<String>,
    pub draft: bool,
    pub closed: bool,
}

impl Turn {
    fn waiting_on(&self) -> BTreeSet<String> {
        self.reviewers.difference(&self.approved).cloned().collect()
    }

    /// Whether the change waits on `who`.
    pub fn holds(&self, who: &str) -> bool {
        self.turn.contains(who)
    }
}

/// One event's effect on a change by `author`.
pub fn step(t: &mut Turn, author: &str, e: &Event) {
    if t.closed {
        return;
    }
    match e {
        Event::Asked(who) if who != author => {
            t.reviewers.insert(who.clone());
            if !t.draft && !t.approved.contains(who) {
                t.turn.insert(who.clone());
            }
        }
        Event::Asked(_) => {}
        Event::Removed(who) => {
            t.reviewers.remove(who);
            t.approved.remove(who);
            t.turn.remove(who);
        }
        Event::Replied(who) => {
            t.turn.remove(who);
            if t.draft {
                return;
            }
            if who == author {
                let back = t.waiting_on();
                t.turn.extend(back);
            } else {
                t.turn.insert(author.to_string());
            }
        }
        Event::Pushed { rebase_only } => {
            if !rebase_only {
                t.approved.clear();
            }
            if !t.draft {
                t.turn = t.waiting_on();
            }
        }
        Event::Approved(who) if who != author => {
            t.approved.insert(who.clone());
            t.reviewers.insert(who.clone());
            t.turn.remove(who);
            if t.waiting_on().is_empty() {
                t.turn.clear();
            }
        }
        Event::Approved(_) => {}
        Event::Draft => {
            t.draft = true;
            t.turn.clear();
        }
        Event::Ready => {
            t.draft = false;
            t.turn = t.waiting_on();
        }
        Event::Closed => {
            t.closed = true;
            t.turn.clear();
        }
    }
}

/// A change's turn after its events, in order.
pub fn fold(author: &str, events: &[Event]) -> Turn {
    let mut t = Turn::default();
    for e in events {
        step(&mut t, author, e);
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAM: &str = "user:sam";
    const KIM: &str = "user:kim";
    const LEE: &str = "user:lee";
    const BOT: &str = "agent:review";

    fn turn(events: &[Event]) -> Vec<String> {
        fold(SAM, events).turn.into_iter().collect()
    }

    fn asked(who: &str) -> Event {
        Event::Asked(who.into())
    }

    fn replied(who: &str) -> Event {
        Event::Replied(who.into())
    }

    fn approved(who: &str) -> Event {
        Event::Approved(who.into())
    }

    #[test]
    fn w26_r05_an_asked_reviewer_or_agent_joins_the_turn() {
        assert_eq!(turn(&[asked(KIM)]), [KIM]);
        assert_eq!(turn(&[asked(KIM), asked(BOT)]), [BOT, KIM]);
        assert_eq!(turn(&[asked(SAM)]), Vec::<String>::new(), "the author never reviews their own change");
    }

    #[test]
    fn w26_r05_a_reply_hands_the_turn_back() {
        // kim asks a question: it's sam's; sam answers: it's kim's again.
        assert_eq!(turn(&[asked(KIM), replied(KIM)]), [SAM]);
        assert_eq!(turn(&[asked(KIM), replied(KIM), replied(SAM)]), [KIM]);
        // The author's answer goes to every reviewer still to approve, not to one who has.
        assert_eq!(turn(&[asked(KIM), asked(LEE), approved(LEE), replied(KIM), replied(SAM)]), [KIM]);
    }

    #[test]
    fn w26_r05_a_push_hands_the_turn_to_the_reviewers() {
        assert_eq!(turn(&[asked(KIM), replied(KIM), Event::Pushed { rebase_only: false }]), [KIM]);
        // A real push needs every approval again; a rebase-only one keeps them.
        assert_eq!(turn(&[asked(KIM), asked(LEE), approved(KIM), Event::Pushed { rebase_only: false }]), [KIM, LEE]);
        assert_eq!(turn(&[asked(KIM), asked(LEE), approved(KIM), Event::Pushed { rebase_only: true }]), [LEE]);
    }

    #[test]
    fn w26_r05_an_approval_or_a_land_empties_the_turn() {
        assert_eq!(turn(&[asked(KIM), approved(KIM)]), Vec::<String>::new());
        assert_eq!(turn(&[asked(KIM), asked(LEE), approved(KIM)]), [LEE], "one approval of two leaves the other's turn");
        assert_eq!(turn(&[asked(KIM), asked(LEE), approved(KIM), approved(LEE)]), Vec::<String>::new());
        let landed = fold(SAM, &[asked(KIM), replied(KIM), Event::Closed, asked(LEE), replied(SAM)]);
        assert!(landed.turn.is_empty() && landed.closed, "nothing after a land moves it");
        assert_eq!(turn(&[asked(KIM), approved(SAM)]), [KIM], "the author's approval counts for nothing");
    }

    #[test]
    fn w26_r05_a_draft_has_no_turn_until_it_asks() {
        let draft = [Event::Draft, asked(KIM), replied(KIM)];
        assert_eq!(turn(&draft), Vec::<String>::new());
        let mut ready = draft.to_vec();
        ready.push(Event::Ready);
        assert_eq!(turn(&ready), [KIM]);
        assert_eq!(turn(&[asked(KIM), Event::Draft]), Vec::<String>::new(), "back to draft clears it");
    }

    #[test]
    fn w26_r05_a_removed_reviewer_leaves_the_turn() {
        let t = fold(SAM, &[asked(KIM), asked(LEE), Event::Removed(KIM.into())]);
        assert_eq!((t.turn.into_iter().collect::<Vec<_>>(), t.reviewers.len()), (vec![LEE.to_string()], 1));
    }

    #[test]
    fn w26_r05_the_dynamic_matrices_example() {
        // D41: kim asked; kim's question hands it to sam; sam's v2 hands it to kim; kim approves.
        let mut t = fold(SAM, &[asked(KIM)]);
        assert!(t.holds(KIM));
        step(&mut t, SAM, &replied(KIM));
        assert!(t.holds(SAM) && !t.holds(KIM));
        step(&mut t, SAM, &Event::Pushed { rebase_only: false });
        assert!(t.holds(KIM) && !t.holds(SAM));
        step(&mut t, SAM, &approved(KIM));
        assert!(t.turn.is_empty());
    }
}
