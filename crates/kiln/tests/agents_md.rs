//! AGENTS.md and CLAUDE.md at the repository's root: what every coding agent reads before it
//! changes kiln. CLAUDE.md only imports AGENTS.md (Claude Code reads AGENTS.md on its own only
//! from v2.1.277, and only where no CLAUDE.md is), AGENTS.md stays short enough to be followed,
//! names the critical-path master plan its CP-* ids come from, every repository path it names is
//! there, and it says how the agent that owns a change composes the change's gate from the steps
//! `ci/gate.sh` runs alone, run side by side by `ci/gates.sh`; both fail closed (W05-R03).

use std::path::{Path, PathBuf};

/// The critical-path master plan (a kiln plan) that CP-* ids come from.
const MASTER_PLAN: &str = "01a157fddb8276a2996246ea931c4808";

/// Where a backticked word names a path in this repository.
const ROOTS: &[&str] = &["crates/", "docs/", "ui/", "vendor/", "scripts/", ".kiln/", "bench/", "ci/", "deploy/", "Cargo.toml", "AGENTS.md", "CLAUDE.md", "README.md"];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The repository paths `text` names in backticks that aren't there under `root`.
fn missing_paths(text: &str, root: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    let prose: String = text.lines().filter(|l| !l.trim_start().starts_with("```")).collect::<Vec<_>>().join("\n");
    for (i, span) in prose.split('`').enumerate() {
        if i % 2 == 0 {
            continue;
        }
        for word in span.split_whitespace() {
            let word = word.trim_end_matches([':', ',', ')', '.']);
            if ROOTS.iter().any(|r| word.starts_with(r)) && !root.join(word).exists() && !missing.iter().any(|m| m == word) {
                missing.push(word.to_owned());
            }
        }
    }
    missing
}

#[test]
fn agents_md_routes_agents_and_names_only_real_paths() -> anyhow::Result<()> {
    let root = root();
    let claude = std::fs::read_to_string(root.join("CLAUDE.md"))?;
    assert_eq!(claude.trim(), "@AGENTS.md", "CLAUDE.md should only import AGENTS.md");

    let agents = std::fs::read_to_string(root.join("AGENTS.md"))?;
    let lines = agents.lines().count();
    assert!(lines <= 200, "AGENTS.md is {lines} lines; keep it under 200 so agents follow it");
    assert!(agents.contains("docs/ci.md") && agents.contains("§0.3"), "AGENTS.md should send agents to docs/ci.md §0.3");
    assert!(agents.contains(MASTER_PLAN), "AGENTS.md should name the critical-path master plan {MASTER_PLAN}");
    for hazard in ["restart, swap, upgrade", "adacachy", "~/.cache", "pkill -f", "cargo fmt", "append-only", "secret", "person's word", "git stash"] {
        assert!(agents.contains(hazard), "AGENTS.md no longer warns about {hazard:?}");
    }
    let missing = missing_paths(&agents, &root);
    assert!(missing.is_empty(), "AGENTS.md names paths that aren't in the repository: {missing:?}");
    Ok(())
}

/// A run of a change's gate as AGENTS.md gives it: what it asks kiln for, and `ci/gate.sh`'s
/// arguments.
#[derive(Debug)]
struct GateRun {
    needs: String,
    args: Vec<String>,
}

/// The runs AGENTS.md composes a change's gate from: each `--needs` of its `ci/gates.sh` recipe (the
/// recipe's lines joined where they end in `\`), with the `ci/gate.sh` arguments after it.
fn gate_runs(agents: &str) -> Vec<GateRun> {
    let mut recipe = String::new();
    for line in agents.lines().map(str::trim).skip_while(|l| !l.starts_with("ci/gates.sh ")) {
        let (part, more) = line.strip_suffix('\\').map_or((line, false), |part| (part, true));
        recipe.push_str(part);
        recipe.push(' ');
        if !more {
            break;
        }
    }
    recipe
        .split("--needs ")
        .skip(1)
        .filter_map(|run| {
            let mut words = run.split_whitespace();
            let needs = words.next()?;
            Some(GateRun { needs: needs.to_owned(), args: words.map(str::to_owned).collect() })
        })
        .collect()
}

