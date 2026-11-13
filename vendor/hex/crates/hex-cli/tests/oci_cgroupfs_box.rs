//! Goal 5's extension (AC-T7.1.1, T-T7.1.4): a rootless `oci` box on the host's network must not be
//! able to change another box's cgroup, nor read the host cgroup tree. Such a box binds the host's
//! `/sys` (it can't mount a sysfs of its own in the host's network namespace), which carries the
//! host's cgroup filesystem beneath the box's own (crates/hex-core/src/bundle.rs, `default_mounts`). A
//! box holding `CAP_SYS_ADMIN` could unmount its own cgroup mounts off and reach that tree.
//!
//! "do X, expect Y" (CLAUDE.md §6), on a rootless node:
//!   - create a box from the bundle the node renders for a rootless host-network box, with
//!     `CAP_SYS_ADMIN` added to its capability sets: expect the create refused by name (the spec's
//!     check refuses a container in a user namespace holding `CAP_SYS_ADMIN` without a network
//!     namespace of its own, on Ada's call, 2026-10-31);
//!   - in a plain rootless host-network box, read its mount table: expect the host cgroup filesystem
//!     its `/sys` bind carries, beneath its own, mounted read-only (the bind's remount makes the
//!     carried cgroup filesystems read-only, which a recursive bind's read-only otherwise leaves to
//!     its top), so a box that ever reached it couldn't change it; and have it unmount its own cgroup
//!     filesystem off that: expect the unmount refused, its `/sys/fs/cgroup` still its own;
//!   - goal 5 extended step 6's probe: beside a victim box, a rootless box holding `CAP_SYS_ADMIN` in a
//!     user namespace and a network namespace of its own (allowed) unmounts its cgroup filesystem and
//!     mounts a fresh cgroup2 one: expect the unmount and the mount to leave it its own cgroup alone,
//!     the victim's cgroup reachable from neither, so no box can change another's before a cgroup is
//!     handed from box to box.
//!
//! Rootless where the caller isn't root, in a delegated cgroup v2 subtree, as oci_box.rs; where that
//! isn't possible it prints SKIP and why rather than pass falsely. The refused box goes through the
//! bundle path (`ClassSpec::Bundle`) because an `OciSpec` carries no capabilities.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use hex_api::{
    Bounds, BoxId, BoxSpec, ClassSpec, Exit, Mount, Network, OciSpec, Program, Rootfs, Runtime,
    Stdio,
};
use hex_core::bundle::{Owner, Place, render};
use hex_node::{Node, OCI_BOXES};

const TEST: &str = "a_rootless_host_network_box_cannot_change_another_boxs_cgroup";

#[test]
fn a_rootless_host_network_box_cannot_change_another_boxs_cgroup() {
    let exe = env!("CARGO_BIN_EXE_hex").to_owned();
    let cache = cache_dir();
    match prepare_cgroup(&cache) {
        Ok(own) => exercise(&own, &cache, &exe),
        Err(direct) if std::env::var_os("HEX_DELEGATED").is_none() => {
            let has_systemd = Command::new("systemd-run")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !has_systemd {
                eprintln!(
                    "SKIP: own cgroup isn't ours to subdivide ({direct}) and no systemd --user for a private scope"
                );
                return;
            }
            let status = Command::new("systemd-run")
                .args(["--user", "--scope", "--quiet", "-p", "Delegate=yes"])
                .arg(std::env::current_exe().expect("test binary path"))
                .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
                .env("HEX_DELEGATED", "1")
                .status()
                .expect("run the test in a delegated scope");
            assert!(status.success(), "the delegated re-run failed");
        }
        // HEX_DELEGATED promised this binary a cgroup delegated to it: a preparation that fails there fails the test,
        // never a skip that would hide the breakage.
        Err(direct) => panic!("its cgroup isn't delegated to it ({direct})"),
    }
}

