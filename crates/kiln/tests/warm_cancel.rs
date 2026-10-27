//! W28-AC46 (docs/ci.md W28-R49, W24-AC02 extended): a warm build after a mid-build cancel equals a
//! cold one. A fixture cargo workspace (`a` <- `b`, whose build script takes 3 s, <- `c`, a binary)
//! is built cold for the reference; built again in an emptied target directory and cut off, once
//! `a` has finished, by the runner's own cancel sequence (W05-R18); then built warm in the same
//! target directory, at the same path. The warm build concludes as the cold one, with the same test
//! outcomes and the same release binary, `a` fresh and `b`, cut off, rebuilt: what a cancelled
//! leg's workspace going back to its lane relies on (W24-R11).

#![cfg(unix)]

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// The runner's cancel (W05-R18): SIGINT to the step's group, SIGTERM 7.5 s later, then SIGKILL.
const SIGINT_GRACE: Duration = Duration::from_millis(7_500);
const SIGTERM_GRACE: Duration = Duration::from_millis(2_500);

fn write(root: &Path, path: &str, text: &str) -> anyhow::Result<()> {
    let p = root.join(path);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(p, text)?;
    Ok(())
}

/// The fixture: `a` (a library, tested) <- `b` (a library whose build script sleeps 3 s) <- `c` (a
/// binary, tested). Path dependencies only: it builds offline.
fn fixture(root: &Path) -> anyhow::Result<()> {
    let package = |name: &str, deps: &str| format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{deps}");
    write(root, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n")?;
    write(root, "a/Cargo.toml", &package("a", ""))?;
    write(root, "a/src/lib.rs", "pub fn base() -> u64 {\n    40\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn base_is_forty() {\n        assert_eq!(super::base(), 40);\n    }\n}\n")?;
    write(root, "b/Cargo.toml", &package("b", "a = { path = \"../a\" }\n"))?;
    write(
        root,
        "b/build.rs",
        "use std::{env, fs, path::Path, thread, time::Duration};\n\nfn main() {\n    thread::sleep(Duration::from_secs(3));\n    let out = env::var(\"OUT_DIR\").unwrap_or_default();\n    let _ = fs::write(Path::new(&out).join(\"two.rs\"), \"pub const TWO: u64 = 2;\\n\");\n    println!(\"cargo:rerun-if-changed=build.rs\");\n}\n",
    )?;
    write(root, "b/src/lib.rs", "include!(concat!(env!(\"OUT_DIR\"), \"/two.rs\"));\n\npub fn answer() -> u64 {\n    a::base() + TWO\n}\n")?;
    write(root, "c/Cargo.toml", &package("c", "b = { path = \"../b\" }\n"))?;
    write(root, "c/src/main.rs", "fn main() {\n    println!(\"the answer is {}\", b::answer());\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn the_answer() {\n        assert_eq!(b::answer(), 42);\n    }\n}\n")?;
    Ok(())
}

/// `cargo <verb> --release` on the fixture, in `target`, its units listed (`-v`).
fn cargo(root: &Path, target: &Path, verb: &str) -> Command {
    let mut c = Command::new(option_env!("CARGO").unwrap_or("cargo"));
    c.args([verb, "--release", "--offline", "-v", "-j", "2", "--target-dir"]).arg(target).current_dir(root).env_remove("CARGO_TARGET_DIR").env_remove("RUSTFLAGS").env_remove("CARGO_ENCODED_RUSTFLAGS");
    c
}

/// A build's exit code and what cargo said.
fn build(root: &Path, target: &Path) -> anyhow::Result<(Option<i32>, String)> {
    let o = cargo(root, target, "build").output()?;
    Ok((o.status.code(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))))
}

/// `cargo test`'s exit code and each test's outcome, in order.
fn tests(root: &Path, target: &Path) -> anyhow::Result<(Option<i32>, Vec<String>)> {
    let o = cargo(root, target, "test").output()?;
    let mut outcomes: Vec<String> = String::from_utf8_lossy(&o.stdout).lines().filter(|l| l.starts_with("test ") && l.contains(" ... ")).map(str::to_owned).collect();
    outcomes.sort();
    Ok((o.status.code(), outcomes))
}

