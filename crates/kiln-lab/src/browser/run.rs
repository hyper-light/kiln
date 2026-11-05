//! Rounds of the browser lab (docs/browser-lab.md, "A round"; plan 01a1a832e9cc steps 4, 7 and 8):
//! for each condition (a profile and its loss), each round draws one seed every target meets, and
//! the targets take their turns in an order that turns round by round. Each target is started fresh
//! for its round; each of its browsers runs each case as a job: K browsers, one on each client
//! link, the server's background CPU measured first with nobody attached, then the job's
//! operations while the server's side (an agent's turn, the origin's replay) runs on its schedule.

use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use super::cells::{self, CpuTick, Sample, Tick};
use super::link::{self, Condition, Ns};
pub use super::runfile::{BrowserRun, JobRecord, Settings, Workload, round_seed, workload};
use super::servers::{self, Certs, Env, Running};
use super::targets::{Server, Target};
use super::topo::{Lab, Scopes};
use crate::standin::now_us;

fn machine() -> Value {
    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let psi = std::fs::read_to_string("/proc/pressure/cpu").unwrap_or_default();
    json!({"load": load.split_whitespace().take(3).collect::<Vec<_>>().join(" "), "cpu_pressure": psi.lines().next().unwrap_or_default(), "host": std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim()})
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(p: &Path) -> Vec<T> {
    std::fs::read_to_string(p).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// The operations a case opens on a target, as the probe page takes them.
fn ops(t: &Target, case: &str, w: &Workload, kiln: Option<&servers::KilnLab>, logs: bool) -> Vec<Value> {
    let sid = |i: usize| kiln.and_then(|k| k.sessions.get(i).cloned()).unwrap_or_else(|| "lab".to_owned());
    let base = |kind: &str| if t.carrier == "fetch-stream" { format!("/fetch-stream/{kind}") } else { format!("/stream/{kind}") };
    // A terminal's and a log's frames are text lines, not JSON, as the UI reads them.
    let stream = |name: &str, path: String, packed: bool| json!({"case": case, "name": name, "carrier": if t.server == Server::Kiln { "ws" } else { t.carrier.as_str() }, "path": path, "packed": packed, "json": !matches!(name, "terminal" | "log")});
    match (case, t.server) {
        ("stream", Server::Kiln) => {
            let mut v = vec![
                stream("session", format!("/api/harness/sessions/{}/ws?z=gzip&patch=state&patch=changes", sid(0)), true),
                stream("terminal", "/api/terminal/ws?target=local&cols=120&rows=40".to_owned(), false),
                stream("events", "/api/events".to_owned(), false),
            ];
            if let Some(job) = kiln.and_then(|k| k.job.clone()).filter(|_| logs) {
                v.push(stream("log", format!("/api/jobs/{job}/logs/ws?step=0"), false));
            }
            v
        }
        ("stream", _) => ["session", "terminal", "events", "log"].iter().filter(|k| logs || **k != "log").map(|k| stream(k, base(k), false)).collect(),
        ("burst", Server::Kiln) => vec![stream("turn", format!("/api/harness/sessions/{}/ws?z=gzip&patch=state&patch=changes", sid(1)), true)],
        ("burst", _) => vec![stream("turn", base("session"), false)],
        _ => {
            // Reads go to the third session, which no turn reaches in a run, so its snapshot is
            // the one step 3 recorded (the same seed and folder say the same things).
            let id = sid(2);
            // The reads the UI's pages made in step 3's recording (docs/browser-lab/catalogue.json).
            let reads = ["/api/agents", "/api/harness/sessions", "/api/overview", "/api/me", "/api/push/notices"];
            let mut requests: Vec<Value> = reads.iter().map(|p| json!({"name": format!("GET {p}"), "path": p})).collect();
            requests.push(if t.server == Server::Kiln {
                json!({"name": "session snapshot", "ws_snapshot": true, "path": format!("/api/harness/sessions/{id}/ws?z=gzip&patch=state&patch=changes")})
            } else {
                json!({"name": "session snapshot", "ndjson": true, "path": "/api/snapshots/lab"})
            });
            // The write goes to the first session, so the third's snapshot stays the recorded one.
            requests.push(json!({"name": "POST /api/harness/sessions/:id/command", "method": "POST", "path": format!("/api/harness/sessions/{}/command", sid(0)), "body": json!({"type": "send", "id": "{id}", "text": "lab:quiet"}).to_string()}));
            #[allow(clippy::cast_precision_loss)]
            let n = (w.duration_s * 1000 / w.request_ms.max(1)) as usize;
            let schedule: Vec<u64> = (0..n as u64).map(|i| i * w.request_ms).collect();
            vec![json!({"case": "request", "name": "requests", "requests": requests, "schedule_ms": schedule, "start_in_ms": 500})]
        }
    }
}

struct Ctx<'a> {
    lab: &'a Lab,
    certs: &'a Certs,
    s: &'a Settings,
    out: &'a Path,
    /// The job's number in the run: a write's ids carry it, so no two jobs against one server send
    /// the same id (kiln answers a repeated id as a repeat, 14 bytes longer).
    job_n: u64,
}

fn spawn_driver(lab: &Lab, web: &Path, i: usize, browser: &str, job: &Path) -> anyhow::Result<Child> {
    let log = std::fs::File::create(job.with_extension("log"))?;
    let args = ["job".to_owned(), job.display().to_string()];
    Ok(super::topo::driver(lab, i, browser, web, &args, job.parent())?.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log).spawn()?)
}

