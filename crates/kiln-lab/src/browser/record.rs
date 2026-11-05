//! Step 3's recording (plan 01a1a832e9cc): the lab kiln up on one good client link, its agents at
//! work as an agent at work is (a turn of 40 events 25 ms apart, then a pause), its terminal's and
//! its job's stand-ins writing, while Chromium opens kiln's own pages for each journey and the
//! browser's network events are written down (`web/record.mjs`). Then `web/catalogue.mjs` makes the
//! catalogue and the replay from them.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::json;

use super::link::{self, Condition, Loss, Ns};
use super::servers::{self, Env};
use super::targets;
use super::topo::{Lab, Scopes};

pub fn run(kiln: &Path, web: &Path, out: &Path, seconds: u64, seed: u64, only: Option<&[String]>) -> anyhow::Result<()> {
    std::fs::create_dir_all(out)?;
    let mut lab = Lab::up(1)?;
    let good = link::profile_named("good").ok_or_else(|| anyhow::anyhow!("no good profile"))?;
    lab.shape(0, &Condition { profile: good, loss: Loss::None, seed })?;
    let scopes = Scopes::mine()?;
    let certs = servers::certs(&out.join("certs"))?;
    let bin = super::standins::install(&out.join("bin"))?;
    let t = targets::named("kiln-today").ok_or_else(|| anyhow::anyhow!("no kiln-today target"))?;
    let env = Env { lab: &lab, scopes: &scopes, certs: &certs, web, bin: &bin, kiln: Some(kiln), h2o: None, caddy: None, payloads: None, term_per_s: 10.0, logs: true, seed };
    let running = servers::start(&env, &t, &out.join("kiln"), 3)?;
    let k = running.kiln.clone().ok_or_else(|| anyhow::anyhow!("no lab kiln"))?;
    eprintln!("kiln-lab: the lab kiln is up: sessions {:?}, log job {:?}", k.sessions, k.job);
    // Agents at work through the recording.
    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let (stop, k) = (stop.clone(), k.clone());
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                // The third session takes no turns: its snapshot is the one a run's reads take.
                for s in k.sessions.iter().take(2) {
                    n += 1;
                    let _ = servers::turn(&k, s, 40, 25, 10_000 + n);
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        })
    };
    let first = k.sessions.first().cloned().unwrap_or_default();
    let mut journeys = vec![
        json!({"name": "mission-control", "path": "/", "seconds": seconds}),
        json!({"name": "agent", "path": format!("/agents/{first}"), "seconds": seconds, "actions": [{"post": format!("/api/harness/sessions/{first}/command"), "body": {"type": "send", "text": "lab:turn:40:25"}, "every_s": 10}]}),
        json!({"name": "terminal", "path": "/", "seconds": seconds, "actions": [{"ws": "/api/terminal/ws?target=local&cols=120&rows=40"}]}),
        // A session's snapshot, as a run's reads take it (the third session, which no turn reached).
        json!({"name": "snapshot", "path": "/", "seconds": 10, "actions": [{"ws": format!("/api/harness/sessions/{}/ws?z=gzip&patch=state&patch=changes", k.sessions.get(2).cloned().unwrap_or_default())}]}),
    ];
    if let (Some(run), Some(job)) = (&k.run, &k.job) {
        // The job's page, and its log's socket as the UI's log view opens it.
        journeys.push(json!({"name": "job-log", "path": format!("/runs/{run}/jobs/{job}"), "seconds": seconds, "actions": [{"ws": format!("/api/jobs/{job}/logs/ws?step=0")}]}));
    }
    if let Some(only) = only {
        journeys.retain(|j| j.get("name").and_then(serde_json::Value::as_str).is_some_and(|n| only.iter().any(|o| o == n)));
    }
    let rec = out.join("recordings");
    std::fs::create_dir_all(&rec)?;
    let spec = json!({"origin": t.origin(), "token": k.admin, "out": rec.display().to_string(), "journeys": journeys});
    let spec_path = out.join("record.json");
    std::fs::write(&spec_path, serde_json::to_vec_pretty(&spec)?)?;
    let argv = vec!["node".to_owned(), web.join("record.mjs").display().to_string(), spec_path.display().to_string()];
    let log = std::fs::File::create(out.join("record.log"))?;
    let status = lab.command(Ns::Client(0), &argv)?.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log).status()?;
    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();
    drop(running);
    anyhow::ensure!(status.success(), "the recording failed ({status}): {}", out.join("record.log").display());
    Ok(())
}
