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
        /// An idempotency key (CP-T3.2): sent again with it, it's the same message, delivered once.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
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
    /// What `kiln-git-sign` asks for an agent's commit (W27-R03): `message` (base64) signed with
    /// the asking process's session's key at its epoch ([`sign_for`]).
    Sign { message: String },
    /// What an agent session's git asks for its credential for kiln (W27-R03, `kiln-git-sign
    /// credential`): the asking process's session's own token ([`credential_for`]); `forget`: kiln
    /// refused the one it was given.
    Credential {
        #[serde(default)]
        forget: bool,
    },
    /// A harness's hook asking before a tool call (`kiln harness-hook`, `codex::hook_args`): the
    /// session it's for is the one whose process asks; its answer (`answer`) may wait for a person.
    Hook {
        #[serde(default)]
        input: Value,
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
        Request::Send { id, text, mode, key } if agent.is_some() => {
            let mode = match mode {
                SendMode::Queue => "queue",
                SendMode::Steer => "steer",
                SendMode::Now => "now",
            };
            as_agent("agents.send", json!({"agent": id, "text": text, "mode": mode, "key": key})).await
        }
        Request::Stop { id: Some(id), .. } if agent.is_some() => as_agent("agents.stop", json!({"agent": id})).await,
        Request::Interrupt { id: Some(id), .. } if agent.is_some() => as_agent("agents.interrupt", json!({"agent": id})).await,
        Request::Stop { id: None, .. } | Request::Interrupt { id: None, .. } if agent.is_some() => json!({"ok": false, "error": "say which agent (its id): an agent stops only the agents it started"}),
        Request::Sign { message } => sign_for(host, peer, &message).await,
        Request::Credential { forget } => credential_for(host, peer, forward, forget).await,
        // Its harness's hook, told by its process: what holds the call answers, maybe later.
        Request::Hook { input } => match agent.as_deref().and_then(|a| host.get(a)) {
            Some(s) => json!({"ok": true, "answer": s.hook(input).await}),
            None => json!({"ok": true, "answer": {}}),
        },
        other => handle(host, other),
    }
}

/// The agent session process `peer` belongs to, told by the process alone: its group of processes
/// or a harness among its ancestors, never by what it says (the mark any process can set).
async fn session_by_peer(host: &Host, peer: Option<u32>) -> Option<String> {
    let pid = peer?;
    let sessions = host.harness_pids();
    crate::rt::blocking(move || super::survivors::session_by_process(pid, &sessions)).await.ok().flatten()
}

/// The tokens agent sessions here were given for their git (`session → (token, until)`).
static CREDENTIALS: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashMap<String, (String, i64)>>> = std::sync::LazyLock::new(Default::default);

/// An agent session's credential for kiln (W27-R03): a token of the agent's own, for its person,
/// which this runner asks the server for (`agents.credential`) as that session, told by the asking
/// process alone ([`session_by_peer`]), and keeps until an hour before it ends. One outside every
/// session is told it's no agent's (`"agent": false`); the token goes back only to the session's
/// own processes. `forget`: kiln refused it, so the next ask gets another.
async fn credential_for(host: &Host, peer: Option<u32>, forward: Option<&Forward>, forget: bool) -> Value {
    let Some(session) = session_by_peer(host, peer).await else {
        return json!({"ok": true, "agent": false});
    };
    if forget {
        CREDENTIALS.lock().remove(&session);
        return json!({"ok": true, "agent": true});
    }
    let now = crate::util::now_unix();
    let kept = CREDENTIALS.lock().get(&session).filter(|(_, until)| *until - 3600 > now).map(|(t, _)| t.clone());
    let token = match kept {
        Some(t) => t,
        None => {
            let a = ask_server(forward, &Asker::Agent(session.clone()), "agents.credential", json!({"agent": session})).await;
            let (Some(t), Some(until)) = (a.pointer("/ok/token").and_then(Value::as_str), a.pointer("/ok/expires_at").and_then(Value::as_i64)) else {
                let why = a.get("error").and_then(Value::as_str).unwrap_or("the kiln server gave none");
                return json!({"ok": false, "error": format!("no credential for agent session {session}: {why}")});
            };
            let mut held = CREDENTIALS.lock();
            // What ended goes (an ended session's among them).
            held.retain(|_, (_, until)| *until > now);
            held.insert(session.clone(), (t.to_owned(), until));
            t.to_owned()
        }
    };
    json!({"ok": true, "agent": true, "username": "kiln", "password": token})
}

/// An agent's commit signature (W27-R03): `message` (base64) signed with the key this runner
/// process holds for the asking process's session at its epoch. The session is told by the process
/// alone, its group of processes or a harness among its ancestors, never by what it says (the
/// mark any process can set): one outside every session gets nothing. The key stays here; the
/// signature and its certificate (DER) go back, base64.
async fn sign_for(host: &Host, peer: Option<u32>, message: &str) -> Value {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let refuse = |why: String| json!({"ok": false, "error": why});
    if peer.is_none() {
        return refuse("the system doesn't say which process asks, so nothing is signed".into());
    }
    let Some(session) = session_by_peer(host, peer).await else {
        return refuse("only a process of an agent session kiln runs here signs as that agent: its group of processes or its harness among its ancestors says so, never what it says".into());
    };
    let Some(epoch) = host.list().into_iter().find(|i| i.id == session).map(|i| i.epoch) else { return refuse(format!("agent session {session} is gone")) };
    let Ok(bytes) = b64.decode(message) else { return refuse("what to sign isn't base64".into()) };
    let slot = crate::runner::identity::slot("agent", Some(&session), Some(epoch));
    // A session just opened, or just moved to its epoch, may have its key's certificate on the way.
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while crate::runner::identity::cert(&slot).is_none() && std::time::Instant::now() < until {
        crate::rt::sleep(std::time::Duration::from_millis(100)).await;
    }
    match crate::runner::identity::sign(&slot, &bytes).and_then(|(sig, pem)| Ok((sig, crate::pki::der_of(&pem)?))) {
        Ok((sig, cert)) => json!({"ok": true, "session": session, "epoch": epoch, "signature": b64.encode(sig), "certificate": b64.encode(cert)}),
        Err(e) => refuse(format!("agent session {session} has no key at epoch {epoch} to sign with: {e:#}")),
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
/// path a terminal was given goes on working after an upgrade. Another process given the same
/// name and directory (a local server's own agent) never takes it from a live one ([`Kept`]).
pub fn path(name: &str, home: &std::path::Path) -> PathBuf {
    let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).take(32).collect();
    runtime_path(&format!("harness-{safe}-{}.sock", tag_of(home)))
}

/// What a socket's name says of the state directory `home` it was made for, so two kiln processes
/// on one machine (a server and runners, or test instances) never share one.
pub(crate) fn tag_of(home: &std::path::Path) -> String {
    crate::util::sha256_hex(home.to_string_lossy().as_bytes()).get(..8).unwrap_or("0").to_owned()
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
        Request::Send { id, text, mode, key } => {
            let ids = pick(&Some(id), &None);
            let Some(target) = ids.first() else { return json!({"ok": false, "error": "no such agent"}) };
            // With a key, the same message each time it's sent (CP-T3.2).
            let message = key.as_deref().map_or_else(crate::util::new_id, |k| crate::util::derive_id(&["socket.send", target, k]));
            let ok = host.command(target, Command::Send { id: message.clone(), text, mode, attachments: Vec::new(), overrides: None, reply_to: None, task: None });
            json!({"ok": ok, "message": message})
        }
        Request::Op { .. } => json!({"ok": false, "error": "kiln's operations go to the kiln server"}),
        // Signing and an agent's credential are told by the asking process, which only `answer` knows.
        Request::Sign { .. } => json!({"ok": false, "error": "signing asks the process's own runner (its socket's answer)"}),
        Request::Credential { .. } => json!({"ok": false, "error": "an agent's credential asks the process's own runner (its socket's answer)"}),
        // Not an agent kiln runs here: nothing of kiln's holds the call.
        Request::Hook { .. } => json!({"ok": true, "answer": {}}),
    }
}

