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

/// How long a home may take to make and export a pod's workspace: a volume made and exported, on a loaded machine.
const HOLD_WAIT: Duration = Duration::from_secs(120);

/// A home's answer to a hold (`crate::workspace::held::hold`): the export's path and attachment, and the folder's owner
/// and folder.
#[derive(serde::Deserialize, serde::Serialize)]
struct Held {
    path: String,
    uid: u32,
    gid: u32,
    folder: String,
}

/// The host and base port `address` (`[slates_fleet.nodes]`'s `host:port`, an IPv6 host in brackets) names.
pub fn host_and_port(address: &str) -> anyhow::Result<(String, u16)> {
    let (host, port) = address.rsplit_once(':').ok_or_else(|| anyhow::anyhow!("{address:?} names no port"))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    anyhow::ensure!(!host.is_empty(), "{address:?} names no host");
    Ok((host.to_owned(), port.parse().map_err(|_| anyhow::anyhow!("{address:?}'s port isn't one"))?))
}

/// The name of session `id`'s pod, its claim and its PersistentVolume: `kiln-agent-` and the session id's last letters
/// and digits, lowercase, a DNS label.
pub fn pod_name(id: &str) -> String {
    let tail: String = id.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect();
    let tail = tail.get(tail.len().saturating_sub(40)..).unwrap_or(&tail);
    format!("kiln-agent-{tail}")
}

impl super::K8s {
    /// Starts session `id` (`spec`) in an agent's pod of its own (docs/agentic.md W10), its workspace held by its home
    /// `home`: the home makes and exports the workspace (`workspace.hold`), the PersistentVolume and its claim are made
    /// from the export at the address `[slates_fleet.nodes]` lists for the home, and the pod is made with a one-time token
    /// for its runner, which is given the session as it comes ([`on_features`]). What it made is taken down again where a
    /// later step fails. The pod's name.
    pub async fn start_agent(&self, hub: &Hub, id: &str, spec: Spec, home: &str) -> anyhow::Result<String> {
        let image = self.cfg.agents_image.clone().filter(|i| !i.trim().is_empty()).ok_or_else(|| anyhow::anyhow!("the server names no image for agents' pods: set `agents_image` under `[kubernetes]`"))?;
        let address = hub.cfg.slates_fleet.nodes.get(home).ok_or_else(|| anyhow::anyhow!("{} isn't in kiln's Slates fleet (`[slates_fleet.nodes]`), so it serves no export for a pod", hub.shown(home)))?;
        let (host, port) = host_and_port(address)?;
        let cwd = crate::harness::session::resolve_cwd(spec.cwd.as_deref());
        let hold = crate::workspace::Ask::Hold(crate::workspace::held::Hold { cwd, level: Some(spec.launched_level()), read_only: false });
        let json = serde_json::to_string(&hold)?;
        let session = id.to_owned();
        let answer: serde_json::Value = hub
            .ask_speaking(home, crate::proto::feature::WORKSPACE_HOLD, HOLD_WAIT, move |req| crate::proto::ServerMsg::Workspace { req, session, json })
            .await?
            .ok_or_else(|| anyhow::anyhow!("{}'s kiln holds no workspace for a pod: upgrade it (System → Updates)", hub.shown(home)))?
            .unwrap_or(serde_json::Value::Null);
        if let Some(e) = answer.get("error").and_then(serde_json::Value::as_str) {
            anyhow::bail!("{} didn't hold the pod's workspace: {e}", hub.shown(home));
        }
        let held: Held = serde_json::from_value(answer)?;
        let name = pod_name(id);
        let made: anyhow::Result<String> = async {
            let export = super::volume::PodExport { host, port, path: held.path, read_only: false, uid: held.uid, gid: held.gid, folder: held.folder };
            let objects = super::volume::objects(&name, &self.cfg.namespace, id, &export).map_err(anyhow::Error::new)?;
            super::volume::make(&self.api, &objects).await?;
            let until = Instant::now() + token_ttl(&self.cfg);
            let token = hub.agent_pods.issue(&name, PodSession { id: id.to_owned(), spec, home: home.to_owned(), opened: false }, until);
            let server_url = self.cfg.server_url.clone().unwrap_or_else(|| hub.cfg.server.public_url.clone());
            let server_quic = self.cfg.server_quic.clone().unwrap_or_else(|| hub.cfg.server.quic_advertise.clone());
            let pod = super::volume::agent_pod(&self.cfg, id, &name, &image, &token, (&server_url, &server_quic), &objects);
            if let Err(e) = self.api.create_pod(&pod).await {
                hub.agent_pods.take(&name);
                if let Err(gone) = super::volume::unmake(&self.api, &name).await {
                    tracing::warn!(pod = %name, error = %format!("{gone:#}"), "a refused agent pod's claim and PersistentVolume stayed");
                }
                return Err(e);
            }
            Ok(name)
        }
        .await;
        if made.is_err() {
            release(hub, home, id).await;
        }
        made
    }
}

/// Asks home `home` to let session `id`'s held workspace go (its export detached, its volume ended); one that can't is
/// said, the volume ending with its instance's own rules.
pub async fn release(hub: &Hub, home: &str, id: &str) {
    let Ok(json) = serde_json::to_string(&crate::workspace::Ask::Release) else { return };
    let session = id.to_owned();
    let asked: anyhow::Result<Option<Option<serde_json::Value>>> = hub.ask_speaking(home, crate::proto::feature::WORKSPACE_HOLD, HOLD_WAIT, move |req| crate::proto::ServerMsg::Workspace { req, session, json }).await;
    if let Err(e) = asked {
        tracing::warn!(home, session = id, error = %format!("{e:#}"), "a pod's held workspace wasn't let go at its home");
    }
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
        assert_eq!(host_and_port("10.0.0.1:17300").ok(), Some(("10.0.0.1".to_owned(), 17300)));
        assert_eq!(host_and_port("[fd7a::1]:17300").ok(), Some(("fd7a::1".to_owned(), 17300)));
        assert_eq!(host_and_port("runner-a.lan:17302").ok(), Some(("runner-a.lan".to_owned(), 17302)));
        assert!(host_and_port("runner-a.lan").is_err() && host_and_port(":17300").is_err() && host_and_port("h:x").is_err());
        let name = pod_name("01A1F7FF-6874e8a4691fe653779c97");
        assert_eq!(name, "kiln-agent-01a1f7ff6874e8a4691fe653779c97");
        assert!(name.len() <= 63 && pod_name(&"9".repeat(200)).len() <= 63);
        let late = pods.issue("kiln-ws-b", PodSession { id: "01a1f8".to_owned(), spec: Spec::default(), home: "studio".to_owned(), opened: false }, Instant::now());
        assert_eq!(pods.enrolling(&late), None, "a token past its time is refused");
    }
}
