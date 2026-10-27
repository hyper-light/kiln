//! A runner's facts (docs/ci.md W13-AC01): what its probes report matches what the machine says
//! through other means (its own tools and files, read here independently), a declared fact and a
//! declared probe appear, and nothing undeclared does.

use std::collections::BTreeMap;

use kiln::runner::probe::{Declared, gather};

/// What `sh -c script` prints, trimmed.
#[cfg(unix)]
fn sh(script: &str) -> Option<String> {
    let o = std::process::Command::new("sh").args(["-c", script]).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

#[cfg(target_os = "linux")]
/// The tightest memory.max on this process's cgroup and its ancestors, when any is set.
#[cfg(target_os = "linux")]
fn cgroup_memory_max() -> Option<u64> {
    let own = std::fs::read_to_string("/proc/self/cgroup").ok()?.lines().find_map(|l| l.strip_prefix("0::"))?.trim().to_owned();
    let mut dir = std::path::Path::new("/sys/fs/cgroup").join(own.trim_start_matches('/'));
    let mut least: Option<u64> = None;
    loop {
        if let Some(m) = std::fs::read_to_string(dir.join("memory.max")).ok().and_then(|v| v.trim().parse::<u64>().ok()) {
            least = Some(least.map_or(m, |l| l.min(m)));
        }
        if !dir.pop() || dir == std::path::Path::new("/sys/fs") {
            return least;
        }
    }
}

fn close(a: u64, b: u64) -> bool {
    a.abs_diff(b) <= b / 100
}

#[test]
fn w13_ac01_facts() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let declared = Declared {
        facts: [("location".to_owned(), "rack-2".to_owned())].into(),
        probes: [("answer".to_owned(), "echo the answer is v4.2.0".to_owned()), ("broken".to_owned(), "exit 3".to_owned())].into(),
        ..Declared::default()
    };
    let (path, wanted) = (dir.path().to_path_buf(), declared.clone());
    let f = kiln::rt::block_on(async move { anyhow::Ok(gather(&path, &wanted).await) })?;

    // The machine's own answers.
    assert_eq!((f.os.as_str(), f.arch.as_str()), (std::env::consts::OS, std::env::consts::ARCH));
    assert_eq!(f.cpu.threads as usize, std::thread::available_parallelism()?.get());
    assert!(f.cpu.cores >= 1 && f.cpu.cores <= f.cpu.threads);
    assert!(f.mem.total > 0 && f.mem.free <= f.mem.total && f.disk.free <= f.disk.total && f.disk.total > 0);
    assert!(f.executors.contains_key("host"));
    assert!(f.probed_at > 0 && f.kiln.is_some());
    #[cfg(target_os = "linux")]
    {
        let meminfo = sh("grep MemTotal /proc/meminfo").unwrap_or_default();
        let kb: u64 = meminfo.split_whitespace().nth(1).and_then(|n| n.parse().ok()).unwrap_or(0);
        // What this process may use: the machine's, or less under a cgroup's memory.max (a gate's
        // unit runs under one).
        let usable = cgroup_memory_max().map_or(kb * 1024, |m| m.min(kb * 1024));
        assert!(close(f.mem.total, usable), "memory {} vs /proc/meminfo {} and cgroup limit {:?}", f.mem.total, kb * 1024, cgroup_memory_max());
        assert_eq!(f.kernel.as_deref(), sh("uname -r").as_deref());
        assert_eq!(f.os_distro.as_deref(), sh(". /etc/os-release && echo \"$ID\"").as_deref().filter(|d| !d.is_empty()));
        let size: u64 = sh(&format!("df -B1 --output=size '{}' | tail -1", dir.path().display())).and_then(|s| s.parse().ok()).unwrap_or(0);
        assert!(close(f.disk.total, size), "disk {} vs df {}", f.disk.total, size);
        assert_eq!(f.virt.kvm, sh("test -r /dev/kvm && test -w /dev/kvm && echo yes").is_some());
        #[cfg(target_arch = "x86_64")]
        {
            // Every flag reported is one the kernel lists.
            let listed = sh("grep -m1 '^flags' /proc/cpuinfo").unwrap_or_default();
            let kernel_name = |f: &str| match f {
                "sse4.2" => "sse4_2".to_owned(),
                "sha" => "sha_ni".to_owned(),
                "avx512vnni" => "avx512_vnni".to_owned(),
                other => other.to_owned(),
            };
            for flag in &f.cpu.flags {
                assert!(listed.split_whitespace().any(|w| w == kernel_name(flag)), "{flag} isn't in /proc/cpuinfo");
            }
        }
    }
    #[cfg(unix)]
    {
        if let Some(v) = sh("git --version") {
            assert_eq!(f.tools.get("git").map(String::as_str), v.split_whitespace().nth(2), "git");
        }
        let docker_up = sh("docker version --format '{{.Server.Version}}' 2>/dev/null || podman version --format '{{.Version}}' 2>/dev/null").is_some_and(|v| !v.is_empty());
        assert_eq!(f.executors.contains_key("docker"), docker_up, "{:?}", f.executors);
    }

    // What was declared, exactly: the fact, and the probe that answered; nothing else.
    let want: BTreeMap<String, String> = [("location".to_owned(), "rack-2".to_owned()), ("answer".to_owned(), "4.2.0".to_owned())].into();
    assert_eq!(f.custom, want);
    Ok(())
}
