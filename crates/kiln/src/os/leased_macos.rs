//! U01: launchd gives each transient job a resource coalition inherited by fork/exec, including
//! reparented and setsid children. A gated, builtin-only helper cannot start work before claim.
//! Persist the boot + coalition identity before opening that gate. Enumeration is only for
//! signalling: capacity reuse requires kernel tasks_started == tasks_exited, never a PID list.
#![allow(unsafe_code)]

use std::ffi::{CString, OsStr, c_void};
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};

use anyhow::Context as _;
use nix::libc;

#[link(name = "System")]
unsafe extern "C" {
    fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut c_void, size: i32) -> i32;
    fn proc_listallpids(buffer: *mut c_void, size: i32) -> i32;
    fn proc_signal_with_audittoken(token: *const u32, signal: i32) -> i32;
    fn coalition_info_resource_usage(id: u64, buffer: *mut c_void, size: usize) -> i32;
}

#[repr(C)]
#[derive(Default)]
struct Unique { uuid: [u8; 16], unique: u64, parent: u64, version: u32, parent_version: u32, reserved: [u64; 2] }

#[derive(Clone, Debug)]
pub(crate) struct Group { root: PathBuf, id: String, boot: String, domain: String, coalition: Arc<AtomicU64> }

fn invalid(message: &str) -> io::Error { io::Error::other(message) }

fn boot() -> io::Result<String> {
    let mut value = [0u8; 128];
    let mut size = value.len();
    // SAFETY: fixed NUL-terminated sysctl name and a correctly sized writable byte buffer.
    if unsafe { libc::sysctlbyname(c"kern.bootsessionuuid".as_ptr(), value.as_mut_ptr().cast(), &raw mut size, std::ptr::null_mut(), 0) } != 0 { return Err(io::Error::last_os_error()); }
    let bytes = value.get(..size).ok_or_else(|| invalid("invalid boot identity size"))?;
    let text = std::str::from_utf8(bytes).map_err(io::Error::other)?.trim_end_matches('\0');
    if text.len() < 16 || !text.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') { return Err(invalid("invalid kernel boot identity")); }
    Ok(text.to_owned())
}

fn quoted(value: &OsStr) -> io::Result<String> {
    let text = value.to_str().ok_or_else(|| invalid("leased command must be UTF-8"))?;
    if text.contains('\0') { return Err(invalid("NUL in leased command")); }
    Ok(format!("'{}'", text.replace('\'', "'\\''")))
}

fn write(path: PathBuf, bytes: &[u8]) -> io::Result<()> {
    let next = path.with_extension("next");
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&next)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(next, &path)?;
    if let Some(parent) = path.parent() { std::fs::File::open(parent)?.sync_all()?; }
    Ok(())
}

fn fifo(path: PathBuf) -> io::Result<()> {
    let text = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: NUL-terminated private path, no existing pipe is replaced.
    if unsafe { libc::mkfifo(text.as_ptr(), 0o600) } != 0 { return Err(io::Error::last_os_error()); }
    Ok(())
}

fn pid_coalition(pid: i32) -> io::Result<u64> {
    let mut ids = [0u64; 5];
    // SAFETY: PROC_PIDCOALITIONINFO is two IDs and three reserved u64s (40 bytes).
    if unsafe { proc_pidinfo(pid, 20, 0, ids.as_mut_ptr().cast(), 40) } != 40 { return Err(io::Error::last_os_error()); }
    ids.first().copied().filter(|id| *id != 0).ok_or_else(|| invalid("process has no resource coalition"))
}

