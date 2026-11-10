//! What a runner reports about itself (docs/ci.md W13-R01, R02): built-in probes, run when it
//! connects, when the server asks (`ServerMsg::ProbeFacts`), and again as what they're found from
//! changes ([`Asks`]: a folder they find tools in, a container engine starting or stopping, a run
//! failing for a tool not found; goal 9, never a clock), and what its administrator declares
//! (`[agent.facts]`, and `[agent.probes]` commands whose output is read as a version). Nothing
//! else is guessed: a fact a probe can't establish is left out.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// The most `kiln/cache@v1`'s local tier keeps in `<work>/_cache` (`cache_max = "20G"`; 10G
    /// when unset, docs/ci.md W08-R09).
    pub cache_max: Option<String>,
    /// The most agent sessions this machine has at work at once (`max_agent_sessions = 128`; from 1
    /// to 4096, kiln's default of 64 when unset): finished, paused and ended ones don't count.
    /// What's set for it on the server's Runners page wins over it (`crate::harness::limit`).
    pub max_agent_sessions: Option<usize>,
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

/// What `program args…` prints (out and err), when it runs and succeeds in time. It's looked for
/// where a leg's steps look (W13-R03): the runner's PATH, then the folders only its user's login
/// shell has (`crate::harness::kinds::search_path`).
async fn output(program: &str, args: &[&str]) -> Option<String> {
    let path = crate::os::which_in(program, &crate::harness::kinds::search_path()?)?;
    let mut c = std::process::Command::new(path);
    c.args(args);
    let o = crate::proc::output(c, TOOL_TIMEOUT).await.ok()?;
    o.status.success().then(|| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

/// The tools a runner's facts report when they're on `PATH`: each as its fact names it, the program
/// and the arguments that print its version.
pub const TOOLS: &[(&str, &str, &[&str])] = &[
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

/// Tools found on `PATH`, with the version each reports.
async fn tools() -> BTreeMap<String, String> {
    let found = futures::future::join_all(TOOLS.iter().map(|(name, program, args)| async move { output(program, args).await.as_deref().and_then(version_in).map(|v| ((*name).to_owned(), v)) })).await;
    found.into_iter().flatten().collect()
}

/// The executors this runner can run legs on right now, each probed: the host always; Firecracker
/// when its microVMs can run here (KVM, Firecracker, its guest kernel and e2fsprogs: W13-R05), with
/// its version and kernel; Docker (or Podman) when its engine answers, with its version and whether
/// it runs rootless. shards is reported as a tool until this runner can run legs on it. Docker
/// isn't one on Windows: a leg in it keeps the host's paths (`C:\…`), which its Linux container
/// has no place for (a job's own containers still run there).
async fn executors() -> BTreeMap<String, Executor> {
    let mut out = BTreeMap::new();
    // How each runs its legs beside what matters on the machine, and why not idle where they
    // can't be (docs/priority.md).
    let priority = crate::rt::blocking(crate::os::contain::priority).await.unwrap_or_default();
    out.insert("host".to_owned(), Executor { version: None, details: [("priority".to_owned(), priority.clone())].into() });
    #[cfg(target_os = "linux")]
    if let Ok(Some(m)) = crate::rt::blocking(super::firecracker::Machine::find).await {
        let kernel = m.kernel.to_string_lossy().into_owned();
        // How its VMMs are confined (W13-R05): a jail of namespaces each, or none here.
        let jail = if m.jail { "namespaces" } else { "none" }.to_owned();
        let version = crate::rt::blocking(move || m.version()).await.ok().flatten();
        out.insert("firecracker".to_owned(), Executor { version, details: [("kernel".to_owned(), kernel), ("jail".to_owned(), jail), ("priority".to_owned(), priority)].into() });
    }
    if cfg!(windows) {
        return out;
    }
    let Ok(Some(bin)) = crate::rt::blocking(super::docker::find).await else { return out };
    let engine = if super::docker::is_podman(&bin) { "podman" } else { "docker" };
    let mut version = None;
    for format in ["{{.Server.Version}}", "{{.Version}}"] {
        if let Some(v) = output(&bin, &["version", "--format", format]).await.as_deref().and_then(version_in) {
            version = Some(v);
            break;
        }
    }
    let rootless = crate::rt::blocking(super::docker::rootless_engine).await.unwrap_or(false);
    let priority = format!("the least CPU weight: --cpu-shares {} (cpu.weight 1)", super::docker::LEG_CPU_SHARES);
    let details = [("engine".to_owned(), engine.to_owned()), ("rootless".to_owned(), rootless.to_string()), ("priority".to_owned(), priority)].into();
    out.insert("docker".to_owned(), Executor { version, details });
    out
}

/// Retained native NVIDIA/AMD providers; macOS keeps its inventory-only system profiler.
async fn gpus() -> (Vec<Gpu>, Option<String>) {
    let (mut out, why) = match crate::rt::blocking(native_gpu_facts).await {
        Ok(Ok(cards)) => (cards, None),
        Ok(Err(why)) => (Vec::new(), Some(why)),
        Err(why) => (Vec::new(), Some(why.to_string())),
    };
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
            out.push(Gpu { vendor: vendor.into(), model, memory, driver: g.get("spdisplays_mtlgpufamilysupport").and_then(serde_json::Value::as_str).map(|m| m.trim_start_matches("spdisplays_").to_owned()), ..Gpu::default() });
        }
    }
    GPU_PRESENT.store(cfg!(any(target_os = "linux", target_os = "windows")) && !out.is_empty(), Ordering::Release);
    (out, why)
}

/// W13-R01: Hex's retained NVML / amdgpu / ADL providers, one instance per process.
/// Sampling uses installed drivers and retained handles; it starts no subprocess.
fn native_gpu_facts() -> Result<Vec<Gpu>, String> {
    static GPUS: std::sync::LazyLock<parking_lot::Mutex<kiln_gpu::Gpus>> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(kiln_gpu::Gpus::open()));
    let mut found = GPUS.lock();
    // A driver installed after startup is discovered on the next facts event, as Hex permits.
    if found.inventory().is_err() { *found = kiln_gpu::Gpus::open(); }
    let mut inventory = found.inventory().map(<[_]>::to_vec).map_err(ToString::to_string)?;
    let mut loads = found.sample().map(<[_]>::to_vec).map_err(ToString::to_string)?;
    if loads.iter().any(|l| l.busy_percent == Err(kiln_gpu::Unread::Gone) || l.memory_used_bytes == Err(kiln_gpu::Unread::Gone)) {
        *found = kiln_gpu::Gpus::open();
        let fresh = found.inventory().map(<[_]>::to_vec).map_err(ToString::to_string)?;
        inventory = fresh;
        let fresh = found.sample().map(<[_]>::to_vec).map_err(ToString::to_string)?;
        loads = fresh;
    }
    Ok(gpu_facts(&inventory, &loads))
}

