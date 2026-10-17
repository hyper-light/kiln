//! docs/ci.md W02-AC08 and W02-R11: the loader reads real commits (a repository made here with
//! `git`): which files run under each `ci.github_workflows` setting and why, overlays, workflows
//! with errors, and that a file is parsed once per content.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use kiln::ci::load::{Commit, Loaded, Source, load_from};
use kiln_ci::expr::Dialect;
use kiln_ci::wf::GithubWorkflows;
use parking_lot::Mutex;

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=kiln", "-c", "user.email=kiln@example.invalid", "-c", "commit.gpgsign=false"]).args(args).output()?;
    anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A repository on disk and an in-memory cache, counting the blobs read.
struct Plain {
    dir: PathBuf,
    cache: Mutex<HashMap<(String, String), Loaded>>,
    reads: AtomicUsize,
}

impl Source for Plain {
    async fn tree(&self, sha: &str) -> anyhow::Result<String> {
        git(&self.dir, &["ls-tree", "-r", sha, "--", ".github/workflows", ".kiln"])
    }

    async fn blob(&self, oid: &str) -> anyhow::Result<String> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        git(&self.dir, &["cat-file", "blob", oid])
    }

    async fn cached(&self, path: &str, key: &str) -> anyhow::Result<Option<Loaded>> {
        Ok(self.cache.lock().get(&(path.to_owned(), key.to_owned())).cloned())
    }

    async fn remember(&self, key: &str, l: &Loaded) -> anyhow::Result<()> {
        self.cache.lock().insert((l.path.clone(), key.to_owned()), l.clone());
        Ok(())
    }
}

/// A repository holding `files`, committed; its commit.
fn repo(files: &[(&str, &str)]) -> anyhow::Result<(tempfile::TempDir, Arc<Plain>, String)> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().to_path_buf();
    git(&dir, &["init", "-q", "-b", "main"])?;
    for (path, text) in files {
        let p = dir.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, text)?;
    }
    git(&dir, &["add", "-A"])?;
    git(&dir, &["commit", "-q", "-m", "workflows"])?;
    let sha = git(&dir, &["rev-parse", "HEAD"])?.trim().to_owned();
    Ok((tmp, Arc::new(Plain { dir, cache: Mutex::default(), reads: AtomicUsize::new(0) }), sha))
}

fn load(src: &Arc<Plain>, sha: &str, setting: GithubWorkflows) -> anyhow::Result<Commit> {
    let (src, sha) = (Arc::clone(src), sha.to_owned());
    kiln::rt::block_on(async move { load_from(&*src, &sha, setting).await })
}

/// (path, runs) of each workflow, in path order.
fn runs(c: &Commit) -> Vec<(&str, bool)> {
    let mut v: Vec<(&str, bool)> = c.workflows.iter().map(|l| (l.path.as_str(), l.runs)).collect();
    v.sort();
    v
}

const GHA: &str = "on: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - id: unit\n        run: make test\n";
const KILN: &str = "name: ci\non: { push: {} }\njobs:\n  rust:\n    runs-on: [k8s]\n    steps:\n      - run: cargo test\n";

