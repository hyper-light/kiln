//! Goal 17's lever (b), measured before it is built (the lead's ask of 2026-11-09): what a claimed init's own read of
//! the spec the agent already checked and sealed costs, as the init meets it. A claimed init decodes its claim
//! (`seed::read_message`), then reads the sealed `config.json` and runs `Spec::parse` (serde_json's `from_slice`, then
//! `Spec::check`) in a process that has never run that code: each page of it faulted in (fork maps no file-backed text
//! in the child) and the heap it writes first touched. So each sample is a process of its own, this binary run again
//! as `child`, timing its first decode and its first parse with the minor faults each took (proc(5), /proc/self/stat,
//! field 10), then each again warm, and `from_slice` and `check` apart, warm. The spec is the record's:
//! create_breakdown_inside.sh's `/run/bundle-true` with HEXBENCH_OWNER and HEXBENCH_DOCKER_PATHS=1; the claim a pruned
//! create of it as the agent sends one. 299 samples, the fewest whose largest bounds the p99 with 95% confidence (Ada's
//! rule: N >= ln(0.05)/ln(0.99) = 298.07); each figure's median and p99, nearest rank.
//!
//! Run: cargo bench -p hex-bench --bench spec_parse
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::print_stdout
)]

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("SKIP: the spec parse bench is Linux-only");
}

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(target_os = "linux")]
mod linux {
    use std::hint::black_box;
    use std::process::Command;
    use std::time::Instant;

    use hex_core::seed::{self, Create, Fixed, MountNamespace};
    use hex_runtime::linux::oci::spec::Spec;

