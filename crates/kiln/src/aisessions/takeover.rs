//! U18/U03: external observations grant stop authority only for an exact OS process.
//!
//! Linux: retained pidfd, full boot UUID/start ticks/proc inode, TERM then KILL.
//! Windows: retained process handle/full creation FILETIME, grace then TerminateProcess;
//! no portable graceful TERM. Jobs, cross-width and unreadable ownership are refused.
//! macOS: boot UUID/unique ID/pidversion with atomic audit-token TERM/KILL, following
//! leased_macos; permission/API failure refuses, with no privileged task-port fallback.
//! Other platforms, legacy bare PIDs and unverifiable identities never grant stop authority.
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use nix::libc;
#[cfg(windows)]
#[path = "takeover_windows.rs"]
mod windows;
#[cfg(target_os = "macos")]
#[path = "takeover_macos.rs"]
mod macos;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "platform", rename_all = "snake_case")]
pub enum ProcessIdentity {
    Linux { boot: String, start_ticks: u64, proc_inode: u64 },
    Windows { created_100ns: u64 },
    MacOs { boot: String, unique: u64, version: u32 },
}

#[cfg(target_os = "linux")]
fn stat(pid: u32) -> Option<(u32, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm may contain spaces and parentheses. Fields after its final ')' start at state (3).
    let fields: Vec<&str> = text.rsplit_once(')')?.1.split_whitespace().collect();
    Some((fields.get(1)?.parse().ok()?, fields.get(19)?.parse().ok()?))
}

pub(super) fn observe(pid: u32) -> Option<ProcessIdentity> {
    #[cfg(target_os = "linux")]
    {
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?.trim().to_owned();
        if boot.is_empty() { return None; }
        use std::os::unix::fs::MetadataExt as _;
        let proc_inode = std::fs::metadata(format!("/proc/{pid}")).ok()?.ino();
        Some(ProcessIdentity::Linux { boot, start_ticks: stat(pid)?.1, proc_inode })
    }
    #[cfg(windows)]
    { windows::observe(pid).ok() }
    #[cfg(target_os = "macos")]
    { macos::observe(pid).ok() }
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    { let _ = pid; None }
}

pub(super) fn snapshot() -> std::collections::HashMap<u32, ProcessIdentity> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir("/proc").into_iter().flatten().flatten().filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse().ok()?;
            Some((pid, observe(pid)?))
        }).collect()
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        let mut sys = sysinfo::System::new();
        sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, sysinfo::ProcessRefreshKind::nothing().without_tasks());
        sys.processes().keys().filter_map(|pid| Some((pid.as_u32(), observe(pid.as_u32())?))).collect()
    }
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    { std::collections::HashMap::new() }
}

// U18/U03: one small decision core shared by signal validation and stale/ownership regressions.
#[cfg(any(target_os = "linux", windows, target_os = "macos"))]
fn authorized(expected: &ProcessIdentity, observed: Option<&ProcessIdentity>, harness: bool, unmanaged: bool) -> bool {
    observed == Some(expected) && harness && unmanaged
}

/// Absence of an ownership mark must be verified, not inferred from an unreadable environment.
/// Reject all kiln marks (including retained sessions), without provider-specific assumptions.
#[cfg(target_os = "linux")]
fn unmanaged(pid: u32) -> bool {
    let prefix = format!("{}=", crate::harness::survivors::MARK);
    let mut at = pid;
    for _ in 0..64 {
        // PID 1 is the ancestry boundary, never a structured harness owner.
        if at == 1 { return true; }
        let Some((parent, _)) = stat(at) else { return false };
        let Ok(group) = std::fs::read_to_string(format!("/proc/{at}/cgroup")) else { return false };
        // Kernel ownership survives a cleared environment and reparenting. Inspect membership
        // directly: querying the shared containment initializer can create directories.
        if group.lines().any(|line| line.split('/').any(|part| matches!(part, "kiln-agents" | "kiln-agents-test"))) { return false; }
        let Ok(env) = std::fs::read(format!("/proc/{at}/environ")) else { return false };
        if env.split(|b| *b == 0).any(|v| v.starts_with(prefix.as_bytes())) { return false; }
        if parent == 0 { return true; }
        if parent == at { return false; }
        at = parent;
    }
    false
}