fn gpu_facts(inventory: &[kiln_gpu::Gpu], loads: &[kiln_gpu::Load]) -> Vec<Gpu> {
    inventory.iter().enumerate().map(|(i, g)| {
        let load = loads.get(i);
        Gpu {
            vendor: match g.vendor { kiln_gpu::Vendor::Nvidia => "nvidia", kiln_gpu::Vendor::Amd => "amd" }.into(),
            model: g.model.clone(), memory: g.memory_total_bytes, driver: Some(g.driver_version.clone()), id: Some(g.pci.to_string()),
            busy_per_mille: load.and_then(|l| l.busy_percent.ok()).map(|p| u16::from(p) * 10),
            memory_used: load.and_then(|l| l.memory_used_bytes.ok()),
        }
    }).collect()
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
pub(crate) fn volume(dir: &Path) -> Option<Disk> {
    let (total, free) = crate::os::disk_space(dir)?;
    Some(Disk { total, free })
}

/// The machine's I/O pressure (docs/ci.md W24-R19): Linux's pressure stall information, the `full`
/// line's `avg10` of `/proc/pressure/io` (the share of the last 10 s in which all non-idle tasks
/// were stalled on I/O at once), in percent. `None` where the kernel doesn't report it.
fn io_pressure() -> Option<f64> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    std::fs::read_to_string("/proc/pressure/io").ok().as_deref().and_then(io_full)
}

