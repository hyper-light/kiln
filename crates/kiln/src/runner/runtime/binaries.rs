//! A step's `tests:` (docs/ci.md W24-R10): the test binaries one build makes, each run when its
//! binary or what it read changed and replayed from its recorded pass when neither did. The build
//! is cargo's (`cargo test --no-run --message-format=json`); a binary's digest is its executable's
//! and what it read its package's files. The server's work ledger decides which run
//! (`/_kiln/work/v1/tests/select`) and keeps each run (`tests/record`): passes are replayed later,
//! failures never.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kiln_ci::work::key::{PathSet, Platform, Read};
use kiln_ci::work::tests::{self as test_results, Binary, Built, Context, Spec};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::{End, Leg, Run, State};

/// A file's SHA-256, in hex.
fn sha256_file(path: &Path) -> anyhow::Result<String> {
    use std::io::Read as _;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(buf.get(..n).unwrap_or_default());
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Every file under `dir` but build output and git's, by path from `root`, with its digest: what a
/// package's tests may read.
fn files_of(root: &Path, dir: &Path, out: &mut PathSet) -> anyhow::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let (p, kind) = (e.path(), e.file_type()?);
        let name = e.file_name();
        if kind.is_dir() {
            if name != "target" && name != ".git" {
                files_of(root, &p, out)?;
            }
        } else if kind.is_file() {
            let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            out.insert(rel, Read::File(sha256_file(&p)?));
        }
    }
    Ok(())
}

