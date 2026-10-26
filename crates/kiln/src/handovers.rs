//! Agent sessions handed from a machine's older runner processes to its newest (a take-over:
//! docs/releases.md, "One runner a machine"), led by the server.
//!
//! While a machine has more than one runner process (`crate::instances`), new work goes to the
//! newest and the older ones take nothing new. What an older one runs it used to keep until it
//! ended, and an agent session may never end: the older process, and its service, stayed for good
//! beside the new one. Now each of its sessions goes to the newest once its turn is over: the
//! older process is told to (a local `Command::MoveTo`), packs the session and sends it here
//! ([`AgentMsg::HarnessHandOver`]); it goes on to the newest as a `HarnessPackage`, which that
//! process takes over and says so ([`AgentMsg::HarnessAdopted`]); the older one hears how it went
//! (`Command::HandedOver`) and lets go, or goes on with it. Once it holds nothing, the newest
//! retires it (`Hub::check_retire`). Both processes must take hand-overs
//! ([`crate::proto::CAP_HANDOVER`]); one from before keeps what it runs, as it always did.
//!
//! [`AgentMsg::HarnessHandOver`]: crate::proto::AgentMsg::HarnessHandOver
//! [`AgentMsg::HarnessAdopted`]: crate::proto::AgentMsg::HarnessAdopted

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::harness::model::Command;
use crate::hub::{Hub, LOG_WITHIN};
use crate::instances::{Owned, PeerId};
use crate::proto::ServerMsg;

/// A hand-over not heard of for this long is asked for again (its older process may have missed
/// the ask: a turn that never ends keeps it waiting no longer than its turn).
const ASK_AGAIN: Duration = Duration::from_secs(30 * 60);

/// One session on its way from an older process of `machine` to its newest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handover {
    pub machine: String,
    pub from: PeerId,
    /// The process its package goes to, fixed at its first piece.
    pub to: Option<PeerId>,
    pub asked: Instant,
    /// Its whole package went to `to`: gone away before saying how it went, it may have taken the
    /// session over (CP-T3.2).
    pub whole: bool,
}

/// What the server has under way.
#[derive(Debug, Default)]
pub struct Handovers {
    by_session: HashMap<String, Handover>,
    /// Hand-overs dropped as their older process went away, by machine: that process may wait for
    /// a word on them still (its session's inbox fenced). Answered when it says it holds them again
    /// (`Hub::handovers_dropped`).
    dropped: HashMap<String, HashSet<String>>,
}

/// The server's word to an older process on a session it handed over (`Command::HandedOver`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Word {
    /// The newest took it over (or says it holds it): let go of it.
    Taken,
    /// The newest said no: go on with it.
    Refused(String),
    /// It went to no process (none newer, or not all of it went): go on with it, unless a try
    /// before went without a word.
    NotPassed(String),
    /// It went whole to the newest, which went away before saying how it went: not known.
    Unsure(String),
    /// No newer process of the machine runs to take it: the older process takes it back unless one
    /// took it before (its machine's record says), and goes on with it.
    NoneNewer(String),
}

impl Word {
    /// What the newest process said of a session handed to it (`AgentMsg::HarnessAdopted`'s
    /// `error`), as the word for the older process (CP-T3.2, the nineteenth adversarial check's): a
    /// refusal only when it says the session isn't there (`reach::node::NOT_TAKEN`, as a move's must);
    /// taken when another of the machine's processes took it ([`TAKEN_ELSEWHERE`]); and any other
    /// answer, one that can't say (its note unreadable), not known, so the older process waits and
    /// asks again.
    ///
    /// [`TAKEN_ELSEWHERE`]: crate::harness::host::TAKEN_ELSEWHERE
    pub(crate) fn from_newest(error: Option<String>) -> Self {
        match error {
            None => Self::Taken,
            Some(why) if why.starts_with(crate::harness::host::TAKEN_ELSEWHERE) => Self::Taken,
            Some(why) if why.starts_with(crate::reach::node::NOT_TAKEN) => Self::Refused(why),
            Some(why) => Self::Unsure(why),
        }
    }

    /// The command that says it (`Command::HandedOver`).
    pub(crate) fn command(self) -> Command {
        match self {
            Self::Taken => Command::HandedOver { error: None, unsure: false, by_newest: false, none_newer: false },
            Self::Refused(why) => Command::HandedOver { error: Some(why), unsure: false, by_newest: true, none_newer: false },
            Self::NotPassed(why) => Command::HandedOver { error: Some(why), unsure: false, by_newest: false, none_newer: false },
            Self::Unsure(why) => Command::HandedOver { error: Some(why), unsure: true, by_newest: false, none_newer: false },
            Self::NoneNewer(why) => Command::HandedOver { error: Some(why), unsure: false, by_newest: false, none_newer: true },
        }
    }
}

