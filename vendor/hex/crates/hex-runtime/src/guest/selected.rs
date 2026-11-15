//! AC-T7.4.1: a dedicated GPU guest's immutable launch receipt. Box records remain the
//! device-holder authority; this receipt names the VM which must end before that holder
//! can be released. An intent with no process identity is uncertainty, never absence.

use std::io::Read;
use std::path::{Path, PathBuf};

use hex_api::{
    BackingKey, BoxId, BoxSpec, CapacityResources, Class, DeviceId, DeviceKind, Execution,
    GpuEligibility, ImageArch, ImageOs, ImagePlatform, Refusal, RouteKey, RouteReceipt,
};
use hex_core::record::{BoxRecord, NativeScope};

use super::{GuestConfig, held};

const HEADER: &str = "hex-selected-guest 5";
const REPORT_HEADER: &str = "hex-selected-guest 4";
const ROUTE_HEADER: &str = "hex-selected-guest 3";
const SCOPE_HEADER: &str = "hex-selected-guest 2";
const LEGACY_HEADER: &str = "hex-selected-guest 1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Phase {
    Intent,
    Running(shards_api::Identity),
    Stopping(shards_api::Identity),
    Released {
        identity: Option<shards_api::Identity>,
        status: shards_api::ManagedStatus,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Record {
    pub id: BoxId,
    pub devices: Vec<DeviceId>,
    pub class: Class,
    pub scope: Option<NativeScope>,
    pub route: Option<RouteReceipt>,
    pub physical: Vec<shards_api::PhysicalGpu>,
    pub report: Option<shards_api::physical::GpuReport>,
    pub readiness: Option<super::observer::Evidence>,
    pub attempt: Option<super::observer::Attempt>,
    pub vcpus: u32,
    pub memory_mib: u64,
    pub phase: Phase,
}

fn invalid(why: &'static str) -> Refusal {
    let mut text = String::new();
    if text.try_reserve_exact(why.len()).is_err() {
        return allocation(why.len());
    }
    text.push_str(why);
    Refusal::Invalid {
        what: "selected GPU guest",
        why: text,
    }
}

fn allocation(bound: usize) -> Refusal {
    Refusal::Exhausted {
        what: "selected GPU guest record",
        bound,
    }
}

/// The VM's configured envelope, charged by the caller's existing admission owner before
/// boot. MiB and CPU milli are unit conversions, not an estimated overhead allowance.
pub fn resources(config: &GuestConfig) -> Result<CapacityResources, Refusal> {
    Ok(CapacityResources {
        cpu_milli: u64::from(config.vcpus)
            .checked_mul(1000)
            .ok_or_else(|| invalid("configured CPU milli overflows"))?,
        mem_bytes: config
            .memory_mib
            .checked_mul(1 << 20)
            .ok_or_else(|| invalid("configured memory bytes overflow"))?,
        ..CapacityResources::default()
    })
}

impl Record {
    pub fn intent(record: &BoxRecord, config: &GuestConfig) -> Result<Self, Refusal> {
        let receipt = Self::retained(record)?;
        let configured = resources(config)?;
        if u64::from(receipt.vcpus).checked_mul(1000) != Some(configured.cpu_milli)
            || receipt.memory_mib.checked_mul(1 << 20) != Some(configured.mem_bytes)
        {
            return Err(invalid("admitted VM disagrees with new configured VM"));
        }
        Ok(receipt)
    }

    pub fn retained(record: &BoxRecord) -> Result<Self, Refusal> {
        let scope = record.native_scope.ok_or(super::books::uncertain(
            &record.spec.id,
            "its record names no native scope for its selected guest",
        )?)?;
        let spec = &record.spec;
        spec.check()?;
        if spec
            .route_execution()
            .is_some_and(|execution| execution != Execution::ShardsDedicated)
        {
            return Err(invalid(
                "the retained route belongs to another execution backend",
            ));
        }
        let reserved = spec
            .allocation
            .ok_or_else(|| invalid("dedicated VM has no original allocation"))?
            .guest()?;
        if reserved.cpu_milli.checked_rem(1000) != Some(0)
            || reserved.mem_bytes.checked_rem(1 << 20) != Some(0)
        {
            return Err(invalid(
                "admitted VM resources are not exact native CPU/MiB units",
            ));
        }
        let vcpus = u32::try_from(
            reserved
                .cpu_milli
                .checked_div(1000)
                .ok_or_else(|| invalid("CPU unit conversion failed"))?,
        )
        .map_err(|_| invalid("admitted vCPUs exceed their native representation"))?;
        let memory_mib = reserved
            .mem_bytes
            .checked_div(1 << 20)
            .ok_or_else(|| invalid("memory unit conversion failed"))?;
        if (!matches!(spec.class.class(), Class::Oci | Class::Shards))
            || (spec.devices.is_empty() && spec.class.class() != Class::Shards)
            || spec.devices.iter().any(|id| id.kind != DeviceKind::Gpu)
        {
            return Err(invalid("a dedicated guest requires exact GPU claims"));
        }
        if spec
            .bounds
            .cpu_max_milli
            .is_none_or(|cpu| u64::from(cpu) > reserved.cpu_milli)
            || spec
                .bounds
                .memory_max
                .is_none_or(|mem| mem > reserved.mem_bytes)
        {
            return Err(invalid("box bounds must fit the configured VM envelope"));
        }
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(spec.devices.len())
            .map_err(|_| allocation(spec.devices.len()))?;
        devices.extend_from_slice(&spec.devices);
        devices.sort_unstable();
        Ok(Self {
            id: spec.id.try_clone()?,
            devices,
            class: spec.class.class(),
            scope: Some(scope),
            route: spec.route,
            physical: Vec::new(),
            report: None,
            readiness: None,
            attempt: None,
            vcpus,
            memory_mib,
            phase: Phase::Intent,
        })
    }

    pub fn matches(&self, spec: &BoxSpec) -> Result<(), Refusal> {
        spec.check()?;
        let allocation = spec
            .allocation
            .ok_or_else(|| invalid("dedicated record lost its admission receipt"))?;
        let guest = allocation.guest()?;
        let cpu = u64::from(self.vcpus)
            .checked_mul(1000)
            .ok_or_else(|| invalid("receipt CPU overflows"))?;
        let memory = self
            .memory_mib
            .checked_mul(1 << 20)
            .ok_or_else(|| invalid("receipt memory overflows"))?;
        if self.id != spec.id
            || self.class != spec.class.class()
            || self.route != spec.route
            || self.devices.len() != spec.devices.len()
            || !self
                .devices
                .iter()
                .all(|device| spec.devices.contains(device))
            || guest.cpu_milli != cpu
            || guest.mem_bytes != memory
        {
            return Err(invalid(
                "recovery disagrees with its original admission and launch receipt",
            ));
        }
        Ok(())
    }

    pub fn matches_record(&self, record: &BoxRecord) -> Result<(), Refusal> {
        self.matches(&record.spec)?;
        if self.scope.is_none() || self.scope != record.native_scope {
            return Err(super::books::uncertain(
                &record.spec.id,
                "its selected guest's scope isn't its record's",
            )?);
        }
        Ok(())
    }

    pub fn identity(&self) -> Result<&shards_api::Identity, Refusal> {
        match &self.phase {
            Phase::Running(identity)
            | Phase::Stopping(identity)
            | Phase::Released {
                identity: Some(identity),
                ..
            } => Ok(identity),
            Phase::Intent | Phase::Released { identity: None, .. } => Err(invalid(
                "a launch intent has no recorded process identity; physical absence is unproved",
            )),
        }
    }

    /// The private frame belongs to this original scope and complete selector
    /// set. It grants no lifecycle authority; native identity/absence is separate.
    pub fn checked_report(&self) -> Result<Option<&shards_api::physical::GpuReport>, Refusal> {
        if self.physical.is_empty() {
            return if self.report.is_none() {
                Ok(None)
            } else {
                Err(invalid("CPU-only receipt cannot carry a GPU report"))
            };
        }
        let report = self.report.as_ref().ok_or(super::books::uncertain(
            &self.id,
            "its selected guest has no native report",
        )?)?;
        let scope = self.scope.ok_or(super::books::uncertain(
            &self.id,
            "its selected guest has no scope",
        )?)?;
        super::physical::check_report(&self.devices, &self.physical, scope, report)?;
        Ok(Some(report))
    }

    pub fn checked_readiness(&self) -> Result<&super::observer::Evidence, Refusal> {
        let bad = |why: &str| {
            super::books::uncertain(&self.id, why).unwrap_or_else(std::convert::identity)
        };
        if self.physical.is_empty() || !matches!(self.phase, Phase::Running(_)) {
            return Err(bad("its selected guest holds no device, or isn't running"));
        }
        self.checked_report()?
            .ok_or_else(|| bad("its selected guest has no native report"))?;
        let readiness = self
            .readiness
            .as_ref()
            .ok_or_else(|| bad("its selected guest has no readiness"))?;
        readiness.check(
            self.scope
                .ok_or_else(|| bad("its selected guest has no scope"))?,
            self.physical.len(),
        )?;
        let attempt = self
            .attempt
            .as_ref()
            .ok_or_else(|| bad("its selected guest has no attempt"))?;
        if !attempt.has_facts()
            || !attempt.matches_readiness(readiness)?
            || !attempt.ready_facts()?
        {
            return Err(bad(
                "its selected guest's attempt has no facts, or none its readiness matches",
            ));
        }
        Ok(readiness)
    }

    /// Work-ready admission is stronger than legacy diagnostic receipt checks.
    /// The legacy payload remains available for the original scope's cleanup.
    pub fn checked_work_ready(&self) -> Result<&super::observer::Evidence, Refusal> {
        let readiness = self.checked_readiness()?;
        readiness
            .report
            .require_work_ready(self.physical.len(), readiness.max_queues)
            .map_err(|_| {
                super::books::uncertain(
                    &self.id,
                    "its selected guest's queues aren't ready for its work",
                )
                .unwrap_or_else(std::convert::identity)
            })?;
        Ok(readiness)
    }

    pub(super) fn encode(&self) -> Result<String, Refusal> {
        let bound = hex_core::bundle::MAX_CONFIG_BYTES;
        let report = self
            .report
            .as_ref()
            .map(|report| {
                self.checked_report()?;
                report
                    .encode()
                    .map_err(|_| invalid("GPU report cannot be encoded exactly"))
            })
            .transpose()?;
        let mut length = Length { bytes: 0, bound };
        self.render(&mut length, report.as_deref())
            .map_err(|_| allocation(bound))?;
        let mut text = String::new();
        text.try_reserve_exact(length.bytes)
            .map_err(|_| allocation(bound))?;
        self.render(&mut text, report.as_deref())
            .map_err(|_| allocation(bound))?;
        if text.len() > bound || parse(&text)? != *self {
            return Err(invalid("receipt is oversized or cannot be encoded exactly"));
        }
        Ok(text)
    }

    fn render(&self, text: &mut impl std::fmt::Write, report: Option<&str>) -> std::fmt::Result {
        let observing = self.attempt.is_some() || self.readiness.is_some();
        writeln!(text, "{}", if observing { HEADER } else { REPORT_HEADER })?;
        writeln!(text, "box {}", self.id)?;
        writeln!(text, "class {}", self.class.name())?;
        write!(text, "scope ")?;
        match self.scope {
            Some(scope) => {
                for byte in scope.bytes() {
                    write!(text, "{byte:02x}")?;
                }
                writeln!(text)?;
            }
            None => writeln!(text, "none")?,
        }
        render_route(text, self.route)?;
        writeln!(text, "vcpus {}", self.vcpus)?;
        writeln!(text, "memory_mib {}", self.memory_mib)?;
        writeln!(text, "devices {}", self.devices.len())?;
        if self.devices.len() != self.physical.len() {
            return Err(std::fmt::Error);
        }
        for (id, physical) in self.devices.iter().zip(&self.physical) {
            writeln!(text, "gpu {}", id.key)?;
            writeln!(text, "native {physical}")?;
        }
        render_report(text, report)?;
        if observing {
            write!(text, "readiness ")?;
            match &self.readiness {
                Some(readiness) => readiness.render(text)?,
                None => write!(text, "none")?,
            }
            writeln!(text)?;
            let attempt = self.attempt.as_ref().ok_or(std::fmt::Error)?;
            write!(text, "observer ")?;
            attempt.render(text)?;
            writeln!(text)?;
            attempt.render_facts(text)?;
        }
        render_phase(text, &self.phase)
    }
}

pub(super) fn render_phase(text: &mut impl std::fmt::Write, phase: &Phase) -> std::fmt::Result {
    match phase {
        Phase::Intent => writeln!(text, "intent"),
        Phase::Released {
            identity: None,
            status,
        } => {
            writeln!(text, "released-unpublished")?;
            render_status(text, *status)
        }
        Phase::Running(id)
        | Phase::Stopping(id)
        | Phase::Released {
            identity: Some(id), ..
        } => {
            let name = match phase {
                Phase::Running(_) => "running",
                Phase::Released { .. } => "released",
                _ => "stopping",
            };
            writeln!(text, "{name}")?;
            writeln!(text, "pid {}", id.pid)?;
            writeln!(text, "started {}", id.started)?;
            writeln!(text, "vsock {}", id.vsock.display())?;
            if let Phase::Released { status, .. } = phase {
                render_status(text, *status)?;
            }
            Ok(())
        }
    }
}

fn render_status(
    out: &mut impl std::fmt::Write,
    status: shards_api::ManagedStatus,
) -> std::fmt::Result {
    match status {
        shards_api::ManagedStatus::Code(code) => writeln!(out, "status code {code}"),
        shards_api::ManagedStatus::Signal(signal) => writeln!(out, "status signal {signal}"),
        shards_api::ManagedStatus::Unknown => writeln!(out, "status unknown"),
    }
}

fn parse_status(line: Option<&str>) -> Result<shards_api::ManagedStatus, Refusal> {
    let value = field(line, "status ")?;
    if value == "unknown" {
        return Ok(shards_api::ManagedStatus::Unknown);
    }
    let (kind, value) = value
        .split_once(' ')
        .ok_or_else(|| invalid("missing terminal status"))?;
    let number = value
        .parse::<i32>()
        .map_err(|_| invalid("invalid native terminal status"))?;
    match kind {
        "code" => Ok(shards_api::ManagedStatus::Code(number)),
        "signal" => Ok(shards_api::ManagedStatus::Signal(number)),
        _ => Err(invalid("unknown native terminal status kind")),
    }
}

struct Length {
    bytes: usize,
    bound: usize,
}

impl std::fmt::Write for Length {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        self.bytes = self
            .bytes
            .checked_add(value.len())
            .filter(|bytes| *bytes <= self.bound)
            .ok_or(std::fmt::Error)?;
        Ok(())
    }
}

