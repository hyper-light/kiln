//! Replacing a machine's runner without a restart, for runners that cannot take a new binary in
//! place (those from before in-place upgrades): `kiln install --agent --upgrade` starts a new
//! runner *beside* the old one, as another process ("instance") of the same machine with its own
//! certificate. The server sends all new work to the newest instance; the old one keeps every
//! terminal, agent session and job it already runs. Once it holds nothing, the server tells the
//! new instance, which stops and disables the old service (after checking the old process really
//! has no terminals left). From then on the new runner upgrades in place (same process, terminals
//! handed across the exec), so this is needed once per machine.
//!
//! Per instance, under `<state>/instances/<id>/`: its certificate (`agent.pem`; the machine's key
//! `agent.key` is shared), `instance.json` (when it was set up), `replaces.json` (the service it
//! replaces) and its own mutable files (agent sessions, terminal handover, the `kiln spawn`
//! socket), so two runner processes sharing a state directory never write the same file.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// A machine's usual runner (no `--instance`).
pub const MAIN: &str = "main";

pub fn instance_dir(state_dir: &Path, id: &str) -> PathBuf {
    state_dir.join("instances").join(id)
}

/// Where a runner keeps its own mutable files: its instance directory, else the state directory.
pub fn own_dir(state_dir: &Path, instance: Option<&str>) -> PathBuf {
    instance.map_or_else(|| state_dir.to_path_buf(), |id| instance_dir(state_dir, id))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceRecord {
    pub id: String,
    pub created_at: i64,
}

/// The service a new instance replaces, and how to stop it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Replaces {
    /// "systemd", "launchd", "task" (a Windows Scheduled Task) or "startup" (its Startup-folder
    /// fallback).
    pub boot: String,
    pub system: bool,
    /// The unit / label / task name.
    pub unit: String,
    /// The old service's binary, also the `kiln` command users run: brought up to date once the
    /// old service is gone (it could not be replaced before: the old supervisor watches it).
    #[serde(default)]
    pub cli_bin: Option<PathBuf>,
    /// Set once the old service was stopped.
    #[serde(default)]
    pub retired_at: Option<i64>,
}

