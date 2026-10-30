//! W05-R34 (docs/ci.md, docs/needs.md §8): a grant is a lease its runner renews. A leg that runs
//! past two of the server's leases keeps its grant and ends on its first attempt: renewed by a lease
//! runner's `Renew`, what it sends fenced by its grant; and, built with `chaos`, by the heartbeat of
//! a runner from before leases, which says nothing of them. W05-R36, built with `chaos`: a runner
//! cut off past its leg's lease gets the leg's next attempt when it's back, and what it sends then
//! about the attempt it lost is refused. W10-R03, built with `chaos`: the lost attempt's tokens die
//! with its grant, and the next attempt's are taken. W05-R35: an operator's drain reaches the
//! engine.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, Server, http, start_env};
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

/// A server whose grants live 20 s, with its own runner (`env` its faults), runs one leg of `secs`
/// seconds: the leg as the server has it once its run ended, with its attempts and its log.
fn outlive(dir: &Path, env: &[(&str, &str)], secs: u32) -> anyhow::Result<(Server, Value, String)> {
    let (s, about, log) = run_leg(dir, env, &format!("echo began; sleep {secs}; echo ended"), u64::from(secs) + 240)?;
    let ended = s.log();
    let (st, run) = (about.pointer("/run/conclusion").cloned().unwrap_or_default(), about.get("run").cloned().unwrap_or_default());
    anyhow::ensure!(st == json!("success"), "the run: {run}\n{ended}");
    Ok((s, about, log))
}

/// A server whose grants live 20 s, with its own runner (`env` its faults), runs one leg whose
/// step is `script` (a shell's lines), within `secs` seconds: the leg as the server has it once
/// its run ended, with its attempts, its run and its log.
fn run_leg(dir: &Path, env: &[(&str, &str)], script: &str, secs: u64) -> anyhow::Result<(Server, Value, String)> {
    let s = push_leg(dir, env, "", script)?;
    let (about, log) = ended(&s, secs)?;
    Ok((s, about, log))
}

/// A server whose grants live 20 s, with its own runner (`env` its faults), and its repository
/// `acme/lease`, pushed a workflow of one job: `job` its keys before `steps` (YAML lines at the
/// job's indent), `script` its one step (a shell's lines). The server, the job's run on its way.
fn push_leg(dir: &Path, env: &[(&str, &str)], job: &str, script: &str) -> anyhow::Result<Server> {
    let body: String = script.lines().map(|l| format!("          {l}\n")).collect();
    push_workflow(dir, env, &format!("on: push\njobs:\n  long:\n    runs-on: local\n{job}    steps:\n      - run: |\n{body}"))
}

/// [`push_leg`], the workflow pushed being `workflow`.
fn push_workflow(dir: &Path, env: &[(&str, &str)], workflow: &str) -> anyhow::Result<Server> {
    let s = start_env(dir, &Options { agent: true, ci: "kiln_workflows = true", extra: "[sched]\nlease = \"20s\"\n" }, false, env)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/lease", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.join("githome"), dir.join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/lease.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "lease\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::write(work.join(".kiln/lease.yaml"), workflow)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "a leg past two leases"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    Ok(s)
}

/// `s`'s run once it ended, within `secs` seconds: its leg as the server has it, with its attempts
/// and its run, and its log.
fn ended(s: &Server, secs: u64) -> anyhow::Result<(Value, String)> {
    let mut run = Value::Null;
    until(s, secs, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/lease&limit=5", &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0").cloned().unwrap_or_default();
        Ok(run.get("finished_at").is_none_or(Value::is_null).then_some(body))
    })?;
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
    let mut about: Value = serde_json::from_str(&about)?;
    if let Some(o) = about.as_object_mut() {
        o.insert("run".into(), run);
    }
    let mut log = String::new();
    for x in about.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.get("idx").and_then(Value::as_u64)) {
        log.push_str(&http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={x}"), &s.admin, None)?.1);
    }
    Ok((about, log))
}

