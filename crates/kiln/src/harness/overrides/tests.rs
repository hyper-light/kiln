//! Kiln plan 01a1dd2e…, P6b.4's acceptance, through kiln's operations as every surface calls them (`crate::ops::call`:
//! REST's `POST /api/ops/<name>` and its own routes, MCP's tools and code mode's `kiln.*` all run it): sessions on the
//! server's own machine, their harness a stand-in for Claude Code that writes down each start (its arguments, its pid)
//! and each control request it's sent; a microVM session's microVM the stand-in runtime (`sandbox::fake::FakeVms`),
//! which records each change its run takes. What's checked is what a person and a client see: `agents.get`, what the
//! harness was told, and what the microVM took.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::harness::model::Command;
use crate::harness::session::{Isolation, Spec};
use crate::hub::Hub;
use crate::ops::{Cx, call};

/// Runs `f` on kiln's runtime, a hang's bound around it, never a pace: a loaded machine takes what time it takes.
fn run(f: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static) -> anyhow::Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::rt::block_on(f));
    });
    match rx.recv_timeout(Duration::from_secs(600)) {
        Ok(r) => r,
        Err(_) => anyhow::bail!("the operations hung"),
    }
}

/// A scratch server whose own machine runs sessions, as `harness::ops`' tests make one.
async fn hub() -> anyhow::Result<Arc<Hub>> {
    let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
    let h = Arc::clone(&hub);
    crate::rt::spawn(async move {
        let _ = crate::harness::registry::Registry::run_local(h).await;
    });
    let h = Arc::clone(&hub);
    crate::rt::spawn(async move {
        let _ = crate::harness::mail_delivery::run(h).await;
    });
    hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
    Ok(hub)
}

/// A stand-in for Claude Code in `dir`, and the file it writes to: a line for each start (`start`, its arguments past
/// its program, its pid) and for each control request (`control`, the request), answered as Claude Code answers them,
/// and each message a turn of its own.
#[cfg(unix)]
fn stand_in(dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    let said = dir.join("said.jsonl");
    let script = dir.join("stand-in.py");
    std::fs::write(&script, r#"import json, os, sys, uuid
log, args = sys.argv[1], sys.argv[2:]
if "--version" in args:
    print("2.1.296 (Claude Code)")
    sys.exit(0)
def note(kind, value):
    with open(log, "a") as f:
        f.write(json.dumps({"kind": kind, "pid": os.getpid(), "value": value}) + "\n")
        f.flush()
def out(value):
    print(json.dumps(value), flush=True)
note("start", args)
session = str(uuid.uuid4())
for flag in ("--session-id", "--resume"):
    if flag in args and args.index(flag) + 1 < len(args):
        session = args[args.index(flag) + 1]
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("type") == "control_request":
        req = m.get("request") or {}
        note("control", req)
        resp = {}
        if req.get("subtype") == "initialize":
            resp = {"commands": [], "models": [], "agents": [], "current_permission_mode": "default", "claude_code_version": "2.1.296"}
        out({"type": "control_response", "response": {"subtype": "success", "request_id": m.get("request_id"), "response": resp}})
    elif m.get("type") == "user":
        uid = m.get("uuid")
        note("user", m.get("message"))
        out({"type": "command_lifecycle", "command_uuid": uid, "state": "started", "session_id": session})
        out({"type": "system", "subtype": "init", "cwd": os.getcwd(), "session_id": session, "tools": [], "mcp_servers": [], "model": "fixture", "permissionMode": "default", "claude_code_version": "2.1.296"})
        out({"type": "result", "subtype": "success", "is_error": False, "duration_ms": 1, "num_turns": 1, "result": "done", "session_id": session, "total_cost_usd": 0, "usage": {"input_tokens": 1, "output_tokens": 1}})
        out({"type": "command_lifecycle", "command_uuid": uid, "state": "completed", "session_id": session})
"#)?;
    let program = dir.join("claude");
    std::fs::write(&program, format!("#!/bin/sh\nexec python3 {} {} \"$@\"\n", script.display(), said.display()))?;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
    Ok((program, said))
}

/// Whether session `s`'s harness is up and at rest.
fn at_rest(s: &crate::harness::session::Session) -> bool {
    let core = s.core.lock();
    !core.meta.features.is_empty() && core.state.status == crate::harness::model::Status::Idle
}

/// What the stand-in wrote down, of `kind`: each line's pid and value.
fn said(log: &Path, kind: &str) -> Vec<(u64, Value)> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("kind").and_then(Value::as_str) == Some(kind))
        .map(|v| (v.get("pid").and_then(Value::as_u64).unwrap_or(0), v.get("value").cloned().unwrap_or(Value::Null)))
        .collect()
}

/// The folders the stand-in was last told through Claude Code's flag settings (`permissions.additionalDirectories`).
fn told_dirs(log: &Path) -> Option<Value> {
    said(log, "control").into_iter().rev().find_map(|(_, r)| (r.get("subtype").and_then(Value::as_str) == Some("apply_flag_settings")).then(|| r.pointer("/settings/permissions/additionalDirectories").cloned()).flatten())
}