/// What a piece of a package that came from an older process does.
#[derive(Debug, PartialEq, Eq)]
pub enum Piece {
    /// Goes on to this process.
    To(PeerId),
    /// Goes nowhere: the older process goes on with the session, told why.
    Refused(String),
    /// Goes nowhere, with no newer process of the machine to take it ([`Word::NoneNewer`]).
    NoneNewer(String),
}

impl Handovers {
    /// Sessions `held` by older process `from` of `machine` to ask it to hand over: those not on
    /// their way already (or asked long enough ago to ask again). Each is marked as asked.
    pub fn due(&mut self, machine: &str, from: PeerId, held: Vec<String>, now: Instant) -> Vec<String> {
        let due: Vec<String> = held
            .into_iter()
            .filter(|id| match self.by_session.get(id) {
                Some(h) => h.from != from || (h.to.is_none() && now.duration_since(h.asked) >= ASK_AGAIN),
                None => true,
            })
            .collect();
        for id in &due {
            self.by_session.insert(id.clone(), Handover { machine: machine.to_owned(), from, to: None, asked: now, whole: false });
        }
        due
    }

    /// A piece of session `id`'s package from process `from` of `machine`, while `newest` is its
    /// machine's newest process (or none other than `from` is). `upgrading`: a new build of the
    /// machine's runner is taking over, from that newest process: a package not on its way yet
    /// waits for it (what the leaving process took would stay with it).
    pub fn piece(&mut self, machine: &str, id: &str, from: PeerId, newest: Option<PeerId>, upgrading: bool) -> Piece {
        const UPGRADING: &str = "a new build of this machine's runner is taking over: it is asked for again once that's done";
        // None under way here (this server started since, or the last ended with no word of how it
        // went): its older process asks again, its package going to the newest, which answers one it
        // took before as taken (CP-T3.2).
        if !self.by_session.contains_key(id) {
            if upgrading {
                return Piece::Refused(UPGRADING.into());
            }
            match newest.filter(|n| *n != from) {
                Some(n) => {
                    self.by_session.insert(id.to_owned(), Handover { machine: machine.to_owned(), from, to: Some(n), asked: Instant::now(), whole: false });
                    return Piece::To(n);
                }
                None => return Piece::NoneNewer("no hand-over of it is under way on the server, and this machine has no newer runner process connected".into()),
            }
        }
        let Some(h) = self.by_session.get_mut(id).filter(|h| h.from == from) else {
            return Piece::Refused("another process of this machine hands it over".into());
        };
        if let Some(to) = h.to {
            return Piece::To(to);
        }
        if upgrading {
            self.by_session.remove(id);
            return Piece::Refused(UPGRADING.into());
        }
        match newest.filter(|n| *n != from) {
            Some(n) => {
                h.to = Some(n);
                Piece::To(n)
            }
            None => {
                self.by_session.remove(id);
                Piece::NoneNewer("this machine has no newer runner process connected any more".into())
            }
        }
    }

    /// Session `id`'s whole package went to the process it goes to.
    pub fn went_whole(&mut self, id: &str) {
        if let Some(h) = self.by_session.get_mut(id) {
            h.whole = true;
        }
    }

    /// Whether session `id`'s package went to process `to`, which hasn't said yet how it went.
    pub fn going_to(&self, id: &str, to: PeerId) -> bool {
        self.by_session.get(id).is_some_and(|h| h.to == Some(to))
    }

    /// Process `by` says it took session `id` over, or couldn't: the hand-over it ends, if it
    /// was the one the package went to.
    pub fn adopted(&mut self, id: &str, by: PeerId) -> Option<Handover> {
        if self.by_session.get(id).is_some_and(|h| h.to == Some(by)) { self.by_session.remove(id) } else { None }
    }

