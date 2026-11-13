//! VM process receipt for the canonical Store's real pool. It grants no resources
//! or holders; every use rechecks the original native commitment in that Store.

use std::io::Read as _;
use std::path::Path;

use hex_api::{BackingKey, BackingPhase, Execution, LifecycleCause, Refusal};
use hex_core::record::{MAX_RECORD_BYTES, PoolRecord};

use super::selected::{self, Phase};
use super::{GuestConfig, held};

const HEADER: &str = "hex-guest-backing 5";
const ATTEMPT_HEADER: &str = "hex-guest-backing 4";
const READINESS_HEADER: &str = "hex-guest-backing 3";
const REPORT_HEADER: &str = "hex-guest-backing 2";
const LEGACY_HEADER: &str = "hex-guest-backing 1";

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Receipt {
    /// An immutable Granted snapshot before native launch. The canonical Store
    /// remains the lifecycle/holder authority as its init and phase advance.
    pub original: PoolRecord,
    pub physical: Vec<shards_api::PhysicalGpu>,
    pub report: Option<shards_api::physical::GpuReport>,
    pub readiness: Option<super::observer::Evidence>,
    pub attempt: Option<super::observer::Attempt>,
    pub phase: Phase,
}

pub(super) fn refused(key: BackingKey, cause: LifecycleCause) -> Refusal {
    Refusal::Backing { key, cause }
}

fn exhausted() -> Refusal {
    Refusal::Exhausted {
        what: "guest backing process receipt",
        bound: MAX_RECORD_BYTES,
    }
}

impl Receipt {
    pub fn intent(
        original: PoolRecord,
        physical: Vec<shards_api::PhysicalGpu>,
        config: &GuestConfig,
    ) -> Result<Self, Refusal> {
        let receipt = Self::retained(original, physical)?;
        let guest = receipt
            .original
            .allocation
            .ok_or_else(|| refused(receipt.original.key, LifecycleCause::Uncertain))?
            .guest()?;
        let configured = selected::resources(config)?;
        if guest.cpu_milli != configured.cpu_milli || guest.mem_bytes != configured.mem_bytes {
            return Err(refused(receipt.original.key, LifecycleCause::Uncertain));
        }
        Ok(receipt)
    }

    pub fn retained(
        original: PoolRecord,
        physical: Vec<shards_api::PhysicalGpu>,
    ) -> Result<Self, Refusal> {
        let receipt = Self {
            original,
            physical,
            report: None,
            readiness: None,
            attempt: None,
            phase: Phase::Intent,
        };
        receipt.check()?;
        Ok(receipt)
    }

    pub fn check(&self) -> Result<(), Refusal> {
        let bad = || refused(self.original.key, LifecycleCause::Uncertain);
        self.original.check()?;
        if self.original.phase != BackingPhase::Granted
            || self.original.init.is_some()
            || self.original.exit.is_some()
            || self.original.allocation.is_none()
            || !matches!(
                self.original.execution,
                Execution::ShardsShared | Execution::ShardsDedicated
            )
            || self.physical.len() != self.original.devices.len()
            || self
                .original
                .devices
                .iter()
                .any(|id| id.kind != hex_api::DeviceKind::Gpu)
            || (self.original.execution == Execution::ShardsShared && !self.physical.is_empty())
        {
            return Err(bad());
        }
        shards_api::physical::check_selection(&self.physical).map_err(|_| bad())?;
        if let Some(report) = &self.report {
            super::physical::check_report(
                &self.original.devices,
                &self.physical,
                self.original.scope(),
                report,
            )?;
        }
        if let Some(readiness) = &self.readiness {
            if self.physical.is_empty() || self.report.is_none() {
                return Err(bad());
            }
            readiness.check(self.original.scope(), self.physical.len())?;
        }
        if let Some(attempt) = &self.attempt
            && (self.physical.is_empty()
                || attempt.configuration == [0; 32]
                || attempt.max_queues == 0)
        {
            return Err(bad());
        }
        Ok(())
    }

