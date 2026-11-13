//! The node VM's boot under shards-init (docs/design/node-under-shards-init.md §4, §6), against a Shards of the test's
//! own: its image boot and its run, each call recorded, and its run's readiness or its end as each case says.

use std::sync::{Arc, Mutex};

use shards_api::image::{
    Bound, Channel, GuestTimes, HostUse, MemberEnd, MemberInfo, MemberName, Microvm, RunEnd, Stats,
    StatsScope,
};

use super::*;

/// One answer of the test's run to a readiness wait: its readiness as it stands (none yet, as at a timeout), or its
/// end.
#[derive(Debug, Clone)]
enum Says {
    Stands(Option<Ready>),
    Ends(RunEnd),
}

/// What the test's run answers, in order: each readiness wait's answer, and each stats read's VM process CPU time (its
/// host's count, D241's `Microvm`), none where its run host says none. Past its script a wait has its last answer.
#[derive(Debug, Clone)]
struct Script {
    readies: Vec<Says>,
    progress: Vec<Option<u64>>,
}

impl Script {
    /// A run whose readiness is `says` at the first wait.
    fn says(says: Says) -> Self {
        Script {
            readies: vec![says],
            progress: Vec::new(),
        }
    }
}

/// Every call the test's Shards took.
#[derive(Debug, Default)]
struct Calls {
    /// Each boot's spec and patience.
    boots: Vec<(ImageSpec, Duration)>,
    /// Each readiness wait's `seen` and timeout.
    waits: Vec<(Option<Ready>, Duration)>,
    /// Each stats read's scope and patience.
    stats: Vec<(StatsScope, Duration)>,
    stops: usize,
}

struct Shards {
    script: Script,
    calls: Arc<Mutex<Calls>>,
}

impl Images for Shards {
    fn backend(&self) -> Result<shards_api::Backend, ImageRefusal> {
        Ok(shards_api::Backend::Kvm)
    }

    fn boot(
        &self,
        spec: &ImageSpec,
        patience: Duration,
    ) -> Result<Box<dyn ImageGuest>, ImageRefusal> {
        self.calls
            .lock()
            .unwrap()
            .boots
            .push((spec.clone(), patience));
        Ok(Box::new(Run {
            script: self.script.clone(),
            calls: Arc::clone(&self.calls),
        }))
    }
}

/// The test's run of the node image: no member, its readiness and progress as its script says.
struct Run {
    script: Script,
    calls: Arc<Mutex<Calls>>,
}

fn unasked() -> ImageRefusal {
    ImageRefusal::Unsupported {
        what: "the test's run",
        why: "the node VM's boot never asks it".to_owned(),
    }
}

impl ImageGuest for Run {
    fn members(&self) -> &[MemberInfo] {
        &[]
    }

    fn published(&self) -> &[Bound] {
        &[]
    }

    // No agent-control endpoint: the node VM's boot asks for none yet (D252, step 39's Link).
    fn control(&self) -> Option<&shards_api::image::ControlAt> {
        None
    }

    // The period its run host says its governor holds a rate over (D247): the test's fixture, which the node VM's boot
    // never reads, holding it to no rate.
    fn rate_period(&self) -> Duration {
        Duration::from_millis(100)
    }

    // How its microVM started (D253): booted alone, as a run given its guest is, which the node VM's boot never reads.
    fn started_from(&self) -> shards_api::image::StartedFrom {
        shards_api::image::StartedFrom::Booted
    }

    fn take_output(&self, _: &MemberName, _: Channel) -> Result<std::io::PipeReader, ImageRefusal> {
        Err(unasked())
    }

    fn take_input(&self, _: &MemberName) -> Result<std::io::PipeWriter, ImageRefusal> {
        Err(unasked())
    }

    fn wait_member(&self, _: &MemberName, _: Duration) -> Result<Option<MemberEnd>, ImageRefusal> {
        Err(unasked())
    }