/// `full avg10` of a PSI file (`some avg10=0.00 avg60=0.00 avg300=0.00 total=0`, then the same
/// for `full`), as a runner's samples read it too (`crate::metrics::psi_avg10`).
fn io_full(text: &str) -> Option<f64> {
    crate::metrics::psi_avg10(text, "full")
}

/// This machine's size, as its facts give it: its CPUs (threads) and memory. What a runner's own
/// admission counts what exclusive legs hold against (docs/ci.md W05-R03).
pub(crate) fn size() -> Option<kiln_ci::sched::state::Resources> {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    (threads > 0 && sys.total_memory() > 0).then(|| kiln_ci::sched::state::Resources { cpu: kiln_ci::sched::state::Cpus::whole(u64::try_from(threads).unwrap_or(u64::MAX)), mem: sys.total_memory(), ..Default::default() })
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
        mem: mem(sys.total_memory(), sys.available_memory(), crate::os::cgroup::memory_limit()),
        disk,
        virt: Virt { kvm, hvf: false, nested },
        binfmt: binfmt(),
        // Its load, for admission (W24-R19).
        work: Some({
            let p = crate::host::pressure();
            kiln_ci::facts::WorkFacts { io_full: io_pressure(), mem_full: p.mem_full, memory_level: p.memory.map(|l| l.as_str().to_owned()) }
        }),
        machine_id: crate::os::machine_id(),
        // What its work volume is made of: file system, devices, their kinds and health
        // (docs/needs.md).
        storage: crate::sense::storage(&dir),
        ..RunnerFacts::default()
    }
}

/// The memory this runner may use (W13-R01): the machine's, no more than its cgroups' limit (a
/// systemd scope's or slice's, a container's, a pod's), which the scheduler gives legs out of
/// in resource mode (W05-R03).
fn mem(total: u64, free: u64, limit: Option<u64>) -> Mem {
    let total = limit.map_or(total, |l| l.min(total));
    Mem { total, free: free.min(total) }
}

/// Every fact of this runner, its probes run now. `looked`: whether something asked for them that
/// may have changed the machine (a connection, the server's ask, a folder changing), not only a
/// run's tool not found ([`crate::sense::health::Health::probed_by`]).
pub async fn gather(work_dir: &Path, declared: &Declared, looked: bool) -> RunnerFacts {
    // Tools are found where legs' steps find them (`output`), the folders only the user's login
    // shell has among them: that shell is asked first, unless it answered since its startup files
    // last changed.
    crate::reach::serve::login_env().await;
    let dir = work_dir.to_path_buf();
    let mut f = crate::rt::blocking(move || machine(&dir)).await.unwrap_or_default();
    // A test build posing as a runner of another platform (`--chaos platform=`, docs/ci.md
    // W04-AC07): that platform's OS and architecture, and so the labels they give.
    if let Some(p) = super::chaos::get().and_then(|c| c.platform.as_deref()).and_then(|p| kiln_ci::plan::matrix::Platform::parse(p).ok()) {
        f.arch_variant = p.variant.clone().or_else(|| (p.fact_arch == "aarch64").then(|| "v8".to_owned()));
        (f.os, f.arch) = (p.fact_os.to_owned(), p.fact_arch.to_owned());
    }
    // Hypervisor.framework on a Mac: the kernel says whether it's supported.
    if cfg!(target_os = "macos") {
        f.virt.hvf = output("sysctl", &["-n", "kern.hv_support"]).await.is_some_and(|v| v.trim() == "1");
    }
    (f.executors, f.tools, (f.gpus, f.gpu_unavailable), f.net) = futures::join!(executors(), tools(), gpus(), net());
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
    f.kiln = Some(crate::updates::kiln_facts().await);
    f.workspaces = Some(crate::workspace::slates::facts().await);
    f.probed_at = crate::util::now_unix();
    // A tool a run here couldn't find, found now: work that needs it is offered again (S6).
    let path = crate::harness::kinds::search_path();
    if let Ok(mut h) = crate::sense::health::runner().lock() {
        h.probed_by(&|t| f.tools.contains_key(t) || path.as_ref().is_some_and(|p| crate::os::which_in(t, p).is_some()), looked);
    }
    f
}