/// What `ci/gate.sh` with `args` says it would do (`--dry-run`), given `needs` as `kiln run --needs`
/// gives them (none: as if run by hand). It's bash: Unix alone.
fn dry_run(root: &Path, args: &[&str], needs: Option<&str>) -> anyhow::Result<String> {
    let mut gate = std::process::Command::new("bash");
    gate.arg(root.join("ci/gate.sh")).args(args).arg("--dry-run").env_remove("KILN_NEEDS");
    if let Some(needs) = needs {
        gate.env("KILN_NEEDS", needs);
    }
    let out = gate.output()?;
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    anyhow::ensure!(out.status.success(), "ci/gate.sh {args:?} --dry-run failed: {said}{}", String::from_utf8_lossy(&out.stderr));
    Ok(said)
}

/// `ci/gate.sh` run for real with `args` and `env` (W05-R03): its runs' folders under `tmp`, the
/// stubs in `tmp`/bin first on PATH, and cargo from `tmp`/cargo/bin, as the script puts CARGO_HOME's
/// bin before the rest; no needs, and no guard but what `args` give it. How it exited, and what it
/// said on stdout and on stderr.
#[cfg(unix)]
fn gate(args: &[&str], tmp: &Path, env: &[(&str, &str)]) -> anyhow::Result<(Option<i32>, String, String)> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let out = std::process::Command::new("bash")
        .arg(root().join("ci/gate.sh"))
        .args(args)
        .env_remove("KILN_NEEDS")
        .env_remove("KILN_GATE_GUARD_URL")
        .env_remove("KILN_GATE_GUARD_CONTAINERS")
        .env("KILN_GATE_DIR", tmp.join("gate"))
        .env("CARGO_HOME", tmp.join("cargo"))
        .env("STUB_DIR", tmp)
        .env("PATH", std::env::join_paths(std::iter::once(tmp.join("bin")).chain(std::env::split_paths(&path)))?)
        .envs(env.iter().copied())
        .output()?;
    Ok((out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned()))
}

/// A program for `ci/gate.sh` to find before the real one: `script`, run by sh, as `dir`/`name`.
#[cfg(unix)]
fn stub(dir: &Path, name: &str, script: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{script}"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// systemd-run --wait --pipe, as a stub: it runs the command after its options, notes its PID for
/// the stub systemctl to stop, and says how it ended as systemd-run does; with STUB_UNIT=nothing it
/// runs nothing and exits 0, as a unit stopped from outside does.
#[cfg(unix)]
const SYSTEMD_RUN: &str = r##"while [ $# -gt 0 ]; do
  case $1 in --unit | -p) shift 2 ;; --*) shift ;; *) break ;; esac
done
if [ "${STUB_UNIT:-}" = nothing ]; then
  echo "Finished with result: success" >&2
  echo "Main processes terminated with: code=killed/status=TERM" >&2
  exit 0
fi
"$@" &
echo $! > "$STUB_DIR/unit.pid"
wait $!
code=$?
if [ $code -eq 0 ]; then echo "Finished with result: success" >&2; else echo "Finished with result: exit-code" >&2; fi
exit $code
"##;

/// systemctl, as a stub: a user manager is there, and stopping the unit stops its command.
#[cfg(unix)]
const SYSTEMCTL: &str = r##"case "$*" in
  *show-environment*) exit 0 ;;
  *stop*) kill -TERM "$(cat "$STUB_DIR/unit.pid")" 2> /dev/null ;;
esac
exit 0
"##;

