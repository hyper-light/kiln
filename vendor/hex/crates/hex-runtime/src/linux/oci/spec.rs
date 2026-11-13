//! The OCI Runtime Specification v1.3.0 `config.json`, the fields Hex's `oci` class reads. Unknown
//! fields are ignored, not refused (the spec requires a runtime not error on an unknown property;
//! runtime-tools' `misc_props` checks it). Hostile input is a typed error, never a panic
//! (CLAUDE.md §6): the model parses, and [`Spec::check`] refuses what the runtime can't honour.

use serde::Deserialize;

use crate::linux::oci::OciError;

/// Longest `config.json` Hex reads: a spec is a few kilobytes; a larger file is refused unparsed.
pub const MAX_CONFIG_BYTES: usize = 1 << 20;

/// A parsed `config.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Spec {
    pub oci_version: String,
    #[serde(default)]
    pub hostname: Option<String>,
    pub root: Root,
    #[serde(default)]
    pub mounts: Vec<Mount>,
    pub process: Option<Process>,
    #[serde(default)]
    pub linux: Linux,
    #[serde(default)]
    pub hooks: Hooks,
    /// Arbitrary metadata; carried into the container state the `state` command and hooks see.
    #[serde(default)]
    pub annotations: std::collections::BTreeMap<String, String>,
}

/// Lifecycle hooks (OCI spec §POSIX-platform Hooks). Each list runs in order at its point; the
/// container state JSON is written to each hook's stdin.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hooks {
    /// Deprecated; runs in the runtime namespace after the container is created, before `start`.
    #[serde(default)]
    pub prestart: Vec<Hook>,
    #[serde(default)]
    pub create_runtime: Vec<Hook>,
    #[serde(default)]
    pub create_container: Vec<Hook>,
    #[serde(default)]
    pub start_container: Vec<Hook>,
    #[serde(default)]
    pub poststart: Vec<Hook>,
    #[serde(default)]
    pub poststop: Vec<Hook>,
}

/// One hook: a program, its arguments and environment, and how long it may run.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hook {
    pub path: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub timeout: Option<i64>,
}

/// The container's root filesystem.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Root {
    pub path: String,
    #[serde(default)]
    pub readonly: bool,
}

/// A mount into the container.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mount {
    pub destination: String,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub options: Vec<String>,
}

/// The first process the container runs.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Process {
    #[serde(default)]
    pub terminal: bool,
    #[serde(default = "root_cwd")]
    pub cwd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub user: User,
    #[serde(default)]
    pub capabilities: Option<Capabilities>,
    #[serde(default)]
    pub rlimits: Vec<Rlimit>,
    #[serde(default)]
    pub no_new_privileges: bool,
    #[serde(default)]
    pub oom_score_adj: Option<i32>,
    /// The AppArmor profile the process takes at its `execve`, which `init::Becoming` applies, refused by name
    /// where AppArmor isn't on ([`super::lsm`]). Read through [`Process::apparmor_profile`].
    #[serde(default)]
    pub apparmor_profile: Option<String>,
    /// The SELinux process label. Same: refused until Hex applies it, never silently dropped (§3).
    #[serde(default)]
    pub selinux_label: Option<String>,
}

impl Process {
    /// The AppArmor profile, where one is set: an empty one is none.
    pub fn apparmor_profile(&self) -> Option<&str> {
        self.apparmor_profile.as_deref().filter(|p| !p.is_empty())
    }
}

fn root_cwd() -> String {
    "/".to_owned()
}

/// Who the first process runs as.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    #[serde(default)]
    pub uid: u32,
    #[serde(default)]
    pub gid: u32,
    #[serde(default)]
    pub additional_gids: Vec<u32>,
    #[serde(default)]
    pub umask: Option<u32>,
}

/// The capability sets, each a list of `CAP_*` names.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    #[serde(default)]
    pub bounding: Vec<String>,
    #[serde(default)]
    pub effective: Vec<String>,
    #[serde(default)]
    pub inheritable: Vec<String>,
    #[serde(default)]
    pub permitted: Vec<String>,
    #[serde(default)]
    pub ambient: Vec<String>,
}

impl Process {
    /// The stack limit this process runs under: its own RLIMIT_STACK soft limit where the spec sets one,
    /// else the node's, which it inherits.
    pub fn stack(&self) -> u64 {
        self.rlimits
            .iter()
            .find(|r| r.kind == "RLIMIT_STACK")
            .map_or_else(crate::linux::process::node_stack, |r| r.soft)
    }
}

/// A resource limit (`setrlimit`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rlimit {
    #[serde(rename = "type")]
    pub kind: String,
    pub hard: u64,
    pub soft: u64,
}

/// The Linux-specific configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Linux {
    #[serde(default)]
    pub namespaces: Vec<Namespace>,
    #[serde(default)]
    pub uid_mappings: Vec<IdMapping>,
    #[serde(default)]
    pub gid_mappings: Vec<IdMapping>,
    #[serde(default)]
    pub devices: Vec<Device>,
    #[serde(default)]
    pub masked_paths: Vec<String>,
    #[serde(default)]
    pub readonly_paths: Vec<String>,
    #[serde(default)]
    pub sysctl: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub rootfs_propagation: Option<String>,
    #[serde(default)]
    pub cgroups_path: Option<String>,
    #[serde(default)]
    pub resources: Option<Resources>,
    /// `linux.seccomp`: the syscall filter. Hex compiles it to a BPF program and loads it in the
    /// container init before `execve` (see [`super::seccomp`]). A profile the compiler can't honour
    /// is refused there, by name (CLAUDE.md §3), never run unfiltered.
    #[serde(default)]
    pub seccomp: Option<Seccomp>,
    /// `linux.mountLabel`: the SELinux label of every filesystem the container's mounts make, refused by name
    /// where the kernel can't apply it ([`super::lsm`]). Read through [`Spec::mount_label`].
    #[serde(default)]
    pub mount_label: Option<String>,
}