/// Waits for `check`, polling (a hang's bound is [`run`]'s).
async fn until(what: &str, mut check: impl FnMut() -> bool) -> anyhow::Result<()> {
    for _ in 0..12_000 {
        if check() {
            return Ok(());
        }
        crate::rt::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("never: {what}")
}

/// `agents.get` of `id`, as a person calls it.
async fn get(hub: &Arc<Hub>, id: &str) -> anyhow::Result<Value> {
    call(owner(hub), "agents.get", json!({"agent": id})).await.map_err(|(s, m)| anyhow::anyhow!("agents.get: {s} {m}"))
}

fn owner(hub: &Arc<Hub>) -> Cx {
    Cx { hub: Arc::clone(hub), caller: crate::authz::Caller::owner() }
}

/// Waits for `agents.get` of `id` to show what `check` looks for.
async fn shows(hub: &Arc<Hub>, id: &str, what: &str, check: impl Fn(&Value) -> bool) -> anyhow::Result<Value> {
    for _ in 0..12_000 {
        let v = get(hub, id).await?;
        if check(&v) {
            return Ok(v);
        }
        crate::rt::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("agents.get never showed {what}: {}", get(hub, id).await?)
}

/// A Claude Code session on the server's own machine, the stand-in its harness, opened and at rest.
async fn opened(hub: &Arc<Hub>, id: &str, dir: &Path, program: &Path, isolation: Option<Isolation>) -> anyhow::Result<()> {
    // An isolated one under the claim kiln's balancer would have placed it with (step 32).
    let claim = isolation.as_ref().map(|_| crate::reach::place::Claim { owner: "mac".into(), epoch: 1, run: format!("session-{id}"), fence: kiln_ci::sched::Fence::default() });
    // The most egress it may be granted live, as kiln's server sets it from the org's policy (`[agents] egress_ceiling`).
    let egress_ceiling = if isolation.is_some() { vec![CEILING.to_owned()] } else { Vec::new() };
    let spec = Spec { kind: "claude".into(), cwd: Some(dir.to_string_lossy().into_owned()), program: Some(program.to_string_lossy().into_owned()), isolation, claim, egress_ceiling, ..Spec::default() };
    let s = hub.harness.local.open(Some(id.to_owned()), spec)?;
    until("its harness at rest", || !s.core.lock().meta.features.is_empty() && s.core.lock().state.status == crate::harness::model::Status::Idle).await?;
    until("the server's copy of it", || hub.harness.info(id).is_some()).await
}

/// The org's policy the tests' microVM sessions start under: the most egress each may be granted live.
const CEILING: &str = "api.example.com:443";

/// The first held override of `list` (`ports`, `egress`, `networks`, `volumes`) in an `agents.get`, and how `place` took it.
fn took<'a>(v: &'a Value, list: &str, place: &str) -> Option<&'a str> {
    v.pointer(&format!("/state/overrides/{list}/0/{place}/took")).and_then(Value::as_str)
}

/// P6b.4 AC1: each operation is on every surface (REST's `GET /api/ops` and its route, MCP's tools, code mode's
/// `kiln.*`), each a change (not read-only, so the confirm rules for state changes hold it), those that take something
/// away destructive (MCP's `destructiveHint`), each its own route under its session's.
#[test]
fn p6b_4_ac1_the_ops_are_on_rest_mcp_and_code_mode_as_changes() -> anyhow::Result<()> {
    run(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let catalog = crate::ops::catalog(&hub);
        let tools = crate::mcp::tools(&hub);
        let code = crate::mcp::code::registry(&hub);
        for (name, destructive, tail) in [
            ("sessions.ports.publish", false, "ports"),
            ("sessions.ports.unpublish", true, "ports/unpublish"),
            ("sessions.egress.grant", false, "egress"),
            ("sessions.egress.revoke", true, "egress/revoke"),
            ("sessions.networks.connect", false, "networks"),
            ("sessions.networks.disconnect", true, "networks/disconnect"),
            ("sessions.volumes.add", false, "volumes"),
            ("sessions.volumes.remove", true, "volumes/remove"),
        ] {
            let listed = catalog.get("operations").and_then(Value::as_array).and_then(|a| a.iter().find(|o| o.get("name") == Some(&json!(name)))).ok_or_else(|| anyhow::anyhow!("{name} isn't in GET /api/ops"))?;
            anyhow::ensure!(listed.get("read_only") == Some(&json!(false)) && listed.get("destructive") == Some(&json!(destructive)), "{name}: {listed}");
            anyhow::ensure!(listed.pointer("/input/properties/agent").is_some(), "{name} names its session: {listed}");
            let tool = tools.iter().find(|t| t.get("name") == Some(&json!(crate::mcp::tool_name(name)))).ok_or_else(|| anyhow::anyhow!("{name} is no MCP tool"))?;
            anyhow::ensure!(tool.pointer("/annotations/readOnlyHint") == Some(&json!(false)), "{name}: {tool}");
            anyhow::ensure!(code.as_array().is_some_and(|a| a.iter().any(|o| o.get("call") == Some(&json!(format!("kiln.{name}"))))), "{name} isn't kiln.{name} in code mode");
            let op = crate::ops::own().iter().find(|o| o.name == name).ok_or_else(|| anyhow::anyhow!("{name} isn't kiln's own"))?;
            let (method, segs) = (op.route)(&json!({"agent": "a1"})).map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            anyhow::ensure!(method == http::Method::POST && segs.join("/") == format!("api/harness/sessions/a1/{tail}"), "{name}: {method} /{}", segs.join("/"));
        }
        Ok(())
    })
}