fn read_json<T: for<'de> Deserialize<'de>>(p: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

fn write_json<T: Serialize>(p: &Path, v: &T) -> anyhow::Result<()> {
    let body = serde_json::to_vec_pretty(v)?;
    Ok(crate::util::write_secret(p, &body, 0o644)?)
}

/// When instance `id` was set up (0 for [`MAIN`] or an unknown one): the newest gets new work.
pub fn created_at(state_dir: &Path, id: &str) -> i64 {
    if id == MAIN {
        return 0;
    }
    read_json::<InstanceRecord>(&instance_dir(state_dir, id).join("instance.json")).map_or(0, |r| r.created_at)
}

/// The machine's certificate and key, as enrolled (shared by its instances).
pub fn machine_credentials(state_dir: &Path) -> anyhow::Result<(String, String)> {
    let cert = std::fs::read_to_string(state_dir.join("agent.pem")).context("this machine is not enrolled (no agent.pem)")?;
    let key = std::fs::read_to_string(state_dir.join("agent.key")).context("this machine is not enrolled (no agent.key)")?;
    Ok((cert, key))
}

/// Ask the server for this instance's own certificate, proving the machine's identity with a
/// current certificate (the machine's, or this instance's own one being renewed) and its key.
pub async fn fetch_certificate(server_url: &str, ca_pem: Option<&str>, name: &str, current_cert: &str, key_pem: &str) -> anyhow::Result<crate::agent::EnrollResponse> {
    use crate::http::client::{Client, Roots};
    let csr = crate::pki::make_csr_with_key(name, key_pem)?;
    let roots = match ca_pem {
        Some(pem) => Roots::WebPkiPlusDer(crate::join::pem_der(pem)?),
        None => Roots::WebPki,
    };
    let http = Client::new(roots, None)?;
    let url = format!("{}/api/agents/instance", server_url.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .json(&crate::agent::InstanceRequest { cert_pem: current_cert.to_owned(), csr })
        .send()
        .await
        .with_context(|| format!("reach {url}"))?;
    if !resp.is_success() {
        let status = resp.status();
        let body = resp.bytes(1 << 16).await.unwrap_or_default();
        anyhow::bail!("the server refused a certificate for a new runner process ({status}): {}", String::from_utf8_lossy(&body).trim());
    }
    resp.json(1 << 20).await.context("instance certificate response")
}

// ------------------------------------------------------------------ retiring the old runner

/// What retiring an older instance did.
#[derive(Debug, PartialEq, Eq)]
pub enum Retired {
    /// The old service was stopped and disabled.
    Stopped(String),
    /// Nothing to do (already retired, or this instance replaces nothing).
    Nothing,
    /// The old process still holds terminals: left alone.
    Busy(usize),
}

/// The old service was told about by `<instance dir>/replaces.json`: stop and disable it, once its
/// processes hold no terminal (checked here as well as by the server). Blocking: runs the
/// platform's service manager.
pub fn retire(state_dir: &Path, me: &str) -> anyhow::Result<Retired> {
    let path = instance_dir(state_dir, me).join("replaces.json");
    let Some(mut r) = read_json::<Replaces>(&path) else { return Ok(Retired::Nothing) };
    // Retired before and not running again (someone may have started it again: stop it again).
    if r.retired_at.is_some() && !service_active(&r.boot, r.system, &r.unit) {
        return Ok(Retired::Nothing);
    }
    let pids = service_pids(&r);
    let held: usize = pids.iter().map(|p| crate::os::pty_masters(*p).unwrap_or(0)).sum();
    if held > 0 {
        return Ok(Retired::Busy(held));
    }
    stop_service(&r)?;
    // The old supervisor no longer watches its binary: bring the `kiln` command up to date.
    if let Some(bin) = &r.cli_bin {
        match std::env::current_exe().and_then(|exe| copy_atomic(&exe, bin)) {
            Ok(()) => tracing::info!(path = %bin.display(), "updated the kiln command to this runner's version"),
            Err(e) => tracing::warn!(error = %e, path = %bin.display(), "could not update the kiln command"),
        }
    }
    r.retired_at = Some(crate::util::now_unix());
    write_json(&path, &r)?;
    Ok(Retired::Stopped(r.unit))
}

fn copy_atomic(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.canonicalize().ok() == to.canonicalize().ok() {
        return Ok(());
    }
    let tmp = to.with_extension(format!("new.{}", std::process::id()));
    std::fs::copy(from, &tmp)?;
    crate::os::make_executable(&tmp)?;
    std::fs::rename(&tmp, to)
}

fn output(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The old service's processes (its supervisor and everything under it), where the platform can
/// tell. Empty when it is not running.
fn service_pids(r: &Replaces) -> Vec<u32> {
    match r.boot.as_str() {
        "systemd" => {
            let mut cmd = Command::new("systemctl");
            if !r.system {
                cmd.arg("--user");
            }
            cmd.args(["show", "-p", "ControlGroup", "--value", &format!("{}.service", r.unit)]);
            let Some(cg) = output(&mut cmd).filter(|c| !c.is_empty()) else { return vec![] };
            let procs = Path::new("/sys/fs/cgroup").join(cg.trim_start_matches('/')).join("cgroup.procs");
            std::fs::read_to_string(procs).map(|s| s.lines().filter_map(|l| l.trim().parse().ok()).collect()).unwrap_or_default()
        }
        _ => vec![],
    }
}

fn stop_service(r: &Replaces) -> anyhow::Result<()> {
    match r.boot.as_str() {
        "systemd" => {
            let mut cmd = Command::new("systemctl");
            if !r.system {
                cmd.arg("--user");
            }
            let st = cmd.args(["disable", "--now", &format!("{}.service", r.unit)]).status().context("run systemctl")?;
            anyhow::ensure!(st.success(), "systemctl disable --now {} failed ({st})", r.unit);
            Ok(())
        }
        "launchd" => {
            let label = format!("io.github.hyper-light.{}", r.unit);
            let domain = if r.system { "system".to_owned() } else { format!("gui/{}", crate::os::uid()) };
            let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{label}")]).status();
            let plist = if r.system {
                PathBuf::from(format!("/Library/LaunchDaemons/{label}.plist"))
            } else {
                crate::os::home_dir().join(format!("Library/LaunchAgents/{label}.plist"))
            };
            let _ = std::fs::remove_file(plist);
            Ok(())
        }
        "task" => {
            let _ = Command::new("schtasks").args(["/End", "/TN", &r.unit]).status();
            let st = Command::new("schtasks").args(["/Delete", "/TN", &r.unit, "/F"]).status().context("run schtasks")?;
            anyhow::ensure!(st.success(), "schtasks /Delete {} failed ({st})", r.unit);
            Ok(())
        }
        other => anyhow::bail!("do not know how to stop a {other} service"),
    }
}

// ------------------------------------------------------------------ installing beside the old one

/// The existing agent service, read from its boot configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Existing {
    pub boot: String,
    pub system: bool,
    pub unit: String,
    /// The binary it runs (`kiln`): what an upgrade replaces.
    pub bin: PathBuf,
    /// The launcher its launchd job starts `bin` from (`<launcher> supervise --exe <bin> -- …`),
    /// which upgrades leave alone (`crate::install::launcher_path`).
    pub launcher: Option<PathBuf>,
    /// What follows `supervise --`: `agent --server … --name …`.
    pub args: Vec<String>,
    /// Its environment (`KEY=value`; never a join token): the new runner gets the same.
    pub env: Vec<String>,
}

/// Split a systemd `ExecStart=` value into words (kiln writes it with POSIX shell quoting).
fn split_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                any = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Parse a kiln agent unit file: `ExecStart=<bin> supervise -- agent …`, and its environment.
pub fn parse_unit(body: &str, unit: &str, system: bool) -> Option<Existing> {
    let exec = body.lines().map(str::trim).find_map(|l| l.strip_prefix("ExecStart="))?;
    let env: Vec<String> = body
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("Environment="))
        .flat_map(split_words)
        .filter(|kv| kv.contains('=') && !kv.starts_with("KILN_JOIN_TOKEN="))
        .collect();
    let words = split_words(exec);
    let (bin, rest) = words.split_first()?;
    let sep = rest.iter().position(|w| w == "--")?;
    let args = rest.get(sep + 1..)?.to_vec();
    (rest.first().map(String::as_str) == Some("supervise") && args.first().map(String::as_str) == Some("agent")).then(|| Existing {
        boot: "systemd".to_owned(),
        system,
        unit: unit.to_owned(),
        bin: PathBuf::from(bin),
        launcher: None,
        args,
        env,
    })
}

/// Read a launchd job kiln wrote (`kiln install --agent` on macOS): the binary (the one
/// `--exe` names when the job starts it from a launcher), what follows `supervise --` in its
/// `ProgramArguments`, and its environment (`KEY=value`), for rewriting the job.
pub fn parse_plist(body: &str, unit: &str, system: bool) -> Option<Existing> {
    let array = body.split("<key>ProgramArguments</key>").nth(1)?.split("</array>").next()?;
    let words: Vec<String> = array.split("<string>").skip(1).filter_map(|s| s.split("</string>").next()).map(xml_unescape).collect();
    let (program, rest) = words.split_first()?;
    let sep = rest.iter().position(|w| w == "--")?;
    let args = rest.get(sep + 1..)?.to_vec();
    let exe = rest.get(..sep).and_then(|opts| arg_value(opts, "--exe"));
    let (bin, launcher) = match exe {
        Some(e) => (PathBuf::from(e), Some(PathBuf::from(program))),
        None => (PathBuf::from(program), None),
    };
    let env = body
        .split("<key>EnvironmentVariables</key>")
        .nth(1)
        .and_then(|e| e.split("</dict>").next())
        .map(|dict| {
            dict.split("<key>")
                .skip(1)
                .filter_map(|kv| {
                    let (k, rest) = kv.split_once("</key>")?;
                    let v = rest.split("<string>").nth(1)?.split("</string>").next()?;
                    Some(format!("{}={}", xml_unescape(k), xml_unescape(v)))
                })
                .filter(|kv| !kv.starts_with("KILN_JOIN_TOKEN="))
                .collect()
        })
        .unwrap_or_default();
    (rest.first().map(String::as_str) == Some("supervise") && args.first().map(String::as_str) == Some("agent")).then(|| Existing {
        boot: "launchd".to_owned(),
        system,
        unit: unit.to_owned(),
        bin,
        launcher,
        args,
        env,
    })
}

pub(crate) fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// The installed agent service (`kiln install --agent`), if any.
pub fn find_existing(unit: &str) -> Option<Existing> {
    if cfg!(target_os = "macos") {
        for system in [false, true] {
            if let Ok(body) = std::fs::read_to_string(crate::install::launchd_plist(system, unit)) {
                return parse_plist(&body, unit, system);
            }
        }
    }
    if cfg!(target_os = "linux") {
        let user = crate::os::config_home().join(format!("systemd/user/{unit}.service"));
        if let Ok(body) = std::fs::read_to_string(&user) {
            return parse_unit(&body, unit, false);
        }
        let system = PathBuf::from(format!("/etc/systemd/system/{unit}.service"));
        if let Ok(body) = std::fs::read_to_string(&system) {
            return parse_unit(&body, unit, true);
        }
    }
    if cfg!(windows) {
        if let Some(task) = crate::install::task_definition(unit) {
            let mut words = vec![task.command];
            words.extend(split_windows_args(&task.arguments));
            return windows_agent(&words, unit, "task");
        }
        // The Startup-folder fallback: `start "" /B <kiln> supervise --detach -- agent …`.
        let script = std::fs::read_to_string(crate::install::startup_script(unit)).ok()?;
        let line = script.lines().find_map(|l| l.trim().strip_prefix("start \"\" /B "))?;
        return windows_agent(&split_windows_args(line), unit, "startup");
    }
    None
}

/// The agent a Windows launcher starts (`<kiln> supervise [--detach] -- agent …`, its words), as
/// [`Existing`] booted by `boot` (`task` or `startup`).
fn windows_agent(words: &[String], unit: &str, boot: &str) -> Option<Existing> {
    let (bin, rest) = words.split_first()?;
    let sep = rest.iter().position(|w| w == "--")?;
    let args = rest.get(sep + 1..)?.to_vec();
    (rest.first().map(String::as_str) == Some("supervise") && args.first().map(String::as_str) == Some("agent")).then(|| Existing {
        boot: boot.to_owned(),
        system: false,
        unit: unit.to_owned(),
        bin: PathBuf::from(bin),
        launcher: None,
        args,
        env: Vec::new(),
    })
}

/// Split a Windows command line into its arguments as `CommandLineToArgvW` does (what
/// `crate::install::win_quote` writes): whitespace separates them outside double quotes,
/// backslashes are literal unless they come before a quote (`2n` of them then are `n`, and the
/// quote quotes; `2n+1` are `n` and a literal quote), and `""` inside quotes is a quote.
fn split_windows_args(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let mut n = 1;
                while chars.next_if_eq(&'\\').is_some() {
                    n += 1;
                }
                if chars.peek() == Some(&'"') {
                    cur.push_str(&"\\".repeat(n / 2));
                    if n % 2 == 1 {
                        chars.next();
                        cur.push('"');
                    }
                } else {
                    cur.push_str(&"\\".repeat(n));
                }
                any = true;
            }
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                cur.push('"');
            }
            '"' => {
                quoted = !quoted;
                any = true;
            }
            ' ' | '\t' if !quoted => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

