//! Two reach nodes, as two runners would run them, talking through the relay the server runs:
//! runs, a dropped connection mid-run, a sync then a run in the copy, an impostor.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use super::directory::RunnerInfo;
use super::local::{self, Ev, RunReq};
use super::node::{self, Config, Handle, In};
use super::relay::{Announce, Frame, Relay};
use crate::proto::{AgentMsg, ServerMsg};

// The doors into placement, driven into this lab: `kiln run`, the MCP `run` tool, and the server
// placing agents (docs/ci.md W05-R03).
mod doors;

/// What a test here waits for at the most, a hang's bound and never a pace: a loaded machine's handshakes, runs,
/// links and the output that goes over them take what time they take (60 s and 20 s ran out under load).
const HANG: Duration = Duration::from_secs(300);

/// What the test does to the "server".
enum Ctl {
    /// Drop a runner's connection (its links close; it hears the server went).
    Drop(&'static str),
    /// It reconnects (a new connection).
    Back(&'static str),
    /// Change what the directory says a runner's key is.
    Key(&'static str, String),
    /// Change what the directory says a runner takes beyond its version (docs/reach.md, "Sizes from
    /// history"): none is a runner from before.
    Features(&'static str, Vec<String>),
    /// Change what the directory says a runner has in all, and free, of memory.
    Mem(&'static str, u64),
}

struct Lab {
    mac: Handle,
    linux: Handle,
    /// What the runners sent the "server" that isn't reach's frames or questions.
    said: async_channel::Receiver<(&'static str, AgentMsg)>,
    /// Each link that ended, as the relay saw it end (a close sent on it, or a connection lost
    /// under it), in order.
    ended: Arc<Mutex<Vec<String>>>,
    /// What linux is handed to take over.
    adoptions: crate::memory::queue::Receiver<node::Adoption>,
    ctl: async_channel::Sender<Ctl>,
    _dirs: Vec<tempfile::TempDir>,
    mirrors: std::path::PathBuf,
    /// What linux's node notes of the copies it holds.
    using: Arc<super::keeper::Using>,
    /// What the "server" answers a command's history question with; none: it never answers.
    sizes: Arc<Mutex<Option<serde_json::Value>>>,
    /// What runners told the "server" their runs used, and who told it.
    measured: Arc<Mutex<Vec<(&'static str, serde_json::Value)>>>,
}

fn info(name: &str, fp: &str) -> RunnerInfo {
    RunnerInfo {
        name: name.into(),
        hostname: format!("{name}.lan"),
        label: None,
        os: if name == "mac" { "macos".into() } else { "linux".into() },
        arch: if name == "mac" { "aarch64".into() } else { "x86_64".into() },
        connected: true,
        reach: true,
        fingerprint: Some(fp.into()),
        cpus: 8,
        mem_total: 16 << 30,
        mem_used: 4 << 30,
        slots: 2,
        via: "direct".into(),
        features: super::history::features(),
        ..RunnerInfo::default()
    }
}

async fn lab() -> anyhow::Result<Lab> {
    lab_with([None, None]).await
}

/// [`lab`], mac and linux serving their peers from these caches (docs/ci.md W24-R27).
async fn lab_with(objects: [Option<Arc<crate::runner::work::store::Store>>; 2]) -> anyhow::Result<Lab> {
    lab_full(objects, false, [super::wire::VERSION, super::wire::VERSION]).await
}

/// [`lab`] with both runners saying they place (docs/needs.md E5): the directory lists the features
/// each announces.
async fn lab_placing() -> anyhow::Result<Lab> {
    lab_full([None, None], true, [super::wire::VERSION, super::wire::VERSION]).await
}

/// [`lab_with`], mac and linux speaking these reach versions (1: a build from before, W27-R08).
async fn lab_at(objects: [Option<Arc<crate::runner::work::store::Store>>; 2], versions: [u32; 2]) -> anyhow::Result<Lab> {
    lab_full(objects, false, versions).await
}

async fn lab_full(objects: [Option<Arc<crate::runner::work::store::Store>>; 2], place: bool, versions: [u32; 2]) -> anyhow::Result<Lab> {
    let [mac_objects, linux_objects] = objects;
    let [mac_version, linux_version] = versions;
    let dirs = vec![tempfile::tempdir()?, tempfile::tempdir()?, tempfile::tempdir()?];
    let mirrors = dirs.get(2).map(|d| d.path().to_path_buf()).unwrap_or_default();
    let (adopt_tx, adoptions) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
    let using = Arc::new(super::keeper::Using::default());
    let cfg = |name: &str, d: &tempfile::TempDir, adopt: Option<crate::memory::queue::Sender<node::Adoption>>| Config {
        name: name.into(),
        dir: d.path().join("reach"),
        accept: true,
        mirrors: mirrors.clone(),
        using: if name == "linux" { Arc::clone(&using) } else { Arc::default() },
        adopt,
        objects: None,
        sense: false,
        on_battery: kiln_ci::pace::power::OnBattery::default(),
        version: super::wire::VERSION,
        root: Vec::new(),
    };
    let (mac, mac_out) = node::start(Config { objects: mac_objects, version: mac_version, ..cfg("mac", dirs.first().ok_or_else(|| anyhow::anyhow!("dir"))?, None) })?;
    let (linux, linux_out) = node::start(Config { objects: linux_objects, version: linux_version, ..cfg("linux", dirs.get(1).ok_or_else(|| anyhow::anyhow!("dir"))?, Some(adopt_tx)) })?;
    let features = if place { crate::proto::feature::all() } else { Vec::new() };
    let directory = Arc::new(Mutex::new(vec![RunnerInfo { features: features.clone(), ..info("mac", mac.fingerprint()) }, RunnerInfo { features, ..info("linux", linux.fingerprint()) }]));
    // Each runner's current connection, as the server would number them.
    let peers: Arc<Mutex<HashMap<&'static str, u64>>> = Arc::new(Mutex::new(HashMap::from([("mac", 10), ("linux", 20)])));
    let (ctl_tx, ctl_rx) = async_channel::unbounded::<Ctl>();
    let (said_tx, said) = async_channel::unbounded();
    let ended: Arc<Mutex<Vec<String>>> = Arc::default();
    let sizes: Arc<Mutex<Option<serde_json::Value>>> = Arc::default();
    let measured: Arc<Mutex<Vec<(&'static str, serde_json::Value)>>> = Arc::default();
    let (sz, me_) = (Arc::clone(&sizes), Arc::clone(&measured));
    let (m, l, e) = (mac.clone(), linux.clone(), Arc::clone(&ended));
    crate::rt::spawn(async move {
        let mut relay = Relay::default();
        relay.announce("mac", Announce { fingerprint: m.fingerprint().into(), accept: true, version: 1, features: Vec::new() });
        relay.announce("linux", Announce { fingerprint: l.fingerprint().into(), accept: true, version: 1, features: Vec::new() });
        let handle_of = |n: &str| if n == "mac" { m.clone() } else { l.clone() };
        loop {
            use futures::FutureExt as _;
            let (from, msg) = futures::select_biased! {
                c = ctl_rx.recv().fuse() => {
                    match c {
                        Ok(Ctl::Drop(name)) => {
                            let peer = peers.lock().get(name).copied().unwrap_or(0);
                            for (to, sm) in relay.lost(peer, "disconnected") {
                                if let ServerMsg::Reach { link, .. } = &sm {
                                    e.lock().push(link.clone());
                                }
                                deliver(&peers, &handle_of, to, sm).await;
                            }
                            peers.lock().remove(name);
                            handle_of(name).feed(In::Server(false)).await;
                        }
                        Ok(Ctl::Back(name)) => {
                            let next = peers.lock().values().max().copied().unwrap_or(0) + 1;
                            peers.lock().insert(name, next);
                            handle_of(name).feed(In::Server(true)).await;
                        }
                        Ok(Ctl::Key(name, fp)) => {
                            if let Some(r) = directory.lock().iter_mut().find(|r| r.name == name) {
                                r.fingerprint = Some(fp);
                            }
                        }
                        Ok(Ctl::Features(name, features)) => {
                            if let Some(r) = directory.lock().iter_mut().find(|r| r.name == name) {
                                r.features = features;
                            }
                        }
                        Ok(Ctl::Mem(name, total)) => {
                            if let Some(r) = directory.lock().iter_mut().find(|r| r.name == name) {
                                (r.mem_total, r.mem_used) = (total, 0);
                            }
                        }
                        Err(_) => return,
                    }
                    continue;
                }
                x = mac_out.recv().fuse() => ("mac", x),
                x = linux_out.recv().fuse() => ("linux", x),
            };
            let Ok(msg) = msg else { return };
            let Some(from_peer) = peers.lock().get(from).copied() else { continue };
            match msg {
                AgentMsg::Reach { to, link, open, data, close } => {
                    if close.is_some() {
                        e.lock().push(link.clone());
                    }
                    let p = Arc::clone(&peers);
                    let out = relay.forward(from, from_peer, Frame { to, link, open, data, close }, move |n| p.lock().get(n).copied(), str::to_owned).sends(from_peer, false);
                    for (to, sm) in out {
                        deliver(&peers, &handle_of, to, sm).await;
                    }
                }
                AgentMsg::ReachAsk { req, json: asked } => {
                    // An agent's question for kiln's operations comes back as what it asked.
                    let ask: serde_json::Value = serde_json::from_str(&asked).unwrap_or_default();
                    let answer = match ask.get("op").and_then(serde_json::Value::as_str) {
                        Some("call") => serde_json::json!({"ok": {"name": ask.get("name"), "input": ask.get("input"), "session": ask.get("session")}}),
                        Some("sizes") => match sz.lock().clone() {
                            Some(v) => v,
                            None => continue,
                        },
                        Some("measured") => {
                            me_.lock().push((from, ask.clone()));
                            serde_json::json!({})
                        }
                        _ => serde_json::json!({ "runners": *directory.lock() }),
                    };
                    handle_of(from).feed(In::Answer { req, answer }).await;
                }
                other => {
                    let _ = said_tx.send((from, other)).await;
                }
            }
        }
    });
    mac.feed(In::Server(true)).await;
    linux.feed(In::Server(true)).await;
    // The "server" takes word of what a link couldn't read.
    mac.feed(In::Unread(true)).await;
    linux.feed(In::Unread(true)).await;
    Ok(Lab { mac, linux, said, ended, adoptions, ctl: ctl_tx, _dirs: dirs, mirrors, using, sizes, measured })
}

async fn deliver(peers: &Mutex<HashMap<&'static str, u64>>, handle_of: &impl Fn(&str) -> Handle, to: u64, sm: ServerMsg) {
    let name = peers.lock().iter().find(|(_, p)| **p == to).map(|(n, _)| *n);
    let (Some(name), ServerMsg::Reach { from, link, open, data, close }) = (name, sm) else { return };
    handle_of(name).feed(In::Frame { from, link, open, data, close }).await;
}

/// Run `req` from mac through the local driver; every event until the end.
async fn run_from_mac(lab: &Lab, req: RunReq, mid: impl Fn(&Ev) -> Option<Ctl>) -> anyhow::Result<Vec<Ev>> {
    run_from(lab, "mac", req, mid).await
}

/// Run `req` from runner `me` (mac or linux) through the local driver; every event until the end.
async fn run_from(lab: &Lab, me: &'static str, req: RunReq, mid: impl Fn(&Ev) -> Option<Ctl>) -> anyhow::Result<Vec<Ev>> {
    let (ev_tx, ev_rx) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
    let (_ctl_tx, ctl_rx) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
    let handle = if me == "mac" { lab.mac.clone() } else { lab.linux.clone() };
    crate::rt::spawn(async move { local::run(&handle, me, req, ev_tx, ctl_rx).await });
    let mut evs = Vec::new();
    loop {
        match crate::rt::timeout(HANG, ev_rx.recv()).await {
            Ok(Ok(e)) => {
                if let Some(c) = mid(&e) {
                    lab.ctl.send(c).await?;
                }
                let end = matches!(e, Ev::Exit { .. } | Ev::Error(_));
                evs.push(e);
                if end {
                    return Ok(evs);
                }
            }
            Ok(Err(_)) => return Ok(evs),
            Err(_) => anyhow::bail!("timed out; so far: {evs:?}"),
        }
    }
}

fn output(evs: &[Ev], err: bool) -> String {
    evs.iter().filter_map(|e| if let Ev::Output { err: e2, data } = e { (*e2 == err).then(|| String::from_utf8_lossy(data).into_owned()) } else { None }).collect()
}

fn exit(evs: &[Ev]) -> Option<(Option<i32>, Option<String>, u64)> {
    evs.iter().find_map(|e| if let Ev::Exit { code, error, lost } = e { Some((*code, error.clone(), *lost)) } else { None })
}

#[cfg(unix)]
#[test]
fn a_run_on_another_runner_streams_back_over_a_post_quantum_link() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab().await?;
        let req = RunReq { target: "linux/amd64".into(), command: "echo hello from $KILN_REACH_FROM; echo oops >&2; exit 3".into(), ..RunReq::default() };
        let evs = run_from_mac(&lab, req, |_| None).await?;
        assert!(evs.iter().any(|e| matches!(e, Ev::Picked { runner, .. } if runner == "linux")), "{evs:?}");
        assert!(evs.iter().any(|e| matches!(e, Ev::Started { runner, .. } if runner == "linux")));
        assert_eq!(output(&evs, false), "hello from mac\n");
        assert_eq!(output(&evs, true), "oops\n");
        assert_eq!(exit(&evs), Some((Some(3), None, 0)));
        // Both pinned each other's key, and the link is ML-KEM hybrid with AES-256-GCM.
        let s = lab.mac.status().await?;
        let peer = s.get("peers").and_then(|p| p.as_array()).and_then(|p| p.first()).cloned().unwrap_or_default();
        assert_eq!(peer.pointer("/secured/suite").and_then(|v| v.as_str()), Some("TLS13_AES_256_GCM_SHA384"), "{s}");
        assert_eq!(peer.pointer("/secured/group").and_then(|v| v.as_str()), Some("secp384r1MLKEM1024"));
        assert!(s.get("pins").and_then(|p| p.as_array()).is_some_and(|p| p.iter().any(|x| x.get("runner").and_then(|r| r.as_str()) == Some("linux"))));
        let l = lab.linux.status().await?;
        assert!(l.get("pins").and_then(|p| p.as_array()).is_some_and(|p| p.iter().any(|x| x.get("runner").and_then(|r| r.as_str()) == Some("mac"))));
        Ok(())
    })
}

/// What the runners told the "server" their runs used, once some runner told it something, or after
/// a while.
async fn measured_by(lab: &Lab) -> Vec<(&'static str, serde_json::Value)> {
    for _ in 0..200 {
        let told = lab.measured.lock().clone();
        if !told.is_empty() {
            return told;
        }
        crate::rt::sleep(Duration::from_millis(20)).await;
    }
    Vec::new()
}

fn notes(evs: &[Ev]) -> Vec<String> {
    evs.iter().filter_map(|e| if let Ev::Note(n) = e { Some(n.clone()) } else { None }).collect()
}

/// H1 (docs/reach.md, "Sizes from history"), a rolling upgrade: a runner from before (its
/// directory entry names nothing it takes) is asked a run as it always was, and does it; once it's
/// upgraded (it names the feature), the run goes with what its asker knows, says what it used
/// before its exit, and the runner that did it tells the server, under the run's key, as itself.
/// The server never answers what the command's history says meanwhile (audit H3): no run waits.
#[cfg(unix)]
#[test]
fn h1_a_run_goes_with_what_its_asker_knows_only_to_a_runner_that_takes_it() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab().await?;
        let folder = tempfile::tempdir()?;
        let req = || RunReq { target: "linux".into(), command: "echo used  here".into(), cwd: Some(folder.path().display().to_string()), ..RunReq::default() };
        // linux from before.
        lab.ctl.send(Ctl::Features("linux", Vec::new())).await?;
        let evs = run_from_mac(&lab, req(), |_| None).await?;
        anyhow::ensure!(exit(&evs) == Some((Some(0), None, 0)) && output(&evs, false) == "used here\n", "{evs:?}");
        anyhow::ensure!(!notes(&evs).iter().any(|n| n.starts_with("used ")), "a runner from before says nothing of what it used: {evs:?}");
        anyhow::ensure!(measured_by(&lab).await.is_empty(), "a runner asked as before keeps nothing");
        // linux upgraded; mac asks the directory again (it keeps one for a while, docs/reach.md §2).
        lab.ctl.send(Ctl::Features("linux", super::history::features())).await?;
        lab.mac.feed(In::Server(true)).await;
        let evs = run_from_mac(&lab, req(), |_| None).await?;
        anyhow::ensure!(exit(&evs) == Some((Some(0), None, 0)), "{evs:?}");
        let at = |f: &dyn Fn(&Ev) -> bool| evs.iter().position(f);
        let (used, ended) = (at(&|e| matches!(e, Ev::Note(n) if n.starts_with("used "))), at(&|e| matches!(e, Ev::Exit { .. })));
        anyhow::ensure!(used.is_some() && used < ended, "what it used comes before its exit: {evs:?}");
        let told = measured_by(&lab).await;
        let (who, m) = told.first().cloned().ok_or_else(|| anyhow::anyhow!("nothing measured was kept"))?;
        anyhow::ensure!(who == "linux" && told.len() == 1, "the runner that did it tells, once: {told:?}");
        anyhow::ensure!(m.get("command") == Some(&serde_json::json!("echo used here")) && m.get("repo").and_then(serde_json::Value::as_str).is_some_and(|r| r.starts_with("local:mac:")), "{m}");
        anyhow::ensure!(m.pointer("/used/exit") == Some(&serde_json::json!(0)) && m.get("size").is_none_or(serde_json::Value::is_null), "placed by its ask: {m}");
        Ok(())
    })
}

/// H2: history has no admission of its own. A run whose runner the directory says has room goes
/// there and runs, however much it asks: nothing here refuses it (placement admits). And it's sized
/// from history only once the person declared it, from what the server said for an earlier run.
#[cfg(unix)]
#[test]
fn h2_history_refuses_nothing_and_sizes_only_when_declared() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab().await?;
        lab.ctl.send(Ctl::Mem("linux", 2 << 50)).await?;
        let folder = tempfile::tempdir()?;
        let req = |target: &str| RunReq { target: target.into(), command: "echo sized".into(), cwd: Some(folder.path().display().to_string()), ..RunReq::default() };
        let evs = run_from_mac(&lab, req("linux,mem=1024t"), |_| None).await?;
        anyhow::ensure!(exit(&evs) == Some((Some(0), None, 0)) && output(&evs, false) == "sized\n", "nothing refused it: {evs:?}");
        // Undeclared: what history says places nothing.
        let rollup = kiln_ci::sizes::Rollup::of(&[kiln_ci::sizes::Kept { at_ms: 1, sized_mem: 0, usage: kiln_ci::sizes::Usage { peak_mem: 1 << 30, mem_how: kiln_ci::sizes::How::Exact, exit: Some(0), oom_counted: kiln_ci::sizes::Counted::Group, ..Default::default() } }]);
        let answer = |sizes: &str| serde_json::json!({"policy": {"sizes": sizes}, "platforms": {"linux/x86_64": rollup}, "kept_ms": super::history::kept_ms(30)});
        let key = super::history::Key::of("mac", &req("linux,mem=2g")).ok_or_else(|| anyhow::anyhow!("no key"))?;
        for (declared, sized) in [("ask", false), ("history", true)] {
            *lab.sizes.lock() = Some(answer(declared));
            // The first run asks the server; what it says places the next.
            run_from_mac(&lab, req("linux,mem=2g"), |_| None).await?;
            for _ in 0..200 {
                if super::history::cached(&key.hash()).is_some_and(|s| s.policy.sizes == if sized { kiln_ci::sizes::Source::History } else { kiln_ci::sizes::Source::Ask }) {
                    break;
                }
                crate::rt::sleep(Duration::from_millis(20)).await;
            }
            let evs = run_from_mac(&lab, req("linux,mem=2g"), |_| None).await?;
            let why = evs.iter().find_map(|e| if let Ev::Picked { why, .. } = e { Some(why.clone()) } else { None }).unwrap_or_default();
            anyhow::ensure!(why.contains("sized from history: 1 GiB") == sized && exit(&evs) == Some((Some(0), None, 0)), "{declared}: {why}");
        }
        Ok(())
    })
}

