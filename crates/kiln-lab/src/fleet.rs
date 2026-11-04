//! A round's kiln: a server of the build under test, with a data directory of its own, and its
//! runners, each a `kiln agent` of the same build whose `claude` is the lab's stand-in. Configured
//! as production runs (the new CI engine off, nothing found on the network), with what a lab can't
//! have off: terminals, KilnCD and the runners' headroom (a busy lab machine would pace the
//! sessions it measures).

use std::net::{TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::Digest as _;

use crate::client::Http;

/// What a kiln of the lab doesn't take from the lab's environment: whatever points at another
/// kiln (a runner the lab runs under has these set), as kiln's own tests leave out
/// (`crates/kiln/tests/common/mod.rs`, `NOT_INHERITED`).
const NOT_INHERITED: &[&str] = &[
    "KILN_SUPERVISOR",
    "KILN_SUPERVISED",
    "KILN_GENERATION",
    "KILN_SUPERVISE_CHILD",
    "KILN_TAKE_OVER_FROM",
    "KILN_UPGRADE_ID",
    "KILN_REACH_SOCK",
    "KILN_REACH_FROM",
    "KILN_HARNESS_SOCK",
    "KILN_CONFIG",
    "KILN_URL",
    "KILN_ADMIN_TOKEN",
    "KILN_SERVER",
    "KILN_JOIN_TOKEN",
    "KILN_AGENT_NAME",
    "KILN_STATE_DIR",
    "KILN_INSTANCE",
    "KILN_LOG",
    "KILN_CHAOS",
    "KILN_MACHINE_ID",
    "GIT_CONFIG_GLOBAL",
];

/// A process of the round's, killed with its process group when the round ends.
pub struct Proc {
    pub child: Child,
}

impl Proc {
    fn kill(&mut self) {
        // Its group first (what it started: a harness, a UI server), then it.
        #[cfg(unix)]
        {
            let _ = Command::new("kill").arg("-KILL").arg(format!("-{}", self.child.id())).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

/// `cmd` without what it mustn't inherit, in a process group of its own.
fn clean(mut cmd: Command) -> Command {
    for v in NOT_INHERITED {
        cmd.env_remove(v);
    }
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd
}

/// The key kiln knows a machine started with `KILN_MACHINE_ID=name` by (as kiln's `os::machine_id`
/// makes one, and its tests' `common::machine`).
pub fn machine_key(name: &str) -> String {
    let digest = sha2::Sha256::digest(format!("kiln machine id\n{}", name.trim().to_ascii_lowercase()));
    hex::encode(digest).chars().take(32).collect()
}

fn free_tcp() -> anyhow::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn free_udp() -> anyhow::Result<u16> {
    Ok(UdpSocket::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// A TOML string of `p`.
fn toml_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut out = String::from("\"");
    for c in s.chars() {
        if matches!(c, '\\' | '"') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The round's kiln.
pub struct Fleet {
    pub dir: PathBuf,
    pub port: u16,
    pub admin: String,
    pub server: Proc,
    pub runners: Vec<(String, Proc)>,
}

impl Fleet {
    /// A server of `kiln` under `dir`, answering: `env` goes to it as well.
    pub fn server(kiln: &Path, dir: &Path, env: &[(String, String)]) -> anyhow::Result<Fleet> {
        let mut why = String::new();
        for _ in 0..3 {
            match Self::try_server(kiln, dir, env)? {
                Ok(f) => return Ok(f),
                Err(w) => why = w,
            }
        }
        anyhow::bail!("the server didn't start: {why}")
    }

    fn try_server(kiln: &Path, dir: &Path, env: &[(String, String)]) -> anyhow::Result<Result<Fleet, String>> {
        let (port, quic) = (free_tcp()?, free_udp()?);
        let data = dir.join("data");
        std::fs::create_dir_all(&data)?;
        let home = dir.join("home");
        std::fs::create_dir_all(&home)?;
        let config = format!(
            r#"[server]
http_addr = "127.0.0.1:{port}"
quic_addr = "127.0.0.1:{quic}"
public_url = "http://127.0.0.1:{port}"
quic_advertise = "127.0.0.1:{quic}"
data_dir = {data}

[gitops]
auto_pipelines = false

[local_agent]
enabled = false

[terminal]
enabled = false

[kubernetes]
enabled = false

[discovery]
enabled = false
announce = false
browse = false
join_requests = false
tailscale = false

[cd]
enabled = false

[ci]
enabled = false
"#,
            data = toml_path(&data),
        );
        std::fs::write(dir.join("kiln.toml"), config)?;
        let log = std::fs::File::create(dir.join("server.log"))?;
        let mut cmd = clean(Command::new(kiln));
        cmd.args(["--config", &dir.join("kiln.toml").to_string_lossy(), "server"])
            .env("HOME", &home)
            .env("KILN_MACHINE_ID", format!("lab server {}", dir.display()))
            .env("KILN_WARM_FLOOR", "0")
            .env("NO_COLOR", "1")
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        let mut server = Proc { child: cmd.spawn()? };
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(status) = server.child.try_wait()? {
                return Ok(Err(format!("it exited ({status}) on ports {port} and {quic}")));
            }
            if let Ok(t) = std::fs::read_to_string(data.join("admin.token")) {
                let admin = t.trim().to_owned();
                let mut http = Http::new(port, &admin);
                match http.send("GET", "/api/me", None) {
                    Ok(a) if a.status == 200 => return Ok(Ok(Fleet { dir: dir.to_path_buf(), port, admin, server, runners: Vec::new() })),
                    Ok(a) if a.status == 401 => return Ok(Err(format!("another server answers on {port}"))),
                    _ => {}
                }
            }
            if Instant::now() > deadline {
                return Ok(Err(format!("it didn't answer within 120 s on {port}")));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Runner `name` of `kiln`, its `claude` the stand-in in `bin` (first on its PATH).
    pub fn runner(&mut self, kiln: &Path, name: &str, bin: &Path) -> anyhow::Result<()> {
        let join = std::fs::read_to_string(self.dir.join("data/join.token"))?.trim().to_owned();
        let home = self.dir.join(format!("{name}-home"));
        std::fs::create_dir_all(&home)?;
        let path = std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())))?;
        let log = std::fs::File::create(self.dir.join(format!("{name}.log")))?;
        let mut cmd = clean(Command::new(kiln));
        cmd.args(["agent", "--server", &format!("http://127.0.0.1:{}", self.port), "--token", &join, "--name", name])
            .arg("--work-dir")
            .arg(self.dir.join(format!("{name}-work")))
            .arg("--state-dir")
            .arg(self.dir.join(format!("{name}-state")))
            .env("HOME", &home)
            .env("KILN_MACHINE_ID", name)
            .env("KILN_WARM_FLOOR", "0")
            .env("NO_COLOR", "1")
            .env("PATH", path)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        self.runners.push((name.to_owned(), Proc { child: cmd.spawn()? }));
        Ok(())
    }

    /// Waits until every runner is connected, then turns each one's headroom off and waits for it
    /// to say so (a build without headroom has nothing to turn off).
    pub fn runners_ready(&mut self, within: Duration) -> anyhow::Result<()> {
        let mut http = Http::new(self.port, &self.admin);
        let deadline = Instant::now() + within;
        let keys: Vec<String> = self.runners.iter().map(|(n, _)| machine_key(n)).collect();
        loop {
            if let Some(status) = self.server.child.try_wait()? {
                anyhow::bail!("the server exited ({status}): {}", tail(&self.dir.join("server.log")));
            }
            for (name, p) in &mut self.runners {
                if let Some(status) = p.child.try_wait()? {
                    anyhow::bail!("runner {name} exited ({status}): {}", tail(&self.dir.join(format!("{name}.log"))));
                }
            }
            let a = http.send("GET", "/api/agents", None)?;
            let v: Value = serde_json::from_slice(&a.body).unwrap_or_default();
            let list = v.as_array().cloned().or_else(|| v.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
            let up = |k: &String| list.iter().any(|a| a.get("name").and_then(Value::as_str) == Some(k.as_str()) && a.get("connected").and_then(Value::as_bool) == Some(true));
            if keys.iter().all(up) {
                break;
            }
            anyhow::ensure!(Instant::now() < deadline, "runners never all connected: {}", String::from_utf8_lossy(&a.body));
            std::thread::sleep(Duration::from_millis(200));
        }
        for k in &keys {
            let path = format!("/api/runners/{k}/headroom");
            let a = http.send_settled("PUT", &path, Some(json!({"enabled": false}).to_string().as_bytes()))?;
            if a.status == 404 {
                continue;
            }
            anyhow::ensure!(a.status == 200, "{k}'s headroom off: {} {}", a.status, String::from_utf8_lossy(&a.body));
            loop {
                let a = http.send("GET", &path, None)?;
                let v: Value = serde_json::from_slice(&a.body).unwrap_or_default();
                if v.get("applied").and_then(Value::as_bool) != Some(false) {
                    break;
                }
                anyhow::ensure!(Instant::now() < deadline, "{k} never applied its headroom off");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        Ok(())
    }
}

/// Opens a stand-in session on runner `name` of the server at `port` in `cwd`, its first message
/// `prompt`: its id.
pub fn open(port: u16, admin: &str, name: &str, cwd: &Path, prompt: &str, within: Duration) -> anyhow::Result<String> {
    std::fs::create_dir_all(cwd)?;
    let mut http = Http::new(port, admin);
    let open = json!({"target": format!("agent:{}", machine_key(name)), "spec": {"kind": "claude", "cwd": cwd.display().to_string(), "prompt": prompt}});
    let deadline = Instant::now() + within;
    loop {
        let a = http.send("POST", "/api/harness/sessions", Some(open.to_string().as_bytes()))?;
        let v: Value = serde_json::from_slice(&a.body).unwrap_or_default();
        if a.status == 200
            && let Some(id) = v.pointer("/0/id").and_then(Value::as_str)
        {
            return Ok(id.to_owned());
        }
        anyhow::ensure!(Instant::now() < deadline, "couldn't open a session on {name}: {} {}", a.status, String::from_utf8_lossy(&a.body));
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The end of a log.
pub fn tail(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines.iter().rev().take(30).rev().copied().collect::<Vec<_>>().join("\n")
}
