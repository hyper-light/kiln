//! What the live tests of observe share (docs/observe.md §13): a kiln server and runners as
//! processes of their own on this machine, each with a home of its own and a clean environment, on
//! real sockets and disks; and the requests that ask them things.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
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

pub fn free_port() -> anyhow::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
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
        let root = tempfile::tempdir()?;
        let (http, quic) = (free_port()?, free_port()?);
        let data = root.path().join("server");
        let cfg = format!(
            r#"
[server]
http_addr = "127.0.0.1:{http}"
quic_addr = "127.0.0.1:{quic}"
public_url = "http://127.0.0.1:{http}"
data_dir = "{data}"

[gitops]
auto_pipelines = false

[local_agent]
enabled = false

[discovery]
enabled = false

[kubernetes]
enabled = false

[cd]
enabled = false

[telemetry]
# Runners' host samples, and every node's own figures, once an hour: the test's writes are the
# writes (and the leader's cutoff, each minute).
sample_seconds = 3600

[observe]
enabled = true
# Each node looks at its host once, as it starts, and then not for an hour: the test's writes are the
# writes.
host_seconds = 3600

{extra}
"#,
            data = data.display(),
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
        c.env_clear().env("HOME", home).env("PATH", "/usr/local/bin:/usr/bin:/bin").env("LANG", "C.UTF-8").env("RUST_LOG", "info,kiln::observe=debug").env("KILN_LOG", "info,kiln::observe=debug");
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
        for p in &procs {
            // kiln itself: the process, or under samply its child.
            let pid = p.child.id();
            let kids = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).unwrap_or_default();
            let target = kids.split_whitespace().next().map_or_else(|| pid.to_string(), str::to_owned);
            let _ = Command::new("kill").args(["-TERM", &target]).status();
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
