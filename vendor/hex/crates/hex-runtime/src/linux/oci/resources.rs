//! A container's `linux.resources` as cgroup v2 writes, and its allowed device list as an eBPF device
//! program (sans-IO; `run` makes the cgroup and applies the plan). The runtime spec gives limits in
//! cgroup v1's shape and lets a runtime convert them, but "if the conversion is not possible the runtime
//! MUST generate an error" (config-linux.md, "Unified"); conversions follow crun's documented table
//! (crun.1.md, "CGROUP v2"), and what cgroup v2 can't hold is refused by name. `unified` keys are
//! cgroup v2 files, written as given after the converted values, so they win.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::linux::oci::OciError;
use crate::linux::oci::spec::{BlockIo, Cpu, DeviceRule, Memory, Resources};

/// `cpu.max`'s period when `cpu.period` is unset: the kernel's default, 100 ms.
const DEFAULT_CPU_PERIOD_US: u64 = 100_000;
/// Most device exceptions one container's program holds. Each costs nine instructions, so 256 keep the
/// program under the verifier's 4,096-instruction limit for unprivileged loads (`BPF_MAXINSNS`,
/// include/uapi/linux/bpf_common.h), well within a privileged one's million.
pub const MAX_DEVICE_EXCEPTIONS: usize = 256;

/// One write into the container's cgroup directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Write {
    pub file: String,
    pub value: String,
}

/// What a container's cgroup needs: the controllers its writes use, the writes in order, and the
/// device program, if the allowed device list restricts anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub controllers: BTreeSet<String>,
    pub writes: Vec<Write>,
    pub devices: Option<Vec<Insn>>,
    /// Whether the device policy allows by default (the spec has an `a *:* rwm` allow). A rootless
    /// container can't load the cgroup device program, so a deny-by-default allow-list it enforces by
    /// construction instead (`run::attach_device_program`); an allow-by-default policy with denials has
    /// no such construction and is refused there. `false` (deny by default) is the safe default value.
    pub devices_allow_by_default: bool,
}