/// Its grant was never lost: one attempt, ended in success, its whole output taken, and nothing in
/// the server's log of a lease run out.
fn kept(s: &Server, about: &Value, log: &str) {
    let attempts = about.get("attempts").and_then(Value::as_array).cloned().unwrap_or_default();
    assert_eq!(attempts.len(), 1, "one attempt: {attempts:?}\n{}", s.log());
    assert_eq!(attempts.first().and_then(|a| a.get("conclusion")), Some(&json!("success")), "{attempts:?}");
    assert!(log.contains("began") && log.contains("ended"), "its output, whole: {log}");
    assert!(!s.log().contains("its lease ran out"), "a grant was lost: {}", s.log());
}

/// W05-R34: a lease runner renews its leg's grant every 10 s (`Renew`), past two leases of 20 s,
/// and what it sends about the leg (fenced by its grant) is taken.
#[test]
fn w05_r34_a_lease_runner_keeps_its_grant_past_two_leases() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (s, about, log) = outlive(dir.path(), &[], 45)?;
    kept(&s, &about, &log);
    Ok(())
}

/// W05-R34: a runner from before leases (`old`: it says nothing past `Build`'s 32 bits) keeps its
/// leg's grant by its heartbeat, which names the legs it runs.
#[cfg(feature = "chaos")]
#[test]
fn w05_r34_an_older_runners_heartbeat_keeps_its_grant() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (s, about, log) = outlive(dir.path(), &[("KILN_CHAOS", "old")], 45)?;
    kept(&s, &about, &log);
    Ok(())
}

/// W05-R36, the example of docs/needs.md E3.3: the server's runner, cut off past its leg's lease
/// (its link down 40 s, the lease 20 s), gets the leg's next attempt when it's back, the only
/// runner there is. What it sends then about the attempt it lost (the output it held while it was
/// away, its step's end, its result) is refused under that attempt's grant: none of it is in the
/// leg's log, and its result ends nothing. The second attempt's is taken, and the leg succeeds.
#[cfg(feature = "chaos")]
#[test]
fn w05_r36_a_runner_cut_off_past_its_lease_has_its_late_writes_refused() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let script = "if [ \"$KILN_ATTEMPT\" = 1 ]; then\n  for i in $(seq 1 20); do echo \"attempt 1 line $i\"; sleep 1; done\nelse\n  echo \"attempt 2 ran\"\nfi";
    let (s, about, log) = run_leg(dir.path(), &[("KILN_CHAOS", "drop=12s,down=40s")], script, 420)?;
    let attempts = about.get("attempts").and_then(Value::as_array).cloned().unwrap_or_default();
    let field = |i: usize, k: &str| attempts.get(i).and_then(|a| a.get(k)).cloned().unwrap_or_default();
    assert_eq!(attempts.len(), 2, "attempt 1 lost, attempt 2 run: {attempts:?}\n{}", s.log());
    assert_eq!((field(0, "conclusion"), field(0, "class")), (json!("failure"), json!("runner-lost")), "attempt 1 lost with its lease, its late result taken for nothing: {attempts:?}");
    assert_eq!(field(0, "runner"), field(1, "runner"), "attempt 2 on the same runner: {attempts:?}");
    assert!(field(0, "error").as_str().is_some_and(|e| e.contains("lease ran out")), "{attempts:?}");
    assert_eq!(field(1, "conclusion"), json!("success"), "{attempts:?}");
    assert_eq!(about.pointer("/run/conclusion"), Some(&json!("success")), "{about}");
    assert!(log.contains("attempt 2 ran"), "attempt 2's output is taken: {log}");
    assert!(!log.contains("attempt 1 line 20"), "a line attempt 1 wrote long after it was cut off is not in the leg's log: {log}");
    assert!(s.log().contains("refused a write under a grant"), "its late writes refused, and its runner told to stop: {}", s.log());
    // Each kind of what it sent late was refused: its output, its step's events, its result.
    let (_, metrics) = http(s.port, "GET", "/metrics", &s.admin, None)?;
    let refused = |what: &str| {
        let key = format!("kiln_ci_refused_writes_total{{what=\"{what}\"}}");
        metrics.lines().find_map(|l| l.strip_prefix(key.as_str())).and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(0.0)
    };
    for what in ["log", "event", "result"] {
        assert!(refused(what) >= 1.0, "no {what} refused: {}", metrics.lines().filter(|l| l.contains("refused_writes")).collect::<Vec<_>>().join("\n"));
    }
    Ok(())
}

