//! The ten harnesses in microVMs through kiln's MCP tools (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d,
//! step 35; plan 01a1d2c16eb074f19cf7ae7f1025c0be steps 1 and 2): `agents_start` naming the Agentfile
//! its microVM is made from, `agents_send` steering, `agents_interrupt`, `agents_stop`, and
//! `agents_start` taking the conversation up again (`resume`), as a person's or an agent's MCP client
//! drives them. A part of `step35.rs`.

use std::time::Duration;

use serde_json::{Value, json};

use crate::vm_e2e::{self, Fixture};

/// How long a tool's answer is waited for: as step 34's drive waits for the same server work over REST
/// (`vm_e2e::open`, `command`, `stop`), longer than the server waits for its runner's answer
/// (`kiln::hub::COMMAND_WITHIN`), so kiln's own refusal arrives before the client gives up.
const ANSWERED_WITHIN: Duration = Duration::from_secs(75);

/// One of kiln's MCP tools called as its server's client calls it (`tools/call` on `/api/mcp`), with
/// the fixture's token, waited on for [`ANSWERED_WITHIN`]: its answer, or what it said when it failed.
pub fn call(f: &Fixture, tool: &str, arguments: Value) -> anyhow::Result<Value> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": arguments}});
    let (code, text) = crate::common::http_within(f.port, "POST", "/api/mcp", &f.admin, Some(&body), ANSWERED_WITHIN)?;
    anyhow::ensure!(code == 200, "{tool}: {code} {text}");
    let v: Value = serde_json::from_str(&text)?;
    let said = v.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or_default();
    anyhow::ensure!(v.pointer("/result/isError").and_then(Value::as_bool) != Some(true), "{tool} {arguments}: {said}");
    Ok(serde_json::from_str(said).unwrap_or_else(|_| Value::String(said.to_owned())))
}

/// The agent `agents_start` started.
fn started(answer: &Value) -> anyhow::Result<String> {
    answer.pointer("/agents/0/id").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("agents_start started none: {answer}"))
}

/// Whether the turn of the words carrying `nonce` is under way: kiln delivered them (a person's
/// message holding it) and its harness works on them.
fn under_way(i: &Value, events: &[Value], nonce: &str) -> bool {
    let delivered = events.iter().position(|e| e.get("kind").and_then(Value::as_str) == Some("user") && e.get("text").and_then(Value::as_str).is_some_and(|t| t.contains(nonce)));
    delivered.is_some_and(|at| vm_e2e::status(i) == "running" || !vm_e2e::said(events.get(at + 1..).unwrap_or_default()).is_empty())
}

/// What the harness said after the words carrying `nonce` were delivered.
fn said_after(events: &[Value], nonce: &str) -> Vec<String> {
    let at = events.iter().position(|e| e.get("kind").and_then(Value::as_str) == Some("user") && e.get("text").and_then(Value::as_str).is_some_and(|t| t.contains(nonce)));
    at.map(|at| vm_e2e::said(events.get(at + 1..).unwrap_or_default())).unwrap_or_default()
}

/// A word of this drive's own.
fn nonce() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    format!("m{:x}{nanos:x}", std::process::id())
}