impl Plan {
    /// Refuses a device policy a container in a user namespace of its own can't be held to. Such a
    /// container gets no device program of its own: loading one needs CAP_BPF in the cgroup's owning
    /// (initial) user namespace, which an unprivileged runtime lacks. A deny-by-default allow-list it
    /// enforces by construction instead — only its devices get nodes, `mknod` is refused across the
    /// user-namespace boundary (`init.rs`), and the mapped host uid's DAC governs access
    /// (docs/design/runtime.md, "Rootless devices", measured on the running hardware). An
    /// allow-by-default policy with denials has no such construction, so it's refused rather than
    /// silently under-enforced (CLAUDE.md §3).
    pub fn check_in_userns(&self) -> Result<(), OciError> {
        if self.devices.is_some() && self.devices_allow_by_default {
            return Err(OciError::Config(
                "linux.resources.devices: a rootless container can't enforce an allow-by-default \
                 device policy with denials — loading the cgroup device program needs CAP_BPF, which \
                 an unprivileged runtime lacks; express the policy as a deny-by-default allow-list, \
                 which a rootless container enforces by construction"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// Adds a write; `controller` is empty for a `cgroup.*` core file, which needs none.
    fn write(&mut self, controller: &str, file: impl Into<String>, value: impl Into<String>) {
        if !controller.is_empty() {
            self.controllers.insert(controller.to_owned());
        }
        self.writes.push(Write {
            file: file.into(),
            value: value.into(),
        });
    }
}

fn refuse(field: &str, why: impl std::fmt::Display) -> OciError {
    OciError::Config(format!("linux.resources.{field}: {why}"))
}

/// The plan for `resources`, or the first field cgroup v2 can't hold.
pub fn plan(r: &Resources) -> Result<Plan, OciError> {
    let mut p = Plan::default();
    if r.network.is_some() {
        return Err(refuse(
            "network",
            "net_cls and net_prio are cgroup v1 controllers, and cgroup v2 has neither",
        ));
    }
    if let Some(m) = &r.memory {
        memory(m, &mut p)?;
    }
    if let Some(c) = &r.cpu {
        cpu(c, &mut p)?;
    }
    if let Some(b) = &r.block_io {
        block_io(b, &mut p)?;
    }
    for h in &r.hugepage_limits {
        let digits = h
            .page_size
            .strip_suffix('B')
            .and_then(|s| s.strip_suffix(['K', 'M', 'G']));
        if !digits.is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())) {
            return Err(refuse(
                "hugepageLimits.pageSize",
                format!("{:?} isn't <digits><K|M|G>B", h.page_size),
            ));
        }
        p.write(
            "hugetlb",
            format!("hugetlb.{}.max", h.page_size),
            h.limit.to_string(),
        );
    }
    if let Some(limit) = r.pids.as_ref().and_then(|pids| pids.limit) {
        // 0 is a limit of zero, "and SHOULD be treated as such by runtimes" (config-linux.md, PIDs).
        let v = match limit {
            -1 => "max".to_owned(),
            l if l >= 0 => l.to_string(),
            l => return Err(refuse("pids.limit", format!("{l} is below -1"))),
        };
        p.write("pids", "pids.max", v);
    }
    for (dev, limits) in &r.rdma {
        if dev.is_empty() || dev.bytes().any(|b| b.is_ascii_whitespace()) {
            return Err(refuse("rdma", format!("{dev:?} isn't a device name")));
        }
        let mut parts = Vec::new();
        if let Some(n) = limits.hca_handles {
            parts.push(format!("hca_handle={n}"));
        }
        if let Some(n) = limits.hca_objects {
            parts.push(format!("hca_object={n}"));
        }
        if parts.is_empty() {
            return Err(refuse(
                "rdma",
                format!("{dev:?} names neither hcaHandles nor hcaObjects"),
            ));
        }
        p.write("rdma", "rdma.max", format!("{dev} {}", parts.join(" ")));
    }
    for (key, value) in &r.unified {
        p.write(&unified_controller(key)?, key.clone(), value.clone());
    }
    p.devices = devices(&r.devices)?;
    // A rootless container enforces an allow-list device policy by construction, not with the device
    // program; whether the policy instead allows by default decides that (`run::attach_device_program`).
    p.devices_allow_by_default = device_state(&r.devices)?.allow_by_default;
    Ok(p)
}

/// The controller a `unified` key needs: its prefix before the first `.`; `cgroup.*` files need none.
/// A key is a file in the container's cgroup directory, so one that could name anything else is
/// refused.
fn unified_controller(key: &str) -> Result<String, OciError> {
    let (prefix, rest) = key
        .split_once('.')
        .ok_or_else(|| refuse("unified", format!("{key:?} isn't <controller>.<file>")))?;
    let plain = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
    };
    if !plain(prefix) || !plain(rest) {
        return Err(refuse(
            "unified",
            format!("{key:?} isn't a cgroup v2 file name"),
        ));
    }
    Ok(if prefix == "cgroup" {
        String::new()
    } else {
        prefix.to_owned()
    })
}

/// A byte count: `-1` is unlimited (`max`).
fn bytes(field: &str, v: i64) -> Result<String, OciError> {
    match v {
        -1 => Ok("max".to_owned()),
        v if v >= 0 => Ok(v.to_string()),
        v => Err(refuse(field, format!("{v} is below -1"))),
    }
}

fn memory(m: &Memory, p: &mut Plan) -> Result<(), OciError> {
    for (field, set) in [
        ("memory.kernel", m.kernel.is_some()),
        ("memory.kernelTCP", m.kernel_tcp.is_some()),
        ("memory.swappiness", m.swappiness.is_some()),
    ] {
        if set {
            return Err(refuse(field, "cgroup v2 has no equivalent"));
        }
    }
    if m.disable_oom_killer == Some(true) {
        return Err(refuse(
            "memory.disableOOMKiller",
            "cgroup v2 has no switch for a cgroup's OOM killer",
        ));
    }
    if m.use_hierarchy == Some(false) {
        return Err(refuse(
            "memory.useHierarchy",
            "cgroup v2 always accounts hierarchically",
        ));
    }
    // checkBeforeUpdate governs an update, and create sets the first limit, so there is nothing to check.
    if let Some(limit) = m.limit {
        p.write("memory", "memory.max", bytes("memory.limit", limit)?);
    }
    if let Some(reservation) = m.reservation {
        p.write(
            "memory",
            "memory.low",
            bytes("memory.reservation", reservation)?,
        );
    }
    if let Some(swap) = m.swap {
        // cgroup v1's swap limit counts memory too, so memory.swap.max is what it leaves past the
        // memory limit (crun.1.md: y = x - memory_limit).
        let v = match (swap, m.limit) {
            (-1, _) => "max".to_owned(),
            (s, Some(l)) if l >= 0 => s
                .checked_sub(l)
                .filter(|d| *d >= 0)
                .ok_or_else(|| {
                    refuse(
                        "memory.swap",
                        format!("{s} is under memory.limit {l}, and v1's swap counts memory too"),
                    )
                })?
                .to_string(),
            (s, _) => {
                return Err(refuse(
                    "memory.swap",
                    format!(
                        "{s} converts to memory.swap.max only beside a memory.limit, since v1's swap counts memory too"
                    ),
                ));
            }
        };
        p.write("memory", "memory.swap.max", v);
    }
    Ok(())
}

/// `cpu.shares` as `cpu.weight`, as crun converts it (src/libcrun/cgroup-internal.h,
/// `convert_shares_to_weight`): 2..=262144 onto 1..=10000 by a quadratic in log2 that keeps the
/// default, 1024, at 100, rounded up. `None` for 0, which is unset.
pub fn shares_to_weight(shares: u64) -> Option<String> {
    match shares {
        0 => None,
        1 | 2 => Some("1".to_owned()),
        s if s >= 262_144 => Some("10000".to_owned()),
        s => {
            let l = f64::from(u32::try_from(s).ok()?).log2();
            let exponent = (l * l + 125.0 * l) / 612.0 - 7.0 / 34.0;
            Some(format!("{:.0}", 10f64.powf(exponent).ceil()))
        }
    }
}

fn cpu(c: &Cpu, p: &mut Plan) -> Result<(), OciError> {
    if c.realtime_runtime.is_some() || c.realtime_period.is_some() {
        return Err(refuse(
            "cpu.realtimeRuntime",
            "cgroup v2 has no realtime bandwidth control (crun.1.md, CGROUP v2)",
        ));
    }
    if let Some(weight) = c.shares.and_then(shares_to_weight) {
        p.write("cpu", "cpu.weight", weight);
    }
    if c.quota.is_some() || c.period.is_some() {
        let period = c.period.unwrap_or(DEFAULT_CPU_PERIOD_US);
        if period == 0 {
            return Err(refuse("cpu.period", "0 is no period"));
        }
        let quota = match c.quota {
            None | Some(-1) => "max".to_owned(),
            Some(q) if q > 0 => q.to_string(),
            Some(q) => {
                return Err(refuse(
                    "cpu.quota",
                    format!("{q} is neither -1 nor positive"),
                ));
            }
        };
        p.write("cpu", "cpu.max", format!("{quota} {period}"));
    }
    if let Some(burst) = c.burst {
        // "If specified, this value MUST be no larger than any positive quota" (config-linux.md, CPU).
        if let Some(q) = c.quota.and_then(|q| u64::try_from(q).ok())
            && q > 0
            && burst > q
        {
            return Err(refuse(
                "cpu.burst",
                format!("{burst} is over cpu.quota {q}"),
            ));
        }
        p.write("cpu", "cpu.max.burst", burst.to_string());
    }
    if let Some(idle) = c.idle {
        if !(0..=1).contains(&idle) {
            return Err(refuse("cpu.idle", format!("{idle} is neither 0 nor 1")));
        }
        p.write("cpu", "cpu.idle", idle.to_string());
    }
    for (file, value) in [("cpuset.cpus", &c.cpus), ("cpuset.mems", &c.mems)] {
        if let Some(v) = value.as_ref().filter(|v| !v.is_empty()) {
            p.write("cpuset", file, v.clone());
        }
    }
    Ok(())
}

/// A blkio weight (10..=1000) as an `io.weight` (1..=10000), linearly (crun.1.md: y = 1 + (x-10)*9999/990).
fn io_weight(field: &str, x: u16) -> Result<u32, OciError> {
    if !(10..=1000).contains(&x) {
        return Err(refuse(field, format!("{x} is outside 10..=1000")));
    }
    u32::from(x)
        .checked_sub(10)
        .and_then(|d| d.checked_mul(9999))
        .and_then(|d| d.checked_div(990))
        .and_then(|d| d.checked_add(1))
        .ok_or_else(|| refuse(field, format!("{x} overflows the conversion")))
}

/// A device number from the spec: non-negative and within a `u32`.
fn dev_num(field: &str, n: i64) -> Result<u32, OciError> {
    u32::try_from(n).map_err(|_| refuse(field, format!("{n} isn't a device number")))
}

fn block_io(b: &BlockIo, p: &mut Plan) -> Result<(), OciError> {
    if b.leaf_weight.is_some() || b.weight_device.iter().any(|d| d.leaf_weight.is_some()) {
        return Err(refuse("blockIO.leafWeight", "cgroup v2 has no leaf weight"));
    }
    if let Some(w) = b.weight {
        p.write(
            "io",
            "io.weight",
            format!("default {}", io_weight("blockIO.weight", w)?),
        );
    }
    for d in &b.weight_device {
        let w = d.weight.ok_or_else(|| {
            refuse(
                "blockIO.weightDevice",
                format!("{}:{} names no weight", d.major, d.minor),
            )
        })?;
        p.write(
            "io",
            "io.weight",
            format!(
                "{}:{} {}",
                dev_num("blockIO.weightDevice.major", d.major)?,
                dev_num("blockIO.weightDevice.minor", d.minor)?,
                io_weight("blockIO.weightDevice.weight", w)?
            ),
        );
    }
    for (field, key, list) in [
        (
            "blockIO.throttleReadBpsDevice",
            "rbps",
            &b.throttle_read_bps_device,
        ),
        (
            "blockIO.throttleWriteBpsDevice",
            "wbps",
            &b.throttle_write_bps_device,
        ),
        (
            "blockIO.throttleReadIOPSDevice",
            "riops",
            &b.throttle_read_iops_device,
        ),
        (
            "blockIO.throttleWriteIOPSDevice",
            "wiops",
            &b.throttle_write_iops_device,
        ),
    ] {
        for d in list {
            p.write(
                "io",
                "io.max",
                format!(
                    "{}:{} {key}={}",
                    dev_num(field, d.major)?,
                    dev_num(field, d.minor)?,
                    d.rate
                ),
            );
        }
    }
    Ok(())
}

// --- The allowed device list ---------------------------------------------------------------------
//
// cgroup v2 has no devices files: a cgroup's device access is an eBPF program of type
// BPF_PROG_TYPE_CGROUP_DEVICE attached to it (Documentation/admin-guide/cgroup-v2.rst, "Device
// controller"). The spec's list is written in cgroup v1's terms and "MUST" be applied in order, so it
// is first run through cgroup v1's own rules (security/device_cgroup.c, devcgroup_update_access), which
// leaves a default (allow or deny) and a list of exceptions, then compiled to a program that checks a
// request against them as the v1 controller did (match_exception for a deny default,
// match_exception_partial for an allow default).

/// `bpf_cgroup_dev_ctx`'s device types (include/uapi/linux/bpf.h).
pub const DEV_BLOCK: u32 = 1;
pub const DEV_CHAR: u32 = 2;
/// `bpf_cgroup_dev_ctx`'s access bits, kept in the upper 16 bits of `access_type`.
pub const ACC_MKNOD: u32 = 1;
pub const ACC_READ: u32 = 2;
pub const ACC_WRITE: u32 = 4;
const ACC_ALL: u32 = ACC_MKNOD | ACC_READ | ACC_WRITE;

/// A major or minor that matches every device (cgroup v1's `*`).
pub const ANY: u32 = u32::MAX;

/// One cgroup v1 device exception: a block or char device, a major and minor (each may be [`ANY`]),
/// and the access it covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exception {
    pub kind: u32,
    pub major: u32,
    pub minor: u32,
    pub access: u32,
}

