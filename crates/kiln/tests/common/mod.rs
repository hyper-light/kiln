//! What the integration tests share. Observe's live tests (docs/observe.md §13): a kiln server and
//! runners as processes of their own on this machine, each with a home of its own and a clean
//! environment, on real sockets and disks, and the requests that ask them things ([`Fleet`]). The
//! others': a throwaway kiln server ([`start`]) and the HTTP they speak to it ([`http`]).
#![allow(dead_code, reason = "each test file uses what it needs")]

#[cfg(unix)]
pub mod agents;
#[cfg(unix)]
pub mod lab;

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub const BIN: &str = env!("CARGO_BIN_EXE_kiln");

/// The value at `path` in `v` (null where there is none), as `serde_json`'s indexing gives it.
pub fn at<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
    static NULL: Value = Value::Null;
    path.iter().try_fold(v, |v, k| v.get(*k)).unwrap_or(&NULL)
}

/// What a kiln of the fleet tells the processes it starts, and what a person's shell may hold: its
/// supervisor's socket and generation, the runner a new build takes over from and the upgrade it's
/// part of, this machine's reach, the person's config, server, tokens, runner name, state, log and
/// faults. A test run under a runner inherits them. A kiln the test starts is its own, so none of them goes on to it:
/// a test's runner that took the runner it runs under for one it takes over from would hold every
/// message it's sent, and one that kept its supervisor's socket would ask the real supervisor to
/// start the builds it's told to upgrade to.
pub const NOT_INHERITED: &[&str] = &[
    "KILN_SUPERVISOR",
    "KILN_SUPERVISED",
    "KILN_GENERATION",
    "KILN_SUPERVISE_CHILD",
    "KILN_TAKE_OVER_FROM",
    "KILN_UPGRADE_ID",
    "KILN_REACH_SOCK",
    "KILN_REACH_FROM",
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
    // A gate's own git config (ci/gate.sh sets it): it overrides the HOME a test's kiln is given,
    // and actions/checkout edits a .gitconfig in the HOME it sets for git, which it then misses.
    "GIT_CONFIG_GLOBAL",
];

/// This build's `kiln`, as a test starts it: with none of [`NOT_INHERITED`] (what the test sets
/// itself goes on).
pub fn kiln() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kiln"));
    for var in NOT_INHERITED {
        cmd.env_remove(var);
    }
    cmd
}

/// A port free now.
pub fn free_port() -> anyhow::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// `p` as a TOML string: a Windows path's backslashes escaped (a TOML string's own escapes).
pub fn toml_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '\\' | '"') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The `file:` URL of local path `p`, as git takes one: `file:///srv/repo` on Unix,
/// `file:///C:/Users/repo` on Windows.
pub fn file_url(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') { format!("file://{s}") } else { format!("file:///{s}") }
}

/// A UDP port free on 127.0.0.1, for a QUIC listener: asked of UDP, not TCP. Windows reserves
/// ranges of each apart (Hyper-V's, Docker Desktop's: `netsh int ipv4 show excludedportrange
/// protocol=udp`); a TCP port free there was refused to UDP (`bind`: 10013, an access forbidden),
/// and the fleet's server had no QUIC listener: no runner joined.
pub fn free_udp_port() -> anyhow::Result<u16> {
    Ok(std::net::UdpSocket::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// How a test's containers and this machine reach each other. Under Linux's own engine a container
/// shares this machine's network (`--network host`): it reaches this machine's services, and this
/// machine its, at 127.0.0.1. Under Docker Desktop (macOS, Windows: its containers in a VM of its
/// own, whose network `--network host` is) a container reaches this machine as
/// `host.docker.internal` (a published port of another container too, through this machine),
/// listens on every address of its own, and this machine reaches its ports where they are
/// published, on 127.0.0.1.
#[derive(Debug, Clone, Copy)]
pub struct DockerNet {
    pub desktop: bool,
}

impl DockerNet {
    /// The engine's, as `docker info` names it (once a process).
    pub fn get() -> Self {
        static DESKTOP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let desktop = *DESKTOP.get_or_init(|| {
            Command::new("docker").args(["info", "--format", "{{.OperatingSystem}}"]).output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("Docker Desktop"))
        });
        Self { desktop }
    }

    /// The address a container reaches this machine's services at.
    pub fn host(&self) -> &'static str {
        if self.desktop { "host.docker.internal" } else { "127.0.0.1" }
    }

    /// The address a container's own server listens on, for this machine to reach it.
    pub fn listen(&self) -> &'static str {
        if self.desktop { "0.0.0.0" } else { "127.0.0.1" }
    }

    /// `docker run`'s arguments for its network: this machine's, or the container's ports `tcp` and
    /// `udp` published on 127.0.0.1.
    pub fn args(&self, tcp: &[u16], udp: &[u16]) -> Vec<String> {
        if !self.desktop {
            return vec!["--network".into(), "host".into()];
        }
        let tcp = tcp.iter().map(|p| format!("127.0.0.1:{p}:{p}"));
        let udp = udp.iter().map(|p| format!("127.0.0.1:{p}:{p}/udp"));
        tcp.chain(udp).flat_map(|p| ["-p".to_owned(), p]).collect()
    }
}

/// One process of the fleet; killed (SIGKILL) when dropped.
pub struct Proc {
    pub child: Child,
    pub log: PathBuf,
}

impl Proc {
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Asks it to stop as an operator's stop does: SIGTERM on Unix; on Windows, which has no
    /// SIGTERM, its stdin closed, as kiln's supervisor asks there.
    pub fn ask_to_stop(&mut self) {
        #[cfg(unix)]
        {
            // kiln itself: the process, or under samply (`KOBS_PROFILE`) its child. Not a child of
            // kiln's own (a `journalctl` it reads, a `git` it runs) when kiln is the process.
            let pid = self.child.id();
            let profiled = std::env::var("KOBS_PROFILE").is_ok_and(|d| !d.is_empty());
            let kid = profiled.then(|| std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).ok()).flatten().and_then(|k| k.split_whitespace().next().map(str::to_owned));
            let _ = Command::new("kill").args(["-TERM", &kid.unwrap_or_else(|| pid.to_string())]).status();
        }
        #[cfg(windows)]
        drop(self.child.stdin.take());
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

pub fn tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all.get(all.len().saturating_sub(lines)..).unwrap_or(&[]).join("\n")
}

pub struct Fleet {
    pub root: tempfile::TempDir,
    pub http: u16,
    pub admin: String,
    /// The server's environment besides the fleet's clean one.
    pub server_env: Vec<(String, String)>,
    pub server: Option<Proc>,
    pub runners: BTreeMap<String, Option<Proc>>,
    /// Each runner's OTLP receiver's port.
    pub otlp: BTreeMap<String, u16>,
}

