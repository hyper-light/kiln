//! A replay on this runner (kiln plan 01a1dd2ea7a777a28970a43fc5f766d0 P7.1b; docs/ci.md W20-R11, "Replays"): a
//! recorded session's tool calls run once in a microVM of one of a split's variants, as the server asks
//! (`ServerMsg::Replay`), and what it measured, as a session of that variant measures it
//! (`harness::session::Measured`).
//!
//! - **Its microVM** is the variant's: its Agentfile's image (kiln's for its harness, or a person's own, its text and
//!   build secrets sent with the ask as a start's are), pinned by its digest as an isolated agent's is
//!   (`super::isolated::source_of`, `super::image`), at the variant's size, through Hex
//!   ([`super::hexvm::HexVms::boot_command`]).
//! - **What runs** is the replay's script (`kiln_ci::replay::script`) as its microVM's own command. No member of its
//!   image is driven, no grant is made, so nothing passes its VM and no model is reached, and no credential is given.
//!   It sees no folder of the machine's but two of its own, gone with it: an empty one at its session's folder's path,
//!   where its steps run, and its scratch (`kiln_ci::replay::AT`), its script and its steps' ends.
//! - **What it measures**, each from Hex: its start, from its boot asked to its box running, and how its microVM
//!   started; its CPU time and its footprint, steady and peak, read where its recording says the step running then ends
//!   (`kiln_ci::replay::wait_ms`), then Hex's final use as its box is deleted once every step has ended.
//! - **A replay that fails** (its image unbuilt, its microVM refused, its run ended before its steps did, its limit
//!   passed) is answered a failure, why, and what it measured before: the server counts it, never drops it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kiln_ci::replay::{self as rp, Plan};
use kiln_seams::sandbox::Share;
use serde::{Deserialize, Serialize};

use super::hexvm::HexVms;
use crate::harness::session::{Isolation, Measured, Spec};

/// One replay, as the server asks it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Ask {
    pub experiment: String,
    /// Which of its experiment's replays this is a run of, its variant, and its round.
    pub replay: u64,
    pub arm: String,
    pub round: usize,
    /// The harness its variant runs: its image is kiln's Agentfile for it where `agentfile` names none.
    pub harness: String,
    /// kiln's Agentfile by its name (`harness-claude`), or a person's own (`agentfile:<id>`) with its text and the
    /// secrets its build is given (`harness::lineage::kept_agentfile`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agentfile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<crate::secrets::mine::BuildSecret>,
    /// Its microVM's vCPUs and MiB; none, Shards' least.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<(u32, u64)>,
    /// The folder its session ran in, on its machine.
    pub folder: String,
    pub plan: Plan,
    /// How long it may run, in seconds.
    pub limit_s: u64,
}

/// What a replay measured.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Answer {
    /// `success` (every step ended, its run read whole) or `failure`.
    pub conclusion: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured: Option<Measured>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<rp::Outcome>,
}

impl Answer {
    fn failed(why: impl Into<String>) -> Self {
        Answer { conclusion: "failure".into(), why: Some(why.into()), ..Answer::default() }
    }
}

/// Why a machine whose microVMs aren't Hex's runs no replay.
pub const NOT_HEX: &str = "this machine's microVMs aren't Hex's: a replay's start, footprint and host CPU are what Hex says of its microVM";

/// The replay `ask` on this machine's microVMs ([`super::microvm`]), its scratch beneath `under`: its variant's image
/// pinned, then [`run_on`] them.
pub async fn answer(ask: Ask, under: &Path) -> Answer {
    let Some(vms) = super::microvm() else { return Answer::failed(super::NO_MICROVM) };
    let Some(hex) = vms.hex.clone() else { return Answer::failed(NOT_HEX) };
    let Some(kind) = crate::harness::kinds::Kind::get(&ask.harness) else { return Answer::failed(format!("{:?} is none of kiln's harnesses", ask.harness)) };
    // Its image named as an isolated start of its variant names it, and checked as that start's is.
    let spec = Spec { isolation: Some(Isolation { image: ask.agentfile.clone(), ..Isolation::microvm() }), agentfile: ask.text.clone(), ..Spec::default() };
    let source = match super::isolated::source_of(kind, &spec) {
        Ok(source) => source,
        Err(why) => return Answer::failed(why),
    };
    let image = match vms.images.pinned(source, &ask.secrets).await {
        Ok(image) => image,
        Err(r) => return Answer::failed(format!("its image, {}: {r}", source.name())),
    };
    run_on(&hex, &image, &ask, under).await
}

