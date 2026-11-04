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
fn ops(t: &Target, case: &str, w: &Workload, kiln: Option<&servers::KilnLab>) -> Vec<Value> {
    let sid = |i: usize| kiln.and_then(|k| k.sessions.get(i).cloned()).unwrap_or_else(|| "lab".to_owned());
    let base = |kind: &str| if t.carrier == "fetch-stream" { format!("/fetch-stream/{kind}") } else { format!("/stream/{kind}") };
    let stream = |name: &str, path: String, packed: bool| json!({"case": case, "name": name, "carrier": if t.server == Server::Kiln { "ws" } else { t.carrier.as_str() }, "path": path, "packed": packed});
    match (case, t.server) {
        ("stream", Server::Kiln) => {
            let mut v = vec![
                stream("session", format!("/api/harness/sessions/{}/ws?z=gzip&patch=state&patch=changes", sid(0)), true),
                stream("terminal", "/api/terminal/ws?target=local&cols=120&rows=40".to_owned(), false),
                stream("events", "/api/events".to_owned(), false),
            ];
            if let Some(job) = kiln.and_then(|k| k.job.clone()) {
                v.push(stream("log", format!("/api/jobs/{job}/logs/ws?step=0"), false));
            }
            v
        }
        ("stream", _) => ["session", "terminal", "events", "log"].iter().map(|k| stream(k, base(k), false)).collect(),
        ("burst", Server::Kiln) => vec![stream("turn", format!("/api/harness/sessions/{}/ws?z=gzip&patch=state&patch=changes", sid(1)), true)],
        ("burst", _) => vec![stream("turn", base("session"), false)],
        _ => {
            let id = sid(0);
            let reads = ["/api/agents".to_owned(), "/api/harness/sessions".to_owned(), format!("/api/harness/sessions/{id}"), "/api/plans".to_owned(), "/api/overview".to_owned()];
            let mut requests: Vec<Value> = reads.iter().map(|p| json!({"name": format!("GET {}", p.replace(&id, ":id")), "path": p})).collect();
            requests.push(json!({"name": "POST /api/harness/sessions/:id/command", "method": "POST", "path": format!("/api/harness/sessions/{id}/command"), "body": json!({"type": "send", "id": "lab-req", "text": "lab:quiet"}).to_string()}));
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
}

fn spawn_driver(lab: &Lab, web: &Path, i: usize, job: &Path) -> anyhow::Result<Child> {
    let argv = vec!["node".to_owned(), web.join("driver.mjs").display().to_string(), "job".to_owned(), job.display().to_string()];
    let log = std::fs::File::create(job.with_extension("log"))?;
    Ok(lab.command(Ns::Client(i), &argv)?.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log).spawn()?)
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
    // The background: the server with nobody attached, two windows.
    let bg = Duration::from_secs(c.s.background_s.max(1));
    let a = cpu_now(r);
    std::thread::sleep(bg);
    let b = cpu_now(r);
    std::thread::sleep(bg);
    let d = cpu_now(r);
    let rate = |x: &CpuTick, y: &CpuTick| {
        #[allow(clippy::cast_precision_loss)]
        let v = (y.server_us + y.origin_us).saturating_sub(x.server_us + x.origin_us) as f64 / (y.t_us.saturating_sub(x.t_us).max(1) as f64 / 1e6);
        v
    };
    let background = rate(&a, &d);
    let noise = (rate(&a, &b) - rate(&b, &d)).abs();
    let ops = ops(t, case, &w, r.kiln.as_ref());
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
            "ops": ops,
            "duration_s": w.duration_s,
            "tail_s": w.tail_s,
            "window_ms": c.s.window_ms,
            "settle_ms": 1500,
        });
        let path = jd.join("job.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&job)?)?;
        drivers.push((i, spawn_driver(c.lab, &c.s.web, i, &path)?, jd));
    }
    // The server's CPU, every window, while the job runs.
    let stop = Arc::new(AtomicBool::new(false));
    let cpu_file = dir.join("cpu.jsonl");
    let sampler = {
        let stop = stop.clone();
        let scope = r.scope.clone();
        let origin = if r.kiln.is_some() { None } else { r.origin_scope.clone() };
        let every = Duration::from_millis(c.s.window_ms.max(50));
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
            for (kind, per_s) in [("session", w.session_per_s), ("terminal", w.lines_per_s), ("log", w.lines_per_s), ("events", w.session_per_s)] {
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
        _ => {}
    }
    let end = until + Duration::from_secs(w.tail_s + 120);
    let mut blocked = false;
    for (_, child, _) in &mut drivers {
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
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    stop.store(true, Ordering::Relaxed);
    let cpu = sampler.join().map_err(|_| anyhow::anyhow!("the CPU sampler stopped"))?;
    std::fs::write(&cpu_file, cpu.iter().filter_map(|c| serde_json::to_string(c).ok()).collect::<Vec<_>>().join("\n"))?;
    let mut clients = Vec::new();
    let mut protocols = std::collections::BTreeMap::new();
    let mut errors = Vec::new();
    let mut calib: Option<f64> = None;
    for (i, _, jd) in &drivers {
        let samples: Vec<Sample> = read_jsonl(&jd.join("samples.jsonl"));
        let ticks: Vec<Tick> = read_jsonl(&jd.join("counters.jsonl"));
        let res: Value = std::fs::read(jd.join("result.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
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
        clients.push((samples, ticks));
    }
    let nums = cells::job_numbers(&clients, &cpu, background, ready_us);
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
        calibration_p99_us: calib,
        background_us_per_s: background,
        noise_us_per_s: noise,
        errors,
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
        let env = Env { lab: &lab, scopes: &scopes, certs: &certs, web: &s.web, bin: &bin, kiln: s.kiln.as_deref(), h2o: s.h2o.as_deref(), caddy: s.caddy.as_deref(), payloads: s.payloads.as_deref() };
        servers::start_control(&env, out)?
    };
    let mut turn_id = 0;
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
                        let env = Env { lab: &lab, scopes: &scopes, certs: &certs, web: &s.web, bin: &bin, kiln: s.kiln.as_deref(), h2o: s.h2o.as_deref(), caddy: s.caddy.as_deref(), payloads: s.payloads.as_deref() };
                        servers::start(&env, t, &dir, 2)
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
                            let mut ctx = Ctx { lab: &lab, certs: &certs, s, out };
                            match job(&mut ctx, &running, t, b, case, &cond, round, &mut turn_id) {
                                Ok(j) => {
                                    eprintln!(
                                        "kiln-lab: {name} r{round} {} {b} {case}: time p50 {:.1} ms p99 {:.1} ms (n {}), bytes/op {:.0}, cpu/op {:.0} µs{}",
                                        t.name,
                                        j.numbers.get("time.p50_ms").copied().unwrap_or(f64::NAN),
                                        j.numbers.get("time.p99_ms").copied().unwrap_or(f64::NAN),
                                        j.numbers.get("time.n").copied().unwrap_or(0.0),
                                        j.numbers.get("bytes.mean").copied().unwrap_or(f64::NAN),
                                        j.numbers.get("cpu_us.mean").copied().unwrap_or(f64::NAN),
                                        if j.errors.is_empty() { String::new() } else { format!("; {} errors, first: {}", j.errors.len(), j.errors.first().cloned().unwrap_or_default()) }
                                    );
                                    run.jobs.push(j);
                                }
                                Err(e) => run.failures.push(format!("{name} r{round} {} {b} {case}: {e:#}", t.name)),
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let o = ops(&t, "request", &w, None);
        let sched = o.first().and_then(|o| o.get("schedule_ms")).and_then(Value::as_array).cloned().unwrap_or_default();
        assert_eq!(sched.len() as u64, w.duration_s * 1000 / w.request_ms);
        assert_eq!(sched.get(1).and_then(Value::as_u64), Some(w.request_ms));
    }
}
