use super::*;
use crate::harness::session::Spec;

fn written(core: &mut Core) -> Vec<Value> {
    core.take_writes().iter().filter_map(|w| serde_json::from_str(w).ok()).collect()
}

fn request(core: &mut Core, method: &str) -> anyhow::Result<Value> {
    let writes = written(core);
    anyhow::ensure!(writes.len() == 1, "expected only {method}: {writes:?}");
    writes.into_iter().find(|w| w.get("method").and_then(Value::as_str) == Some(method)).ok_or_else(|| anyhow::anyhow!("no {method} request"))
}

fn reply(a: &mut Acp, core: &mut Core, request: &Value, result: Value) {
    a.line(core, &json!({"jsonrpc": "2.0", "id": request.get("id"), "result": result}).to_string());
}

fn update(a: &mut Acp, core: &mut Core, options: Vec<Value>) {
    a.line(core, &json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "config_option_update", "configOptions": options}}}).to_string());
}

#[test]
fn a_session_keeps_effort_choices_learned_after_it_started() -> anyhow::Result<()> {
    let config = vec![
        json!({"id": "model", "category": "model", "type": "select", "currentValue": "catalog-current", "options": [{"value": "catalog-current", "name": "Current"}, {"value": "catalog-later", "name": "Later"}]}),
        json!({"id": "effort", "category": "thought_level", "type": "select", "currentValue": "high", "options": [{"value": "high", "name": "High"}]}),
    ];
    let mut a = Acp::from_spec(&Spec { kind: "devin".into(), ..Spec::default() });
    let mut core = Core::new();
    let new = initialize(&mut a, &mut core, false)?;
    reply(&mut a, &mut core, &new, json!({"sessionId": "s1", "configOptions": config}));
    assert!(core.meta.models.iter().find(|m| m.id == "catalog-later").is_some_and(|m| m.efforts.is_empty()));
    super::super::kinds::note_models("devin", &[
        ModelOption { id: "catalog-current".into(), efforts: vec!["obsolete".into()], ..ModelOption::default() },
        ModelOption { id: "catalog-later".into(), efforts: vec!["max".into()], ..ModelOption::default() },
    ]);
    update(&mut a, &mut core, config);
    assert_eq!(core.meta.models.iter().find(|m| m.id == "catalog-later").map(|m| m.efforts.as_slice()), Some(["max".to_owned()].as_slice()));
    assert_eq!(core.meta.models.iter().find(|m| m.id == "catalog-current").map(|m| m.efforts.as_slice()), Some(["high".to_owned()].as_slice()));
    Ok(())
}

fn send(a: &mut Acp, core: &mut Core, id: &str) {
    a.command(core, Command::Send { id: id.into(), text: format!("work {id}"), mode: SendMode::Queue, attachments: Vec::new(), overrides: None });
}

fn options(mode: &str, model: &str, effort: &str) -> Vec<Value> {
    let efforts = if model == "careful" { json!([{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]) } else { json!([{"value": "low", "name": "Low"}]) };
    vec![
        json!({"id": "permissions", "name": "Mode", "category": "mode", "type": "select", "currentValue": mode, "options": [{"value": "accept-edits", "name": "Code"}, {"value": "ask", "name": "Ask"}, {"value": "plan", "name": "Plan"}, {"value": "bypass", "name": "Bypass"}]}),
        json!({"id": "engine", "name": "Model", "category": "model", "type": "select", "currentValue": model, "options": [{"value": "default", "name": "Default"}, {"value": "careful", "name": "Careful"}]}),
        json!({"id": "thinking", "name": "Thinking", "category": "thought_level", "type": "select", "currentValue": effort, "options": efforts}),
    ]
}

fn initialize(a: &mut Acp, core: &mut Core, load: bool) -> anyhow::Result<Value> {
    a.start(core);
    let init = request(core, "initialize")?;
    reply(a, core, &init, json!({"protocolVersion": 1, "agentCapabilities": {"loadSession": load}}));
    request(core, if load { "session/load" } else { "session/new" })
}

fn opened(config: Vec<Value>) -> anyhow::Result<(Acp, Core)> {
    let mut a = Acp::from_spec(&Spec { kind: "devin".into(), ..Spec::default() });
    let mut core = Core::new();
    core.state.cwd = "/work".into();
    let session = initialize(&mut a, &mut core, false)?;
    reply(&mut a, &mut core, &session, json!({"sessionId": "s1", "configOptions": config}));
    anyhow::ensure!(written(&mut core).is_empty(), "an empty startup sent a request");
    Ok((a, core))
}

#[test]
fn model_changes_keep_each_models_own_efforts_and_the_original_default() -> anyhow::Result<()> {
    let (mut a, mut core) = opened(options("accept-edits", "default", "low"))?;
    a.command(&mut core, Command::SetModel { model: "careful".into(), kind: None });
    let setting = request(&mut core, "session/set_config_option")?;
    reply(&mut a, &mut core, &setting, json!({"configOptions": options("accept-edits", "careful", "high")}));
    assert_eq!(core.meta.models.iter().map(|m| (m.id.as_str(), m.efforts.clone(), m.default)).collect::<Vec<_>>(), [
        ("default", vec!["low".to_owned()], true),
        ("careful", vec!["low".to_owned(), "high".to_owned()], false),
    ]);
    let frozen = serde_json::to_value(&a)?;
    a = serde_json::from_value(frozen)?;
    a.command(&mut core, Command::SetModel { model: "default".into(), kind: None });
    let setting = request(&mut core, "session/set_config_option")?;
    reply(&mut a, &mut core, &setting, json!({"configOptions": options("accept-edits", "default", "low")}));
    assert_eq!(core.meta.models.iter().find(|m| m.id == "careful").map(|m| m.efforts.clone()), Some(vec!["low".into(), "high".into()]));
    assert_eq!(core.meta.models.iter().find(|m| m.default).map(|m| m.id.as_str()), Some("default"));
    update(&mut a, &mut core, options("accept-edits", "default", "low").into_iter().filter(|c| c.get("category").and_then(Value::as_str) != Some("thought_level")).collect());
    assert!(core.meta.models.iter().find(|m| m.id == "default").is_some_and(|m| m.efforts.is_empty()));
    assert!(core.meta.models.iter().find(|m| m.id == "careful").is_some_and(|m| m.efforts.len() == 2));
    Ok(())
}

#[test]
fn requested_mode_model_and_effort_are_applied_in_order_before_the_first_prompt() -> anyhow::Result<()> {
    let mut a = Acp::from_spec(&Spec { kind: "devin".into(), mode: Some("plan".into()), model: Some("careful".into()), effort: Some("high".into()), ..Spec::default() });
    let mut core = Core::new();
    core.state.cwd = "/work".into();
    let session = initialize(&mut a, &mut core, false)?;
    send(&mut a, &mut core, "m1");
    assert!(written(&mut core).is_empty());
    reply(&mut a, &mut core, &session, json!({"sessionId": "s1", "configOptions": options("accept-edits", "default", "low")}));
    for (id, value, next) in [
        ("permissions", "plan", options("plan", "default", "low")),
        ("engine", "careful", options("plan", "careful", "low")),
        ("thinking", "high", options("plan", "careful", "high")),
    ] {
        let setting = request(&mut core, "session/set_config_option")?;
        assert_eq!(setting.get("params"), Some(&json!({"sessionId": "s1", "configId": id, "value": value})));
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Queued));
        reply(&mut a, &mut core, &setting, json!({"configOptions": next}));
    }
    let prompt = request(&mut core, "session/prompt")?;
    assert_eq!(prompt.pointer("/params/prompt"), Some(&json!([{"type": "text", "text": "work m1"}])));
    assert_eq!((core.state.mode.as_str(), core.state.model.as_str(), core.state.effort.as_str()), ("plan", "careful", "high"));
    assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Started));
    Ok(())
}

