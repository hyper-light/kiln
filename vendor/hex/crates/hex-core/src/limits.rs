//! The descriptor limit a box gets where its spec names none (native plan step 6): Docker 29's,
//! 1024:524288 (Ada, 2026-11-06), not the node's own, which the installed node raises for the files it
//! holds (docs/research/runtime/nofile-limit.md §3-§5). An unprivileged process can't raise its hard
//! limit (getrlimit(2)), so the default is capped at the node's.

/// Docker 29's soft limit for a container that names none: select(2)'s `FD_SETSIZE`.
pub const NOFILE_SOFT: u64 = 1024;
/// Docker 29's hard limit for a container that names none: systemd's default hard limit.
pub const NOFILE_HARD: u64 = 524_288;

/// A soft and a hard descriptor limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nofile {
    pub soft: u64,
    pub hard: u64,
}

/// The limit a box gets where its spec names no `RLIMIT_NOFILE`, under a node whose own hard limit is
/// `node_hard` (`None`: unlimited): [`NOFILE_SOFT`]:[`NOFILE_HARD`], each capped at `node_hard`.
#[must_use]
pub fn default_nofile(node_hard: Option<u64>) -> Nofile {
    let hard = node_hard.map_or(NOFILE_HARD, |h| h.min(NOFILE_HARD));
    Nofile {
        soft: NOFILE_SOFT.min(hard),
        hard,
    }
}

/// Whether a spec's `process.rlimits`, by their `type`s, names `RLIMIT_NOFILE`.
pub fn names_nofile<'a>(kinds: impl IntoIterator<Item = &'a str>) -> bool {
    kinds.into_iter().any(|k| k == "RLIMIT_NOFILE")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// HEX-INST-6 (native plan step 6): a box whose spec names no RLIMIT_NOFILE gets Docker 29's
    /// 1024:524288 under a node whose hard limit is at or past it (the installed node's 1,048,576, or
    /// unlimited), and the node's own hard limit where that is lower.
    #[test]
    fn a_box_naming_no_nofile_gets_dockers_default_capped_at_the_nodes() {
        let docker = Nofile {
            soft: 1024,
            hard: 524_288,
        };
        assert_eq!(default_nofile(Some(1_048_576)), docker);
        assert_eq!(default_nofile(Some(524_288)), docker);
        assert_eq!(default_nofile(None), docker);
        assert_eq!(
            default_nofile(Some(4096)),
            Nofile {
                soft: 1024,
                hard: 4096
            }
        );
        assert_eq!(
            default_nofile(Some(512)),
            Nofile {
                soft: 512,
                hard: 512
            }
        );
    }

    /// HEX-INST-6: only a spec naming RLIMIT_NOFILE keeps its own; any other limit leaves the default.
    #[test]
    fn only_a_named_nofile_replaces_the_default() {
        assert!(names_nofile(["RLIMIT_CORE", "RLIMIT_NOFILE"]));
        assert!(!names_nofile(["RLIMIT_CORE", "RLIMIT_NPROC"]));
        assert!(!names_nofile([]));
    }

    proptest! {
        /// HEX-INST-6 (model-based, CLAUDE.md §6): for any node hard limit, the default never asks to
        /// raise it, never exceeds Docker's, and its soft limit never exceeds its hard.
        #[test]
        fn the_default_is_never_above_the_node_or_docker(hard in proptest::option::of(0u64..u64::MAX)) {
            let d = default_nofile(hard);
            prop_assert!(d.soft <= d.hard);
            prop_assert!(d.soft <= NOFILE_SOFT && d.hard <= NOFILE_HARD);
            if let Some(h) = hard {
                prop_assert!(d.hard <= h);
                prop_assert_eq!(d.hard, h.min(NOFILE_HARD));
            } else {
                prop_assert_eq!(d.hard, NOFILE_HARD);
            }
        }
    }
}
