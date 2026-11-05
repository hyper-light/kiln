//! Experiments measured where the agent that asked works (docs/ci.md W20-R11), without CI: each
//! variant checked out in a git worktree of its own, from that agent's repository (the base commit
//! with the variant's change applied and committed), and a command run there as often as the
//! experiment asks, as `kiln run` runs one here (the person's login environment, bash, what it
//! leaves running ended with it). What comes back: its exit code, the end of what it printed, how
//! long it took, and the metrics it wrote to `$KILN_METRICS`, a JSON line each
//! (`{"name": "p95_ms", "value": 12.3}`).
//!
//! A measurement starts at once ([`start`]) and is asked about until it ends ([`measured`]); one
//! nobody asks about any more ends at its time limit. The worktrees are kiln's, under its data
//! folder, made once for each variant and kept for the experiment's rounds (a build in one is
//! incremental from the second round on), and removed with it ([`unmeasure`]).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::reach::serve::Event;
use crate::reach::wire::{RunSpec, Signal};

/// One command to measure one variant by, once.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Measure {
    /// This measurement, the same however often it's asked about (`E3/a+b/bench/0`).
    pub key: String,
    pub experiment: String,
    pub variant: String,
    /// The commit every variant starts from.
    pub base: String,
    /// The variant's change on it, as `git diff --binary` says it, in base64 (a change's files
    /// needn't be UTF-8); empty for the baseline.
    pub patch: String,
    pub command: String,
    /// Stopped after this many seconds.
    pub limit_s: u64,
}

/// How much of the end of what a command printed its answer keeps.
const TAIL: usize = 8 << 10;
/// How long a measurement's answer is kept after it ends, for a server that asks again.
const KEPT: Duration = Duration::from_secs(3600);
/// How long fetching the base commit may take.
const FETCH: Duration = Duration::from_secs(300);
/// How long making a worktree may take (a large repository's checkout).
const MAKE: Duration = Duration::from_secs(600);

enum State {
    /// Its command's process (its group's leader, once started), and whether it was asked to stop.
    Running { pid: Option<u32>, stopped: bool },
    Done { at: Instant, answer: Value },
}

static MEASURING: LazyLock<Mutex<HashMap<String, State>>> = LazyLock::new(Mutex::default);
/// One worktree made at a time: two criteria of a variant measured at once share its worktree.
static MAKING: futures::lock::Mutex<()> = futures::lock::Mutex::new(());

/// Starts measurement `m` in its variant's worktree of the repository `work` is in (made the first
/// time): `{"started": key}` at once, however often it's asked.
pub async fn start(work: &Path, m: Measure) -> anyhow::Result<Value> {
    start_in(&crate::config::default_data_dir(), work, m).await
}

/// [`start`], its worktrees under the data folder `data`.
async fn start_in(data: &Path, work: &Path, m: Measure) -> anyhow::Result<Value> {
    anyhow::ensure!(!m.key.is_empty() && !m.command.trim().is_empty(), "a measurement needs its key and its command");
    if MEASURING.lock().contains_key(&m.key) {
        return Ok(json!({"started": m.key}));
    }
    let (tree, metrics) = {
        let _one = MAKING.lock().await;
        let tree = worktree(data, work, &m).await?;
        let metrics = tree.with_file_name(format!("{}.metrics", m.key.replace(['/', '\\'], "_")));
        (tree, metrics)
    };
    let _ = std::fs::remove_file(&metrics);
    {
        let mut all = MEASURING.lock();
        if all.contains_key(&m.key) {
            return Ok(json!({"started": m.key}));
        }
        all.retain(|_, s| !matches!(s, State::Done { at, .. } if at.elapsed() > KEPT));
        all.insert(m.key.clone(), State::Running { pid: None, stopped: false });
    }
    let env = vec![
        ("KILN_METRICS".to_owned(), metrics.to_string_lossy().into_owned()),
        ("KILN_EXPERIMENT".to_owned(), m.experiment.clone()),
        ("KILN_VARIANT".to_owned(), m.variant.clone()),
    ];
    let spec = RunSpec { command: m.command.clone(), cwd: Some(tree.to_string_lossy().into_owned()), ..RunSpec::default() };
    let (tx, rx) = async_channel::bounded::<Event>(16);
    drop(crate::reach::serve::start(spec, tree, None, env, tx));
    let key = m.key.clone();
    crate::rt::spawn(follow(key, rx, metrics, Duration::from_secs(m.limit_s.max(1))));
    Ok(json!({"started": m.key}))
}