#[test]
fn a_failed_initial_permission_change_keeps_all_prompts_waiting() -> anyhow::Result<()> {
    let mut a = Acp::from_spec(&Spec { kind: "devin".into(), mode: Some("ask".into()), ..Spec::default() });
    let mut core = Core::new();
    let session = initialize(&mut a, &mut core, false)?;
    send(&mut a, &mut core, "m1");
    reply(&mut a, &mut core, &session, json!({"sessionId": "s1", "configOptions": options("accept-edits", "default", "low")}));
    let setting = request(&mut core, "session/set_config_option")?;
    a.line(&mut core, &json!({"jsonrpc": "2.0", "id": setting.get("id"), "error": {"code": -32602, "message": "mode refused"}}).to_string());
    send(&mut a, &mut core, "m2");
    assert!(written(&mut core).is_empty());
    assert_eq!(core.state.mode, "accept-edits");
    assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Queued));
    assert_eq!(core.state.delivery.get("m2"), Some(&Delivery::Queued));
    assert!(core.events().any(|e| matches!(&e.body, Body::Notice { level: Level::Error, text } if text.contains("mode refused"))));
    a.command(&mut core, Command::SetMode { mode: "ask".into() });
    let correction = request(&mut core, "session/set_config_option")?;
    reply(&mut a, &mut core, &correction, json!({"configOptions": options("ask", "default", "low")}));
    let prompt = request(&mut core, "session/prompt")?;
    assert_eq!(prompt.pointer("/params/prompt/0/text"), Some(&json!("work m1")));
    assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Started));
    assert_eq!(core.state.delivery.get("m2"), Some(&Delivery::Queued));
    Ok(())
}

#[test]
fn grouped_models_and_uncategorized_options_use_existing_selectors_and_controls() -> anyhow::Result<()> {
    let config = vec![
        json!({"id": "engine", "name": "Model", "category": "model", "type": "select", "currentValue": "small", "options": [{"group": "recommended", "name": "Recommended", "options": [{"value": "small", "name": "Small"}, {"value": "large", "name": "Large"}]}]}),
        json!({"id": "style", "name": "Style", "type": "select", "currentValue": "short", "options": [{"value": "short", "name": "Short"}, {"value": "long", "name": "Long"}]}),
        json!({"id": "format", "name": "Format", "category": "_custom", "type": "select", "currentValue": "plain", "options": [{"value": "plain", "name": "Plain"}, {"value": "markdown", "name": "Markdown"}]}),
    ];
    let (mut a, mut core) = opened(config.clone())?;
    assert_eq!(core.meta.models.iter().map(|m| (m.id.as_str(), m.label.as_str())).collect::<Vec<_>>(), [("small", "Recommended · Small"), ("large", "Recommended · Large")]);
    assert_eq!(core.meta.controls.iter().map(|c| (c.id.as_str(), c.label.as_str(), c.kind)).collect::<Vec<_>>(), [("acp:style", "Style", ControlKind::Select), ("acp:format", "Format", ControlKind::Select)]);
    for (id, value) in [("acp:style", "long"), ("acp:format", "markdown")] {
        a.command(&mut core, Command::Control { id: id.into(), value: Some(json!(value)) });
        let setting = request(&mut core, "session/set_config_option")?;
        assert_eq!(setting.pointer("/params/configId").and_then(Value::as_str), id.strip_prefix("acp:"));
        assert_eq!(setting.pointer("/params/value"), Some(&json!(value)));
        reply(&mut a, &mut core, &setting, json!({"configOptions": config}));
    }
    a.command(&mut core, Command::SetModel { model: "large".into(), kind: None });
    let setting = request(&mut core, "session/set_config_option")?;
    assert_eq!(setting.pointer("/params/configId"), Some(&json!("engine")));
    assert_eq!(setting.pointer("/params/value"), Some(&json!("large")));
    Ok(())
}

