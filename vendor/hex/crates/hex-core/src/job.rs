//! The rule a Windows box is bounded by (docs/design/runtime.md, decision "each OS runs its own boxes
//! natively"): a box is a job object nested under the node's, and this says what its job holds for its
//! [`Bounds`] (a commit limit, an active-process limit, a CPU weight or a hard cap), refusing by name each
//! bound a job can't hold, and how a member's exit code reads once Hex has ended the job for a signal.
//! Sans-IO (CLAUDE.md §9): `hex_runtime::windows` sets the job and reads it back. Names, flags and ranges
//! are Windows' own (MicrosoftDocs/sdk-api 656e965; docs/research/runtime/windows-job-objects.md).

use std::num::NonZeroU32;

use hex_api::{Bounds, Exit, MIN_MEMORY, Refusal, Signal};

/// The platform a refusal here names.
pub const PLATFORM: &str = "windows";

/// The exit code Hex ends a box's job with for a signal (`TerminateJobObject`'s `uExitCode`), the
/// signal's number in its low byte: NTSTATUS-shaped, severity error with the customer bit (bit 29) set
/// and facility 0x048, so no status of Windows' own is one of these (MS-ERREF §2.3).
pub const TERMINATED: u32 = 0xE048_0000;

/// Windows' default CPU weight, the share a job gets when Hex's weight is cgroup's default 100
/// (`jobobject_cpu_rate_control_information.md:147`: "The default is 5").
pub const DEFAULT_WEIGHT: u32 = 5;
/// The CPU rate control's denominator: a hard cap is cycles per 10,000 of the whole machine's
/// (`jobobject_cpu_rate_control_information.md:132`).
pub const RATE_SCALE: u32 = 10_000;

/// What a box's job holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    /// The job's commit limit in bytes (`JobMemoryLimit` with `JOB_OBJECT_LIMIT_JOB_MEMORY`): the memory
    /// every process in the box may commit together. It is the box's bound itself (floored at
    /// [`MIN_MEMORY`]): Windows refuses whole a commit that would cross it, so the job never commits past
    /// it (bench/install/records/windows-jupiter-20261105.txt entry 22, experiment e20).
    pub memory: Option<u64>,
    /// The job's active-process limit (`ActiveProcessLimit` with `JOB_OBJECT_LIMIT_ACTIVE_PROCESS`).
    /// Windows counts processes, not their threads, which `pids.max` counts too on Linux.
    pub processes: Option<u32>,
    /// The job's CPU rate control.
    pub cpu: Option<Cpu>,
}

/// A job's CPU rate control (`JOBOBJECT_CPU_RATE_CONTROL_INFORMATION`): a weight or a hard cap, one at
/// a time, as the structure's union holds one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cpu {
    /// `JOB_OBJECT_CPU_RATE_CONTROL_WEIGHT_BASED`: 1 to 9, [`DEFAULT_WEIGHT`] the default share.
    Weight(u32),
    /// `JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP`: the cycles of the whole machine the job may use, per
    /// [`RATE_SCALE`].
    Cap(u32),
}

/// What a box's job holds for `bounds` on a machine of `processors` logical processors. A bound a job
/// can't hold is refused by name, never dropped (CLAUDE.md §3).
pub fn limits(bounds: &Bounds, processors: NonZeroU32) -> Result<Limits, Refusal> {
    bounds.check()?;
    let refuse = |what: &'static str, why: &str| {
        Err(Refusal::Platform {
            what,
            platform: PLATFORM,
            why: why.to_owned(),
        })
    };
    if bounds.memory_high.is_some() {
        return refuse(
            "memory_high",
            "a job object has a commit limit, which refuses past it, and no level to throttle at",
        );
    }
    if bounds.io_weight.is_some() {
        return refuse(
            "io_weight",
            "the process class holds no disk share on Windows",
        );
    }
    if bounds.io_latency_us.is_some() {
        return refuse(
            "io_latency_us",
            "the process class holds no disk latency target on Windows",
        );
    }
    if bounds.io_max.is_some() {
        return refuse(
            "io_max",
            "the process class holds no disk throughput cap on Windows",
        );
    }
    let cpu = match (bounds.cpu_weight, bounds.cpu_max_milli) {
        (Some(_), Some(_)) => {
            return refuse(
                "cpu_weight",
                "a job holds a CPU weight or a hard cap, not both, and cpu_max_milli is set too",
            );
        }
        (Some(w), None) => Some(Cpu::Weight(weight(w))),
        (None, Some(milli)) => Some(Cpu::Cap(cap(milli, processors)?)),
        (None, None) => None,
    };
    let memory = bounds.memory_max.map(|bound| bound.max(MIN_MEMORY));
    Ok(Limits {
        memory,
        processes: bounds.pids_max,
        cpu,
    })
}