/// The cgroup v1 device state after a list of rules: the default and its exceptions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceState {
    pub allow_by_default: bool,
    pub exceptions: Vec<Exception>,
}

impl DeviceState {
    /// Whether the state permits `access` to device `kind` `major:minor`, by cgroup v1's checks.
    pub fn permits(&self, kind: u32, major: u32, minor: u32, access: u32) -> bool {
        let matches = |e: &&Exception| {
            e.kind == kind
                && (e.major == ANY || e.major == major)
                && (e.minor == ANY || e.minor == minor)
        };
        if self.allow_by_default {
            // match_exception_partial: denied when a deny exception shares any requested access.
            !self
                .exceptions
                .iter()
                .filter(matches)
                .any(|e| e.access & access != 0)
        } else {
            // match_exception: allowed when an allow exception covers every requested access.
            self.exceptions
                .iter()
                .filter(matches)
                .any(|e| access & !e.access == 0)
        }
    }
}

/// The devices a container may always use, allowed after the spec's list as crun allows them
/// (src/libcrun/cgroup-resources.c, `default_devices`): `mknod` of any device, and the nodes the spec
/// says a runtime "MUST also supply" (config-linux.md, "Default Devices") with the terminal ones:
/// null 1:3, random 1:8, full 1:7, tty 5:0, zero 1:5, urandom 1:9, console 5:1, pts 136:*, ptmx 5:2,
/// and tun 10:200. runc appends the same list but console (libcontainer/specconv, AllowedDevices).
const DEFAULT_DEVICES: [Exception; 12] = [
    dev(DEV_CHAR, ANY, ANY, ACC_MKNOD),
    dev(DEV_BLOCK, ANY, ANY, ACC_MKNOD),
    dev(DEV_CHAR, 1, 3, ACC_ALL),
    dev(DEV_CHAR, 1, 8, ACC_ALL),
    dev(DEV_CHAR, 1, 7, ACC_ALL),
    dev(DEV_CHAR, 5, 0, ACC_ALL),
    dev(DEV_CHAR, 1, 5, ACC_ALL),
    dev(DEV_CHAR, 1, 9, ACC_ALL),
    dev(DEV_CHAR, 5, 1, ACC_ALL),
    dev(DEV_CHAR, 136, ANY, ACC_ALL),
    dev(DEV_CHAR, 5, 2, ACC_ALL),
    dev(DEV_CHAR, 10, 200, ACC_ALL),
];

const fn dev(kind: u32, major: u32, minor: u32, access: u32) -> Exception {
    Exception {
        kind,
        major,
        minor,
        access,
    }
}