#[test]
fn later_options_in_the_same_category_keep_their_own_control() -> anyhow::Result<()> {
    let mut config = options("ask", "default", "low");
    config.push(json!({"id": "secondary", "name": "Secondary model", "category": "model", "type": "select", "currentValue": "small", "options": [{"value": "small", "name": "Small"}, {"value": "large", "name": "Large"}]}));
    let (mut a, mut core) = opened(config.clone())?;
    assert_eq!(core.state.model, "default");
    assert!(core.meta.controls.iter().any(|c| c.id == "acp:secondary" && c.group == "Model"));
    a.command(&mut core, Command::SetModel { model: "careful".into(), kind: None });
    let primary = request(&mut core, "session/set_config_option")?;
    assert_eq!(primary.pointer("/params/configId"), Some(&json!("engine")));
    reply(&mut a, &mut core, &primary, json!({"configOptions": config}));
    a.command(&mut core, Command::Control { id: "acp:secondary".into(), value: Some(json!("large")) });
    let secondary = request(&mut core, "session/set_config_option")?;
    assert_eq!(secondary.pointer("/params/configId"), Some(&json!("secondary")));
    Ok(())
}

#[test]
fn boolean_controls_advertise_support_and_send_typed_values() -> anyhow::Result<()> {
    let mut a = Acp::from_spec(&Spec { kind: "devin".into(), ..Spec::default() });
    let mut core = Core::new();
    a.start(&mut core);
    let init = request(&mut core, "initialize")?;
    assert_eq!(init.pointer("/params/clientCapabilities/session/configOptions/boolean"), Some(&json!({})));
    reply(&mut a, &mut core, &init, json!({"protocolVersion": 1, "agentCapabilities": {}}));
    let session = request(&mut core, "session/new")?;
    let config = |value| vec![json!({"id": "verbose", "name": "Verbose", "type": "boolean", "currentValue": value})];
    reply(&mut a, &mut core, &session, json!({"sessionId": "s1", "configOptions": config(false)}));
    a.command(&mut core, Command::Control { id: "acp:verbose".into(), value: Some(json!("true")) });
    assert!(written(&mut core).is_empty(), "a string must not become a boolean");
    a.command(&mut core, Command::Control { id: "acp:verbose".into(), value: Some(json!(true)) });
    let setting = request(&mut core, "session/set_config_option")?;
    assert_eq!(setting.get("params"), Some(&json!({"sessionId": "s1", "configId": "verbose", "type": "boolean", "value": true})));
    assert_eq!(core.state.control_values.get("acp:verbose"), Some(&json!(false)));
    reply(&mut a, &mut core, &setting, json!({"configOptions": config(true)}));
    assert_eq!(core.state.control_values.get("acp:verbose"), Some(&json!(true)));
    assert!(core.meta.controls.iter().any(|c| c.id == "acp:verbose" && c.kind == ControlKind::Toggle && c.value == Some(json!(true))));
    Ok(())
}

#[test]
fn a_setter_response_updates_dependent_effort_without_a_notification() -> anyhow::Result<()> {
    let (mut a, mut core) = opened(options("ask", "default", "low"))?;
    assert_eq!(core.meta.efforts.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["low"]);
    a.command(&mut core, Command::SetModel { model: "careful".into(), kind: None });
    let model = request(&mut core, "session/set_config_option")?;
    assert_eq!(core.state.model, "default");
    send(&mut a, &mut core, "m1");
    assert!(written(&mut core).is_empty(), "a pending setting holds the next prompt");
    reply(&mut a, &mut core, &model, json!({"configOptions": options("ask", "careful", "low")}));
    assert_eq!(core.state.model, "careful");
    assert_eq!(core.meta.efforts.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["low", "high"]);
    let prompt = request(&mut core, "session/prompt")?;
    reply(&mut a, &mut core, &prompt, json!({"stopReason": "end_turn"}));
    a.command(&mut core, Command::SetEffort { effort: "high".into() });
    let effort = request(&mut core, "session/set_config_option")?;
    assert_eq!(effort.pointer("/params/configId"), Some(&json!("thinking")));
    assert_eq!(core.state.effort, "low");
    reply(&mut a, &mut core, &effort, json!({"configOptions": options("ask", "careful", "high")}));
    assert_eq!(core.state.effort, "high");
    Ok(())
}

#[test]
fn runtime_model_and_effort_changes_wait_for_each_other_before_a_prompt() -> anyhow::Result<()> {
    let (mut a, mut core) = opened(options("ask", "default", "low"))?;
    a.command(&mut core, Command::SetModel { model: "careful".into(), kind: None });
    let model = request(&mut core, "session/set_config_option")?;
    a.command(&mut core, Command::SetEffort { effort: "high".into() });
    send(&mut a, &mut core, "m1");
    assert!(written(&mut core).is_empty(), "effort waits for the new model's choices");
    let mut a: Acp = serde_json::from_value(serde_json::to_value(a)?)?;
    reply(&mut a, &mut core, &model, json!({"configOptions": options("ask", "careful", "low")}));
    let effort = request(&mut core, "session/set_config_option")?;
    assert_eq!(effort.get("params"), Some(&json!({"sessionId": "s1", "configId": "thinking", "value": "high"})));
    assert_eq!(core.state.effort, "low");
    assert_ne!(core.state.delivery.get("m1"), Some(&Delivery::Started));
    reply(&mut a, &mut core, &effort, json!({"configOptions": options("ask", "careful", "high")}));
    let prompt = request(&mut core, "session/prompt")?;
    assert_eq!(prompt.pointer("/params/prompt"), Some(&json!([{"type": "text", "text": "work m1"}])));
    assert_eq!((core.state.model.as_str(), core.state.effort.as_str()), ("careful", "high"));
    Ok(())
}