/// Whether a box's job reached its memory bound: its peak (`PeakJobMemoryUsed`, the job's own accounting,
/// read synchronously) past its commit limit. Windows refuses whole a commit that would cross the limit
/// and counts its ask in the peak, so the job's commit never passes the limit and its peak does exactly
/// when a commit was refused at the bound (bench/install/records/windows-jupiter-20261105.txt entry 22, experiment e20). The job's memory
/// notification can't tell this: under load Windows sets it seconds late, and never once the member's
/// commit has fallen back (entry 21, experiment e18). The peak doesn't fall, so a box reads as having
/// reached its bound from then on: once, not once per refused commit, which the job doesn't count.
pub fn reached_bound(peak: u64, limits: &Limits) -> bool {
    limits.memory.is_some_and(|limit| peak > limit)
}

/// The most a box's job has committed, from its peak: the peak itself, but at most the commit limit,
/// since the peak counts a refused commit's whole ask and the job never commits past the limit
/// (bench/install/records/windows-jupiter-20261105.txt entry 22, experiment e20).
pub fn committed_peak(peak: u64, limits: &Limits) -> u64 {
    limits.memory.map_or(peak, |limit| peak.min(limit))
}

/// The hard cap for `milli` thousandths of a CPU on a machine of `processors`: cycles per
/// [`RATE_SCALE`] of the whole machine, as Docker's Windows runtime maps `--cpus`
/// (`moby/moby` daemon/oci_windows.go:438, `NanoCPUs / NumCPU / (1e9/10000)` = `cpus × 10000 / cpus`,
/// over `GetActiveProcessorCount` of every group, not one). Rounded down, so the cap never exceeds what
/// was asked (CLAUDE.md §3: Hex refuses a value it would have to round up, where Docker floors it to 1).
/// A bound that maps below 1 (finer than the machine's least cap) or above [`RATE_SCALE`] (more CPU
/// than the whole machine) is refused by name.
pub fn cap(milli: u32, processors: NonZeroU32) -> Result<u32, Refusal> {
    // milli / (1000 × processors) of the machine, per 10,000: milli × 10 / processors.
    let rate = u64::from(milli)
        .checked_mul(10)
        .and_then(|r| r.checked_div(u64::from(processors.get())))
        .unwrap_or(u64::MAX);
    if (1..=u64::from(RATE_SCALE)).contains(&rate) {
        // In 1..=RATE_SCALE, so it fits u32; the fallback is never reached.
        return Ok(u32::try_from(rate).unwrap_or(RATE_SCALE));
    }
    Err(Refusal::Platform {
        what: "cpu_max_milli",
        platform: PLATFORM,
        why: if rate < 1 {
            format!(
                "{milli} thousandths of a CPU is below the least hard cap of this machine's {} \
                 processors (1 per {RATE_SCALE} of them)",
                processors.get()
            )
        } else {
            format!(
                "{milli} thousandths of a CPU is more than this machine's {} processors, which a \
                 hard cap can't express",
                processors.get()
            )
        },
    })
}

