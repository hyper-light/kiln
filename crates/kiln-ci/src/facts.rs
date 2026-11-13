//! What a runner reports about itself (docs/ci.md W13-R01, W13-R02): the facts routing rules
//! and requirements are decided over (W05-R22). Sent by runners as JSON in `AgentMsg::Facts`;
//! every field is optional or defaulted so runners and servers of different versions agree.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct RunnerFacts {
    /// `linux`, `macos`, `windows`, … (lower case, Rust's `std::env::consts::OS` spelling).
    pub os: String,
    pub os_version: Option<String>,
    /// e.g. `ubuntu`, `arch`, `debian`, from `/etc/os-release`'s `ID`.
    pub os_distro: Option<String>,
    pub kernel: Option<String>,
    /// `x86_64`, `aarch64`, … (Rust's `std::env::consts::ARCH` spelling).
    pub arch: String,
    /// e.g. `v8` for arm64, `v7` for arm.
    pub arch_variant: Option<String>,
    pub cpu: Cpu,
    pub mem: Mem,
    /// The volume holding the runner's work directory.
    pub disk: Disk,
    pub gpus: Vec<Gpu>,
    /// Why no native GPU counters can be read on this host, as the retained provider reports it.
    #[cfg_attr(feature = "ts", ts(optional))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_unavailable: Option<String>,
    pub virt: Virt,
    /// Executors that work right now, each with its version and details.
    pub executors: BTreeMap<String, Executor>,
    /// Platforms registered for QEMU user emulation (`linux/arm64`, …).
    pub binfmt: Vec<String>,
    /// Tools found on `PATH`, name → version.
    pub tools: BTreeMap<String, String>,
    pub net: Net,
    /// Facts an administrator declared, and the results of declared probes (W13-R02).
    pub custom: BTreeMap<String, String>,
    /// When the facts were gathered (unix seconds).
    pub probed_at: i64,
    /// The kiln the runner runs (kiln's `fleet`).
    pub kiln: Option<KilnFacts>,
    /// Whether its agents can have workspaces of their own (Slates volumes), and why not.
    pub workspaces: Option<Workspaces>,
    /// What W24's work needs to know of the machine (docs/ci.md W24, `RunnerFacts.work`).
    pub work: Option<WorkFacts>,
    /// The machine itself: a hash of the id its OS keeps for it (W13-R01). The server keeps it with
    /// the runner's name, so another machine joining with the same host name isn't taken for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    /// What the work directory's volume is made of: its file system and the devices under it,
    /// each with its type, interconnect and health (docs/needs.md, W13-R01). Probed with the
    /// other facts; the counters that change (throughput, latency, errors) are measured when a
    /// decision needs them, not reported here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage: Option<Storage>,
    /// Runs that finished on it since it last said (docs/needs.md): sent when one finishes, kept
    /// by the server for the learned run-time model ([`crate::runtime`]), and not kept with the
    /// facts.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<crate::runtime::Observation>,
    /// What its own end of its link to the server negotiated (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 34):
    /// said with the facts it sends as it connects, kept by the server with that connection, and not kept with the
    /// facts. None from a runner from before it, and in a probe's report.
    #[cfg_attr(feature = "ts", ts(optional))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<Negotiated>,
}

/// What one end of a TLS 1.3 connection negotiated, as rustls names its cipher suite and key exchange group
/// (`TLS13_AES_256_GCM_SHA384`, `secp384r1MLKEM1024`): a link's `secured`, as kiln's reach says its links'.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct Negotiated {
    pub suite: String,
    pub group: String,
}

/// The work directory's file system and the devices it is on, as found without privilege
/// (docs/needs.md; W13-R01).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct Storage {
    /// Its type as the OS names it (`ext4`, `btrfs`, `xfs`, `apfs`, `NTFS`, `nfs4`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fs: Option<String>,
    /// Where it is mounted (`/`, `/System/Volumes/Data`, `C:\`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mount: Option<String>,
    /// What it is mounted from (`/dev/nvme0n1p3`, `/dev/disk3s5`, `server:/export`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// A file system served over the network (NFS, SMB, Ceph, a FUSE file system): no local
    /// device under it.
    pub network: bool,
    /// The physical devices under it (several for btrfs or md over many, a mirror, a stripe).
    pub devices: Vec<StorageDevice>,
}

