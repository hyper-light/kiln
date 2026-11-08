//! The doors into placement (docs/ci.md W05-R03: a job's needs are "in the one spelling `kiln run
//! --needs` and the MCP tools take too"; docs/needs.md E5), each driven into this lab's runners
//! that place: `kiln run`'s front end (`cli::run`, its arguments as clap reads them, over the local
//! socket a runner serves), the MCP server's `run` tool (its JSON input, as an agent calls it), and
//! kiln's server placing a child agent (`harness::lineage`'s `place_agents`, through a test's hub).
//! Each shows the ask reached the runners' placement as it was asked: queued with placement's
//! reasons or never with its need, held in the admission of the runner it started on, never past
//! that runner's size, and its room released at its end.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kiln_ci::place::Answer;
use serde_json::{Value, json};

use super::{HANG, Lab, eventually, held_and_cap, lab_placing, reading};
use crate::proto::{AgentMsg, ServerMsg};
use crate::reach::node::{Handle, In};

/// How many asks of class `class` wait in runner `h`'s admission now (`place_view`'s `queued`).
async fn waiting(h: &Handle, class: &str) -> anyhow::Result<u64> {
    let v = h.place_view().await?;
    Ok(v.get("queued").and_then(|q| q.get(class)).and_then(Value::as_u64).unwrap_or(0))
}

/// Both runners full of work kiln didn't place (all their CPUs busy), each knowing the other
/// places: what's asked now waits at their admission.
async fn both_full(lab: &Lab) -> anyhow::Result<()> {
    lab.mac.feed(In::Sampled(reading(100.0))).await;
    lab.linux.feed(In::Sampled(reading(100.0))).await;
    lab.mac.directory().await?;
    lab.linux.directory().await?;
    Ok(())
}

/// A command that writes what it was given of its needs (`KILN_NEEDS`) and which runner asked for
/// it (`KILN_REACH_FROM`) into `dir`, then holds its room until `dir/go` is there.
fn held_until_go(dir: &Path) -> String {
    let d = dir.display();
    format!("printf %s \"$KILN_NEEDS\" > '{d}/needs'; printf %s \"$KILN_REACH_FROM\" > '{d}/from'; while [ ! -e '{d}/go' ]; do sleep 0.05; done")
}

/// A command of 2 CPUs asked of mac while both runners are full waits at linux's admission (an
/// interactive ask, as a person's or an agent's turn waits on it); once linux measures room, its
/// admission offers it and it starts there, holding the 2 CPUs it asked for, within linux's size.
async fn waits_then_holds_its_ask_on_linux(lab: &Lab) -> anyhow::Result<()> {
    let l = &lab.linux;
    let queued = eventually(|| async move { anyhow::Ok(waiting(l, "interactive").await? >= 1) }).await?;
    anyhow::ensure!(queued, "it waits at linux's admission: {}", l.place_view().await?);
    l.feed(In::Sampled(reading(0.0))).await;
    let held = eventually(|| async move { anyhow::Ok(held_and_cap(l).await?.0.first() == Some(&2_000)) }).await?;
    let (now, cap) = held_and_cap(l).await?;
    anyhow::ensure!(held && now.iter().zip(&cap).all(|(h, c)| h <= c), "its 2 CPUs held on linux as it runs, within its size: {now:?} of {cap:?}");
    Ok(())
}

/// The command in `dir` ([`held_until_go`]) ran on linux for mac, given its needs as asked, and
/// its room on linux is free again.
async fn ran_for_mac_as_asked_then_let_go(lab: &Lab, dir: &Path) -> anyhow::Result<()> {
    let needs = std::fs::read_to_string(dir.join("needs"))?;
    let from = std::fs::read_to_string(dir.join("from"))?;
    anyhow::ensure!(needs.contains("cpus=2") && from.contains("mac"), "it ran on linux for mac, given its needs as asked: {needs:?} {from:?}");
    let l = &lab.linux;
    let free = eventually(|| async move { anyhow::Ok(held_and_cap(l).await?.0 == [0, 0, 0, 0]) }).await?;
    anyhow::ensure!(free, "its room on linux is free again: {:?}", held_and_cap(l).await?);
    Ok(())
}

