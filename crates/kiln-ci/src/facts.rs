//! What a runner reports about itself (docs/ci.md W13-R01, W13-R02): the facts routing rules
//! and requirements are decided over (W05-R22). Sent by runners as JSON in `AgentMsg::Facts`;
//! every field is optional or defaulted so runners and servers of different versions agree.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
}

/// Agents' workspaces of their own on a machine: Slates volumes over their folders.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
#[serde(default)]
pub struct Cpu {
    pub model: Option<String>,
    pub cores: u32,
    pub threads: u32,
    /// Feature flags (`avx2`, `avx512f`, `sve`, …).
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mem {
    pub total: u64,
    pub free: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Disk {
    pub total: u64,
    pub free: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Gpu {
    pub vendor: String,
    pub model: String,
    pub memory: u64,
    pub driver: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Virt {
    /// `/dev/kvm` is usable by the runner.
    pub kvm: bool,
    /// Hypervisor.framework is usable (macOS).
    pub hvf: bool,
    pub nested: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Executor {
    pub version: Option<String>,
    /// Free-form details (`rootless: true`, shards' features, …).
    pub details: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Net {
    /// The internet is reachable directly.
    pub egress: bool,
    /// Only through this proxy.
    pub proxy: Option<String>,
}