#[test]
fn generic_controls_keep_their_order_while_a_turn_is_running() -> anyhow::Result<()> {
    let config = |verbose, style| {
        let mut config = options("ask", "default", "low");
        config.extend([
            json!({"id": "verbose", "name": "Verbose", "type": "boolean", "currentValue": verbose}),
            json!({"id": "style", "name": "Style", "type": "select", "currentValue": style, "options": [{"value": "short", "name": "Short"}, {"value": "long", "name": "Long"}]}),
        ]);
        config
    };
    let (mut a, mut core) = opened(config(false, "short"))?;
    send(&mut a, &mut core, "m1");
    let _prompt = request(&mut core, "session/prompt")?;
    a.command(&mut core, Command::Control { id: "acp:verbose".into(), value: Some(json!(true)) });
    let verbose = request(&mut core, "session/set_config_option")?;
    a.command(&mut core, Command::Control { id: "acp:style".into(), value: Some(json!("long")) });
    assert!(written(&mut core).is_empty());
    assert_eq!(verbose.pointer("/params/type"), Some(&json!("boolean")));
    reply(&mut a, &mut core, &verbose, json!({"configOptions": config(true, "short")}));
    let style = request(&mut core, "session/set_config_option")?;
    assert_eq!(style.get("params"), Some(&json!({"sessionId": "s1", "configId": "style", "value": "long"})));
    reply(&mut a, &mut core, &style, json!({"configOptions": config(true, "long")}));
    assert_eq!(core.state.control_values.get("acp:verbose"), Some(&json!(true)));
    assert_eq!(core.state.control_values.get("acp:style"), Some(&json!("long")));
    assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Started));
    Ok(())
}

#[test]
fn startup_checks_its_mode_after_successful_setters_and_allows_an_explicit_correction() -> anyhow::Result<()> {
    for changed_by in ["mode", "model"] {
        let mut a = Acp::from_spec(&Spec { kind: "devin".into(), mode: Some("plan".into()), model: (changed_by == "model").then(|| "careful".into()), ..Spec::default() });
        let mut core = Core::new();
        let session = initialize(&mut a, &mut core, false)?;
        send(&mut a, &mut core, "m1");
        reply(&mut a, &mut core, &session, json!({"sessionId": "s1", "configOptions": options("accept-edits", "default", "low")}));
        let mode = request(&mut core, "session/set_config_option")?;
        let model = if changed_by == "model" {
            reply(&mut a, &mut core, &mode, json!({"configOptions": options("plan", "default", "low")}));
            let model = request(&mut core, "session/set_config_option")?;
            reply(&mut a, &mut core, &model, json!({"configOptions": options("accept-edits", "careful", "low")}));
            "careful"
        } else {
            reply(&mut a, &mut core, &mode, json!({"configOptions": options("accept-edits", "default", "low")}));
            "default"
        };
        assert!(written(&mut core).is_empty(), "{changed_by}: a successful setter reset the requested permission");
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Queued));
        assert!(!a.ready);
        assert_eq!(a.failed_setting.as_deref(), Some("mode"));
        a.command(&mut core, Command::SetMode { mode: "ask".into() });
        let mode = request(&mut core, "session/set_config_option")?;
        reply(&mut a, &mut core, &mode, json!({"configOptions": options("ask", model, "low")}));
        let _prompt = request(&mut core, "session/prompt")?;
        assert_eq!(core.state.mode, "ask");
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Started));
    }
    Ok(())
}

#[test]
fn configuration_updates_replace_removed_controls_models_and_effort() -> anyhow::Result<()> {
    let mut config = options("plan", "careful", "high");
    config.push(json!({"id": "style", "name": "Style", "type": "select", "currentValue": "short", "options": [{"value": "short", "name": "Short"}]}));
    let (mut a, mut core) = opened(config)?;
    assert_eq!(core.state.control_values.get("acp:style"), Some(&json!("short")));
    update(&mut a, &mut core, Vec::new());
    assert!(core.meta.models.is_empty() && core.state.model.is_empty());
    assert!(core.meta.efforts.is_empty() && core.state.effort.is_empty());
    assert!(!core.meta.features.iter().any(|f| f == "set_model" || f == "set_effort"));
    assert!(!core.meta.controls.iter().any(|c| c.id == "acp:style"));
    assert!(!core.state.control_values.contains_key("acp:style"));
    a.command(&mut core, Command::Control { id: "acp:style".into(), value: Some(json!("short")) });
    a.command(&mut core, Command::SetMode { mode: "plan".into() });
    a.command(&mut core, Command::SetModel { model: "careful".into(), kind: None });
    assert!(written(&mut core).is_empty(), "a removed option cannot still be changed");
    Ok(())
}

#[test]
fn configuration_values_win_over_legacy_mode_events() -> anyhow::Result<()> {
    let (mut a, mut core) = opened(options("plan", "careful", "high"))?;
    a.update(&mut core, &json!({"sessionUpdate": "current_mode_update", "currentModeId": "bypass"}));
    assert_eq!(core.state.mode, "plan");
    Ok(())
}

#[test]
fn unknown_types_and_malformed_values_are_kept_without_becoming_controls() -> anyhow::Result<()> {
    let mut config = vec![
        json!({"id": "future", "name": "Future", "category": "model", "type": "_future", "currentValue": "a", "options": [{"value": "a", "name": "A"}]}),
        json!({"id": "bad-toggle", "name": "Bad toggle", "type": "boolean", "currentValue": "false"}),
        json!({"id": "bad-model", "name": "Bad model", "category": "model", "type": "select", "currentValue": null, "options": [{"value": "a", "name": "A"}]}),
    ];
    config.extend(options("ask", "default", "low"));
    let (mut a, mut core) = opened(config.clone())?;
    assert!(core.meta.controls.is_empty());
    assert_eq!(core.state.model, "default");
    for id in ["future", "bad-toggle", "bad-model"] {
        a.command(&mut core, Command::Control { id: format!("acp:{id}"), value: Some(json!("a")) });
    }
    assert!(written(&mut core).is_empty());
    let a: Acp = serde_json::from_value(serde_json::to_value(a)?)?;
    assert_eq!(a.config, config);
    Ok(())
}

