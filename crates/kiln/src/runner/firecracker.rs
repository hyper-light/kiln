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
//! - **Deployments** (W15-R02): a `microvm` environment's image, booted as a microVM of its own
//!   (`kiln guest serve`) that outlives its job. Its service is kept running and checked in there;
//!   this runner's ports reach it over vsock, and it takes them from the deployment before it once
//!   it's healthy. Kept in the cache, so the runner boots it again when it starts.
//! - **Still to come**: the jailer around each VMM, and warm pools of booted snapshots.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

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

/// Each connection the guest makes to vsock `port`, spliced to `127.0.0.1:<to>` here (turned away
/// while `to` is 0), until `stop`.
fn relay(socks: &Socks, port: u32, to: Arc<AtomicU16>, stop: Arc<AtomicBool>) -> anyhow::Result<()> {
    let l = UnixListener::bind(socks.port(port))?;
    l.set_nonblocking(true)?;
    std::thread::Builder::new().name(format!("vsock-{port}")).spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match l.accept() {
                Ok((s, _)) => {
                    let to = to.load(Ordering::Relaxed);
                    if to == 0 {
                        continue;
                    }
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
    let a = &leg.a;
    let image = a.executor.image.clone().filter(|i| !i.trim().is_empty()).unwrap_or_else(|| crate::pipeline::toolchain_image(None).to_owned());
    // Its pool's microVM, booted before it came (W13-R09), when that's of its image.
    let warm = a.executor.slot.as_deref().and_then(|s| WARM.lock().remove(s));
    let warm = match warm {
        Some(b) if b.image == image => Some(b),
        Some(b) => {
            crate::rt::spawn(b.discard());
            None
        }
        None => None,
    };
    let booted = match warm {
        Some(b) => b,
        None => {
            let m = crate::rt::blocking(Machine::find)
                .await?
                .context("this runner can't run microVMs now: they need KVM, Firecracker, its guest kernel and e2fsprogs (`kiln runner setup firecracker`)")?;
            let key = crate::util::sha256_hex(a.job_id.as_bytes()).get(..12).unwrap_or("leg").to_owned();
            boot_for(&m, &leg.work_dir.join("_firecracker"), &key, &image, &super::rootfs::Pull::of(a)).await?
        }
    };
    run_in(booted, leg, ca).await
}

/// A microVM booted for a leg, its guest dialled in and ready for one.
pub struct Booted {
    vm: Vm,
    socks: Socks,
    dir: PathBuf,
    conn: UnixStream,
    stop: Arc<AtomicBool>,
    /// Where its connections out go (the leg's egress proxy), once it has a leg.
    proxy_to: Arc<AtomicU16>,
    /// Where the server's port on its loopback goes, once it has a leg.
    server_to: Arc<AtomicU16>,
    image: String,
    /// The image's environment.
    env: Vec<(String, String)>,
}

impl Booted {
    /// Let go without a leg: its guest hung up on, its VMM stopped, its disks and sockets gone.
    async fn discard(self) {
        let Booted { vm, socks, dir, conn, stop, .. } = self;
        stop.store(true, Ordering::Relaxed);
        drop(conn);
        let _ = crate::rt::blocking(move || drop(vm)).await;
        drop(socks);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A microVM of `image`, booted and ready for a leg, as `key` (its directories' name).
async fn boot_for(m: &Machine, cache: &Path, key: &str, image: &str, pull: &super::rootfs::Pull) -> anyhow::Result<Booted> {
    let kiln = kiln_disk(m, cache).await.context("kiln's disk for the microVM")?;
    let root = super::rootfs::prepare(m, cache, &kiln, image, pull).await.with_context(|| format!("the image {image}"))?;
    let dir = cache.join("vms").join(key);
    let socks = Socks::new(key)?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let scratch = dir.join("scratch.ext4");
    {
        let (m, scratch) = (m.clone(), scratch.clone());
        crate::rt::blocking(move || mkfs(&m, &scratch, SCRATCH, "scratch", None)).await??;
    }
    // Its ways out, pointed where they go once it has a leg.
    let stop = Arc::new(AtomicBool::new(false));
    let (proxy_to, server_to) = (Arc::new(AtomicU16::new(0)), Arc::new(AtomicU16::new(0)));
    relay(&socks, guest::PROXY_PORT, Arc::clone(&proxy_to), Arc::clone(&stop))?;
    relay(&socks, guest::SERVER_PORT, Arc::clone(&server_to), Arc::clone(&stop))?;
    let control = UnixListener::bind(socks.port(guest::CONTROL_PORT))?;
    let cpus = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, 4);
    let vcpus = u32::try_from(cpus).unwrap_or(2);
    let vm = boot(
        m,
        &dir,
        &socks,
        &[Drive { id: "kiln", path: &kiln, read_only: true }, Drive { id: "image", path: &root.path, read_only: true }, Drive { id: "scratch", path: &scratch, read_only: false }],
        "init",
        vcpus,
        2048,
    )?;
    let (vm, conn) = crate::rt::blocking(move || {
        let mut vm = vm;
        let conn = vm.accept(&control, BOOT).and_then(|mut c| {
            c.set_read_timeout(Some(BOOT))?;
            match guest::recv::<FromGuest>(&mut c) {
                Ok(Some(FromGuest::Ready)) => {
                    c.set_read_timeout(None)?;
                    Ok(c)
                }
                _ => Err(anyhow::anyhow!("the microVM's guest didn't say it was ready:\n{}", tail(&vm.console))),
            }
        });
        (vm, conn)
    })
    .await?;
    Ok(Booted { vm, socks, dir, conn: conn?, stop, proxy_to, server_to, image: image.to_owned(), env: root.env })
}

/// A leg run in a microVM booted for it.
async fn run_in(b: Booted, leg: &Leg, ca: Option<Vec<u8>>) -> anyhow::Result<JobResult> {
    let a = &leg.a;
    let Booted { vm, socks, dir, conn, stop, proxy_to, server_to, env, .. } = b;
    // Its ways out: the leg's egress proxy here (W18-R08), and the server's own port when it's on
    // this machine's loopback.
    let allow = a.network.as_ref().map(|n| n.allow.clone()).unwrap_or_else(|| vec!["*".to_owned()]);
    let (pool, proxy_port) = super::egress::Pool::local(super::egress::Policy::for_leg(&a.runtime, allow)).await?;
    proxy_to.store(proxy_port, Ordering::Relaxed);
    let server_port = loopback_port(&a.runtime);
    server_to.store(server_port.unwrap_or(0), Ordering::Relaxed);
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
    let start = guest::LegStart { assignment: serde_json::to_string(a)?, runner_name: leg.runner_name.clone(), cache_max: leg.cache_max, env, ca, server_port };
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
    LEGS.lock().insert(a.job_id.clone(), to_tx.clone());
    drop(to_tx);
    // Bounded here too: the leg's own timeout, its cleanup and a margin.
    let bound = Duration::from_secs(u64::from(a.timeout_minutes.max(1)) * 60 + 15 * 60);
    let relayed = crate::rt::timeout(bound, async {
        while let Ok(f) = from_rx.recv().await {
            match f {
                FromGuest::Done(r) => return Some(r),
                FromGuest::Bye => return None,
                f => say(&leg.up, f).await,
            }
        }
        None
    })
    .await;
    let end = End { job: a.job_id.clone(), up: leg.up.clone(), vm, socks, stop, dir, done };
    match relayed {
        Ok(Some(r)) => {
            // A kept leg (W17-R02) keeps its microVM, with its terminals and its reruns: what it
            // says still goes up until it lets go, and the microVM ends then.
            crate::rt::spawn(async move {
                let _ = crate::rt::timeout(KEPT, async {
                    while let Ok(f) = from_rx.recv().await {
                        if matches!(f, FromGuest::Bye) {
                            break;
                        }
                        say(&end.up, f).await;
                    }
                })
                .await;
                end.finish().await;
            });
            Ok(r)
        }
        Ok(None) => {
            let said = end.finish().await;
            anyhow::bail!("the microVM ended before its leg did:\n{said}")
        }
        Err(_) => {
            let said = end.finish().await;
            anyhow::bail!("the leg's microVM ran past {} minutes and was stopped:\n{said}", bound.as_secs() / 60)
        }
    }
}

/// What a leg's microVM says, as this runner says it.
async fn say(up: &crate::exec::Uplink, f: FromGuest) {
    use crate::exec::Up;
    match f {
        FromGuest::Msg(m) => up.msg(m).await,
        FromGuest::Log(c) => {
            let _ = up.0.send(Up::Log(c)).await;
        }
        FromGuest::Term(m) => {
            let _ = up.0.send(Up::Term(m)).await;
        }
        FromGuest::Ready | FromGuest::Done(_) | FromGuest::Built(_) | FromGuest::Bye | FromGuest::Health { .. } | FromGuest::Output(_) | FromGuest::Exited { .. } => {}
    }
}

/// How long a kept leg's microVM may stay after its result: as long as a leg is kept, and an hour.
const KEPT: Duration = Duration::from_secs((super::kept::MAX_KEEP_MINUTES as u64 + 60) * 60);

/// A leg's microVM and what it holds, until it's done with.
struct End {
    job: String,
    up: crate::exec::Uplink,
    vm: Vm,
    socks: Socks,
    stop: Arc<AtomicBool>,
    dir: PathBuf,
    done: crate::rt::Shutdown,
}

impl End {
    /// The microVM ended (or stopped), its terminals said to be over, its disks and sockets gone:
    /// the end of its console.
    async fn finish(self) -> String {
        self.done.cancel();
        LEGS.lock().remove(&self.job);
        let ended: Vec<String> = {
            let mut sessions = SESSIONS.lock();
            let ids: Vec<String> = sessions.iter().filter(|(_, j)| **j == self.job).map(|(s, _)| s.clone()).collect();
            for s in &ids {
                sessions.remove(s);
            }
            ids
        };
        for session in ended {
            let _ = self.up.0.send(crate::exec::Up::Term(crate::proto::AgentMsg::TermExit { session, code: None, error: Some("the leg's microVM ended".into()) })).await;
        }
        let console = self.vm.console.clone();
        let vm = self.vm;
        let _ = crate::rt::blocking(move || vm.finish(SHUTDOWN)).await;
        self.stop.store(true, Ordering::Relaxed);
        let said = tail(&console);
        drop(self.socks);
        let _ = std::fs::remove_dir_all(&self.dir);
        said
    }
}

/// A `firecracker` pool's microVMs (W13-R09), booted and waiting for a leg each, by executor.
static WARM: std::sync::LazyLock<parking_lot::Mutex<HashMap<String, Booted>>> = std::sync::LazyLock::new(Default::default);

/// Starts pool executor `ask.executor`: a microVM of the pool's image, booted and waiting for its
/// leg, which starts at once when it comes.
pub async fn start_warm(work: &Path, ask: &super::pool::Ask) -> anyhow::Result<()> {
    let m = crate::rt::blocking(Machine::find).await?.context("this runner can't run microVMs now: they need KVM, Firecracker, its guest kernel and e2fsprogs")?;
    let image = ask.image.clone().filter(|i| !i.trim().is_empty()).context("the pool names no image")?;
    let pull = super::rootfs::Pull { origin: ask.registry.clone().unwrap_or_default(), token: ask.pull_token.clone().unwrap_or_default(), mirrors: Vec::new() };
    let key = format!("p{}", crate::util::sha256_hex(ask.executor.as_bytes()).get(..11).unwrap_or("ool"));
    let b = boot_for(&m, &work.join("_firecracker"), &key, &image, &pull).await?;
    tracing::info!(executor = %ask.executor, pool = %ask.pool, image = %image, "pool microVM ready");
    WARM.lock().insert(ask.executor.clone(), b);
    Ok(())
}

/// A pool's microVM let go (its pool stopped it).
pub async fn discard_warm(id: &str) {
    let b = WARM.lock().remove(id);
    if let Some(b) = b {
        b.discard().await;
    }
}

/// Every pool microVM here let go: the server forgot them (a new connection).
pub async fn drain_warm() {
    let all: Vec<Booted> = WARM.lock().drain().map(|(_, b)| b).collect();
    for b in all {
        b.discard().await;
    }
}

/// The legs running (or kept) in microVMs here, by leg: what goes in to each.
static LEGS: std::sync::LazyLock<parking_lot::Mutex<HashMap<String, async_channel::Sender<ToGuest>>>> = std::sync::LazyLock::new(Default::default);
/// Terminals into those legs (W17-R01), by session: their leg.
static SESSIONS: std::sync::LazyLock<parking_lot::Mutex<HashMap<String, String>>> = std::sync::LazyLock::new(Default::default);

/// `m` sent in to leg `job`'s microVM: `false` when it doesn't run in one here.
pub fn to_leg(job: &str, m: ToGuest) -> bool {
    let tx = LEGS.lock().get(job).cloned();
    tx.is_some_and(|tx| tx.try_send(m).is_ok())
}

/// A terminal into leg `job` (or a command run there), opened in its microVM when it runs in one
/// here; `false` when it doesn't.
pub fn open_terminal(job: &str, session: &str, cols: u16, rows: u16, command: Option<String>) -> bool {
    if !LEGS.lock().contains_key(job) {
        return false;
    }
    SESSIONS.lock().insert(session.to_owned(), job.to_owned());
    to_leg(job, ToGuest::TermOpen { session: session.to_owned(), cols, rows, command })
}

/// Whether terminal `session` is into a microVM's leg here.
pub fn in_microvm(session: &str) -> bool {
    SESSIONS.lock().contains_key(session)
}

/// `m` for terminal `session`, sent in when it's into a microVM's leg here; `false` when it isn't.
pub fn to_session(session: &str, m: ToGuest) -> bool {
    let job = SESSIONS.lock().get(session).cloned();
    let Some(job) = job else { return false };
    if matches!(m, ToGuest::TermClose { .. }) {
        SESSIONS.lock().remove(session);
    }
    to_leg(&job, m);
    true
}

/// A `microvm` environment's deployment (W15-R02), as its step asked for it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Deployment {
    /// `<repository>-<environment>`: one at a time of each here.
    pub project: String,
    pub image: String,
    /// What runs in place of the image's command (after its entrypoint, as Docker's).
    pub command: Option<Vec<String>>,
    pub env: Vec<(String, String)>,
    /// `(address, port here, port in the microVM)`.
    pub ports: Vec<(String, u16, u16)>,
    /// In place of the image's own `HEALTHCHECK`.
    pub health: Option<guest::Health>,
    pub vcpus: u32,
    pub memory_mib: u64,
    /// The hosts it may reach (W18-R08), `*` for anywhere but this machine's own addresses.
    pub allow: Vec<String>,
}

/// What a deployment's step hears as it goes: how it's going, and what its service writes.
pub enum Said {
    Note(String),
    Output(Vec<u8>),
}

/// A deployment as it's kept here: what was asked, and what it boots from (its image's root file
/// system, its service), so it's booted again when this runner starts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Kept {
    deployment: Deployment,
    root: PathBuf,
    service: guest::Service,
}

/// A deployment's microVM, running.
struct Live {
    vm: Vm,
    socks: Socks,
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    to: async_channel::Sender<ToGuest>,
    /// Its way out (W18-R08), as long as it runs.
    _egress: super::egress::Pool,
    /// Which of the deployments' microVMs here it is.
    n: u64,
    booted: Instant,
}

impl Live {
    /// Its service stopped and its microVM ended, its disks and sockets gone: the end of its
    /// console.
    async fn end(self) -> String {
        let Live { vm, socks, dir, stop, to, .. } = self;
        let _ = to.send(ToGuest::Stop).await;
        let console = vm.console.clone();
        let _ = crate::rt::blocking(move || vm.finish(SHUTDOWN)).await;
        stop.store(true, Ordering::Relaxed);
        let said = tail(&console);
        drop(socks);
        let _ = std::fs::remove_dir_all(&dir);
        said
    }
}

/// The deployments running here, by project.
static DEPLOYED: std::sync::LazyLock<parking_lot::Mutex<HashMap<String, Live>>> = std::sync::LazyLock::new(Default::default);
/// The ports here deployments are reached on, by address and port.
static PORTS: std::sync::LazyLock<parking_lot::Mutex<HashMap<(String, u16), Arc<Door>>>> = std::sync::LazyLock::new(Default::default);
/// The microVMs deployments have had here, counted.
static BOOTED: AtomicU64 = AtomicU64::new(0);

/// A port here a deployment is reached on: whose it is, and where its connections go now (a
/// microVM's vsock and the guest's port for its service's), turned away while that's nowhere.
struct Door {
    project: String,
    to: parking_lot::Mutex<Option<(PathBuf, u32)>>,
    stop: AtomicBool,
}

/// `address:port` opened for `project`'s deployment.
fn open_door(address: &str, port: u16, project: &str) -> anyhow::Result<Arc<Door>> {
    let l = std::net::TcpListener::bind((address, port)).with_context(|| format!("listening on {address}:{port}"))?;
    l.set_nonblocking(true)?;
    let door = Arc::new(Door { project: project.to_owned(), to: parking_lot::Mutex::new(None), stop: AtomicBool::new(false) });
    let d = Arc::clone(&door);
    std::thread::Builder::new().name(format!("deployed-{port}")).spawn(move || {
        while !d.stop.load(Ordering::Relaxed) {
            match l.accept() {
                Ok((c, _)) => {
                    let Some((uds, vport)) = d.to.lock().clone() else { continue };
                    let _ = std::thread::Builder::new().name(format!("deployed-{port}-conn")).spawn(move || {
                        if c.set_nonblocking(false).is_ok()
                            && let Ok(v) = connect_in(&uds, vport)
                        {
                            guest::splice(c, v);
                        }
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(25)),
                Err(_) => std::thread::sleep(Duration::from_millis(250)),
            }
        }
    })?;
    Ok(door)
}

/// A connection into the microVM whose vsock is `uds`, to the guest's vsock `port` (Firecracker's
/// `CONNECT <port>`, answered `OK <port>`).
fn connect_in(uds: &Path, port: u32) -> anyhow::Result<UnixStream> {
    let mut s = UnixStream::connect(uds)?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.write_all(format!("CONNECT {port}\n").as_bytes())?;
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while line.len() < 64 {
        s.read_exact(&mut b)?;
        if b == *b"\n" {
            break;
        }
        line.extend_from_slice(&b);
    }
    anyhow::ensure!(line.starts_with(b"OK "), "the microVM turned the connection away: {}", String::from_utf8_lossy(&line));
    s.set_read_timeout(None)?;
    Ok(s)
}

/// `d`'s ports: those its project has here already, and the rest opened now (the second of each
/// pair says which). One another project has, or one this machine can't open, fails it.
fn doors_for(d: &Deployment) -> anyhow::Result<Vec<(Arc<Door>, bool)>> {
    let mut doors: Vec<(Arc<Door>, bool)> = Vec::new();
    let fail = |doors: &[(Arc<Door>, bool)], e: anyhow::Error| {
        close_new(doors);
        Err(e)
    };
    for (address, port, _) in &d.ports {
        let had = PORTS.lock().get(&(address.clone(), *port)).cloned();
        match had {
            Some(door) if door.project == d.project => doors.push((door, false)),
            Some(door) => return fail(&doors, anyhow::anyhow!("{address}:{port} here is {}'s", door.project)),
            None => match open_door(address, *port, &d.project) {
                Ok(door) => {
                    PORTS.lock().insert((address.clone(), *port), Arc::clone(&door));
                    doors.push((door, true));
                }
                Err(e) => return fail(&doors, e),
            },
        }
    }
    Ok(doors)
}

/// The ports `doors_for` opened, closed again: the deployment didn't come up.
fn close_new(doors: &[(Arc<Door>, bool)]) {
    let mut ports = PORTS.lock();
    for (door, new) in doors {
        if *new {
            door.stop.store(true, Ordering::Relaxed);
            ports.retain(|_, d| !Arc::ptr_eq(d, door));
        }
    }
}

/// A deployment's service: the image's entrypoint and command (the step's in place of the
/// image's), environment, directory, user and health check, with the step's.
fn service_of(d: &Deployment, root: &super::rootfs::Rootfs) -> guest::Service {
    let mut argv = root.runs.entrypoint.clone();
    argv.extend(d.command.clone().unwrap_or_else(|| root.runs.cmd.clone()));
    let mut env = root.env.clone();
    env.extend(d.env.iter().cloned());
    guest::Service {
        argv,
        env,
        cwd: root.runs.workdir.clone(),
        user: root.runs.user.clone(),
        health: d.health.clone().or_else(|| root.runs.health.clone()),
        ports: d.ports.iter().map(|(_, _, p)| *p).collect(),
    }
}

/// Where deployments are kept.
fn kept_dir(cache: &Path) -> PathBuf {
    cache.join("deployments")
}

/// Deploys `d` (W15-R02): its image pulled (with `pull`), its microVM booted with its service and
/// followed (`said` hears how it goes and what the service writes) until it's healthy, within
/// `within`; then it takes the ports from the deployment before it, which ends, and is kept. One
/// that doesn't come up healthy ends, and the one before goes on.
pub async fn deploy(cache: &Path, d: Deployment, pull: &super::rootfs::Pull, said: async_channel::Sender<Said>, within: Duration) -> anyhow::Result<()> {
    let note = |s: String| {
        let _ = said.try_send(Said::Note(s));
    };
    let m = crate::rt::blocking(Machine::find).await?.context("this runner can't run microVMs now: they need KVM, Firecracker, its guest kernel and e2fsprogs (`kiln runner setup firecracker`)")?;
    let _ = CACHE.set(cache.to_path_buf());
    let _one = super::rootfs::lock(&format!("deploy-{}", d.project)).await;
    let doors = doors_for(&d)?;
    let started = async {
        let kiln = kiln_disk(&m, cache).await.context("kiln's disk for the microVM")?;
        note(format!("Its image, {}", d.image));
        let root = super::rootfs::prepare(&m, cache, &kiln, &d.image, pull).await.with_context(|| format!("the image {}", d.image))?;
        let kept = Kept { service: service_of(&d, &root), deployment: d.clone(), root: root.path };
        anyhow::ensure!(!kept.service.argv.is_empty(), "the image has no entrypoint or command: say what runs (`command`)");
        note(format!("Booting its microVM: {}", kept.service.argv.join(" ")));
        let (live, from) = boot_deployment(&m, cache, &kiln, &kept).await?;
        Ok((kept, live, from))
    };
    let (kept, live, from) = match started.await {
        Ok(x) => x,
        Err(e) => {
            close_new(&doors);
            return Err(e);
        }
    };
    let healthy = crate::rt::timeout(within, async {
        while let Ok(f) = from.recv().await {
            match f {
                FromGuest::Health { healthy: true, said: why } => {
                    note(format!("Healthy: {why}"));
                    return Ok(());
                }
                FromGuest::Health { healthy: false, said: why } => note(format!("Not healthy yet: {why}")),
                FromGuest::Output(o) => {
                    let _ = said.try_send(Said::Output(o));
                }
                FromGuest::Exited { code, again_secs } => {
                    note(format!("Its service ended ({}); it starts again in {again_secs}s", code.map_or_else(|| "on a signal".to_owned(), |c| format!("exit {c}"))))
                }
                FromGuest::Bye => return Err(anyhow::anyhow!("its microVM ended")),
                _ => {}
            }
        }
        Err(anyhow::anyhow!("its microVM's guest hung up"))
    })
    .await;
    let failed = match healthy {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e),
        Err(_) => Some(anyhow::anyhow!("it wasn't healthy within {}s", within.as_secs())),
    };
    if let Some(e) = failed {
        close_new(&doors);
        let console = live.end().await;
        note("It didn't come up: the deployment before it, if there is one, goes on".to_owned());
        return Err(anyhow::anyhow!("{e:#}\n\nIts microVM's console ended:\n{console}"));
    }
    let old = install(&kept, live, from, &doors);
    if let Some(old) = old {
        note("The deployment before it ends".to_owned());
        old.end().await;
    }
    // Kept for this runner only: its environment may hold secrets.
    let dir = kept_dir(cache);
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{}.json", d.project));
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&file)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(&serde_json::to_vec_pretty(&kept)?)?;
    }
    Ok(())
}

/// `live` in place of its project's deployment before it (which it returns, to end): the ports
/// point at it, those the one before had and it doesn't close, and what it says from here on is
/// followed.
fn install(kept: &Kept, live: Live, from: async_channel::Receiver<FromGuest>, doors: &[(Arc<Door>, bool)]) -> Option<Live> {
    let project = kept.deployment.project.clone();
    for ((door, _), (_, _, port)) in doors.iter().zip(&kept.deployment.ports) {
        *door.to.lock() = Some((live.socks.uds(), guest::INBOUND + u32::from(*port)));
    }
    {
        let mut ports = PORTS.lock();
        ports.retain(|_, d| {
            let keep = d.project != project || doors.iter().any(|(o, _)| Arc::ptr_eq(o, d));
            if !keep {
                d.stop.store(true, Ordering::Relaxed);
            }
            keep
        });
    }
    let n = live.n;
    let old = DEPLOYED.lock().insert(project.clone(), live);
    crate::rt::spawn(follow(project, n, from));
    old
}

/// What a deployment's microVM says once it's up: how its health goes (logged), and its end, when
/// that comes before it's replaced (booted again then, from what's kept).
async fn follow(project: String, n: u64, from: async_channel::Receiver<FromGuest>) {
    while let Ok(f) = from.recv().await {
        match f {
            FromGuest::Health { healthy, said } => tracing::info!(deployment = %project, healthy, said = %said, "a deployment's health"),
            FromGuest::Exited { code, again_secs } => tracing::warn!(deployment = %project, code = ?code, again_secs, "a deployment's service ended"),
            FromGuest::Bye => break,
            _ => {}
        }
    }
    let ended = {
        let mut deployed = DEPLOYED.lock();
        if deployed.get(&project).is_some_and(|l| l.n == n) { deployed.remove(&project) } else { None }
    };
    let Some(live) = ended else { return };
    // Booted again, unless it ended soon after booting: then it waits for its next deployment, or
    // for this runner to start again.
    let lasted = live.booted.elapsed();
    let said = live.end().await;
    if lasted < Duration::from_secs(60) {
        tracing::warn!(deployment = %project, console = %said, "a deployment's microVM ended within a minute of booting: it stays down until it's deployed again");
        return;
    }
    tracing::warn!(deployment = %project, console = %said, "a deployment's microVM ended: booting it again");
    crate::rt::sleep(Duration::from_secs(5)).await;
    let Some(cache) = CACHE.get() else { return };
    let file = kept_dir(cache).join(format!("{project}.json"));
    let kept = std::fs::read(&file).map_err(anyhow::Error::from).and_then(|b| Ok(serde_json::from_slice::<Kept>(&b)?));
    if let Err(e) = match kept {
        Ok(kept) => relaunch(cache, kept).await,
        Err(e) => Err(e),
    } {
        tracing::warn!(deployment = %project, error = %format!("{e:#}"), "a deployment couldn't be booted again");
    }
}

/// Where this runner keeps its microVMs, once it has booted a deployment again.
static CACHE: OnceLock<PathBuf> = OnceLock::new();

/// The deployments kept in `cache` (W15-R02), booted again: this runner has started.
pub async fn restore(cache: PathBuf) {
    let _ = CACHE.set(cache.clone());
    let Ok(entries) = std::fs::read_dir(kept_dir(&cache)) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let kept = match std::fs::read(&path).map_err(anyhow::Error::from).and_then(|b| Ok(serde_json::from_slice::<Kept>(&b)?)) {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %format!("{e:#}"), "a kept deployment doesn't read");
                continue;
            }
        };
        let project = kept.deployment.project.clone();
        match relaunch(&cache, kept).await {
            Ok(()) => tracing::info!(deployment = %project, "booted a deployment again"),
            Err(e) => tracing::warn!(deployment = %project, error = %format!("{e:#}"), "a deployment couldn't be booted again"),
        }
    }
}