/// P6b.4 AC2: a session on its machine's host is offered only its volumes' operations (`agents.get`'s `live`), and
/// each network operation is refused, naming its engine, nothing changed; a microVM session is offered them all.
#[cfg(unix)]
#[test]
fn p6b_4_ac2_a_host_session_refuses_network_changes_naming_its_engine() -> anyhow::Result<()> {
    run(async {
        let hub = hub().await?;
        let dir = tempfile::tempdir()?;
        let (program, _log) = stand_in(&dir.path().join("h"))?;
        opened(&hub, "host-net", dir.path(), &program, None).await?;
        let v = get(&hub, "host-net").await?;
        anyhow::ensure!(v.pointer("/live/engine") == Some(&json!("host")), "{v}");
        anyhow::ensure!(v.pointer("/live/ops") == Some(&json!(["sessions.volumes.add", "sessions.volumes.remove"])), "only its volumes': {v}");
        for (name, input) in [
            ("sessions.ports.publish", json!({"agent": "host-net", "bind": "127.0.0.1", "host_port": 8080, "guest_port": 3000, "protocol": "tcp"})),
            ("sessions.ports.unpublish", json!({"agent": "host-net", "bind": "127.0.0.1", "host_port": 8080, "protocol": "tcp"})),
            ("sessions.egress.grant", json!({"agent": "host-net", "host": "api.example.com", "port": 443})),
            ("sessions.egress.revoke", json!({"agent": "host-net", "host": "api.example.com", "port": 443})),
            ("sessions.networks.connect", json!({"agent": "host-net", "network": "team"})),
            ("sessions.networks.disconnect", json!({"agent": "host-net", "network": "team"})),
        ] {
            let refused = call(owner(&hub), name, input).await.err().ok_or_else(|| anyhow::anyhow!("{name} was taken on a host session"))?;
            anyhow::ensure!(refused.0 == http::StatusCode::CONFLICT && refused.1.contains("engine host") && refused.1.contains("microvm"), "{name}: {refused:?}");
        }
        let after = get(&hub, "host-net").await?;
        anyhow::ensure!(after.pointer("/state/overrides").is_none_or(|o| o.as_object().is_none_or(|o| o.is_empty())), "nothing changed: {after}");
        // The same session's runner refuses one sent by the generic command route too, in its timeline, nothing kept.
        let generic = Command::Override { change: super::Change::Connect(super::Network { member: super::Member::Harness("claude".into()), name: "team".into() }) };
        let _ = hub.harness.command(&hub, "host-net", generic).await;
        let session = hub.harness.local.get("host-net").ok_or_else(|| anyhow::anyhow!("no session"))?;
        until("its refusal in its timeline", || session.core.lock().events().any(|e| matches!(&e.body, crate::harness::model::Body::Notice { text, .. } if text.contains("engine host")))).await?;
        anyhow::ensure!(session.core.lock().state.overrides.is_empty(), "nothing kept");
        anyhow::ensure!(hub.harness.close(&hub, "host-net").await, "it closed");
        Ok(())
    })
}

