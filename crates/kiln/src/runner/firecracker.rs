//! The `firecracker` executor (docs/ci.md W13-R05): each leg in a Firecracker microVM of its own,
//! on a Linux host with KVM.
//!
//! - **Its disks.** kiln's own disk is the microVM's root file system: this kiln (with its loader
//!   and libraries when it's linked dynamically) on a small ext4, made once per build of kiln. The
//!   leg's image is an ext4 made from its OCI image (`rootfs.rs`), read-only and shared by every
//!   leg of that image. Its scratch disk, sparse and its own, takes what the leg writes: its
//!   workspace, its caches, and every change to the image's files.
//! - **Inside**, `kiln guest` (`guest.rs`) runs the leg as the `host` executor runs one; its events
//!   and logs come back over vsock and go to the server as this runner's.
//! - **Its network.** The microVM has no network device: its one way out is a vsock tunnel to the
//!   leg's egress proxy here (W18-R08), with the leg's policy, or to anywhere but this machine's
//!   own and link-local addresses when the leg has none. The server's port is forwarded too when
//!   the server is on this machine's loopback.
//! - **Still to come**: the jailer around each VMM, warm pools of booted snapshots, and terminals
//!   into a running microVM (W17-R01).

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;

use super::guest::{self, FromGuest, ToGuest};
use super::runtime::Leg;
use crate::proto::JobResult;
use crate::store::Status;

/// A leg's scratch disk: sparse, so only what's written takes room.
const SCRATCH: u64 = 32 << 30;
/// How long a microVM may take to boot and dial in.
const BOOT: Duration = Duration::from_secs(120);
/// How long a microVM may take to end once its leg's done.
const SHUTDOWN: Duration = Duration::from_secs(30);

/// What this host runs microVMs with.
#[derive(Debug, Clone)]
pub struct Machine {
    pub firecracker: PathBuf,
    /// The guest kernel (an uncompressed `vmlinux` with virtio block devices, vsock, ext4 and
    /// overlayfs built in, as Firecracker's own CI kernels are).
    pub kernel: PathBuf,
    /// `mke2fs`, which makes the disks.
    pub mkfs: PathBuf,
}

impl Machine {
    /// This host's, if it can run microVMs: KVM it may open, Firecracker (`KILN_FIRECRACKER`,
    /// else `~/.kiln/bin`, else `PATH`), a guest kernel (`KILN_FIRECRACKER_KERNEL`, else
    /// `~/.kiln/firecracker/vmlinux`) and e2fsprogs' `mke2fs`.
    pub fn find() -> Option<Machine> {
        if !cfg!(target_os = "linux") {
            return None;
        }
        std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").ok()?;
        let kiln = crate::os::home_dir().join(".kiln");
        let from_env = |k: &str| std::env::var_os(k).map(PathBuf::from).filter(|p| p.is_file());
        let firecracker = from_env("KILN_FIRECRACKER").or_else(|| Some(kiln.join("bin/firecracker")).filter(|p| p.is_file())).or_else(|| crate::os::which("firecracker"))?;
        let kernel = from_env("KILN_FIRECRACKER_KERNEL").or_else(|| Some(kiln.join("firecracker/vmlinux")).filter(|p| p.is_file()))?;
        let mkfs = crate::os::which("mke2fs").or_else(|| ["/usr/sbin/mke2fs", "/sbin/mke2fs"].into_iter().map(PathBuf::from).find(|p| p.is_file()))?;
        Some(Machine { firecracker, kernel, mkfs })
    }

    /// Firecracker's version (`v1.17.0`).
    pub fn version(&self) -> Option<String> {
        let o = Command::new(&self.firecracker).arg("--version").stdin(Stdio::null()).output().ok()?;
        String::from_utf8_lossy(&o.stdout).split_whitespace().find(|w| w.starts_with('v') && w.contains('.')).map(str::to_owned)
    }
}

/// An ext4 of `size` bytes (sparse) at `image`, with `from`'s files in it when given.
pub fn mkfs(m: &Machine, image: &Path, size: u64, label: &str, from: Option<&Path>) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(image);
    std::fs::File::create(image)?.set_len(size)?;
    let mut cmd = Command::new(&m.mkfs);
    cmd.args(["-q", "-F", "-t", "ext4", "-m", "0", "-L", label, "-E", "lazy_itable_init=1,lazy_journal_init=1,root_owner=0:0"]);
    if let Some(d) = from {
        cmd.arg("-d").arg(d);
    }
    let o = cmd.arg(image).stdin(Stdio::null()).output().context("running mke2fs")?;
    anyhow::ensure!(o.status.success(), "mke2fs {}: {}", image.display(), String::from_utf8_lossy(&o.stderr).trim());
    Ok(())
}

