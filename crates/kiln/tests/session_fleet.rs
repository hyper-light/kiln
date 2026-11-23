//! CP-T3.2. Independently enrolled nodes, real peer QUIC, and durable receipts across a process death.
#![cfg(unix)]

mod common;

use anyhow::{Context as _, ensure};
use kiln::convo::raft::GroupStatus;
use serde_json::{Value, json};
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

fn request(port: u16, method: &str, path: &str, token: &str, body: Option<&Value>) -> anyhow::Result<Value> {
    let key = if method == "POST" && path == "/api/harness/sessions" { body.and_then(|v| v.pointer("/ids/0")).and_then(Value::as_str) } else { None };
    let (code, text) = common::http_with_key(port, method, path, token, body, Duration::from_secs(30), key).with_context(||format!("{method} {path} on fixture port {port}"))?;
    ensure!((200..300).contains(&code), "{method} {path} returned {code}: {text}");
    Ok(serde_json::from_str(&text)?)
}

/// Deliver the POST and close before reading its reply, as a disconnected browser would.
fn lose_start_reply(port: u16, token: &str, key: &str, body: &Value) -> anyhow::Result<()> {
    let json = serde_json::to_vec(body)?;
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))?;
    write!(stream, "POST /api/harness/sessions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nIdempotency-Key: {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", json.len())?;
    stream.write_all(&json)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    Ok(())
}

/// Wait for the application receipt even if the socket streams history first.
fn socket_send(port: u16, admin: &str, session: &str, target: &str, command: &Value) -> anyhow::Result<Value> {
    use tungstenite::client::IntoClientRequest as _;
    let mut req = format!("ws://127.0.0.1:{port}/api/harness/sessions/{session}/ws").into_client_request()?;
    req.headers_mut().insert("authorization", format!("Bearer {admin}").parse()?);
    let stream = std::net::TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let (mut client, _) = tungstenite::client(req, stream).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let id = command.get("id").and_then(Value::as_str).context("missing command id")?;
    client.send(tungstenite::Message::text(json!({"t":"cmd", "id":id, "target":target, "c":command}).to_string()))?;
    let until = Instant::now() + Duration::from_secs(30);
    while Instant::now() < until {
        let frame = client.read()?;
        if !frame.is_text() { continue; }
        let value: Value = serde_json::from_str(frame.to_text()?)?;
        if value.get("id").and_then(Value::as_str) == Some(id) && matches!(value.get("t").and_then(Value::as_str), Some("ack" | "nack")) {
            ensure!(value.get("t").and_then(Value::as_str) == Some("ack"), "socket refused the message: {value}");
            return Ok(value);
        }
    }
    anyhow::bail!("no socket receipt for {id}")
}