/// The agent that owns a change composes its gate (W05-R03): AGENTS.md gives a run per platform,
/// Linux, macOS and Windows, each through `kiln run --needs` with the CPUs and memory its steps need,
/// which it leaves to the agent to measure (no picked number, and not one Linux run of 12 CPUs and
/// 40 GiB), scoped to the change, sends work to no machine by name, and runs them with
/// `ci/gates.sh`, which waits for each: never detached, nor left to a bare `wait`, both of which
/// exit 0 whatever the runs said. `ci/gate.sh`, given each run's needs, says what it was asked,
/// holds itself to it and lists that run's steps alone, natively, before it runs any.
#[test]
fn a_change_s_gate_is_a_run_per_platform_each_asked_for_by_what_it_needs() -> anyhow::Result<()> {
    let root = root();
    let agents = std::fs::read_to_string(root.join("AGENTS.md"))?;
    for name in ["fedora", "jupiter", "--on ", "`on:`"] {
        assert!(!agents.contains(name), "AGENTS.md sends work to a machine by name ({name:?})");
    }
    assert!(!agents.contains("cpus=12,mem=40g"), "AGENTS.md still asks for the gate as one run of 12 CPUs and 40 GiB");
    assert!(
        !agents.lines().any(|l| l.trim() == "wait" || (l.contains("kiln run --needs") && l.contains("--detach"))),
        "AGENTS.md's gate still detaches its runs, or leaves them to a bare wait"
    );
    let runs = gate_runs(&agents);
    for platform in ["linux", "macos", "windows"] {
        assert!(runs.iter().any(|r| r.needs.split(',').any(|w| w.starts_with(platform))), "AGENTS.md's ci/gates.sh composes no gate run on {platform}: {runs:?}");
    }
    for run in &runs {
        for key in ["cpus=", "mem="] {
            let asked = run.needs.split(',').find_map(|w| w.strip_prefix(key));
            assert!(asked.is_some_and(|v| v.starts_with('<')), "a gate run asks for {key} by what its steps were measured to need, never a number AGENTS.md picks: {run:?}");
        }
        assert!(run.args.iter().any(|a| a == "--since" || a == "-p"), "a gate run is scoped to the crates of the change: {run:?}");
        assert!(!run.args.iter().any(|a| a == "--detach" || a == "--dry-run"), "a gate run is waited for, and runs: {run:?}");
        if !cfg!(unix) {
            continue;
        }
        // What an agent measured, say; and scoped to kiln here: --since reads the change's
        // history, with git and cargo.
        let (cpus, mem) = (3, 9);
        let needs = run.needs.split(',').map(|w| if w.starts_with("cpus=") { format!("cpus={cpus}") } else if w.starts_with("mem=") { format!("mem={mem}g") } else { w.to_owned() }).collect::<Vec<_>>().join(",");
        let mut args = Vec::new();
        let mut given = run.args.iter();
        while let Some(a) = given.next() {
            if a == "--since" {
                given.next();
                args.extend(["-p", "kiln"]);
            } else {
                args.push(a.as_str());
            }
        }
        let said = dry_run(&root, &args, Some(&needs))?;
        for line in [
            format!("asked: {cpus} CPUs, {mem}G of memory (from KILN_NEEDS={needs})"),
            format!("caps: MemoryMax={mem}G CPUWeight=1 Nice=19"),
            format!("cargo: -j {cpus}, {cpus} test threads"),
            "natively on ".to_owned(),
        ] {
            assert!(said.contains(&line), "ci/gate.sh {args:?} --dry-run doesn't say {line:?}:\n{said}");
        }
        let named: Vec<&str> = ["clippy", "test"].into_iter().filter(|s| args.contains(s)).collect();
        for (step, line) in [("clippy", "clippy: cargo clippy --locked -p kiln --all-targets"), ("test", "test: cargo test --locked -p kiln --lib --no-run")] {
            let runs_it = named.is_empty() || named.contains(&step);
            assert_eq!(said.contains(line), runs_it, "ci/gate.sh {args:?} runs {step} only when it's named, or no step is:\n{said}");
        }
        assert!(!said.contains("zigbuild") && !said.contains("--target "), "ci/gate.sh cross-compiles instead of checking natively:\n{said}");
    }
    Ok(())
}

