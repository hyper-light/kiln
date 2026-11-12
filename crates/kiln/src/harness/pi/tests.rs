//! Pi's adapter fed the records Pi's RPC mode sends, as its docs give them (docs/rpc-commands.md,
//! docs/json.md, docs/rpc-extension-ui.md, Pi 1.5.2) and its source writes them
//! (`dist/modes/rpc/rpc-mode.js`).

use serde_json::{Value, json};

use super::*;
use crate::harness::model::{Attachment, Decision};

fn written(core: &mut Core) -> Vec<Value> {
    core.take_writes().iter().filter_map(|w| serde_json::from_str(w).ok()).collect()
}

fn of_type<'a>(w: &'a [Value], kind: &str) -> Vec<&'a Value> {
    w.iter().filter(|v| v.get("type").and_then(Value::as_str) == Some(kind)).collect()
}

fn feed(a: &mut Pi, core: &mut Core, lines: &[Value]) {
    for l in lines {
        a.line(core, &l.to_string());
    }
}

fn ok(id: &Value, command: &str, data: Value) -> Value {
    json!({"id": id, "type": "response", "command": command, "success": true, "data": data})
}

const SESSION: &str = "01a1d2c3-pi-session";

fn model(provider: &str, id: &str, name: &str, reasoning: bool, map: Value) -> Value {
    json!({"id": id, "name": name, "api": "anthropic-messages", "provider": provider, "baseUrl": "https://api.anthropic.com", "reasoning": reasoning, "thinkingLevelMap": map, "input": ["text", "image"], "contextWindow": 200000, "maxTokens": 16384, "cost": {"input": 3.0, "output": 15.0, "cacheRead": 0.3, "cacheWrite": 3.75}})
}

/// Started and answered `get_state`: it takes prompts.
fn started(spec: &Spec) -> (Pi, Core) {
    let (a, core, _) = started_asking(spec);
    (a, core)
}

/// The same, with what it asked as it started.
fn started_asking(spec: &Spec) -> (Pi, Core, Vec<Value>) {
    let mut core = Core::new();
    core.state.cwd = "/w".into();
    let mut a = Pi::new(spec);
    a.start(&mut core);
    let w = written(&mut core);
    let state = of_type(&w, "get_state").first().and_then(|v| v.get("id")).cloned().unwrap_or(Value::Null);
    let data = json!({"model": model("anthropic", "claude-sonnet-4-20250514", "Claude Sonnet 4", true, json!({})), "thinkingLevel": "medium", "isStreaming": false, "isCompacting": false, "steeringMode": "one-at-a-time", "followUpMode": "one-at-a-time", "sessionFile": "/home/test/.pi/agent/sessions/w/s.jsonl", "sessionId": SESSION, "autoCompactionEnabled": true, "messageCount": 0, "pendingMessageCount": 0});
    feed(&mut a, &mut core, &[ok(&state, "get_state", data)]);
    let _ = written(&mut core);
    (a, core, w)
}

fn send(a: &mut Pi, core: &mut Core, id: &str, text: &str, mode: SendMode) {
    a.command(core, Command::Send { id: id.into(), text: text.into(), mode, attachments: vec![], overrides: None, reply_to: None, task: None });
}