/// What kind of device holds data, by what it is attached through, not its medium flag alone (a
/// virtio disk says it rotates; a disk image says it is solid state).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    /// NVMe over PCI Express (or Apple's fabric).
    Nvme,
    /// Solid state on another local bus (SATA, SAS, USB, eMMC).
    Ssd,
    /// Rotating media.
    Hdd,
    /// Over the network (NVMe over TCP, RDMA or FC; iSCSI; NBD; Ceph RBD).
    Network,
    /// A hypervisor's or a file's (virtio, Xen, Hyper-V, VMware, a loop device, a disk image).
    Virtual,
    /// Memory (`tmpfs`, `zram`, a RAM disk).
    Ram,
    /// What its OS doesn't say.
    #[default]
    #[serde(other)]
    Unknown,
}

/// One device under the work volume.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct StorageDevice {
    /// Its name on its OS (`nvme0n1`, `disk0`, `PhysicalDrive0`).
    pub name: String,
    pub kind: DeviceKind,
    /// What it is attached through (`pcie`, `tcp`, `rdma`, `sata`, `sas`, `usb`, `virtio`,
    /// `Apple Fabric`, `PCI-Express`, `NVMe`, …), as its OS names it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interconnect: Option<String>,
    /// What its driver declares (Linux `queue/rotational`, macOS `Medium Type`, Windows
    /// `IncursSeekPenalty`): reported, and weighed only with the interconnect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotational: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// `internal` or `external` (macOS's `Physical Interconnect Location`; a disk image's `file`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// Its own account of its health, where an unprivileged reader can have it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<DeviceHealth>,
}

/// A device's health as it reports it: NVMe's SMART / Health Information log page (NVMe Base
/// Specification §5.16.1.3), read through macOS's NVMeSMARTLib or Windows' protocol-specific
/// storage query, both unprivileged; or the OS's own verdict.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct DeviceHealth {
    /// NVMe's critical warning bits (spare below threshold, temperature, reliability degraded,
    /// read-only, volatile backup failed); 0 when none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub critical_warning: Option<u8>,
    /// Spare capacity left, percent, and the threshold the device warns below.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_spare: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_spare_threshold: Option<u8>,
    /// The vendor's estimate of its life used, percent (may pass 100).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percentage_used: Option<u8>,
    /// Unrecovered data integrity errors over its life.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_errors: Option<u64>,
    /// Error information log entries over its life.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_log_entries: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power_on_hours: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsafe_shutdowns: Option<u64>,
    /// Its composite temperature, kelvin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_k: Option<u16>,
    /// The OS's verdict where it gives one (Windows' `HealthStatus`: `healthy`, `warning`,
    /// `unhealthy`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

impl DeviceHealth {
    /// NVMe's SMART / Health Information log page (512 bytes, little-endian; NVMe Base
    /// Specification §5.16.1.3): byte 0 critical warning, 1-2 composite temperature (K), 3
    /// available spare, 4 its threshold, 5 percentage used, 128 power-on hours, 144 unsafe
    /// shutdowns, 160 media errors, 176 error log entries (each a 128-bit count, read to 64).
    pub fn of_nvme_log(page: &[u8]) -> Option<DeviceHealth> {
        let b = |i: usize| page.get(i).copied();
        let u64_at = |i: usize| page.get(i..i + 8).and_then(|s| s.try_into().ok()).map(u64::from_le_bytes);
        let temp = page.get(1..3).and_then(|s| s.try_into().ok()).map(u16::from_le_bytes).filter(|t| *t > 0);
        page.get(511)?;
        Some(DeviceHealth {
            critical_warning: b(0),
            temperature_k: temp,
            available_spare: b(3),
            available_spare_threshold: b(4),
            percentage_used: b(5),
            power_on_hours: u64_at(128),
            unsafe_shutdowns: u64_at(144),
            media_errors: u64_at(160),
            error_log_entries: u64_at(176),
            status: None,
        })
    }
}

/// The machine as W24's work sees it: its load now (admission, W24-R19). The work volume's free
/// disk is [`RunnerFacts::disk`]'s. Probed with the other facts, and said again as each run on
/// the runner ends (with [`RunnerFacts::runs`]): on change, never on a timer (docs/needs.md).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct WorkFacts {
    /// The share of time in which all the machine's non-idle tasks were stalled on I/O at once, in
    /// percent: Linux's pressure stall information, `/proc/pressure/io`'s `full`. Probed, its
    /// 10-second average; said as a run ends, the exact share over the interval since the run event
    /// before. `None` where the system doesn't report it.
    pub io_full: Option<f64>,
    /// The same of memory: the share in which all of them were stalled on memory at once
    /// (`/proc/pressure/memory`'s `full`), in percent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_full: Option<f64>,
    /// macOS's memory pressure level (`normal`, `warn`, `critical`); it keeps no stall times.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_level: Option<String>,
}