// ------------------------------------------------------------------ what asks for them again

/// A run here failed for a tool not found (`crate::sense::run`, as `crate::sense::health` reads
/// its end): what the facts say of their tools may be wrong now (one removed), so they're probed
/// again (W13-R01; goal 9).
static MISSING: std::sync::LazyLock<crate::rt::Notify> = std::sync::LazyLock::new(crate::rt::Notify::new);

/// A run here failed for a tool not found: the facts are probed again.
pub fn tool_missing() {
    MISSING.notify();
}

/// W13-R01: coalesced workload-change events; a CPU-only host never invokes a GPU probe.
static GPU_CHANGED: std::sync::LazyLock<crate::rt::Notify> = std::sync::LazyLock::new(crate::rt::Notify::new);
static GPU_PRESENT: AtomicBool = AtomicBool::new(false);

pub fn gpu_changed() {
    if GPU_PRESENT.load(Ordering::Acquire) { GPU_CHANGED.notify(); }
}

pub async fn gpu_change() { GPU_CHANGED.notified().await; }

/// A runner's facts' probes, one at a time (W13-R01): an ask that comes while one runs gives one
/// more after it, a look if any of those asks was one.
#[derive(Debug, Default)]
pub struct Probing {
    busy: AtomicBool,
    again: AtomicBool,
    looked: AtomicBool,
    full: AtomicBool,
    facts: parking_lot::Mutex<Option<RunnerFacts>>,
}

impl Probing {
    /// Asked for a probe (a look at the machine anew when `looked`): whether this caller runs it,
    /// none running. One running takes the ask as its next.
    pub fn take(&self, looked: bool) -> bool {
        self.full.store(true, Ordering::SeqCst);
        self.claim(looked)
    }

    pub fn take_gpu(&self) -> bool { self.claim(false) }

    fn claim(&self, looked: bool) -> bool {
        if looked {
            self.looked.store(true, Ordering::SeqCst);
        }
        if self.busy.swap(true, Ordering::SeqCst) {
            // W13-R01: let a finisher contend while the next request is being published.
            #[cfg(test)]
            std::thread::yield_now();
            self.again.store(true, Ordering::SeqCst);
            return false;
        }
        true
    }

    /// The local CI admission reads the same retained inventory as placement.
    pub fn sized(&self, base: Option<kiln_ci::sched::state::Resources>) -> Option<kiln_ci::sched::state::Resources> {
        base.map(|mut size| {
            if let Some(facts) = self.facts.lock().as_ref() { size.gpu = u32::try_from(facts.gpus.len()).unwrap_or(u32::MAX); }
            size
        })
    }

    /// One publisher and one retained facts value. GPU events refresh counters and live room;
    /// a full probe requested meanwhile is never lost or overwritten by an older GPU snapshot.
    pub async fn read(&self, dir: &Path, declared: &Declared, looked: bool) -> RunnerFacts {
        let previous = self.facts.lock().clone();
        let (full, looked) = self.read_kind(looked);
        let facts = if full || previous.is_none() {
            gather(dir, declared, looked).await
        } else {
            let mut facts = previous.unwrap_or_default();
            let dir = dir.to_path_buf();
            if let Ok(fresh) = crate::rt::blocking(move || machine(&dir)).await {
                (facts.mem, facts.disk, facts.work, facts.storage) = (fresh.mem, fresh.disk, fresh.work, fresh.storage);
            }
            (facts.gpus, facts.gpu_unavailable) = gpus().await;
            facts.probed_at = crate::util::now_unix();
            facts
        };
        self.remember(&facts);
        facts
    }

    fn read_kind(&self, looked: bool) -> (bool, bool) {
        (self.full.swap(false, Ordering::SeqCst), looked)
    }