#[test]
fn its_flags_follow_the_session() {
    let flag = |a: &[String], f: &str| a.windows(2).find(|w| w.first().map(String::as_str) == Some(f)).and_then(|w| w.get(1)).cloned();
    let spec = Spec { resume: Some(SESSION.into()), model: Some("anthropic/claude-opus-5-5".into()), effort: Some("xhigh".into()), mode: Some(FULL.into()), allow_bypass: true, ..Spec::default() };
    let a = args(&spec);
    assert!(a.starts_with(&["--mode", "rpc"].map(str::to_owned)), "{a:?}");
    assert_eq!(flag(&a, "--session-id").as_deref(), Some(SESSION));
    assert_eq!(flag(&a, "--model").as_deref(), Some("anthropic/claude-opus-5-5"));
    assert_eq!(flag(&a, "--thinking").as_deref(), Some("xhigh"));
    // Full access: Pi's own tools, nothing narrowed.
    assert_eq!(flag(&a, "--tools"), None);
    // A new session's: it edits files, runs no commands, and no MCP tool; a new session id each time.
    let new = args(&Spec::default());
    assert_eq!(flag(&new, "--tools").as_deref(), Some("read,grep,find,ls,edit,write"));
    assert_eq!(flag(&new, "--exclude-tools").as_deref(), Some("mcp__*"));
    let id = flag(&new, "--session-id").unwrap_or_default();
    assert!(session_id(&id) && id != SESSION, "{id}");
    assert_ne!(flag(&args(&Spec::default()), "--session-id"), Some(id));
    assert_eq!(flag(&args(&Spec { mode: Some(READ_ONLY.into()), ..Spec::default() }), "--tools").as_deref(), Some("read,grep,find,ls"));
    // Full access it may not use: it edits files and runs no commands.
    assert_eq!(flag(&args(&Spec { mode: Some(FULL.into()), allow_bypass: false, ..Spec::default() }), "--tools").as_deref(), Some("read,grep,find,ls,edit,write"));
    // A level Pi doesn't have, and a session id it wouldn't take, are left out.
    let odd = args(&Spec { effort: Some("ultra".into()), resume: Some("../not an id".into()), ..Spec::default() });
    assert_eq!(flag(&odd, "--thinking"), None);
    assert_ne!(flag(&odd, "--session-id").as_deref(), Some("../not an id"));
}

#[test]
fn it_says_its_session_then_a_message_goes_as_a_prompt_under_its_own_id() {
    let mut core = Core::new();
    let mut a = Pi::new(&Spec::default());
    a.start(&mut core);
    let w = written(&mut core);
    for kind in ["get_state", "get_available_models", "get_commands"] {
        assert_eq!(of_type(&w, kind).len(), 1, "{kind}: {w:?}");
    }
    // Before it says its session, a message waits in kiln's queue.
    send(&mut a, &mut core, "m0", "early", SendMode::Queue);
    assert_eq!(core.state.queue.len(), 1);
    assert!(of_type(&written(&mut core), "prompt").is_empty());
    let state = of_type(&w, "get_state").first().and_then(|v| v.get("id")).cloned().unwrap_or(Value::Null);
    feed(&mut a, &mut core, &[ok(&state, "get_state", json!({"model": model("anthropic", "claude-sonnet-4-20250514", "Claude Sonnet 4", true, json!({})), "thinkingLevel": "high", "isStreaming": false, "sessionId": SESSION, "messageCount": 0}))]);
    assert_eq!(core.state.harness_session.as_deref(), Some(SESSION));
    assert_eq!((core.state.model.as_str(), core.state.effort.as_str()), ("anthropic/claude-sonnet-4-20250514", "high"));
    let w = written(&mut core);
    let prompt = of_type(&w, "prompt");
    assert_eq!(prompt.len(), 1, "{w:?}");
    assert_eq!(prompt.first().and_then(|p| p.get("id")), Some(&json!("m0")));
    assert_eq!(prompt.first().and_then(|p| p.get("message")), Some(&json!("early")));
    assert_eq!(of_type(&w, "get_available_thinking_levels").len(), 1);
    assert_eq!(core.state.delivery.get("m0"), Some(&Delivery::Started));
    assert_eq!(core.state.status, Status::Running);
    assert!(core.state.queue.is_empty());
}