/// Serve `host` on the socket at `path` until this process exits (a runner's: kiln's operations
/// go through its link to the server). `take`: this process takes the path over even from a live
/// one (a runner's new build once it has the machine's sessions, or its old one taking them
/// back), else it serves it once nothing else does ([`Kept`]). Unix only.
pub fn serve(host: &Arc<Host>, path: PathBuf, take: bool) -> anyhow::Result<PathBuf> {
    serve_with(host, path, None, take)
}

/// What became of a socket's path since this process bound it there.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Held {
    /// The file this process bound is there still.
    Ours,
    /// Another live process serves it.
    Theirs,
    /// Nothing answers there: no file, or the socket of a process that has ended.
    Dead,
    /// Whether anything answers there couldn't be told ([`Probe::Unknown`]): left as it is, and
    /// looked at again.
    Unknown(nix::errno::Errno),
}

/// The file at `path` (its device and inode), if there is one.
#[cfg(unix)]
fn file_id(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// What a probe of a socket's path found ([`probe`]).
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Probe {
    /// A live process listens there: the connection was made, or waits in its queue, which is full
    /// (Linux says so; macOS refuses it, and one of this process's own listeners is on the path's
    /// file, or another process of this user listens on the path).
    Answers,
    /// The probe reached the path and nothing listens there: no file (`ENOENT`), the file of a
    /// socket nothing listens on or a file that isn't a socket (`ECONNREFUSED`, on macOS once none
    /// of this process's listeners is on the path's file and no other process of this user listens
    /// on the path; `ENOTSOCK`, as macOS says the last).
    Dead,
    /// It couldn't tell: the probe couldn't be made (no descriptor or memory left for it, a path too
    /// long to name), or its connection failed some other way (no permission, a socket of another
    /// kind there). Never taken as dead: a loaded process or host fails a probe of a live socket so,
    /// and it was read as dead once, which let a sweep or a process binding the path remove the
    /// socket a live one serves.
    Unknown(nix::errno::Errno),
}

/// Whether a live process answers at `path` ([`Probe`]). Without waiting: a connection the listener
/// hasn't taken yet (its queue full) is a live one's.
#[cfg(unix)]
pub(crate) fn probe(path: &std::path::Path) -> Probe {
    probe_with(path, &mut Refused::new())
}

/// [`probe`], what a refused connection says looked up (macOS) at most once for all the probes made
/// with `refused` (a sweep's).
#[cfg(unix)]
fn probe_with(path: &std::path::Path, refused: &mut Refused) -> Probe {
    use nix::errno::Errno;
    use std::os::fd::AsRawFd as _;
    let made = nix::sys::socket::UnixAddr::new(path).and_then(|addr| Ok((addr, probe_socket()?)));
    let (addr, fd) = match made {
        Ok(made) => made,
        Err(e) => return Probe::Unknown(e),
    };
    match nix::sys::socket::connect(fd.as_raw_fd(), &addr) {
        Ok(()) | Err(Errno::EAGAIN | Errno::EINPROGRESS) => Probe::Answers,
        Err(Errno::ECONNREFUSED) => refused.meaning(path),
        Err(Errno::ENOENT | Errno::ENOTSOCK) => Probe::Dead,
        Err(e) => Probe::Unknown(e),
    }
}

/// The socket a probe connects with: non-blocking, and closed on exec, so a program started while
/// it's open never holds it, nor the connection it makes to whatever answers. Both at once where the
/// system can (Linux); macOS has no such flags, and sets them as soon as it's made.
#[cfg(unix)]
fn probe_socket() -> nix::Result<std::os::fd::OwnedFd> {
    use nix::sys::socket::{AddressFamily, SockFlag, SockType};
    #[cfg(target_os = "linux")]
    let fd = nix::sys::socket::socket(AddressFamily::Unix, SockType::Stream, SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC, None)?;
    #[cfg(not(target_os = "linux"))]
    let fd = {
        let fd = nix::sys::socket::socket(AddressFamily::Unix, SockType::Stream, SockFlag::empty(), None)?;
        nix::fcntl::fcntl(&fd, nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC))?;
        nix::fcntl::fcntl(&fd, nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK))?;
        fd
    };
    Ok(fd)
}

/// What a refused connection says ([`probe`]). Linux refuses one only where nothing listens (a
/// listener whose queue is full says EAGAIN): dead.
#[cfg(all(unix, not(target_os = "macos")))]
struct Refused;

#[cfg(all(unix, not(target_os = "macos")))]
impl Refused {
    fn new() -> Self {
        Self
    }

    fn meaning(&mut self, _path: &std::path::Path) -> Probe {
        Probe::Dead
    }
}

/// macOS refuses a connection where a listener's queue is full as where nothing listens: what it
/// says is looked up there ([`macos::Refused`]).
#[cfg(target_os = "macos")]
#[path = "socket_macos.rs"]
mod macos;
#[cfg(target_os = "macos")]
use macos::Refused;

