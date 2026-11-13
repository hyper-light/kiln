//! Physical identities carried from a runtime's catalog to a box's exact claims (AC-T7.3.1).
//! These values describe and request devices; they confer no attachment authority by themselves.

use serde::{Deserialize, Deserializer, Serialize, de};

use crate::{BoxId, Refusal};

/// One entry for every `u16` device index, as in the scheduler's existing device address space.
pub const MAX_DEVICES: usize = 1 << u16::BITS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    Gpu,
    /// An IOMMU isolation group, not an independently assignable PCI function.
    Vfio,
    Block,
    Net,
}

impl DeviceKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Gpu => "gpu",
            Self::Vfio => "vfio",
            Self::Block => "block",
            Self::Net => "net",
        }
    }
}

impl<'de> Deserialize<'de> for DeviceKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Kind;
        impl de::Visitor<'_> for Kind {
            type Value = DeviceKind;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a lowercase device kind")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<DeviceKind, E> {
                match value {
                    "gpu" => Ok(DeviceKind::Gpu),
                    "vfio" => Ok(DeviceKind::Vfio),
                    "block" => Ok(DeviceKind::Block),
                    "net" => Ok(DeviceKind::Net),
                    _ => Err(E::custom("device.kind is unsupported")),
                }
            }
        }
        deserializer.deserialize_str(Kind)
    }
}

/// A physical identity within its runtime pool. A GPU key is the native unsigned 64-bit
/// identity: packed PCI domain:32/bus:8/device:5/function:3 on PCI hosts, Metal registry ID on
/// Apple silicon. Its runtime validates that native correspondence without narrowing. VFIO
/// keys are `u32` IOMMU group numbers; block keys pack
/// major:32/minor:32; network keys are nonzero `u32` interface indices. Pool identity remains
/// necessary: a key from one node or guest never names another pool's device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceId {
    pub kind: DeviceKind,
    pub key: u64,
}

impl DeviceId {
    pub fn check(&self) -> Result<(), Refusal> {
        let why = match self.kind {
            DeviceKind::Vfio if u32::try_from(self.key).is_err() => {
                Some("vfio key exceeds the 32-bit isolation group")
            }
            DeviceKind::Net if self.key == 0 || u32::try_from(self.key).is_err() => {
                Some("net key is not a nonzero 32-bit interface index")
            }
            _ => None,
        };
        match why {
            Some(why) => Err(Refusal::Invalid {
                what: "device id",
                why: why.to_owned(),
            }),
            None => Ok(()),
        }
    }
}

/// The actual canonical owner; a pool is never disguised as a box or reported as free.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DeviceHolder {
    Box(BoxId),
    Backing(crate::runtime::BackingKey),
}

impl DeviceHolder {
    pub fn try_clone(&self) -> Result<Self, Refusal> {
        Ok(match self {
            Self::Box(id) => Self::Box(id.try_clone()?),
            Self::Backing(key) => Self::Backing(*key),
        })
    }

    pub fn box_id(&self) -> Option<&BoxId> {
        match self {
            Self::Box(id) => Some(id),
            Self::Backing(_) => None,
        }
    }

    pub fn backing_key(&self) -> Option<crate::runtime::BackingKey> {
        match self {
            Self::Backing(key) => Some(*key),
            Self::Box(_) => None,
        }
    }
}

/// A catalog fact: opaque model metadata, size in bytes where applicable, and its current holder.
/// Neither free metadata nor a reported holder substitutes for runtime admission or attachment.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub model: String,
    pub size: u64,
    pub holder: Option<DeviceHolder>,
}

/// A factual native snapshot around the existing catalog metadata. Each None counter remains
/// unknown; Some(0) is an actual known zero. This grants no holder or attachment authority.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalDevice {
    pub info: DeviceInfo,
    pub busy_per_mille: Option<u16>,
    pub memory_used_bytes: Option<u64>,
}

/// A qualified unavailable VM observation; it neither replaces native facts nor grants holders.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendUnavailable {
    pub backend: crate::Class,
    #[serde(with = "unavailable_cause")]
    pub cause: Refusal,
}

