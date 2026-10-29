//! A runner's legs and its tool facts have what its user has (docs/ci.md W13-R01, W13-R03). A
//! runner's service keeps the PATH of the terminal it was installed from, so a toolchain installed
//! since in a folder that PATH lacks (node from nvm, cargo from rustup: a Mac's launchd service
//! whose legs said `npm: command not found`) is on its user's PATH alone, as their login shell has
//! it: a leg's steps run it, after the runner's own folders, and the runner's facts list it.
//!
//! `cargo test -p kiln --test runner_path`

#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, http, start};
use serde_json::{Value, json};

/// The runner's own PATH: a service's, with none of its user's toolchains on it.
const RUNNER_PATH: &str = "/usr/bin:/bin";

/// `path`, executable, saying `text`.
fn script(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// A login shell for `$SHELL` whose PATH has `dir/login-bin` before the runner's folders, as a
/// user's startup files put nvm's and rustup's: it runs the command it's given (`-l -i -c CMD`).
fn login_shell(dir: &Path) -> anyhow::Result<String> {
    let shell = dir.join("login-shell");
    script(&shell, &format!("#!/bin/sh\nPATH='{}:{RUNNER_PATH}'\nexport PATH\nwhile [ $# -gt 1 ]; do shift; done\nexec /bin/sh -c \"$1\"\n", dir.join("login-bin").display()))?;
    Ok(shell.to_string_lossy().into_owned())
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("git").current_dir(dir).args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "init.defaultBranch=main"]).args(args).output()?;
    anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(())
}

/// W13-R03: a leg's step runs a tool only its runner's user's login shell has on PATH, and its
/// PATH has the runner's own folders first.
#[test]
fn w13_r03_a_legs_steps_find_a_tool_on_its_users_path_alone() -> anyhow::Result<()> {
    if !Path::new("/usr/bin/git").is_file() {
        eprintln!("skipped: no /usr/bin/git for a runner whose PATH is {RUNNER_PATH}");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let (repo, home) = (dir.path().join("repo"), dir.path().join("home"));
    let login_bin = dir.path().join("login-bin");
    script(&login_bin.join("kiln-login-tool"), "#!/bin/sh\necho \"the user's own tool ran\"\n")?;
    let shell = login_shell(dir.path())?;
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::create_dir_all(&home)?;
    std::fs::write(repo.join(".kiln/path.yaml"), "name: path\non: push\njobs:\n  tool:\n    runs-on: self-hosted\n    steps:\n      - run: |\n          kiln-login-tool\n          echo \"leg-path=$PATH\"\n")?;
    git(&repo, &["init", "-q"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "init"])?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kiln"));
    cmd.current_dir(&repo).args(["workflow", "run", "path"]).env("HOME", &home).env("XDG_DATA_HOME", home.join("data")).env("XDG_CONFIG_HOME", home.join("config")).env("PATH", RUNNER_PATH).env("SHELL", &shell);
    for var in ["KILN_CONFIG", "KILN_URL", "KILN_ADMIN_TOKEN", "KILN_SUPERVISED", "KILN_GENERATION", "KILN_LOG", "KILN_CONTAINER_NETWORK", "KILN_REACH_SOCK"] {
        cmd.env_remove(var);
    }
    let o = cmd.output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(out.contains("the user's own tool ran"), "{out}");
    let path = out.lines().find_map(|l| l.split_once("leg-path=").map(|(_, p)| p.trim().to_owned())).ok_or_else(|| anyhow::anyhow!("no leg-path line in:\n{out}"))?;
    let dirs: Vec<&str> = path.split(':').collect();
    let at = |d: &str| dirs.iter().position(|x| *x == d);
    let (own, users) = (at("/usr/bin"), at(&login_bin.to_string_lossy()));
    assert!(matches!((own, users), (Some(o), Some(u)) if o < u), "the runner's folders, then its user's: {path}");
    Ok(())
}

/// W13-R01: the runner's facts list a tool only its user's login shell has on PATH, with the
/// version it says.
#[test]
fn w13_r01_a_tool_on_its_users_path_alone_is_in_the_runners_facts() -> anyhow::Result<()> {
    // A tool the probes look for that the runner's own PATH hasn't got.
    let Some(tool) = ["shards", "dotnet", "firecracker"].into_iter().find(|t| !RUNNER_PATH.split(':').any(|d| Path::new(d).join(t).exists())) else {
        eprintln!("skipped: every candidate tool is on {RUNNER_PATH}");
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    script(&dir.path().join("login-bin").join(tool), &format!("#!/bin/sh\necho '{tool} 9.8.7'\n"))?;
    let shell = login_shell(dir.path())?;
    let (_runner, log) = agents::runner_env(dir.path(), s.port, "box", None, Talk { lines: 1, every_ms: 20 }, &[("PATH", RUNNER_PATH), ("SHELL", &shell)])?;
    agents::connected(s.port, &s.admin, "box", &log, Duration::from_secs(120))?;

    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let (_, body) = http(s.port, "POST", "/api/agents/box/probe", &s.admin, Some(&json!({})))?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let tools = v.pointer("/facts/tools").cloned().unwrap_or_default();
        if tools.get(tool).and_then(Value::as_str) == Some("9.8.7") {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "box's facts never listed {tool}: {body}\n{}", std::fs::read_to_string(&log).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(500));
    }
}