/// H9: an agent's question through its runner can't say what a run used; its other questions go
/// through. And the server keeps what a runner said a run used as that runner's, on the platform
/// it knows the runner by, whatever the message says.
#[cfg(unix)]
#[test]
fn h9_what_a_run_used_is_only_its_runners_to_say() -> anyhow::Result<()> {
    use crate::hub::tests::{hear, join_runner, scratch_hub_on};
    use crate::proto::LANE_CONTROL;
    crate::rt::block_on(async move {
        let lab = lab().await?;
        let forged = r#"{"op": "measured", "repo": "acme/app", "command": "make", "used": {"peak_mem": 1, "mem_how": "exact"}}"#;
        let refused = super::history::for_agent(&lab.mac, forged.into()).await;
        anyhow::ensure!(refused.as_ref().is_err_and(|e| e.to_string().contains("never an agent's")), "{refused:?}");
        let call = super::history::for_agent(&lab.mac, r#"{"op": "call", "name": "runners", "input": {}}"#.into()).await?;
        anyhow::ensure!(call.contains("runners"), "{call}");
        crate::rt::sleep(Duration::from_millis(200)).await;
        anyhow::ensure!(lab.measured.lock().is_empty(), "nothing an agent said was kept");
        // The server's side: kept as the runner on whose connection it came, on its platform.
        let hub = scratch_hub_on(Arc::new(kiln_seams::clock::SimClock::new()), |_| {}).await?;
        join_runner(&hub, 2, "linux", 0);
        let said = r#"{"op": "measured", "repo": "acme/app", "command": "make", "runner": "mac", "platform": "macos/aarch64", "used": {"peak_mem": 1073741824, "mem_how": "exact", "exit": 0, "oom_counted": "group"}}"#;
        hear(&hub, 2, LANE_CONTROL, &AgentMsg::ReachAsk { req: 5, json: said.into() }).await?;
        let key = super::history::Key { repo: "acme/app".into(), command: "make".into() }.hash();
        let mut kept = std::collections::BTreeMap::new();
        for _ in 0..200 {
            kept = super::history::rollups(&hub.store, key.clone()).await?;
            if !kept.is_empty() {
                break;
            }
            crate::rt::sleep(Duration::from_millis(20)).await;
        }
        anyhow::ensure!(kept.len() == 1 && kept.get("linux/x86_64").is_some_and(|r| r.runs == 1 && r.peak == 1 << 30), "{kept:?}");
        Ok(())
    })
}

#[cfg(unix)]
#[test]
fn an_agents_question_for_the_server_goes_through_its_runner_and_back() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab().await?;
        let ask = serde_json::json!({"op": "call", "session": "s1", "name": "ci.runs.list", "input": {"limit": 2}});
        // Several at once, each answered as its own.
        let (a, b) = futures::join!(lab.mac.query(ask.to_string()), lab.linux.query(ask.to_string()));
        for answer in [a?, b?] {
            let v: serde_json::Value = serde_json::from_str(&answer)?;
            assert_eq!(v.pointer("/ok/name"), Some(&serde_json::json!("ci.runs.list")), "{v}");
            assert_eq!(v.pointer("/ok/input/limit"), Some(&serde_json::json!(2)));
            assert_eq!(v.pointer("/ok/session"), Some(&serde_json::json!("s1")));
        }
        // Away from the server: refused at once, not left to wait. Until mac's node takes the drop its questions
        // are answered, and one on its way as it takes it is answered as dropped: neither is the refusal, and how
        // long the drop takes to reach the node is a loaded machine's pace. Each question's bound only ends a
        // hang, and so does the whole wait's.
        lab.ctl.send(Ctl::Drop("mac")).await?;
        let until = std::time::Instant::now() + HANG;
        let refused = loop {
            let asked = crate::rt::timeout(HANG, lab.mac.query(ask.to_string())).await.map_err(|_| anyhow::anyhow!("a question away from the server was left to wait"))?;
            match asked.map_err(|e| format!("{e:#}")) {
                Err(e) if !e.contains("dropped before it answered") => break Some(e),
                _ if std::time::Instant::now() >= until => break None,
                _ => {}
            }
            crate::rt::sleep(Duration::from_millis(20)).await;
        };
        assert!(refused.as_deref().is_some_and(|e| e.contains("not connected")), "{refused:?}");
        Ok(())
    })
}

#[cfg(unix)]
#[test]
fn a_dropped_connection_mid_run_loses_nothing() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab().await?;
        let ctl = lab.ctl.clone();
        let req = RunReq { target: "linux".into(), command: "for i in 1 2 3 4 5 6 7 8; do echo line$i; sleep 0.15; done".into(), ..RunReq::default() };
        // After the second line, mac loses the server for a moment, then comes back.
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d = Arc::clone(&dropped);
        let evs = run_from_mac(&lab, req, move |e| {
            if let Ev::Output { data, .. } = e
                && String::from_utf8_lossy(data).contains("line2")
                && !d.swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                let ctl = ctl.clone();
                crate::rt::spawn(async move {
                    crate::rt::sleep(Duration::from_millis(400)).await;
                    let _ = ctl.send(Ctl::Back("mac")).await;
                });
                return Some(Ctl::Drop("mac"));
            }
            None
        })
        .await?;
        assert!(dropped.load(std::sync::atomic::Ordering::Relaxed));
        let lines: Vec<String> = output(&evs, false).lines().map(str::to_owned).collect();
        assert_eq!(lines, (1..=8).map(|i| format!("line{i}")).collect::<Vec<_>>(), "every line exactly once, in order");
        assert_eq!(exit(&evs), Some((Some(0), None, 0)));
        Ok(())
    })
}

