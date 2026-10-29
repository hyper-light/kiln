//! Agent sessions on a runner of a test server: the runner process, the stand-in Claude Code it
//! runs (`tests/fixtures/fake-claude`: no model), and a viewer's WebSocket to the server.

use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The runner, stopped when the test ends.
pub struct Runner(pub Child);

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// How the stand-in harness talks: `lines` assistant messages a turn, `every_ms` apart.
#[derive(Clone, Copy)]
pub struct Talk {
    pub lines: usize,
    pub every_ms: u64,
}

/// A runner of the server at `port`, named `name`, the stand-in harness first on its PATH and
/// `chaos` its faults (a `kiln agent --chaos` spec; a build with the `chaos` feature): its log.
pub fn runner(dir: &Path, port: u16, name: &str, chaos: Option<&str>, talk: Talk) -> anyhow::Result<(Runner, PathBuf)> {
    let path = format!("{}:{}", dir.join("bin").display(), std::env::var("PATH").unwrap_or_default());
    runner_env(dir, port, name, chaos, talk, &[("PATH", &path)])
}

/// [`runner`], with `env` in its environment (and its harnesses'): `FAKE_CLAUDE_FIRST_LINES`, say,
/// the stand-in harness first on its PATH. Started again under the same name, it's the same runner,
/// its state where it left it.
pub fn runner_with(dir: &Path, port: u16, name: &str, chaos: Option<&str>, talk: Talk, env: &[(&str, &str)]) -> anyhow::Result<(Runner, PathBuf)> {
    let path = format!("{}:{}", dir.join("bin").display(), std::env::var("PATH").unwrap_or_default());
    let mut all = vec![("PATH", path.as_str())];
    all.extend_from_slice(env);
    runner_env(dir, port, name, chaos, talk, &all)
}

/// [`runner`], with `env` in its environment (its `PATH`, its `SHELL`): the stand-in harness is in
/// `<dir>/bin`, on its PATH only if `env` puts it there.
pub fn runner_env(dir: &Path, port: u16, name: &str, chaos: Option<&str>, talk: Talk, env: &[(&str, &str)]) -> anyhow::Result<(Runner, PathBuf)> {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin)?;
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-claude/claude");
    std::fs::copy(&fake, bin.join("claude"))?;
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755))?;
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    let home = dir.join("home");
    std::fs::create_dir_all(&home)?;
    let log_path = dir.join(format!("{name}.log"));
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let mut cmd = super::kiln();
    cmd.args(["agent", "--server", &format!("http://127.0.0.1:{port}"), "--token", &join, "--name", name]);
    if let Some(c) = chaos {
        cmd.args(["--chaos", c]);
    }
    let child = cmd
        .arg("--work-dir")
        .arg(dir.join(format!("{name}-work")))
        .arg("--state-dir")
        .arg(dir.join(format!("{name}-state")))
        .env("HOME", &home)
        // Git's global configuration its home's, as a server's is (`common::server_command`).
        .env_remove("GIT_CONFIG_GLOBAL")
        .env("NO_COLOR", "1")
        .env("FAKE_CLAUDE_LINES", talk.lines.to_string())
        .env("FAKE_CLAUDE_EVERY_MS", talk.every_ms.to_string())
        .envs(env.iter().copied())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?;
    Ok((Runner(child), log_path))
}

/// A folder of this machine's programs (symlinks to /usr/bin's and /bin's) without any harness kiln
/// knows ([`kiln::harness::kinds::Kind::all`]'s programs), as a PATH: a harness is on it only where a
/// test puts one, also on a machine with one installed system-wide (adacachy: pacman's claude-code
/// and codex in /usr/bin), which a runner or server would otherwise find first.
pub fn path_without_harnesses(dir: &Path) -> anyhow::Result<String> {
    let system = dir.join("system-bin");
    std::fs::create_dir_all(&system)?;
    let harnesses: Vec<&str> = kiln::harness::kinds::Kind::all().iter().map(|k| k.program).collect();
    for folder in ["/usr/bin", "/bin"] {
        for entry in std::fs::read_dir(folder)?.flatten() {
            let name = entry.file_name();
            if harnesses.iter().any(|h| name == *h) || std::fs::symlink_metadata(system.join(&name)).is_ok() {
                continue;
            }
            std::os::unix::fs::symlink(entry.path(), system.join(&name))?;
        }
    }
    Ok(system.to_string_lossy().into_owned())
}