/// `kiln run`'s arguments, as its CLI reads them.
#[derive(clap::Parser)]
struct Kiln {
    #[command(flatten)]
    run: crate::reach::cli::RunArgs,
}

/// mac's reach socket, served as a runner serves its own (`local::serve`), and the one `kiln run`
/// finds while this is held (`local::find_socket`): none again once it's dropped.
struct Served;

impl Served {
    fn mac(lab: &Lab, dir: &Path) -> anyhow::Result<Self> {
        let sock = crate::reach::local::serve(lab.mac.clone(), "mac".to_owned(), dir.join("reach.sock"), true, None)?;
        *crate::reach::local::TEST_SOCKET.lock() = Some(sock.clone());
        let served = Self;
        anyhow::ensure!(crate::reach::local::find_socket()? == sock, "kiln run finds the lab's socket, never this machine's runner");
        Ok(served)
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        *crate::reach::local::TEST_SOCKET.lock() = None;
    }
}

/// `kiln run <args>` started as its CLI runs it (`cli::run`): what it comes to, its error in words.
fn kiln_run(args: &[&str]) -> anyhow::Result<async_channel::Receiver<Result<(), String>>> {
    use clap::Parser as _;
    let a = Kiln::try_parse_from(std::iter::once("kiln").chain(args.iter().copied()))?.run;
    let (tx, rx) = async_channel::bounded(1);
    crate::rt::spawn(async move {
        let _ = tx.send(crate::reach::cli::run(a, crate::rt::Shutdown::new()).await.map_err(|e| format!("{e:#}"))).await;
    });
    Ok(rx)
}

/// The MCP server's tools for an agent session, on mac's socket: each call off the runtime (it
/// blocks, as an agent's `tools/call` waits), its `structuredContent` and whether it's an error.
struct Tools(Arc<dyn Fn(&str, &Value) -> Value + Send + Sync>);

impl Tools {
    async fn call(&self, name: &'static str, args: Value) -> anyhow::Result<(Value, bool)> {
        let f = Arc::clone(&self.0);
        let r = crate::rt::blocking(move || f(name, &args)).await?;
        Ok((r.get("structuredContent").cloned().unwrap_or_default(), r.get("isError").and_then(Value::as_bool) == Some(true)))
    }