fn exercise(own: &Path, cache: &Path, exe: &str) {
    let uid = host_id("-u");
    let gid = host_id("-g");
    eprintln!(
        "oci_cgroupfs_box: {} (uid {uid}), kernel {}",
        if uid != 0 { "rootless" } else { "as root" },
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .unwrap_or_default()
            .trim()
    );
    if uid == 0 {
        // As root a box shares the host user namespace and gets a fresh sysfs, not a bind of the
        // host's, so there is no host cgroup filesystem beneath its own: nothing to guard here.
        eprintln!("SKIP: this guard is for a rootless node; as root no host cgroup fs is bound in");
        return;
    }
    if !userns_available() {
        eprintln!("SKIP: unprivileged user namespaces aren't available here");
        return;
    }

    let work = tempfile::Builder::new()
        .prefix("ocicgfs-")
        .tempdir_in(cache)
        .expect("work dir");
    let out = work.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let plain_rootfs = work.path().join("rootfs-plain");
    build_rootfs(&plain_rootfs);
    let attacker_rootfs = work.path().join("rootfs-attacker");
    build_rootfs(&attacker_rootfs);

    let box_root = own.join("hex-oci-cgfs");
    let state = work.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let node = Node::open(&box_root, &state, &state)
        .expect("open a node")
        .with_oci_helper(PathBuf::from(exe));

    // A box holding CAP_SYS_ADMIN without a network namespace of its own: the bundle the node renders
    // for a rootless host-network box, with CAP_SYS_ADMIN added to its capability sets. Its create is
    // refused by name, and nothing of it is left.
    let bundle_dir = work.path().join("attacker-bundle");
    std::fs::create_dir_all(&bundle_dir).unwrap();
    render_attacker_bundle(&bundle_dir, &attacker_rootfs, Owner::Rootless { uid, gid });
    let attacker = BoxId::new("attacker").unwrap();
    let made = node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: attacker.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(bundle_dir),
        owner: None,
        band: 0,
    });
    let _attacker_cleanup = Deleting(&node, attacker.clone());
    let why = match made {
        Ok(_) => panic!("a rootless host-network box holding CAP_SYS_ADMIN was made"),
        Err(e) => e.to_string(),
    };
    assert!(
        why.contains("CAP_SYS_ADMIN") && why.contains("network namespace"),
        "the box was refused, but not by name: {why}"
    );
    assert!(
        !box_root.join(OCI_BOXES).join(attacker.as_str()).exists(),
        "the refused box left its cgroup"
    );

    // A plain rootless host-network box reads its mount table into /out.
    let plain = BoxId::new("plain").unwrap();
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: plain.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Oci(Box::new(OciSpec {
            rootfs: Rootfs::Dir(plain_rootfs.clone()),
            init: Program::of(&["/usr/bin/sleep", "600"]),
            mounts: vec![bind("/usr", "/usr", true), bind_path(&out, "/out", false)],
            hostname: None,
            network: Network::Host,
            hosts: Vec::new(),
        })),
        owner: None,
        band: 0,
    })
    .expect("create the plain box");
    let _plain_cleanup = Deleting(&node, plain.clone());
    node.start(&plain).expect("start the plain box");
    let mut reader = Program::of(&[
        "/usr/bin/sh",
        "-c",
        "cat /proc/self/mountinfo > /out/mountinfo",
    ]);
    reader
        .env
        .push(("PATH".to_owned(), "/usr/bin:/bin".to_owned()));
    let ended = hex_api::conformance::end_of(
        &mut node
            .exec(&plain, reader, Stdio::null())
            .expect("exec the mount table's reader"),
    )
    .expect("wait for the reader");
    assert_eq!(ended, Exit::Code(0), "the reader failed");
    let mountinfo = std::fs::read_to_string(out.join("mountinfo")).expect("the box's mount table");

    // The cgroup filesystems at /sys/fs/cgroup, in the order they were mounted: the host's, carried by
    // the /sys bind, then the box's own over it (proc_pid_mountinfo(5): field 5 the mount point, field
    // 6 its options, the filesystem type after " - ").
    let cgroups: Vec<(&str, &str)> = mountinfo
        .lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let fields: Vec<&str> = left.split(' ').collect();
            let kind = right.split(' ').next()?;
            (matches!(kind, "cgroup2" | "cgroup") && fields.get(4) == Some(&"/sys/fs/cgroup"))
                .then(|| (*fields.get(4).unwrap_or(&""), *fields.get(5).unwrap_or(&"")))
        })
        .collect();
    assert!(
        cgroups.len() >= 2,
        "the host's cgroup filesystem and the box's own are both at /sys/fs/cgroup:\n{mountinfo}"
    );
    for (point, options) in &cgroups[..cgroups.len() - 1] {
        assert!(
            options.split(',').any(|o| o == "ro"),
            "the host cgroup filesystem the box's /sys bind carries at {point} is writable ({options}):\n{mountinfo}"
        );
    }

    // The plain box can't unmount its own cgroup filesystem off the host's: it holds no CAP_SYS_ADMIN.
    let mut unmount = Program::of(&[
        "/usr/bin/sh",
        "-c",
        "umount -l /sys/fs/cgroup 2>/out/umount.err; echo $? > /out/umount.rc; \
         cat /proc/self/mountinfo > /out/after-umount",
    ]);
    unmount
        .env
        .push(("PATH".to_owned(), "/usr/bin:/bin".to_owned()));
    let ended = hex_api::conformance::end_of(
        &mut node
            .exec(&plain, unmount, Stdio::null())
            .expect("exec the unmount"),
    )
    .expect("wait for the unmount");
    assert_eq!(ended, Exit::Code(0), "the unmount's script failed");
    let rc = std::fs::read_to_string(out.join("umount.rc")).unwrap();
    assert_ne!(
        rc.trim(),
        "0",
        "a box without CAP_SYS_ADMIN unmounted its cgroup filesystem: {}",
        std::fs::read_to_string(out.join("umount.err")).unwrap_or_default()
    );
    let after = std::fs::read_to_string(out.join("after-umount")).unwrap();
    assert_eq!(
        after
            .lines()
            .filter(|l| l.contains(" /sys/fs/cgroup ") && l.contains(" - cgroup2 "))
            .count(),
        cgroups.len(),
        "the box's cgroup filesystems are as they were:\n{after}"
    );
    node.delete(&plain).expect("delete the plain box");

    // Goal 5 extended, step 6's probe: a victim box, and beside it a box holding CAP_SYS_ADMIN in a user
    // namespace and a network namespace of its own, which unmounts its cgroup filesystem and mounts a
    // fresh one. Neither view reaches the victim's cgroup.
    let victim = BoxId::new("victim-cgfs").unwrap();
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: victim.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Oci(Box::new(OciSpec {
            rootfs: Rootfs::Dir(plain_rootfs.clone()),
            init: Program::of(&["/usr/bin/sleep", "600"]),
            mounts: vec![bind("/usr", "/usr", true)],
            hostname: None,
            network: Network::Private,
            hosts: Vec::new(),
        })),
        owner: None,
        band: 0,
    })
    .expect("create the victim box");
    let _victim_cleanup = Deleting(&node, victim.clone());
    node.start(&victim).expect("start the victim box");
    let prober_dir = work.path().join("prober-bundle");
    std::fs::create_dir_all(&prober_dir).unwrap();
    render_cap_sys_admin_bundle(
        &prober_dir,
        &attacker_rootfs,
        Owner::Rootless { uid, gid },
        Network::Private,
        &[bind("/usr", "/usr", true), bind_path(&out, "/out", false)],
    );
    let prober = BoxId::new("prober-cgfs").unwrap();
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: prober.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(prober_dir),
        owner: None,
        band: 0,
    })
    .expect("a box holding CAP_SYS_ADMIN with a network namespace of its own is made");
    let _prober_cleanup = Deleting(&node, prober.clone());
    node.start(&prober).expect("start the prober");
    let mut probe = Program::of(&[
        "/usr/bin/sh",
        "-c",
        "umount -l /sys/fs/cgroup; echo $? > /out/probe-umount.rc; \
         find /sys/fs/cgroup -maxdepth 4 > /out/probe-sys.ls 2>&1; \
         mkdir -p /tmp/cg && mount -t cgroup2 none /tmp/cg; echo $? > /out/probe-mount.rc; \
         find /tmp/cg -maxdepth 4 > /out/probe-cg.ls 2>&1; true",
    ]);
    probe
        .env
        .push(("PATH".to_owned(), "/usr/bin:/bin".to_owned()));
    let ended = hex_api::conformance::end_of(
        &mut node
            .exec(&prober, probe, Stdio::null())
            .expect("exec the probe"),
    )
    .expect("wait for the probe");
    assert_eq!(ended, Exit::Code(0), "the probe's script failed");
    let seen = |file: &str| std::fs::read_to_string(out.join(file)).unwrap_or_default();
    let (sys, cg) = (seen("probe-sys.ls"), seen("probe-cg.ls"));
    for (view, listing) in [
        ("/sys/fs/cgroup after the unmount", &sys),
        ("a fresh cgroup2", &cg),
    ] {
        assert!(
            !listing.contains(victim.as_str()) && !listing.contains("hex-oci-cgfs"),
            "the prober reached another box's cgroup through {view}:\n{listing}"
        );
    }
    // Its fresh cgroup2 mount, where the mount was let through, is rooted at its own cgroup: no
    // sub-cgroup of any other box is under it.
    let dirs: Vec<&str> = cg
        .lines()
        .filter(|l| !l.contains("/cgroup.") && !l.contains("/memory.") && !l.contains("/cpu"))
        .filter(|l| !l.contains("/io.") && !l.contains("/pids.") && !l.contains("/misc."))
        .collect();
    eprintln!(
        "step 6 probe: unmount rc {}, mount rc {}, reachable: {dirs:?}",
        seen("probe-umount.rc").trim(),
        seen("probe-mount.rc").trim()
    );
    node.delete(&prober).expect("delete the prober");
    node.delete(&victim).expect("delete the victim");
}

