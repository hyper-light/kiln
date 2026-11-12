//! A container's proc made ahead, its masks and read-only paths already on it (decision 6's update, part 2):
//! which specs one can serve, and the configuration it is made for. A proc shows one pid namespace, so it is
//! made for a pid namespace made ahead too, which the container's init is cloned into; kernel work is the
//! node's refill worker's, and a spec this can't serve keeps ordinary construction, counted by reason.
use super::{mount::Op, spec::Spec};
use hex_core::devfs::Unpooled;
use rustix::mount::MountFlags;

/// What a prepared proc is made for: its mount's data and flags, and the masked and read-only paths beneath
/// it, each relative to the proc's root, in the spec's order. Every byte of it is the spec's, so a key is bounded by
/// the spec's own bound (`spec::MAX_CONFIG_BYTES`), and the pool's four keys by four of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub data: String,
    pub flags: MountFlags,
    pub masked: Vec<String>,
    pub readonly: Vec<String>,
}

/// The one proc at `/proc` of a container with a pid namespace of its own, with no other mount at or beneath
/// `/proc`: the index of its mount in `plan` and its key. The masks and read-only paths beneath `/proc` are
/// made on the prepared proc, so they must name entries the kernel makes, never through `..`.
pub fn eligible(spec: &Spec, plan: &[Op]) -> Result<(u32, Key), Unpooled> {
    if !spec.makes_namespace("pid") {
        return Err(Unpooled::PidNamespace);
    }
    let mut procs = plan
        .iter()
        .enumerate()
        .filter(|(_, op)| beneath_proc(&op.destination).is_some());
    let (index, op) = procs.next().ok_or(Unpooled::Missing)?;
    if procs.next().is_some() {
        return Err(Unpooled::Beneath);
    }
    if beneath_proc(&op.destination).is_some_and(|rel| !rel.is_empty()) {
        return Err(Unpooled::Beneath);
    }
    if op.is_bind || op.fstype != "proc" {
        return Err(Unpooled::Kind);
    }
    let within = |paths: &[String]| -> Result<Vec<String>, Unpooled> {
        let mut rel = Vec::new();
        for path in paths {
            if parts(path).any(|p| p == "..") {
                return Err(Unpooled::Beneath);
            }
            match beneath_proc(path) {
                Some(r) if r.is_empty() => return Err(Unpooled::Beneath),
                Some(r) => rel.push(r),
                None => {}
            }
        }
        Ok(rel)
    };
    let key = Key {
        data: op.data.clone(),
        flags: op.flags,
        masked: within(&spec.linux.masked_paths)?,
        readonly: within(&spec.linux.readonly_paths)?,
    };
    Ok((u32::try_from(index).map_err(|_| Unpooled::Capacity)?, key))
}

fn parts(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|s| !s.is_empty() && *s != ".")
}

/// `path` relative to `/proc` (empty for `/proc` itself), joined by `/`, or `None` outside it.
pub fn beneath_proc(path: &str) -> Option<String> {
    let mut p = parts(path);
    if p.next() != Some("proc") {
        return None;
    }
    Some(p.collect::<Vec<_>>().join("/"))
}

#[cfg(test)]
mod tests {
    use super::super::mount;
    use super::*;

    const DOCKER_MASKED: &str = r#"["/proc/acpi","/proc/asound","/proc/interrupts","/proc/kcore","/proc/keys","/proc/latency_stats","/proc/sched_debug","/proc/scsi","/proc/timer_list","/proc/timer_stats","/sys/devices/virtual/powercap","/sys/firmware"]"#;
    const DOCKER_READONLY: &str =
        r#"["/proc/bus","/proc/fs","/proc/irq","/proc/sys","/proc/sysrq-trigger"]"#;

