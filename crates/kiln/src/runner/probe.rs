//! What a runner reports about itself (docs/ci.md W13-R01, R02): built-in probes, run when it
//! connects, every 10 minutes and when the server asks (`ServerMsg::ProbeFacts`), and what its
//! administrator declares (`[agent.facts]`, and `[agent.probes]` commands whose output is read as
//! a version). Nothing else is guessed: a fact a probe can't establish is left out.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use kiln_ci::facts::{Cpu, Disk, Executor, Gpu, Mem, Net, RunnerFacts, Virt};
use serde::{Deserialize, Serialize};

/// What an administrator declares of a runner (W13-R02).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Declared {
    /// Facts as they are (`location = "rack-2"`).
    pub facts: BTreeMap<String, String>,
    /// Commands whose output is read as a version (`cuda = "nvcc --version"`).
    pub probes: BTreeMap<String, String>,
}

impl Declared {
    /// `[agent]` of the configuration file at `path`, when it has one: what a `kiln agent` reads.
    pub fn from_file(path: &Path) -> Declared {
        let Ok(text) = std::fs::read_to_string(path) else { return Declared::default() };
        let Ok(table) = text.parse::<toml::Table>() else { return Declared::default() };
        table.get("agent").cloned().and_then(|a| a.try_into().ok()).unwrap_or_default()
    }
}

const TOOL_TIMEOUT: Duration = Duration::from_secs(5);

/// The first version number in `text`: `git version 2.47.1` → `2.47.1`, `go1.22.3` → `1.22.3`,
/// `v20.11.0` → `20.11.0`.
pub fn version_in(text: &str) -> Option<String> {
    for token in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.')) {
        let Some(start) = token.find(|c: char| c.is_ascii_digit()) else { continue };
        let rest = token.get(start..).unwrap_or_default();
        let v: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        let v = v.trim_end_matches('.');
        if v.contains('.') && v.split('.').all(|p| !p.is_empty()) {
            return Some(v.to_owned());
        }
    }
    None
}

/// What `program args…` prints (out and err), when it runs and succeeds in time.
async fn output(program: &str, args: &[&str]) -> Option<String> {
    let path = crate::os::which(program)?;
    let mut c = std::process::Command::new(path);
    c.args(args);
    let o = crate::proc::output(c, TOOL_TIMEOUT).await.ok()?;
    o.status.success().then(|| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

/// Tools found on `PATH`, with the version each reports.
async fn tools() -> BTreeMap<String, String> {
    let list: &[(&str, &str, &[&str])] = &[
        ("git", "git", &["--version"]),
        ("node", "node", &["--version"]),
        ("python3", "python3", &["--version"]),
        ("cargo", "cargo", &["--version"]),
        ("go", "go", &["version"]),
        ("java", "java", &["-version"]),
        ("dotnet", "dotnet", &["--version"]),
        ("docker", "docker", &["--version"]),
        ("docker buildx", "docker", &["buildx", "version"]),
        ("podman", "podman", &["--version"]),
        ("shards", "shards", &["--version"]),
        ("firecracker", "firecracker", &["--version"]),
        ("kubectl", "kubectl", &["version", "--client"]),
    ];
    let found = futures::future::join_all(list.iter().map(|(name, program, args)| async move { output(program, args).await.as_deref().and_then(version_in).map(|v| ((*name).to_owned(), v)) })).await;
    found.into_iter().flatten().collect()
}

/// The executors this runner can run legs on right now, each probed: the host always; Docker (or
/// Podman) when its engine answers, with its version and whether it runs rootless. Firecracker and
/// shards are reported as tools until this runner can run legs on them.
async fn executors() -> BTreeMap<String, Executor> {
    let mut out = BTreeMap::new();
    out.insert("host".to_owned(), Executor::default());
    let Ok(Some(bin)) = crate::rt::blocking(super::docker::find).await else { return out };
    let engine = if bin.ends_with("podman") { "podman" } else { "docker" };
    let mut version = None;
    for format in ["{{.Server.Version}}", "{{.Version}}"] {
        if let Some(v) = output(&bin, &["version", "--format", format]).await.as_deref().and_then(version_in) {
            version = Some(v);
            break;
        }
    }
    let rootless = crate::rt::blocking(super::docker::rootless_engine).await.unwrap_or(false);
    let details = [("engine".to_owned(), engine.to_owned()), ("rootless".to_owned(), rootless.to_string())].into();
    out.insert("docker".to_owned(), Executor { version, details });
    out
}

/// GPUs: NVIDIA's through `nvidia-smi`, a Mac's through `system_profiler`.
async fn gpus() -> Vec<Gpu> {
    let mut out = Vec::new();
    if let Some(text) = output("nvidia-smi", &["--query-gpu=name,memory.total,driver_version", "--format=csv,noheader,nounits"]).await {
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let parts: Vec<&str> = line.split(',').map(str::trim).collect();
            let (Some(model), Some(mib)) = (parts.first(), parts.get(1)) else { continue };
            out.push(Gpu { vendor: "nvidia".into(), model: (*model).to_owned(), memory: mib.parse::<u64>().unwrap_or(0) << 20, driver: parts.get(2).map(|d| (*d).to_owned()) });
        }
    }
    if cfg!(target_os = "macos")
        && let Some(text) = output("system_profiler", &["SPDisplaysDataType", "-json"]).await
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
    {
        for g in v.get("SPDisplaysDataType").and_then(serde_json::Value::as_array).into_iter().flatten() {
            let model = g.get("sppci_model").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
            if model.is_empty() {
                continue;
            }
            let vendor = g.get("spdisplays_vendor").and_then(serde_json::Value::as_str).unwrap_or_default().to_ascii_lowercase();
            let vendor = if vendor.contains("apple") || model.starts_with("Apple") { "apple" } else if vendor.contains("amd") || vendor.contains("ati") { "amd" } else if vendor.contains("intel") { "intel" } else { "other" };
            // Discrete GPUs say their memory ("8 GB"); Apple silicon shares the machine's.
            let memory = g.get("spdisplays_vram").or_else(|| g.get("spdisplays_vram_shared")).and_then(serde_json::Value::as_str).and_then(|s| s.split_whitespace().next()?.parse::<u64>().ok()).map_or(0, |gb| gb << 30);
            out.push(Gpu { vendor: vendor.into(), model, memory, driver: g.get("spdisplays_mtlgpufamilysupport").and_then(serde_json::Value::as_str).map(|m| m.trim_start_matches("spdisplays_").to_owned()) });
        }
    }
    out
}

/// Whether the internet is reachable directly, and the proxy the environment names (its
/// credentials left out).
async fn net() -> Net {
    let egress = crate::rt::timeout(Duration::from_secs(6), crate::http::io::connect("github.com", 443)).await.is_ok_and(|r| r.is_ok());
    let proxy = ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty())).map(|p| match p.split_once("://") {
        Some((scheme, rest)) => format!("{scheme}://{}", rest.rsplit_once('@').map_or(rest, |(_, h)| h)),
        None => p.rsplit_once('@').map_or(p.as_str(), |(_, h)| h).to_owned(),
    });
    Net { egress, proxy }
}