pub struct TakeoverProcess {
    #[cfg(any(target_os = "linux", windows, target_os = "macos"))]
    pid: u32,
    #[cfg(any(target_os = "linux", windows, target_os = "macos"))]
    identity: ProcessIdentity,
    #[cfg(target_os = "linux")]
    handle: std::os::fd::OwnedFd,
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
}

impl TakeoverProcess {
    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)]
    pub fn bind(pid: u32, identity: Option<&ProcessIdentity>) -> Result<Self, String> {
        use std::os::fd::FromRawFd as _;
        let identity = identity.ok_or("no observed OS process identity")?.clone();
        if pid <= 1 || pid == std::process::id() { return Err("protected process".into()); }
        // SAFETY: pidfd_open takes integer arguments and returns a newly owned descriptor.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        let raw = i32::try_from(raw).map_err(|_| "invalid process handle")?;
        if raw < 0 { return Err(std::io::Error::last_os_error().to_string()); }
        // SAFETY: successful pidfd_open returned this descriptor exclusively to us.
        let handle = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
        let process = Self { pid, identity, handle };
        process.exact()?;
        Ok(process)
    }

    #[cfg(windows)]
    pub fn bind(pid: u32, identity: Option<&ProcessIdentity>) -> Result<Self, String> {
        let identity = identity.ok_or("no observed OS process identity")?.clone();
        if pid <= 4 || pid == std::process::id() { return Err("protected process".into()); }
        let handle = windows::open(pid, true)?;
        if windows::identity(&handle)? != identity { return Err("observed process has ended or its PID was reused".into()); }
        Ok(Self { pid, identity, handle })
    }

    #[cfg(target_os = "macos")]
    pub fn bind(pid: u32, identity: Option<&ProcessIdentity>) -> Result<Self, String> {
        let identity = identity.ok_or("no observed OS process identity")?.clone();
        if pid <= 1 || pid == std::process::id() { return Err("protected process".into()); }
        if macos::observe(pid)? != identity { return Err("observed process has ended or its PID was reused".into()); }
        Ok(Self { pid, identity })
    }

    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    pub fn bind(_pid: u32, _identity: Option<&ProcessIdentity>) -> Result<Self, String> {
        Err("identity-bound external signalling is unavailable on this platform".into())
    }

    #[cfg(target_os = "linux")]
    fn exact(&self) -> Result<(), String> {
        if observe(self.pid).as_ref() != Some(&self.identity) { return Err("observed process has ended or its PID was reused".into()); }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)]
    pub fn signal(&self, kind: &str, force: bool) -> Result<(), String> {
        use std::os::fd::AsRawFd as _;
        let harness = super::runs(self.pid, kind);
        let unmanaged = unmanaged(self.pid);
        let observed = observe(self.pid);
        if !authorized(&self.identity, observed.as_ref(), harness, unmanaged) {
            return Err("process identity, harness or unmanaged ownership changed or is unverifiable".into());
        }
        // SAFETY: this owned pidfd binds the signal to the validated process even if its PID
        // is reused between validation and the syscall. No descendant receives bare-PID signals.
        let result = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, self.handle.as_raw_fd(), if force { libc::SIGKILL } else { libc::SIGTERM }, std::ptr::null::<libc::siginfo_t>(), 0) };
        if result < 0 { return Err(std::io::Error::last_os_error().to_string()); }
        Ok(())
    }

    #[cfg(windows)]
    pub fn signal(&self, kind: &str, force: bool) -> Result<(), String> {
        let harness = super::runs(self.pid, kind);
        let unmanaged = windows::unmanaged(self.pid, &self.handle).is_ok();
        let observed = windows::identity(&self.handle).ok();
        if !authorized(&self.identity, observed.as_ref(), harness, unmanaged) {
            return Err("process identity, harness or unmanaged ownership changed or is unverifiable".into());
        }
        // Windows has no safe general TERM equivalent. Keep this exact handle during the
        // grace period; only the escalation calls TerminateProcess, never a PID or job kill.
        if force { windows::terminate(&self.handle)?; }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub fn signal(&self, kind: &str, force: bool) -> Result<(), String> {
        let harness = super::runs(self.pid, kind);
        let unmanaged = macos::unmanaged(self.pid).is_ok();
        let observed = macos::observe(self.pid).ok();
        if !authorized(&self.identity, observed.as_ref(), harness, unmanaged) {
            return Err("process identity, harness or unmanaged ownership changed or is unverifiable".into());
        }
        macos::signal(self.pid, &self.identity, force)
    }

    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    pub fn signal(&self, _kind: &str, _force: bool) -> Result<(), String> {
        Err("identity-bound external signalling is unavailable on this platform".into())
    }
}