/// P6b.4 AC3 on the host: a folder given a running Claude Code session joins its harness's allowed folders live (its
/// flag settings' `permissions.additionalDirectories`, the same process: no relaunch) and its scope; `agents.get`
/// shows it and how each place took it. Started again (by force here), its harness is given it as it starts
/// (`--add-dir`), and the session still holds it. Taken back, its harness is told and the session holds none.
#[cfg(unix)]
#[test]
fn p6b_4_ac3_a_volume_on_a_host_session_joins_its_harnesss_folders_and_its_scope() -> anyhow::Result<()> {
    run(async {
        let hub = hub().await?;
        let dir = tempfile::tempdir()?;
        let (program, log) = stand_in(&dir.path().join("h"))?;
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data)?;
        let data = data.to_string_lossy().into_owned();
        opened(&hub, "host-vol", dir.path(), &program, None).await?;
        let first = said(&log, "start");
        anyhow::ensure!(first.len() == 1, "{first:?}");
        let pid = first.first().map(|(p, _)| *p).unwrap_or(0);
        let answer = call(owner(&hub), "sessions.volumes.add", json!({"agent": "host-vol", "path": data, "access": "rw"})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
        anyhow::ensure!(answer.get("taken") == Some(&json!(true)) && answer.get("engine") == Some(&json!("host")), "{answer}");
        let v = shows(&hub, "host-vol", "its volume, taken", |v| took(v, "volumes", "harness") == Some("now") && took(v, "volumes", "scope") == Some("now")).await?;
        anyhow::ensure!(v.pointer("/state/overrides/volumes/0/volume/path") == Some(&json!(data)) && v.pointer("/state/overrides/volumes/0/volume/at") == Some(&json!(data)), "{v}");
        anyhow::ensure!(v.pointer("/state/overrides/volumes/0/microvm").is_none(), "no microVM on the host: {v}");
        // Its harness's effective config: told the folder live, in the same process.
        until("its harness told", || told_dirs(&log) == Some(json!([data.clone()]))).await?;
        anyhow::ensure!(said(&log, "start").len() == 1 && said(&log, "control").iter().all(|(p, _)| *p == pid), "no relaunch: {:?}", said(&log, "start"));
        // Given again as it starts: a restart (by force) starts it with the folder as its own.
        let _ = hub.harness.command(&hub, "host-vol", Command::ForceStop).await;
        until("its harness started again", || said(&log, "start").len() >= 2).await?;
        let again = said(&log, "start");
        let args = again.last().map(|(_, a)| a.clone()).unwrap_or_default();
        let args: Vec<&str> = args.as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        anyhow::ensure!(args.windows(2).any(|w| w == ["--add-dir", data.as_str()]), "{args:?}");
        let v = shows(&hub, "host-vol", "its volume, still held", |v| took(v, "volumes", "harness") == Some("now")).await?;
        anyhow::ensure!(v.pointer("/state/overrides/volumes/0/volume/path") == Some(&json!(data)), "{v}");
        // Taken back.
        call(owner(&hub), "sessions.volumes.remove", json!({"agent": "host-vol", "path": data})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
        shows(&hub, "host-vol", "no volume", |v| v.pointer("/state/overrides/volumes").is_none()).await?;
        until("its harness told none", || told_dirs(&log) == Some(json!([]))).await?;
        anyhow::ensure!(hub.harness.close(&hub, "host-vol").await, "it closed");
        Ok(())
    })
}

/// The stand-in runtime's microVMs on the server's own machine, kiln's Agentfile for Claude Code's image already in
/// their store, its member the stand-in.
#[cfg(unix)]
fn microvms(hub: &Hub, dir: &Path, program: &Path) -> anyhow::Result<Arc<crate::sandbox::fake::FakeVms>> {
    let claude = crate::agentfiles::builtin::ALL.iter().find(|b| b.harness == "claude").ok_or_else(|| anyhow::anyhow!("kiln ships no Agentfile for Claude Code"))?;
    let (images, pinned) = crate::sandbox::image::tests::holding(dir, crate::sandbox::image::Source::Builtin(claude))?;
    let vms = Arc::new(crate::sandbox::fake::FakeVms::with(&pinned, "claude", program));
    hub.harness.local.hold_isolated_in(crate::sandbox::Microvms::new(Arc::clone(&vms) as Arc<dyn kiln_seams::Sandbox>, images));
    Ok(vms)
}

/// P6b.4 AC1, AC2 and AC3 in a microVM (the person: "If MicroVMs, it adds the volume to the microvm(s) for that
/// session, grants the permissions specified, and adds it to the harness's/agent's allow directories and session's
/// scope"): a port published, egress granted, a network joined and a folder plugged in, each taken by the session's
/// microVM live (the stand-in runtime records each on the same microVM, none launched again) and its harness told the
/// folder in the same process; `agents.get` offers every operation there and shows each override as its microVM took
/// it. Started again (by force), a new run comes back with all four given again, its harness given the folder as it
/// starts. Each is then taken back, the microVM letting go of each.
#[cfg(unix)]
#[test]
fn p6b_4_a_microvm_session_takes_each_change_live_and_again_as_it_starts() -> anyhow::Result<()> {
    use kiln_seams::sandbox::{Change as Seam, Member as SeamMember, PlugFor, PlugSource};
    run(async {
        let hub = hub().await?;
        let dir = tempfile::tempdir()?;
        let (program, log) = stand_in(&dir.path().join("h"))?;
        let vms = microvms(&hub, dir.path(), &program)?;
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data)?;
        let data = data.to_string_lossy().into_owned();
        opened(&hub, "vm-live", dir.path(), &program, Some(Isolation::microvm())).await?;
        // What its launch declared (Shards' ImageSpec::plug_roots and egress_ceiling, D271): its project's top-level
        // directory and its session's volume root, and the org's ceiling.
        let cwd = dir.path().to_string_lossy().into_owned();
        let roots: Vec<String> = crate::sandbox::isolated::plug_root_of(&cwd).into_iter().chain([crate::sandbox::isolated::VOLUMES.to_owned()]).collect();
        let v = shows(&hub, "vm-live", "what its launch declared", |v| v.pointer("/state/overrides/roots").is_some()).await?;
        anyhow::ensure!(v.pointer("/live/engine") == Some(&json!("microvm")) && v.pointer("/live/ops").and_then(Value::as_array).is_some_and(|o| o.len() == 8), "every operation offered: {v}");
        anyhow::ensure!(v.pointer("/state/overrides/roots") == Some(&json!(roots)) && v.pointer("/state/overrides/ceiling") == Some(&json!([CEILING])), "{v}");
        let launched = vms.launched();
        anyhow::ensure!(launched.first().is_some_and(|l| l.plug_roots == roots && l.egress_ceiling == [CEILING]), "declared at its launch: {launched:?}");
        let member = || SeamMember("claude".into());
        let ask = |name: &'static str, input: Value| {
            let cx = owner(&hub);
            async move { call(cx, name, input).await.map_err(|(s, m)| anyhow::anyhow!("{name}: {s} {m}")) }
        };
        ask("sessions.ports.publish", json!({"agent": "vm-live", "bind": "127.0.0.1", "host_port": 8080, "guest_port": 3000, "protocol": "tcp"})).await?;
        ask("sessions.egress.grant", json!({"agent": "vm-live", "host": "API.example.com", "port": 443})).await?;
        ask("sessions.networks.connect", json!({"agent": "vm-live", "network": "team"})).await?;
        ask("sessions.volumes.add", json!({"agent": "vm-live", "path": data, "access": "ro"})).await?;
        // Past what its launch declared: egress past its ceiling, a folder outside its plug roots with nothing restarted.
        let past = call(owner(&hub), "sessions.egress.grant", json!({"agent": "vm-live", "host": "elsewhere.example.com", "port": 443})).await.err().ok_or_else(|| anyhow::anyhow!("egress past its ceiling was taken"))?;
        anyhow::ensure!(past.0 == http::StatusCode::CONFLICT && past.1.contains("past the most egress"), "{past:?}");
        let outside = call(owner(&hub), "sessions.volumes.add", json!({"agent": "vm-live", "path": data, "access": "ro", "at": "/opt/data"})).await.err().ok_or_else(|| anyhow::anyhow!("a folder outside its plug roots was taken"))?;
        anyhow::ensure!(outside.0 == http::StatusCode::CONFLICT && outside.1.contains("outside every folder") && outside.1.contains(crate::sandbox::isolated::VOLUMES), "{outside:?}");
        // Beneath its session's volume root by default, plugged as Shards' ImageGuest::plug takes one.
        let at = format!("{}/data", crate::sandbox::isolated::VOLUMES);
        let plug = Seam::Plug { source: PlugSource::Host(data.clone()), to: PlugFor::Members(vec![member()]), at: at.clone(), read_only: true };
        let all = vec![
            Seam::Publish { guest_port: 3000, udp: false, host: Some("127.0.0.1".into()), host_port: 8080 },
            Seam::Grant { member: member(), host: "api.example.com".into(), port: 443 },
            Seam::Connect { member: member(), network: "team".into() },
            plug.clone(),
        ];
        let on = |vm: &str| vms.changed().into_iter().filter(|(on, _)| on == vm).map(|(_, c)| c).collect::<Vec<_>>();
        until("its microVM took all four", || on("fake-vm-1") == all).await?;
        let v = shows(&hub, "vm-live", "each override, taken by its microVM", |v| ["ports", "egress", "networks", "volumes"].iter().all(|l| took(v, l, "microvm") == Some("now"))).await?;
        anyhow::ensure!(took(&v, "volumes", "harness") == Some("now") && took(&v, "volumes", "scope") == Some("now"), "{v}");
        anyhow::ensure!(v.pointer("/state/overrides/volumes/0/volume/access") == Some(&json!("ro")), "{v}");
        anyhow::ensure!(v.pointer("/state/overrides/volumes/0/volume/at") == Some(&json!(at)), "{v}");
        // In place: the same microVM, none launched again, and its harness the same process, told the folder where it sees it.
        until("its harness told the folder", || told_dirs(&log) == Some(json!([at.clone()]))).await?;
        anyhow::ensure!(vms.launched().len() == 1 && said(&log, "start").len() == 1, "one microVM, one harness: {:?}", said(&log, "start"));
        anyhow::ensure!(matches!(hub.harness.info("vm-live").and_then(|i| i.runs), Some(crate::harness::session::Runs::Microvm { vm: Some(ref vm), .. }) if vm == "fake-vm-1"), "the same box");
        // Started again (by force): its next run takes all four again, its harness the folder as it starts.
        let _ = hub.harness.command(&hub, "vm-live", Command::ForceStop).await;
        until("its second run took all four", || on("fake-vm-2") == all).await?;
        let resumed = vms.launched();
        let args = resumed.get(1).map(|l| l.args.clone()).unwrap_or_default();
        anyhow::ensure!(args.windows(2).any(|w| w == ["--add-dir", at.as_str()]), "{args:?}");
        shows(&hub, "vm-live", "each override, held again", |v| ["ports", "egress", "networks", "volumes"].iter().all(|l| took(v, l, "microvm") == Some("now"))).await?;
        // Each taken back: its microVM lets go of each.
        ask("sessions.ports.unpublish", json!({"agent": "vm-live", "bind": "127.0.0.1", "host_port": 8080, "protocol": "tcp"})).await?;
        ask("sessions.egress.revoke", json!({"agent": "vm-live", "host": "api.example.com", "port": 443})).await?;
        ask("sessions.networks.disconnect", json!({"agent": "vm-live", "network": "team"})).await?;
        ask("sessions.volumes.remove", json!({"agent": "vm-live", "path": data})).await?;
        let gone = vec![
            Seam::Unpublish { udp: false, host: "127.0.0.1:8080".into() },
            Seam::Revoke { member: member(), host: "api.example.com".into(), port: 443 },
            Seam::Disconnect { member: member(), network: "team".into() },
            Seam::Unplug { at: at.clone() },
        ];
        until("its microVM let go of each", || on("fake-vm-2").get(4..) == Some(gone.as_slice())).await?;
        shows(&hub, "vm-live", "none held", |v| v.pointer("/state/overrides/ports").is_none() && v.pointer("/state/overrides/volumes").is_none()).await?;
        anyhow::ensure!(hub.harness.close(&hub, "vm-live").await, "it closed");
        Ok(())
    })
}