/// Agents' workspaces of their own on a machine: Slates volumes over their folders.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct Workspaces {
    /// Agents here can have them.
    pub available: bool,
    /// The Slates binary.
    pub slates: Option<String>,
    /// How a volume is mounted here: `fuse` (Linux) or `nfs` (macOS).
    pub mount: Option<String>,
    /// Its Slates instance answers now.
    pub running: bool,
    /// Why agents here can't have them.
    pub why_not: Option<String>,
}

/// The kiln a runner runs, and where kiln's source is on its machine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct KilnFacts {
    /// The commit it was built from; empty for a local build that knows none.
    pub commit: String,
    /// The start of its binary's SHA-256.
    pub build: String,
    pub version: String,
    /// The platform it was built for (`x86_64-unknown-linux-musl`, …).
    pub target: String,
    /// A checkout of kiln on the machine.
    pub checkout: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct Cpu {
    pub model: Option<String>,
    pub cores: u32,
    pub threads: u32,
    /// Feature flags (`avx2`, `avx512f`, `sve`, …).
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct Mem {
    pub total: u64,
    pub free: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct Disk {
    pub total: u64,
    pub free: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct Gpu {
    pub vendor: String,
    pub model: String,
    pub memory: u64,
    pub driver: Option<String>,
    /// Stable hardware identity when the provider reports one (PCI address or vendor UUID).
    #[cfg_attr(feature = "ts", ts(optional))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Utilisation in thousandths, absent when unreadable; never an invented idle sample.
    #[cfg_attr(feature = "ts", ts(optional))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy_per_mille: Option<u16>,
    /// Device memory in use, bytes; absent when unreadable.
    #[cfg_attr(feature = "ts", ts(optional))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_used: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct Virt {
    /// `/dev/kvm` is usable by the runner.
    pub kvm: bool,
    /// Hypervisor.framework is usable (macOS).
    pub hvf: bool,
    pub nested: bool,
    /// It starts microVMs itself: a hypervisor it can use and the runtime that boots them, which kiln
    /// carries from kiln plan 01a1cd8e862e7169a074db3d5fa33f3d's step 16 (Shards, through Hex). Said by
    /// the runner; one that doesn't say so starts none (step 32: where microVM agents and the work
    /// they start are placed).
    pub microvm: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(rename = "ExecutorFacts"))]
#[serde(default)]
pub struct Executor {
    pub version: Option<String>,
    /// Free-form details (`rootless: true`, shards' features, …).
    pub details: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct Net {
    /// The internet is reachable directly.
    pub egress: bool,
    /// Only through this proxy.
    pub proxy: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W13-R01 (docs/needs.md): an NVMe health log page as the specification lays it out.
    #[test]
    fn w13_r01_nvme_health_log_page() {
        let mut page = vec![0u8; 512];
        let mut put = |at: usize, bytes: &[u8]| {
            if let Some(s) = page.get_mut(at..at + bytes.len()) {
                s.copy_from_slice(bytes);
            }
        };
        put(0, &[0x04]);
        put(1, &310u16.to_le_bytes());
        put(3, &[97, 10, 3]);
        put(128, &4321u64.to_le_bytes());
        put(144, &12u64.to_le_bytes());
        put(160, &2u64.to_le_bytes());
        put(176, &40u64.to_le_bytes());
        let h = DeviceHealth::of_nvme_log(&page);
        let want = DeviceHealth { critical_warning: Some(4), temperature_k: Some(310), available_spare: Some(97), available_spare_threshold: Some(10), percentage_used: Some(3), power_on_hours: Some(4321), unsafe_shutdowns: Some(12), media_errors: Some(2), error_log_entries: Some(40), status: None };
        assert_eq!(h, Some(want));
        assert_eq!(DeviceHealth::of_nvme_log(page.get(..100).unwrap_or(&[])), None, "a short page");
    }

    /// Old runners' facts read as before; a device kind a newer runner names reads as unknown.
    #[test]
    fn w13_r01_storage_is_optional_and_grows() -> Result<(), serde_json::Error> {
        let f: RunnerFacts = serde_json::from_str(r#"{"os":"linux","arch":"x86_64"}"#)?;
        assert_eq!(f.storage, None);
        assert!(!serde_json::to_string(&f)?.contains("storage"));
        let d: StorageDevice = serde_json::from_str(r#"{"name":"x","kind":"optane"}"#)?;
        assert_eq!(d.kind, DeviceKind::Unknown);
        let d: StorageDevice = serde_json::from_str(r#"{"name":"nvme0n1","kind":"nvme","interconnect":"pcie"}"#)?;
        assert_eq!((d.kind, d.interconnect.as_deref()), (DeviceKind::Nvme, Some("pcie")));
        Ok(())
    }
}
