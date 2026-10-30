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

/// What the test does to the "server".
enum Ctl {
    /// Drop a runner's connection (its links close; it hears the server went).
    Drop(&'static str),
    /// It reconnects (a new connection).
    Back(&'static str),
    /// Change what the directory says a runner's key is.
    Key(&'static str, String),
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
    adoptions: async_channel::Receiver<node::Adoption>,
    ctl: async_channel::Sender<Ctl>,
    _dirs: Vec<tempfile::TempDir>,
    mirrors: std::path::PathBuf,
    /// What linux's node notes of the copies it holds.
    using: Arc<super::keeper::Using>,
}

fn info(name: &str, fp: &str) -> RunnerInfo {
    RunnerInfo {
        name: name.into(),
        hostname: format!("{name}.lan"),
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
        ..RunnerInfo::default()
    }
}

async fn lab() -> anyhow::Result<Lab> {
    lab_with([None, None]).await
}

/// [`lab`], mac and linux serving their peers from these caches (docs/ci.md W24-R27).
async fn lab_with(objects: [Option<Arc<crate::runner::work::store::Store>>; 2]) -> anyhow::Result<Lab> {
    let [mac_objects, linux_objects] = objects;
    let dirs = vec![tempfile::tempdir()?, tempfile::tempdir()?, tempfile::tempdir()?];
    let mirrors = dirs.get(2).map(|d| d.path().to_path_buf()).unwrap_or_default();
    let (adopt_tx, adoptions) = async_channel::unbounded();
    let using = Arc::new(super::keeper::Using::default());
    let cfg = |name: &str, d: &tempfile::TempDir, adopt: Option<async_channel::Sender<node::Adoption>>| Config {
        name: name.into(),
        dir: d.path().join("reach"),
        accept: true,
        mirrors: mirrors.clone(),
        using: if name == "linux" { Arc::clone(&using) } else { Arc::default() },
        adopt,
        objects: None,
    };
    let (mac, mac_out) = node::start(Config { objects: mac_objects, ..cfg("mac", dirs.first().ok_or_else(|| anyhow::anyhow!("dir"))?, None) })?;
    let (linux, linux_out) = node::start(Config { objects: linux_objects, ..cfg("linux", dirs.get(1).ok_or_else(|| anyhow::anyhow!("dir"))?, Some(adopt_tx)) })?;
    let directory = Arc::new(Mutex::new(vec![info("mac", mac.fingerprint()), info("linux", linux.fingerprint())]));
    // Each runner's current connection, as the server would number them.
    let peers: Arc<Mutex<HashMap<&'static str, u64>>> = Arc::new(Mutex::new(HashMap::from([("mac", 10), ("linux", 20)])));
    let (ctl_tx, ctl_rx) = async_channel::unbounded::<Ctl>();
    let (said_tx, said) = async_channel::unbounded();
    let ended: Arc<Mutex<Vec<String>>> = Arc::default();
    let (m, l, e) = (mac.clone(), linux.clone(), Arc::clone(&ended));
    crate::rt::spawn(async move {
        let mut relay = Relay::default();
        relay.announce("mac", Announce { fingerprint: m.fingerprint().into(), accept: true, version: 1 });
        relay.announce("linux", Announce { fingerprint: l.fingerprint().into(), accept: true, version: 1 });
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
                    let out = relay.forward(from, from_peer, Frame { to, link, open, data, close }, move |n| p.lock().get(n).copied()).sends(from_peer, false);
                    for (to, sm) in out {
                        deliver(&peers, &handle_of, to, sm).await;
                    }
                }
                AgentMsg::ReachAsk { req, json: asked } => {
                    // An agent's question for kiln's operations comes back as what it asked.
                    let ask: serde_json::Value = serde_json::from_str(&asked).unwrap_or_default();
                    let answer = match ask.get("op").and_then(serde_json::Value::as_str) {
                        Some("call") => serde_json::json!({"ok": {"name": ask.get("name"), "input": ask.get("input"), "session": ask.get("session")}}),
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
    Ok(Lab { mac, linux, said, ended, adoptions, ctl: ctl_tx, _dirs: dirs, mirrors, using })
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
    let (ev_tx, ev_rx) = async_channel::unbounded();
    let (_ctl_tx, ctl_rx) = async_channel::unbounded();
    let handle = if me == "mac" { lab.mac.clone() } else { lab.linux.clone() };
    crate::rt::spawn(async move { local::run(&handle, me, req, ev_tx, ctl_rx).await });
    let mut evs = Vec::new();
    loop {
        match crate::rt::timeout(Duration::from_secs(60), ev_rx.recv()).await {
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
        // Away from the server: refused at once, not left to wait.
        lab.ctl.send(Ctl::Drop("mac")).await?;
        let mut refused = None;
        for _ in 0..50 {
            match lab.mac.query(ask.to_string()).await {
                Err(e) => {
                    refused = Some(format!("{e:#}"));
                    break;
                }
                Ok(_) => crate::rt::sleep(Duration::from_millis(20)).await,
            }
        }
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
    let (ev_tx, ev_rx) = async_channel::unbounded();
    let (_ctl_tx, ctl_rx) = async_channel::unbounded();
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
            let (from, m) = crate::rt::timeout(Duration::from_secs(20), lab.said.recv()).await??;
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
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
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
        hear(&hub, 2, LANE_CONTROL, &AgentMsg::ReachHello { json: serde_json::to_string(&Announce { fingerprint: "f".into(), accept: true, version: 1 })? }).await?;
        let link = garbled.clone().unwrap_or_default();
        hear(&hub, 1, LANE_REACH, &AgentMsg::Reach { to: "linux".into(), link: link.clone(), open: true, data: Vec::new(), close: None }).await?;
        hear(&hub, 2, LANE_REACH, &said).await?;
        let rows = refused_rows(&hub, "runner:mac").await?;
        anyhow::ensure!(rows.iter().any(|(t, r, d)| t == "reach_link" && r == "unknown" && d.starts_with("variant 99") && d.contains(&format!("as linux read it, on link {link}"))), "{rows:?}");
        for name in ["mac", "linux"] {
            let shown = hub.refusals.lock().kept(&Page::Runner(name.into())).is_some_and(|k| k.counts.iter().any(|c| c.wire == Wire::ReachLink && c.code == Code::Unknown));
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
        let hello = AgentMsg::ReachHello { json: serde_json::to_string(&Announce { fingerprint: "f".into(), accept: true, version: 1 })? };
        hear(&hub, 1, LANE_CONTROL, &hello).await?;
        hear(&hub, 2, LANE_CONTROL, &hello).await?;
        let frame = |to: &str, open: bool| AgentMsg::Reach { to: to.into(), link: "L1".into(), open, data: b"x".to_vec(), close: None };
        hear(&hub, 1, LANE_REACH, &frame("linux", true)).await?;
        anyhow::ensure!(hub.reach.lock().ends("L1") == Some(("mac".into(), "linux".into())), "the link is open");
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
        let t = told(&hub, 3);
        anyhow::ensure!(t.as_ref().is_some_and(|(w, c, why)| *w == Wire::Relay && *c == Code::Malformed && why.contains("link L1 is mac's and linux's")), "{t:?}");
        anyhow::ensure!(hub.reach.lock().links() == 1, "the link goes on");
        let rows = refused_rows(&hub, "runner:intruder").await?;
        anyhow::ensure!(rows.iter().any(|(t, r, d)| t == "relay" && r == "malformed" && d.contains("link L1 is mac's and linux's")), "{rows:?}");
        let shown = |name: &str, wire: Wire, code: Code| hub.refusals.lock().kept(&Page::Runner(name.into())).is_some_and(|k| k.counts.iter().any(|c| c.wire == wire && c.code == code));
        anyhow::ensure!(shown("intruder", Wire::Relay, Code::Malformed), "on the intruder's page");
        // The relay's other refusals carry their code to a runner that takes word of them.
        hear(&hub, 1, LANE_REACH, &frame("linux", true)).await?;
        anyhow::ensure!(hub.sent.lock().iter().any(|(p, m)| *p == 1 && matches!(m, ServerMsg::ReachRefused { code: super::relay::Refusal::Taken, .. })), "a link id taken, said so");
        // A runner from before (no CAP_UNREAD) is told in words, as before, and nothing new; its
        // frame on a link not its own still goes nowhere and is noted.
        join_runner(&hub, 4, "old", CAP_WIRE);
        hear(&hub, 4, LANE_REACH, &frame("linux", true)).await?;
        let to_old: Vec<ServerMsg> = hub.sent.lock().iter().filter(|(p, _)| *p == 4).map(|(_, m)| m.clone()).collect();
        anyhow::ensure!(matches!(to_old.as_slice(), [ServerMsg::Reach { open: false, close: Some(why), .. }] if why.contains("taken")), "{to_old:?}");
        clock.advance(Duration::from_millis(100));
        hear(&hub, 4, LANE_REACH, &frame("linux", false)).await?;
        anyhow::ensure!(hub.sent.lock().iter().filter(|(p, _)| *p == 4).count() == 1, "the old runner is told nothing new");
        anyhow::ensure!(shown("old", Wire::Relay, Code::Malformed) && hub.reach.lock().links() == 1, "noted on its page, and the link goes on");
        // A job pod's frame: refused, and noted.
        if let Some(a) = hub.agents.lock().get_mut("intruder") {
            a.ephemeral = true;
        }
        clock.advance(Duration::from_millis(100));
        hear(&hub, 3, LANE_REACH, &frame("mac", true)).await?;
        let t = told(&hub, 3);
        anyhow::ensure!(t.as_ref().is_some_and(|(_, _, why)| why.contains("job pod")), "{t:?}");
        // Word of what a link couldn't read: noted against the runner that sent it, on both
        // runners' pages.
        clock.advance(Duration::from_millis(100));
        hear(&hub, 2, LANE_CONTROL, &AgentMsg::LinkUnread { from: "mac".into(), link: "L1".into(), what: "variant 99".into(), code: Code::Unknown, why: "Ask has 12 variants".into() }).await?;
        let total = |name: &str| hub.refusals.lock().kept(&Page::Runner(name.into())).map(Kept::total);
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
    r.announce("linux", Announce { fingerprint: "f".into(), accept: true, version: 1 });
    let peers = |n: &str| (n == "linux").then_some(20).or((n == "mac").then_some(10));
    r.forward("mac", 10, Frame { to: "linux".into(), link: "L1".into(), open: true, data: Vec::new(), close: None }, peers);
    assert_eq!(r.ends("L1"), Some(("mac".into(), "linux".into())));
    r.forward("mac", 10, Frame { to: "linux".into(), link: "L1".into(), open: false, data: Vec::new(), close: Some("done".into()) }, peers);
    assert_eq!(r.links(), 0);
    assert_eq!(r.ends("L1"), Some(("mac".into(), "linux".into())), "kept a while after it closed");
    assert_eq!(r.ends("L2"), None);
}