pub(super) fn write(path: &Path, record: &Record) -> Result<(), Refusal> {
    let text = record.encode()?;
    held::replace(path, text.as_bytes())
}

pub(super) fn read(path: &Path) -> Result<Option<Record>, Refusal> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Refusal::os("opening selected guest receipt", error)),
    };
    let bound = hex_core::bundle::MAX_CONFIG_BYTES;
    let room = bound.checked_add(1).ok_or_else(|| allocation(bound))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(room)
        .map_err(|_| allocation(room))?;
    bytes.resize(room, 0);
    let mut read = 0usize;
    let mut file = file.take(u64::try_from(room).map_err(|_| allocation(room))?);
    while read < room {
        let tail = bytes.get_mut(read..).ok_or_else(|| allocation(room))?;
        let count = file
            .read(tail)
            .map_err(|e| Refusal::os("reading selected guest receipt", e))?;
        if count == 0 {
            break;
        }
        read = read.checked_add(count).ok_or_else(|| allocation(room))?;
    }
    if read > bound {
        return Err(invalid("receipt exceeds the native configuration bound"));
    }
    let text = std::str::from_utf8(bytes.get(..read).ok_or_else(|| allocation(room))?)
        .map_err(|_| invalid("receipt is not UTF-8"))?;
    parse(text).map(Some)
}