/// `linux.seccomp` (config-linux.md, "Seccomp"): a syscall filter. `default_action` is taken when
/// no `syscalls` entry matches; each entry names syscalls, the action for them, and optional
/// argument conditions. [`super::seccomp`] compiles this to a BPF program.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Seccomp {
    /// The action when no rule matches, e.g. `SCMP_ACT_ERRNO`. Required; a missing one is malformed.
    pub default_action: String,
    /// The errno `default_action` returns when it is `SCMP_ACT_ERRNO`/`SCMP_ACT_TRACE`; else unset.
    #[serde(default)]
    pub default_errno_ret: Option<u32>,
    /// The `SCMP_ARCH_*` the profile applies to; empty means the native architecture only.
    #[serde(default)]
    pub architectures: Vec<String>,
    /// `SECCOMP_FILTER_FLAG_*` for the `seccomp(2)` call.
    #[serde(default)]
    pub flags: Vec<String>,
    /// The syscall groups, each an action for the syscalls it names under its argument conditions.
    #[serde(default)]
    pub syscalls: Vec<SeccompSyscall>,
}

/// One `linux.seccomp.syscalls` entry: an action for the named syscalls, under all of `args`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeccompSyscall {
    #[serde(default)]
    pub names: Vec<String>,
    /// This group's action, e.g. `SCMP_ACT_ALLOW`. Required; a missing one is malformed.
    pub action: String,
    /// The errno this group returns when its action is `SCMP_ACT_ERRNO`/`SCMP_ACT_TRACE`.
    #[serde(default)]
    pub errno_ret: Option<u32>,
    /// The argument conditions; all must hold for the group to match (AND). Empty matches always.
    #[serde(default)]
    pub args: Vec<SeccompArg>,
}

/// One `args` condition: compares syscall argument `index` against `value` (and `value_two` for a
/// masked compare) with `op` (an `SCMP_CMP_*`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeccompArg {
    pub index: u32,
    pub value: u64,
    #[serde(default)]
    pub value_two: Option<u64>,
    /// The comparison, e.g. `SCMP_CMP_EQ`. Required; a missing one is malformed.
    pub op: String,
}

/// `linux.resources` (config-linux.md, "Control groups"): cgroup v1-shaped limits, which
/// [`super::resources`] converts to cgroup v2 files, and `unified`, cgroup v2 files by name.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resources {
    /// The allowed device list, applied in order.
    #[serde(default)]
    pub devices: Vec<DeviceRule>,
    #[serde(default)]
    pub memory: Option<Memory>,
    #[serde(default)]
    pub cpu: Option<Cpu>,
    #[serde(default, rename = "blockIO")]
    pub block_io: Option<BlockIo>,
    #[serde(default)]
    pub hugepage_limits: Vec<HugepageLimit>,
    /// `net_cls` and `net_prio`, which cgroup v2 doesn't have: present is refused, whatever it holds.
    #[serde(default)]
    pub network: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    pub pids: Option<Pids>,
    #[serde(default)]
    pub rdma: std::collections::BTreeMap<String, Rdma>,
    #[serde(default)]
    pub unified: std::collections::BTreeMap<String, String>,
}

/// One entry of the allowed device list.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRule {
    pub allow: bool,
    /// `a` (all), `c` (char) or `b` (block); unset is all.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// Unset is every major (minor).
    #[serde(default)]
    pub major: Option<i64>,
    #[serde(default)]
    pub minor: Option<i64>,
    /// Some of `r`, `w` and `m`; unset is all three.
    #[serde(default)]
    pub access: Option<String>,
}

/// `linux.resources.memory`, in bytes, `-1` for unlimited.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub reservation: Option<i64>,
    /// Memory plus swap, as cgroup v1 counts it.
    #[serde(default)]
    pub swap: Option<i64>,
    #[serde(default)]
    pub kernel: Option<i64>,
    #[serde(default, rename = "kernelTCP")]
    pub kernel_tcp: Option<i64>,
    #[serde(default)]
    pub swappiness: Option<u64>,
    #[serde(default, rename = "disableOOMKiller")]
    pub disable_oom_killer: Option<bool>,
    #[serde(default)]
    pub use_hierarchy: Option<bool>,
    #[serde(default)]
    pub check_before_update: Option<bool>,
}

/// `linux.resources.cpu`: times in microseconds.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cpu {
    #[serde(default)]
    pub shares: Option<u64>,
    #[serde(default)]
    pub quota: Option<i64>,
    #[serde(default)]
    pub burst: Option<u64>,
    #[serde(default)]
    pub period: Option<u64>,
    #[serde(default)]
    pub realtime_runtime: Option<i64>,
    #[serde(default)]
    pub realtime_period: Option<u64>,
    #[serde(default)]
    pub cpus: Option<String>,
    #[serde(default)]
    pub mems: Option<String>,
    #[serde(default)]
    pub idle: Option<i64>,
}