/// `ci/gate.sh` runs any one of its steps alone, on the crates named, or every step on the whole
/// workspace, held to the CPUs and memory it was asked for; it has no size of its own (W05-R03), and
/// a dry run asked none says so.
#[test]
fn ci_gate_sh_runs_one_step_alone_on_the_crates_named() -> anyhow::Result<()> {
    if !cfg!(unix) {
        return Ok(());
    }
    let root = root();
    let clippy = dry_run(&root, &["clippy", "-p", "kiln-ci", "--cpus", "2", "--mem", "6g"], None)?;
    for line in [
        "asked: 2 CPUs, 6G of memory (from --cpus and --mem)",
        "caps: MemoryMax=6G CPUWeight=1 Nice=19",
        "cargo: -j 2, 2 test threads",
        "scope: kiln-ci\n",
        "1. clippy: cargo clippy --locked -p kiln-ci --all-targets -j 2 -- -D warnings\n",
    ] {
        assert!(clippy.contains(line), "ci/gate.sh clippy -p kiln-ci doesn't say {line:?}:\n{clippy}");
    }
    assert!(!clippy.contains("cargo test"), "ci/gate.sh clippy runs tests too:\n{clippy}");
    let test = dry_run(&root, &["test", "-p", "kiln-ci", "-p", "kiln-cd", "--cpus", "2", "--mem", "6g"], None)?;
    for line in ["scope: kiln-cd kiln-ci\n", "1. test: cargo test --locked -p kiln-cd -p kiln-ci --lib --no-run -j 2, then cargo test --locked -p kiln-cd -p kiln-ci --no-fail-fast -j 2"] {
        assert!(test.contains(line), "ci/gate.sh test -p kiln-ci -p kiln-cd doesn't say {line:?}:\n{test}");
    }
    assert!(!test.contains("cargo clippy"), "ci/gate.sh test runs clippy too:\n{test}");
    let all = dry_run(&root, &[], None)?;
    for line in [
        "asked: not the CPUs (cpus=) and the memory (mem=)",
        "scope: the workspace, every crate\n",
        "1. clippy: cargo clippy --locked --workspace --all-targets -j <N>",
        "2. test: cargo test --locked --workspace --lib --no-run",
    ] {
        assert!(all.contains(line), "ci/gate.sh with no step, scope or size doesn't say {line:?}:\n{all}");
    }
    Ok(())
}

/// Asked no size, it runs nothing (W05-R03): no CPUs, or no memory, and it exits 2 before any step.
#[cfg(unix)]
#[test]
fn w05_r03_asked_no_size_it_runs_nothing() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let cases: [&[&str]; 3] = [&["clippy", "-p", "kiln-ci"], &["clippy", "-p", "kiln-ci", "--cpus", "2"], &["clippy", "-p", "kiln-ci", "--mem", "6g"]];
    for args in cases {
        let (code, said, err) = gate(args, tmp.path(), &[])?;
        assert_eq!(code, Some(2), "ci/gate.sh {args:?}, asked no size, exits 2:\n{said}{err}");
        assert!(err.contains("it has no size of its own, and runs nothing"), "ci/gate.sh {args:?} says why:\n{err}");
        assert!(!said.contains(":::") && !tmp.path().join("gate/runs").exists(), "ci/gate.sh {args:?} ran something:\n{said}");
    }
    Ok(())
}

/// An empty crate scopes nothing, and is refused (W05-R03): `-p ""` once gave an empty scope that
/// passed. So is any option's empty value.
#[cfg(unix)]
#[test]
fn w05_r03_an_empty_crate_is_refused() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let cases: [&[&str]; 4] = [&["-p", ""], &["clippy", "-p", "", "--cpus", "1", "--mem", "1g"], &["--package", "", "--dry-run"], &["--since", "", "--dry-run"]];
    for args in cases {
        let (code, said, err) = gate(args, tmp.path(), &[])?;
        assert_eq!(code, Some(2), "ci/gate.sh {args:?} refuses an empty value:\n{said}{err}");
        assert!(err.contains("takes a value, not an empty one"), "ci/gate.sh {args:?} says why:\n{err}");
        assert!(!said.contains("GATE: PASS"), "ci/gate.sh {args:?} passed:\n{said}");
    }
    Ok(())
}

/// A step that fails fails the gate (W05-R03): exit 1, and GATE: FAIL naming the step. No systemd
/// user manager here, whatever the machine has (systemctl is a stub that finds none): the steps run
/// as the script's child; cargo is a stub whose clippy fails.
#[cfg(unix)]
#[test]
fn w05_r03_a_failing_step_fails_the_gate() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    stub(&tmp.path().join("bin"), "systemctl", "exit 1\n")?;
    stub(&tmp.path().join("cargo/bin"), "cargo", "case $1 in clippy) echo 'error: a lint fired'; exit 101 ;; esac\nexit 0\n")?;
    let (code, said, err) = gate(&["clippy", "-p", "kiln-ci", "--cpus", "1", "--mem", "1g"], tmp.path(), &[])?;
    assert_eq!(code, Some(1), "a failing step exits 1:\n{said}{err}");
    assert!(said.lines().next_back().is_some_and(|l| l.starts_with("GATE: FAIL (clippy")), "a failing step ends GATE: FAIL (clippy …):\n{said}");
    assert!(said.contains("| error: a lint fired"), "it shows what failed:\n{said}");
    Ok(())
}