#[cfg(unix)]
#[test]
fn a_tree_syncs_over_and_the_command_runs_in_the_copy() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab().await?;
        let repo = tempfile::tempdir()?;
        let r = repo.path();
        let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(r).args(args).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output();
        git(&["init", "-q"])?;
        std::fs::create_dir_all(r.join("crates/app"))?;
        std::fs::write(r.join("crates/app/data.txt"), "synced!\n")?;
        std::fs::write(r.join(".gitignore"), "target/\n")?;
        std::fs::create_dir_all(r.join("target"))?;
        std::fs::write(r.join("target/big"), vec![0u8; 4096])?;
        git(&["add", "."])?;
        git(&["commit", "-qm", "init"])?;
        std::fs::write(r.join("crates/app/data.txt"), "synced and edited!\n")?;
        let req = RunReq {
            target: "linux".into(),
            command: "cat data.txt; git status --porcelain; test -e ../../target && echo LEAKED || true".into(),
            cwd: Some(r.join("crates/app").to_string_lossy().into_owned()),
            sync: true,
            ..RunReq::default()
        };
        let evs = run_from_mac(&lab, req.clone(), |_| None).await?;
        assert!(evs.iter().any(|e| matches!(e, Ev::Synced { sent, .. } if *sent >= 2)), "{evs:?}");
        let out = output(&evs, false);
        assert!(out.starts_with("synced and edited!\n"), "{out}");
        assert!(out.contains("M crates/app/data.txt"), "git sees the same change over there: {out}");
        assert!(!out.contains("LEAKED"), "ignored build output stays home");
        assert!(evs.iter().any(|e| matches!(e, Ev::Started { cwd, .. } if cwd.starts_with(&lab.mirrors.join("mac").to_string_lossy().into_owned()) && cwd.ends_with("crates/app"))));
        // Again: nothing changed here, so all that goes is the index `git status` refreshed there.
        let evs = run_from_mac(&lab, req.clone(), |_| None).await?;
        assert!(evs.iter().any(|e| matches!(e, Ev::Synced { sent: 0 | 1, .. })), "{evs:?}");
        let evs = run_from_mac(&lab, RunReq { command: "true".into(), ..req }, |_| None).await?;
        assert!(evs.iter().any(|e| matches!(e, Ev::Synced { sent: 1, .. })), "{evs:?}");
        Ok(())
    })
}

/// W27-R08 (the Transfers plan's step 12): a link runs at the reach version both ends speak. New to
/// new it's 2, and a sync's workspace, a run and its output each go with a custody frame; a runner
/// from before (1) refuses a Hello at 2 and is asked again at 1, or asks at 1 itself, and no link at
/// 1 carries a custody frame. A run in a synced copy works every way.
#[cfg(unix)]
#[test]
fn w27_r08_a_link_runs_at_the_version_both_speak_and_only_2_carries_custody() -> anyhow::Result<()> {
    use serde_json::Value;
    crate::rt::block_on(async move {
        for (mac_v, linux_v) in [(2, 2), (2, 1), (1, 2), (1, 1)] {
            let lab = lab_at([None, None], [mac_v, linux_v]).await?;
            let src = tempfile::tempdir()?;
            std::fs::write(src.path().join("data.txt"), "synced!\n")?;
            let req = RunReq { target: "linux".into(), command: "cat data.txt".into(), cwd: Some(src.path().to_string_lossy().into_owned()), sync: true, ..RunReq::default() };
            let evs = run_from_mac(&lab, req, |_| None).await?;
            assert_eq!((output(&evs, false).as_str(), exit(&evs)), ("synced!\n", Some((Some(0), None, 0))), "mac at {mac_v}, linux at {linux_v}: {evs:?}");
            let (m, l) = (lab.mac.status().await?, lab.linux.status().await?);
            let (peer, serving) = (m.pointer("/peers/0").cloned().unwrap_or_default(), l.pointer("/serving/0").cloned().unwrap_or_default());
            let at = u64::from(mac_v.min(linux_v));
            let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64);
            assert_eq!((n(&peer, "version"), n(&serving, "version")), (Some(at), Some(at)), "both ends at {at}: {m} {l}");
            let frames = (n(&peer, "custody").unwrap_or(0), n(&serving, "custody").unwrap_or(0));
            if at >= 2 {
                // The doer: the workspace's and the run's; the asker: their receipts and the output's.
                assert!(frames.0 >= 3 && frames.1 >= 2, "custody frames at 2: {frames:?}");
            } else {
                assert_eq!(frames, (0, 0), "no custody frame on a link at 1");
            }
        }
        Ok(())
    })
}

#[cfg(unix)]
#[test]
fn a_copy_is_in_use_while_its_sync_and_run_are_at_it_and_noted_as_the_agents_that_asked() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab().await?;
        let repo = tempfile::tempdir()?;
        let r = repo.path();
        let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(r).args(args).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output();
        git(&["init", "-q"])?;
        git(&["remote", "add", "origin", "https://github.com/hyper-light/kiln.git"])?;
        std::fs::write(r.join("a.txt"), "a\n")?;
        git(&["add", "."])?;
        git(&["commit", "-qm", "init"])?;
        // While the command runs, the copy is in use; it says which agent asked (docs/reach.md §5.1).
        let req = RunReq {
            target: "linux".into(),
            command: "sleep 0.6; echo $KILN_REACH_AGENT".into(),
            cwd: Some(r.to_string_lossy().into_owned()),
            sync: true,
            env: vec![(super::keeper::AGENT_ENV.to_owned(), "agent-7".to_owned())],
            ..RunReq::default()
        };
        let key = super::sync::mirror_key("mac", &std::fs::canonicalize(r)?);
        let using = Arc::clone(&lab.using);
        let k = ("mac".to_owned(), key.clone());
        let seen_busy = crate::rt::spawn(async move {
            for _ in 0..200 {
                if using.snapshot().get(&k).is_some_and(|u| u.busy > 0) {
                    return true;
                }
                crate::rt::sleep(Duration::from_millis(10)).await;
            }
            false
        });
        let evs = run_from_mac(&lab, req, |_| None).await?;
        assert_eq!(output(&evs, false), "agent-7\n");
        assert!(matches!(seen_busy.await, Ok(true)), "in use while it ran");
        // Done with, it's no longer in use: the last use and the agent stay noted.
        let mut done = None;
        for _ in 0..100 {
            done = lab.using.snapshot().get(&("mac".to_owned(), key.clone())).cloned().filter(|u| u.busy == 0);
            if done.is_some() {
                break;
            }
            crate::rt::sleep(Duration::from_millis(20)).await;
        }
        let done = done.ok_or_else(|| anyhow::anyhow!("still in use: {:?}", lab.using.snapshot()))?;
        assert_eq!(done.agent.as_deref(), Some("agent-7"));
        assert!(done.used > 0);
        assert!(lab.mirrors.join("mac").join(&key).join("a.txt").is_file());
        Ok(())
    })
}

#[cfg(unix)]
#[test]
fn an_impostor_key_is_refused_and_a_changed_key_is_not_believed() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab().await?;
        // First contact goes fine and pins linux's key.
        let ok = run_from_mac(&lab, RunReq { target: "linux".into(), command: "true".into(), ..RunReq::default() }, |_| None).await?;
        assert_eq!(exit(&ok).map(|e| e.0), Some(Some(0)), "{ok:?}");
        // The server now says linux has another key (a compromised server, a swapped machine).
        lab.ctl.send(Ctl::Key("linux", "ab".repeat(32))).await?;
        // mac's connection drops and comes back: the next link checks the pin against that.
        lab.ctl.send(Ctl::Drop("mac")).await?;
        crate::rt::sleep(Duration::from_millis(100)).await;
        lab.ctl.send(Ctl::Back("mac")).await?;
        crate::rt::sleep(Duration::from_millis(100)).await;
        let evs = run_from_mac(&lab, RunReq { target: "linux".into(), command: "echo must-not-run".into(), ..RunReq::default() }, |_| None).await?;
        assert!(!output(&evs, false).contains("must-not-run"));
        assert!(evs.iter().any(|e| matches!(e, Ev::Error(w) if w.contains("reach key changed"))), "{evs:?}");
        Ok(())
    })
}

#[cfg(unix)]
#[test]
fn a_runner_that_presents_the_wrong_key_gets_nothing() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab().await?;
        // Before anything is pinned, the directory lists a key that isn't linux's.
        lab.ctl.send(Ctl::Key("linux", "cd".repeat(32))).await?;
        let evs = run_from_mac(&lab, RunReq { target: "linux".into(), command: "echo should-not-run".into(), ..RunReq::default() }, |_| None).await?;
        assert!(!output(&evs, false).contains("should-not-run"));
        assert!(evs.iter().any(|e| matches!(e, Ev::Error(_) | Ev::Exit { code: None, .. })), "{evs:?}");
        Ok(())
    })
}

#[cfg(unix)]
#[test]
fn thousands_of_small_files_and_a_big_one_sync_without_stalling() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab().await?;
        let tree = tempfile::tempdir()?;
        let t = tree.path();
        for d in 0..60 {
            let dir = t.join(format!("d{d:02}"));
            std::fs::create_dir_all(&dir)?;
            for f in 0..100 {
                std::fs::write(dir.join(format!("f{f:03}.txt")), format!("file {d}/{f}\n").repeat(8))?;
            }
        }
        std::fs::write(t.join("big.bin"), (0..5_000_000u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>())?;
        let req = RunReq {
            target: "linux".into(),
            command: "find . -type f | wc -l; wc -c < big.bin; cksum < big.bin".into(),
            cwd: Some(t.to_string_lossy().into_owned()),
            sync: true,
            ..RunReq::default()
        };
        let started = std::time::Instant::now();
        let evs = run_from_mac(&lab, req.clone(), |_| None).await?;
        let took = started.elapsed();
        let out = output(&evs, false);
        let lines: Vec<&str> = out.lines().map(str::trim).collect();
        let local_sum = std::process::Command::new("sh").arg("-c").arg("cksum < big.bin").current_dir(t).output()?;
        assert_eq!(lines.first().copied(), Some("6001"), "{evs:?}");
        assert_eq!(lines.get(1).copied(), Some("5000000"));
        assert_eq!(lines.get(2).copied(), Some(String::from_utf8_lossy(&local_sum.stdout).trim()));
        assert!(took < Duration::from_secs(60), "took {took:?}");
        // Again, unchanged: nothing goes.
        let evs = run_from_mac(&lab, req, |_| None).await?;
        assert!(evs.iter().any(|e| matches!(e, Ev::Synced { sent: 0, .. })), "{evs:?}");
        Ok(())
    })
}

#[test]
fn a_run_on_another_machine_is_marked_as_one_from_its_start() {
    use serde_json::json;
    // Where it goes, as said; empty while kiln picks the best.
    assert_eq!(super::remote("kiln", "run", &json!({"on": "macos", "command": "make"})).as_deref(), Some("macos"));
    assert_eq!(super::remote("kiln-reach", "run", &json!({"command": "make"})).as_deref(), Some(""));
    assert_eq!(super::remote("kiln_reach", "run", &json!({"on": " linux/amd64 "})).as_deref(), Some("linux/amd64"));
    // Not a command elsewhere: another of its tools, or another server's `run`.
    assert_eq!(super::remote("kiln", "runners", &json!({})), None);
    assert_eq!(super::remote("github", "run", &json!({"on": "x"})), None);
}