/// `linux.resources.blockIO`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockIo {
    #[serde(default)]
    pub weight: Option<u16>,
    #[serde(default)]
    pub leaf_weight: Option<u16>,
    #[serde(default)]
    pub weight_device: Vec<WeightDevice>,
    #[serde(default)]
    pub throttle_read_bps_device: Vec<ThrottleDevice>,
    #[serde(default)]
    pub throttle_write_bps_device: Vec<ThrottleDevice>,
    #[serde(default, rename = "throttleReadIOPSDevice")]
    pub throttle_read_iops_device: Vec<ThrottleDevice>,
    #[serde(default, rename = "throttleWriteIOPSDevice")]
    pub throttle_write_iops_device: Vec<ThrottleDevice>,
}

/// One device's IO weight.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WeightDevice {
    pub major: i64,
    pub minor: i64,
    #[serde(default)]
    pub weight: Option<u16>,
    #[serde(default)]
    pub leaf_weight: Option<u16>,
}

/// One device's IO rate limit, in bytes or operations per second.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThrottleDevice {
    pub major: i64,
    pub minor: i64,
    pub rate: u64,
}

/// A HugeTLB limit for one page size (`2MB`, `1GB`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HugepageLimit {
    pub page_size: String,
    pub limit: u64,
}

/// `linux.resources.pids`: `-1` is no limit, and `0` is a limit of zero.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pids {
    #[serde(default)]
    pub limit: Option<i64>,
}

/// One RDMA device's limits.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rdma {
    #[serde(default)]
    pub hca_handles: Option<u32>,
    #[serde(default)]
    pub hca_objects: Option<u32>,
}

/// A namespace the container runs in; `path` joins an existing one instead of making a new one.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Namespace {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub path: Option<String>,
}

/// A uid or gid mapping for a user namespace.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdMapping {
    #[serde(rename = "containerID")]
    pub container_id: u32,
    #[serde(rename = "hostID")]
    pub host_id: u32,
    pub size: u32,
}

/// A device node to make in the container.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    #[serde(rename = "type")]
    pub kind: String,
    pub path: String,
    #[serde(default)]
    pub major: Option<i64>,
    #[serde(default)]
    pub minor: Option<i64>,
    #[serde(default)]
    pub file_mode: Option<u32>,
    #[serde(default)]
    pub uid: Option<u32>,
    #[serde(default)]
    pub gid: Option<u32>,
}