/// Drives `kind` in a microVM made from `image` through kiln's MCP tools: started naming its
/// Agentfile on the fixture's runner, a message steered into a running turn, a turn interrupted,
/// ended, and its conversation taken up again by a new agent, which is ended too. Its first turn's
/// checks of where it ran are the REST drive's ([`vm_e2e::in_microvm`]). A vendor's CLI takes ROOT's
/// task alone: the caller holds it back first ([`super::untasked`]).
pub fn drive(f: &Fixture, kind: &str, image: &str, project: &str) -> anyhow::Result<()> {
    let marker = std::env::var("VM_E2E_DONE_MARKER").unwrap_or_else(|_| vm_e2e::STUB_DONE.to_owned());
    let stub = marker == vm_e2e::STUB_DONE;
    let n = nonce();
    let prompt = vm_e2e::task("VM_E2E_PROMPT", kind, &n, format!("do the task {n}"));
    let what = format!("{kind} from {image} through kiln's MCP");
    let done = |i: &Value, e: &[Value]| vm_e2e::status(i) == "idle" && vm_e2e::said_marker(&vm_e2e::said(e), &marker);
    let mut pace = vm_e2e::Pace::default();

    // Start, naming its Agentfile and the runner that starts microVMs.
    let id = started(&call(f, "agents_start", json!({"kind": kind, "on": f.machine, "cwd": project, "prompt": prompt, "image": image}))?)?;
    let (info, _) = pace.until(f, &id, vm_e2e::Asked::Opening, &format!("{what}: its first turn"), done)?;
    anyhow::ensure!(
        info.pointer("/isolation/kind").and_then(Value::as_str) == Some("microvm") && info.pointer("/isolation/image").and_then(Value::as_str) == Some(image),
        "{what}: kiln holds it as {}",
        info.get("isolation").unwrap_or(&Value::Null)
    );

    // Steer: a message into a turn under way, answered.
    let (long, steer) = (format!("long-{n}"), format!("STEER {n}"));
    // Its words carry the drive's own, whoever's they are, so its delivery is told from any other.
    let long_text = format!("{} {long}", vm_e2e::task("VM_E2E_STEERED_PROMPT", kind, &n, vm_e2e::stub_steered_turn()));
    call(f, "agents_send", json!({"agent": id, "text": long_text, "mode": "queue"}))?;
    pace.until(f, &id, vm_e2e::Asked::Message(&long), &format!("{what}: its long turn under way"), |i, e| under_way(i, e, &long))?;
    call(f, "agents_send", json!({"agent": id, "text": steer, "mode": "steer"}))?;
    pace.until(f, &id, vm_e2e::Asked::Steered(&steer), &format!("{what}: answering what was steered in"), |i, e| {
        vm_e2e::status(i) == "idle" && (!stub || vm_e2e::said(e).iter().any(|l| (l.starts_with("STEERED ") || l.starts_with("HEARD ")) && l.contains(&steer)))
    })?;

    // Interrupt: a turn under way stops, short of its end, within the time a turn may take.
    let stop_me = format!("stop-{n}");
    let stop_text = format!("{} {stop_me}", vm_e2e::task("VM_E2E_STOPPED_PROMPT", kind, &n, vm_e2e::stub_stopped_turn()));
    call(f, "agents_send", json!({"agent": id, "text": stop_text, "mode": "queue"}))?;
    pace.until(f, &id, vm_e2e::Asked::Message(&stop_me), &format!("{what}: its turn to stop under way"), |i, e| under_way(i, e, &stop_me))?;
    call(f, "agents_interrupt", json!({"agent": id}))?;
    let (_, e) = pace.until(f, &id, vm_e2e::Asked::Stop { said: None }, &format!("{what}: stopping its turn"), |i, _| vm_e2e::status(i) == "idle")?;
    anyhow::ensure!(!stub || !vm_e2e::said_marker(&said_after(&e, &stop_me), &marker), "{what}: the interrupted turn ran to its end");

    // Stop: ended for good, its conversation kept to take up again.
    let hs = vm_e2e::info(f, &id)?.get("harness_session").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("{what}: kiln knows no harness session of {id}"))?;
    let ended = call(f, "agents_stop", json!({"agent": id}))?;
    anyhow::ensure!(ended.get("ended").and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| x.as_str() == Some(id.as_str()))), "{what}: agents_stop said {ended}");
    pace.unlisted(f, &id, &format!("{what}: {id} still listed after agents_stop"))?;

    // Resume: a new agent takes the conversation up again, in a microVM from the same Agentfile.
    let again = vm_e2e::task("VM_E2E_RESUMED_PROMPT", kind, &n, format!("do the task again {n}"));
    let resumed = started(&call(f, "agents_start", json!({"kind": kind, "on": f.machine, "cwd": project, "prompt": again, "image": image, "resume": hs}))?)?;
    let (i, _) = pace.until(f, &resumed, vm_e2e::Asked::Opening, &format!("{what}: taking its conversation up again"), done)?;
    anyhow::ensure!(i.get("harness_session").and_then(Value::as_str) == Some(hs.as_str()), "{what}: the resumed agent's harness session is {}, not {hs}", i.get("harness_session").unwrap_or(&Value::Null));
    anyhow::ensure!(i.pointer("/isolation/image").and_then(Value::as_str) == Some(image), "{what}: resumed outside its Agentfile: {}", i.get("isolation").unwrap_or(&Value::Null));
    call(f, "agents_stop", json!({"agent": resumed}))?;
    Ok(())
}

/// Each harness through kiln's MCP tools, from kiln's Agentfile and from a person's own; every
/// failure said at the end.
#[test]
fn s35_every_harness_in_a_microvm_through_kilns_mcp_tools() -> anyhow::Result<()> {
    let gaps = super::gaps()?;
    let dir = tempfile::tempdir()?;
    let f = vm_e2e::up(dir.path(), "vm", &[])?;
    if !vm_e2e::isolation_ready(&f)? {
        return Ok(());
    }
    let project = vm_e2e::project(dir.path())?;
    let mut failed = Vec::new();
    for kind in super::asked_kinds() {
        if !super::tried(&gaps, kind, "linux_build") {
            continue;
        }
        if !super::tried(&gaps, kind, "resume") {
            super::say_gaps(&gaps, kind, "resume");
        }
        // A vendor's CLI, from kiln's Agentfile or a person's own made from it, takes ROOT's task alone.
        if let Some(why) = super::untasked(kind, &vm_e2e::builtin_image(kind)) {
            super::held_back(why, &mut failed);
            continue;
        }
        let mut images = vec![vm_e2e::builtin_image(kind)];
        match super::rest::users_own(&f, kind) {
            Ok(Some(own)) => images.push(own),
            Ok(None) => eprintln!("SKIP: {kind} from a person's own Agentfile through MCP: the stand-in's image is asked for and no text of a person's is named"),
            Err(e) => failed.push(format!("{kind}: keeping a person's own Agentfile: {e:#}")),
        }
        for image in images {
            if let Err(e) = drive(&f, kind, &image, &project) {
                failed.push(format!("{e:#}"));
            }
        }
    }
    anyhow::ensure!(failed.is_empty(), "{} of the drives failed:\n{}", failed.len(), failed.join("\n\n"));
    Ok(())
}