/// Where `path` stands for a process that bound the file `ours` there (`None`: it bound none). Any
/// other file there, or none, is as a probe finds it.
#[cfg(unix)]
pub(crate) fn held(path: &std::path::Path, ours: Option<(u64, u64)>) -> Held {
    if ours.is_some() && file_id(path) == ours {
        return Held::Ours;
    }
    match probe(path) {
        Probe::Answers => Held::Theirs,
        Probe::Dead => Held::Dead,
        Probe::Unknown(e) => Held::Unknown(e),
    }
}

/// How old a socket's file must be before a sweep may take it as left behind: a process binds its
/// file a moment before it listens on it.
#[cfg(unix)]
const LEFT_AFTER: std::time::Duration = std::time::Duration::from_secs(60);
/// How often a process sweeps its sockets' folder while it runs.
#[cfg(unix)]
const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(600);

/// Removes the kiln sockets in `dir` (`harness-*.sock`, `reach-*.sock`) nothing answers on whose
/// files are older than `min_age`: each was left by a process that ended without removing it, as a
/// test's or a lab's runner does when it's killed, and nothing binds its path again. A socket a live
/// process serves answers, so it stays; so does one its probe couldn't tell of ([`Probe::Unknown`]),
/// looked at again by the next sweep. How many went.
#[cfg(unix)]
pub(crate) fn sweep(dir: &std::path::Path, min_age: std::time::Duration) -> usize {
    use std::os::unix::fs::FileTypeExt as _;
    let mut gone = 0;
    let mut refused = Refused::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("harness-") || name.starts_with("reach-")) || !name.ends_with(".sock") {
            continue;
        }
        let path = e.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        let old = meta.modified().ok().and_then(|m| m.elapsed().ok()).is_some_and(|age| age >= min_age);
        if !meta.file_type().is_socket() || !old {
            continue;
        }
        match probe_with(&path, &mut refused) {
            Probe::Dead => gone += usize::from(std::fs::remove_file(&path).is_ok()),
            Probe::Unknown(e) => tracing::debug!(path = %path.display(), error = %e, "the socket sweep couldn't tell whether a process answers here; it stays"),
            Probe::Answers => {}
        }
    }
    gone
}

/// [`sweep`] of the folder `path` is in, on a thread of its own, at most once per [`SWEEP_EVERY`]
/// in this process: when it first binds a socket, then while it serves them.
#[cfg(unix)]
fn sweep_now_and_then(path: &std::path::Path) {
    static LAST: parking_lot::Mutex<Option<std::time::Instant>> = parking_lot::Mutex::new(None);
    {
        let mut last = LAST.lock();
        if last.is_some_and(|t| t.elapsed() < SWEEP_EVERY) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    let Some(dir) = path.parent().map(std::path::Path::to_path_buf) else { return };
    let spawned = std::thread::Builder::new().name("kiln-socket-sweep".into()).spawn(move || {
        let gone = sweep(&dir, LEFT_AFTER);
        if gone > 0 {
            tracing::info!(dir = %dir.display(), gone, "removed the sockets of kiln processes that ended without removing them");
        }
    });
    if let Err(e) = spawned {
        tracing::debug!(error = %e, "the socket sweep couldn't start; it's tried again later");
    }
}

/// A listener this process bound at a path ([`bind`]). On macOS, while it listens, its file is one
/// this process's probes know it listens on (`socket_macos.rs`): a refused probe judges this
/// process's own listeners by the files they're bound to, as libproc names a listener only by the
/// path it was bound to, which stays its name after its file is replaced.
#[cfg(unix)]
pub(crate) struct Listening {
    listener: std::os::unix::net::UnixListener,
    #[cfg(target_os = "macos")]
    id: (u64, u64),
}

#[cfg(unix)]
impl Listening {
    /// `listener`, bound to the file `id`.
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    fn new(listener: std::os::unix::net::UnixListener, id: (u64, u64)) -> Self {
        #[cfg(target_os = "macos")]
        macos::listens(id);
        Self {
            listener,
            #[cfg(target_os = "macos")]
            id,
        }
    }
}

#[cfg(unix)]
impl std::ops::Deref for Listening {
    type Target = std::os::unix::net::UnixListener;

    fn deref(&self) -> &Self::Target {
        &self.listener
    }
}

#[cfg(target_os = "macos")]
impl Drop for Listening {
    fn drop(&mut self) {
        macos::stops(self.id);
    }
}

/// A listener on `path`, in a directory only this user enters, and the file it's bound to; `None`
/// when another live process serves the path, or whether one does couldn't be told
/// ([`Probe::Unknown`]: the path is left as it is, for a [`Kept`] to look at again), and this one
/// doesn't `take` it. A second kiln process given the machine's runner's name and state directory
/// (a local server's own agent, restarted as a new kiln was installed) must never cut the runner
/// off from the agents and terminals it serves. A file nothing answers on was left by a process
/// that has ended, and goes.
#[cfg(unix)]
pub(crate) fn bind(path: &std::path::Path, take: bool) -> anyhow::Result<Option<(Listening, (u64, u64))>> {
    use std::os::unix::fs::PermissionsExt as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    if !take {
        match probe(path) {
            Probe::Dead => {}
            Probe::Answers => return Ok(None),
            Probe::Unknown(e) => {
                tracing::info!(path = %path.display(), error = %e, "whether a live process serves this socket couldn't be told: it's left as it is, and looked at again");
                return Ok(None);
            }
        }
    }
    let _ = std::fs::remove_file(path);
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let id = file_id(path).ok_or_else(|| anyhow::anyhow!("{} was gone as soon as it was bound", path.display()))?;
    Ok(Some((Listening::new(listener, id), id)))
}

/// How often a quiet socket looks at its path; and while another process serves it.
#[cfg(unix)]
const LOOK: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(unix)]
const LOOK_THEIRS: std::time::Duration = std::time::Duration::from_secs(30);

/// A socket path this process serves, or will. Looked at while it's quiet: once it's left dead
/// (its file gone, or the socket of a process that has ended) this process binds it again; while
/// another live process serves it, it's left to that one until it ends. So a machine's runner gets
/// its sockets back from whatever took them and then went, and no two processes take one from each
/// other.
#[cfg(unix)]
pub(crate) struct Kept {
    path: PathBuf,
    what: &'static str,
    listener: Option<Listening>,
    ours: Option<(u64, u64)>,
    looked: std::time::Instant,
    theirs: bool,
}