/// The value of `--flag` (or `--flag=value`) in agent arguments.
pub fn arg_value(args: &[String], flag: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix(flag).and_then(|r| r.strip_prefix('=')) {
            return Some(v.to_owned());
        }
    }
    None
}

/// Instances set up to replace `unit`, newest first.
pub fn replacing(state_dir: &Path, unit: &str) -> Vec<(String, Replaces)> {
    let mut out: Vec<(i64, String, Replaces)> = std::fs::read_dir(state_dir.join("instances"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let id = e.file_name().to_string_lossy().into_owned();
            let r: Replaces = read_json(&e.path().join("replaces.json"))?;
            (r.unit == unit).then(|| (created_at(state_dir, &id), id, r))
        })
        .collect();
    out.sort_by_key(|x| std::cmp::Reverse(x.0));
    out.into_iter().map(|(_, id, r)| (id, r)).collect()
}

/// The unit name of an instance's own service.
pub fn instance_unit(base: &str, id: &str) -> String {
    format!("{base}-{}", id.chars().filter(char::is_ascii_alphanumeric).take(8).collect::<String>())
}

fn service_active(boot: &str, system: bool, unit: &str) -> bool {
    match boot {
        "systemd" => {
            let mut cmd = Command::new("systemctl");
            if !system {
                cmd.arg("--user");
            }
            cmd.args(["is-active", "--quiet", &format!("{unit}.service")]).status().is_ok_and(|s| s.success())
        }
        _ => false,
    }
}