/// This diagnostic has only fixed-size backing identity and the existing three typed causes.
/// General Refusal contains static names/text and deliberately has no serde implementation.
mod unavailable_cause {
    use super::*;

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Cause {
        key: crate::BackingKey,
        cause: crate::LifecycleCause,
    }

    pub(super) fn serialize<S: serde::Serializer>(
        value: &Refusal,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        if !BackendUnavailable::qualified(crate::Class::Firecracker, value) {
            return Err(serde::ser::Error::custom(
                "only qualified backing observation causes may be encoded",
            ));
        }
        let Refusal::Backing { key, cause } = value else {
            return Err(serde::ser::Error::custom(
                "unavailable observation has no backing identity",
            ));
        };
        Cause {
            key: *key,
            cause: *cause,
        }
        .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Refusal, D::Error> {
        let value = Cause::deserialize(deserializer)?;
        let cause = Refusal::Backing {
            key: value.key,
            cause: value.cause,
        };
        if !BackendUnavailable::qualified(crate::Class::Firecracker, &cause) {
            return Err(de::Error::custom(
                "only Pending, Busy or Uncertain backing observations may be decoded",
            ));
        }
        Ok(cause)
    }
}

impl BackendUnavailable {
    /// There are exactly two VM observer classes in the existing public Class enum.
    pub const MAX_COUNT: usize = [crate::Class::Firecracker, crate::Class::Shards].len();

    pub fn from_refusal(backend: crate::Class, cause: Refusal) -> Result<Self, Refusal> {
        if !Self::qualified(backend, &cause) {
            return Err(cause);
        }
        Ok(Self { backend, cause })
    }

    fn qualified(backend: crate::Class, cause: &Refusal) -> bool {
        matches!(backend, crate::Class::Firecracker | crate::Class::Shards)
            && matches!(
                cause,
                Refusal::Backing {
                    cause: crate::LifecycleCause::Pending
                        | crate::LifecycleCause::Busy
                        | crate::LifecycleCause::Uncertain,
                    ..
                }
            )
    }

    pub fn check(&self) -> Result<(), Refusal> {
        if !Self::qualified(self.backend, &self.cause) {
            return Err(invalid_report(
                "only qualified VM backing observations may be unavailable",
            ));
        }
        Ok(())
    }

    /// Fresh permission is scoped to the actual source key. A current canonical cold envelope
    /// can be provisioned before readings exist; workload admission still requires Live and
    /// observed eligibility. Old retained metadata must not be passed as a current backing here.
    pub fn blocks_route(
        &self,
        offer: &crate::runtime::RouteOffer,
        backing: Option<&crate::runtime::BackingOffer>,
    ) -> Result<bool, Refusal> {
        self.check()?;
        offer.check()?;
        if let Some(backing) = backing {
            offer.check_backing(backing)?;
        }
        let Refusal::Backing { key, cause } = &self.cause else {
            return Err(invalid_report("unavailable route has no backing identity"));
        };
        if self.backend != offer.key.backend_class() || offer.backing != Some(*key) {
            return Ok(false);
        }
        Ok(!(*cause == crate::LifecycleCause::Pending
            && backing.is_some_and(|backing| {
                backing.key == *key
                    && matches!(
                        backing.phase,
                        crate::runtime::BackingPhase::Prepared
                            | crate::runtime::BackingPhase::Granted
                    )
            })))
    }

