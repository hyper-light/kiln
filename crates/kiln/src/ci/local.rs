//! `kiln workflow run` (docs/ci.md W17-R06): a working tree's workflows, run on this machine or on a
//! server's runners.
//!
//! - **The commit.** The working tree as git sees it (tracked files, and untracked ones git doesn't
//!   ignore) becomes a commit on `HEAD`, made with a temporary index so neither the index nor any
//!   branch moves. With nothing changed it is `HEAD`.
//! - **Here**, the default: a server of its own in this process, CI only, on loopback, with this
//!   machine as its one runner and a directory that goes when the run ends. It reads the commit
//!   through git's alternates, so nothing is copied, and it has the services legs talk to (caches,
//!   artifacts, OIDC tokens). Every leg runs here: `runs-on` keeps only what says which OS and
//!   architecture a leg needs, and a leg this machine can't run says so and stops the run.
//! - **`--server URL`**: the commit goes to that server as a thin pack over the commits it has, and
//!   its runners run it. Runs there are untrusted unless you're the server's owner.
//!
//! The runs' logs print as they come (stdout; what happens to runs and legs on stderr), and the
//! exit code is 0 when every run succeeded.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use super::cli::{Api, enc};
use crate::config::Config;
use crate::http::client::Method;
use crate::rt::Shutdown;

#[derive(clap::Args)]
pub struct WorkflowArgs {
    #[command(subcommand)]
    cmd: WorkflowCmd,
}

#[derive(clap::Subcommand)]
enum WorkflowCmd {
    /// Run this working tree's workflows: here, in a server of its own, or on a server's runners
    /// (`--server`). Exits 0 when every run succeeds.
    Run(RunArgs),
}

#[derive(clap::Args)]
pub struct RunArgs {
    /// The workflow: its path, file name or name. Every workflow the event starts when omitted.
    workflow: Option<String>,
    /// Only this job, and the jobs it needs.
    #[arg(long)]
    job: Option<String>,
    /// The event.
    #[arg(long, default_value = "push")]
    event: String,
    /// GitHub's payload of the event, a JSON file (`github.event`); kiln makes one otherwise.
    #[arg(long)]
    payload: Option<PathBuf>,
    /// Only matrix combinations with this value, e.g. `os=ubuntu-latest` (repeatable).
    #[arg(long = "matrix", value_name = "KEY=VALUE")]
    matrix: Vec<String>,
    /// A `workflow_dispatch` input (repeatable).
    #[arg(long = "input", value_name = "NAME=VALUE")]
    inputs: Vec<String>,
    /// Secrets for this run: `NAME=value` lines, as a dotenv file has them.
    #[arg(long)]
    secret_file: Option<PathBuf>,
    /// A variable, in `vars` (repeatable).
    #[arg(long = "var", value_name = "NAME=VALUE")]
    vars: Vec<String>,
    /// Where legs run here: `host`, `docker`, `firecracker` or `shards`. By default as a server
    /// routes them: GitHub's Ubuntu labels in kiln's runner image when Docker is here, the rest on
    /// the host.
    #[arg(long)]
    executor: Option<String>,
    /// Run on this server's runners instead.
    #[arg(long)]
    server: Option<String>,
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// The repository on that server (`owner/name`); by default the one `origin` points at.
    #[arg(long)]
    repo: Option<String>,
    /// Write what happened as JSON here: each run's conclusion, its legs' and their steps'.
    #[arg(long)]
    report: Option<PathBuf>,
}

/// `kiln workflow …`.
pub async fn workflow(args: WorkflowArgs, cfg: &Config, shutdown: Shutdown) -> Result<()> {
    let WorkflowCmd::Run(a) = args.cmd;
    let code = run(&a, cfg, &shutdown).await?;
    if code != 0 {
        return Err(anyhow::Error::new(crate::reach::cli::ExitWith(code)));
    }
    Ok(())
}

fn pairs(list: &[String], flag: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for p in list {
        let Some((k, v)) = p.split_once('=') else { bail!("{flag} takes NAME=VALUE, not `{p}`") };
        out.insert(k.trim().to_owned(), v.to_owned());
    }
    Ok(out)
}

/// `NAME=value` lines; `#` comments, blank lines, `export ` and quotes as dotenv files have them.
fn dotenv(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        let v = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')).or_else(|| v.strip_prefix('\'').and_then(|x| x.strip_suffix('\''))).unwrap_or(v);
        out.insert(k.trim().to_owned(), v.replace("\\n", "\n"));
    }
    out
}

