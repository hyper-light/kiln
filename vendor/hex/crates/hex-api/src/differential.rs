//! The differential suite (CLAUDE.md §8; native plan step 7): one suite every class on every OS
//! passes, run as each OS's tests. It is the [`conformance`] suite and, beside it, scenarios: scripts
//! of calls on several boxes and programs at once, played on the class under test and on the
//! simulator, whose trace is the oracle. A real class passes a scenario when what it shows at each
//! step (a refusal's kind, a box's state and exit, a program's end and output) is what the simulator
//! shows for the same class. A step that reads the runtime follows it for the oracle's answer as decision 9 waits
//! ([`follow`]): within the suite's W* ([`conformance::ENDS`]), and past it, its run-out told, while the runtime's
//! party lives, the life that can still change its answer. So a runtime that reports a fact late passes; one that
//! differs fails at its party's end, naming the step; and a real runtime that differs and stays alive is followed, its
//! run-out told, until its caller's own deadline. Each scenario cites `HEX-DIFF-n`.

use std::io::{PipeReader, PipeWriter, Read, Write};
use std::time::{Duration, Instant};

use crate::conformance::{self, Act, Failure, Fixture};
use crate::patience::Party;
use crate::sim::{SimFixture, SimRuntime};
use crate::{
    Bounds, BoxId, BoxState, Class, ClassSpec, Exit, Proc, Refusal, Runtime, Signal, Stdio,
};

/// A box's bound, as a scenario names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// No bound.
    Free,
    /// Memory at most this many bytes.
    Memory(u64),
    /// A CPU weight of 0, which no runtime takes.
    Invalid,
}

impl Bound {
    fn bounds(self) -> Bounds {
        match self {
            Self::Free => Bounds::default(),
            Self::Memory(m) => Bounds {
                memory_max: Some(m),
                ..Bounds::default()
            },
            Self::Invalid => Bounds {
                cpu_weight: Some(0),
                ..Bounds::default()
            },
        }
    }
}

/// How a step reads a program's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// The exit as it is.
    Exact,
    /// Whether it exited 0: a bound's refusal ends a program by signal on one OS and by a failed
    /// allocation on another, and both are "not 0".
    Ok,
    /// Whether a signal ended it: delete ends a program by KILL, which Windows reports as its own code.
    Signalled,
}

/// One call of a scenario. Boxes are named by their place in the scenario (0, 1, ...), programs by
/// the order the scenario started them; a box the scenario never made is one no runtime has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Create box `b`; an `oci` box is started too, as the conformance suite's boxes are.
    Create(usize, Bound),
    /// Exec a program that does this in box `b`, its stdin and stdout piped to the scenario.
    Exec(usize, Act),
    /// Write these words and a newline to program `p`'s stdin, then close it.
    Feed(usize, &'static str),
    /// Follow program `p` to its end ([`follow`]), and read its end so; where the simulator's program still runs,
    /// watch it as [`Step::Running`] does.
    Wait(usize, End),
    /// Read all program `p` wrote to stdout (after it ended).
    Read(usize),
    /// Watch program `p` for the suite's learned watch ([`conformance::still_running`]): for a liveness
    /// no later step proves, such as a program in another box surviving a signal or a delete, whose end
    /// (by any signal) would read the same as its own box's delete.
    Running(usize),
    /// Whether program `p` runs now, with no watch: for a liveness a later step proves, since a program that
    /// had ended on its own would show a different end or output there.
    Alive(usize),
    /// Signal box `b`.
    Signal(usize, Signal),
    /// Inspect box `b`: its state and its init's exit.
    Look(usize),
    /// Whether list shows box `b`.
    Listed(usize),
    /// Whether box `b`'s usage counts a kill for its memory bound.
    Killed(usize),
    /// Delete box `b`.
    Delete(usize),
}

/// What a step showed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    Done,
    Refused(&'static str),
    Box(BoxState, Option<Exit>),
    Exit(Exit),
    Ok(bool),
    Signalled(bool),
    StillRunning,
    /// A step on a program whose exec was refused.
    NeverStarted,
    Out(String),
    Listed(bool),
    Killed(bool),
}

/// A script of calls, played on the class under test and on the simulator.
pub struct Scenario {
    pub id: &'static str,
    pub says: &'static str,
    pub steps: &'static [Step],
}

const MIB: u64 = 1 << 20;