#[test]
fn legacy_mode_and_model_setters_keep_their_protocol_and_apply_startup_settings() -> anyhow::Result<()> {
    let mut a = Acp::from_spec(&Spec { kind: "gemini".into(), mode: Some("plan".into()), model: Some("careful".into()), ..Spec::default() });
    let mut core = Core::new();
    let session = initialize(&mut a, &mut core, false)?;
    send(&mut a, &mut core, "m1");
    reply(&mut a, &mut core, &session, json!({"sessionId": "s1", "modes": {"currentModeId": "ask", "availableModes": [{"id": "ask", "name": "Ask"}, {"id": "plan", "name": "Plan"}]}, "models": {"currentModelId": "default", "availableModels": [{"modelId": "default", "name": "Default"}, {"modelId": "careful", "name": "Careful"}]}}));
    let mode = request(&mut core, "session/set_mode")?;
    assert_eq!(mode.get("params"), Some(&json!({"sessionId": "s1", "modeId": "plan"})));
    reply(&mut a, &mut core, &mode, json!({}));
    assert_eq!(core.state.mode, "plan");
    let model = request(&mut core, "session/set_model")?;
    assert_eq!(model.get("params"), Some(&json!({"sessionId": "s1", "modelId": "careful"})));
    reply(&mut a, &mut core, &model, json!({}));
    assert_eq!(core.state.model, "careful");
    let prompt = request(&mut core, "session/prompt")?;
    reply(&mut a, &mut core, &prompt, json!({"stopReason": "end_turn"}));
    a.command(&mut core, Command::SetMode { mode: "ask".into() });
    let mode = request(&mut core, "session/set_mode")?;
    reply(&mut a, &mut core, &mode, json!({}));
    assert_eq!(core.state.mode, "ask");
    a.command(&mut core, Command::SetModel { model: "default".into(), kind: None });
    let model = request(&mut core, "session/set_model")?;
    reply(&mut a, &mut core, &model, json!({}));
    assert_eq!(core.state.model, "default");
    Ok(())
}

fn old_image(a: Acp) -> anyhow::Result<Acp> {
    let mut frozen = serde_json::to_value(a)?;
    let state = frozen.as_object_mut().ok_or_else(|| anyhow::anyhow!("adapter is not an object"))?;
    for key in ["config", "has_config", "settings", "startup_mode", "changes", "configuring", "applying", "failed_setting", "ready", "replaying", "account", "refresh", "empty"] {
        state.remove(key);
    }
    Ok(serde_json::from_value(frozen)?)
}

#[test]
fn old_frozen_startups_keep_prompts_until_the_session_is_open() -> anyhow::Result<()> {
    for stage in ["initialize", "new", "load"] {
        let mut a = Acp::from_spec(&Spec { kind: "gemini".into(), resume: (stage == "load").then(|| "s1".into()), ..Spec::default() });
        let mut core = Core::new();
        a.start(&mut core);
        let init = request(&mut core, "initialize")?;
        let pending = if stage == "initialize" {
            init
        } else {
            reply(&mut a, &mut core, &init, json!({"protocolVersion": 1, "agentCapabilities": {"loadSession": stage == "load"}}));
            request(&mut core, if stage == "load" { "session/load" } else { "session/new" })?
        };
        let mut a = old_image(a)?;
        send(&mut a, &mut core, "m1");
        assert!(written(&mut core).is_empty(), "{stage}: sent before the session opened");
        let session = if stage == "initialize" {
            reply(&mut a, &mut core, &pending, json!({"protocolVersion": 1, "agentCapabilities": {}}));
            request(&mut core, "session/new")?
        } else {
            pending
        };
        reply(&mut a, &mut core, &session, if stage == "load" { json!({}) } else { json!({"sessionId": "s1"}) });
        let prompt = request(&mut core, "session/prompt")?;
        assert_eq!(prompt.pointer("/params/sessionId"), Some(&json!("s1")));
        assert_eq!(prompt.pointer("/params/prompt"), Some(&json!([{"type": "text", "text": "work m1"}])));
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Started));
    }
    Ok(())
}

#[test]
fn old_frozen_open_sessions_continue_without_initializing_again() -> anyhow::Result<()> {
    let (a, mut core) = opened(Vec::new())?;
    let mut a = old_image(a)?;
    send(&mut a, &mut core, "m1");
    let prompt = request(&mut core, "session/prompt")?;
    assert_eq!(prompt.pointer("/params/sessionId"), Some(&json!("s1")));
    Ok(())
}

#[test]
fn a_concurrent_setter_cannot_complete_an_initial_permission_change() -> anyhow::Result<()> {
    let mut a = Acp::from_spec(&Spec { kind: "devin".into(), mode: Some("plan".into()), ..Spec::default() });
    let mut core = Core::new();
    let session = initialize(&mut a, &mut core, false)?;
    send(&mut a, &mut core, "m1");
    reply(&mut a, &mut core, &session, json!({"sessionId": "s1", "configOptions": options("accept-edits", "default", "low")}));
    let mode = request(&mut core, "session/set_config_option")?;
    a.command(&mut core, Command::SetModel { model: "careful".into(), kind: None });
    assert!(written(&mut core).is_empty());
    a.line(&mut core, &json!({"jsonrpc": "2.0", "id": mode.get("id"), "error": {"code": -32602, "message": "mode refused"}}).to_string());
    let model = request(&mut core, "session/set_config_option")?;
    reply(&mut a, &mut core, &model, json!({"configOptions": options("accept-edits", "careful", "low")}));
    send(&mut a, &mut core, "m2");
    assert!(written(&mut core).is_empty(), "another setter's success must not release a failed startup");
    assert!(!a.ready);
    assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Queued));
    assert_eq!(core.state.delivery.get("m2"), Some(&Delivery::Queued));
    assert!(core.events().any(|e| matches!(&e.body, Body::Notice { level: Level::Error, text } if text.contains("mode refused"))));
    Ok(())
}

