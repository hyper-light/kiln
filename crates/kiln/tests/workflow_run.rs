//! `kiln workflow run` (docs/ci.md W17-R06, W17-AC04): a working tree's workflow runs on this
//! machine with the server's engine and runtime, uncommitted work and all, and the exit code says
//! how it went. The binary itself runs here, its server and runner in its own process.

#![cfg(unix)]

use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git").current_dir(dir).args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "init.defaultBranch=main"]).args(args).output()?;
    anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The kiln dialect checks the commit out by itself, from the server: nothing here needs the
/// network.
const WORKFLOW: &str = r#"name: local
on: push
jobs:
  build:
    runs-on: [self-hosted, gpu]
    outputs:
      v: ${{ steps.s.outputs.v }}
    steps:
      - id: s
        run: |
          echo "built $(cat change.txt)"
          echo "v=42" >> "$GITHUB_OUTPUT"
          echo "token=${{ secrets.TOKEN }} who=${{ vars.WHO }}"
  test:
    needs: build
    runs-on: self-hosted
    strategy:
      matrix:
        n: [1, 2, 3]
    steps:
      - run: echo "test ${{ matrix.n }} got ${{ needs.build.outputs.v }}"
  fails:
    runs-on: self-hosted
    steps:
      - run: echo "about to fail" && exit 3
"#;

/// `kiln workflow run ARGS` in `repo`: its exit code, and what it printed (stdout, then stderr).
fn workflow_run(repo: &Path, home: &Path, args: &[&str]) -> anyhow::Result<(i32, String)> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kiln"));
    cmd.current_dir(repo).args(["workflow", "run"]).args(args).env("HOME", home).env("XDG_DATA_HOME", home.join("data")).env("XDG_CONFIG_HOME", home.join("config"));
    for var in ["KILN_CONFIG", "KILN_URL", "KILN_ADMIN_TOKEN", "KILN_SUPERVISED", "KILN_GENERATION", "KILN_LOG", "KILN_CONTAINER_NETWORK"] {
        cmd.env_remove(var);
    }
    let out = cmd.output()?;
    Ok((out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))
}

#[test]
fn w17_ac04_a_working_tree_runs_here() -> anyhow::Result<()> {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipped: no git");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let (repo, home) = (dir.path().join("repo"), dir.path().join("home"));
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::create_dir_all(&home)?;
    std::fs::write(repo.join(".kiln/local.yaml"), WORKFLOW)?;
    std::fs::write(repo.join("change.txt"), "from the last commit")?;
    git(&repo, &["init", "-q"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "init"])?;
    // Work not committed: the run takes it.
    std::fs::write(repo.join("change.txt"), "from the working tree")?;
    let secrets = dir.path().join("secrets.env");
    std::fs::write(&secrets, "TOKEN=hunter2-never-shown\n")?;

    let (code, out) = workflow_run(&repo, &home, &["local", "--job", "test", "--secret-file", &secrets.to_string_lossy(), "--var", "WHO=me"])?;
    assert_eq!(code, 0, "{out}");
    for want in ["built from the working tree", "token=*** who=me", "test 1 got 42", "test 2 got 42", "test 3 got 42", "✓ local #1: success"] {
        assert!(out.contains(want), "{want:?} in:\n{out}");
    }
    assert!(!out.contains("hunter2") && !out.contains("about to fail"), "the secret masked, and only the job asked for and what it needs:\n{out}");

    // One matrix combination; then the job that fails, and the exit code says so.
    let (code, out) = workflow_run(&repo, &home, &["local", "--job", "test", "--matrix", "n=2"])?;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("test 2 got 42") && !out.contains("test 1 got") && !out.contains("test 3 got"), "{out}");
    let (code, out) = workflow_run(&repo, &home, &["local", "--job", "fails"])?;
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("about to fail") && out.contains("✗ fails failure"), "{out}");

    // The repository as it was: the change uncommitted, one commit, no index left behind; the
    // runs' own files gone.
    assert_eq!(git(&repo, &["status", "--porcelain"])?.trim(), "M change.txt");
    assert_eq!(git(&repo, &["rev-list", "--count", "HEAD"])?.trim(), "1");
    let gitdir = std::fs::read_dir(repo.join(".git"))?.filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().starts_with("kiln-")).count();
    assert_eq!(gitdir, 0, "no temporary index left");
    let runs = home.join("data/kiln/workflow-runs");
    assert_eq!(std::fs::read_dir(&runs).map(Iterator::count).unwrap_or(0), 0, "{} is empty", runs.display());
    Ok(())
}
