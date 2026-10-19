//! The local socket `kiln spawn` and `kiln agents` talk to from inside a kiln shell. Every kiln
//! terminal carries `KILN_HARNESS_SOCK` (this socket) and `KILN_TERM_SESSION` (its own id), so an
//! agent started from a shell runs in that shell's directory and environment and shows up under
//! it in the UI. One JSON request per connection, one JSON answer. The socket is the owner's alone
//! (mode 0600 in a 0700 directory).
//!
//! What asks is told by its process, never by what it says: a command an agent kiln runs here
//! started (its harness is an ancestor, or it carries the agent's mark) is that agent, so what it
//! starts are its children and it steers only its own (docs/agents.md, "Agents that run agents");
//! anything else is the person in the kiln shell it runs in. kiln's operations (`kiln spawn --on`,
//! `kiln agents wait`, `kiln agents result`) are asked of the kiln server: in the server's own
//! process directly, from a runner through its link to the server.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::host::Host;
use super::model::{Command, SendMode};
use super::session::Spec;

/// A request from the CLI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Spawn {
        kind: String,
        #[serde(default)]
        count: Option<usize>,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        shell: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
        /// Run this executable instead of the harness's usual one.
        #[serde(default)]
        program: Option<String>,
    },
    List {
        #[serde(default)]
        shell: Option<String>,
    },
    /// Stop agents: one id, or every agent of `shell` (all of them without one).
    Stop {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        shell: Option<String>,
    },
    Interrupt {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        shell: Option<String>,
    },
    Send {
        id: String,
        text: String,
        #[serde(default)]
        mode: SendMode,
    },
    /// One of kiln's operations (`agents.start`, `agents.wait`, …), asked of the kiln server for
    /// whoever asks: an agent kiln runs here, else the person in kiln shell `terminal`.
    Op {
        name: String,
        #[serde(default)]
        input: Value,
        #[serde(default)]
        terminal: Option<String>,
    },
}

/// Who asks the server, as the socket tells them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asker {
    /// An agent session kiln runs on this machine.
    Agent(String),
    /// The person in this kiln shell (a terminal kiln opened).
    Terminal(String),
}

/// How a machine asks the kiln server one of its operations, for whoever asks: `{"ok": answer}` or
/// `{"error", "status"?}`. The server's own sets it (in-process); a runner's goes through its
/// link to the server.
pub type Forward = Arc<dyn Fn(Asker, String, Value) -> Pin<Box<dyn Future<Output = Value> + Send>> + Send + Sync>;

/// Ask the kiln server operation `name` for `asker`: through `forward` (the server's own socket),
/// else through this runner's link to the server.
async fn ask_server(forward: Option<&Forward>, asker: &Asker, name: &str, input: Value) -> Value {
    if let Some(f) = forward {
        return f(asker.clone(), name.to_owned(), input).await;
    }
    let Some(node) = crate::reach::node::global() else {
        return json!({"error": "this machine's runner has no link to the kiln server for this (no reach here)"});
    };
    let mut q = json!({"op": "call", "name": name, "input": input});
    if let Some(o) = q.as_object_mut() {
        match asker {
            Asker::Agent(s) => o.insert("session".into(), json!(s)),
            Asker::Terminal(t) => o.insert("terminal".into(), json!(t)),
        };
    }
    match node.query(q.to_string()).await {
        Ok(a) => serde_json::from_str(&a).unwrap_or_else(|e| json!({"error": format!("the server's answer was unreadable: {e}")})),
        Err(e) => json!({"error": format!("{e:#}")}),
    }
}

/// The server's answer as the socket's.
fn relayed(a: Value) -> Value {
    match a.get("ok") {
        Some(v) => json!({"ok": true, "result": v}),
        None => json!({"ok": false, "error": a.get("error").and_then(Value::as_str).unwrap_or("the kiln server refused")}),
    }
}

/// The agent session a connection's process belongs to, of `host`'s.
async fn agent_of(host: &Host, peer: Option<u32>) -> Option<String> {
    let pid = peer?;
    let sessions = host.harness_pids();
    crate::rt::blocking(move || super::survivors::session_of(pid, &sessions)).await.ok().flatten()
}