/// This process's replays so far, each scratch folder's name its own.
static SCRATCHES: AtomicU64 = AtomicU64::new(0);

/// The ask's scratch beneath `under`, named for this process and its count of replays (one an earlier process of the
/// same id left there replaced): its folder at `AT` with its script in it, and the empty folder its steps start in;
/// and the two as its microVM sees them.
async fn scratch(ask: &Ask, under: &Path) -> std::io::Result<(PathBuf, Vec<Share>)> {
    let folder = guest_folder(ask);
    let script = rp::script(&ask.plan, &folder);
    let dir = under.join(format!("replay-{}-{}", std::process::id(), SCRATCHES.fetch_add(1, Ordering::Relaxed)));
    let made = dir.clone();
    crate::rt::blocking(move || -> std::io::Result<()> {
        if made.exists() {
            std::fs::remove_dir_all(&made)?;
        }
        std::fs::create_dir_all(made.join("at"))?;
        std::fs::create_dir_all(made.join("work"))?;
        std::fs::write(made.join("at").join(rp::SCRIPT), script)
    })
    .await
    .map_err(std::io::Error::other)??;
    let share = |host: PathBuf, at: &str| Share { host: host.to_string_lossy().into_owned(), at: at.to_owned(), writable: true, members: Vec::new() };
    let shares = vec![share(dir.join("at"), rp::AT), share(dir.join("work"), &folder)];
    Ok((dir, shares))
}

/// Scratch folder `dir` removed, with whatever its steps left in it.
async fn remove(dir: PathBuf) {
    let shown = dir.display().to_string();
    match crate::rt::blocking(move || std::fs::remove_dir_all(&dir)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(folder = %shown, error = %e, "a replay's scratch folder wasn't removed"),
        Err(e) => tracing::warn!(folder = %shown, error = %format!("{e:#}"), "a replay's scratch folder wasn't removed"),
    }
}

/// Where a microVM sees its session's folder, as an isolated session of it saw it (`isolated::guest_path`); where no
/// path names it there, its replay's own folder beneath kiln's projects' place.
fn guest_folder(ask: &Ask) -> String {
    super::isolated::guest_path(&ask.folder, super::isolated::PROJECTS).unwrap_or_else(|_| super::isolated::PROJECTS.to_owned())
}

/// What `at`'s steps' ends say so far.
async fn ends_in(at: &Path) -> rp::Ends {
    let file = at.join(rp::ENDED);
    let text = crate::rt::blocking(move || std::fs::read_to_string(file).unwrap_or_default()).await.unwrap_or_default();
    rp::ends(&text)
}