pub(super) fn remove(path: &Path) -> Result<(), Refusal> {
    held::remove(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("receipt has no parent"))?;
    match held::sync_folder(parent) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Refusal::os("syncing selected guest receipt removal", e)),
    }
}

fn parse(text: &str) -> Result<Record, Refusal> {
    if text.len() > hex_core::bundle::MAX_CONFIG_BYTES || !text.ends_with('\n') {
        return Err(invalid("oversized or truncated receipt"));
    }
    let mut lines = text.lines();
    let version = match lines.next() {
        Some(HEADER) => 5,
        Some(REPORT_HEADER) => 4,
        Some(ROUTE_HEADER) => 3,
        Some(SCOPE_HEADER) => 2,
        Some(LEGACY_HEADER) => 1,
        _ => return Err(invalid("unknown receipt version")),
    };
    let id = BoxId::new(field(lines.next(), "box ")?)?;
    let class = match field(lines.next(), "class ")? {
        "oci" => Class::Oci,
        "shards" => Class::Shards,
        _ => return Err(invalid("receipt names another backend")),
    };
    let scope = if version == 1 {
        None
    } else {
        parse_scope(field(lines.next(), "scope ")?)?
    };
    let route = if version >= 3 {
        parse_route(field(lines.next(), "route ")?)?
    } else {
        None
    };
    let vcpus = field(lines.next(), "vcpus ")?
        .parse::<u32>()
        .map_err(|_| invalid("invalid vCPU count"))?;
    let memory_mib = field(lines.next(), "memory_mib ")?
        .parse::<u64>()
        .map_err(|_| invalid("invalid memory size"))?;
    let count = field(lines.next(), "devices ")?
        .parse::<usize>()
        .map_err(|_| invalid("invalid device count"))?;
    if (count == 0 && class != Class::Shards)
        || count > hex_api::MAX_DEVICES
        || vcpus == 0
        || memory_mib == 0
    {
        return Err(invalid("empty or excessive selected guest envelope"));
    }
    let mut devices = Vec::new();
    let mut physical = Vec::new();
    physical
        .try_reserve_exact(count)
        .map_err(|_| allocation(count))?;
    devices
        .try_reserve_exact(count)
        .map_err(|_| allocation(count))?;
    for _ in 0..count {
        let key = field(lines.next(), "gpu ")?
            .parse::<u64>()
            .map_err(|_| invalid("invalid GPU identity"))?;
        let id = DeviceId {
            kind: DeviceKind::Gpu,
            key,
        };
        if devices.last().is_some_and(|previous| *previous >= id) {
            return Err(invalid("GPU identities are repeated or unordered"));
        }
        devices.push(id);
        let native = field(lines.next(), "native ")?
            .parse::<shards_api::PhysicalGpu>()
            .map_err(|_| invalid("invalid native GPU selector"))?;
        physical.push(native);
    }
    shards_api::physical::check_selection(&physical)
        .map_err(|_| invalid("native selectors repeat or exceed their representation"))?;
    if let Some(route) = route {
        route.check(class, count != 0)?;
        if route.key.execution != Execution::ShardsDedicated {
            return Err(invalid(
                "a selected receipt requires dedicated Shards execution",
            ));
        }
    }
    let report = if version >= 4 {
        parse_report(field(lines.next(), "report ")?, scope, &physical)?
    } else {
        None
    };
    let (readiness, attempt) = if version >= 5 {
        if physical.is_empty() || report.is_none() {
            return Err(invalid(
                "CPU-only or reportless receipt cannot contain a GPU observer",
            ));
        }
        let scope = scope.ok_or_else(|| invalid("GPU observer receipt lost its original scope"))?;
        let readiness = super::observer::Evidence::parse(
            field(lines.next(), "readiness ")?,
            scope,
            physical.len(),
        )?;
        let mut attempt = super::observer::Attempt::parse(field(lines.next(), "observer ")?)?
            .ok_or_else(|| invalid("GPU observer receipt has no actual attempt"))?;
        attempt.parse_facts(&mut lines)?;
        (readiness, Some(attempt))
    } else {
        (None, None)
    };
    let phase = parse_phase(&mut lines)?;
    if lines.next().is_some() {
        return Err(invalid("receipt has trailing data"));
    }
    Ok(Record {
        id,
        devices,
        class,
        scope,
        route,
        physical,
        report,
        readiness,
        attempt,
        vcpus,
        memory_mib,
        phase,
    })
}