/// `kiln install --agent --upgrade`: run this binary as a new runner beside the installed one.
/// Safe to re-run: an instance already set up is brought up to this binary (in place) instead.
pub async fn install_alongside(unit: &str, say: &(dyn Fn(String) + Sync)) -> anyhow::Result<()> {
    let existing = find_existing(unit).ok_or_else(|| {
        anyhow::anyhow!("no installed kiln agent service ({unit}) found to upgrade beside; install one with `kiln install --agent` first")
    })?;
    // launchd (macOS): the binary the job's runner runs is replaced (never its launcher, which
    // macOS knows the job by), and the runner takes it up as a fresh process once its jobs are
    // done, keeping its terminals; a runner from before launchers has launchd start it afresh,
    // from a launcher, instead.
    if existing.boot == "launchd" {
        copy_atomic(&std::env::current_exe()?, &existing.bin).with_context(|| format!("install {}", existing.bin.display()))?;
        say(format!("runner      {} (taken up once its jobs are done; it keeps its terminals)", existing.bin.display()));
        return Ok(());
    }
    // Windows: the binary the runner's Scheduled Task (or Startup-folder script) starts is
    // replaced, the running one moved aside as Windows allows; its supervisor sees it change and
    // restarts the runner into it.
    if existing.boot == "task" || existing.boot == "startup" {
        crate::install::place_binary(&existing.bin)?;
        say(format!("runner      {} (its supervisor restarts the runner into it)", existing.bin.display()));
        return Ok(());
    }
    let state_dir = arg_value(&existing.args, "--state-dir").map_or_else(|| crate::config::default_data_dir().join("agent"), PathBuf::from);
    let (machine_cert, key) = machine_credentials(&state_dir)?;
    let server = arg_value(&existing.args, "--server").or_else(|| crate::join::stored(&state_dir, "server")).ok_or_else(|| {
        anyhow::anyhow!("cannot tell which server the installed agent uses (no --server in {unit})")
    })?;
    let name = crate::pki::leaf_name(&crate::pki::Leaf { cert_pem: machine_cert.clone(), key_pem: String::new() })
        .ok_or_else(|| anyhow::anyhow!("the machine's certificate names no machine"))?;
    let runner_dir = crate::config::default_data_dir().join("runner");
    let stable_bin = runner_dir.join(if cfg!(windows) { "kiln.exe" } else { "kiln" });
    let exe = std::env::current_exe()?;

    // Already set up: take this binary up in place (the new runner re-executes itself, keeping its
    // terminals) and make sure its service runs.
    if let Some((id, r)) = replacing(&state_dir, unit).into_iter().next() {
        let new_unit = instance_unit(unit, &id);
        copy_atomic(&exe, &stable_bin).with_context(|| format!("install {}", stable_bin.display()))?;
        say(format!("runner      {} (instance {id}, service {new_unit})", stable_bin.display()));
        if !service_active(&existing.boot, existing.system, &new_unit) {
            let mut cmd = Command::new("systemctl");
            if !existing.system {
                cmd.arg("--user");
            }
            let _ = cmd.args(["enable", "--now", &format!("{new_unit}.service")]).status();
        }
        say(if r.retired_at.is_some() {
            format!("status      the old runner ({unit}) is already retired")
        } else {
            format!("status      the old runner ({unit}) keeps what it runs and retires itself once that ends")
        });
        return Ok(());
    }

    // A new instance: its own certificate first (fails fast if the server says no).
    let id = uuid::Uuid::now_v7().to_string();
    let dir = instance_dir(&state_dir, &id);
    std::fs::create_dir_all(&dir)?;
    let ca = std::fs::read_to_string(state_dir.join("ca.pem")).ok();
    let e = fetch_certificate(&server, ca.as_deref(), &name, &machine_cert, &key).await?;
    crate::util::write_secret(&dir.join("agent.pem"), e.cert_pem.as_bytes(), 0o644)?;
    write_json(&dir.join("instance.json"), &InstanceRecord { id: id.clone(), created_at: crate::util::now_unix() })?;
    say(format!("instance    {id} (its own certificate for {name})"));

    // Its binary: a versioned copy, and the stable path its service runs (replaced in place later).
    std::fs::create_dir_all(runner_dir.join("versions"))?;
    let versioned = runner_dir.join("versions").join(format!("kiln-{}-{}", env!("CARGO_PKG_VERSION"), id.get(..8).unwrap_or("x")));
    copy_atomic(&exe, &versioned)?;
    copy_atomic(&exe, &stable_bin)?;
    say(format!("runner      {}", stable_bin.display()));

    write_json(
        &dir.join("replaces.json"),
        &Replaces { boot: existing.boot.clone(), system: existing.system, unit: unit.to_owned(), cli_bin: Some(existing.bin.clone()), retired_at: None },
    )?;

    let new_unit = instance_unit(unit, &id);
    let mut args = vec!["supervise".to_owned(), "--".to_owned()];
    args.extend(existing.args.iter().cloned());
    args.push("--instance".to_owned());
    args.push(id.clone());
    crate::install::start_service(existing.system, &new_unit, &stable_bin, &args, &state_dir, &existing.env)?;
    say(format!("service     {new_unit} started beside {unit}"));
    // Wait until the new runner reached the server (its certificate and connection work).
    let ok = wait_for_connect(&dir, Duration::from_secs(30)).await;
    say(if ok {
        "status      new terminals, agents and jobs go to the new runner; the old one keeps what it runs and retires itself once that ends".to_owned()
    } else {
        format!("status      started; check `journalctl --user -u {new_unit}` if it does not appear in Runners")
    });
    Ok(())
}