#[test]
fn the_timeline_shows_a_remote_run_as_a_command_with_where_it_ran() {
    use crate::harness::model::ToolKind;
    let input = serde_json::json!({"command": "cargo test", "on": "linux/amd64"});
    let (kind, title, detail) = super::describe("kiln-reach", "run", &input).unwrap_or((ToolKind::Other, String::new(), None));
    assert_eq!((kind, title.as_str(), detail.as_deref()), (ToolKind::Shell, "cargo test", Some("on linux/amd64")));
    assert!(super::describe("other-server", "run", &input).is_none());
    // Claude Code hands the tool's text; Codex its structured answer.
    let text = serde_json::json!(r#"{"runner":"beta","output":"ok\n","running":false,"exit_code":3}"#);
    let s = super::shown("mcp__kiln-reach__run", &text);
    assert!(s.as_ref().is_some_and(|s| s.output == "ok\n" && s.exit_code == Some(3) && s.runner.as_deref() == Some("beta")));
    let structured = serde_json::json!({"structuredContent": {"runner": "beta", "output": "tick\n", "running": true, "id": "r2"}});
    let s = super::shown("mcp__kiln_reach__run", &structured);
    assert!(s.is_some_and(|s| s.output.contains("still running in the background as r2") && s.exit_code.is_none()));
    assert!(super::shown("mcp__kiln-reach__runners", &text).is_none());
}

#[cfg(unix)]
#[test]
fn a_hand_over_arrives_whole_and_says_how_it_went() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab().await?;
        // linux takes over what it is handed, checking it arrived whole.
        let taker = lab.adoptions.clone();
        crate::rt::spawn(async move {
            if let Ok(a) = taker.recv().await {
                let ok = a.from == "mac" && a.data.len() == 5_000_000 && a.data.iter().enumerate().all(|(i, b)| *b == (i % 253) as u8);
                let _ = a.reply.send(if ok { Ok(()) } else { Err(format!("arrived wrong: {} bytes from {}", a.data.len(), a.from)) });
            }
        });
        let data: Vec<u8> = (0..5_000_000usize).map(|i| (i % 253) as u8).collect();
        lab.mac.adopt("linux", &data, false).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
        // A refusal comes back as it was said.
        let taker = lab.adoptions.clone();
        crate::rt::spawn(async move {
            if let Ok(a) = taker.recv().await {
                let _ = a.reply.send(Err("no room for it here".into()));
            }
        });
        let refused = lab.mac.adopt("linux", b"small", false).await;
        assert_eq!(refused, Err(node::NotAdopted::Refused("no room for it here".to_owned())));
        // A runner that takes nothing over says so.
        let nobody = lab.linux.adopt("mac", b"x", false).await;
        assert!(nobody.is_err_and(|e| e.to_string().contains("doesn't take over")));
        Ok(())
    })
}

/// CP-T3.2 (the sixth adversarial check's): a hand-over whose answer is lost after all of it went
/// may have been taken over, and says so (`Unknown`, never a plain refusal: the runner it left
/// would go on with the session too). Asked again, a runner from before NOT_TAKEN that says it runs
/// the session already took it over the time before; said without a try before, that's a refusal.
/// Refused saying it wasn't taken, it isn't there; refused otherwise after a try whose answer was
/// lost, it may be (the seventh check's).
#[cfg(unix)]
#[test]
fn a_hand_over_whose_answer_is_lost_may_have_been_taken_over() -> anyhow::Result<()> {
    use crate::harness::host::RUNS_HERE;
    crate::rt::block_on(async move {
        let lab = lab().await?;
        // linux takes it over, and its connection drops before its answer goes back.
        let (taker, ctl) = (lab.adoptions.clone(), lab.ctl.clone());
        crate::rt::spawn(async move {
            if let Ok(a) = taker.recv().await {
                let _ = ctl.send(Ctl::Drop("linux")).await;
                crate::rt::sleep(Duration::from_millis(300)).await;
                let _ = a.reply.send(Ok(()));
            }
        });
        let lost = lab.mac.adopt("linux", b"a session", false).await;
        assert!(matches!(lost, Err(node::NotAdopted::Unknown(_))), "{lost:?}");
        lab.ctl.send(Ctl::Back("linux")).await?;
        let answers = |answer: Result<(), String>| {
            let taker = lab.adoptions.clone();
            crate::rt::spawn(async move {
                if let Ok(a) = taker.recv().await {
                    let _ = a.reply.send(answer);
                }
            });
        };
        answers(Err(RUNS_HERE.into()));
        assert_eq!(lab.mac.adopt("linux", b"a session", true).await, Ok(()), "asked again: taken over the time before");
        answers(Err(RUNS_HERE.into()));
        assert_eq!(lab.mac.adopt("linux", b"a session", false).await, Err(node::NotAdopted::Refused(RUNS_HERE.into())));
        // Asked again and refused, saying it wasn't taken: it isn't there.
        answers(Err(format!("{}no room for it here", node::NOT_TAKEN)));
        assert_eq!(lab.mac.adopt("linux", b"a session", true).await, Err(node::NotAdopted::Refused("no room for it here".into())));
        // Asked again and refused by a runner from before NOT_TAKEN: it can't say whether the time
        // before took it over (the session may have ended there since).
        answers(Err("no room for it here".into()));
        let unsure = lab.mac.adopt("linux", b"a session", true).await;
        assert!(matches!(unsure, Err(node::NotAdopted::Unknown(_))), "{unsure:?}");
        Ok(())
    })
}

/// Run `req` from mac as `kiln run --reuse` does, against `svc`; every event until the end.
async fn reuse_from_mac(lab: &Lab, svc: &Arc<crate::runner::work::service::Service>, req: RunReq) -> anyhow::Result<Vec<Ev>> {
    let (ev_tx, ev_rx) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
    let (_ctl_tx, ctl_rx) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
    let (mac, svc) = (lab.mac.clone(), Arc::clone(svc));
    crate::rt::spawn(async move { super::reuse::run_with(Some(&svc), &mac, "mac", req, ev_tx, ctl_rx).await });
    let mut evs = Vec::new();
    loop {
        match crate::rt::timeout(Duration::from_secs(60), ev_rx.recv()).await {
            Ok(Ok(e)) => {
                let end = matches!(e, Ev::Exit { .. } | Ev::Error(_));
                evs.push(e);
                if end {
                    return Ok(evs);
                }
            }
            Ok(Err(_)) => return Ok(evs),
            Err(_) => anyhow::bail!("timed out; so far: {evs:?}"),
        }
    }
}

fn noted(evs: &[Ev], what: &str) -> bool {
    evs.iter().any(|e| matches!(e, Ev::Note(n) if n.starts_with(what)))
}

/// W24-R24: `kiln run --reuse` runs a command once and records its success; the same run over an
/// identical tree gives that result back, its output as it was, without running (nor syncing); a
/// changed tree runs again; a failure is never recorded; a run that reads input isn't keyed.
#[cfg(unix)]
#[test]
fn w24_r24_kiln_run_reuse_gives_back_an_identical_runs_result() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab().await?;
        let cas = tempfile::tempdir()?;
        let svc = crate::runner::work::service::Service::open(cas.path())?;
        let repo = tempfile::tempdir()?;
        let r = repo.path();
        let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(r).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output();
        git(&["init", "-q"])?;
        std::fs::create_dir_all(r.join("crates/app"))?;
        std::fs::write(r.join("crates/app/data.txt"), "one\n")?;
        git(&["add", "."])?;
        git(&["commit", "-qm", "init"])?;
        // What ran, counted outside the tree.
        let ran = tempfile::tempdir()?;
        let count = ran.path().join("count");
        let req = RunReq {
            target: "linux".into(),
            command: format!("echo x >> {}; cat data.txt; echo to-stderr >&2", count.display()),
            cwd: Some(r.join("crates/app").to_string_lossy().into_owned()),
            sync: true,
            ..RunReq::default()
        };
        let runs = || std::fs::read_to_string(&count).map(|s| s.lines().count()).unwrap_or(0);
        let first = reuse_from_mac(&lab, &svc, req.clone()).await?;
        assert_eq!((exit(&first), output(&first, false), output(&first, true)), (Some((Some(0), None, 0)), "one\n".to_owned(), "to-stderr\n".to_owned()), "{first:?}");
        assert!(noted(&first, "recorded") && runs() == 1, "{first:?}");
        // Identical: the same result, nothing run or synced.
        let again = reuse_from_mac(&lab, &svc, req.clone()).await?;
        assert_eq!((exit(&again), output(&again, false), output(&again, true)), (Some((Some(0), None, 0)), "one\n".to_owned(), "to-stderr\n".to_owned()), "{again:?}");
        assert!(noted(&again, "reused") && runs() == 1, "{again:?}");
        assert!(!again.iter().any(|e| matches!(e, Ev::Synced { .. } | Ev::Started { .. })), "{again:?}");
        // The tree changed: it runs.
        std::fs::write(r.join("crates/app/data.txt"), "two\n")?;
        let changed = reuse_from_mac(&lab, &svc, req.clone()).await?;
        assert!(output(&changed, false) == "two\n" && noted(&changed, "recorded") && runs() == 2, "{changed:?}");
        // Back as it was: the first result again.
        std::fs::write(r.join("crates/app/data.txt"), "one\n")?;
        let back = reuse_from_mac(&lab, &svc, req.clone()).await?;
        assert!(output(&back, false) == "one\n" && noted(&back, "reused") && runs() == 2, "{back:?}");
        // A failure isn't recorded: it runs each time.
        let failing = RunReq { command: format!("echo x >> {}; exit 3", count.display()), ..req.clone() };
        for n in [3, 4] {
            let f = reuse_from_mac(&lab, &svc, failing.clone()).await?;
            assert!(exit(&f) == Some((Some(3), None, 0)) && !noted(&f, "recorded") && runs() == n, "{f:?}");
        }
        // Input isn't part of any key: it runs, saying why.
        let input = reuse_from_mac(&lab, &svc, RunReq { stdin: true, ..req.clone() }).await;
        assert!(input.is_ok_and(|evs| noted(&evs, "not reused: it reads input")));
        Ok(())
    })
}

/// Peers as a test names them: these runners, through this node.
struct Via(Handle, Vec<String>);

impl crate::runner::work::peers::Peers for Via {
    fn holders<'a>(&'a self, _repo: &'a str, _digest: &'a str) -> futures::future::BoxFuture<'a, Vec<String>> {
        Box::pin(async move { self.1.clone() })
    }

    fn get<'a>(&'a self, peer: &'a str, repo: &'a str, digest: &'a str, into: &'a std::path::Path) -> futures::future::BoxFuture<'a, anyhow::Result<bool>> {
        Box::pin(async move { self.0.object(peer, repo, digest, into).await })
    }
}

/// W24-R27: two runners, one holding a 1 GiB object of a repository's cache. The other fetches it
/// from that peer over their link, not the server, and keeps it once its digest checks out; a
/// repository it wasn't held for gets nothing; a copy that isn't what its name says is refused,
/// and nothing of it is kept.
#[test]
fn a_peers_1_gib_object_comes_over_the_link_and_a_corrupted_copy_is_refused() -> anyhow::Result<()> {
    use std::io::Write as _;
    use crate::runner::work::peers::fetch;
    use crate::runner::work::store::{Scope, Store};
    crate::rt::block_on(async move {
        let d = tempfile::tempdir()?;
        let holder = Arc::new(Store::open(&d.path().join("linux"))?);
        let big = d.path().join("big");
        {
            let mut f = std::fs::File::create(&big)?;
            let block: Vec<u8> = (0..1u32 << 20).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
            for n in 0..1024u32 {
                let mut b = block.clone();
                if let Some(head) = b.get_mut(..4) {
                    head.copy_from_slice(&n.to_le_bytes());
                }
                f.write_all(&b)?;
            }
        }
        let (digest, size) = holder.put_file(&big)?;
        anyhow::ensure!(size == 1 << 30, "{size}");
        holder.hold(&Scope::new("acme/app", "trusted"), &digest)?;
        let lab = lab_with([None, Some(Arc::clone(&holder))]).await?;
        let peers = Via(lab.mac.clone(), vec!["linux".to_owned()]);
        let mac = Arc::new(Store::open(&d.path().join("mac"))?);
        let got = fetch(&mac, &peers, "acme/app", &digest).await?;
        anyhow::ensure!(got.from.as_deref() == Some("linux") && got.refused.is_empty() && mac.has(&digest), "{got:?}");
        // Another repository's ask finds nothing of it.
        let other = Arc::new(Store::open(&d.path().join("other"))?);
        let none = fetch(&other, &peers, "acme/else", &digest).await?;
        anyhow::ensure!(none.from.is_none() && !other.has(&digest), "{none:?}");
        // The copy linux serves from, corrupted: refused, and not kept.
        let served = holder.root().join("peers").join(digest.replace(':', "-"));
        let mut bytes = std::fs::read(&served)?;
        if let Some(b) = bytes.get_mut(12_345) {
            *b ^= 0xff;
        }
        std::fs::write(&served, &bytes)?;
        drop(bytes);
        let fresh = Arc::new(Store::open(&d.path().join("fresh"))?);
        let bad = fetch(&fresh, &peers, "acme/app", &digest).await?;
        anyhow::ensure!(bad.from.is_none() && bad.refused == ["linux"] && !fresh.has(&digest), "{bad:?}");
        Ok(())
    })
}

/// What a runner's node says of `peer` now (`key`: its `link`, or the `error` its last link
/// ended on), as its status says.
async fn of_peer(h: &Handle, peer: &str, key: &str) -> anyhow::Result<Option<String>> {
    let s = h.status().await?;
    let peers = s.get("peers").and_then(serde_json::Value::as_array).cloned().unwrap_or_default();
    Ok(peers.iter().find(|p| p.get("runner").and_then(serde_json::Value::as_str) == Some(peer)).and_then(|p| p.get(key)).and_then(serde_json::Value::as_str).map(str::to_owned))
}