    /// Placement's answer for run `id`, as the tool `output` says it, once `want` holds of it.
    async fn placement(&self, id: &str, want: impl Fn(&Answer) -> bool) -> anyhow::Result<Answer> {
        let until = std::time::Instant::now() + HANG;
        loop {
            let (v, _) = self.call("output", json!({"id": id, "wait_s": 0})).await?;
            if let Some(a) = v.get("placement").and_then(|p| serde_json::from_value::<Answer>(p.clone()).ok()).filter(|a| want(a)) {
                return Ok(a);
            }
            anyhow::ensure!(std::time::Instant::now() < until, "no such answer from placement; `output` says {v}");
            crate::rt::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// docs/ci.md W05-R03 (docs/needs.md E5): `kiln run --needs` and the MCP `run` tool's `needs` reach
/// the runners' placement as asked. Asked of mac (the asker's runner, which owns its placement)
/// while both runners are full: `kiln run --needs cpus=2`, its arguments as clap reads them, run by
/// `cli::run` over the reach socket mac serves, waits at linux's admission; once linux measures
/// room it starts there, holding its 2 CPUs, the command given `KILN_NEEDS`, and ends as its
/// command did, its room free again. `--needs cpus=64` is answered never, `kiln run` saying why.
/// The MCP `run` tool's input (`needs`, `cwd`, `sync`, `background`), as an agent calls it on the
/// same socket, does the same, its result carrying placement's answers: queued with its reasons,
/// started on linux with the 2 CPUs held for it, then its end; and never for 64 CPUs, as an error.
#[test]
fn w05_r03_kiln_run_and_mcp_run_doors_reach_placement() -> anyhow::Result<()> {
    crate::rt::block_on(async move {
        let lab = lab_placing().await?;
        let socks = tempfile::tempdir()?;
        let served = Served::mac(&lab, socks.path())?;
        let sock: PathBuf = socks.path().join("reach.sock");
        let asked = kiln_ci::sched::needs::parse("cpus=2").map_err(anyhow::Error::msg)?.request;

        // kiln run's door.
        both_full(&lab).await?;
        let work = tempfile::tempdir()?;
        let (wd, command) = (work.path().display().to_string(), held_until_go(work.path()));
        let ended = kiln_run(&["--needs", "cpus=2", "--no-sync", "--cwd", wd.as_str(), "-q", "--", command.as_str()])?;
        waits_then_holds_its_ask_on_linux(&lab).await?;
        std::fs::write(work.path().join("go"), "")?;
        let ended = crate::rt::timeout(HANG, ended.recv()).await??;
        anyhow::ensure!(ended == Ok(()), "kiln run ended as its command did: {ended:?}");
        ran_for_mac_as_asked_then_let_go(&lab, work.path()).await?;
        let never = crate::rt::timeout(HANG, kiln_run(&["--needs", "cpus=64", "--no-sync", "-q", "--", "true"])?.recv()).await??;
        anyhow::ensure!(never.as_ref().is_err_and(|why| why.contains("never: no runner")), "kiln run says placement's never, and why: {never:?}");
        drop(served);

        // The MCP run tool's door.
        let tools = Tools(Arc::new(crate::reach::mcp::tools_on(&sock, &format!("doors-mcp-{}", crate::util::new_id()))));
        both_full(&lab).await?;
        let work = tempfile::tempdir()?;
        let wd = work.path().display().to_string();
        let (left, failed) = tools.call("run", json!({"command": held_until_go(work.path()), "needs": "cpus=2", "cwd": wd.as_str(), "sync": false, "background": true})).await?;
        let id = left.get("id").and_then(Value::as_str).filter(|_| !failed).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("left running, by its id: {left}"))?;
        let queued = tools.placement(&id, |a| matches!(a, Answer::Queued { .. })).await?;
        anyhow::ensure!(matches!(&queued, Answer::Queued { reasons, .. } if !reasons.is_empty()), "queued, with placement's reasons: {queued:?}");
        waits_then_holds_its_ask_on_linux(&lab).await?;
        let started = tools.placement(&id, |a| matches!(a, Answer::Started { .. })).await?;
        anyhow::ensure!(matches!(&started, Answer::Started { runner, got, .. } if runner == "linux" && got.cpu == asked.cpu), "started on linux, its 2 CPUs held for it: {started:?}");
        std::fs::write(work.path().join("go"), "")?;
        let (end, failed) = tools.call("output", json!({"id": id, "wait_s": HANG.as_secs()})).await?;
        let said = |k: &str| end.get(k).cloned().unwrap_or_default();
        anyhow::ensure!(!failed && said("running") == json!(false) && said("exit_code") == json!(0) && said("runner") == json!("linux"), "it ended on linux as its command did: {end}");
        ran_for_mac_as_asked_then_let_go(&lab, work.path()).await?;
        let (never, failed) = tools.call("run", json!({"command": "true", "needs": "cpus=64", "cwd": wd.as_str(), "sync": false})).await?;
        let answer = never.get("placement").and_then(|p| serde_json::from_value::<Answer>(p.clone()).ok());
        let why = never.get("error").and_then(Value::as_str).unwrap_or_default();
        anyhow::ensure!(failed && matches!(answer, Some(Answer::Never { .. })) && why.contains("never: no runner"), "the MCP run says placement's never, and why: {never}");
        let (l, m) = (&lab.linux, &lab.mac);
        anyhow::ensure!(held_and_cap(l).await?.0 == [0, 0, 0, 0] && waiting(l, "interactive").await? == 0 && waiting(m, "interactive").await? == 0, "nothing held or waiting after: {} {}", l.place_view().await?, m.place_view().await?);
        anyhow::Ok(())
    })
}

/// What kiln's server sends runner processes 1 (mac) and 2 (linux) reaches `lab`'s nodes
/// (`ServerMsg::Place`: own an agent session's start, or let go of it), and what their nodes say
/// of it reaches the server as its net loop hears it (`AgentMsg::Placed`): the two runners'
/// connections, as far as placing sessions goes, until the sender this gives is dropped.
fn connect(hub: &Arc<crate::hub::Hub>, lab: &Lab) -> async_channel::Sender<()> {
    let (stop, stopped) = async_channel::bounded::<()>(1);
    let (h, mac, linux) = (Arc::clone(hub), lab.mac.clone(), lab.linux.clone());
    crate::rt::spawn(async move {
        let mut read = 0;
        loop {
            let sent: Vec<(u64, ServerMsg)> = {
                let s = h.sent.lock();
                let new = s.get(read..).map(<[_]>::to_vec).unwrap_or_default();
                read = s.len();
                new
            };
            for (peer, m) in sent {
                if let ServerMsg::Place { req, json } = m {
                    let node = if peer == 1 { &mac } else { &linux };
                    node.feed(In::SessionPlace { req, json }).await;
                }
            }
            // A test's hub keeps what it sends (`Hub::sent`) and tells no one: looked at again shortly.
            if crate::rt::timeout(Duration::from_millis(10), stopped.recv()).await.is_ok() {
                return;
            }
        }
    });
    let (h, said) = (Arc::clone(hub), lab.said.clone());
    crate::rt::spawn(async move {
        while let Ok((from, m)) = said.recv().await {
            if matches!(m, AgentMsg::Placed { .. }) {
                let _ = crate::hub::tests::hear(&h, if from == "mac" { 1 } else { 2 }, crate::proto::LANE_CONTROL, &m).await;
            }
        }
    });
    stop
}

/// docs/ci.md W05-R03, AUDIT M4 (docs/needs.md E5): the server's door for agent sessions, end to
/// end. kiln's server (a test's hub, mac and linux connected as processes 1 and 2, each placing
/// sessions, linux alone with the harness) places a child agent of a parent on mac, asking 4 CPUs:
/// `place_agents` asks mac's node to own its start (`ServerMsg::Place`), mac's owner probes linux,
/// linux's admission takes it, and mac's word comes back to the server (`AgentMsg::Placed`): go,
/// on linux, with its claim. Started there under that claim, as its runner's host starts it
/// (`Placer::session_start`), it holds its 4 of linux's 8 CPUs until it ends: a second child asking
/// 6 meanwhile waits at linux's admission, never placed past linux's size, and is placed once the
/// first ends (`Placer::session_ended`), in the room the first released. Nothing is held after.
#[test]
fn w05_r03_place_agents_holds_a_child_agents_claim_from_start_to_end() -> anyhow::Result<()> {
    use crate::harness::lineage::place_agents_for_tests as place_agents;
    crate::rt::block_on(async move {
        let lab = lab_placing().await?;
        // mac full of work kiln didn't place; linux idle: 8 CPUs, 16 GiB.
        lab.mac.feed(In::Sampled(reading(100.0))).await;
        lab.linux.feed(In::Sampled(reading(0.0))).await;
        lab.mac.directory().await?;
        lab.linux.directory().await?;
        let hub = crate::hub::tests::scratch_hub().await?;
        for (peer, name) in [(1, "mac"), (2, "linux")] {
            crate::hub::tests::join_runner(&hub, peer, name, crate::proto::CAP_WIRE);
            // Idle as the server last heard (it filters by this, never ranks: placement chooses).
            hub.put_agent(crate::hub::AgentLive { sample: Some(reading(0.0)), ..crate::hub::tests::live_runner(name, "linux", "x86_64") });
            hub.instances.update(|i| i.set_features(peer, vec![crate::proto::feature::SESSIONS.to_owned()]));
        }
        let offering = |ids: &[&str]| {
            crate::harness::kinds::offered()
                .into_iter()
                .map(|mut k| {
                    k.available = ids.contains(&k.id.as_str());
                    k
                })
                .collect::<Vec<_>>()
        };
        hub.harness.set_kinds("agent:mac", offering(&[]));
        hub.harness.set_kinds("agent:linux", offering(&["claude"]));
        let stop = connect(&hub, &lab);
        let parent = crate::harness::session::Info { id: "parent-1".into(), kind: "claude".into(), target: "agent:mac".into(), ..Default::default() };
        let refused = |(code, why): crate::harness::lineage::Refused| anyhow::anyhow!("{code}: {why}");
        let within = |(held, cap): ([u64; 4], [u64; 4])| held.iter().zip(&cap).all(|(h, c)| h <= c);

        let first = crate::rt::timeout(HANG, place_agents(&hub, Some(&parent), Some("cpus=4"), "claude", 1)).await?.map_err(refused)?;
        let [(on, Some(claim))] = first.as_slice() else { anyhow::bail!("one child, with its claim: {first:?}") };
        anyhow::ensure!(on == "agent:linux" && claim.owner == "mac" && claim.run.starts_with("session-"), "placed on linux by mac's owner, its parent's runner: {first:?}");
        anyhow::ensure!(hub.session_places.lock().is_empty(), "the server waits on no more words of it once it's placed");
        anyhow::ensure!(lab.linux.session_started(claim.clone(), Some("child-1".into())).await?, "its claim was live on linux at its start");
        let hc = held_and_cap(&lab.linux).await?;
        anyhow::ensure!(hc.0.first() == Some(&4_000) && within(hc), "its 4 CPUs held on linux from its start: {hc:?}");

        // A second child asking 6 beside the first's 4 of 8: it waits, and nothing passes linux's size.
        let (placed_tx, placed_rx) = async_channel::bounded(1);
        let (h, p) = (Arc::clone(&hub), parent.clone());
        crate::rt::spawn(async move {
            let _ = placed_tx.send(place_agents(&h, Some(&p), Some("cpus=6"), "claude", 1).await).await;
        });
        let l = &lab.linux;
        let queued = eventually(|| async move { anyhow::Ok(waiting(l, "interactive").await? >= 1) }).await?;
        anyhow::ensure!(queued, "the second waits at linux's admission: {}", l.place_view().await?);
        anyhow::ensure!(placed_rx.is_empty(), "the second isn't placed while the first holds its room");
        let hc = held_and_cap(l).await?;
        anyhow::ensure!(hc.0.first() == Some(&4_000) && within(hc), "the first's 4 CPUs alone held while the second waits: {hc:?}");

        // The first ends: its room is free at once, and the second is placed in it.
        lab.linux.session_ended(claim.clone()).await;
        let second = crate::rt::timeout(HANG, placed_rx.recv()).await??.map_err(refused)?;
        let [(on, Some(next))] = second.as_slice() else { anyhow::bail!("the second, with its claim: {second:?}") };
        anyhow::ensure!(on == "agent:linux" && next.owner == "mac" && next.run != claim.run, "the second placed on linux once the first ended: {second:?}");
        anyhow::ensure!(lab.linux.session_started(next.clone(), Some("child-2".into())).await?, "the second's claim was live on linux at its start");
        let hc = held_and_cap(l).await?;
        anyhow::ensure!(hc.0.first() == Some(&6_000) && within(hc), "the second's 6 CPUs held, the first's 4 let go: {hc:?}");
        lab.linux.session_ended(next.clone()).await;
        let hc = held_and_cap(l).await?;
        anyhow::ensure!(hc.0 == [0, 0, 0, 0] && waiting(l, "interactive").await? == 0, "all of it free again: {hc:?}");
        drop(stop);
        anyhow::Ok(())
    })
}
