//! One thing to many agents at once (`POST /api/harness/broadcast`), on every machine: a prompt or
//! a slash command, a command as the console sends one (mode, model, effort, interrupt, restart,
//! ...), or an answer to every tool call waiting on you, to the agents picked: by id, by machine,
//! harness and state, or all of them. A slash command is read in each agent's own catalog, as its
//! console would read it: the same words do what that harness does with them, or are skipped,
//! saying why. Each agent's outcome is reported.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model::{Attachment, ChoiceAnswer, Command, Decision, SendMode, Status};
use super::registry::Sent;
use super::session::Info;
use crate::hub::Hub;

/// Which agents. `ids` picks exactly those; otherwise `all`, or the filters (machines, harnesses,
/// states), each one a set to match. With neither nothing is picked: everything is asked for.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct To {
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub ids: Vec<String>,
    /// Machine names; "local" (or "server") is the server's own.
    #[serde(default)]
    pub machines: Vec<String>,
    /// Harnesses: "claude", "codex", ...
    #[serde(default)]
    pub kinds: Vec<String>,
    /// "starting", "idle", "running", "waiting", "exited". Without it: every agent that hasn't
    /// ended (an ended one is picked by id, or by asking for "exited").
    #[serde(default)]
    pub status: Vec<String>,
}

/// What to send. One of `text` (with or without `attachments`, or they alone), `command`,
/// `approvals`.
#[derive(Debug, Clone, Deserialize)]
pub struct Broadcast {
    pub to: To,
    /// A prompt, or "/name args": read in each agent's catalog.
    #[serde(default)]
    pub text: Option<String>,
    /// Files with the prompt (uploaded first: `POST /api/harness/files`), each agent its own copy.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// For a prompt: "queue" (after the current turn, the default), "steer" (into it), "now"
    /// (stopping it).
    #[serde(default)]
    pub mode: Option<SendMode>,
    /// A command as the console sends one: `{"type":"interrupt"}`, `{"type":"set_mode","mode":"plan"}`.
    #[serde(default)]
    pub command: Option<Value>,
    /// Answer every tool call waiting on you: "allow" or "deny" (questions are left to answer).
    #[serde(default)]
    pub approvals: Option<String>,
}

/// One agent's outcome.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Outcome {
    pub id: String,
    pub name: String,
    pub machine: String,
    pub kind: String,
    /// "sent", "skipped" or "failed".
    pub result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub id: String,
    pub sent: usize,
    pub skipped: usize,
    pub failed: usize,
    pub results: Vec<Outcome>,
}

/// The machine an agent runs on, as people name it.
fn machine(target: &str) -> &str {
    target.strip_prefix("agent:").unwrap_or(target)
}

fn status_word(s: Status) -> String {
    serde_json::to_value(s).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

/// The agents `to` picks, in the order listed.
pub fn pick(infos: &[Info], to: &To) -> Vec<Info> {
    if !to.ids.is_empty() {
        return infos.iter().filter(|i| to.ids.contains(&i.id)).cloned().collect();
    }
    let filtered = !(to.machines.is_empty() && to.kinds.is_empty() && to.status.is_empty());
    if !to.all && !filtered {
        return Vec::new();
    }
    let local = |m: &str| m == "local" || m == "server";
    infos
        .iter()
        .filter(|i| to.machines.is_empty() || to.machines.iter().any(|m| m == machine(&i.target) || (local(m) && i.target == "local")))
        .filter(|i| to.kinds.is_empty() || to.kinds.contains(&i.kind))
        .filter(|i| if to.status.is_empty() { i.status != Status::Exited } else { to.status.contains(&status_word(i.status)) })
        .cloned()
        .collect()
}

/// What to send one agent, read for that agent: `Err` says why it is skipped.
fn plan(b: &Broadcast, info: &Info, meta: Option<&Value>, approvals: &[(String, bool)], client: &str) -> Result<Vec<Command>, String> {
    let text = b.text.as_deref().map_or("", str::trim);
    if !text.is_empty() || !b.attachments.is_empty() {
        let send = |text: String| Command::Send { id: client.to_owned(), text, mode: b.mode.unwrap_or_default(), attachments: b.attachments.clone(), overrides: None };
        let Some(rest) = text.strip_prefix('/') else { return Ok(vec![send(text.to_owned())]) };
        let (name, args) = rest.split_once(char::is_whitespace).map_or((rest, ""), |(n, a)| (n, a.trim()));
        let found = meta.and_then(|m| m.get("commands")).and_then(Value::as_array).and_then(|cs| {
            cs.iter().find(|c| c.get("name").and_then(Value::as_str) == Some(name) || c.get("aliases").and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| x.as_str() == Some(name))))
        });
        // Not in its catalog: the words go as they are (its harness may know them, as a skill).
        let Some(route) = found.and_then(|c| c.get("route")) else { return Ok(vec![send(text.to_owned())]) };
        return match route.get("kind").and_then(Value::as_str) {
            Some("control") => {
                let filled = fill(route.get("command").cloned().unwrap_or(Value::Null), args, client);
                serde_json::from_value::<Command>(filled).map(|c| vec![c]).map_err(|e| format!("/{name} can't be sent from here ({e})"))
            }
            Some("view") => Err(format!("/{name} opens {} in its console: nothing to send", route.get("view").and_then(Value::as_str).unwrap_or("a panel"))),
            Some("unsupported") => Err(route.get("reason").and_then(Value::as_str).map_or_else(|| format!("{} can't do /{name} from kiln", info.kind_name), |r| format!("/{name}: {r}"))),
            Some("terminal") => Err(format!("/{name} is done in {}'s own screen: open it from that agent", info.kind_name)),
            _ => {
                let words = route.get("text").and_then(Value::as_str).map_or_else(|| text.to_owned(), |t| t.replace("{args}", args));
                Ok(vec![send(words)])
            }
        };
    }
    if let Some(c) = &b.command {
        let mut c: Command = serde_json::from_value(c.clone()).map_err(|e| format!("not a command kiln knows: {e}"))?;
        if let Command::Send { id, .. } = &mut c {
            *id = client.to_owned();
        }
        return Ok(vec![c]);
    }
    if let Some(answer) = b.approvals.as_deref() {
        let decision = match answer {
            "allow" => Decision::Allow,
            "deny" => Decision::Deny,
            other => return Err(format!("approvals is \"allow\" or \"deny\", not {other:?}")),
        };
        let open: Vec<Command> = approvals
            .iter()
            .filter(|(_, question)| !question)
            .map(|(id, _)| Command::Approve { id: id.clone(), decision, message: None, always: None, answers: None, choices: None::<Vec<ChoiceAnswer>> })
            .collect();
        return if open.is_empty() {
            Err(if approvals.is_empty() { "nothing waiting on you".to_owned() } else { "only questions waiting: answer those in its console".to_owned() })
        } else {
            Ok(open)
        };
    }
    Err("nothing to send: give text, command or approvals".to_owned())
}

