//! CP-T3.2: retained messages go through every registered destination's actual wire protocol.

use serde_json::{Value, json};

use super::{Adapter, Kind, Protocol};
use crate::harness::core::Core;
use crate::harness::model::{Attachment, Body, Command, Delivery, SendMode, Status};
use crate::harness::session::Spec;

fn writes(core: &mut Core) -> anyhow::Result<Vec<Value>> {
    core.take_writes().iter().map(|line| serde_json::from_str(line).map_err(anyhow::Error::from)).collect()
}

fn request(writes: &[Value], method: &str) -> anyhow::Result<Value> {
    let mut found = writes.iter().filter(|v| v.get("method").and_then(Value::as_str).or_else(|| v.pointer("/request/subtype").and_then(Value::as_str)) == Some(method));
    let request = found.next().ok_or_else(|| anyhow::anyhow!("no {method} request: {writes:?}"))?;
    anyhow::ensure!(found.next().is_none(), "more than one {method} request: {writes:?}");
    Ok(request.clone())
}

fn line(adapter: &mut Adapter, core: &mut Core, value: Value) {
    adapter.line(core, &value.to_string());
}

fn ready(kind: &Kind, adapter: &mut Adapter, core: &mut Core) -> anyhow::Result<Vec<Value>> {
    let initial = writes(core)?;
    match kind.protocol {
        Protocol::ClaudeStreamJson => {
            let initialize = request(&initial, "initialize")?;
            line(adapter, core, json!({"type": "control_response", "response": {"subtype": "success", "request_id": initialize.get("request_id"), "response": {}}}));
        }
        Protocol::CodexAppServer | Protocol::Acp(_) => {
            let initialize = request(&initial, "initialize")?;
            line(adapter, core, json!({"id": initialize.get("id"), "result": {}}));
            let opening = writes(core)?;
            let (method, result) = if kind.protocol == Protocol::CodexAppServer {
                ("thread/start", json!({"thread": {"id": "destination", "turns": []}}))
            } else {
                ("session/new", json!({"sessionId": "destination"}))
            };
            let opening = request(&opening, method)?;
            line(adapter, core, json!({"id": opening.get("id"), "result": result}));
        }
        Protocol::AgyStreamJson => line(adapter, core, json!({"event": "init", "conversation_id": "destination", "init": {}})),
        // Pi says its session when asked (`get_state`): it takes prompts from then on.
        Protocol::PiRpc => {
            let state = initial.iter().find(|v| v.get("type").and_then(Value::as_str) == Some("get_state")).ok_or_else(|| anyhow::anyhow!("no get_state: {initial:?}"))?;
            line(adapter, core, json!({"id": state.get("id"), "type": "response", "command": "get_state", "success": true, "data": {"sessionId": "destination", "thinkingLevel": "medium", "isStreaming": false, "messageCount": 0}}));
        }
        Protocol::Terminal => anyhow::bail!("{} has no structured message protocol", kind.id),
    }
    writes(core)
}

fn messages(writes: &[Value]) -> Vec<&Value> {
    writes.iter().filter(|v| {
        matches!(v.get("type").and_then(Value::as_str), Some("user" | "prompt"))
            || v.get("event").and_then(Value::as_str) == Some("user")
            || matches!(v.get("method").and_then(Value::as_str), Some("turn/start" | "session/prompt"))
    }).collect()
}

fn finish(kind: &Kind, adapter: &mut Adapter, core: &mut Core, id: &str, sent: &Value) {
    match kind.protocol {
        Protocol::ClaudeStreamJson => {
            line(adapter, core, json!({"type": "command_lifecycle", "command_uuid": id, "state": "started", "uuid": format!("started-{id}"), "session_id": "destination"}));
            line(adapter, core, json!({"type": "result", "subtype": "success", "is_error": false, "result": "done", "stop_reason": "end_turn", "session_id": "destination"}));
            line(adapter, core, json!({"type": "command_lifecycle", "command_uuid": id, "state": "completed", "uuid": format!("completed-{id}"), "session_id": "destination"}));
        }
        Protocol::CodexAppServer => {
            let turn = format!("turn-{id}");
            line(adapter, core, json!({"id": sent.get("id"), "result": {"turn": {"id": turn}}}));
            line(adapter, core, json!({"method": "turn/completed", "params": {"threadId": "destination", "turn": {"id": turn, "status": "completed", "items": []}}}));
        }
        Protocol::Acp(_) => line(adapter, core, json!({"id": sent.get("id"), "result": {"stopReason": "end_turn"}})),
        Protocol::AgyStreamJson => line(adapter, core, json!({"event": "result", "result": {"conversation_id": "destination", "status": "SUCCESS", "response": "done", "num_turns": 1}})),
        Protocol::PiRpc => {
            line(adapter, core, json!({"id": sent.get("id"), "type": "response", "command": "prompt", "success": true, "data": {"disposition": "started"}}));
            line(adapter, core, json!({"type": "agent_start"}));
            line(adapter, core, json!({"type": "agent_settled", "aborted": false}));
        }
        Protocol::Terminal => {}
    }
}

fn attachment(name: &str) -> Attachment {
    Attachment { name: name.into(), mime: "text/plain".into(), data_base64: String::new(), file_id: None, path: Some(format!("/work/{name}")) }
}