fn cpu_now(r: &Running) -> CpuTick {
    let (server_us, origin_us) = r.cpu_us();
    CpuTick { t_us: now_us(), server_us, origin_us: if r.kiln.is_some() { 0 } else { origin_us } }
}

/// One job: K browsers of `browser` on `t` running `case`.
#[allow(clippy::too_many_arguments)]
fn job(c: &mut Ctx<'_>, r: &Running, t: &Target, browser: &str, case: &str, cond: &Condition, round: usize, turn_id: &mut u64) -> anyhow::Result<JobRecord> {
    let mut w = workload(case, &cond.profile);
    if let Some(d) = c.s.duration_s {
        w.duration_s = d;
    }
    let dir = c.out.join("raw").join(cond.name().replace('/', "-")).join(format!("r{round:02}")).join(format!("{}-{browser}-{case}", t.name));
    std::fs::create_dir_all(&dir)?;
    // The background: the server with nobody attached, once it is at rest (a server just started
    // is still settling in: two windows in a row within a quarter, or 2 ms a second, of each other;
    // 90 s at most), two windows.
    let bg = Duration::from_secs(c.s.background_s.max(1));
    let rate = |x: &CpuTick, y: &CpuTick| {
        #[allow(clippy::cast_precision_loss)]
        let v = (y.server_us + y.origin_us).saturating_sub(x.server_us + x.origin_us) as f64 / (y.t_us.saturating_sub(x.t_us).max(1) as f64 / 1e6);
        v
    };
    let settle_until = Instant::now() + Duration::from_secs(90);
    let mut a = cpu_now(r);
    std::thread::sleep(bg);
    let mut b = cpu_now(r);
    loop {
        std::thread::sleep(bg);
        let d = cpu_now(r);
        let (x, y) = (rate(&a, &b), rate(&b, &d));
        if (x - y).abs() <= (x.max(y) * 0.25).max(2000.0) || Instant::now() > settle_until {
            break;
        }
        (a, b) = (b, d);
    }
    let d = cpu_now(r);
    let before = rate(&a, &d);
    let noise = (rate(&a, &b) - rate(&b, &d)).abs();
    let ops = ops(t, case, &w, r.kiln.as_ref(), c.s.logs);
    // A capture on each client link, headers only, for as long as the job runs.
    let mut captures = Vec::new();
    if c.s.capture {
        for i in 0..c.s.k {
            let pcap = dir.join(format!("c{i}.pcap"));
            let argv = vec!["python3".to_owned(), "-I".to_owned(), c.s.web.join("capture.py").display().to_string(), "eth0".to_owned(), pcap.display().to_string()];
            captures.push((i, pcap, c.lab.command(Ns::Client(i), &argv)?.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // Where the server's access log stands before the job: the job's own requests come after.
    let log_from = r.access_log.as_ref().and_then(|p| std::fs::metadata(p).ok()).map_or(0, |m| m.len());
    let mut drivers = Vec::new();
    for i in 0..c.s.k {
        let jd = dir.join(format!("c{i}"));
        std::fs::create_dir_all(&jd)?;
        let job = json!({
            "name": format!("{} {} {} {} r{round} c{i}", cond.name(), t.name, browser, case),
            "browser": browser,
            "origin": t.origin(),
            "login": r.kiln.as_ref().map(|k| json!({"path": "/api/auth/login", "token": k.admin})),
            "control": format!("wss://{}:{}/", link::control_server_ip(i), servers::CONTROL_PORT),
            "chromium_args": t.chromium_args(&c.certs.spki),
            "protocol_path": if t.server == Server::Kiln { "/api/me".to_owned() } else { "/api/agents".to_owned() },
            "ops": ops.iter().map(|o| { let mut o = o.clone(); if let Some(m) = o.as_object_mut() { m.insert("client".into(), json!(i)); m.insert("job".into(), json!(c.job_n)); } o }).collect::<Vec<_>>(),
            "duration_s": w.duration_s,
            "tail_s": w.tail_s,
            "window_ms": c.s.window_ms,
            "settle_ms": 1500,
            "keylog": c.s.capture.then(|| jd.join("keys.log").display().to_string()),
        });
        let path = jd.join("job.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&job)?)?;
        drivers.push((i, spawn_driver(c.lab, &c.s.web, i, browser, &path)?, jd));
    }
    // The server's CPU, every window, while the job runs.
    let stop = Arc::new(AtomicBool::new(false));
    let cpu_file = dir.join("cpu.jsonl");
    let sampler = {
        let stop = stop.clone();
        let scope = r.scope.clone();
        let origin = if r.kiln.is_some() { None } else { r.origin_scope.clone() };
        let every = Duration::from_millis(c.s.window_ms.max(10));
        std::thread::spawn(move || {
            let mut out = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let server_us = Scopes::cpu_us(&scope).unwrap_or(0);
                let origin_us = origin.as_ref().and_then(|o| Scopes::cpu_us(o).ok()).unwrap_or(0);
                out.push(CpuTick { t_us: now_us(), server_us, origin_us });
                std::thread::sleep(every);
            }
            out
        })
    };
    // Every browser ready (its operations open, its link quiet) before the server's side starts.
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut ready_us = 0u64;
    for (_, child, jd) in &mut drivers {
        loop {
            if let Ok(t) = std::fs::read_to_string(jd.join("ready")) {
                ready_us = ready_us.max(t.trim().parse().unwrap_or(0));
                break;
            }
            if child.try_wait()?.is_some() || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let started = Instant::now();
    let until = started + Duration::from_secs(w.duration_s);
    // The server's side.
    match (case, &r.kiln, &r.replay_sock) {
        ("stream", Some(k), _) => {
            if let Some(s) = k.sessions.first() {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let n = (w.session_per_s * w.duration_s as f64) as u64;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let every = (1000.0 / w.session_per_s.max(0.01)) as u64;
                *turn_id += 1;
                servers::turn(k, s, n, every, *turn_id)?;
            }
        }
        ("stream", None, Some(sock)) => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            for (kind, per_s) in [("session", w.session_per_s), ("terminal", w.lines_per_s), ("log", w.lines_per_s), ("events", w.session_per_s)].into_iter().filter(|(k, _)| c.s.logs || *k != "log") {
                let n = (per_s * w.duration_s as f64) as u64;
                let every = (1000.0 / per_s.max(0.01)) as u64;
                let _ = servers::replay(sock, kind, n, every, 0);
            }
        }
        ("burst", _, _) => {
            let mut from = 0;
            while Instant::now() + Duration::from_secs(w.burst_gap_s / 2) < until {
                match (&r.kiln, &r.replay_sock) {
                    (Some(k), _) => {
                        if let Some(s) = k.sessions.get(1).or(k.sessions.first()) {
                            *turn_id += 1;
                            servers::turn(k, s, w.burst_events, w.burst_every_ms, *turn_id)?;
                        }
                    }
                    (None, Some(sock)) => {
                        servers::replay(sock, "session", w.burst_events, w.burst_every_ms, from)?;
                        from += w.burst_events;
                    }
                    _ => {}
                }
                std::thread::sleep(Duration::from_secs(w.burst_gap_s));
            }
        }
        ("request", _, _) => {
            if let Some(at) = c.s.stall_s {
                std::thread::sleep(Duration::from_secs(at));
                // Every process of the server held for 2 s, as a stalled server is.
                let pids: Vec<String> = r.pids().iter().map(|(_, p)| p.to_string()).collect();
                let _ = std::process::Command::new("kill").arg("-STOP").args(&pids).status();
                std::thread::sleep(Duration::from_secs(2));
                let _ = std::process::Command::new("kill").arg("-CONT").args(&pids).status();
            }
        }
        _ => {}
    }
    let end = until + Duration::from_secs(w.tail_s + 120);
    let mut blocked = false;
    // How each driver ended: its exit status, and whether the lab stopped it at the job's end.
    let mut ended: Vec<(Option<std::process::ExitStatus>, bool)> = Vec::new();
    for (_, child, _) in &mut drivers {
        let mut killed = false;
        while child.try_wait()?.is_none() {
            // A link that drops UDP partway (goal 5's middlebox): from its time on.
            if let link::Udp::DroppedAfter { after_s } = cond.profile.udp
                && !blocked
                && started.elapsed() >= Duration::from_secs(after_s)
            {
                for i in 0..c.s.k {
                    c.lab.block_udp(i)?;
                }
                blocked = true;
            }
            if Instant::now() > end {
                let _ = child.kill();
                killed = true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        ended.push((child.try_wait()?, killed));
    }
    stop.store(true, Ordering::Relaxed);
    let cpu = sampler.join().map_err(|_| anyhow::anyhow!("the CPU sampler stopped"))?;
    let server_protocols = r.access_log.as_ref().map(|p| logged_protocols(p, log_from)).unwrap_or_default();
    // The background again after the job, nobody attached: the job's is the mean of the two.
    let e = cpu_now(r);
    std::thread::sleep(bg);
    let f = cpu_now(r);
    let after = rate(&e, &f);
    let background = f64::midpoint(before, after);
    std::fs::write(&cpu_file, cpu.iter().filter_map(|c| serde_json::to_string(c).ok()).collect::<Vec<_>>().join("\n"))?;
    let mut clients = Vec::new();
    let mut protocols = std::collections::BTreeMap::new();
    let mut errors = Vec::new();
    let mut calib: Option<f64> = None;
    let mut grain: Option<f64> = None;
    let mut no_result = 0usize;
    for (n, (i, _, jd)) in drivers.iter().enumerate() {
        let samples: Vec<Sample> = read_jsonl(&jd.join("samples.jsonl"));
        let ticks: Vec<Tick> = read_jsonl(&jd.join("counters.jsonl"));
        let res: Value = std::fs::read(jd.join("result.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        // A driver that left no result didn't measure its link: a failure of the lab's, named,
        // never a link whose operations had nothing to say.
        if res.is_null() {
            no_result += 1;
            let (status, killed) = ended.get(n).copied().unwrap_or((None, false));
            let how = if killed { "stopped by the lab at the job's end".to_owned() } else { status.map_or_else(|| "its end unknown".to_owned(), |s| format!("ended with {s}")) };
            errors.push(format!("c{i}: the driver left no result ({how}; {} samples)", samples.len()));
        }
        if let Some(p) = res.get("protocols").and_then(Value::as_object) {
            for (k, v) in p {
                protocols.insert(k.clone(), v.as_str().unwrap_or_default().to_owned());
            }
        }
        for e in res.get("errors").and_then(Value::as_array).cloned().unwrap_or_default().iter().take(5) {
            errors.push(format!("c{i}: {}", e.get("error").and_then(Value::as_str).unwrap_or_default().chars().take(300).collect::<String>()));
        }
        if let Some(r) = res.pointer("/calibration/residuals_us").and_then(Value::as_array) {
            let mut v: Vec<f64> = r.iter().filter_map(Value::as_f64).collect();
            v.sort_by(f64::total_cmp);
            if let Some(p) = cells::quantile(&v, 0.99) {
                calib = Some(calib.map_or(p, |c: f64| c.max(p)));
            }
        }
        if let Some(g) = res.get("timer_grain_us").and_then(Value::as_f64) {
            grain = Some(grain.map_or(g, |c: f64| c.max(g)));
        }
        clients.push((samples, ticks));
    }
    // The capture's bytes against the counters', over the window both saw.
    let mut capture = None;
    for (i, pcap, mut child) in captures {
        // SIGTERM: it writes what it holds and ends.
        let _ = std::process::Command::new("kill").arg("-TERM").arg(child.id().to_string()).status();
        let _ = child.wait();
        if let (Ok(bytes), Some((_, ticks))) = (std::fs::read(&pcap), clients.get(i))
            && let (Some(f), Some(l)) = (ticks.iter().find(|t| t.t_us >= ready_us), ticks.last())
        {
            let seen = super::pcap::bytes_between(&bytes, f.t_us, l.t_us);
            let counted = (l.rx_bytes + l.tx_bytes).saturating_sub(f.rx_bytes + f.tx_bytes);
            let (a, b) = capture.unwrap_or((0, 0));
            capture = Some((a + seen, b + counted));
        }
    }
    // Every capture is stopped first (above), so a failed job leaves nothing running.
    if no_result > 0 && no_result == drivers.len() {
        anyhow::bail!("no driver left a result: {}", errors.join("; "));
    }
    let mut nums = cells::job_numbers(&clients, &cpu, background, ready_us);
    #[allow(clippy::cast_precision_loss)]
    nums.numbers.insert("drivers.no_result".into(), no_result as f64);
    std::fs::write(dir.join("times_ms.json"), serde_json::to_vec(&nums.times_ms)?)?;
    Ok(JobRecord {
        condition: cond.name(),
        profile: cond.profile.name.clone(),
        loss: cond.loss.name(),
        seed: cond.seed,
        round,
        target: t.name.clone(),
        browser: browser.to_owned(),
        case: case.to_owned(),
        k: c.s.k,
        numbers: nums.numbers,
        protocols,
        server_protocols,
        calibration_p99_us: calib,
        timer_grain_us: grain,
        background_us_per_s: background,
        noise_us_per_s: noise.max((before - after).abs()),
        errors,
        capture,
        machine: machine(),
        started_us: now_us(),
    })
}

/// Runs the settings' matrix into `out` (run.json, rewritten as each job ends).
pub fn run(s: &Settings, out: &Path) -> anyhow::Result<BrowserRun> {
    std::fs::create_dir_all(out)?;
    let mut lab = Lab::up(s.k)?;
    let scopes = Scopes::mine()?;
    let certs = servers::certs(&out.join("certs"))?;
    let bin = super::standins::install(&out.join("bin"))?;
    let mut run = BrowserRun { command: std::env::args().collect::<Vec<_>>().join(" "), started_us: now_us(), settings: s.clone(), certs_spki: certs.spki.clone(), jobs: Vec::new(), failures: Vec::new(), network: lab.log.clone() };
    let control = {
        let env = Env { lab: &lab, scopes: &scopes, certs: &certs, web: &s.web, bin: &bin, kiln: s.kiln.as_deref(), h2o: s.h2o.as_deref(), caddy: s.caddy.as_deref(), payloads: s.payloads.as_deref(), term_per_s: 1.0, logs: s.logs, seed: s.seed };
        servers::start_control(&env, out)?
    };
    let mut turn_id = 0;
    let mut job_n: u64 = 0;
    for p in &s.profiles {
        for loss in &s.losses {
            for round in 0..s.rounds {
                let name = format!("{}/{}", p.name, loss.name());
                let seed = round_seed(s.seed, &name, round);
                for i in 0..s.k {
                    lab.shape(i, &Condition { profile: p.clone(), loss: loss.clone(), seed: seed.wrapping_add(i as u64) })?;
                }
                let cond = Condition { profile: p.clone(), loss: loss.clone(), seed };
                let mut order: Vec<&Target> = s.targets.iter().collect();
                if !order.is_empty() {
                    let n = order.len();
                    order.rotate_left(round % n);
                }
                for t in order {
                    let dir = out.join("servers").join(format!("{}-r{round}-{}", name.replace('/', "-"), t.name));
                    let started = {
                        let env = Env { lab: &lab, scopes: &scopes, certs: &certs, web: &s.web, bin: &bin, kiln: s.kiln.as_deref(), h2o: s.h2o.as_deref(), caddy: s.caddy.as_deref(), payloads: s.payloads.as_deref(), term_per_s: workload("stream", p).lines_per_s, logs: s.logs, seed: s.seed };
                        servers::start(&env, t, &dir, 3)
                    };
                    let running = match started {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("kiln-lab: {name} round {round}: {} didn't start: {e:#}", t.name);
                            run.failures.push(format!("{name} round {round}: {} didn't start: {e:#}", t.name));
                            continue;
                        }
                    };
                    eprintln!("kiln-lab: {name} round {round}: {} up ({:?})", t.name, running.pids());
                    for b in t.browsers.iter().filter(|b| s.browsers.contains(b)) {
                        for case in &s.cases {
                            job_n += 1;
                            let mut ctx = Ctx { lab: &lab, certs: &certs, s, out, job_n };
                            match job(&mut ctx, &running, t, b, case, &cond, round, &mut turn_id) {
                                Ok(j) => {
                                    eprintln!(
                                        "kiln-lab: {name} r{round} {} {b} {case}: time p50 {:.1} ms p99 {:.1} ms (n {}, of them {} missed), bytes/op {:.0}, cpu/op {:.0} µs{}",
                                        t.name,
                                        j.numbers.get("time.p50_ms").copied().unwrap_or(f64::NAN),
                                        j.numbers.get("time.p99_ms").copied().unwrap_or(f64::NAN),
                                        j.numbers.get("time.n").copied().unwrap_or(0.0),
                                        j.numbers.get("missed").copied().unwrap_or(0.0),
                                        j.numbers.get("bytes.mean").copied().unwrap_or(f64::NAN),
                                        j.numbers.get("cpu_us.mean").copied().unwrap_or(f64::NAN),
                                        if j.errors.is_empty() { String::new() } else { format!("; {} errors, first: {}", j.errors.len(), j.errors.first().cloned().unwrap_or_default()) }
                                    );
                                    run.jobs.push(j);
                                }
                                Err(e) => {
                                    eprintln!("kiln-lab: {name} r{round} {} {b} {case}: failed: {e:#}", t.name);
                                    run.failures.push(format!("{name} r{round} {} {b} {case}: {e:#}", t.name));
                                }
                            }
                            std::fs::write(out.join("run.json"), serde_json::to_vec_pretty(&run)?)?;
                        }
                    }
                    drop(running);
                }
            }
        }
    }
    drop(control);
    std::fs::write(out.join("run.json"), serde_json::to_vec_pretty(&run)?)?;
    Ok(run)
}

/// Step 5's test: each target started and stopped `times` times. Each start must answer on its port
/// (TCP, and UDP too for an HTTP/3 target), each stop leave no process of it (its recorded PIDs
/// gone and its scopes empty). A line for each target, and whether all passed.
pub fn start_stop(s: &Settings, out: &Path, times: usize) -> anyhow::Result<(String, bool)> {
    use std::fmt::Write as _;
    std::fs::create_dir_all(out)?;
    let lab = Lab::up(1)?;
    let scopes = Scopes::mine()?;
    let certs = servers::certs(&out.join("certs"))?;
    let bin = super::standins::install(&out.join("bin"))?;
    let mut text = String::new();
    let mut all = true;
    for t in &s.targets {
        let (mut up, mut clean, mut why) = (0, 0, String::new());
        for i in 0..times {
            let env = Env { lab: &lab, scopes: &scopes, certs: &certs, web: &s.web, bin: &bin, kiln: s.kiln.as_deref(), h2o: s.h2o.as_deref(), caddy: s.caddy.as_deref(), payloads: s.payloads.as_deref(), term_per_s: 1.0, logs: s.logs, seed: s.seed };
            let dir = out.join(format!("{}-{i}", t.name));
            match servers::start(&env, t, &dir, 1) {
                Ok(r) => {
                    let pids: Vec<u32> = r.pids().iter().map(|(_, p)| *p).collect();
                    let udp = !t.http3 || pids.iter().any(|p| udp_bound(*p, t.port));
                    if udp {
                        up += 1;
                    } else {
                        why = format!("start {i}: no UDP socket on {}", t.name);
                    }
                    // The first time, every static body it serves against the catalogue's own.
                    if i == 0
                        && let (Some(p), true) = (s.payloads.as_deref(), r.kiln.is_none())
                    {
                        let (same, of, odd) = served_bodies(&lab, t, &p.join("docroot"));
                        let _ = writeln!(text, "- {}: {same} of {of} static bodies served as the catalogue holds them{}", t.name, odd.map_or_else(String::new, |o| format!(" (not: {o})")));
                        all &= same == of && of > 0;
                        // And every stream frame the origin replays, as the server sends it on, on
                        // both carriers (check-origin.mjs's `served`, in the client's namespace).
                        if let Some(sock) = &r.replay_sock {
                            let (ok, said) = served_streams(&lab, t, &s.web, sock, p);
                            let _ = writeln!(text, "- {}: streams through the server: {}{said}", t.name, if ok { "every frame holds" } else { "**some frames differ**" });
                            all &= ok;
                        }
                    }
                    let scopes_of = [Some(r.scope.clone()), r.origin_scope.clone()];
                    drop(r);
                    std::thread::sleep(Duration::from_millis(300));
                    let alive = pids.iter().filter(|p| Path::new(&format!("/proc/{p}")).exists()).count();
                    let left: usize = scopes_of.iter().flatten().map(|d| std::fs::read_to_string(d.join("cgroup.procs")).unwrap_or_default().lines().count()).sum();
                    if alive == 0 && left == 0 {
                        clean += 1;
                    } else {
                        why = format!("stop {i}: {alive} recorded processes alive, {left} in its scopes");
                    }
                }
                Err(e) => why = format!("start {i}: {e:#}"),
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
        let ok = up == times && clean == times;
        all &= ok;
        let _ = writeln!(text, "- {}: up {up} of {times}, stopped clean {clean} of {times}{}", t.name, if why.is_empty() { String::new() } else { format!(" (last: {why})") });
    }
    Ok((text, all))
}

/// Each path's protocol in a reference server's access log from byte `from` on: h2o's lines are
/// `HTTP/2 /path`, Caddy's JSON with `request.proto` and `request.uri`. Ids in paths as the page
/// names them (`:id`).
fn logged_protocols(log: &Path, from: u64) -> std::collections::BTreeMap<String, String> {
    let bytes = std::fs::read(log).unwrap_or_default();
    let text = String::from_utf8_lossy(bytes.get(usize::try_from(from).unwrap_or(usize::MAX)..).unwrap_or_default());
    let mut out = std::collections::BTreeMap::new();
    for line in text.lines() {
        let (proto, uri) = if let Ok(v) = serde_json::from_str::<Value>(line) {
            (v.pointer("/request/proto").and_then(Value::as_str).unwrap_or_default().to_owned(), v.pointer("/request/uri").and_then(Value::as_str).unwrap_or_default().to_owned())
        } else {
            let (p, u) = line.split_once(' ').unwrap_or_default();
            (p.to_owned(), u.to_owned())
        };
        let name = match proto.as_str() {
            "HTTP/2" | "HTTP/2.0" => "h2",
            "HTTP/3" | "HTTP/3.0" => "h3",
            "HTTP/1.1" => "http/1.1",
            _ => continue,
        };
        let path = uri.split('?').next().unwrap_or_default();
        let path: String = path.split('/').map(|seg| if seg.len() == 32 && seg.chars().all(|c| c.is_ascii_hexdigit()) { ":id" } else { seg }).collect::<Vec<_>>().join("/");
        out.insert(path, name.to_owned());
    }
    out
}

/// Whether a process's network namespace has a UDP socket bound to `port`.
fn udp_bound(pid: u32, port: u16) -> bool {
    let hex = format!(":{port:04X}");
    ["udp", "udp6"].iter().any(|f| std::fs::read_to_string(format!("/proc/{pid}/net/{f}")).unwrap_or_default().lines().skip(1).any(|l| l.split_whitespace().nth(1).is_some_and(|a| a.ends_with(&hex))))
}

/// Every stream the origin replays, read through `t`'s HTTP/2 (gzip asked for) on both carriers by
/// `check-origin.mjs served` in the first client's namespace: whether every frame held, and what
/// it said of each stream.
fn served_streams(lab: &Lab, t: &Target, web: &Path, sock: &Path, payloads: &Path) -> (bool, String) {
    let argv = vec!["node".to_owned(), web.join("check-origin.mjs").display().to_string(), "served".to_owned(), t.origin(), sock.display().to_string(), payloads.display().to_string()];
    match lab.command(Ns::Client(0), &argv).and_then(|mut c| Ok(c.stdin(Stdio::null()).output()?)) {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout);
            let lines: Vec<String> = out.lines().filter(|l| l.starts_with('{')).map(|l| format!("\n    {}", l.chars().take(400).collect::<String>())).collect();
            (o.status.success(), lines.concat())
        }
        Err(e) => (false, format!(" ({e:#})")),
    }
}

/// Each file under `docroot` fetched from `t` by curl in the first client's namespace (HTTP/2,
/// decompressed): how many came back byte for byte, of how many, and the first that didn't.
fn served_bodies(lab: &Lab, t: &Target, docroot: &Path) -> (usize, usize, Option<String>) {
    let mut files = Vec::new();
    let mut stack = vec![docroot.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() { stack.push(p) } else { files.push(p) }
        }
    }
    let (mut same, mut odd) = (0, None);
    for f in &files {
        let Ok(rel) = f.strip_prefix(docroot) else { continue };
        let url = format!("{}/{}", t.origin(), rel.to_string_lossy());
        let argv = vec!["curl".to_owned(), "-sk".to_owned(), "--http2".to_owned(), "--compressed".to_owned(), url.clone()];
        let got = lab.command(Ns::Client(0), &argv).and_then(|mut c| Ok(c.stdin(Stdio::null()).output()?));
        match (got, std::fs::read(f)) {
            (Ok(o), Ok(want)) if o.stdout == want => same += 1,
            _ => odd = Some(url),
        }
    }
    (same, files.len(), odd)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// plan 01a1a832e9cc step 5: the protocol each path came over, from h2o's and Caddy's logs.
    #[test]
    fn g1_s5_access_logs_name_each_paths_protocol() {
        let dir = std::env::temp_dir().join(format!("kiln-lab-log-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join("access.log");
        let old = "HTTP/1.1 /old\n";
        let body = format!("{old}HTTP/2 /stream/session\n{}\nnot a line\n", r#"{"request":{"proto":"HTTP/3.0","uri":"/api/harness/sessions/01a1a94fe73a77a1a8fff1e1d19098c1/command?x=1"}}"#);
        let _ = std::fs::write(&log, body);
        let got = logged_protocols(&log, old.len() as u64);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got.get("/stream/session").map(String::as_str), Some("h2"));
        assert_eq!(got.get("/api/harness/sessions/:id/command").map(String::as_str), Some("h3"));
        assert!(!got.contains_key("/old"));
    }

    #[test]
    fn g1_s6_a_round_seed_is_the_same_for_every_target_and_differs_by_round() {
        assert_eq!(round_seed(1, "50k/1", 0), round_seed(1, "50k/1", 0));
        assert_ne!(round_seed(1, "50k/1", 0), round_seed(1, "50k/1", 1));
        assert_ne!(round_seed(1, "50k/1", 0), round_seed(1, "good/1", 0));
    }

    #[test]
    fn g1_s4_requests_are_open_loop_on_a_fixed_schedule() {
        let p = link::profile_named("50k").unwrap_or_else(|| link::profiles().swap_remove(0));
        let w = workload("request", &p);
        let t = super::super::targets::named("h2o-h2").unwrap_or_else(|| super::super::targets::targets().swap_remove(0));
        let o = ops(&t, "request", &w, None, false);
        let sched = o.first().and_then(|o| o.get("schedule_ms")).and_then(Value::as_array).cloned().unwrap_or_default();
        assert_eq!(sched.len() as u64, w.duration_s * 1000 / w.request_ms);
        assert_eq!(sched.get(1).and_then(Value::as_u64), Some(w.request_ms));
    }
}