/// Answer a request from process `peer`: an agent's go to the server (what it starts are its
/// children; it steers only its own), and so do kiln's operations; the rest is this machine's.
pub async fn answer(host: &Host, req: Request, peer: Option<u32>, forward: Option<&Forward>) -> Value {
    let agent = agent_of(host, peer).await;
    let as_agent = |name: &'static str, input: Value| {
        let a = agent.clone().map(Asker::Agent);
        async move {
            match a {
                Some(a) => relayed(ask_server(forward, &a, name, input).await),
                None => json!({"ok": false, "error": "no agent"}),
            }
        }
    };
    match req {
        Request::Op { name, input, terminal } => {
            let asker = match (agent.clone(), terminal) {
                (Some(a), _) => Asker::Agent(a),
                (None, Some(t)) if !t.is_empty() => Asker::Terminal(t),
                _ => return json!({"ok": false, "error": "not in a kiln shell: kiln can't tell who asks (open a terminal in kiln)"}),
            };
            relayed(ask_server(forward, &asker, &name, input).await)
        }
        // An agent's `kiln spawn`: started by the server, as its children, on this machine.
        Request::Spawn { kind, count, prompt, model, mode, name, cwd, .. } if agent.is_some() => {
            let a = as_agent("agents.start", json!({"kind": kind, "count": count, "prompt": prompt, "model": model, "mode": mode, "name": name, "cwd": cwd})).await;
            if a.get("ok") != Some(&json!(true)) {
                return a;
            }
            let sessions: Vec<Value> = a.pointer("/result/agents").and_then(Value::as_array).into_iter().flatten().map(|i| json!({"id": i.get("id"), "name": i.get("name"), "kind": i.get("kind")})).collect();
            json!({"ok": true, "sessions": sessions})
        }
        Request::Send { id, text, mode } if agent.is_some() => {
            let mode = match mode {
                SendMode::Queue => "queue",
                SendMode::Steer => "steer",
                SendMode::Now => "now",
            };
            as_agent("agents.send", json!({"agent": id, "text": text, "mode": mode})).await
        }
        Request::Stop { id: Some(id), .. } if agent.is_some() => as_agent("agents.stop", json!({"agent": id})).await,
        Request::Interrupt { id: Some(id), .. } if agent.is_some() => as_agent("agents.interrupt", json!({"agent": id})).await,
        Request::Stop { id: None, .. } | Request::Interrupt { id: None, .. } if agent.is_some() => json!({"ok": false, "error": "say which agent (its id): an agent stops only the agents it started"}),
        other => handle(host, other),
    }
}

/// The process at the other end of a connection, where the system says.
#[cfg(target_os = "linux")]
fn peer_pid(s: &std::os::unix::net::UnixStream) -> Option<u32> {
    nix::sys::socket::getsockopt(s, nix::sys::socket::sockopt::PeerCredentials).ok().and_then(|c| u32::try_from(c.pid()).ok())
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn peer_pid(s: &std::os::unix::net::UnixStream) -> Option<u32> {
    nix::sys::socket::getsockopt(s, nix::sys::socket::sockopt::LocalPeerPid).ok().and_then(|p| u32::try_from(p).ok())
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos", target_os = "ios"))))]
fn peer_pid(_s: &std::os::unix::net::UnixStream) -> Option<u32> {
    None
}

/// Where this process's socket lives: the user's runtime directory (else a private one in /tmp),
/// named after the agent so a server and runners on one machine do not collide. `home` is the
/// runner's state directory: every runner process of a machine (an upgrade's new build beside
/// the old one) has the same socket, served by the one that runs the machine's sessions, so the
/// path a terminal was given goes on working after an upgrade.
pub fn path(name: &str, home: &std::path::Path) -> PathBuf {
    let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).take(32).collect();
    // Two kiln processes on one machine (a server and runners, or test instances) never share one.
    let tag = crate::util::sha256_hex(home.to_string_lossy().as_bytes());
    runtime_path(&format!("harness-{safe}-{}.sock", tag.get(..8).unwrap_or("0")))
}

/// `file` in the user's runtime directory for sockets (else a private one in /tmp).
pub fn runtime_path(file: &str) -> PathBuf {
    let private = std::env::temp_dir().join(format!("kiln-{}", crate::os::uid())).join(file);
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_dir()).map(|d| d.join("kiln").join(file));
    // Unix socket paths are short (104 bytes on macOS with the terminator): a long runtime dir
    // falls back.
    match runtime {
        Some(p) if p.as_os_str().len() < 100 => p,
        _ => private,
    }
}