/// The new runner writes `connected` into its instance directory once the server welcomed it.
async fn wait_for_connect(dir: &Path, timeout: Duration) -> bool {
    let marker = dir.join("connected");
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if marker.exists() {
            return true;
        }
        crate::rt::sleep(Duration::from_millis(300)).await;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_installed_agent_unit() -> anyhow::Result<()> {
        let body = "[Service]\nType=simple\nEnvironment=KILN_CONFIG=/c.toml\nEnvironment=\"PATH=/a b:/c\"\nEnvironment=KILN_JOIN_TOKEN=secret\nExecStart=/home/u/.local/bin/kiln supervise -- agent --server http://10.0.0.2:30070 --name 'bench box' --labels a,b --quic 10.0.0.2:30071 --slots 8\nRestart=always\n";
        let e = parse_unit(body, "kiln-agent", false).ok_or_else(|| anyhow::anyhow!("not parsed"))?;
        assert_eq!(e.bin, PathBuf::from("/home/u/.local/bin/kiln"));
        assert_eq!(
            e.args,
            ["agent", "--server", "http://10.0.0.2:30070", "--name", "bench box", "--labels", "a,b", "--quic", "10.0.0.2:30071", "--slots", "8"]
        );
        assert_eq!(arg_value(&e.args, "--name").as_deref(), Some("bench box"));
        assert_eq!(e.env, ["KILN_CONFIG=/c.toml", "PATH=/a b:/c"], "the environment carries over, never a join token");
        assert_eq!(arg_value(&["--state-dir=/x".to_owned()], "--state-dir").as_deref(), Some("/x"));
        assert!(parse_unit("ExecStart=/bin/kiln supervise -- server\n", "kiln", false).is_none());
        assert_eq!(instance_unit("kiln-agent", "0199a2b3-c4d5"), "kiln-agent-0199a2b3");
        Ok(())
    }

    #[test]
    fn reads_the_installed_agent_task() -> anyhow::Result<()> {
        // What `kiln install` quotes, `CommandLineToArgvW` (and this) splits back.
        let args = ["supervise", "--detach", "--", "agent", "--name", "bench box", "--labels", "a,b", "--note", r#"say "hi""#, r"C:\Program Files\kiln\", ""].map(str::to_owned);
        let line = args.iter().map(|a| crate::install::win_quote(a)).collect::<Vec<_>>().join(" ");
        assert_eq!(split_windows_args(&line), args);
        // A Scheduled Task's command and arguments.
        let bin = r"C:\Users\me\AppData\Local\Programs\kiln\kiln.exe";
        let mut words = vec![bin.to_owned()];
        words.extend(split_windows_args(&line));
        let e = windows_agent(&words, "kiln-agent", "task").ok_or_else(|| anyhow::anyhow!("not parsed"))?;
        assert_eq!((e.boot.as_str(), e.bin.as_path()), ("task", Path::new(bin)));
        assert_eq!(e.args.get(..3), Some(&["agent".to_owned(), "--name".to_owned(), "bench box".to_owned()][..]));
        // The Startup-folder script's line, after `start "" /B `.
        let startup = format!("{} {line}", crate::install::win_quote(bin));
        assert_eq!(windows_agent(&split_windows_args(&startup), "kiln-agent", "startup").map(|e| e.bin), Some(PathBuf::from(bin)));
        assert!(windows_agent(&split_windows_args(r"C:\kiln.exe supervise -- server"), "kiln", "task").is_none(), "not an agent");
        Ok(())
    }

    #[test]
    fn instance_files_are_its_own() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(own_dir(dir.path(), None), dir.path());
        assert_eq!(own_dir(dir.path(), Some("abc")), dir.path().join("instances").join("abc"));
        assert_eq!(created_at(dir.path(), MAIN), 0);
        let d = instance_dir(dir.path(), "abc");
        std::fs::create_dir_all(&d)?;
        write_json(&d.join("instance.json"), &InstanceRecord { id: "abc".into(), created_at: 42 })?;
        write_json(&d.join("replaces.json"), &Replaces { boot: "systemd".into(), system: false, unit: "kiln-agent".into(), cli_bin: None, retired_at: None })?;
        assert_eq!(created_at(dir.path(), "abc"), 42);
        assert_eq!(replacing(dir.path(), "kiln-agent").len(), 1);
        assert!(replacing(dir.path(), "other").is_empty());
        Ok(())
    }
}