async fn git(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    let mut cmd = std::process::Command::new("git");
    crate::os::git_no_prompts(&mut cmd).current_dir(dir).args(args).stdin(std::process::Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = crate::proc::output(cmd, Duration::from_secs(600)).await?;
    if !out.status.success() {
        bail!("git {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The working tree as a commit on `HEAD` (`HEAD` itself when nothing changed), and whether it's a
/// new one. Made in a copy of the index, so the index and the branches stay as they are.
async fn working_tree(root: &Path) -> Result<(String, bool)> {
    let head = git(root, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"], &[]).await.ok().filter(|s| !s.is_empty());
    let index = PathBuf::from(git(root, &["rev-parse", "--git-path", "index"], &[]).await?);
    let index = if index.is_absolute() { index } else { root.join(index) };
    let tmp = index.with_file_name(format!("kiln-workflow-run-{}.index", std::process::id()));
    // From a copy of the index, `git add` hashes only what changed since.
    if index.exists() {
        std::fs::copy(&index, &tmp).with_context(|| format!("copy {}", index.display()))?;
    }
    let tmp_s = tmp.to_string_lossy().into_owned();
    let made = async {
        let env = [("GIT_INDEX_FILE", tmp_s.as_str())];
        git(root, &["add", "--all"], &env).await?;
        let tree = git(root, &["write-tree"], &env).await?;
        if let Some(h) = &head
            && git(root, &["rev-parse", &format!("{h}^{{tree}}")], &[]).await? == tree
        {
            return Ok((h.clone(), false));
        }
        let mut args = vec!["commit-tree", tree.as_str(), "-m", "kiln workflow run: the working tree"];
        if let Some(h) = &head {
            args.extend(["-p", h.as_str()]);
        }
        // Who made it, when git wasn't told.
        let anonymous = git(root, &["config", "user.email"], &[]).await.map_or(true, |e| e.is_empty());
        let who: &[(&str, &str)] = if anonymous { &[("GIT_AUTHOR_NAME", "kiln"), ("GIT_AUTHOR_EMAIL", "kiln@localhost"), ("GIT_COMMITTER_NAME", "kiln"), ("GIT_COMMITTER_EMAIL", "kiln@localhost")] } else { &[] };
        Ok((git(root, &args, who).await?, true))
    }
    .await;
    let _ = std::fs::remove_file(&tmp);
    made
}

/// `owner/name` from a remote's URL (`https://host/owner/name.git`, `git@host:owner/name`).
fn owner_name(url: &str) -> Option<String> {
    let s = url.trim().trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    let mut parts = s.rsplit(['/', ':']);
    let name = parts.next().filter(|n| !n.is_empty())?;
    let owner = parts.next().filter(|o| !o.is_empty() && !o.contains('@'))?;
    Some(format!("{owner}/{name}"))
}

/// The repository's name: `--repo`, else from `origin`, else `local/<folder>`.
async fn repo_name(root: &Path, given: Option<&str>) -> String {
    if let Some(r) = given {
        return r.to_owned();
    }
    if let Some(n) = git(root, &["remote", "get-url", "origin"], &[]).await.ok().as_deref().and_then(owner_name) {
        return n;
    }
    let folder = root.file_name().map(|f| f.to_string_lossy().replace(|c: char| c.is_whitespace() || c == '/', "-")).unwrap_or_else(|| "repo".into());
    format!("local/{folder}")
}

async fn run(a: &RunArgs, cfg: &Config, shutdown: &Shutdown) -> Result<u8> {
    let cwd = std::env::current_dir()?;
    let root = PathBuf::from(git(&cwd, &["rev-parse", "--show-toplevel"], &[]).await.context("kiln workflow run works in a git repository")?);
    let branch = git(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"], &[]).await.unwrap_or_else(|_| "HEAD".into());
    let (sha, made) = working_tree(&root).await?;
    let payload = match &a.payload {
        Some(p) => Some(serde_json::from_str::<Value>(&std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?).with_context(|| format!("{} isn't JSON", p.display()))?),
        None => None,
    };
    let secrets = match &a.secret_file {
        Some(p) => dotenv(&std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?),
        None => BTreeMap::new(),
    };
    let inputs: serde_json::Map<String, Value> = pairs(&a.inputs, "--input")?.into_iter().map(|(k, v)| (k, Value::String(v))).collect();
    let ask = json!({
        "sha": sha,
        "ref": if branch == "HEAD" { String::new() } else { branch.clone() },
        "event": a.event,
        "payload": payload,
        "workflows": a.workflow.iter().collect::<Vec<_>>(),
        "job": a.job,
        "matrix": pairs(&a.matrix, "--matrix")?,
        "inputs": inputs,
        "secrets": secrets,
        "vars": pairs(&a.vars, "--var")?,
    });
    let what = if made { format!("the working tree (commit {} on {branch})", &sha[..12]) } else { format!("{branch} at {}", &sha[..12]) };
    match &a.server {
        Some(url) => {
            if a.executor.is_some() {
                bail!("--executor is for runs here; on a server its routing decides where legs run");
            }
            let api = Api::connect(cfg, Some(url), a.token.as_deref()).await?;
            let repo = repo_name(&root, a.repo.as_deref()).await;
            say(&format!("{what}: on {url}'s runners, as {repo}"));
            there(&api, &root, &repo, &sha, ask, a.report.as_deref(), shutdown).await
        }
        None => {
            say(&format!("{what}: here"));
            here(a, &root, ask, shutdown).await
        }
    }
}

fn say(s: &str) {
    let _ = writeln!(std::io::stderr(), "[kiln] {s}");
}

/// On a server: the commit goes as a pack unless the server has it, then the runs.
async fn there(api: &Api, root: &Path, repo: &str, sha: &str, ask: Value, report: Option<&Path>, shutdown: &Shutdown) -> Result<u8> {
    let heads = api.call(Method::Get, &format!("/repos/{}/heads", enc(repo)), None).await.with_context(|| format!("{repo} on the server (--repo names another)"))?;
    let heads: Vec<String> = heads.get("heads").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
    if !heads.iter().any(|h| h == sha) {
        upload(api, root, repo, sha, &heads).await?;
    }
    let started = api.call(Method::Post, &format!("/repos/{}/local-runs", enc(repo)), Some(ask)).await?;
    if started.get("trusted").and_then(Value::as_bool) == Some(false) {
        say("these runs are untrusted (you aren't the server's owner): a read-only token, and only the secrets you sent");
    }
    let code = follow(api, &started, false, shutdown).await?;
    if let Some(p) = report {
        write_report(api, &started, p).await?;
    }
    Ok(code)
}

/// `--report`: each run's conclusion, its legs' and their steps', as JSON.
async fn write_report(api: &Api, started: &Value, path: &Path) -> Result<()> {
    let mut runs = Vec::new();
    for r in started.get("runs").and_then(Value::as_array).into_iter().flatten() {
        let Some(id) = r.get("id").and_then(Value::as_str) else { continue };
        let v = api.call(Method::Get, &format!("/runs/{id}"), None).await?;
        let mut legs = Vec::new();
        for l in v.get("legs").and_then(Value::as_array).into_iter().flatten() {
            let lid = l.get("id").and_then(Value::as_str).unwrap_or_default();
            let d = api.call(Method::Get, &format!("/legs/{lid}"), None).await?;
            let mut steps: Vec<&Value> = d.get("steps").and_then(Value::as_array).into_iter().flatten().collect();
            steps.sort_by_key(|s| (s.get("seq").and_then(Value::as_i64).unwrap_or(i64::MAX), s.get("idx").and_then(Value::as_i64).unwrap_or_default()));
            let steps: Vec<Value> = steps.iter().map(|s| json!({ "name": s.get("name"), "conclusion": s.get("conclusion"), "outcome": s.get("outcome") })).collect();
            legs.push(json!({ "key": l.get("key"), "status": l.get("status"), "conclusion": l.get("conclusion"), "error": l.get("error"), "steps": steps }));
        }
        let run = v.get("run").cloned().unwrap_or_default();
        runs.push(json!({ "workflow": r.get("workflow"), "name": run.get("name"), "number": run.get("number"), "status": run.get("status"), "conclusion": run.get("conclusion"), "error": run.get("error"), "legs": legs }));
    }
    let skipped = started.get("skipped").cloned().unwrap_or(Value::Array(Vec::new()));
    std::fs::write(path, serde_json::to_vec_pretty(&json!({ "runs": runs, "skipped": skipped }))?).with_context(|| format!("write {}", path.display()))
}

/// The commit's objects to the server: a thin pack over the commits it has here too, or the whole
/// history when the server can't take that.
async fn upload(api: &Api, root: &Path, repo: &str, sha: &str, heads: &[String]) -> Result<()> {
    let have: Vec<&String> = {
        let mut have = Vec::new();
        for h in heads {
            if git(root, &["cat-file", "-e", &format!("{h}^{{commit}}")], &[]).await.is_ok() {
                have.push(h);
            }
        }
        have
    };
    let thin = !have.is_empty();
    match send_pack(api, root, repo, sha, &have).await {
        Ok(bytes) => {
            say(&format!("sent the commit ({} KiB)", bytes / 1024));
            Ok(())
        }
        Err(e) if thin => {
            say(&format!("the server took no thin pack ({e:#}); sending the whole history"));
            let bytes = send_pack(api, root, repo, sha, &[]).await?;
            say(&format!("sent the history ({} KiB)", bytes / 1024));
            Ok(())
        }
        Err(e) => Err(e),
    }
}

async fn send_pack(api: &Api, root: &Path, repo: &str, sha: &str, bases: &[&String]) -> Result<u64> {
    let mut cmd = std::process::Command::new("git");
    crate::os::git_no_prompts(&mut cmd).current_dir(root).args(["pack-objects", "--revs", "--stdout", "-q"]);
    if !bases.is_empty() {
        cmd.arg("--thin");
    }
    let child = crate::proc::spawn_duplex(cmd)?;
    let mut revs = format!("{sha}\n");
    for b in bases {
        revs.push_str(&format!("^{b}\n"));
    }
    child.stdin.send(revs.into_bytes()).await.ok();
    let crate::proc::Duplex { stdin, stdout, stderr, exit, .. } = child;
    drop(stdin);
    let (tx, rx) = async_channel::bounded::<bytes::Bytes>(16);
    let pump = crate::rt::spawn(async move {
        let mut n = 0u64;
        while let Ok(c) = stdout.recv().await {
            n += c.len() as u64;
            if tx.send(bytes::Bytes::from(c)).await.is_err() {
                break;
            }
        }
        n
    });
    let errs = crate::rt::spawn(async move {
        let mut all = Vec::new();
        while let Ok(c) = stderr.recv().await {
            all.extend(c);
        }
        all
    });
    let res = api.upload(&format!("/repos/{}/packs?sha={sha}", enc(repo)), rx).await;
    let sent = pump.await.unwrap_or(0);
    let code = exit.await.ok().flatten();
    let errs = errs.await.unwrap_or_default();
    if code != Some(0) {
        bail!("git pack-objects: {}", String::from_utf8_lossy(&errs).trim());
    }
    res?;
    Ok(sent)
}

/// A free loopback port, TCP or UDP.
fn free_port(udp: bool) -> Result<u16> {
    Ok(if udp { std::net::UdpSocket::bind("127.0.0.1:0")?.local_addr()?.port() } else { std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port() })
}

/// Here: a server of its own, the runs, then everything it made goes.
async fn here(a: &RunArgs, root: &Path, ask: Value, shutdown: &Shutdown) -> Result<u8> {
    let dir = crate::config::default_data_dir().join("workflow-runs").join(crate::util::new_id());
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let result = here_in(a, root, ask, &dir, shutdown).await;
    // Workspaces, tool caches, the server's files and its mirror (whose objects are the
    // repository's own): all of it goes.
    crate::runner::workspaces::release(&dir).await;
    result
}

/// The server's configuration: CI only, on loopback, its one runner this machine.
fn server_config(dir: &Path, http: u16, quic: u16, executor: Option<&str>) -> Config {
    let mut c = Config::default();
    c.server.http_addr = format!("127.0.0.1:{http}");
    c.server.quic_addr = format!("127.0.0.1:{quic}");
    c.server.public_url = format!("http://127.0.0.1:{http}");
    c.server.quic_advertise = c.server.quic_addr.clone();
    c.server.data_dir = dir.join("server");
    c.server.admin_token_file = None;
    c.server.https = false;
    c.repos.clear();
    c.gitops.orgs.clear();
    c.secrets.clear();
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    c.local_agent.enabled = true;
    c.local_agent.labels = Vec::new();
    c.local_agent.slots = u32::try_from((cpus / 2).max(2)).unwrap_or(2);
    c.local_agent.work_dir = Some(dir.join("work"));
    c.kubernetes.enabled = false;
    c.terminal.enabled = false;
    c.discovery.enabled = false;
    c.discovery.announce = false;
    c.discovery.browse = false;
    c.discovery.join_requests = false;
    c.cd.enabled = false;
    c.ci.enabled = true;
    c.ci.kiln_workflows = true;
    c.ci.github_labels = true;
    c.ci.local = true;
    c.ci.routing = match executor {
        Some(e) => vec![kiln_ci::sched::routing::Rule { executor: vec![e.to_owned()], image: (e != "host").then(|| "kiln/runner-ubuntu-24.04".to_owned()), ..Default::default() }],
        None => Vec::new(),
    };
    c
}

async fn here_in(a: &RunArgs, root: &Path, ask: Value, dir: &Path, shutdown: &Shutdown) -> Result<u8> {
    if let Some(e) = a.executor.as_deref()
        && !matches!(e, "host" | "docker" | "firecracker" | "shards")
    {
        bail!("--executor is host, docker, firecracker or shards, not `{e}`");
    }
    let repo = repo_name(root, a.repo.as_deref()).await;
    let cfg = server_config(dir, free_port(false)?, free_port(true)?, a.executor.as_deref());
    // The server reads the repository through alternates: nothing is copied.
    let mirror = cfg.data("git").join(format!("{}.git", repo.replace('/', "__")));
    std::fs::create_dir_all(cfg.data("git"))?;
    git(root, &["clone", "--quiet", "--bare", "--shared", &root.to_string_lossy(), &mirror.to_string_lossy()], &[]).await?;
    let token = crate::server::admin_token(&cfg)?;
    // Its server listens on loopback, which containers reach on the host's network (Linux); Docker
    // Desktop's `host.docker.internal` reaches the host's loopback as it is.
    if cfg!(target_os = "linux") {
        crate::runner::docker::use_host_network();
    }
    let url = cfg.server.public_url.clone();
    let server_down = Shutdown::new();
    let server = {
        let (cfg, sd) = (cfg.clone(), server_down.clone());
        crate::rt::spawn(async move { crate::server::run(cfg, sd).await })
    };
    let api = Api::local(&url, &token)?;
    let result = async {
        let up = Instant::now();
        while api.call(Method::Get, "/runs?limit=1", None).await.is_err() {
            if up.elapsed() > Duration::from_secs(30) {
                bail!("kiln's own server didn't come up");
            }
            crate::rt::sleep(Duration::from_millis(100)).await;
        }
        // Its runner, this machine, connected and taking work.
        while !api.call_api(Method::Get, "/api/agents", None).await?.as_array().is_some_and(|a| a.iter().any(|x| x.get("connected").and_then(Value::as_bool) == Some(true))) {
            if up.elapsed() > Duration::from_secs(60) {
                bail!("this machine's runner didn't connect to kiln's own server");
            }
            crate::rt::sleep(Duration::from_millis(100)).await;
        }
        api.call_api(Method::Post, "/api/repos", Some(json!({ "name": repo, "clone_url": format!("file://{}", root.display()) }))).await?;
        let started = api.call(Method::Post, &format!("/repos/{}/local-runs", enc(&repo)), Some(ask)).await?;
        let code = follow(&api, &started, true, shutdown).await?;
        if let Some(p) = &a.report {
            write_report(&api, &started, p).await?;
        }
        Ok(code)
    }
    .await;
    server_down.cancel();
    let _ = crate::rt::timeout(Duration::from_secs(10), server).await;
    result
}

/// A leg as the follower has seen it.
#[derive(Default)]
struct Seen {
    key: String,
    /// Bytes printed of each step's log, and what of its last line is still to come.
    printed: HashMap<u32, usize>,
    partial: HashMap<u32, String>,
    /// What it was last said to wait for, and since when it has waited for what never comes here.
    said: String,
    never_since: Option<Instant>,
    done: bool,
}

/// What a leg waits for, in words, and whether that can't change on a server of this machine
/// alone; `None` for waits that end by themselves (a busy runner, a retry's delay).
fn waiting(w: &Value) -> Option<(String, bool)> {
    let o = match w {
        Value::String(s) if !s.is_empty() => return Some((s.clone(), false)),
        Value::Object(o) => o,
        _ => return None,
    };
    let list = |k: &str| o.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default();
    let text = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    Some(match o.get("reason").and_then(Value::as_str).unwrap_or_default() {
        "capacity" | "retry" | "" => return None,
        "unroutable" => (format!("no runner meets its requirements ({})", list("failing")), true),
        "capability" => (format!("{} can't run what it uses (Docker, or actions)", list("runners")), true),
        "no-runner" => ("no runner that can take it is connected".to_owned(), false),
        "approval" => (format!("a reviewer's approval to deploy to {} ({})", text("environment"), list("reviewers")), true),
        "wait-timer" => (format!("the wait timer of {}", text("environment")), false),
        "concurrency" => (format!("its concurrency group {}", text("group")), false),
        other => (other.replace('-', " "), false),
    })
}

/// Prints the runs' logs as they come and what happens to them, until they end. The exit code:
/// 0 when every run succeeded, 130 when stopped here, else 1.
async fn follow(api: &Api, started: &Value, here: bool, shutdown: &Shutdown) -> Result<u8> {
    let runs: Vec<(String, String)> = started.get("runs").and_then(Value::as_array).map(|a| a.iter().filter_map(|r| Some((r.get("id")?.as_str()?.to_owned(), r.get("workflow")?.as_str()?.to_owned()))).collect()).unwrap_or_default();
    for s in started.get("skipped").and_then(Value::as_array).into_iter().flatten() {
        say(&format!("{}: not run, {}", s.get("workflow").and_then(Value::as_str).unwrap_or("?"), s.get("why").and_then(Value::as_str).unwrap_or("?")));
    }
    if runs.is_empty() {
        say("no workflow runs for this event");
        return Ok(1);
    }
    let color = std::io::stdout().is_terminal();
    let mut legs: HashMap<String, Seen> = HashMap::new();
    let mut ended: HashMap<String, String> = HashMap::new();
    let mut announced: HashSet<String> = HashSet::new();
    let mut stuck: HashSet<String> = HashSet::new();
    let mut stopping = false;
    let mut out = std::io::stdout();
    loop {
        if shutdown.is_cancelled() && !stopping {
            stopping = true;
            say("stopping: cancelling the runs");
            for (id, _) in runs.iter().filter(|(id, _)| !ended.contains_key(id)) {
                let _ = api.call(Method::Post, &format!("/runs/{id}/cancel"), Some(json!({}))).await;
            }
        }
        for (id, path) in &runs {
            if ended.contains_key(id) {
                continue;
            }
            let v = api.call(Method::Get, &format!("/runs/{id}"), None).await?;
            let run = v.get("run").cloned().unwrap_or_default();
            let text = |k: &str| run.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
            if announced.insert(id.clone()) {
                say(&format!("{} #{} ({path}) started", text("name"), run.get("number").and_then(Value::as_i64).unwrap_or(0)));
            }
            let mut running = false;
            for l in v.get("legs").and_then(Value::as_array).into_iter().flatten() {
                let lid = l.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                let status = l.get("status").and_then(Value::as_str).unwrap_or_default().to_owned();
                let seen = legs.entry(lid.clone()).or_insert_with(|| Seen { key: l.get("key").and_then(Value::as_str).unwrap_or_default().to_owned(), ..Seen::default() });
                if seen.done {
                    continue;
                }
                let terminal = crate::store::Status::parse(&status).is_terminal();
                running |= matches!(status.as_str(), "running" | "assigned");
                match waiting(l.get("wait_reason").unwrap_or(&Value::Null)).filter(|_| matches!(status.as_str(), "queued" | "pending")) {
                    Some((what, never)) => {
                        if what != seen.said {
                            say(&format!("{} waits: {what}", seen.key));
                            seen.said = what;
                        }
                        // Here no other runner or reviewer comes: what this machine can't do stops
                        // the run, once its runner has had time to say what it can.
                        if here && never {
                            let since = *seen.never_since.get_or_insert_with(Instant::now);
                            if since.elapsed() > Duration::from_secs(5) {
                                stuck.insert(id.clone());
                            }
                        }
                    }
                    None => seen.never_since = None,
                }
                if status == "queued" || status == "pending" {
                    continue;
                }
                print_logs(api, &lid, seen, color, &mut out).await?;
                if terminal {
                    seen.done = true;
                    let conclusion = l.get("conclusion").and_then(Value::as_str).unwrap_or(&status).to_owned();
                    let error = l.get("error").and_then(Value::as_str).filter(|e| !e.is_empty()).map(|e| format!(": {e}")).unwrap_or_default();
                    let mark = if matches!(conclusion.as_str(), "success" | "skipped") { "✓" } else { "✗" };
                    say(&format!("{mark} {} {conclusion}{error}", seen.key));
                }
            }
            if stuck.contains(id) && !running && !stopping {
                say(&format!("{}: a leg can't run on this machine (above); cancelling the run. --job runs one job and what it needs", text("name")));
                let _ = api.call(Method::Post, &format!("/runs/{id}/cancel"), Some(json!({}))).await;
                stuck.remove(id);
            }
            let status = text("status");
            if crate::store::Status::parse(&status).is_terminal() && v.get("legs").and_then(Value::as_array).into_iter().flatten().all(|l| l.get("id").and_then(Value::as_str).is_none_or(|i| legs.get(i).is_some_and(|s| s.done))) {
                let conclusion = run.get("conclusion").and_then(Value::as_str).unwrap_or(&status).to_owned();
                let error = run.get("error").and_then(Value::as_str).filter(|e| !e.is_empty()).map(|e| format!("\n{e}")).unwrap_or_default();
                say(&format!("{} {} #{}: {conclusion}{error}", if conclusion == "success" { "✓" } else { "✗" }, text("name"), run.get("number").and_then(Value::as_i64).unwrap_or(0)));
                ended.insert(id.clone(), conclusion);
            }
        }
        if ended.len() == runs.len() {
            break;
        }
        crate::rt::sleep(Duration::from_millis(500)).await;
    }
    let _ = out.flush();
    Ok(if stopping {
        130
    } else if ended.values().all(|c| matches!(c.as_str(), "success" | "skipped" | "neutral")) {
        0
    } else {
        1
    })
}

/// What a leg's steps logged since last time, each line after the leg's key.
async fn print_logs(api: &Api, leg: &str, seen: &mut Seen, color: bool, out: &mut std::io::Stdout) -> Result<()> {
    let d = api.call(Method::Get, &format!("/legs/{leg}"), None).await?;
    // In the order they started.
    let mut steps: Vec<(i64, u32, String)> = d.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|s| Some((s.get("seq").and_then(Value::as_i64).unwrap_or(i64::MAX), u32::try_from(s.get("idx")?.as_i64()?).ok()?, s.get("status").and_then(Value::as_str).unwrap_or_default().to_owned()))).collect();
    steps.sort();
    let (dim, plain) = if color { ("\x1b[2m", "\x1b[0m") } else { ("", "") };
    for (_, idx, status) in steps {
        if status == "pending" || status == "queued" {
            continue;
        }
        let from = seen.printed.get(&idx).copied().unwrap_or(0);
        let text = api.call(Method::Get, &format!("/legs/{leg}/logs?step={idx}&from={from}"), None).await?;
        let text = text.as_str().unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        seen.printed.insert(idx, from + text.len());
        let pending = seen.partial.entry(idx).or_default();
        pending.push_str(text);
        let finished = crate::store::Status::parse(&status).is_terminal();
        while let Some(at) = pending.find('\n') {
            let line: String = pending.drain(..=at).collect();
            let _ = write!(out, "{dim}{}│{plain} {line}", seen.key);
        }
        if finished && !pending.is_empty() {
            let _ = writeln!(out, "{dim}{}│{plain} {}", seen.key, std::mem::take(pending));
        }
    }
    let _ = out.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w17_r06_names_and_files() {
        assert_eq!(owner_name("https://github.com/acme/app.git").as_deref(), Some("acme/app"));
        assert_eq!(owner_name("git@github.com:acme/app.git").as_deref(), Some("acme/app"));
        assert_eq!(owner_name("ssh://git@host:2222/acme/app").as_deref(), Some("acme/app"));
        assert_eq!(owner_name("app"), None);
        let env = dotenv("# secrets\nTOKEN=abc\nexport QUOTED=\"a b\"\nSINGLE='x=y'\n\nMULTI=one\\ntwo\n");
        assert_eq!(env.get("TOKEN").map(String::as_str), Some("abc"));
        assert_eq!(env.get("QUOTED").map(String::as_str), Some("a b"));
        assert_eq!(env.get("SINGLE").map(String::as_str), Some("x=y"));
        assert_eq!(env.get("MULTI").map(String::as_str), Some("one\ntwo"));
    }
}
