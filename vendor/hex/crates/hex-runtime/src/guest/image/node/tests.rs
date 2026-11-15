//! A node VM's READY as its boot follows it ([`follow_ready`]; docs/design/node-under-shards-init.md §6, decision 9),
//! on a run of the test's own whose readiness or end each case scripts, each wait and stats read recorded; and Shards'
//! refusals typed. The boot itself, its spec, its record and its stop are `guest::image`'s w39 cases', through
//! `ImageClass::boot_node`, the one boot path a node VM has (the review's C-6).

use std::sync::{Arc, Mutex};

use shards_api::image::{
    Bound, Channel, ChannelEnd, GuestTimes, HostUse, ImageRefusal, MemberEnd, MemberInfo,
    MemberName, Microvm, RunEnd, RunEnded, Stats, StatsScope, Unused,
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

/// Every call the test's run took.
#[derive(Debug, Default)]
struct Calls {
    /// Each readiness wait's `seen` and timeout.
    waits: Vec<(Option<Ready>, Duration)>,
    /// Each stats read's scope and patience.
    stats: Vec<(StatsScope, Duration)>,
}

/// The test's run of the node image: no member, its readiness and progress as its script says.
struct Run {
    script: Script,
    calls: Arc<Mutex<Calls>>,
}

fn unasked() -> ImageRefusal {
    ImageRefusal::Unsupported {
        what: "the test's run",
        why: "following a node VM's READY never asks it".to_owned(),
    }
}

impl ImageGuest for Run {
    fn members(&self) -> &[MemberInfo] {
        &[]
    }

    fn published(&self) -> &[Bound] {
        &[]
    }

    fn control(&self) -> Option<&shards_api::image::ControlAt> {
        None
    }

    fn rate_period(&self) -> Option<Duration> {
        None
    }

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
            Some(Says::Ends(end)) => Err(ImageRefusal::Ended(RunEnded::unused(
                end.clone(),
                Unused::Untold,
            ))),
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

    fn save(&self, _: &std::path::Path, _: Duration) -> Result<(), ImageRefusal> {
        Err(unasked())
    }

    fn stop(&self) -> Result<(), ImageRefusal> {
        Err(unasked())
    }

    fn wait_for(&self, _: Duration) -> Result<Option<RunEnded>, ImageRefusal> {
        Err(unasked())
    }

    fn keep_diagnostics(&self, _: std::fs::File, _: u64) -> Result<(), ImageRefusal> {
        Err(unasked())
    }

    fn wait_channel(
        &self,
        _: &MemberName,
        _: Channel,
        _: Duration,
    ) -> Result<Option<ChannelEnd>, ImageRefusal> {
        Err(unasked())
    }

    fn take_gpu_report(
        &self,
        _: Duration,
    ) -> Result<shards_api::physical::GpuReport, ImageRefusal> {
        Err(unasked())
    }
}

/// The test's run following `script`, with the calls it records.
fn run(script: Script) -> (Run, Arc<Mutex<Calls>>) {
    let calls = Arc::new(Mutex::new(Calls::default()));
    (
        Run {
            script,
            calls: Arc::clone(&calls),
        },
        calls,
    )
}

/// The READY wait of these cases: each case's own, its answers scripted rather than timed.
fn ready() -> Patience {
    Patience::measuring("test.hex_runtime.node_ready")
}

/// A W* learned as a node learns it: the time a READY took at a cold start, here of a run of the test's that says it at
/// once (decision 9: the slowest answer seen).
fn learned() -> Option<Duration> {
    let (run, _) = run(Script::says(Says::Stands(Some(Ready::OnTime))));
    let began = Instant::now();
    follow_ready(&run, None, began, &ready()).unwrap();
    Some(began.elapsed())
}

/// AC-T7.4.1, joint plan step 39 (original; §6, decision 9): with no READY seen yet in its process, a node VM's READY is
/// followed by its run alone. Do follow READY with no W* on a run that says it. Expect one wait, nothing seen and no
/// timeout, its answer READY on time.
#[test]
fn original_a_cold_ready_is_followed_by_its_run_alone() {
    let (run, calls) = run(Script::says(Says::Stands(Some(Ready::OnTime))));
    assert_eq!(
        follow_ready(&run, None, Instant::now(), &ready()).unwrap(),
        Ready::OnTime
    );
    assert_eq!(calls.lock().unwrap().waits, [(None, Duration::MAX)]);
}

/// AC-T7.4.1, joint plan step 39 (original; §6): a node VM whose run ends before its init says it is ready is no guest.
/// Do follow READY on runs that end first, by their own exit and failed. Expect each refused, typed, naming the run's end;
/// `boot_node` stops a run its READY refuses (C-2).
#[test]
fn original_a_node_vm_whose_run_ends_before_ready_is_refused() {
    for end in [
        RunEnd::Exited(1),
        RunEnd::Failed("its VM failed".to_owned()),
    ] {
        let (run, _) = run(Script::says(Says::Ends(end.clone())));
        match follow_ready(&run, None, Instant::now(), &ready()) {
            Err(Refusal::Os { why, .. }) => {
                assert!(
                    why.contains("before its init said it was ready"),
                    "{end:?}: {why}"
                );
            }
            other => panic!("{end:?}: {other:?}"),
        }
    }
}

/// AC-T7.4.1, decision 9 (original; the person's rule of 2026-11-13): a boot slower than every earlier one is not
/// refused. Its node's slowest READY seen (W*) bounds it only while READYs come within it. Do follow READY with W*
/// learned on a run still starting at W*, its VM's CPU time advancing over the next window, then READY. Expect READY
/// waited for to W*, then on past it a window at a time, the run's progress read between windows, its READY the answer.
#[test]
fn original_a_boot_slower_than_every_earlier_one_is_not_refused() {
    let (run, calls) = run(Script {
        readies: vec![
            Says::Stands(None),
            Says::Stands(None),
            Says::Stands(Some(Ready::OnTime)),
        ],
        progress: vec![Some(100), Some(200), Some(300)],
    });
    assert_eq!(
        follow_ready(&run, learned(), Instant::now(), &ready()).unwrap(),
        Ready::OnTime
    );
    let calls = calls.lock().unwrap();
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
/// then: a VM's boot is the one place decision 9 judges a stall. Do follow READY with W* learned on a run still starting
/// at W*, its VM's CPU time standing still over the next window. Expect it refused, typed, naming the progress that
/// stood.
#[test]
fn original_a_boot_with_no_progress_over_a_whole_window_past_its_bound_is_refused() {
    let (run, _) = run(Script {
        readies: vec![Says::Stands(None)],
        progress: vec![Some(100), Some(100)],
    });
    match follow_ready(&run, learned(), Instant::now(), &ready()) {
        Err(Refusal::Os { why, .. }) => assert!(why.contains("no progress"), "{why}"),
        other => panic!("a stalled run's READY: {other:?}"),
    }
}

/// AC-T7.4.1, decision 9: a deadline passed is waited past. A run that carries one (Hex gives none) says its ready-by
/// passed, then READY late. Do follow READY on such a run. Expect the late READY the answer, the wait going on past
/// what it had seen.
#[test]
fn a_passed_deadline_is_waited_past_to_its_late_ready() {
    let (run, calls) = run(Script {
        readies: vec![
            Says::Stands(Some(Ready::Passed(7))),
            Says::Stands(Some(Ready::Late(9))),
        ],
        progress: Vec::new(),
    });
    assert_eq!(
        follow_ready(&run, None, Instant::now(), &ready()).unwrap(),
        Ready::Late(9)
    );
    assert_eq!(
        calls.lock().unwrap().waits[1].0,
        Some(Ready::Passed(7)),
        "the wait went on past what it had seen"
    );
}

/// AC-T7.4.1 (§4): Shards' refusals are typed by what they refused. Do map an image the store lacks, a grant the image
/// doesn't give, no backend, and a run host that failed. Expect the image's and the grant's as the node image's
/// Invalid, no backend as Platform naming the OS, and the run host's as Os.
#[test]
fn shards_refusals_are_typed_by_what_they_refused() {
    assert!(matches!(
        refused(BOOTING, ImageRefusal::Image("not in the store".to_owned())),
        Refusal::Invalid {
            what: "the node image (HEX_GUEST_IMAGE)",
            ..
        }
    ));
    assert!(matches!(
        refused(BOOTING, ImageRefusal::Widening("no NESTING".to_owned())),
        Refusal::Invalid {
            what: "the node image (HEX_GUEST_IMAGE)",
            ..
        }
    ));
    assert!(matches!(
        refused(
            BOOTING,
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
        refused(BOOTING, ImageRefusal::Host("it ended".to_owned())),
        Refusal::Os { .. }
    ));
}