    fn stats(&self, scope: StatsScope, patience: Duration) -> Result<Stats, ImageRefusal> {
        let mut calls = self.calls.lock().unwrap();
        let read = calls.stats.len();
        calls.stats.push((scope, patience));
        let cpu_ns = self
            .script
            .progress
            .get(read)
            .or(self.script.progress.last())
            .copied()
            .flatten();
        Ok(Stats {
            microvm: cpu_ns.map(|cpu_ns| Microvm {
                host: Ok(HostUse {
                    cpu_ns,
                    resident_bytes: 0,
                    footprint_bytes: 0,
                }),
                guest: Ok(GuestTimes::default()),
            }),
            ..Stats::default()
        })
    }

    fn room(&self, _: Option<i64>, _: Duration) -> Result<Option<u64>, ImageRefusal> {
        Err(unasked())
    }

    fn rate(
        &self,
        _: Option<std::num::NonZeroU64>,
        _: Duration,
    ) -> Result<Option<std::num::NonZeroU64>, ImageRefusal> {
        Err(unasked())
    }

    fn resize(&self, _: &MemberName, _: u16, _: u16, _: Duration) -> Result<(), ImageRefusal> {
        Err(unasked())
    }

    fn wait_ready(
        &self,
        seen: Option<Ready>,
        timeout: Duration,
    ) -> Result<Option<Ready>, ImageRefusal> {
        let mut calls = self.calls.lock().unwrap();
        let asked = calls.waits.len();
        calls.waits.push((seen, timeout));
        match self
            .script
            .readies
            .get(asked)
            .or(self.script.readies.last())
        {
            Some(Says::Stands(ready)) => Ok(*ready),
            Some(Says::Ends(end)) => Err(ImageRefusal::Ended(end.clone())),
            None => Ok(None),
        }
    }

    fn plug(
        &self,
        _: &MemberName,
        _: &std::path::Path,
        _: &str,
        _: bool,
        _: Duration,
    ) -> Result<(), ImageRefusal> {
        Err(unasked())
    }

    fn unplug(&self, _: &str, _: Duration) -> Result<(), ImageRefusal> {
        Err(unasked())
    }

    fn stop(&self) -> Result<(), ImageRefusal> {
        self.calls.lock().unwrap().stops += 1;
        Ok(())
    }

    fn wait_for(&self, _: Duration) -> Result<Option<RunEnd>, ImageRefusal> {
        Ok(Some(RunEnd::Stopped))
    }

    fn keep_diagnostics(&self, _: std::fs::File) -> Result<(), ImageRefusal> {
        Err(unasked())
    }
}

/// A node's guest under shards-init, its node image, size and shares as a node configures them: inputs to the boot,
/// not a guest's size or an image (any values do).
fn config() -> HostedConfig {
    HostedConfig {
        binary: PathBuf::from("/opt/shards"),
        image: "hex-node@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .to_owned(),
        vcpus: 2,
        memory_mib: 2048,
        shares: vec![
            Share {
                dir: PathBuf::from("/Users"),
                read_only: false,
                at: "/run/hex/shares/0".to_owned(),
            },
            Share {
                dir: PathBuf::from("/opt/tools"),
                read_only: true,
                at: "/run/hex/shares/1".to_owned(),
            },
        ],
        env: vec![(
            OsString::from("SHARDS_HOME"),
            OsString::from("/var/hex/shards"),
        )],
    }
}

/// The node's guest under shards-init, its first boot to come (nothing learned), over a Shards whose run follows
/// `script`.
fn hosted(script: Script) -> (Hosted, Arc<Mutex<Calls>>) {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let shards = Shards {
        script,
        calls: Arc::clone(&calls),
    };
    (Hosted::new(Arc::new(shards), config()), calls)
}

/// The same, once its node has seen a READY: its wait bounded by the slowest seen (W*). Any learned bound does, the
/// cases' runs being at or past it as their scripts say.
fn learned(script: Script) -> (Hosted, Arc<Mutex<Calls>>) {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let shards = Shards {
        script,
        calls: Arc::clone(&calls),
    };
    let ready = Patience::seeded("guest.hosted.ready", Duration::from_millis(1));
    (Hosted::with_ready(Arc::new(shards), config(), ready), calls)
}

