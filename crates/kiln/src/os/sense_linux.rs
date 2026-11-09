//! Linux's reads for [`crate::sense`]: procfs and sysfs, each file read into a stack buffer no
//! larger than the lines wanted, nothing privileged.

use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use crate::sense::{CgroupRaw, PsiRaw, Raw, Space, parse};

/// Reads up to `buf.len()` bytes of `path`: the first lines of a procfs file without the rest.
fn read_into<'a>(path: &Path, buf: &'a mut [u8]) -> Option<&'a str> {
    let mut f = File::open(path).ok()?;
    let mut n = 0;
    while let Some(rest) = buf.get_mut(n..) {
        if rest.is_empty() {
            break;
        }
        match f.read(rest) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    std::str::from_utf8(buf.get(..n)?).ok()
}

fn small(path: &Path) -> Option<String> {
    let mut buf = [0u8; 512];
    read_into(path, &mut buf).map(str::to_owned)
}

fn psi_at(dir: &Path, cpu: &str, memory: &str, io: &str) -> PsiRaw {
    let f = |name: &str| {
        let mut buf = [0u8; 256];
        read_into(&dir.join(name), &mut buf).and_then(parse::psi)
    };
    PsiRaw { cpu: f(cpu), memory: f(memory), io: f(io) }
}

/// The runner's own cgroup v2 directory and its path; none in the root cgroup or on v1.
fn own_cgroup() -> Option<(String, PathBuf)> {
    let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = own.lines().find_map(|l| l.strip_prefix("0::"))?.trim().to_owned();
    if path == "/" || path.is_empty() {
        return None;
    }
    let dir = Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
    dir.join("cgroup.procs").exists().then_some((path, dir))
}

/// Logical CPUs this process may run on (its affinity mask).
fn cpus() -> u32 {
    let n = nix::sched::sched_getaffinity(nix::unistd::Pid::from_raw(0)).map(|set| (0..nix::sched::CpuSet::count()).filter(|i| set.is_set(*i).unwrap_or(false)).count()).unwrap_or(0);
    u32::try_from(n).ok().filter(|n| *n > 0).unwrap_or_else(|| u32::try_from(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)).unwrap_or(1))
}

/// kiln's other services beside the runner's own cgroup: `kiln*.service` in its parent, as kiln's
/// installer names its units (the gate's guard watches the same, `ci/gate.sh`).
fn sibling_services(own: &Path) -> Vec<(String, PathBuf)> {
    let Some(parent) = own.parent() else { return Vec::new() };
    let Ok(dir) = std::fs::read_dir(parent) else { return Vec::new() };
    let mut out: Vec<(String, PathBuf)> = dir
        .flatten()
        .map(|e| e.path())
        .filter(|p| p != own)
        .filter_map(|p| {
            let name = p.file_name()?.to_str()?.to_owned();
            (name.starts_with("kiln") && name.ends_with(".service") && p.join("io.pressure").exists()).then_some((name, p))
        })
        .collect();
    out.sort();
    out
}

pub struct Reader {
    work_dir: PathBuf,
    cgroup: Option<(String, PathBuf)>,
    services: Vec<(String, PathBuf)>,
    cpus: u32,
    device: Option<super::sense_disk_linux::Device>,
}

impl Reader {
    pub fn new(work_dir: &Path) -> Reader {
        let work_dir = crate::sense::canonical(work_dir);
        let device = super::sense_disk_linux::Device::of(&work_dir);
        let cgroup = own_cgroup();
        let services = cgroup.as_ref().map(|(_, d)| sibling_services(d)).unwrap_or_default();
        Reader { cgroup, services, cpus: cpus(), device, work_dir }
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    fn cgroup(&self) -> Option<CgroupRaw> {
        let (path, dir) = self.cgroup.as_ref()?;
        let root = Path::new("/sys/fs/cgroup");
        // The least limit of it and the cgroups above it (a slice's holds the services under it).
        let above = || dir.ancestors().take_while(|d| d.starts_with(root) && *d != root);
        let mem_limit = above().filter_map(|d| small(&d.join("memory.max"))).filter_map(|t| parse::limit(&t)).min();
        let cpu_limit = above().filter_map(|d| small(&d.join("cpu.max"))).filter_map(|t| parse::cpu_max(&t)).reduce(f64::min);
        let mem_used = small(&dir.join("memory.current")).and_then(|c| c.trim().parse::<u64>().ok()).map(|current| {
            // `inactive_file` is in memory.stat's first lines (anon, file, kernel, …, inactive_file).
            let mut buf = [0u8; 1024];
            let inactive = read_into(&dir.join("memory.stat"), &mut buf).and_then(|t| parse::keyed(t, "inactive_file")).unwrap_or(0);
            current.saturating_sub(inactive)
        });
        let cpu_usage_us = {
            let mut buf = [0u8; 64];
            read_into(&dir.join("cpu.stat"), &mut buf).and_then(|t| parse::keyed(t, "usage_usec"))
        };
        Some(CgroupRaw { path: path.clone(), psi: psi_at(dir, "cpu.pressure", "memory.pressure", "io.pressure"), mem_limit, mem_used, cpu_limit, cpu_usage_us })
    }

    pub fn read(&mut self) -> Raw {
        let cpu = {
            let mut buf = [0u8; 512];
            read_into(Path::new("/proc/stat"), &mut buf).and_then(parse::proc_stat_cpu)
        };
        let (mem_total, mem_available) = {
            let mut buf = [0u8; 256];
            let text = read_into(Path::new("/proc/meminfo"), &mut buf).unwrap_or_default();
            (parse::meminfo(text, "MemTotal").unwrap_or(0), parse::meminfo(text, "MemAvailable").unwrap_or(0))
        };
        let space = crate::os::disk_space(&self.work_dir).map(|(total, free)| Space { total, free, important: None });
        Raw {
            cpu,
            cpus: self.cpus,
            mem_total,
            mem_available,
            space,
            psi: psi_at(Path::new("/proc/pressure"), "cpu", "memory", "io"),
            cgroup: self.cgroup(),
            memory_level: None,
            device: self.device.as_mut().and_then(super::sense_disk_linux::Device::counters),
            services: self.services.iter().map(|(n, d)| (n.clone(), psi_at(d, "cpu.pressure", "memory.pressure", "io.pressure"))).collect(),
        }
    }
}

/// The work directory's storage facts (docs/needs.md, W13-R01).
pub fn storage(work_dir: &Path) -> Option<kiln_ci::facts::Storage> {
    super::sense_disk_linux::facts(work_dir)
}