/// P5.2's page (the base beside the live overrides): `sessions.grants` says what each session's launch gave it as it
/// started. A host session: its working folder, the folder its harness is allowed, nothing narrowing its egress, no
/// microVM's plug roots or ceiling. A microVM session: its folder and the machine's folders it sees as its runner
/// launched them (each where it sees it, and whether it may write it), the folders its harness is allowed, its plug
/// roots and egress ceiling as its launch declared them, what it reaches past its microVM, and no port or network,
/// none being given at a launch; a port published live shows among its live overrides, its base unchanged. A session
/// kiln doesn't run is 404. Dropping the runner's record of its launch (`granted`) leaves its base unsaid
/// (`reported` false), which the microVM half fails on.
#[cfg(unix)]
#[test]
fn p5_2_a_sessions_grants_are_what_its_launch_gave_beside_its_live_overrides() -> anyhow::Result<()> {
    run(async {
        let hub = hub().await?;
        let dir = tempfile::tempdir()?;
        let (program, _log) = stand_in(&dir.path().join("h"))?;
        let ask = |name: &'static str, input: Value| {
            let cx = owner(&hub);
            async move { call(cx, name, input).await.map_err(|(s, m)| anyhow::anyhow!("{name}: {s} {m}")) }
        };
        let cwd = dir.path().to_string_lossy().into_owned();

        opened(&hub, "host-base", dir.path(), &program, None).await?;
        // Its folder as kiln holds it (resolved as its session resolved it).
        let cwd = hub.harness.info("host-base").map(|i| i.cwd).unwrap_or(cwd);
        let g = ask("sessions.grants", json!({"agent": "host-base"})).await?;
        anyhow::ensure!(g.get("engine") == Some(&json!("host")) && g.pointer("/base/folder") == Some(&json!(cwd)) && g.pointer("/base/allowed_dirs") == Some(&json!([cwd])), "{g}");
        anyhow::ensure!(g.pointer("/base/egress") == Some(&json!({"as": "host"})) && g.pointer("/base/plug_roots") == Some(&json!([])) && g.pointer("/base/egress_ceiling") == Some(&json!([])), "{g}");

        let vms = microvms(&hub, dir.path(), &program)?;
        opened(&hub, "vm-base", dir.path(), &program, Some(Isolation::microvm())).await?;
        shows(&hub, "vm-base", "what its launch declared", |v| v.pointer("/state/overrides/roots").is_some() && v.pointer("/info/runs/granted").is_some()).await?;
        let launched = vms.launched();
        let launch = launched.first().ok_or_else(|| anyhow::anyhow!("nothing was launched"))?;
        let g = ask("sessions.grants", json!({"agent": "vm-base"})).await?;
        anyhow::ensure!(g.get("engine") == Some(&json!("microvm")) && g.pointer("/base/reported") == Some(&json!(true)), "its runner said its base: {g}");
        anyhow::ensure!(g.pointer("/base/folder") == Some(&json!(launch.folder)), "its folder as launched: {g}");
        let volumes: Vec<Value> = launch.shares.iter().map(|s| json!({"host": s.host, "at": s.at, "writable": s.writable})).collect();
        anyhow::ensure!(g.pointer("/base/volumes") == Some(&json!(volumes)) && !volumes.is_empty(), "the folders it sees, as launched: {g}");
        let allowed = g.pointer("/base/allowed_dirs").and_then(Value::as_array).cloned().unwrap_or_default();
        anyhow::ensure!(launch.folder.iter().chain(launch.shares.iter().map(|s| &s.at)).all(|d| allowed.contains(&json!(d))), "its harness allowed each: {g}");
        anyhow::ensure!(g.pointer("/base/plug_roots") == Some(&json!(launch.plug_roots)) && g.pointer("/base/egress_ceiling") == Some(&json!([CEILING])), "{g}");
        anyhow::ensure!(g.pointer("/base/egress").is_some_and(|e| !e.is_null()) && g.pointer("/base/ports") == Some(&json!([])) && g.pointer("/base/networks") == Some(&json!([])), "{g}");
        ask("sessions.ports.publish", json!({"agent": "vm-base", "bind": "127.0.0.1", "host_port": 8081, "guest_port": 3000, "protocol": "tcp"})).await?;
        shows(&hub, "vm-base", "its port, held", |v| v.pointer("/state/overrides/ports/0").is_some()).await?;
        let g = ask("sessions.grants", json!({"agent": "vm-base"})).await?;
        anyhow::ensure!(g.pointer("/live/ports/0").is_some() && g.pointer("/base/ports") == Some(&json!([])) && g.pointer("/live/roots").is_none(), "live beside an unchanged base: {g}");

        let gone = call(owner(&hub), "sessions.grants", json!({"agent": "nobody"})).await.err().ok_or_else(|| anyhow::anyhow!("a session kiln doesn't run answered"))?;
        anyhow::ensure!(gone.0 == http::StatusCode::NOT_FOUND, "{gone:?}");
        for id in ["host-base", "vm-base"] {
            anyhow::ensure!(hub.harness.close(&hub, id).await, "{id} closed");
        }
        Ok(())
    })
}

