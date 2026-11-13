//! AC-T7.1.2: another box's fsync p99 while one box builds at full tilt, against idle, is at most
//! 2x. Two sibling `process` boxes share a disk: a *victim* with a tight `io.latency` target runs
//! the fsync probe, a *builder* with a low `io.weight` saturates the disk. The benchmark reports
//! the victim's fsync p99 idle and under load, and their ratio. The AC measures this on adacachy;
//! this harness runs anywhere with cgroup v2 + a delegated subtree (it re-runs itself once inside a
//! `systemd-run --user --scope -p Delegate=yes` when its own cgroup is shared).
//!
//! Recorded in bench/ per CLAUDE.md §7 (release binary, named hardware, load average under 2).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::cast_precision_loss
)]

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("SKIP: the noisy_neighbor bench is Linux-only");
}

#[cfg(target_os = "linux")]
fn main() {
    std::process::exit(linux::run());
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::Read;
    use std::path::PathBuf;
    use std::process::Command;

    use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Program, Runtime, Stdio};
    use hex_node::Node;

    const HEX: &str = env!("CARGO_BIN_EXE_hex");
    /// The victim's fsync-latency target: while it is missed, the kernel throttles the sibling.
    const VICTIM_LATENCY_US: u32 = 10_000;
    /// The builder's share of the disk against the victim's default 100 — it yields under contention.
    const BUILDER_IO_WEIGHT: u16 = 10;
    const PROBE_ITERS: u32 = 400;
    const BUILD_SECONDS: u64 = 10;

    pub fn run() -> i32 {
        let own = match prepare() {
            Ok(o) => o,
            Err(e) if std::env::var_os("HEX_NN_DELEGATED").is_none() => {
                if !Command::new("systemd-run")
                    .arg("--version")
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false)
                {
                    eprintln!("SKIP: no cgroup delegation ({e}) and no systemd --user");
                    return 0;
                }
                let me = std::env::current_exe().expect("current exe");
                let st = Command::new("systemd-run")
                    .args(["--user", "--scope", "--quiet", "-p", "Delegate=yes"])
                    .arg(&me)
                    .env("HEX_NN_DELEGATED", "1")
                    .status()
                    .expect("systemd-run");
                return i32::from(!st.success());
            }
            Err(e) => {
                eprintln!("SKIP: no usable cgroup delegation ({e})");
                return 0;
            }
        };

        let cache = PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/hex-t7/nn");
        std::fs::create_dir_all(&cache).expect("cache");
        let state = tempfile::Builder::new()
            .prefix("nn-")
            .tempdir_in(&cache)
            .expect("state");
        let node = Node::open(&own.join(".nn-boxes"), state.path(), &cache).expect("node");

        let victim = BoxId::new("victim").unwrap();
        let builder = BoxId::new("builder").unwrap();
        node.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: victim.clone(),
            bounds: Bounds {
                io_latency_us: Some(VICTIM_LATENCY_US),
                ..Bounds::default()
            },
            class: ClassSpec::Process,
            owner: None,
        })
        .expect("victim box");
        node.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: builder.clone(),
            bounds: Bounds {
                io_weight: Some(BUILDER_IO_WEIGHT),
                ..Bounds::default()
            },
            class: ClassSpec::Process,
            owner: None,
        })
        .expect("builder box");
        let vfile = cache.join("victim.dat").to_string_lossy().into_owned();
        let bfile = cache.join("builder.dat").to_string_lossy().into_owned();

        let idle = probe(&node, &victim, &vfile).expect("idle probe");

        // Start the builder saturating the disk, then probe the victim while it runs.
        let mut hog = node
            .exec(
                &builder,
                Program::of(&[
                    HEX,
                    "selftest-box",
                    "disk-hog",
                    &bfile,
                    &BUILD_SECONDS.to_string(),
                ]),
                Stdio::null(),
            )
            .expect("start builder");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let loaded = probe(&node, &victim, &vfile).expect("loaded probe");
        let _ = hog.wait_for(std::time::Duration::from_secs(
            BUILD_SECONDS.saturating_add(5),
        ));

        let _ = node.delete(&victim);
        let _ = node.delete(&builder);

        let ratio = if idle == 0 {
            f64::INFINITY
        } else {
            loaded as f64 / idle as f64
        };
        println!(
            "noisy_neighbor: victim fsync p99 idle={idle}us loaded={loaded}us ratio={ratio:.2}x (builder io_weight={BUILDER_IO_WEIGHT}, victim io.latency={VICTIM_LATENCY_US}us)"
        );
        println!(
            "AC-T7.1.2 (ratio<=2x): {}",
            if ratio <= 2.0 { "PASS" } else { "FAIL" }
        );
        i32::from(ratio > 2.0)
    }

    /// Runs the fsync probe in the victim box and returns its p99 in microseconds.
    fn probe(node: &Node, victim: &BoxId, file: &str) -> Option<u128> {
        let (mut r, w) = std::io::pipe().ok()?;
        let mut proc = node
            .exec(
                victim,
                Program::of(&[
                    HEX,
                    "selftest-box",
                    "fsync-probe",
                    file,
                    &PROBE_ITERS.to_string(),
                ]),
                Stdio {
                    stdout: Some(w),
                    ..Stdio::null()
                },
            )
            .ok()?;
        proc.wait_for(std::time::Duration::from_secs(120)).ok()?;
        let mut out = String::new();
        r.read_to_string(&mut out).ok()?;
        out.split_whitespace()
            .find_map(|tok| tok.strip_prefix("p99_us="))
            .and_then(|n| n.parse().ok())
    }

    /// Moves this process into a leaf so its own cgroup can delegate controllers to the box root.
    fn prepare() -> Result<PathBuf, String> {
        let own = hex_runtime::linux::cgroup::own_cgroup().map_err(|e| e.to_string())?;
        let leaf = own.join(".nn-agent");
        std::fs::create_dir_all(&leaf).map_err(|e| format!("leaf: {e}"))?;
        std::fs::write(leaf.join("cgroup.procs"), format!("{}", std::process::id()))
            .map_err(|e| format!("move self: {e}"))?;
        hex_runtime::linux::cgroup::BoxRoot::open(&own.join(".nn-probe"), Vec::new())
            .map_err(|e| e.to_string())?;
        let _ = std::fs::remove_dir(own.join(".nn-probe"));
        Ok(own)
    }
}