    fn remember(&self, facts: &RunnerFacts) {
        let mut retained = facts.clone();
        // Runtime observations are a drained batch, published once, not part of retained facts.
        retained.runs.clear();
        *self.facts.lock() = Some(retained);
    }

    /// The probe that ran ends: whether another is to run now (asked meanwhile, and taken here),
    /// and whether it's a look.
    pub fn done(&self) -> Option<bool> {
        self.busy.store(false, Ordering::SeqCst);
        if !self.again.swap(false, Ordering::SeqCst) || self.busy.swap(true, Ordering::SeqCst) {
            return None;
        }
        Some(self.looked.swap(false, Ordering::SeqCst))
    }

    /// Whether the probe about to run is a look (taken once).
    pub fn looks(&self) -> bool {
        self.looked.swap(false, Ordering::SeqCst)
    }
}

/// What asks for a runner's facts again as it changes (W13-R01; goal 9: it was every 10 minutes):
/// a change in a folder of the search path, where its tools are found (`crate::harness::kinds::
/// path_changed`, watched anew as the login shell's PATH changes); in kiln's own folders
/// (`~/.kiln/bin`'s Firecracker, its guest kernel); a container engine's socket made or gone (its
/// engine started or stopped: Docker's, Podman's, Colima's, `DOCKER_HOST`'s); `/dev/kvm` coming;
/// and a run failing for a tool not found.
pub struct Asks {
    own: Arc<crate::os::watch::Watched>,
    engines: Arc<crate::os::watch::Watched>,
}

impl Default for Asks {
    fn default() -> Self {
        Self::new()
    }
}

impl Asks {
    pub fn new() -> Asks {
        let home = crate::os::home_dir();
        let kiln = home.join(".kiln");
        // Firecracker's, its guest kernel and KVM: Linux's alone.
        let own_dirs = if cfg!(target_os = "linux") { vec![kiln.join("bin"), kiln.join("firecracker"), PathBuf::from("/dev")] } else { Vec::new() };
        let own = crate::os::watch::Watched::new(&own_dirs, false, &["firecracker", "vmlinux", "kvm"]);
        let (dirs, names) = engine_sockets(&home);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let engines = crate::os::watch::Watched::new(&dirs, false, &names);
        Asks { own, engines }
    }

    /// The next ask: whether it's a look at the machine anew (a folder changed), or a run's tool
    /// not found (`false`).
    pub async fn next(&self) -> bool {
        use futures::FutureExt as _;
        futures::select_biased! {
            () = MISSING.notified().fuse() => false,
            () = crate::harness::kinds::path_changed().fuse() => true,
            () = self.own.changed().fuse() => true,
            () = self.engines.changed().fuse() => true,
        }
    }

    /// The wakes its folders took (tests count an idle runner's).
    pub fn wakes(&self) -> u64 {
        self.own.wakes().saturating_add(self.engines.wakes())
    }
}