/// The link a runner's node has to `peer` now, as its status says.
async fn link_to(h: &Handle, peer: &str) -> anyhow::Result<Option<String>> {
    of_peer(h, peer, "link").await
}

/// Typed messages, step 5: a message a runner can't read inside a link is answered `Unread`
/// there, and that link ends, alone: the link between the two the other way goes on. The runner
/// that couldn't read it tells the server, naming the runner that sent it, and the server's row
/// names both.
#[cfg(unix)]
#[test]
fn a_message_a_runner_cant_read_in_a_link_is_answered_unread_and_only_its_link_ends() -> anyhow::Result<()> {
    use crate::hub::tests::{hear, join_runner, refused_rows, scratch_hub_on};
    use crate::proto::{CAP_UNREAD, LANE_CONTROL, LANE_REACH};
    use crate::wire::unread::{Code, Page, Wire};
    crate::rt::block_on(async move {
        let lab = lab().await?;
        // A link each way: mac runs on linux, and linux on mac.
        let evs = run_from(&lab, "mac", RunReq { target: "linux/amd64".into(), command: "echo there".into(), ..RunReq::default() }, |_| None).await?;
        assert_eq!(output(&evs, false), "there\n", "{evs:?}");
        let evs = run_from(&lab, "linux", RunReq { target: "macos/aarch64".into(), command: "echo back".into(), ..RunReq::default() }, |_| None).await?;
        assert_eq!(output(&evs, false), "back\n", "{evs:?}");
        let (garbled, other) = (link_to(&lab.mac, "linux").await?, link_to(&lab.linux, "mac").await?);
        anyhow::ensure!(garbled.is_some() && other.is_some() && garbled != other, "a link each way: {garbled:?} {other:?}");
        // Mac sends linux a message of a kind no runner has (variant 99) on its link.
        lab.mac.garble("linux", vec![0, 0, 0, 1, 99]).await;
        let (from, said) = loop {
            let (from, m) = crate::rt::timeout(HANG, lab.said.recv()).await??;
            if matches!(m, AgentMsg::LinkUnread { .. }) {
                break (from, m);
            }
        };
        match &said {
            AgentMsg::LinkUnread { from: sender, link, what, code, why } => {
                assert_eq!((from, sender.as_str(), Some(link), what.as_str(), *code), ("linux", "mac", garbled.as_ref(), "variant 99", Code::Unknown), "{why}");
            }
            other => anyhow::bail!("the server heard {other:?}"),
        }
        // Mac was answered Unread, and its link ended on that word, not on a close alone; linux's
        // link to mac goes on.
        let deadline = std::time::Instant::now() + HANG;
        while link_to(&lab.mac, "linux").await? == garbled && std::time::Instant::now() < deadline {
            crate::rt::sleep(Duration::from_millis(50)).await;
        }
        anyhow::ensure!(link_to(&lab.mac, "linux").await? != garbled, "the garbled link ended");
        let ended_on = of_peer(&lab.mac, "linux", "error").await?.unwrap_or_default();
        anyhow::ensure!(ended_on.starts_with("linux couldn't read variant 99 (unknown)"), "mac's link ended on {ended_on:?}, not on linux's Unread");
        anyhow::ensure!(link_to(&lab.linux, "mac").await? == other, "the other link goes on");
        // The server, told so on reach's lane, notes it against mac, the runner that sent it,
        // naming linux, which couldn't read it, on both runners' pages, as its relay knows the
        // link between them.
        let hub = scratch_hub_on(Arc::new(kiln_seams::clock::SimClock::new()), |_| {}).await?;
        join_runner(&hub, 1, "mac", CAP_UNREAD);
        join_runner(&hub, 2, "linux", CAP_UNREAD);
        hear(&hub, 2, LANE_CONTROL, &AgentMsg::ReachHello { json: serde_json::to_string(&Announce { fingerprint: "f".into(), accept: true, version: 1, features: Vec::new() })? }).await?;
        let link = garbled.clone().unwrap_or_default();
        hear(&hub, 1, LANE_REACH, &AgentMsg::Reach { to: "linux".into(), link: link.clone(), open: true, data: Vec::new(), close: None }).await?;
        hear(&hub, 2, LANE_REACH, &said).await?;
        let rows = refused_rows(&hub, "runner:mac").await?;
        anyhow::ensure!(rows.iter().any(|(t, r, d)| t == "reach_link" && r == "unknown" && d.starts_with("variant 99") && d.contains(&format!("as linux read it, on link {link}"))), "{rows:?}");
        for name in ["mac", "linux"] {
            let shown = hub.refusals.load().kept(&Page::Runner(name.into())).is_some_and(|k| k.counts.iter().any(|c| c.wire == Wire::ReachLink && c.code == Code::Unknown));
            anyhow::ensure!(shown, "on {name}'s page");
        }
        // And mac reaches linux again, on a new link.
        let evs = run_from(&lab, "mac", RunReq { target: "linux/amd64".into(), command: "echo again".into(), ..RunReq::default() }, |_| None).await?;
        assert_eq!(output(&evs, false), "again\n", "{evs:?}");
        anyhow::ensure!(link_to(&lab.linux, "mac").await? == other, "the other link still goes on");
        // Of every link that ended in the test, as the relay saw them end, only the garbled one.
        let ended: std::collections::BTreeSet<String> = lab.ended.lock().iter().cloned().collect();
        let others = ended.iter().filter(|l| Some(*l) != garbled.as_ref()).count();
        anyhow::ensure!(ended.contains(&link) && others == 0, "links dropped beside the garbled one: {others} (ended: {ended:?})");
        Ok(())
    })
}