/// A catalog command's JSON with what was typed after it, and who asks, filled in.
fn fill(v: Value, args: &str, client: &str) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace("{args}", args).replace("{client}", client)),
        Value::Array(a) => Value::Array(a.into_iter().map(|x| fill(x, args, client)).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, x)| (k, fill(x, args, client))).collect()),
        other => other,
    }
}

/// Send it: every picked agent at once.
pub async fn run(hub: &Arc<Hub>, b: &Broadcast) -> anyhow::Result<Report> {
    let prompt = b.text.as_deref().is_some_and(|t| !t.trim().is_empty()) || !b.attachments.is_empty();
    let given = [prompt, b.command.is_some(), b.approvals.is_some()].into_iter().filter(|x| *x).count();
    anyhow::ensure!(given == 1, "send one of text (or attachments), command or approvals");
    // Each agent gets the files themselves: an upload goes with the first message that sends it.
    let mut b = b.clone();
    b.attachments = hub.harness.inline_uploads(std::mem::take(&mut b.attachments));
    let b = &b;
    let id = format!("bc-{}", crate::util::new_id());
    let picked = pick(&hub.harness.list(), &b.to);
    let sends = picked.iter().enumerate().map(|(n, info)| {
        let client = format!("{id}-{n}");
        async move {
            let meta = hub.harness.meta(&info.id);
            let approvals: Vec<(String, bool)> = hub.harness.state(&info.id).map(|s| s.approvals.iter().map(|a| (a.id.clone(), !a.questions.is_empty())).collect()).unwrap_or_default();
            let outcome = |result: &'static str, reason: Option<String>| Outcome {
                id: info.id.clone(),
                name: info.name.clone(),
                machine: machine(&info.target).to_owned(),
                kind: info.kind.clone(),
                result,
                reason,
            };
            let commands = match plan(b, info, meta.as_ref(), &approvals, &client) {
                Ok(c) => c,
                Err(why) => return outcome("skipped", Some(why)),
            };
            for c in commands {
                match hub.harness.command(hub, &info.id, c).await {
                    Sent::Ok => {}
                    Sent::NoSession => return outcome("failed", Some("it has gone".to_owned())),
                    Sent::Unreachable => return outcome("failed", Some(format!("{} is not connected", machine(&info.target)))),
                }
            }
            outcome("sent", None)
        }
    });
    let results = futures::future::join_all(sends).await;
    let count = |r: &str| results.iter().filter(|o| o.result == r).count();
    Ok(Report { id, sent: count("sent"), skipped: count("skipped"), failed: count("failed"), results })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agent(id: &str, target: &str, kind: &str, status: &str) -> Option<Info> {
        serde_json::from_value(json!({"id": id, "kind": kind, "kind_name": kind, "target": target, "name": id, "cwd": "/w", "created_at": 0, "last_activity": 0, "status": status})).ok()
    }

    fn fleet() -> Vec<Info> {
        [("a1", "agent:adacachy", "claude", "running"), ("a2", "agent:adacachy", "codex", "idle"), ("a3", "agent:mac", "claude", "waiting"), ("a4", "local", "claude", "exited")]
            .into_iter()
            .filter_map(|(i, t, k, s)| agent(i, t, k, s))
            .collect()
    }

    #[test]
    fn agents_are_picked() {
        assert_eq!(fleet().len(), 4, "the fixtures read");
        let ids = |to: To| pick(&fleet(), &to).into_iter().map(|i| i.id).collect::<Vec<_>>();
        assert!(ids(To::default()).is_empty(), "nothing unless asked for");
        assert_eq!(ids(To { all: true, ..To::default() }), ["a1", "a2", "a3"], "all: every agent that hasn't ended");
        assert_eq!(ids(To { kinds: vec!["claude".into()], ..To::default() }), ["a1", "a3"]);
        assert_eq!(ids(To { machines: vec!["adacachy".into()], status: vec!["idle".into()], ..To::default() }), ["a2"]);
        assert_eq!(ids(To { status: vec!["exited".into()], ..To::default() }), ["a4"]);
        assert_eq!(ids(To { machines: vec!["server".into()], status: vec!["exited".into()], ..To::default() }), ["a4"]);
        assert_eq!(ids(To { ids: vec!["a4".into(), "a2".into()], ..To::default() }), ["a2", "a4"], "by id, ended or not");
    }

    #[test]
    fn a_slash_command_is_read_in_each_agents_catalog() {
        let info = agent("a1", "agent:adacachy", "claude", "idle");
        assert!(info.is_some(), "the fixture reads");
        let Some(info) = info else { return };
        let meta = json!({"commands": [
            {"name": "compact", "route": {"kind": "prompt", "text": "/compact {args}"}},
            {"name": "rename", "aliases": ["name"], "route": {"kind": "control", "command": {"type": "control", "id": "rename", "value": "{args}"}}},
            {"name": "model", "route": {"kind": "view", "view": "model"}},
            {"name": "doctor", "route": {"kind": "unsupported", "reason": "a setting of Claude Code's terminal interface"}}
        ]});
        let say = |text: &str| Broadcast { to: To::default(), text: Some(text.into()), attachments: Vec::new(), mode: None, command: None, approvals: None };
        let sent = |text: &str| plan(&say(text), &info, Some(&meta), &[], "bc-1-0");
        assert!(matches!(sent("/compact keep the plan").as_deref(), Ok([Command::Send { text, id, .. }]) if text == "/compact keep the plan" && id == "bc-1-0"));
        assert!(matches!(sent("/name nightly").as_deref(), Ok([Command::Control { .. }])), "an alias, a control");
        assert_eq!(sent("/model").err().as_deref(), Some("/model opens model in its console: nothing to send"));
        assert_eq!(sent("/doctor").err().as_deref(), Some("/doctor: a setting of Claude Code's terminal interface"));
        assert!(matches!(sent("/my-skill go").as_deref(), Ok([Command::Send { text, .. }]) if text == "/my-skill go"), "not in its catalog: as typed");
        assert!(matches!(sent("Run the tests.").as_deref(), Ok([Command::Send { .. }])));
    }

    #[test]
    fn files_go_with_the_prompt_to_each_agent_or_alone() {
        let info = agent("a1", "agent:adacachy", "claude", "idle");
        assert!(info.is_some(), "the fixture reads");
        let Some(info) = info else { return };
        let file = Attachment { name: "shot.png".into(), mime: "image/png".into(), data_base64: "iVBORw0KGgo=".into(), file_id: None, path: None };
        let with = |text: Option<&str>| Broadcast { to: To::default(), text: text.map(str::to_owned), attachments: vec![file.clone()], mode: Some(SendMode::Steer), command: None, approvals: None };
        for (text, said) in [(Some("Look at this"), "Look at this"), (None, "")] {
            let sent = plan(&with(text), &info, None, &[], "bc-1-0");
            assert!(
                matches!(sent.as_deref(), Ok([Command::Send { text, attachments, mode: SendMode::Steer, .. }]) if text == said && attachments.len() == 1 && attachments.first().is_some_and(|a| a.name == "shot.png")),
                "{sent:?}"
            );
        }
    }

    #[test]
    fn waiting_tool_calls_are_answered_but_questions_left() {
        let info = agent("a1", "agent:adacachy", "claude", "waiting");
        assert!(info.is_some(), "the fixture reads");
        let Some(info) = info else { return };
        let b = Broadcast { to: To::default(), text: None, attachments: Vec::new(), mode: None, command: None, approvals: Some("allow".into()) };
        let waiting = [("p1".to_owned(), false), ("q1".to_owned(), true)];
        assert!(matches!(plan(&b, &info, None, &waiting, "c").as_deref(), Ok([Command::Approve { id, decision: Decision::Allow, .. }]) if id == "p1"));
        assert_eq!(plan(&b, &info, None, &waiting[1..], "c").err().as_deref(), Some("only questions waiting: answer those in its console"));
        assert_eq!(plan(&b, &info, None, &[], "c").err().as_deref(), Some("nothing waiting on you"));
        let cmd = Broadcast { to: To::default(), text: None, attachments: Vec::new(), mode: None, command: Some(json!({"type": "interrupt"})), approvals: None };
        assert!(matches!(plan(&cmd, &info, None, &[], "c").as_deref(), Ok([Command::Interrupt])));
    }
}