fn usage(cid: u64) -> io::Result<bool> {
    let mut accounting = [0u64; 64];
    // SAFETY: the kernel copies the supported prefix of the resource usage struct. Its first
    // two u64s are tasks_started/tasks_exited; the buffer covers every current version.
    if unsafe { coalition_info_resource_usage(cid, accounting.as_mut_ptr().cast(), std::mem::size_of_val(&accounting)) } != 0 {
        let error = io::Error::last_os_error();
        // Coalition IDs are not recycled within a boot. ESRCH is a destroyed, empty coalition.
        return if error.raw_os_error() == Some(libc::ESRCH) { Ok(true) } else { Err(error) };
    }
    let started = accounting.first().copied().ok_or_else(|| invalid("missing coalition accounting"))?;
    let ended = accounting.get(1).copied().ok_or_else(|| invalid("missing coalition accounting"))?;
    if ended > started { return Err(invalid("invalid coalition task accounting")); }
    Ok(started == ended)
}

impl Group {
    pub(crate) fn new(admission: &crate::runner::processes::Admission, id: &str) -> io::Result<Self> {
        if !(id.starts_with("reach-") || id.starts_with("ci-")) || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') { return Err(invalid("invalid leased job id")); }
        // SAFETY: getuid has no pointer arguments and does not mutate process state.
        let uid = unsafe { libc::getuid() };
        let gui = format!("gui/{uid}");
        // A logged-in desktop keeps its GUI bootstrap namespace. Headless runners use the
        // per-user background domain, whose jobs explicitly declare Background session type.
        let domain = if Command::new("/bin/launchctl").args(["print", &gui]).output()?.status.success() { gui } else { format!("user/{uid}") };
        Ok(Self { root: admission.root().join("macos"), id: id.to_owned(), boot: boot()?, domain, coalition: Arc::new(AtomicU64::new(0)) })
    }

    pub(crate) fn identity(&self) -> String { format!("macos-coalition:{}:{}:{}:{}", self.boot, self.domain, self.id, self.coalition.load(Ordering::Acquire)) }

    pub(crate) fn recover(admission: &crate::runner::processes::Admission, identity: &str, id: &str) -> io::Result<Self> {
        let mut out = Self::new(admission, id)?;
        let prefix = "macos-coalition:";
        let text = identity.strip_prefix(prefix).ok_or_else(|| invalid("invalid leased coalition identity"))?;
        let mut parts = text.split(':');
        let old_boot = parts.next().ok_or_else(|| invalid("missing boot identity"))?;
        let domain = parts.next().ok_or_else(|| invalid("missing launchd domain"))?;
        if parts.next() != Some(id) { return Err(invalid("invalid leased coalition identity")); }
        let cid = parts.next().and_then(|p| p.parse::<u64>().ok()).ok_or_else(|| invalid("missing coalition id"))?;
        // Recovery retains the recorded domain even across logout or a new GUI session.
        // Its coalition ID, not the currently available bootstrap domain, proves ownership.
        // SAFETY: getuid has no pointer arguments and does not mutate process state.
        let uid = unsafe { libc::getuid() };
        if parts.next().is_some() || (domain != format!("gui/{uid}") && domain != format!("user/{uid}")) || old_boot.len() < 16 || !old_boot.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') { return Err(invalid("invalid leased coalition identity")); }
        out.coalition.store(cid, Ordering::Release);
        out.boot = old_boot.to_owned();
        out.domain = domain.to_owned();
        Ok(out)
    }

    fn dir(&self) -> PathBuf { self.root.join(&self.id) }
    fn label(&self) -> String { format!("dev.kiln.lease.{}", self.id) }
    fn service(&self) -> String { format!("{}/{}", self.domain, self.label()) }