/// AC-T7.4.1, joint plan step 39 (original; docs/design/node-under-shards-init.md §4): the node VM boots under
/// shards-init with Hex's own init the run's own command under the nesting grant. Do: boot the node's guest under
/// shards-init on a Shards whose run says READY. Expect, in the one image boot asked:
/// - the pinned image, its size and binary, and the run host's environment;
/// - `/init` alone as its command, and the nesting grant;
/// - at its cold start, no deadline, the boot following its run host (`Duration::MAX`);
/// - the shares at the paths the direct boot's init mounts them at;
/// - no member, no published port, none of the image's grants past the VM, and no credential.
///
/// Then READY waited for before the boot returns: none seen yet, following its run, its answer the guest's.
#[test]
fn original_the_node_vm_boots_under_shards_init_with_hexs_own_init_and_waits_for_its_ready() {
    let (hosted, calls) = hosted(Script::says(Says::Stands(Some(Ready::OnTime))));
    let booted = hosted.boot().unwrap();
    assert_eq!(booted.ready, Ready::OnTime);
    let calls = calls.lock().unwrap();
    let [(spec, patience)] = calls.boots.as_slice() else {
        panic!("{:?}", calls.boots);
    };
    assert_eq!(*patience, Duration::MAX, "the boot follows its run host");
    let config = config();
    assert_eq!(spec.image, config.image);
    assert_eq!((spec.vcpus, spec.memory_mib), (2, 2048));
    assert_eq!(spec.command, Some(vec!["/init".to_owned()]));
    assert!(spec.nesting, "Hex's init needs the nesting grant (D234)");
    assert_eq!(
        spec.ready_by, None,
        "a cold start gives no deadline: it follows its run"
    );
    assert_eq!(spec.binary, Some(PathBuf::from("/opt/shards")));
    assert_eq!(spec.env, config.env);
    assert_eq!(
        spec.shares,
        vec![
            ImageShare {
                host: PathBuf::from("/Users"),
                at: "/run/hex/shares/0".to_owned(),
                read_only: false,
            },
            ImageShare {
                host: PathBuf::from("/opt/tools"),
                at: "/run/hex/shares/1".to_owned(),
                read_only: true,
            },
        ]
    );
    assert!(spec.members.is_empty() && spec.publish.is_empty() && spec.credentials.is_empty());
    assert_eq!(
        spec.egress,
        Narrowing {
            egress: Keep::Only(Vec::new()),
            mcp: Keep::Only(Vec::new()),
            destinations: Keep::Only(Vec::new()),
            dns: false,
            members: None,
        },
        "default deny: none of the image's grants past the VM"
    );
    assert_eq!(
        calls.waits,
        [(None, Duration::MAX)],
        "READY is waited for, following the run"
    );
    assert_eq!(calls.stops, 0);
}

/// AC-T7.4.1, joint plan step 39 (original; §6): a node VM whose run ends before its init says it is ready is no
/// guest. Do: boot on a Shards whose run ends before READY, by its own exit and failed. Expect: the boot refused,
/// typed, naming the run's end, never a guest returned; and the run stopped, so nothing of it outlives the refusal.
#[test]
fn original_a_node_vm_whose_run_ends_before_ready_is_refused_and_stopped() {
    for end in [
        RunEnd::Exited(1),
        RunEnd::Failed("its VM failed".to_owned()),
    ] {
        let (hosted, calls) = hosted(Script::says(Says::Ends(end.clone())));
        match hosted.boot() {
            Err(Refusal::Os { why, .. }) => {
                assert!(
                    why.contains("before its init said it was ready"),
                    "{end:?}: {why}"
                );
            }
            Err(other) => panic!("{end:?}: {other:?}"),
            Ok(_) => panic!("{end:?}: a guest whose run ended was returned"),
        }
        assert_eq!(calls.lock().unwrap().stops, 1, "{end:?}");
    }
}