/// An attempt's tokens, as its step wrote them: its runtime token (`ACTIONS_RUNTIME_TOKEN`), its
/// ID token request token (`ACTIONS_ID_TOKEN_REQUEST_TOKEN`), its GITHUB_TOKEN (a job token,
/// `KILN_TOKEN`), and its run and leg.
#[cfg(feature = "chaos")]
struct Tokens {
    runtime: String,
    id: String,
    job: String,
    run: String,
    leg: String,
}

/// Attempt `n`'s tokens, once its step has written them under `out`.
#[cfg(feature = "chaos")]
fn tokens(out: &Path, n: u32) -> Option<Tokens> {
    let d = out.join(n.to_string());
    let r = |k: &str| std::fs::read_to_string(d.join(k)).ok();
    Some(Tokens { runtime: r("runtime")?, id: r("id")?, job: r("job")?, run: r("run")?, leg: r("leg")? })
}

/// How `t`'s three uses are answered (their statuses): an artifact's upload started (`name`) with
/// its runtime token, the repository's git ref read with its GITHUB_TOKEN, an ID token asked with
/// its request token.
#[cfg(feature = "chaos")]
fn uses(s: &Server, t: &Tokens, name: &str) -> anyhow::Result<[u16; 3]> {
    let start = json!({ "workflow_run_backend_id": t.run, "workflow_job_run_backend_id": t.leg, "name": name, "version": 4 });
    let (upload, _) = http(s.port, "POST", "/twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact", &t.runtime, Some(&start))?;
    let (git, _) = http(s.port, "GET", "/api/v3/repos/acme/lease/git/ref/heads/main", &t.job, None)?;
    let (id, _) = http(s.port, "GET", "/_kiln/oidc/token?api-version=1&audience=kiln-test", &t.id, None)?;
    Ok([upload, git, id])
}

/// How the commit of `t`'s upload `name` (empty) is answered: its status.
#[cfg(feature = "chaos")]
fn commit(s: &Server, t: &Tokens, name: &str) -> anyhow::Result<u16> {
    let empty = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let body = json!({ "workflow_run_backend_id": t.run, "workflow_job_run_backend_id": t.leg, "name": name, "size": "0", "hash": empty });
    Ok(http(s.port, "POST", "/twirp/github.actions.results.api.v1.ArtifactService/FinalizeArtifact", &t.runtime, Some(&body))?.0)
}

