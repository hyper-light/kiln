//! AGENTS.md and CLAUDE.md at the repository's root: what every coding agent reads before it
//! changes kiln. CLAUDE.md only imports AGENTS.md (Claude Code reads AGENTS.md on its own only
//! from v2.1.277, and only where no CLAUDE.md is), AGENTS.md stays short enough to be followed,
//! names the critical-path master plan its CP-* ids come from, and every repository path it names
//! is there.

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

/// Agents gate through kiln (W05-R03): AGENTS.md asks for the gate by what it needs and sends work
/// to no machine by name, and `ci/gate.sh`, given those needs, says the steps it runs and the caps
/// it holds itself to before it runs any.
#[test]
fn the_gate_is_asked_for_by_what_it_needs() -> anyhow::Result<()> {
    let root = root();
    let agents = std::fs::read_to_string(root.join("AGENTS.md"))?;
    let flat = agents.split_whitespace().collect::<Vec<_>>().join(" ");
    let recipe = flat.split("`kiln run --needs ").nth(1).filter(|r| r.contains("-- ci/gate.sh`")).ok_or_else(|| anyhow::anyhow!("AGENTS.md doesn't say to gate with `kiln run --needs … -- ci/gate.sh`"))?;
    let needs = recipe.split_whitespace().next().unwrap_or_default();
    for name in ["fedora", "jupiter", "--on ", "`on:`"] {
        assert!(!agents.contains(name), "AGENTS.md sends work to a machine by name ({name:?})");
    }
    if cfg!(unix) {
        let out = std::process::Command::new("bash").arg(root.join("ci/gate.sh")).arg("--dry-run").env("KILN_NEEDS", needs).output()?;
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "ci/gate.sh --dry-run failed: {said}{}", String::from_utf8_lossy(&out.stderr));
        for line in [
            format!("asked: 12 CPUs, 40G of memory (from KILN_NEEDS={needs})"),
            "caps: MemoryMax=40G CPUWeight=1 Nice=19".to_owned(),
            "cargo: -j 6, 6 test threads".to_owned(),
            "clippy-linux: cargo clippy --locked --workspace --all-targets".to_owned(),
            "test: cargo test --locked --workspace --no-fail-fast".to_owned(),
        ] {
            assert!(said.contains(&line), "ci/gate.sh --dry-run doesn't say {line:?}:\n{said}");
        }
    }
    Ok(())
}

#[test]
fn a_path_that_isnt_there_is_named() {
    let root = root();
    let text = "See `docs/ci.md` §0, `crates/kiln/src/no_such_module.rs`, and run `python3 scripts/no_such_script.py check`; `~/.cache` and `cp/tN-*` aren't repository paths.";
    assert_eq!(missing_paths(text, &root), vec!["crates/kiln/src/no_such_module.rs".to_owned(), "scripts/no_such_script.py".to_owned()]);
}