/// What a test sets in the server's own tables; the fleet's choices otherwise.
pub struct Tables<'a> {
    /// `[cd]`'s keys: off, unless the test deploys.
    pub cd: &'a str,
    /// How often the server looks at its host and scrapes its own registry (`[observe]
    /// host_seconds`): hourly, unless the test reads what the registry says as it changes.
    pub host_seconds: u32,
}

impl Default for Tables<'_> {
    fn default() -> Self {
        Self { cd: "enabled = false", host_seconds: 3600 }
    }
}

impl Fleet {
    pub fn new() -> anyhow::Result<Self> {
        Self::with(|_| String::new())
    }

    /// A fleet whose server's configuration has `extra(root)` too.
    pub fn with(extra: impl Fn(&Path) -> String) -> anyhow::Result<Self> {
        Self::with_env(extra, &[])
    }

    /// A fleet whose server's configuration has `extra(root)` too, and whose server has `env`.
    pub fn with_env(extra: impl Fn(&Path) -> String, env: &[(&str, &str)]) -> anyhow::Result<Self> {
        Self::with_tables(&Tables::default(), extra, env)
    }

    /// A fleet whose server's own tables say what `tables` does, its configuration has
    /// `extra(root)` too, and the server has `env`.
    pub fn with_tables(tables: &Tables<'_>, extra: impl Fn(&Path) -> String, env: &[(&str, &str)]) -> anyhow::Result<Self> {
        let root = tempfile::tempdir()?;
        let (http, quic) = (free_port()?, free_udp_port()?);
        let data = root.path().join("server");
        let cfg = format!(
            r#"
[server]
http_addr = "127.0.0.1:{http}"
quic_addr = "127.0.0.1:{quic}"
public_url = "http://127.0.0.1:{http}"
quic_advertise = "127.0.0.1:{quic}"
data_dir = {data}

[gitops]
auto_pipelines = false

[local_agent]
enabled = false

[discovery]
enabled = false

[kubernetes]
enabled = false

[cd]
{cd}

[telemetry]
# Runners' host samples, and every node's own figures, once an hour: the test's writes are the
# writes (and the leader's cutoff, each minute).
sample_seconds = 3600

[observe]
enabled = true
# Each node looks at its host once, as it starts, and then not for an hour: the test's writes are the
# writes.
host_seconds = {host_seconds}

{extra}
"#,
            data = toml_path(&data),
            cd = tables.cd,
            host_seconds = tables.host_seconds,
            extra = extra(root.path())
        );
        std::fs::write(root.path().join("kiln.toml"), cfg)?;
        let server_env = env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        let mut f = Self { root, http, admin: String::new(), server_env, server: None, runners: BTreeMap::new(), otlp: BTreeMap::new() };
        f.start_server()?;
        f.admin = std::fs::read_to_string(data.join("admin.token"))?.trim().to_owned();
        Ok(f)
    }

    /// The command that starts a node of the fleet (`label`: `server`, or a runner's name); under
    /// samply, its profile saved as `<label>.json.gz` in `KOBS_PROFILE`, when that names a folder.
    pub fn command(&self, home: &Path, label: &str) -> anyhow::Result<Command> {
        std::fs::create_dir_all(home)?;
        let mut c = match std::env::var("KOBS_PROFILE") {
            Ok(dir) if !dir.is_empty() => {
                let mut c = Command::new(std::env::var("SAMPLY").unwrap_or_else(|_| "samply".into()));
                c.args(["record", "--save-only", "--unstable-presymbolicate", "-o"]).arg(Path::new(&dir).join(format!("{label}.json.gz"))).arg("--").arg(BIN);
                c
            }
            _ => Command::new(BIN),
        };
        // A clean environment and a home of its own: nothing of this machine's leaks in.
        c.env_clear().env("HOME", home).env("LANG", "C.UTF-8").env("RUST_LOG", "info,kiln::observe=debug").env("KILN_LOG", "info,kiln::observe=debug");
        #[cfg(not(windows))]
        c.env("PATH", "/usr/local/bin:/usr/bin:/bin");
        // Windows: the profile is the home kiln reads first, a process needs its system's folders,
        // and kiln is asked to stop by its stdin closing ([`Proc::ask_to_stop`]).
        #[cfg(windows)]
        {
            c.env("USERPROFILE", home).env("APPDATA", home.join("AppData").join("Roaming")).env("LOCALAPPDATA", home.join("AppData").join("Local"));
            for k in ["SystemRoot", "SystemDrive", "windir", "PATH", "PATHEXT", "TEMP", "TMP", "ComSpec"] {
                if let Some(v) = std::env::var_os(k) {
                    c.env(k, v);
                }
            }
            c.env("KILN_SUPERVISED", "1").stdin(Stdio::piped());
        }
        // The C library's allocator tuned from outside, for a test that measures it.
        if let Ok(t) = std::env::var("GLIBC_TUNABLES") {
            c.env("GLIBC_TUNABLES", t);
        }
        Ok(c)
    }