/// The replay `ask` in a microVM of `hex`'s booting `image`, its scratch beneath `under` (module docs).
pub async fn run_on(hex: &HexVms, image: &str, ask: &Ask, under: &Path) -> Answer {
    let (dir, shares) = match scratch(ask, under).await {
        Ok(made) => made,
        Err(e) => return Answer::failed(format!("its scratch folder beneath {}: {e}", under.display())),
    };
    let at = dir.join("at");
    let limit = Duration::from_secs(ask.limit_s.max(1));
    let asked = Instant::now();
    let deadline = asked.checked_add(limit);
    let vm = match hex.boot_command(image, vec!["/bin/sh".into(), format!("{}/{}", rp::AT, rp::SCRIPT)], &shares, ask.size).await {
        Ok(vm) => vm,
        Err(why) => {
            remove(dir).await;
            return Answer::failed(format!("its microVM didn't start: {why}"));
        }
    };
    let start_us = u64::try_from(asked.elapsed().as_micros()).unwrap_or(u64::MAX);
    let started_from = vm.started_from().await.map(|f| super::hexvm::said(f).to_owned());
    let (mut idle, mut seen, mut why) = (0u32, 0usize, None);
    let ends = loop {
        let meter = vm.meter();
        let read_at = Instant::now();
        let read = crate::rt::blocking(move || meter.read()).await;
        let read_ms = u64::try_from(read_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let ends = ends_in(&at).await;
        if ends.done {
            break ends;
        }
        match read {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                why = Some(format!("its run ended before its steps did: {e}"));
                break ends;
            }
            Err(e) => {
                why = Some(format!("its use wasn't read: {e:#}"));
                break ends;
            }
        }
        (idle, seen) = if ends.ended.len() > seen { (0, ends.ended.len()) } else { (idle.saturating_add(1), seen) };
        // Every step ended and `done` not yet written: read again once a read's time has passed.
        let wait = rp::wait_ms(&ask.plan, ends.ended.len(), idle, read_ms).unwrap_or(read_ms.max(1));
        let due = Instant::now().checked_add(Duration::from_millis(wait));
        match (due, deadline) {
            (Some(due), Some(deadline)) if due < deadline => crate::rt::sleep_until(due).await,
            _ => {
                why = Some(format!("its limit of {} seconds ran out", limit.as_secs()));
                break ends;
            }
        }
    };
    let lived = match vm.end().await {
        Ok(lived) => lived,
        Err(e) => {
            why.get_or_insert_with(|| format!("its microVM wasn't deleted, or its charge wasn't kept: {e}"));
            None
        }
    };
    // What its steps said before its box went, read again now that none writes.
    let ends = if ends.done { ends } else { ends_in(&at).await };
    remove(dir).await;
    let measured = Measured {
        start_us: Some(start_us),
        started_from,
        cpu_ns: lived.map(|l| l.cpu_ns),
        window_ns: lived.map(|l| l.window_ns),
        footprint_steady_bytes: lived.map(|l| l.footprint_steady_bytes),
        footprint_peak_bytes: lived.map(|l| l.footprint_peak_bytes),
    };
    let finished = ends.done && why.is_none();
    Answer { conclusion: if finished { "success" } else { "failure" }.into(), why, measured: Some(measured), outcome: Some(rp::outcome(&ask.plan, &ends)) }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use hex_api::sim::{SIM_IMAGE, SimRuntime};
    use hex_api::{BoxId, BoxInfo, BoxSpec, Class, ClassSpec, HostUse, Member, Proc, Program, Refusal as H, Runtime, StartedFrom, Stdio, Usage};
    use kiln_ci::place::holder::BoxName;
    use kiln_ci::replay::{Kind, Ran, Recorded};
    use serde_json::json;

    use super::*;

    /// Hex's simulator, with what it doesn't model: a VM's host use as Hex's image runtime answers it (its footprint
    /// the memory its box was sized to, its CPU the guest time its simulated vCPUs ran), how each started as Shards'
    /// templates have it (the first of an image booted saving its template, every later one restored), and the guest
    /// running its own command: at its start, its script's steps' ends written to its scratch as `ends` says them (none:
    /// the guest wrote nothing, its run still running). Each spec it was asked to create kept.
    struct Guest {
        sim: SimRuntime,
        sizes: parking_lot::Mutex<HashMap<String, u64>>,
        images: parking_lot::Mutex<HashSet<String>>,
        froms: parking_lot::Mutex<HashMap<String, StartedFrom>>,
        specs: parking_lot::Mutex<Vec<BoxSpec>>,
        ends: Option<String>,
    }

    impl Guest {
        fn new(ends: Option<&str>) -> Self {
            Guest { sim: SimRuntime::new(), sizes: Default::default(), images: Default::default(), froms: Default::default(), specs: Default::default(), ends: ends.map(str::to_owned) }
        }

        fn host_use_of(&self, id: &BoxId) -> Result<HostUse, H> {
            let u = self.sim.usage(id)?;
            let guest_ns = match u.guest {
                Some(hex_api::GuestTime::PerVcpuNs(v)) => v,
                _ => Vec::new(),
            };
            let footprint_bytes = self.sizes.lock().get(&id.to_string()).copied().unwrap_or(0);
            Ok(HostUse { cpu_ns: guest_ns.iter().sum(), footprint_bytes, resident_bytes: footprint_bytes, guest_ns, window: Duration::from_millis(1), ..HostUse::default() })
        }
    }

    impl Runtime for Guest {
        fn classes(&self) -> Vec<Class> {
            self.sim.classes()
        }
        fn create(&self, spec: BoxSpec) -> Result<BoxInfo, H> {
            let id = spec.id.to_string();
            let mem = spec.allocation.and_then(|a| a.vm_size().ok()).map_or(0, |s| s.memory_mib << 20);
            let image = match &spec.class {
                ClassSpec::ShardsImage(i) => i.image.clone(),
                _ => String::new(),
            };
            let from = if self.images.lock().insert(image) { StartedFrom::Saved } else { StartedFrom::Restored };
            self.sizes.lock().insert(id.clone(), mem);
            self.froms.lock().insert(id, from);
            self.specs.lock().push(spec.clone());
            self.sim.create(spec)
        }
        fn start(&self, id: &BoxId) -> Result<(), H> {
            self.sim.start(id)?;
            let spec = self.specs.lock().iter().find(|s| s.id == *id).cloned();
            if let (Some(ends), Some(ClassSpec::ShardsImage(image))) = (&self.ends, spec.map(|s| s.class)) {
                let at = image.shares.iter().find(|m| m.destination == rp::AT).map(|m| m.source.clone());
                if let Some(at) = at {
                    std::fs::write(at.join(rp::ENDED), ends).map_err(|e| H::Broken(e.to_string()))?;
                }
            }
            Ok(())
        }
        fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, H> {
            self.sim.exec(id, program, stdio)
        }
        fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, H> {
            self.sim.run(spec, program, stdio)
        }
        fn signal(&self, id: &BoxId, signal: hex_api::Signal) -> Result<(), H> {
            self.sim.signal(id, signal)
        }
        fn inspect(&self, id: &BoxId) -> Result<BoxInfo, H> {
            self.sim.inspect(id)
        }
        fn usage(&self, id: &BoxId) -> Result<Usage, H> {
            let mut u = self.sim.usage(id)?;
            u.started_from = self.froms.lock().get(&id.to_string()).copied();
            Ok(u)
        }
        fn list(&self) -> Result<Vec<BoxInfo>, H> {
            self.sim.list()
        }
        fn delete(&self, id: &BoxId) -> Result<(), H> {
            self.sim.delete(id)
        }
        fn member(&self, id: &BoxId, member: &Member) -> Result<Box<dyn Proc>, H> {
            self.sim.member(id, member)
        }
        fn delete_with_host_use(&self, id: &BoxId) -> Result<Option<HostUse>, H> {
            let used = self.host_use_of(id).ok();
            self.sim.delete(id).map(|()| used)
        }
        fn host_use(&self, id: &BoxId) -> Result<HostUse, H> {
            self.host_use_of(id)
        }
    }

    fn sim_hex(guest: Guest) -> (Arc<Guest>, HexVms) {
        let rt = Arc::new(guest);
        let n = AtomicU64::new(0);
        let vms = HexVms::new(Arc::<Guest>::clone(&rt), move || Some(BoxName::Unreserved { epoch: 9, n: n.fetch_add(1, Ordering::Relaxed) }));
        (rt, vms)
    }

    /// A session's record: a test run, a file written, a model's turn (a subagent), and a build that failed.
    fn session() -> Plan {
        let call = |kind, name: &str, input: serde_json::Value, ran, exit_code| Recorded { kind, name: name.into(), input, ran, exit_code, ms: Some(5), shortened: false };
        rp::plan(&[
            call(Kind::Shell, "Bash", json!({"command": "cargo test"}), Ran::Done, Some(0)),
            call(Kind::Edit, "Write", json!({"file_path": "notes.md", "content": "# notes\n"}), Ran::Done, None),
            call(Kind::Agent, "Task", json!({"prompt": "review it"}), Ran::Done, None),
            call(Kind::Shell, "Bash", json!({"command": "make"}), Ran::Error, Some(2)),
        ])
    }

    fn ask(arm: &str, size: Option<(u32, u64)>) -> Ask {
        Ask { experiment: "E7".into(), replay: 1, arm: arm.into(), harness: "claude".into(), size, folder: "/work/app".into(), plan: session(), limit_s: 60, ..Ask::default() }
    }

    /// P7.1b ("replay one session against two variants: both variants' measures; no model endpoint contacted"), over
    /// Hex's simulator: each variant's replay boots its image at its size with the replay's script as its own command,
    /// no member driven, no grant, no credential, and no folder of the machine's but its two of its own, so nothing it
    /// runs reaches past its microVM, a model's endpoint least of all. Each has the measures a session has: its start
    /// (cold the first time, from its template after), its footprint by its size, its host CPU, its steps' ends held to
    /// the record's. Its scratch goes with it.
    #[test]
    fn p7_1b_a_replay_measures_each_variants_microvm_and_reaches_nothing() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let under = tempfile::tempdir()?;
            let (rt, hex) = sim_hex(Guest::new(Some("0 0\n1 0\n2 2\ndone\n")));
            let small = run_on(&hex, SIM_IMAGE, &ask("small", Some((1, 512))), under.path()).await;
            let big = run_on(&hex, SIM_IMAGE, &ask("big", Some((1, 1024))), under.path()).await;
            for a in [&small, &big] {
                assert_eq!((a.conclusion.as_str(), a.why.as_deref()), ("success", None), "{a:?}");
                assert_eq!(a.outcome, Some(rp::Outcome { steps: 3, ended: 3, diverged: 0 }), "{a:?}");
                assert!(a.measured.as_ref().is_some_and(|m| m.start_us.is_some() && m.cpu_ns.is_some()), "{a:?}");
            }
            let m = |a: &Answer| a.measured.clone().unwrap_or_default();
            assert_eq!((m(&small).footprint_steady_bytes, m(&small).footprint_peak_bytes), (Some(512 << 20), Some(512 << 20)));
            assert_eq!(m(&big).footprint_steady_bytes, Some(1024 << 20));
            assert_eq!((m(&small).started_from.as_deref(), m(&big).started_from.as_deref()), (Some("saved"), Some("restored")));
            let specs = rt.specs.lock().clone();
            assert_eq!(specs.len(), 2);
            for spec in &specs {
                let ClassSpec::ShardsImage(image) = &spec.class else { anyhow::bail!("not an image box: {spec:?}") };
                assert_eq!(image.command.as_deref(), Some(&["/bin/sh".to_owned(), format!("{}/{}", rp::AT, rp::SCRIPT)][..]));
                assert!(image.grants.is_empty() && image.credentials.is_empty() && image.sign_ins.is_empty(), "nothing passes its VM: {image:?}");
                assert!(image.members.is_empty(), "no member driven, no harness's conversation: {image:?}");
                let at: Vec<&str> = image.shares.iter().map(|m| m.destination.as_str()).collect();
                assert_eq!(at, [rp::AT, "/work/app"], "its own two folders alone");
                assert!(image.shares.iter().all(|m| m.source.starts_with(under.path()) && !m.read_only), "{image:?}");
            }
            assert_eq!(rt.list()?, [], "its boxes are gone");
            assert_eq!(std::fs::read_dir(under.path())?.count(), 0, "its scratch went with it");
            anyhow::Ok(())
        })
    }

    /// P7.1b ("a failing variant's failures are counted"): a size Shards can't boot is refused, a failure said, never a
    /// missing replay; a step that ended otherwise than recorded diverged; and a run whose steps never all end is ended
    /// at its limit, a failure with what it measured.
    #[test]
    fn p7_1b_a_replay_that_cant_run_or_runs_out_fails_and_divergence_is_counted() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let under = tempfile::tempdir()?;
            let (_, hex) = sim_hex(Guest::new(Some("0 0\n1 0\n2 127\ndone\n")));
            let refused = run_on(&hex, SIM_IMAGE, &ask("huge", Some((1, u64::MAX))), under.path()).await;
            assert_eq!(refused.conclusion, "failure", "{refused:?}");
            assert!(refused.why.as_deref().is_some_and(|w| w.contains("didn't start")), "{refused:?}");
            let diverged = run_on(&hex, SIM_IMAGE, &ask("ok", None), under.path()).await;
            assert_eq!(diverged.outcome, Some(rp::Outcome { steps: 3, ended: 3, diverged: 1 }), "{diverged:?}");
            let (rt, hung) = sim_hex(Guest::new(Some("0 0\n")));
            let short = Ask { limit_s: 1, ..ask("slow", None) };
            let over = run_on(&hung, SIM_IMAGE, &short, under.path()).await;
            assert_eq!(over.conclusion, "failure", "{over:?}");
            assert!(over.why.as_deref().is_some_and(|w| w.contains("limit of 1 seconds")), "{over:?}");
            assert_eq!(over.outcome, Some(rp::Outcome { steps: 3, ended: 1, diverged: 0 }), "{over:?}");
            assert!(over.measured.as_ref().is_some_and(|m| m.footprint_steady_bytes.is_some()), "what it measured before: {over:?}");
            assert_eq!(rt.list()?, [], "its box is gone");
            anyhow::Ok(())
        })
    }
}