/// W10-R03, the example of docs/needs.md E3.4: a leg's tokens are its attempt's and die with its
/// grant. The server's runner is cut off past its leg's lease, as in W05-R36's test. While attempt
/// 1's grant is live its tokens are taken: its runtime token starts an artifact's upload (and its
/// commit is taken in hand), its GITHUB_TOKEN reads the repository's git ref, its request token
/// gets an ID token. Once its loss is recorded, all of them are refused (401) within a second by
/// the test's clock, the upload's commit too. Attempt 2's are taken while it runs, on the same
/// runner back, attempt 1's still not; and once attempt 2 ended, its own are refused.
#[cfg(feature = "chaos")]
#[test]
fn w10_r03_a_lost_attempts_tokens_stop_working() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let out = dir.path().join("tokens");
    std::fs::create_dir_all(&out)?;
    let job = "    permissions:\n      contents: read\n      id-token: write\n";
    let script = format!(
        "d=\"{}/$KILN_ATTEMPT\"\nmkdir -p \"$d.tmp\"\nprintf %s \"$ACTIONS_RUNTIME_TOKEN\" > \"$d.tmp/runtime\"\nprintf %s \"$ACTIONS_ID_TOKEN_REQUEST_TOKEN\" > \"$d.tmp/id\"\nprintf %s \"$KILN_TOKEN\" > \"$d.tmp/job\"\nprintf %s \"$KILN_RUN\" > \"$d.tmp/run\"\nprintf %s \"$KILN_JOB\" > \"$d.tmp/leg\"\nmv \"$d.tmp\" \"$d\"\nif [ \"$KILN_ATTEMPT\" = 1 ]; then sleep 25; else sleep 4; fi",
        out.display()
    );
    let s = push_leg(dir.path(), &[("KILN_CHAOS", "drop=12s,down=40s")], job, &script)?;
    let wait_for = |n: u32| -> anyhow::Result<Tokens> {
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            if let Some(t) = tokens(&out, n) {
                return Ok(t);
            }
            anyhow::ensure!(Instant::now() < deadline, "attempt {n}'s step never wrote its tokens\n{}", s.log());
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    // Attempt 1, its grant live: each of its tokens taken.
    let first = wait_for(1)?;
    assert_eq!(uses(&s, &first, "a1")?, [200, 200, 200], "attempt 1's tokens, its grant live\n{}", s.log());
    assert_ne!(commit(&s, &first, "a1")?, 401, "its upload's commit is taken in hand");
    // Its loss recorded (the leg's attempt 1 ended, runner-lost), looked for every 100 ms.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (_, body) = http(s.port, "GET", &format!("/api/ci/legs/{}", first.leg), &s.admin, None)?;
        let leg: Value = serde_json::from_str(&body)?;
        if leg.pointer("/attempts/0/conclusion") == Some(&json!("failure")) {
            assert_eq!(leg.pointer("/attempts/0/class"), Some(&json!("runner-lost")), "{leg}");
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "attempt 1 was never lost: {leg}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(100));
    }
    let seen = Instant::now();
    let refused = loop {
        let (u, c) = (uses(&s, &first, "a1-again")?, commit(&s, &first, "a1")?);
        if u == [401, 401, 401] && c == 401 {
            break seen.elapsed();
        }
        anyhow::ensure!(seen.elapsed() < Duration::from_secs(5), "attempt 1's tokens still taken 5 s after its loss was recorded: {u:?}, its commit {c}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(50));
    };
    eprintln!("w10_r03: attempt 1's tokens refused {} ms after its loss was seen recorded", refused.as_millis());
    assert!(refused <= Duration::from_secs(1), "refused {refused:?} after its loss was recorded");
    // Attempt 2, on the runner back: its tokens taken while it runs, attempt 1's still not.
    let second = wait_for(2)?;
    assert_eq!(uses(&s, &second, "a2")?, [200, 200, 200], "attempt 2's tokens, its grant live\n{}", s.log());
    assert_eq!(uses(&s, &first, "a1-later")?, [401, 401, 401], "attempt 1's, attempt 2 running");
    let (about, _) = ended(&s, 180)?;
    let attempts = about.get("attempts").and_then(Value::as_array).cloned().unwrap_or_default();
    assert_eq!(attempts.len(), 2, "{attempts:?}");
    assert_eq!(attempts.get(1).and_then(|a| a.get("conclusion")), Some(&json!("success")), "{attempts:?}");
    // Attempt 2's grant ended with its result: its tokens with it.
    assert_eq!(uses(&s, &second, "a2-after")?, [401, 401, 401], "attempt 2's, its grant ended");
    Ok(())
}

/// W05-R05, G10, docs/needs.md E3.3: a grant is written before its Assign is sent. The server's
/// first claim is held back 3 s and then fails (KILN_CHAOS claim=3s): no Assign goes, so the leg's
/// step doesn't start while the claim is held, nor after it failed. The leg goes back to its queue
/// and runs 15 s later on its next grant, the same attempt, its only one, on the only runner there
/// is (kept off none).
#[cfg(feature = "chaos")]
#[test]
fn w05_r05_a_grant_is_in_the_store_before_assign_is_sent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mark = dir.path().join("started");
    let script = format!("date +%s > \"{}\"\necho started", mark.display());
    let s = push_leg(dir.path(), &[("KILN_CHAOS", "claim=3s")], "", &script)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while !s.log().contains("the grant couldn't be written") {
        anyhow::ensure!(!mark.exists(), "the step started while its grant's claim was held back\n{}", s.log());
        anyhow::ensure!(Instant::now() < deadline, "the claim never failed\n{}", s.log());
        std::thread::sleep(Duration::from_millis(100));
    }
    let failed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    anyhow::ensure!(!mark.exists(), "the step started under the grant that wasn't written\n{}", s.log());
    let (about, log) = ended(&s, 180)?;
    let attempts = about.get("attempts").and_then(Value::as_array).cloned().unwrap_or_default();
    assert_eq!(attempts.len(), 1, "the same attempt, its only one: {attempts:?}\n{}", s.log());
    assert_eq!(attempts.first().and_then(|a| a.get("conclusion")), Some(&json!("success")), "{attempts:?}");
    assert!(log.contains("started"), "{log}");
    let started: u64 = std::fs::read_to_string(&mark)?.trim().parse()?;
    assert!(started >= failed + 14, "queued again 15 s after its claim failed: started at {started}, failed at {failed}");
    Ok(())
}