pub(super) fn render_report(
    text: &mut impl std::fmt::Write,
    report: Option<&str>,
) -> std::fmt::Result {
    write!(text, "report ")?;
    match report {
        None => writeln!(text, "none"),
        Some(frame) => {
            for byte in frame.as_bytes() {
                write!(text, "{byte:02x}")?;
            }
            writeln!(text)
        }
    }
}

pub(super) fn parse_report(
    text: &str,
    scope: Option<NativeScope>,
    physical: &[shards_api::PhysicalGpu],
) -> Result<Option<shards_api::physical::GpuReport>, Refusal> {
    if text == "none" {
        return Ok(None);
    }
    let scope = scope.ok_or_else(|| invalid("GPU report has no original native scope"))?;
    let bound = shards_api::physical::report_bytes(physical.len())
        .map_err(|_| invalid("GPU report count exceeds its checked bound"))?;
    let maximum = bound.checked_mul(2).ok_or_else(|| allocation(bound))?;
    let (pairs, tail) = text.as_bytes().as_chunks::<2>();
    if text.is_empty()
        || text.len() > maximum
        || !tail.is_empty()
        || !text.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid("GPU report has malformed or excessive bytes"));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(text.len().checked_div(2).ok_or_else(|| allocation(bound))?)
        .map_err(|_| allocation(bound))?;
    for pair in pairs {
        let pair = std::str::from_utf8(pair).map_err(|_| invalid("GPU report is not ASCII hex"))?;
        bytes
            .push(u8::from_str_radix(pair, 16).map_err(|_| invalid("GPU report has invalid hex"))?);
    }
    let frame =
        std::str::from_utf8(&bytes).map_err(|_| invalid("GPU report frame is not UTF-8"))?;
    shards_api::physical::GpuReport::parse(frame, *scope.bytes(), physical)
        .map(Some)
        .map_err(|_| invalid("GPU report frame differs from original scope or selectors"))
}

