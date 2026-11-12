//! What Hex's installer sets up on each OS, and the refusal a node gives where its machine lacks it
//! (native plan step 6; docs/design/runtime.md, decision "the installer, per OS"). A class whose setup
//! is missing is refused by name, saying what is missing and that the installer sets it up; the
//! shells (`hex-runtime`'s per-OS modules) find the facts these decide on.

use std::collections::BTreeSet;
use std::path::Path;

use hex_api::Refusal;

use crate::cgroup::CONTROLLERS;

/// Linux's installer as Hex's repository holds it, and how it is run.
pub const LINUX_INSTALLER: &str = "sudo bash scripts/install/linux.sh install";
/// Windows' installer as Hex's repository holds it, and how it is run.
pub const WINDOWS_INSTALLER: &str =
    "scripts/install/windows.ps1 install, from a PowerShell run as administrator";

/// What a Linux node refuses without its delegation: the box root both its classes' boxes are cgroups
/// under, named with them.
pub const LINUX_CLASSES: &str = "the box root of the process and oci classes";

/// Why a Linux node's box root can't be made where it goes: the delegation the installer sets up is
/// missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Undelegated {
    /// The parent cgroup lacks these of [`CONTROLLERS`]; it has `available`.
    Controllers {
        missing: Vec<&'static str>,
        available: BTreeSet<String>,
    },
    /// The parent cgroup isn't this user's to write: the kernel said `why`.
    NotOurs(String),
    /// The parent cgroup holds processes of its own, so cgroup v2 can't hand its controllers down.
    Occupied(String),
}

/// The controllers of [`CONTROLLERS`] that `available` (a `cgroup.controllers`) lacks, in order.
#[must_use]
pub fn missing_controllers(available: &BTreeSet<String>) -> Vec<&'static str> {
    CONTROLLERS
        .iter()
        .copied()
        .filter(|c| !available.contains(*c))
        .collect()
}

/// `Ok` when `parent`'s `available` controllers hold every one a box uses, else the refusal of
/// [`LINUX_CLASSES`] naming the missing ones.
pub fn check_controllers(parent: &Path, available: &BTreeSet<String>) -> Result<(), Refusal> {
    let missing = missing_controllers(available);
    if missing.is_empty() {
        return Ok(());
    }
    Err(undelegated(
        parent,
        Undelegated::Controllers {
            missing,
            available: available.clone(),
        },
    ))
}

/// The refusal of [`LINUX_CLASSES`] for a box root under `parent` that `why` keeps from being made.
#[must_use]
pub fn undelegated(parent: &Path, why: Undelegated) -> Refusal {
    let parent = parent.display();
    let what = match why {
        Undelegated::Controllers { missing, available } => {
            let have: Vec<&str> = available.iter().map(String::as_str).collect();
            format!(
                "the {} controller{} {} delegated to {parent} (it has {})",
                missing.join(", "),
                if missing.len() == 1 { "" } else { "s" },
                if missing.len() == 1 {
                    "isn't"
                } else {
                    "aren't"
                },
                if have.is_empty() {
                    "none".to_owned()
                } else {
                    have.join(" ")
                },
            )
        }
        Undelegated::NotOurs(e) => {
            format!("{parent} isn't delegated to the node agent's user ({e})")
        }
        Undelegated::Occupied(e) => format!(
            "{parent} holds processes of its own, so cgroup v2 can't hand its controllers to the box root ({e}); the installed hex-node service runs the node in a leaf beside its boxes"
        ),
    };
    Refusal::Platform {
        what: LINUX_CLASSES,
        platform: "linux",
        why: format!("{what}; Hex's installer sets the delegation up: {LINUX_INSTALLER}"),
    }
}

/// A Windows optional feature Hex's installer turns on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WindowsFeature {
    /// Windows containers through the Host Compute Service: the `oci` class's Windows images.
    Containers,
    /// The Windows Hypervisor Platform: the Shards guest that carries Linux images.
    HypervisorPlatform,
}