/// CPU feature flags the processor has, as the standard library detects them.
fn cpu_flags() -> Vec<String> {
    #[allow(unused_mut)]
    let mut out: Vec<&str> = Vec::new();
    #[cfg(target_arch = "x86_64")]
    {
        macro_rules! x86 {
            ($($f:tt),*) => { $( if std::arch::is_x86_feature_detected!($f) { out.push($f); } )* };
        }
        x86!("sse4.2", "popcnt", "aes", "pclmulqdq", "avx", "avx2", "fma", "bmi1", "bmi2", "f16c", "sha", "avx512f", "avx512bw", "avx512vl", "avx512vnni", "vaes", "vpclmulqdq");
    }
    #[cfg(target_arch = "aarch64")]
    {
        macro_rules! arm {
            ($($f:tt),*) => { $( if std::arch::is_aarch64_feature_detected!($f) { out.push($f); } )* };
        }
        arm!("neon", "aes", "sha2", "sha3", "crc", "lse", "dotprod", "fp16", "sve", "sve2", "bf16", "i8mm");
    }
    out.into_iter().map(str::to_owned).collect()
}

/// QEMU user emulation's registered platforms (`/proc/sys/fs/binfmt_misc/qemu-*`, enabled).
fn binfmt() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/proc/sys/fs/binfmt_misc") else { return Vec::new() };
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let arch = name.strip_prefix("qemu-")?.to_owned();
            let text = std::fs::read_to_string(e.path()).ok()?;
            (text.lines().next() == Some("enabled")).then_some(arch)
        })
        .map(|arch| {
            match arch.as_str() {
                "aarch64" => "linux/arm64",
                "arm" => "linux/arm/v7",
                "x86_64" => "linux/amd64",
                "i386" => "linux/386",
                "riscv64" => "linux/riscv64",
                "ppc64le" => "linux/ppc64le",
                "s390x" => "linux/s390x",
                "mips64el" => "linux/mips64le",
                "loongarch64" => "linux/loong64",
                other => return format!("linux/{other}"),
            }
            .to_owned()
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Whether `/dev/kvm` is usable by this process, and nested virtualization is on.
fn kvm() -> (bool, bool) {
    let usable = cfg!(target_os = "linux") && std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").is_ok();
    let nested = ["/sys/module/kvm_intel/parameters/nested", "/sys/module/kvm_amd/parameters/nested"].iter().any(|p| std::fs::read_to_string(p).is_ok_and(|v| matches!(v.trim(), "Y" | "y" | "1")));
    (usable, nested)
}

/// The work directory's own file system (a `tmpfs` too, which `sysinfo`'s disks leave out): its
/// size and what this user may still write.
#[cfg(unix)]
#[allow(clippy::useless_conversion, reason = "the block counts are u32 on some systems, u64 on others")]
pub(crate) fn volume(dir: &Path) -> Option<Disk> {
    let s = nix::sys::statvfs::statvfs(dir).ok()?;
    let frag = u64::from(s.fragment_size());
    Some(Disk { total: u64::from(s.blocks()).saturating_mul(frag), free: u64::from(s.blocks_available()).saturating_mul(frag) })
}

#[cfg(not(unix))]
pub(crate) fn volume(_dir: &Path) -> Option<Disk> {
    None
}

/// The machine: its OS, CPU, memory, the work directory's volume, virtualization and emulation.
fn machine(work_dir: &Path) -> RunnerFacts {
    use sysinfo::{Disks, System};
    let mut sys = System::new();
    sys.refresh_cpu_all();
    sys.refresh_memory();
    let threads = u32::try_from(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)).unwrap_or(1);
    let cores = System::physical_core_count().and_then(|n| u32::try_from(n).ok()).unwrap_or(threads);
    let model = sys.cpus().first().map(|c| c.brand().trim().to_owned()).filter(|b| !b.is_empty());
    let dir = work_dir.canonicalize().unwrap_or_else(|_| work_dir.to_path_buf());
    let disk = volume(&dir).unwrap_or_else(|| {
        let disks = Disks::new_with_refreshed_list();
        disks.list().iter().filter(|d| dir.starts_with(d.mount_point())).max_by_key(|d| d.mount_point().as_os_str().len()).map(|d| Disk { total: d.total_space(), free: d.available_space() }).unwrap_or_default()
    });
    let (kvm, nested) = kvm();
    let arch = std::env::consts::ARCH.to_owned();
    RunnerFacts {
        os: std::env::consts::OS.to_owned(),
        os_version: System::os_version(),
        os_distro: cfg!(target_os = "linux").then(System::distribution_id).filter(|d| !d.is_empty()),
        kernel: System::kernel_version(),
        arch_variant: match arch.as_str() {
            "aarch64" => Some("v8".to_owned()),
            "arm" => Some("v7".to_owned()),
            _ => None,
        },
        arch,
        cpu: Cpu { model, cores, threads, flags: cpu_flags() },
        mem: Mem { total: sys.total_memory(), free: sys.available_memory() },
        disk,
        virt: Virt { kvm, hvf: false, nested },
        binfmt: binfmt(),
        ..RunnerFacts::default()
    }
}

/// Every fact of this runner, its probes run now.
pub async fn gather(work_dir: &Path, declared: &Declared) -> RunnerFacts {
    let dir = work_dir.to_path_buf();
    let mut f = crate::rt::blocking(move || machine(&dir)).await.unwrap_or_default();
    // Hypervisor.framework on a Mac: the kernel says whether it's supported.
    if cfg!(target_os = "macos") {
        f.virt.hvf = output("sysctl", &["-n", "kern.hv_support"]).await.is_some_and(|v| v.trim() == "1");
    }
    (f.executors, f.tools, f.gpus, f.net) = futures::join!(executors(), tools(), gpus(), net());
    f.custom = declared.facts.clone();
    for (name, script) in &declared.probes {
        let (program, args) = crate::os::default_step_shell(script);
        let mut c = std::process::Command::new(program);
        c.args(args);
        if let Ok(o) = crate::proc::output(c, Duration::from_secs(30)).await
            && o.status.success()
            && let Some(v) = version_in(&format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
        {
            f.custom.insert(name.clone(), v);
        }
    }
    f.kiln = Some(crate::updates::kiln_facts());
    f.workspaces = Some(crate::workspace::slates::facts().await);
    f.probed_at = crate::util::now_unix();
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w13_r01_versions() {
        let cases = [
            ("git version 2.47.1", Some("2.47.1")),
            ("go version go1.22.3 linux/amd64", Some("1.22.3")),
            ("v20.11.0\n", Some("20.11.0")),
            ("Python 3.12.4", Some("3.12.4")),
            ("openjdk version \"21.0.2\" 2024-01-16", Some("21.0.2")),
            ("github.com/docker/buildx v0.12.1 542e5d8", Some("0.12.1")),
            ("cargo 1.83.0 (5ffbef321 2024-10-29)", Some("1.83.0")),
            ("Firecracker v1.7.0\n", Some("1.7.0")),
            ("x86_64 nothing here", None),
        ];
        for (text, want) in cases {
            assert_eq!(version_in(text).as_deref(), want, "{text}");
        }
    }

    #[test]
    fn w13_r02_declared() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("kiln.toml");
        std::fs::write(&path, "[server]\nhttp_addr = \"0.0.0.0:1\"\n\n[agent.facts]\nlocation = \"rack-2\"\n\n[agent.probes]\ncuda = \"nvcc --version\"\n")?;
        let d = Declared::from_file(&path);
        assert_eq!((d.facts.get("location").map(String::as_str), d.probes.get("cuda").map(String::as_str)), (Some("rack-2"), Some("nvcc --version")));
        assert_eq!(Declared::from_file(&dir.path().join("none.toml")), Declared::default());
        Ok(())
    }
}