#[cfg(all(test, any(target_os = "linux", windows, target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn u18_u03_takeover_rejects_legacy_stale_and_managed_identity() -> anyhow::Result<()> {
        // Only this isolated child is inspected; no user process receives a signal.
        #[cfg(unix)]
        let mut command = { let mut command = std::process::Command::new("sleep"); command.arg("30"); command };
        #[cfg(windows)]
        let mut command = { let mut command = std::process::Command::new("powershell.exe"); command.args(["-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 30"]); command };
        let mut child = command.env(crate::harness::survivors::MARK, "u18-fixture").spawn()?;
        let checked = (|| -> anyhow::Result<()> {
            let pid = child.id();
            let identity = observe(pid).ok_or_else(|| anyhow::anyhow!("fixture identity unavailable"))?;
            // U18/U03: even a supplied identity can never grant authority over kiln or PID 1.
            // Binding rejects these IDs before opening a signal-capable OS handle.
            for protected in [std::process::id(), 1] {
                assert!(matches!(TakeoverProcess::bind(protected, Some(&identity)), Err(error) if error == "protected process"));
            }
            #[cfg(windows)]
            assert!(matches!(TakeoverProcess::bind(4, Some(&identity)), Err(error) if error == "protected process"));
            let legacy: crate::harness::session::Spec = serde_json::from_value(serde_json::json!({ "kind": "codex", "takeover_pid": pid, "takeover_stop": true }))?;
            assert!(legacy.takeover_identity.is_none());
            assert!(TakeoverProcess::bind(pid, legacy.takeover_identity.as_ref()).is_err());
            let encoded = serde_json::to_value(&identity)?;
            assert_eq!(serde_json::from_value::<ProcessIdentity>(encoded)?, identity);
            let mut stale = identity.clone();
            match &mut stale {
                ProcessIdentity::Linux { start_ticks, .. } => *start_ticks = start_ticks.saturating_add(1),
                ProcessIdentity::Windows { created_100ns } => *created_100ns = created_100ns.saturating_add(1),
                ProcessIdentity::MacOs { unique, .. } => *unique = unique.saturating_add(1),
            }
            assert!(TakeoverProcess::bind(pid, Some(&stale)).is_err());
            let exact = TakeoverProcess::bind(pid, Some(&identity)).map_err(anyhow::Error::msg)?;
            assert_eq!(observe(pid).as_ref(), Some(&identity));
            assert!(authorized(&identity, Some(&identity), true, true));
            assert!(!authorized(&identity, Some(&stale), true, true));
            assert!(!authorized(&identity, None, true, true));
            assert!(!authorized(&identity, Some(&identity), true, false));
            assert!(!authorized(&identity, Some(&identity), false, true));
            #[cfg(target_os = "linux")]
            assert!(!unmanaged(pid));
            #[cfg(windows)]
            assert!(windows::unmanaged(pid, &exact.handle).is_err());
            #[cfg(target_os = "macos")]
            assert!(macos::unmanaged(pid).is_err());
            assert!(exact.signal("codex", false).is_err());
            assert!(exact.signal("codex", true).is_err());
            assert!(child.try_wait()?.is_none());
            Ok(())
        })();
        let _ = child.kill();
        let _ = child.wait();
        checked
    }
}