#[test]
fn an_early_prompt_does_not_hide_a_resumed_sessions_first_history_replay() -> anyhow::Result<()> {
    let mut a = Acp::from_spec(&Spec { kind: "devin".into(), resume: Some("saved".into()), ..Spec::default() });
    let mut core = Core::new();
    a.start(&mut core);
    let init = request(&mut core, "initialize")?;
    send(&mut a, &mut core, "m1");
    reply(&mut a, &mut core, &init, json!({"protocolVersion": 1, "agentCapabilities": {"loadSession": true}}));
    let session = request(&mut core, "session/load")?;
    for (kind, text) in [("user_message_chunk", "saved question"), ("agent_message_chunk", "saved answer")] {
        a.line(&mut core, &json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "saved", "update": {"sessionUpdate": kind, "content": {"type": "text", "text": text}}}}).to_string());
    }
    reply(&mut a, &mut core, &session, json!({"configOptions": []}));
    let prompt = request(&mut core, "session/prompt")?;
    assert_eq!(prompt.pointer("/params/sessionId"), Some(&json!("saved")));
    assert_eq!(prompt.pointer("/params/prompt"), Some(&json!([{"type": "text", "text": "work m1"}])));
    let messages: Vec<&str> = core.events().filter_map(|e| match &e.body { Body::User { text, .. } | Body::Assistant { text, .. } => Some(text.as_str()), _ => None }).collect();
    assert_eq!(messages, ["work m1", "saved question", "saved answer"]);
    Ok(())
}

fn account_session() -> anyhow::Result<(Acp, Core)> {
    let mut a = Acp::from_spec(&Spec { kind: "devin".into(), ..Spec::default() });
    let mut core = Core::new();
    core.state.cwd = "/work".into();
    a.start(&mut core);
    let init = request(&mut core, "initialize")?;
    reply(&mut a, &mut core, &init, json!({"protocolVersion": 1, "agentCapabilities": {"loadSession": true}}));
    let session = request(&mut core, "session/new")?;
    reply(&mut a, &mut core, &session, json!({"sessionId": "s1", "configOptions": options("ask", "default", "low")}));
    Ok((a, core))
}

fn account(a: &mut Acp, core: &mut Core) {
    a.command(core, Command::Control { id: "account".into(), value: None });
}

#[test]
fn signing_in_reloads_the_account_and_models_without_replaying_kept_messages() -> anyhow::Result<()> {
    let (mut a, mut core) = account_session()?;
    core.upsert("earlier", None, Body::Assistant { text: "saved answer".into(), streaming: false });
    a.offer_mcp(vec![json!({"name": "kiln", "command": "/bin/kiln", "args": ["mcp"], "env": []})]);
    account(&mut a, &mut core);
    let load = request(&mut core, "session/load")?;
    assert_eq!(load.get("params"), Some(&json!({"sessionId": "s1", "cwd": "/work", "mcpServers": [{"name": "kiln", "command": "/bin/kiln", "args": ["mcp"], "env": []}]})));
    send(&mut a, &mut core, "m1");
    a.update(&mut core, &json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "saved answer"}}));
    reply(&mut a, &mut core, &load, json!({"configOptions": options("accept-edits", "careful", "high")}));
    let mode = request(&mut core, "session/set_config_option")?;
    assert_eq!(mode.pointer("/params/value"), Some(&json!("ask")), "a reload must keep the existing permission");
    reply(&mut a, &mut core, &mode, json!({"configOptions": options("ask", "careful", "high")}));
    let prompt = request(&mut core, "session/prompt")?;
    assert_eq!(prompt.pointer("/params/sessionId"), Some(&json!("s1")));
    assert_eq!((core.state.model.as_str(), core.state.effort.as_str()), ("careful", "high"));
    assert_eq!(core.events().filter(|e| matches!(&e.body, Body::Assistant { text, .. } if text == "saved answer")).count(), 1);
    Ok(())
}

#[test]
fn signing_in_waits_for_the_turn_and_setter_then_reloads_before_queued_settings_and_prompts() -> anyhow::Result<()> {
    let (mut a, mut core) = account_session()?;
    send(&mut a, &mut core, "m1");
    let prompt = request(&mut core, "session/prompt")?;
    a.command(&mut core, Command::SetModel { model: "careful".into(), kind: None });
    let model = request(&mut core, "session/set_config_option")?;
    account(&mut a, &mut core);
    a.command(&mut core, Command::SetEffort { effort: "high".into() });
    send(&mut a, &mut core, "m2");
    assert!(written(&mut core).is_empty());
    let mut a: Acp = serde_json::from_value(serde_json::to_value(a)?)?;
    reply(&mut a, &mut core, &model, json!({"configOptions": options("ask", "careful", "low")}));
    assert!(written(&mut core).is_empty(), "the current turn still owns the session");
    reply(&mut a, &mut core, &prompt, json!({"stopReason": "end_turn"}));
    let load = request(&mut core, "session/load")?;
    reply(&mut a, &mut core, &load, json!({"configOptions": options("ask", "careful", "low")}));
    let effort = request(&mut core, "session/set_config_option")?;
    assert_eq!(effort.pointer("/params/configId"), Some(&json!("thinking")));
    reply(&mut a, &mut core, &effort, json!({"configOptions": options("ask", "careful", "high")}));
    let prompt = request(&mut core, "session/prompt")?;
    assert_eq!(prompt.pointer("/params/prompt/0/text"), Some(&json!("work m2")));
    Ok(())
}

