//! Recorded agent traces (joint plan step 14, item 3; `bench/charge/record_trace.py`): a real harness session's load as
//! `millis threads mib` steps, so many milliseconds of so many busy threads holding so many MiB, `#` comments and blank
//! lines aside, its header naming where its memory came from. hex-sim's runs replay them in place of the generator's
//! draws (`sim::Scenario::traces`), each VM playing one ([`Played`]), as hex-cli's `charge_fleet` replays them on
//! hardware.

use std::time::Duration;

use super::ChargeError;

/// One step of a recorded trace: `millis` milliseconds of `threads` busy threads holding `mib` MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub millis: u64,
    pub threads: u64,
    pub mib: u64,
}

/// Where a trace's memory came from, as its header names it (`# memory: ...`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Memory {
    /// Its cgroup's memory controller's count (`memory.current`): its page cache and kernel memory too.
    Cgroup,
    /// Its processes' resident sets (`procs-rss`): their own pages alone.
    ProcsRss,
}

/// A recorded trace: its steps, in order, and where its memory came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    pub memory: Memory,
    pub steps: Vec<Step>,
}

/// A recorded trace, from its text: each line that is neither blank nor a comment (from `#` on) a step of three
/// numbers; its memory's source the header's `# memory:` line, its cgroup's where it names none (the recorder's only
/// source before it named one). Refused, typed, for a line that isn't three numbers, a source it doesn't know, and a
/// trace with no step.
pub fn parse(text: &str) -> Result<Trace, ChargeError> {
    let mut memory = Memory::Cgroup;
    let mut steps = Vec::new();
    for line in text.lines() {
        let (data, comment) = line.split_once('#').unwrap_or((line, ""));
        let data = data.trim();
        if data.is_empty() {
            if let Some(source) = comment.trim().strip_prefix("memory:") {
                memory = match source.trim().split([',', ' ']).next() {
                    Some("cgroup") => Memory::Cgroup,
                    Some("procs-rss") => Memory::ProcsRss,
                    _ => {
                        return Err(ChargeError::Invalid {
                            what: "a recorded trace's memory",
                            why: "its source is neither cgroup nor procs-rss",
                        });
                    }
                };
            }
            continue;
        }
        let mut words = data.split_whitespace().map(str::parse::<u64>);
        let (Some(Ok(millis)), Some(Ok(threads)), Some(Ok(mib)), None) =
            (words.next(), words.next(), words.next(), words.next())
        else {
            return Err(ChargeError::Invalid {
                what: "a recorded trace's step",
                why: "a line isn't `millis threads mib`",
            });
        };
        steps.push(Step {
            millis,
            threads,
            mib,
        });
    }
    if steps.is_empty() {
        return Err(ChargeError::Invalid {
            what: "a recorded trace",
            why: "it has no step",
        });
    }
    Ok(Trace { memory, steps })
}

const NANOS_PER_MILLI: u128 = 1_000_000;
const MIB: u64 = 1 << 20;

/// A trace as one VM plays it, window by window (item 3): its steps, the one it is in and how far into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Played {
    steps: Vec<Step>,
    at: usize,
    into_ns: u128,
}

impl Played {
    /// `trace`, from its start.
    pub fn new(trace: &Trace) -> Self {
        Self {
            steps: trace.steps.clone(),
            at: 0,
            into_ns: 0,
        }
    }

    /// How many windows of `window` it lasts: its steps' time over the window, rounded up.
    pub fn windows(&self, window: Duration) -> u64 {
        let total = self.steps.iter().fold(0u128, |sum, step| {
            sum.saturating_add(u128::from(step.millis).saturating_mul(NANOS_PER_MILLI))
        });
        u64::try_from(total.div_ceil(window.as_nanos().max(1))).unwrap_or(u64::MAX)
    }

    /// What it asks over its next `window`: its busy threads' CPU over the window, in milli-CPU, rounded up, and the
    /// most any of the window's steps holds, in bytes; none once it has played. A window its trace ends within asks
    /// what the trace has left over the whole window.
    pub fn ask(&mut self, window: Duration) -> Option<(u64, u64)> {
        let span = window.as_nanos();
        let mut left = span;
        let mut cpu_ns = 0u128;
        let mut mib = 0u64;
        let mut played = false;
        while left > 0 {
            let Some(step) = self.steps.get(self.at) else {
                break;
            };
            played = true;
            let length = u128::from(step.millis).saturating_mul(NANOS_PER_MILLI);
            let take = left.min(length.saturating_sub(self.into_ns));
            cpu_ns = cpu_ns.saturating_add(take.saturating_mul(u128::from(step.threads)));
            mib = mib.max(step.mib);
            left = left.saturating_sub(take);
            self.into_ns = self.into_ns.saturating_add(take);
            if self.into_ns >= length {
                self.at = self.at.saturating_add(1);
                self.into_ns = 0;
            }
        }
        if !played {
            return None;
        }
        let milli = cpu_ns.saturating_mul(1_000).div_ceil(span.max(1));
        Some((
            u64::try_from(milli).unwrap_or(u64::MAX),
            mib.saturating_mul(MIB),
        ))
    }
}