    pub fn start_server(&mut self) -> anyhow::Result<()> {
        let log = self.root.path().join("server.log");
        let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
        let child = self
            .command(&self.root.path().join("home-server"), "server")?
            .arg("--config")
            .arg(self.root.path().join("kiln.toml"))
            .arg("server")
            .envs(self.server_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdout(Stdio::from(out.try_clone()?))
            .stderr(Stdio::from(out))
            .spawn()?;
        self.server = Some(Proc { child, log });
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(60) {
            if self.get_raw("/healthz").is_ok_and(|(code, _)| code == 200) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        anyhow::bail!("the server did not come up:\n{}", self.logs())
    }

    pub fn start_runner(&mut self, name: &str) -> anyhow::Result<()> {
        self.start_runner_with(name, &[])
    }

    /// A runner with `env` besides the fleet's (after it: the same name wins).
    pub fn start_runner_with(&mut self, name: &str, env: &[(&str, &str)]) -> anyhow::Result<()> {
        let join = std::fs::read_to_string(self.root.path().join("server").join("join.token"))?.trim().to_owned();
        let dir = self.root.path().join(name);
        let log = self.root.path().join(format!("{name}.log"));
        let otlp = match self.otlp.get(name) {
            Some(&p) => p,
            None => free_port()?,
        };
        self.otlp.insert(name.to_owned(), otlp);
        let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
        let child = self
            .command(&dir.join("home"), name)?
            .args(["agent", "--server", &format!("http://127.0.0.1:{}", self.http), "--token", &join, "--name", name, "--slots", "1", "--labels", "baremetal", "--no-terminal", "--no-reach"])
            .env("KILN_OTLP_ADDR", format!("127.0.0.1:{otlp}"))
            // OTLP/gRPC's port is a machine's own: a test that wants it gives one of its own.
            .env("KILN_OTLP_GRPC_ADDR", "off")
            .env("KILN_OBSERVE_HOST_SECONDS", "3600")
            .arg("--state-dir")
            .arg(dir.join("state"))
            .arg("--work-dir")
            .arg(dir.join("work"))
            .envs(env.iter().copied())
            .stdout(Stdio::from(out.try_clone()?))
            .stderr(Stdio::from(out))
            .spawn()?;
        self.runners.insert(name.to_owned(), Some(Proc { child, log }));
        Ok(())
    }

    /// Stops runner `name` as an operator's stop does ([`Proc::ask_to_stop`]), waiting for it to be
    /// gone.
    pub fn stop_runner(&mut self, name: &str) {
        if let Some(Some(mut p)) = self.runners.insert(name.to_owned(), None) {
            p.ask_to_stop();
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(60) {
                if matches!(p.child.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            p.kill();
        }
    }

    pub fn kill_runner(&mut self, name: &str) {
        if let Some(Some(mut p)) = self.runners.insert(name.to_owned(), None) {
            p.kill();
        }
    }

    /// Stops every node gracefully (SIGTERM to kiln, so a profiler it runs under saves its
    /// profile), waiting for each to be gone.
    pub fn stop_gently(&mut self) {
        let mut procs: Vec<Proc> = self.runners.values_mut().filter_map(Option::take).collect();
        procs.extend(self.server.take());
        for p in &mut procs {
            p.ask_to_stop();
        }
        for mut p in procs {
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(120) {
                if matches!(p.child.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            p.kill();
        }
    }

    pub fn kill_server(&mut self) {
        if let Some(mut p) = self.server.take() {
            p.kill();
        }
    }

    pub fn logs(&self) -> String {
        let mut out = String::new();
        if let Some(s) = &self.server {
            out.push_str(&format!("--- server\n{}\n", tail(&s.log, 40)));
        }
        for (name, p) in &self.runners {
            let log = p.as_ref().map_or_else(|| self.root.path().join(format!("{name}.log")), |p| p.log.clone());
            out.push_str(&format!("--- {name}\n{}\n", tail(&log, 40)));
        }
        out
    }

    /// A request to the server: its status and its body.
    pub fn request(&self, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<(u16, Vec<u8>)> {
        let body = body.map(serde_json::to_vec).transpose()?.unwrap_or_default();
        let auth = if self.admin.is_empty() { String::new() } else { format!("Authorization: Bearer {}\r\n", self.admin) };
        raw(self.http, method, path, &format!("{auth}Content-Type: application/json\r\n"), &body)
    }
}

/// A request to `127.0.0.1:port` with `headers` (each ending in CRLF) and `body`: its status and
/// its body, chunked or not.
pub fn raw(port: u16, method: &str, path: &str, headers: &str, body: &[u8]) -> anyhow::Result<(u16, Vec<u8>)> {
    {
        let mut s = TcpStream::connect(("127.0.0.1", port))?;
        s.set_read_timeout(Some(Duration::from_secs(30)))?;
        write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{headers}Content-Length: {}\r\n\r\n", body.len())?;
        s.write_all(body)?;
        let mut raw = Vec::new();
        s.read_to_end(&mut raw)?;
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow::anyhow!("no header end"))?;
        let head = String::from_utf8_lossy(raw.get(..split).unwrap_or(&[])).into_owned();
        let code: u16 = head.split_whitespace().nth(1).ok_or_else(|| anyhow::anyhow!("no status"))?.parse()?;
        let mut rest = raw.get(split + 4..).unwrap_or(&[]).to_vec();
        if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
            let mut out = Vec::new();
            let mut at = rest.as_slice();
            loop {
                let line_end = at.windows(2).position(|w| w == b"\r\n").ok_or_else(|| anyhow::anyhow!("chunk size"))?;
                let size = usize::from_str_radix(String::from_utf8_lossy(at.get(..line_end).unwrap_or(&[])).trim(), 16)?;
                at = at.get(line_end + 2..).unwrap_or(&[]);
                if size == 0 {
                    break;
                }
                out.extend_from_slice(at.get(..size).ok_or_else(|| anyhow::anyhow!("chunk"))?);
                at = at.get(size + 2..).unwrap_or(&[]);
            }
            rest = out;
        }
        Ok((code, rest))
    }
}

impl Fleet {
    pub fn get_raw(&self, path: &str) -> anyhow::Result<(u16, Vec<u8>)> {
        self.request("GET", path, None)
    }

    pub fn call(&self, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<Value> {
        let (code, b) = self.request(method, path, body)?;
        anyhow::ensure!((200..300).contains(&code), "{method} {path}: {code} {}", String::from_utf8_lossy(&b));
        Ok(serde_json::from_slice(&b)?)
    }

    pub fn status(&self) -> anyhow::Result<Value> {
        self.call("GET", "/api/observe/v1/status", None)
    }

    /// Waits until `done` holds of the group's status, or fails with the fleet's logs.
    pub fn until(&self, within: Duration, what: &str, done: impl Fn(&Value) -> bool) -> anyhow::Result<Value> {
        let start = Instant::now();
        let mut last = Value::Null;
        while start.elapsed() < within {
            if let Ok(s) = self.status() {
                if done(&s) {
                    return Ok(s);
                }
                last = s;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        anyhow::bail!("{what}: not within {within:?}; the status was {last:#}\n{}", self.logs())
    }

    /// Every replica's full digest at one index, equal: asked again until one index holds them all
    /// (the leader's cutoff entry may come between two of them).
    pub fn verify(&self, replicas: usize) -> anyhow::Result<(u64, u64)> {
        for _ in 0..30 {
            self.call("POST", "/api/observe/v1/verify", Some(&json!({})))?;
            std::thread::sleep(Duration::from_millis(1500));
            let s = self.status()?;
            let mut seen: Vec<(u64, u64)> = Vec::new();
            let pair = |v: &Value| match v.as_array().map(Vec::as_slice) {
                Some([at, d]) => Some((at.as_u64().unwrap_or(0), d.as_u64().unwrap_or(0))),
                _ => None,
            };
            seen.extend(pair(at(&s, &["verified"])));
            for m in at(&s, &["machines"]).as_array().into_iter().flatten() {
                seen.extend(pair(at(m, &["status", "verified"])));
            }
            let first = seen.first().copied().unwrap_or_default();
            if seen.len() == replicas && seen.iter().all(|x| x.0 == first.0) {
                anyhow::ensure!(seen.iter().all(|x| x.1 == first.1), "replicas at index {} hold different contents: {seen:?}\n{}", first.0, self.logs());
                return Ok(first);
            }
        }
        anyhow::bail!("the replicas' digests never met at one index\n{}", self.logs())
    }

    /// Points of series `metric{host:h}` for each host, every 10 s over `[t0, t0 + n·10 s)`.
    pub fn write(&self, metric: &str, hosts: &[&str], t0: i64, n: i64, value: impl Fn(&str, i64) -> f64) -> anyhow::Result<()> {
        let series: Vec<Value> = hosts.iter().map(|h| json!({ "metric": metric, "type": "gauge", "tags": [format!("host:{h}")], "points": (0..n).map(|i| json!([(t0 + i * 10_000) as f64 / 1000.0, value(h, i)])).collect::<Vec<_>>() })).collect();
        self.call("POST", "/api/observe/v1/series", Some(&json!({ "series": series })))?;
        Ok(())
    }

    /// The query's values, by host, on a 10 s grid.
    pub fn query(&self, q: &str, from: i64, to: i64) -> anyhow::Result<BTreeMap<String, Vec<Option<f64>>>> {
        let a = self.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": from, "to": to, "step": 10_000 })))?;
        let mut out = BTreeMap::new();
        for s in at(&a, &["series"]).as_array().into_iter().flatten() {
            let host = at(s, &["tags", "host"]).as_str().unwrap_or("").to_owned();
            out.insert(host, at(s, &["values"]).as_array().into_iter().flatten().map(Value::as_f64).collect());
        }
        Ok(out)
    }

    pub fn members(&self, s: &Value) -> BTreeMap<String, u64> {
        at(s, &["machines"]).as_array().into_iter().flatten().map(|m| (at(m, &["machine"]).as_str().unwrap_or("").to_owned(), at(m, &["member"]).as_u64().unwrap_or(0))).collect()
    }
}

/// What a mock service received: each request's path, headers (lowercased names) and body.
#[derive(Debug, Clone)]
pub struct Got {
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Got {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// A service for monitors to notify, on a port of its own: it takes every POST, answers 200 (or,
/// for the first `refuse` requests to a path, 429 with `Retry-After: 1`), and keeps what came.
pub struct Mock {
    pub port: u16,
    pub got: std::sync::Arc<std::sync::Mutex<Vec<Got>>>,
}

impl Mock {
    pub fn start(refuse: &[(&str, usize)]) -> anyhow::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let got: std::sync::Arc<std::sync::Mutex<Vec<Got>>> = std::sync::Arc::default();
        let refused: std::sync::Arc<std::sync::Mutex<BTreeMap<String, usize>>> = std::sync::Arc::new(std::sync::Mutex::new(refuse.iter().map(|(p, n)| ((*p).to_owned(), *n)).collect()));
        let g = got.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let (g, refused) = (g.clone(), refused.clone());
                std::thread::spawn(move || {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    // Headers, then the body by its length.
                    let head_end = loop {
                        let Ok(n) = s.read(&mut chunk) else { return };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i;
                        }
                    };
                    let head = String::from_utf8_lossy(buf.get(..head_end).unwrap_or(&[])).into_owned();
                    let mut lines = head.lines();
                    let path = lines.next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("").to_owned();
                    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
                    let len: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
                    let mut body = buf.get(head_end + 4..).unwrap_or(&[]).to_vec();
                    while body.len() < len {
                        let Ok(n) = s.read(&mut chunk) else { break };
                        if n == 0 {
                            break;
                        }
                        body.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
                    }
                    let refuse = {
                        let mut r = refused.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match r.get_mut(&path) {
                            Some(n) if *n > 0 => {
                                *n -= 1;
                                true
                            }
                            _ => false,
                        }
                    };
                    g.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(Got { path, headers, body });
                    let answer = if refuse {
                        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    } else {
                        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                    };
                    let _ = s.write_all(answer.as_bytes());
                });
            }
        });
        Ok(Self { port, got })
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    pub fn at(&self, path: &str) -> Vec<Got> {
        self.got.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().filter(|g| g.path == path).cloned().collect()
    }
}

/// What a [`Service`] answers a request: its method and what came (its path and query, headers,
/// body) to a status and a JSON body.
pub type Answer = dyn Fn(&str, &Got) -> (u16, String) + Send + Sync;

/// A service on a port of its own that answers as the one it stands in for does (`answer`, which
/// keeps what state it needs), keeping each request in order with its method.
pub struct Service {
    pub port: u16,
    pub got: std::sync::Arc<std::sync::Mutex<Vec<(String, Got)>>>,
}

impl Service {
    pub fn start(answer: std::sync::Arc<Answer>) -> anyhow::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let got: std::sync::Arc<std::sync::Mutex<Vec<(String, Got)>>> = std::sync::Arc::default();
        let g = got.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let (g, answer) = (g.clone(), answer.clone());
                std::thread::spawn(move || {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
                    let (mut buf, mut chunk) = (Vec::new(), [0u8; 8192]);
                    let head_end = loop {
                        let Ok(n) = s.read(&mut chunk) else { return };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i;
                        }
                    };
                    let head = String::from_utf8_lossy(buf.get(..head_end).unwrap_or(&[])).into_owned();
                    let mut lines = head.lines();
                    let mut first = lines.next().unwrap_or("").split_whitespace();
                    let (method, path) = (first.next().unwrap_or("").to_owned(), first.next().unwrap_or("").to_owned());
                    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
                    let len: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
                    let mut body = buf.get(head_end + 4..).unwrap_or(&[]).to_vec();
                    while body.len() < len {
                        let Ok(n) = s.read(&mut chunk) else { break };
                        if n == 0 {
                            break;
                        }
                        body.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
                    }
                    let came = Got { path, headers, body };
                    let (status, out) = answer(&method, &came);
                    g.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((method, came));
                    let _ = write!(s, "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}", out.len());
                });
            }
        });
        Ok(Self { port, got })
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The requests that came, each with its method.
    pub fn requests(&self) -> Vec<(String, Got)> {
        self.got.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

/// How long an image's build may take before it is given up on and made again.
pub const BUILD_WITHIN: Duration = Duration::from_secs(600);

/// `docker` with these arguments (and `stdin`, when given), its output. A build is given up on
/// past [`BUILD_WITHIN`], the CLI and what it started (buildx) killed, and made once more: a
/// build's session can wedge in Docker Desktop (one waited 31 minutes on a Mac Studio for its base
/// image's manifest; the next build, with the first killed, went through at once).
pub fn docker(args: &[&str], stdin: Option<&str>) -> anyhow::Result<String> {
    let build = args.first() == Some(&"build");
    match docker_within(args, stdin, build.then_some(BUILD_WITHIN)) {
        Err(e) if build && e.to_string().contains("given up on") => {
            eprintln!("  {e}: building again");
            docker_within(args, stdin, Some(BUILD_WITHIN))
        }
        other => other,
    }
}

/// `docker` with these arguments, killed past `within` when given (its process group on Unix, its
/// process tree on Windows, so a CLI's plugin goes with it).
fn docker_within(args: &[&str], stdin: Option<&str>, within: Option<Duration>) -> anyhow::Result<String> {
    let mut c = Command::new("docker");
    c.args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut c, 0);
    let mut child = c.spawn()?;
    if let (Some(text), Some(mut i)) = (stdin, child.stdin.take()) {
        i.write_all(text.as_bytes())?;
    }
    let drain = |p: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut s = Vec::new();
            if let Some(mut p) = p {
                let _ = p.read_to_end(&mut s);
            }
            s
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>));
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if let Some(w) = within.filter(|w| start.elapsed() > *w) {
            let pid = child.id().to_string();
            #[cfg(unix)]
            let _ = Command::new("kill").args(["-KILL", "--", &format!("-{pid}")]).status();
            #[cfg(windows)]
            let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &pid]).output();
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("docker {args:?} given up on after {w:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let (stdout, stderr) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
    anyhow::ensure!(status.success(), "docker {args:?}: {}\n{}", String::from_utf8_lossy(&stderr), String::from_utf8_lossy(&stdout));
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// `%XX` as its byte: a query's value as it was before it was encoded.
pub fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while let Some(&c) = b.get(i) {
        match b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok()) {
            Some(v) if c == b'%' => {
                out.push(v);
                i += 3;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The value of `name` in a path's query, decoded.
pub fn param(path: &str, name: &str) -> String {
    path.split_once('?').and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix(name).and_then(|v| v.strip_prefix('=')))).map(decode).unwrap_or_default()
}

/// Whether runner `name` is a learner, up, and its last report's digest is the one the leader had
/// at its index (`seen`: the leader's digest at each index a status of the leader's said).
fn agrees(s: &Value, name: &str, seen: &BTreeMap<u64, Value>) -> bool {
    let learners: Vec<u64> = at(s, &["learners"]).as_array().into_iter().flatten().filter_map(Value::as_u64).collect();
    at(s, &["machines"]).as_array().into_iter().flatten().any(|m| {
        at(m, &["machine"]) == name
            && at(m, &["up"]).as_bool() == Some(true)
            && learners.contains(&at(m, &["member"]).as_u64().unwrap_or(0))
            && at(m, &["status", "applied"]).as_u64().and_then(|a| seen.get(&a)).is_some_and(|d| d == at(m, &["status", "digest"]))
    })
}

impl Fleet {
    /// Waits for every runner named to be a learner, up, and agreeing with the leader: its last
    /// report's digest the one the leader had at its index, an index the leader was seen at while
    /// waiting (or all the leader has now, as [`caught_up`]). Nodes write what they measure every
    /// few seconds and a runner reports every ten, so a report is some entries behind the leader's
    /// index now: held to the leader then, not now (on adacachy a runner stayed 5 entries behind
    /// for 90 s, caught up all along). Indexes seen while waiting come after what was written
    /// before it, so a runner agreeing at one holds that too.
    pub fn until_caught_up(&self, within: Duration, what: &str, names: &[&str]) -> anyhow::Result<Value> {
        let seen = std::cell::RefCell::new(BTreeMap::<u64, Value>::new());
        self.until(within, what, |s| {
            if let Some(i) = at(s, &["digest_at"]).as_u64() {
                seen.borrow_mut().insert(i, at(s, &["digest"]).clone());
            }
            caught_up(s, names) || names.iter().all(|n| agrees(s, n, &seen.borrow()))
        })
    }
}

/// Whether every runner named is a learner, up, and has applied all the leader has.
pub fn caught_up(s: &Value, names: &[&str]) -> bool {
    let applied = at(s, &["applied"]).as_u64().unwrap_or(0);
    let learners: Vec<u64> = at(s, &["learners"]).as_array().into_iter().flatten().filter_map(Value::as_u64).collect();
    names.iter().all(|n| {
        at(s, &["machines"]).as_array().into_iter().flatten().any(|m| at(m, &["machine"]) == *n && at(m, &["up"]).as_bool() == Some(true) && learners.contains(&at(m, &["member"]).as_u64().unwrap_or(0)) && at(m, &["status", "applied"]).as_u64().unwrap_or(0) >= applied && at(m, &["status", "digest"]) == at(s, &["digest"]))
    })
}

/// Unix milliseconds now.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

// A throwaway server, for the tests that are not observe's.

/// One HTTP/1.1 request: the status and the body.
pub fn http(port: u16, method: &str, path: &str, token: &str, body: Option<&Value>) -> anyhow::Result<(u16, String)> {
    http_within(port, method, path, token, body, Duration::from_secs(30))
}

/// [`http`], its answer waited for up to `within`.
pub fn http_within(port: u16, method: &str, path: &str, token: &str, body: Option<&Value>, within: Duration) -> anyhow::Result<(u16, String)> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(within))?;
    let payload = body.map(Value::to_string).unwrap_or_default();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status: u16 = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { dechunk(rest) } else { rest.to_owned() };
    Ok((status, body))
}

