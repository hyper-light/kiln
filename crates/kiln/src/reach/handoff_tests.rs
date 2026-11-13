//! The hand-off in kiln's own nodes (LB03, W05-R25; docs/reach.md §2.4): three reach nodes, as
//! three runners run them, through the relay as the server runs it. The owner of a detached run
//! that an agent asked for is killed while the run runs elsewhere; the runner holding it hands it
//! off when it ends, a node adopts it, and the agent is told how it ended.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use super::directory::RunnerInfo;
use super::local::{self, Ev, RunReq};
use super::node::{self, Config, Handle, In};
use super::relay::{Announce, Frame, Relay};
use crate::proto::{AgentMsg, ServerMsg};

/// A hang's bound, never a pace (as reach's own tests wait).
const HANG: Duration = Duration::from_secs(300);
const NAMES: [&str; 3] = ["mac", "linux", "studio"];

struct Lab3 {
    nodes: HashMap<&'static str, Handle>,
    /// Kill a node: its connection to the "server" ends for good.
    kill: async_channel::Sender<(&'static str, futures::channel::oneshot::Sender<()>)>,
    _dirs: Vec<tempfile::TempDir>,
}

impl Lab3 {
    fn node(&self, name: &str) -> anyhow::Result<Handle> {
        self.nodes.get(name).cloned().ok_or_else(|| anyhow::anyhow!("no node {name}"))
    }