/// Typed messages, step 5, on the server: what its relay can't take is refused typed and noted,
/// while the link goes on (a frame from a runner that isn't one of a link's ends, a job pod's);
/// its other refusals carry their code to a runner that takes word of them; word of what a link
/// couldn't read is noted against the runner that sent it, on both runners' pages, and refused
/// when the relay knows no such link between them; a question it doesn't know is answered
/// `unknown`.
#[test]
fn the_server_refuses_what_its_relay_cant_take_typed_and_notes_it() -> anyhow::Result<()> {
    use crate::hub::tests::{hear, join_runner, refused_rows, scratch_hub_on};
    use crate::proto::{CAP_UNREAD, CAP_WIRE, LANE_CONTROL, LANE_REACH};
    use crate::wire::unread::{Code, Kept, Page, Wire};
    crate::rt::block_on(async {
        let clock = kiln_seams::clock::SimClock::new();
        let hub = scratch_hub_on(Arc::new(clock.clone()), |_| {}).await?;
        for (peer, name) in [(1, "mac"), (2, "linux"), (3, "intruder")] {
            join_runner(&hub, peer, name, CAP_UNREAD | CAP_WIRE);
        }
        let hello = AgentMsg::ReachHello { json: serde_json::to_string(&Announce { fingerprint: "f".into(), accept: true, version: 1, features: Vec::new() })? };
        hear(&hub, 1, LANE_CONTROL, &hello).await?;
        hear(&hub, 2, LANE_CONTROL, &hello).await?;
        let frame = |to: &str, open: bool| AgentMsg::Reach { to: to.into(), link: "L1".into(), open, data: b"x".to_vec(), close: None };
        hear(&hub, 1, LANE_REACH, &frame("linux", true)).await?;
        // The relay's owner takes frames in its turn: what it answers comes after those sent before.
        anyhow::ensure!(hub.relay().ask(|r| r.ends("L1")).await.flatten() == Some(("mac".into(), "linux".into())), "the link is open");
        let told = |hub: &crate::hub::Hub, to: u64| {
            hub.sent.lock().iter().rev().find_map(|(p, m)| match m {
                ServerMsg::Unread { wire, code, why, .. } if *p == to => Some((*wire, *code, why.clone())),
                _ => None,
            })
        };
        // A third runner's frame on it goes nowhere: refused to it, typed, and noted naming the
        // link's ends, while the link goes on.
        clock.advance(Duration::from_millis(100));
        hear(&hub, 3, LANE_REACH, &frame("linux", false)).await?;
        hub.relay().ask(|_| ()).await;
        let t = told(&hub, 3);
        anyhow::ensure!(t.as_ref().is_some_and(|(w, c, why)| *w == Wire::Relay && *c == Code::Malformed && why.contains("link L1 is mac's and linux's")), "{t:?}");
        anyhow::ensure!(hub.relay().ask(|r| r.links()).await == Some(1), "the link goes on");
        let rows = refused_rows(&hub, "runner:intruder").await?;
        anyhow::ensure!(rows.iter().any(|(t, r, d)| t == "relay" && r == "malformed" && d.contains("link L1 is mac's and linux's")), "{rows:?}");
        let shown = |name: &str, wire: Wire, code: Code| hub.refusals.load().kept(&Page::Runner(name.into())).is_some_and(|k| k.counts.iter().any(|c| c.wire == wire && c.code == code));
        anyhow::ensure!(shown("intruder", Wire::Relay, Code::Malformed), "on the intruder's page");
        // The relay's other refusals carry their code to a runner that takes word of them.
        hear(&hub, 1, LANE_REACH, &frame("linux", true)).await?;
        hub.relay().ask(|_| ()).await;
        anyhow::ensure!(hub.sent.lock().iter().any(|(p, m)| *p == 1 && matches!(m, ServerMsg::ReachRefused { code: super::relay::Refusal::Taken, .. })), "a link id taken, said so");
        // A runner from before (no CAP_UNREAD) is told in words, as before, and nothing new; its
        // frame on a link not its own still goes nowhere and is noted.
        join_runner(&hub, 4, "old", CAP_WIRE);
        hear(&hub, 4, LANE_REACH, &frame("linux", true)).await?;
        hub.relay().ask(|_| ()).await;
        let to_old: Vec<ServerMsg> = hub.sent.lock().iter().filter(|(p, _)| *p == 4).map(|(_, m)| m.clone()).collect();
        anyhow::ensure!(matches!(to_old.as_slice(), [ServerMsg::Reach { open: false, close: Some(why), .. }] if why.contains("taken")), "{to_old:?}");
        clock.advance(Duration::from_millis(100));
        hear(&hub, 4, LANE_REACH, &frame("linux", false)).await?;
        hub.relay().ask(|_| ()).await;
        anyhow::ensure!(hub.sent.lock().iter().filter(|(p, _)| *p == 4).count() == 1, "the old runner is told nothing new");
        anyhow::ensure!(shown("old", Wire::Relay, Code::Malformed) && hub.relay().ask(|r| r.links()).await == Some(1), "noted on its page, and the link goes on");
        // A job pod's frame: refused, and noted.
        hub.change_agent("intruder", |a| a.ephemeral = true);
        clock.advance(Duration::from_millis(100));
        hear(&hub, 3, LANE_REACH, &frame("mac", true)).await?;
        let t = told(&hub, 3);
        anyhow::ensure!(t.as_ref().is_some_and(|(_, _, why)| why.contains("job pod")), "{t:?}");
        // Word of what a link couldn't read: noted against the runner that sent it, on both
        // runners' pages.
        clock.advance(Duration::from_millis(100));
        hear(&hub, 2, LANE_CONTROL, &AgentMsg::LinkUnread { from: "mac".into(), link: "L1".into(), what: "variant 99".into(), code: Code::Unknown, why: "Ask has 12 variants".into() }).await?;
        let total = |name: &str| hub.refusals.load().kept(&Page::Runner(name.into())).map(Kept::total);
        anyhow::ensure!((total("mac"), total("linux")) == (Some(1), Some(1)), "{:?} {:?}", total("mac"), total("linux"));
        let rows = refused_rows(&hub, "runner:mac").await?;
        anyhow::ensure!(rows.iter().any(|(t, r, d)| t == "reach_link" && r == "unknown" && d.contains("as linux read it, on link L1")), "{rows:?}");
        // About a link the relay doesn't know between them: refused, and nothing against linux.
        clock.advance(Duration::from_millis(100));
        hear(&hub, 3, LANE_CONTROL, &AgentMsg::LinkUnread { from: "linux".into(), link: "L1".into(), what: "variant 9".into(), code: Code::Malformed, why: "cut short".into() }).await?;
        let t = told(&hub, 3);
        anyhow::ensure!(t.as_ref().is_some_and(|(w, _, why)| *w == Wire::Relay && why.contains("no such link")), "{t:?}");
        anyhow::ensure!(total("linux") == Some(1), "nothing against linux");
        // A question the server doesn't know is answered unknown.
        hear(&hub, 1, LANE_CONTROL, &AgentMsg::ReachAsk { req: 9, json: r#"{"op":"teleport"}"#.into() }).await?;
        let answer = hub.sent.lock().iter().find_map(|(p, m)| match m {
            ServerMsg::ReachAnswer { req: 9, json } if *p == 1 => serde_json::from_str::<serde_json::Value>(json).ok(),
            _ => None,
        });
        anyhow::ensure!(answer.as_ref().and_then(|a| a.get("code")).and_then(serde_json::Value::as_str) == Some("unknown"), "{answer:?}");
        anyhow::ensure!(shown("mac", Wire::RunnerPayload, Code::Unknown), "the unknown question on mac's page");
        Ok(())
    })
}

/// The relay knows a link's ends while it's open, and a while after it closed.
#[test]
fn a_links_ends_are_known_open_and_lately_closed() {
    let mut r = Relay::default();
    r.announce("linux", Announce { fingerprint: "f".into(), accept: true, version: 1, features: Vec::new() });
    let peers = |n: &str| (n == "linux").then_some(20).or((n == "mac").then_some(10));
    r.forward("mac", 10, Frame { to: "linux".into(), link: "L1".into(), open: true, data: Vec::new(), close: None }, peers, str::to_owned);
    assert_eq!(r.ends("L1"), Some(("mac".into(), "linux".into())));
    r.forward("mac", 10, Frame { to: "linux".into(), link: "L1".into(), open: false, data: Vec::new(), close: Some("done".into()) }, peers, str::to_owned);
    assert_eq!(r.links(), 0);
    assert_eq!(r.ends("L1"), Some(("mac".into(), "linux".into())), "kept a while after it closed");
    assert_eq!(r.ends("L2"), None);
}

/// Start `req` from `me` through the local driver and, once it started, go as a client does
/// (`kiln run --detach` killed, its terminal closed): its controls and its events dropped.
async fn start_then_leave(lab: &Lab, me: &'static str, req: RunReq) -> anyhow::Result<()> {
    let (ev_tx, ev_rx) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
    let (ctl_tx, ctl_rx) = crate::memory::queue::unbounded::<local::Req>(&crate::memory::owners::CHANNELS);
    let handle = if me == "mac" { lab.mac.clone() } else { lab.linux.clone() };
    crate::rt::spawn(async move { local::run(&handle, me, req, ev_tx, ctl_rx).await });
    loop {
        match crate::rt::timeout(HANG, ev_rx.recv()).await {
            Ok(Ok(Ev::Started { .. })) => break,
            Ok(Ok(e @ (Ev::Exit { .. } | Ev::Error(_)))) => anyhow::bail!("it ended before its client went: {e:?}"),
            Ok(Ok(_)) => {}
            Ok(Err(_)) => anyhow::bail!("its events ended before it started"),
            Err(_) => anyhow::bail!("it didn't start in {HANG:?}"),
        }
    }
    drop(ctl_tx);
    drop(ev_rx);
    Ok(())
}

/// What `done` holds once [`past_the_buffer`]'s command wrote it: given up on only once the MiB it notes it wrote
/// (`moved`, beside `done`) stand still for [`HANG`], so a run held up stops the wait and one a loaded machine runs
/// slowly never does. Its output gives way 8 MiB a node's tick (1 s) once it's held up: 60 s for all of it ran out
/// under load, ticks and all far behind.
async fn finishes(done: &std::path::Path) -> Option<String> {
    let moved = moved_beside(done);
    let (mut last, mut still_since) = (String::new(), std::time::Instant::now());
    loop {
        if let Ok(s) = std::fs::read_to_string(done)
            && !s.is_empty()
        {
            return Some(s);
        }
        let now = std::fs::read_to_string(&moved).unwrap_or_default();
        if now != last {
            (last, still_since) = (now, std::time::Instant::now());
        } else if still_since.elapsed() >= HANG {
            return None;
        }
        crate::rt::sleep(Duration::from_millis(100)).await;
    }
}

/// Where [`past_the_buffer`]'s command notes the MiB it wrote: beside `done`.
fn moved_beside(done: &std::path::Path) -> std::path::PathBuf {
    done.with_file_name("moved")
}

/// Until `ok` holds, looking every 50 ms, up to [`HANG`]: a wait on the condition, which a loaded machine takes
/// what time it takes to meet (counted rounds of 20 ms and 50 ms, 2 s and 10 s all told, were a pace).
async fn eventually<F: std::future::Future<Output = anyhow::Result<bool>>>(mut ok: impl FnMut() -> F) -> anyhow::Result<bool> {
    let until = std::time::Instant::now() + HANG;
    loop {
        if ok().await? {
            return Ok(true);
        }
        if std::time::Instant::now() >= until {
            return Ok(false);
        }
        crate::rt::sleep(Duration::from_millis(50)).await;
    }
}

/// A count a runner's node gives of `peer` now (`runs`, `detached`), as its status says.
async fn count_of(h: &Handle, peer: &str, key: &str) -> anyhow::Result<u64> {
    let s = h.status().await?;
    let peers = s.get("peers").and_then(serde_json::Value::as_array).cloned().unwrap_or_default();
    Ok(peers.iter().find(|p| p.get("runner").and_then(serde_json::Value::as_str) == Some(peer)).and_then(|p| p.get(key)).and_then(serde_json::Value::as_u64).unwrap_or(0))
}

/// The runs a runner's node does for others now, as its status says.
async fn serving(h: &Handle) -> anyhow::Result<u64> {
    let s = h.status().await?;
    Ok(s.get("serving").and_then(serde_json::Value::as_array).map(|a| a.iter().filter_map(|x| x.get("runs").and_then(serde_json::Value::as_u64)).sum()).unwrap_or(0))
}

/// Shell text that waits for `go`, fills the node and pipe-reader queues a MiB at a time (noting each MiB written
/// beside `done`, [`moved_beside`]), then writes `done`. W24 M0 step 4's gate: 20 MB fits in the node's 16 MiB plus
/// the pipe reader's 256 x 64 KiB queue, so the command could finish before any unread output had to give way.
fn past_the_buffer(go: &std::path::Path, done: &std::path::Path) -> String {
    let mib = node::BUFFER.saturating_mul(4) >> 20;
    let moved = moved_beside(done);
    format!(
        "until [ -e '{}' ]; do sleep 0.1; done; i=0; while [ $i -lt {mib} ]; do head -c 1048576 /dev/zero; i=$((i+1)); echo $i > '{}'; done; echo done > '{}'",
        go.display(),
        moved.display(),
        done.display()
    )
}

/// `kiln run --detach` (docs/reach.md §4.2): once its client went, the run is followed on alone by
/// its asking runner, which takes its output and lets it go, and links again after a drop: the run
/// goes on past 16 MiB of output nobody reads and past a lost connection. Once it ends both runners
/// forget it, and it never was a run the asking runner's upgrade would wait for.
#[cfg(unix)]
#[test]
fn a_detached_run_goes_on_after_its_client_followed_on_alone() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab().await?;
        let dir = tempfile::tempdir()?;
        let (go, done) = (dir.path().join("go"), dir.path().join("done"));
        start_then_leave(&lab, "mac", RunReq { target: "linux".into(), command: past_the_buffer(&go, &done), detach: true, ..RunReq::default() }).await?;
        let l = &lab;
        let alone = eventually(|| async move { anyhow::Ok(count_of(&l.mac, "linux", "detached").await? == 1) }).await?;
        anyhow::ensure!(alone, "mac stopped following the run when its client went");
        anyhow::ensure!(count_of(&lab.mac, "linux", "runs").await? == 1);
        anyhow::ensure!(lab.mac.busy() == 0, "a detached run would hold mac's upgrade");
        // mac's connection drops and comes back while it waits: mac links to linux again for it.
        let before = link_to(&lab.mac, "linux").await?;
        anyhow::ensure!(before.is_some(), "no link for the run");
        lab.ctl.send(Ctl::Drop("mac")).await?;
        crate::rt::sleep(Duration::from_millis(500)).await;
        lab.ctl.send(Ctl::Back("mac")).await?;
        let was = &before;
        let again = eventually(|| async move {
            let now = link_to(&l.mac, "linux").await?;
            anyhow::Ok(now.is_some() && now != *was)
        })
        .await?;
        anyhow::ensure!(again, "mac didn't link to linux again for the detached run");
        std::fs::write(&go, b"")?;
        anyhow::ensure!(finishes(&done).await.as_deref() == Some("done\n"), "the detached run never got past its output");
        let forgotten = eventually(|| async move { anyhow::Ok(count_of(&l.mac, "linux", "runs").await? == 0 && serving(&l.linux).await? == 0) }).await?;
        anyhow::ensure!(forgotten, "the ended run was kept on one side");
        Ok(())
    })
}

/// A detached run whose asking runner never comes back (restarted, its connection gone for good)
/// waits for no one (docs/reach.md §4.1): it isn't stopped for want of its asker, and its output,
/// held unread, gives way oldest first instead of making it wait, so it goes on to its end.
#[cfg(unix)]
#[test]
fn a_detached_run_whose_asker_never_comes_back_goes_on_its_unread_output_giving_way() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab().await?;
        let dir = tempfile::tempdir()?;
        let (go, done) = (dir.path().join("go"), dir.path().join("done"));
        start_then_leave(&lab, "mac", RunReq { target: "linux".into(), command: past_the_buffer(&go, &done), detach: true, ..RunReq::default() }).await?;
        lab.ctl.send(Ctl::Drop("mac")).await?;
        std::fs::write(&go, b"")?;
        anyhow::ensure!(finishes(&done).await.as_deref() == Some("done\n"), "the run waited on output nobody will read");
        Ok(())
    })
}

/// A detached run whose asker keeps the link up but takes none of its output (a runner from before
/// detached runs were followed on alone: it stopped following when the client went) gives way
/// too, once none was taken for 10 s.
#[cfg(unix)]
#[test]
fn a_detached_run_whose_asker_takes_none_of_its_output_gives_way_after_a_while() -> anyhow::Result<()> {
    use super::wire::{self, RunSpec};
    crate::rt::block_on(async move {
        // mac speaks reach 1, as a runner from before does: a link at 2 (W27-R08) refuses a run
        // that comes without its asker's hand-on, which this one, asked straight on the handle, lacks.
        let lab = lab_at([None, None], [1, super::wire::VERSION]).await?;
        let dir = tempfile::tempdir()?;
        let (go, done) = (dir.path().join("go"), dir.path().join("done"));
        lab.mac.connect("linux").await?;
        // W24 M0 step 4's loaded gate: the keeper lasts for this fixture, including a slow
        // handshake. A fixed sleep can end before this test starts measuring the unread output.
        let (keep, ready) = (dir.path().join("keep"), dir.path().join("keeper-started"));
        std::fs::write(&keep, b"")?;
        let (kept, kept_said) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
        lab.mac.run("linux", RunSpec { command: format!("printf 'ready\\n' > '{}'; while [ -e '{}' ]; do sleep 0.1; done", ready.display(), keep.display()), ..RunSpec::default() }, kept).await?;
        // W05-R03: Started is a live notification, not replayed when a link resumes. Observe the
        // keeper's own readiness instead; a refused or failed start still fails with its terminal word.
        let started = async {
            loop {
                loop {
                    match kept_said.try_recv() {
                        Ok(ended @ wire::Say::Exit { .. }) => anyhow::bail!("the run keeping the link up ended before it was ready: {ended:?}"),
                        Ok(_) => {}
                        Err(async_channel::TryRecvError::Empty) => break,
                        Err(async_channel::TryRecvError::Closed) => anyhow::bail!("the run keeping the link up stopped reporting before it was ready"),
                    }
                }
                if std::fs::read(&ready).is_ok_and(|text| text == b"ready\n") {
                    return anyhow::Ok(());
                }
                crate::rt::sleep(Duration::from_millis(50)).await;
            }
        };
        crate::rt::timeout(HANG, started).await.map_err(|_| anyhow::anyhow!("the run keeping the link up did not become ready"))??;
        anyhow::ensure!(count_of(&lab.mac, "linux", "runs").await? == 1 && serving(&lab.linux).await? == 1, "the ready keeper is still followed and served");
        // Asked as a runner from before asks a detached run, then let go of.
        let (says, _said) = crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS);
        let id = lab.mac.run("linux", RunSpec { command: past_the_buffer(&go, &done), linger_ms: Some(wire::DETACHED), ..RunSpec::default() }, says).await?;
        lab.mac.leave("linux", Some(id), None).await;
        // leave enqueues the request. This status request follows it on the same queue: no
        // output is released until the asker has stopped following, so none can be acknowledged.
        anyhow::ensure!(count_of(&lab.mac, "linux", "runs").await? == 1, "the asker still followed the detached run");
        let t = std::time::Instant::now();
        std::fs::write(&go, b"")?;
        anyhow::ensure!(finishes(&done).await.as_deref() == Some("done\n"), "the run waited on output its asker never takes");
        anyhow::ensure!(t.elapsed() >= Duration::from_secs(8), "it gave way with a link up before 10 s: {:?}", t.elapsed());
        let ended = lab.ended.lock().clone();
        anyhow::ensure!(ended.is_empty(), "the link did not stay up: {ended:?}");
        std::fs::remove_file(&keep)?;
        Ok(())
    })
}

