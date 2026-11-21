//! W9's A/B (Ada's ruling, 2026-11-20): an agent's container on its workspace's Slates volume by the export route (the
//! engine's own NFS volume of Slates' export, [`super::container`]) against the route it replaced (Slates' bind of the
//! machine's mount into the container), with the container's own tmpfs as the control. The same workloads (git's clone,
//! `gc` and `fsck`; a release `cargo build`; an offline `npm install` of local packages; hard links, renames and a delete
//! while open) run in the same image at the same path on each route, the routes interleaved round by round, and each run
//! says how long its workload took and a digest of what it made (`tests/fixtures/container-ab/workload.sh`).
//!
//! **What it holds.** Every route that ran makes the same tree as the container's own disk, workload by workload: digests
//! equal, or the test fails naming the route and workload. A route that can't run (the bind, where the engine never
//! answers over it) is said, with why, and the others still compared. Every run's seconds are printed (`AB <workload>
//! <route> round <i>: <s>`), and a summary of each route's best and median.
//!
//! Gated: `CONTAINER_AB=1`, `KILN_TEST_KILN` (a kiln executable in Slates' roles, release for timings), a container
//! engine whose agents' containers kiln runs, and the images `CONTAINER_AB_RUST` (default `rust:1.99.1-bookworm`) and
//! `CONTAINER_AB_NODE` (default `node:24-bookworm`) already present. `CONTAINER_AB_ROUNDS` sets the rounds (default 5).
//! Its instance and everything it made are ended at its end, whatever happened.
//!
//!   CONTAINER_AB=1 KILN_TEST_KILN=<target>/release/kiln cargo test -p kiln --lib container_ab -- --nocapture

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::slates::Made;

/// How long the whole A/B may take.
const WITHIN: Duration = Duration::from_secs(3 * 3600);
/// How long one workload's container may take before its route is judged not to answer.
const RUN_WITHIN: Duration = Duration::from_secs(900);
/// How long a workload's container on the bind may take before the bind is judged not to answer.
const BIND_WITHIN: Duration = Duration::from_secs(180);
/// The folder every route's workload works in, beneath the workspace's mount point (the same path on every route, so
/// what a build embeds of its path agrees).
const WORK: &str = "ab-work";

/// A route a workload's container runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// The container's own file system (a tmpfs of its own at the same path): the control.
    Local,
    /// Slates' bind of the machine's mount into the container (the route replaced).
    Bind,
    /// The engine's own NFS volume of Slates' export (the route built).
    Export,
}

impl Route {
    const ALL: [Route; 3] = [Route::Local, Route::Bind, Route::Export];

    fn word(self) -> &'static str {
        match self {
            Route::Local => "local",
            Route::Bind => "bind",
            Route::Export => "export",
        }
    }
}

/// The workloads, each with the image it runs in.
fn workloads() -> [(&'static str, String); 4] {
    let rust = std::env::var("CONTAINER_AB_RUST").unwrap_or_else(|_| "rust:1.99.1-bookworm".to_owned());
    let node = std::env::var("CONTAINER_AB_NODE").unwrap_or_else(|_| "node:24-bookworm".to_owned());
    [("git", rust.clone()), ("cargo", rust.clone()), ("npm", node), ("fs", rust)]
}

/// One workload's run: its seconds and its digest, or why it didn't run.
type Ran = Result<(f64, String), String>;

#[test]
fn w9_ab_the_export_route_makes_what_the_containers_own_disk_makes_and_is_timed_against_the_bind() -> anyhow::Result<()> {
    if std::env::var_os("CONTAINER_AB").is_none() {
        eprintln!("skipped: CONTAINER_AB=1 runs the A/B");
        return Ok(());
    }
    let Some(program) = std::env::var_os("KILN_TEST_KILN") else {
        eprintln!("skipped: KILN_TEST_KILN names no kiln executable");
        return Ok(());
    };
    let instance = format!("kilnab{}", std::process::id());
    let o = super::slates::Override { instance, program: Some(program.into()), quick: true };
    anyhow::ensure!(super::slates::set_override(o), "another test pointed workspaces elsewhere");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::rt::block_on(run()));
    });
    match rx.recv_timeout(WITHIN) {
        Ok(r) => r,
        Err(_) => anyhow::bail!("the A/B hung (its Slates instance is left running)"),
    }
}