#[test]
fn signing_in_waits_for_the_last_permission_answer() -> anyhow::Result<()> {
    let (mut a, mut core) = account_session()?;
    a.line(&mut core, &json!({"jsonrpc": "2.0", "id": "approval", "method": "session/request_permission", "params": {"sessionId": "s1", "toolCall": {"toolCallId": "t1", "title": "Run", "kind": "execute"}, "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]}}).to_string());
    account(&mut a, &mut core);
    assert!(written(&mut core).is_empty());
    a.command(&mut core, Command::Approve { id: "approval".into(), decision: Decision::Allow, message: None, always: None, answers: None, choices: None });
    let writes = written(&mut core);
    assert_eq!(writes.len(), 2);
    assert!(writes.iter().any(|w| w.get("id") == Some(&json!("approval")) && w.pointer("/result/outcome/optionId") == Some(&json!("allow"))));
    assert!(writes.iter().any(|w| w.get("method") == Some(&json!("session/load"))));
    Ok(())
}

#[test]
fn an_empty_unpersisted_session_gets_one_new_session_after_signing_in() -> anyhow::Result<()> {
    let (mut a, mut core) = account_session()?;
    account(&mut a, &mut core);
    let load = request(&mut core, "session/load")?;
    send(&mut a, &mut core, "m1");
    a.line(&mut core, &json!({"jsonrpc": "2.0", "id": load.get("id"), "error": {"code": -32016, "message": "session_not_found"}}).to_string());
    let new = request(&mut core, "session/new")?;
    reply(&mut a, &mut core, &new, json!({"sessionId": "s2", "configOptions": options("ask", "careful", "low")}));
    let prompt = request(&mut core, "session/prompt")?;
    assert_eq!(prompt.pointer("/params/sessionId"), Some(&json!("s2")));
    assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Started));
    Ok(())
}

#[test]
fn a_failed_account_reload_holds_prompts_and_another_sign_in_can_retry() -> anyhow::Result<()> {
    let (mut a, mut core) = account_session()?;
    account(&mut a, &mut core);
    let load = request(&mut core, "session/load")?;
    a.line(&mut core, &json!({"jsonrpc": "2.0", "id": load.get("id"), "error": {"code": -32000, "message": "authentication failed"}}).to_string());
    send(&mut a, &mut core, "m1");
    assert!(written(&mut core).is_empty(), "auth failures cannot replace the conversation");
    assert!(!a.ready);
    account(&mut a, &mut core);
    let load = request(&mut core, "session/load")?;
    assert_eq!(load.pointer("/params/sessionId"), Some(&json!("s1")));
    reply(&mut a, &mut core, &load, json!({"configOptions": options("ask", "careful", "high")}));
    let _prompt = request(&mut core, "session/prompt")?;
    assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Started));
    Ok(())
}

#[test]
fn ordinary_resume_failures_never_create_a_new_conversation() -> anyhow::Result<()> {
    for code in [-32016, -32000] {
        let mut a = Acp::from_spec(&Spec { kind: "devin".into(), resume: Some("saved".into()), ..Spec::default() });
        let mut core = Core::new();
        let load = initialize(&mut a, &mut core, true)?;
        send(&mut a, &mut core, "m1");
        a.line(&mut core, &json!({"jsonrpc": "2.0", "id": load.get("id"), "error": {"code": code, "message": "cannot load session"}}).to_string());
        assert!(written(&mut core).is_empty(), "{code}: an ordinary load must not fall back");
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Queued));
    }
    Ok(())
}

#[test]
fn signing_in_after_session_creation_failed_keeps_the_original_prompt_and_permission() -> anyhow::Result<()> {
    let mut a = Acp::from_spec(&Spec { kind: "devin".into(), mode: Some("ask".into()), ..Spec::default() });
    let mut core = Core::new();
    a.start(&mut core);
    let init = request(&mut core, "initialize")?;
    reply(&mut a, &mut core, &init, json!({"protocolVersion": 1, "agentCapabilities": {"loadSession": true}}));
    let new = request(&mut core, "session/new")?;
    send(&mut a, &mut core, "m1");
    a.line(&mut core, &json!({"jsonrpc": "2.0", "id": new.get("id"), "error": {"code": -32000, "message": "authentication required"}}).to_string());
    account(&mut a, &mut core);
    let new = request(&mut core, "session/new")?;
    reply(&mut a, &mut core, &new, json!({"sessionId": "s1", "configOptions": options("accept-edits", "careful", "low")}));
    let mode = request(&mut core, "session/set_config_option")?;
    assert_eq!(mode.pointer("/params/value"), Some(&json!("ask")));
    reply(&mut a, &mut core, &mode, json!({"configOptions": options("ask", "careful", "low")}));
    let prompt = request(&mut core, "session/prompt")?;
    assert_eq!(prompt.pointer("/params/prompt/0/text"), Some(&json!("work m1")));
    Ok(())
}