#[cfg(unix)]
impl Kept {
    /// Bound now (`take`: as [`bind`] says), or once nothing else serves it. `what` names it in the
    /// log.
    pub(crate) fn new(path: PathBuf, take: bool, what: &'static str) -> anyhow::Result<Self> {
        let bound = bind(&path, take)?;
        sweep_now_and_then(&path);
        if bound.is_none() {
            tracing::info!(path = %path.display(), "{what}: another kiln process serves this machine's socket, or may; this one serves it once nothing answers there");
        }
        let theirs = bound.is_none();
        let (listener, ours) = bound.map_or((None, None), |(l, id)| (Some(l), Some(id)));
        Ok(Self { path, what, listener, ours, looked: std::time::Instant::now(), theirs })
    }

    pub(crate) fn listener(&self) -> Option<&std::os::unix::net::UnixListener> {
        self.listener.as_deref()
    }

    /// [`Kept::look`], if it's time to.
    pub(crate) fn check(&mut self) -> bool {
        if self.looked.elapsed() < if self.theirs { LOOK_THEIRS } else { LOOK } {
            return false;
        }
        sweep_now_and_then(&self.path);
        self.look()
    }

    /// Look at the path now, and bind it again if it's dead: true when this process serves it
    /// anew.
    pub(crate) fn look(&mut self) -> bool {
        self.looked = std::time::Instant::now();
        match held(&self.path, self.ours) {
            Held::Ours => {
                self.theirs = false;
                false
            }
            Held::Theirs => {
                self.theirs = true;
                false
            }
            Held::Unknown(e) => {
                tracing::debug!(path = %self.path.display(), error = %e, "{}: whether another process serves this machine's socket couldn't be told; it's looked at again", self.what);
                false
            }
            Held::Dead => match bind(&self.path, false) {
                Ok(Some((l, id))) => {
                    tracing::info!(path = %self.path.display(), "{}: nothing answered at this machine's socket; this process serves it again", self.what);
                    (self.listener, self.ours, self.theirs) = (Some(l), Some(id), false);
                    true
                }
                Ok(None) => {
                    self.theirs = true;
                    false
                }
                Err(e) => {
                    tracing::warn!(path = %self.path.display(), error = %format!("{e:#}"), "{}: this machine's socket couldn't be bound again", self.what);
                    false
                }
            },
        }
    }
}

static SERVED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static OWN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The socket this process serves, once it does: every harness it starts gets it as
/// `KILN_HARNESS_SOCK`, so `kiln spawn` and `kiln agents` work in an agent's own commands too
/// (they're told apart from a person's by their process).
pub fn served() -> Option<&'static str> {
    SERVED.get().map(String::as_str)
}

/// This process's own socket, once it serves one ([`serve_own`]): every harness it starts gets it
/// as `KILN_RUNNER_SOCK`, and Codex's hook names it.
pub fn own_served() -> Option<&'static str> {
    OWN.get().map(String::as_str)
}

/// Where this process's own socket ([`serve_own`]) lives: beside the machine's, `path`, named for
/// this process. `None` where that's too long for a socket's path.
#[cfg(unix)]
pub(crate) fn own_path(path: &std::path::Path) -> Option<PathBuf> {
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let own = path.with_file_name(format!("{stem}-p{}.sock", std::process::id()));
    (own.as_os_str().len() < 100).then_some(own)
}