impl DeviceState {
    /// One rule by cgroup v1's update rules (devcgroup_update_access): an allow under an allow default
    /// (or a deny under a deny default) takes the access from the exceptions naming exactly this
    /// device (dev_exception_rm); otherwise the device's exception gains it (dev_exception_add).
    fn update(&mut self, allow: bool, ex: Exception) -> Result<(), OciError> {
        let same = |e: &Exception| (e.kind, e.major, e.minor) == (ex.kind, ex.major, ex.minor);
        if allow == self.allow_by_default {
            self.exceptions.retain_mut(|e| {
                if same(e) {
                    e.access &= !ex.access;
                }
                e.access != 0
            });
        } else if let Some(e) = self.exceptions.iter_mut().find(|e| same(e)) {
            e.access |= ex.access;
        } else {
            if self.exceptions.len() >= MAX_DEVICE_EXCEPTIONS {
                return Err(refuse(
                    "devices",
                    format!("more than {MAX_DEVICE_EXCEPTIONS} device exceptions"),
                ));
            }
            self.exceptions.push(ex);
        }
        Ok(())
    }
}

/// The device state a container gets: deny by default, as runc's emulator starts (its zero value)
/// and crun's program ends; then the spec's rules by cgroup v1's update rules, in order; then
/// [`DEFAULT_DEVICES`], allowed last, so a spec that denies everything still has the devices the spec
/// makes a runtime supply. Only a list that ends allowing everything leaves the container unrestricted.
pub fn device_state(rules: &[DeviceRule]) -> Result<DeviceState, OciError> {
    let mut st = DeviceState {
        allow_by_default: false,
        exceptions: Vec::new(),
    };
    for (i, rule) in rules.iter().enumerate() {
        let field = || format!("devices[{i}]");
        let kind = match rule.kind.as_deref() {
            None | Some("a") => None,
            Some("b") => Some(DEV_BLOCK),
            Some("c") => Some(DEV_CHAR),
            Some(other) => {
                return Err(refuse(&field(), format!("type {other:?} isn't a, b or c")));
            }
        };
        let num = |n: Option<i64>| match n {
            None => Ok(ANY),
            Some(n) => u32::try_from(n)
                .ok()
                .filter(|n| *n != ANY)
                .ok_or_else(|| refuse(&field(), format!("{n} isn't a device number"))),
        };
        let (major, minor) = (num(rule.major)?, num(rule.minor)?);
        let access = match rule.access.as_deref() {
            None => ACC_ALL,
            Some(a) => a.chars().try_fold(0u32, |acc, c| match c {
                'r' => Ok(acc | ACC_READ),
                'w' => Ok(acc | ACC_WRITE),
                'm' => Ok(acc | ACC_MKNOD),
                other => Err(refuse(
                    &field(),
                    format!("access {other:?} isn't r, w or m"),
                )),
            })?,
        };
        let Some(kind) = kind else {
            // Type `a` resets the whole state, whatever else the entry says (devcgroup_update_access).
            st = DeviceState {
                allow_by_default: rule.allow,
                exceptions: Vec::new(),
            };
            continue;
        };
        st.update(rule.allow, dev(kind, major, minor, access))?;
    }
    for d in DEFAULT_DEVICES {
        st.update(true, d)?;
    }
    Ok(st)
}

/// One eBPF instruction (`struct bpf_insn`, include/uapi/linux/bpf.h): opcode, registers, offset and
/// immediate, laid out as the kernel reads it. The register byte is two bitfields, `dst_reg:4` then
/// `src_reg:4`, which put the destination in the low nibble on a little-endian machine, as every Linux
/// target Hex builds for (x86_64, aarch64) is.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    pub code: u8,
    pub regs: u8,
    pub off: i16,
    pub imm: i32,
}

impl Insn {
    /// The instruction's bytes as `struct bpf_insn` holds them in memory, which is how the kernel dumps
    /// a loaded program's translated instructions (native byte order).
    pub fn bytes(&self) -> [u8; 8] {
        let [o0, o1] = self.off.to_ne_bytes();
        let [i0, i1, i2, i3] = self.imm.to_ne_bytes();
        [self.code, self.regs, o0, o1, i0, i1, i2, i3]
    }
}

// The opcodes the device program uses (include/uapi/linux/bpf_common.h and bpf.h).
pub const LDX_MEM_W: u8 = 0x61; // BPF_LDX | BPF_MEM | BPF_W: dst = *(u32 *)(src + off)
pub const MOV32_X: u8 = 0xbc; // BPF_ALU | BPF_MOV | BPF_X: dst = (u32) src
pub const AND32_K: u8 = 0x54; // BPF_ALU | BPF_AND | BPF_K: dst &= imm (32-bit)
pub const RSH32_K: u8 = 0x74; // BPF_ALU | BPF_RSH | BPF_K: dst >>= imm (32-bit)
pub const JNE32_K: u8 = 0x56; // BPF_JMP32 | BPF_JNE | BPF_K: if (u32) dst != imm, pc += off
pub const JEQ32_K: u8 = 0x16; // BPF_JMP32 | BPF_JEQ | BPF_K: if (u32) dst == imm, pc += off
pub const MOV64_K: u8 = 0xb7; // BPF_ALU64 | BPF_MOV | BPF_K: dst = imm
pub const EXIT: u8 = 0x95; // BPF_JMP | BPF_EXIT: return r0