#[test]
fn a_turn_streams_runs_a_tool_and_settles() {
    let (mut a, mut core) = started(&Spec::default());
    send(&mut a, &mut core, "m1", "List the files", SendMode::Queue);
    let _ = written(&mut core);
    let usage = json!({"input": 100, "output": 20, "cacheRead": 50, "cacheWrite": 0, "totalTokens": 170, "cost": {"input": 0.0003, "output": 0.0003, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0006}});
    feed(
        &mut a,
        &mut core,
        &[
            ok(&json!("m1"), "prompt", json!({"disposition": "started"})),
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
            json!({"type": "message_start", "message": {"role": "user", "content": "List the files", "timestamp": 1733234401000_u64}}),
            json!({"type": "message_end", "message": {"role": "user", "content": "List the files", "timestamp": 1733234401000_u64}}),
            json!({"type": "message_start", "message": {"role": "assistant", "content": [], "stopReason": "pending"}}),
            json!({"type": "message_update", "usage": usage, "assistantMessageEvent": {"type": "thinking_start", "contentIndex": 0}}),
            json!({"type": "message_update", "usage": usage, "assistantMessageEvent": {"type": "thinking_delta", "contentIndex": 0, "delta": "A listing."}}),
            json!({"type": "message_update", "usage": usage, "assistantMessageEvent": {"type": "text_delta", "contentIndex": 1, "delta": "Let me "}}),
            json!({"type": "message_update", "usage": usage, "assistantMessageEvent": {"type": "text_delta", "contentIndex": 1, "delta": "look."}}),
            json!({"type": "message_update", "usage": usage, "assistantMessageEvent": {"type": "toolcall_end", "contentIndex": 2, "toolCall": {"type": "toolCall", "id": "call_abc123", "name": "bash", "arguments": {"command": "ls -la"}}}}),
            json!({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "thinking", "thinking": "A listing."}, {"type": "text", "text": "Let me look."}, {"type": "toolCall", "id": "call_abc123", "name": "bash", "arguments": {"command": "ls -la"}}], "provider": "anthropic", "model": "claude-sonnet-4-20250514", "usage": usage, "stopReason": "toolUse"}}),
            json!({"type": "tool_execution_start", "toolCallId": "call_abc123", "toolName": "bash", "args": {"command": "ls -la"}}),
            json!({"type": "tool_execution_update", "toolCallId": "call_abc123", "toolName": "bash", "args": {"command": "ls -la"}, "partialResult": {"content": [{"type": "text", "text": "partial output"}], "details": {}}}),
            json!({"type": "tool_execution_end", "toolCallId": "call_abc123", "toolName": "bash", "result": {"content": [{"type": "text", "text": "complete output"}], "details": {}}, "isError": false, "durationMs": 12}),
            json!({"type": "turn_end", "message": {"role": "assistant"}, "toolResults": []}),
            json!({"type": "message_start", "message": {"role": "assistant", "content": [], "stopReason": "pending"}}),
            json!({"type": "message_update", "usage": usage, "assistantMessageEvent": {"type": "text_delta", "contentIndex": 0, "delta": "Two files."}}),
            json!({"type": "message_update", "usage": usage, "assistantMessageEvent": {"type": "text_end", "contentIndex": 0, "content": "Two files."}}),
            json!({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "text", "text": "Two files."}], "usage": usage, "stopReason": "stop"}}),
            json!({"type": "agent_end", "messages": [], "willRetry": false}),
        ],
    );
    assert_eq!(core.state.status, Status::Running, "agent_end isn't the end: retries and steering can follow");
    assert!(core.events().any(|e| matches!(&e.body, Body::Thinking { text, streaming: false, .. } if text == "A listing.")));
    assert!(core.events().any(|e| matches!(&e.body, Body::Assistant { text, streaming: false } if text == "Let me look.")));
    assert!(core.events().any(|e| matches!(&e.body, Body::Tool(t) if t.tool == ToolKind::Shell && t.title == "ls -la" && t.status == ToolStatus::Done && t.output.as_deref() == Some("complete output"))));
    feed(&mut a, &mut core, &[json!({"type": "agent_settled", "aborted": false})]);
    assert!(core.events().any(|e| matches!(&e.body, Body::Assistant { text, .. } if text == "Two files.")));
    assert_eq!(core.state.summary, "Two files.");
    assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Completed));
    assert_eq!((core.state.usage.input_tokens, core.state.usage.output_tokens, core.state.usage.cache_read_tokens, core.state.usage.turns), (200, 40, 100, 1));
    assert!((core.state.usage.cost_usd - 0.0012).abs() < 1e-9);
    assert_eq!(core.state.status, Status::Idle);
    // How full its context is, asked as the turn ends.
    let w = written(&mut core);
    let stats = of_type(&w, "get_session_stats");
    assert_eq!(stats.len(), 1, "{w:?}");
    let id = stats.first().and_then(|s| s.get("id")).cloned().unwrap_or(Value::Null);
    feed(&mut a, &mut core, &[ok(&id, "get_session_stats", json!({"sessionId": SESSION, "tokens": {"input": 200, "output": 40, "cacheRead": 100, "cacheWrite": 0, "total": 340}, "cost": 0.0012, "contextUsage": {"tokens": 60000, "contextWindow": 200000, "percent": 30}}))]);
    assert_eq!((core.state.usage.context_tokens, core.state.usage.context_window), (Some(60000), Some(200000)));
}