#[test]
fn signing_in_retries_the_exact_unavailable_startup_model_then_its_effort() -> anyhow::Result<()> {
    let unavailable = |mode| {
        let mut config = options(mode, "default", "low");
        for option in &mut config {
            if matches!(option.get("category").and_then(Value::as_str), Some("model" | "thought_level")) && let Some(option) = option.as_object_mut() {
                option.insert("options".into(), json!([]));
            }
        }
        config
    };
    let mut a = Acp::from_spec(&Spec { kind: "devin".into(), mode: Some("plan".into()), model: Some("careful".into()), effort: Some("high".into()), ..Spec::default() });
    let mut core = Core::new();
    a.start(&mut core);
    let init = request(&mut core, "initialize")?;
    reply(&mut a, &mut core, &init, json!({"protocolVersion": 1, "agentCapabilities": {"loadSession": true}}));
    let new = request(&mut core, "session/new")?;
    send(&mut a, &mut core, "m1");
    reply(&mut a, &mut core, &new, json!({"sessionId": "s1", "configOptions": unavailable("accept-edits")}));
    let mode = request(&mut core, "session/set_config_option")?;
    reply(&mut a, &mut core, &mode, json!({"configOptions": unavailable("plan")}));
    assert!(written(&mut core).is_empty(), "an unavailable model holds without retrying itself");
    assert_eq!(a.failed_setting.as_deref(), Some("model"));
    assert_eq!(a.applying, Some(("model".into(), "careful".into())));
    let mut a: Acp = serde_json::from_value(serde_json::to_value(a)?)?;
    account(&mut a, &mut core);
    let load = request(&mut core, "session/load")?;
    reply(&mut a, &mut core, &load, json!({"configOptions": options("accept-edits", "default", "low")}));
    for (id, value, config) in [
        ("permissions", "plan", options("plan", "default", "low")),
        ("engine", "careful", options("plan", "careful", "low")),
        ("thinking", "high", options("plan", "careful", "high")),
    ] {
        let setting = request(&mut core, "session/set_config_option")?;
        assert_eq!(setting.get("params"), Some(&json!({"sessionId": "s1", "configId": id, "value": value})));
        assert_eq!(core.state.delivery.get("m1"), Some(&Delivery::Queued));
        reply(&mut a, &mut core, &setting, json!({"configOptions": config}));
    }
    let prompt = request(&mut core, "session/prompt")?;
    assert_eq!(prompt.pointer("/params/prompt/0/text"), Some(&json!("work m1")));
    assert_eq!((core.state.mode.as_str(), core.state.model.as_str(), core.state.effort.as_str()), ("plan", "careful", "high"));
    assert!(a.applying.is_none() && a.failed_setting.is_none());
    Ok(())
}

#[test]
fn local_modes_can_explicitly_correct_a_rejected_initial_mode() -> anyhow::Result<()> {
    for mode in [KILN_ASK, KILN_BYPASS] {
        let mut a = Acp::from_spec(&Spec { kind: "gemini".into(), mode: Some("plan".into()), allow_bypass: false, ..Spec::default() });
        let mut core = Core::new();
        let new = initialize(&mut a, &mut core, false)?;
        send(&mut a, &mut core, "m1");
        reply(&mut a, &mut core, &new, json!({"sessionId": "s1"}));
        let native = request(&mut core, "session/set_mode")?;
        a.line(&mut core, &json!({"jsonrpc": "2.0", "id": native.get("id"), "error": {"code": -32601, "message": "no native modes"}}).to_string());
        a.command(&mut core, Command::SetMode { mode: mode.into() });
        let _prompt = request(&mut core, "session/prompt")?;
        assert_eq!(core.state.mode, mode);
        assert!(a.ready && a.failed_setting.is_none() && a.applying.is_none());
        assert_eq!(core.effects.iter().filter(|e| matches!(e, Effect::AllowBypass)).count(), usize::from(mode == KILN_BYPASS));
    }
    Ok(())
}

#[test]
fn only_a_successful_explicit_native_bypass_grants_bypass_permission() -> anyhow::Result<()> {
    let mut a = Acp::from_spec(&Spec { kind: "devin".into(), allow_bypass: false, ..Spec::default() });
    let mut core = Core::new();
    let new = initialize(&mut a, &mut core, false)?;
    reply(&mut a, &mut core, &new, json!({"sessionId": "s1", "configOptions": options("ask", "default", "low")}));
    update(&mut a, &mut core, options("bypass", "default", "low"));
    assert!(!core.effects.iter().any(|e| matches!(e, Effect::AllowBypass)), "an unsolicited update grants nothing");
    a.command(&mut core, Command::SetMode { mode: "bypass".into() });
    let bypass = request(&mut core, "session/set_config_option")?;
    a.line(&mut core, &json!({"jsonrpc": "2.0", "id": bypass.get("id"), "error": {"code": -32602, "message": "bypass refused"}}).to_string());
    assert!(!core.effects.iter().any(|e| matches!(e, Effect::AllowBypass)));
    for (requested, actual) in [("ask", "bypass"), ("bypass", "ask")] {
        a.command(&mut core, Command::SetMode { mode: requested.into() });
        let mode = request(&mut core, "session/set_config_option")?;
        reply(&mut a, &mut core, &mode, json!({"configOptions": options(actual, "default", "low")}));
        assert!(!core.effects.iter().any(|e| matches!(e, Effect::AllowBypass)), "{requested} became {actual}");
    }
    a.command(&mut core, Command::SetMode { mode: "bypass".into() });
    let mode = request(&mut core, "session/set_config_option")?;
    reply(&mut a, &mut core, &mode, json!({"configOptions": options("bypass", "default", "low")}));
    assert_eq!(core.effects.iter().filter(|e| matches!(e, Effect::AllowBypass)).count(), 1);
    Ok(())
}

#[test]
fn a_missing_nonempty_session_is_not_replaced_during_account_refresh() -> anyhow::Result<()> {
    let (mut a, mut core) = account_session()?;
    send(&mut a, &mut core, "m1");
    let prompt = request(&mut core, "session/prompt")?;
    reply(&mut a, &mut core, &prompt, json!({"stopReason": "end_turn"}));
    account(&mut a, &mut core);
    let load = request(&mut core, "session/load")?;
    send(&mut a, &mut core, "m2");
    a.line(&mut core, &json!({"jsonrpc": "2.0", "id": load.get("id"), "error": {"code": -32016, "message": "session_not_found"}}).to_string());
    assert!(written(&mut core).is_empty());
    assert_eq!(a.session.as_deref(), Some("s1"));
    assert!(!a.ready);
    assert_eq!(core.state.delivery.get("m2"), Some(&Delivery::Queued));
    assert!(core.events().any(|e| matches!(&e.body, Body::Notice { level: Level::Error, text } if text.contains("session_not_found"))));
    Ok(())
}
