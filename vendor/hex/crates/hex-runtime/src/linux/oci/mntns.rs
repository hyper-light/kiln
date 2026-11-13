//! How a pruned create's mount namespace is made (decision 6's update, `docs/research/runtime/goal6-mntns.md`):
//! the seed's pruned namespace copied by the init's clone (goal 5's way, every kernel), a namespace holding only
//! its root made by that clone (`CLONE_EMPTY_MNTNS`, Linux 7.1), or one the agent makes from the rootfs's tree
//! (`OPEN_TREE_NAMESPACE`, Linux 7.0) for the init to join. A node chooses one way at start: the one the record
//! shows fastest, where the kernel has it ([`default_way`]), or, for the bench's A/B and the lab, the one its builder
//! is asked for (`Node::with_mount_namespace`). The node agent reads no environment for it (Ada's 2026-11-06 rule).

use super::OciError;
pub use hex_core::seed::MountNamespace;
use rustix::io::Errno;

/// `open_tree`'s flag that clones the tree into a new mount namespace and returns that namespace
/// (`include/uapi/linux/mount.h:65`, Linux 7.0), which rustix doesn't name.
pub const OPEN_TREE_NAMESPACE: u32 = 1 << 1;
/// `fsmount`'s flag that makes the new mount a new mount namespace's tree (`include/uapi/linux/mount.h:113`,
/// Linux 7.1).
pub const FSMOUNT_NAMESPACE: u32 = 0x2;
/// `clone3`'s flag for a mount namespace holding only a clone of its parent's root mount
/// (`include/uapi/linux/sched.h:42`, Linux 7.1).
pub const CLONE_EMPTY_MNTNS: u64 = 1 << 37;

/// Whether this process may make a mount namespace from a tree, as [`probe`] finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenTreeNamespace {
    /// The kernel has the call and this process may make one.
    Available,
    /// The kernel has the call, but this process lacks `CAP_SYS_ADMIN` in its user namespace.
    NotPermitted,
    /// The kernel predates it (Linux 7.0).
    Absent,
}

/// What [`probe`]'s `open_tree` answering `result` means. The probe names a path beneath a pipe's
/// descriptor, which is no directory: a kernel without the flag refuses it at its flag check, `EINVAL`
/// (v6.19 `fs/namespace.c:3073-3076`); one with it checks the capability, `EPERM`, then fails the lookup,
/// `ENOTDIR` (`path_init`), before it makes anything (v7.1 `:3219-3232`). Any other answer is a kernel this
/// reading doesn't cover, refused by type.
pub fn classify(result: Result<(), Errno>) -> Result<OpenTreeNamespace, OciError> {
    match result {
        Err(Errno::NOTDIR) => Ok(OpenTreeNamespace::Available),
        Err(Errno::PERM) => Ok(OpenTreeNamespace::NotPermitted),
        Err(Errno::INVAL) => Ok(OpenTreeNamespace::Absent),
        Err(e) => Err(OciError::os(
            "probing open_tree(OPEN_TREE_NAMESPACE)",
            format!("an answer that means neither support nor its absence: {e}"),
        )),
        Ok(()) => Err(OciError::os(
            "probing open_tree(OPEN_TREE_NAMESPACE)",
            "a namespace made from no tree",
        )),
    }
}