/// Where a container engine's socket is made as it starts (and goes as it stops): the folders, and
/// the sockets' names.
fn engine_sockets(home: &Path) -> (Vec<PathBuf>, Vec<String>) {
    if cfg!(windows) {
        return (Vec::new(), Vec::new());
    }
    let mut dirs = vec![PathBuf::from("/var/run"), PathBuf::from("/run"), PathBuf::from("/run/podman"), home.join(".docker").join("run"), home.join(".colima").join("default")];
    let mut names = vec!["docker.sock".to_owned(), "podman.sock".to_owned()];
    if let Some(run) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute()) {
        dirs.push(run.join("podman"));
        dirs.push(run);
    }
    if let Some(sock) = std::env::var("DOCKER_HOST").ok().and_then(|h| h.strip_prefix("unix://").map(PathBuf::from))
        && let (Some(dir), Some(name)) = (sock.parent(), sock.file_name())
    {
        dirs.push(dir.to_path_buf());
        names.push(name.to_string_lossy().into_owned());
    }
    (dirs, names)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W13-R01, goal 9: facts are probed one at a time, and what asks while one runs gives one
    /// more after it, a look if any of those asks was one; a run's tool not found tells the facts'
    /// asks at once. The planted mistakes these catch: a probe asked meanwhile let go (the old
    /// `busy` flag alone), and `tool_missing` telling nothing.
    #[test]
    fn w13_r01_g9_what_asks_while_a_probe_runs_gives_one_after_it() -> anyhow::Result<()> {
        let p = Probing::default();
        assert!(p.take(true), "idle: this ask runs it");
        assert!(p.looks(), "a look");
        assert!(!p.take(false), "one running takes the next ask");
        assert!(!p.take(false));
        assert_eq!(p.done(), Some(false), "one more after it, for both asks, not a look");
        assert!(!p.take(true), "the one after it runs: this ask is its next");
        assert_eq!(p.done(), Some(true), "a look asked meanwhile is one");
        assert_eq!(p.done(), None, "nothing asked since: done");
        assert!(p.take(false) && !p.looks(), "idle again; a run's tool not found is no look");
        assert_eq!(p.done(), None);
        crate::rt::block_on(async {
            while crate::rt::timeout(Duration::from_millis(200), MISSING.notified()).await.is_ok() {}
            tool_missing();
            anyhow::ensure!(crate::rt::timeout(Duration::from_secs(5), MISSING.notified()).await.is_ok(), "a tool not found told nothing");
            Ok(())
        })
    }

    /// W05-R03: a runner in a cgroup offers what the cgroup lets it use, not the machine's.
    #[test]
    fn w05_r03_a_runner_offers_its_cgroups_memory() {
        let g = 1u64 << 30;
        assert_eq!(mem(124 * g, 49 * g, Some(40 * g)), Mem { total: 40 * g, free: 40 * g }, "a 40G scope on a 124G machine");
        assert_eq!(mem(124 * g, 30 * g, Some(40 * g)), Mem { total: 40 * g, free: 30 * g });
        assert_eq!(mem(16 * g, 8 * g, Some(40 * g)), Mem { total: 16 * g, free: 8 * g }, "a limit over the machine's");
        assert_eq!(mem(124 * g, 49 * g, None), Mem { total: 124 * g, free: 49 * g }, "no limit");
    }

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
        assert_eq!(d.max_agent_sessions, None);
        assert_eq!(Declared::from_file(&dir.path().join("none.toml")), Declared::default());
        // Beside them, how many agent sessions it has at work at once (docs/agents.md).
        std::fs::write(&path, "[agent]\nmax_agent_sessions = 128\n\n[agent.facts]\nlocation = \"rack-2\"\n")?;
        let d = Declared::from_file(&path);
        assert_eq!((d.max_agent_sessions, d.facts.get("location").map(String::as_str)), (Some(128), Some("rack-2")));
        Ok(())
    }

    /// W24-R19: the I/O pressure a runner reports is PSI's `full avg10`.
    #[test]
    fn w24_r19_io_pressure() {
        let psi = "some avg10=12.50 avg60=8.00 avg300=2.10 total=123456\nfull avg10=7.25 avg60=3.00 avg300=1.00 total=65432\n";
        assert_eq!(io_full(psi), Some(7.25));
        assert_eq!(io_full("some avg10=1.00 avg60=0.00 avg300=0.00 total=1\n"), None, "a kernel without `full`");
        assert_eq!(io_full("full avg10=nan avg60=0.00\n"), None);
        assert_eq!(io_full(""), None);
        if cfg!(target_os = "linux") && std::path::Path::new("/proc/pressure/io").exists() {
            assert!(io_pressure().is_some_and(|p| (0.0..=100.0).contains(&p)), "{:?}", io_pressure());
        }
    }
}

