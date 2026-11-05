//! W05-R34 (docs/ci.md, docs/needs.md §8): a grant is a lease its runner renews. A leg that runs
//! past two of the server's leases keeps its grant and ends on its first attempt: renewed by a lease
//! runner's `Renew`, what it sends fenced by its grant; and, built with `chaos`, by the heartbeat of
//! a runner from before leases, which says nothing of them. W05-R36, built with `chaos`: a runner
//! cut off past its leg's lease gets the leg's next attempt when it's back, and what it sends then
//! about the attempt it lost is refused. W10-R03, built with `chaos`: the lost attempt's tokens die
//! with its grant, and the next attempt's are taken. W05-R35: an operator's drain reaches the
//! engine. W05-AC16 (docs/needs.md E3.9), with two runner processes of the test's own and a lease
//! of 5 s: a runner paused past its lease has nothing it writes taken once the leg's next attempt
//! starts on the other, and its tokens no longer work; and, built with `chaos`, a stop item on a
//! runner cut off stops itself at its lease's end.

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
    push_to(&s, dir, workflow)?;
    Ok(s)
}

/// `s`'s repository `acme/lease`, made and pushed a first commit, then `workflow`.
fn push_to(s: &Server, dir: &Path, workflow: &str) -> anyhow::Result<()> {
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
    until(s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::write(work.join(".kiln/lease.yaml"), workflow)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "a leg past two leases"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    Ok(())
}

/// `script` (a shell's lines) as a step of a job's `steps`.
fn step(script: &str) -> String {
    let body: String = script.lines().map(|l| format!("          {l}\n")).collect();
    format!("      - run: |\n{body}")
}

/// A server whose grants live 5 s, without a runner of its own (`env` its faults).
fn bare(dir: &Path, env: &[(&str, &str)]) -> anyhow::Result<Server> {
    start_env(dir, &Options { agent: false, ci: "kiln_workflows = true", extra: "[sched]\nlease = \"5s\"\n" }, false, env)
}

/// A runner process of the test's own (`kiln agent`), its output in its log: killed when dropped,
/// carried on first if it was paused.
struct Runner {
    child: std::process::Child,
    log: std::path::PathBuf,
}

impl Runner {
    /// Its process sent `sig`: `STOP` pauses it as a GC pause or a machine's sleep would, `CONT`
    /// carries it on.
    fn signal(&self, sig: &str) -> anyhow::Result<()> {
        let st = Command::new("kill").arg(format!("-{sig}")).arg(self.child.id().to_string()).status()?;
        anyhow::ensure!(st.success(), "kill -{sig} {}: {st}", self.child.id());
        Ok(())
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.signal("CONT");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runner `name` of `s`, labelled `labels`, one slot, its home and folders under `dir` (`env` its
/// faults), once the server has it connected.
fn runner(s: &Server, dir: &Path, name: &str, labels: &str, env: &[(&str, &str)]) -> anyhow::Result<Runner> {
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    let home = dir.join(format!("{name}-home"));
    std::fs::create_dir_all(&home)?;
    let log = dir.join(format!("{name}.log"));
    let out = std::fs::File::create(&log)?;
    let child = common::kiln()
        .args(["agent", "--server", &format!("http://127.0.0.1:{}", s.port), "--token", &join, "--name", name, "--labels", labels, "--slots", "1", "--no-terminal"])
        .arg("--work-dir")
        .arg(dir.join(format!("{name}-work")))
        .arg("--state-dir")
        .arg(dir.join(format!("{name}-state")))
        .env("HOME", &home)
        // A machine of its own (docs/machines.md): a test's runners would all be this one.
        .env("KILN_MACHINE_ID", name)
        .env("NO_COLOR", "1")
        .env_remove("KILN_CONFIG")
        .envs(env.iter().copied())
        .stdout(std::process::Stdio::from(out.try_clone()?))
        .stderr(std::process::Stdio::from(out))
        .spawn()?;
    let r = Runner { child, log };
    until(s, 120, &format!("runner {name} to connect"), || {
        let (_, body) = http(s.port, "GET", "/api/agents", &s.admin, None)?;
        let agents: Value = serde_json::from_str(&body).unwrap_or_default();
        let list = agents.as_array().cloned().or_else(|| agents.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
        // Known by its machine id (docs/machines.md), as main's tests find theirs.
        let up = list.iter().any(|a| a.get("name").and_then(Value::as_str) == Some(common::machine(name).as_str()) && a.get("connected").and_then(Value::as_bool).unwrap_or(true));
        Ok((!up).then(|| format!("{body}\n{}", r.log())))
    })?;
    Ok(r)
}

/// One HTTP/1.1 request of `body`'s bytes with `headers`: its status and its body.
fn send(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> anyhow::Result<(u16, String)> {
    use std::io::{Read as _, Write as _};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes())?;
    s.write_all(body)?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (h, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    Ok((h.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0), rest.to_owned()))
}

/// The server's metric of writes refused under a grant that isn't their leg's (W05-R36), of kind
/// `what`: `log`, `event` (a native leg's step events among them), `step` (an older leg's), or
/// `result`.
fn refused_writes(s: &Server, what: &str) -> anyhow::Result<f64> {
    let (_, metrics) = http(s.port, "GET", "/metrics", &s.admin, None)?;
    let key = format!("kiln_ci_refused_writes_total{{what=\"{what}\"}}");
    Ok(metrics.lines().find_map(|l| l.strip_prefix(key.as_str())).and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(0.0))
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
struct Tokens {
    runtime: String,
    id: String,
    job: String,
    run: String,
    leg: String,
}

/// Attempt `n`'s tokens, once its step has written them under `out`.
fn tokens(out: &Path, n: u32) -> Option<Tokens> {
    let d = out.join(n.to_string());
    let r = |k: &str| std::fs::read_to_string(d.join(k)).ok();
    Some(Tokens { runtime: r("runtime")?, id: r("id")?, job: r("job")?, run: r("run")?, leg: r("leg")? })
}

/// How `t`'s three uses are answered (their statuses): an artifact's upload started (`name`) with
/// its runtime token, the repository's git ref read with its GITHUB_TOKEN, an ID token asked with
/// its request token.
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
/// writes a line every fifth of a second for 30 s, then `end`, into a file named for its job and
/// leg: 30 s by the clock, not a count of lines, since a loop that starts a process each turn runs
/// slower on macOS. The runner is back 40 s after its link dropped, and the server then has it stop
/// the finisher's attempt, its grant lost, so the step ends well before.
#[cfg(feature = "chaos")]
#[test]
fn w05_r37_a_runner_cut_off_stops_its_stop_grants_at_its_lease_end() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let marks = dir.path().join("marks");
    std::fs::create_dir_all(&marks)?;
    let job = |name: &str| {
        let file = format!("{}/{name}-$KILN_JOB", marks.display());
        let line = format!("if [ \"$KILN_ATTEMPT\" = 1 ]; then end=$(( $(date +%s) + 30 )); while [ $(date +%s) -lt $end ]; do echo tick >> \"{file}\"; sleep 0.2; done; echo end >> \"{file}\"; fi");
        format!("  {name}:\n    runs-on: local\n    steps:\n      - run: |\n          {line}\n")
    };
    let workflow = format!("on: push\njobs:\n{}{}", job("stopper"), job("finisher"));
    let s = push_workflow(dir.path(), &[("KILN_CHAOS", "drop=12s,down=40s,stop=stopper")], &workflow)?;
    // Each job's leg, how many lines it wrote, and whether it wrote its `end`, once both have started.
    let legs = || -> anyhow::Result<Vec<(String, String, usize, bool)>> {
        let mut v = Vec::new();
        for e in std::fs::read_dir(&marks)? {
            let name = e?.file_name().to_string_lossy().into_owned();
            if let Some((job, leg)) = name.split_once('-') {
                let text = std::fs::read_to_string(marks.join(&name))?;
                v.push((job.to_owned(), leg.to_owned(), text.lines().count(), text.lines().any(|l| l == "end")));
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
    let [(finisher, f_leg, ..), (stopper, s_leg, ..)] = <[_; 2]>::try_from(legs()?).map_err(|l| anyhow::anyhow!("two legs: {l:?}"))?;
    assert_eq!((finisher.as_str(), stopper.as_str()), ("finisher", "stopper"));
    assert!(stopped.contains(&s_leg) && !s.log().lines().any(|l| l.contains("stopping it") && l.contains(&f_leg)), "the stopper's leg stopped, not the finisher's: {stopped}");
    // The stopper's step stops writing, short of its end; the finisher's writes on to its end.
    std::thread::sleep(Duration::from_secs(3));
    let at = legs()?;
    std::thread::sleep(Duration::from_secs(3));
    let of = |l: &[(String, String, usize, bool)], job: &str| l.iter().find(|(j, ..)| j == job).map_or((0, false), |(_, _, n, end)| (*n, *end));
    assert!(of(&legs()?, "stopper") == of(&at, "stopper") && !of(&at, "stopper").1, "the stop grant's step stopped, short of its end: {at:?}");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !of(&legs()?, "finisher").1 {
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

/// `url` (an absolute URL on the server) as its path and query.
fn path_of(url: &str) -> String {
    url.split_once("://").and_then(|(_, rest)| rest.find('/').map(|i| rest.get(i..).unwrap_or("/").to_owned())).unwrap_or_else(|| url.to_owned())
}

/// `t`'s attempt starts uploading artifact `name` as upload-artifact does (`CreateArtifact`, with
/// its runtime token): the answer's status, and where the artifact's bytes go.
fn create(s: &Server, t: &Tokens, name: &str) -> anyhow::Result<(u16, Option<String>)> {
    let body = json!({ "workflow_run_backend_id": t.run, "workflow_job_run_backend_id": t.leg, "name": name, "version": 4 });
    let (st, answer) = http(s.port, "POST", "/twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact", &t.runtime, Some(&body))?;
    let url = serde_json::from_str::<Value>(&answer).ok().and_then(|v| v.get("signed_upload_url").and_then(Value::as_str).map(path_of));
    Ok((st, url))
}

/// `t`'s upload of artifact `name` finished: `bytes` put at `url` (one `PUT` of a `BlockBlob`),
/// then finalized with its runtime token (`FinalizeArtifact`, their size and hash). Each one's
/// status.
fn finish(s: &Server, t: &Tokens, name: &str, url: &str, bytes: &[u8]) -> anyhow::Result<[u16; 2]> {
    use sha2::Digest as _;
    let (put, _) = send(s.port, "PUT", url, &[("x-ms-blob-type", "BlockBlob")], bytes)?;
    let hash = format!("sha256:{}", hex::encode(sha2::Sha256::digest(bytes)));
    let body = json!({ "workflow_run_backend_id": t.run, "workflow_job_run_backend_id": t.leg, "name": name, "size": bytes.len().to_string(), "hash": hash });
    let (fin, _) = http(s.port, "POST", "/twirp/github.actions.results.api.v1.ArtifactService/FinalizeArtifact", &t.runtime, Some(&body))?;
    Ok([put, fin])
}

/// W05-AC16, the example of docs/needs.md E3.9 (W05-R36, R34, W10-R03): a runner cut off past its
/// lease has nothing it writes taken once the leg's next attempt starts, and its tokens no longer
/// work. A server whose grants live 5 s, and two runners of the test's own. Attempt 1 runs on A, B
/// beside it: by its third step it has written its tokens, printed 40 lines and finished two
/// steps, and it has started uploading `dist`. A is paused (SIGSTOP, as a GC pause or a sleep
/// would): its grant runs out unrenewed, and attempt 2 starts on B. A paused, attempt 1's tokens
/// are refused (its upload, its GITHUB_TOKEN on kiln's git API, its ID token); attempt 2's are
/// taken, its `dist` uploaded whole; and attempt 1's step, running on (a process of its own),
/// prints 10 lines more and ends failing (exit 3), so that anything of it taken would show. A
/// carries on 17 s after its pause, its connection dropped meanwhile (QUIC's idle timeout is 10 s),
/// attempt 2 still running on B: what A sends of attempt 1 (those lines, its step's end, its
/// result) is refused under attempt 1's grant, the upload it finishes is refused, and it's told to
/// stop attempt 1. Then attempt 2 ends, and the leg's records are its own: its result, its steps,
/// its one artifact, and no line attempt 1 printed after attempt 2 started (those it printed while
/// its grant was live stay as they were taken).
#[test]
fn w05_ac16_a_runner_cut_off_past_its_lease_writes_nothing_after_the_next_attempt_starts() -> anyhow::Result<()> {
    use sha2::Digest as _;
    let dir = tempfile::tempdir()?;
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out)?;
    let o = out.display();
    // Its tokens where the test reads them, then 20 lines; 20 more; and a third step that waits:
    // attempt 1's until the test says attempt 2 runs, then prints 10 lines and fails; attempt 2's
    // until the test is done with it, so that its grant is live while A sends what it held.
    let first = format!(
        "d=\"{o}/$KILN_ATTEMPT\"\nmkdir -p \"$d.tmp\"\nprintf %s \"$ACTIONS_RUNTIME_TOKEN\" > \"$d.tmp/runtime\"\nprintf %s \"$ACTIONS_ID_TOKEN_REQUEST_TOKEN\" > \"$d.tmp/id\"\nprintf %s \"$KILN_TOKEN\" > \"$d.tmp/job\"\nprintf %s \"$KILN_RUN\" > \"$d.tmp/run\"\nprintf %s \"$KILN_JOB\" > \"$d.tmp/leg\"\nmv \"$d.tmp\" \"$d\"\nfor i in $(seq 1 20); do echo \"attempt $KILN_ATTEMPT line $i\"; done"
    );
    let second = "for i in $(seq 21 40); do echo \"attempt $KILN_ATTEMPT line $i\"; done";
    let third = format!(
        "if [ \"$KILN_ATTEMPT\" = 1 ]; then\n  touch \"{o}/third\"\n  while [ ! -e \"{o}/go\" ]; do sleep 0.2; done\n  for i in $(seq 1 10); do echo \"attempt 1 late line $i\"; done\n  exit 3\nfi\nwhile [ ! -e \"{o}/done\" ]; do sleep 0.2; done\necho \"attempt 2 ran\""
    );
    let workflow = format!("on: push\njobs:\n  cut:\n    runs-on: cut\n    permissions:\n      contents: read\n      id-token: write\n    steps:\n{}{}{}", step(&first), step(second), step(&third));
    let s = bare(dir.path(), &[])?;
    let a = runner(&s, dir.path(), "a", "cut", &[])?;
    push_to(&s, dir.path(), &workflow)?;
    let wait = |what: &str, secs: u64, ok: &dyn Fn() -> anyhow::Result<bool>| -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !ok()? {
            anyhow::ensure!(Instant::now() < deadline, "{what}\n{}\n--- a:\n{}", s.log(), a.log());
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    };
    // Attempt 1 on A; B there beside it.
    wait("attempt 1 never wrote its tokens", 180, &|| Ok(tokens(&out, 1).is_some()))?;
    let one = tokens(&out, 1).ok_or_else(|| anyhow::anyhow!("attempt 1's tokens"))?;
    let _b = runner(&s, dir.path(), "b", "cut", &[])?;
    // Its grant live, its upload starts.
    let (st, url) = create(&s, &one, "dist")?;
    anyhow::ensure!(st == 200, "attempt 1's upload, its grant live: {st}\n{}", s.log());
    let url = url.ok_or_else(|| anyhow::anyhow!("attempt 1's upload has nowhere to go"))?;
    // Two steps done, 40 lines printed: A paused.
    wait("attempt 1's third step never started", 120, &|| Ok(out.join("third").exists()))?;
    anyhow::ensure!(tokens(&out, 2).is_none(), "attempt 1's grant was lost before A was paused (the machine too loaded for a lease of 5 s?)\n{}", s.log());
    a.signal("STOP")?;
    let paused = Instant::now();
    // Its grant runs out; attempt 2 starts on B.
    wait("attempt 2 never started", 120, &|| Ok(tokens(&out, 2).is_some()))?;
    let two = tokens(&out, 2).ok_or_else(|| anyhow::anyhow!("attempt 2's tokens"))?;
    eprintln!("w05_ac16: attempt 2 started on B {} ms after A was paused", paused.elapsed().as_millis());
    let (_, body) = http(s.port, "GET", &format!("/api/ci/legs/{}", one.leg), &s.admin, None)?;
    let now: Value = serde_json::from_str(&body)?;
    assert_eq!(now.pointer("/attempts/0/class"), Some(&json!("runner-lost")), "attempt 1 lost: {now}");
    assert_eq!((now.pointer("/attempts/0/runner"), now.pointer("/attempts/1/runner")), (Some(&json!("a")), Some(&json!("b"))), "{now}");
    // Attempt 1's tokens refused; attempt 2's taken, its `dist` uploaded whole.
    assert_eq!(uses(&s, &one, "dist-1")?, [401, 401, 401], "attempt 1's tokens, its grant lost\n{}", s.log());
    let dist = b"attempt 2's dist.tar";
    let (st, url2) = create(&s, &two, "dist")?;
    let url2 = url2.ok_or_else(|| anyhow::anyhow!("attempt 2's upload has nowhere to go: {st}"))?;
    assert_eq!(finish(&s, &two, "dist", &url2, dist)?, [201, 200], "attempt 2's dist, its grant live\n{}", s.log());
    // Attempt 1's step, running on while A is paused, prints its late lines and fails.
    std::fs::write(out.join("go"), "")?;
    std::thread::sleep(Duration::from_secs(17).saturating_sub(paused.elapsed()));
    a.signal("CONT")?;
    // What A held and ran on goes to the server under attempt 1's grant: refused, each kind.
    let kinds = || ["log", "event", "result"].map(|k| (k, refused_writes(&s, k).unwrap_or(0.0)));
    wait("A's late writes, each kind refused", 120, &|| Ok(kinds().iter().all(|(_, n)| *n >= 1.0))).map_err(|e| anyhow::anyhow!("{e}\nrefused so far: {:?}", kinds()))?;
    let [_, fin] = finish(&s, &one, "dist", &url, b"attempt 1's dist.tar")?;
    assert_eq!(fin, 401, "attempt 1's upload, finished once A carried on");
    assert_eq!(uses(&s, &one, "dist-1")?, [401, 401, 401], "attempt 1's tokens, A back");
    let server_log = dir.path().join("server.log");
    wait("A was never told to stop attempt 1", 60, &|| Ok(std::fs::read_to_string(&server_log).unwrap_or_default().contains("told to stop it")))?;
    // Attempt 2 ends; the leg's records are its own.
    std::fs::write(out.join("done"), "")?;
    let (about, log) = ended(&s, 120)?;
    let attempts = about.get("attempts").and_then(Value::as_array).cloned().unwrap_or_default();
    let field = |i: usize, k: &str| attempts.get(i).and_then(|x| x.get(k)).cloned().unwrap_or_default();
    assert_eq!(attempts.len(), 2, "{attempts:?}");
    assert_eq!((field(0, "conclusion"), field(0, "class"), field(0, "runner")), (json!("failure"), json!("runner-lost"), json!(common::machine("a"))), "attempt 1 lost with its lease, its late result taken for nothing: {attempts:?}");
    assert!(field(0, "error").as_str().is_some_and(|e| e.contains("lease ran out")), "{attempts:?}");
    assert_eq!((field(1, "conclusion"), field(1, "runner")), (json!("success"), json!(common::machine("b"))), "{attempts:?}");
    assert_eq!(about.pointer("/run/conclusion"), Some(&json!("success")), "{about}");
    let steps: Vec<Value> = about.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.get("status").cloned()).collect();
    assert!(!steps.is_empty() && steps.iter().all(|st| *st == json!("succeeded")), "the steps are attempt 2's, not attempt 1's failed late: {steps:?}");
    assert!(log.contains("attempt 2 ran"), "attempt 2's output is taken: {log}");
    assert!(!log.contains("attempt 1 late line"), "nothing attempt 1 printed after attempt 2 started is in the leg's log: {log}");
    let run = about.pointer("/run/id").and_then(Value::as_str).unwrap_or_default();
    let (_, body) = http(s.port, "GET", &format!("/api/ci/runs/{run}/artifacts"), &s.admin, None)?;
    let artifacts = serde_json::from_str::<Value>(&body)?.get("artifacts").and_then(Value::as_array).cloned().unwrap_or_default();
    let digest = json!(format!("sha256:{}", hex::encode(sha2::Sha256::digest(dist))));
    assert!(artifacts.len() == 1 && artifacts.iter().all(|x| x.get("name") == Some(&json!("dist")) && x.get("digest") == Some(&digest)), "its one artifact, attempt 2's dist: {artifacts:?}");
    Ok(())
}

/// W05-AC16's second case, docs/needs.md E3.9 (W05-R37): a stop item on runner A, cut off, stops
/// itself at its lease's end. The server's `stopper` legs stop on losing their lease (KILN_CHAOS
/// stop=stopper), its grants living 5 s. A, a runner process of the test's own, takes the leg as
/// it connects and drops its link 6 s later, for 20 s (KILN_CHAOS drop=6s,down=20s): its renewals
/// unanswered, it stops the leg at its lease's end by its own clock, and the step's process (a
/// shell printing a line every fifth of a second) is gone within a second of that end.
#[cfg(feature = "chaos")]
#[test]
fn w05_ac16_a_stop_grant_cut_off_stops_itself() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out)?;
    let script = format!("echo $$ > \"{o}/pid.tmp\"\nmv \"{o}/pid.tmp\" \"{o}/pid\"\nwhile true; do echo tick; sleep 0.2; done", o = out.display());
    let s = bare(dir.path(), &[("KILN_CHAOS", "stop=stopper")])?;
    push_to(&s, dir.path(), &format!("on: push\njobs:\n  stopper:\n    runs-on: cut\n    steps:\n{}", step(&script)))?;
    let a = runner(&s, dir.path(), "a", "cut", &[("KILN_CHAOS", "drop=6s,down=20s")])?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let pid = loop {
        if let Ok(p) = std::fs::read_to_string(out.join("pid")) {
            break p.trim().to_owned();
        }
        anyhow::ensure!(Instant::now() < deadline, "the stop item's step never started\n{}\n--- a:\n{}", s.log(), a.log());
        std::thread::sleep(Duration::from_millis(50));
    };
    // How its process takes signals (Linux says), for when it's slow to go.
    let signals = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default().lines().filter(|l| l.starts_with("Sig") || l.starts_with("PPid") || l.starts_with("NSpid")).collect::<Vec<_>>().join("; ");
    // A, cut off, stops it: saying where its lease ended and when it stopped it, by its clock.
    let deadline = Instant::now() + Duration::from_secs(120);
    let (stopped, seen) = loop {
        if let Some(l) = a.log().lines().find(|l| l.contains("it stops on losing it: stopping it")) {
            break (l.to_owned(), Instant::now());
        }
        anyhow::ensure!(Instant::now() < deadline, "A never stopped its stop grant\n{}\n--- a:\n{}", s.log(), a.log());
        std::thread::sleep(Duration::from_millis(20));
    };
    let alive = |pid: &str| Command::new("kill").args(["-0", pid]).stderr(std::process::Stdio::null()).status().is_ok_and(|st| st.success());
    let gone = loop {
        if !alive(&pid) {
            break seen.elapsed();
        }
        anyhow::ensure!(seen.elapsed() < Duration::from_secs(10), "the step's process {pid} still runs 10 s after A stopped it: {stopped}\n--- a:\n{}", a.log());
        std::thread::sleep(Duration::from_millis(10));
    };
    let num = |after: &str| stopped.split(after).nth(1).and_then(|t| t.split(' ').next()).and_then(|n| n.parse::<u64>().ok());
    let (Some(end), Some(now)) = (num("(at "), num(", now ")) else { anyhow::bail!("A's word on its lease, unread: {stopped}") };
    let late = Duration::from_millis(now.saturating_sub(end)) + gone;
    eprintln!("w05_ac16: the stop item's process gone {} ms after A's lease end ({} ms to its stop by A's clock, then {} ms)", late.as_millis(), now.saturating_sub(end), gone.as_millis());
    assert!(now >= end && late <= Duration::from_secs(1), "gone {late:?} after its lease's end: {stopped}\nits signals: {signals}\n--- a:\n{}", a.log().lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"));
    Ok(())
}