/// P6b.4 AC3: a folder given read-only is the session's scope for reading, and refused for changing, by kiln at each
/// call it holds (`hold::verdict`); a folder given read-write is inside the session's folder for its file calls.
#[test]
fn p6b_4_ac3_a_volume_is_the_sessions_scope_as_its_access_says() {
    use crate::harness::model::{Perms, ToolKind};
    let mut core = crate::harness::core::Core::new();
    core.state.cwd = "/work/proj".into();
    let volume = |path: &str, access| super::HeldVolume {
        volume: super::Volume { path: path.into(), at: path.into(), access, members: vec![super::Member::Harness("claude".into())] },
        harness: super::Took::Now { how: String::new() },
        scope: super::Took::Now { how: String::new() },
        microvm: None,
    };
    core.state.overrides.volumes = vec![volume("/data", super::Access::Rw), volume("/docs", super::Access::Ro)];
    let verdict = |core: &crate::harness::core::Core, kind, path: &str| crate::harness::hold::verdict(core, None, kind, &[path]).map(|(v, _)| v);
    // No perms of its own: a read-only folder's changes are refused all the same; reads go.
    assert_eq!(verdict(&core, ToolKind::Edit, "/docs/a.md"), Some("deny"));
    assert_eq!(verdict(&core, ToolKind::Read, "/docs/a.md"), None);
    assert_eq!(verdict(&core, ToolKind::Edit, "/data/a.csv"), None);
    // Held to its folder: the folders it was given are inside its scope, others not.
    core.state.perms = Some(Perms { folder: Some("/work/proj".into()), ..Perms::default() });
    assert_eq!(verdict(&core, ToolKind::Edit, "/data/a.csv"), None, "a read-write folder given it");
    assert_eq!(verdict(&core, ToolKind::Read, "/docs/a.md"), None, "a read-only one, for reading");
    assert_eq!(verdict(&core, ToolKind::Edit, "/docs/a.md"), Some("deny"), "and not for changing");
    assert_eq!(verdict(&core, ToolKind::Read, "/etc/passwd"), Some("deny"), "nothing else");
    assert_eq!(verdict(&core, ToolKind::Edit, "/data-not/x"), Some("deny"), "a sibling isn't inside it");
}