/// Every scenario, in order.
pub const SCENARIOS: &[Scenario] = &[
    Scenario {
        id: "HEX-DIFF-1",
        says: "two boxes are apart: one's program, signal and delete leave the other's running",
        steps: &[
            Step::Create(0, Bound::Free),
            Step::Create(1, Bound::Free),
            Step::Exec(0, Act::Say("from box zero")),
            Step::Exec(1, Act::Wait),
            Step::Wait(0, End::Exact),
            Step::Read(0),
            Step::Listed(0),
            Step::Listed(1),
            Step::Look(1),
            Step::Exec(0, Act::Wait),
            Step::Signal(0, Signal::TERM),
            Step::Wait(2, End::Exact),
            Step::Look(0),
            Step::Running(1),
            Step::Delete(0),
            Step::Look(0),
            Step::Listed(0),
            Step::Running(1),
            Step::Look(1),
            Step::Delete(1),
            Step::Wait(1, End::Signalled),
            Step::Listed(1),
        ],
    },
    Scenario {
        id: "HEX-DIFF-2",
        says: "programs in one box: one's exit leaves the others running; signal TERM ends them all, and a second signal finds nothing to signal",
        steps: &[
            Step::Create(0, Bound::Free),
            Step::Exec(0, Act::Wait),
            Step::Exec(0, Act::Wait),
            Step::Exec(0, Act::Fail(3)),
            Step::Wait(2, End::Exact),
            Step::Alive(0),
            Step::Alive(1),
            Step::Look(0),
            Step::Signal(0, Signal::TERM),
            Step::Wait(0, End::Exact),
            Step::Wait(1, End::Exact),
            Step::Look(0),
            Step::Signal(0, Signal::TERM),
            Step::Delete(0),
        ],
    },
    Scenario {
        id: "HEX-DIFF-3",
        says: "a refused call changes nothing: an invalid create makes no box, a second create of an id is refused, and every call on a box never made is NoSuchBox",
        steps: &[
            Step::Create(0, Bound::Invalid),
            Step::Look(0),
            Step::Listed(0),
            Step::Create(0, Bound::Free),
            Step::Create(0, Bound::Free),
            Step::Look(0),
            Step::Exec(1, Act::Wait),
            Step::Signal(1, Signal::KILL),
            Step::Killed(1),
            Step::Look(1),
            Step::Delete(1),
            Step::Delete(0),
            Step::Delete(0),
        ],
    },
    Scenario {
        id: "HEX-DIFF-4",
        says: "a deleted box's id is free again: a box made anew under it is fresh and runs programs",
        steps: &[
            Step::Create(0, Bound::Free),
            Step::Exec(0, Act::Wait),
            Step::Delete(0),
            Step::Wait(0, End::Signalled),
            Step::Create(0, Bound::Free),
            Step::Look(0),
            Step::Exec(0, Act::Say("made again")),
            Step::Wait(1, End::Exact),
            Step::Read(1),
            Step::Delete(0),
        ],
    },
    Scenario {
        id: "HEX-DIFF-5",
        says: "a box bounded to 256 MiB: a program using 16 MiB exits 0 and none is killed; one using 1 GiB doesn't exit 0 and the kill is counted",
        steps: &[
            // Test inputs, not tunings: only the order under < bound < over matters. The bound is the
            // native bar's 256 MiB box (bench/native/macos/RESULT.md). The program's own footprint is at
            // most 6 MiB (a climb stopped at 250-254 MiB in that box on all three OSes, bench/native/RESULT.md),
            // so 16 MiB stays under with 234 MiB of margin and 1 GiB passes it four times over.
            Step::Create(0, Bound::Memory(256 * MIB)),
            Step::Exec(0, Act::Hog(16 * MIB)),
            Step::Wait(0, End::Ok),
            Step::Killed(0),
            Step::Exec(0, Act::Hog(1024 * MIB)),
            Step::Wait(1, End::Ok),
            Step::Killed(0),
            Step::Delete(0),
        ],
    },
    Scenario {
        id: "HEX-DIFF-6",
        says: "programs side by side: one echoing stdin keeps running while another says words; closing its stdin ends it with what it was fed",
        steps: &[
            Step::Create(0, Bound::Free),
            Step::Exec(0, Act::Echo),
            Step::Exec(0, Act::Say("beside the echo")),
            Step::Wait(1, End::Exact),
            Step::Read(1),
            Step::Alive(0),
            Step::Feed(0, "fed to the echo"),
            Step::Wait(0, End::Exact),
            Step::Read(0),
            Step::Delete(0),
        ],
    },
];

/// The conformance suite, then every scenario against the simulator's trace for the fixture's class;
/// returns the failures.
pub fn run(rt: &dyn Runtime, fx: &dyn Fixture) -> Vec<Failure> {
    let mut failures = conformance::run(rt, fx);
    let oracle = match oracle_of(fx) {
        Ok(o) => o,
        Err(why) => {
            failures.push(Failure {
                case: "HEX-DIFF",
                why,
            });
            return failures;
        }
    };
    for s in SCENARIOS {
        if let Err(why) = differ(rt, fx, &oracle, s, &|| fx.party()) {
            failures.push(Failure { case: s.id, why });
        }
    }
    failures
}

/// The simulator's fixture for the class the fixture makes, made the same way (structured spec or
/// bundle).
fn oracle_of(fx: &dyn Fixture) -> Result<SimFixture, String> {
    let id = BoxId::new("hex-differential-oracle").map_err(|e| e.to_string())?;
    Ok(match fx.spec(id, Bounds::default()).class {
        ClassSpec::Process => SimFixture::Process,
        ClassSpec::Bundle(_) => SimFixture::Bundle,
        ClassSpec::Oci(_) => SimFixture::Spec,
        ClassSpec::Firecracker(_) => SimFixture::Firecracker,
        ClassSpec::Shards(_) => SimFixture::Shards,
        ClassSpec::ShardsImage(_) => SimFixture::ShardsImage,
    })
}