#[test]
fn a_message_during_a_turn_is_steered_in_and_a_stop_takes_back_what_pi_hadnt_taken() {
    let (mut a, mut core) = started(&Spec::default());
    send(&mut a, &mut core, "m1", "Refactor it", SendMode::Queue);
    feed(&mut a, &mut core, &[ok(&json!("m1"), "prompt", json!({"disposition": "started"})), json!({"type": "agent_start"})]);
    let _ = written(&mut core);
    // Steered in: Pi takes it before its next model call.
    send(&mut a, &mut core, "s1", "Use the new API", SendMode::Steer);
    send(&mut a, &mut core, "s2", "And keep the tests", SendMode::Steer);
    let w = written(&mut core);
    let steer = of_type(&w, "steer");
    assert_eq!(steer.iter().map(|s| (s.get("id").cloned(), s.get("message").cloned())).collect::<Vec<_>>(), [(Some(json!("s1")), Some(json!("Use the new API"))), (Some(json!("s2")), Some(json!("And keep the tests")))]);
    assert_eq!(core.state.delivery.get("s1"), Some(&Delivery::Sent));
    feed(&mut a, &mut core, &[ok(&json!("s1"), "steer", json!({"disposition": "queued"})), ok(&json!("s2"), "steer", json!({"disposition": "queued"})), json!({"type": "queue_update", "steering": ["And keep the tests"], "followUp": []})]);
    // A stop: what Pi hasn't taken comes back first, then the run is aborted.
    a.command(&mut core, Command::Interrupt);
    let w = written(&mut core);
    let kinds: Vec<&str> = w.iter().filter_map(|v| v.get("type").and_then(Value::as_str)).collect();
    assert_eq!(kinds, ["clear_queue", "abort"]);
    let clear = w.first().and_then(|v| v.get("id")).cloned().unwrap_or(Value::Null);
    let abort = w.get(1).and_then(|v| v.get("id")).cloned().unwrap_or(Value::Null);
    feed(&mut a, &mut core, &[ok(&clear, "clear_queue", json!({"steering": ["And keep the tests"], "followUp": []}))]);
    assert_eq!(core.state.queue.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(), ["s2"]);
    assert_eq!(core.state.delivery.get("s2"), Some(&Delivery::Queued));
    feed(&mut a, &mut core, &[json!({"type": "agent_settled", "aborted": true}), json!({"id": abort, "type": "response", "command": "abort", "success": true})]);
    assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Cancelled));
    assert_eq!(core.state.delivery.get("s1"), Some(&Delivery::Cancelled), "taken before the stop: it went with the turn");
    assert!(core.events().any(|e| matches!(&e.body, Body::Notice { text, .. } if text == "Interrupted")));
    // What came back goes as the next turn.
    let w = written(&mut core);
    let prompt = of_type(&w, "prompt");
    assert_eq!(prompt.first().and_then(|p| p.get("id")), Some(&json!("s2")), "{w:?}");
    assert_eq!(core.state.delivery.get("s2"), Some(&Delivery::Started));
}