/// Renders the rootless host-network bundle the node would render for the attacker's spec, adds
/// CAP_SYS_ADMIN to its capability sets, and writes `config.json` and `hosts` into `bundle_dir`.
fn render_attacker_bundle(bundle_dir: &Path, rootfs: &Path, owner: Owner) {
    render_cap_sys_admin_bundle(
        bundle_dir,
        rootfs,
        owner,
        Network::Host,
        &[bind("/usr", "/usr", true)],
    );
}

/// Renders the bundle the node would render for a box on `network` with `mounts`, adds CAP_SYS_ADMIN to
/// its capability sets, and writes `config.json` and `hosts` into `bundle_dir`.
fn render_cap_sys_admin_bundle(
    bundle_dir: &Path,
    rootfs: &Path,
    owner: Owner,
    network: Network,
    mounts: &[Mount],
) {
    let spec = OciSpec {
        rootfs: Rootfs::Dir(rootfs.to_path_buf()),
        init: Program::of(&["/usr/bin/sleep", "600"]),
        mounts: mounts.to_vec(),
        hostname: None,
        network,
        hosts: Vec::new(),
    };
    let rendered = render(
        &spec,
        &Place {
            dir: bundle_dir,
            owner,
            masked: &[],
        },
    )
    .expect("render the attacker bundle");
    // Add CAP_SYS_ADMIN to each capability set. The rendered config lists Docker's 14 defaults, which
    // end in CAP_AUDIT_WRITE and leave out CAP_SYS_ADMIN; each of the bounding, effective and
    // permitted sets ends in that one, so extending it there adds the capability to all three.
    let config = String::from_utf8(rendered.config).expect("the rendered config is UTF-8");
    assert_eq!(
        config.matches("\"CAP_AUDIT_WRITE\"").count(),
        3,
        "three capability sets to extend with CAP_SYS_ADMIN: {config}"
    );
    let config = config.replace(
        "\"CAP_AUDIT_WRITE\"",
        "\"CAP_AUDIT_WRITE\", \"CAP_SYS_ADMIN\"",
    );
    std::fs::write(bundle_dir.join("config.json"), config).expect("write config.json");
    std::fs::write(bundle_dir.join("hosts"), &rendered.hosts).expect("write hosts");
}