/// Plays `s` on the simulator, then on `rt` against the simulator's trace, each reading followed for the simulator's
/// answer while `party`, the life that can still change the runtime's answer, lives ([`follow`]); the first step where
/// they differ is the failure.
fn differ(
    rt: &dyn Runtime,
    fx: &dyn Fixture,
    oracle: &SimFixture,
    s: &Scenario,
    party: &dyn Fn() -> Party,
) -> Result<(), String> {
    let sim = SimRuntime::with_term_ending_init(fx.term_ends_init());
    let want = play(&sim, oracle, s, None, party).map_err(|e| format!("the oracle: {e}"))?;
    let got = play(rt, fx, s, Some(&want), party)?;
    let mut steps = s.steps.iter().enumerate().zip(want.iter().zip(&got));
    match steps.find(|(_, (w, g))| w != g) {
        Some(((i, step), (w, g))) => Err(format!(
            "at step {i} ({step:?}) the simulator showed {w:?} and the runtime {g:?}"
        )),
        None if want.len() == got.len() => Ok(()),
        None => Err(format!(
            "the simulator showed {} steps and the runtime {}",
            want.len(),
            got.len()
        )),
    }
}

/// A program a scenario started, with its ends of its pipes.
struct Started {
    proc: Box<dyn Proc>,
    stdin: Option<PipeWriter>,
    stdout: Option<PipeReader>,
}

struct Play<'a> {
    rt: &'a dyn Runtime,
    fx: &'a dyn Fixture,
    scenario: &'static str,
    /// The life that can still change the runtime's answers ([`differ`]).
    party: &'a dyn Fn() -> Party,
    /// Each program the scenario exec'd, in order; `None` where the exec was refused.
    started: Vec<Option<Started>>,
}

/// Plays `s` and returns what each step showed. With `want`, a step that reads the runtime follows it for the oracle's
/// answer ([`follow`]); without, the simulator's play, each reading is its answer, a program's end its own.
fn play(
    rt: &dyn Runtime,
    fx: &dyn Fixture,
    s: &Scenario,
    want: Option<&[Seen]>,
    party: &dyn Fn() -> Party,
) -> Result<Vec<Seen>, String> {
    let mut p = Play {
        rt,
        fx,
        scenario: s.id,
        party,
        started: Vec::new(),
    };
    let ids = p.ids(s.steps)?;
    // Boxes left by an earlier run of the suite are removed first; absent is the usual case.
    for id in &ids {
        let _ = rt.delete(id);
    }
    let mut seen = Vec::with_capacity(s.steps.len());
    let mut outcome = Ok(());
    for (i, step) in s.steps.iter().enumerate() {
        match p.step(i, step, want.and_then(|w| w.get(i))) {
            Ok(s) => seen.push(s),
            Err(e) => {
                outcome = Err(format!("at {step:?}: {e}"));
                break;
            }
        }
    }
    for id in &ids {
        let _ = rt.delete(id);
    }
    outcome.map(|()| seen)
}