    pub fn try_clone(&self) -> Result<Self, Refusal> {
        self.check()?;
        let Refusal::Backing { key, cause } = &self.cause else {
            return Err(invalid_report(
                "unavailable observation has no backing identity",
            ));
        };
        Ok(Self {
            backend: self.backend,
            cause: Refusal::Backing {
                key: *key,
                cause: *cause,
            },
        })
    }
}

fn invalid_report(why: &'static str) -> Refusal {
    let mut text = String::new();
    if text.try_reserve_exact(why.len()).is_err() {
        return Refusal::Exhausted {
            what: "native physical report refusal bytes",
            bound: why.len(),
        };
    }
    text.push_str(why);
    Refusal::Invalid {
        what: "native physical report",
        why: text,
    }
}

/// A flat snapshot from the native Store/turn owner's one provider. Overlapping backend routes
/// must name this same stable authority; telemetry does not report or grant a holder.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalReport {
    pub authority: String,
    pub devices: Vec<PhysicalDevice>,
    /// Explicit qualified observer failures beside actual successful native data, never an
    /// empty-success replacement for that provider or a physical termination receipt.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "PhysicalReport::deserialize_unavailable"
    )]
    pub unavailable: Vec<BackendUnavailable>,
}

impl PhysicalReport {
    fn deserialize_unavailable<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<BackendUnavailable>, D::Error> {
        crate::runtime::bounded_values(
            deserializer,
            BackendUnavailable::MAX_COUNT,
            "unavailable observer count exceeds VM classes",
        )
    }

    pub fn try_clone(&self) -> Result<Self, Refusal> {
        self.check()?;
        let mut authority = String::new();
        authority
            .try_reserve_exact(self.authority.len())
            .map_err(|_| Refusal::Exhausted {
                what: "physical report authority",
                bound: self.authority.len(),
            })?;
        authority.push_str(&self.authority);
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(self.devices.len())
            .map_err(|_| Refusal::Exhausted {
                what: "physical report entries",
                bound: self.devices.len(),
            })?;
        for device in &self.devices {
            devices.push(device.try_clone()?);
        }
        let mut unavailable = Vec::new();
        unavailable
            .try_reserve_exact(self.unavailable.len())
            .map_err(|_| Refusal::Exhausted {
                what: "unavailable VM observations",
                bound: BackendUnavailable::MAX_COUNT,
            })?;
        for observation in &self.unavailable {
            unavailable.push(observation.try_clone()?);
        }
        Ok(Self {
            authority,
            devices,
            unavailable,
        })
    }

    pub fn check(&self) -> Result<(), Refusal> {
        let invalid = invalid_report;
        if self.authority.is_empty() || self.authority.contains('\0') {
            return Err(invalid("native Store/turn authority is empty or invalid"));
        }
        if self.unavailable.len() > BackendUnavailable::MAX_COUNT {
            return Err(Refusal::Exhausted {
                what: "unavailable VM observations",
                bound: BackendUnavailable::MAX_COUNT,
            });
        }
        for (at, observation) in self.unavailable.iter().enumerate() {
            observation.check()?;
            if self
                .unavailable
                .iter()
                .take(at)
                .any(|earlier| earlier.backend == observation.backend)
            {
                return Err(invalid("unavailable VM observation backend is duplicated"));
            }
        }
        check_ids(self.devices.iter().map(|device| device.info.id))?;
        for device in &self.devices {
            device.info.check()?;
            if device.info.holder.is_some() {
                return Err(invalid("telemetry cannot report or grant holders"));
            }
            if (device.busy_per_mille.is_some() || device.memory_used_bytes.is_some())
                && (device.info.id.kind != DeviceKind::Gpu
                    || device.busy_per_mille.is_some_and(|busy| busy > 1000))
            {
                return Err(invalid("load is not a GPU reading within its native units"));
            }
            // Apple mapped allocation may exceed its working-set budget. Preserve known pressure.
        }
        Ok(())
    }
}

impl PhysicalDevice {
    pub fn try_clone(&self) -> Result<Self, Refusal> {
        Ok(Self {
            info: self.info.try_clone()?,
            busy_per_mille: self.busy_per_mille,
            memory_used_bytes: self.memory_used_bytes,
        })
    }
}

/// A device a runtime lists among its capacity's devices but won't offer now, and why (AC-T7.4.1): a VFIO function no
/// reset clears, which a VM had on this boot. Never free while so listed; its holder, if any, stays what it is.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceUnavailable {
    pub id: DeviceId,
    pub why: String,
}

