//! A runner process's Build (`AgentMsg::Build`, which every runner but a job's pod sends as it connects):
//! the build it runs and what it can do, taken on the hub's loop in pieces that give its shard up between
//! them once its turn's quantum would pass (`crate::rt::Turns`; goal 9, plan 01a1a87b2cec step 3). It was
//! one poll of a dozen things (its process, its upgrades, its front door, sync's, observe's and the
//! cluster's announcements, the conversation store's mirrors and a restore, CI's word), which held the
//! HTTP listener's shard 5 to 34 ms as runners connected in the lab's g9p6: the mirrors' file written and
//! synced there, now the kept files' writer's, off the shards (`crate::keep::synced`).
//!
//! The pieces keep the order they had, the runner's process first: from there it's built
//! (`Hub::runner_built`) and may be given work, so its Build comes before its first job, and every piece
//! after it, CI's word among them, finds it built. The hub's loop takes the runner's next message once
//! its Build's last piece is done, as it did.

use std::sync::Arc;

use hyper_transport::PeerId;

use super::Hub;
use crate::proto::{self, ServerMsg};
use crate::wire::events::ServerEvent;

/// A piece of a runner process's Build, in the order they're taken ([`pieces`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Piece {
    /// Its build and what it can do, on its process (`Instances::set_build`): built from here.
    Process,
    /// It sends its legs' events and results again after a reconnect: told they're taken once (a
    /// receipt without a job).
    Received,
    /// Upgrades of it to that build, through; the fleet's turn; the UI told.
    Updates,
    /// Its front door, from the process that runs it (its other processes stand by); the UI asks again.
    FrontDoor,
    /// Repo sync: its newest process that syncs does, every other one lets go.
    Sync,
    /// Observe's replica: its newest process that replicates does, every other one lets go.
    Observe,
    /// The cluster's learner: its newest process that can hold one does, every other one lets go.
    Cluster,
    /// The conversation store's mirrors picked again, and a restore from it asked for when due.
    Mirrors,
    /// CI's engine: what it can do decides which legs it may take.
    Ci,
    /// Previews' deployments it's left to take down (docs/ci.md W15-R07), on their own.
    Previews,
}

/// Every piece, in the order they're taken.
const ALL: [Piece; 10] = [Piece::Process, Piece::Received, Piece::Updates, Piece::FrontDoor, Piece::Sync, Piece::Observe, Piece::Cluster, Piece::Mirrors, Piece::Ci, Piece::Previews];

impl Piece {
    /// What a Build must say it can do for this piece to be taken: nothing, for those every one takes.
    fn needs(self) -> u32 {
        match self {
            Piece::Received => proto::CAP_RECEIVED,
            Piece::Sync => proto::CAP_SYNC,
            Piece::Observe => proto::CAP_OBSERVE,
            Piece::Cluster => proto::CAP_CLUSTER,
            Piece::Mirrors => proto::CAP_CONVO,
            Piece::Previews => proto::CAP_UNDEPLOY,
            Piece::Process | Piece::Updates | Piece::FrontDoor | Piece::Ci => 0,
        }
    }
}

/// The pieces a Build saying it can do `caps` takes, in order.
pub(crate) fn pieces(caps: u32) -> impl Iterator<Item = Piece> {
    ALL.into_iter().filter(move |p| caps & p.needs() == p.needs())
}

impl Hub {
    /// Runner process `peer` of `agent` said it runs `build` and can do `caps`: taken in its pieces, in order.
    pub(super) async fn on_build(self: &Arc<Self>, peer: PeerId, agent: &str, build: String, caps: u32) {
        self.on_build_noting(peer, agent, &build, caps, &mut |_: Piece| {}).await;
    }

    /// [`Hub::on_build`], `noted` told of each piece once it's done (the tests').
    async fn on_build_noting(self: &Arc<Self>, peer: PeerId, agent: &str, build: &str, caps: u32, noted: &mut (dyn FnMut(Piece) + Send)) {
        // A turn of its own: begun at the shard's next when it's wanted, so its first piece doesn't run on
        // in the poll that took the messages before it.
        crate::rt::maybe_yield().await;
        let mut turns = crate::rt::Turns::new();
        for piece in pieces(caps) {
            self.build_piece(piece, peer, agent, build, caps).await;
            noted(piece);
            turns.end().await;
        }
    }