    /// The record's `true` bundle's config.json (create_breakdown_inside.sh with HEXBENCH_OWNER and
    /// HEXBENCH_DOCKER_PATHS=1): Docker's default capabilities, masked and read-only paths.
    const CONFIG: &str = r#"{ "ociVersion":"1.0.2","root":{"path":"rootfs","readonly":false},"hostname":"b",
  "process":{"cwd":"/","args":["/usr/bin/true"],"env":["PATH=/usr/bin:/usr/sbin"],"user":{"uid":0,"gid":0},"capabilities":{"bounding":["CAP_CHOWN","CAP_DAC_OVERRIDE","CAP_FSETID","CAP_FOWNER","CAP_MKNOD","CAP_NET_RAW","CAP_SETGID","CAP_SETUID","CAP_SETFCAP","CAP_SETPCAP","CAP_NET_BIND_SERVICE","CAP_SYS_CHROOT","CAP_KILL","CAP_AUDIT_WRITE"],"effective":["CAP_CHOWN","CAP_DAC_OVERRIDE","CAP_FSETID","CAP_FOWNER","CAP_MKNOD","CAP_NET_RAW","CAP_SETGID","CAP_SETUID","CAP_SETFCAP","CAP_SETPCAP","CAP_NET_BIND_SERVICE","CAP_SYS_CHROOT","CAP_KILL","CAP_AUDIT_WRITE"],"permitted":["CAP_CHOWN","CAP_DAC_OVERRIDE","CAP_FSETID","CAP_FOWNER","CAP_MKNOD","CAP_NET_RAW","CAP_SETGID","CAP_SETUID","CAP_SETFCAP","CAP_SETPCAP","CAP_NET_BIND_SERVICE","CAP_SYS_CHROOT","CAP_KILL","CAP_AUDIT_WRITE"]}},
  "mounts":[{"destination":"/usr","type":"bind","source":"/usr","options":["rbind","ro"]},
            {"destination":"/proc","type":"proc","source":"proc"},
            {"destination":"/dev","type":"tmpfs","source":"tmpfs","options":["nosuid","mode=755","size=65536k"]}],
  "linux":{"namespaces":[{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"},{"type":"network"}],"maskedPaths":["/proc/acpi","/proc/asound","/proc/interrupts","/proc/kcore","/proc/keys","/proc/latency_stats","/proc/sched_debug","/proc/scsi","/proc/timer_list","/proc/timer_stats","/sys/devices/virtual/powercap","/sys/firmware"],"readonlyPaths":["/proc/bus","/proc/fs","/proc/irq","/proc/sys","/proc/sysrq-trigger"]} }"#;

    /// Samples: the fewest whose largest bounds the p99 with 95% confidence (Ada's rule).
    const SAMPLES: usize = 299;

    const COLUMNS: [&str; 8] = [
        "claim decode, first (ns)",
        "its minor faults",
        "spec parse and check, first (ns)",
        "its minor faults",
        "claim decode, warm (ns)",
        "spec parse and check, warm (ns)",
        "from_slice alone, warm (ns)",
        "check alone, warm (ns)",
    ];

    pub fn main() {
        if std::env::args_os().nth(1).is_some_and(|a| a == "child") {
            child();
            return;
        }
        let exe = std::env::current_exe().expect("this bench's binary");
        let mut rows: Vec<Vec<u64>> = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let out = Command::new(&exe).arg("child").output().expect("a child");
            assert!(out.status.success(), "a child failed: {out:?}");
            let line = String::from_utf8(out.stdout).expect("a child's line");
            rows.push(
                line.split_whitespace()
                    .map(|v| v.parse().expect("a figure"))
                    .collect(),
            );
        }
        println!(
            "spec_parse: {SAMPLES} processes, the record's spec ({} bytes)",
            CONFIG.len()
        );
        println!("| figure | median | p99 |");
        println!("|---|---|---|");
        for (i, name) in COLUMNS.iter().enumerate() {
            let mut column: Vec<u64> = rows.iter().map(|r| r[i]).collect();
            column.sort_unstable();
            // Nearest rank: the ceiling of num/den of the samples, counted from 1.
            let rank =
                |num: usize, den: usize| column[(column.len() * num).div_ceil(den).max(1) - 1];
            println!("| {name} | {} | {} |", rank(1, 2), rank(99, 100));
        }
    }

    /// One sample, in a process of its own: the claim's decode and the spec's parse first, each with its minor
    /// faults, then each warm, and the parse's halves apart.
    fn child() {
        let config = CONFIG.as_bytes();
        let create = Create {
            bundle: "/run/bundle-true".to_owned(),
            dir: "/var/lib/hex/oci/state/v000001-000".to_owned(),
            cgroup: Some("/sys/fs/cgroup/hex/oci/slot-1".to_owned()),
            network: true,
            pruned: true,
            binds: 1,
            path_joins: 0,
            dev_mount: Some(2),
            ipc_uts: true,
            config: true,
            mount_namespace: MountNamespace::Copy,
            proc_mount: Some(1),
            masked_directory: true,
            // A claim carries only what its init reads, and no cgroup directory: the init is in its cgroup already.
            fixed: Fixed::Lean { cgroup: false },
        };
        let bytes = seed::create(&create).expect("the claim's bytes");
        let fds = create.fds();
        // The fault counter's own read, its code and buffer first touched here.
        let _ = minflt();
        let f0 = minflt();
        let t = Instant::now();
        black_box(seed::read_message(black_box(&bytes), bytes.len(), fds).expect("the claim"));
        let decode_first = t.elapsed().as_nanos();
        let f1 = minflt();
        let t = Instant::now();
        black_box(Spec::parse(black_box(config)).expect("the spec"));
        let parse_first = t.elapsed().as_nanos();
        let f2 = minflt();
        let t = Instant::now();
        black_box(seed::read_message(black_box(&bytes), bytes.len(), fds).expect("the claim"));
        let decode_warm = t.elapsed().as_nanos();
        let t = Instant::now();
        black_box(Spec::parse(black_box(config)).expect("the spec"));
        let parse_warm = t.elapsed().as_nanos();
        let t = Instant::now();
        let spec: Spec = serde_json::from_slice(black_box(config)).expect("the spec");
        let json_warm = t.elapsed().as_nanos();
        let t = Instant::now();
        black_box(black_box(&spec).check()).expect("checked");
        let check_warm = t.elapsed().as_nanos();
        println!(
            "{decode_first} {} {parse_first} {} {decode_warm} {parse_warm} {json_warm} {check_warm}",
            f1 - f0,
            f2 - f1
        );
    }

    /// This process's minor faults so far: /proc/self/stat's tenth field, the eighth after the name's closing
    /// parenthesis.
    fn minflt() -> u64 {
        let stat = std::fs::read_to_string("/proc/self/stat").expect("/proc/self/stat");
        let (_, fields) = stat.rsplit_once(')').expect("a stat line");
        fields
            .split_whitespace()
            .nth(7)
            .expect("minflt")
            .parse()
            .expect("a count")
    }
}