/// The record: a change held already as asked changes nothing; one naming what it holds otherwise is refused, saying
/// what it holds; a removal of what it doesn't hold is refused; the revision counts what it took. Network changes are a
/// microVM's, refused on the host naming its engine; a host session's volume is its harness's, at its own path.
#[test]
fn p6b_4_the_record_takes_each_change_once_and_refuses_what_it_cant() -> anyhow::Result<()> {
    use super::*;
    let harness = || Member::Harness("claude".into());
    let port = Port { bind: "127.0.0.1".into(), host_port: 8080, guest_port: 3000, protocol: Protocol::Tcp };
    let mut o = Overrides::default();
    assert_eq!(take(&mut o, &Change::Publish(port.clone())).map_err(anyhow::Error::msg)?, Taken::Added);
    assert_eq!(take(&mut o, &Change::Publish(port.clone())).map_err(anyhow::Error::msg)?, Taken::Same);
    let elsewhere = Port { guest_port: 4000, ..port.clone() };
    assert!(take(&mut o, &Change::Publish(elsewhere)).is_err_and(|e| e.contains("published already")));
    assert_eq!(o.revision, 1);
    let key = PortKey { bind: "127.0.0.1".into(), host_port: 8080, protocol: Protocol::Tcp };
    assert_eq!(take(&mut o, &Change::Unpublish(key.clone())).map_err(anyhow::Error::msg)?, Taken::Removed(Removed::Port(port.clone())));
    assert!(take(&mut o, &Change::Unpublish(key)).is_err_and(|e| e.contains("isn't published")));
    assert_eq!(o.revision, 2);
    for change in [Change::Publish(port), Change::Grant(Egress { member: harness(), host: "example.com".into(), port: 443 }), Change::Connect(Network { member: harness(), name: "team".into() })] {
        assert!(check(&change, false, "claude").is_err_and(|e| e.contains("engine host")), "{change:?}");
        assert!(check(&change, true, "claude").is_ok(), "{change:?}");
    }
    for bad in [
        Change::Grant(Egress { member: harness(), host: "-bad-.example".into(), port: 443 }),
        Change::Grant(Egress { member: harness(), host: "example.com".into(), port: 0 }),
        Change::Publish(Port { bind: "localhost".into(), host_port: 1, guest_port: 1, protocol: Protocol::Udp }),
        Change::Connect(Network { member: Member::Harness("codex".into()), name: "team".into() }),
        Change::Add(Volume { path: "relative/dir".into(), at: "relative/dir".into(), access: Access::Rw, members: vec![harness()] }),
    ] {
        assert!(check(&bad, true, "claude").is_err(), "{bad:?}");
    }
    let host_volume = Volume { path: "/data".into(), at: "/data".into(), access: Access::Ro, members: vec![harness()] };
    assert!(check(&Change::Add(host_volume.clone()), false, "claude").is_ok());
    assert!(check(&Change::Add(Volume { at: "/elsewhere".into(), ..host_volume.clone() }), false, "claude").is_err());
    assert!(check(&Change::Add(Volume { members: vec![Member::Mcp("db".into())], ..host_volume }), false, "claude").is_err_and(|e| e.contains("one member, its harness")));
    // What its microVM declared: a folder strictly beneath a plug root fits, as Shards plugs one (D271); egress within its
    // ceiling alone.
    let declared = Overrides { roots: vec!["/work".into(), "/volumes".into()], ceiling: vec!["api.example.com:443".into(), "[::1]:8080".into()], ..Overrides::default() };
    let vol = |at: &str| Change::Add(Volume { path: "/data".into(), at: at.into(), access: Access::Rw, members: vec![harness()] });
    assert!(fits(&vol("/volumes/data"), &declared).is_ok());
    assert!(fits(&vol("/work/proj/sub"), &declared).is_ok());
    assert!(fits(&vol("/volumes"), &declared).is_err(), "a root itself is no plug's place: strictly beneath one");
    assert!(fits(&vol("/volumes-other/data"), &declared).is_err(), "a sibling of a root isn't beneath it");
    assert!(fits(&vol("/opt/data"), &declared).is_err_and(|e| e.contains("outside every folder")));
    assert_eq!(crate::sandbox::isolated::plug_root_of("/Users/ada/proj").as_deref(), Some("/Users"));
    for refused in ["/tmp/x", "/proc/1", "/etc/kiln", "/run/x", "/dev/null", "/sys/x", "relative", "/"] {
        assert_eq!(crate::sandbox::isolated::plug_root_of(refused), None, "{refused}");
    }
    let grant = |host: &str, port| Change::Grant(Egress { member: harness(), host: host.into(), port });
    assert!(fits(&grant("API.example.com", 443), &declared).is_ok());
    assert!(fits(&grant("::1", 8080), &declared).is_ok());
    assert!(fits(&grant("api.example.com", 80), &declared).is_err_and(|e| e.contains("past the most egress")));
    assert!(fits(&grant("api.example.com", 443), &Overrides::default()).is_err_and(|e| e.contains("none")), "no ceiling declared: none live");
    assert!(fits(&Change::Remove("/data".into()), &Overrides::default()).is_ok(), "a removal always fits");
    assert_eq!(Member::read(None, "claude"), Ok(harness()));
    assert_eq!(Member::read(Some("agent:helper"), "claude"), Ok(Member::Agent("helper".into())));
    assert!(Member::read(Some("worker"), "claude").is_err());
    Ok(())
}

