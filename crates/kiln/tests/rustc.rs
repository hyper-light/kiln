//! `kiln rustc` (docs/ci.md W24-R09) as cargo's `RUSTC_WRAPPER`, over a workspace of three crates.

use std::path::Path;
use std::process::Command;

fn krate(root: &Path, name: &str, deps: &str, value: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(root.join(name).join("src"))?;
    std::fs::write(root.join(name).join("Cargo.toml"), format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{deps}"))?;
    std::fs::write(root.join(name).join("src/lib.rs"), format!("pub fn value() -> u32 {{ {value} }}\n#[cfg(test)]\nmod tests {{\n    #[test]\n    fn positive() {{ assert!(super::value() > 0); }}\n}}\n"))?;
    Ok(())
}

/// One build of the workspace and its tests, from nothing (a target folder of its own), through
/// the wrapper: what the wrapper said of each unit.
fn build(root: &Path, cache: &Path, n: u32) -> anyhow::Result<String> {
    let log = root.join(format!("log-{n}"));
    let out = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["test", "--no-run", "--offline", "--workspace"])
        .current_dir(root)
        .env("RUSTC_WRAPPER", env!("CARGO_BIN_EXE_kiln"))
        .env("CARGO_TARGET_DIR", root.join(format!("target-{n}")))
        .env("CARGO_INCREMENTAL", "0")
        .env("KILN_RUSTC_CACHE", cache)
        .env("KILN_RUSTC_LOG", &log)
        .output()?;
    anyhow::ensure!(out.status.success(), "build {n}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(std::fs::read_to_string(&log).unwrap_or_default())
}

fn said<'a>(log: &'a str, verdict: &str) -> Vec<&'a str> {
    let mut v: Vec<&str> = log.lines().filter_map(|l| l.strip_prefix(verdict)).map(|s| s.trim().split('-').next().unwrap_or_default()).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// W24-R09: a change that touches no Rust source hits every unit, libraries and test binaries
/// alike, in a build from nothing; a change in one crate rebuilds it and what depends on it, and
/// the rest hit.
#[test]
fn w24_r09_a_ui_only_change_hits_every_unit_and_a_crate_change_rebuilds_its_dependents() -> anyhow::Result<()> {
    let d = tempfile::tempdir()?;
    let (root, cache) = (d.path().join("ws"), d.path().join("cache"));
    std::fs::create_dir_all(root.join("ui"))?;
    std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"alpha\", \"beta\", \"gamma\"]\nresolver = \"2\"\n")?;
    krate(&root, "alpha", "", "1")?;
    krate(&root, "beta", "alpha = { path = \"../alpha\" }\n", "alpha::value() + 1")?;
    krate(&root, "gamma", "", "3")?;
    std::fs::write(root.join("ui/app.tsx"), "export const A = 1;\n")?;
    let first = build(&root, &cache, 1)?;
    anyhow::ensure!(said(&first, "hit").is_empty() && said(&first, "miss") == ["alpha", "beta", "gamma"], "{first}");
    // A UI-only change.
    std::fs::write(root.join("ui/app.tsx"), "export const A = 2;\n")?;
    let second = build(&root, &cache, 2)?;
    anyhow::ensure!(said(&second, "miss").is_empty() && said(&second, "hit") == ["alpha", "beta", "gamma"], "every unit hits: {second}");
    // A change in alpha's interface: it and beta rebuild; gamma hits.
    std::fs::write(root.join("alpha/src/lib.rs"), "pub fn value() -> u32 { 1 }\npub fn more() -> u32 { 2 }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn positive() { assert!(super::value() > 0); }\n}\n")?;
    let third = build(&root, &cache, 3)?;
    anyhow::ensure!(said(&third, "miss") == ["alpha", "beta"] && said(&third, "hit") == ["gamma"], "{third}");
    Ok(())
}