#[test]
fn send_now_stops_the_turn_and_goes_first() {
    let (mut a, mut core) = started(&Spec::default());
    send(&mut a, &mut core, "m1", "Long task", SendMode::Queue);
    send(&mut a, &mut core, "m2", "Later", SendMode::Queue);
    feed(&mut a, &mut core, &[ok(&json!("m1"), "prompt", json!({"disposition": "started"})), json!({"type": "agent_start"})]);
    let _ = written(&mut core);
    send(&mut a, &mut core, "now", "Stop and do this", SendMode::Now);
    assert_eq!(core.state.queue.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(), ["now", "m2"]);
    assert_eq!(of_type(&written(&mut core), "abort").len(), 1);
    feed(&mut a, &mut core, &[json!({"type": "agent_settled", "aborted": true})]);
    let w = written(&mut core);
    assert_eq!(of_type(&w, "prompt").first().and_then(|p| p.get("message")), Some(&json!("Stop and do this")));
}

#[test]
fn a_refused_prompt_is_said_and_the_next_one_goes() {
    let (mut a, mut core) = started(&Spec::default());
    send(&mut a, &mut core, "m1", "Hello", SendMode::Queue);
    send(&mut a, &mut core, "m2", "Again", SendMode::Queue);
    let _ = written(&mut core);
    feed(&mut a, &mut core, &[json!({"id": "m1", "type": "response", "command": "prompt", "success": false, "error": "No API key found for anthropic.\n\nUse /login or set an API key environment variable."})]);
    assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Cancelled));
    assert!(core.events().any(|e| matches!(&e.body, Body::Notice { level: Level::Error, text } if text.contains("No API key found"))));
    assert_eq!(of_type(&written(&mut core), "prompt").first().and_then(|p| p.get("id")), Some(&json!("m2")));
}

#[test]
fn an_extensions_command_that_takes_the_prompt_ends_the_turn_at_once() {
    let (mut a, mut core) = started(&Spec::default());
    send(&mut a, &mut core, "m1", "/mycommand", SendMode::Queue);
    let _ = written(&mut core);
    feed(&mut a, &mut core, &[ok(&json!("m1"), "prompt", json!({"disposition": "handled"}))]);
    assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Completed));
    assert_eq!(core.state.status, Status::Idle);
}