impl Spec {
    /// A spec from `config.json`'s bytes, or why they aren't one.
    pub fn parse(bytes: &[u8]) -> Result<Self, OciError> {
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(OciError::Config(format!(
                "config.json is {} bytes, over {MAX_CONFIG_BYTES}",
                bytes.len()
            )));
        }
        let spec: Spec = serde_json::from_slice(bytes)
            .map_err(|e| OciError::Config(format!("config.json: {e}")))?;
        spec.check()?;
        Ok(spec)
    }

    /// Refuses a spec the runtime can't honour, by name.
    pub fn check(&self) -> Result<(), OciError> {
        if self.oci_version.is_empty() {
            return Err(OciError::Config("ociVersion is empty".to_owned()));
        }
        // The init's arguments and environment, counted as execve counts them and held to the bound of the
        // box's own stack (its RLIMIT_STACK, else the node's) before anything is made, so no box is left whose
        // init can't start (HEX-RT-14).
        if let Some(p) = &self.process {
            crate::linux::process::fits_process(&p.args, &p.env, p.stack())
                .map_err(|e| OciError::Config(format!("process: {e}")))?;
        }
        if self.root.path.is_empty() {
            return Err(OciError::Config("root.path is empty".to_owned()));
        }
        // A destination must be absolute, so a mount can't escape the container's tree.
        for m in &self.mounts {
            if !m.destination.starts_with('/') {
                return Err(OciError::Config(format!(
                    "mount destination {:?} isn't absolute",
                    m.destination
                )));
            }
        }
        for p in self
            .linux
            .masked_paths
            .iter()
            .chain(&self.linux.readonly_paths)
        {
            if !p.starts_with('/') {
                return Err(OciError::Config(format!("path {p:?} isn't absolute")));
            }
        }
        if let Some(process) = &self.process {
            if process.args.first().is_none_or(String::is_empty) {
                return Err(OciError::Config("process.args is empty".to_owned()));
            }
            // A spec that names a security control Hex can't yet apply is refused by name, never run
            // without it (CLAUDE.md §3: a dropped security control is a silent failure).
            if process
                .selinux_label
                .as_deref()
                .is_some_and(|s| !s.is_empty())
            {
                return Err(OciError::Config(
                    "process.selinuxLabel is set: Hex does not yet apply a SELinux process label, and \
                     refuses rather than run without it"
                        .to_owned(),
                ));
            }
        }
        // linux.seccomp is honoured: the container init compiles it and loads the BPF filter before
        // execve (super::seccomp). A profile the compiler can't honour is refused there, by name, so
        // the refusal is no longer blanket here. linux.mountLabel and process.apparmorProfile are applied
        // too, and refused where the host can't apply them (super::lsm).
        // A container in a user namespace that doesn't make a network namespace of its own can't mount a
        // sysfs (the network namespace's owner must), so its /sys is a bind of one, the host's where it
        // shares the host's network. Holding CAP_SYS_ADMIN, it could unmount the mounts over that bind
        // and read every box's cgroup tree beneath it, which the bind's read-only remount leaves
        // readable (docs/research/runtime/goal5-cgroupfs.md). Refused, the combination, on Ada's call
        // (2026-10-31).
        if self.in_user_namespace()
            && !self.makes_namespace("network")
            && self.holds_capability("CAP_SYS_ADMIN")
        {
            return Err(OciError::Config(
                "a container in a user namespace that holds CAP_SYS_ADMIN (named, or kept from its \
                 creator where the spec names no capabilities) without a network namespace of its own \
                 could reach the host's cgroup tree through the /sys it must bind: give it a network \
                 namespace of its own, or capabilities without CAP_SYS_ADMIN"
                    .to_owned(),
            ));
        }
        // Each hook's path is absolute and its timeout above zero (config.md:556, :559-560), and the
        // startContainer hooks' timeouts sum to the seconds `start` waits on them besides its own.
        let hooks = &self.hooks;
        for (kind, list) in [
            ("prestart", &hooks.prestart),
            ("createRuntime", &hooks.create_runtime),
            ("createContainer", &hooks.create_container),
            ("startContainer", &hooks.start_container),
            ("poststart", &hooks.poststart),
            ("poststop", &hooks.poststop),
        ] {
            if let Some(hook) = list.iter().find(|h| !h.path.starts_with('/')) {
                return Err(OciError::Config(format!(
                    "a {kind} hook's path {:?} isn't absolute",
                    hook.path
                )));
            }
            super::hooks::budget(kind, list)?;
        }
        Ok(())
    }

    /// Whether the container's process could change its own cgroup or what is attached to it, so a
    /// cgroup it leaves behind isn't handed to another box (goal 5 extended, step 6): it may hold
    /// CAP_SYS_ADMIN, which mounts a cgroup filesystem it can write (a spec that gives no capability
    /// sets keeps its creator's, all of them for a root node's container), or it holds CAP_BPF with
    /// CAP_NET_ADMIN, which load a cgroup device program (Linux v6.19, kernel/bpf/syscall.c:2930-2941)
    /// and attach it to the cgroup it can open.
    pub fn could_change_its_cgroup(&self) -> bool {
        let given = self
            .process
            .as_ref()
            .is_some_and(|p| p.capabilities.is_some());
        !given
            || self.holds_capability("CAP_SYS_ADMIN")
            || (self.holds_capability("CAP_BPF") && self.holds_capability("CAP_NET_ADMIN"))
    }

    /// Whether the container may be given its owner's earlier box's network namespace, and hand its own on
    /// to its owner's next box: only if it couldn't change it (goal 5 extended, step 6; Ada's call of
    /// 2026-11-03; docs/research/runtime/goal5-netns-reuse.md). Refused, by why:
    /// - [`Refused::NetAdmin`]: its spec gives no capability sets, so it keeps its creator's, all of them for
    ///   a root node's container, or names CAP_NET_ADMIN or CAP_SYS_ADMIN in one. Every rtnetlink change, every
    ///   interface ioctl, a tun device and a firewall table need CAP_NET_ADMIN (Linux v6.19
    ///   net/core/rtnetlink.c:6885; net/ipv4/devinet.c:1099-1111; drivers/net/tun.c:2757;
    ///   net/netfilter/nfnetlink.c:661), and CAP_SYS_ADMIN mounts a proc it can write;
    /// - [`Refused::ProcSysWritable`]: it mounts a proc whose `/proc/sys` isn't read-only, through which a
    ///   process running as the namespace's root writes its net sysctls without CAP_NET_ADMIN
    ///   (net/sysctl_net.c:42-54, fs/proc/proc_sysctl.c:425-434);
    /// - [`Refused::OwnUserNamespace`]: its network namespace would belong to a user namespace of its own,
    ///   where the node's belong to the node's.
    pub fn may_share_its_network(&self) -> Result<(), hex_core::netns_reuse::Refused> {
        use hex_core::netns_reuse::Refused;
        if self.in_user_namespace() {
            return Err(Refused::OwnUserNamespace);
        }
        let given = self
            .process
            .as_ref()
            .is_some_and(|p| p.capabilities.is_some());
        if !given
            || self.holds_capability("CAP_NET_ADMIN")
            || self.holds_capability("CAP_SYS_ADMIN")
        {
            return Err(Refused::NetAdmin);
        }
        let proc_mounted = self
            .mounts
            .iter()
            .any(|m| m.kind.as_deref() == Some("proc"));
        let sys_read_only = self.linux.readonly_paths.iter().any(|p| {
            let p = p.trim_end_matches('/');
            p == "/proc/sys" || p == "/proc"
        });
        if proc_mounted && !sys_read_only {
            return Err(Refused::ProcSysWritable);
        }
        Ok(())
    }

    /// Whether the process may hold `cap`: one of its capability sets names it, or the spec names no
    /// capabilities, so the init changes none (init.rs, `caps`) and it keeps every one of its creator's.
    fn holds_capability(&self, cap: &str) -> bool {
        self.process.as_ref().is_some_and(|p| {
            p.capabilities.as_ref().is_none_or(|c| {
                [
                    &c.bounding,
                    &c.effective,
                    &c.inheritable,
                    &c.permitted,
                    &c.ambient,
                ]
                .iter()
                .any(|set| set.iter().any(|name| name == cap))
            })
        })
    }

    /// Whether the spec asks for a namespace of `kind` (made fresh, no `path`).
    pub fn makes_namespace(&self, kind: &str) -> bool {
        self.linux
            .namespaces
            .iter()
            .any(|n| n.kind == kind && n.path.is_none())
    }

    /// Whether the container runs in a user namespace of its own or one it joins by `path`: either way
    /// it is outside the initial user namespace, where `mknod` of a device is refused and a device policy
    /// is enforced by construction rather than by a cgroup device program the runtime loads.
    /// The mount label, where one is set: an empty one is none.
    pub fn mount_label(&self) -> Option<&str> {
        self.linux.mount_label.as_deref().filter(|l| !l.is_empty())
    }

    pub fn in_user_namespace(&self) -> bool {
        self.linux.namespaces.iter().any(|n| n.kind == "user")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"{
        "ociVersion": "1.3.0",
        "root": {"path": "rootfs", "readonly": true},
        "process": {"args": ["/bin/sh"], "cwd": "/"},
        "mounts": [{"destination": "/proc", "type": "proc", "source": "proc"}],
        "linux": {"namespaces": [{"type": "pid"}, {"type": "mount"}], "maskedPaths": ["/proc/kcore"]}
    }"#;

    /// HEX-RT-14 at create, from the box's own stack: do parse a config whose process sets RLIMIT_STACK to
    /// 1 MiB (a 256 KiB exec bound) and has a 300 KiB variable, which the node's own default stack would take.
    /// Expect it refused against the box's stack, by name.
    #[test]
    fn an_init_is_held_to_its_own_stack_limit() {
        let value = "x".repeat(300 << 10);
        let json = format!(
            r#"{{"ociVersion":"1.3.0","root":{{"path":"rootfs"}},"process":{{"args":["/bin/sh"],"cwd":"/","env":["BIG={value}"],"rlimits":[{{"type":"RLIMIT_STACK","hard":1048576,"soft":1048576}}]}}}}"#
        );
        let r = Spec::parse(json.as_bytes());
        assert!(
            matches!(r, Err(OciError::Config(ref why)) if why.contains("allows 262144")),
            "{r:?}"
        );
    }

    /// HEX-RT-14 at create: do parse a config whose init has a variable one byte past this kernel's
    /// per-string execve bound (read now); expect it refused as the config, naming the bound, before
    /// anything is made, so no box is left that can't start.
    #[test]
    fn an_init_past_the_kernels_exec_bound_is_refused_at_parse() {
        let most = crate::linux::process::exec_limit().string.unwrap();
        // "BIG=" and the value with its NUL pass `most` by a byte.
        let value = "x".repeat(most - "BIG=".len());
        let json = format!(
            r#"{{"ociVersion":"1.3.0","root":{{"path":"rootfs"}},"process":{{"args":["/bin/sh"],"cwd":"/","env":["BIG={value}"]}}}}"#
        );
        assert!(json.len() <= MAX_CONFIG_BYTES, "the config itself fits");
        let r = Spec::parse(json.as_bytes());
        assert!(
            matches!(r, Err(OciError::Config(ref why)) if why.contains("execve")),
            "{r:?}"
        );
    }

    #[test]
    fn a_minimal_config_parses_with_its_fields() {
        let spec = Spec::parse(MINIMAL.as_bytes()).unwrap();
        assert_eq!(spec.oci_version, "1.3.0");
        assert!(spec.root.readonly);
        assert_eq!(spec.process.as_ref().unwrap().args, ["/bin/sh"]);
        assert_eq!(spec.mounts[0].kind.as_deref(), Some("proc"));
        assert!(spec.makes_namespace("pid"));
        assert!(!spec.makes_namespace("network"));
        assert_eq!(spec.linux.masked_paths, ["/proc/kcore"]);
    }

    /// Goal 5 extended, step 6 (decision 5, "Who never shares"): a box that could change its own cgroup
    /// never shares one. Given no capability sets, it keeps its creator's, all of them; or it holds
    /// CAP_SYS_ADMIN in any set; or CAP_BPF with CAP_NET_ADMIN (kernel/bpf/syscall.c:2930-2941, v6.19).
    #[test]
    fn a_box_that_could_change_its_cgroup_is_known_by_its_capability_sets() {
        let with = |process: &str| {
            let json = format!(
                r#"{{"ociVersion":"1.3.0","root":{{"path":"rootfs"}},
                    "process":{{"args":["/bin/sh"],"cwd":"/"{process}}},"linux":{{}}}}"#
            );
            Spec::parse(json.as_bytes())
                .unwrap()
                .could_change_its_cgroup()
        };
        let sets = |set: &str, names: &str| format!(r#","capabilities":{{"{set}":[{names}]}}"#);
        assert!(with(""), "no capability sets: the creator's, all of them");
        assert!(
            !with(&sets("bounding", r#""CAP_CHOWN","CAP_NET_RAW","CAP_KILL""#)),
            "sets that name neither"
        );
        assert!(
            with(&sets("bounding", r#""CAP_SYS_ADMIN""#)),
            "CAP_SYS_ADMIN, bounding"
        );
        assert!(
            with(&sets("ambient", r#""CAP_SYS_ADMIN""#)),
            "CAP_SYS_ADMIN, ambient"
        );
        assert!(
            with(&sets("effective", r#""CAP_BPF","CAP_NET_ADMIN""#)),
            "CAP_BPF with CAP_NET_ADMIN"
        );
        assert!(!with(&sets("effective", r#""CAP_BPF""#)), "CAP_BPF alone");
        assert!(
            !with(&sets("effective", r#""CAP_NET_ADMIN""#)),
            "CAP_NET_ADMIN alone"
        );
    }

    /// Goal 5 extended, step 6 (Ada's call of 2026-11-03): a box may share its owner's network namespaces
    /// only if it couldn't have changed its own: capability sets given and naming neither CAP_NET_ADMIN nor
    /// CAP_SYS_ADMIN, `/proc/sys` read-only (or no proc mounted), and no user namespace of its own.
    #[test]
    fn a_box_may_share_its_network_only_if_it_could_change_nothing_of_it() {
        use hex_core::netns_reuse::Refused;
        let docker = r#""capabilities":{"bounding":["CAP_CHOWN","CAP_NET_RAW","CAP_KILL"],"effective":["CAP_CHOWN","CAP_NET_RAW","CAP_KILL"]}"#;
        let spec = |caps: &str, readonly: &str, proc_mounted: bool, user: bool| {
            let mounts = if proc_mounted {
                r#"[{"destination":"/proc","type":"proc","source":"proc"}]"#
            } else {
                "[]"
            };
            let user = if user { r#",{"type":"user"}"# } else { "" };
            let json = format!(
                r#"{{"ociVersion":"1.3.0","root":{{"path":"rootfs"}},
                    "process":{{"args":["/bin/sh"],"cwd":"/"{caps}}},"mounts":{mounts},
                    "linux":{{"namespaces":[{{"type":"pid"}},{{"type":"network"}}{user}],
                              "readonlyPaths":{readonly}}}}}"#
            );
            Spec::parse(json.as_bytes()).unwrap()
        };
        let sys = r#"["/proc/sys"]"#;
        let caps = format!(",{docker}");
        assert_eq!(
            spec(&caps, sys, true, false).may_share_its_network(),
            Ok(())
        );
        assert_eq!(
            spec("", sys, true, false).may_share_its_network(),
            Err(Refused::NetAdmin),
            "no capability sets: the creator's, all of them"
        );
        for held in ["CAP_NET_ADMIN", "CAP_SYS_ADMIN"] {
            let c = format!(r#","capabilities":{{"ambient":["{held}"]}}"#);
            assert_eq!(
                spec(&c, sys, true, false).may_share_its_network(),
                Err(Refused::NetAdmin),
                "{held}"
            );
        }
        assert_eq!(
            spec(&caps, "[]", true, false).may_share_its_network(),
            Err(Refused::ProcSysWritable)
        );
        assert_eq!(
            spec(&caps, r#"["/proc/sys/"]"#, true, false).may_share_its_network(),
            Ok(()),
            "a trailing slash is the same path"
        );
        assert_eq!(
            spec(&caps, r#"["/proc"]"#, true, false).may_share_its_network(),
            Ok(()),
            "all of /proc read-only"
        );
        assert_eq!(
            spec(&caps, "[]", false, false).may_share_its_network(),
            Ok(()),
            "no proc mounted, and none it could mount"
        );
        assert_eq!(
            spec(&caps, sys, true, true).may_share_its_network(),
            Err(Refused::OwnUserNamespace)
        );
    }

    #[test]
    fn a_security_control_hex_cannot_yet_apply_is_refused_not_dropped() {
        // CLAUDE.md §3: a security control Hex can't honour is refused by name, never run without
        // (a dropped LSM control is a silent security failure). Parse runs check. linux.seccomp,
        // linux.mountLabel and process.apparmorProfile are no longer here: Hex applies them, and refuses
        // where the host can't (super::lsm); process.selinuxLabel it doesn't yet apply.
        let json = r#"{"ociVersion":"1.0.0","root":{"path":"rootfs"},
            "process":{"args":["/bin/sh"],"selinuxLabel":"system_u:system_r:container_t:s0"}}"#;
        let r = Spec::parse(json.as_bytes());
        assert!(
            matches!(&r, Err(OciError::Config(why)) if why.contains("process.selinuxLabel")),
            "process.selinuxLabel should be refused with a Config error naming it, got {r:?}"
        );
    }

    // AC-T7.1.4 (runtime-tools linux_mount_label, linux_process_apparmor_profile): parse a spec that sets
    // linux.mountLabel or process.apparmorProfile, expect it accepted with the label kept for the init to
    // apply, not refused as before (docs/design/runtime.md, "SELinux mount labels and AppArmor profiles").
    #[test]
    fn a_mount_label_and_an_apparmor_profile_are_parsed_to_be_applied() {
        let json = r#"{"ociVersion":"1.0.0","root":{"path":"rootfs"},
            "process":{"args":["/bin/sh"],"apparmorProfile":"acme_secure_profile"},
            "linux":{"mountLabel":"system_u:object_r:svirt_sandbox_file_t:s0:c715,c811"}}"#;
        let s = Spec::parse(json.as_bytes()).expect("both are applied, so the spec parses");
        assert_eq!(
            s.mount_label(),
            Some("system_u:object_r:svirt_sandbox_file_t:s0:c715,c811")
        );
        assert_eq!(
            s.process.as_ref().and_then(|p| p.apparmor_profile()),
            Some("acme_secure_profile")
        );
        let empty = r#"{"ociVersion":"1.0.0","root":{"path":"rootfs"},
            "process":{"args":["/bin/sh"],"apparmorProfile":""},"linux":{"mountLabel":""}}"#;
        let s = Spec::parse(empty.as_bytes()).expect("empty is unset");
        assert_eq!(s.mount_label(), None, "an empty label is no label");
        assert_eq!(s.process.as_ref().and_then(|p| p.apparmor_profile()), None);
    }

    // T-T7.1.4: a container in a user namespace that doesn't make a network namespace of its own can't
    // mount a sysfs, so its /sys is a bind of the host's, and holding CAP_SYS_ADMIN it could unmount
    // the mounts over that bind and read every box's cgroup tree beneath it
    // (docs/research/runtime/goal5-cgroupfs.md). Such a spec is refused by name, from any capability
    // set; with a network namespace of its own, without CAP_SYS_ADMIN, or outside a user namespace, it
    // parses.
    #[test]
    fn a_user_namespaced_container_on_a_borrowed_network_with_cap_sys_admin_is_refused() {
        let spec = |namespaces: &str, set: &str, cap: &str| {
            format!(
                r#"{{"ociVersion":"1.0.0","root":{{"path":"rootfs"}},
                    "process":{{"args":["/bin/sh"],"capabilities":{{"{set}":["{cap}"]}}}},
                    "linux":{{"namespaces":[{namespaces}]}}}}"#
            )
        };
        let host_network = r#"{"type":"user"},{"type":"pid"},{"type":"mount"}"#;
        let joined_network =
            r#"{"type":"user"},{"type":"mount"},{"type":"network","path":"/proc/1/ns/net"}"#;
        for set in [
            "bounding",
            "effective",
            "inheritable",
            "permitted",
            "ambient",
        ] {
            for namespaces in [host_network, joined_network] {
                let r = Spec::parse(spec(namespaces, set, "CAP_SYS_ADMIN").as_bytes());
                assert!(
                    matches!(&r, Err(OciError::Config(why))
                        if why.contains("CAP_SYS_ADMIN") && why.contains("network namespace")),
                    "CAP_SYS_ADMIN in {set} with {namespaces} should be refused by name, got {r:?}"
                );
            }
        }
        let own_network = r#"{"type":"user"},{"type":"network"},{"type":"mount"}"#;
        let no_user_namespace = r#"{"type":"pid"},{"type":"mount"}"#;
        for ok in [
            spec(own_network, "bounding", "CAP_SYS_ADMIN"),
            spec(host_network, "bounding", "CAP_CHOWN"),
            spec(no_user_namespace, "bounding", "CAP_SYS_ADMIN"),
        ] {
            assert!(Spec::parse(ok.as_bytes()).is_ok(), "{ok} should parse");
        }
    }

    // T-T7.1.4: a process whose spec names no capabilities keeps every one of its creator's (the init
    // changes none: init.rs, `caps`), CAP_SYS_ADMIN in its user namespace among them, so in a user
    // namespace on a borrowed network it is refused as a spec naming CAP_SYS_ADMIN is. With a network
    // namespace of its own, outside a user namespace, or with no process to run, it parses.
    #[test]
    fn a_user_namespaced_container_naming_no_capabilities_on_a_borrowed_network_is_refused() {
        let spec = |namespaces: &str, process: &str| {
            format!(
                r#"{{"ociVersion":"1.0.0","root":{{"path":"rootfs"}}{process},
                    "linux":{{"namespaces":[{namespaces}]}}}}"#
            )
        };
        let runs = r#","process":{"args":["/bin/sh"]}"#;
        for namespaces in [
            r#"{"type":"user"},{"type":"pid"},{"type":"mount"}"#,
            r#"{"type":"user"},{"type":"mount"},{"type":"network","path":"/proc/1/ns/net"}"#,
        ] {
            let r = Spec::parse(spec(namespaces, runs).as_bytes());
            assert!(
                matches!(&r, Err(OciError::Config(why))
                    if why.contains("CAP_SYS_ADMIN") && why.contains("names no capabilities")),
                "no capabilities with {namespaces} should be refused by name, got {r:?}"
            );
        }
        for ok in [
            spec(
                r#"{"type":"user"},{"type":"network"},{"type":"mount"}"#,
                runs,
            ),
            spec(r#"{"type":"pid"},{"type":"mount"}"#, runs),
            spec(r#"{"type":"user"},{"type":"pid"},{"type":"mount"}"#, ""),
        ] {
            assert!(Spec::parse(ok.as_bytes()).is_ok(), "{ok} should parse");
        }
    }

    // AC-T7.1.4 / CLAUDE.md §6: linux.seccomp now parses into its fields (it is no longer refused at
    // the spec), so a wrong field name wouldn't silently drop part of the filter. The compile step
    // (super::seccomp) is what refuses an unsupported profile; here we only check the model reads.
    #[test]
    fn a_seccomp_profile_parses_into_its_fields() {
        let json = r#"{"ociVersion":"1.0.0","root":{"path":"rootfs"},"process":{"args":["/bin/sh"]},
            "linux":{"seccomp":{
                "defaultAction":"SCMP_ACT_ERRNO","defaultErrnoRet":38,
                "architectures":["SCMP_ARCH_X86_64","SCMP_ARCH_X86"],
                "flags":["SECCOMP_FILTER_FLAG_TSYNC"],
                "syscalls":[
                    {"names":["read","write"],"action":"SCMP_ACT_ALLOW"},
                    {"names":["socket"],"action":"SCMP_ACT_ERRNO","errnoRet":22,
                     "args":[{"index":0,"value":16,"op":"SCMP_CMP_EQ"},
                             {"index":2,"value":9,"valueTwo":9,"op":"SCMP_CMP_MASKED_EQ"}]}
                ]}}}"#;
        let spec = Spec::parse(json.as_bytes()).unwrap();
        let sc = spec.linux.seccomp.unwrap();
        assert_eq!(sc.default_action, "SCMP_ACT_ERRNO");
        assert_eq!(sc.default_errno_ret, Some(38));
        assert_eq!(sc.architectures, ["SCMP_ARCH_X86_64", "SCMP_ARCH_X86"]);
        assert_eq!(sc.flags, ["SECCOMP_FILTER_FLAG_TSYNC"]);
        assert_eq!(sc.syscalls.len(), 2);
        assert_eq!(sc.syscalls[0].names, ["read", "write"]);
        assert_eq!(sc.syscalls[0].action, "SCMP_ACT_ALLOW");
        let netlink = &sc.syscalls[1];
        assert_eq!(
            (netlink.action.as_str(), netlink.errno_ret),
            ("SCMP_ACT_ERRNO", Some(22))
        );
        assert_eq!(netlink.args[0].index, 0);
        assert_eq!(
            (netlink.args[1].value, netlink.args[1].value_two),
            (9, Some(9))
        );
        assert_eq!(netlink.args[1].op, "SCMP_CMP_MASKED_EQ");
    }

    // Hostile input to the seccomp model is a typed error, not a panic (CLAUDE.md §6): a group
    // missing its required `action`, or an arg missing `op`, is malformed and refused at parse.
    #[test]
    fn a_malformed_seccomp_profile_is_a_reason_not_a_panic() {
        for bad in [
            // syscalls entry with no action
            r#"{"ociVersion":"1.0.0","root":{"path":"rootfs"},"process":{"args":["/bin/sh"]},
                "linux":{"seccomp":{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["read"]}]}}}"#,
            // no defaultAction
            r#"{"ociVersion":"1.0.0","root":{"path":"rootfs"},"process":{"args":["/bin/sh"]},
                "linux":{"seccomp":{"syscalls":[]}}}"#,
            // arg with no op
            r#"{"ociVersion":"1.0.0","root":{"path":"rootfs"},"process":{"args":["/bin/sh"]},
                "linux":{"seccomp":{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[
                    {"names":["read"],"action":"SCMP_ACT_ERRNO","args":[{"index":0,"value":1}]}]}}}"#,
        ] {
            assert!(
                matches!(Spec::parse(bad.as_bytes()), Err(OciError::Config(_))),
                "malformed seccomp should be a Config error: {bad}"
            );
        }
    }

    // AC-T7.1.4: linux.resources parses under the spec's own field names, several of which aren't
    // plain camelCase (blockIO, kernelTCP, disableOOMKiller, throttleReadIOPSDevice): a wrong name
    // would read as absent, and the limit it holds would silently not be applied.
    #[test]
    fn resources_parse_under_the_specs_field_names() {
        let json = MINIMAL.replace(
            "\"maskedPaths\": [\"/proc/kcore\"]",
            r#""maskedPaths": ["/proc/kcore"], "cgroupsPath": "/a/b",
               "resources": {"devices": [{"allow": false, "access": "rwm"}],
                 "memory": {"limit": 1024, "swap": 2048, "kernelTCP": 1, "disableOOMKiller": false},
                 "cpu": {"shares": 1024, "realtimeRuntime": 1, "cpus": "0-1"},
                 "blockIO": {"weight": 500, "throttleReadIOPSDevice": [{"major": 8, "minor": 0, "rate": 100}]},
                 "hugepageLimits": [{"pageSize": "2MB", "limit": 1}],
                 "network": {"classID": 1},
                 "pids": {"limit": -1},
                 "rdma": {"mlx5_0": {"hcaHandles": 1}},
                 "unified": {"memory.high": "1"}}"#,
        );
        let spec = Spec::parse(json.as_bytes()).unwrap();
        assert_eq!(spec.linux.cgroups_path.as_deref(), Some("/a/b"));
        let r = spec.linux.resources.unwrap();
        assert!(!r.devices[0].allow);
        let m = r.memory.unwrap();
        assert_eq!(
            (m.limit, m.swap, m.kernel_tcp, m.disable_oom_killer),
            (Some(1024), Some(2048), Some(1), Some(false))
        );
        let c = r.cpu.unwrap();
        assert_eq!((c.shares, c.realtime_runtime), (Some(1024), Some(1)));
        assert_eq!(c.cpus.as_deref(), Some("0-1"));
        let b = r.block_io.unwrap();
        assert_eq!(b.weight, Some(500));
        assert_eq!(b.throttle_read_iops_device[0].rate, 100);
        assert_eq!(r.hugepage_limits[0].page_size, "2MB");
        assert!(r.network.is_some());
        assert_eq!(r.pids.unwrap().limit, Some(-1));
        assert_eq!(r.rdma["mlx5_0"].hca_handles, Some(1));
        assert_eq!(r.unified["memory.high"], "1");
    }

    #[test]
    fn an_unknown_field_is_ignored_not_refused() {
        let with_extra =
            MINIMAL.replace("\"readonly\": true", "\"readonly\": true, \"madeUp\": 42");
        assert!(Spec::parse(with_extra.as_bytes()).is_ok());
    }

    #[test]
    fn a_relative_mount_destination_or_path_is_refused() {
        let bad = MINIMAL.replace("/proc", "proc");
        assert!(matches!(
            Spec::parse(bad.as_bytes()),
            Err(OciError::Config(_))
        ));
    }

    #[test]
    fn hostile_bytes_are_a_reason_not_a_panic() {
        for bad in [
            "",
            "{}",
            "null",
            "[]",
            "{\"ociVersion\":\"\"}",
            "{not json",
            &"{".repeat(10_000),
        ] {
            assert!(Spec::parse(bad.as_bytes()).is_err(), "{bad:?} parsed");
        }
        assert!(Spec::parse(&vec![b'{'; MAX_CONFIG_BYTES + 1]).is_err());
    }
}
