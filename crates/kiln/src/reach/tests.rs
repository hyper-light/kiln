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
    };
    let (mac, mac_out) = node::start(cfg("mac", dirs.first().ok_or_else(|| anyhow::anyhow!("dir"))?, None))?;
    let (linux, linux_out) = node::start(cfg("linux", dirs.get(1).ok_or_else(|| anyhow::anyhow!("dir"))?, Some(adopt_tx)))?;
    let directory = Arc::new(Mutex::new(vec![info("mac", mac.fingerprint()), info("linux", linux.fingerprint())]));
    // Each runner's current connection, as the server would number them.
    let peers: Arc<Mutex<HashMap<&'static str, u64>>> = Arc::new(Mutex::new(HashMap::from([("mac", 10), ("linux", 20)])));
    let (ctl_tx, ctl_rx) = async_channel::unbounded::<Ctl>();
    let (m, l) = (mac.clone(), linux.clone());
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
                    let p = Arc::clone(&peers);
                    let out = relay.forward(from, from_peer, Frame { to, link, open, data, close }, move |n| p.lock().get(n).copied());
                    for (to, sm) in out {
                        deliver(&peers, &handle_of, to, sm).await;
                    }
                }
                AgentMsg::ReachAsk { req, json: asked } => {
                    // An agent's question for kiln's operations comes back as what it asked.
                    let ask: serde_json::Value = serde_json::from_str(&asked).unwrap_or_default();
                    let json = match ask.get("op").and_then(serde_json::Value::as_str) {
                        Some("call") => serde_json::json!({"ok": {"name": ask.get("name"), "input": ask.get("input"), "session": ask.get("session")}}).to_string(),
                        _ => serde_json::json!({ "runners": *directory.lock() }).to_string(),
                    };
                    handle_of(from).feed(In::Answer { req, json }).await;
                }
                _ => {}
            }
        }
    });
    mac.feed(In::Server(true)).await;
    linux.feed(In::Server(true)).await;
    Ok(Lab { mac, linux, adoptions, ctl: ctl_tx, _dirs: dirs, mirrors, using })
}

async fn deliver(peers: &Mutex<HashMap<&'static str, u64>>, handle_of: &impl Fn(&str) -> Handle, to: u64, sm: ServerMsg) {
    let name = peers.lock().iter().find(|(_, p)| **p == to).map(|(n, _)| *n);
    let (Some(name), ServerMsg::Reach { from, link, open, data, close }) = (name, sm) else { return };
    handle_of(name).feed(In::Frame { from, link, open, data, close }).await;
}

/// Run `req` from mac through the local driver; every event until the end.
async fn run_from_mac(lab: &Lab, req: RunReq, mid: impl Fn(&Ev) -> Option<Ctl>) -> anyhow::Result<Vec<Ev>> {
    let (ev_tx, ev_rx) = async_channel::unbounded();
    let (_ctl_tx, ctl_rx) = async_channel::unbounded();
    let mac = lab.mac.clone();
    crate::rt::spawn(async move { local::run(&mac, "mac", req, ev_tx, ctl_rx).await });
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
