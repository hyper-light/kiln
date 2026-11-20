//! Agents in pods of their own on kiln's Kubernetes engine (docs/agentic.md W10): the server's record of each pod it
//! made for an agent session, from the pod's start to its end.
//!
//! **One token, one pod, one session.** Each agent pod's runner enrolls with a one-time token bound to its pod's name
//! ([`issue`]), as a pool executor's does, and is named for its pod whatever it asks ([`enrolling`]). Once it names its
//! features (its link is up and it speaks), the session it was made for is opened on it, once, under the session's own id
//! ([`on_features`]): the id its home holds the workspace by, so the session's review, drift and landing are asked of
//! its home by that id ([`home_of`]), never of the pod.
//!
//! A record stays until the pod's session ends and the pod, its claim and PersistentVolume are deleted and its home lets
//! the workspace go (the reaper).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::harness::session::Spec;
use crate::hub::Hub;

/// An agent pod the server made: the session it runs, the runner that holds the session's workspace (its home), and
/// whether its session was opened on it yet.
#[derive(Clone)]
pub struct PodSession {
    /// The session's id: its home holds its workspace by it.
    pub id: String,
    pub spec: Spec,
    pub home: String,
    pub opened: bool,
}

/// How long a pod's token may wait for its runner to enroll: its image pulled and its kiln started, as a job's pod's.
pub fn token_ttl(cfg: &crate::config::Kubernetes) -> Duration {
    Duration::from_secs(u64::from(cfg.start_timeout_seconds) + 120)
}

/// What the hub keeps of agent pods: tokens waiting for their runner, and each pod's session by the pod's name.
#[derive(Default)]
pub struct AgentPods {
    tokens: crate::rcu::Rcu<HashMap<String, (String, Instant)>>,
    pods: crate::rcu::Rcu<HashMap<String, PodSession>>,
}

impl AgentPods {
    /// A one-time token for pod `pod`'s runner, good until `until`, and the session it runs: those past their time go
    /// as one is added.
    pub fn issue(&self, pod: &str, session: PodSession, until: Instant) -> String {
        let token = crate::util::random_token();
        self.tokens.update(|t| {
            t.retain(|_, (_, exp)| *exp > Instant::now());
            t.insert(token.clone(), (pod.to_owned(), until));
        });
        self.pods.update(|p| {
            p.insert(pod.to_owned(), session.clone());
        });
        token
    }

    /// The pod whose runner enrolls with `token`: taken once, refused once its time is up.
    pub fn enrolling(&self, token: &str) -> Option<String> {
        self.tokens.update(|t| t.remove(token)).filter(|(_, exp)| *exp > Instant::now()).map(|(pod, _)| pod)
    }

    /// Pod `runner`'s session to open on it, once, its id and its spec: none for a runner that's no agent pod, or whose
    /// session is open.
    pub fn to_open(&self, runner: &str) -> Option<(String, Spec)> {
        let mut open = None;
        self.pods.update(|p| {
            if let Some(s) = p.get_mut(runner).filter(|s| !s.opened) {
                s.opened = true;
                open = Some((s.id.clone(), s.spec.clone()));
            }
        });
        open
    }

    /// The runner holding pod `runner`'s session's workspace: none for a runner that's no agent pod.
    pub fn home_of(&self, runner: &str) -> Option<String> {
        self.pods.load().get(runner).map(|s| s.home.clone())
    }

    /// Pod `runner`'s record, taken out as its end is reaped.
    pub fn take(&self, runner: &str) -> Option<PodSession> {
        self.pods.update(|p| p.remove(runner))
    }
}

/// A runner named its features: an agent pod's runner is given the session it was made for, opened under the session's
/// own id. One whose session is open already, or no agent pod's, is left as it is.
pub async fn on_features(hub: &Hub, runner: &str) {
    let Some((id, spec)) = hub.agent_pods.to_open(runner) else { return };
    let target = format!("agent:{runner}");
    match hub.harness.open_as(hub, &target, spec, 1, std::slice::from_ref(&id)).await {
        Ok(_) => tracing::info!(pod = runner, session = %id, "an agent pod's session opened on its runner"),
        Err(e) => tracing::warn!(pod = runner, session = %id, error = %format!("{e:#}"), "an agent pod's session didn't open on its runner"),
    }
}

/// The runner that answers questions about the folder of a session on runner `machine`: its home where `machine` is an
/// agent pod, else none (its own machine answers).
pub fn home_of(hub: &Hub, machine: &str) -> Option<String> {
    hub.agent_pods.home_of(machine)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W10. Do: issue a pod's token and take it as its runner enrolls, twice; then open its session, twice; ask its home;
    /// let a token's time run out. Expect the token good once, naming the pod; the session given once, the pod's home
    /// known until its record is taken; and a token past its time refused.
    #[test]
    fn w10_an_agent_pods_runner_enrolls_once_and_is_given_its_one_session_once() {
        let pods = AgentPods::default();
        let spec = Spec { kind: "claude".to_owned(), ..Spec::default() };
        let token = pods.issue("kiln-ws-a", PodSession { id: "01a1f7".to_owned(), spec, home: "studio".to_owned(), opened: false }, Instant::now() + Duration::from_secs(60));
        assert_eq!(pods.enrolling(&token), Some("kiln-ws-a".to_owned()));
        assert_eq!(pods.enrolling(&token), None, "a token is good once");
        assert_eq!(pods.to_open("kiln-ws-a").map(|(id, s)| (id, s.kind)), Some(("01a1f7".to_owned(), "claude".to_owned())));
        assert!(pods.to_open("kiln-ws-a").is_none(), "its session is opened once");
        assert!(pods.to_open("some-runner").is_none());
        assert_eq!(pods.home_of("kiln-ws-a"), Some("studio".to_owned()));
        assert!(pods.take("kiln-ws-a").is_some() && pods.home_of("kiln-ws-a").is_none());
        let late = pods.issue("kiln-ws-b", PodSession { id: "01a1f8".to_owned(), spec: Spec::default(), home: "studio".to_owned(), opened: false }, Instant::now());
        assert_eq!(pods.enrolling(&late), None, "a token past its time is refused");
    }
}