/// A kept deployment booted again, from its image's root file system as it was deployed.
async fn relaunch(cache: &Path, kept: Kept) -> anyhow::Result<()> {
    let m = crate::rt::blocking(Machine::find).await?.context("this runner can't run microVMs now")?;
    let _one = super::rootfs::lock(&format!("deploy-{}", kept.deployment.project)).await;
    anyhow::ensure!(kept.root.is_file(), "its image's root file system ({}) is gone: deploy it again", kept.root.display());
    let doors = doors_for(&kept.deployment)?;
    let booted = async {
        let kiln = kiln_disk(&m, cache).await.context("kiln's disk for the microVM")?;
        boot_deployment(&m, cache, &kiln, &kept).await
    };
    let (live, from) = match booted.await {
        Ok(x) => x,
        Err(e) => {
            close_new(&doors);
            return Err(e);
        }
    };
    if let Some(old) = install(&kept, live, from, &doors) {
        old.end().await;
    }
    Ok(())
}

/// A deployment's microVM booted from what's `kept`, its guest given its service: it, and what it
/// says.
async fn boot_deployment(m: &Machine, cache: &Path, kiln: &Path, kept: &Kept) -> anyhow::Result<(Live, async_channel::Receiver<FromGuest>)> {
    let n = BOOTED.fetch_add(1, Ordering::Relaxed);
    let key = format!("d{}", crate::util::sha256_hex(format!("{}:{n}", kept.deployment.project).as_bytes()).get(..11).unwrap_or("eployed"));
    let dir = cache.join("vms").join(&key);
    let socks = Socks::new(&key)?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let scratch = dir.join("scratch.ext4");
    {
        let (m, scratch) = (m.clone(), scratch.clone());
        crate::rt::blocking(move || mkfs(&m, &scratch, SCRATCH, "scratch", None)).await??;
    }
    // Its way out: an egress proxy of its own, with its policy.
    let (egress, proxy) = super::egress::Pool::local(super::egress::Policy::new(kept.deployment.allow.clone(), &[])).await?;
    let stop = Arc::new(AtomicBool::new(false));
    relay(&socks, guest::PROXY_PORT, Arc::new(AtomicU16::new(proxy)), Arc::clone(&stop))?;
    let control = UnixListener::bind(socks.port(guest::CONTROL_PORT))?;
    let d = &kept.deployment;
    let vm = boot(
        m,
        &dir,
        &socks,
        &[Drive { id: "kiln", path: kiln, read_only: true }, Drive { id: "image", path: &kept.root, read_only: true }, Drive { id: "scratch", path: &scratch, read_only: false }],
        "serve",
        d.vcpus.max(1),
        d.memory_mib.max(128),
    )?;
    let service = kept.service.clone();
    let (vm, conn) = crate::rt::blocking(move || {
        let mut vm = vm;
        let conn = vm.accept(&control, BOOT).and_then(|mut c| {
            c.set_read_timeout(Some(BOOT))?;
            match guest::recv::<FromGuest>(&mut c) {
                Ok(Some(FromGuest::Ready)) => {
                    c.set_read_timeout(None)?;
                    guest::send(&mut c, &ToGuest::Serve(Box::new(service)))?;
                    Ok(c)
                }
                _ => Err(anyhow::anyhow!("the microVM's guest didn't say it was ready:\n{}", tail(&vm.console))),
            }
        });
        (vm, conn)
    })
    .await?;
    let conn = conn?;
    let (from_tx, from_rx) = async_channel::unbounded::<FromGuest>();
    let (to_tx, to_rx) = async_channel::unbounded::<ToGuest>();
    let (mut reader, mut writer) = (conn.try_clone()?, conn);
    std::thread::Builder::new().name("deployed-in".into()).spawn(move || {
        while let Ok(Some(f)) = guest::recv::<FromGuest>(&mut reader) {
            if from_tx.send_blocking(f).is_err() {
                break;
            }
        }
    })?;
    std::thread::Builder::new().name("deployed-out".into()).spawn(move || {
        while let Ok(m) = to_rx.recv_blocking() {
            if guest::send(&mut writer, &m).is_err() {
                break;
            }
        }
    })?;
    Ok((Live { vm, socks, dir, stop, to: to_tx, _egress: egress, n, booted: Instant::now() }, from_rx))
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
