//! A person's own copy of one of kiln's UI agents at work in their place (kiln plan
//! 01a1dd2e8618733a87f2793aab1322a8 P5.1b), through the operations as REST, MCP and code mode call
//! them: the about card a conversation's "ask about this code" starts. What a list shows of kiln's own,
//! and copying them, is `tests/agentdefs.rs`'.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::authz::{Caller, Grant, Rights, gate::Kind};
use crate::ops::{Cx, Fail};

/// A person, `user:<name>`, an admin here (they see every agent, and may start agents on the server).
fn person(name: &str) -> anyhow::Result<Caller> {
    let admin = Grant::parse("admin").map_err(anyhow::Error::msg)?;
    let mut caller = Caller::with(Kind::User, &format!("user:{name}"), Some(Rights { grants: vec![admin], ..Default::default() }), None);
    caller.user = Some(name.to_owned());
    Ok(caller)
}

fn failed((code, why): Fail) -> anyhow::Error {
    anyhow::anyhow!("{code}: {why}")
}

/// P5.1b's test "replace the ask agent with a copy": Ada copies kiln's about card, makes hers Codex
/// on a model of its, and has it stand in for kiln's for her (agentdefs.use). Asking about code in a
/// Claude Code conversation, her card is hers (Codex, her model); Bo's, asked of the same conversation,
/// is kiln's (the conversation's own harness, no model of its own). Hers back to kiln's, her card is
/// kiln's again. Taking `uses::ask_for` out of `agents.about` starts kiln's for her; keying a use by
/// kiln's agent alone, not its person too, starts hers for Bo.
#[test]
fn p5_1b_a_persons_copy_stands_in_for_kilns_ask_agent_for_them_alone() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|c| {
            c.terminal.enabled = true;
            c.conversations.off = true;
        })
        .await?;
        let h = Arc::clone(&hub);
        crate::rt::spawn(async move {
            let _ = crate::harness::registry::Registry::run_local(h).await;
        });
        hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
        let folder = tempfile::tempdir()?;
        let spec = crate::harness::session::Spec { kind: "claude".into(), cwd: Some(folder.path().to_string_lossy().into_owned()), name: Some("Main".into()), ..Default::default() };
        let main = hub.harness.open(&hub, "local", spec, 1).await?.into_iter().next().map(|i| i.id).ok_or_else(|| anyhow::anyhow!("no agent"))?;
        let (ada, bo) = (person("ada")?, person("bo")?);
        let op = |caller: &Caller, name: &'static str, input: Value| crate::ops::call(Cx { hub: Arc::clone(&hub), caller: caller.clone() }, name, input);
        // An ask about a line of the conversation's code: the card's agent, as agents.get reads it.
        let ask = |caller: &Caller| {
            let (caller, hub, main) = (caller.clone(), Arc::clone(&hub), main.clone());
            async move {
                let asked = crate::rt::timeout(Duration::from_secs(120), crate::ops::call(Cx { hub: Arc::clone(&hub), caller: caller.clone() }, "agents.about", json!({"agent": main, "path": "src/lib.rs", "line": 3, "code": "fn main() {}", "selection": "main"}))).await?.map_err(failed)?;
                let card = asked.pointer("/agents/0/id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no card started: {asked}"))?.to_owned();
                let got = crate::ops::call(Cx { hub, caller }, "agents.get", json!({"agent": card})).await.map_err(failed)?;
                anyhow::Ok(got.get("info").cloned().unwrap_or_default())
            }
        };

        let model = crate::harness::kinds::models_of("codex").into_iter().next().map(|m| m.id).ok_or_else(|| anyhow::anyhow!("no models listed for codex"))?;
        let copy = op(&ada, "agentdefs.copy", json!({"id": crate::agentdefs::builtin::ABOUT, "name": "my-about"})).await.map_err(failed)?;
        let id = copy.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        op(&ada, "agentdefs.update", json!({"id": id, "harness": "codex", "model": model, "instructions": "Answer in one sentence."})).await.map_err(failed)?;
        let used = op(&ada, "agentdefs.use", json!({"builtin": crate::agentdefs::builtin::ABOUT, "id": id})).await.map_err(failed)?;
        anyhow::ensure!(used == json!({"uses": {crate::agentdefs::builtin::ABOUT: id}}), "{used}");

        let hers = ask(&ada).await?;
        anyhow::ensure!(hers.get("kind") == Some(&json!("codex")) && hers.get("model") == Some(&json!(model)), "her ask is her copy: {hers}");
        let his = ask(&bo).await?;
        anyhow::ensure!(his.get("kind") == Some(&json!("claude")) && his.get("model") == Some(&json!("")), "his ask is kiln's: {his}");

        op(&ada, "agentdefs.use", json!({"builtin": crate::agentdefs::builtin::ABOUT, "id": null})).await.map_err(failed)?;
        let again = ask(&ada).await?;
        anyhow::ensure!(again.get("kind") == Some(&json!("claude")), "kiln's again: {again}");
        anyhow::Ok(())
    })
}