pub(super) fn parse_phase<'a>(lines: &mut impl Iterator<Item = &'a str>) -> Result<Phase, Refusal> {
    let phase = match lines.next() {
        Some("intent") => Phase::Intent,
        Some("released-unpublished") => Phase::Released {
            identity: None,
            status: parse_status(lines.next())?,
        },
        Some(phase @ ("running" | "stopping" | "released")) => {
            let pid = field(lines.next(), "pid ")?
                .parse::<u32>()
                .map_err(|_| invalid("invalid process identity"))?;
            let started = field(lines.next(), "started ")?
                .parse::<u64>()
                .map_err(|_| invalid("invalid process start"))?;
            let vsock = field(lines.next(), "vsock ")?;
            if pid == 0
                || started == 0
                || vsock.is_empty()
                || vsock.contains('\0')
                || vsock.contains('\r')
            {
                return Err(invalid("empty process identity or socket"));
            }
            let identity = shards_api::Identity {
                pid,
                started,
                vsock: {
                    let mut path = PathBuf::new();
                    path.try_reserve_exact(vsock.len())
                        .map_err(|_| allocation(vsock.len()))?;
                    path.push(vsock);
                    path
                },
            };
            match phase {
                "running" => Phase::Running(identity),
                "released" => Phase::Released {
                    identity: Some(identity),
                    status: parse_status(lines.next())?,
                },
                _ => Phase::Stopping(identity),
            }
        }
        _ => return Err(invalid("unknown launch phase")),
    };
    Ok(phase)
}

fn parse_scope(text: &str) -> Result<Option<NativeScope>, Refusal> {
    if text == "none" {
        return Ok(None);
    }
    // NativeScope is the canonical core's fixed 128-bit incarnation, encoded as
    // two ASCII hex digits per byte. Never derive it from a reusable socket/PID.
    Ok(Some(NativeScope::from_bytes(parse_hex(text)?)))
}

fn parse_hex<const N: usize>(text: &str) -> Result<[u8; N], Refusal> {
    let mut bytes = [0; N];
    let digits = bytes
        .len()
        .checked_mul(2)
        .ok_or_else(|| invalid("receipt identity representation overflows"))?;
    if text.len() != digits || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("receipt identity is not exact ASCII hexadecimal"));
    }
    let (pairs, _) = text.as_bytes().as_chunks::<2>();
    for (byte, pair) in bytes.iter_mut().zip(pairs) {
        *byte = u8::from_str_radix(
            std::str::from_utf8(pair).map_err(|_| invalid("receipt identity is not ASCII"))?,
            16,
        )
        .map_err(|_| invalid("receipt identity byte is invalid"))?;
    }
    Ok(bytes)
}

fn render_route(out: &mut impl std::fmt::Write, receipt: Option<RouteReceipt>) -> std::fmt::Result {
    let Some(receipt) = receipt else {
        return writeln!(out, "route none");
    };
    let key = receipt.key;
    let os = match key.image.os {
        ImageOs::Linux => "linux",
        ImageOs::Windows => "windows",
        ImageOs::Macos => "macos",
    };
    let arch = match key.image.arch {
        ImageArch::Amd64 => "amd64",
        ImageArch::Arm64 => "arm64",
    };
    let execution = match key.execution {
        Execution::Native => "native",
        Execution::ShardsShared => "shards-shared",
        Execution::ShardsDedicated => "shards-dedicated",
        Execution::Firecracker => "firecracker",
        Execution::NodeVm => "node-vm",
    };
    let gpu = match key.gpu {
        GpuEligibility::Any => "any",
        GpuEligibility::None => "none",
        GpuEligibility::Required => "required",
    };
    write!(
        out,
        "route {} {os} {arch} {execution} {gpu} ",
        key.class.name()
    )?;
    match receipt.backing {
        Some(backing) => {
            for byte in backing.pool {
                write!(out, "{byte:02x}")?;
            }
            write!(out, " ")?;
            for byte in backing.generation {
                write!(out, "{byte:02x}")?;
            }
            writeln!(out)
        }
        None => writeln!(out, "none none"),
    }
}