    pub fn checked_report(&self) -> Result<Option<&shards_api::physical::GpuReport>, Refusal> {
        self.check()?;
        if self.physical.is_empty() {
            return Ok(None);
        }
        self.report
            .as_ref()
            .map(Some)
            .ok_or_else(|| refused(self.original.key, LifecycleCause::Uncertain))
    }

    pub fn identity(&self) -> Result<&shards_api::Identity, Refusal> {
        match &self.phase {
            Phase::Running(identity)
            | Phase::Stopping(identity)
            | Phase::Released {
                identity: Some(identity),
                ..
            } => Ok(identity),
            Phase::Intent | Phase::Released { identity: None, .. } => {
                Err(refused(self.original.key, LifecycleCause::Uncertain))
            }
        }
    }

    pub fn released_identity(&self) -> Result<Option<&shards_api::Identity>, Refusal> {
        match &self.phase {
            Phase::Released { identity, .. } => Ok(identity.as_ref()),
            _ => Err(refused(self.original.key, LifecycleCause::Uncertain)),
        }
    }

    pub fn checked_readiness(&self) -> Result<&super::observer::Evidence, Refusal> {
        let readiness = self.readiness_payload()?;
        if !self
            .attempt
            .as_ref()
            .ok_or_else(|| refused(self.original.key, LifecycleCause::Uncertain))?
            .ready_facts()?
        {
            return Err(refused(self.original.key, LifecycleCause::Uncertain));
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
            .map_err(|_| refused(self.original.key, LifecycleCause::Uncertain))?;
        Ok(readiness)
    }

    fn readiness_payload(&self) -> Result<&super::observer::Evidence, Refusal> {
        self.check()?;
        if !matches!(self.phase, Phase::Running(_)) {
            return Err(refused(self.original.key, LifecycleCause::Uncertain));
        }
        let readiness = self
            .readiness
            .as_ref()
            .ok_or_else(|| refused(self.original.key, LifecycleCause::Uncertain))?;
        let attempt = self
            .attempt
            .as_ref()
            .ok_or_else(|| refused(self.original.key, LifecycleCause::Uncertain))?;
        if !attempt.matches_readiness(readiness)? {
            return Err(refused(self.original.key, LifecycleCause::Uncertain));
        }
        Ok(readiness)
    }

    pub fn matches(&self, record: &PoolRecord) -> Result<(), Refusal> {
        self.check()?;
        record.check()?;
        if self.original.key != record.key
            || self.original.native_commitment()? != record.native_commitment()?
        {
            return Err(refused(record.key, LifecycleCause::Uncertain));
        }
        if record.phase == BackingPhase::Live && !self.physical.is_empty() {
            // Preserve v4 provenance/payload checks. A v5 opaque diagnostic is
            // not usable readiness, but cannot prevent the same scope's cleanup.
            self.readiness_payload()?;
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<String, Refusal> {
        self.check()?;
        // Reuse the exact canonical scalar/array codec rather than another cost
        // formula or grant schema. Its decoder's existing serde allocation seam
        // remains explicit in the source/evidence record.
        let report = self
            .report
            .as_ref()
            .map(|report| {
                report
                    .encode()
                    .map_err(|_| refused(self.original.key, LifecycleCause::Uncertain))
            })
            .transpose()?;
        let original = self.original.encode()?;
        let original = std::str::from_utf8(&original)
            .map_err(|_| refused(self.original.key, LifecycleCause::Uncertain))?;
        struct Length(usize);
        impl std::fmt::Write for Length {
            fn write_str(&mut self, text: &str) -> std::fmt::Result {
                self.0 = self
                    .0
                    .checked_add(text.len())
                    .filter(|count| *count <= MAX_RECORD_BYTES)
                    .ok_or(std::fmt::Error)?;
                Ok(())
            }
        }
        let mut length = Length(0);
        self.render(&mut length, original, report.as_deref())
            .map_err(|_| exhausted())?;
        let mut text = String::new();
        text.try_reserve_exact(length.0).map_err(|_| exhausted())?;
        self.render(&mut text, original, report.as_deref())
            .map_err(|_| exhausted())?;
        if parse(&text)? != *self {
            return Err(refused(self.original.key, LifecycleCause::Uncertain));
        }
        Ok(text)
    }

    fn render(
        &self,
        out: &mut impl std::fmt::Write,
        original: &str,
        report: Option<&str>,
    ) -> std::fmt::Result {
        let facts = self.attempt.as_ref().filter(|attempt| attempt.has_facts());
        let header = if facts.is_some() {
            HEADER
        } else {
            ATTEMPT_HEADER
        };
        writeln!(out, "{header}")?;
        write!(out, "original {original}")?;
        writeln!(out, "physical {}", self.physical.len())?;
        for physical in &self.physical {
            writeln!(out, "native {physical}")?;
        }
        selected::render_report(out, report)?;
        write!(out, "readiness ")?;
        match &self.readiness {
            Some(readiness) => readiness.render(out)?,
            None => write!(out, "none")?,
        }
        writeln!(out)?;
        write!(out, "observer ")?;
        match &self.attempt {
            Some(attempt) => attempt.render(out)?,
            None => write!(out, "none")?,
        }
        writeln!(out)?;
        if let Some(attempt) = facts {
            attempt.render_facts(out)?;
        }
        selected::render_phase(out, &self.phase)
    }
}

pub(super) fn write(path: &Path, receipt: &Receipt) -> Result<(), Refusal> {
    held::replace(path, receipt.encode()?.as_bytes())
}

pub(super) fn read(path: &Path) -> Result<Option<Receipt>, Refusal> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Refusal::os("opening guest backing receipt", error)),
    };
    let room = MAX_RECORD_BYTES.checked_add(1).ok_or_else(exhausted)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(room).map_err(|_| exhausted())?;
    bytes.resize(room, 0);
    let mut count = 0usize;
    let mut file = file.take(u64::try_from(room).map_err(|_| exhausted())?);
    while count < room {
        let tail = bytes.get_mut(count..).ok_or_else(exhausted)?;
        let read = file
            .read(tail)
            .map_err(|why| Refusal::os("reading guest backing receipt", why))?;
        if read == 0 {
            break;
        }
        count = count.checked_add(read).ok_or_else(exhausted)?;
    }
    if count > MAX_RECORD_BYTES {
        return Err(exhausted());
    }
    parse(
        std::str::from_utf8(bytes.get(..count).ok_or_else(exhausted)?)
            .map_err(|_| super::route::invalid("guest backing receipt is not UTF-8"))?,
    )
    .map(Some)
}