/// Measurement `key`: `{"running": true}` while it runs (asked to stop, with `stop`), then
/// `{"done": {exit_code, error, metrics, output, seconds, over, stopped}}`.
pub fn measured(key: &str, stop: bool) -> Value {
    let mut all = MEASURING.lock();
    match all.get_mut(key) {
        None => json!({"error": format!("no measurement {key} on this machine: its runner may have started again since")}),
        Some(State::Running { pid, stopped }) => {
            if stop {
                *stopped = true;
                if let Some(p) = *pid {
                    crate::reach::serve::signal_group(p, Signal::Terminate);
                }
            }
            json!({"running": true})
        }
        Some(State::Done { answer, .. }) => answer.clone(),
    }
}

/// Experiment `experiment` is over: its measurements still running stop, and its worktrees of the
/// repository `work` is in go.
pub async fn unmeasure(work: &Path, experiment: &str) -> anyhow::Result<Value> {
    unmeasure_in(&crate::config::default_data_dir(), work, experiment).await
}

async fn unmeasure_in(data: &Path, work: &Path, experiment: &str) -> anyhow::Result<Value> {
    let prefix = format!("{experiment}/");
    {
        let mut all = MEASURING.lock();
        for (_, s) in all.iter_mut().filter(|(k, _)| k.starts_with(&prefix)) {
            if let State::Running { pid: Some(p), stopped } = s {
                *stopped = true;
                crate::reach::serve::signal_group(*p, Signal::Terminate);
            }
        }
        all.retain(|k, s| !(k.starts_with(&prefix) && matches!(s, State::Done { .. })));
    }
    let facts = super::git::facts(work).await?;
    let top = PathBuf::from(&facts.top);
    let root = root_of(data, &top, experiment);
    let mut removed = 0usize;
    for entry in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.is_dir() {
            let mut c = super::git::git(&top);
            c.args(["worktree", "remove", "--force"]).arg(&p);
            let _ = super::git::run(c, MAKE).await;
            removed += 1;
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    let mut c = super::git::git(&top);
    c.args(["worktree", "prune"]);
    let _ = super::git::run(c, MAKE).await;
    Ok(json!({"removed": removed}))
}

/// Where experiment `experiment`'s worktrees of the repository at `top` are: kiln's, under its data
/// folder `data` (never in the repository, which sync copies to other machines).
fn root_of(data: &Path, top: &Path, experiment: &str) -> PathBuf {
    let of = crate::util::sha256_hex(top.to_string_lossy().as_bytes());
    let name: String = experiment.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    data.join("experiments").join(format!("{name}-{}", of.get(..12).unwrap_or(&of)))
}

/// Variant `m.variant`'s worktree: its base with its change committed, made the first time (and
/// again if what it's made of changed).
async fn worktree(data: &Path, work: &Path, m: &Measure) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(!m.variant.is_empty() && m.variant.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+')), "`{}` isn't a variant's name", m.variant);
    let facts = super::git::facts(work).await?;
    let top = PathBuf::from(&facts.top);
    let root = root_of(data, &top, &m.experiment);
    let tree = root.join(&m.variant);
    let mark = root.join(format!("{}.made", m.variant));
    let made = format!("{}\n{}", m.base, crate::util::sha256_hex(m.patch.as_bytes()));
    if tree.join(".git").exists() && std::fs::read_to_string(&mark).is_ok_and(|t| t == made) {
        return Ok(tree);
    }
    if tree.exists() {
        let mut c = super::git::git(&top);
        c.args(["worktree", "remove", "--force"]).arg(&tree);
        let _ = super::git::run(c, MAKE).await;
        let _ = std::fs::remove_dir_all(&tree);
    }
    std::fs::create_dir_all(&root)?;
    have(&top, &m.base, &facts.remotes).await?;
    let mut c = super::git::git(&top);
    c.args(["worktree", "add", "--detach", "--quiet"]).arg(&tree).arg(&m.base);
    super::git::run(c, MAKE).await?;
    if !m.patch.is_empty() {
        let patch = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, m.patch.trim()).map_err(|e| anyhow::anyhow!("{}'s change isn't base64: {e}", m.variant))?;
        let file = root.join(format!("{}.patch", m.variant));
        std::fs::write(&file, patch)?;
        let mut c = super::git::git(&tree);
        c.args(["apply", "--index", "--binary", "--whitespace=nowarn"]).arg(&file);
        let applied = super::git::run(c, MAKE).await;
        let _ = std::fs::remove_file(&file);
        applied.map_err(|e| anyhow::anyhow!("{}'s change doesn't apply on {}: {e:#}", m.variant, short(&m.base)))?;
        let mut c = super::git::git(&tree);
        c.args(["-c", "user.name=kiln", "-c", "user.email=kiln@localhost", "commit", "--quiet", "--no-verify", "--no-gpg-sign", "--allow-empty", "-m"]).arg(format!("{} {}", m.experiment, m.variant));
        super::git::run(c, MAKE).await?;
    }
    std::fs::write(&mark, made)?;
    Ok(tree)
}