/// The verdict is what the steps wrote once every one had ended, never an exit code alone
/// (W05-R03): a unit stopped from outside, or one whose bash SIGHUP or SIGPIPE ended, has
/// systemd-run exit 0 without the steps' verdict, and the gate fails, saying how the unit ended.
/// systemd-run is a stub that runs nothing and exits 0.
#[cfg(unix)]
#[test]
fn w05_r03_a_run_that_ends_without_its_steps_verdict_fails() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    stub(&tmp.path().join("bin"), "systemd-run", SYSTEMD_RUN)?;
    stub(&tmp.path().join("bin"), "systemctl", SYSTEMCTL)?;
    stub(&tmp.path().join("cargo/bin"), "cargo", "exit 0\n")?;
    let (code, said, err) = gate(&["clippy", "-p", "kiln-ci", "--cpus", "1", "--mem", "1g", "--io", "off"], tmp.path(), &[("STUB_UNIT", "nothing")])?;
    assert_eq!(code, Some(1), "a run without its steps' verdict exits 1:\n{said}{err}");
    assert!(
        said.lines().next_back().is_some_and(|l| l.starts_with("GATE: FAIL (it ended before its steps said: exit 0, unit result success")),
        "a run without its steps' verdict fails, saying how its unit ended:\n{said}"
    );
    Ok(())
}

/// A guard stop yields (W05-R03): with the machine strained as many checks running as asked, the run
/// stops and says GATE: YIELDED, exit 75, which tells "run it later, or elsewhere" from "the code is
/// wrong". systemd-run and systemctl are stubs (a unit that runs its command; its stop), as are curl
/// (--guard-url answering in 9.9 s) and cargo (a step that runs until it's stopped, two minutes at
/// most). Its one check comes after 10 s, avg10's window.
#[cfg(unix)]
#[test]
fn w05_r03_a_guard_stop_yields_with_75() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let bin = tmp.path().join("bin");
    stub(&bin, "systemd-run", SYSTEMD_RUN)?;
    stub(&bin, "systemctl", SYSTEMCTL)?;
    stub(&bin, "curl", "printf 9.9\n")?;
    stub(&tmp.path().join("cargo/bin"), "cargo", "n=0\nwhile kill -0 $PPID 2> /dev/null && [ $n -lt 120 ]; do sleep 1; n=$((n + 1)); done\n")?;
    let args = ["clippy", "-p", "kiln-ci", "--cpus", "1", "--mem", "1g", "--io", "off", "--guard-url", "http://127.0.0.1:9/", "--guard-checks", "1"];
    let (code, said, err) = gate(&args, tmp.path(), &[])?;
    assert_eq!(code, Some(75), "a guard stop exits 75:\n{said}{err}");
    assert!(said.lines().next_back().is_some_and(|l| l.starts_with("GATE: YIELDED (")), "a guard stop ends GATE: YIELDED:\n{said}");
    Ok(())
}

/// `ci/gates.sh` passes only when every run exited 0 and said GATE: PASS (W05-R03): a run whose
/// client exits 0 with no verdict (as a stopped `--detach` client does), or 0 saying GATE: FAIL,
/// fails it; one that yielded, none failing, makes it exit 75; and it never detaches a run. kiln is
/// a stub that answers by a word in each run's needs.
#[cfg(unix)]
#[test]
fn w05_r03_ci_gates_sh_passes_only_when_every_run_passed() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let bin = tmp.path().join("bin");
    stub(
        &bin,
        "kiln",
        r##"echo "$*" >> "$STUB_DIR/kiln.calls"
case $3 in
  *pass*) echo "GATE: PASS"; exit 0 ;;
  *silent*) echo "[kiln] left running on its runner (--detach)"; exit 0 ;;
  *liar*) echo "GATE: FAIL (test)"; exit 0 ;;
  *fail*) echo "GATE: FAIL (clippy)"; exit 1 ;;
  *yield*) echo "GATE: YIELDED (strained)"; exit 75 ;;
