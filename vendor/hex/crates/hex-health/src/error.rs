//! Every way the health core refuses, typed (CLAUDE.md §3).

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HealthError {
    #[error("the configuration is refused: {0}")]
    Config(&'static str),
    #[error("{what} is full at its bound of {bound}")]
    Full { what: &'static str, bound: usize },
    #[error("time went backwards in {what}: {now_ns} ns is before {last_ns} ns")]
    TimeBackwards {
        what: &'static str,
        now_ns: u64,
        last_ns: u64,
    },
    #[error("arithmetic overflowed in {0}")]
    Overflow(&'static str),
    #[error("a disk probe is already outstanding")]
    ProbeOutstanding,
    #[error("no disk probe is outstanding")]
    NoProbe,
}