    /// Process `peer` went away: the hand-overs it was part of end. Those whose package went to it
    /// are returned: their older process hears how that went. Those of the process that went are
    /// kept, to be answered when it says it holds them again ([`Handovers::dropped_in`]).
    pub fn lost(&mut self, peer: PeerId) -> Vec<(String, Handover)> {
        let ended: Vec<String> = self.by_session.iter().filter(|(_, h)| h.from == peer || h.to == Some(peer)).map(|(id, _)| id.clone()).collect();
        let ended: Vec<(String, Handover)> = ended.into_iter().filter_map(|id| self.by_session.remove(&id).map(|h| (id, h))).collect();
        for (id, h) in ended.iter().filter(|(_, h)| h.from == peer) {
            self.dropped.entry(h.machine.clone()).or_default().insert(id.clone());
        }
        ended.into_iter().filter(|(_, h)| h.from != peer).collect()
    }

    /// Of the sessions a process of `machine` says it holds (`held`), those whose hand-over was
    /// dropped as a process of it went away: no longer remembered once said.
    pub fn dropped_in(&mut self, machine: &str, held: &[String]) -> Vec<String> {
        let Some(d) = self.dropped.get_mut(machine) else { return Vec::new() };
        let out: Vec<String> = held.iter().filter(|id| d.remove(*id)).cloned().collect();
        if d.is_empty() {
            self.dropped.remove(machine);
        }
        out
    }
}

impl Hub {
    /// Every older runner process of a crowded machine hands its agent sessions to the newest, each
    /// once its turn is over: asked here, once (`Handovers::due`).
    pub(crate) async fn check_handover(&self) {
        let crowded = self.instances.lock().crowded();
        if crowded.is_empty() {
            return;
        }
        let mut asks: Vec<(String, PeerId, String)> = Vec::new();
        for (name, peer) in crowded {
            let held: Vec<String> = {
                let inst = self.instances.lock();
                let Some(newest) = inst.newest(&name) else { continue };
                // Nor while a new build takes the machine over: its newest is the process leaving
                // then. Asked for once it's promoted (`Hub::newest_changed`).
                if peer == newest || !inst.hands_over(peer) || !inst.hands_over(newest) || inst.upgrading(&name) {
                    continue;
                }
                inst.owned_by(peer)
                    .into_iter()
                    .filter_map(|o| match o {
                        Owned::Agent(id) => Some(id),
                        _ => None,
                    })
                    .collect()
            };
            // An agent in a terminal stays with the process it runs in until it ends, as terminals do.
            let held: Vec<String> = held
                .into_iter()
                .filter(|id| {
                    self.harness.info(id).and_then(|i| crate::harness::kinds::Kind::get(&i.kind)).is_none_or(|k| !matches!(k.protocol, crate::harness::kinds::Protocol::Terminal))
                })
                .collect();
            if held.is_empty() {
                continue;
            }
            let due = self.handovers.lock().due(&name, peer, held, Instant::now());
            asks.extend(due.into_iter().map(|id| (name.clone(), peer, id)));
        }
        for (name, peer, id) in asks {
            let c = Command::MoveTo { to: name.clone(), cwd: String::new(), note: None, now: false, kind: None, model: None, local: true };
            let Ok(json) = serde_json::to_string(&c) else { continue };
            tracing::info!(agent = %name, peer, session = %id, "an older runner process of this machine hands this agent session to the newest once its turn is over");
            self.command_peers(&name, vec![peer], &ServerMsg::HarnessCmd { session: id, json }).await.log_when_answered(LOG_WITHIN);
        }
    }

    /// A piece of an agent session's package, from the older process handing it over: on to the
    /// machine's newest process. Refused, the older process hears so and goes on with it.
    pub(crate) async fn handover_piece(&self, agent: &str, peer: PeerId, session: String, offset: u64, data: Vec<u8>, done: bool) {
        let (newest, upgrading) = {
            let inst = self.instances.lock();
            (inst.newest(agent).filter(|n| inst.hands_over(*n)), inst.upgrading(agent))
        };
        let piece = self.handovers.lock().piece(agent, &session, peer, newest, upgrading);
        match piece {
            Piece::To(to) => {
                let msg = ServerMsg::HarnessPackage { session: session.clone(), offset, data, done };
                if let Ok(b) = crate::proto::encode(&msg)
                    && self.net.send(to, crate::proto::LANE_CONVO, crate::net::Kind::Log, b).await
                {
                    if done {
                        self.handovers.lock().went_whole(&session);
                        tracing::info!(%agent, %session, from = peer, to, "an agent session's package went on to this machine's newest runner process");
                    }
                    return;
                }
                // Not all of it went: the newest can't have taken it from this try.
                let lost = self.handovers.lock().adopted(&session, to);
                if lost.is_some() {
                    self.handed_over(agent, peer, &session, Word::NotPassed("this machine's newest runner process went away as it was handed over".into())).await;
                }
            }
            Piece::Refused(why) => {
                if done {
                    self.handed_over(agent, peer, &session, Word::NotPassed(why)).await;
                }
            }
            Piece::NoneNewer(why) => {
                if done {
                    self.handed_over(agent, peer, &session, Word::NoneNewer(why)).await;
                }
            }
        }
    }