    fn spec(namespaces: &str, mounts: &str, masked: &str, readonly: &str) -> Spec {
        Spec::parse(
            format!(
                r#"{{"ociVersion":"1.0.2","root":{{"path":"rootfs"}},
        "mounts":[{mounts}],
        "linux":{{"namespaces":[{namespaces}],"maskedPaths":{masked},"readonlyPaths":{readonly}}}}}"#
            )
            .as_bytes(),
        )
        .unwrap()
    }

    const PROC: &str = r#"{"destination":"/proc","type":"proc","source":"proc","options":["nosuid","noexec","nodev"]}"#;
    const PID: &str = r#"{"type":"pid"},{"type":"mount"}"#;

    fn check(s: &Spec) -> Result<(u32, Key), Unpooled> {
        eligible(s, &mount::plan(&s.mounts).unwrap())
    }

    /// HEX-MNTNS-6 (decision 6's update, part 2): read Docker's default spec, its proc among other mounts.
    /// Expect the proc's index and flags, and of its masks and read-only paths those beneath `/proc`,
    /// relative to it and in order, `/sys`'s left to ordinary construction.
    #[test]
    fn dockers_paths_beneath_proc_go_on_the_prepared_proc() {
        let usr =
            r#"{"destination":"/usr","type":"bind","source":"/usr","options":["rbind","ro"]}"#;
        let s = spec(
            PID,
            &format!("{usr},{PROC}"),
            DOCKER_MASKED,
            DOCKER_READONLY,
        );
        let (index, key) = check(&s).unwrap();
        assert_eq!(index, 1);
        assert_eq!(
            key.flags,
            MountFlags::NOSUID | MountFlags::NOEXEC | MountFlags::NODEV
        );
        assert_eq!(
            key.masked,
            [
                "acpi",
                "asound",
                "interrupts",
                "kcore",
                "keys",
                "latency_stats",
                "sched_debug",
                "scsi",
                "timer_list",
                "timer_stats"
            ]
        );
        assert_eq!(key.readonly, ["bus", "fs", "irq", "sys", "sysrq-trigger"]);
        let other = spec(PID, PROC, "[]", r#"["/proc/sys"]"#);
        assert_ne!(check(&other).unwrap().1, key, "another set is another key");
    }

    /// HEX-MNTNS-6: specs a prepared proc can't stand for. Expect each counted under its reason: no pid
    /// namespace of its own (none, or one by path), no proc, two procs, a mount beneath `/proc`, a proc
    /// that's a bind or another filesystem, a mask of `/proc` itself or through `..`; and a key past the
    /// 4,096 bytes once picked for it served, bounded by its spec alone.
    #[test]
    fn specs_a_prepared_proc_cannot_serve_keep_ordinary_setup() {
        assert_eq!(
            check(&spec(r#"{"type":"mount"}"#, PROC, "[]", "[]")),
            Err(Unpooled::PidNamespace)
        );
        assert_eq!(
            check(&spec(
                r#"{"type":"pid","path":"/proc/1/ns/pid"},{"type":"mount"}"#,
                PROC,
                "[]",
                "[]"
            )),
            Err(Unpooled::PidNamespace)
        );
        assert_eq!(check(&spec(PID, "", "[]", "[]")), Err(Unpooled::Missing));
        assert_eq!(
            check(&spec(PID, &format!("{PROC},{PROC}"), "[]", "[]")),
            Err(Unpooled::Beneath)
        );
        let beneath =
            r#"{"destination":"/proc/sys/fs/binfmt_misc","type":"tmpfs","source":"tmpfs"}"#;
        assert_eq!(
            check(&spec(PID, &format!("{PROC},{beneath}"), "[]", "[]")),
            Err(Unpooled::Beneath)
        );
        let bound = r#"{"destination":"/proc","type":"bind","source":"/proc","options":["rbind"]}"#;
        assert_eq!(check(&spec(PID, bound, "[]", "[]")), Err(Unpooled::Kind));
        let tmp = r#"{"destination":"/proc","type":"tmpfs","source":"tmpfs"}"#;
        assert_eq!(check(&spec(PID, tmp, "[]", "[]")), Err(Unpooled::Kind));
        assert_eq!(
            check(&spec(PID, PROC, r#"["/proc"]"#, "[]")),
            Err(Unpooled::Beneath)
        );
        assert_eq!(
            check(&spec(PID, PROC, "[]", r#"["/proc/sys/../../etc"]"#)),
            Err(Unpooled::Beneath)
        );
        // A key is bounded by the spec it comes from (spec::MAX_CONFIG_BYTES), with no bound of its own: one
        // over the 4,096 bytes once picked for it is served from the pool.
        let long = format!(r#"["/proc/{}"]"#, "a".repeat(4 * 4096));
        assert_eq!(
            check(&spec(PID, PROC, &long, "[]")).map(|(_, k)| k.masked.len()),
            Ok(1)
        );
        assert_eq!(beneath_proc("//proc/./sys/"), Some("sys".to_owned()));
        assert_eq!(beneath_proc("/procfs/sys"), None);
    }

    /// HEX-MNTNS-6: as root, make a proc ahead for Docker's paths beneath `/proc`. Expect, on Linux 7.1 and
    /// later, a pid namespace and a proc tree whose masked file reads empty, whose masked directory lists
    /// nothing, and whose read-only path refuses a write; below 7.1, the pid namespace's refusal by type.
    /// Skipped, said, where not root (the root lab runs it).
    #[test]
    fn a_proc_is_made_ahead_with_its_masks_as_root() {
        if !rustix::process::geteuid().is_root() {
            eprintln!("SKIP: making a proc ahead needs root");
            return;
        }
        let s = spec(PID, PROC, DOCKER_MASKED, DOCKER_READONLY);
        let (_, key) = check(&s).unwrap();
        let sources = super::super::init::ProcSources::open().unwrap();
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap();
        let mut v = release
            .trim()
            .split(['.', '-'])
            .map(|p| p.parse::<u32>().unwrap_or(0));
        // 7.1 is the first kernel that opens a pid namespace with no process yet: below it
        // `pidns_for_children_get` refuses one (v6.19 kernel/pid_namespace.c:372-379), and v7.1 dropped the check
        // (:361-373), as init.rs's PID_NAMESPACE_AHEAD says.
        let at_least_7_1 = (v.next().unwrap(), v.next().unwrap()) >= (7, 1);
        let made = super::super::init::prepare_proc(&key, &sources);
        if !at_least_7_1 {
            // Below 7.1 the kernel won't open a pid namespace with no process yet: refused by type.
            assert!(
                matches!(
                    &made,
                    Err(crate::linux::oci::OciError::Os {
                        op: super::super::init::PID_NAMESPACE_AHEAD,
                        ..
                    })
                ),
                "kernel {}: {:?}",
                release.trim(),
                made.err()
            );
            return;
        }
        let made = made.unwrap_or_else(|e| panic!("making a proc ahead: {e}"));
        let at = |rel: &str, flags| {
            rustix::fs::openat(&made.tree, rel, flags, rustix::fs::Mode::empty())
        };
        let kcore = at("kcore", rustix::fs::OFlags::RDONLY).unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(
            rustix::io::read(&kcore, &mut buf).unwrap(),
            0,
            "kcore masked"
        );
        if let Ok(acpi) = at(
            "acpi",
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
        ) {
            let mut dir = rustix::fs::Dir::read_from(&acpi).unwrap();
            let names: Vec<_> = std::iter::from_fn(|| dir.read())
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n != "." && n != "..")
                .collect();
            assert!(names.is_empty(), "acpi masked: {names:?}");
        }
        assert_eq!(
            at("sysrq-trigger", rustix::fs::OFlags::WRONLY).err(),
            Some(rustix::io::Errno::ROFS),
            "sysrq-trigger read-only"
        );
        // No superblock per masked directory: every masked directory of two procs made ahead is the one
        // empty filesystem the maker keeps (crun binds one empty directory alike, linux.c:1181-1210), so
        // making a proc ahead makes proc's superblock alone.
        let second = super::super::init::prepare_proc(&key, &sources)
            .unwrap_or_else(|e| panic!("making a second proc ahead: {e}"));
        let mut devs = std::collections::BTreeSet::new();
        let mut found = 0;
        for tree in [&made.tree, &second.tree] {
            for rel in &key.masked {
                let Ok(fd) = rustix::fs::openat(
                    tree,
                    rel.as_str(),
                    rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY,
                    rustix::fs::Mode::empty(),
                ) else {
                    continue;
                };
                let st = rustix::fs::fstat(&fd).unwrap();
                devs.insert((st.st_dev, st.st_ino));
                found += 1;
            }
        }
        assert!(found >= 2, "masked directories to compare: {found}");
        assert_eq!(
            devs.len(),
            1,
            "one empty filesystem for every masked directory: {devs:?}"
        );
    }
}