/// The same, kiln's operations answered by `forward` (the server's own socket).
#[cfg(unix)]
pub fn serve_with(host: &Arc<Host>, path: PathBuf, forward: Option<Forward>, take: bool) -> anyhow::Result<PathBuf> {
    use std::os::fd::AsRawFd as _;

    let mut kept = Kept::new(path.clone(), take, "`kiln spawn`")?;
    let _ = SERVED.set(path.to_string_lossy().into_owned());
    serve_own(host, &path, forward.clone());
    let host = Arc::clone(host);
    crate::rt::spawn(async move {
        loop {
            let Some(listener) = kept.listener() else {
                // Another process serves it: looked at now and then, served once it's gone.
                crate::rt::sleep(std::time::Duration::from_secs(1)).await;
                kept.check();
                continue;
            };
            let fd = listener.as_raw_fd();
            match listener.accept() {
                Ok((stream, _)) => answer_connection(stream, Arc::clone(&host), forward.clone(), false),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Woken when a connection waits, and looked at again each second regardless:
                    // a wakeup missed costs a second, never an asker left waiting. Quiet, it
                    // looks at its path too.
                    if let Ok(Err(_)) = crate::rt::timeout(std::time::Duration::from_secs(1), hyper_rt::readiness::readable(fd)).await {
                        crate::rt::sleep(std::time::Duration::from_millis(200)).await;
                    }
                    kept.check();
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

/// The socket of this process's own, beside the machine's `path`: what a session asks reaches the
/// runner that started it, which alone knows it as an agent and holds its key, even while another
/// kiln process of the machine serves the machine's socket. An upgrade's new build runs beside the
/// old one, which keeps the machine's socket until it ends: on 2026-11-02 every commit of a session
/// adacachy's new build had started went to the old one, which neither knew the request nor held
/// the key (W27-R03); its hooks went there too, held by nothing, and its `kiln spawn` and `kiln
/// agents stop` were taken as its person's. Its harnesses get it as `KILN_RUNNER_SOCK`
/// ([`own_served`]); once this process has ended, the sweep takes it.
#[cfg(unix)]
fn serve_own(host: &Arc<Host>, path: &std::path::Path, forward: Option<Forward>) {
    use std::os::fd::AsRawFd as _;

    let Some(own) = own_path(path) else {
        tracing::warn!(path = %path.display(), "no room beside the machine's socket for this process's own: its sessions ask the machine's");
        return;
    };
    let listener = match bind(&own, true) {
        Ok(Some((l, _))) => l,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(path = %own.display(), error = %format!("{e:#}"), "this process's own socket couldn't be bound: its sessions ask the machine's");
            return;
        }
    };
    let _ = OWN.set(own.to_string_lossy().into_owned());
    let host = Arc::clone(host);
    crate::rt::spawn(async move {
        let fd = listener.as_raw_fd();
        loop {
            match listener.accept() {
                Ok((stream, _)) => answer_connection(stream, Arc::clone(&host), forward.clone(), true),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if let Ok(Err(_)) = crate::rt::timeout(std::time::Duration::from_secs(1), hyper_rt::readiness::readable(fd)).await {
                        crate::rt::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    tracing::warn!(error = %e, "accept on this process's own socket failed");
                    crate::rt::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
    });
}

/// One connection on a socket this process serves, answered on a task of its own: one request
/// line, its answer. `own`: on this process's own socket ([`answer_own`]).
#[cfg(unix)]
fn answer_connection(stream: std::os::unix::net::UnixStream, host: Arc<Host>, forward: Option<Forward>, own: bool) {
    use std::os::fd::OwnedFd;

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
            Ok(req) if own => answer_own(&host, req, peer, forward.as_ref()).await,
            Ok(req) => answer(&host, req, peer, forward.as_ref()).await,
            Err(e) => json!({"ok": false, "error": format!("bad request: {e}")}),
        };
        let mut out = answer.to_string().into_bytes();
        out.push(b'\n');
        let _ = crate::os::proc::write_all(&fd, &out).await;
    });
}

/// A request on this process's own socket ([`serve_own`]), from a process of a session this
/// process runs: answered as on the machine's socket (its `kiln spawn` and `kiln agents`, its
/// hooks, its signature and its credential). Asked by a process of no session here (one an upgrade
/// handed to the machine's new build, or anything else), it says so (`elsewhere`), and the asker
/// asks the machine's socket instead (`cli::request_first`).
#[cfg(unix)]
async fn answer_own(host: &Host, req: Request, peer: Option<u32>, forward: Option<&Forward>) -> Value {
    if session_by_peer(host, peer).await.is_none() {
        return json!({"ok": false, "elsewhere": true, "error": "no agent session this kiln process runs asks: the machine's socket answers it"});
    }
    answer(host, req, peer, forward).await
}

/// No local socket on Windows yet: `kiln spawn` is not available there.
#[cfg(not(unix))]
pub fn serve_with(_host: &Arc<Host>, _path: PathBuf, _forward: Option<Forward>, _take: bool) -> anyhow::Result<PathBuf> {
    anyhow::bail!("kiln spawn needs Unix sockets")
}

/// Leaves at `path`, in place of whatever was there, the file of a socket nothing listens on, as a
/// process that ended does, for tests: bound and never listened on (by a socket made as a probe's
/// is, closed on exec). One bound to listen and closed stays listening while a program another test
/// starts meanwhile holds the process's descriptors (until it runs its own, later on a loaded host),
/// and a sweep found it live (2026-11-07, `harness::` beside busy loops).
#[cfg(all(test, unix))]
pub(crate) fn leave_ended(path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd as _;
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    nix::sys::socket::bind(probe_socket()?.as_raw_fd(), &nix::sys::socket::UnixAddr::new(path)?)?;
    Ok(())
}

/// `listener`, a listener bound at `path`, as this process's own as [`bind`] makes them, for tests
/// whose listener needs what `bind` doesn't give it (a queue of their own size).
#[cfg(all(test, unix))]
fn own_listener(listener: std::os::fd::OwnedFd, path: &std::path::Path) -> anyhow::Result<Listening> {
    let id = file_id(path).ok_or_else(|| anyhow::anyhow!("{} isn't there", path.display()))?;
    Ok(Listening::new(std::os::unix::net::UnixListener::from(listener), id))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 2026-10-29 outage left 1,532 sockets of ended test and lab runners in the runtime
    /// folder: a sweep takes those nothing answers on, and leaves a live one, a young one (bound,
    /// not yet listening), and anything not a kiln socket. The ended ones are left as
    /// [`leave_ended`] leaves them: bound to listen and closed, one stayed live in a program another
    /// test started meanwhile, and wasn't swept.
    #[cfg(unix)]
    #[test]
    fn sockets_nothing_answers_on_are_swept_and_live_ones_stay() -> anyhow::Result<()> {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir()?;
        let ended = |name: &str| -> anyhow::Result<PathBuf> {
            let p = dir.path().join(name);
            leave_ended(&p)?;
            Ok(p)
        };
        let (dead_reach, dead_harness) = (ended("reach-lab-0-deadbeef.sock")?, ended("harness-lab-0-deadbeef.sock")?);
        let live = dir.path().join("reach-adacachy-4b48ed43.sock");
        let _listening = UnixListener::bind(&live)?;
        let (foreign, notes) = (ended("someone-else.sock")?, dir.path().join("reach-notes.sock"));
        std::fs::write(&notes, "not a socket")?;
        anyhow::ensure!(sweep(dir.path(), std::time::Duration::from_secs(3600)) == 0, "a young socket went");
        let gone = sweep(dir.path(), std::time::Duration::ZERO);
        anyhow::ensure!(gone == 2, "{gone} went: not just the two ended kiln sockets");
        anyhow::ensure!(!dead_reach.exists() && !dead_harness.exists(), "an ended runner's socket stayed");
        anyhow::ensure!(live.exists() && foreign.exists() && notes.exists(), "a live socket, another program's or a plain file went");
        Ok(())
    }

    /// A folder under `dir` whose sockets' paths are too long to name in a socket's address (Linux:
    /// a socket is bound there through the folder's descriptor), and that descriptor.
    #[cfg(target_os = "linux")]
    fn too_deep(dir: &std::path::Path) -> anyhow::Result<(PathBuf, std::fs::File)> {
        let mut deep = dir.to_path_buf();
        while nix::sys::socket::UnixAddr::new(&deep).is_ok() {
            deep.push("a-folder-deeper-than-a-socket-names");
        }
        std::fs::create_dir_all(&deep)?;
        let folder = std::fs::File::open(&deep)?;
        Ok((deep, folder))
    }

    /// A probe ([`probe`]) says a socket is dead only where it reached the path and nothing listens
    /// there: no file, the file of a socket nothing listens on, a file that isn't a socket. A live
    /// listener whose queue is full answers. Where it couldn't reach a listener it can't tell (a
    /// socket of another kind there, a live one it may not reach, a path too long to name), as where
    /// it can't be made for want of a descriptor or memory on a loaded host: never dead, as each of
    /// them once was.
    #[cfg(unix)]
    #[test]
    fn a_probe_says_dead_only_where_it_reached_the_path_and_nothing_listens() -> anyhow::Result<()> {
        use nix::errno::Errno;
        use std::os::unix::fs::PermissionsExt as _;
        use std::os::unix::net::{UnixDatagram, UnixListener};
        let dir = tempfile::tempdir()?;
        let at = |name: &str| dir.path().join(name);
        let _live = UnixListener::bind(at("live.sock"))?;
        leave_ended(&at("ended.sock"))?;
        std::fs::write(at("plain.sock"), "not a socket")?;
        let _other = UnixDatagram::bind(at("other.sock"))?;
        let mut cases = vec![("live.sock", Probe::Answers), ("ended.sock", Probe::Dead), ("missing.sock", Probe::Dead), ("plain.sock", Probe::Dead), ("other.sock", Probe::Unknown(Errno::EPROTOTYPE))];
        // A live socket it may not reach (root may).
        let _shut = UnixListener::bind(at("shut.sock"))?;
        std::fs::set_permissions(at("shut.sock"), std::fs::Permissions::from_mode(0o000))?;
        cases.push(("shut.sock", if nix::unistd::geteuid().is_root() { Probe::Answers } else { Probe::Unknown(Errno::EACCES) }));
        for (name, want) in cases {
            let got = probe(&at(name));
            anyhow::ensure!(got == want, "{name}: {got:?}, not {want:?}");
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd as _;
            // A live socket whose path is too long to name: no probe of it can be made.
            let (deep, folder) = too_deep(dir.path())?;
            let _far = UnixListener::bind(format!("/proc/self/fd/{}/far.sock", folder.as_raw_fd()))?;
            let far = deep.join("far.sock");
            anyhow::ensure!(far.exists() && probe(&far) == Probe::Unknown(Errno::ENAMETOOLONG), "a live socket too far to name: {:?}", probe(&far));
        }
        Ok(())
    }

    /// Connections made to `path` as a probe's are, queued until its listener takes no more (Linux: one
    /// past its backlog, then EAGAIN; macOS: half again as many, then refused), at most `room` (a
    /// queue that never fills is said).
    #[cfg(unix)]
    fn queued_until_full(path: &std::path::Path, room: usize) -> anyhow::Result<Vec<std::os::fd::OwnedFd>> {
        use nix::errno::Errno;
        use std::os::fd::AsRawFd as _;
        let mut queued = Vec::new();
        loop {
            let c = probe_socket()?;
            match nix::sys::socket::connect(c.as_raw_fd(), &nix::sys::socket::UnixAddr::new(path)?) {
                Ok(()) | Err(Errno::EINPROGRESS) => queued.push(c),
                Err(Errno::EAGAIN | Errno::ECONNREFUSED) => return Ok(queued),
                Err(e) => anyhow::bail!("a connection to queue: {e}"),
            }
            anyhow::ensure!(queued.len() <= room, "its queue took {} and never filled", queued.len());
        }
    }

    /// A live listener whose queue is full answers, and keeps its path: Linux says so (EAGAIN); macOS
    /// refuses the connection as where nothing listens (xnu's `sonewconn`), and its probe finds the
    /// listener (`socket_macos.rs`): another process's by the path it's bound to, this process's own
    /// by the path's file. It was read as dead there, and a busy listener lost its path to a sweep, or
    /// to a process binding it. Once this process's own has its file replaced by a dead one, the path
    /// is dead: its listener's name, which is still the path, says nothing of the file there now (taken
    /// by name, that kept a dead path live on a Mac, and kiln's `Kept` never served it again).
    #[cfg(unix)]
    #[test]
    fn a_listener_whose_queue_is_full_answers_and_keeps_its_path() -> anyhow::Result<()> {
        use nix::sys::socket::{Backlog, UnixAddr};
        use std::io::BufRead as _;
        use std::os::fd::AsRawFd as _;
        let dir = tempfile::tempdir()?;
        let backlog = 1;
        let room = 2 * usize::try_from(backlog)? + 1;
        // This process's own, bound as kiln binds its sockets ([`own_listener`]), with a queue of one,
        // closed on exec as kiln's are: one the child below inherited would be a listener of another
        // process at the path, and the path live by it.
        let own = dir.path().join("harness-busy-0b05ee01.sock");
        let fd = probe_socket()?;
        nix::sys::socket::bind(fd.as_raw_fd(), &UnixAddr::new(&own)?)?;
        nix::sys::socket::listen(&fd, Backlog::new(backlog)?)?;
        let listener = own_listener(fd, &own)?;
        let _own_queued = queued_until_full(&own, room)?;
        anyhow::ensure!(probe(&own) == Probe::Answers, "this process's own listener, its queue full: {:?}", probe(&own));
        // Another process's: a child listening with a queue of one, never taking a connection (it
        // ends as its input does).
        let theirs = dir.path().join("reach-busy-0b05ee00.sock");
        let child = std::process::Command::new("python3")
            .args(["-c", &format!("import socket, sys; s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen({backlog}); print('listening', flush=True); sys.stdin.read()"), &theirs.to_string_lossy()])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(child) => Some(child),
            Err(e) => {
                eprintln!("another process's busy listener not tried: no python3 ({e})");
                None
            }
        };
        let mut _theirs_queued = Vec::new();
        if let Some(c) = child.as_mut() {
            let mut line = String::new();
            std::io::BufReader::new(c.stdout.take().ok_or_else(|| anyhow::anyhow!("the child has no stdout"))?).read_line(&mut line)?;
            anyhow::ensure!(line.trim() == "listening", "the child said {line:?}");
            _theirs_queued = queued_until_full(&theirs, room)?;
            anyhow::ensure!(probe(&theirs) == Probe::Answers, "another process's listener, its queue full: {:?}", probe(&theirs));
            anyhow::ensure!(bind(&theirs, false)?.is_none() && theirs.exists(), "a process took a busy listener's path");
        }
        anyhow::ensure!(sweep(dir.path(), std::time::Duration::ZERO) == 0 && own.exists(), "a sweep took a busy listener's socket");
        anyhow::ensure!(bind(&own, false)?.is_none() && own.exists(), "a process took its own busy listener's path");
        // Its file replaced by a dead one, its listener still listening: dead.
        leave_ended(&own)?;
        anyhow::ensure!(probe(&own) == Probe::Dead, "a dead file where this process's own listener's was: {:?}", probe(&own));
        drop(listener);
        if let Some(mut c) = child {
            drop(c.stdin.take());
            let _ = c.wait();
        }
        Ok(())
    }

    /// A probe's socket is closed on exec, and never blocks: a program started while one is open (by
    /// another thread) doesn't hold it, nor the connection it made. It was made without close-on-exec,
    /// and a program started meanwhile kept it, connected to whatever answered, for as long as it ran.
    #[cfg(unix)]
    #[test]
    fn a_probes_socket_is_closed_on_exec_and_never_blocks() -> anyhow::Result<()> {
        use std::os::fd::AsRawFd as _;
        let fd = probe_socket()?;
        let flags = nix::fcntl::OFlag::from_bits_truncate(nix::fcntl::fcntl(&fd, nix::fcntl::FcntlArg::F_GETFL)?);
        anyhow::ensure!(flags.contains(nix::fcntl::OFlag::O_NONBLOCK), "a probe waits on a listener whose queue is full");
        let held = std::process::Command::new("sh").args(["-c", &format!("test -e /dev/fd/{}", fd.as_raw_fd())]).status()?;
        anyhow::ensure!(!held.success(), "a program started while a probe's socket was open holds it");
        Ok(())
    }

    /// A socket its probe couldn't tell of ([`Probe::Unknown`]) is left as it is: a sweep leaves it,
    /// as it leaves a live one; a process that doesn't take its path leaves it to whatever is there;
    /// and a [`Kept`] looking at it does nothing. Each once read it as dead: the sweep removed it, and
    /// a process starting took its path from a live one.
    #[cfg(unix)]
    #[test]
    fn a_socket_its_probe_couldnt_tell_of_is_neither_swept_nor_bound_over() -> anyhow::Result<()> {
        use nix::errno::Errno;
        use std::os::unix::fs::PermissionsExt as _;
        use std::os::unix::net::{UnixDatagram, UnixListener};
        let dir = tempfile::tempdir()?;
        let at = |name: &str| dir.path().join(name);
        leave_ended(&at("reach-lab-0-deadbeef.sock"))?;
        let _live = UnixListener::bind(at("reach-adacachy-4b48ed43.sock"))?;
        // A live socket of another kind (a datagram's) at a kiln socket's path: the probe's
        // connection fails, as one that can't be made on a loaded host does.
        let other = at("harness-other-0badf00d.sock");
        let theirs = UnixDatagram::bind(&other)?;
        // A live one it may not reach (root may, and finds it live).
        let shut = at("harness-shut-0c0ffee0.sock");
        let _shut = UnixListener::bind(&shut)?;
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000))?;
        anyhow::ensure!(sweep(dir.path(), std::time::Duration::ZERO) == 1, "not just the ended kiln socket went");
        anyhow::ensure!(!at("reach-lab-0-deadbeef.sock").exists(), "the ended socket stayed");
        for p in [at("reach-adacachy-4b48ed43.sock"), other.clone(), shut.clone()] {
            anyhow::ensure!(p.exists(), "{} went", p.display());
        }
        // Not taken: whatever is there still has it, and where it stands can't be told.
        anyhow::ensure!(bind(&other, false)?.is_none(), "a path its probe couldn't tell of was bound over");
        anyhow::ensure!(held(&other, None) == Held::Unknown(Errno::EPROTOTYPE), "{:?}", held(&other, None));
        let sender = UnixDatagram::unbound()?;
        sender.send_to(b"still there", &other)?;
        let mut got = [0u8; 16];
        anyhow::ensure!(theirs.recv(&mut got).ok() == Some(11), "the socket that was there lost its path");
        let mut kept = Kept::new(other.clone(), false, "test")?;
        anyhow::ensure!(kept.listener().is_none() && !kept.look() && kept.listener().is_none(), "a Kept took a path its probe couldn't tell of");
        if !nix::unistd::geteuid().is_root() {
            anyhow::ensure!(bind(&shut, false)?.is_none() && held(&shut, None) == Held::Unknown(Errno::EACCES), "a live socket it may not reach was bound over");
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd as _;
            // A live socket whose path is too long to name: no probe of it can be made, and neither
            // the sweep nor a process binding its path removes it.
            let (deep, folder) = too_deep(dir.path())?;
            let _far = UnixListener::bind(format!("/proc/self/fd/{}/reach-far-0f0f0f0f.sock", folder.as_raw_fd()))?;
            let far = deep.join("reach-far-0f0f0f0f.sock");
            anyhow::ensure!(sweep(&deep, std::time::Duration::ZERO) == 0 && far.exists(), "a live socket too far to name was swept");
            anyhow::ensure!(bind(&far, false)?.is_none() && far.exists(), "a live socket too far to name was bound over");
        }
        Ok(())
    }

    /// One request on the socket at `path`, from this process (on the blocking pool: the socket is
    /// served on the runtime). Linux's, as the one test that asks is (on macOS it was never used,
    /// which its clippy refuses).
    #[cfg(target_os = "linux")]
    async fn ask(path: PathBuf, req: Value) -> anyhow::Result<Value> {
        crate::rt::blocking(move || -> anyhow::Result<Value> {
            use std::io::{BufRead as _, Write as _};
            let mut s = std::os::unix::net::UnixStream::connect(&path)?;
            // A loaded machine (the whole suite at once) takes its time: an answer, not a race.
            s.set_read_timeout(Some(std::time::Duration::from_secs(60)))?;
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
                serve_with(&hub.harness.local, path.clone(), Some(crate::harness::ops::forward(Arc::clone(&hub))), false)?;
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
        match rx.recv_timeout(std::time::Duration::from_secs(180)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("the socket hung"),
        }
    }

    /// W27-R03, as on adacachy on 2026-11-02: an upgrade's old build keeps the machine's socket and
    /// knows neither a request for a key nor the new build's sessions. The new build's own socket
    /// answers for the sessions it runs (here this test's process, as a session's harness): its
    /// credential, and its `kiln spawn` as the agent (its child), not as its person. Its sessions get
    /// it as `KILN_RUNNER_SOCK`.
    #[cfg(target_os = "linux")]
    #[test]
    fn w27_r03_a_runner_beside_the_one_serving_the_machines_socket_answers_its_own_sessions() -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = crate::rt::block_on(async {
                let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
                let dir = tempfile::tempdir()?.keep();
                let spec = Spec { kind: "claude".into(), cwd: Some(dir.to_string_lossy().into_owned()), mode: Some("default".into()), program: Some(dir.join("no-claude").to_string_lossy().into_owned()), ..Spec::default() };
                let session = hub.harness.open(&hub, "local", spec, 1).await?.first().map(|i| i.id.clone()).ok_or_else(|| anyhow::anyhow!("no session"))?;
                let s = hub.harness.local.get(&session).ok_or_else(|| anyhow::anyhow!("not on the host"))?;
                for _ in 0..200 {
                    if s.info().status == crate::harness::model::Status::Exited {
                        break;
                    }
                    crate::rt::sleep(std::time::Duration::from_millis(20)).await;
                }
                *s.pid.lock() = Some(std::process::id());
                // The old build, serving the machine's socket.
                let path = dir.join("h.sock");
                let old = std::os::unix::net::UnixListener::bind(&path)?;
                std::thread::spawn(move || {
                    use std::io::{BufRead as _, Write as _};
                    for mut c in old.incoming().flatten() {
                        let mut line = String::new();
                        if let Ok(r) = c.try_clone() {
                            let _ = std::io::BufReader::new(r).read_line(&mut line);
                        }
                        let _ = c.write_all(b"{\"ok\":false,\"error\":\"bad request: unknown variant `credential`, expected one of `spawn`, `list`, `stop`, `interrupt`, `send`, `op`\"}\n");
                    }
                });
                serve_with(&hub.harness.local, path.clone(), Some(crate::harness::ops::forward(Arc::clone(&hub))), false)?;
                let own = own_path(&path).ok_or_else(|| anyhow::anyhow!("no path for this process's own socket"))?;
                anyhow::ensure!(own_served().is_some_and(|k| k.ends_with(&format!("-p{}.sock", std::process::id()))), "its sessions aren't given a socket of its own");
                // Its session's credential, asked as kiln-git-sign asks it: by this process, of its own socket.
                let mut answered = serde_json::Value::Null;
                for _ in 0..100 {
                    let (own, machine) = (own.clone(), path.clone());
                    match crate::rt::blocking(move || crate::harness::cli::request_first(Some(&own), &machine, &Request::Credential { forget: false }, std::time::Duration::from_secs(60))).await? {
                        Ok(v) => answered = v,
                        Err(e) => answered = json!({"refused": format!("{e:#}")}),
                    }
                    if !answered.to_string().contains("cannot reach kiln") {
                        break;
                    }
                    crate::rt::sleep(std::time::Duration::from_millis(50)).await;
                }
                let said = answered.to_string();
                anyhow::ensure!(!said.contains("unknown variant"), "asked of the old build: {said}");
                anyhow::ensure!(answered.get("agent") == Some(&json!(true)) || said.contains(&session), "not answered for its session: {said}");
                // Its `kiln spawn`, as the agent: what it starts is its child.
                let nowhere = "/nonexistent/kiln-socket-test";
                let started = ask(own.clone(), json!({"op": "spawn", "kind": "claude", "mode": "plan", "cwd": nowhere})).await?;
                let child = started.pointer("/sessions/0/id").and_then(Value::as_str).unwrap_or("").to_owned();
                anyhow::ensure!(hub.harness.info(&child).is_some_and(|i| i.parent.as_deref() == Some(session.as_str())), "not started as the agent's child: {started}");
                let ended = ask(own, json!({"op": "stop", "id": child})).await?;
                anyhow::ensure!(ended.get("ok") == Some(&json!(true)), "{ended}");
                Ok(())
            });
            let _ = tx.send(r);
        });
        match rx.recv_timeout(std::time::Duration::from_secs(180)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("the socket hung"),
        }
    }

    /// Every connection waiting on `l`, taken: how many.
    #[cfg(unix)]
    fn drain(l: &std::os::unix::net::UnixListener) -> usize {
        std::iter::from_fn(|| l.accept().ok()).count()
    }

    /// A second kiln process given the runner's name and state directory (a local server's own
    /// agent, as on adacachy on 2026-10-25) leaves a socket another live process serves to that
    /// one; a runner's new build taking the machine over takes it.
    #[cfg(unix)]
    #[test]
    fn a_socket_a_live_process_serves_is_left_to_it_unless_taken_over() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("h.sock");
        let (first, first_id) = bind(&path, false)?.ok_or_else(|| anyhow::anyhow!("the free path wasn't bound"))?;
        anyhow::ensure!(bind(&path, false)?.is_none(), "a socket a live process serves was taken");
        anyhow::ensure!(held(&path, Some(first_id)) == Held::Ours);
        let _asker = std::os::unix::net::UnixStream::connect(&path)?;
        anyhow::ensure!(drain(&first) >= 1, "the first process lost its socket");
        let (second, second_id) = bind(&path, true)?.ok_or_else(|| anyhow::anyhow!("not taken over"))?;
        anyhow::ensure!(held(&path, Some(second_id)) == Held::Ours && held(&path, Some(first_id)) == Held::Theirs);
        let _asker = std::os::unix::net::UnixStream::connect(&path)?;
        anyhow::ensure!(drain(&second) >= 1 && drain(&first) == 0, "the socket taken over didn't move");
        Ok(())
    }

    /// A socket left dead (what took it has ended) is served again by the process keeping it the
    /// next time it looks; one another live process serves is left to that one until it ends.
    #[cfg(unix)]
    #[test]
    fn a_kept_socket_left_dead_is_served_again() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("h.sock");
        let mut kept = Kept::new(path.clone(), false, "test")?;
        anyhow::ensure!(kept.listener().is_some() && !kept.look(), "the free path wasn't served");
        // Something takes the path and ends: its file stays, and nothing answers there. Each process
        // that ends here leaves its path as [`leave_ended`] does: one bound to listen and closed stays
        // live in a program another test starts meanwhile.
        leave_ended(&path)?;
        anyhow::ensure!(held(&path, kept.ours) == Held::Dead);
        anyhow::ensure!(kept.look(), "the dead path wasn't served again");
        let _asker = std::os::unix::net::UnixStream::connect(&path)?;
        anyhow::ensure!(kept.listener().is_some_and(|l| drain(l) >= 1), "the socket served again took no connection");
        // While another live process serves it, it's that one's; once that one ends, it's back.
        let other = bind(&path, true)?;
        anyhow::ensure!(!kept.look() && held(&path, kept.ours) == Held::Theirs, "a live process's socket was taken back");
        drop(other);
        leave_ended(&path)?;
        anyhow::ensure!(kept.look(), "not served again once the other process ended");
        // A process that starts while another serves the path waits, and serves it once that one ends.
        let (live, _) = bind(&path, true)?.ok_or_else(|| anyhow::anyhow!("not taken over"))?;
        let mut later = Kept::new(path.clone(), false, "test")?;
        anyhow::ensure!(later.listener().is_none(), "a live process's socket was taken at start");
        drop(live);
        leave_ended(&path)?;
        anyhow::ensure!(later.look() && later.listener().is_some(), "not served once the path was free");
        Ok(())
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