fn insn(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> Insn {
    Insn {
        code,
        regs: (dst & 0x0f) | (src & 0x0f).wrapping_shl(4),
        off,
        imm,
    }
}

/// An immediate the program compares a `u32` against, as the instruction's `i32` bit pattern: JMP32
/// compares the low 32 bits, so the bits, not the sign, are what count.
fn imm(v: u32) -> i32 {
    i32::from_ne_bytes(v.to_ne_bytes())
}

/// The device program for `rules`, or `None` when they restrict nothing (allow by default, no
/// exceptions), in which case no program is attached.
pub fn devices(rules: &[DeviceRule]) -> Result<Option<Vec<Insn>>, OciError> {
    let st = device_state(rules)?;
    if st.allow_by_default && st.exceptions.is_empty() {
        return Ok(None);
    }
    Ok(Some(compile(&st)))
}

/// The default policy's program: what a spec that lists no devices gets, and one that denies them all,
/// since [`device_state`] allows [`DEFAULT_DEVICES`] after any list.
pub fn default_program() -> Result<Vec<Insn>, OciError> {
    Ok(compile(&device_state(&[])?))
}

/// The program for a policy that allows every device. Under the node's oci boxes' root, whose program is
/// the default policy's, a box with such a policy carries this as its own, so it never takes the root's
/// (goal 5 extended, step 1); elsewhere such a policy gets no program, as crun's does.
pub fn allow_all() -> Vec<Insn> {
    compile(&DeviceState {
        allow_by_default: true,
        exceptions: Vec::new(),
    })
}

/// Compiles a device state: r1 points at `bpf_cgroup_dev_ctx`; the program returns 1 to allow.
pub fn compile(st: &DeviceState) -> Vec<Insn> {
    let mut out = vec![
        insn(LDX_MEM_W, 2, 1, 0, 0),    // r2 = access_type
        insn(MOV32_X, 3, 2, 0, 0),      // r3 = r2
        insn(AND32_K, 3, 0, 0, 0xffff), // r3 = device type
        insn(MOV32_X, 4, 2, 0, 0),      // r4 = r2
        insn(RSH32_K, 4, 0, 0, 16),     // r4 = access bits
        insn(LDX_MEM_W, 5, 1, 4, 0),    // r5 = major
        insn(LDX_MEM_W, 6, 1, 8, 0),    // r6 = minor
    ];
    let decision = i32::from(!st.allow_by_default);
    for e in &st.exceptions {
        // A block: checks that skip to its end when the request doesn't match, then the decision.
        let mut checks = vec![(JNE32_K, 3, imm(e.kind))];
        if e.major != ANY {
            checks.push((JNE32_K, 5, imm(e.major)));
        }
        if e.minor != ANY {
            checks.push((JNE32_K, 6, imm(e.minor)));
        }
        // Under a deny default an exception allows only what it covers entirely; under an allow
        // default it denies any request that shares an access bit with it.
        let (mask, skip) = if st.allow_by_default {
            (e.access, JEQ32_K)
        } else {
            (!e.access & ACC_ALL, JNE32_K)
        };
        // The block is the checks, then mov r7 / and r7 / the access jump, then mov r0 / exit.
        let len = checks.len().saturating_add(5);
        for (at, (code, reg, value)) in checks.into_iter().enumerate() {
            out.push(insn(code, reg, 0, to_end(len, at), value));
        }
        let at = len.saturating_sub(5);
        out.push(insn(MOV32_X, 7, 4, 0, 0));
        out.push(insn(AND32_K, 7, 0, 0, imm(mask)));
        out.push(insn(skip, 7, 0, to_end(len, at.saturating_add(2)), 0));
        out.push(insn(MOV64_K, 0, 0, 0, decision));
        out.push(insn(EXIT, 0, 0, 0, 0));
    }
    out.push(insn(MOV64_K, 0, 0, 0, i32::from(st.allow_by_default)));
    out.push(insn(EXIT, 0, 0, 0, 0));
    out
}

/// The jump offset from instruction `at` of a block `len` long to the first instruction after it.
fn to_end(len: usize, at: usize) -> i16 {
    i16::try_from(len.saturating_sub(at).saturating_sub(1)).unwrap_or(i16::MAX)
}

// --- Where the cgroup goes ----------------------------------------------------------------------

/// Where a container's cgroup goes, relative to the cgroup v2 mount point. An absolute `cgroupsPath`
/// is taken relative to the mount point, as the spec requires; a relative one is rooted there too, and
/// an unset one is `/<id>`, as crun's cgroupfs driver does (src/libcrun/cgroup-cgroupfs.c,
/// `make_cgroup_path`). The spec lets a runtime refuse a value (config-linux.md, "Cgroups Path"): one
/// with an empty, `.` or `..` component, a `:` (the systemd driver's `slice:prefix:name`, which Hex
/// doesn't run) or a control character is refused, as is a path that names the mount point itself.
pub fn cgroup_path(cgroups_path: Option<&str>, id: &str) -> Result<PathBuf, OciError> {
    let raw = cgroups_path.unwrap_or(id);
    let field = "cgroupsPath";
    if raw.contains(':') {
        return Err(OciError::Config(format!(
            "linux.{field}: {raw:?} is in the systemd driver's slice:prefix:name form, which Hex's oci class doesn't run"
        )));
    }
    let mut out = PathBuf::new();
    for part in raw.trim_start_matches('/').split('/') {
        if part.is_empty() || part == "." || part == ".." || part.chars().any(char::is_control) {
            return Err(OciError::Config(format!(
                "linux.{field}: {raw:?} has a component {part:?} that isn't a plain name"
            )));
        }
        out.push(part);
    }
    Ok(out)
}

/// Where a container's cgroup goes, relative to the cgroup v2 mount point, for a runtime that runs as root
/// (`rootless_own` `None`): [`cgroup_path`]'s place. For one that runs rootless (`rootless_own` its own cgroup, relative
/// to the mount point), a spec that names no cgroupsPath puts it beside the runtime's own cgroup, the parent's child
/// named by the id, as crun 1.30.1's cgroupfs driver makes it where the generated path can't be used
/// (`make_new_sibling_cgroup`, src/libcrun/cgroup-cgroupfs.c:96-110, taken at :128-155): the hierarchy's root is
/// root's alone, so `/<id>` there is refused to every rootless runtime, and a rootless runtime's own cgroup is beneath
/// the one delegated to it. A cgroupsPath the spec names goes where [`cgroup_path`] puts it, either way.
pub fn cgroup_path_for(
    cgroups_path: Option<&str>,
    id: &str,
    rootless_own: Option<&Path>,
) -> Result<PathBuf, OciError> {
    let place = cgroup_path(cgroups_path, id)?;
    Ok(match (cgroups_path, rootless_own.and_then(Path::parent)) {
        (None, Some(beside)) => beside.join(place),
        _ => place,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::oci::devshare;
    use crate::linux::oci::spec::{HugepageLimit, Pids, Rdma, ThrottleDevice, WeightDevice};
    use proptest::prelude::*;

    fn writes(p: &Plan) -> Vec<(&str, &str)> {
        p.writes
            .iter()
            .map(|w| (w.file.as_str(), w.value.as_str()))
            .collect()
    }

    fn refused(r: &Resources, field: &str) {
        match plan(r) {
            Err(OciError::Config(why)) => {
                assert!(why.contains(field), "{why:?} doesn't name {field:?}")
            }
            other => panic!("{field}: {other:?}"),
        }
    }

    // AC-T7.1.4: shares convert exactly as crun converts them; the vectors are Python's IEEE doubles
    // and libm pow, as crun's C computes (src/libcrun/cgroup-internal.h).
    #[test]
    fn shares_convert_to_weight_as_crun_does() {
        for (shares, weight) in [
            (0, None),
            (1, Some(1)),
            (2, Some(1)),
            (3, Some(2)),
            (100, Some(17)),
            (256, Some(35)),
            (512, Some(59)),
            (1000, Some(99)),
            (1024, Some(100)),
            (2048, Some(174)),
            (4096, Some(303)),
            (10_000, Some(627)),
            (100_000, Some(4340)),
            (262_143, Some(10_000)),
            (262_144, Some(10_000)),
            (u64::MAX, Some(10_000)),
        ] {
            assert_eq!(
                shares_to_weight(shares),
                weight.map(|w: u32| w.to_string()),
                "{shares}"
            );
        }
    }

    // AC-T7.1.4: every convertible field becomes its cgroup v2 file, with crun's conversions.
    #[test]
    fn each_resource_becomes_its_cgroup_v2_file() {
        let r = Resources {
            memory: Some(Memory {
                limit: Some(512 << 20),
                reservation: Some(256 << 20),
                swap: Some(768 << 20),
                ..Memory::default()
            }),
            cpu: Some(Cpu {
                shares: Some(1024),
                quota: Some(50_000),
                period: Some(100_000),
                burst: Some(10_000),
                cpus: Some("0-3".into()),
                mems: Some("0".into()),
                idle: Some(0),
                ..Cpu::default()
            }),
            block_io: Some(BlockIo {
                weight: Some(100),
                weight_device: vec![WeightDevice {
                    major: 259,
                    minor: 0,
                    weight: Some(1000),
                    leaf_weight: None,
                }],
                throttle_write_bps_device: vec![ThrottleDevice {
                    major: 8,
                    minor: 16,
                    rate: 1 << 20,
                }],
                ..BlockIo::default()
            }),
            hugepage_limits: vec![HugepageLimit {
                page_size: "2MB".into(),
                limit: 1 << 30,
            }],
            pids: Some(Pids { limit: Some(64) }),
            rdma: [(
                "mlx5_0".to_owned(),
                Rdma {
                    hca_handles: Some(3),
                    hca_objects: None,
                },
            )]
            .into(),
            unified: [("memory.high".to_owned(), "400000000".to_owned())].into(),
            ..Resources::default()
        };
        let p = plan(&r).unwrap();
        assert_eq!(
            writes(&p),
            vec![
                ("memory.max", "536870912"),
                ("memory.low", "268435456"),
                ("memory.swap.max", "268435456"),
                ("cpu.weight", "100"),
                ("cpu.max", "50000 100000"),
                ("cpu.max.burst", "10000"),
                ("cpu.idle", "0"),
                ("cpuset.cpus", "0-3"),
                ("cpuset.mems", "0"),
                ("io.weight", "default 910"),
                ("io.weight", "259:0 10000"),
                ("io.max", "8:16 wbps=1048576"),
                ("hugetlb.2MB.max", "1073741824"),
                ("pids.max", "64"),
                ("rdma.max", "mlx5_0 hca_handle=3"),
                ("memory.high", "400000000"),
            ]
        );
        let want: BTreeSet<String> = ["cpu", "cpuset", "hugetlb", "io", "memory", "pids", "rdma"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(p.controllers, want);
        // No device list is a deny-all one, so the default devices' program is there all the same.
        assert_eq!(p.devices, devices(&[]).unwrap());
    }

    // AC-T7.1.4: -1 is max, a pids limit of 0 is a limit, and quota with no period takes the default.
    #[test]
    fn unlimited_values_and_defaults_are_the_kernels() {
        let r = Resources {
            memory: Some(Memory {
                limit: Some(-1),
                swap: Some(-1),
                ..Memory::default()
            }),
            cpu: Some(Cpu {
                quota: Some(-1),
                ..Cpu::default()
            }),
            pids: Some(Pids { limit: Some(0) }),
            ..Resources::default()
        };
        assert_eq!(
            writes(&plan(&r).unwrap()),
            vec![
                ("memory.max", "max"),
                ("memory.swap.max", "max"),
                ("cpu.max", "max 100000"),
                ("pids.max", "0"),
            ]
        );
        // Empty resources write nothing and need no controller; the device program is the defaults'.
        let empty = plan(&Resources::default()).unwrap();
        assert!(empty.writes.is_empty() && empty.controllers.is_empty());
        assert!(empty.devices.is_some());
    }

    // CLAUDE.md §3: what cgroup v2 can't hold is refused by name, never dropped.
    #[test]
    fn what_cgroup_v2_cant_hold_is_refused_by_name() {
        let mem = |m: Memory| Resources {
            memory: Some(m),
            ..Resources::default()
        };
        refused(
            &mem(Memory {
                kernel: Some(1),
                ..Memory::default()
            }),
            "memory.kernel",
        );
        refused(
            &mem(Memory {
                swappiness: Some(60),
                ..Memory::default()
            }),
            "memory.swappiness",
        );
        refused(
            &mem(Memory {
                disable_oom_killer: Some(true),
                ..Memory::default()
            }),
            "memory.disableOOMKiller",
        );
        refused(
            &mem(Memory {
                swap: Some(1 << 30),
                ..Memory::default()
            }),
            "memory.swap",
        );
        refused(
            &mem(Memory {
                limit: Some(1 << 30),
                swap: Some(1 << 20),
                ..Memory::default()
            }),
            "memory.swap",
        );
        refused(
            &mem(Memory {
                limit: Some(-2),
                ..Memory::default()
            }),
            "memory.limit",
        );
        let cpu = |c: Cpu| Resources {
            cpu: Some(c),
            ..Resources::default()
        };
        refused(
            &cpu(Cpu {
                realtime_runtime: Some(1),
                ..Cpu::default()
            }),
            "cpu.realtimeRuntime",
        );
        refused(
            &cpu(Cpu {
                quota: Some(1000),
                burst: Some(2000),
                ..Cpu::default()
            }),
            "cpu.burst",
        );
        refused(
            &cpu(Cpu {
                quota: Some(0),
                ..Cpu::default()
            }),
            "cpu.quota",
        );
        refused(
            &Resources {
                block_io: Some(BlockIo {
                    leaf_weight: Some(100),
                    ..BlockIo::default()
                }),
                ..Resources::default()
            },
            "blockIO.leafWeight",
        );
        refused(
            &Resources {
                block_io: Some(BlockIo {
                    weight: Some(5),
                    ..BlockIo::default()
                }),
                ..Resources::default()
            },
            "blockIO.weight",
        );
        refused(
            &Resources {
                network: Some(serde::de::IgnoredAny),
                ..Resources::default()
            },
            "network",
        );
        refused(
            &Resources {
                pids: Some(Pids { limit: Some(-5) }),
                ..Resources::default()
            },
            "pids.limit",
        );
        refused(
            &Resources {
                hugepage_limits: vec![HugepageLimit {
                    page_size: "2XB".into(),
                    limit: 1,
                }],
                ..Resources::default()
            },
            "hugepageLimits",
        );
        for key in [
            "memory",
            "../memory.max",
            "memory/max",
            "memory.",
            ".max",
            "a b.max",
        ] {
            refused(
                &Resources {
                    unified: [(key.to_owned(), "1".to_owned())].into(),
                    ..Resources::default()
                },
                "unified",
            );
        }
    }

    // AC-T7.1.4: a unified cgroup.* key needs no controller; others need their prefix.
    #[test]
    fn unified_keys_name_their_controller() {
        assert_eq!(unified_controller("cgroup.max.depth").unwrap(), "");
        assert_eq!(unified_controller("io.max").unwrap(), "io");
        assert_eq!(unified_controller("hugetlb.1GB.max").unwrap(), "hugetlb");
    }

    fn rule(
        allow: bool,
        kind: Option<&str>,
        major: Option<i64>,
        minor: Option<i64>,
        access: &str,
    ) -> DeviceRule {
        DeviceRule {
            allow,
            kind: kind.map(str::to_owned),
            major,
            minor,
            access: Some(access.to_owned()),
        }
    }

    /// The device program's decision for one request, by running it: just enough of eBPF for the
    /// opcodes `compile` emits, over `bpf_cgroup_dev_ctx`.
    fn run(prog: &[Insn], kind: u32, access: u32, major: u32, minor: u32) -> bool {
        let ctx = [(access << 16) | kind, major, minor];
        let mut r = [0u64; 11];
        let mut pc = 0usize;
        for _ in 0..prog.len() {
            let i = prog[pc];
            let (dst, src) = (usize::from(i.regs & 0x0f), usize::from(i.regs >> 4));
            let k = u32::from_ne_bytes(i.imm.to_ne_bytes());
            pc += 1;
            match i.code {
                LDX_MEM_W => r[dst] = u64::from(ctx[usize::try_from(i.off).unwrap() / 4]),
                MOV32_X => r[dst] = r[src] & 0xffff_ffff,
                AND32_K => r[dst] = (r[dst] & u64::from(k)) & 0xffff_ffff,
                RSH32_K => r[dst] = (r[dst] & 0xffff_ffff) >> k,
                JNE32_K => {
                    if (r[dst] & 0xffff_ffff) != u64::from(k) {
                        pc += usize::try_from(i.off).unwrap();
                    }
                }
                JEQ32_K => {
                    if (r[dst] & 0xffff_ffff) == u64::from(k) {
                        pc += usize::try_from(i.off).unwrap();
                    }
                }
                MOV64_K => r[dst] = u64::from(k),
                EXIT => return r[0] == 1,
                other => panic!("opcode {other:#x}"),
            }
        }
        panic!("the program ran off its end");
    }

    // AC-T7.1.4: runtime-tools' generated spec denies every device (generate.go: `[{"allow": false,
    // "access": "rwm"}]`). Its containers still have the devices a runtime must supply, as under runc
    // and crun, and nothing else: the bug the 2026-10-23 validation run found (4 of 58, every init's
    // mknod of /dev/null refused) is that a deny-all list denied those too.
    #[test]
    fn a_deny_all_list_still_allows_the_devices_a_runtime_must_supply() {
        let prog = devices(&[rule(false, None, None, None, "rwm")])
            .unwrap()
            .expect("a deny default needs a program");
        let rwm = ACC_ALL;
        for (major, minor, what) in [
            (1, 3, "null"),
            (1, 5, "zero"),
            (1, 7, "full"),
            (1, 8, "random"),
            (1, 9, "urandom"),
            (5, 0, "tty"),
            (5, 1, "console"),
            (5, 2, "ptmx"),
            (136, 4, "a pts"),
            (10, 200, "tun"),
        ] {
            assert!(run(&prog, DEV_CHAR, rwm, major, minor), "{what}");
        }
        assert!(run(&prog, DEV_BLOCK, ACC_MKNOD, 8, 0), "mknod of any node");
        assert!(!run(&prog, DEV_BLOCK, ACC_READ, 8, 0), "reading a disk");
        assert!(!run(&prog, DEV_CHAR, ACC_READ, 1, 1), "/dev/mem");
        assert!(
            !run(&prog, DEV_BLOCK, ACC_READ | ACC_WRITE, 1, 3),
            "a block device with char 1:3's numbers"
        );
    }

    // AC-T7.1.4: no list is the same as a deny-all one, as runc's and crun's containers get; a spec's
    // own allow adds to the defaults, and a list that ends allowing everything needs no program.
    #[test]
    fn no_list_restricts_as_deny_all_and_allow_all_needs_no_program() {
        assert_eq!(
            devices(&[]).unwrap(),
            devices(&[rule(false, None, None, None, "rwm")]).unwrap()
        );
        let fuse = devices(&[rule(true, Some("c"), Some(10), Some(229), "rw")])
            .unwrap()
            .expect("still deny by default");
        assert!(run(&fuse, DEV_CHAR, ACC_READ | ACC_WRITE, 10, 229));
        assert!(!run(&fuse, DEV_BLOCK, ACC_READ, 8, 0));
        let everything = vec![
            rule(false, None, None, None, "rwm"),
            rule(true, Some("a"), None, None, "rwm"),
        ];
        assert_eq!(devices(&everything).unwrap(), None);
    }

    /// A device rule. Its majors and minors are fixtures, kept small (majors 1-3, minors 0-2, beside the requests'
    /// 0-4 and 0-3) so that rules and requests name the same devices often and the rules' order decides.
    fn arb_rule() -> impl Strategy<Value = DeviceRule> {
        (
            any::<bool>(),
            prop_oneof![
                Just(None),
                Just(Some("a")),
                Just(Some("b")),
                Just(Some("c"))
            ],
            prop_oneof![Just(None), (1i64..4).prop_map(Some)],
            prop_oneof![Just(None), (0i64..3).prop_map(Some)],
            prop_oneof![
                Just("r"),
                Just("w"),
                Just("m"),
                Just("rw"),
                Just("rwm"),
                Just("wm")
            ],
        )
            .prop_map(|(allow, kind, major, minor, access)| rule(allow, kind, major, minor, access))
    }

    proptest! {
        // CLAUDE.md §6, model-based: for any rule list and any request, the compiled program decides
        // exactly as cgroup v1's checks do on the state the rules leave.
        #[test]
        fn the_program_decides_as_cgroup_v1_would(
            rules in proptest::collection::vec(arb_rule(), 0..12),
            kind in prop_oneof![Just(DEV_BLOCK), Just(DEV_CHAR)],
            access in 1u32..8,
            major in 0u32..5,
            minor in 0u32..4,
        ) {
            let st = device_state(&rules).unwrap();
            let want = st.permits(kind, major, minor, access);
            let got = match devices(&rules).unwrap() {
                None => true,
                Some(prog) => run(&prog, kind, access, major, minor),
            };
            prop_assert_eq!(got, want);
        }

        // AC-T7.1.4 (goal 5 extended, step 1), model-based: for any rule list, the program that decides for a box
        // (its own, or the oci boxes' root's default where the box is given nothing: devshare::give)
        // decides every request — each type, each access, majors and minors that the rules and the
        // default devices name and some they don't — as the policy's oracle does; and so with no program
        // at the root.
        #[test]
        fn the_program_that_decides_for_a_box_is_its_policys(
            rules in proptest::collection::vec(arb_rule(), 0..12),
        ) {
            let st = device_state(&rules).unwrap();
            let default = default_program().unwrap();
            for root in [Some(default.as_slice()), None] {
                let deciding = match devshare::give(devices(&rules).unwrap().as_deref(), root) {
                    devshare::Give::Nothing => root.map(<[Insn]>::to_vec),
                    devshare::Give::Program(p) => Some(p),
                };
                for kind in [DEV_BLOCK, DEV_CHAR] {
                    // Every access: r, w and m (1, 2, 4) and each union of them.
                    for access in 1u32..8 {
                        // The majors and minors of the devices the default program allows
                        // (Documentation/admin-guide/devices.txt: 1 mem, whose 3 null, 5 zero, 7 full, 8 random and
                        // 9 urandom; 5 tty, whose 0 tty, 1 console and 2 ptmx; 136 the ptys; 10 misc, whose 200 tun),
                        // those the rules name (majors 1-3, minors 0-2), and majors 0 and 4, which neither names.
                        for major in [0u32, 1, 2, 3, 4, 5, 10, 136] {
                            for minor in [0u32, 1, 2, 3, 5, 7, 8, 9, 200] {
                                let got = deciding
                                    .as_ref()
                                    .is_none_or(|p| run(p, kind, access, major, minor));
                                prop_assert_eq!(
                                    got,
                                    st.permits(kind, major, minor, access),
                                    "root {}, type {} {}:{} access {}",
                                    root.is_some(), kind, major, minor, access
                                );
                            }
                        }
                    }
                }
            }
        }

        // Hostile input (CLAUDE.md §6): any resources are a plan or a typed refusal, never a panic.
        #[test]
        fn any_resources_are_a_plan_or_a_refusal(
            limit in any::<i64>(), swap in any::<i64>(), shares in any::<u64>(),
            quota in any::<i64>(), period in any::<u64>(), weight in any::<u16>(),
            pids in any::<i64>(), key in "\\PC{0,12}", major in any::<i64>(),
        ) {
            let r = Resources {
                memory: Some(Memory { limit: Some(limit), swap: Some(swap), ..Memory::default() }),
                cpu: Some(Cpu { shares: Some(shares), quota: Some(quota), period: Some(period), ..Cpu::default() }),
                block_io: Some(BlockIo { weight: Some(weight), ..BlockIo::default() }),
                pids: Some(Pids { limit: Some(pids) }),
                unified: [(key, "1".to_owned())].into(),
                devices: vec![rule(false, Some("c"), Some(major), None, "rw")],
                ..Resources::default()
            };
            let _ = plan(&r);
        }
    }

    // AC-T7.1.4: the cgroup's place, as crun's cgroupfs driver puts it, and the values refused.
    #[test]
    fn the_cgroup_path_is_cruns_and_odd_paths_are_refused() {
        assert_eq!(cgroup_path(None, "c1").unwrap(), PathBuf::from("c1"));
        assert_eq!(
            cgroup_path(Some("/a/b"), "c1").unwrap(),
            PathBuf::from("a/b")
        );
        assert_eq!(
            cgroup_path(Some("a/b"), "c1").unwrap(),
            PathBuf::from("a/b")
        );
        for bad in [
            "/",
            "",
            "a//b",
            "a/../b",
            "./a",
            "system.slice:docker:c1",
            "a\nb",
        ] {
            assert!(
                matches!(cgroup_path(Some(bad), "c1"), Err(OciError::Config(_))),
                "{bad:?}"
            );
        }
    }

    // AC-T7.1.4 (runtime-tools' rootless subset): a rootless runtime's container of a spec naming no cgroupsPath goes
    // beside the runtime's own cgroup, as crun's does where the hierarchy's root can't be written; as root, and for a
    // path the spec names, the place is cgroup_path's.
    #[test]
    fn a_rootless_runtimes_container_goes_beside_its_own_cgroup_and_roots_where_it_went() {
        let own = Path::new("user.slice/user-1000.slice/user@1000.service/app.slice/s.scope/lab");
        assert_eq!(
            cgroup_path_for(None, "c1", None).unwrap(),
            PathBuf::from("c1")
        );
        assert_eq!(
            cgroup_path_for(None, "c1", Some(own)).unwrap(),
            PathBuf::from("user.slice/user-1000.slice/user@1000.service/app.slice/s.scope/c1")
        );
        // A runtime at its cgroup namespace's root (its own cgroup `/`), and one a level below it.
        assert_eq!(
            cgroup_path_for(None, "c1", Some(Path::new(""))).unwrap(),
            PathBuf::from("c1")
        );
        assert_eq!(
            cgroup_path_for(None, "c1", Some(Path::new("lab"))).unwrap(),
            PathBuf::from("c1")
        );
        for rootless in [None, Some(own)] {
            assert_eq!(
                cgroup_path_for(Some("/a/b"), "c1", rootless).unwrap(),
                PathBuf::from("a/b")
            );
        }
        for bad in ["..", "a:b", ""] {
            assert!(
                matches!(
                    cgroup_path_for(None, bad, Some(own)),
                    Err(OciError::Config(_))
                ),
                "{bad:?}"
            );
        }
    }
}