impl WindowsFeature {
    /// Every feature the installer turns on.
    pub const ALL: [Self; 2] = [Self::Containers, Self::HypervisorPlatform];

    /// The feature's name as DISM and `Enable-WindowsOptionalFeature` take it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Containers => "Containers",
            Self::HypervisorPlatform => "HypervisorPlatform",
        }
    }
}

/// A listing line's bytes past its feature's name: a space, the one-digit `InstallState` (1-4,
/// Win32_OptionalFeature's) and the CRLF PowerShell ends each line with.
const LINE_TAIL: usize = " 1\r\n".len();

/// Most bytes of a feature listing [`features_off`] reads: its format's own size, a line for each feature
/// it asks for, so a listing longer than Windows writes is refused (the 36 bytes jupiter writes, measured).
pub const FEATURE_LISTING_MAX: usize = listing_size();

const fn listing_size() -> usize {
    // Every feature of `ALL`, by its array's shape: a feature added there must be added here. The sums
    // saturate: a size past usize::MAX (never, for two names) would bound nothing tighter anyway.
    let [a, b] = WindowsFeature::ALL;
    line_size(a).saturating_add(line_size(b))
}

/// One feature's listing line: its name, then [`LINE_TAIL`].
const fn line_size(f: WindowsFeature) -> usize {
    f.name().len().saturating_add(LINE_TAIL)
}

/// The slowest recorded feature read (the PowerShell predecessor of [`FEATURE_QUERY`]), over both of
/// jupiter's records (2026-11-06, 50 runs):
/// 2,421 ms, of the Windows agent's 30 (bench/install/records/windows-jupiter-20261105.txt entry 19 on
/// cp/t7-native-windows: min 1,048, p50 1,183, p90 1,398 ms); step 6's 20 peaked at 1,683 ms at 79% CPU
/// (bench/install/records/windows-feature-query-jupiter-20261106.txt). Measured, not picked: a slower
/// recorded run raises it.
pub const FEATURE_QUERY_SLOWEST: std::time::Duration = std::time::Duration::from_millis(2_421);

/// The longest the node waits for [`FEATURE_QUERY`] before refusing it by name: the swarm's
/// failure-bound rule, twice the slowest recorded run, the factor being the false-alarm margin (the Hex
/// lead, 2026-11-06). A slower machine is a new record, not a longer guess.
pub const FEATURE_QUERY_PATIENCE: std::time::Duration = FEATURE_QUERY_SLOWEST.saturating_mul(2);

/// The query an unprivileged node makes of WMI, in process, for the installer's features' state (WQL on
/// `ROOT\CIMV2`'s `Win32_OptionalFeature`): each feature's `Name` and `InstallState`.
pub const FEATURE_QUERY: &str = "SELECT Name, InstallState FROM Win32_OptionalFeature WHERE Name = 'Containers' OR Name = 'HypervisorPlatform'";

/// The features of [`WindowsFeature::ALL`] that are off, from [`FEATURE_QUERY`]'s output: a feature is
/// on only when listed with `InstallState` 1 (Enabled; 2 Disabled, 3 Absent, 4 Unknown); one Windows
/// doesn't list is off. Anything else in the listing (an unknown name or state, a feature listed
/// twice, more than [`FEATURE_LISTING_MAX`] bytes) is refused, never guessed at.
pub fn features_off(listing: &str) -> Result<BTreeSet<WindowsFeature>, Refusal> {
    let bad = |why: String| Refusal::Invalid {
        what: "the Windows feature listing",
        why,
    };
    if listing.len() > FEATURE_LISTING_MAX {
        return Err(bad(format!(
            "{} bytes, past its bound of {FEATURE_LISTING_MAX}",
            listing.len()
        )));
    }
    let mut on = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for line in listing.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (name, state) = line
            .split_once(' ')
            .ok_or_else(|| bad(format!("{line:?} isn't \"<name> <state>\"")))?;
        let feature = WindowsFeature::ALL
            .into_iter()
            .find(|f| f.name() == name)
            .ok_or_else(|| bad(format!("{name:?} isn't a feature it asked for")))?;
        if !seen.insert(feature) {
            return Err(bad(format!("{name} is listed twice")));
        }
        match state.trim() {
            "1" => {
                on.insert(feature);
            }
            "2" | "3" | "4" => {}
            other => return Err(bad(format!("{name}'s state {other:?} isn't 1-4"))),
        }
    }
    Ok(WindowsFeature::ALL
        .into_iter()
        .filter(|f| !on.contains(f))
        .collect())
}