    /// The newest process took an agent session over, or couldn't (`error`): the older process
    /// that handed it over lets go of it, or goes on with it, or (the newest can't say) asks again.
    pub(crate) async fn handover_adopted(&self, agent: &str, peer: PeerId, session: String, error: Option<String>) {
        let pending = self.handovers.lock().adopted(&session, peer);
        let itself = error.is_none();
        let Some(h) = pending else {
            // Taken over by that process itself, by a hand-over the server no longer remembers (it
            // was dropped as the older process went away, or the server started again): its word
            // still counts, so nothing later says nobody took it. The older process lets go when it
            // says it holds it (the newer keeps a session two processes say they hold).
            if itself {
                tracing::info!(%agent, %session, to = peer, "a runner process of this machine took over an agent session whose hand-over the server had lost track of");
                self.instances.lock().own(Owned::Agent(session.clone()), peer);
                if let Some(label) = self.instances.lock().get(peer).map(crate::instances::Instance::label) {
                    self.harness.held_by(&session, &label);
                }
                self.handovers.lock().dropped_in(agent, std::slice::from_ref(&session));
            }
            return;
        };
        let word = Word::from_newest(error);
        match (&word, itself) {
            // Taken over by that process itself: it holds it now.
            (Word::Taken, true) => {
                tracing::info!(%agent, %session, from = h.from, to = peer, "this machine's newest runner process took an agent session over");
                self.instances.lock().own(Owned::Agent(session.clone()), peer);
                if let Some(label) = self.instances.lock().get(peer).map(crate::instances::Instance::label) {
                    self.harness.held_by(&session, &label);
                }
            }
            // Taken over on the machine before, by another of its processes: the older one lets go,
            // and this one isn't its holder.
            (Word::Taken, false) => tracing::info!(%agent, %session, from = h.from, to = peer, "this machine took an agent session over before, and another of its runner processes runs it: the older one lets go"),
            (Word::Unsure(why), _) => tracing::warn!(%agent, %session, %why, "this machine's newest runner process can't say whether it took an agent session over: the older one waits, and asks again"),
            (Word::Refused(why) | Word::NotPassed(why) | Word::NoneNewer(why), _) => tracing::warn!(%agent, %session, %why, "this machine's newest runner process didn't take an agent session over: it goes on where it was"),
        }
        self.handed_over(agent, h.from, &session, word).await;
        self.check_retire().await;
    }

    /// Runner process `peer` of `agent` says it holds agent sessions `held` (its inventory): those
    /// whose hand-over was dropped as it went away get their word now. With a newer process of the
    /// machine running, they are asked for again ([`Hub::check_handover`]); with none, nothing can
    /// take them now: it takes each try back unless one was taken (its machine's record says) and
    /// goes on with it, its inbox taking messages again.
    pub(crate) async fn handovers_dropped(&self, agent: &str, peer: PeerId, held: &[String]) {
        let dropped = self.handovers.lock().dropped_in(agent, held);
        if dropped.is_empty() {
            return;
        }
        let newer = {
            let inst = self.instances.lock();
            inst.newest(agent).is_some_and(|n| n != peer && inst.hands_over(n))
        };
        if newer {
            self.check_handover().await;
            return;
        }
        for session in dropped {
            tracing::info!(%agent, %session, peer, "a hand-over dropped as its runner process went away: no newer process of this machine runs now, so it goes on there");
            self.handed_over(agent, peer, &session, Word::NoneNewer("no newer runner process of this machine runs now".into())).await;
        }
    }

    /// Runner process `peer` went away: hand-overs whose package went to it end. Their older
    /// processes go on with them, but for those whose whole package went to it: it may have taken
    /// them over before it went, so whether it did isn't known, and their older process waits and
    /// asks again (CP-T3.2, the eighteenth adversarial check's).
    pub(crate) async fn handovers_lost(&self, peer: PeerId) {
        let ended = self.handovers.lock().lost(peer);
        for (session, h) in ended {
            let why = "this machine's newest runner process went away".to_owned();
            self.handed_over(&h.machine, h.from, &session, if h.whole { Word::Unsure(why) } else { Word::NotPassed(why) }).await;
        }
    }