    async fn build_piece(self: &Arc<Self>, piece: Piece, peer: PeerId, agent: &str, build: &str, caps: u32) {
        match piece {
            Piece::Process => self.instances.update(|i| i.set_build(peer, build.to_owned(), caps)),
            Piece::Received => self.send_peer(peer, &ServerMsg::Received { job: String::new() }).await,
            Piece::Updates => {
                self.updates.on_build(self, agent, build);
                self.updates.fleet.wake();
                self.emit(&ServerEvent::Updates);
            }
            Piece::FrontDoor => crate::frontdoor::control::on_build(self, agent, caps).await,
            Piece::Sync => self.sync_announced(agent).await,
            Piece::Observe => crate::observe::hub::announced(self, agent).await,
            Piece::Cluster => self.cluster_announced(agent).await,
            Piece::Mirrors => {
                self.mirrors_seen();
                self.harness.restore_due(self, agent);
            }
            Piece::Ci => self.ci.send_held(crate::ci::dispatch::Msg::RunnerUp { agent: agent.to_owned(), running: Vec::new() }).await,
            Piece::Previews => {
                let (hub, agent) = (Arc::clone(self), agent.to_owned());
                crate::rt::spawn(async move { crate::ci::previews::runner_up(&hub, &agent).await });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::hub::tests::{live_runner, scratch_hub};

    /// Runner `name` connected to a test's hub as process `peer`, its Hello taken, its record live, and no
    /// Build said yet.
    fn joined_unbuilt(hub: &Hub, peer: PeerId, name: &str) {
        hub.peers.update(|m| m.insert(peer, name.to_owned()));
        hub.put_agent(live_runner(name, "linux", "x86_64"));
        let hello = proto::Hello {
            protocol: proto::VERSION,
            name: name.to_owned(),
            labels: Vec::new(),
            slots: 1,
            hostname: name.to_owned(),
            os: "linux".into(),
            arch: "x86_64".into(),
            version: "test".into(),
            ephemeral: false,
            dedicated_job: None,
            running: Vec::new(),
            terminal: true,
        };
        let now = crate::util::now_unix();
        hub.instances.update(|i| {
            i.join(peer, name, hello.clone(), now);
        });
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a Build takes the pieces its capabilities call for, in the order the
    /// hub's loop took them in one poll before: its process first, so it's built before anything else of its
    /// Build is done and before CI hears of it; a runner from before the capabilities takes its process, its
    /// upgrades, its front door's word and CI's.
    #[test]
    fn g9_s3_a_builds_pieces_are_those_its_capabilities_call_for_in_order() {
        use Piece::*;
        assert_eq!(pieces(u32::MAX).collect::<Vec<_>>(), ALL, "every piece, in order");
        assert_eq!(pieces(0).collect::<Vec<_>>(), [Process, Updates, FrontDoor, Ci]);
        // Each capability adds its piece alone, in its place.
        for (cap, piece) in [(proto::CAP_RECEIVED, Received), (proto::CAP_SYNC, Sync), (proto::CAP_OBSERVE, Observe), (proto::CAP_CLUSTER, Cluster), (proto::CAP_CONVO, Mirrors), (proto::CAP_UNDEPLOY, Previews)] {
            let want: Vec<Piece> = ALL.into_iter().filter(|p| p.needs() == 0 || *p == piece).collect();
            assert_eq!(pieces(cap).collect::<Vec<_>>(), want, "{piece:?}");
        }
        for caps in [0, u32::MAX, proto::CAP_CONVO | proto::CAP_SYNC | proto::CAP_FRONT_DOOR] {
            let order: Vec<Piece> = pieces(caps).collect();
            assert_eq!(order.first(), Some(&Process), "its process first");
            let at = |p: Piece| order.iter().position(|q| *q == p);
            assert!(at(Process) < at(Ci) && at(Mirrors).is_none_or(|m| Some(m) < at(Ci)), "CI's word after the rest, as before: {order:?}");
        }
    }

    /// Goal 9, plan 01a1a87b2cec step 3: each piece of a Build taken through the hub does its work, in order.
    /// Its runner reads unbuilt (no work for it) until its process takes its build and capabilities, the first
    /// piece, and built at every piece after; its legs' receipt goes to it; the UI hears of its upgrades, then of
    /// its front door.
    #[test]
    fn g9_s3_a_builds_pieces_each_do_their_work_in_order() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            joined_unbuilt(&hub, 1, "box");
            anyhow::ensure!(!hub.runner_built("box"), "no Build said yet: no job goes to it (`Hub::pick_agent`)");
            let mut events = hub.subscribe_events()?;
            let caps = proto::CAP_RECEIVED | proto::CAP_FRONT_DOOR | proto::CAP_SYNC | proto::CAP_CLUSTER | proto::CAP_CONVO;
            let watched = Arc::clone(&hub);
            let mut seen: Vec<(Piece, bool, bool)> = Vec::new();
            let received = |hub: &Hub| hub.sent.lock().iter().any(|(p, m)| *p == 1 && matches!(m, ServerMsg::Received { job } if job.is_empty()));
            hub.on_build_noting(1, "box", "b2", caps, &mut |p: Piece| seen.push((p, watched.runner_built("box"), received(&watched)))).await;
            let order: Vec<Piece> = seen.iter().map(|(p, _, _)| *p).collect();
            anyhow::ensure!(order == pieces(caps).collect::<Vec<_>>(), "{order:?}");
            anyhow::ensure!(seen.iter().all(|(_, built, _)| *built), "built from its first piece on: {seen:?}");
            anyhow::ensure!(seen.iter().map(|(p, _, r)| (*p, *r)).take(2).eq([(Piece::Process, false), (Piece::Received, true)]), "its receipt with its second piece: {seen:?}");
            let process = hub.instances.load().get(1).map(|i| (i.build.clone(), i.caps));
            anyhow::ensure!(process == Some((Some("b2".to_owned()), caps)), "{process:?}");
            let mut told = Vec::new();
            while let Ok(text) = events.try_recv() {
                told.push(crate::wire::events::kind_of(&text).unwrap_or_default().to_owned());
            }
            let ours: Vec<&str> = told.iter().map(String::as_str).filter(|k| *k == "updates" || *k == "front_door").collect();
            anyhow::ensure!(ours == ["updates", "front_door"], "{told:?}");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a Build's pieces give its shard up between them once its turn's
    /// quantum would pass. Each piece here as long as the shard's quantum, another task on the same shard that
    /// always wants it runs between every two, and the pieces go on in order after it; the Build was one poll.
    #[test]
    fn g9_s3_a_builds_pieces_give_its_shard_up_between_them() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            joined_unbuilt(&hub, 1, "box");
            let (ran, stop) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
            let other = {
                let (ran, stop) = (Arc::clone(&ran), Arc::clone(&stop));
                crate::rt::spawn(async move {
                    while !stop.load(Ordering::SeqCst) {
                        ran.fetch_add(1, Ordering::SeqCst);
                        hyper_rt::futures::yield_now().await;
                    }
                })
            };
            let quantum = Duration::from_nanos(hyper_rt::futures::step_budget_ns().unwrap_or(crate::rt::STEP_BUDGET_NS));
            let caps = proto::CAP_RECEIVED | proto::CAP_SYNC | proto::CAP_CONVO;
            let mut seen: Vec<(Piece, u64)> = Vec::new();
            hub.on_build_noting(1, "box", "b2", caps, &mut |p: Piece| {
                let began = Instant::now();
                while began.elapsed() <= quantum {
                    std::hint::spin_loop();
                }
                seen.push((p, ran.load(Ordering::SeqCst)));
            })
            .await;
            stop.store(true, Ordering::SeqCst);
            let _ = other.await;
            let order: Vec<Piece> = seen.iter().map(|(p, _)| *p).collect();
            anyhow::ensure!(order == pieces(caps).collect::<Vec<_>>(), "{order:?}");
            for pair in seen.windows(2) {
                if let [(a, before), (b, after)] = pair {
                    anyhow::ensure!(after > before, "{a:?} and {b:?} in one turn: the other task didn't run between them ({seen:?})");
                }
            }
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: the scheduler walks a queue of jobs no runner can take one pick at a time, its turn
    /// taken before each (`rt::Turns`), where it walked them all in one poll every second; a thousand walked so are all
    /// still queued, as before. (Its store calls give the shard up before the walk, so another task's running while it
    /// works can't tell the turns apart: the lab's held polls do, by its site.)
    #[test]
    fn g9_s3_the_schedulers_queue_is_walked_in_turns() -> anyhow::Result<()> {
        use crate::store::{Repo, Run, Status};
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            let repo = hub
                .store
                .upsert_repo(Repo { id: "r1".into(), name: "acme/x".into(), clone_url: "https://example/x".into(), branches: vec![], poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: None, mirror: None })
                .await?;
            let mut yaml = String::from("name: ci\njobs:\n");
            for n in 0..1000 {
                yaml.push_str(&format!("  j{n}: {{steps: [{{run: x}}]}}\n"));
            }
            let planned = crate::pipeline::plan(&crate::pipeline::parse(&yaml)?, &crate::pipeline::Context::default())?;
            let run = Run { id: "run1".into(), repo_id: repo.id.clone(), repo_name: repo.name.clone(), number: 0, pipeline: "ci".into(), event: "push".into(), branch: "main".into(), sha: "abc".into(), message: String::new(), author: String::new(), status: Status::Queued, created_at: 0, started_at: None, finished_at: None, error: None, concurrency_group: None };
            hub.store.create_run(run, planned).await?;
            anyhow::ensure!(hub.store.queued_jobs(2000).await?.len() == 1000, "a thousand queued");
            hub.schedule_once().await?;
            anyhow::ensure!(hub.store.queued_jobs(2000).await?.len() == 1000, "no runner: every one still queued");
            let code: String = include_str!("../hub.rs").split("\n#[cfg(test)]\npub(crate) mod tests {").next().unwrap_or_default().lines().map(|l| l.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n");
            let walk = code.split("async fn schedule_once(&self)").nth(1).and_then(|rest| rest.split("for job in queued {").nth(1)).unwrap_or_default();
            anyhow::ensure!(walk.trim_start().starts_with("turns.end().await;"), "each job's pick in a turn, its poll ended with it: {}", walk.get(..200).unwrap_or(walk));
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a machine's agent sessions, as its runner processes are counted and an older one's
    /// retirement checked, are found by id without a copy of every session's whole info (`Registry::ids_on`): the same
    /// sessions the list's filter found.
    #[test]
    fn g9_s3_a_machines_sessions_are_found_without_copying_every_sessions_info() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::registry::Location;
        use crate::harness::session::Info;
        crate::rt::block_on(async {
            let hub = scratch_hub().await?;
            let (here, there) = (Location::Machine("g9s3-box".into()), Location::Machine("g9s3-other".into()));
            for (id, from) in [("g9s3-a", &here), ("g9s3-b", &there), ("g9s3-c", &here), ("g9s3-d", &Location::Local)] {
                let info = Info { id: id.into(), kind: "claude".into(), name: format!("Agent {id}"), ..Info::default() };
                hub.harness.ingest(from, id, Update::Open { info: Box::new(info), since: None }).await;
            }
            for target in ["agent:g9s3-box", "agent:g9s3-other", "local", "agent:g9s3-none"] {
                let mut found = hub.harness.ids_on(target);
                found.sort();
                let listed: Vec<String> = hub.harness.list().into_iter().filter(|i| i.target == target).map(|i| i.id).collect();
                anyhow::ensure!(found == listed, "{target}: {found:?} != {listed:?}");
            }
            anyhow::ensure!(hub.harness.ids_on("agent:g9s3-box").len() == 2, "the machine's two");
            let code: String = include_str!("../hub.rs").split("\n#[cfg(test)]\npub(crate) mod tests {").next().unwrap_or_default().lines().map(|l| l.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n");
            for f in ["pub fn runner_processes(&self", "pub(crate) async fn check_retire(&self)"] {
                let body = code.split(f).nth(1).and_then(|rest| rest.split("\n    }\n").next()).unwrap_or_default();
                anyhow::ensure!(!body.is_empty() && !body.contains("harness.list()") && body.contains("harness.ids_on("), "{f} finds a machine's sessions without copying them: {body}");
            }
            Ok(())
        })
    }
}
