//! AC-T7.1.5: the memory the node agent adds per box is at most 1 MiB. The agent keeps no per-box
//! threads or buffers — a box is a cgroup (the kernel's memory) and a record (on disk) — so this
//! measures the agent process's resident-set growth across creating many boxes and divides by the
//! count. It needs a cgroup v2 subtree with every one of cpu, io, memory and pids delegated; where this
//! process's cgroup lacks one, it re-runs itself once inside a `systemd-run --user --scope -p Delegate=yes`.
//! Recorded in bench/.
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
    eprintln!("SKIP: the node-memory bench is Linux-only");
}

#[cfg(target_os = "linux")]
fn main() {
    use std::path::PathBuf;
    use std::process::Command;

    use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Runtime};
    use hex_node::Node;

    /// How many boxes to create for the measurement.
    const BOXES: usize = 500;

    fn own_cgroup() -> PathBuf {
        let own = std::fs::read_to_string("/proc/self/cgroup").expect("/proc/self/cgroup");
        let rel = own
            .lines()
            .find_map(|l| l.strip_prefix("0::"))
            .expect("0:: line")
            .trim_start_matches('/');
        PathBuf::from("/sys/fs/cgroup").join(rel)
    }

    /// Resident set size of this process, in bytes (from `/proc/self/status` VmRSS, in kibibytes).
    fn rss_bytes() -> u64 {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|kb| kb.parse::<u64>().ok())
            .map(|kb| kb.saturating_mul(1024))
            .unwrap_or(0)
    }

    fn prepare() -> Result<PathBuf, String> {
        let own = own_cgroup();
        let leaf = own.join("hex-agent");
        std::fs::create_dir_all(&leaf).map_err(|e| format!("leaf: {e}"))?;
        std::fs::write(leaf.join("cgroup.procs"), format!("{}", std::process::id()))
            .map_err(|e| format!("move self: {e}"))?;
        // The probe root opening proves every controller a box uses is delegated here: `BoxRoot::open`
        // refuses by name otherwise (HEX-INST-1).
        hex_runtime::linux::cgroup::BoxRoot::open(&own.join("hex-probe"), Vec::new())
            .map_err(|e| e.to_string())?;
        let _ = std::fs::remove_dir(own.join("hex-probe"));
        Ok(own)
    }

    let own = match prepare() {
        Ok(o) => o,
        Err(e) if std::env::var_os("HEX_DELEGATED").is_none() => {
            if !Command::new("systemd-run")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                eprintln!("SKIP: no cgroup delegation ({e}) and no systemd --user");
                return;
            }
            let me = std::env::current_exe().expect("current exe");
            let status = Command::new("systemd-run")
                .args(["--user", "--scope", "--quiet", "-p", "Delegate=yes"])
                .arg(&me)
                .env("HEX_DELEGATED", "1")
                .status()
                .expect("systemd-run");
            std::process::exit(i32::from(!status.success()));
        }
        Err(e) => {
            eprintln!("SKIP: no usable cgroup delegation ({e})");
            return;
        }
    };

    let cache =
        PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/hex-t7/memtest");
    std::fs::create_dir_all(&cache).expect("cache");
    let state = tempfile::Builder::new()
        .prefix("mem-")
        .tempdir_in(&cache)
        .expect("state");
    let node = Node::open(&own.join("hex-boxes-mem"), state.path(), state.path()).expect("node");

    // Touch the allocator and warm paths with a few boxes first, so the measured delta is per-box
    // steady-state, not one-off setup.
    for i in 0..16 {
        let id = BoxId::new(&format!("warm{i}")).unwrap();
        node.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id,
            bounds: Bounds::default(),
            class: ClassSpec::Process,
            owner: None,
        })
        .unwrap();
    }
    let before = rss_bytes();
    for i in 0..BOXES {
        let id = BoxId::new(&format!("mem{i}")).unwrap();
        node.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id,
            bounds: Bounds {
                memory_max: Some(64 << 20),
                pids_max: Some(64),
                ..Bounds::default()
            },
            class: ClassSpec::Process,
            owner: None,
        })
        .unwrap();
    }
    let after = rss_bytes();
    let per_box = after.saturating_sub(before) / (BOXES as u64);

    // Clean the boxes up.
    for i in 0..BOXES {
        let id = BoxId::new(&format!("mem{i}")).unwrap();
        let _ = node.delete(&id);
    }
    for i in 0..16 {
        let id = BoxId::new(&format!("warm{i}")).unwrap();
        let _ = node.delete(&id);
    }

    println!(
        "node_memory: boxes={BOXES} rss_before={before} rss_after={after} per_box_bytes={per_box} ({} KiB) in {}",
        per_box / 1024,
        own.display()
    );
    println!(
        "AC-T7.1.5 (<=1 MiB/box): {}",
        if per_box <= (1 << 20) { "PASS" } else { "FAIL" }
    );
    std::process::exit(i32::from(per_box > (1 << 20)));
}
