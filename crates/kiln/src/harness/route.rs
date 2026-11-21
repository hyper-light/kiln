//! A message's route (Ada, 2026-11-20): the runner its session is on and who sent it, carried with it on every hop.
//! Whichever kiln node a message reaches sends it on to that runner, where the session's own inbox takes it into
//! custody (docs/sends.md, "Routed to its session"); a node that isn't the session's holder never keeps it. A runner
//! that gets one for a session it doesn't hold (it moved on, or was never here) sends it on in turn
//! (`AgentMsg::HarnessForward`), one hop further, to whoever holds it now.
//!
//! It travels as `route` beside the command in the JSON a [`crate::proto::ServerMsg::HarnessSend`] carries (as
//! `relayed` does): a runner from before routing reads the command and leaves it, though none is sent one.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Most times a message is sent on after the node that first routed it: past that it's refused `no_session` (holders
/// that each say it's elsewhere).
pub const HOPS_MAX: u8 = 4;

/// Where a message goes and whose it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    /// The runner its session is on, as the node that sent it on last heard (`local`: the server's own host).
    pub target: String,
    /// Who sent it, as `harness::ops::key_scope` names a caller (`admin`, `user:<id>`, `agent:<id>`), or `server`
    /// for kiln's own: how the session's inbox admits it (a person's goes first, held to no depth: `mail::person`).
    pub sender: String,
    /// Times it was sent on since it was first routed.
    #[serde(default)]
    pub hops: u8,
    /// An explicitly replaceable status (news, W20's coalescing): a newer one from the same sender under the same key
    /// replaces one still waiting in the session's inbox. Instructions and results never carry one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<super::mail::StatusKey>,
}

#[derive(Deserialize)]
struct Carried {
    #[serde(default)]
    route: Option<Route>,
}

impl Route {
    /// A route to `target` for `sender`'s message, first routed now.
    pub fn new(target: impl Into<String>, sender: impl Into<String>) -> Self {
        Self { target: target.into(), sender: sender.into(), hops: 0, status: None }
    }

    /// The same message's route one hop further, to `target`: none past [`HOPS_MAX`].
    pub fn onward(&self, target: impl Into<String>) -> Option<Self> {
        (self.hops < HOPS_MAX).then(|| Self { target: target.into(), hops: self.hops.saturating_add(1), ..self.clone() })
    }

    /// `command` (a command as JSON) with this route beside it, and kiln's record of relaying another agent's words
    /// (`relayed`, docs/ci.md W27-R08), as a runner is sent it. None when it isn't a JSON object.
    pub fn attach(&self, command: Value, relayed: Option<&Value>) -> Option<String> {
        let mut command = command;
        let object = command.as_object_mut()?;
        object.insert("route".into(), serde_json::to_value(self).ok()?);
        if let Some(relayed) = relayed {
            object.insert("relayed".into(), relayed.clone());
        }
        serde_json::to_string(&command).ok()
    }

    /// The route the JSON of a command as a runner was sent it carries: none when it carries none (sent as before
    /// routing), or it can't be read.
    pub fn of(raw: &str) -> Option<Self> {
        serde_json::from_str::<Carried>(raw).ok()?.route
    }

    /// `raw` with its route replaced by this one (the same message, sent on).
    pub fn replace_in(&self, raw: &str) -> Option<String> {
        let command: Value = serde_json::from_str(raw).ok()?;
        self.attach(command, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_route_travels_beside_its_command_and_a_runner_from_before_reads_the_command() -> anyhow::Result<()> {
        let route = Route { status: Some(super::super::mail::StatusKey { task: None, key: "internal-news".into() }), ..Route::new("mac-1", "agent:a1") };
        let command = json!({"type": "send", "id": "m1", "text": "hello", "mode": "queue"});
        let raw = route.attach(command, Some(&json!({"proof": 1}))).ok_or_else(|| anyhow::anyhow!("not attached"))?;
        anyhow::ensure!(Route::of(&raw) == Some(route.clone()));
        let parsed: super::super::model::Command = serde_json::from_str(&raw)?;
        anyhow::ensure!(super::super::registry::message_id(&parsed).as_deref() == Some("m1"), "{parsed:?}");
        anyhow::ensure!(serde_json::from_str::<Value>(&raw)?.get("relayed").is_some());
        anyhow::ensure!(Route::of(r#"{"type":"send","id":"m1","text":"x"}"#).is_none(), "a command sent as before routing carries none");
        Ok(())
    }

    #[test]
    fn a_route_goes_on_at_most_its_hops() -> anyhow::Result<()> {
        let mut route = Route::new("a", "admin");
        for hop in 1..=HOPS_MAX {
            route = route.onward(format!("r{hop}")).ok_or_else(|| anyhow::anyhow!("hop {hop} refused"))?;
            anyhow::ensure!(route.hops == hop && route.sender == "admin");
        }
        anyhow::ensure!(route.onward("past").is_none(), "past its hops it goes no further");
        let raw = Route::new("a", "admin").attach(json!({"type": "send", "id": "m", "text": "x"}), None).ok_or_else(|| anyhow::anyhow!("not attached"))?;
        let on = Route::new("a", "admin").onward("b").ok_or_else(|| anyhow::anyhow!("no hop"))?;
        let again = on.replace_in(&raw).ok_or_else(|| anyhow::anyhow!("not replaced"))?;
        anyhow::ensure!(Route::of(&again).map(|r| (r.target, r.hops)) == Some(("b".into(), 1)));
        Ok(())
    }
}