fn enroll(dir: &Path, server: &common::Server, name: &str) -> anyhow::Result<String> {
    use kiln::util::{random_token, write_secret};
    let state = dir.join(format!("{name}-state"));
    std::fs::create_dir_all(&state)?;
    let url = format!("http://127.0.0.1:{}", server.port);
    let me = kiln::join::Machine {
        name: name.to_owned(), hostname: name.to_owned(), machine_id: Some(common::machine(name)),
        os: std::env::consts::OS.to_owned(), arch: std::env::consts::ARCH.to_owned(), labels: Vec::new(),
    };
    let stop = kiln::rt::Shutdown::new();
    let (cancel, dest) = (stop.clone(), state.clone());
    let (show, code) = std::sync::mpsc::channel();
    let enrollment = std::thread::spawn(move || kiln::rt::block_on(async move {
        kiln::join::request(&url, &me, &dest, &|message| {
            if let Some(code) = message.lines().find_map(|line| line.trim().strip_prefix("Verification code:")) {
                let _ = show.send(code.split_whitespace().collect::<String>());
            }
        }, &cancel).await
    }));
    let decision = (|| -> anyhow::Result<()> {
        let expected = code.recv_timeout(Duration::from_secs(90))?;
        let id = eventually("fixture enrollment was not confirmed", || {
            let requests = request(server.port, "GET", "/api/join-requests", &server.admin, None)?;
            requests.as_array().into_iter().flatten().find(|r| {
                r.get("name").and_then(Value::as_str) == Some(name) && r.get("code").and_then(Value::as_str) == Some(expected.as_str())
            }).and_then(|r| r.get("id")).and_then(Value::as_str).map(str::to_owned).context("no matching verified code")
        })?;
        let (status, _) = common::http(server.port, "POST", &format!("/api/join-requests/{id}/approve"), &server.admin, Some(&json!({"session_node":true})))?;
        ensure!((200..300).contains(&status), "join approval returned {status}");
        Ok(())
    })();
    if decision.is_err() { stop.cancel(); }
    let joined = enrollment.join().map_err(|_| anyhow::anyhow!("enrollment thread stopped"))?;
    decision?;
    ensure!(joined?.name == common::machine(name), "unexpected approved identity");
    write_secret(&state.join("session-policy.json"), br#"{"auto_failover":true}"#, 0o600)?;
    let admin = random_token();
    write_secret(&state.join("session-admin.token"), admin.as_bytes(), 0o600)?;
    Ok(admin)
}

fn eventually<T>(why: &str, mut f: impl FnMut() -> anyhow::Result<T>) -> anyhow::Result<T> {
    let until = Instant::now() + Duration::from_secs(90);
    loop {
        match f() {
            Ok(value) => return Ok(value),
            Err(e) if Instant::now() >= until => return Err(e.context(why.to_owned())),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn group(port: u16, admin: &str, id: &str) -> anyhow::Result<GroupStatus> {
    #[derive(serde::Deserialize)]
    struct Groups { groups: Vec<GroupStatus> }
    let status: Groups = serde_json::from_value(request(port, "GET", "/api/convo/raft", admin, None)?)?;
    status.groups.into_iter().find(|g| g.session == id).context("no such session group")
}

fn provider_received(row: &Value, text: &str) -> bool {
    row.pointer("/message/content").is_some_and(|content| {
        content.as_str() == Some(text) || content.as_array().is_some_and(|parts| {
            parts.iter().any(|p| p.get("text").and_then(Value::as_str) == Some(text))
        })
    })
}

fn provider_pids(record: &Path, text: &str) -> anyhow::Result<Vec<u64>> {
    let mut pids = Vec::new();
    for row in std::fs::read_to_string(record)?.lines() {
        let row: Value = serde_json::from_str(row)?;
        if provider_received(&row, text) {
            pids.push(row.get("pid").and_then(Value::as_u64).context("provider record lacks its pid")?);
        }
    }
    Ok(pids)
}

type FixtureNode = (&'static str, u16, String, Option<common::agents::Runner>);

fn start_fleet(dir: &Path, log: &str, server_sessions: bool) -> anyhow::Result<(common::Server, Vec<FixtureNode>)> {
    use common::agents::{Talk, runner_with};
    let extra = format!("[discovery]\nenabled = false\nannounce = false\nbrowse = false\njoin_requests = true\ntailscale = false\n[terminal]\nenabled = {server_sessions}");
    let opts = common::Options { extra: &extra, ci: "enabled = false", agent: server_sessions };
    let server = common::start_env(dir, &opts, false, &[("KILN_CONVO_FAILOVER", "1")])?;
    let mut nodes = Vec::new();
    for name in ["fleet-a", "fleet-b", "fleet-c"] {
        let admin = enroll(dir, &server, name)?;
        let port = common::free_port()?;
        let listener = format!("127.0.0.1:{port}");
        let home = dir.join(format!("{name}-home"));
        std::fs::create_dir_all(&home)?;
        let home = home.to_str().context("fixture HOME is not UTF-8")?;
        let (runner, _) = runner_with(dir, server.port, name, None, Talk { lines: 1, every_ms: 0 },
            &[("KILN_NODE_HTTP_ADDR", &listener), ("HOME", home), ("FAKE_CLAUDE_RECORD", "1"), ("KILN_LOG", log)])?;
        nodes.push((name, port, admin, Some(runner)));
    }
    Ok((server, nodes))
}

/// Prepare real follower WALs before the timed run. The idle sessions have an independent
/// owner, with takeover switched off for those sessions only. Reopening the measured nodes
/// cannot start thousands of providers or introduce recovery retries during the measurement.
fn seed_latency_replicas(dir: &Path, count: usize) -> anyhow::Result<()> {
    use kiln::convo::{raft::{Manager, Policy}, rows::Row};
    use kiln::harness::{model::{State, Status}, session::Info};
    use std::{collections::BTreeMap, sync::Arc};
    if count == 0 { return Ok(()); }
    let founder = common::machine("latency-idle-founder");
    let server = common::machine(&format!("server {}", dir.display()));
    let mut locations = vec![(founder.clone(), dir.join("idle-founder")), (server.clone(), dir.join("data"))];
    locations.extend(["fleet-a", "fleet-b", "fleet-c"].map(|n| (common::machine(n), dir.join(format!("{n}-state")))));
    let mut managers = BTreeMap::new();
    let mut stores = Vec::new();
    for (name, data) in locations {
        let wal = data.join("convo-raft");
        std::fs::create_dir_all(&wal)?;
        kiln::util::write_secret(&wal.join("node-id"), name.as_bytes(), 0o600)?;
        let store = kiln::convo::open(&data, Default::default()).context("open benchmark conversation store")?;
        let m = Manager::open_with_policy(&name, &wal, Some(store.clone()), Policy { auto_failover: false })?;
        m.set_name_registrar(&server)?;
        managers.insert(name, m);
        stores.push(store);
    }
    let managers = Arc::new(managers);
    for m in managers.values() {
        m.register_members(managers.keys().map(|n| (n.clone(), true, true)).collect());
    }
    let stop = kiln::rt::Shutdown::new();
    let mut workers = Vec::new();
    for (name, m) in managers.iter() {
        let (name, from, nodes, stop) = (name.clone(), m.clone(), managers.clone(), stop.clone());
        workers.push(std::thread::spawn(move || kiln::rt::block_on(async move {
            use futures::FutureExt as _;
            let outgoing = from.outgoing();
            loop {
                let frame = futures::select! {
                    () = stop.cancelled().fuse() => return Ok(()),
                    next = outgoing.recv().fuse() => next?,
                };
                let peer = nodes.get(&frame.to_name).context("unknown staging peer")?;
                peer.step_owned(&name, &frame.session, frame.bytes)?;
                if frame.snapshot_last { from.report_snapshot(&frame.session, &frame.to_name, true); }
            }
        })));
    }
    let result = (|| -> anyhow::Result<()> {
        let owner = managers.get(&founder).context("missing idle owner")?;
        let state = serde_json::to_string(&State::default())?;
        for i in 0..count {
            let id = format!("idle-{i}");
            let info = Info { id: id.clone(), name: id.clone(), target: format!("agent:{founder}"), kind: "claude".into(), status: Status::Idle, ..Default::default() };
            let row = Row::Session { session: id.clone(), info: Some(serde_json::to_string(&info)?), state: Some(state.clone()), meta: None, changes: None, epoch: Some(1), open: true, at: kiln::util::now_ms() };
            owner.open_leader(&id, 1, vec![row])?;
        }
        eventually("idle replica WALs did not settle", || {
            for (name, m) in managers.iter() {
                let n = m.all_status().into_iter().filter(|g| g.founded && g.owner.as_ref() == Some(&founder)).count();
                ensure!(n == count, "{name} has {n}/{count} committed sessions");
            }
            Ok(())
        })
    })();
    stop.cancel();
    let routed = workers.into_iter().map(|w| w.join().map_err(|_| anyhow::anyhow!("benchmark WAL router stopped")).and_then(|r| r)).collect::<Vec<_>>();
    for m in managers.values() { m.shutdown(); }
    drop(managers);
    for store in stores { store.close()?; }
    for status in routed { status?; }
    result
}

/// CP-T3.2. Time only after the origin and destination reopened the measured population.
#[test]
#[ignore = "scale measurement; set KILN_TEST_SESSION_TIER and KILN_TEST_SESSION_SAMPLES on a placed runner"]
fn cp_t3_2_session_delivery_latency() -> anyhow::Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let tier: usize = std::env::var("KILN_TEST_SESSION_TIER")?.parse()?;
    let samples: usize = std::env::var("KILN_TEST_SESSION_SAMPLES")?.parse()?;
    ensure!(samples > 0, "specify a positive sample count");
    let cache = std::env::var_os("CARGO_TARGET_DIR").context("set a private CARGO_TARGET_DIR for the fixture")?;
    let dir = tempfile::Builder::new().prefix("session-latency-").tempdir_in(cache)?;
    seed_latency_replicas(dir.path(), tier)?;
    let record = dir.path().join(".fake-claude-messages");
    std::fs::File::create(&record)?;
    let (server, nodes) = start_fleet(dir.path(), "info,kiln.session_latency=debug", true)?;
    let [(_, a, admin_a, _), (b_name, b, admin_b, _), _] = nodes.as_slice() else { anyhow::bail!("expected three fixture nodes") };
    let target = common::machine(b_name);
    let paths = [("node", *a, admin_a), ("server", server.port, &server.admin)];
    for (port, token) in [(*a, admin_a), (*b, admin_b)] {
        eventually("idle groups did not replicate", || {
            let status = request(port, "GET", "/api/convo/raft", token, None)?;
            let groups = status.get("groups").and_then(Value::as_array).context("missing group status")?;
            let resident = groups.iter().filter(|g| {
                g.get("session").and_then(Value::as_str).is_some_and(|s| s.starts_with("idle-"))
                    && g.get("founded").and_then(Value::as_bool) == Some(true)
            }).count();
            ensure!(resident == tier, "{resident}/{tier} committed idle groups");
            Ok(())
        })?;
    }
    // Enrollment and the first QUIC handshake precede these steady-state samples.
    // Reuse one slot per origin so an unanswered warm-up cannot spawn a second provider.
    for &(path, port, token) in &paths {
        let id = kiln::util::new_id();
        let spec = json!({"ids":[id], "target":format!("agent:{target}"), "spec":{"kind":"claude","cwd":dir.path(),"prompt":id}});
        eventually(&format!("{path} could not start its warm-up session on {target}"), || {
            let answer = request(port, "POST", "/api/harness/sessions", token, Some(&spec))?;
            ensure!(answer.pointer("/0/id").and_then(Value::as_str) == Some(&id) && matches!(answer.pointer("/0/outcome").and_then(Value::as_str), Some("forwarded" | "started")), "warm-up pending via {path}: {answer}");
            Ok(())
        })?;
        eventually("target provider did not handle the warm-up", || {
            let pids = provider_pids(&record, &id)?;
            ensure!(pids.len() == 1, "warm-up {id} through {path} reached providers {pids:?}, destination group {:?}", group(*b, admin_b, &id));
            Ok(())
        })?;
    }
    // Reverse each pair to distribute warm-up and machine-load drift across both routes.
    for trial in 0..samples {
        for slot in 0..paths.len() {
            let &(path, port, token) = paths.get((slot + trial) % paths.len()).context("benchmark route")?;
            let id = kiln::util::new_id();
            let spec = json!({"ids":[id], "target":format!("agent:{target}"), "spec":{"kind":"claude","cwd":dir.path(),"prompt":id}});
            let before = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let answer = request(port, "POST", "/api/harness/sessions", token, Some(&spec))?;
            ensure!(answer.pointer("/0/id").and_then(Value::as_str) == Some(&id), "start via {path}: {answer}");
            if path == "node" {
                ensure!(answer.pointer("/0/via").and_then(Value::as_str) == Some("quic"), "start did not use direct QUIC: {answer}");
            }
            let handled_ns = eventually("provider did not receive its selected start", || {
                for line in std::fs::read_to_string(&record)?.lines() {
                    let row: Value = serde_json::from_str(line)?;
                    if provider_received(&row, &id) {
                        return row.get("at_ns").and_then(Value::as_u64).context("provider did not record its clock");
                    }
                }
                anyhow::bail!("waiting for {id}")
            })?;
            let running_ns = u128::from(handled_ns).checked_sub(before).context("fixture clock moved backward")?;
            let body = json!({"type":"send","id":kiln::util::new_id(),"text":"latency sample","target":target});
            let begin = Instant::now();
            let receipt = request(port, "POST", &format!("/api/harness/sessions/{id}/command"), token, Some(&body))?;
            let send_ns = begin.elapsed().as_nanos();
            ensure!(receipt.pointer("/accepted/seq").and_then(Value::as_u64).is_some_and(|s| s > 0), "missing send receipt: {receipt}");
            println!("idle_groups={tier} trial={trial} path={path} start_to_provider_ns={running_ns} send_to_receipt_ns={send_ns}");
        }
    }
    let log = std::fs::read_to_string(dir.path().join("fleet-a.log"))?;
    for row in log.lines().filter(|r| r.contains("direct session start reply")) { println!("{row}"); }
    Ok(())
}

#[test]
fn cp_t3_2_selected_founder_routes_to_its_own_queue_and_survivors_keep_its_receipt() -> anyhow::Result<()> {
    use common::agents::{Talk, runner_with};
    let dir = tempfile::Builder::new().prefix("kiln-session-fleet-").tempdir()?;
    eprintln!("session fleet fixture {}", dir.path().display());
    let (server, mut nodes) = start_fleet(dir.path(), "info", false)?;
    let bootstrap_port = server.port;
    let [(name_a, port_a, admin_a, runner_a), (name_b, port_b, admin_b, runner_b), (name_c, port_c, admin_c, _)] = nodes.as_mut_slice() else { anyhow::bail!("expected three test nodes") };
    let a = common::machine(name_a);
    let b = common::machine(name_b);
    let id = kiln::util::new_id();
    let other = kiln::util::new_id();
    let deliveries = dir.path().join(".fake-claude-messages");
    std::fs::File::create(&deliveries)?;
    let session = format!("/api/harness/sessions/{id}/command");
    // Stable slot ids also cover the lost-start-response retry.
    let start = json!({"ids":[id], "target":format!("agent:{b}"), "spec":{"kind":"claude", "cwd":dir.path(), "prompt":"hello"}});
    eventually("connect to A for initial start", || lose_start_reply(*port_a, admin_a, &id, &start))?;
    eventually("start from node A on the selected node B", || {
        let value = request(*port_a, "POST", "/api/harness/sessions", admin_a, Some(&start))?;
        ensure!(value.pointer("/0/id") == Some(&json!(id)) && value.pointer("/0/outcome").and_then(Value::as_str) == Some("forwarded"), "start did not return the selected id: {value}");
        Ok(())
    })?;
    let epoch = eventually("B did not commit a replicated voting group", || {
        let g = group(*port_b, admin_b, &id)?;
        ensure!(g.owner.as_deref() == Some(&b) && g.is_leader && g.voters.len() >= 3, "group={g:?}");
        Ok(g.epoch)
    })?;
    for (p, token) in [(*port_a, &*admin_a), (*port_c, &*admin_c)] {
        eventually("session absent from a peer", || { let g = group(p, token, &id)?; ensure!(g.owner.as_deref() == Some(&b), "owner={:?}", g.owner); Ok(()) })?;
    }
    let independent = json!({"ids":[other], "target":format!("agent:{a}"), "spec":{"kind":"claude", "cwd":dir.path()}});
    eventually("start a second session on A", || {
        let value = request(*port_a, "POST", "/api/harness/sessions", admin_a, Some(&independent))?;
        ensure!(value.pointer("/0/id") == Some(&json!(other)) && value.pointer("/0/outcome").and_then(Value::as_str) == Some("started"), "second start got {value}");
        Ok(())
    })?;
    let stable = eventually("second session lacks its selected owner", || {
        let g = group(*port_a, admin_a, &other)?;
        ensure!(g.owner.as_deref() == Some(&a) && g.is_leader && g.voters.len() >= 3, "other={g:?}");
        Ok(g)
    })?;
    let initial_pid = eventually("selected runner did not deliver its opening prompt exactly once", || {
        let pids = provider_pids(&deliveries, "hello")?;
        let [pid] = pids.as_slice() else { anyhow::bail!("opening recorded by {pids:?}") };
        Ok(*pid)
    })?;
    let message = json!({"type":"send", "id":"fleet-receipt", "text":"one logical delivery", "target":b});
    let original = request(*port_a, "POST", &session, admin_a, Some(&message))?;
    ensure!(original.pointer("/accepted/seq").and_then(Value::as_u64).is_some_and(|s| s > 0), "first receipt={original}");
    // Fix 3. Race a send to B with B's termination. Retrying the same message through A
    // has to identify one commit, including if B sent the answer just before it exited.
    let racing = json!({"type":"send", "id":"at-handoff", "text":"arrived during the move", "target":b});
    let raced = {
        let barrier = std::sync::Barrier::new(2);
        let gate = &barrier;
        std::thread::scope(|child| -> anyhow::Result<Value> {
            let submit = child.spawn(|| {
                gate.wait();
                eventually("message during the move was not committed", || {
                    let reply = request(*port_a, "POST", &session, admin_a, Some(&racing))?;
                    ensure!(reply.pointer("/accepted/seq").and_then(Value::as_u64).is_some(), "send during move returned {reply}");
                    Ok(reply)
                })
            });
            gate.wait();
            drop(runner_b.take());
            submit.join().map_err(|_| anyhow::anyhow!("send during move stopped"))?
        })?
    };
    let owner = eventually("no successor after the selected runner died", || {
        let g = group(*port_a, admin_a, &id)?;
        let next = g.owner.as_deref().context("no owner yet")?;
        ensure!(next != b && g.epoch > epoch, "group={g:?}");
        Ok(next.to_owned())
    })?;
    // CP-T3.2: this client still targets the dead founder. Only committed custody can route its retry.
    let repeated = eventually("new owner did not recover the original receipt", || {
        let value = request(*port_a, "POST", &session, admin_a, Some(&message))?;
        ensure!(value.pointer("/accepted/repeat") == Some(&json!(true)), "retry={value}");
        Ok(value)
    })?;
    for field in ["/accepted/seq", "/accepted/at", "/accepted/epoch"] {
        let first = original.pointer(field).with_context(|| format!("receipt lacks {field}"))?;
        ensure!(repeated.pointer(field) == Some(first), "receipt {field} changed");
    }
    let replay = request(*port_a, "POST", &session, admin_a, Some(&racing))?;
    ensure!(replay.pointer("/accepted/repeat") == Some(&json!(true)) &&
        ["/accepted/seq", "/accepted/epoch", "/accepted/at"].iter().all(|key| replay.pointer(key) == raced.pointer(key)),
        "custody changed the commit of the in-flight send");
    let unchanged = group(*port_a, admin_a, &other)?;
    ensure!(unchanged.owner == stable.owner && unchanged.epoch == stable.epoch, "unrelated session changed custody");
    server.crash();
    let following = json!({"type":"send", "id":"after-bootstrap", "text":"continue on elected owner", "target":b});
    eventually("owner could not accept a new message with bootstrap offline", || {
        let answer = request(*port_a, "POST", &session, admin_a, Some(&following))?;
        ensure!(answer.pointer("/accepted/seq").and_then(Value::as_u64).is_some_and(|s| s > 0), "got {answer}");
        Ok(())
    })?;
    eventually("bootstrap-offline WebSocket retry did not return the durable receipt", || {
        let ack = socket_send(*port_a, admin_a, &id, &b, &following)?;
        ensure!(ack.pointer("/accepted/repeat") == Some(&json!(true)), "socket retry={ack}");
        Ok(())
    })?;
    eventually("the recovered native provider did not handle the next turn exactly once", || {
        let pids = provider_pids(&deliveries, "continue on elected owner")?;
        ensure!(matches!(pids.as_slice(), [pid] if *pid != initial_pid), "providers after election={pids:?}, original provider={initial_pid}");
        Ok(())
    })?;
    let arguments = json!({"agent":id, "target":owner, "key":"surviving-mcp", "text":"authenticated node operation"});
    let rpc = json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":"agents_send","arguments":arguments}});
    let mcp = eventually("node MCP send with bootstrap offline", || {
        let out = request(*port_a, "POST", "/api/mcp", admin_a, Some(&rpc))?;
        ensure!(out.pointer("/result/structuredContent/accepted/seq").and_then(Value::as_u64).is_some(), "MCP response={out}");
        Ok(out)
    })?;
    let retry = request(*port_a, "POST", "/api/ops/agents.send", admin_a, Some(&arguments))?;
    ensure!(retry.pointer("/accepted/repeat") == Some(&json!(true)) && retry.pointer("/accepted/seq") == mcp.pointer("/result/structuredContent/accepted/seq"), "named operation and MCP received different receipts");
    let cli = common::kiln().env("KILN_URL", format!("http://127.0.0.1:{port_a}")).env("KILN_ADMIN_TOKEN", &*admin_a)
        .args(["agents", "send", &id, "--on", &owner, "--key", "surviving-mcp", "authenticated node operation"]).output()?;
    ensure!(cli.status.success(), "bootstrap-offline CLI send returned {}: {} {}", cli.status, String::from_utf8_lossy(&cli.stdout), String::from_utf8_lossy(&cli.stderr));
    ensure!(provider_pids(&deliveries, "hello")?.as_slice() == [initial_pid], "the opening prompt was dispatched again after failover");
    let delivered = provider_pids(&deliveries, "arrived during the move")?;
    ensure!(delivered.len() == 1, "in-flight send handled by {delivered:?}");
    ensure!(a != b);
    let db = rusqlite::Connection::open_with_flags(dir.path().join("data/kiln.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let central: i64 = db.query_row("SELECT count(*) FROM agent_mail WHERE recipient=?1", [&id], |r| r.get(0))?;
    ensure!(central == 0, "the forwarding node kept {central} central mail rows");
    let disconnected_id = kiln::util::new_id();
    let c = common::machine(name_c);
    let selected = json!({"ids":[disconnected_id], "target":format!("agent:{c}"), "spec":{"kind":"claude", "cwd":dir.path()}});
    eventually("two surviving voters could not start a new session without bootstrap", || {
        let value = request(*port_a, "POST", "/api/harness/sessions", admin_a, Some(&selected))?;
        ensure!(value.pointer("/0/id") == Some(&json!(disconnected_id)) && value.pointer("/0/outcome").and_then(Value::as_str) == Some("forwarded"), "offline start returned {value}");
        let g = group(*port_c, admin_c, &disconnected_id)?;
        ensure!(g.founded && g.is_leader && g.owner.as_deref() == Some(c.as_str()), "offline founder={g:?}");
        Ok(())
    })?;
    // Fix 4. B is a known fleet member with both its direct path and its relay gone.
    // The 202 tells the sender that A now owes delivery even if the browser closes.
    let held_id = kiln::util::new_id();
    let held_start = json!({"ids":[held_id], "target":format!("agent:{b}"),
        "spec":{"kind":"claude","cwd":dir.path(),"prompt":"start held across link loss"}});
    let (held_code, held_report) = common::http_with_key(*port_a, "POST", "/api/harness/sessions", admin_a, Some(&held_start), Duration::from_secs(50), Some(&held_id)).context("request to the disconnected runner did not return a held-start receipt")?;
    let held_report: Value = serde_json::from_str(&held_report)?;
    ensure!(held_code == 202 && held_report.pointer("/0/id") == Some(&json!(held_id)) &&
            held_report.pointer("/0/outcome").and_then(Value::as_str) == Some("held"), "unreachable start returned {held_code} {held_report}");
    ensure!(provider_pids(&deliveries, "start held across link loss")?.is_empty(), "a forwarding node started B's session");
    // A has acknowledged durable custody, so it may crash before B's link returns.
    drop(runner_a.take());
    let bind_a = format!("127.0.0.1:{port_a}");
    let home_a = dir.path().join(format!("{name_a}-home"));
    let (source, _) = runner_with(dir.path(), bootstrap_port, name_a, None, Talk { lines: 1, every_ms: 0 },
        &[("KILN_NODE_HTTP_ADDR", &bind_a), ("HOME", home_a.to_str().context("fixture HOME is not UTF-8")?), ("FAKE_CLAUDE_RECORD", "1")])?;
    *runner_a = Some(source);
    eventually("A restarted without its acknowledged outbox receipt", || {
        let status = request(*port_a, "GET", &format!("/api/harness/start-requests/{held_id}"), admin_a, None)?;
        ensure!(status.pointer("/slots/0/outcome").and_then(Value::as_str) == Some("held"), "start status={status}");
        Ok(())
    })?;
    // Restart the former leader from its on-disk WAL, pinned peer directory and expired uplink.
    let bind = format!("127.0.0.1:{port_b}");
    let home = dir.path().join(format!("{name_b}-home"));
    let (rejoined, _) = runner_with(dir.path(), bootstrap_port, name_b, None, Talk { lines: 1, every_ms: 0 },
        &[("KILN_NODE_HTTP_ADDR", &bind), ("HOME", home.to_str().context("fixture HOME is not UTF-8")?), ("FAKE_CLAUDE_RECORD", "1")])?;
    *runner_b = Some(rejoined);
    eventually("restarted former leader did not learn the later owner with bootstrap offline", || {
        let g = group(*port_b, admin_b, &id)?;
        ensure!(g.founded && !g.is_leader && g.owner.as_deref() == Some(owner.as_str()) && g.epoch > epoch, "former leader={g:?}");
        let r = request(*port_b, "POST", &session, admin_b, Some(&message))?;
        ensure!(r.pointer("/accepted/repeat") == Some(&json!(true)) && r.pointer("/accepted/seq") == original.pointer("/accepted/seq"), "former leader's forwarded receipt={r}");
        Ok(())
    })?;
    eventually("B's returning link did not drain A's start outbox", || {
        let status = request(*port_a, "GET", &format!("/api/harness/start-requests/{held_id}"), admin_a, None)?;
        ensure!(status.pointer("/slots/0/outcome").and_then(Value::as_str) == Some("forwarded"), "held start status={status}");
        let g = group(*port_b, admin_b, &held_id)?;
        ensure!(g.is_leader && g.owner.as_deref() == Some(&b), "held start has wrong founder");
        let pids = provider_pids(&deliveries, "start held across link loss")?;
        ensure!(pids.len() == 1, "held start provider deliveries {pids:?}");
        Ok(())
    })?;
    Ok(())
}