impl Play<'_> {
    /// The id of box `b` of this scenario.
    fn id(&self, b: usize) -> Result<BoxId, String> {
        let scenario = self.scenario.to_ascii_lowercase();
        BoxId::new(&format!("{scenario}-{b}")).map_err(|e| e.to_string())
    }

    /// Every box id the steps name.
    fn ids(&self, steps: &[Step]) -> Result<Vec<BoxId>, String> {
        let mut ids: Vec<BoxId> = Vec::new();
        for step in steps {
            let b = match step {
                Step::Create(b, _)
                | Step::Exec(b, _)
                | Step::Signal(b, _)
                | Step::Look(b)
                | Step::Listed(b)
                | Step::Killed(b)
                | Step::Delete(b) => *b,
                Step::Feed(..)
                | Step::Wait(..)
                | Step::Read(_)
                | Step::Running(_)
                | Step::Alive(_) => continue,
            };
            let id = self.id(b)?;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    /// Program `p`, `None` where its exec was refused.
    fn started(&mut self, p: usize) -> Result<Option<&mut Started>, String> {
        self.started
            .get_mut(p)
            .map(Option::as_mut)
            .ok_or_else(|| format!("the scenario execs no program {p}"))
    }

    fn step(&mut self, i: usize, step: &Step, want: Option<&Seen>) -> Result<Seen, String> {
        let (rt, fx, party) = (self.rt, self.fx, self.party);
        Ok(match step {
            Step::Create(b, bound) => {
                let id = self.id(*b)?;
                let spec = self.fx.spec(id.clone(), bound.bounds());
                let class = spec.class.class();
                done(rt.create(spec).and_then(|_| {
                    if class != Class::Process {
                        rt.start(&id)
                    } else {
                        Ok(())
                    }
                }))
            }
            Step::Exec(b, act) => {
                let id = self.id(*b)?;
                let (in_r, in_w) = std::io::pipe().map_err(|e| format!("pipe: {e}"))?;
                let (out_r, out_w) = std::io::pipe().map_err(|e| format!("pipe: {e}"))?;
                let stdio = Stdio {
                    stdin: Some(in_r),
                    stdout: Some(out_w),
                    stderr: None,
                    ..Stdio::null()
                };
                match rt.exec(&id, self.fx.program(act), stdio) {
                    Ok(proc) => {
                        // A program that reads no stdin gets its end at once.
                        let stdin = if *act == Act::Echo { Some(in_w) } else { None };
                        self.started.push(Some(Started {
                            proc,
                            stdin,
                            stdout: Some(out_r),
                        }));
                        Seen::Done
                    }
                    Err(e) => {
                        self.started.push(None);
                        Seen::Refused(kind(&e))
                    }
                }
            }
            Step::Feed(p, words) => {
                let Some(s) = self.started(*p)? else {
                    return Ok(Seen::NeverStarted);
                };
                let mut w = s
                    .stdin
                    .take()
                    .ok_or_else(|| format!("program {p}'s stdin is closed"))?;
                w.write_all(format!("{words}\n").as_bytes())
                    .map_err(|e| format!("write: {e}"))?;
                Seen::Done
            }
            Step::Wait(p, end) => {
                let Some(s) = self.started(*p)? else {
                    return Ok(Seen::NeverStarted);
                };
                match want {
                    // The simulator's program still runs: watched as [`Step::Running`] watches, for an end that
                    // would show it doesn't.
                    Some(Seen::StillRunning) => {
                        match s.proc.wait_for(watch()?).map_err(|e| e.to_string())? {
                            None => Seen::StillRunning,
                            Some(exit) => seen_end(*end, exit),
                        }
                    }
                    // The simulator's own play: its program's end, however late (a scenario waits only on one that
                    // ends there).
                    None => seen_end(
                        *end,
                        conformance::end_of(&mut s.proc).map_err(|e| e.to_string())?,
                    ),
                    // Followed to its end, on its own events (`Duration::MAX`: until it), while the runtime lives.
                    Some(want) => follow(fx, (i, step), want, party, Reads::Events, |slice| {
                        Ok(
                            match s
                                .proc
                                .wait_for(slice.unwrap_or(Duration::MAX))
                                .map_err(|e| e.to_string())?
                            {
                                None => Seen::StillRunning,
                                Some(exit) => seen_end(*end, exit),
                            },
                        )
                    })?,
                }
            }
            Step::Read(p) => {
                let Some(s) = self.started(*p)? else {
                    return Ok(Seen::NeverStarted);
                };
                let mut r = s
                    .stdout
                    .take()
                    .ok_or_else(|| format!("program {p}'s stdout was read"))?;
                let mut out = String::new();
                r.read_to_string(&mut out)
                    .map_err(|e| format!("read: {e}"))?;
                Seen::Out(out)
            }
            Step::Running(p) | Step::Alive(p) => {
                let Some(s) = self.started(*p)? else {
                    return Ok(Seen::NeverStarted);
                };
                let watch = match step {
                    Step::Running(_) => watch()?,
                    _ => Duration::ZERO,
                };
                match s.proc.wait_for(watch).map_err(|e| e.to_string())? {
                    None => Seen::StillRunning,
                    Some(exit) => Seen::Exit(exit),
                }
            }
            Step::Signal(b, signal) => done(rt.signal(&self.id(*b)?, *signal)),
            Step::Look(b) => {
                let id = self.id(*b)?;
                let look = || match rt.inspect(&id) {
                    Ok(info) => Seen::Box(info.state, info.exit),
                    Err(e) => Seen::Refused(kind(&e)),
                };
                match want {
                    None => look(),
                    Some(want) => {
                        follow(fx, (i, step), want, party, Reads::Polled, |_| Ok(look()))?
                    }
                }
            }
            Step::Listed(b) => {
                let id = self.id(*b)?;
                let listed = rt.list().map_err(|e| format!("list: {e}"))?;
                Seen::Listed(listed.iter().any(|i| i.id == id))
            }
            Step::Killed(b) => {
                let id = self.id(*b)?;
                let look = || match rt.usage(&id) {
                    Ok(u) => Seen::Killed(u.oom_kills > 0),
                    Err(e) => Seen::Refused(kind(&e)),
                };
                match want {
                    None => look(),
                    Some(want) => {
                        follow(fx, (i, step), want, party, Reads::Polled, |_| Ok(look()))?
                    }
                }
            }
            Step::Delete(b) => done(rt.delete(&self.id(*b)?)),
        })
    }
}

/// How long a step watches a program it expects to keep running: the slowest end seen
/// ([`conformance::still_running`]), refused where none has been, with nothing to watch against.
fn watch() -> Result<Duration, String> {
    conformance::still_running()
        .ok_or_else(|| "no program's end seen yet to watch against".to_owned())
}

/// A program's `exit` as step `end` reads it.
fn seen_end(end: End, exit: Exit) -> Seen {
    match end {
        End::Exact => Seen::Exit(exit),
        End::Ok => Seen::Ok(exit == Exit::Code(0)),
        End::Signalled => Seen::Signalled(matches!(exit, Exit::Signal(_))),
    }
}

/// How a followed step reads the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reads {
    /// On the runtime's own events, as a program's end comes: each reading waits at most the slice it is given.
    Events,
    /// A box's state or usage, which has no event to wait on: read again after waiting as long as the last read took,
    /// so reading holds at most half the time, at whatever a read costs on this platform.
    Polled,
}

