//! A throwaway kiln server for the integration tests, and the HTTP they speak to it.

#![allow(dead_code, reason = "each test file uses what it needs")]

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde_json::Value;

/// A port free now.
pub fn free_port() -> anyhow::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// One HTTP/1.1 request: the status and the body.
pub fn http(port: u16, method: &str, path: &str, token: &str, body: Option<&Value>) -> anyhow::Result<(u16, String)> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
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
}

impl Server {
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
        let _ = self.child.kill();
        let _ = self.child.wait();
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
    start_env(dir, o, https, &[])
}

/// `start_with`, the server's environment given `env` too.
pub fn start_env(dir: &Path, o: &Options<'_>, https: bool, env: &[(&str, &str)]) -> anyhow::Result<Server> {
    let (port, quic) = (free_port()?, free_port()?);
    let data = dir.join("data");
    let work = dir.join("work");
    let config = format!(
        r#"[server]
http_addr = "127.0.0.1:{port}"
quic_addr = "127.0.0.1:{quic}"
public_url = "{scheme}://127.0.0.1:{port}"
quic_advertise = "127.0.0.1:{quic}"
data_dir = "{data}"
https = {https}

[gitops]
auto_pipelines = false

[local_agent]
enabled = {agent}
labels = ["local"]
slots = 2
work_dir = "{work}"

[kubernetes]
enabled = false

[terminal]
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
enabled = true
{ci}

{extra}
"#,
        data = data.display(),
        work = work.display(),
        scheme = if https { "https" } else { "http" },
        agent = o.agent,
        ci = o.ci,
        extra = o.extra,
    );
    let cfg = dir.join("kiln.toml");
    std::fs::write(&cfg, config)?;
    let home = dir.join("home");
    std::fs::create_dir_all(&home)?;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kiln"));
    // Its log, for a test that wants to say why it failed (`Server::log`).
    let log = std::fs::File::create(dir.join("server.log"))?;
    cmd.args(["--config", &cfg.to_string_lossy(), "server"]).env("HOME", &home).env("NO_COLOR", "1").stdout(log.try_clone()?).stderr(log);
    for var in ["KILN_CONFIG", "KILN_URL", "KILN_ADMIN_TOKEN", "KILN_SUPERVISED", "KILN_GENERATION", "KILN_LOG", "KILN_CHAOS"] {
        cmd.env_remove(var);
    }
    cmd.envs(env.iter().copied());
    let child = cmd.spawn()?;
    let mut server = Server { child, port, admin: String::new(), dir: dir.to_path_buf() };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(t) = std::fs::read_to_string(data.join("admin.token"))
            && let Ok((200, _)) = http(port, "GET", "/healthz", "", None)
        {
            server.admin = t.trim().to_owned();
            return Ok(server);
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