/// The release binary's SHA-256 and what it prints.
fn binary(target: &Path) -> anyhow::Result<(String, String)> {
    let path = target.join("release").join("c");
    let digest = Sha256::digest(std::fs::read(&path)?).iter().map(|b| format!("{b:02x}")).collect();
    let out = Command::new(&path).output()?;
    Ok((digest, String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// Whether cargo recorded `package`'s library as built: its fingerprint is written once the unit
/// finished, never for one cut off (W28-R49, f18).
fn finished(target: &Path, package: &str) -> bool {
    let Ok(dirs) = std::fs::read_dir(target.join("release").join(".fingerprint")) else { return false };
    dirs.flatten().any(|d| d.file_name().to_string_lossy().starts_with(&format!("{package}-")) && d.path().join(format!("lib-{package}")).exists())
}

/// The build started again in `target` and cut off as the runner cancels a step, once `a` has
/// finished and while `b`'s build script runs. What cargo said, and how long the cancel took.
async fn cut_off(root: &Path, target: &Path) -> anyhow::Result<(String, Duration)> {
    let mut running = kiln::proc::spawn_group(cargo(root, target, "build"))?;
    let said = Arc::new(Mutex::new(Vec::new()));
    let (rx, log) = (running.output.clone(), Arc::clone(&said));
    kiln::rt::spawn(async move {
        while let Ok(chunk) = rx.recv().await {
            if let Ok(mut l) = log.lock() {
                l.extend(chunk);
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(300);
    while !finished(target, "a") {
        anyhow::ensure!(Instant::now() < deadline, "a never finished");
        kiln::rt::sleep(Duration::from_millis(20)).await;
    }
    anyhow::ensure!(!finished(target, "b"), "b finished before the cancel");
    let t = Instant::now();
    kiln::proc::interrupt_group(&mut running, SIGINT_GRACE, SIGTERM_GRACE).await;
    let took = t.elapsed();
    kiln::rt::sleep(Duration::from_millis(200)).await;
    let text = said.lock().map(|l| String::from_utf8_lossy(&l).into_owned()).unwrap_or_default();
    Ok((text, took))
}

/// W28-AC46: cold, cut off, then warm in the same target directory at the same path. The warm
/// build concludes as the cold one did, with the same test outcomes and the same release binary;
/// what finished before the cancel is fresh, and what was cut off is rebuilt.
#[test]
fn w28_ac46_a_warm_build_after_a_mid_build_cancel_equals_a_cold_one() -> anyhow::Result<()> {
    let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let root = base.path().join("fixture");
    fixture(&root)?;
    let target = root.join("target");

    let (cold, said) = build(&root, &target)?;
    anyhow::ensure!(cold == Some(0), "the cold build failed:\n{said}");
    let cold_bin = binary(&target)?;
    let cold_tests = tests(&root, &target)?;
    assert_eq!(cold_bin.1, "the answer is 42\n");
    assert_eq!(cold_tests.0, Some(0), "{cold_tests:?}");
    assert_eq!(cold_tests.1.len(), 2, "a's test and c's: {cold_tests:?}");

    std::fs::remove_dir_all(&target)?;
    let (r, t) = (root.clone(), target.clone());
    let (said, took) = kiln::rt::block_on(async move { cut_off(&r, &t).await })?;
    assert!(took < SIGINT_GRACE + SIGTERM_GRACE, "the cancel took {took:?}:\n{said}");
    assert!(!target.join("release").join("c").exists(), "the cut-off build never got to c:\n{said}");
    assert!(finished(&target, "a") && !finished(&target, "b"), "a finished and b was cut off:\n{said}");

    let (warm, said) = build(&root, &target)?;
    assert_eq!(warm, cold, "the warm build concludes as the cold one:\n{said}");
    assert!(said.contains("Fresh a v0.1.0"), "a, finished before the cancel, is fresh:\n{said}");
    assert!(said.contains("Compiling b v0.1.0"), "b, cut off, is rebuilt:\n{said}");
    assert!(said.contains("Compiling c v0.1.0"), "c is built:\n{said}");
    assert_eq!(binary(&target)?, cold_bin, "the same release binary, byte for byte");
    assert_eq!(tests(&root, &target)?, cold_tests, "the same test outcomes");
    Ok(())
}