/// The features' state as `hex setup features` prints it for the Windows installer: one `<name> on|off`
/// line per feature of [`WindowsFeature::ALL`], in its order, `off` for each in `off`.
#[must_use]
pub fn render_features(off: &BTreeSet<WindowsFeature>) -> String {
    WindowsFeature::ALL
        .iter()
        .map(|f| {
            format!(
                "{} {}\n",
                f.name(),
                if off.contains(f) { "off" } else { "on" }
            )
        })
        .collect()
}

/// The Windows `oci` class's refusal: the features it needs that are `off`, by name, and that the
/// installer turns them on; with none off, that the class itself is still to build (`not_built`).
#[must_use]
pub fn windows_oci(off: &BTreeSet<WindowsFeature>, not_built: &str) -> Refusal {
    let why = if off.is_empty() {
        not_built.to_owned()
    } else {
        let names: Vec<&str> = off.iter().map(|f| f.name()).collect();
        format!(
            "the Windows feature{} it needs {} off: {}; Hex's installer turns {} on ({WINDOWS_INSTALLER}; Windows may ask for one reboot)",
            if names.len() == 1 { "" } else { "s" },
            if names.len() == 1 { "is" } else { "are" },
            names.join(", "),
            if names.len() == 1 { "it" } else { "them" },
        )
    };
    Refusal::Platform {
        what: "the oci class",
        platform: "windows",
        why,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Every controller fedora's kernel (6.19.10) lists at its cgroup v2 root, as the installer lab's record
    /// reads it (bench/install/records/linux-fb2e5ca-20261106T054108Z.txt, "/: controllers [...]"): the
    /// names a parent's `cgroup.controllers` can hold, the model's whole input space.
    const KERNEL_CONTROLLERS: [&str; 9] = [
        "cpuset", "cpu", "io", "memory", "hugetlb", "pids", "rdma", "misc", "dmem",
    ];

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    fn why(r: &Refusal) -> &str {
        match r {
            Refusal::Platform { why, .. } => why,
            other => panic!("not a platform refusal: {other:?}"),
        }
    }

    /// HEX-INST-1 (native plan step 6): a box root whose parent has every controller a box uses opens;
    /// one lacking the io controller is refused by name for both Linux classes, saying what's missing,
    /// what the parent has, and that the installer sets it up.
    #[test]
    fn a_parent_lacking_a_controller_refuses_the_classes_by_name() {
        let parent =
            Path::new("/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice");
        assert_eq!(
            check_controllers(parent, &set(&["cpuset", "cpu", "io", "memory", "pids"])),
            Ok(())
        );
        let refused = check_controllers(parent, &set(&["cpu", "memory", "pids"])).unwrap_err();
        let Refusal::Platform { what, platform, .. } = &refused else {
            panic!("{refused:?}")
        };
        assert_eq!((*what, *platform), (LINUX_CLASSES, "linux"));
        let text = why(&refused);
        assert!(text.starts_with(&format!(
            "the io controller isn't delegated to {} (it has cpu memory pids)",
            parent.display()
        )));
        assert!(text.ends_with(LINUX_INSTALLER), "{text}");
        let none = check_controllers(parent, &BTreeSet::new()).unwrap_err();
        assert!(why(&none).starts_with(&format!(
            "the cpu, io, memory, pids controllers aren't delegated to {} (it has none)",
            parent.display()
        )));
    }

    /// HEX-INST-1: a parent that isn't the node's user's, or that holds processes, is refused by name,
    /// naming the parent, the kernel's word, and the installer.
    #[test]
    fn an_undelegated_or_occupied_parent_refuses_the_classes_by_name() {
        let parent = Path::new("/sys/fs/cgroup/user.slice");
        let not_ours = undelegated(parent, Undelegated::NotOurs("Permission denied".to_owned()));
        assert_eq!(
            why(&not_ours),
            format!(
                "/sys/fs/cgroup/user.slice isn't delegated to the node agent's user (Permission denied); Hex's installer sets the delegation up: {LINUX_INSTALLER}"
            )
        );
        let occupied = undelegated(
            parent,
            Undelegated::Occupied("Device or resource busy".to_owned()),
        );
        assert!(why(&occupied).starts_with(
            "/sys/fs/cgroup/user.slice holds processes of its own, so cgroup v2 can't hand its controllers to the box root (Device or resource busy)"
        ));
        assert!(why(&occupied).ends_with(LINUX_INSTALLER));
    }

    proptest! {
        /// HEX-INST-1 (model-based, CLAUDE.md §6): for any set of controllers a parent has, it is
        /// refused exactly when one of [`CONTROLLERS`] is missing, and the refusal names exactly the
        /// missing ones (an oracle over the set), never one it has.
        #[test]
        fn the_refusal_names_exactly_the_missing_controllers(
            have in proptest::collection::btree_set(
                proptest::sample::select(&KERNEL_CONTROLLERS[..]),
                0..=KERNEL_CONTROLLERS.len(),
            )
        ) {
            let available: BTreeSet<String> = have.iter().map(|s| (*s).to_owned()).collect();
            let oracle: Vec<&str> = ["cpu", "io", "memory", "pids"]
                .into_iter()
                .filter(|c| !have.contains(c))
                .collect();
            match check_controllers(Path::new("/p"), &available) {
                Ok(()) => prop_assert!(oracle.is_empty()),
                Err(r) => {
                    prop_assert!(!oracle.is_empty());
                    let named = why(&r)
                        .strip_prefix("the ")
                        .and_then(|w| w.split(" controller").next())
                        .unwrap()
                        .to_owned();
                    prop_assert_eq!(named, oracle.join(", "));
                }
            }
        }
    }

    /// HEX-INST-2: the feature listing reads as Windows writes it: a feature is on only at state 1, and
    /// one Windows doesn't list is off.
    #[test]
    fn a_feature_is_on_only_when_listed_enabled() {
        let both: BTreeSet<WindowsFeature> = WindowsFeature::ALL.into_iter().collect();
        assert_eq!(
            features_off("Containers 1\r\nHypervisorPlatform 1\r\n"),
            Ok(BTreeSet::new())
        );
        assert_eq!(
            features_off("Containers 2\nHypervisorPlatform 1\n"),
            Ok([WindowsFeature::Containers].into())
        );
        assert_eq!(features_off("HypervisorPlatform 3\n"), Ok(both.clone()));
        assert_eq!(features_off(""), Ok(both));
        for hostile in [
            "Containers\n",
            "Containers 5\n",
            "Containers 1\nContainers 1\n",
            "Microsoft-Hyper-V 1\n",
            "Containers one\n",
        ] {
            assert!(
                matches!(features_off(hostile), Err(Refusal::Invalid { .. })),
                "{hostile:?}"
            );
        }
        assert!(features_off(&"x".repeat(FEATURE_LISTING_MAX + 1)).is_err());
    }

    /// HEX-INST-2: the feature query's deadline is the swarm's failure-bound rule over its records: twice
    /// the slowest recorded run, the factor the false-alarm margin. Of jupiter's 50 runs (20 at 79% CPU,
    /// bench/install/records/windows-feature-query-jupiter-20261106.txt; 30 by the Windows agent at
    /// 14:27Z, cp/t7-native-windows 01217f8 bench/install/windows-constants.md:20) the slowest is 2,421 ms.
    #[test]
    fn the_feature_querys_deadline_is_twice_its_slowest_recorded_run() {
        assert_eq!(
            FEATURE_QUERY_SLOWEST,
            std::time::Duration::from_millis(2_421)
        );
        assert_eq!(
            FEATURE_QUERY_PATIENCE,
            std::time::Duration::from_millis(4_842)
        );
    }

    /// HEX-INST-9 (native plan step 6): `hex setup features` prints each feature the installer turns on,
    /// one `<name> on|off` line in [`WindowsFeature::ALL`]'s order, which the Windows installer reads in
    /// place of a query of its own.
    #[test]
    fn the_features_state_renders_one_line_each_in_order() {
        let off: BTreeSet<WindowsFeature> = [WindowsFeature::HypervisorPlatform].into();
        assert_eq!(
            render_features(&off),
            "Containers on\nHypervisorPlatform off\n"
        );
        assert_eq!(
            render_features(&BTreeSet::new()),
            "Containers on\nHypervisorPlatform on\n"
        );
    }

    /// HEX-INST-2: the listing's bound is its format's own size, every feature's `<name> <state>\r\n`
    /// line: the 36 bytes jupiter's PowerShell wrote, 20 runs of 20 (2026-11-06, Windows 11 26200), fit
    /// exactly, and a byte past them is refused rather than read.
    #[test]
    fn the_listing_bound_is_the_formats_own_size() {
        let jupiter = "HypervisorPlatform 1\r\nContainers 1\r\n";
        assert_eq!(jupiter.len(), 36);
        assert_eq!(FEATURE_LISTING_MAX, jupiter.len());
        assert_eq!(features_off(jupiter), Ok(BTreeSet::new()));
        assert!(features_off(&format!("{jupiter} ")).is_err());
    }

    proptest! {
        /// HEX-INST-2, hostile input to the parser (CLAUDE.md §6): any text up to one byte past the bound
        /// (so both sides of it are reached), never a panic; what it accepts names only features it asked for.
        #[test]
        fn the_feature_listing_parser_never_panics(text in proptest::string::string_regex(
            &format!("(?s).{{0,{}}}", FEATURE_LISTING_MAX + 1)
        ).unwrap()) {
            if let Ok(off) = features_off(&text) {
                prop_assert!(off.iter().all(|f| WindowsFeature::ALL.contains(f)));
            }
        }
    }

    /// HEX-INST-2 (native plan step 6): the Windows oci class names each feature that's off and says
    /// the installer turns it on; with both on, it says only what the class still waits on.
    #[test]
    fn the_windows_oci_class_names_each_feature_that_is_off() {
        let not_built = "a Shards guest carries Linux images (native plan step 5)";
        let both: BTreeSet<WindowsFeature> = WindowsFeature::ALL.into_iter().collect();
        assert_eq!(
            why(&windows_oci(&both, not_built)),
            format!(
                "the Windows features it needs are off: Containers, HypervisorPlatform; Hex's installer turns them on ({WINDOWS_INSTALLER}; Windows may ask for one reboot)"
            )
        );
        let one: BTreeSet<WindowsFeature> = [WindowsFeature::HypervisorPlatform].into();
        assert!(why(&windows_oci(&one, not_built)).starts_with(
            "the Windows feature it needs is off: HypervisorPlatform; Hex's installer turns it on"
        ));
        let on = windows_oci(&BTreeSet::new(), not_built);
        assert_eq!(
            on,
            Refusal::Platform {
                what: "the oci class",
                platform: "windows",
                why: not_built.to_owned(),
            }
        );
    }
}