/// P6c.5 (the person: "right now on bare metal I can swap between claude code, codex, cursor, devin, antigravity,
/// opencode, pi, etc. with just a click. We need to keep that!"): the same one-click switch (`Command::SetModel` naming
/// another harness, as the composer's control sends it) in a microVM session: Claude Code to Codex and back, each harness
/// swapped into the same microVM (the stand-in runtime records each swap, none launched again), and the conversation
/// carried each time, so the person's first words reach the last harness. Codex's member is kiln's stand-in for every
/// harness's interface (`tests/fixtures/vm-stub`), in Codex's dialect.
#[cfg(unix)]
#[test]
fn p6c_5_a_one_click_harness_switch_in_a_microvm_keeps_its_microvm_and_its_conversation() -> anyhow::Result<()> {
    run(async {
        let hub = hub().await?;
        let dir = tempfile::tempdir()?;
        let (claude, log) = stand_in(&dir.path().join("h"))?;
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin)?;
        let codex = bin.join("codex");
        std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vm-stub/harness/stub"), &codex)?;
        let builtin = |h: &str| crate::agentfiles::builtin::ALL.iter().find(|b| b.harness == h).ok_or_else(|| anyhow::anyhow!("kiln ships no Agentfile for {h}"));
        let (_, claude_image) = crate::sandbox::image::tests::holding(dir.path(), crate::sandbox::image::Source::Builtin(builtin("claude")?))?;
        let (images, codex_image) = crate::sandbox::image::tests::holding(dir.path(), crate::sandbox::image::Source::Builtin(builtin("codex")?))?;
        let vms = Arc::new(crate::sandbox::fake::FakeVms::with(&claude_image, "claude", &claude).and(&codex_image, "codex", &codex).swapping());
        hub.harness.local.hold_isolated_in(crate::sandbox::Microvms::new(Arc::clone(&vms) as Arc<dyn kiln_seams::Sandbox>, images));
        opened(&hub, "vm-switch", dir.path(), &claude, Some(Isolation::microvm())).await?;
        let session = hub.harness.local.get("vm-switch").ok_or_else(|| anyhow::anyhow!("no session"))?;
        let say = |text: &'static str| {
            let cx = owner(&hub);
            async move { call(cx, "agents.send", json!({"agent": "vm-switch", "text": text})).await.map_err(|(s, m)| anyhow::anyhow!("agents.send: {s} {m}")) }
        };
        let heard = |words: &str| said(&log, "user").iter().filter(|(_, m)| m.to_string().contains(words)).count();
        let answers = || session.core.lock().events().filter(|e| matches!(e.body, crate::harness::model::Body::Assistant { .. })).count();
        let same_box = |vms: &crate::sandbox::fake::FakeVms| vms.launched().len() == 1 && vms.swapped().iter().all(|(vm, _)| vm == "fake-vm-1");
        say("Remember the word kumquat.").await?;
        until("Claude Code heard it", || heard("kumquat") == 1).await?;
        until("Claude Code at rest", || at_rest(&session)).await?;
        // One click: Codex, swapped into the same microVM.
        let _ = hub.harness.command(&hub, "vm-switch", Command::SetModel { model: String::new(), kind: Some("codex".into()) }).await;
        until("Codex, at rest", || session.info().kind == "codex" && at_rest(&session)).await?;
        anyhow::ensure!(same_box(&vms) && vms.swapped().len() == 1, "one microVM, its member swapped: {:?} {:?}", vms.launched().len(), vms.swapped());
        anyhow::ensure!(vms.swapped().first().is_some_and(|(_, l)| l.program == "codex" && l.boundary == kiln_seams::sandbox::Boundary::MicroVm { image: codex_image.clone() }), "Codex's member, of its own image: {:?}", vms.swapped());
        let before = answers();
        say("What comes next?").await?;
        until("Codex answered", || answers() > before && at_rest(&session)).await?;
        // And back to Claude Code, in the same microVM still: the conversation carried to it.
        let _ = hub.harness.command(&hub, "vm-switch", Command::SetModel { model: String::new(), kind: Some("claude".into()) }).await;
        until("Claude Code again, at rest", || session.info().kind == "claude" && at_rest(&session)).await?;
        anyhow::ensure!(same_box(&vms) && vms.swapped().len() == 2, "one microVM, swapped twice: {:?}", vms.swapped());
        say("What was the word?").await?;
        until("the first words, carried to the last harness", || said(&log, "user").iter().any(|(_, m)| m.to_string().contains("What was the word?") && m.to_string().contains("kumquat"))).await?;
        anyhow::ensure!(said(&log, "start").len() == 2, "Claude Code started twice, each in the one microVM: {:?}", said(&log, "start"));
        anyhow::ensure!(matches!(hub.harness.info("vm-switch").and_then(|i| i.runs), Some(crate::harness::session::Runs::Microvm { vm: Some(ref vm), .. }) if vm == "fake-vm-1"), "the same box");
        anyhow::ensure!(hub.harness.close(&hub, "vm-switch").await, "it closed");
        Ok(())
    })
}

/// P6c.5: where the machine's microVMs swap no harness in place (Hex's until its wire calls land), the switch is refused
/// as before, saying why, and the session goes on in its harness, in its microVM.
#[cfg(unix)]
#[test]
fn p6c_5_a_switch_where_microvms_swap_none_is_refused_and_the_session_goes_on() -> anyhow::Result<()> {
    run(async {
        let hub = hub().await?;
        let dir = tempfile::tempdir()?;
        let (program, _log) = stand_in(&dir.path().join("h"))?;
        let vms = microvms(&hub, dir.path(), &program)?;
        opened(&hub, "vm-noswap", dir.path(), &program, Some(Isolation::microvm())).await?;
        let session = hub.harness.local.get("vm-noswap").ok_or_else(|| anyhow::anyhow!("no session"))?;
        let _ = hub.harness.command(&hub, "vm-noswap", Command::SetModel { model: String::new(), kind: Some("codex".into()) }).await;
        until("its switch refused", || session.core.lock().events().any(|e| matches!(&e.body, crate::harness::model::Body::Notice { text, .. } if text == crate::sandbox::isolated::NO_SWITCH))).await?;
        anyhow::ensure!(session.info().kind == "claude" && vms.launched().len() == 1 && vms.swapped().is_empty(), "it goes on as it was");
        anyhow::ensure!(hub.harness.close(&hub, "vm-noswap").await, "it closed");
        Ok(())
    })
}
