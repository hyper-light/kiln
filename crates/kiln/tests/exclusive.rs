//! W05-R03 (docs/ci.md): an exclusive leg that asks for CPUs holds them, and other work runs beside
//! it on the rest. A bench holding half this machine's CPUs runs at the same time as ordinary legs
//! on one runner, and nothing but the bench is allowed on its CPUs: each leg says, as it runs, the
//! CPUs its processes may use (`Cpus_allowed_list`). Before, an exclusive leg waited for its runner
//! to be empty and nothing ran beside it.

#![cfg(target_os = "linux")]

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    anyhow::ensure!(o.status.success(), "git {args:?}: {out}");
    Ok(out)
}

fn until(s: &Server, secs: u64, what: &str, mut f: impl FnMut() -> anyhow::Result<Option<String>>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match f()? {
            None => return Ok(()),
            Some(last) => anyhow::ensure!(Instant::now() < deadline, "{what}: {last}\n{}", s.log()),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// `0-7,16-23` as its CPUs.
fn cpus(s: &str) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    for part in s.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
                    out.extend(a..=b);
                }
            }
            None => out.extend(part.parse::<u32>().ok()),
        }
    }
    out
}

/// One leg as its step said: when it started and ended, and what CPUs it could use when.
struct Said {
    key: String,
    from: u128,
    to: u128,
    samples: Vec<(u128, BTreeSet<u32>)>,
}

/// W05-R03: a bench (exclusive, asking for half the CPUs) runs beside ordinary legs on one runner of
/// two slots, its CPUs whole cores of its own (a whole L3 domain where half is one), and no ordinary
/// leg's process is allowed on them while it runs.
#[test]
fn w05_r03_a_bench_runs_beside_other_work_its_cores_its_own() -> anyhow::Result<()> {
    let threads = std::thread::available_parallelism()?.get();
    if threads < 8 {
        eprintln!("skipped: {threads} CPUs, too few to hold half and leave a core or two");
        return Ok(());
    }
    let half = threads / 2;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: "" })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/bench", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/bench.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "bench\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    // Each leg says, twice a second, the CPUs it may use: the bench of a CPU-bound loop it runs, the
    // others of their own step. The two ordinary legs differ in length, so whichever starts first,
    // the bench runs beside one of them.
    let sample = |n: u32, of: &str| format!("for i in $(seq 1 {n}); do echo \"cpus $(date +%s%N) $(grep Cpus_allowed_list /proc/{of}/status | cut -f2)\"; sleep 0.5; done");
    let bench = format!("echo \"start $(date +%s%N)\"; ( while true; do true; done ) & spin=$!; {}; kill $spin; echo \"end $(date +%s%N)\"", sample(12, "$spin"));
    let ordinary = |n: u32| format!("echo \"start $(date +%s%N)\"; {}; echo \"end $(date +%s%N)\"", sample(n, "self"));
    let workflow = format!(
        "on: push\njobs:\n  bench:\n    runs-on: local\n    requires: [\"cpu.threads >= 1\"]\n    exclusive: true\n    resources: {{ cpu: {half}, memory: 1Gi }}\n    steps:\n      - run: {bench}\n  short:\n    runs-on: local\n    requires: [\"cpu.threads >= 1\"]\n    steps:\n      - run: {}\n  long:\n    runs-on: local\n    requires: [\"cpu.threads >= 1\"]\n    steps:\n      - run: {}\n",
        ordinary(12),
        ordinary(24)
    );
    std::fs::write(work.join(".kiln/bench.yaml"), workflow)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "bench"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let mut run = Value::Null;
    until(&s, 300, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/bench&limit=5", &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0").cloned().unwrap_or_default();
        Ok(run.get("finished_at").is_none_or(Value::is_null).then_some(body))
    })?;
    assert_eq!(run.get("conclusion"), Some(&json!("success")), "{run}\n{}", s.log());
    // What each leg said.
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let mut said: Vec<Said> = Vec::new();
    for l in serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
        let (leg, key) = (l.get("id").and_then(Value::as_str).unwrap_or_default(), l.get("key").and_then(Value::as_str).unwrap_or_default().to_owned());
        let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
        let mut log = String::new();
        for x in serde_json::from_str::<Value>(&about)?.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.get("idx").and_then(Value::as_u64)) {
            log.push_str(&http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={x}"), &s.admin, None)?.1);
        }
        let at = |what: &str| log.lines().find_map(|l| l.split_once(what).and_then(|(_, t)| t.trim().parse::<u128>().ok()));
        let (Some(from), Some(to)) = (at("start "), at("end ")) else { anyhow::bail!("{key}'s times: {log}") };
        let samples = log
            .lines()
            .filter_map(|l| {
                let rest = l.split_once("cpus ")?.1;
                let (t, list) = rest.split_once(' ')?;
                Some((t.trim().parse::<u128>().ok()?, cpus(list)))
            })
            .collect();
        said.push(Said { key, from, to, samples });
    }
    let find = |k: &str| said.iter().find(|x| x.key.starts_with(k)).ok_or_else(|| anyhow::anyhow!("no leg {k}"));
    let b = find("bench")?;
    // What the runner said of what the bench holds, for when it isn't so.
    let told: String = std::fs::read_to_string(dir.path().join("server.log")).unwrap_or_default().lines().filter(|l| l.contains("CPUs") || l.contains("pinned") || l.contains("holds") || l.contains("leg assigned")).collect::<Vec<_>>().join("\n");
    // The bench's loop ran on one set of CPUs all along: half the machine's, whole cores of its own.
    let held = b.samples.first().map(|(_, c)| c.clone()).unwrap_or_default();
    let all: String = said.iter().map(|x| format!("{} {}–{}: {}", x.key, x.from, x.to, x.samples.iter().map(|(t, c)| format!("{t}:{}", c.len())).collect::<Vec<_>>().join(" "))).collect::<Vec<_>>().join("\n");
    assert!(held.len() >= half && held.len() < threads, "the bench holds about half: {held:?}\n{told}\n{all}");
    assert!(b.samples.iter().all(|(_, c)| *c == held), "its CPUs stayed its own: {:?}", b.samples);
    // It ran at the same time as other work on its runner, where before it waited for it to empty.
    let overlaps = |x: &Said| x.from < b.to && b.from < x.to;
    let (short, long) = (find("short")?, find("long")?);
    assert!(overlaps(short) || overlaps(long), "the bench beside an ordinary leg: bench {}–{}, short {}–{}, long {}–{}", b.from, b.to, short.from, short.to, long.from, long.to);
    // And while it ran, no ordinary leg's process was allowed on its CPUs.
    let beside: Vec<&(u128, BTreeSet<u32>)> = [short, long].iter().flat_map(|x| x.samples.iter()).filter(|(t, _)| *t > b.from && *t < b.to).collect();
    assert!(!beside.is_empty(), "samples beside the bench: short {:?}, long {:?}", short.samples, long.samples);
    for (t, c) in beside {
        assert!(c.is_disjoint(&held), "at {t} an ordinary leg may use {c:?}, the bench's being {held:?}");
    }
    Ok(())
}