    async fn kill(&self, name: &'static str) -> anyhow::Result<()> {
        let (ack, done) = futures::channel::oneshot::channel();
        self.kill.send((name, ack)).await.map_err(|_| anyhow::anyhow!("the relay stopped before killing {name}"))?;
        crate::rt::timeout(HANG, done).await.map_err(|_| anyhow::anyhow!("the relay did not acknowledge killing {name}"))??;
        Ok(())
    }
}

fn info(name: &str, fp: &str) -> RunnerInfo {
    RunnerInfo {
        name: name.into(),
        hostname: format!("{name}.lan"),
        os: "linux".into(),
        arch: "x86_64".into(),
        connected: true,
        reach: true,
        fingerprint: Some(fp.into()),
        cpus: 8,
        mem_total: 16 << 30,
        mem_used: 0,
        slots: 2,
        via: "direct".into(),
        features: crate::proto::feature::all(),
        ..RunnerInfo::default()
    }
}

/// A reading of a machine: `mem_gib` of memory in all, nothing busy.
fn reading(mem_gib: u64) -> crate::metrics::HostSample {
    crate::metrics::HostSample { cpus: 8, cpu_pct: 0.0, mem_total: mem_gib << 30, mem_used: 0, disk_total: 100 << 30, disk_used: 10 << 30, ..crate::metrics::HostSample::default() }
}

async fn deliver(peers: &Mutex<HashMap<&'static str, u64>>, nodes: &HashMap<&'static str, Handle>, to: u64, sm: ServerMsg) {
    let name = peers.lock().iter().find(|(_, p)| **p == to).map(|(n, _)| *n);
    let (Some(name), ServerMsg::Reach { from, link, open, data, close }) = (name, sm) else { return };
    if let Some(h) = nodes.get(name) {
        h.feed(In::Frame { from, link, open, data, close }).await;
    }
}

async fn lab3() -> anyhow::Result<Lab3> {
    let mut dirs = Vec::new();
    for _ in 0..=NAMES.len() {
        dirs.push(tempfile::tempdir()?);
    }
    let mirrors = dirs.last().map(|d| d.path().to_path_buf()).unwrap_or_default();
    let mut nodes = HashMap::new();
    let mut outs = Vec::new();
    for (i, name) in NAMES.into_iter().enumerate() {
        let dir = dirs.get(i).ok_or_else(|| anyhow::anyhow!("dir"))?;
        let cfg = Config {
            name: name.into(),
            dir: dir.path().join("reach"),
            accept: true,
            mirrors: mirrors.clone(),
            adopt: None,
            adopt_policy: crate::harness::moving::stream::StoragePolicy::Off,
            using: Arc::default(),
            objects: None,
            sense: false,
            on_battery: kiln_ci::pace::power::OnBattery::default(),
            place: kiln_ci::place::runner::Settings::default(),
            version: super::wire::VERSION,
            root: Vec::new(),
            capacity: None,
        };
        let (h, out) = node::start(cfg)?;
        nodes.insert(name, h);
        outs.push((name, out));
    }
    let directory: Vec<RunnerInfo> = NAMES.iter().filter_map(|n| nodes.get(n).map(|h| info(n, h.fingerprint()))).collect();
    let peers: Arc<Mutex<HashMap<&'static str, u64>>> = Arc::new(Mutex::new(NAMES.iter().enumerate().map(|(i, n)| (*n, 10 * (u64::try_from(i).unwrap_or(0) + 1))).collect()));
    let (kill_tx, kill_rx) = async_channel::unbounded::<(&'static str, futures::channel::oneshot::Sender<()>)>();
    let (said_tx, said_rx) = async_channel::unbounded::<(&'static str, AgentMsg)>();
    for (name, out) in outs {
        let tx = said_tx.clone();
        crate::rt::spawn(async move {
            while let Ok(m) = out.recv().await {
                if tx.send((name, m)).await.is_err() {
                    return;
                }
            }
        });
    }
    let relay_nodes = nodes.clone();
    crate::rt::spawn(async move {
        let mut relay = Relay::default();
        for n in NAMES {
            if let Some(h) = relay_nodes.get(n) {
                relay.announce(n, Announce { fingerprint: h.fingerprint().into(), accept: true, version: 1, features: Vec::new() });
            }
        }
        loop {
            use futures::FutureExt as _;
            let (from, msg) = futures::select_biased! {
                k = kill_rx.recv().fuse() => {
                    let Ok((name, ack)) = k else { return };
                    let peer = peers.lock().get(name).copied().unwrap_or(0);
                    for (to, sm) in relay.lost(peer, "disconnected from the kiln server") {
                        deliver(&peers, &relay_nodes, to, sm).await;
                    }
                    peers.lock().remove(name);
                    if let Some(h) = relay_nodes.get(name) {
                        h.feed(In::Server(false)).await;
                    }
                    let _ = ack.send(());
                    continue;
                }
                m = said_rx.recv().fuse() => match m {
                    Ok(m) => m,
                    Err(_) => return,
                },
            };
            let Some(from_peer) = peers.lock().get(from).copied() else { continue };
            match msg {
                AgentMsg::Reach { to, link, open, data, close } => {
                    let p = Arc::clone(&peers);
                    let out = relay.forward(from, from_peer, Frame { to, link, open, data, close }, move |n| p.lock().get(n).copied(), str::to_owned).sends(from_peer, false);
                    for (to, sm) in out {
                        deliver(&peers, &relay_nodes, to, sm).await;
                    }
                }
                AgentMsg::ReachAsk { req, json: _ } => {
                    // LB03: Hub::reach_directory retains disconnected non-ephemeral runners
                    // (hub.rs), with connected/reach false. Omitting one means permanently
                    // forgetting its ended runs to Placer::directory, not losing its connection.
                    let listed: Vec<RunnerInfo> = {
                        let peers = peers.lock();
                        directory.iter().cloned().map(|mut r| {
                            r.connected = peers.contains_key(r.name.as_str());
                            r.reach &= r.connected;
                            r
                        }).collect()
                    };
                    if let Some(h) = relay_nodes.get(from) {
                        h.feed(In::Answer { req, answer: serde_json::json!({ "runners": listed }) }).await;
                    }
                }
                _ => {}
            }
        }
    });
    for h in nodes.values() {
        h.feed(In::Server(true)).await;
        h.feed(In::Unread(true)).await;
    }
    Ok(Lab3 { nodes, kill: kill_tx, _dirs: dirs })
}

async fn eventually(mut ok: impl FnMut() -> bool) -> bool {
    let until = std::time::Instant::now() + HANG;
    while std::time::Instant::now() < until {
        if ok() {
            return true;
        }
        crate::rt::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// LB03, W05-R25: a detached run an agent asked of mac starts on linux (mac's own machine is too
/// small for it, so its runners are linux and studio); mac is killed while it runs. When it ends,
/// linux, holding its end and unable to reach mac (its link dropped as mac's connection ended: the
/// transport's own verdict, no ping), hands it off by rendezvous order; the adopter claims it at
/// linux and studio, both promise, it owns the run, linux tells it the end, and the agent is told
/// that it runs on under the adopter and how it ended, through told. It started once.
#[test]
fn lb03_w05_r25_an_owner_killed_mid_run_has_its_run_adopted_and_its_agent_told() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab3().await?;
        let (mac, linux, studio) = (lab.node("mac")?, lab.node("linux")?, lab.node("studio")?);
        mac.feed(In::Sampled(reading(1))).await;
        linux.feed(In::Sampled(reading(16))).await;
        studio.feed(In::Sampled(reading(16))).await;
        for h in [&mac, &linux, &studio] {
            h.directory().await?;
        }
        let dir = tempfile::tempdir()?;
        let go = dir.path().join("go");
        let session = format!("lb03-{}", crate::util::new_id());
        let req = RunReq {
            target: "mem=4g".into(),
            command: format!("while [ ! -e '{}' ]; do sleep 0.05; done; echo ran", go.display()),
            detach: true,
            asker: Some(session.clone()),
            ..RunReq::default()
        };
        let (ev_tx, ev_rx) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
        let (_ctl_tx, ctl_rx) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
        let asker = mac.clone();
        crate::rt::spawn(async move { local::run(&asker, "mac", req, ev_tx, ctl_rx).await });
        // It starts on linux, mac's word.
        let mut evs = Vec::new();
        loop {
            match crate::rt::timeout(HANG, ev_rx.recv()).await {
                Ok(Ok(e)) => {
                    let started = matches!(e, Ev::Started { .. });
                    evs.push(e);
                    if started {
                        break;
                    }
                }
                _ => anyhow::bail!("it never started: {evs:?}"),
            }
        }
        // mac is killed mid-run, for good; then the run ends.
        anyhow::ensure!(evs.iter().any(|e| matches!(e, Ev::Placed { .. })), "the run bypassed placement: {evs:?}");
        lab.kill("mac").await?;
        // The relay has queued every loss frame. These ordered node requests observe those
        // inputs before the process may end: a killed owner mid-run, not a race with the kill.
        for h in [&mac, &linux, &studio] {
            let _ = crate::rt::timeout(HANG, h.place_view()).await.map_err(|_| anyhow::anyhow!("{} did not observe the owner's disconnection", h.name()))??;
        }
        std::fs::write(&go, b"")?;
        let told = eventually(|| crate::reach::told::heard_of(&session).iter().any(|n| matches!(n.event, crate::reach::told::RunEvent::Ended) && n.error.is_none())).await;
        let news = crate::reach::told::heard_of(&session);
        if !told {
            let mut views = Vec::new();
            for h in [&mac, &linux, &studio] {
                let view = crate::rt::timeout(HANG, h.place_view()).await.map_err(|_| anyhow::anyhow!("{} did not answer the handoff diagnostic", h.name()))??;
                views.push((h.name().to_owned(), view));
            }
            while let Ok(e) = ev_rx.try_recv() {
                evs.push(e);
            }
            anyhow::bail!("its agent was told how it ended: {news:?}; placement: {views:?}; events: {evs:?}");
        }
        anyhow::ensure!(news.iter().any(|n| matches!(n.event, crate::reach::told::RunEvent::Started) && n.on.contains("linux")), "and that it runs on at linux under its adopter: {news:?}");
        let v = linux.place_view().await?;
        anyhow::ensure!(v.get("held").cloned() == Some(serde_json::json!([0, 0, 0, 0])), "its room is free on linux: {v}");
        anyhow::Ok(())
    })
}