fn bind(source: &str, destination: &str, read_only: bool) -> Mount {
    bind_path(Path::new(source), destination, read_only)
}

fn bind_path(source: &Path, destination: &str, read_only: bool) -> Mount {
    Mount {
        source: source.to_path_buf(),
        destination: destination.to_owned(),
        read_only,
    }
}

/// Deletes its box when dropped, so a check that fails leaves no container running.
struct Deleting<'a>(&'a Node, BoxId);

impl Drop for Deleting<'_> {
    fn drop(&mut self) {
        let _ = self.0.delete(&self.1);
    }
}

fn userns_available() -> bool {
    Command::new("unshare")
        .args(["-U", "true"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn host_id(flag: &str) -> u32 {
    let out = Command::new("id").arg(flag).output().expect("id");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("id is a number")
}

/// A rootfs of the host's `/usr`, with the host's top-level links into it (`/bin`, `/lib64`, …) so
/// the loader and `/bin/sh` resolve as on the host, and the directories the box's mounts land on.
fn build_rootfs(rootfs: &Path) {
    for d in ["proc", "dev", "sys", "tmp", "usr", "out"] {
        std::fs::create_dir_all(rootfs.join(d)).unwrap();
    }
    for link in ["bin", "sbin", "lib", "lib64"] {
        if let Ok(target) = std::fs::read_link(Path::new("/").join(link)) {
            let target = target.to_string_lossy().trim_start_matches('/').to_owned();
            std::os::unix::fs::symlink(target, rootfs.join(link)).unwrap();
        }
    }
}

/// This process's own cgroup, delegated to it and prepared once for every test in this binary
/// (`hex_testkit::cgroup::delegated`: the process and every process of its own moved into a leaf), or why it isn't
/// ours to subdivide. The probe root's opening proves every controller a box uses is delegated here: `BoxRoot::open`
/// refuses by name otherwise (HEX-INST-1).
fn prepare_cgroup(disk: &Path) -> Result<PathBuf, String> {
    hex_testkit::cgroup::delegated(|own| {
        let probe = hex_runtime::linux::cgroup::BoxRoot::open(
            &own.join("hex-probe"),
            hex_runtime::linux::disks::under(disk).unwrap_or_default(),
        )
        .map(|_| ())
        .map_err(|e| e.to_string());
        let _ = std::fs::remove_dir(own.join("hex-probe"));
        probe
    })
}

fn cache_dir() -> PathBuf {
    let cache =
        PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/hex-t7/ocicgfs");
    std::fs::create_dir_all(&cache).expect("cache dir");
    cache
}