/// W05-R37, the example of docs/needs.md E3.5: the server's runner, cut off (its link down 40 s,
/// the lease 20 s), stops a stop grant at the end of its own lease, by its own clock, within a
/// second; a finish grant on it runs on to its end. The `stopper` job's legs stop when their lease
/// is lost (KILN_CHAOS stop=stopper), the `finisher`'s run to their end, as CI legs do. Each step
/// writes a line every fifth of a second for 40 s, into a file named for its job and leg.
#[cfg(feature = "chaos")]
#[test]
fn w05_r37_a_runner_cut_off_stops_its_stop_grants_at_its_lease_end() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let marks = dir.path().join("marks");
    std::fs::create_dir_all(&marks)?;
    let job = |name: &str| {
        let line = format!("if [ \"$KILN_ATTEMPT\" = 1 ]; then for i in $(seq 1 200); do echo $i >> \"{}/{name}-$KILN_JOB\"; sleep 0.2; done; fi", marks.display());
        format!("  {name}:\n    runs-on: local\n    steps:\n      - run: |\n          {line}\n")
    };
    let workflow = format!("on: push\njobs:\n{}{}", job("stopper"), job("finisher"));
    let s = push_workflow(dir.path(), &[("KILN_CHAOS", "drop=12s,down=40s,stop=stopper")], &workflow)?;
    // Each job's leg and how many lines it wrote, once both have started.
    let legs = || -> anyhow::Result<Vec<(String, String, usize)>> {
        let mut v = Vec::new();
        for e in std::fs::read_dir(&marks)? {
            let name = e?.file_name().to_string_lossy().into_owned();
            if let Some((job, leg)) = name.split_once('-') {
                v.push((job.to_owned(), leg.to_owned(), std::fs::read_to_string(marks.join(&name))?.lines().count()));
            }
        }
        v.sort();
        Ok(v)
    };
    let deadline = Instant::now() + Duration::from_secs(180);
    let stopped = loop {
        if let Some(l) = s.log().lines().find(|l| l.contains("it stops on losing it: stopping it")) {
            break l.to_owned();
        }
        anyhow::ensure!(Instant::now() < deadline, "the runner never stopped a stop grant: {:?}\n{}", legs()?, s.log());
        std::thread::sleep(Duration::from_millis(200));
    };
    // Stopped within a second of its lease's end, by the runner's clock.
    let num = |after: &str| stopped.split(after).nth(1).and_then(|t| t.split(' ').next()).and_then(|n| n.parse::<u64>().ok());
    let (end, now) = (num("(at "), num(", now "));
    assert!(matches!((end, now), (Some(e), Some(n)) if n >= e && n - e <= 1000), "{stopped}");
    eprintln!("w05_r37: the stop grant stopped {} ms after its lease's end, by the runner's clock", now.zip(end).map_or(0, |(n, e)| n - e));
    let [(finisher, f_leg, _), (stopper, s_leg, _)] = <[_; 2]>::try_from(legs()?).map_err(|l| anyhow::anyhow!("two legs: {l:?}"))?;
    assert_eq!((finisher.as_str(), stopper.as_str()), ("finisher", "stopper"));
    assert!(stopped.contains(&s_leg) && !s.log().lines().any(|l| l.contains("stopping it") && l.contains(&f_leg)), "the stopper's leg stopped, not the finisher's: {stopped}");
    // The stopper's step stops writing, short of its end; the finisher's writes on to its end.
    std::thread::sleep(Duration::from_secs(3));
    let at = legs()?;
    std::thread::sleep(Duration::from_secs(3));
    let count = |l: &[(String, String, usize)], job: &str| l.iter().find(|(j, ..)| j == job).map_or(0, |(.., n)| *n);
    assert!(count(&legs()?, "stopper") == count(&at, "stopper") && count(&at, "stopper") < 200, "the stop grant's step stopped: {at:?}");
    let deadline = Instant::now() + Duration::from_secs(60);
    while count(&legs()?, "finisher") < 200 {
        anyhow::ensure!(Instant::now() < deadline, "the finish grant ran on to its end: {:?}\n{}", legs()?, s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

/// W05-R35, the example of docs/needs.md E3.6: an operator drains the server's runner while it
/// runs legs `a` and `b` (`POST /api/agents/:name/drain`). It gets no third: `c`, which waits for
/// `a`, doesn't start when `a` ends, though the runner has room; `a` and `b` finish. Undrained, it
/// takes `c`, and the run succeeds.
#[test]
fn w05_r35_an_operators_drain_reaches_the_engine() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let marks = dir.path().join("marks");
    std::fs::create_dir_all(&marks)?;
    let step = |name: &str, secs: u32| format!("      - run: |\n          touch \"{}/{name}.started\"\n          sleep {secs}\n", marks.display());
    let workflow = format!(
        "on: push\njobs:\n  a:\n    runs-on: local\n    steps:\n{}  b:\n    runs-on: local\n    steps:\n{}  c:\n    runs-on: local\n    needs: [a]\n    steps:\n{}",
        step("a", 8),
        step("b", 8),
        step("c", 0)
    );
    let s = push_workflow(dir.path(), &[], &workflow)?;
    let started = |name: &str| marks.join(format!("{name}.started")).exists();
    until(&s, 120, "a and b running", || Ok((!(started("a") && started("b"))).then(|| s.log())))?;
    let (_, agents) = http(s.port, "GET", "/api/agents", &s.admin, None)?;
    let agents: Value = serde_json::from_str(&agents)?;
    let name = agents.as_array().and_then(|a| a.first()).or_else(|| agents.pointer("/agents/0")).and_then(|a| a.get("name")).and_then(Value::as_str).unwrap_or_default().to_owned();
    let drain = |on: bool| http(s.port, "POST", &format!("/api/agents/{name}/drain"), &s.admin, Some(&json!({ "draining": on })));
    let (st, body) = drain(true)?;
    anyhow::ensure!(st == 204, "drained: {st} {body}");
    // a and b finish; c, ready once a ends, doesn't start while the runner is drained.
    let state = |job: &str| -> anyhow::Result<Value> {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/lease&limit=5", &s.admin, None)?;
        let run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
        let legs = serde_json::from_str::<Value>(&detail)?.get("legs").cloned().unwrap_or_default();
        Ok(legs.as_array().into_iter().flatten().find(|l| l.get("key").and_then(Value::as_str) == Some(job) || l.get("name").and_then(Value::as_str) == Some(job)).and_then(|l| l.get("status")).cloned().unwrap_or_default())
    };
    until(&s, 120, "a and b finished", || {
        let (a, b) = (state("a")?, state("b")?);
        Ok((!(a == json!("succeeded") && b == json!("succeeded"))).then(|| format!("a {a}, b {b}")))
    })?;
    std::thread::sleep(Duration::from_secs(5));
    assert!(!started("c"), "drained, the runner takes no third leg: c is {}\n{}", state("c")?, s.log());
    let (st, body) = drain(false)?;
    anyhow::ensure!(st == 204, "undrained: {st} {body}");
    let (about, _) = ended(&s, 120)?;
    assert!(started("c"), "undrained, it takes c");
    assert_eq!(about.pointer("/run/conclusion"), Some(&json!("success")), "{about}");
    Ok(())
}
