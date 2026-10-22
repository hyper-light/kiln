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
        // Its process group with it: what it started (its UI server) is never left running.
        #[cfg(unix)]
        if let Ok(pid) = i32::try_from(self.child.id()) {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
        }
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

/// [`start`], with KilnCD and deploying by API running (no cluster of its own).
pub fn start_cd(dir: &Path, o: &Options<'_>) -> anyhow::Result<Server> {
    start_with(dir, o, true)
}

fn start_with(dir: &Path, o: &Options<'_>, cd: bool) -> anyhow::Result<Server> {
    let (port, quic) = (free_port()?, free_port()?);
    let data = dir.join("data");
    let work = dir.join("work");
    let config = format!(
        r#"[server]
http_addr = "127.0.0.1:{port}"
quic_addr = "127.0.0.1:{quic}"
public_url = "http://127.0.0.1:{port}"
quic_advertise = "127.0.0.1:{quic}"
data_dir = "{data}"
https = false

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
enabled = {cd}

[ci]
enabled = true
{ci}

{extra}
"#,
        data = data.display(),
        work = work.display(),
        agent = o.agent,
        cd = cd,
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
    for var in ["KILN_CONFIG", "KILN_URL", "KILN_ADMIN_TOKEN", "KILN_SUPERVISED", "KILN_GENERATION", "KILN_LOG"] {
        cmd.env_remove(var);
    }
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
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