esac
exit 3
"##,
    )?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path)))?;
    let cases: [(&[&str], i32, &str); 5] = [
        (&["--needs", "linux/amd64,pass", "--since", "origin/main", "--needs", "macos,pass", "clippy", "--since", "origin/main"], 0, "GATE: PASS"),
        (&["--needs", "linux/amd64,pass", "-p", "kiln", "--needs", "macos,silent", "clippy", "-p", "kiln"], 1, "GATE: FAIL"),
        (&["--needs", "linux/amd64,pass", "-p", "kiln", "--needs", "macos,liar", "-p", "kiln"], 1, "GATE: FAIL"),
        (&["--needs", "linux/amd64,pass", "-p", "kiln", "--needs", "windows,yield", "clippy", "-p", "kiln"], 75, "GATE: YIELDED"),
        (&["--needs", "linux/amd64,fail", "-p", "kiln", "--needs", "windows,yield", "clippy", "-p", "kiln"], 1, "GATE: FAIL"),
    ];
    for (args, code, verdict) in cases {
        let out = std::process::Command::new("bash")
            .arg(root().join("ci/gates.sh"))
            .args(args)
            .env("PATH", &path)
            .env("STUB_DIR", tmp.path())
            .env("XDG_CACHE_HOME", tmp.path().join("cache"))
            .output()?;
        let said = String::from_utf8_lossy(&out.stdout);
        assert_eq!(out.status.code(), Some(code), "ci/gates.sh {args:?} exits {code}:\n{said}");
        assert!(said.lines().next_back().is_some_and(|l| l.starts_with(verdict)), "ci/gates.sh {args:?} ends {verdict:?}:\n{said}");
    }
    let calls = std::fs::read_to_string(tmp.path().join("kiln.calls"))?;
    assert!(calls.lines().any(|l| l == "run --needs macos,pass -- ci/gate.sh clippy --since origin/main"), "ci/gates.sh runs each through kiln run --needs:\n{calls}");
    assert!(!calls.contains("--detach"), "ci/gates.sh detaches a run:\n{calls}");
    Ok(())
}