fn parse_route(text: &str) -> Result<Option<RouteReceipt>, Refusal> {
    if text == "none" {
        return Ok(None);
    }
    let mut fields = text.split(' ');
    let class = match fields.next() {
        Some("oci") => Class::Oci,
        Some("shards") => Class::Shards,
        _ => return Err(invalid("route names another workload class")),
    };
    let os = match fields.next() {
        Some("linux") => ImageOs::Linux,
        Some("windows") => ImageOs::Windows,
        Some("macos") => ImageOs::Macos,
        _ => return Err(invalid("route has an unknown image OS")),
    };
    let arch = match fields.next() {
        Some("amd64") => ImageArch::Amd64,
        Some("arm64") => ImageArch::Arm64,
        _ => return Err(invalid("route has an unknown image architecture")),
    };
    let execution = match fields.next() {
        Some("shards-dedicated") => Execution::ShardsDedicated,
        _ => return Err(invalid("route is not dedicated Shards execution")),
    };
    let gpu = match fields.next() {
        Some("any") => GpuEligibility::Any,
        Some("none") => GpuEligibility::None,
        Some("required") => GpuEligibility::Required,
        _ => return Err(invalid("route has an unknown GPU mode")),
    };
    let backing = match (fields.next(), fields.next()) {
        (Some("none"), Some("none")) => None,
        (Some(pool), Some(generation)) => Some(BackingKey {
            pool: parse_hex(pool)?,
            generation: parse_hex(generation)?,
        }),
        _ => return Err(invalid("route has an incomplete backing identity")),
    };
    if fields.next().is_some() {
        return Err(invalid("route has trailing fields"));
    }
    Ok(Some(RouteReceipt {
        key: RouteKey {
            class,
            image: ImagePlatform { os, arch },
            execution,
            gpu,
        },
        backing,
    }))
}