#[test]
fn w02_ac08_precedence() -> anyhow::Result<()> {
    let (_tmp, src, sha) = repo(&[(".github/workflows/ci.yml", GHA), (".kiln/ci.yaml", KILN)])?;
    let auto = load(&src, &sha, GithubWorkflows::Auto)?;
    assert_eq!(runs(&auto), vec![(".github/workflows/ci.yml", false), (".kiln/ci.yaml", true)]);
    assert!(!auto.github_run);
    assert_eq!(auto.github_note, "GitHub workflows not run: this commit has .kiln/ workflows");
    let on = load(&src, &sha, GithubWorkflows::On)?;
    assert_eq!(runs(&on), vec![(".github/workflows/ci.yml", true), (".kiln/ci.yaml", true)]);
    let off = load(&src, &sha, GithubWorkflows::Off)?;
    assert_eq!(runs(&off), vec![(".github/workflows/ci.yml", false), (".kiln/ci.yaml", true)]);
    assert_eq!(off.github_note, "GitHub workflows are turned off for this repository");
    // Each file was read once; the later loads came from the cache.
    assert_eq!(src.reads.load(Ordering::Relaxed), 2);
    // Without a kiln workflow, auto runs GitHub's.
    let (_tmp, src, sha) = repo(&[(".github/workflows/ci.yml", GHA), (".kiln/review.yaml", "required: 1\n")])?;
    let only = load(&src, &sha, GithubWorkflows::Auto)?;
    assert_eq!(runs(&only), vec![(".github/workflows/ci.yml", true)]);
    assert!(only.github_run);
    Ok(())
}

#[test]
fn w02_r01_what_is_a_workflow() -> anyhow::Result<()> {
    let (_tmp, src, sha) = repo(&[
        (".github/workflows/ci.yml", GHA),
        (".github/workflows/nested/no.yml", GHA),
        (".github/workflows/notes.md", "not a workflow"),
        (".kiln/ci.yaml", KILN),
        (".kiln/review.yaml", "required: 1\n"),
        (".kiln/actions/greet/action.yml", "name: greet\nruns: { using: composite, steps: [] }\n"),
        ("ci.yml", GHA),
    ])?;
    let c = load(&src, &sha, GithubWorkflows::On)?;
    assert_eq!(runs(&c), vec![(".github/workflows/ci.yml", true), (".kiln/ci.yaml", true)]);
    let gha = c.workflows.iter().find(|l| l.path.starts_with(".github")).map(|l| l.dialect);
    assert_eq!(gha, Some(Dialect::Gha));
    Ok(())
}

#[test]
fn w02_r07_overlays_apply_and_key_the_cache() -> anyhow::Result<()> {
    let overlay = "jobs:\n  test:\n    runs-on: [k8s]\n    retry: { max: 2 }\n    steps:\n      missing: { retry: 1 }\n";
    let (_tmp, src, sha) = repo(&[(".github/workflows/ci.yml", GHA), (".kiln/overlays/ci.yaml", overlay)])?;
    let c = load(&src, &sha, GithubWorkflows::Auto)?;
    let l = c.workflows.first().ok_or_else(|| anyhow::anyhow!("no workflow"))?;
    let job = l.workflow.as_ref().and_then(|w| w.jobs.first()).ok_or_else(|| anyhow::anyhow!("no job"))?;
    assert!(job.kiln.retry.is_some(), "the overlay's retry is in the model");
    let labels = match &job.runs_on {
        Some(kiln_ci::wf::ir::RunsOn::Labels(l)) => l.iter().map(|t| t.src.clone()).collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    assert_eq!(labels, vec!["k8s"], "the overlay maps runs-on");
    assert!(l.diagnostics.iter().any(|d| d.code == "KCI4010"), "a step the workflow doesn't have warns: {:?}", l.diagnostics);
    assert!(!l.has_errors());
    Ok(())
}

#[test]
fn w02_r11_a_broken_workflow_is_returned_with_its_diagnostics() -> anyhow::Result<()> {
    let (_tmp, src, sha) = repo(&[(".kiln/ci.yaml", "name: ci\non: push\njobs:\n  a:\n    steps:\n      - run: x\n        uses: y@v1\n")])?;
    let c = load(&src, &sha, GithubWorkflows::Auto)?;
    let l = c.workflows.first().ok_or_else(|| anyhow::anyhow!("no workflow"))?;
    assert!(l.has_errors());
    let d = l.diagnostics.iter().find(|d| d.code == "KCI1012").ok_or_else(|| anyhow::anyhow!("no KCI1012 in {:?}", l.diagnostics))?;
    assert_eq!(d.range.start.line, 6, "diagnostics keep their lines");
    Ok(())
}