/// Answer one request against `host`.
pub fn handle(host: &Host, req: Request) -> Value {
    let pick = |id: &Option<String>, shell: &Option<String>| -> Vec<String> {
        host.list()
            .into_iter()
            .filter(|i| id.as_ref().is_none_or(|x| &i.id == x || i.id.starts_with(x.as_str())))
            .filter(|i| id.is_some() || shell.is_none() || i.shell == *shell)
            .map(|i| i.id)
            .collect()
    };
    match req {
        Request::Spawn { kind, count, prompt, model, mode, name, shell, cwd, env, program } => {
            let count = count.unwrap_or(1).clamp(1, 16);
            let mut made = Vec::new();
            for i in 0..count {
                let spec = Spec {
                    kind: kind.clone(),
                    cwd: cwd.clone(),
                    model: model.clone(),
                    mode: mode.clone(),
                    prompt: prompt.clone(),
                    name: match (&name, count) {
                        (Some(n), 1) => Some(n.clone()),
                        (Some(n), _) => Some(format!("{n} {}", i + 1)),
                        (None, 1) => None,
                        (None, _) => super::kinds::Kind::get(&kind).map(|k| format!("{} {}", k.name, i + 1)),
                    },
                    shell: shell.clone(),
                    env: env.clone(),
                    program: program.clone(),
                    ..Spec::default()
                };
                match host.open(None, spec) {
                    Ok(s) => {
                        let info = s.info();
                        made.push(json!({"id": info.id, "name": info.name, "kind": info.kind}));
                    }
                    Err(e) => return json!({"ok": false, "error": format!("{e:#}"), "sessions": made}),
                }
            }
            json!({"ok": true, "sessions": made})
        }
        Request::List { shell } => {
            let list: Vec<_> = host.list().into_iter().filter(|i| shell.is_none() || i.shell == shell).collect();
            json!({"ok": true, "sessions": list})
        }
        Request::Stop { id, shell } => {
            let ids = pick(&id, &shell);
            let n = ids.iter().filter(|i| host.close(i)).count();
            json!({"ok": true, "stopped": n})
        }
        Request::Interrupt { id, shell } => {
            let ids = pick(&id, &shell);
            let n = ids.iter().filter(|i| host.command(i, Command::Interrupt)).count();
            json!({"ok": true, "interrupted": n})
        }
        Request::Send { id, text, mode } => {
            let ids = pick(&Some(id), &None);
            let Some(target) = ids.first() else { return json!({"ok": false, "error": "no such agent"}) };
            let ok = host.command(target, Command::Send { id: crate::util::new_id(), text, mode, attachments: Vec::new(), overrides: None });
            json!({"ok": ok})
        }
        Request::Op { .. } => json!({"ok": false, "error": "kiln's operations go to the kiln server"}),
    }
}

/// Serve `host` on the socket at `path` until this process exits (a runner's: kiln's operations
/// go through its link to the server). Unix only.
pub fn serve(host: &Arc<Host>, path: PathBuf) -> anyhow::Result<PathBuf> {
    serve_with(host, path, None)
}

static SERVED: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The socket this process serves, once it does: every harness it starts gets it as
/// `KILN_HARNESS_SOCK`, so `kiln spawn` and `kiln agents` work in an agent's own commands too
/// (they're told apart from a person's by their process).
pub fn served() -> Option<&'static str> {
    SERVED.get().map(String::as_str)
}

