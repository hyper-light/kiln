//! A throwaway kiln server for the integration tests, and the HTTP they speak to it.

#![allow(dead_code, reason = "each test file uses what it needs")]

#[cfg(unix)]
pub mod agents;
#[cfg(unix)]
pub mod lab;

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde_json::Value;

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
    start_full(dir, o, false, true, &[])
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
/// of its own.
fn server_command(dir: &Path, log: std::fs::File) -> anyhow::Result<Command> {
    let home = dir.join("home");
    std::fs::create_dir_all(&home)?;
    let mut cmd = kiln();
    cmd.args(["--config", &dir.join("kiln.toml").to_string_lossy(), "server"]).env("HOME", &home).env("NO_COLOR", "1").stdout(log.try_clone()?).stderr(log);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    Ok(cmd)
}

fn start_once(dir: &Path, o: &Options<'_>, https: bool, cd: bool, env: &[(&str, &str)]) -> anyhow::Result<Result<Server, String>> {
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
