//! The node's guest under shards-init (docs/design/node-under-shards-init.md §4, §6): its node VM booted through
//! Shards' image boot (D231) from Hex's node image (`scripts/guest/node.Agentfile`). Hex's own init is the run's own
//! command under the nesting grant (D234), PID 1 of the workload's own PID namespace, and the boot is followed to the
//! READY that init says once its node and relay are ready.
//!
//! Its host reaches that node only through a lane of Shards' agent-control endpoint to the relay's sockets (§5), which
//! Shards doesn't give yet. Until it does, a node configured for this form is refused `Awaits(NestedChannel)`
//! ([`super::GuestConfig::from_vars`]), and `node_hosted_real` boots it to READY for ROOT's measurement.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hex_api::Refusal;
use hex_core::patience::Patience;
use shards_api::image::{
    ImageGuest, ImageRefusal, ImageShare, ImageSpec, Images, Keep, Narrowing, Ready, StatsScope,
};

use super::shares::Share;

/// The command shards-init runs: Hex's own init, `argv[0]` `/init` as the kernel names an initramfs's first process,
/// so that `hex` runs as init under either host (hex-cli's `runs_as_init`).
pub const INIT: &str = "/init";

/// The node VM under shards-init, as its node boots it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedConfig {
    /// The `shards` binary that hosts its run (`SHARDS_BIN`).
    pub binary: PathBuf,
    /// The node image, pinned by digest (`HEX_GUEST_IMAGE`): Hex's own init at `/init`, declaring `NESTING`.
    pub image: String,
    /// The VM's size, the node's to give from its budget (shards-guest.md §1), as the direct boot's.
    pub vcpus: u32,
    pub memory_mib: u64,
    /// The host directories the node shares, each where Hex's init finds it (`/run/hex/shares/<i>`).
    pub shares: Vec<Share>,
    /// The run host's own environment: `SHARDS_HOME`, whose store holds the image, and the rest of the node's Shards
    /// paths.
    pub env: Vec<(OsString, OsString)>,
}

impl HostedConfig {
    /// The image boot that runs Hex's own init under the nesting grant. Its parts:
    /// - **command**: [`INIT`] alone;
    /// - **no deadline**: Hex's init would end the boot at one, and a node's learned bound is when its wait tells the
    ///   census a run-out, never when a boot ends (decision 9);
    /// - **shares**: at the paths the direct boot's init mounts them at, so the host's rewrite is one;
    /// - **no member**: none of the image's own (`NESTING` declares none);
    /// - **no published port and no grant past the VM** (default deny): the host reaches the node through its run's
    ///   agent-control endpoint alone (§5);
    /// - **no credential**.
    pub fn spec(&self) -> ImageSpec {
        ImageSpec {
            image: self.image.clone(),
            vcpus: self.vcpus,
            memory_mib: self.memory_mib,
            command: Some(vec![INIT.to_owned()]),
            members: Vec::new(),
            publish: Vec::new(),
            egress: Narrowing {
                egress: Keep::Only(Vec::new()),
                mcp: Keep::Only(Vec::new()),
                destinations: Keep::Only(Vec::new()),
                dns: false,
                members: None,
            },
            credentials: Vec::new(),
            // No credential, so nothing leases one (D242).
            lessor: None,
            shares: self
                .shares
                .iter()
                .map(|share| ImageShare {
                    host: share.dir.clone(),
                    at: share.at.clone(),
                    read_only: share.read_only,
                })
                .collect(),
            nesting: true,
            ready_by: None,
            binary: Some(self.binary.clone()),
            env: self.env.clone(),
            // No agent-control endpoint yet (D252): step 39's Link asks for one.
            control: None,
            // No GPU: the node VM renders on none (D234's `gpus=` is step 39's).
            gpu_devices: Vec::new(),
        }
    }
}

/// A node VM under shards-init whose init has said it is ready.
pub struct HostedGuest {
    pub guest: Box<dyn ImageGuest>,
    /// How its init said it: on time, as a run given no deadline says it, or late past a deadline its run carried.
    pub ready: Ready,
    /// From the boot's ask to its READY: the node VM's start, its run host's, its VM's and its init's.
    pub took: Duration,
}

/// The node's guest under shards-init: what boots it, and how long its READY takes, learned from its own boots.
pub struct Hosted {
    images: Arc<dyn Images>,
    config: HostedConfig,
    /// `guest.hosted.ready`, W*: the slowest READY this node has seen, none before the first ([`follow_ready`]).
    ready: Patience,
}

impl Hosted {
    pub fn new(images: Arc<dyn Images>, config: HostedConfig) -> Self {
        Self {
            images,
            config,
            ready: Patience::measuring("guest.hosted.ready"),
        }
    }

    /// The same, its READY's wait as `ready` has learned it: a node that has seen READYs.
    #[cfg(test)]
    pub(crate) fn with_ready(
        images: Arc<dyn Images>,
        config: HostedConfig,
        ready: Patience,
    ) -> Self {
        Self {
            images,
            config,
            ready,
        }
    }