/// Follows step `at` (its place and its call) until the runtime shows the simulator's `want`, as decision 9 waits
/// (the rule at a local site, docs/research/runtime/learned-waits.md): `read` gives the runtime's reading, as `reads`
/// says, waiting at most the slice it is given for one (`None`: until its own next event); within W*
/// ([`conformance::ENDS`]) a slice is what is left of W*, and past it the run-out is told once, to the census and to
/// the test ([`Fixture::told`]), naming the step, the simulator's answer and the runtime's last, and slices have no
/// end. `party` is looked at before each reading: once it has ended, nothing can change the runtime's answer, so that
/// reading is the last, and what it shows is the step's, the difference the scenario then fails at. The answer, however
/// long it took, is the sample W* rises to.
fn follow(
    fx: &dyn Fixture,
    at: (usize, &Step),
    want: &Seen,
    party: &dyn Fn() -> Party,
    reads: Reads,
    mut read: impl FnMut(Option<Duration>) -> Result<Seen, String>,
) -> Result<Seen, String> {
    let asked = Instant::now();
    let bound = conformance::ENDS.deadline();
    let mut past = false;
    loop {
        let ended = party() == Party::Ended;
        let slice = if ended {
            Some(Duration::ZERO)
        } else if past {
            None
        } else {
            bound.map(|within| within.saturating_sub(asked.elapsed()))
        };
        let reading = Instant::now();
        let seen = read(slice)?;
        let took = reading.elapsed();
        if seen == *want {
            conformance::ENDS.answered(asked.elapsed());
            return Ok(seen);
        }
        if ended {
            return Ok(seen);
        }
        if !past
            && let Some(within) = bound
            && asked.elapsed() >= within
        {
            // Past W*: no sample (Karn's point), the run-out told once, and the runtime followed while it lives.
            conformance::ENDS.ran_out(within);
            let (i, step) = at;
            fx.told(&format!(
                "at step {i} ({step:?}) the simulator showed {want:?} and the runtime {seen:?}, past {within:?}: \
                 following the runtime while it lives"
            ));
            past = true;
        }
        if reads == Reads::Polled {
            conformance::rest(took)?;
        }
    }
}

fn done(r: Result<(), Refusal>) -> Seen {
    match r {
        Ok(()) => Seen::Done,
        Err(e) => Seen::Refused(kind(&e)),
    }
}