/// Waits until the server lists runner `name` connected.
pub fn connected(port: u16, admin: &str, name: &str, log: &Path, within: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + within;
    loop {
        let (_, body) = super::http(port, "GET", "/api/agents", admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let list = v.as_array().cloned().or_else(|| v.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
        if list.iter().any(|a| a.get("name").and_then(Value::as_str) == Some(name) && a.get("connected").and_then(Value::as_bool) == Some(true)) {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "runner {name} never connected: {body}\n{}", std::fs::read_to_string(log).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Waits for the runner's next connection to begin (its log says so again), then a moment more.
pub fn just_connected(log: &Path, deadline: Instant) {
    let count = || std::fs::read_to_string(log).unwrap_or_default().matches("connected to server").count();
    let seen = count();
    while count() == seen && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));
}

/// Opens a stand-in Claude Code session on runner `name`, prompting it: its id. Tries again while
/// the runner is between connections or hasn't said what it offers yet.
pub fn open_session(port: u16, admin: &str, name: &str, cwd: &Path, within: Duration) -> anyhow::Result<String> {
    let open = json!({"target": format!("agent:{name}"), "spec": {"kind": "claude", "cwd": cwd.display().to_string(), "prompt": "stream"}});
    let deadline = Instant::now() + within;
    loop {
        // Waited for longer than the server waits for its runner's answer (`hub::COMMAND_WITHIN`),
        // so it's the server's answer that comes, not a timeout of the test's own.
        let (code, body) = super::http_within(port, "POST", "/api/harness/sessions", admin, Some(&open), Duration::from_secs(75))?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        if code == 200
            && let Some(id) = v.pointer("/0/id").and_then(Value::as_str)
        {
            return Ok(id.to_owned());
        }
        anyhow::ensure!(Instant::now() < deadline, "couldn't open a session: {code} {body}");
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Reads exactly `n` bytes.
pub fn take(s: &mut std::net::TcpStream, n: usize) -> anyhow::Result<Vec<u8>> {
    let mut b = vec![0u8; n];
    s.read_exact(&mut b)?;
    Ok(b)
}

/// One WebSocket frame from the server (unmasked): its opcode and payload.
pub fn frame(s: &mut std::net::TcpStream) -> anyhow::Result<(u8, Vec<u8>)> {
    let h = take(s, 2)?;
    let (b0, b1) = (h.first().copied().unwrap_or(0), h.get(1).copied().unwrap_or(0));
    let len = match b1 & 0x7f {
        126 => take(s, 2)?.iter().fold(0usize, |a, b| (a << 8) | usize::from(*b)),
        127 => take(s, 8)?.iter().fold(0usize, |a, b| (a << 8) | usize::from(*b)),
        n => usize::from(n),
    };
    Ok((b0 & 0x0f, take(s, len)?))
}

/// A WebSocket to `path` on the server at `port`, upgraded.
pub fn socket(port: u16, token: &str, path: &str) -> anyhow::Result<std::net::TcpStream> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(60)))?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )?;
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.extend(take(&mut s, 1)?);
        anyhow::ensure!(head.len() < 16 << 10, "no handshake answer: {}", String::from_utf8_lossy(&head));
    }
    anyhow::ensure!(head.starts_with(b"HTTP/1.1 101"), "not upgraded: {}", String::from_utf8_lossy(&head));
    Ok(s)
}

/// Sends `text` on `s` as a client does, masked (RFC 6455, §5.2).
pub fn send_text(s: &mut std::net::TcpStream, text: &str) -> anyhow::Result<()> {
    let payload = text.as_bytes();
    let mut f = vec![0x81u8];
    match payload.len() {
        n if n < 126 => f.push(0x80 | u8::try_from(n)?),
        n if n < 1 << 16 => {
            f.push(0x80 | 126);
            f.extend(u16::try_from(n)?.to_be_bytes());
        }
        n => {
            f.push(0x80 | 127);
            f.extend(u64::try_from(n)?.to_be_bytes());
        }
    }
    let mask = [0x37u8, 0xfa, 0x21, 0x3d];
    f.extend(mask);
    f.extend(payload.iter().zip(mask.iter().cycle()).map(|(b, m)| b ^ m));
    s.write_all(&f)?;
    Ok(())
}

/// A viewer's text frames from a session socket, up to the `synced` that ends what it's sent on
/// attaching, and the bytes they came to.
pub fn until_synced(s: &mut std::net::TcpStream) -> anyhow::Result<(Vec<Value>, usize)> {
    let mut frames = Vec::new();
    let mut bytes = 0;
    loop {
        let (op, payload) = frame(s)?;
        if op != 1 {
            continue;
        }
        bytes += payload.len();
        let v: Value = serde_json::from_slice(&payload)?;
        let synced = v.get("t").and_then(Value::as_str) == Some("synced");
        frames.push(v);
        if synced {
            return Ok((frames, bytes));
        }
    }
}

/// What a viewer gets on attaching to session `id` (`query` after the path's `?`).
pub fn attach(port: u16, token: &str, id: &str, query: &str) -> anyhow::Result<(Vec<Value>, usize)> {
    let mut s = socket(port, token, &format!("/api/harness/sessions/{id}/ws{query}"))?;
    until_synced(&mut s)
}

/// The timeline's events among a viewer's frames, in order.
pub fn events(frames: &[Value]) -> Vec<Value> {
    frames
        .iter()
        .filter(|f| f.get("t").and_then(Value::as_str) == Some("events"))
        .flat_map(|f| f.get("events").and_then(Value::as_array).cloned().unwrap_or_default())
        .collect()
}

/// The stand-in's line numbers in `events`, in order (`line 00042` is 42).
pub fn lines(events: &[Value]) -> Vec<usize> {
    events
        .iter()
        .filter_map(|e| e.get("text").and_then(Value::as_str))
        .filter_map(|t| t.strip_prefix("line ").and_then(|n| n.trim().parse().ok()))
        .collect()
}