    /// Boots the node VM and follows it to READY.
    /// - **The boot** waits on its run host: `Duration::MAX`, until the run starts or its host ends.
    /// - **READY** is followed as [`follow_ready`] says. Its answer, however long it took, is the sample that W*, the
    ///   slowest READY seen, rises to.
    /// - **A boot that fails to READY** stops its run, and is refused, typed by what failed.
    pub fn boot(&self) -> Result<HostedGuest, Refusal> {
        let bound = self.ready.deadline();
        let began = Instant::now();
        let guest = self
            .images
            .boot(&self.config.spec(), Duration::MAX)
            .map_err(|why| refused(BOOTING, why))?;
        match follow_ready(guest.as_ref(), bound, began, &self.ready) {
            Ok(ready) => {
                let took = began.elapsed();
                self.ready.answered(took);
                Ok(HostedGuest { guest, ready, took })
            }
            Err(why) => {
                stop(guest.as_ref());
                Err(why)
            }
        }
    }
}

const BOOTING: &str = "booting the node VM under shards-init";
const WAITING: &str = "waiting for the node VM under shards-init to say it is ready";

/// Follows a node VM's run to its init's READY: the person's rule of 2026-11-13, as decision 9 builds it.
/// - **Within W*** (`bound`, the slowest READY its node has seen, from `began`), READY is the answer.
/// - **Past W*** the run-out is told to the census (`ready`), and is no sample (Karn's point). The wait goes on a W*
///   window at a time while the run progresses, and is refused, stalled, only where a whole window shows none. Its
///   progress is its VM process's CPU time as its host counts it (D241). A window with no reading of it judges no
///   stall: the run is then followed by its life alone, as at a cold start.
/// - **At the cold start**, or after a run-out (no W*), the wait follows the run's life.
/// - **The run's end**, its host's end or a refusal of Shards' ends the wait at any time, typed.
/// - **A ready-by passed**, of a run that carries one (Hex gives none), is waited past to its late READY: D234's table
///   moves from it to ready alone.
fn follow_ready(
    guest: &dyn ImageGuest,
    bound: Option<Duration>,
    began: Instant,
    ready: &Patience,
) -> Result<Ready, Refusal> {
    let mut seen = None;
    let Some(window) = bound else {
        return wait_to(guest, &mut seen, Duration::MAX)?.ok_or_else(|| {
            Refusal::Broken("Shards answered no readiness to a wait with no timeout".to_owned())
        });
    };
    if let Some(said) = wait_to(guest, &mut seen, window.saturating_sub(began.elapsed()))? {
        return Ok(said);
    }
    ready.ran_out(window);
    let mut last = progress(guest, window)?;
    // Ends at READY, the run's end, or a whole window with its progress standing: the run's progress is its bound.
    loop {
        if let Some(said) = wait_to(guest, &mut seen, window)? {
            return Ok(said);
        }
        let now = progress(guest, window)?;
        if let (Some(was), Some(is)) = (last, now)
            && is <= was
        {
            return Err(Refusal::Os {
                op: WAITING,
                why: format!(
                    "no progress over a whole {window:?} past the slowest READY its node has seen: its VM \
                     process's CPU time stood at {was} ns"
                ),
            });
        }
        last = now.or(last);
    }
}

/// The run's READY within `timeout`, none by then. A ready-by passed, `seen` already or said now, is waited past to its
/// late READY: D234's table moves at most twice before ready, from starting to ready-by passed, then to ready late.
fn wait_to(
    guest: &dyn ImageGuest,
    seen: &mut Option<Ready>,
    timeout: Duration,
) -> Result<Option<Ready>, Refusal> {
    for _ in 0..2 {
        match guest.wait_ready(*seen, timeout) {
            Ok(Some(Ready::Passed(past))) => *seen = Some(Ready::Passed(past)),
            Ok(said) => return Ok(said),
            Err(why) => return Err(refused(WAITING, why)),
        }
    }
    Err(Refusal::Broken(
        "the node VM's readiness moved past D234's table".to_owned(),
    ))
}

/// The run's progress as its host counts it: its VM process's CPU time, D241's `Microvm` in the run's stats, read
/// within `window`. None where its run host has no such reading, or no answer came within the window.
fn progress(guest: &dyn ImageGuest, window: Duration) -> Result<Option<u64>, Refusal> {
    match guest.stats(StatsScope::Run, window) {
        Ok(stats) => Ok(stats
            .microvm
            .and_then(|microvm| microvm.host.ok())
            .map(|host| host.cpu_ns)),
        Err(ImageRefusal::Unsupported { .. }) => Ok(None),
        Err(why) => Err(refused(WAITING, why)),
    }
}

/// Ends a node VM that won't serve: a stop is never refused (D231), and its run's end is the stop's own return.
fn stop(guest: &dyn ImageGuest) {
    let _ = guest.stop();
}

/// Shards' refusal, typed by what it refused.
fn refused(op: &'static str, why: ImageRefusal) -> Refusal {
    const WHAT: &str = "the node VM under shards-init";
    match why {
        ImageRefusal::NoBackend { os, arch } => Refusal::Platform {
            what: WHAT,
            platform: os,
            why: format!("shards boots no guest on {os} {arch}"),
        },
        ImageRefusal::Spec { what, why } => Refusal::Invalid {
            what: WHAT,
            why: format!("{what}: {why}"),
        },
        ImageRefusal::Image(why) | ImageRefusal::Widening(why) => Refusal::Invalid {
            what: "the node image (HEX_GUEST_IMAGE)",
            why,
        },
        ImageRefusal::Unsupported { what, why } => Refusal::Platform {
            what,
            platform: std::env::consts::OS,
            why,
        },
        ImageRefusal::Ended(end) => Refusal::Os {
            op,
            why: format!("its run ended before its init said it was ready: {end:?}"),
        },
        other => Refusal::Os {
            op,
            why: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests;
