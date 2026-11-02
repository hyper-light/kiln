//! AGENTS.md and CLAUDE.md at the repository's root: what every coding agent reads before it
//! changes kiln. CLAUDE.md only imports AGENTS.md (Claude Code reads AGENTS.md on its own only
//! from v2.1.277, and only where no CLAUDE.md is), AGENTS.md stays short enough to be followed,
//! names the critical-path master plan its CP-* ids come from, every repository path it names is
//! there, and it says how the agent that owns a change composes the change's gate from the steps
//! `ci/gate.sh` runs alone.

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

/// The runs AGENTS.md composes a change's gate from: each line that runs `ci/gate.sh` through
/// `kiln run --needs`.
fn gate_runs(agents: &str) -> Vec<GateRun> {
    agents
        .lines()
        .filter_map(|l| {
            let rest = l.trim().trim_end_matches('&').trim().strip_prefix("kiln run --needs ")?;
            let (needs, command) = rest.split_once(' ')?;
            let (_, args) = command.split_once("-- ci/gate.sh")?;
            Some(GateRun { needs: needs.to_owned(), args: args.split_whitespace().map(str::to_owned).collect() })
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

/// The agent that owns a change composes its gate (W05-R03): AGENTS.md gives a run per platform,
/// Linux, macOS and Windows, each through `kiln run --needs` with the CPUs and memory its steps need
/// (not one Linux run of 12 CPUs and 40 GiB), scoped to the change, and sends work to no machine by
/// name. `ci/gate.sh`, given each run's needs, says what it was asked, holds itself to it and lists
/// that run's steps alone, natively, before it runs any.
#[test]
fn a_change_s_gate_is_a_run_per_platform_each_asked_for_by_what_it_needs() -> anyhow::Result<()> {
    let root = root();
    let agents = std::fs::read_to_string(root.join("AGENTS.md"))?;
    for name in ["fedora", "jupiter", "--on ", "`on:`"] {
        assert!(!agents.contains(name), "AGENTS.md sends work to a machine by name ({name:?})");
    }
    assert!(!agents.contains("cpus=12,mem=40g"), "AGENTS.md still asks for the gate as one run of 12 CPUs and 40 GiB");
    let runs = gate_runs(&agents);
    for platform in ["linux", "macos", "windows"] {
        assert!(runs.iter().any(|r| r.needs.split(',').any(|w| w.starts_with(platform))), "AGENTS.md composes no gate run on {platform}: {runs:?}");
    }
    for run in &runs {
        let need = |key: &str| run.needs.split(',').find_map(|w| w.strip_prefix(key)).and_then(|v| v.trim_end_matches('g').parse::<u64>().ok());
        let (Some(cpus), Some(mem)) = (need("cpus="), need("mem=")) else {
            anyhow::bail!("a gate run asks for the CPUs and the memory (in GiB) its steps need: {run:?}");
        };
        assert!(cpus < 12 && mem < 40, "a gate run asks for what its steps need, not for 12 CPUs and 40 GiB: {run:?}");
        assert!(run.args.iter().any(|a| a == "--since" || a == "-p"), "a gate run is scoped to the crates of the change: {run:?}");
        if !cfg!(unix) {
            continue;
        }
        // Scoped to kiln here: --since reads the change's history, with git and cargo.
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
        let said = dry_run(&root, &args, Some(&run.needs))?;
        let jobs = (mem / 6).min(cpus).max(1);
        for line in [
            format!("asked: {cpus} CPUs, {mem}G of memory (from KILN_NEEDS={})", run.needs),
            format!("caps: MemoryMax={mem}G CPUWeight=1 Nice=19"),
            format!("cargo: -j {jobs}, {jobs} test threads"),
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
/// workspace; asked for nothing, it holds itself to 4 CPUs and 16 GiB (the standard runner kiln's
/// GitHub CI runs these steps on), not to 12 CPUs and 40 GiB.
#[test]
fn ci_gate_sh_runs_one_step_alone_on_the_crates_named() -> anyhow::Result<()> {
    if !cfg!(unix) {
        return Ok(());
    }
    let root = root();
    let clippy = dry_run(&root, &["clippy", "-p", "kiln-ci"], None)?;
    for line in [
        "asked: 4 CPUs, 16G of memory (from nothing",
        "caps: MemoryMax=16G CPUWeight=1 Nice=19",
        "cargo: -j 2, 2 test threads",
        "scope: kiln-ci\n",
        "1. clippy: cargo clippy --locked -p kiln-ci --all-targets -j 2 -- -D warnings\n",
    ] {
        assert!(clippy.contains(line), "ci/gate.sh clippy -p kiln-ci doesn't say {line:?}:\n{clippy}");
    }
    assert!(!clippy.contains("cargo test"), "ci/gate.sh clippy runs tests too:\n{clippy}");
    let test = dry_run(&root, &["test", "-p", "kiln-ci", "-p", "kiln-cd"], None)?;
    for line in ["scope: kiln-cd kiln-ci\n", "1. test: cargo test --locked -p kiln-cd -p kiln-ci --lib --no-run -j 2, then cargo test --locked -p kiln-cd -p kiln-ci --no-fail-fast -j 2"] {
        assert!(test.contains(line), "ci/gate.sh test -p kiln-ci -p kiln-cd doesn't say {line:?}:\n{test}");
    }
    assert!(!test.contains("cargo clippy"), "ci/gate.sh test runs clippy too:\n{test}");
    let all = dry_run(&root, &[], None)?;
    for line in ["scope: the workspace, every crate\n", "1. clippy: cargo clippy --locked --workspace --all-targets", "2. test: cargo test --locked --workspace --lib --no-run"] {
        assert!(all.contains(line), "ci/gate.sh with no step or scope doesn't say {line:?}:\n{all}");
    }
    Ok(())
}

/// What a change affects, as `ci/gate.sh --since` scopes a gate to it (docs/ci.md's affected(),
/// W24-R17): each changed file's crate, the crates whose sources name a changed file that's in none
/// (a doc a test reads), every crate below a changed Cargo.toml, and every crate depending on those,
/// through dev-dependencies too; a file in no crate that none names affects none, and with nothing
/// in scope there's nothing to run. In a workspace of its own: a; b on a; c, which names docs/c.md;
/// d, dev-depending on b; and e.
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
        ("c", "", "// Its tests read docs/c.md.\n"),
        ("d", "\n[dev-dependencies]\nb = { path = \"../b\" }\n", ""),
        ("e", "", ""),
    ] {
        write(&format!("crates/{name}/Cargo.toml"), &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{deps}"))?;
        write(&format!("crates/{name}/src/lib.rs"), &format!("pub fn f() {{}}\n{src}"))?;
    }
    write("docs/c.md", "c\n")?;
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
    write("notes.txt", "in no crate\n")?;
    let said = run("bash", &["ci/gate.sh", "--since", "HEAD", "--dry-run"])?;
    for line in [
        "scope: a b c d\n",
        "  a: changed: crates/a/src/lib.rs\n",
        "  c: names docs/c.md (crates/c/src/lib.rs)\n",
        "  b: depends on a\n",
        "  d: depends on a\n",
        "  in no crate, and named in none: notes.txt\n",
        "1. clippy: cargo clippy --locked -p a -p b -p c -p d --all-targets",
    ] {
        assert!(said.contains(line), "ci/gate.sh --since HEAD doesn't say {line:?}:\n{said}");
    }

    commit("a change")?;
    write("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n# Every crate builds from this.\n")?;
    let said = run("bash", &["ci/gate.sh", "test", "--since", "HEAD", "--dry-run"])?;
    for line in ["scope: a b c d e\n", "  e: Cargo.toml changed", "1. test: cargo test --locked -p a -p b -p c -p d -p e --lib --no-run"] {
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