/// A microVM's vsock sockets, in a short directory of their own (removed with it): a Unix socket's
/// path must be shorter than 108 bytes, and a runner's work directory may be longer.
pub struct Socks(PathBuf);

impl Socks {
    pub fn new(key: &str) -> anyhow::Result<Socks> {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("kiln-vm-{key}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(Socks(dir))
    }

    /// Firecracker's own, for connections into the guest.
    pub fn uds(&self) -> PathBuf {
        self.0.join("v.sock")
    }

    /// Where the guest's connections to the host's `port` come.
    pub fn port(&self, port: u32) -> PathBuf {
        self.0.join(format!("v.sock_{port}"))
    }
}

impl Drop for Socks {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One of a microVM's disks, in the order the guest finds them (`/dev/vda`, `/dev/vdb`, …).
pub struct Drive<'a> {
    pub id: &'a str,
    pub path: &'a Path,
    pub read_only: bool,
}

/// A microVM booted from kiln's disk (the first drive) into `kiln guest <mode>`, its console
/// written to `console.log` in `dir`, its vsock in `socks`.
pub fn boot(m: &Machine, dir: &Path, socks: &Socks, drives: &[Drive<'_>], mode: &str, vcpus: u32, memory_mib: u64) -> anyhow::Result<Vm> {
    let config = serde_json::json!({
        "boot-source": {
            "kernel_image_path": m.kernel,
            "boot_args": format!("console=ttyS0 reboot=k panic=1 pci=off quiet root=/dev/vda ro rootfstype=ext4 init=/kiln -- guest {mode}"),
        },
        "drives": drives.iter().map(|d| serde_json::json!({ "drive_id": d.id, "path_on_host": d.path, "is_root_device": false, "is_read_only": d.read_only })).collect::<Vec<_>>(),
        "machine-config": { "vcpu_count": vcpus, "mem_size_mib": memory_mib },
        "vsock": { "guest_cid": 3, "uds_path": socks.uds() },
        "entropy": {},
    });
    let path = dir.join("vm.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?)?;
    let console = dir.join("console.log");
    let out = std::fs::File::create(&console)?;
    let child = Command::new(&m.firecracker)
        .arg("--no-api")
        .arg("--config-file")
        .arg(&path)
        .args(["--level", "Warning"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .spawn()
        .with_context(|| format!("starting {}", m.firecracker.display()))?;
    Ok(Vm { child: Some(child), console })
}

/// A running microVM's VMM: killed if it's dropped before it ended.
pub struct Vm {
    child: Option<Child>,
    pub console: PathBuf,
}

impl Vm {
    /// Whether it has ended.
    fn ended(&mut self) -> Option<std::process::ExitStatus> {
        self.child.as_mut().and_then(|c| c.try_wait().ok().flatten())
    }

    /// Its guest's connection to `l`, once it dials in within `within`.
    pub fn accept(&mut self, l: &UnixListener, within: Duration) -> anyhow::Result<UnixStream> {
        l.set_nonblocking(true)?;
        let deadline = Instant::now() + within;
        loop {
            match l.accept() {
                Ok((s, _)) => {
                    s.set_nonblocking(false)?;
                    return Ok(s);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
            if let Some(status) = self.ended() {
                anyhow::bail!("the microVM ended ({status}) before its guest dialled in:\n{}", tail(&self.console));
            }
            anyhow::ensure!(Instant::now() < deadline, "the microVM's guest didn't dial in within {within:?}:\n{}", tail(&self.console));
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// It ends itself once its guest is done; past `within` it's killed.
    pub fn finish(mut self, within: Duration) {
        let deadline = Instant::now() + within;
        while self.ended().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// The end of a console log, for saying why a microVM didn't do what it should.
pub fn tail(console: &Path) -> String {
    let text = std::fs::read(console).unwrap_or_default();
    let from = text.len().saturating_sub(4096);
    String::from_utf8_lossy(text.get(from..).unwrap_or_default()).trim().to_owned()
}

/// Each connection the guest makes to vsock `port`, spliced to `127.0.0.1:to` here, until `stop`.
fn relay(socks: &Socks, port: u32, to: u16, stop: Arc<AtomicBool>) -> anyhow::Result<()> {
    let l = UnixListener::bind(socks.port(port))?;
    l.set_nonblocking(true)?;
    std::thread::Builder::new().name(format!("vsock-{port}")).spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match l.accept() {
                Ok((s, _)) => {
                    let _ = std::thread::Builder::new().name(format!("vsock-{port}-conn")).spawn(move || {
                        if s.set_nonblocking(false).is_ok()
                            && let Ok(t) = std::net::TcpStream::connect(("127.0.0.1", to))
                        {
                            guest::splice(t, s);
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(25)),
                Err(_) => return,
            }
        }
    })?;
    Ok(())
}

/// kiln's own disk: this binary as `/kiln` and, when it's linked dynamically, its loader and
/// libraries where it looks for them, with the directories `kiln guest` mounts on. Made once per
/// build of kiln, kept in `cache`.
pub async fn kiln_disk(m: &Machine, cache: &Path) -> anyhow::Result<PathBuf> {
    let _one = super::rootfs::lock("kiln-disk").await;
    let (m, cache) = (m.clone(), cache.to_path_buf());
    crate::rt::blocking(move || make_kiln_disk(&m, &cache)).await?
}

fn make_kiln_disk(m: &Machine, cache: &Path) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;
    let exe = std::env::current_exe()?;
    let md = std::fs::metadata(&exe)?;
    let key = crate::util::sha256_hex(format!("{}:{}:{}:{}:{}", exe.display(), md.len(), md.mtime(), md.mtime_nsec(), md.ino()).as_bytes());
    let key = key.get(..16).unwrap_or("kiln");
    std::fs::create_dir_all(cache)?;
    let disk = cache.join(format!("kiln-{key}.ext4"));
    if disk.is_file() {
        return Ok(disk);
    }
    let staging = cache.join(format!("kiln-{key}.d"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let me = staging.join("kiln");
    if std::fs::hard_link(&exe, &me).is_err() {
        std::fs::copy(&exe, &me)?;
    }
    for (at, from) in libraries(&exe) {
        let to = staging.join(at.strip_prefix("/").unwrap_or(&at));
        if let Some(p) = to.parent() {
            std::fs::create_dir_all(p)?;
        }
        if !to.exists() {
            std::fs::copy(&from, &to).with_context(|| format!("copying {}", from.display()))?;
        }
    }
    for d in ["dev", "proc", "sys", "tmp", "mnt/image", "mnt/scratch", "mnt/root"] {
        std::fs::create_dir_all(staging.join(d))?;
    }
    let size = dir_size(&staging)?;
    let part = disk.with_extension("part");
    mkfs(m, &part, size + size / 8 + (64 << 20), "kiln", Some(&staging))?;
    std::fs::rename(&part, &disk)?;
    let _ = std::fs::remove_dir_all(&staging);
    // Earlier builds' disks.
    for e in std::fs::read_dir(cache)?.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.starts_with("kiln-") && n.ends_with(".ext4") && e.path() != disk {
            let _ = std::fs::remove_file(e.path());
        }
    }
    Ok(disk)
}

/// What a dynamically linked `exe` loads, as `ldd` says: where it's looked for, and the file
/// that is (the loader is asked for by the path the binary names). Nothing for a static build.
fn libraries(exe: &Path) -> Vec<(PathBuf, PathBuf)> {
    let Ok(o) = Command::new("ldd").arg(exe).stdin(Stdio::null()).output() else { return Vec::new() };
    let text = String::from_utf8_lossy(&o.stdout);
    if !o.status.success() || text.contains("not a dynamic executable") || text.contains("statically linked") {
        return Vec::new();
    }
    parse_ldd(&text)
}

/// `ldd`'s lines, as pairs of where each file is asked for and where it is.
fn parse_ldd(text: &str) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim) {
        // `libc.so.6 => /usr/lib/libc.so.6 (0x…)`, `/lib64/ld-linux-x86-64.so.2 => /usr/lib64/… (0x…)`
        let (left, right) = match line.split_once("=>") {
            Some((l, r)) => (l.trim(), r.split_whitespace().next()),
            None => (line.split_whitespace().next().unwrap_or_default(), None),
        };
        let found = right.filter(|r| r.starts_with('/')).or_else(|| left.starts_with('/').then_some(left));
        let Some(found) = found else { continue };
        out.push((PathBuf::from(found), PathBuf::from(found)));
        if left.starts_with('/') && left != found {
            out.push((PathBuf::from(left), PathBuf::from(found)));
        }
    }
    out
}

fn dir_size(dir: &Path) -> anyhow::Result<u64> {
    let mut total = 0u64;
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let m = e.metadata()?;
        total += if m.is_dir() { dir_size(&e.path())? + 4096 } else { m.len() + 4096 };
    }
    Ok(total)
}

/// The server's port, when the leg reaches it on this machine's loopback: the guest gets it on
/// its own.
fn loopback_port(rt: &kiln_ci::wire::Runtime) -> Option<u16> {
    let url = [&rt.results_url, &rt.facade_url, &rt.api_url, &rt.cache_url].into_iter().find(|u| u.contains("://"))?;
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let (host, port) = kiln_ci::trust::split_host(authority);
    let port = port.unwrap_or(if scheme.eq_ignore_ascii_case("https") { 443 } else { 80 });
    matches!(host.trim_start_matches('[').trim_end_matches(']'), "127.0.0.1" | "localhost" | "::1").then_some(port)
}

/// A leg in a microVM of its own; what stops it before its guest runs it fails it, saying why.
pub async fn run(leg: Leg, ca: Option<Vec<u8>>) -> JobResult {
    let job = leg.a.job_id.clone();
    match drive(&leg, ca).await {
        Ok(r) => r,
        Err(e) => {
            let why = format!("{e:#}");
            tracing::warn!(job = %job, error = %why, "a leg's microVM didn't run it");
            JobResult { job, status: Status::Failed, exit_code: None, error: Some(why), peak_rss: None, cpu_seconds: None }
        }
    }
}

async fn drive(leg: &Leg, ca: Option<Vec<u8>>) -> anyhow::Result<JobResult> {
    use crate::exec::Up;
    let m = crate::rt::blocking(Machine::find).await?.context("this runner can't run microVMs now: they need KVM, Firecracker, its guest kernel and e2fsprogs (`kiln runner setup firecracker`)")?;
    let a = &leg.a;
    let cache = leg.work_dir.join("_firecracker");
    let image = a.executor.image.clone().filter(|i| !i.trim().is_empty()).unwrap_or_else(|| crate::pipeline::toolchain_image(None).to_owned());
    let kiln = kiln_disk(&m, &cache).await.context("kiln's disk for the microVM")?;
    let root = super::rootfs::prepare(&m, &cache, &kiln, &image, a).await.with_context(|| format!("the image {image}"))?;
    let key = crate::util::sha256_hex(a.job_id.as_bytes()).get(..12).unwrap_or("leg").to_owned();
    let dir = cache.join("vms").join(&key);
    let socks = Socks::new(&key)?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let scratch = dir.join("scratch.ext4");
    {
        let (m, scratch) = (m.clone(), scratch.clone());
        crate::rt::blocking(move || mkfs(&m, &scratch, SCRATCH, "scratch", None)).await??;
    }
    // Its ways out: the leg's egress proxy here, and the server's own port when it's on loopback.
    let allow = a.network.as_ref().map(|n| n.allow.clone()).unwrap_or_else(|| vec!["*".to_owned()]);
    let (pool, proxy_port) = super::egress::Pool::local(super::egress::Policy::for_leg(&a.runtime, allow)).await?;
    let stop = Arc::new(AtomicBool::new(false));
    relay(&socks, guest::PROXY_PORT, proxy_port, Arc::clone(&stop))?;
    let server_port = loopback_port(&a.runtime);
    if let Some(p) = server_port {
        relay(&socks, guest::SERVER_PORT, p, Arc::clone(&stop))?;
    }
    let control = UnixListener::bind(socks.port(guest::CONTROL_PORT))?;
    let cpus = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, 4);
    let vcpus = u32::try_from(cpus).unwrap_or(2);
    let mut vm = boot(
        &m,
        &dir,
        &socks,
        &[Drive { id: "kiln", path: &kiln, read_only: true }, Drive { id: "image", path: &root.path, read_only: true }, Drive { id: "scratch", path: &scratch, read_only: false }],
        "init",
        vcpus,
        2048,
    )?;
    let (vm, conn) = crate::rt::blocking(move || {
        let conn = vm.accept(&control, BOOT);
        (vm, conn)
    })
    .await?;
    let conn = conn?;
    // Frames each way on threads of their own.
    let (from_tx, from_rx) = async_channel::unbounded::<FromGuest>();
    let (to_tx, to_rx) = async_channel::unbounded::<ToGuest>();
    let (mut reader, mut writer) = (conn.try_clone()?, conn);
    std::thread::Builder::new().name("microvm-in".into()).spawn(move || {
        while let Ok(Some(f)) = guest::recv::<FromGuest>(&mut reader) {
            if from_tx.send_blocking(f).is_err() {
                break;
            }
        }
    })?;
    std::thread::Builder::new().name("microvm-out".into()).spawn(move || {
        while let Ok(m) = to_rx.recv_blocking() {
            if guest::send(&mut writer, &m).is_err() {
                break;
            }
        }
    })?;
    anyhow::ensure!(matches!(from_rx.recv().await, Ok(FromGuest::Ready)), "the microVM's guest didn't say it was ready:\n{}", tail(&vm.console));
    let start = guest::LegStart { assignment: serde_json::to_string(a)?, runner_name: leg.runner_name.clone(), cache_max: leg.cache_max, env: root.env.clone(), ca, server_port };
    to_tx.send(ToGuest::Leg(Box::new(start))).await.map_err(|_| anyhow::anyhow!("the microVM's guest hung up"))?;
    // A cancel goes in; so does what the egress proxy refuses, as it does.
    let done = crate::rt::Shutdown::new();
    {
        let (cancel, done, to) = (leg.cancel.clone(), done.clone(), to_tx.clone());
        crate::rt::spawn(async move {
            let c = std::pin::pin!(cancel.cancelled());
            let d = std::pin::pin!(done.cancelled());
            if let futures::future::Either::Left(_) = futures::future::select(c, d).await {
                let _ = to.send(ToGuest::Cancel).await;
            }
        });
    }
    {
        let (done, to, pool) = (done.clone(), to_tx.clone(), Arc::new(pool));
        crate::rt::spawn(async move {
            while done.sleep(Duration::from_millis(500)).await {
                for b in pool.blocked() {
                    let _ = to.send(ToGuest::Blocked(b)).await;
                }
            }
        });
    }
    drop(to_tx);
    // Bounded here too: the leg's own timeout, its cleanup and a margin.
    let bound = Duration::from_secs(u64::from(a.timeout_minutes.max(1)) * 60 + 15 * 60);
    let relayed = crate::rt::timeout(bound, async {
        while let Ok(f) = from_rx.recv().await {
            match f {
                FromGuest::Msg(m) => leg.up.msg(m).await,
                FromGuest::Log(c) => {
                    let _ = leg.up.0.send(Up::Log(c)).await;
                }
                FromGuest::Term(m) => {
                    let _ = leg.up.0.send(Up::Term(m)).await;
                }
                FromGuest::Done(r) => return Some(r),
                FromGuest::Ready | FromGuest::Built(_) => {}
            }
        }
        None
    })
    .await;
    done.cancel();
    let console = vm.console.clone();
    let _ = crate::rt::blocking(move || vm.finish(SHUTDOWN)).await;
    stop.store(true, Ordering::Relaxed);
    let said = tail(&console);
    drop(socks);
    let _ = std::fs::remove_dir_all(&dir);
    match relayed {
        Ok(Some(r)) => Ok(r),
        Ok(None) => anyhow::bail!("the microVM ended before its leg did:\n{said}"),
        Err(_) => anyhow::bail!("the leg's microVM ran past {} minutes and was stopped:\n{said}", bound.as_secs() / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server's loopback port is forwarded into the guest; a server elsewhere is reached
    /// through the egress proxy.
    #[test]
    fn w13_r05_a_loopback_server_is_forwarded() {
        let rt = |u: &str| kiln_ci::wire::Runtime { results_url: u.into(), ..kiln_ci::wire::Runtime::default() };
        assert_eq!(loopback_port(&rt("http://127.0.0.1:7070/_kiln/results/")), Some(7070));
        assert_eq!(loopback_port(&rt("https://localhost/x")), Some(443));
        assert_eq!(loopback_port(&rt("http://[::1]:8080/")), Some(8080));
        assert_eq!(loopback_port(&rt("https://kiln.example.com/x")), None);
        assert_eq!(loopback_port(&rt("")), None);
    }

    /// `ldd`'s lines: each library where it's found, and the loader also where the binary asks
    /// for it.
    #[test]
    fn w13_r05_a_dynamic_kiln_brings_its_libraries() {
        let lines = "\tlinux-vdso.so.1 (0x00007ffd)\n\tlibc.so.6 => /usr/lib/libc.so.6 (0x00007f6a)\n\t/lib64/ld-linux-x86-64.so.2 => /usr/lib64/ld-linux-x86-64.so.2 (0x00007f6b)\n";
        let at: Vec<(String, String)> = parse_ldd(lines).into_iter().map(|(a, f)| (a.to_string_lossy().into_owned(), f.to_string_lossy().into_owned())).collect();
        let pair = |a: &str, f: &str| (a.to_owned(), f.to_owned());
        assert_eq!(
            at,
            [
                pair("/usr/lib/libc.so.6", "/usr/lib/libc.so.6"),
                pair("/usr/lib64/ld-linux-x86-64.so.2", "/usr/lib64/ld-linux-x86-64.so.2"),
                pair("/lib64/ld-linux-x86-64.so.2", "/usr/lib64/ld-linux-x86-64.so.2"),
            ]
        );
    }
}
