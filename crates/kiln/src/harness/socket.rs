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
}

/// The file at `path` (its device and inode), if there is one.
#[cfg(unix)]
fn file_id(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// Whether a live process answers at `path`. Without waiting: a connection the listener hasn't
/// taken yet (its queue full) is a live one's.
#[cfg(unix)]
pub(crate) fn answers(path: &std::path::Path) -> bool {
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr};
    use std::os::fd::AsRawFd as _;
    let Ok(addr) = UnixAddr::new(path) else { return false };
    let Ok(fd) = nix::sys::socket::socket(AddressFamily::Unix, SockType::Stream, SockFlag::empty(), None) else { return false };
    if nix::fcntl::fcntl(&fd, nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK)).is_err() {
        return false;
    }
    matches!(nix::sys::socket::connect(fd.as_raw_fd(), &addr), Ok(()) | Err(nix::errno::Errno::EAGAIN | nix::errno::Errno::EINPROGRESS))
}

/// Where `path` stands for a process that bound the file `ours` there (`None`: it bound none).
#[cfg(unix)]
pub(crate) fn held(path: &std::path::Path, ours: Option<(u64, u64)>) -> Held {
    match file_id(path) {
        Some(f) if Some(f) == ours => Held::Ours,
        Some(_) if answers(path) => Held::Theirs,
        _ => Held::Dead,
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
/// process serves answers, so it stays. How many went.
#[cfg(unix)]
pub(crate) fn sweep(dir: &std::path::Path, min_age: std::time::Duration) -> usize {
    use std::os::unix::fs::FileTypeExt as _;
    let mut gone = 0;
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("harness-") || name.starts_with("reach-")) || !name.ends_with(".sock") {
            continue;
        }
        let path = e.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        let old = meta.modified().ok().and_then(|m| m.elapsed().ok()).is_some_and(|age| age >= min_age);
        if meta.file_type().is_socket() && old && !answers(&path) && std::fs::remove_file(&path).is_ok() {
            gone += 1;
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

/// A listener on `path`, in a directory only this user enters, and the file it's bound to; `None`
/// when another live process serves the path and this one doesn't `take` it. A second kiln process
/// given the machine's runner's name and state directory (a local server's own agent, restarted
/// as a new kiln was installed) must never cut the runner off from the agents and terminals it
/// serves. A file nothing answers on was left by a process that has ended, and goes.
#[cfg(unix)]
pub(crate) fn bind(path: &std::path::Path, take: bool) -> anyhow::Result<Option<(std::os::unix::net::UnixListener, (u64, u64))>> {
    use std::os::unix::fs::PermissionsExt as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    if !take && answers(path) {
        return Ok(None);
    }
    let _ = std::fs::remove_file(path);
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let id = file_id(path).ok_or_else(|| anyhow::anyhow!("{} was gone as soon as it was bound", path.display()))?;
    Ok(Some((listener, id)))
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
    listener: Option<std::os::unix::net::UnixListener>,
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
            tracing::info!(path = %path.display(), "{what}: another kiln process serves this machine's socket; this one serves it once that one stops");
        }
        let theirs = bound.is_none();
        let (listener, ours) = bound.map_or((None, None), |(l, id)| (Some(l), Some(id)));
        Ok(Self { path, what, listener, ours, looked: std::time::Instant::now(), theirs })
    }

    pub(crate) fn listener(&self) -> Option<&std::os::unix::net::UnixListener> {
        self.listener.as_ref()
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

/// The socket this process serves, once it does: every harness it starts gets it as
/// `KILN_HARNESS_SOCK`, so `kiln spawn` and `kiln agents` work in an agent's own commands too
/// (they're told apart from a person's by their process).
pub fn served() -> Option<&'static str> {
    SERVED.get().map(String::as_str)
}

/// The same, kiln's operations answered by `forward` (the server's own socket).
#[cfg(unix)]
pub fn serve_with(host: &Arc<Host>, path: PathBuf, forward: Option<Forward>, take: bool) -> anyhow::Result<PathBuf> {
    use std::os::fd::{AsRawFd as _, OwnedFd};

    let mut kept = Kept::new(path.clone(), take, "`kiln spawn`")?;
    let _ = SERVED.set(path.to_string_lossy().into_owned());
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

/// No local socket on Windows yet: `kiln spawn` is not available there.
#[cfg(not(unix))]
pub fn serve_with(_host: &Arc<Host>, _path: PathBuf, _forward: Option<Forward>, _take: bool) -> anyhow::Result<PathBuf> {
    anyhow::bail!("kiln spawn needs Unix sockets")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 2026-10-29 outage left 1,532 sockets of ended test and lab runners in the runtime
    /// folder: a sweep takes those nothing answers on, and leaves a live one, a young one (bound,
    /// not yet listening), and anything not a kiln socket.
    #[cfg(unix)]
    #[test]
    fn sockets_nothing_answers_on_are_swept_and_live_ones_stay() -> anyhow::Result<()> {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir()?;
        let ended = |name: &str| -> anyhow::Result<PathBuf> {
            let p = dir.path().join(name);
            drop(UnixListener::bind(&p)?);
            Ok(p)
        };
        let (dead_reach, dead_harness) = (ended("reach-lab-0-deadbeef.sock")?, ended("harness-lab-0-deadbeef.sock")?);
        let live = dir.path().join("reach-adacachy-4b48ed43.sock");
        let _listening = UnixListener::bind(&live)?;
        let (foreign, notes) = (ended("someone-else.sock")?, dir.path().join("reach-notes.sock"));
        std::fs::write(&notes, "not a socket")?;
        anyhow::ensure!(sweep(dir.path(), std::time::Duration::from_secs(3600)) == 0, "a young socket went");
        anyhow::ensure!(sweep(dir.path(), std::time::Duration::ZERO) == 2, "not just the two ended kiln sockets went");
        anyhow::ensure!(!dead_reach.exists() && !dead_harness.exists(), "an ended runner's socket stayed");
        anyhow::ensure!(live.exists() && foreign.exists() && notes.exists(), "a live socket, another program's or a plain file went");
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
        // Something takes the path and ends: its file stays, and nothing answers there.
        drop(bind(&path, true)?);
        anyhow::ensure!(held(&path, kept.ours) == Held::Dead);
        anyhow::ensure!(kept.look(), "the dead path wasn't served again");
        let _asker = std::os::unix::net::UnixStream::connect(&path)?;
        anyhow::ensure!(kept.listener().is_some_and(|l| drain(l) >= 1), "the socket served again took no connection");
        // While another live process serves it, it's that one's; once that one ends, it's back.
        let other = bind(&path, true)?;
        anyhow::ensure!(!kept.look() && held(&path, kept.ours) == Held::Theirs, "a live process's socket was taken back");
        drop(other);
        anyhow::ensure!(kept.look(), "not served again once the other process ended");
        // A process that starts while another serves the path waits, and serves it once that one ends.
        let (live, _) = bind(&path, true)?.ok_or_else(|| anyhow::anyhow!("not taken over"))?;
        let mut later = Kept::new(path.clone(), false, "test")?;
        anyhow::ensure!(later.listener().is_none(), "a live process's socket was taken at start");
        drop(live);
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