#[test]
fn a_usage_limit_pauses_the_session() {
    let (mut a, mut core) = started(&Spec::default());
    send(&mut a, &mut core, "m1", "Go", SendMode::Queue);
    feed(
        &mut a,
        &mut core,
        &[
            ok(&json!("m1"), "prompt", json!({"disposition": "started"})),
            json!({"type": "agent_start"}),
            json!({"type": "message_start", "message": {"role": "assistant", "content": []}}),
            json!({"type": "message_end", "message": {"role": "assistant", "content": [], "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0, "cost": {"total": 0}}, "stopReason": "error", "errorMessage": "429 You've hit your usage limit. Your limit resets at 5pm."}}),
            json!({"type": "agent_settled", "aborted": false}),
        ],
    );
    assert!(core.state.pause.is_some(), "a usage limit pauses it");
    assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Completed));
}

#[test]
fn its_models_and_levels_are_pis_and_a_new_model_goes_over_rpc() {
    let (mut a, mut core, asked) = started_asking(&Spec::default());
    let models = json!({"models": [
        model("anthropic", "claude-sonnet-4-20250514", "Claude Sonnet 4", true, json!({})),
        model("openai", "gpt-5.6", "GPT-5.6", true, json!({"xhigh": "xhigh", "max": "max", "minimal": null})),
        model("openrouter", "meta/llama-4", "Llama 4", false, json!({})),
    ]});
    let id = of_type(&asked, "get_available_models").first().and_then(|v| v.get("id")).cloned().unwrap_or(Value::Null);
    feed(&mut a, &mut core, &[ok(&id, "get_available_models", models)]);
    let efforts = |id: &str| core.meta.models.iter().find(|m| m.id == id).map(|m| m.efforts.clone()).unwrap_or_default();
    assert_eq!(efforts("anthropic/claude-sonnet-4-20250514"), ["off", "minimal", "low", "medium", "high"]);
    assert_eq!(efforts("openai/gpt-5.6"), ["off", "low", "medium", "high", "xhigh", "max"]);
    assert_eq!(efforts("openrouter/meta/llama-4"), ["off"]);
    assert!(core.meta.models.iter().any(|m| m.id == "anthropic/claude-sonnet-4-20250514" && m.default));
    // A model whose id has a slash: its provider is what comes before the first.
    a.command(&mut core, Command::SetModel { model: "openrouter/meta/llama-4".into(), kind: None });
    let w = written(&mut core);
    let set = of_type(&w, "set_model");
    assert_eq!(set.first().map(|s| (s.get("provider").cloned(), s.get("modelId").cloned())), Some((Some(json!("openrouter")), Some(json!("meta/llama-4")))));
    let id = set.first().and_then(|s| s.get("id")).cloned().unwrap_or(Value::Null);
    feed(&mut a, &mut core, &[ok(&id, "set_model", model("openrouter", "meta/llama-4", "Llama 4", false, json!({})))]);
    assert_eq!(core.state.model, "openrouter/meta/llama-4");
    // Its levels are the model's.
    let w = written(&mut core);
    let levels = of_type(&w, "get_available_thinking_levels").first().and_then(|v| v.get("id")).cloned().unwrap_or(Value::Null);
    feed(&mut a, &mut core, &[ok(&levels, "get_available_thinking_levels", json!({"levels": ["off"]}))]);
    assert_eq!(core.meta.efforts.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["off"]);
    a.command(&mut core, Command::SetEffort { effort: "high".into() });
    assert!(of_type(&written(&mut core), "set_thinking_level").is_empty(), "a level its model doesn't take isn't asked for");
    feed(&mut a, &mut core, &[json!({"type": "thinking_level_changed", "level": "off"})]);
    assert_eq!(core.state.effort, "off");
}

#[test]
fn a_new_mode_starts_it_again_on_the_same_session_once_the_turn_ends() {
    let (mut a, mut core) = started(&Spec { allow_bypass: false, ..Spec::default() });
    assert_eq!(core.state.mode, EDIT);
    send(&mut a, &mut core, "m1", "Work", SendMode::Queue);
    feed(&mut a, &mut core, &[ok(&json!("m1"), "prompt", json!({"disposition": "started"})), json!({"type": "agent_start"})]);
    let _ = written(&mut core);
    core.effects.clear();
    a.command(&mut core, Command::SetMode { mode: FULL.into() });
    assert_eq!(core.state.mode, FULL);
    assert!(!core.effects.iter().any(|e| matches!(e, Effect::Restart { .. })), "not while a turn runs");
    assert!(core.events().any(|e| matches!(&e.body, Body::Notice { text, .. } if text.contains("from the next turn"))));
    // Meanwhile what's sent waits for the new process.
    feed(&mut a, &mut core, &[json!({"type": "agent_settled", "aborted": false})]);
    assert!(core.effects.iter().any(|e| matches!(e, Effect::AllowBypass)), "Full access needs the session's permission to skip checks");
    assert!(core.effects.iter().any(|e| matches!(e, Effect::Restart { resume_at: None })));
    send(&mut a, &mut core, "m2", "Next", SendMode::Queue);
    assert!(of_type(&written(&mut core), "prompt").is_empty());
    assert_eq!(core.state.queue.len(), 1);
}

#[test]
fn an_extensions_dialogs_are_a_persons_to_answer() {
    let (mut a, mut core) = started(&Spec::default());
    feed(
        &mut a,
        &mut core,
        &[
            json!({"type": "extension_ui_request", "id": "uuid-1", "method": "select", "title": "Allow dangerous command?", "options": ["Allow", "Block"]}),
            json!({"type": "extension_ui_request", "id": "uuid-2", "method": "confirm", "title": "Clear session?", "message": "All messages will be lost."}),
            json!({"type": "extension_ui_request", "id": "uuid-3", "method": "input", "title": "Enter a value", "placeholder": "type something..."}),
            json!({"type": "extension_ui_request", "id": "uuid-5", "method": "notify", "message": "Command blocked by user", "notifyType": "warning"}),
            json!({"type": "extension_ui_request", "id": "uuid-6", "method": "setStatus", "statusKey": "my-ext", "statusText": "Turn 3 running..."}),
        ],
    );
    assert_eq!(core.state.approvals.len(), 3);
    let select = core.state.approvals.iter().find(|x| x.id == "pi-ui-uuid-1").map(|x| x.questions.iter().flat_map(|q| q.options.iter().map(|o| o.label.clone())).collect::<Vec<_>>());
    assert_eq!(select, Some(vec!["Allow".to_owned(), "Block".to_owned()]));
    assert!(core.events().any(|e| matches!(&e.body, Body::Notice { level: Level::Warn, text } if text == "Command blocked by user")));
    let choose = |label: Option<&str>, other: Option<&str>| Some(vec![ChoiceAnswer { question: String::new(), id: None, selected: label.map(|l| vec![l.to_owned()]).unwrap_or_default(), other: other.map(str::to_owned), note: None }]);
    a.command(&mut core, Command::Approve { id: "pi-ui-uuid-1".into(), decision: Decision::Allow, message: None, always: None, answers: None, choices: choose(Some("Block"), None) });
    a.command(&mut core, Command::Approve { id: "pi-ui-uuid-2".into(), decision: Decision::Deny, message: None, always: None, answers: None, choices: None });
    a.command(&mut core, Command::Approve { id: "pi-ui-uuid-3".into(), decision: Decision::Allow, message: None, always: None, answers: None, choices: choose(None, Some("forty-two")) });
    let w = written(&mut core);
    assert_eq!(w, [
        json!({"type": "extension_ui_response", "id": "uuid-1", "value": "Block"}),
        json!({"type": "extension_ui_response", "id": "uuid-2", "confirmed": false}),
        json!({"type": "extension_ui_response", "id": "uuid-3", "value": "forty-two"}),
    ]);
    assert!(core.state.approvals.is_empty());
    // Dismissed, an editor's is cancelled.
    feed(&mut a, &mut core, &[json!({"type": "extension_ui_request", "id": "uuid-4", "method": "editor", "title": "Edit some text", "prefill": "Line 1"})]);
    a.command(&mut core, Command::Approve { id: "pi-ui-uuid-4".into(), decision: Decision::Deny, message: None, always: None, answers: None, choices: None });
    assert_eq!(written(&mut core), [json!({"type": "extension_ui_response", "id": "uuid-4", "cancelled": true})]);
}

#[test]
fn a_compaction_runs_between_turns_and_is_marked_where_it_ran() {
    let (mut a, mut core) = started(&Spec::default());
    send(&mut a, &mut core, "m1", "Work", SendMode::Queue);
    feed(&mut a, &mut core, &[ok(&json!("m1"), "prompt", json!({"disposition": "started"})), json!({"type": "agent_start"})]);
    let _ = written(&mut core);
    a.command(&mut core, Command::Control { id: "compact".into(), value: Some(json!({"text": "Focus on code changes"})) });
    assert!(of_type(&written(&mut core), "compact").is_empty(), "not while the turn runs");
    send(&mut a, &mut core, "m2", "Then this", SendMode::Queue);
    feed(&mut a, &mut core, &[json!({"type": "agent_settled", "aborted": false})]);
    let w = written(&mut core);
    let compact = of_type(&w, "compact");
    assert_eq!(compact.first().and_then(|c| c.get("customInstructions")), Some(&json!("Focus on code changes")));
    assert!(of_type(&w, "prompt").is_empty(), "the queue waits for the compaction");
    let id = compact.first().and_then(|c| c.get("id")).cloned().unwrap_or(Value::Null);
    let result = json!({"summary": "Summary of conversation...", "firstKeptEntryId": "abc123", "tokensBefore": 150000, "estimatedTokensAfter": 32000, "details": {}});
    feed(&mut a, &mut core, &[json!({"type": "compaction_start", "reason": "manual"}), json!({"type": "compaction_end", "reason": "manual", "result": result, "aborted": false, "willRetry": false}), ok(&id, "compact", result.clone())]);
    assert!(core.state.compacting.is_none());
    assert!(core.events().any(|e| e.id.starts_with("compaction:") && matches!(&e.body, Body::Notice { level: Level::Info, .. })));
    assert_eq!(of_type(&written(&mut core), "prompt").first().and_then(|p| p.get("id")), Some(&json!("m2")));
}

#[test]
fn the_conversation_carried_over_goes_with_the_first_prompt_and_files_by_their_paths() {
    let (mut a, mut core) = started(&Spec::default());
    a.offer_context(Some("The conversation so far: we picked Postgres.".into()));
    assert!(!a.seeded());
    let image = Attachment { name: "shot.png".into(), mime: "image/png".into(), data_base64: "aW1n".into(), file_id: None, path: Some("/w/shot.png".into()) };
    let notes = Attachment { name: "notes.txt".into(), mime: "text/plain".into(), data_base64: String::new(), file_id: None, path: Some("/w/notes.txt".into()) };
    a.command(&mut core, Command::Send { id: "m1".into(), text: "Go on".into(), mode: SendMode::Queue, attachments: vec![image, notes], overrides: None, reply_to: None, task: None });
    assert!(a.seeded());
    let w = written(&mut core);
    let p = of_type(&w, "prompt");
    let message = p.first().and_then(|p| p.get("message")).and_then(Value::as_str).unwrap_or_default();
    assert!(message.starts_with("The conversation so far: we picked Postgres.\n\nGo on") && message.ends_with("[Attached: /w/notes.txt]"), "{message}");
    assert_eq!(p.first().and_then(|p| p.pointer("/images/0")), Some(&json!({"type": "image", "data": "aW1n", "mimeType": "image/png"})));
}

#[test]
fn a_session_it_couldnt_find_is_said_new() {
    let mut core = Core::new();
    core.upsert("old", None, Body::User { text: "Earlier".into(), images: 0, client_id: None, origin: None, attachments: vec![], reply_to: None });
    let mut a = Pi::new(&Spec { resume: Some(SESSION.into()), ..Spec::default() });
    a.start(&mut core);
    let w = written(&mut core);
    let state = of_type(&w, "get_state").first().and_then(|v| v.get("id")).cloned().unwrap_or(Value::Null);
    feed(&mut a, &mut core, &[ok(&state, "get_state", json!({"sessionId": SESSION, "thinkingLevel": "medium", "messageCount": 0}))]);
    assert!(core.events().any(|e| matches!(&e.body, Body::Notice { text, .. } if text.contains("couldn't find its session"))));
}

#[test]
fn it_is_written_out_and_read_back_whole() -> anyhow::Result<()> {
    let (mut a, mut core) = started(&Spec::default());
    send(&mut a, &mut core, "m1", "Work", SendMode::Queue);
    feed(&mut a, &mut core, &[json!({"type": "extension_ui_request", "id": "u1", "method": "confirm", "title": "Proceed?", "message": "It writes files."})]);
    let out = serde_json::to_value(&a)?;
    let back: Pi = serde_json::from_value(out.clone())?;
    assert_eq!(serde_json::to_value(&back)?, out);
    assert_eq!(back.retained_input_ids().iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), ["m1"]);
    Ok(())
}