pub fn dechunk(mut s: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = s.split_once("\r\n") {
        let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if n == 0 {
            break;
        }
        out.push_str(rest.get(..n).unwrap_or(rest));
        s = rest.get(n + 2..).unwrap_or("");
    }
    out
}

pub struct Server {
    child: Child,
    pub port: u16,
    pub admin: String,
    dir: std::path::PathBuf,
    /// Its environment beyond the test's, kept for a restart.
    #[allow(dead_code)]
    env: Vec<(String, String)>,
    /// What its runner left in Docker stays when it's dropped ([`Server::crash`]).
    leave: bool,
}

impl Server {
    /// Stopped (killed, as a crash or a host's restart would), then started again on its ports and
    /// its data: its log goes on in `server.log`.
    #[allow(dead_code)]
    pub fn restart(&mut self) -> anyhow::Result<()> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let log = std::fs::OpenOptions::new().append(true).open(self.dir.join("server.log"))?;
            let mut cmd = server_command(&self.dir, log)?;
            cmd.envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            self.child = cmd.spawn()?;
            // Up once it answers; its ports may take a moment to be free again.
            while self.child.try_wait()?.is_none() {
                if let Ok((200, _)) = http(self.port, "GET", "/healthz", "", None) {
                    return Ok(());
                }
                anyhow::ensure!(Instant::now() < deadline, "the server didn't start again");
                std::thread::sleep(Duration::from_millis(100));
            }
            anyhow::ensure!(Instant::now() < deadline, "the server didn't start again:\n{}", self.log());
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Killed, as a crash would be: what its runner left in Docker stays, for its next start on the
    /// same directory to find (dropped, a server's leftovers go with it).
    #[allow(dead_code)]
    pub fn crash(mut self) {
        self.leave = true;
    }