impl DeviceUnavailable {
    pub fn try_clone(&self) -> Result<Self, Refusal> {
        let mut why = String::new();
        why.try_reserve_exact(self.why.len())
            .map_err(|_| Refusal::Exhausted {
                what: "unavailable device reason",
                bound: self.why.len(),
            })?;
        why.push_str(&self.why);
        Ok(Self { id: self.id, why })
    }
}

impl DeviceInfo {
    pub fn try_clone(&self) -> Result<Self, Refusal> {
        let mut model = String::new();
        model
            .try_reserve_exact(self.model.len())
            .map_err(|_| Refusal::Exhausted {
                what: "device model",
                bound: self.model.len(),
            })?;
        model.push_str(&self.model);
        Ok(Self {
            id: self.id,
            model,
            size: self.size,
            holder: self
                .holder
                .as_ref()
                .map(DeviceHolder::try_clone)
                .transpose()?,
        })
    }

    pub fn check(&self) -> Result<(), Refusal> {
        self.id.check()
    }
}

pub(crate) fn check_ids(ids: impl ExactSizeIterator<Item = DeviceId>) -> Result<(), Refusal> {
    let count = ids.len();
    if count > MAX_DEVICES {
        return Err(Refusal::Invalid {
            what: "devices",
            why: "count exceeds the u16 device index address space".to_owned(),
        });
    }
    // A fallible, count-bounded copy keeps duplicate validation O(n log n), without changing the
    // catalog order that the scheduler's indices refer to. sort_unstable needs no further buffer.
    let mut sorted = Vec::new();
    sorted
        .try_reserve_exact(count)
        .map_err(|_| Refusal::Exhausted {
            what: "device identities",
            bound: count,
        })?;
    for id in ids {
        id.check()?;
        sorted.push(id);
    }
    sorted.sort_unstable();
    let mut previous = None;
    for id in sorted {
        if previous == Some(id) {
            return Err(Refusal::Invalid {
                what: "devices",
                why: "duplicate device identity".to_owned(),
            });
        }
        previous = Some(id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Bounds, BoxSpec, Capacity, CapacityResources, ClassSpec, Program, Runtime, Stdio};

    /// AC-T7.4.1: native pools are actual owners, while old box-holder JSON remains byte-compatible.
    #[test]
    fn physical_holder_preserves_legacy_box_and_exact_backing_generation() {
        let owner: DeviceHolder = serde_json::from_str("\"old-box\"").unwrap();
        assert_eq!(owner.box_id().unwrap().as_str(), "old-box");
        assert_eq!(serde_json::to_string(&owner).unwrap(), "\"old-box\"");
        let key = crate::runtime::BackingKey {
            pool: [7; 32],
            generation: [9; 16],
        };
        let owner = DeviceHolder::Backing(key);
        let copied = owner.try_clone().unwrap();
        assert_eq!(copied, owner);
        assert!(owner.box_id().is_none());
        assert_eq!(owner.backing_key(), Some(key));
        assert_eq!(
            serde_json::from_str::<DeviceHolder>(&serde_json::to_string(&owner).unwrap()).unwrap(),
            owner
        );
        assert!(serde_json::from_str::<DeviceHolder>("{\"pool\":[],\"generation\":[]}").is_err());
        let mut report = PhysicalReport {
            authority: "one-native-owner".into(),
            unavailable: Vec::new(),
            devices: vec![PhysicalDevice {
                info: DeviceInfo {
                    id: id(DeviceKind::Gpu, 7),
                    model: "observed".into(),
                    size: 1,
                    holder: Some(owner),
                },
                busy_per_mille: None,
                memory_used_bytes: None,
            }],
        };
        assert!(
            report.check().is_err(),
            "telemetry cannot grant a Box or Backing holder"
        );
        report.devices[0].info.holder = None;
        assert!(report.check().is_ok());
    }

    /// AC-T7.4.1: unknown independent counters survive the public snapshot and confer no holder.
    #[test]
    fn physical_report_keeps_independent_unknown_counters_and_refuses_grants() {
        let info = DeviceInfo {
            id: id(DeviceKind::Gpu, u64::MAX),
            model: "actual-native".into(),
            size: 4096,
            holder: None,
        };
        let report = PhysicalReport {
            authority: "one-store-owner".into(),
            unavailable: Vec::new(),
            devices: vec![PhysicalDevice {
                info,
                busy_per_mille: None,
                memory_used_bytes: Some(0),
            }],
        };
        report.check().unwrap();
        let bytes = serde_json::to_vec(&report).unwrap();
        assert_eq!(
            serde_json::from_slice::<PhysicalReport>(&bytes).unwrap(),
            report
        );
        assert_eq!(report.devices[0].busy_per_mille, None);
        assert_eq!(report.devices[0].memory_used_bytes, Some(0));
        let mut malformed = report.try_clone().unwrap();
        malformed.devices[0].info.holder =
            Some(DeviceHolder::Box(BoxId::new("not-a-grant").unwrap()));
        assert!(malformed.check().is_err());
        malformed = report.try_clone().unwrap();
        malformed.devices[0].busy_per_mille = Some(1001);
        assert!(malformed.check().is_err());
        malformed = report.try_clone().unwrap();
        malformed
            .devices
            .push(report.devices[0].try_clone().unwrap());
        assert!(malformed.check().is_err());
        malformed = report;
        malformed.authority.clear();
        assert!(malformed.check().is_err());
    }

    fn id(kind: DeviceKind, key: u64) -> DeviceId {
        DeviceId { kind, key }
    }

    fn spec(devices: Vec<DeviceId>) -> BoxSpec {
        BoxSpec {
            id: BoxId::new("device-contract").unwrap(),
            bounds: Bounds::default(),
            class: ClassSpec::Process,
            owner: None,
            devices,
            allocation: None,
            route: None,
            band: 0,
        }
    }

    fn capacity(devices: Vec<DeviceInfo>) -> Capacity {
        Capacity {
            routes: Vec::new(),
            device_routes: Vec::new(),
            backings: Vec::new(),
            id: "device-pool".to_owned(),
            total: CapacityResources::default(),
            free: CapacityResources::default(),
            devices,
            cost: None,
            unavailable: Vec::new(),
            learned: None,
        }
    }

    fn info(id: DeviceId) -> DeviceInfo {
        DeviceInfo {
            id,
            model: "synthetic-private-model".to_owned(),
            size: u64::MAX,
            holder: Some(DeviceHolder::Box(
                BoxId::new("synthetic-private-holder").unwrap(),
            )),
        }
    }

    /// AC-T7.3.1: physical identity bounds, including all major/minor bits and VFIO group zero.
    #[test]
    fn physical_key_domains_are_checked_without_truncation() {
        for device in [
            id(DeviceKind::Gpu, 0),
            // Metal registry identities are u64; PCI-only narrowing loses valid native IDs.
            id(DeviceKind::Gpu, u64::MAX),
            id(DeviceKind::Gpu, (u64::from(u32::MAX) << 16) | 0xffff),
            id(DeviceKind::Vfio, 0),
            id(DeviceKind::Vfio, u64::from(u32::MAX)),
            id(DeviceKind::Block, 0),
            id(DeviceKind::Block, u64::MAX),
            id(DeviceKind::Net, 1),
            id(DeviceKind::Net, u64::from(u32::MAX)),
        ] {
            assert_eq!(device.check(), Ok(()));
            assert_eq!(info(device).check(), Ok(()));
            assert_eq!(spec(vec![device]).check(), Ok(()));
            assert_eq!(capacity(vec![info(device)]).check(), Ok(()));
        }
        for device in [
            id(DeviceKind::Vfio, 1 << 32),
            id(DeviceKind::Net, 0),
            id(DeviceKind::Net, 1 << 32),
        ] {
            for error in [
                device.check().unwrap_err(),
                info(device).check().unwrap_err(),
                spec(vec![device]).check().unwrap_err(),
                capacity(vec![info(device)]).check().unwrap_err(),
            ] {
                assert!(matches!(
                    error,
                    Refusal::Invalid {
                        what: "device id",
                        ..
                    }
                ));
                let text = error.to_string();
                assert!(text.contains(device.kind.name()));
                assert!(!text.contains(&device.key.to_string()));
                assert!(!text.contains("synthetic-private"));
            }
        }
    }

    /// AC-T7.3.1: every existing u16 index remains usable; duplicates cannot mint another offer.
    #[test]
    fn claims_and_offers_check_the_full_index_space_and_duplicate_identity() {
        let devices: Vec<_> = (0..MAX_DEVICES)
            .map(|key| id(DeviceKind::Vfio, u64::try_from(key).unwrap()))
            .collect();
        assert_eq!(spec(devices.clone()).check(), Ok(()));
        assert_eq!(
            capacity(devices.iter().copied().map(info).collect()).check(),
            Ok(())
        );
        let mut oversized = devices.clone();
        oversized.push(id(DeviceKind::Vfio, MAX_DEVICES as u64));
        assert!(matches!(
            spec(oversized.clone()).check(),
            Err(Refusal::Invalid {
                what: "devices",
                ..
            })
        ));
        assert!(matches!(
            capacity(oversized.into_iter().map(info).collect()).check(),
            Err(Refusal::Invalid {
                what: "devices",
                ..
            })
        ));
        let duplicate = vec![devices[0], devices[1], devices[0]];
        assert!(
            matches!(spec(duplicate.clone()).check(), Err(Refusal::Invalid { what: "devices", ref why }) if why.contains("duplicate"))
        );
        assert!(
            matches!(capacity(duplicate.into_iter().map(info).collect()).check(), Err(Refusal::Invalid { what: "devices", ref why }) if why.contains("duplicate"))
        );
        // Equal numeric keys of distinct physical kinds remain distinct.
        assert_eq!(
            spec(vec![id(DeviceKind::Gpu, 1), id(DeviceKind::Net, 1)]).check(),
            Ok(())
        );
    }

    /// AC-T7.3.1: empty claims/offers preserve old JSON; kind names and populated fields round-trip.
    #[test]
    fn device_json_preserves_legacy_empty_fields_and_lowercase_kinds() {
        let old_spec = r#"{"id":"device-contract","bounds":{"cpu_weight":null,"cpu_max_milli":null,"memory_max":null,"memory_high":null,"io_weight":null,"io_latency_us":null,"io_max":null,"pids_max":null},"class":"process"}"#;
        let old_capacity = r#"{"id":"device-pool","total":{"cpu_milli":0,"mem_bytes":0,"disk_bytes":0},"free":{"cpu_milli":0,"mem_bytes":0,"disk_bytes":0}}"#;
        let legacy: BoxSpec = serde_json::from_str(old_spec).unwrap();
        assert_eq!(legacy, spec(Vec::new()));
        assert_eq!(serde_json::to_string(&legacy).unwrap(), old_spec);
        let legacy: Capacity = serde_json::from_str(old_capacity).unwrap();
        assert_eq!(legacy, capacity(Vec::new()));
        assert_eq!(serde_json::to_string(&legacy).unwrap(), old_capacity);
        for kind in [
            DeviceKind::Gpu,
            DeviceKind::Vfio,
            DeviceKind::Block,
            DeviceKind::Net,
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), kind.name());
            let device = info(id(kind, 1));
            let json = serde_json::to_string(&device).unwrap();
            assert_eq!(serde_json::from_str::<DeviceInfo>(&json).unwrap(), device);
            let claimed = spec(vec![device.id]);
            assert_eq!(
                serde_json::from_str::<BoxSpec>(&serde_json::to_string(&claimed).unwrap()).unwrap(),
                claimed
            );
            let offered = capacity(vec![device]);
            assert_eq!(
                serde_json::from_str::<Capacity>(&serde_json::to_string(&offered).unwrap())
                    .unwrap(),
                offered
            );
        }
        let error = serde_json::from_str::<DeviceKind>(r#""synthetic-private-unknown-kind""#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("device.kind"));
        assert!(!error.contains("synthetic-private"));
    }

    /// AC-T7.3.1: a schema capability must not become simulated physical attachment or a box.
    #[test]
    fn an_unsupported_backend_refuses_claims_before_effects_and_still_cleans_up() {
        let runtime = crate::sim::SimRuntime::new();
        let claimed = spec(vec![id(DeviceKind::Gpu, 1)]);
        assert!(matches!(
            claimed.check_no_devices("test"),
            Err(Refusal::Platform {
                what: "devices",
                platform: "test",
                ..
            })
        ));
        assert!(matches!(
            runtime.create(claimed.clone()),
            Err(Refusal::Platform {
                what: "devices",
                platform: "sim",
                ..
            })
        ));
        assert!(matches!(
            runtime.run(
                claimed.clone(),
                Program::of(&["say", "never"]),
                Stdio::default()
            ),
            Err(Refusal::Platform {
                what: "devices",
                platform: "sim",
                ..
            })
        ));
        assert!(runtime.list().unwrap().is_empty());
        let invalid = spec(vec![id(DeviceKind::Net, 0)]);
        assert!(matches!(
            invalid.check_no_devices("test"),
            Err(Refusal::Invalid {
                what: "device id",
                ..
            })
        ));
        let legitimate = spec(Vec::new());
        runtime.create(legitimate.clone()).unwrap();
        assert_eq!(runtime.list().unwrap().len(), 1);
        runtime.delete(&legitimate.id).unwrap();
        assert!(runtime.list().unwrap().is_empty());
    }
}