/// The job weight for Hex's `cpu_weight` (cgroup's `cpu.weight`, 1 to 10,000, 100 the default), mapped
/// onto Windows' 1 to 9 ([`DEFAULT_WEIGHT`] 5 the default). The mapping is piecewise-linear and
/// monotonic, pinning the two defaults together (100 ↦ 5): a weight under 100 maps into 1..=5, one over
/// into 5..=9. The share each Windows weight gives is measured in bench/native/windows/RESULT.md.
fn weight(w: u16) -> u32 {
    let v = u64::from(w.clamp(1, 10_000));
    let mapped = if v <= 100 {
        // 1 ↦ 1, 100 ↦ 5: 1 + ⌊4 × (v − 1) / 99⌋.
        v.saturating_sub(1)
            .saturating_mul(4)
            .checked_div(99)
            .unwrap_or(0)
            .saturating_add(1)
    } else {
        // 100 ↦ 5, 10000 ↦ 9: 5 + ⌊4 × (v − 100) / 9900⌋.
        v.saturating_sub(100)
            .saturating_mul(4)
            .checked_div(9_900)
            .unwrap_or(0)
            .saturating_add(5)
    };
    u32::try_from(mapped.clamp(1, 9)).unwrap_or(DEFAULT_WEIGHT)
}

/// Whether Windows can send `signal` to a box: it has no signals, so a box takes KILL and TERM, which
/// both end its job (`TerminateJobObject`); any other is refused by name.
pub fn ends_job(signal: Signal) -> Result<(), Refusal> {
    if signal == Signal::KILL || signal == Signal::TERM {
        Ok(())
    } else {
        Err(Refusal::Invalid {
            what: "signal",
            why: format!(
                "Windows has no signal {}: a box takes KILL and TERM, which both end its job",
                signal.number()
            ),
        })
    }
}

/// The exit code Hex ends a box's job with for `signal`: [`TERMINATED`] with the signal's number.
pub fn terminated_code(signal: Signal) -> u32 {
    TERMINATED | u32::try_from(signal.number()).unwrap_or(0)
}

/// The signals Hex has ended a box's job for: a set of their numbers, 1 to 64.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sent(u64);

impl Sent {
    /// Notes that Hex ended the box's job for `signal`.
    pub fn add(&mut self, signal: Signal) {
        if let Some(bit) = bit(signal.number()) {
            self.0 |= bit;
        }
    }

    /// Whether Hex ended the box's job for signal number `n`.
    pub fn has(self, n: i32) -> bool {
        bit(n).is_some_and(|b| self.0 & b != 0)
    }
}

fn bit(n: i32) -> Option<u64> {
    let n = u32::try_from(n).ok().filter(|n| (1..=64).contains(n))?;
    1u64.checked_shl(n.checked_sub(1)?)
}

/// How a member's end reads from its exit code: the signal Hex ended its job for, when the code is that
/// signal's [`terminated_code`] and Hex sent it; else the code itself, its 32 bits as an `i32` (a crash's
/// NTSTATUS, `0xC0000005`, reads as its negative value).
pub fn exit(code: u32, sent: Sent) -> Exit {
    let n = i32::from(code.to_le_bytes()[0]);
    if code & !0xFF == TERMINATED && sent.has(n) {
        return Exit::Signal(n);
    }
    Exit::Code(i32::from_ne_bytes(code.to_ne_bytes()))
}

/// The command line for `args`: each argument quoted as the Microsoft C runtime parses `argv`
/// (`docs/research/runtime/windows-job-objects.md`, cpp-docs main-function-command-line-args.md), so a
/// program reads back exactly the arguments it was given. The caller has refused a NUL in any argument
/// (`Program::check`).
pub fn command_line(args: &[String]) -> String {
    let mut s = String::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        push_quoted(arg, &mut s);
    }
    s
}

/// Appends `arg` to a command line, quoted as the C runtime parses it: a run of backslashes is doubled
/// only before a quote or the closing quote, and an embedded quote is escaped with a backslash.
fn push_quoted(arg: &str, out: &mut String) {
    let bare = !arg.is_empty()
        && !arg
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\u{0b}' | '"'));
    if bare {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes: usize = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes = backslashes.saturating_add(1),
            '"' => {
                // The run of backslashes is literal (double it) and the quote is escaped (one more).
                for _ in 0..backslashes.saturating_mul(2).saturating_add(1) {
                    out.push('\\');
                }
                out.push('"');
                backslashes = 0;
            }
            other => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push(other);
            }
        }
    }
    // Trailing backslashes precede the closing quote, so they are doubled to stay literal.
    for _ in 0..backslashes.saturating_mul(2) {
        out.push('\\');
    }
    out.push('"');
}