/// The engine's command line, answering within `within`: its output, or why not.
fn engine(program: &str, args: &[String], within: Duration) -> Result<String, String> {
    let mut child = Command::new(program).args(args).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let until = Instant::now() + within;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(100)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("no answer in {} s", within.as_secs()));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("{}{}", String::from_utf8_lossy(&out.stdout).trim(), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A word's value in a workload's output (`SECS 1.2` → `1.2`).
fn said<'a>(out: &'a str, word: &str) -> Option<&'a str> {
    out.lines().rev().find_map(|l| l.strip_prefix(word).and_then(|r| r.strip_prefix(' ')))
}

/// The inputs every workload reads, made once under `dir`: a bare repository with history, a crate of many modules and
/// no dependencies, an npm app with local packages, and the workload script.
fn inputs(dir: &Path) -> anyhow::Result<PathBuf> {
    let at = dir.join("in");
    let sh = |cmd: &str| -> anyhow::Result<()> {
        let out = Command::new("sh").arg("-c").arg(cmd).current_dir(&at).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output()?;
        anyhow::ensure!(out.status.success(), "{cmd}: {}", String::from_utf8_lossy(&out.stderr));
        Ok(())
    };
    std::fs::create_dir_all(&at)?;
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/container-ab/workload.sh"), at.join("workload.sh"))?;
    sh("git init -q -b main src-repo && cd src-repo && git config user.name Ada && git config user.email ada@example.com && mkdir src && \
        i=0; while [ $i -lt 60 ]; do printf 'line %s\\n' $i >> README.md; printf 'fn f%s() {}\\n' $i > src/m$i.rs; \
        git add -A && GIT_AUTHOR_DATE=@$((1700000000+i)) GIT_COMMITTER_DATE=@$((1700000000+i)) git commit -qm \"c$i\"; i=$((i+1)); done && \
        cd .. && git clone -q --bare src-repo repo.git")?;
    sh("mkdir -p crate/src && printf '[package]\\nname = \"ab\"\\nversion = \"0.1.0\"\\nedition = \"2021\"\\n[profile.release]\\ncodegen-units = 1\\n' > crate/Cargo.toml && \
        i=0; main=''; while [ $i -lt 80 ]; do printf 'pub fn f(x: u64) -> u64 { (0..x).map(|v| v.wrapping_mul(%s) ^ (v >> 3)).fold(0, u64::wrapping_add) }\\n' $i > crate/src/m$i.rs; \
        main=\"$main mod m$i;\"; i=$((i+1)); done && \
        printf '%s\\nfn main() { let mut t = 0u64; ' \"$main\" > crate/src/main.rs && \
        i=0; while [ $i -lt 80 ]; do printf 't = t.wrapping_add(m%s::f(1000)); ' $i >> crate/src/main.rs; i=$((i+1)); done && \
        printf 'println!(\"{}\", t); }\\n' >> crate/src/main.rs && printf '# This file is automatically @generated by Cargo.\\nversion = 4\\n\\n[[package]]\\nname = \"ab\"\\nversion = \"0.1.0\"\\n' > crate/Cargo.lock")?;
    sh("mkdir -p npm/app && i=0; deps=''; while [ $i -lt 20 ]; do mkdir -p npm/p$i && \
        printf '{\"name\":\"p%s\",\"version\":\"1.0.%s\",\"main\":\"index.js\"}\\n' $i $i > npm/p$i/package.json && \
        printf 'module.exports = () => %s;\\n' $i > npm/p$i/index.js && deps=\"$deps\\\"p$i\\\":\\\"file:../p$i\\\",\"; i=$((i+1)); done && \
        printf '{\"name\":\"app\",\"version\":\"1.0.0\",\"dependencies\":{%s}}\\n' \"${deps%,}\" > npm/app/package.json && \
        printf 'let t = 0; for (let i = 0; i < 20; i++) t += require(\"p\" + i)(); console.log(t);\\n' > npm/app/index.js")?;
    Ok(at)
}

/// What the routes were given: the engine, whom it runs as, the workspace, the bind's entry, the export's volume.
struct Routes {
    program: String,
    user: (u32, u32),
    made: Made,
    input: PathBuf,
    bind: Result<String, String>,
    export: Result<String, String>,
}