/// Asks the kernel whether this process may make a mount namespace from a tree, making nothing and taking
/// no lock ([`classify`] says how).
pub fn probe() -> Result<OpenTreeNamespace, OciError> {
    let (not_a_dir, _w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
        .map_err(|e| OciError::os("probing open_tree(OPEN_TREE_NAMESPACE)", e))?;
    let flags = rustix::mount::OpenTreeFlags::from_bits_retain(
        OPEN_TREE_NAMESPACE | rustix::mount::OpenTreeFlags::OPEN_TREE_CLOEXEC.bits(),
    );
    classify(rustix::mount::open_tree(&not_a_dir, "probe", flags).map(drop))
}

/// The record the default follows (`bench/competitive/probes/mntns_default.txt`): each way's create p50 on a
/// 7.1+ bench host of record, in the order its reader ranked them, or the prediction until all three are measured.
pub const RECORD: &str = include_str!("../../../../../bench/competitive/probes/mntns_default.txt");

/// The ways in `record`'s order of preference: where all three are measured, the order of its lines, as the record's
/// reader ranked them (mntns_record.py, the lead's rule of 2026-11-09: by create p50 where a sign test on the rounds
/// below shows a difference at the record's 95%, else by part 19 p99 at the heaviest load, then create p99 there; the
/// p50 a line names doesn't rank it); else its prediction. A record that names a way twice, misses one, or says what
/// this reading doesn't cover is refused by type.
pub fn order(record: &str) -> Result<Vec<MountNamespace>, OciError> {
    let bad = |why: String| OciError::Config(format!("the mount-namespace record: {why}"));
    let way = |word: &str| match word {
        "copy" => Ok(MountNamespace::Copy),
        "empty" => Ok(MountNamespace::Empty),
        "tree" => Ok(MountNamespace::FromTree),
        other => Err(bad(format!("{other:?} names no way"))),
    };
    let mut measured: Vec<(MountNamespace, Option<f64>)> = Vec::new();
    let mut prediction = None;
    for line in record.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("prediction:") {
            let ways = rest
                .split_whitespace()
                .map(way)
                .collect::<Result<Vec<_>, _>>()?;
            prediction = Some(ways);
            continue;
        }
        let mut words = line.split_whitespace();
        let (Some(w), Some(p50)) = (words.next(), words.next()) else {
            return Err(bad(format!("{line:?} is no way and p50")));
        };
        let w = way(w)?;
        let p50 = match p50 {
            "unmeasured" => None,
            ms => Some(
                ms.parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .ok_or_else(|| bad(format!("{ms:?} is no p50 in ms")))?,
            ),
        };
        if measured.iter().any(|(m, _)| *m == w) {
            return Err(bad(format!("{w:?} is named twice")));
        }
        measured.push((w, p50));
    }
    let prediction = prediction.ok_or_else(|| bad("no prediction".to_owned()))?;
    if measured.len() != 3 {
        return Err(bad(format!("{} ways, not 3", measured.len())));
    }
    // All measured: the record's own order, its reader's ranking. Any unmeasured: the prediction.
    if measured.iter().any(|(_, p50)| p50.is_none()) {
        return Ok(prediction);
    }
    Ok(measured.into_iter().map(|(w, _)| w).collect())
}

/// The `clone3` namespace flags of an init whose mount namespace is made `way`, from the spec's `flags`, which hold
/// `CLONE_NEWNS` (the seed's namespace copied): from its tree, the init is cloned without it and joins the namespace
/// the agent made; empty, `CLONE_EMPTY_MNTNS` takes its place. What a fresh clone takes ([`super::seed`]'s
/// `clone_pruned`), and so what an init parked for that way is cloned with and a claim is held to (decision 8).
pub fn clone_flags(flags: u64, way: MountNamespace) -> u64 {
    let newns = u64::try_from(libc::CLONE_NEWNS).unwrap_or(0);
    match way {
        MountNamespace::FromTree => flags & !newns,
        MountNamespace::Empty => (flags & !newns) | CLONE_EMPTY_MNTNS,
        MountNamespace::Copy => flags,
    }
}

/// The way a node takes unless asked for another: the first in `record`'s order ([`order`]) that this node can
/// take. The copy it always can; from its tree where the probe finds the call; empty where the probe finds
/// Linux 7.0's call too, since a 7.0 kernel serves it with the copy after one refused clone a seed.
pub fn default_from(
    record: &str,
    open_tree: OpenTreeNamespace,
) -> Result<MountNamespace, OciError> {
    let takeable = |w: &MountNamespace| match w {
        MountNamespace::Copy => true,
        MountNamespace::FromTree | MountNamespace::Empty => {
            open_tree == OpenTreeNamespace::Available
        }
    };
    Ok(order(record)?
        .into_iter()
        .find(takeable)
        .unwrap_or(MountNamespace::Copy))
}