fn send(id: &str, text: &str, attachments: Vec<Attachment>, reply_to: Option<&str>) -> Command {
    Command::Send { id: id.into(), text: text.into(), mode: SendMode::Queue, attachments, overrides: None, reply_to: reply_to.map(str::to_owned), task: None }
}

/// CP-T3.2: after a harness ends or its runner loses the process, each destination takes the
/// retained queue in order, honoring Edit and Unsend, with the same attachment and reply.
#[test]
fn cp_t3_2_every_registered_harness_delivers_retained_messages_once() -> anyhow::Result<()> {
    for kind in Kind::all().iter().filter(|kind| kind.structured()) {
        for crash in [false, true] {
            let mut core = Core::new();
            core.state.cwd = "/work".into();
            let source = Kind::get("codex").ok_or_else(|| anyhow::anyhow!("no source harness"))?;
            let mut before = source.adapter(&Spec { kind: source.id.into(), ..Spec::default() });
            before.start(&mut core);
            before.command(&mut core, send("first", "original", vec![attachment("before.txt")], Some("earlier")));
            before.command(&mut core, send("removed", "removed", Vec::new(), None));
            before.command(&mut core, send("second", "second", Vec::new(), None));
            if !crash {
                before.exited(&mut core, None, None);
            }
            let state = serde_json::from_value(serde_json::to_value(&core.state)?)?;
            let mut retained = Core::new();
            retained.restore(core.events().cloned().collect(), state);
            retained.state.exit = None;
            retained.set_status(Status::Starting);
            let mut adapter = kind.adapter(&Spec { kind: kind.id.into(), ..Spec::default() });
            adapter.command(&mut retained, Command::QueueEdit { id: "first".into(), text: "edited".into(), attachments: Some(vec![attachment("edited.txt")]) });
            adapter.command(&mut retained, Command::QueueRemove { id: "removed".into() });
            assert_eq!(retained.state.queue.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(), ["first", "second"], "{}, crash={crash}", kind.id);
            let first = retained.state.queue.first().ok_or_else(|| anyhow::anyhow!("{} lost the first message", kind.id))?;
            assert_eq!((&first.attachments, first.reply_to.as_deref()), (&vec![attachment("edited.txt")], Some("earlier")), "{}", kind.id);
            adapter.start(&mut retained);
            let initial = ready(kind, &mut adapter, &mut retained)?;
            let first = messages(&initial);
            anyhow::ensure!(first.len() == 1, "{}, crash={crash}: initial messages: {initial:?}", kind.id);
            let first = first.first().ok_or_else(|| anyhow::anyhow!("{} sent no first message", kind.id))?;
            let payload = first.to_string();
            anyhow::ensure!(payload.contains("edited") && payload.contains("/work/edited.txt") && !payload.contains("original") && !payload.contains("before.txt") && !payload.contains("removed") && !payload.contains("second"), "{} sent the wrong first payload: {payload}", kind.id);
            if matches!(kind.protocol, Protocol::ClaudeStreamJson | Protocol::CodexAppServer | Protocol::PiRpc) {
                assert_eq!(first.get("uuid").or_else(|| first.pointer("/params/clientUserMessageId")).or_else(|| first.get("id").filter(|_| kind.protocol == Protocol::PiRpc)), Some(&json!("first")), "{}", kind.id);
            }
            assert!(matches!(retained.get("first").map(|e| &e.body), Some(Body::User { text, attachments, reply_to: Some(reply), client_id: Some(id), .. }) if text == "edited" && attachments == &["edited.txt"] && reply == "earlier" && id == "first"), "{} lost message metadata", kind.id);
            assert_eq!(retained.state.queue.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(), ["second"], "{}", kind.id);
            adapter.drain(&mut retained);
            adapter.tick(&mut retained);
            assert!(messages(&writes(&mut retained)?).is_empty(), "{} sent another message during its first turn", kind.id);
            finish(kind, &mut adapter, &mut retained, "first", first);
            let next = writes(&mut retained)?;
            let second = messages(&next);
            anyhow::ensure!(second.len() == 1, "{} didn't send the second message once: {next:?}", kind.id);
            let second = second.first().ok_or_else(|| anyhow::anyhow!("{} sent no second message", kind.id))?;
            anyhow::ensure!(second.to_string().contains("second") && !second.to_string().contains("edited"), "{} sent the wrong second payload: {second}", kind.id);
            finish(kind, &mut adapter, &mut retained, "second", second);
            assert!(retained.state.queue.is_empty(), "{} left messages queued", kind.id);
            for id in ["first", "second"] {
                assert_eq!(retained.state.delivery.get(id), Some(&Delivery::Completed), "{}: {id}", kind.id);
                assert_eq!(retained.events().filter(|e| matches!(&e.body, Body::User { client_id: Some(client), .. } if client == id)).count(), 1, "{}: {id}", kind.id);
            }
            assert!(matches!(retained.state.delivery.get("removed"), None | Some(Delivery::Cancelled)), "{} marked removed input delivered", kind.id);
            assert!(retained.get("removed").is_none(), "{} showed removed startup input as sent", kind.id);
            let mut idle = writes(&mut retained)?;
            for _ in 0..2 {
                adapter.drain(&mut retained);
                adapter.tick(&mut retained);
                idle.extend(writes(&mut retained)?);
            }
            assert!(messages(&idle).is_empty(), "{} sent an extra copy after delivery: {idle:?}", kind.id);
        }
    }
    Ok(())
}