#[cfg(test)]
mod unavailable_tests {
    use super::*;

    /// AC-T7.4.1: explicit VM uncertainty cannot fabricate GPUs or hide invalid native authority.
    #[test]
    fn qualified_vm_unavailable_is_bounded_unique_and_preserves_actual_report() {
        let refused = || Refusal::Backing {
            key: crate::BackingKey {
                pool: [1; 32],
                generation: [2; 16],
            },
            cause: crate::LifecycleCause::Uncertain,
        };
        let mut report = PhysicalReport {
            authority: "actual-native-owner".into(),
            devices: Vec::new(),
            unavailable: vec![
                BackendUnavailable::from_refusal(crate::Class::Firecracker, refused()).unwrap(),
            ],
        };
        report.check().unwrap();
        assert_eq!(report.try_clone().unwrap(), report);
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(
            serde_json::from_value::<PhysicalReport>(encoded.clone()).unwrap(),
            report
        );
        let mut malformed = encoded;
        malformed["unavailable"][0]["cause"]["cause"] = serde_json::json!("finished");
        assert!(serde_json::from_value::<PhysicalReport>(malformed).is_err());
        assert!(report.devices.is_empty());
        let legacy: PhysicalReport =
            serde_json::from_str(r#"{"authority":"actual-native-owner","devices":[]}"#).unwrap();
        assert!(legacy.unavailable.is_empty());
        report
            .unavailable
            .push(BackendUnavailable::from_refusal(crate::Class::Firecracker, refused()).unwrap());
        assert!(report.check().is_err());
        report.unavailable.pop();
        report.authority.clear();
        assert!(report.check().is_err());
        assert!(BackendUnavailable::from_refusal(crate::Class::Oci, refused()).is_err());
        let invalid = Refusal::Invalid {
            what: "native physical report authority",
            why: "mismatch".into(),
        };
        assert!(
            serde_json::to_value(BackendUnavailable {
                backend: crate::Class::Firecracker,
                cause: invalid.clone(),
            })
            .is_err()
        );
        assert_eq!(
            BackendUnavailable::from_refusal(crate::Class::Firecracker, invalid.clone())
                .unwrap_err(),
            invalid
        );
        for cause in [
            crate::LifecycleCause::Pending,
            crate::LifecycleCause::Busy,
            crate::LifecycleCause::Uncertain,
        ] {
            let diagnostic = BackendUnavailable::from_refusal(
                crate::Class::Firecracker,
                Refusal::Backing {
                    key: crate::BackingKey {
                        pool: [1; 32],
                        generation: [2; 16],
                    },
                    cause,
                },
            )
            .unwrap();
            let mut encoded = serde_json::to_value(&diagnostic).unwrap();
            assert_eq!(
                serde_json::from_value::<BackendUnavailable>(encoded.clone()).unwrap(),
                diagnostic
            );
            encoded["cause"]["why"] = serde_json::json!("general refusal text must not decode");
            assert!(serde_json::from_value::<BackendUnavailable>(encoded).is_err());
        }
        let first = BackendUnavailable::from_refusal(crate::Class::Firecracker, refused()).unwrap();
        let second = BackendUnavailable::from_refusal(crate::Class::Shards, refused()).unwrap();
        let oversized = serde_json::json!({"authority":"actual-native-owner","devices":[],"unavailable":[
            first, second, {"bad":"third must not decode"}
        ]});
        assert!(
            serde_json::from_value::<PhysicalReport>(oversized)
                .unwrap_err()
                .to_string()
                .contains("count exceeds")
        );
    }
    /// AC-T7.4.1: permission is keyed, and cold permission requires a current canonical envelope.
    #[test]
    fn qualified_route_permission_is_keyed_and_cannot_invent_cold_metadata() {
        use crate::{
            BackingKey, BackingOffer, BackingPhase, CapacityResources, Class, Execution,
            GpuEligibility, ImageArch, ImageOs, ImagePlatform, LifecycleCause, RouteKey,
            RouteOffer, RuntimeCost,
        };
        let floor = CapacityResources {
            cpu_milli: 1000,
            mem_bytes: crate::MIN_MEMORY,
            disk_bytes: 0,
        };
        let cost = RuntimeCost {
            id: [1; 32],
            floor,
            ceiling: floor,
            quantum: CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 1,
            },
            guest_overhead: CapacityResources::default(),
            overhead: CapacityResources::default(),
        };
        let key = BackingKey {
            pool: [2; 32],
            generation: [3; 16],
        };
        let mut backing = BackingOffer {
            guest: Default::default(),
            key,
            cost,
            phase: BackingPhase::Prepared,
            devices: vec![DeviceId {
                kind: DeviceKind::Gpu,
                key: 1,
            }],
        };
        let offer = RouteOffer {
            key: RouteKey {
                class: Class::Shards,
                image: ImagePlatform {
                    os: ImageOs::Linux,
                    arch: ImageArch::Amd64,
                },
                execution: Execution::ShardsDedicated,
                gpu: GpuEligibility::Required,
            },
            cost: Some(cost),
            backing: Some(key),
        };
        let pending = BackendUnavailable::from_refusal(
            Class::Shards,
            Refusal::Backing {
                key,
                cause: LifecycleCause::Pending,
            },
        )
        .unwrap();
        assert!(!pending.blocks_route(&offer, Some(&backing)).unwrap());
        assert!(pending.blocks_route(&offer, None).unwrap());
        backing.phase = BackingPhase::Granted;
        assert!(!pending.blocks_route(&offer, Some(&backing)).unwrap());
        backing.phase = BackingPhase::Live;
        assert!(pending.blocks_route(&offer, Some(&backing)).unwrap());
        let uncertain = BackendUnavailable::from_refusal(
            Class::Shards,
            Refusal::Backing {
                key,
                cause: LifecycleCause::Uncertain,
            },
        )
        .unwrap();
        backing.phase = BackingPhase::Prepared;
        assert!(uncertain.blocks_route(&offer, Some(&backing)).unwrap());
        let other = BackingKey {
            pool: [4; 32],
            generation: [5; 16],
        };
        let shared = RouteOffer {
            key: RouteKey {
                class: Class::Oci,
                execution: Execution::ShardsShared,
                gpu: GpuEligibility::None,
                ..offer.key
            },
            cost: None,
            backing: Some(other),
        };
        let warm = BackingOffer {
            guest: Default::default(),
            key: other,
            cost,
            phase: BackingPhase::Live,
            devices: Vec::new(),
        };
        assert!(!uncertain.blocks_route(&shared, Some(&warm)).unwrap());
        assert!(pending.blocks_route(&offer, Some(&warm)).is_err());
    }
}