fn parse(text: &str) -> Result<Receipt, Refusal> {
    if text.len() > MAX_RECORD_BYTES || !text.ends_with('\n') {
        return Err(exhausted());
    }
    let (header, original) = text
        .split_once('\n')
        .ok_or_else(|| super::route::invalid("guest backing receipt is truncated"))?;
    if header != HEADER
        && header != ATTEMPT_HEADER
        && header != READINESS_HEADER
        && header != REPORT_HEADER
        && header != LEGACY_HEADER
    {
        return Err(super::route::invalid(
            "guest backing receipt has no canonical pool provenance",
        ));
    }
    let original = original
        .strip_prefix("original ")
        .ok_or_else(|| super::route::invalid("guest backing receipt lacks its grant"))?;
    let end = original
        .find('\n')
        .and_then(|end| end.checked_add(1))
        .ok_or_else(exhausted)?;
    let snapshot = PoolRecord::decode(original.get(..end).ok_or_else(exhausted)?.as_bytes())?;
    let mut lines = original.get(end..).ok_or_else(exhausted)?.lines();
    let count = lines
        .next()
        .and_then(|line| line.strip_prefix("physical "))
        .ok_or_else(|| super::route::invalid("guest backing receipt lacks exact physical count"))?;
    if count.is_empty() || !count.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(super::route::invalid(
            "guest backing physical count is not canonical",
        ));
    }
    let count = count.parse::<usize>().map_err(|_| exhausted())?;
    if count != snapshot.devices.len() || count > hex_api::MAX_DEVICES {
        return Err(super::route::invalid(
            "guest backing physical claims changed",
        ));
    }
    let mut physical = Vec::new();
    physical.try_reserve_exact(count).map_err(|_| exhausted())?;
    for _ in 0..count {
        let native = lines
            .next()
            .and_then(|line| line.strip_prefix("native "))
            .ok_or_else(|| super::route::invalid("guest backing physical selector is missing"))?;
        physical.push(
            native
                .parse()
                .map_err(|_| super::route::invalid("guest backing physical selector is invalid"))?,
        );
    }
    let report = if header != LEGACY_HEADER {
        let frame = lines
            .next()
            .and_then(|line| line.strip_prefix("report "))
            .ok_or_else(|| super::route::invalid("guest backing report is missing"))?;
        selected::parse_report(frame, Some(snapshot.scope()), &physical)?
    } else {
        None
    };
    let readiness = if header == HEADER || header == ATTEMPT_HEADER || header == READINESS_HEADER {
        let frame = lines
            .next()
            .and_then(|line| line.strip_prefix("readiness "))
            .ok_or_else(|| super::route::invalid("guest backing readiness is missing"))?;
        super::observer::Evidence::parse(frame, snapshot.scope(), physical.len())?
    } else {
        // Legacy report-only receipts are readable for cleanup, never promoted
        // to guest readiness or an initialized Live physical offer.
        None
    };
    let mut attempt = if header == HEADER || header == ATTEMPT_HEADER {
        let line = lines
            .next()
            .and_then(|line| line.strip_prefix("observer "))
            .ok_or_else(|| super::route::invalid("guest backing observer attempt is missing"))?;
        super::observer::Attempt::parse(line)?
    } else {
        None
    };
    if header == HEADER {
        attempt
            .as_mut()
            .ok_or_else(|| super::route::invalid("v5 receipt lacks its observer attempt"))?
            .parse_facts(&mut lines)?;
    }
    let phase = selected::parse_phase(&mut lines)?;
    if lines.next().is_some() {
        return Err(super::route::invalid(
            "guest backing receipt has trailing data",
        ));
    }
    let receipt = Receipt {
        original: snapshot,
        physical,
        report,
        readiness,
        attempt,
        phase,
    };
    receipt.check()?;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-T7.4.1: a real Pool scope owns the same private renderer frame; its
    /// canonical claim/grant survives the codec without a synthetic box row.
    #[test]
    fn pool_private_report_keeps_original_scope_and_exact_native_claim() {
        let mut record = granted();
        record.execution = Execution::ShardsDedicated;
        record.devices = vec![hex_api::DeviceId {
            kind: hex_api::DeviceKind::Gpu,
            key: 256,
        }];
        let physical = shards_api::PhysicalGpu::Pci {
            domain: 0,
            bus: 1,
            device: 0,
            function: 0,
        };
        let mut receipt =
            Receipt::intent(record, vec![physical], &super::super::tests::config()).unwrap();
        receipt.report = Some(shards_api::physical::GpuReport {
            scope: *receipt.original.scope().bytes(),
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
        assert_eq!(parse(&text).unwrap(), receipt);
        assert!(receipt.checked_report().unwrap().is_some());
        receipt.report.as_mut().unwrap().scope = [8; 16];
        assert!(receipt.encode().is_err());
        assert!(parse(&text.replace("report ", "report +")).is_err());
    }

    fn granted() -> PoolRecord {
        let config = super::super::tests::config();
        let resources = selected::resources(&config).unwrap();
        let cost = hex_api::RuntimeCost {
            id: [1; 32],
            floor: resources,
            ceiling: resources,
            quantum: hex_api::CapacityResources {
                cpu_milli: 1000,
                mem_bytes: 1 << 20,
                disk_bytes: 1,
            },
            guest_overhead: hex_api::CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 0,
            },
            overhead: hex_api::CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 0,
            },
        };
        PoolRecord {
            version: hex_core::record::RECORD_VERSION,
            key: BackingKey {
                pool: [2; 32],
                generation: [3; 16],
            },
            execution: Execution::ShardsShared,
            cost,
            allocation: Some(
                cost.allocate(hex_api::CapacityResources::default())
                    .unwrap(),
            ),
            devices: Vec::new(),
            phase: BackingPhase::Granted,
            init: None,
            exit: None,
        }
    }

    /// AC-T7.4.1: this VM receipt retains the SAME grant/scope across every phase;
    /// canonical init/lifecycle publication cannot change its commitment.
    #[test]
    fn backing_process_receipt_retains_exact_grant_and_rejects_transplants() {
        let mut receipt =
            Receipt::intent(granted(), Vec::new(), &super::super::tests::config()).unwrap();
        let mut canonical = granted();
        canonical.init = Some(hex_core::record::ProcId {
            pid: 42,
            start_ticks: 7,
        });
        canonical.phase = BackingPhase::Live;
        receipt.matches(&canonical).unwrap();
        for phase in [
            Phase::Intent,
            Phase::Running(shards_api::Identity {
                pid: 42,
                started: 7,
                vsock: "/pool/socket".into(),
            }),
            Phase::Released {
                identity: None,
                status: shards_api::ManagedStatus::Unknown,
            },
        ] {
            receipt.phase = phase;
            assert_eq!(parse(&receipt.encode().unwrap()).unwrap(), receipt);
        }
        canonical.key.generation = [4; 16];
        assert!(receipt.matches(&canonical).is_err());
        assert!(parse("hex-guest 1\npid 42\nstarted 7\nvsock /pool/socket\n").is_err());
    }

    /// AC-T7.4.1: no truncated, excessive, substituted physical grant or legacy
    /// pid/socket receipt can silently initialize another native backing.
    #[test]
    fn backing_process_receipt_refuses_torn_or_forged_provenance() {
        let receipt =
            Receipt::intent(granted(), Vec::new(), &super::super::tests::config()).unwrap();
        let text = receipt.encode().unwrap();
        for end in 0..text.len() {
            assert!(parse(text.get(..end).unwrap()).is_err());
        }
        for bad in [
            text.replace("physical 0", "physical +0"),
            text.replace("physical 0", "physical 65537"),
            text.replace("\"granted\"", "\"live\""),
            format!("{text}extra\n"),
        ] {
            assert!(parse(&bad).is_err());
        }
    }

    /// AC-T7.4.1: v4 remains byte compatible and cleanup-readable. New v5
    /// diagnostics retain the same native grant and cannot initialize readiness.
    #[test]
    fn observer_v5_diagnostics_preserve_legacy_v4_and_same_grant_cleanup() {
        let mut record = granted();
        record.execution = Execution::ShardsDedicated;
        record.devices = vec![hex_api::DeviceId {
            kind: hex_api::DeviceKind::Gpu,
            key: 256,
        }];
        let physical = shards_api::PhysicalGpu::Pci {
            domain: 0,
            bus: 1,
            device: 0,
            function: 0,
        };
        let mut receipt =
            Receipt::intent(record, vec![physical], &super::super::tests::config()).unwrap();
        let mut attempt = super::super::observer::Attempt::parse(&format!(
            "{} 2 waiting eof 2 - eof 2 -",
            "09".repeat(32)
        ))
        .unwrap()
        .unwrap();
        receipt.attempt = Some(attempt);
        let v4 = receipt.encode().unwrap();
        assert!(v4.starts_with("hex-guest-backing 4\n"));
        assert_eq!(parse(&v4).unwrap().encode().unwrap(), v4);
        attempt = receipt.attempt.take().unwrap();
        attempt.parse_facts(&mut "observer-exit\t2\t0000000000000001\t1\t00\nobserver-failure\twaiting\t1\t0000000000000001\t1\t00\nobserver-cleanup\tfailed\t1\t0000000000000001\t1\t00\n".lines()).unwrap();
        receipt.attempt = Some(attempt);
        let v5 = receipt.encode().unwrap();
        assert!(v5.starts_with("hex-guest-backing 5\n"));
        let mut retained = parse(&v5).unwrap();
        assert_eq!(retained, receipt);
        assert!(retained.checked_readiness().is_err());
        retained.phase = Phase::Released {
            identity: None,
            status: shards_api::ManagedStatus::Unknown,
        };
        let released = retained.encode().unwrap();
        assert_eq!(parse(&released).unwrap(), retained);
        assert_eq!(retained.original, receipt.original);
        assert!(parse(&v5.replace("observer-exit\t", "observer-cleanup\t")).is_err());
    }
}