fn field<'a>(line: Option<&'a str>, name: &str) -> Result<&'a str, Refusal> {
    line.and_then(|line| line.strip_prefix(name))
        .ok_or_else(|| invalid("missing or reordered receipt field"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Record {
        Record {
            id: BoxId::new("selected-box").unwrap(),
            devices: vec![DeviceId {
                kind: DeviceKind::Gpu,
                key: 256,
            }],
            class: Class::Shards,
            scope: Some(NativeScope::from_bytes([7; 16])),
            route: None,
            physical: vec![shards_api::PhysicalGpu::Pci {
                domain: 0,
                bus: 1,
                device: 0,
                function: 0,
            }],
            report: None,
            readiness: None,
            attempt: None,
            vcpus: 1,
            memory_mib: 256,
            phase: Phase::Intent,
        }
    }

    fn observed_record() -> Record {
        use std::fmt::Write as _;
        let mut receipt = record();
        let scope = receipt.scope.unwrap();
        let physical = receipt.physical[0];
        receipt.report = Some(shards_api::physical::GpuReport {
            scope: *scope.bytes(),
            probe: shards_api::GpuProbe {
                devices: vec![shards_api::ResolvedGpu {
                    physical,
                    pci: Some(physical),
                    luid: None,
                    uuid: [3; 16],
                    name: None,
                    device_local_bytes: None,
                }],
                host_visible_bytes: 1,
            },
        });
        // Model data only: this unit checks receipt authority and gating, not
        // driver execution, immutable artifacts or native GPU/termination facts.
        let inventory = format!(
            "hex-vulkan-guest-inventory 1 1\ndevice 0 4202496 22 1 1 2 2 {} 677075 76656e7573 4d657361 2 1 1 2 1\nheap 0 0 1 4096\n",
            "03".repeat(16)
        );
        receipt.readiness = Some(super::super::observer::Evidence {
            scope,
            configuration: [9; 32],
            max_queues: 2,
            report: super::super::observer::Report::parse(inventory.as_bytes(), 1, 2).unwrap(),
        });
        let mut stdout = String::new();
        for byte in inventory.as_bytes() {
            write!(&mut stdout, "{byte:02x}").unwrap();
        }
        let mut attempt = super::super::observer::Attempt::parse(&format!(
            "{} 2 complete eof {} {} eof 1 -",
            "09".repeat(32),
            super::super::observer::output_bound(1).unwrap(),
            stdout,
        ))
        .unwrap()
        .unwrap();
        let frame =
            hex_core::wire::snapshot_exit(hex_api::Exit::Code(0), hex_core::wire::MAX_MESSAGE)
                .unwrap();
        let mut encoded = String::new();
        for byte in &frame {
            write!(&mut encoded, "{byte:02x}").unwrap();
        }
        attempt.parse_facts(&mut format!("observer-exit\t1\t{:016x}\t{}\t{encoded}\nobserver-failure\tnone\nobserver-cleanup\tdeleted\n", hex_core::wire::IDENTITY, frame.len()).lines()).unwrap();
        receipt.attempt = Some(attempt);
        receipt.phase = Phase::Running(shards_api::Identity {
            pid: 42,
            started: 7,
            vsock: "/model/v".into(),
        });
        receipt
    }

    /// AC-T7.4.1: a parent/legacy native report cannot certify the new selected
    /// guest. Only this exact scope, visible set, artifact identity and completed
    /// actual-observer protocol may gate payload execution.
    #[test]
    fn selected_observer_receipt_requires_its_own_scope_set_and_complete_outcome() {
        let mut receipt = observed_record();
        receipt.checked_readiness().unwrap();
        let text = receipt.encode().unwrap();
        assert!(text.starts_with("hex-selected-guest 5\n"));
        assert_eq!(parse(&text).unwrap(), receipt);
        receipt.readiness.as_mut().unwrap().scope = NativeScope::from_bytes([8; 16]);
        assert!(receipt.checked_readiness().is_err());
        assert!(receipt.encode().is_err());
        let mut receipt = parse(&text).unwrap();
        receipt.attempt.as_mut().unwrap().phase = super::super::observer::AttemptPhase::Waiting;
        assert!(receipt.checked_readiness().is_err());
        receipt.attempt.as_mut().unwrap().phase = super::super::observer::AttemptPhase::Complete;
        receipt.readiness.as_mut().unwrap().configuration = [10; 32];
        assert!(receipt.checked_readiness().is_err());
        let mut receipt = parse(&text).unwrap();
        receipt.devices.push(DeviceId {
            kind: DeviceKind::Gpu,
            key: 512,
        });
        receipt.physical.push(shards_api::PhysicalGpu::Pci {
            domain: 0,
            bus: 2,
            device: 0,
            function: 0,
        });
        assert!(receipt.checked_readiness().is_err());
        let mut legacy = parse(&text).unwrap();
        legacy.readiness = None;
        legacy.attempt = None;
        let v4 = legacy.encode().unwrap();
        assert!(v4.starts_with("hex-selected-guest 4\n"));
        assert_eq!(parse(&v4).unwrap().encode().unwrap(), v4);
        assert!(legacy.checked_readiness().is_err());
        legacy.phase = Phase::Released {
            identity: None,
            status: shards_api::ManagedStatus::Unknown,
        };
        assert_eq!(parse(&legacy.encode().unwrap()).unwrap(), legacy);
    }

    /// AC-T7.4.1: inventory completion stays cleanup-readable but cannot authorize
    /// the new queue/fence work-ready contract, even with successful exit/EOF.
    #[test]
    fn selected_inventory_receipt_cannot_authorize_work_ready_payload() {
        let receipt = observed_record();
        receipt.checked_readiness().unwrap();
        assert!(receipt.checked_work_ready().is_err());
        let retained = parse(&receipt.encode().unwrap()).unwrap();
        assert_eq!(retained, receipt);
        assert!(retained.checked_work_ready().is_err());
    }

    /// AC-T7.4.1: malformed/reordered/foreign diagnostics never become usable
    /// readiness, yet an opaque wire fact remains readable for SAME scope cleanup.
    #[test]
    fn selected_observer_opaque_facts_remain_cleanup_only_and_cpu_v4_unchanged() {
        let receipt = observed_record();
        let text = receipt.encode().unwrap();
        for bad in [
            text.replacen("readiness ", "readiness 00", 1),
            text.replacen("observer-exit\t", "observer-cleanup\t", 1),
            text.replacen("observer-cleanup\tdeleted", "observer-cleanup\tunknown", 1),
        ] {
            assert!(parse(&bad).is_err());
        }
        let foreign = text.replacen("observer-exit\t1\t", "observer-exit\t2\t", 1);
        let mut retained = parse(&foreign).unwrap();
        assert!(retained.checked_readiness().is_err());
        retained.phase = Phase::Released {
            identity: None,
            status: shards_api::ManagedStatus::Unknown,
        };
        assert_eq!(parse(&retained.encode().unwrap()).unwrap(), retained);
        let mut cpu = record();
        cpu.devices.clear();
        cpu.physical.clear();
        let v4 = cpu.encode().unwrap();
        assert!(v4.starts_with("hex-selected-guest 4\n"));
        assert_eq!(parse(&v4).unwrap().encode().unwrap(), v4);
        assert!(cpu.attempt.is_none() && cpu.readiness.is_none());
    }

    /// AC-T7.4.1: every lifecycle phase retains the same exact claims and VM identity.
    #[test]
    fn durable_phases_preserve_exact_claims() {
        let identity = shards_api::Identity {
            pid: 42,
            started: 7,
            vsock: "/private/selected.sock".into(),
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipt");
        for phase in [
            Phase::Intent,
            Phase::Running(identity.clone()),
            Phase::Stopping(identity.clone()),
            Phase::Released {
                identity: Some(identity),
                status: shards_api::ManagedStatus::Signal(9),
            },
            Phase::Released {
                identity: None,
                status: shards_api::ManagedStatus::Unknown,
            },
        ] {
            let record = Record { phase, ..record() };
            write(&path, &record).unwrap();
            assert_eq!(read(&path).unwrap(), Some(record));
        }
        remove(&path).unwrap();
        assert_eq!(read(&path).unwrap(), None);
        assert!(record().identity().is_err());
    }

    /// AC-T7.4.1: torn/unknown/duplicate receipts cannot authorize a replacement VM.
    #[test]
    fn hostile_receipts_never_become_absence() {
        let text = record().encode().unwrap();
        for end in 0..text.len() {
            assert!(parse(text.get(..end).unwrap()).is_err());
        }
        for bad in [
            text.replace("devices 1\ngpu 256", "devices 2\ngpu 256\ngpu 256"),
            text.replace("devices 1", "devices 65537"),
            text.replace("intent", "running\npid 0\nstarted 7\nvsock /a"),
            format!("{text}trailing\n"),
        ] {
            assert!(parse(&bad).is_err());
        }
    }

    /// AC-T7.4.1: the exact native frame survives durable receipt round-trip,
    /// bound to the original scope and selectors without a new holder ledger.
    #[test]
    fn private_renderer_report_round_trip_retains_original_scope_and_native_set() {
        let mut receipt = record();
        let physical = receipt.physical[0];
        receipt.report = Some(shards_api::physical::GpuReport {
            scope: *receipt.scope.unwrap().bytes(),
            probe: shards_api::GpuProbe {
                devices: vec![shards_api::ResolvedGpu {
                    physical,
                    pci: Some(physical),
                    luid: None,
                    uuid: [3; 16],
                    name: None,
                    device_local_bytes: None,
                }],
                host_visible_bytes: 1,
            },
        });
        let text = receipt.encode().unwrap();
        let retained = parse(&text).unwrap();
        assert_eq!(retained, receipt);
        assert!(retained.checked_report().unwrap().is_some());
        receipt.scope = Some(NativeScope::from_bytes([8; 16]));
        assert!(receipt.checked_report().is_err());
        assert!(receipt.encode().is_err());
        assert!(parse(&text.replace("report ", "report +")).is_err());
    }

    /// An old transport receipt remains readable for cleanup. Its missing
    /// private observation cannot authorize Live/adoption or a replacement query.
    #[test]
    fn legacy_gpu_receipt_keeps_claims_without_manufacturing_private_report() {
        let receipt = record();
        let current = receipt.encode().unwrap();
        // Without an observer the receipt is written as v4, the version rewritten here.
        assert!(current.starts_with(REPORT_HEADER), "{current}");
        let text = current
            .replace(REPORT_HEADER, ROUTE_HEADER)
            .replace("report none\n", "");
        let retained = parse(&text).unwrap();
        assert_eq!(retained.devices, receipt.devices);
        assert_eq!(retained.scope, receipt.scope);
        assert!(retained.report.is_none());
        assert!(retained.checked_report().is_err());
    }

    /// AC-T7.4.1: legacy transport receipts decode without inventing a native
    /// incarnation; malformed or transplanted scope bytes supply no authority.
    #[test]
    fn legacy_and_hostile_scope_receipts_cannot_authorize_a_new_incarnation() {
        let text = record().encode().unwrap();
        // Without an observer the receipt is written as v4, the version rewritten here.
        assert!(text.starts_with(REPORT_HEADER), "{text}");
        let legacy = text
            .replace(REPORT_HEADER, LEGACY_HEADER)
            .replace("scope 07070707070707070707070707070707\n", "")
            .replace("route none\n", "")
            .replace("report none\n", "");
        assert_eq!(parse(&legacy).unwrap().scope, None);
        let previous = text
            .replace(REPORT_HEADER, SCOPE_HEADER)
            .replace("route none\n", "")
            .replace("report none\n", "");
        assert_eq!(parse(&previous).unwrap().route, None);
        for scope in [
            "0707070707070707070707070707070+07",
            "070707070707070707070707070707070",
            "070707070707070707070707070707070707",
            "07070707070707070707070707070707gg",
        ] {
            assert!(parse(&text.replace("07070707070707070707070707070707", scope)).is_err());
        }
        let mut receipt = record();
        receipt.scope = None;
        assert_eq!(parse(&receipt.encode().unwrap()).unwrap().scope, None);
    }

    /// AC-T7.4.1: the host route and backing incarnation survive transport
    /// recovery exactly; legacy receipts cannot silently inherit today's route.
    #[test]
    fn selected_route_and_backing_are_durable_and_not_upgraded_from_legacy() {
        let mut receipt = record();
        receipt.route = Some(RouteReceipt {
            key: RouteKey {
                class: Class::Shards,
                image: ImagePlatform {
                    os: ImageOs::Linux,
                    arch: ImageArch::Arm64,
                },
                execution: Execution::ShardsDedicated,
                gpu: GpuEligibility::Required,
            },
            backing: Some(BackingKey {
                pool: [3; 32],
                generation: [4; 16],
            }),
        });
        let encoded = receipt.encode().unwrap();
        assert_eq!(parse(&encoded).unwrap(), receipt);
        for bad in [
            encoded.replace("shards-dedicated", "shards-shared"),
            encoded.replace("linux arm64", "windows arm64"),
            encoded.replace("required", "none"),
            encoded.replace(&"03".repeat(32), &format!("+{}", "3".repeat(63))),
            encoded.replace(&"04".repeat(16), &"04".repeat(15)),
        ] {
            assert!(parse(&bad).is_err());
        }
        let mut legacy = parse(&receipt.encode().unwrap()).unwrap();
        legacy.route = None;
        assert_ne!(legacy, receipt, "legacy has no host execution provenance");
    }
}