/// The same, kiln's operations answered by `forward` (the server's own socket).
#[cfg(unix)]
pub fn serve_with(host: &Arc<Host>, path: PathBuf, forward: Option<Forward>) -> anyhow::Result<PathBuf> {
    use std::os::fd::{AsRawFd as _, OwnedFd};
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::net::UnixListener;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    // A socket left by a previous process of ours (it cannot be in use: the name is ours).
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let _ = SERVED.set(path.to_string_lossy().into_owned());
    listener.set_nonblocking(true)?;
    let host = Arc::clone(host);
    crate::rt::spawn(async move {
        let fd = listener.as_raw_fd();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let (host, forward) = (Arc::clone(&host), forward.clone());
                    // Who asks: the process at the other end.
                    let peer = peer_pid(&stream);
                    crate::rt::spawn(async move {
                        if stream.set_nonblocking(true).is_err() {
                            return;
                        }
                        let fd = OwnedFd::from(stream);
                        let mut buf = Vec::new();
                        let mut chunk = vec![0u8; 16 * 1024];
                        // One line, at most 1 MiB (an environment can be large, not larger).
                        while !buf.contains(&b'\n') && buf.len() < (1 << 20) {
                            match crate::rt::timeout(std::time::Duration::from_secs(10), crate::os::proc::read_chunk(&fd, &mut chunk)).await {
                                Ok(Ok(Some(n))) => buf.extend_from_slice(chunk.get(..n).unwrap_or_default()),
                                _ => break,
                            }
                        }
                        let answer = match serde_json::from_slice::<Request>(buf.split(|b| *b == b'\n').next().unwrap_or_default()) {
                            Ok(req) => answer(&host, req, peer, forward.as_ref()).await,
                            Err(e) => json!({"ok": false, "error": format!("bad request: {e}")}),
                        };
                        let mut out = answer.to_string().into_bytes();
                        out.push(b'\n');
                        let _ = crate::os::proc::write_all(&fd, &out).await;
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if hyper_rt::readiness::readable(fd).await.is_err() {
                        crate::rt::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    tracing::warn!(error = %e, "agent socket accept failed");
                    crate::rt::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
    });
    Ok(path)
}

/// No local socket on Windows yet: `kiln spawn` is not available there.
#[cfg(not(unix))]
pub fn serve_with(_host: &Arc<Host>, _path: PathBuf, _forward: Option<Forward>) -> anyhow::Result<PathBuf> {
    anyhow::bail!("kiln spawn needs Unix sockets")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One request on the socket at `path`, from this process (on the blocking pool: the socket is
    /// served on the runtime). Linux's, as the one test that asks is (on macOS it was never used,
    /// which its clippy refuses).
    #[cfg(target_os = "linux")]
    async fn ask(path: PathBuf, req: Value) -> anyhow::Result<Value> {
        crate::rt::blocking(move || -> anyhow::Result<Value> {
            use std::io::{BufRead as _, Write as _};
            let mut s = std::os::unix::net::UnixStream::connect(&path)?;
            s.set_read_timeout(Some(std::time::Duration::from_secs(20)))?;
            let mut line = req.to_string().into_bytes();
            line.push(b'\n');
            s.write_all(&line)?;
            let mut answer = String::new();
            std::io::BufReader::new(s).read_line(&mut answer)?;
            Ok(serde_json::from_str(&answer)?)
        })
        .await?
    }

    /// The socket's real path: a process that is an agent kiln runs (here this test's own, as that
    /// agent's harness) asks for bypass under a parent that asks, through `kiln spawn` and through
    /// `agents.start`: refused both ways; within its parent's mode, its child starts. Under a
    /// timeout, so a lock held across the forwarding would show.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_agent_s_kiln_spawn_is_held_to_its_parent() -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = crate::rt::block_on(async {
                let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
                let dir = tempfile::tempdir()?.keep();
                let spec = Spec { kind: "claude".into(), cwd: Some(dir.to_string_lossy().into_owned()), mode: Some("default".into()), program: Some(dir.join("no-claude").to_string_lossy().into_owned()), ..Spec::default() };
                let parent = hub.harness.open(&hub, "local", spec, 1).await?.first().map(|i| i.id.clone()).ok_or_else(|| anyhow::anyhow!("no parent"))?;
                // Its harness can't start; once it has stopped, this test's process is its harness.
                let s = hub.harness.local.get(&parent).ok_or_else(|| anyhow::anyhow!("not on the host"))?;
                for _ in 0..200 {
                    if s.info().status == crate::harness::model::Status::Exited {
                        break;
                    }
                    crate::rt::sleep(std::time::Duration::from_millis(20)).await;
                }
                *s.pid.lock() = Some(std::process::id());
                let path = dir.join("h.sock");
                serve_with(&hub.harness.local, path.clone(), Some(crate::harness::ops::forward(Arc::clone(&hub))))?;
                let nowhere = "/nonexistent/kiln-socket-test";
                let refused = ask(path.clone(), json!({"op": "spawn", "kind": "claude", "mode": "bypassPermissions", "cwd": nowhere})).await?;
                anyhow::ensure!(refused.get("ok") == Some(&json!(false)) && refused.get("error").and_then(Value::as_str).is_some_and(|e| e.contains("above its own ask")), "{refused}");
                let op = ask(path.clone(), json!({"op": "op", "name": "agents.start", "input": {"kind": "codex", "mode": "full-access", "cwd": nowhere}})).await?;
                anyhow::ensure!(op.get("ok") == Some(&json!(false)), "{op}");
                let started = ask(path.clone(), json!({"op": "spawn", "kind": "claude", "mode": "plan", "cwd": nowhere})).await?;
                let child = started.pointer("/sessions/0/id").and_then(Value::as_str).unwrap_or("").to_owned();
                anyhow::ensure!(hub.harness.info(&child).is_some_and(|i| i.parent.as_deref() == Some(parent.as_str())), "{started}");
                // An agent stops only its own.
                let other = ask(path.clone(), json!({"op": "stop", "id": "0000"})).await?;
                anyhow::ensure!(other.get("ok") == Some(&json!(false)), "{other}");
                let ended = ask(path, json!({"op": "stop", "id": child})).await?;
                anyhow::ensure!(ended.get("ok") == Some(&json!(true)) && hub.harness.info(&child).is_none(), "{ended}");
                Ok(())
            });
            let _ = tx.send(r);
        });
        match rx.recv_timeout(std::time::Duration::from_secs(60)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("the socket hung"),
        }
    }

    #[test]
    fn requests_parse_and_route() -> anyhow::Result<()> {
        let r: Request = serde_json::from_str(r#"{"op":"spawn","kind":"claude","count":2,"prompt":"hi","shell":"t1","env":[["A","b"]]}"#)?;
        assert!(matches!(&r, Request::Spawn { count: Some(2), shell: Some(s), .. } if s == "t1"));
        crate::rt::block_on(async move {
            let (tx, _rx) = async_channel::unbounded();
            let host = Host::new(tx, None, Default::default());
            let dir = tempfile::tempdir()?;
            let missing = dir.path().join("no-claude").to_string_lossy().into_owned();
            // Two sessions in shell t1 (their program is missing: they exist and say so).
            let spawned = handle(&host, Request::Spawn {
                kind: "claude".into(),
                count: Some(2),
                prompt: None,
                model: None,
                mode: None,
                name: None,
                shell: Some("t1".into()),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                env: vec![],
                program: Some(missing),
            });
            assert_eq!(spawned.get("ok"), Some(&json!(true)));
            assert_eq!(spawned.get("sessions").and_then(Value::as_array).map(Vec::len), Some(2));
            let listed = handle(&host, Request::List { shell: Some("t1".into()) });
            assert_eq!(listed.get("sessions").and_then(Value::as_array).map(Vec::len), Some(2));
            assert_eq!(handle(&host, Request::List { shell: Some("other".into()) }).get("sessions").and_then(Value::as_array).map(Vec::len), Some(0));
            let stopped = handle(&host, Request::Stop { id: None, shell: Some("t1".into()) });
            assert_eq!(stopped.get("stopped"), Some(&json!(2)));
            assert!(host.list().is_empty());
            Ok(())
        })
    }
}