    fn cid(&self) -> io::Result<u64> {
        let cached = self.coalition.load(Ordering::Acquire);
        if cached != 0 { return Ok(cached); }
        let path = self.dir().join("coalition");
        let cid = match std::fs::read_to_string(&path) {
            Ok(text) => text.trim().parse().map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // The helper is still gated, including after a crash during bootstrap. Read
                // launchd's kernel identity before bootout, which would erase this mapping.
                let output = Command::new("/bin/launchctl").args(["print", &self.service()]).output()?;
                if !output.status.success() {
                    let missing = format!("Could not find service \"{}\"", self.label());
                    if output.status.code() == Some(113) && String::from_utf8_lossy(&output.stderr).contains(&missing) {
                        return Err(io::Error::new(io::ErrorKind::NotFound, "leased launchd job is absent"));
                    }
                    return Err(invalid("launchd has no recoverable leased coalition identity"));
                }
                let text = String::from_utf8_lossy(&output.stdout);
                let section = text.split("resource coalition = {").nth(1).ok_or_else(|| invalid("launchd did not report resource coalition"))?;
                let id = section.lines().find_map(|line| line.trim().strip_prefix("ID = ")).and_then(|id| id.trim().parse::<u64>().ok()).filter(|id| *id != 0).ok_or_else(|| invalid("invalid launchd coalition id"))?;
                write(path, format!("{id}\n").as_bytes())?;
                Ok(id)
            }
            Err(e) => Err(e),
        }?;
        if cid == 0 { return Err(invalid("invalid coalition id")); }
        self.coalition.store(cid, Ordering::Release);
        Ok(cid)
    }

    /// Called only after the receiver's local crash fence is durable, before durable claim.
    pub(crate) fn initialize(&self, permit: &std::fs::File, host: Option<&std::fs::File>) -> anyhow::Result<()> {
        let dir = self.dir();
        std::fs::create_dir_all(dir.parent().context("leased cache has no parent")?)?;
        std::fs::create_dir(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        for name in ["input", "output", "error", "control"] { fifo(dir.join(name))?; }
        let ready = quoted(dir.join("ready").as_os_str())?;
        let control = quoted(dir.join("control").as_os_str())?;
        let payload = quoted(dir.join("payload").as_os_str())?;
        let input = quoted(dir.join("input").as_os_str())?;
        let output = quoted(dir.join("output").as_os_str())?;
        let error = quoted(dir.join("error").as_os_str())?;
        let exit = quoted(dir.join("exit").as_os_str())?;
        let opened = quoted(dir.join("opened").as_os_str())?;
        // No external command, fork or workload before read(go). launchd may not restart the
        // helper; its original coalition is the sole identity through the full execution.
        let helper = format!("printf '%s\\n' \"$$\" > {ready} || exit 125\nexec 3<> {control} || exit 125\nIFS= read -r -t 30 gate <&3 || exit 125\n[ \"$gate\" = go ] || exit 125\n{{ printf '%s\\n' opened > {opened} || exit 125; /bin/sh {payload}; }} < {input} > {output} 2> {error}\nstatus=$?\nprintf '%s\\n' \"$status\" > {exit}\nexit \"$status\"\n");
        let escape = |text: &str| text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;");
        let session = if self.domain.starts_with("user/") { "<key>LimitLoadToSessionType</key><string>Background</string>" } else { "" };
        let plist = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>Label</key><string>{}</string><key>ProgramArguments</key><array><string>/bin/sh</string><string>-c</string><string>{}</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><false/><key>AbandonProcessGroup</key><false/>{session}</dict></plist>", self.label(), escape(&helper));
        write(dir.join("job.plist"), plist.as_bytes())?;
        let mut command = Command::new("/bin/launchctl");
        command.args(["bootstrap", &self.domain]).arg(dir.join("job.plist"));
        let mut inherited = vec![permit.try_clone()?];
        if let Some(host) = host { inherited.push(host.try_clone()?); }
        // SAFETY: fork inherits the already-locked file descriptions atomically. Clear CLOEXEC
        // only in this child so an orphaned bootstrap retains both locks until it finishes.
        // Recovery can never observe an absent job while a stale bootstrap could still submit
        // it later. Only fcntl(2), with no allocation or lock, runs between fork and exec.
        unsafe {
            command.pre_exec(move || {
                for file in &inherited {
                    if libc::fcntl(file.as_raw_fd(), libc::F_SETFD, 0) == -1 { return Err(io::Error::last_os_error()); }
                }
                Ok(())
            });
        }
        let result = command.output()?;
        anyhow::ensure!(result.status.success(), "cannot bootstrap leased launchd job: {}", String::from_utf8_lossy(&result.stderr));
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(pid) = std::fs::read_to_string(dir.join("ready"))
                && let Ok(pid) = pid.trim().parse::<i32>()
            {
                let cid = pid_coalition(pid)?;
                // Reject accidental sharing with the receiver's resource coalition.
                anyhow::ensure!(cid != pid_coalition(i32::try_from(std::process::id())?)?, "launchd reused receiver coalition");
                write(dir.join("coalition"), format!("{cid}\n").as_bytes())?;
                self.coalition.store(cid, Ordering::Release);
                anyhow::ensure!(!usage(cid)?, "leased helper ended before claim");
                return Ok(());
            }
            anyhow::ensure!(Instant::now() < until, "leased launchd helper did not become ready");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(crate) fn spawn(&self, cmd: Command) -> anyhow::Result<crate::os::proc::Duplex> {
        anyhow::ensure!(self.boot == boot()?, "leased coalition belongs to an earlier boot");
        let cid = self.cid()?;
        anyhow::ensure!(!usage(cid)?, "leased coalition ended before spawn");
        let dir = self.dir();
        let mut payload = String::new();
        if let Some(cwd) = cmd.get_current_dir() { payload.push_str(&format!("cd -- {} || exit 125\n", quoted(cwd.as_os_str())?)); }
        // Explicit environment only; the launchd helper's environment cannot reach work.
        payload.push_str("exec /usr/bin/env -i");
        for (key, value) in cmd.get_envs().filter_map(|(k, v)| v.map(|v| (k, v))) {
            let mut assignment = key.to_os_string(); assignment.push("="); assignment.push(value);
            payload.push(' '); payload.push_str(&quoted(&assignment)?);
        }
        payload.push(' '); payload.push_str(&quoted(cmd.get_program())?);
        for arg in cmd.get_args() { payload.push(' '); payload.push_str(&quoted(arg)?); }
        payload.push('\n');
        write(dir.join("payload"), payload.as_bytes())?;
        let open = |name: &str, read: bool, write: bool| std::fs::OpenOptions::new().read(read).write(write).custom_flags(libc::O_NONBLOCK).open(dir.join(name));
        // Dummy endpoints bridge the moment before the gated helper opens workload pipes.
        // They close once the real endpoints are open, preserving ordinary pipe EOF even if
        // CI deliberately retains a background service whose output was redirected elsewhere.
        let input_dummy = open("input", true, false)?;
        let input = OwnedFd::from(open("input", false, true)?);
        // Captures may close stdin before the helper opens its input FIFO. Keep a writer
        // through that open too; the read-only dummy alone cannot unblock the helper.
        let input_writer = input.try_clone()?;
        let out = OwnedFd::from(open("output", true, false)?);
        let out_dummy = open("output", false, true)?;
        let err = OwnedFd::from(open("error", true, false)?);
        let err_dummy = open("error", false, true)?;
        let mut control = open("control", false, true)?;
        let pid: u32 = std::fs::read_to_string(dir.join("ready"))?.trim().parse()?;
        let (exit_tx, exit_rx) = futures::channel::oneshot::channel();
        // Set up every fallible pipe operation before opening the workload gate.
        let duplex = crate::os::proc::pump(pid, input, out, err, exit_rx, true)?;
        write(dir.join("go"), b"go\n")?;
        control.write_all(b"go\n")?;
        let group = self.clone();
        crate::rt::spawn(async move {
            // The helper writes this with a builtin inside the fully redirected brace group.
            // No workload can open/close its pipes before this marker was written.
            while !group.dir().join("opened").try_exists().unwrap_or(false)
                && !group.dir().join("exit").try_exists().unwrap_or(false)
                && !group.empty_proven().unwrap_or(false)
            { crate::rt::sleep(Duration::from_millis(10)).await; }
            drop((input_dummy, input_writer, out_dummy, err_dummy));
        });
        let group = self.clone();
        crate::rt::spawn(async move {
            let code = loop {
                if let Ok(text) = std::fs::read_to_string(group.dir().join("exit"))
                    && let Ok(code) = text.trim().parse::<i32>()
                { break Some(code); }
                if group.empty_proven().unwrap_or(false) {
                    // The first read can race the helper's final printf. Kernel emptiness
                    // proves that write has finished, so read its status once more.
                    break std::fs::read_to_string(group.dir().join("exit")).ok().and_then(|s| s.trim().parse::<i32>().ok());
                }
                crate::rt::sleep(Duration::from_millis(50)).await;
            };
            let _ = exit_tx.send(code);
            // The caller decides descendant lifetime. Reach fences on shell exit; CI may
            // retain a background service until its enclosing Leg ends. Kernel proof remains
            // mandatory before either path releases resource ownership.
        });
        Ok(duplex)
    }

    pub(crate) fn empty_proven(&self) -> io::Result<bool> {
        if self.boot != boot()? { return Ok(true); }
        match self.cid() {
            Ok(cid) => usage(cid),
            Err(e) if e.kind() == io::ErrorKind::NotFound && !self.dir().join("go").try_exists()? => {
                // No gate was ever opened. The permit lock is held by this reconciler, and
                // bootstrap inherits that lock through exec, so no old bootstrap can submit a
                // job after this absence check. This covers a crash before allocation starts.
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }

    pub(crate) fn kill(&self) {
        let Ok(current) = boot() else { return };
        if self.boot != current { return; }
        let Ok(cid) = self.cid() else { return };
        // Disable this unique helper before checking zero tasks, preventing launchd restarts.
        let _ = Command::new("/bin/launchctl").args(["bootout", &self.service()]).output();
        let mut pids = vec![0i32; 65536];
        // SAFETY: buffer covers the declared capacity. Truncation cannot cause false proof:
        // the caller always asks the kernel's independent coalition task accounting next.
        let count = unsafe { proc_listallpids(pids.as_mut_ptr().cast(), i32::try_from(pids.len() * 4).unwrap_or(0)) };
        for pid in pids.into_iter().take(usize::try_from(count).unwrap_or(0)).filter(|p| *p > 1) {
            let mut unique = Unique::default();
            // SAFETY: exact PROC_PIDUNIQIDENTIFIERINFO struct, including pidversion.
            if unsafe { proc_pidinfo(pid, 17, 0, (&raw mut unique).cast(), 56) } != 56 { continue; }
            if pid_coalition(pid).ok() != Some(cid) { continue; }
            let mut token = [0u32; 8];
            if let Some(slot) = token.get_mut(5) { *slot = u32::try_from(pid).unwrap_or(0); }
            if let Some(slot) = token.get_mut(7) { *slot = unique.version; }
            // SAFETY: the kernel checks pidversion while signalling; recycled PIDs cannot be
            // killed between coalition inspection and signal. Normal signal permission applies.
            unsafe { proc_signal_with_audittoken(token.as_ptr(), libc::SIGKILL) };
        }
    }

    pub(crate) fn remove(&self) {
        if !matches!(self.empty_proven(), Ok(true)) { return; }
        let _ = Command::new("/bin/launchctl").args(["bootout", &self.service()]).output();
        let _ = std::fs::remove_dir_all(self.dir());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Cleanup(Group);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let until = Instant::now() + Duration::from_secs(5);
            loop {
                self.0.kill();
                if self.0.empty_proven().unwrap_or(false) { self.0.remove(); break; }
                if Instant::now() >= until { break; }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    /// U01: the real kernel identity, including escaped process groups, survives recovery.
    #[test]
    #[ignore = "requires an allocated macOS launchd user domain and /usr/bin/python3; run centrally"]
    fn u01_macos_coalition_recovers_and_fences_double_fork_setsid() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let id = format!("ci-u01-{}", crate::util::new_id());
            let admission = crate::runner::processes::Admission::private(&tempfile::tempdir()?.keep())?;
            let group = Group::new(&admission, &id)?;
            let _cleanup = Cleanup(group.clone());
            let root = admission.root();
            std::fs::create_dir_all(root)?;
            let lock_path = root.join(format!("{id}.test-lock"));
            let permit = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&lock_path)?;
            permit.try_lock()?;
            group.initialize(&permit, None)?;
            anyhow::ensure!(!group.empty_proven()?, "the gated helper must hold its coalition");
            let marker = group.dir().join("descendant");
            let mut cmd = Command::new("/usr/bin/python3");
            cmd.env_clear().env("PATH", "/usr/bin:/bin").args(["-c", "import os,sys,time; assert sys.stdin.buffer.read() == b'', 'closed stdin did not reach EOF'; r,w=os.pipe(); p=os.fork();\nif p: os.close(w); os.read(r,1); os._exit(0)\nos.close(r); os.setsid(); p=os.fork();\nif p: os._exit(0)\nnull=os.open('/dev/null',os.O_RDWR); os.dup2(null,0); os.dup2(null,1); os.dup2(null,2); open(sys.argv[1]+'.next','w').write(str(os.getpid())); os.rename(sys.argv[1]+'.next',sys.argv[1]); os.write(w,b'1'); time.sleep(30)"]).arg(&marker);
            let duplex = group.spawn(cmd)?;
            // U03: close immediately, even before the helper opens input; its workload must
            // see EOF before it forks, and the same coalition must still own its service.
            duplex.stdin.close();
            let until = Instant::now() + Duration::from_secs(5);
            while !marker.try_exists()? {
                anyhow::ensure!(Instant::now() < until, "double-forked child did not become ready");
                crate::rt::sleep(Duration::from_millis(10)).await;
            }
            let pid: i32 = std::fs::read_to_string(marker)?.parse()?;
            let cid = group.cid()?;
            anyhow::ensure!(pid_coalition(pid)? == cid, "setsid child escaped its resource coalition");
            let restored = Group::recover(&admission, &group.identity(), &id)?;
            anyhow::ensure!(!restored.empty_proven()?, "a reparented child still owns capacity");
            let code = crate::rt::timeout(Duration::from_secs(2), duplex.exit).await
                .context("leased command did not publish its exit status")??;
            anyhow::ensure!(code == Some(0), "main command must finish while its redirected service remains live");
            crate::rt::timeout(Duration::from_secs(2), async {
                while duplex.stdout.recv().await.is_ok() {}
                while duplex.stderr.recv().await.is_ok() {}
            }).await.context("leased command pipes did not reach EOF after its main process exited")?;
            anyhow::ensure!(!restored.empty_proven()?, "pipe EOF must not release the retained service's reservation");
            loop {
                restored.kill();
                if restored.empty_proven()? { break; }
                anyhow::ensure!(Instant::now() < until, "kernel did not prove stopped coalition");
                crate::rt::sleep(Duration::from_millis(20)).await;
            }
            restored.remove();
            drop(permit);
            std::fs::remove_file(lock_path)?;
            Ok(())
        })
    }

    #[test]
    fn u01_macos_recovery_rejects_cross_execution_identity() -> anyhow::Result<()> {
        let admission = crate::runner::processes::Admission::private(&tempfile::tempdir()?.keep())?;
        let group = Group::new(&admission, "ci-u01-fixture")?;
        anyhow::ensure!(Group::recover(&admission, &group.identity(), "ci-other").is_err(), "another execution cannot claim the same coalition");
        anyhow::ensure!(Group::recover(&admission, "macos-coalition:invalid:gui/0:ci-u01-fixture:1", "ci-u01-fixture").is_err(), "malformed identity must not become proof");
        Ok(())
    }
}