impl Routes {
    /// The engine's run of `workload` in `image` on `route`: its seconds and digest, or why it didn't run.
    fn run(&self, route: Route, workload: &str, image: &str) -> Ran {
        let target = format!("{}/{WORK}", self.made.mount);
        // The control's tmpfs is executable (a built program runs from it) and anyone's to write; the others' mounts are
        // the routes' own.
        let (flag, mount) = match route {
            Route::Local => ("--tmpfs", format!("{}:rw,exec,mode=1777", self.made.mount)),
            Route::Bind => ("--mount", self.bind.clone()?),
            Route::Export => ("--mount", format!("type=volume,source={},target={},volume-nocopy", self.export.clone()?, self.made.mount)),
        };
        let (uid, gid) = self.user;
        let args: Vec<String> = [
            "run", "--rm", "--network", "none", "--user", &format!("{uid}:{gid}"), flag, &mount, "--mount", &format!("type=bind,source={},target=/in,readonly", self.input.display()),
            "--entrypoint", "sh", image, "/in/workload.sh", workload, &target,
        ]
        .map(str::to_owned)
        .to_vec();
        // The bind hung Docker Desktop before (2026-11-20, 36 minutes): it's given three.
        let within = if route == Route::Bind { BIND_WITHIN } else { RUN_WITHIN };
        let out = engine(&self.program, &args, within)?;
        let secs = said(&out, "SECS").and_then(|s| s.parse::<f64>().ok()).ok_or_else(|| format!("no SECS: {out}"))?;
        let digest = said(&out, "DIGEST").ok_or_else(|| format!("no DIGEST: {out}"))?.to_owned();
        let silly = said(&out, "SILLY").map(|s| format!(" silly={s}")).unwrap_or_default();
        Ok((secs, format!("{digest}{silly}")))
    }
}

async fn run() -> anyhow::Result<()> {
    let program = crate::runner::docker::find().ok_or_else(|| anyhow::anyhow!("no container engine answers here"))?;
    let judged = crate::rt::blocking({
        let p = program.clone();
        move || slates_api::container_engine::export_engine(&p)
    })
    .await?
    .map_err(|f| anyhow::anyhow!("Slates refused the engine: {f}"))?;
    eprintln!("AB engine: {:?} {} at {}", judged.profile.engine, judged.profile.server_version, judged.endpoint);
    let dir = tempfile::tempdir_in(dirs_home()?)?;
    // Its workspaces under the person's home too, so the bind's source lies in what Docker Desktop shares.
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state)?;
    super::slates::init(&state);
    let input = inputs(dir.path())?;
    let over = dir.path().join("project");
    std::fs::create_dir_all(&over)?;
    std::fs::write(over.join("README.md"), "the project\n")?;
    let made = super::slates::make(&format!("kiln-ab{}", std::process::id()), &over).await?;
    let r = ab(&program, judged.profile.engine, made.clone(), input).await;
    // Taken down as a container's workspace is: Docker Desktop's VM holds what a bind touched past the container, so a
    // mount that stays busy is let go of (`remove_after_container`). Its instance is stopped only once no mount stays.
    match super::slates::remove_after_container(&made).await {
        Ok(()) => {
            if let Some(pid) = super::slates::started_anchor() {
                let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).output();
            }
        }
        Err(e) => eprintln!("AB: the workspace's take-down: {e:#} (its instance is left running beneath its mount)"),
    }
    r
}

/// A folder under the person's home (Docker Desktop shares `/Users` by default; the default temporary folder lies in
/// `/private/var/folders`).
fn dirs_home() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("no HOME"))?;
    let at = PathBuf::from(home).join(".cache").join("kiln-container-ab");
    std::fs::create_dir_all(&at)?;
    Ok(at)
}