/// What a change affects, as `ci/gate.sh --since` scopes a gate to it (docs/ci.md's affected(),
/// W24-R17; W05-R03): each changed file's crate; the crates whose code reads a changed file by path,
/// a string literal naming it or a folder above it (`../../.kiln`), a comment naming it being no
/// read; a crate that builds a changed one by its manifest's path, not as a Cargo dependency; every
/// crate below a changed Cargo.toml; and every crate depending on those, through dev-dependencies
/// too. A changed file in no crate that none reads can't be placed, and puts the whole workspace in
/// scope; only an empty change has nothing to run. In a workspace of its own: a; b on a; c, reading
/// docs/c.md; d, dev-depending on b; e; f, naming docs/c.md in comments only; g, reading .kiln/; h,
/// building p by its manifest's path; and p.
#[test]
fn ci_gate_sh_scopes_a_change_to_its_crates_and_what_depends_on_them() -> anyhow::Result<()> {
    if !cfg!(unix) {
        return Ok(());
    }
    let tmp = tempfile::tempdir()?;
    let ws = tmp.path();
    let write = |path: &str, text: &str| -> std::io::Result<()> {
        let path = ws.join(path);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, text)
    };
    write("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n")?;
    for (name, deps, src) in [
        ("a", "", ""),
        ("b", "\n[dependencies]\na = { path = \"../a\" }\n", ""),
        ("c", "", "#[test]\nfn reads_its_doc() {\n    let _ = std::fs::read_to_string(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../../docs/c.md\"));\n}\n"),
        ("d", "\n[dev-dependencies]\nb = { path = \"../b\" }\n", ""),
        ("e", "", ""),
        ("f", "", "// Its tests read docs/c.md, says this comment, which is no read.\n/// Nor is `docs/c.md` in a doc comment.\n"),
        ("g", "", "#[test]\nfn reads_kilns_workflows() {\n    let _ = std::path::Path::new(env!(\"CARGO_MANIFEST_DIR\")).join(\"../../.kiln\");\n}\n"),
        ("h", "", "#[test]\nfn builds_p() {\n    let _ = std::path::Path::new(env!(\"CARGO_MANIFEST_DIR\")).join(\"../p/Cargo.toml\");\n}\n"),
        ("p", "\n[lib]\ncrate-type = [\"cdylib\"]\n", ""),
    ] {
        write(&format!("crates/{name}/Cargo.toml"), &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{deps}"))?;
        write(&format!("crates/{name}/src/lib.rs"), &format!("pub fn f() {{}}\n{src}"))?;
    }
    write("docs/c.md", "c\n")?;
    write(".kiln/ci.yaml", "on: push\n")?;
    write("ci/gate.sh", &std::fs::read_to_string(root().join("ci/gate.sh"))?)?;
    // git as no machine's config has it: no signing, no hooks.
    let home = tempfile::tempdir()?;
    let gitconfig = home.path().join("gitconfig");
    std::fs::write(&gitconfig, "")?;
    let run = |program: &str, args: &[&str]| -> anyhow::Result<String> {
        let out = std::process::Command::new(program)
            .args(args)
            .current_dir(ws)
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("CARGO_NET_OFFLINE", "true")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("KILN_NEEDS")
            .output()?;
        let said = String::from_utf8_lossy(&out.stdout).into_owned();
        anyhow::ensure!(out.status.success(), "{program} {args:?} failed: {said}{}", String::from_utf8_lossy(&out.stderr));
        Ok(said)
    };
    let commit = |message: &str| -> anyhow::Result<()> {
        run("git", &["add", "-A"])?;
        run("git", &["-c", "user.name=kiln", "-c", "user.email=kiln@localhost", "commit", "-q", "-m", message])?;
        Ok(())
    };
    run(env!("CARGO"), &["generate-lockfile", "--offline"])?;
    run("git", &["init", "-q"])?;
    commit("the workspace")?;

    write("crates/a/src/lib.rs", "pub fn f() {}\npub fn g() {}\n")?;
    write("docs/c.md", "c, changed\n")?;
    write(".kiln/ci.yaml", "on: pull_request\n")?;
    write("crates/p/src/lib.rs", "pub fn f() {}\npub fn g() {}\n")?;
    let said = run("bash", &["ci/gate.sh", "--since", "HEAD", "--dry-run"])?;
    for line in [
        "scope: a b c d g h p\n",
        "  a: changed: crates/a/src/lib.rs\n",
        "  c: reads docs/c.md (crates/c/src/lib.rs:",
        "  g: reads .kiln/ci.yaml (crates/g/src/lib.rs:",
        "  h: reads crates/p/src/lib.rs (crates/h/src/lib.rs:",
        " names crates/p/Cargo.toml)\n",
        "  b: depends on a\n",
        "  d: depends on a\n",
        "1. clippy: cargo clippy --locked -p a -p b -p c -p d -p g -p h -p p --all-targets",
    ] {
        assert!(said.contains(line), "ci/gate.sh --since HEAD doesn't say {line:?}:\n{said}");
    }
    assert!(!said.contains("  e:") && !said.contains("  f:"), "a comment naming a changed file is no read of it:\n{said}");

    commit("a change")?;
    write("notes.txt", "in no crate, and read by none\n")?;
    let said = run("bash", &["ci/gate.sh", "--since", "HEAD", "--dry-run"])?;
    for line in ["scope: the workspace, every crate\n", "  in no crate, and read by none: notes.txt\n", "1. clippy: cargo clippy --locked --workspace --all-targets"] {
        assert!(said.contains(line), "a changed file it can't place scopes the whole workspace, but ci/gate.sh --since HEAD doesn't say {line:?}:\n{said}");
    }

    commit("notes")?;
    write("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n# Every crate builds from this.\n")?;
    let said = run("bash", &["ci/gate.sh", "test", "--since", "HEAD", "--dry-run"])?;
    for line in ["scope: a b c d e f g h p\n", "  e: Cargo.toml changed", "1. test: cargo test --locked -p a -p b -p c -p d -p e -p f -p g -p h -p p --lib --no-run"] {
        assert!(said.contains(line), "ci/gate.sh test --since HEAD doesn't say {line:?}:\n{said}");
    }

    commit("the manifest")?;
    let said = run("bash", &["ci/gate.sh", "--since", "HEAD", "--dry-run"])?;
    assert!(said.contains("scope: none") && said.contains("steps: none"), "with nothing changed, ci/gate.sh checks nothing:\n{said}");
    Ok(())
}

#[test]
fn a_path_that_isnt_there_is_named() {
    let root = root();
    let text = "See `docs/ci.md` §0, `crates/kiln/src/no_such_module.rs`, and run `python3 scripts/no_such_script.py check`; `~/.cache` and `cp/tN-*` aren't repository paths.";
    assert_eq!(missing_paths(text, &root), vec!["crates/kiln/src/no_such_module.rs".to_owned(), "scripts/no_such_script.py".to_owned()]);
}
