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

use std::collections::HashMap;
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
}

/// What the server has under way.
#[derive(Debug, Default)]
pub struct Handovers {
    by_session: HashMap<String, Handover>,
}

/// What a piece of a package that came from an older process does.
#[derive(Debug, PartialEq, Eq)]
pub enum Piece {
    /// Goes on to this process.
    To(PeerId),
    /// Goes nowhere: the older process goes on with the session, told why.
    Refused(String),
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
            self.by_session.insert(id.clone(), Handover { machine: machine.to_owned(), from, to: None, asked: now });
        }
        due
    }

    /// A piece of session `id`'s package from process `from`, while `newest` is its machine's
    /// newest process (or none other than `from` is).
    pub fn piece(&mut self, id: &str, from: PeerId, newest: Option<PeerId>) -> Piece {
        let Some(h) = self.by_session.get_mut(id).filter(|h| h.from == from) else {
            return Piece::Refused("no hand-over of it is under way on the server (it may have restarted)".into());
        };
        if let Some(to) = h.to {
            return Piece::To(to);
        }
        match newest.filter(|n| *n != from) {
            Some(n) => {
                h.to = Some(n);
                Piece::To(n)
            }
            None => {
                self.by_session.remove(id);
                Piece::Refused("this machine has no newer runner process connected any more".into())
            }
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
    /// are returned: their older process goes on with them.
    pub fn lost(&mut self, peer: PeerId) -> Vec<(String, Handover)> {
        let ended: Vec<String> = self.by_session.iter().filter(|(_, h)| h.from == peer || h.to == Some(peer)).map(|(id, _)| id.clone()).collect();
        ended.into_iter().filter_map(|id| self.by_session.remove(&id).map(|h| (id, h))).filter(|(_, h)| h.from != peer).collect()
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
                if peer == newest || !inst.hands_over(peer) || !inst.hands_over(newest) {
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
        let newest = {
            let inst = self.instances.lock();
            inst.newest(agent).filter(|n| inst.hands_over(*n))
        };
        let piece = self.handovers.lock().piece(&session, peer, newest);
        match piece {
            Piece::To(to) => {
                let msg = ServerMsg::HarnessPackage { session: session.clone(), offset, data, done };
                if let Ok(b) = crate::proto::encode(&msg)
                    && self.net.send(to, crate::proto::LANE_CONVO, crate::net::Kind::Log, b).await
                {
                    if done {
                        tracing::info!(%agent, %session, from = peer, to, "an agent session's package went on to this machine's newest runner process");
                    }
                    return;
                }
                let lost = self.handovers.lock().adopted(&session, to);
                if lost.is_some() {
                    self.handed_over(agent, peer, &session, Some("this machine's newest runner process went away as it was handed over".into())).await;
                }
            }
            Piece::Refused(why) => {
                if done {
                    self.handed_over(agent, peer, &session, Some(why)).await;
                }
            }
        }
    }

    /// The newest process took an agent session over, or couldn't (`error`): the older process
    /// that handed it over lets go of it, or goes on with it.
    pub(crate) async fn handover_adopted(&self, agent: &str, peer: PeerId, session: String, error: Option<String>) {
        let Some(h) = self.handovers.lock().adopted(&session, peer) else { return };
        match &error {
            None => {
                tracing::info!(%agent, %session, from = h.from, to = peer, "this machine's newest runner process took an agent session over");
                self.instances.lock().own(Owned::Agent(session.clone()), peer);
                if let Some(label) = self.instances.lock().get(peer).map(crate::instances::Instance::label) {
                    self.harness.held_by(&session, &label);
                }
            }
            Some(why) => tracing::warn!(%agent, %session, %why, "this machine's newest runner process couldn't take an agent session over: it goes on where it was"),
        }
        self.handed_over(agent, h.from, &session, error).await;
        self.check_retire().await;
    }

    /// Runner process `peer` went away: hand-overs whose package went to it end, and their older
    /// processes go on with them.
    pub(crate) async fn handovers_lost(&self, peer: PeerId) {
        let ended = self.handovers.lock().lost(peer);
        for (session, h) in ended {
            self.handed_over(&h.machine, h.from, &session, Some("this machine's newest runner process went away".into())).await;
        }
    }

    /// Of two processes of a machine that say they hold one agent session, the older: it lets go
    /// of it (the newer has it).
    pub(crate) async fn let_go_of(&self, agent: &str, older: PeerId, session: &str) {
        tracing::info!(%agent, %session, older, "two runner processes of this machine say they hold an agent session: the older lets go of it");
        self.handed_over(agent, older, session, None).await;
    }

    async fn handed_over(&self, agent: &str, to: PeerId, session: &str, error: Option<String>) {
        let Ok(json) = serde_json::to_string(&Command::HandedOver { error }) else { return };
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
        assert_eq!(h.piece("s1", 1, Some(2)), Piece::To(2));
        assert_eq!(h.piece("s1", 1, Some(3)), Piece::To(2));
        assert!(h.going_to("s1", 2) && !h.going_to("s1", 3) && !h.going_to("s2", 2));
        // Only from the process that was asked.
        assert!(matches!(h.piece("s1", 9, Some(2)), Piece::Refused(_)));
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
        assert!(matches!(h.piece("s1", 1, Some(1)), Piece::Refused(_)));
        // Ended: asked again later.
        assert_eq!(h.due("box", 1, vec!["s1".into()], t), vec!["s1".to_owned()]);
        assert!(matches!(h.piece("s1", 1, None), Piece::Refused(_)));
    }

    #[test]
    fn a_process_going_away_ends_its_hand_overs() {
        let mut h = Handovers::default();
        let t = Instant::now();
        h.due("box", 1, vec!["s1".into(), "s2".into()], t);
        h.due("box", 4, vec!["s3".into()], t);
        assert_eq!(h.piece("s1", 1, Some(2)), Piece::To(2));
        assert_eq!(h.piece("s3", 4, Some(5)), Piece::To(5));
        // The newest went away: what went to it goes on where it was, said so.
        let lost: Vec<String> = h.lost(2).into_iter().map(|(id, x)| format!("{id}<-{}", x.from)).collect();
        assert_eq!(lost, vec!["s1<-1".to_owned()]);
        // The older went away: nothing to tell it; its sessions are asked for again on its return.
        assert!(h.lost(1).is_empty());
        assert_eq!(h.due("box", 1, vec!["s2".into()], t), vec!["s2".to_owned()]);
        assert_eq!(h.adopted("s3", 5).map(|x| x.machine), Some("box".to_owned()));
    }

    #[test]
    fn a_hand_over_never_heard_of_is_asked_for_again() {
        let mut h = Handovers::default();
        let t = Instant::now();
        h.due("box", 1, vec!["s1".into()], t);
        assert!(h.due("box", 1, vec!["s1".into()], t + ASK_AGAIN - Duration::from_secs(1)).is_empty());
        assert_eq!(h.due("box", 1, vec!["s1".into()], t + ASK_AGAIN), vec!["s1".to_owned()]);
        // Once its package is on its way, it isn't.
        assert_eq!(h.piece("s1", 1, Some(2)), Piece::To(2));
        assert!(h.due("box", 1, vec!["s1".into()], t + ASK_AGAIN * 3).is_empty());
    }
}