/// AC-T7.4.1, decision 9 (original; the person's rule of 2026-11-13): a boot slower than every earlier one is not
/// stopped. Its node's slowest READY seen (W*) is a bound only while READYs come within it. Do: boot with W* learned on
/// a Shards whose run is still starting at W*, its VM's CPU time advancing over the next window, then says READY.
/// Expect:
/// - the run given no deadline, as Hex's init would end the boot at one;
/// - READY waited for to W*, then on past it, following the run, a window at a time;
/// - the run's progress read between windows, its VM process's CPU time;
/// - the guest returned, its run never stopped.
#[test]
fn original_a_boot_slower_than_every_earlier_one_is_not_stopped() {
    let (hosted, calls) = learned(Script {
        readies: vec![
            Says::Stands(None),
            Says::Stands(None),
            Says::Stands(Some(Ready::OnTime)),
        ],
        progress: vec![Some(100), Some(200), Some(300)],
    });
    let booted = hosted.boot().unwrap();
    assert_eq!(booted.ready, Ready::OnTime);
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls.boots[0].0.ready_by, None,
        "a learned bound is never the run's deadline"
    );
    assert_eq!(
        calls.stops, 0,
        "a boot slower than the slowest seen was stopped"
    );
    assert_eq!(calls.waits.len(), 3, "{:?}", calls.waits);
    assert!(
        calls
            .waits
            .iter()
            .all(|(_, timeout)| *timeout < Duration::MAX),
        "past W* each window is W*: {:?}",
        calls.waits
    );
    assert!(
        !calls.stats.is_empty(),
        "past W* its run's progress was never read"
    );
}

/// AC-T7.4.1, decision 9 (original): past W*, a run that shows no progress over a whole W* window is refused, and only
/// then. Do: boot with W* learned on a Shards whose run is still starting at W*, its VM's CPU time standing still over
/// the next window. Expect: the boot refused, typed, naming the progress that stood, and its run stopped.
#[test]
fn original_a_boot_with_no_progress_over_a_whole_window_past_its_bound_is_refused() {
    let (hosted, calls) = learned(Script {
        readies: vec![Says::Stands(None)],
        progress: vec![Some(100), Some(100)],
    });
    match hosted.boot() {
        Err(Refusal::Os { why, .. }) => assert!(why.contains("no progress"), "{why}"),
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("a stalled run's guest was returned"),
    }
    assert_eq!(calls.lock().unwrap().stops, 1);
}

/// AC-T7.4.1, decision 9: a deadline passed is waited past. A run that carries one (Hex gives none) says its ready-by
/// passed, then READY late. Do: boot on a Shards whose run says its deadline passed, then READY late. Expect: the late
/// READY is the guest's, its run never stopped.
#[test]
fn a_passed_deadline_is_waited_past_to_its_late_ready() {
    let (hosted, calls) = hosted(Script {
        readies: vec![
            Says::Stands(Some(Ready::Passed(7))),
            Says::Stands(Some(Ready::Late(9))),
        ],
        progress: Vec::new(),
    });
    assert_eq!(hosted.boot().unwrap().ready, Ready::Late(9));
    let calls = calls.lock().unwrap();
    assert_eq!(calls.stops, 0);
    assert_eq!(
        calls.waits[1].0,
        Some(Ready::Passed(7)),
        "the wait went on past what it had seen"
    );
}

/// AC-T7.4.1 (§4): Shards' refusals are typed by what they refused. Do: map an image the store lacks, a grant the
/// image doesn't give, no backend, and a run host that failed. Expect: the image's and the grant's as the node image's
/// Invalid, no backend as Platform naming the OS, and the run host's as Os.
#[test]
fn shards_refusals_are_typed_by_what_they_refused() {
    let op = "booting the node VM under shards-init";
    assert!(matches!(
        refused(op, ImageRefusal::Image("not in the store".to_owned())),
        Refusal::Invalid {
            what: "the node image (HEX_GUEST_IMAGE)",
            ..
        }
    ));
    assert!(matches!(
        refused(op, ImageRefusal::Widening("no NESTING".to_owned())),
        Refusal::Invalid {
            what: "the node image (HEX_GUEST_IMAGE)",
            ..
        }
    ));
    assert!(matches!(
        refused(
            op,
            ImageRefusal::NoBackend {
                os: "macos",
                arch: "x86_64"
            }
        ),
        Refusal::Platform {
            platform: "macos",
            ..
        }
    ));
    assert!(matches!(
        refused(op, ImageRefusal::Host("it ended".to_owned())),
        Refusal::Os { .. }
    ));
}