/// `kiln run --detach` on its own runner's machine goes on after its client too, its output read
/// to its end so that it never waits on it (docs/reach.md §4.2).
#[cfg(unix)]
#[test]
fn a_detached_run_on_its_own_machine_goes_on_after_its_client() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab().await?;
        let dir = tempfile::tempdir()?;
        let (go, done) = (dir.path().join("go"), dir.path().join("done"));
        start_then_leave(&lab, "mac", RunReq { target: "mac".into(), command: past_the_buffer(&go, &done), detach: true, ..RunReq::default() }).await?;
        std::fs::write(&go, b"")?;
        anyhow::ensure!(finishes(&done).await.as_deref() == Some("done\n"), "the detached run stopped with its client");
        Ok(())
    })
}

/// Goal 9, plan 01a1a87b2cec step 3: a runner's ReachHello, as the hub's loop takes it, is an announcement
/// and a line, and nothing in it waits: its announcement goes in the directory's copy at once (a copy of the
/// runners' announcements swapped in whole) and in the relay's in its owner's turn, handed over without
/// waiting, before any frame sent after it; its line goes to the log's writer (`crate::log_lines`), where it
/// was a write to stdout under its lock. A link opened to it right after is one the relay knows it takes.
#[test]
fn g9_s3_a_reach_hello_is_announced_at_once_and_to_the_relay_in_its_turn() -> anyhow::Result<()> {
    use crate::hub::tests::{hear, join_runner, scratch_hub};
    use crate::proto::{CAP_WIRE, LANE_CONTROL, LANE_REACH};
    crate::rt::block_on(async {
        let hub = scratch_hub().await?;
        join_runner(&hub, 1, "mac", CAP_WIRE);
        join_runner(&hub, 2, "linux", CAP_WIRE);
        let said = Announce { fingerprint: "f".into(), accept: true, version: 1, features: Vec::new() };
        hear(&hub, 2, LANE_CONTROL, &AgentMsg::ReachHello { json: serde_json::to_string(&said)? }).await?;
        anyhow::ensure!(hub.reach.announced().get("linux") == Some(&said), "the directory has it as soon as the hub's loop took it");
        hear(&hub, 1, LANE_REACH, &AgentMsg::Reach { to: "linux".into(), link: "L1".into(), open: true, data: b"x".to_vec(), close: None }).await?;
        let relayed = hub.relay().ask(|r| r.announced("linux").cloned()).await.flatten();
        anyhow::ensure!(relayed.as_ref() == Some(&said), "the relay has it: {relayed:?}");
        anyhow::ensure!(hub.relay().ask(|r| r.ends("L1")).await.flatten() == Some(("mac".into(), "linux".into())), "the link sent after it opened to it");
        anyhow::ensure!(hub.sent.lock().iter().any(|(p, m)| *p == 2 && matches!(m, ServerMsg::Reach { from, open: true, .. } if from == "mac")), "and went on to it");
        Ok(())
    })
}


// ------------------------------------------------------------------ placement (docs/needs.md E5)

/// A machine of 8 CPUs and 16 GiB as its sampler reads it, `busy` percent of its CPUs in use.
fn reading(busy: f32) -> crate::metrics::HostSample {
    crate::metrics::HostSample { cpus: 8, cpu_pct: busy, mem_total: 16 << 30, mem_used: 2 << 30, disk_total: 100 << 30, disk_used: 10 << 30, ..crate::metrics::HostSample::default() }
}

/// What placement told the asker, in order, in words.
fn answers(evs: &[Ev]) -> Vec<String> {
    evs.iter().filter_map(|e| if let Ev::Placed { json } = e { serde_json::from_str::<kiln_ci::place::Answer>(json).ok().map(|a| a.words(0)) } else { None }).collect()
}

/// docs/needs.md E5, M3 of the balancer's core: a command asked of mac while both runners are full
/// is answered queued at once, with its place and reasons; when linux's next reading shows room,
/// linux's own admission offers it (a measurement, not a timer or a poll), mac's owner takes it,
/// and it starts there, once. The asker hears each change pushed over its socket.
#[cfg(unix)]
#[test]
fn e5_w05_r39_a_queued_command_starts_once_when_room_frees_without_polling() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab_placing().await?;
        // Both full: 100 % of their CPUs busy with work kiln didn't place.
        lab.mac.feed(In::Sampled(reading(100.0))).await;
        lab.linux.feed(In::Sampled(reading(100.0))).await;
        // The directory, so each knows the other places.
        lab.mac.directory().await?;
        lab.linux.directory().await?;
        let req = RunReq { target: "cpus=2".into(), command: "echo placed on $(hostname -s 2>/dev/null || echo it); echo $KILN_REACH_FROM".into(), ..RunReq::default() };
        let linux = lab.linux.clone();
        let once = std::sync::atomic::AtomicBool::new(false);
        let evs = run_from_mac(&lab, req, move |e| {
            // Once mac's asker hears it's queued, linux measures room: nothing else is told it.
            if let Ev::Placed { json } = e
                && json.contains("\"queued\"")
                && !once.swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                let l = linux.clone();
                crate::rt::spawn(async move {
                    l.feed(In::Sampled(reading(0.0))).await;
                });
            }
            None
        })
        .await?;
        let said = answers(&evs);
        assert!(said.first().is_some_and(|a| a.starts_with("queued")), "first answered queued: {said:?} {evs:?}");
        assert!(said.iter().any(|a| a.contains("linux")), "it started on linux, the runner with room: {said:?}");
        let started: Vec<&Ev> = evs.iter().filter(|e| matches!(e, Ev::Started { .. })).collect();
        assert_eq!(started.len(), 1, "started once: {evs:?}");
        assert_eq!(exit(&evs).map(|e| e.0), Some(Some(0)), "{evs:?}");
        assert!(output(&evs, false).contains("mac"), "it ran on linux for mac: {}", output(&evs, false));
        // Its room is free again on linux, and nothing waits there.
        let v = lab.linux.place_view().await?;
        assert_eq!(v.get("held").cloned(), Some(serde_json::json!([0, 0, 0, 0])), "{v}");
        anyhow::Ok(())
    })
}

/// docs/needs.md E5, M3: a run queued on a full runner waits through its owner's connection
/// dropping and coming back, and starts when room frees: the disconnect loses nothing.
#[cfg(unix)]
#[test]
fn e5_a_queued_command_waits_through_a_disconnect_and_starts_after() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab_placing().await?;
        lab.mac.feed(In::Sampled(reading(100.0))).await;
        lab.linux.feed(In::Sampled(reading(100.0))).await;
        lab.mac.directory().await?;
        lab.linux.directory().await?;
        let req = RunReq { target: "cpus=2".into(), command: "echo after".into(), ..RunReq::default() };
        let (linux, ctl) = (lab.linux.clone(), lab.ctl.clone());
        // Once: the drop and the return make answers of their own.
        let once = std::sync::atomic::AtomicBool::new(false);
        let evs = run_from_mac(&lab, req, move |e| {
            if let Ev::Placed { json } = e
                && json.contains("\"queued\"")
                && !once.swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                let (l, c) = (linux.clone(), ctl.clone());
                crate::rt::spawn(async move {
                    // mac's connection drops and comes back, then linux has room.
                    let _ = c.send(Ctl::Drop("mac")).await;
                    crate::rt::sleep(Duration::from_millis(300)).await;
                    let _ = c.send(Ctl::Back("mac")).await;
                    crate::rt::sleep(Duration::from_millis(300)).await;
                    l.feed(In::Sampled(reading(0.0))).await;
                });
            }
            None
        })
        .await?;
        assert_eq!(evs.iter().filter(|e| matches!(e, Ev::Started { .. })).count(), 1, "started once: {evs:?}");
        assert_eq!(exit(&evs).map(|e| e.0), Some(Some(0)), "{evs:?}");
        assert!(output(&evs, false).contains("after"), "{evs:?}");
        anyhow::Ok(())
    })
}

/// The asking agent hears placement's answers for a run it isn't waiting on (AUDIT O5, docs/needs.md
/// E5), as `req` says it was asked of mac while both runners are full: queued (its place), then
/// started on linux by linux's word, once linux measures room. Every word pushed (the asker's
/// socket, the told outbox), none polled; what the agent's news has of it, in order.
#[cfg(unix)]
async fn o5_told_of(req: RunReq, session: &str) -> anyhow::Result<(Vec<Ev>, Vec<super::told::RunNews>)> {
    let lab = lab_placing().await?;
    lab.mac.feed(In::Sampled(reading(100.0))).await;
    lab.linux.feed(In::Sampled(reading(100.0))).await;
    lab.mac.directory().await?;
    lab.linux.directory().await?;
    let linux = lab.linux.clone();
    let once = std::sync::atomic::AtomicBool::new(false);
    let evs = run_from_mac(&lab, req, move |e| {
        if let Ev::Placed { json } = e
            && json.contains("\"queued\"")
            && !once.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            let l = linux.clone();
            crate::rt::spawn(async move { l.feed(In::Sampled(reading(0.0))).await });
        }
        None
    })
    .await?;
    Ok((evs, super::told::heard_of(session)))
}

/// AUDIT O5: an MCP `run` its agent left queued is told of each change of placement's answer under
/// the line its MCP server follows it by (the name it gave its runner, which its command never
/// sees): queued with its place, then started on linux, by linux's word. Nothing says to ask
/// again; the run starts once, with no poll.
#[cfg(unix)]
#[test]
fn o5_an_mcp_run_left_queued_hears_each_answer_and_starts_without_polling() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let session = format!("o5-mcp-{}", crate::util::new_id());
        let env = vec![(super::told::KEY_ENV.to_owned(), "mcp-r1".to_owned()), (super::told::ID_ENV.to_owned(), "r1".to_owned())];
        let req = RunReq { target: "cpus=2".into(), command: "echo key=${KILN_TOLD_KEY:-none}".into(), env, asker: Some(session.clone()), ..RunReq::default() };
        let (evs, news) = o5_told_of(req, &session).await?;
        assert_eq!(evs.iter().filter(|e| matches!(e, Ev::Started { .. })).count(), 1, "started once: {evs:?}");
        assert!(output(&evs, false).contains("key=none"), "the told names are its runner's alone: {}", output(&evs, false));
        assert!(news.iter().all(|n| n.key == "mcp-r1" && n.id == "r1"), "one line, its MCP server's: {news:?}");
        let queued = news.iter().find(|n| n.event == super::told::RunEvent::Queued);
        assert!(queued.is_some_and(|n| n.place.is_some()), "told it waits, and where: {news:?}");
        assert!(answers(&evs).iter().all(|a| !a.contains("ask again")), "{evs:?}");
        if answers(&evs).iter().any(|a| a.starts_with("started")) {
            assert!(news.iter().any(|n| n.event == super::told::RunEvent::Started && n.on == "linux"), "told it started on linux: {news:?}");
        }
        let events: Vec<_> = news.iter().map(|n| n.event).collect();
        assert!(events.windows(2).all(|w| w.first() <= w.get(1)), "in order: {events:?}");
        anyhow::Ok(())
    })
}

/// AUDIT O5: a detached `kiln run` an agent asked for while every runner was full: its asker hears
/// each answer on its socket, and its agent through told, under placement's own name for it (the
/// line its follower would use had its client gone), then the line closed as its client saw it end.
#[cfg(unix)]
#[test]
fn o5_a_queued_detached_kiln_run_hears_each_answer_and_its_line_closes() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let session = format!("o5-cli-{}", crate::util::new_id());
        let req = RunReq { target: "cpus=2".into(), command: "echo detached".into(), detach: true, asker: Some(session.clone()), ..RunReq::default() };
        let (evs, news) = o5_told_of(req, &session).await?;
        let said = answers(&evs);
        assert!(said.first().is_some_and(|a| a.starts_with("queued")), "its asker heard it queued first: {said:?}");
        assert_eq!(evs.iter().filter(|e| matches!(e, Ev::Started { .. })).count(), 1, "started once: {evs:?}");
        let key = news.first().map(|n| n.key.clone()).unwrap_or_default();
        assert!(key.starts_with("mac-") && news.iter().all(|n| n.key == key), "one line, placement's run: {news:?}");
        assert_eq!(news.first().map(|n| n.event), Some(super::told::RunEvent::Queued), "{news:?}");
        assert_eq!(news.last().map(|n| n.event), Some(super::told::RunEvent::Seen), "closed as its client saw it end: {news:?}");
        anyhow::Ok(())
    })
}

/// A CI leg of `cpus` CPUs, as the runner's `Assign` admits it (AUDIT M4).
fn leg(id: &str, cpus: u64) -> anyhow::Result<kiln_ci::place::Ask> {
    let needs = kiln_ci::sched::needs::parse(&format!("cpus={cpus}")).map_err(anyhow::Error::msg)?;
    Ok(kiln_ci::place::Ask { run: id.to_owned(), needs, class: kiln_ci::sched::state::Class::Batch, ..kiln_ci::place::Ask::default() })
}

