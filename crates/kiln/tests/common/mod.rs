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
    start_env(dir, o, https, &[])
}

/// `start_with`, the server's environment given `env` too. A port picked here can be taken before
/// the server binds it (tests run side by side): then the server exits, or another test's server
/// answers on it and refuses this one's token. Either way it starts again, on new ports.
pub fn start_env(dir: &Path, o: &Options<'_>, https: bool, env: &[(&str, &str)]) -> anyhow::Result<Server> {
    let mut why = String::new();
    for _ in 0..3 {
        match start_once(dir, o, https, env)? {
            Ok(server) => return Ok(server),
            Err(w) => why = w,
        }
    }
    anyhow::bail!("the server didn't start: {why}")
}

/// `kiln server` with `dir`'s configuration, its home in `dir`, writing to `log`.
fn server_command(dir: &Path, log: std::fs::File) -> anyhow::Result<Command> {
    let home = dir.join("home");
    std::fs::create_dir_all(&home)?;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kiln"));
    cmd.args(["--config", &dir.join("kiln.toml").to_string_lossy(), "server"]).env("HOME", &home).env("NO_COLOR", "1").stdout(log.try_clone()?).stderr(log);
    for var in ["KILN_CONFIG", "KILN_URL", "KILN_ADMIN_TOKEN", "KILN_SUPERVISED", "KILN_GENERATION", "KILN_LOG", "KILN_CHAOS"] {
        cmd.env_remove(var);
    }
    Ok(cmd)
}

fn start_once(dir: &Path, o: &Options<'_>, https: bool, env: &[(&str, &str)]) -> anyhow::Result<Result<Server, String>> {
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
        data = data.display(),
        work = work.display(),
        scheme = if https { "https" } else { "http" },
        agent = o.agent,
        ci = o.ci,
        extra = o.extra,
        // CD stays off unless a test turns it on with a `[cd]` of its own.
        cd = if o.extra.contains("[cd]") { "" } else { "[cd]\nenabled = false\n" },
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