#[cfg(test)]
mod gpu_port_tests {
    use super::*;
    #[test]
    fn w13_r01_native_gpu_facts_keep_identity_and_partial_counter_failure() -> Result<(), String> {
        let inventory = vec![kiln_gpu::Gpu { vendor: kiln_gpu::Vendor::Nvidia, model: "A100".into(), memory_total_bytes: 80 << 30, pci: kiln_gpu::PciAddress::parse("0000:01:00.0").ok_or("expected test value")?, driver_version: "550.54".into() }];
        let cards = gpu_facts(&inventory, &[kiln_gpu::Load { busy_percent: Err(kiln_gpu::Unread::Unsupported), memory_used_bytes: Ok(12 << 30) }]);
        let a = cards.first().ok_or("expected test value")?;
        assert_eq!(a.busy_per_mille, None);
        assert_eq!(a.memory_used, Some(12 << 30));
        assert_eq!(a.id.as_deref(), Some("0000:01:00.0"));
        let missing = gpu_facts(&inventory, &[]);
        assert_eq!(missing.first().ok_or("expected test value")?.memory, 80 << 30);
        assert_eq!(missing.first().ok_or("expected test value")?.memory_used, None);
        Ok(())
    }
}

#[cfg(test)]
mod gpu_events_tests {
    use super::*;
    #[test]
    fn w13_r01_gpu_events_and_full_probe_share_one_publisher() -> Result<(), String> {
        let p = Probing::default();
        assert!(p.take_gpu());
        assert!(!p.full.load(Ordering::SeqCst));
        assert!(!p.take(true)); // A full read arriving during GPU I/O is retained.
        assert!(p.full.load(Ordering::SeqCst));
        assert_eq!(p.done(), Some(true));
        assert_eq!(p.done(), None);
        *p.facts.lock() = Some(RunnerFacts { gpus: vec![Gpu::default(); 2], ..RunnerFacts::default() });
        let base = kiln_ci::sched::state::Resources { cpu: kiln_ci::sched::state::Cpus::whole(8), ..Default::default() };
        assert_eq!(p.sized(Some(base)).ok_or("expected test value")?.gpu, 2);
        assert_eq!(p.sized(Some(base)).ok_or("expected test value")?.cpu, base.cpu);
        let facts = RunnerFacts { runs: vec![kiln_ci::runtime::Observation::default()], ..RunnerFacts::default() };
        p.remember(&facts);
        assert_eq!(facts.runs.len(), 1);
        assert!(p.facts.lock().as_ref().ok_or("expected test value")?.runs.is_empty(), "GPU refreshes must not train the same observation twice");
        Ok(())
    }
}

#[cfg(test)]
mod gpu_publisher_handoff_tests {
    use super::*;

    #[test]
    fn w13_r01_gpu_publisher_keeps_a_look_requested_before_its_read() {
        let p = Probing::default();
        assert!(p.take_gpu());
        let looked = p.looks();
        assert!(!p.take(true));
        assert_eq!(p.read_kind(looked), (true, true), "the full read must include the pending look, even when the GPU worker started first");
    }

    #[test]
    fn w13_r01_gpu_publisher_never_strands_a_request_at_handoff() {
        use std::sync::{Barrier, atomic::AtomicBool};
        let p = Probing::default();
        let barrier = Barrier::new(3);
        let claimed = AtomicBool::new(false);
        let continued = AtomicBool::new(false);
        let rounds = 20_000;
        let mut wrong = 0;
        std::thread::scope(|scope| {
            let (shared, latch, claimed_out, continued_out) = (&p, &barrier, &claimed, &continued);
            scope.spawn(move || {
                for _ in 0..rounds {
                    latch.wait();
                    claimed_out.store(shared.take(true), Ordering::SeqCst);
                    latch.wait();
                }
            });
            scope.spawn(move || {
                for _ in 0..rounds {
                    latch.wait();
                    continued_out.store(shared.done().is_some(), Ordering::SeqCst);
                    latch.wait();
                }
            });
            for _ in 0..rounds {
                if !p.take(false) { wrong += 1; }
                let _ = p.read_kind(p.looks());
                barrier.wait();
                barrier.wait();
                if usize::from(claimed.load(Ordering::SeqCst)) + usize::from(continued.load(Ordering::SeqCst)) != 1 { wrong += 1; }
                // One active read and at most one coalesced request: two finishes drain both.
                let _ = p.done();
                let _ = p.done();
            }
        });
        assert_eq!(wrong, 0, "exactly one publisher must own the request after its predecessor finishes");
    }
}