/// A refusal's kind: what the contract says, apart from each runtime's wording.
fn kind(r: &Refusal) -> &'static str {
    match r {
        Refusal::Invalid { .. } => "Invalid",
        Refusal::NoSuchBox(_) => "NoSuchBox",
        Refusal::Exists(_) => "Exists",
        Refusal::NotRunning(..) => "NotRunning",
        Refusal::Platform { .. } => "Platform",
        Refusal::Full(_) => "Full",
        Refusal::Exhausted { .. } => "Exhausted",
        Refusal::Os { .. } => "Os",
        Refusal::Broken(_) => "Broken",
        Refusal::Lifecycle { .. } => "Lifecycle",
        Refusal::Backing { .. } => "Backing",
        Refusal::Connect { .. } => "Connect",
        Refusal::Uncertain { .. } => "Uncertain",
        // What it awaits is the contract's, not a runtime's wording.
        Refusal::Awaits { needs } => match needs {
            crate::Upstream::GuestCpuTimePerVcpu => "Awaits(GuestCpuTimePerVcpu)",
            crate::Upstream::MemberExec => "Awaits(MemberExec)",
            crate::Upstream::MemberGrantsByName => "Awaits(MemberGrantsByName)",
            crate::Upstream::NestedRuntime => "Awaits(NestedRuntime)",
            crate::Upstream::WindowsRunHost => "Awaits(WindowsRunHost)",
            crate::Upstream::AttachAfterCrash => "Awaits(AttachAfterCrash)",
            crate::Upstream::MemberSignal => "Awaits(MemberSignal)",
            crate::Upstream::MemberGrantsByAddress => "Awaits(MemberGrantsByAddress)",
            crate::Upstream::MemberHostService => "Awaits(MemberHostService)",
            crate::Upstream::GuestRoom => "Awaits(GuestRoom)",
            crate::Upstream::RunHostStdioOnly => "Awaits(RunHostStdioOnly)",
            crate::Upstream::PublishReceiver => "Awaits(PublishReceiver)",
            crate::Upstream::RingPinsWithinRoom => "Awaits(RingPinsWithinRoom)",
            crate::Upstream::LiveMove => "Awaits(LiveMove)",
            crate::Upstream::MemberDestinations => "Awaits(MemberDestinations)",
            crate::Upstream::NestedChannel => "Awaits(NestedChannel)",
            crate::Upstream::RatePeriod => "Awaits(RatePeriod)",
        },
        Refusal::Failed { .. } => "Failed",
        Refusal::Unconfigured { .. } => "Unconfigured",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoxInfo, Program, Usage};

    /// HEX-DIFF-1..6: the simulator agrees with itself for each class and each way to make an oci box.
    #[test]
    fn the_simulator_passes_the_differential_suite() {
        for fx in [
            SimFixture::Process,
            SimFixture::Bundle,
            SimFixture::Spec,
            SimFixture::Firecracker,
            SimFixture::Shards,
        ] {
            let failures = run(&SimRuntime::new(), &fx);
            assert!(failures.is_empty(), "{fx:?}: {failures:#?}");
        }
    }

    /// A runtime that is the simulator but for one lie.
    struct Liar {
        sim: SimRuntime,
        lie: Lie,
        ran: std::sync::Mutex<Vec<BoxId>>,
    }

    #[derive(Clone, Copy, Debug)]
    enum Lie {
        /// Signal of a box with nothing running is taken, not refused as NotRunning.
        SignalsNothing,
        /// Delete leaves the box listed.
        KeepsDeleted,
        /// Inspect reports a box as running for good once anything ran in it.
        StuckRunning,
        /// Exec is refused in a box whose programs have all ended (macOS's first class did this).
        NoExecAfterEnd,
    }

    impl Runtime for Liar {
        fn classes(&self) -> Vec<Class> {
            self.sim.classes()
        }
        fn create(&self, spec: crate::BoxSpec) -> Result<BoxInfo, Refusal> {
            self.sim.create(spec)
        }
        fn start(&self, id: &BoxId) -> Result<(), Refusal> {
            self.sim.start(id)
        }
        fn exec(&self, id: &BoxId, p: Program, s: Stdio) -> Result<Box<dyn Proc>, Refusal> {
            let mut ran = self.ran.lock().unwrap();
            if matches!(self.lie, Lie::NoExecAfterEnd)
                && ran.contains(id)
                && self.sim.inspect(id)?.state == BoxState::Empty
            {
                return Err(Refusal::os("spawning a program in a box", "EPERM"));
            }
            ran.push(id.clone());
            self.sim.exec(id, p, s)
        }
        fn run(
            &self,
            spec: crate::BoxSpec,
            p: Program,
            s: Stdio,
        ) -> Result<Box<dyn Proc>, Refusal> {
            self.sim.run(spec, p, s)
        }
        fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
            match (self.lie, self.sim.signal(id, signal)) {
                (Lie::SignalsNothing, Err(Refusal::NotRunning(..))) => Ok(()),
                (_, r) => r,
            }
        }
        fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
            let mut info = self.sim.inspect(id)?;
            if matches!(self.lie, Lie::StuckRunning) && info.state == BoxState::Empty {
                info.state = BoxState::Running;
            }
            Ok(info)
        }
        fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
            self.sim.usage(id)
        }
        fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
            self.sim.list()
        }
        fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
            match self.lie {
                Lie::KeepsDeleted => self.sim.inspect(id).map(|_| ()),
                _ => self.sim.delete(id),
            }
        }
    }

    /// Non-vacuity (CLAUDE.md §6): do a runtime that lies in one way, expect the scenario that reads
    /// that fact to fail, naming the step where it differs.
    #[test]
    fn a_runtime_that_differs_from_the_simulator_fails_the_scenario_that_shows_it() {
        let cases = [
            (Lie::SignalsNothing, "HEX-DIFF-2", "at step 12 (Signal(0"),
            (Lie::KeepsDeleted, "HEX-DIFF-1", "at step 15 (Look(0)"),
            (Lie::StuckRunning, "HEX-DIFF-2", "at step 11 (Look(0)"),
            (
                Lie::NoExecAfterEnd,
                "HEX-DIFF-1",
                "at step 9 (Exec(0, Wait)",
            ),
        ];
        for (lie, scenario, step) in cases {
            let liar = Liar {
                sim: SimRuntime::new(),
                lie,
                ran: std::sync::Mutex::default(),
            };
            let s = SCENARIOS.iter().find(|s| s.id == scenario).unwrap();
            // A double's answers can't change: its party has ended, so each reading is its last, and the scenario
            // fails at once where it differs, through the follow every runtime takes.
            let why = differ(
                &liar,
                &SimFixture::Process,
                &SimFixture::Process,
                s,
                &|| Party::Ended,
            )
            .unwrap_err();
            assert!(why.contains(step), "{lie:?}: {why}");
        }
    }

    /// A fixture of the simulator's, its runtime a double whose answers can't change once given: its party has
    /// ended, as a follow looks at it.
    struct Settled(SimFixture);

    impl Fixture for Settled {
        fn exec_limit(&self) -> Option<crate::ExecLimit> {
            self.0.exec_limit()
        }
        fn spec(&self, id: BoxId, bounds: crate::Bounds) -> crate::BoxSpec {
            self.0.spec(id, bounds)
        }
        fn init(&self, id: BoxId, act: &Act) -> crate::BoxSpec {
            self.0.init(id, act)
        }
        fn program(&self, act: &Act) -> Program {
            self.0.program(act)
        }
        fn term_ends_init(&self) -> bool {
            self.0.term_ends_init()
        }
        fn refused_forks_counted(&self) -> bool {
            self.0.refused_forks_counted()
        }
        fn party(&self) -> Party {
            Party::Ended
        }
    }

    /// A fixture of the simulator's whose class ends an init on TERM, as Windows' oci class does.
    struct TermEnds;

    impl Fixture for TermEnds {
        fn exec_limit(&self) -> Option<crate::ExecLimit> {
            Some(crate::sim::SIM_EXEC_LIMIT)
        }
        fn spec(&self, id: BoxId, bounds: crate::Bounds) -> crate::BoxSpec {
            SimFixture::Bundle.spec(id, bounds)
        }
        fn init(&self, id: BoxId, act: &Act) -> crate::BoxSpec {
            SimFixture::Bundle.init(id, act)
        }
        fn program(&self, act: &Act) -> Program {
            SimFixture::Bundle.program(act)
        }
        fn term_ends_init(&self) -> bool {
            true
        }
    }

    /// HEX-DIFF-1, 2 (the lead's 2026-11-05 ruling: what TERM does to an init is the class's): do a
    /// runtime whose init ends on TERM, expect the suite to pass when its fixture says so, and to fail
    /// at the first look at the box when the fixture says the init ignores TERM (Linux's rule).
    #[test]
    fn what_term_does_to_an_init_follows_the_fixture() {
        let ends = SimRuntime::with_term_ending_init(true);
        assert_eq!(run(&ends, &TermEnds), Vec::new());
        // The simulator's answers can't change once given: its party has ended, so the run fails where it differs.
        let failures = run(&ends, &Settled(SimFixture::Bundle));
        assert!(
            failures
                .iter()
                .any(|f| f.case == "HEX-DIFF-2" && f.why.contains("at step 11 (Look(0)")),
            "{failures:#?}"
        );
    }

    /// Do: follow a reading that takes a measurable time and never shows the oracle's answer, its party ending at the
    /// fourth reading. Expect: each re-read waits at least as long as the read before it took, so reading holds at
    /// most half the time, at whatever a read costs on this platform (no picked interval); the reading the party's end
    /// left returned, the scenario's difference.
    #[test]
    fn a_reread_waits_as_long_as_the_last_read_took() {
        let reads = std::cell::RefCell::new(Vec::new());
        let want = Seen::Listed(true);
        let step = Step::Listed(0);
        let party = || {
            if reads.borrow().len() < 3 {
                Party::Live
            } else {
                Party::Ended
            }
        };
        let got = follow(
            &SimFixture::Process,
            (0, &step),
            &want,
            &party,
            Reads::Polled,
            |_| {
                let start = Instant::now();
                // A read that costs five times the fixed re-read interval the suite had (10 ms), so a fixed
                // interval fails even where the OS's timer oversleeps it (macOS under load did, at 20 ms):
                // the relation tested holds for any cost.
                let mut x = 0u64;
                while start.elapsed() < Duration::from_millis(50) {
                    x = std::hint::black_box(x.wrapping_add(1));
                }
                reads.borrow_mut().push((start, Instant::now()));
                Ok(Seen::Listed(false))
            },
        );
        assert_eq!(got, Ok(Seen::Listed(false)));
        let reads = reads.into_inner();
        assert_eq!(reads.len(), 4, "{} reads", reads.len());
        for pair in reads.windows(2) {
            let (s0, e0) = pair[0];
            let (s1, _) = pair[1];
            assert!(
                s1 - e0 >= e0 - s0,
                "re-read after {:?}, the read took {:?}",
                s1 - e0,
                e0 - s0
            );
        }
    }

    /// A program that is the simulator's, but ends on its own `after` its start (as a waiting program
    /// that is killed late, or a box that ends slowly under load, would look).
    struct EndsAfter {
        inner: Box<dyn Proc>,
        started: Instant,
        after: Duration,
    }

    impl Proc for EndsAfter {
        fn pid(&self) -> u32 {
            self.inner.pid()
        }
        fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
            match self.inner.try_wait()? {
                Some(e) => Ok(Some(e)),
                None if self.started.elapsed() >= self.after => Ok(Some(Exit::Signal(9))),
                None => Ok(None),
            }
        }
        fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
            if let Some(e) = self.try_wait()? {
                return Ok(Some(e));
            }
            let left = self.after.saturating_sub(self.started.elapsed());
            if timeout < left {
                conformance::rest(timeout).map_err(Refusal::Broken)?;
                return self.try_wait();
            }
            conformance::rest(left).map_err(Refusal::Broken)?;
            self.try_wait()
        }
        fn kill(&mut self) -> Result<(), Refusal> {
            self.inner.kill()
        }
        fn streams(&mut self) -> Result<crate::Given, Refusal> {
            self.inner.streams()
        }
    }

    /// The simulator, but every program it starts ends on its own 770 ms after its start.
    struct LateEnds(SimRuntime);

    impl Runtime for LateEnds {
        fn classes(&self) -> Vec<Class> {
            self.0.classes()
        }
        fn create(&self, spec: crate::BoxSpec) -> Result<BoxInfo, Refusal> {
            self.0.create(spec)
        }
        fn start(&self, id: &BoxId) -> Result<(), Refusal> {
            self.0.start(id)
        }
        fn exec(&self, id: &BoxId, p: Program, s: Stdio) -> Result<Box<dyn Proc>, Refusal> {
            Ok(Box::new(EndsAfter {
                inner: self.0.exec(id, p, s)?,
                started: Instant::now(),
                // The slowest end measured on adalundhe-macbook-pro at load 21 (2026-11-06): a real end
                // that late, well inside the slowest on record, must not read as still running.
                after: Duration::from_millis(770),
            }))
        }
        fn run(
            &self,
            spec: crate::BoxSpec,
            p: Program,
            s: Stdio,
        ) -> Result<Box<dyn Proc>, Refusal> {
            self.0.run(spec, p, s)
        }
        fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
            self.0.signal(id, signal)
        }
        fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
            self.0.inspect(id)
        }
        fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
            self.0.usage(id)
        }
        fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
            self.0.list()
        }
        fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
            self.0.delete(id)
        }
    }

    /// A program's liveness that a later step proves costs no watch (the lead, 2026-11-07): HEX-DIFF-2's
    /// programs, still running after a sibling exits, are ended by TERM (a program that had ended on its own
    /// would show Code(0), not Signal(15)), and HEX-DIFF-6's echo hands back what it is fed, so each checks
    /// liveness at the moment. Do: once an end of a second has been seen, so a watch lasts that long at least, run
    /// both against the simulator, timed. Expect: each passes in less than one [`conformance::still_running`] watch.
    #[test]
    fn a_liveness_a_later_step_proves_waits_out_no_watch() {
        conformance::ENDS.answered(Duration::from_secs(1));
        for id in ["HEX-DIFF-2", "HEX-DIFF-6"] {
            let s = SCENARIOS.iter().find(|s| s.id == id).unwrap();
            let t = Instant::now();
            differ(
                &SimRuntime::new(),
                &SimFixture::Process,
                &SimFixture::Process,
                s,
                &|| Party::Live,
            )
            .unwrap();
            let took = t.elapsed();
            assert!(
                conformance::still_running().is_some_and(|watch| took < watch),
                "{id} took {took:?}"
            );
        }
    }

    /// HEX-DIFF-1 (the negative watch is the slowest end seen, decision 9): do a runtime whose waiting programs end
    /// 770 ms after they start, a real end seen under load, once an end of 800 ms has been seen. Expect HEX-DIFF-1 to
    /// fail at its first `Running(1)`: a watch that says a program does not end lasts as long as the slowest end
    /// seen, so it sees that end. The double's answers can't change: its party has ended.
    #[test]
    fn a_program_that_ends_late_is_not_read_as_still_running() {
        conformance::ENDS.answered(Duration::from_millis(800));
        let s = SCENARIOS.iter().find(|s| s.id == "HEX-DIFF-1").unwrap();
        let why = differ(
            &LateEnds(SimRuntime::new()),
            &SimFixture::Process,
            &SimFixture::Process,
            s,
            &|| Party::Ended,
        )
        .unwrap_err();
        assert!(why.contains("(Running(1))"), "{why}");
    }

    /// Do a scenario whose box the runtime shows late, its runtime alive; expect it to pass, since a reading follows
    /// the runtime for the oracle's answer rather than taking the first.
    #[test]
    fn a_box_shown_late_passes() {
        struct Late {
            sim: SimRuntime,
            reads: std::sync::atomic::AtomicU32,
        }
        impl Runtime for Late {
            fn classes(&self) -> Vec<Class> {
                self.sim.classes()
            }
            fn create(&self, spec: crate::BoxSpec) -> Result<BoxInfo, Refusal> {
                self.sim.create(spec)
            }
            fn start(&self, id: &BoxId) -> Result<(), Refusal> {
                self.sim.start(id)
            }
            fn exec(&self, id: &BoxId, p: Program, s: Stdio) -> Result<Box<dyn Proc>, Refusal> {
                self.sim.exec(id, p, s)
            }
            fn run(
                &self,
                spec: crate::BoxSpec,
                p: Program,
                s: Stdio,
            ) -> Result<Box<dyn Proc>, Refusal> {
                self.sim.run(spec, p, s)
            }
            fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
                self.sim.signal(id, signal)
            }
            fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
                let mut info = self.sim.inspect(id)?;
                // The first three reads lag: they show the box as running.
                let n = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n < 3 {
                    info.state = BoxState::Running;
                }
                Ok(info)
            }
            fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
                self.sim.usage(id)
            }
            fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
                self.sim.list()
            }
            fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
                self.sim.delete(id)
            }
        }
        let late = Late {
            sim: SimRuntime::new(),
            reads: 0.into(),
        };
        let s = SCENARIOS.iter().find(|s| s.id == "HEX-DIFF-2").unwrap();
        differ(
            &late,
            &SimFixture::Process,
            &SimFixture::Process,
            s,
            &|| Party::Live,
        )
        .unwrap();
        // HEX-DIFF-2 reads the box twice; the second read waited out two lagging readings.
        assert_eq!(late.reads.load(std::sync::atomic::Ordering::SeqCst), 4);
    }
}