/// The environment block for `env`, UTF-16: each `name=value` NUL-terminated, the strings sorted
/// case-insensitively (Windows reads a sorted block, `changing-environment-variables.md:15`), the whole
/// double-NUL-terminated. Nothing is inherited; an empty environment is an empty block (two NULs). The
/// caller has refused a NUL or an `=` in a name and a NUL in a value (`Program::check`).
pub fn environment(env: &[(String, String)]) -> Vec<u16> {
    let mut vars: Vec<&(String, String)> = env.iter().collect();
    vars.sort_by(|a, b| {
        a.0.to_uppercase()
            .cmp(&b.0.to_uppercase())
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in vars {
        block.extend(k.encode_utf16());
        block.push(u16::from(b'='));
        block.extend(v.encode_utf16());
        block.push(0);
    }
    // An empty block is still a valid double-NUL-terminated block.
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

#[cfg(test)]
mod tests {
    use hex_api::IoRates;
    use proptest::prelude::*;

    use super::*;

    const MIB: u64 = 1 << 20;

    fn cpus(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

    /// HEX-WIN-1: each bound a job holds becomes its job's limit: memory_max the commit limit, pids_max
    /// the active-process limit,
    /// cpu_max_milli a hard cap of the whole machine.
    #[test]
    fn each_bound_a_job_holds_becomes_its_limit() {
        let b = Bounds {
            memory_max: Some(256 * MIB),
            pids_max: Some(8),
            cpu_max_milli: Some(1500),
            ..Bounds::default()
        };
        assert_eq!(
            limits(&b, cpus(32)),
            Ok(Limits {
                memory: Some(256 * MIB),
                processes: Some(8),
                // 1.5 CPUs of 32 is 4.6875% of the machine: 468 per 10,000, rounded down.
                cpu: Some(Cpu::Cap(468)),
            })
        );
        assert_eq!(limits(&Bounds::default(), cpus(32)), Ok(Limits::default()));
    }

    /// HEX-WIN-2: the job's commit limit is the box's bound itself, since Windows refuses whole a commit
    /// that would cross it (bench record windows-jupiter-20261105.txt entry 22); a bound under the floor
    /// floors at [`MIN_MEMORY`]. Do: ask for the limits of bounds from the floor to 1 GiB. Expect: each
    /// commit limit equal to its bound.
    #[test]
    fn the_commit_limit_is_the_bound() {
        let at = |bound| {
            limits(
                &Bounds {
                    memory_max: Some(bound),
                    ..Bounds::default()
                },
                cpus(32),
            )
            .expect("limits")
            .memory
        };
        for bound in [MIN_MEMORY, 2 * MIB, 8 * MIB, 64 * MIB, 256 * MIB, 1 << 30] {
            assert_eq!(at(bound), Some(bound));
        }
    }

    /// HEX-RT-5 (native plan step 3): a box reached its memory bound exactly when its job's peak is past
    /// the job's commit limit (a refused commit's ask counted), and its committed peak is at most the
    /// limit; a box with no memory bound never reached one, and its committed peak is its peak. Do: judge
    /// peaks around a 256 MiB box's commit limit, and any peak with no bound. Expect: at or under the
    /// limit not reached, past it reached and committed at the limit; unbounded never reached, committed
    /// as read.
    #[test]
    fn a_box_reached_its_bound_when_its_peak_is_past_its_commit_limit() {
        let l = limits(
            &Bounds {
                memory_max: Some(256 * MIB),
                ..Bounds::default()
            },
            cpus(32),
        )
        .expect("limits");
        let limit = l.memory.expect("a commit limit");
        assert!(!reached_bound(16 * MIB, &l));
        assert!(!reached_bound(limit, &l));
        assert!(reached_bound(limit + 1, &l));
        assert_eq!(committed_peak(16 * MIB, &l), 16 * MIB);
        assert_eq!(committed_peak(limit + 64 * MIB, &l), limit);
        assert!(!reached_bound(u64::MAX, &Limits::default()));
        assert_eq!(committed_peak(u64::MAX, &Limits::default()), u64::MAX);
    }

    /// HEX-WIN-1: what a job can't hold is refused by name, never dropped (CLAUDE.md §3).
    #[test]
    fn what_a_job_cant_hold_is_refused_by_name() {
        let cases = [
            (
                Bounds {
                    memory_max: Some(256 * MIB),
                    memory_high: Some(128 * MIB),
                    ..Bounds::default()
                },
                "memory_high",
            ),
            (
                Bounds {
                    io_weight: Some(100),
                    ..Bounds::default()
                },
                "io_weight",
            ),
            (
                Bounds {
                    io_latency_us: Some(1000),
                    ..Bounds::default()
                },
                "io_latency_us",
            ),
            (
                Bounds {
                    io_max: Some(IoRates {
                        read_bps: Some(1),
                        ..IoRates::default()
                    }),
                    ..Bounds::default()
                },
                "io_max",
            ),
            (
                Bounds {
                    cpu_weight: Some(100),
                    cpu_max_milli: Some(1000),
                    ..Bounds::default()
                },
                "cpu_weight",
            ),
        ];
        for (b, name) in cases {
            match limits(&b, cpus(32)) {
                Err(Refusal::Platform {
                    what,
                    platform: "windows",
                    ..
                }) => assert_eq!(what, name),
                other => panic!("{b:?}: {other:?}"),
            }
        }
        // A bound out of the kernel's own range is refused as it is everywhere.
        let bad = Bounds {
            cpu_weight: Some(0),
            ..Bounds::default()
        };
        assert!(matches!(
            limits(&bad, cpus(32)),
            Err(Refusal::Invalid {
                what: "cpu_weight",
                ..
            })
        ));
    }

    /// HEX-WIN-1: a hard cap is never looser than asked (rounded down); the whole machine is the
    /// loosest cap; a bound finer than the machine's least cap, or more than the whole machine, is
    /// refused by name rather than rounded up or silently un-capped (CLAUDE.md §3).
    #[test]
    fn a_cap_is_never_looser_than_asked() {
        let at = |milli, n| {
            limits(
                &Bounds {
                    cpu_max_milli: Some(milli),
                    ..Bounds::default()
                },
                cpus(n),
            )
            .map(|l| l.cpu)
        };
        // 1.5 of 32 CPUs is 468.75 per 10,000, rounded down.
        assert_eq!(at(1_500, 32), Ok(Some(Cpu::Cap(468))));
        // The whole machine is a cap at RATE_SCALE (the loosest legal hard cap); milli just over it
        // still floors to the whole machine, not above it.
        assert_eq!(at(32_000, 32), Ok(Some(Cpu::Cap(10_000))));
        assert_eq!(at(32_003, 32), Ok(Some(Cpu::Cap(10_000))));
        assert_eq!(at(31_999, 32), Ok(Some(Cpu::Cap(9_999))));
        // More than the whole machine (the rate would exceed RATE_SCALE): refused, not un-capped.
        for over in [33_000u32, 64_000, u32::MAX] {
            assert!(
                matches!(
                    at(over, 32),
                    Err(Refusal::Platform {
                        what: "cpu_max_milli",
                        ..
                    })
                ),
                "{over} on 32 cpus"
            );
        }
        // Finer than the least cap of a large machine: refused.
        assert!(matches!(
            at(10, 4096),
            Err(Refusal::Platform {
                what: "cpu_max_milli",
                ..
            })
        ));
    }

    proptest! {
        /// HEX-WIN-1 (model-based, CLAUDE.md §6): for any cap and machine, the rate held is at most the
        /// share asked and within one rate step of it; below the least step, or above the whole machine,
        /// is refused.
        #[test]
        fn the_cap_matches_its_oracle(milli in 1u32..=u32::MAX, processors in 1u32..=4096) {
            let got = cap(milli, cpus(processors));
            // The oracle in exact integers: the share asked, in units of 1/(10,000 × 1,000) of a CPU.
            let asked = u128::from(milli) * 10_000;
            let step = u128::from(processors) * 1_000;
            let whole = u128::from(RATE_SCALE) * step;
            match got {
                Ok(rate) => {
                    let held = u128::from(rate) * step;
                    prop_assert!((1..=RATE_SCALE).contains(&rate));
                    prop_assert!(held <= asked);
                    prop_assert!(asked < held + step);
                }
                Err(_) => prop_assert!(asked < step || asked > whole),
            }
        }
    }

    /// HEX-WIN-1: the cgroup weight maps onto Windows' 1..=9, monotonic, with the defaults pinned
    /// (100 ↦ 5) and the ends at the ends (1 ↦ 1, 10000 ↦ 9).
    #[test]
    fn the_weight_maps_onto_windows_nine() {
        assert_eq!(weight(1), 1);
        assert_eq!(weight(100), DEFAULT_WEIGHT);
        assert_eq!(weight(10_000), 9);
        let mut last = 0;
        for v in 1u16..=10_000 {
            let w = weight(v);
            assert!((1..=9).contains(&w), "weight({v}) = {w}");
            assert!(w >= last, "weight not monotonic at {v}: {w} < {last}");
            last = w;
        }
    }

    /// HEX-WIN-3: Windows takes KILL and TERM, which end the job; any other signal is refused by name.
    #[test]
    fn a_box_takes_kill_and_term_only() {
        assert_eq!(ends_job(Signal::KILL), Ok(()));
        assert_eq!(ends_job(Signal::TERM), Ok(()));
        for s in [
            Signal::HUP,
            Signal::INT,
            Signal::QUIT,
            Signal::USR1,
            Signal::USR2,
        ] {
            assert!(matches!(
                ends_job(s),
                Err(Refusal::Invalid { what: "signal", .. })
            ));
        }
    }

    /// HEX-WIN-3: a member ended by Hex's own signal reads as that signal; the same code without Hex
    /// having sent it, and any other code, read as the code, bit for bit.
    #[test]
    fn a_member_ended_for_a_signal_reads_as_it() {
        let mut sent = Sent::default();
        assert_eq!(
            exit(terminated_code(Signal::TERM), sent),
            Exit::Code(i32::from_ne_bytes(0xE048_000Fu32.to_ne_bytes()))
        );
        sent.add(Signal::TERM);
        assert_eq!(exit(terminated_code(Signal::TERM), sent), Exit::Signal(15));
        assert_eq!(
            exit(terminated_code(Signal::KILL), sent),
            Exit::Code(i32::from_ne_bytes(0xE048_0009u32.to_ne_bytes()))
        );
        sent.add(Signal::KILL);
        assert_eq!(exit(terminated_code(Signal::KILL), sent), Exit::Signal(9));
        assert_eq!(exit(0, sent), Exit::Code(0));
        assert_eq!(exit(3, sent), Exit::Code(3));
        // An access violation's status reads as Windows' own tools print it, a negative i32.
        assert_eq!(exit(0xC000_0005, sent), Exit::Code(-1_073_741_819));
        // TERMINATED's own low byte, 0, is no signal.
        assert_eq!(
            exit(TERMINATED, sent),
            Exit::Code(i32::from_ne_bytes(TERMINATED.to_ne_bytes()))
        );
    }

    proptest! {
        /// HEX-WIN-3 (oracle): every signal 1 to 64 round-trips through its code once sent, and no other
        /// code reads as a signal.
        #[test]
        fn exit_codes_match_their_oracle(code in any::<u32>(), signals in proptest::collection::vec(1i64..=64, 0..8)) {
            let mut sent = Sent::default();
            for s in &signals {
                sent.add(Signal::new(*s).unwrap());
            }
            let n = i32::try_from(code & 0xFF).unwrap();
            let ours = code & 0xFFFF_FF00 == TERMINATED && signals.iter().any(|s| i64::from(n) == *s);
            let want = if ours { Exit::Signal(n) } else { Exit::Code(i32::from_ne_bytes(code.to_ne_bytes())) };
            prop_assert_eq!(exit(code, sent), want);
        }
    }

    /// The Microsoft C runtime's `argv` parse of a command line with no program-name special case
    /// (cpp-docs main-function-command-line-args.md, "Parsing C++ command-line arguments"): whitespace
    /// separates arguments outside quotes; `2n` backslashes before a quote are `n` backslashes and the
    /// quote toggles quoting; `2n+1` are `n` and a literal quote; other backslashes are literal.
    fn crt_argv(line: &str) -> Vec<String> {
        let chars: Vec<char> = line.chars().collect();
        let mut args = Vec::new();
        let mut i = 0;
        loop {
            while i < chars.len() && matches!(chars[i], ' ' | '\t') {
                i += 1;
            }
            if i >= chars.len() {
                return args;
            }
            let mut arg = String::new();
            let mut quoted = false;
            while i < chars.len() && (quoted || !matches!(chars[i], ' ' | '\t')) {
                let mut slashes = 0;
                while i < chars.len() && chars[i] == '\\' {
                    slashes += 1;
                    i += 1;
                }
                if i < chars.len() && chars[i] == '"' {
                    arg.extend(std::iter::repeat_n('\\', slashes / 2));
                    if slashes % 2 == 1 {
                        arg.push('"');
                    } else if quoted && i + 1 < chars.len() && chars[i + 1] == '"' {
                        // A doubled quote inside quotes is one literal quote (post-2008 CRT).
                        arg.push('"');
                        i += 1;
                    } else {
                        quoted = !quoted;
                    }
                    i += 1;
                } else {
                    arg.extend(std::iter::repeat_n('\\', slashes));
                    if i < chars.len() && (quoted || !matches!(chars[i], ' ' | '\t')) {
                        arg.push(chars[i]);
                        i += 1;
                    }
                }
            }
            args.push(arg);
        }
    }

    /// HEX-WIN-4: known cases quote as the C runtime reads them: a bare word stays bare, a space or a
    /// quote is quoted, backslashes are doubled only before a quote, and an empty argument is `""`.
    #[test]
    fn a_command_line_quotes_each_argument_as_the_c_runtime_reads_it() {
        let line =
            |args: &[&str]| command_line(&args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>());
        assert_eq!(line(&["hex", "say", "hi"]), "hex say hi");
        assert_eq!(line(&["a b", ""]), r#""a b" """#);
        assert_eq!(line(&[r#"say "hi""#]), r#""say \"hi\"""#);
        assert_eq!(line(&[r"C:\dir\"]), r"C:\dir\");
        assert_eq!(line(&[r"C:\my dir\"]), r#""C:\my dir\\""#);
    }

    /// HEX-WIN-4: a sorted, double-NUL-terminated environment block; an empty one is two NULs.
    #[test]
    fn an_environment_block_is_sorted_and_double_nul_terminated() {
        let env = vec![
            ("b".to_owned(), "2".to_owned()),
            ("A".to_owned(), "1".to_owned()),
        ];
        let want: Vec<u16> = "A=1\0b=2\0\0".encode_utf16().collect();
        assert_eq!(environment(&env), want);
        assert_eq!(environment(&[]), vec![0, 0]);
    }

    proptest! {
        /// HEX-WIN-4 (oracle, CLAUDE.md §6): any arguments without a NUL read back exactly through the C
        /// runtime's parse, so a program in a box sees the arguments it was given.
        #[test]
        fn a_command_line_round_trips_through_the_c_runtime(args in proptest::collection::vec("[a-z \\\\\"\t]{0,8}", 1..6)) {
            prop_assert_eq!(crt_argv(&command_line(&args)), args);
        }
    }
}