    /// The end of the server's log.
    #[allow(dead_code)]
    pub fn log(&self) -> String {
        let text = std::fs::read_to_string(self.dir.join("server.log")).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        lines.iter().rev().take(60).rev().copied().collect::<Vec<_>>().join("\n")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Its process group with it: what it started (its UI server) is never left running.
        #[cfg(unix)]
        if let Ok(pid) = i32::try_from(self.child.id()) {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let work = self.dir.join("work");
        if !self.leave && work.is_dir() {
            sweep(&work);
        }
    }
}

/// What a killed server's runner left in Docker (its legs' and its pools' containers, then their
/// networks) removed, as the runner would itself on its next start: found by the label it puts on
/// all it makes (`runner::docker::owner_label` of its work directory), so no other server's. Left
/// behind, each network holds one of the engine's few address pools until no leg on the machine
/// can start.
fn sweep(work: &Path) {
    use sha2::Digest as _;
    let path = std::path::absolute(work).unwrap_or_else(|_| work.to_path_buf());
    let h = hex::encode(sha2::Sha256::digest(path.to_string_lossy().as_bytes()));
    let label = format!("label=kiln.runner-work={}", h.get(..16).unwrap_or(&h));
    let ids = |list: &[&str]| -> Vec<String> {
        Command::new("docker").args(list).args(["--filter", &label]).output().map(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().map(str::to_owned).collect()).unwrap_or_default()
    };
    let containers = ids(&["ps", "-aq"]);
    if !containers.is_empty() {
        let _ = Command::new("docker").args(["rm", "--force"]).args(&containers).output();
    }
    let networks = ids(&["network", "ls", "-q"]);
    if !networks.is_empty() {
        let _ = Command::new("docker").args(["network", "rm"]).args(&networks).output();
    }
}

/// What a test server has besides the defaults.
#[derive(Default)]
pub struct Options<'a> {
    /// Its own runner (labels `local`), working under `<dir>/work`.
    pub agent: bool,
    /// More of `[ci]`.
    pub ci: &'a str,
    /// More sections: `[[repos]]`, ….
    pub extra: &'a str,
}