/// What a runner's admission holds and may hold, in its four dimensions (`place_view`).
async fn held_and_cap(h: &Handle) -> anyhow::Result<([u64; 4], [u64; 4])> {
    let v = h.place_view().await?;
    let of = |k: &str| serde_json::from_value::<[u64; 4]>(v.get(k).cloned().unwrap_or_default()).map_err(anyhow::Error::from);
    Ok((of("held")?, of("cap")?))
}

/// AUDIT M4, G1 (docs/needs.md E5): CI legs, commands and the room they hold are one ledger on a
/// runner. A leg admitted holds its room; a command that doesn't fit beside it waits, and starts
/// on that runner once the leg ends (its end frees the room in the same input, nobody polling);
/// a leg that doesn't fit beside what's held is refused, saying why, for the engine to place
/// elsewhere. At every step what's held is within the runner's size in every dimension.
#[cfg(unix)]
#[test]
fn m4_g1_ci_legs_and_commands_never_pass_a_runners_budget_together() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab_placing().await?;
        // mac full of work kiln didn't place; linux idle: 8 CPUs, 16 GiB.
        lab.mac.feed(In::Sampled(reading(100.0))).await;
        lab.linux.feed(In::Sampled(reading(0.0))).await;
        lab.mac.directory().await?;
        lab.linux.directory().await?;
        let within = |(held, cap): ([u64; 4], [u64; 4])| held.iter().zip(&cap).all(|(h, c)| h <= c);
        // A leg of 6 CPUs: admitted, its room held.
        assert_eq!(lab.linux.admit_leg(leg("leg-1", 6)?).await?, Ok(()));
        let hc = held_and_cap(&lab.linux).await?;
        assert!(hc.0.first() == Some(&6_000) && within(hc), "{hc:?}");
        // A leg of 4 beside it doesn't fit: refused, saying why.
        let refused = lab.linux.admit_leg(leg("leg-2", 4)?).await?;
        assert!(refused.as_ref().is_err_and(|why| why.contains("cpu") || why.contains("room")), "{refused:?}");
        // A command of 4 CPUs asked of mac: queued (2 free on linux), then started on linux once
        // leg-1 ends; meanwhile a leg of 3 is refused beside the 6 held.
        let (said_tx, said_rx) = async_channel::unbounded::<(Result<(), String>, ([u64; 4], [u64; 4]))>();
        let linux = lab.linux.clone();
        let once = std::sync::atomic::AtomicBool::new(false);
        let req = RunReq { target: "cpus=4".into(), command: "echo after the leg".into(), ..RunReq::default() };
        let evs = run_from_mac(&lab, req, move |e| {
            if let Ev::Placed { json } = e
                && json.contains("\"queued\"")
                && !once.swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                let (l, tx) = (linux.clone(), said_tx.clone());
                crate::rt::spawn(async move {
                    let third = l.admit_leg(leg("leg-3", 3).unwrap_or_default()).await.unwrap_or(Ok(()));
                    let hc = held_and_cap(&l).await.unwrap_or_default();
                    let _ = tx.send((third, hc)).await;
                    l.leg_ended("leg-1", true).await;
                });
            }
            None
        })
        .await?;
        let (third, hc) = said_rx.recv().await?;
        assert!(third.is_err(), "a leg of 3 beside 6 held of 8 is refused: {third:?}");
        assert!(within(hc), "{hc:?}");
        assert_eq!(evs.iter().filter(|e| matches!(e, Ev::Started { .. })).count(), 1, "started once: {evs:?}");
        assert!(answers(&evs).iter().any(|a| a.contains("linux")), "on linux, once the leg ended: {:?}", answers(&evs));
        assert_eq!(exit(&evs).map(|e| e.0), Some(Some(0)), "{evs:?}");
        // All gone: nothing held.
        let hc = held_and_cap(&lab.linux).await?;
        assert_eq!(hc.0, [0, 0, 0, 0], "{hc:?}");
        anyhow::Ok(())
    })
}

/// AUDIT O1 (core's half), O2: a runner says its admission's hint when it changes something
/// decided by, never per sample: its first; nothing for a reading that moves the room but no
/// answer; again when a dimension's room goes to none (its CPUs, here), and when it's strained.
#[test]
fn o1_a_runners_hint_goes_only_on_a_change_decided_by() -> anyhow::Result<()> {
    use super::place::{Placer, sample_of};
    let mut p = Placer::new("r", 1, kiln_ci::facts::RunnerFacts::default(), Vec::new(), sample_of(&reading(10.0)), kiln_ci::place::runner::Settings::default(), 0);
    let first = p.hint_due(0, None).ok_or_else(|| anyhow::anyhow!("its first hint"))?;
    let h: crate::offers::hint::Hint = serde_json::from_str(&first)?;
    assert!(h.whole && h.seq == 1 && h.room.first().is_some_and(|c| *c > 0), "{first}");
    let _ = p.measured(sample_of(&reading(10.0)), 1);
    assert_eq!(p.hint_due(1, None), None, "a reading that changes nothing is no news");
    // The placement kernel picks by a runner's exact room (docs/lb.md "Adopted"): a reading that
    // moves its word is news to owners' kernels, though it changes no answer of the admission's.
    let _ = p.measured(sample_of(&reading(12.0)), 2);
    let moved: crate::offers::hint::Hint = serde_json::from_str(&p.hint_due(2, None).ok_or_else(|| anyhow::anyhow!("the kernel's word moved: news"))?)?;
    assert!(moved.seq == 2 && moved.fast != h.fast && moved.room.first() < h.room.first(), "its kernel word moved: {moved:?}");
    let _ = p.measured(sample_of(&reading(100.0)), 3);
    let full: crate::offers::hint::Hint = serde_json::from_str(&p.hint_due(3, None).ok_or_else(|| anyhow::anyhow!("no room is news"))?)?;
    assert!(full.seq == 3 && full.room.first() == Some(&0) && !full.whole, "its CPUs went to none: {full:?}");
    let strained = p.hint_due(4, Some("stalled".into())).ok_or_else(|| anyhow::anyhow!("strain is news"))?;
    assert!(strained.contains("strained"), "{strained}");
    // Another runner's room goes to its owner; its own is no hint to itself.
    p.hinted(&serde_json::to_string(&crate::offers::hint::Room { runner: "other".into(), epoch: 1, seq: 1, room: [4000, 8 << 30, 0, 0], fast: None })?);
    p.hinted(&serde_json::to_string(&crate::offers::hint::Room { runner: "r".into(), epoch: 1, seq: 1, room: [0; 4], fast: None })?);
    Ok(())
}

/// AUDIT M4, G1 (docs/needs.md E5): an agent session placed by a runner's owner holds its room on
/// the runner it was placed on from its start to its end, in the one ledger with CI legs and
/// commands. kiln's server asks mac to own a session's start among the machines that can run its
/// harness (linux), asking 4 CPUs: mac's owner probes linux, linux admits it, and mac says go with
/// its claim. Started there, it holds 4 of linux's 8 CPUs: a leg of 6 beside it is refused, one of
/// 4 admitted; a command of 2 then waits, and starts on linux once the session ends, nobody
/// polling. What's held never passes linux's size.
#[cfg(unix)]
#[test]
fn m4_g1_sessions_legs_and_commands_never_pass_a_runners_budget_together() -> anyhow::Result<()> {
    use super::place::{SessionAsk, SessionWord};
    crate::rt::block_on(async move {
        let lab = lab_placing().await?;
        lab.mac.feed(In::Sampled(reading(100.0))).await;
        lab.linux.feed(In::Sampled(reading(0.0))).await;
        lab.mac.directory().await?;
        lab.linux.directory().await?;
        let within = |(held, cap): ([u64; 4], [u64; 4])| held.iter().zip(&cap).all(|(h, c)| h <= c);
        let needs = kiln_ci::sched::needs::parse("cpus=4").map_err(anyhow::Error::msg)?;
        let ask = kiln_ci::place::Ask { run: "session-1".into(), needs, class: kiln_ci::sched::state::Class::Interactive, among: vec!["linux".into()], key: Some("session:claude".into()), ..kiln_ci::place::Ask::default() };
        lab.mac.feed(In::SessionPlace { req: 7, json: serde_json::to_string(&SessionAsk::Ask { ask: Box::new(ask) })? }).await;
        let claim = loop {
            let (from, m) = crate::rt::timeout(Duration::from_secs(60), lab.said.recv()).await??;
            if let (true, AgentMsg::Placed { req: 7, json }) = (from == "mac", m)
                && let SessionWord::Go { runner, claim } = serde_json::from_str::<SessionWord>(&json)?
            {
                anyhow::ensure!(runner == "linux", "placed on {runner}");
                break claim;
            }
        };
        anyhow::ensure!(lab.linux.session_started(claim.clone(), None).await?, "its claim was live on linux");
        let hc = held_and_cap(&lab.linux).await?;
        anyhow::ensure!(hc.0.first() == Some(&4_000) && within(hc), "the session holds its 4 CPUs: {hc:?}");
        let six = lab.linux.admit_leg(leg("leg-6", 6)?).await?;
        anyhow::ensure!(six.is_err(), "a leg of 6 beside the session's 4 of 8: {six:?}");
        anyhow::ensure!(lab.linux.admit_leg(leg("leg-4", 4)?).await? == Ok(()), "a leg of 4 fits beside it");
        let hc = held_and_cap(&lab.linux).await?;
        anyhow::ensure!(hc.0.first() == Some(&8_000) && within(hc), "{hc:?}");
        let (seen_tx, seen_rx) = async_channel::unbounded::<([u64; 4], [u64; 4])>();
        let (linux, once) = (lab.linux.clone(), std::sync::atomic::AtomicBool::new(false));
        let session = claim.clone();
        let req = RunReq { target: "cpus=2".into(), command: "echo after the session".into(), ..RunReq::default() };
        let evs = run_from_mac(&lab, req, move |e| {
            if let Ev::Placed { json } = e
                && json.contains("\"queued\"")
                && !once.swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                let (l, tx, c) = (linux.clone(), seen_tx.clone(), session.clone());
                crate::rt::spawn(async move {
                    let _ = tx.send(held_and_cap(&l).await.unwrap_or_default()).await;
                    l.session_ended(c).await;
                });
            }
            None
        })
        .await?;
        let hc = seen_rx.recv().await?;
        anyhow::ensure!(within(hc), "while the command waited: {hc:?}");
        anyhow::ensure!(evs.iter().filter(|e| matches!(e, Ev::Started { .. })).count() == 1, "started once: {evs:?}");
        anyhow::ensure!(answers(&evs).iter().any(|a| a.contains("linux")), "on linux, once the session ended: {:?}", answers(&evs));
        lab.linux.leg_ended("leg-4", true).await;
        let hc = held_and_cap(&lab.linux).await?;
        anyhow::ensure!(hc.0 == [0, 0, 0, 0], "all of it free again: {hc:?}");
        anyhow::Ok(())
    })
}

/// docs/needs.md E5, AUDIT O1: a runner's hint quotes its admission's answer for each size waiting
/// or held there (here: a run of 4 CPUs holding room, so 4 CPUs would wait for it, or start where
/// room is), and says when it's draining (an upgrade pending): a change decided by, so it's said.
#[test]
fn o1_a_runners_hint_quotes_its_sizes_and_says_when_it_drains() -> anyhow::Result<()> {
    use super::place::{Placer, sample_of};
    use crate::offers::hint::{Hint, Taking};
    let mut p = Placer::new("r", 1, kiln_ci::facts::RunnerFacts::default(), Vec::new(), sample_of(&reading(0.0)), kiln_ci::place::runner::Settings::default(), 0);
    let needs = kiln_ci::sched::needs::parse("cpus=4").map_err(anyhow::Error::msg)?;
    let _ = p.directory(&[], 0);
    let _ = p.ask(kiln_ci::place::Ask { run: "r1".into(), needs, ..kiln_ci::place::Ask::default() }, 0);
    let h: Hint = serde_json::from_str(&p.hint_due(1, None).ok_or_else(|| anyhow::anyhow!("its first hint"))?)?;
    anyhow::ensure!(h.quotes.iter().any(|q| q.ask.first() == Some(&4_000)), "a quote for the size held here: {h:?}");
    let _ = p.drain(true, 2);
    let h: Hint = serde_json::from_str(&p.hint_due(2, None).ok_or_else(|| anyhow::anyhow!("draining is news"))?)?;
    anyhow::ensure!(h.state == Taking::Draining, "{h:?}");
    anyhow::ensure!(h.quotes.iter().all(|q| !matches!(q.answer, crate::offers::hint::Quoted::Now)), "nothing starts now while it drains: {h:?}");
    Ok(())
}