/// Commit `sha` in the repository at `top`: there already, or fetched from one of its remotes.
async fn have(top: &Path, sha: &str, remotes: &[super::git::Remote]) -> anyhow::Result<()> {
    let there = || {
        let mut c = super::git::git(top);
        c.args(["cat-file", "-e", &format!("{sha}^{{commit}}")]);
        c
    };
    if super::git::run(there(), MAKE).await.is_ok() {
        return Ok(());
    }
    for r in remotes {
        let mut c = super::git::git(top);
        c.args(["fetch", "--quiet", "--no-tags", &r.name, sha]);
        if std::env::var_os("GIT_SSH_COMMAND").is_none() {
            c.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
        }
        if super::git::run(c, FETCH).await.is_ok() && super::git::run(there(), MAKE).await.is_ok() {
            return Ok(());
        }
    }
    anyhow::bail!("commit {} isn't in {}, and none of its remotes gave it", short(sha), top.display())
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// Follows measurement `key`'s command to its end (stopped at `limit`), and keeps what it came to.
async fn follow(key: String, rx: async_channel::Receiver<Event>, metrics: PathBuf, limit: Duration) {
    use futures::FutureExt as _;
    let began = Instant::now();
    let mut deadline = Box::pin(crate::rt::sleep(limit).fuse());
    let (mut tail, mut over) = (Vec::<u8>::new(), false);
    let (code, error) = loop {
        futures::select_biased! {
            e = rx.recv().fuse() => match e {
                Ok(Event::Started { pid, .. }) => {
                    if let Some(State::Running { pid: p, stopped }) = MEASURING.lock().get_mut(&key) {
                        *p = Some(pid);
                        // Asked to stop before it started.
                        if *stopped {
                            crate::reach::serve::signal_group(pid, Signal::Terminate);
                        }
                    }
                }
                Ok(Event::Output { data, .. }) => {
                    tail.extend_from_slice(&data);
                    if tail.len() > 2 * TAIL {
                        tail.drain(..tail.len() - TAIL);
                    }
                }
                Ok(Event::Exit { code, error, .. }) => break (code, error),
                Err(_) => break (None, Some("kiln lost track of it".to_owned())),
            },
            () = deadline => {
                over = true;
                if let Some(State::Running { pid: Some(p), .. }) = MEASURING.lock().get(&key) {
                    crate::reach::serve::signal_group(*p, Signal::Kill);
                }
            }
        }
    };
    let output = String::from_utf8_lossy(tail.get(tail.len().saturating_sub(TAIL)..).unwrap_or_default()).into_owned();
    let found = read_metrics(&std::fs::read_to_string(&metrics).unwrap_or_default());
    let _ = std::fs::remove_file(&metrics);
    let mut all = MEASURING.lock();
    let stopped = matches!(all.get(&key), Some(State::Running { stopped: true, .. }));
    let answer = json!({"done": {"exit_code": code, "error": error, "metrics": found, "output": output, "seconds": began.elapsed().as_secs_f64(), "over": over, "stopped": stopped}});
    all.insert(key, State::Done { at: Instant::now(), answer });
}

/// The metrics in `text` (`$KILN_METRICS`): each a JSON line `{"name", "value"}`, the mean of one
/// said more than once; lines that aren't one are passed over.
fn read_metrics(text: &str) -> BTreeMap<String, f64> {
    let mut sums: BTreeMap<String, (f64, u32)> = BTreeMap::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
        let (Some(name), Some(value)) = (v.get("name").and_then(Value::as_str), v.get("value").and_then(Value::as_f64)) else { continue };
        if name.trim().is_empty() || !value.is_finite() {
            continue;
        }
        let s = sums.entry(name.trim().to_owned()).or_insert((0.0, 0));
        *s = (s.0 + value, s.1 + 1);
    }
    sums.into_iter().map(|(k, (sum, n))| (k, sum / f64::from(n.max(1)))).collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// What git printed, as it printed it.
    fn raw(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
        let o = std::process::Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .output()?;
        anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    }

    fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
        Ok(raw(dir, args)?.trim().to_owned())
    }

    async fn until_done(key: &str) -> anyhow::Result<Value> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let v = measured(key, false);
            if let Some(done) = v.get("done") {
                return Ok(done.clone());
            }
            anyhow::ensure!(Instant::now() < deadline && v.get("running").is_some(), "{key}: {v}");
            crate::rt::sleep(Duration::from_millis(50)).await;
        }
    }

    #[test]
    fn w20_r11_metrics_are_json_lines_and_a_repeated_one_is_their_mean() {
        let m = read_metrics("{\"name\": \"p95_ms\", \"value\": 10}\nnot json\n{\"name\": \"p95_ms\", \"value\": 14, \"unit\": \"ms\"}\n{\"name\": \"size\", \"value\": 3.5}\n{\"value\": 1}\n");
        assert_eq!(m.get("p95_ms"), Some(&12.0));
        assert_eq!(m.get("size"), Some(&3.5));
        assert_eq!(m.len(), 2, "{m:?}");
    }

    /// A variant measured without CI: its worktree beside the agent's repository is the base with
    /// its change, the command runs there and its metrics come back; a failing command says so with
    /// what it printed; the baseline is the base itself; one stopped is said to be; and the
    /// experiment's end takes its worktrees.
    #[test]
    fn w20_r11_a_variant_is_measured_in_its_own_worktree() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            // kiln's data folder, where the worktrees go, in the test's own.
            let data = dir.path().join("data");
            let repo = dir.path().join("repo");
            std::fs::create_dir_all(&repo)?;
            git(&repo, &["init", "-q"])?;
            std::fs::write(repo.join("speed"), "100\n")?;
            git(&repo, &["add", "."])?;
            git(&repo, &["commit", "-qm", "base"])?;
            let base = git(&repo, &["rev-parse", "HEAD"])?;
            git(&repo, &["checkout", "-qb", "fast"])?;
            std::fs::write(repo.join("speed"), "80\n")?;
            git(&repo, &["commit", "-qam", "faster"])?;
            let patch = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw(&repo, &["diff", "--binary", &base, "HEAD"])?);
            git(&repo, &["checkout", "-q", "main"])?;
            let bench = "echo \"{\\\"name\\\": \\\"ms\\\", \\\"value\\\": $(cat speed)}\" >> \"$KILN_METRICS\"; echo measured; [ \"$(cat speed)\" -lt 90 ]";
            let ask = |variant: &str, patch: &str, round: u32| Measure {
                key: format!("E1/{variant}/bench/{round}"),
                experiment: "E1".into(),
                variant: variant.into(),
                base: base.clone(),
                patch: patch.into(),
                command: bench.into(),
                limit_s: 60,
            };
            assert!(start_in(&data, &repo, ask("fast", &patch, 0)).await?.get("started").is_some());
            let done = until_done("E1/fast/bench/0").await?;
            assert_eq!(done.get("exit_code"), Some(&json!(0)), "{done}");
            assert_eq!(done.pointer("/metrics/ms"), Some(&json!(80.0)), "{done}");
            assert!(done.get("output").and_then(Value::as_str).is_some_and(|o| o.contains("measured")), "{done}");
            // Asked again: the same answer, not a second run.
            assert_eq!(measured("E1/fast/bench/0", false).get("done"), Some(&done));

            start_in(&data, &repo, ask("baseline", "", 0)).await?;
            let done = until_done("E1/baseline/bench/0").await?;
            assert_eq!((done.get("exit_code"), done.pointer("/metrics/ms")), (Some(&json!(1)), Some(&json!(100.0))), "the base, failing its check: {done}");
            // The person's checkout is as it was.
            assert_eq!(std::fs::read_to_string(repo.join("speed"))?, "100\n");

            // A second round reuses the worktree.
            let tree = root_of(&data, Path::new(&git(&repo, &["rev-parse", "--show-toplevel"])?), "E1").join("fast");
            std::fs::write(tree.join("built"), "kept\n")?;
            start_in(&data, &repo, ask("fast", &patch, 1)).await?;
            until_done("E1/fast/bench/1").await?;
            assert!(tree.join("built").exists(), "made once, kept for its rounds");

            // Stopped.
            let slow = Measure { key: "E1/fast/sleep/0".into(), command: "sleep 30".into(), ..ask("fast", &patch, 0) };
            start_in(&data, &repo, slow).await?;
            assert_eq!(measured("E1/fast/sleep/0", true).get("running"), Some(&json!(true)));
            let done = until_done("E1/fast/sleep/0").await?;
            assert_eq!(done.get("stopped"), Some(&json!(true)), "{done}");

            let gone = unmeasure_in(&data, &repo, "E1").await?;
            assert_eq!(gone.get("removed"), Some(&json!(2)), "{gone}");
            assert!(!tree.exists());
            assert!(git(&repo, &["worktree", "list"])?.lines().count() == 1, "its worktrees pruned");
            assert!(measured("E1/fast/bench/0", false).get("error").is_some(), "its answers go with it");
            Ok(())
        })
    }
}