async fn ab(program: &str, engine: slates_api::container_engine::Engine, made: Made, input: PathBuf) -> anyhow::Result<()> {
    let user = super::held::owner(Path::new(&made.mount))?;
    // The bind, as the replaced route made it: Slates' bind, its source checked, its entry as Docker's `--mount` says it.
    let volume = super::slates::volume_id(&made.volume)?;
    let point = made.mount.clone();
    let bound = super::slates::call("container bind", move |i| i.bind_into_container(volume, &point, &point, slates_api::Intent::Write)).await;
    let (bind, bind_attachment) = match bound {
        Ok(b) => {
            let checked = { let b = b.clone(); crate::rt::blocking(move || b.check_source()).await? };
            let entry = format!("type=bind,source={},target={},bind-recursive=disabled,bind-propagation=private", b.binding.source, b.binding.destination);
            (checked.map(|()| entry).map_err(|f| f.to_string()), Some(b.attachment))
        }
        Err(e) => (Err(format!("Slates refused the bind: {e:#}")), None),
    };
    // The export, as the built route makes it.
    let exported = super::slates::export(&made, user.0, false).await?;
    let port = super::slates::call("status", move |i| i.status(volume)).await?.nfs_port.ok_or_else(|| anyhow::anyhow!("no NFS port"))?;
    let host = if engine == slates_api::container_engine::Engine::DockerDesktop { "host.docker.internal" } else { "127.0.0.1" };
    let name = format!("{}-ab", made.name);
    let create: Vec<String> = ["volume", "create", "--driver", "local", "--opt", "type=nfs", "--opt", &format!("o=addr={host},port={port},nfsvers=4.2,proto=tcp,hard,nosuid,nodev"), "--opt", &format!("device=:{}", exported.path), &name]
        .map(str::to_owned)
        .to_vec();
    let export = engine_run(program, &create).map(|_| name.clone());
    let routes = Routes { program: program.to_owned(), user, made: made.clone(), input, bind, export };
    let rounds: usize = std::env::var("CONTAINER_AB_ROUNDS").ok().and_then(|n| n.parse().ok()).unwrap_or(5);
    let mut results: Vec<(String, Route, usize, Ran)> = Vec::new();
    for (workload, image) in workloads() {
        for round in 0..rounds {
            // Interleaved: each round starts on another route.
            for route in Route::ALL.iter().copied().cycle().skip(round % Route::ALL.len()).take(Route::ALL.len()) {
                // A route that didn't answer once isn't asked again.
                if results.iter().any(|(w, r, _, ran)| w == workload && *r == route && ran.as_ref().is_err_and(|e| e.starts_with("no answer"))) {
                    continue;
                }
                let ran = routes.run(route, workload, &image);
                match &ran {
                    Ok((secs, digest)) => eprintln!("AB {workload} {} round {round}: {secs:.3} s digest {digest}", route.word()),
                    Err(why) => eprintln!("AB {workload} {} round {round}: DIDN'T RUN: {}", route.word(), super::container::without_token(why, &exported.path)),
                }
                results.push((workload.to_owned(), route, round, ran));
            }
        }
    }
    let _ = engine_run(program, &["volume", "rm", "--force", &name].map(str::to_owned));
    let _ = super::slates::detach(exported.attachment).await;
    if let Some(a) = bind_attachment {
        let _ = super::slates::detach(a).await;
    }
    // The summary, and the check: every route that ran made what the control made.
    let mut wrong = Vec::new();
    for (workload, _) in workloads() {
        let control: Vec<&String> = results.iter().filter(|(w, r, _, _)| w == workload && *r == Route::Local).filter_map(|(_, _, _, ran)| ran.as_ref().ok().map(|(_, d)| d)).collect();
        for route in Route::ALL {
            let ran: Vec<&(f64, String)> = results.iter().filter(|(w, r, _, _)| w == workload && *r == route).filter_map(|(_, _, _, ran)| ran.as_ref().ok()).collect();
            let mut secs: Vec<f64> = ran.iter().map(|(s, _)| *s).collect();
            secs.sort_by(f64::total_cmp);
            let all: Vec<String> = secs.iter().map(|s| format!("{s:.3}")).collect();
            eprintln!("AB SUMMARY {workload} {}: ran {}/{rounds}, best {}, median {}, all [{}]", route.word(), ran.len(), all.first().cloned().unwrap_or_else(|| "-".into()), all.get(all.len() / 2).cloned().unwrap_or_else(|| "-".into()), all.join(", "));
            for (_, digest) in &ran {
                let base = digest.split(' ').next().unwrap_or_default();
                if control.iter().any(|c| c.split(' ').next() != Some(base)) || control.is_empty() {
                    wrong.push(format!("{workload} on {}: {digest} vs the control's {control:?}", route.word()));
                }
            }
        }
    }
    anyhow::ensure!(wrong.is_empty(), "routes made other trees than the container's own disk: {wrong:#?}");
    anyhow::ensure!(results.iter().any(|(_, r, _, ran)| *r == Route::Export && ran.is_ok()), "the export route never ran");
    Ok(())
}

/// The engine's command, answering within [`RUN_WITHIN`].
fn engine_run(program: &str, args: &[String]) -> Result<String, String> {
    engine(program, args, RUN_WITHIN)
}
