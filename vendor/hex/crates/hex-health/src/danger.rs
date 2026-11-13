//! Dangerous workloads (S8): which runtime security signals trip. A workload that trips is stopped
//! where it is; where it may start again is the scheduler's, by the kinds Hex places (OCI containers,
//! Firecracker and Shards microVMs: `hex_sim::danger`), and never a deprioritisation.
//!
//! What trips: a call the workload's seccomp profile kills for (`SECCOMP_RET_KILL_PROCESS` or
//! `_THREAD`, which seccomp(2) reports to the audit log), an egress attempt no grant names (S3,
//! `AC-T7.7.2`), and an attempt to gain privilege (a capability or set-id change the box refused). A
//! call failed with an errno (`SECCOMP_RET_ERRNO`) does not: profiles fail calls they don't support that
//! way and well-behaved programs fall back, so it says nothing about intent.

/// A runtime security signal from a workload's box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// The seccomp profile killed it for a call.
    SeccompKill,
    /// The seccomp profile failed a call with an errno.
    SeccompErrno,
    /// It tried to reach a destination no grant names.
    EgressRefused,
    /// It tried to gain a capability or change identity, and was refused.
    PrivilegeEscalation,
}

/// Whether `signal` trips: the workload is stopped and isolated.
pub fn trips(signal: Signal) -> bool {
    signal != Signal::SeccompErrno
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S8 dangerous: a seccomp kill, ungranted egress and a refused privilege gain each trip.
    #[test]
    fn kills_egress_and_privilege_gains_trip() {
        for s in [
            Signal::SeccompKill,
            Signal::EgressRefused,
            Signal::PrivilegeEscalation,
        ] {
            assert!(trips(s), "{s:?}");
        }
    }

    /// An errno'd call is not a trip.
    #[test]
    fn an_errno_is_not_a_trip() {
        assert!(!trips(Signal::SeccompErrno));
    }
}