/// A shell word, quoted.
fn quoted(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

impl Leg {
    /// Runs a `tests:` step (W24-R10): the build, then the binaries the ledger says to run, each
    /// run recorded; the others replayed, saying from which run and why.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn tests_step(&self, st: &mut State, idx: u32, spec: &Spec, wd: &Path, env: BTreeMap<String, String>, deadline: Instant, shell: Option<&str>) -> anyhow::Result<super::Ran> {
        let temp = env.get("RUNNER_TEMP").map_or_else(std::env::temp_dir, PathBuf::from).join(format!("kiln-tests-{}", crate::util::new_id()));
        std::fs::create_dir_all(&temp)?;
        let messages = temp.join("build.json");
        let args: Vec<String> = spec.cargo.iter().map(|a| quoted(a)).collect();
        let build = format!("cargo test --no-run --message-format=json-render-diagnostics {} > {}", args.join(" "), quoted(&messages.to_string_lossy()));
        let built = self.exec(st, idx, Run::Script { script: &build, shell }, wd, env.clone(), deadline).await?;
        if !matches!(built.end, End::Exit(Some(0))) {
            return Ok(built);
        }
        let found = test_results::built(&std::fs::read_to_string(&messages).unwrap_or_default());
        // Each binary's digest, and the files of each package.
        let (root, list) = (wd.to_path_buf(), found.clone());
        let (binaries, files) = crate::rt::blocking(move || -> anyhow::Result<(Vec<Binary>, PathSet)> {
            let mut files = PathSet::new();
            let mut binaries = Vec::new();
            for b in &list {
                binaries.push(Binary { package: b.package.clone(), name: b.name.clone(), args: Vec::new(), digest: Some(sha256_file(Path::new(&b.executable))?) });
                files_of(&root, Path::new(&b.dir), &mut files)?;
            }
            Ok((binaries, files))
        })
        .await??;
        let context = Context { env: BTreeMap::new(), platform: Platform { os: std::env::consts::OS.to_owned(), arch: std::env::consts::ARCH.to_owned(), ..Platform::default() }, trust: String::new() };
        let (status, v) = self.work_post("tests/select", &json!({ "binaries": binaries, "context": context, "files": files }), Duration::from_secs(60)).await?;
        anyhow::ensure!(status == 200, "the server couldn't say which test binaries run ({status}): {v}");
        let choices: Vec<Value> = v.get("binaries").and_then(Value::as_array).cloned().unwrap_or_default();
        let runs = |b: &Built| choices.iter().find(|c| c.get("package").and_then(Value::as_str) == Some(b.package.as_str()) && c.get("name").and_then(Value::as_str) == Some(b.name.as_str()));
        let text = |c: Option<&Value>, k: &str| c.and_then(|c| c.get(k)).and_then(Value::as_str).unwrap_or_default().to_owned();
        let mut to_run: Vec<(usize, &Built, String)> = Vec::new();
        for (i, b) in found.iter().enumerate() {
            let c = runs(b);
            if c.and_then(|c| c.get("run")).and_then(Value::as_bool).unwrap_or(true) {
                to_run.push((i, b, text(c, "why")));
            } else {
                let from = c.and_then(|c| c.get("from"));
                let number = from.and_then(|f| f.get("number")).and_then(Value::as_i64).map_or_else(|| text(from, "run"), |n| format!("#{n}"));
                let passed = from.and_then(|f| f.pointer("/ran/cases")).and_then(Value::as_object).map_or(0, |m| m.values().filter(|x| x.as_str() == Some("passed")).count());
                self.note(idx, &format!("tests: {}/{} replayed from run {number}, {passed} passed: {}", b.package, b.name, text(c, "why"))).await;
            }
        }
        if to_run.is_empty() {
            self.note(idx, "tests: every test binary replayed; none ran").await;
            return Ok(super::Ran { end: End::Exit(Some(0)), outputs: BTreeMap::new(), state: Vec::new() });
        }
        // Each binary to run in its package's folder, as cargo runs it, its output kept to read.
        let mut script = String::new();
        for (i, b, why) in &to_run {
            script.push_str(&format!("echo {}\n", quoted(&format!("tests: running {}/{}: {why}", b.package, b.name))));
            let (out, rc, secs) = (temp.join(format!("out.{i}")), temp.join(format!("rc.{i}")), temp.join(format!("s.{i}")));
            // How long each took, in seconds, for the timing that balances shards (W16-R06).
            script.push_str(&format!(
                "t0=$(date +%s); {{ cd {} && CARGO_MANIFEST_DIR={} {}; echo $? > {}; }} 2>&1 | tee {}; echo $(( $(date +%s) - t0 )) > {}\n",
                quoted(&b.dir),
                quoted(&b.dir),
                quoted(&b.executable),
                quoted(&rc.to_string_lossy()),
                quoted(&out.to_string_lossy()),
                quoted(&secs.to_string_lossy())
            ));
        }
        let ran = self.exec(st, idx, Run::Script { script: &script, shell }, wd, env, deadline).await?;
        let mut all_passed = true;
        for (i, b, _) in &to_run {
            let out = std::fs::read_to_string(temp.join(format!("out.{i}"))).unwrap_or_default();
            let exit = std::fs::read_to_string(temp.join(format!("rc.{i}"))).ok().and_then(|s| s.trim().parse::<i32>().ok());
            let ms = std::fs::read_to_string(temp.join(format!("s.{i}"))).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0).saturating_mul(1000);
            let r = test_results::Ran { exit, cases: test_results::libtest(&out), ms };
            all_passed &= r.passed();
            let Some(binary) = binaries.get(*i).cloned() else { continue };
            let rel = root_rel(wd, &b.dir);
            let prefix = if rel.is_empty() { String::new() } else { format!("{rel}/") };
            let reads: PathSet = files.iter().filter(|(p, _)| p.starts_with(&prefix)).map(|(p, r)| (p.clone(), r.clone())).collect();
            match self.work_post("tests/record", &json!({ "binary": binary, "context": context, "reads": reads, "ran": r }), Duration::from_secs(60)).await {
                Ok((201, _)) => {}
                Ok((s, v)) => self.note(idx, &format!("tests: {}/{} not recorded ({s}): {v}", b.package, b.name)).await,
                Err(e) => self.note(idx, &format!("tests: {}/{} not recorded: {e:#}", b.package, b.name)).await,
            }
        }
        let _ = std::fs::remove_dir_all(&temp);
        let end = match ran.end {
            End::Exit(_) if all_passed => End::Exit(Some(0)),
            End::Exit(Some(0)) => End::Exit(Some(1)),
            other => other,
        };
        Ok(super::Ran { end, ..ran })
    }
}

/// `dir` from the workspace `wd`, as files are keyed.
fn root_rel(wd: &Path, dir: &str) -> String {
    Path::new(dir).strip_prefix(wd).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default()
}