    /// Of two processes of a machine that say they hold one agent session, the older: it lets go
    /// of it (the newer has it).
    pub(crate) async fn let_go_of(&self, agent: &str, older: PeerId, session: &str) {
        tracing::info!(%agent, %session, older, "two runner processes of this machine say they hold an agent session: the older lets go of it");
        self.handed_over(agent, older, session, Word::Taken).await;
    }

    async fn handed_over(&self, agent: &str, to: PeerId, session: &str, word: Word) {
        let Ok(json) = serde_json::to_string(&word.command()) else { return };
        self.command_peers(agent, vec![to], &ServerMsg::HarnessCmd { session: session.to_owned(), json }).await.log_when_answered(LOG_WITHIN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_asked_for_once_and_its_package_goes_to_the_newest() {
        let mut h = Handovers::default();
        let t = Instant::now();
        assert_eq!(h.due("box", 1, vec!["s1".into(), "s2".into()], t), vec!["s1".to_owned(), "s2".to_owned()]);
        // Asked already: not again while it's on its way.
        assert!(h.due("box", 1, vec!["s1".into()], t + Duration::from_secs(5)).is_empty());
        // Its pieces go to the newest at the first, and stay with it.
        assert_eq!(h.piece("box", "s1", 1, Some(2), false), Piece::To(2));
        assert_eq!(h.piece("box", "s1", 1, Some(3), false), Piece::To(2));
        assert!(h.going_to("s1", 2) && !h.going_to("s1", 3) && !h.going_to("s2", 2));
        // Only from the process that was asked.
        assert!(matches!(h.piece("box", "s1", 9, Some(2), false), Piece::Refused(_)));
        // Taken over by the process it went to; nobody else ends it.
        assert_eq!(h.adopted("s1", 3), None);
        assert_eq!(h.adopted("s1", 2).map(|x| x.from), Some(1));
        assert_eq!(h.adopted("s1", 2), None);
    }

    #[test]
    fn with_no_newer_process_the_older_goes_on_with_it() {
        let mut h = Handovers::default();
        let t = Instant::now();
        h.due("box", 1, vec!["s1".into()], t);
        assert!(matches!(h.piece("box", "s1", 1, Some(1), false), Piece::NoneNewer(_)));
        // Ended: asked again later.
        assert_eq!(h.due("box", 1, vec!["s1".into()], t), vec!["s1".to_owned()]);
        assert!(matches!(h.piece("box", "s1", 1, None, false), Piece::NoneNewer(_)));
    }

    #[test]
    fn a_process_going_away_ends_its_hand_overs() {
        let mut h = Handovers::default();
        let t = Instant::now();
        h.due("box", 1, vec!["s1".into(), "s2".into()], t);
        h.due("box", 4, vec!["s3".into()], t);
        assert_eq!(h.piece("box", "s1", 1, Some(2), false), Piece::To(2));
        assert_eq!(h.piece("box", "s3", 4, Some(5), false), Piece::To(5));
        // The newest went away: what went to it goes on where it was, said so.
        let lost: Vec<String> = h.lost(2).into_iter().map(|(id, x)| format!("{id}<-{}", x.from)).collect();
        assert_eq!(lost, vec!["s1<-1".to_owned()]);
        // The older went away: nothing to tell it; its sessions are asked for again on its return.
        assert!(h.lost(1).is_empty());
        assert_eq!(h.due("box", 1, vec!["s2".into()], t), vec!["s2".to_owned()]);
        assert_eq!(h.adopted("s3", 5).map(|x| x.machine), Some("box".to_owned()));
    }

    /// CP-T3.2 (the nineteenth adversarial check's): the newest's answer to a hand-over, as the older
    /// process hears it: a refusal only where it says it wasn't taken over (reach's `NOT_TAKEN`);
    /// taken over on the machine before, taken; any other (it can't say now), not known, so the
    /// older process waits and asks again.
    #[test]
    fn the_newest_s_answer_is_a_refusal_only_when_it_says_it_wasn_t_taken() {
        use crate::harness::host::TAKEN_ELSEWHERE;
        use crate::reach::node::NOT_TAKEN;
        assert_eq!(Word::from_newest(None), Word::Taken);
        assert_eq!(Word::from_newest(Some(format!("{TAKEN_ELSEWHERE}it ran there"))), Word::Taken);
        assert_eq!(Word::from_newest(Some(format!("{NOT_TAKEN}no room"))), Word::Refused(format!("{NOT_TAKEN}no room")));
        assert_eq!(Word::from_newest(Some("its record can't be read now".into())), Word::Unsure("its record can't be read now".into()));
        assert!(matches!(Word::Unsure("x".into()).command(), Command::HandedOver { error: Some(_), unsure: true, by_newest: false, .. }));
        assert!(matches!(Word::Refused("x".into()).command(), Command::HandedOver { error: Some(_), unsure: false, by_newest: true, .. }));
        assert!(matches!(Word::Taken.command(), Command::HandedOver { error: None, .. }));
    }

    #[test]
    fn a_hand_over_never_heard_of_is_asked_for_again() {
        let mut h = Handovers::default();
        let t = Instant::now();
        h.due("box", 1, vec!["s1".into()], t);
        assert!(h.due("box", 1, vec!["s1".into()], t + ASK_AGAIN - Duration::from_secs(1)).is_empty());
        assert_eq!(h.due("box", 1, vec!["s1".into()], t + ASK_AGAIN), vec!["s1".to_owned()]);
        // Once its package is on its way, it isn't.
        assert_eq!(h.piece("box", "s1", 1, Some(2), false), Piece::To(2));
        assert!(h.due("box", 1, vec!["s1".into()], t + ASK_AGAIN * 3).is_empty());
    }

    #[test]
    fn a_hand_over_dropped_with_its_older_process_is_remembered_until_it_says_it_holds_it() {
        let mut h = Handovers::default();
        let t = Instant::now();
        h.due("box", 1, vec!["s1".into(), "s2".into()], t);
        h.due("mac", 7, vec!["s9".into()], t);
        assert_eq!(h.piece("box", "s1", 1, Some(2), false), Piece::To(2));
        // The older process goes: nothing to tell it now, but both its hand-overs are kept.
        assert!(h.lost(1).is_empty());
        assert!(h.dropped_in("mac", &["s1".into()]).is_empty(), "another machine's aren't these");
        // Back, it holds s1 (s2 ended meanwhile): s1 is answered, once.
        assert_eq!(h.dropped_in("box", &["s1".into(), "s3".into()]), vec!["s1".to_owned()]);
        assert!(h.dropped_in("box", &["s1".into()]).is_empty());
        assert_eq!(h.dropped_in("box", &["s2".into()]), vec!["s2".to_owned()]);
        assert!(!h.dropped.contains_key("box"), "nothing left of the machine");
        // The newer process going: its older one is told at once, and nothing is kept.
        assert_eq!(h.piece("mac", "s9", 7, Some(8), false), Piece::To(8));
        assert_eq!(h.lost(8).len(), 1);
        assert!(h.dropped_in("mac", &["s9".into()]).is_empty());
    }

    #[test]
    fn a_package_waits_while_a_new_build_takes_the_machine_over() {
        let mut h = Handovers::default();
        let t = Instant::now();
        h.due("box", 1, vec!["s1".into(), "s2".into()], t);
        // One on its way already goes on where it goes.
        assert_eq!(h.piece("box", "s1", 1, Some(2), false), Piece::To(2));
        assert_eq!(h.piece("box", "s1", 1, Some(2), true), Piece::To(2));
        // One not yet doesn't go to the process leaving: refused, and asked for again later.
        assert!(matches!(h.piece("box", "s2", 1, Some(2), true), Piece::Refused(_)));
        assert!(!h.going_to("s2", 2));
        assert_eq!(h.due("box", 1, vec!["s2".into()], t), vec!["s2".to_owned()]);
        // Nor one sent again with none under way here (CP-T3.2's: it goes to the newest otherwise).
        assert!(matches!(h.piece("box", "s3", 1, Some(2), true), Piece::Refused(_)));
        assert!(!h.going_to("s3", 2));
        assert_eq!(h.piece("box", "s3", 1, Some(2), false), Piece::To(2));
    }

    /// CP-T3.2 with main's dropped hand-overs: with no newer process of the machine, the word an older
    /// process hears says so ([`Word::NoneNewer`]): it takes the try back unless one was taken.
    #[test]
    fn with_no_newer_process_the_word_says_none_runs() {
        let mut h = Handovers::default();
        assert!(matches!(h.piece("box", "s1", 1, None, false), Piece::NoneNewer(_)), "none under way and none newer");
        assert!(matches!(Word::NoneNewer("x".into()).command(), Command::HandedOver { error: Some(_), unsure: false, by_newest: false, none_newer: true }));
        assert!(matches!(Word::NotPassed("x".into()).command(), Command::HandedOver { none_newer: false, .. }));
    }
}