/// A server of its own under `dir`, up and answering.
pub fn start(dir: &Path, o: &Options<'_>) -> anyhow::Result<Server> {
    start_with(dir, o, false)
}

/// `start`, serving HTTPS too (on the same port) when `https`: its address is `https://`, the
/// kiln CA's certificate.
pub fn start_with(dir: &Path, o: &Options<'_>, https: bool) -> anyhow::Result<Server> {
    start_full(dir, o, https, false, &[])
}

/// [`start`], with KilnCD and deploying by API running (no cluster of its own).
pub fn start_cd(dir: &Path, o: &Options<'_>) -> anyhow::Result<Server> {
    let s = start_full(dir, o, false, true, &[])?;
    // Deploying by API starts beside the server, not with it (its records copied first): until it
    // has, its calls are answered 503.
    let started = Instant::now();
    loop {
        let (code, body) = http(s.port, "GET", "/api/deploy/environments", &s.admin, None)?;
        if code != 503 {
            return Ok(s);
        }
        anyhow::ensure!(started.elapsed() < Duration::from_secs(60), "deploying by API never started: {body}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `start_with`, the server's environment given `env` too.
pub fn start_env(dir: &Path, o: &Options<'_>, https: bool, env: &[(&str, &str)]) -> anyhow::Result<Server> {
    start_full(dir, o, https, false, env)
}

/// A port picked here can be taken before the server binds it (tests run side by side): then the
/// server exits, or another test's server answers on it and refuses this one's token. Either way it
/// starts again, on new ports.
fn start_full(dir: &Path, o: &Options<'_>, https: bool, cd: bool, env: &[(&str, &str)]) -> anyhow::Result<Server> {
    let mut why = String::new();
    for _ in 0..3 {
        match start_once(dir, o, https, cd, env)? {
            Ok(server) => return Ok(server),
            Err(w) => why = w,
        }
    }
    anyhow::bail!("the server didn't start: {why}")
}

/// `kiln server` with `dir`'s configuration, its home in `dir`, writing to `log`, in a process group
/// of its own. Git's global configuration is its home's, as an installed server's is: a gate's
/// `GIT_CONFIG_GLOBAL` (ci/gate.sh's own empty file) would reach its jobs' steps, where
/// `actions/checkout` writes credentials to the global configuration and then edits
/// `$HOME/.gitconfig` under a home of its own ("Unable to replace auth placeholder", W14-AC05).
fn server_command(dir: &Path, log: std::fs::File) -> anyhow::Result<Command> {
    let home = dir.join("home");
    std::fs::create_dir_all(&home)?;
    let mut cmd = kiln();
    cmd.args(["--config", &dir.join("kiln.toml").to_string_lossy(), "server"]).env("HOME", &home).env_remove("GIT_CONFIG_GLOBAL").env("NO_COLOR", "1").stdout(log.try_clone()?).stderr(log);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    Ok(cmd)
}

fn start_once(dir: &Path, o: &Options<'_>, https: bool, cd: bool, env: &[(&str, &str)]) -> anyhow::Result<Result<Server, String>> {
    let (port, quic) = (free_port()?, free_udp_port()?);
    let data = dir.join("data");
    let work = dir.join("work");
    // Runners' live samples place nothing (W05-R03's headroom): a test's runners share the machine
    // running the tests, whose load isn't theirs to wait for. A test that says otherwise has
    // `headroom` in a `[ci.scheduler]` of its own.
    let extra = if o.extra.contains("headroom") {
        o.extra.to_owned()
    } else if o.extra.contains("[ci.scheduler]") {
        o.extra.replacen("[ci.scheduler]", "[ci.scheduler]\nheadroom = false", 1)
    } else {
        format!("{}\n[ci.scheduler]\nheadroom = false\n", o.extra)
    };
    let config = format!(
        r#"[server]
http_addr = "127.0.0.1:{port}"
quic_addr = "127.0.0.1:{quic}"
public_url = "{scheme}://127.0.0.1:{port}"
quic_advertise = "127.0.0.1:{quic}"
data_dir = {data}
https = {https}

[gitops]
auto_pipelines = false

[local_agent]
enabled = {agent}
labels = ["local"]
slots = 2
work_dir = {work}

{kubernetes}
{terminal}
[discovery]
enabled = false
announce = false
browse = false
join_requests = false
tailscale = false

{cd}
[ci]
enabled = true
{ci}

{extra}
"#,
        data = toml_path(&data),
        work = toml_path(&work),
        scheme = if https { "https" } else { "http" },
        agent = o.agent,
        ci = o.ci,
        extra = extra,
        // CD stays off unless the test asks for it (`start_cd`) or has a `[cd]` of its own.
        cd = if o.extra.contains("[cd]") {
            ""
        } else if cd {
            "[cd]\nenabled = true\n"
        } else {
            "[cd]\nenabled = false\n"
        },
        // So do terminals (its own runner's, and into legs: docs/ci.md W17-R01).
        terminal = if o.extra.contains("[terminal]") { "" } else { "[terminal]\nenabled = false\n" },
        // And pods (a `k8s` pool's: docs/ci.md W13-R07).
        kubernetes = if o.extra.contains("[kubernetes]") { "" } else { "[kubernetes]\nenabled = false\n" },
    );
    std::fs::write(dir.join("kiln.toml"), config)?;
    // Its log, for a test that wants to say why it failed (`Server::log`).
    let log = std::fs::File::create(dir.join("server.log"))?;
    let mut cmd = server_command(dir, log)?;
    cmd.envs(env.iter().copied());
    let child = cmd.spawn()?;
    let mut server = Server { child, port, admin: String::new(), dir: dir.to_path_buf(), env: env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(), leave: false };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = server.child.try_wait()? {
            return Ok(Err(format!("it exited ({status}) on ports {port} and {quic}")));
        }
        if let Ok(t) = std::fs::read_to_string(data.join("admin.token"))
            && let Ok((200, _)) = http(port, "GET", "/healthz", "", None)
        {
            let t = t.trim().to_owned();
            // This server, not another test's on the same port.
            match http(port, "GET", "/api/repos", &t, None) {
                Ok((200, _)) => {
                    // Its QUIC port too: when another test's server holds it, this one's runner
                    // dials that one and never connects.
                    if std::fs::read_to_string(dir.join("server.log")).is_ok_and(|l| l.contains(&format!("bind 127.0.0.1:{quic}"))) {
                        return Ok(Err(format!("another server holds its QUIC port {quic}")));
                    }
                    server.admin = t;
                    return Ok(Ok(server));
                }
                Ok((401, _)) => return Ok(Err(format!("another server answers on port {port}"))),
                _ => {}
            }
        }
        anyhow::ensure!(Instant::now() < deadline, "the server didn't start");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A concrete path for a pattern: `*` an id, `**` two segments.
pub fn concrete(pattern: &str) -> String {
    pattern
        .split('/')
        .map(|p| match p {
            "*" => "x0",
            "**" => "x1/x2",
            p => p,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// An app whose template makes its source repository on another host (`host`: `github`, `gitlab`
/// or `bitbucket`; docs/deploy-api.md §3.1, step 4), from `starter` (a repository there holding
/// `file`, with `text` in it): `acme/<host>-web` is made there with the starter's files as its one
/// commit, kiln has it as one of its repositories (with `builds`, its first commit's KilnCI run
/// starts), and a name the host has already is refused. `git_root` is where the fake host keeps
/// its repositories.
pub fn source_on_host(s: &Server, host: &str, git_root: &Path, starter: &str, (file, text): (&str, &str), builds: bool) -> anyhow::Result<()> {
    let template = |name: &str, repo: &str| serde_json::json!({ "name": name, "package": { "kind": "helm", "repo": "oci://ghcr.io/acme/charts", "chart": "{app}" }, "repository": { "host": host, "name": repo, "starter": starter, "branch": "main" } });
    let (st, body) = http(s.port, "POST", "/api/deploy/templates", &s.admin, Some(&template(&format!("{host}-src"), "acme/{app}")))?;
    anyhow::ensure!((200..300).contains(&st), "the template: {st} {body}");
    let name = format!("{host}-web");
    let (st, body) = http(s.port, "POST", "/api/deploy/apps", &s.admin, Some(&serde_json::json!({ "name": name, "project": "shop", "template": format!("{host}-src") })))?;
    anyhow::ensure!(st == 201, "{name}: {st} {body}\n{}", s.log());
    let made: Value = serde_json::from_str(&body)?;
    anyhow::ensure!(made.get("source") == Some(&Value::String(format!("acme/{name}"))), "its source: {made}");
    // On the host: the starter's files, one commit.
    let bare = git_root.join(format!("acme/{name}.git"));
    let git = |args: &[&str]| -> anyhow::Result<String> {
        let o = Command::new("git").arg("-C").arg(&bare).args(args).env("GIT_CONFIG_NOSYSTEM", "1").output()?;
        anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    anyhow::ensure!(git(&["rev-list", "--count", "main"])? == "1", "one commit");
    anyhow::ensure!(git(&["show", &format!("main:{file}")])?.contains(text), "the starter's files");
    // One of kiln's repositories, as it is on the host.
    let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
    let repos: Value = serde_json::from_str(&body)?;
    let repo = repos.as_array().and_then(|l| l.iter().find(|r| r.get("name") == Some(&Value::String(format!("acme/{name}"))))).cloned().unwrap_or_default();
    anyhow::ensure!(repo.get("clone_url").and_then(Value::as_str).is_some_and(|u| u.ends_with(&format!("/acme/{name}.git")) && u.starts_with("http://127.0.0.1:")), "kiln's repository: {repo}");
    // Its first commit builds, as a push of it would.
    if builds {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let (_, body) = http(s.port, "GET", "/api/runs", &s.admin, None)?;
            let runs: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let list = runs.get("runs").and_then(Value::as_array).or_else(|| runs.as_array()).cloned().unwrap_or_default();
            if list.iter().any(|r| r.get("repo_name") == Some(&Value::String(format!("acme/{name}")))) {
                break;
            }
            anyhow::ensure!(Instant::now() < deadline, "no run of acme/{name}: {body}\n{}", s.log());
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    // A name the host has already: refused, nothing made.
    let (st, body) = http(s.port, "POST", "/api/deploy/templates", &s.admin, Some(&template(&format!("{host}-fixed"), &format!("acme/{name}"))))?;
    anyhow::ensure!((200..300).contains(&st), "the second template: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/deploy/apps", &s.admin, Some(&serde_json::json!({ "name": format!("{host}-other"), "project": "shop", "template": format!("{host}-fixed") })))?;
    anyhow::ensure!(st == 409 && body.contains("already"), "a name taken: {st} {body}");
    Ok(())
}

/// git in `dir`, as a person (ada) runs it: its output.
#[allow(dead_code)]
pub fn git_as_ada(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git")
        .args(["-c", "user.name=ada", "-c", "user.email=ada@example.com", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// Lays a chart on kiln-base 0.1.2 into the repository at `bare` on its main, as `kiln app init`
/// lays one out for `app`, a person's commit.
#[allow(dead_code)]
pub fn lay_chart(bare: &Path, app: &str) -> anyhow::Result<()> {
    let work = tempfile::tempdir()?;
    git_as_ada(work.path(), &["clone", "-q", &bare.to_string_lossy(), "."])?;
    let chart = work.path().join(format!("charts/{app}"));
    std::fs::create_dir_all(&chart)?;
    std::fs::write(chart.join("Chart.yaml"), format!("apiVersion: v2\nname: {app}\nversion: 0.1.0\ndependencies:\n  # kiln's base chart.\n  - name: kiln-base\n    version: 0.1.2\n    repository: oci://kiln.example.com/kiln/charts\n"))?;
    std::fs::write(chart.join("values.yaml"), "replicaCount: 1\nimage:\n  repository: ghcr.io/acme/web\n")?;
    git_as_ada(work.path(), &["add", "."])?;
    git_as_ada(work.path(), &["commit", "-qm", "a chart on kiln-base"])?;
    git_as_ada(work.path(), &["push", "-q", "origin", "HEAD:main"])?;
    Ok(())
}

/// A fleet change in the source repository [`source_on_host`] made (docs/deploy-api.md §3.3):
/// `acme/<host>-web` given a chart on kiln-base, kiln-base moved in it by a fleet change is one
/// pull request there (a merge request on GitLab) of only that change, on the change's branch;
/// asked again, the same one; merged there (`merged`: the fake host told its number was merged
/// into the commit, as a person merging it would make it so), it reads as merged and the change
/// as done; one the repository holds already proposes nothing; values set by another, withdrawn,
/// are closed there.
#[allow(dead_code)]
pub fn fleet_on_host(s: &Server, host: &str, git_root: &Path, merged: impl Fn(u64, &str) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let app = format!("{host}-web");
    let bare = git_root.join(format!("acme/{app}.git"));
    lay_chart(&bare, &app)?;
    let ask = |key: &str, source: Value| -> anyhow::Result<Value> {
        let (st, body) = http(s.port, "POST", "/api/deploy/fleets", &s.admin, Some(&serde_json::json!({ "apps": [app], "source": source, "idempotency_key": key })))?;
        anyhow::ensure!(st == 201, "a fleet change {key}: {st} {body}\n{}", s.log());
        Ok(serde_json::from_str(&body)?)
    };
    let one = |v: &Value| v.pointer("/proposals/0").cloned().unwrap_or_default();
    let base = format!("{host}-base");
    let v = ask(&base, serde_json::json!({ "base": "0.1.9" }))?;
    let p = one(&v);
    anyhow::ensure!(
        p.get("state") == Some(&Value::from("open")) && p.get("host") == Some(&Value::from(host)) && p.get("number") == Some(&Value::from(1)) && p.get("files") == Some(&serde_json::json!([format!("charts/{app}/Chart.yaml")])),
        "kiln-base moved: {v}"
    );
    let branch = format!("kiln/fleet/{base}");
    let proposed = git_as_ada(&bare, &["show", &format!("{branch}:charts/{app}/Chart.yaml")])?;
    anyhow::ensure!(proposed.contains("version: 0.1.9") && proposed.contains("repository: oci://kiln.example.com/kiln/charts") && proposed.contains("# kiln's base chart.") && !proposed.contains("0.1.2"), "only its version moves: {proposed}");
    anyhow::ensure!(git_as_ada(&bare, &["log", "-1", "--format=%B", &branch])?.contains(&format!("Kiln-Fleet: {base}")));
    let again = ask(&base, serde_json::json!({ "base": "0.1.9" }))?;
    anyhow::ensure!(one(&again).get("number") == Some(&Value::from(1)), "the same pull request: {again}");
    // Merged there: merged, and the change done.
    let (main, head) = (git_as_ada(&bare, &["rev-parse", "main"])?, git_as_ada(&bare, &["rev-parse", &branch])?);
    let tree = git_as_ada(&bare, &["rev-parse", &format!("{head}^{{tree}}")])?;
    let merge = git_as_ada(&bare, &["commit-tree", &tree, "-p", &main, "-p", &head, "-m", "Merge kiln's fleet change"])?;
    git_as_ada(&bare, &["update-ref", "refs/heads/main", &merge])?;
    merged(1, &merge)?;
    let (st, body) = http(s.port, "GET", &format!("/api/deploy/fleets/{base}"), &s.admin, None)?;
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    anyhow::ensure!(st == 200 && one(&v).get("state") == Some(&Value::from("merged")) && v.get("done") == Some(&Value::from(true)), "merged: {st} {body}\n{}", s.log());
    let v = ask(&format!("{host}-base-again"), serde_json::json!({ "base": "0.1.9" }))?;
    anyhow::ensure!(one(&v).get("state") == Some(&Value::from("unchanged")), "held already: {v}");
    // Values set, then withdrawn: closed there.
    let set = format!("{host}-replicas");
    let v = ask(&set, serde_json::json!({ "set": { "replicaCount": "3" } }))?;
    anyhow::ensure!(one(&v).get("state") == Some(&Value::from("open")) && one(&v).get("number") == Some(&Value::from(2)), "values set: {v}");
    let values = git_as_ada(&bare, &["show", &format!("kiln/fleet/{set}:charts/{app}/values.yaml")])?;
    anyhow::ensure!(values.contains("replicaCount: 3") && values.contains("repository: ghcr.io/acme/web"), "{values}");
    let (st, body) = http(s.port, "DELETE", &format!("/api/deploy/fleets/{set}"), &s.admin, None)?;
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    anyhow::ensure!(st == 200 && one(&v).get("state") == Some(&Value::from("closed")), "withdrawn: {st} {body}");
    Ok(())
}