/// [`default_from`] the record built in ([`RECORD`]).
pub fn default_way(open_tree: OpenTreeNamespace) -> Result<MountNamespace, OciError> {
    default_from(RECORD, open_tree)
}

/// The way a node takes: `asked` (`copy`, `empty` or `tree`) where given, else [`default_way`]. A namespace
/// from its tree asked of a kernel or process that can't make one is refused by type. Empty asked of a
/// kernel without `CLONE_EMPTY_MNTNS` isn't: the seed learns that from its first clone, makes the copy and
/// says so in its answer, and the node counts the create as copied.
pub fn choose(
    open_tree: OpenTreeNamespace,
    asked: Option<&str>,
) -> Result<MountNamespace, OciError> {
    match asked {
        None => default_way(open_tree),
        Some("copy") => Ok(MountNamespace::Copy),
        Some("empty") => Ok(MountNamespace::Empty),
        Some("tree") if open_tree == OpenTreeNamespace::Available => Ok(MountNamespace::FromTree),
        Some("tree") => Err(OciError::Config(format!(
            "the way asked, tree, is a mount namespace made from its tree, which this node can't make \
             ({open_tree:?})"
        ))),
        Some(other) => Err(OciError::Config(format!(
            "the way asked, {other:?}, names no way to make a mount namespace: copy, empty or tree"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HEX-MNTNS-1: do the probe's `open_tree` and get each answer the kernel gives; expect ENOTDIR to
    /// mean the call is there, EPERM that this process may not, EINVAL that the kernel predates it, and
    /// any other answer refused by type rather than read as either.
    #[test]
    fn each_answer_to_the_probe_means_what_the_kernel_says() {
        assert_eq!(
            classify(Err(Errno::NOTDIR)).unwrap(),
            OpenTreeNamespace::Available
        );
        assert_eq!(
            classify(Err(Errno::PERM)).unwrap(),
            OpenTreeNamespace::NotPermitted
        );
        assert_eq!(
            classify(Err(Errno::INVAL)).unwrap(),
            OpenTreeNamespace::Absent
        );
        for other in [Errno::BADF, Errno::NOSYS, Errno::NOENT, Errno::NOMEM] {
            assert!(classify(Err(other)).is_err(), "{other:?} is refused");
        }
        assert!(classify(Ok(())).is_err());
    }

    /// HEX-MNTNS-9 ("the faster path is the default"; the lead's rule of 2026-11-09): read a record with all three
    /// ways measured, then one with any unmeasured. Expect the first the node can take in the record's own order as
    /// the default in the first, whatever the p50s on its lines (its reader ranked the ways: by create p50 where a
    /// sign test on the rounds below shows a difference at the record's 95%, else by part 19 p99, then create p99),
    /// and the prediction's first takeable in the second; a 6.19 node (no call) the copy either way.
    #[test]
    fn the_default_follows_its_record() {
        use OpenTreeNamespace::*;
        let measured = "# a run\ntree 1.20 run-a\ncopy 0.95 run-a\nempty 1.10 run-a\nprediction: tree copy empty\n";
        assert_eq!(
            order(measured).unwrap(),
            [
                MountNamespace::FromTree,
                MountNamespace::Copy,
                MountNamespace::Empty
            ]
        );
        assert_eq!(
            default_from(measured, Available).unwrap(),
            MountNamespace::FromTree
        );
        let empty_first = "empty 1.3 r\ncopy 0.9 r\ntree 1.2 r\nprediction: tree copy empty\n";
        assert_eq!(
            default_from(empty_first, Available).unwrap(),
            MountNamespace::Empty
        );
        assert_eq!(
            default_from(empty_first, Absent).unwrap(),
            MountNamespace::Copy
        );
        let predicted = "tree 1.2 r\ncopy unmeasured\nempty 0.9 r\nprediction: tree copy empty\n";
        assert_eq!(
            default_from(predicted, Available).unwrap(),
            MountNamespace::FromTree
        );
        assert_eq!(
            default_from(predicted, NotPermitted).unwrap(),
            MountNamespace::Copy
        );
    }

    /// HEX-MNTNS-9: hostile records. Expect each refused by type, never read as a default: a way named twice, one
    /// missing, an unknown word, a p50 that isn't a positive number of ms, no prediction.
    #[test]
    fn a_record_that_says_nothing_valid_is_refused() {
        for bad in [
            "tree 1 r\ntree 2 r\ncopy 1 r\nprediction: tree\n",
            "tree 1 r\ncopy 1 r\nprediction: tree copy\n",
            "tree 1 r\ncopy 1 r\nfast 1 r\nprediction: tree\n",
            "tree -1 r\ncopy 1 r\nempty 1 r\nprediction: tree\n",
            "tree NaN r\ncopy 1 r\nempty 1 r\nprediction: tree\n",
            "tree 1 r\ncopy 1 r\nempty 1 r\n",
            "tree\ncopy 1 r\nempty 1 r\nprediction: tree\n",
            "tree 1 r\ncopy 1 r\nempty 1 r\nprediction: tree sideways\n",
        ] {
            assert!(matches!(order(bad), Err(OciError::Config(_))), "{bad:?}");
        }
    }

    /// HEX-MNTNS-9, doc truth: read the record built in. Expect it valid and measured, no way unmeasured, and its
    /// default the first of its lines a node can take: the copy, by its tail, on a 7.x node and on 6.19 alike (goal
    /// 17's record, mntns-host-20261109T171050Z at 0c29b3ec: no way's create p50 below each other's at 95%, the copy's
    /// part 19 p99 the lowest at 12 loops).
    #[test]
    fn the_record_built_in_reads_and_names_todays_default() {
        assert!(
            !RECORD
                .lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .any(|l| l.contains("unmeasured")),
            "the record built in is measured: no way's line says unmeasured"
        );
        assert_eq!(
            default_way(OpenTreeNamespace::Available).unwrap(),
            MountNamespace::Copy
        );
        assert_eq!(
            default_way(OpenTreeNamespace::Absent).unwrap(),
            MountNamespace::Copy
        );
    }

    /// HEX-MNTNS-1: choose with and without a way asked, on each probe's answer; expect the record's default (the
    /// copy, goal 17's record) where nothing is asked, whatever the probe finds, each word taken where it can be
    /// served, the tree refused by type where it can't, and an unknown word refused.
    #[test]
    fn a_node_takes_the_way_asked_where_it_can_and_else_its_default() {
        use OpenTreeNamespace::*;
        assert_eq!(choose(Available, None).unwrap(), MountNamespace::Copy);
        assert_eq!(choose(NotPermitted, None).unwrap(), MountNamespace::Copy);
        assert_eq!(choose(Absent, None).unwrap(), MountNamespace::Copy);
        for probe in [Available, NotPermitted, Absent] {
            assert_eq!(choose(probe, Some("copy")).unwrap(), MountNamespace::Copy);
            assert_eq!(choose(probe, Some("empty")).unwrap(), MountNamespace::Empty);
            assert!(choose(probe, Some("Tree")).is_err());
            assert!(choose(probe, Some("")).is_err());
        }
        assert_eq!(
            choose(Available, Some("tree")).unwrap(),
            MountNamespace::FromTree
        );
        assert!(matches!(
            choose(Absent, Some("tree")),
            Err(OciError::Config(_))
        ));
        assert!(matches!(
            choose(NotPermitted, Some("tree")),
            Err(OciError::Config(_))
        ));
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    /// HEX-MNTNS-1: probe this kernel; expect an answer it classifies (never a refusal), and on a kernel
    /// below 7.0 (fedora's 6.19) that the call is absent, read from its release.
    #[test]
    fn the_probe_reads_this_kernel() {
        let found = probe().unwrap();
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap();
        let major: u32 = release.split('.').next().unwrap().parse().unwrap();
        if major < 7 {
            assert_eq!(found, OpenTreeNamespace::Absent, "{release}");
        }
        eprintln!("kernel {} probe {found:?}", release.trim());
    }
}
