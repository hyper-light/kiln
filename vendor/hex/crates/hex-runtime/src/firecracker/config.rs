use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::{fs::File, io::Read};

use hex_api::{BoxId, BoxSpec, ClassSpec};
use hex_api::{Refusal, RuntimeCost};
use hex_core::stream::files::Limits;
use serde::Deserialize;
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::invalid;

/// Official VFIO development chain; not a mainline/released Firecracker feature.
pub const PREBOOT_REVISION: &str = "500efc6bb169705f5d9417cb411e3f6723e051fe";

/// Administrator configuration. These paths, deadlines and measured VMM allowance never arrive
/// in a peer's box payload. ROOT records the actual binary hash with its build/source receipt.
#[derive(Debug, Clone)]
pub struct Config {
    pub binary: PathBuf,
    pub binary_sha256: [u8; 32],
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub state_dir: PathBuf,
    pub pool_id: String,
    pub boot_timeout: Duration,
    pub stop_timeout: Duration,
    pub operation_timeout: Duration,
    /// Exact evidence content is verified against cost.id before capability exposure.
    pub cost_evidence: PathBuf,
    pub cost: RuntimeCost,
    pub stage_limits: Limits,
    /// Video BIOS images, by host PCI function, for passthrough GPUs that have no ROM of their own
    /// (an integrated GPU, whose platform firmware publishes its image): the VMM gives each to its
    /// guest in a VFCT ACPI table (vendor patch 0004). An operator's input named by path and pinned by
    /// SHA-256, verified at launch as the VMM binary is; none where absent (AC-T7.4.1).
    pub vbios: Vec<Vbios>,
}

/// One host PCI function's video BIOS image, an operator's input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vbios {
    /// The function, as the device catalog names it (canonical `dddd:bb:dd.f`).
    pub sbdf: String,
    pub path: PathBuf,
    pub sha256: [u8; 32],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VbiosFile {
    sbdf: Text,
    path: Text,
    sha256: Text,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    binary: Text,
    binary_sha256: Text,
    kernel: Text,
    initrd: Text,
    state_dir: Text,
    pool_id: Text,
    boot_ms: u32,
    stop_ms: u32,
    operation_ms: u32,
    cost_evidence: Text,
    cost: RuntimeCost,
    staging: Staging,
    #[serde(default)]
    vbios: Vec<VbiosFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Staging {
    entries: u64,
    bytes: u64,
    path: usize,
}

struct Text(String);

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = Text;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("bounded configuration text")
            }
            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Text, E> {
                let mut owned = String::new();
                owned
                    .try_reserve_exact(text.len())
                    .map_err(|_| E::custom("configuration text allocation refused"))?;
                owned.push_str(text);
                Ok(Text(owned))
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

fn digest(text: &str) -> Result<[u8; 32], Refusal> {
    let mut digest = [0u8; 32];
    if text.len()
        != digest
            .len()
            .checked_mul(2)
            .ok_or_else(|| invalid("digest length overflow"))?
    {
        return Err(invalid(
            "SHA256 pin must contain exactly 64 hexadecimal digits",
        ));
    }
    let (pairs, remainder) = text.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(invalid(
            "SHA256 pin contains an incomplete hexadecimal pair",
        ));
    }
    for (out, pair) in digest.iter_mut().zip(pairs) {
        let byte = std::str::from_utf8(pair)
            .ok()
            .and_then(|pair| u8::from_str_radix(pair, 16).ok())
            .ok_or_else(|| invalid("SHA256 pin is not hexadecimal"))?;
        *out = byte;
    }
    Ok(digest)
}

fn path(text: Text) -> Result<PathBuf, Refusal> {
    let mut path = PathBuf::new();
    path.try_reserve_exact(text.0.len())
        .map_err(|_| invalid("configuration path allocation refused"))?;
    path.push(text.0);
    Ok(path)
}

#[derive(Debug, Serialize)]
pub(super) struct Passthrough {
    pub id: String,
    pub sbdf: String,
    /// A display or processing-accelerator function, which the guest catalogs as its own GPU.
    #[serde(skip)]
    pub graphics: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) struct VmConfig<'a> {
    boot_source: BootSource<'a>,
    // Pinned resources::VmmConfig requires this Vec even for an initrd-only VM; it has no default.
    drives: [(); 0],
    machine_config: MachineConfig,
    vsock: Vsock<'a>,
    device_passthrough: Vec<DevicePassthrough<'a>>,
}

/// One passthrough function as the VMM's configuration names it, with its video BIOS image where
/// one is configured (vendor patch 0004's `vbios_path`).
#[derive(Serialize)]
struct DevicePassthrough<'a> {
    id: &'a str,
    sbdf: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    vbios_path: Option<&'a Path>,
}

#[derive(Serialize)]
struct BootSource<'a> {
    kernel_image_path: &'a Path,
    initrd_path: &'a Path,
    boot_args: String,
}

#[derive(Serialize)]
struct MachineConfig {
    vcpu_count: u8,
    mem_size_mib: u64,
    smt: bool,
}

#[derive(Serialize)]
struct Vsock<'a> {
    guest_cid: u32,
    uds_path: &'a Path,
}

impl Config {
    /// One strict administrator grammar. No field has a default, including evidence, operation
    /// deadlines and staging limits. The native config byte ceiling bounds growing inputs too.
    pub fn from_file(path: &Path) -> Result<Self, Refusal> {
        let mut file =
            File::open(path).map_err(|e| Refusal::os("opening Firecracker configuration", e))?;
        let bound = hex_core::bundle::MAX_CONFIG_BYTES;
        let room = bound
            .checked_add(1)
            .ok_or_else(|| invalid("configuration byte bound overflows"))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(room)
            .map_err(|_| invalid("configuration allocation refused"))?;
        bytes.resize(room, 0);
        let mut used = 0usize;
        while used < room {
            let tail = bytes
                .get_mut(used..)
                .ok_or_else(|| invalid("configuration read overflows"))?;
            let count = file
                .read(tail)
                .map_err(|e| Refusal::os("reading Firecracker configuration", e))?;
            if count == 0 {
                break;
            }
            used = used
                .checked_add(count)
                .ok_or_else(|| invalid("configuration read overflows"))?;
        }
        Self::parse(
            bytes
                .get(..used)
                .ok_or_else(|| invalid("configuration read overflows"))?,
        )
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, Refusal> {
        if bytes.len() > hex_core::bundle::MAX_CONFIG_BYTES {
            return Err(invalid(
                "configuration exceeds the native config byte ceiling",
            ));
        }
        let input: FileConfig = serde_json::from_slice(bytes).map_err(|_| {
            invalid("malformed, missing, duplicate or unknown configuration fields")
        })?;
        let config = Self {
            binary: path(input.binary)?,
            binary_sha256: digest(&input.binary_sha256.0)?,
            kernel: path(input.kernel)?,
            initrd: path(input.initrd)?,
            state_dir: path(input.state_dir)?,
            pool_id: input.pool_id.0,
            boot_timeout: Duration::from_millis(u64::from(input.boot_ms)),
            stop_timeout: Duration::from_millis(u64::from(input.stop_ms)),
            operation_timeout: Duration::from_millis(u64::from(input.operation_ms)),
            cost_evidence: path(input.cost_evidence)?,
            cost: input.cost,
            stage_limits: Limits {
                entries: input.staging.entries,
                bytes: input.staging.bytes,
                path: input.staging.path,
            },
            vbios: vbios(input.vbios)?,
        };
        config.check()?;
        Ok(config)
    }

    pub fn check(&self) -> Result<(), Refusal> {
        for path in [
            &self.binary,
            &self.kernel,
            &self.initrd,
            &self.state_dir,
            &self.cost_evidence,
        ]
        .into_iter()
        .chain(self.vbios.iter().map(|image| &image.path))
        {
            if !path.is_absolute() || path.to_str().is_none_or(|p| p.contains('\0')) {
                return Err(invalid(
                    "administrator paths must be absolute UTF-8 without NUL",
                ));
            }
        }
        if self.pool_id.is_empty() || self.pool_id.len() > hex_core::wire::MAX_MESSAGE {
            return Err(invalid(
                "resource pool identity is empty or exceeds the wire bound",
            ));
        }
        self.boot_millis()?;
        millis(self.stop_timeout)?;
        millis(self.operation_timeout)?;
        self.cost.check()?;
        if self
            .cost
            .ceiling
            .checked_add(self.cost.overhead)
            .is_none_or(|host| u32::try_from(host.cpu_milli).is_err())
        {
            return Err(invalid(
                "configured host CPU ceiling cannot be enforced by the native bound API",
            ));
        }
        // CPU milli and MiB are the pinned VMM's actual configuration units. A profile using
        // finer units would admit resources which cannot be configured exactly.
        if self.binary_sha256 == [0; 32]
            || self.cost.quantum.cpu_milli != 1000
            || self.cost.quantum.mem_bytes != 1 << 20
            || self.cost.ceiling.cpu_milli > 32_000
            || self.stage_limits.entries == 0
            || self.stage_limits.bytes == 0
            || self.stage_limits.path == 0
        {
            return Err(invalid(
                "operation deadlines and staging budgets must be positive",
            ));
        }
        Ok(())
    }

    /// Verify bytes before this adapter may advertise its cost or execute its configured VMM.
    /// The coordinator reviews measurements; a matching digest identifies that evidence exactly,
    /// and is not itself proof of a measured allowance or GPU/native effect.
    pub fn verify_inputs(&self) -> Result<(), Refusal> {
        self.check()?;
        if hash_file(
            &self.cost_evidence,
            Some(hex_core::bundle::MAX_CONFIG_BYTES),
        )? != self.cost.id
        {
            return Err(invalid(
                "runtime cost evidence differs from its required SHA256 pin",
            ));
        }
        if hash_file(&self.binary, None)? != self.binary_sha256 {
            return Err(invalid(
                "Firecracker executable differs from its required SHA256 pin",
            ));
        }
        for image in &self.vbios {
            if hash_file(&image.path, Some(hex_core::bundle::MAX_CONFIG_BYTES))? != image.sha256 {
                return Err(invalid(format!(
                    "the video BIOS image for {} differs from its required SHA256 pin",
                    image.sbdf
                )));
            }
        }
        Ok(())
    }

    pub(super) fn boot_millis(&self) -> Result<u32, Refusal> {
        millis(self.boot_timeout)
    }

    pub(super) fn machine_memory(&self, spec: &BoxSpec) -> Result<u64, Refusal> {
        self.guest(spec)?
            .mem_bytes
            .checked_div(1 << 20)
            .filter(|mib| *mib > 0)
            .ok_or_else(|| {
                invalid("memory reservation cannot hold guest plus measured VMM allowance")
            })
    }

    pub(super) fn guest(&self, spec: &BoxSpec) -> Result<hex_api::CapacityResources, Refusal> {
        if spec.bounds.cpu_max_milli.is_none() || spec.bounds.memory_max.is_none() {
            return Err(invalid(
                "finite original VM workload CPU and memory caps required",
            ));
        }
        let allocation = spec
            .allocation
            .ok_or_else(|| invalid("initialized Firecracker allocation required before create"))?;
        if allocation.cost != self.cost {
            return Err(invalid(
                "admitted cost profile differs from initialized backend",
            ));
        }
        spec.check()?;
        allocation.guest()
    }

    pub(super) fn machine<'a>(
        &'a self,
        spec: &BoxSpec,
        vsock: &'a Path,
        devices: &'a [Passthrough],
    ) -> Result<VmConfig<'a>, Refusal> {
        self.check()?;
        spec.check()?;
        if !matches!(spec.class, ClassSpec::Firecracker(_)) {
            return Err(invalid(
                "this endpoint executes only explicit Firecracker payloads",
            ));
        }
        self.machine_guest(self.guest(spec)?, vsock, devices, &spec.id)
    }

    pub(super) fn machine_guest<'a>(
        &'a self,
        guest: hex_api::CapacityResources,
        vsock: &'a Path,
        devices: &'a [Passthrough],
        owner: &BoxId,
    ) -> Result<VmConfig<'a>, Refusal> {
        self.check()?;
        if !self.cost.ceiling.holds(guest)
            || !guest.holds(self.cost.floor)
            || guest.cpu_milli.checked_rem(1000) != Some(0)
            || guest.mem_bytes.checked_rem(1 << 20) != Some(0)
        {
            return Err(invalid(
                "guest envelope differs from configured native units/bounds",
            ));
        }
        let count = guest
            .cpu_milli
            .checked_div(1000)
            .and_then(|n| u8::try_from(n).ok())
            .filter(|n| *n > 0 && *n <= 32)
            .ok_or_else(|| invalid("vCPU count exceeds Firecracker's pinned 1..=32 API bound"))?;
        let prefix = "console=ttyS0 panic=-1 hex.firecracker-ready hex.firecracker-ready-ms=";
        let mut boot_args = String::new();
        let bound = prefix
            .len()
            .checked_add(10)
            .ok_or_else(|| invalid("boot argument size overflows"))?;
        boot_args
            .try_reserve_exact(bound)
            .map_err(|_| invalid("boot argument allocation refused"))?;
        boot_args.push_str(prefix);
        use std::fmt::Write as _;
        write!(&mut boot_args, "{}", self.boot_millis()?)
            .map_err(|_| invalid("boot budget cannot be encoded"))?;
        // The guest catalogs each GPU function its VMM attaches by the guest's own PCI identity;
        // the words carry only the owner and how many (guest::gpu::Boot::Pci).
        let gpus = devices.iter().filter(|device| device.graphics).count();
        if gpus != 0 {
            crate::guest::gpu::append_pci_authority(&mut boot_args, owner, gpus)?;
        }
        let mut device_passthrough = Vec::new();
        device_passthrough
            .try_reserve_exact(devices.len())
            .map_err(|_| invalid("passthrough configuration allocation refused"))?;
        device_passthrough.extend(devices.iter().map(|device| {
            DevicePassthrough {
                id: &device.id,
                sbdf: &device.sbdf,
                vbios_path: self
                    .vbios
                    .iter()
                    .find(|image| image.sbdf == device.sbdf)
                    .map(|image| image.path.as_path()),
            }
        }));
        Ok(VmConfig {
            boot_source: BootSource {
                kernel_image_path: &self.kernel,
                initrd_path: &self.initrd,
                boot_args,
            },
            drives: [],
            machine_config: MachineConfig {
                vcpu_count: count,
                mem_size_mib: guest
                    .mem_bytes
                    .checked_div(1 << 20)
                    .filter(|mib| *mib > 0)
                    .ok_or_else(|| invalid("guest memory must contain complete positive MiB"))?,
                smt: false,
            },
            // CID3 is the first guest CID after reserved hypervisor0/local1/host2 (vsock(7)).
            // Every microVM has a separate Unix endpoint, so no authority depends on a global CID.
            vsock: Vsock {
                guest_cid: 3,
                uds_path: vsock,
            },
            device_passthrough,
        })
    }
}

/// Whether `sbdf` names a PCI function as the device catalog does: Linux's `dddd:bb:dd.f`.
fn canonical_function(sbdf: &str) -> Result<(), Refusal> {
    let pci = hex_gpu::PciAddress::parse(sbdf)
        .filter(|pci| pci.domain <= u32::from(u16::MAX))
        .ok_or_else(|| invalid("a video BIOS image names no PCI function"))?;
    let mut canonical = String::new();
    canonical
        .try_reserve_exact("ffff:ff:1f.7".len())
        .map_err(|_| invalid("PCI function allocation refused"))?;
    use std::fmt::Write as _;
    write!(&mut canonical, "{pci}").map_err(|_| invalid("PCI function cannot be encoded"))?;
    if canonical != sbdf {
        return Err(invalid(
            "a video BIOS image must name its PCI function as dddd:bb:dd.f",
        ));
    }
    Ok(())
}

/// The configured video BIOS images: each a canonical function's, at most one a function.
fn vbios(files: Vec<VbiosFile>) -> Result<Vec<Vbios>, Refusal> {
    let mut images: Vec<Vbios> = Vec::new();
    images
        .try_reserve_exact(files.len())
        .map_err(|_| invalid("video BIOS configuration allocation refused"))?;
    for file in files {
        canonical_function(&file.sbdf.0)?;
        if images.iter().any(|image| image.sbdf == file.sbdf.0) {
            return Err(invalid(
                "a PCI function names more than one video BIOS image",
            ));
        }
        images.push(Vbios {
            sha256: digest(&file.sha256.0)?,
            path: path(file.path)?,
            sbdf: file.sbdf.0,
        });
    }
    Ok(images)
}

fn millis(duration: Duration) -> Result<u32, Refusal> {
    u32::try_from(duration.as_millis())
        .ok()
        .filter(|ms| *ms > 0 && Duration::from_millis(u64::from(*ms)) == duration)
        .ok_or_else(|| invalid("deadline must fit positive u32 milliseconds"))
}

fn hash_file(path: &Path, byte_bound: Option<usize>) -> Result<[u8; 32], Refusal> {
    let mut file =
        File::open(path).map_err(|e| Refusal::os("opening pinned Firecracker input", e))?;
    let metadata = file
        .metadata()
        .map_err(|e| Refusal::os("reading pinned input metadata", e))?;
    if !metadata.is_file()
        || byte_bound
            .is_some_and(|bound| u64::try_from(bound).is_ok_and(|bound| metadata.len() > bound))
    {
        return Err(invalid(
            "pinned input is not a regular file within its byte bound",
        ));
    }
    // The observed regular-file length bounds the whole operation; one excess byte detects a
    // growing input. Reuse the native config byte ceiling for the transient hash buffer.
    let limit = metadata.len();
    let mut buffer = Vec::new();
    let room = hex_core::bundle::MAX_CONFIG_BYTES;
    buffer
        .try_reserve_exact(room)
        .map_err(|_| invalid("input hash buffer allocation refused"))?;
    buffer.resize(room, 0);
    let mut hasher = Sha256::new();
    let mut used = 0u64;
    loop {
        let remaining = limit
            .checked_sub(used)
            .and_then(|left| left.checked_add(1))
            .ok_or_else(|| invalid("pinned input length overflow"))?;
        let count = usize::try_from(
            remaining.min(u64::try_from(room).map_err(|_| invalid("hash buffer length overflow"))?),
        )
        .map_err(|_| invalid("pinned input length overflow"))?;
        let count = file
            .read(
                buffer
                    .get_mut(..count)
                    .ok_or_else(|| invalid("hash buffer length overflow"))?,
            )
            .map_err(|e| Refusal::os("reading pinned Firecracker input", e))?;
        if count == 0 {
            break;
        }
        used = used
            .checked_add(u64::try_from(count).map_err(|_| invalid("input read count overflow"))?)
            .filter(|used| *used <= limit)
            .ok_or_else(|| invalid("pinned input grew while reading"))?;
        hasher.update(
            buffer
                .get(..count)
                .ok_or_else(|| invalid("input read count overflow"))?,
        );
    }
    if used != limit {
        return Err(invalid("pinned input shortened while reading"));
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_api::{Bounds, BoxId, Network, OciSpec, Program, Rootfs};

    fn cost() -> RuntimeCost {
        use hex_api::CapacityResources;
        RuntimeCost {
            id: [1; 32],
            floor: CapacityResources {
                cpu_milli: 1000,
                mem_bytes: 64 << 20,
                disk_bytes: 0,
            },
            ceiling: CapacityResources {
                cpu_milli: 32000,
                mem_bytes: 256 << 20,
                disk_bytes: 1 << 30,
            },
            quantum: CapacityResources {
                cpu_milli: 1000,
                mem_bytes: 1 << 20,
                disk_bytes: 1,
            },
            guest_overhead: CapacityResources {
                cpu_milli: 0,
                mem_bytes: 3 << 20,
                disk_bytes: 0,
            },
            overhead: CapacityResources {
                cpu_milli: 25,
                mem_bytes: 5 << 20,
                disk_bytes: 0,
            },
        }
    }

    fn config() -> Config {
        Config {
            binary: "/private/firecracker".into(),
            binary_sha256: [2; 32],
            kernel: "/private/vmlinux".into(),
            initrd: "/private/initrd".into(),
            state_dir: "/private/owned".into(),
            pool_id: "one-physical-host".into(),
            boot_timeout: Duration::from_secs(3),
            stop_timeout: Duration::from_secs(1),
            operation_timeout: Duration::from_secs(1),
            cost_evidence: "/private/cost-evidence".into(),
            cost: cost(),
            stage_limits: Limits {
                entries: 10,
                bytes: 1 << 20,
                path: 100,
            },
            vbios: Vec::new(),
        }
    }

    fn spec() -> BoxSpec {
        BoxSpec {
            id: BoxId::new("fixture").unwrap(),
            bounds: Bounds {
                cpu_max_milli: Some(1500),
                memory_max: Some(65 << 20),
                ..Bounds::default()
            },
            class: ClassSpec::Firecracker(Box::new(OciSpec {
                rootfs: Rootfs::Dir("/fixture/root".into()),
                init: Program::of(&["wait"]),
                mounts: Vec::new(),
                hostname: None,
                network: Network::Private,
                hosts: Vec::new(),
            })),
            owner: None,
            devices: Vec::new(),
            route: None,
            allocation: Some(
                cost()
                    .allocate(hex_api::CapacityResources {
                        cpu_milli: 1500,
                        mem_bytes: 65 << 20,
                        disk_bytes: 0,
                    })
                    .unwrap(),
            ),
            band: 0,
        }
    }

    /// AC-T7.4.1: the exact selected function, CPU cap and charged guest memory reach Firecracker's
    /// real config grammar; the host readiness budget is explicit, with no Shards config.
    #[test]
    fn preboot_config_carries_exact_physical_selection_and_charged_vm_bounds() {
        let config = config();
        let spec = spec();
        let devices = [Passthrough {
            id: "pci_100".into(),
            sbdf: "0000:01:00.0".into(),
            graphics: true,
        }];
        let value = serde_json::to_value(
            config
                .machine(&spec, Path::new("/private/owned/vsock"), &devices)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(value["device-passthrough"][0]["sbdf"], "0000:01:00.0");
        assert_eq!(value["device-passthrough"][0]["id"], "pci_100");
        assert_eq!(value["machine-config"]["vcpu_count"], 2);
        assert_eq!(value["machine-config"]["mem_size_mib"], 68);
        assert!(
            value["boot-source"]["boot_args"]
                .as_str()
                .unwrap()
                .contains("hex.firecracker-ready-ms=3000")
        );
        assert_eq!(value["vsock"]["uds_path"], "/private/owned/vsock");
    }

    /// AC-T7.4.1: pinned FC500ef/6b1a resources.rs:98-118 requires drives with no serde default.
    /// Native QA's actual VMM parser rejected the omitted field before any guest boot.
    #[test]
    fn initrd_only_config_satisfies_the_pinned_required_drives_schema() {
        #[derive(Deserialize)]
        struct PinnedRequired {
            #[serde(rename = "boot-source")]
            _boot_source: serde::de::IgnoredAny,
            drives: Vec<serde::de::IgnoredAny>,
        }
        let config = config();
        let spec = spec();
        let value = serde_json::to_value(
            config
                .machine(&spec, Path::new("/private/owned/vsock"), &[])
                .unwrap(),
        )
        .unwrap();
        let parsed: PinnedRequired = serde_json::from_value(value.clone()).unwrap();
        assert!(parsed.drives.is_empty());
        let mut omitted = value;
        omitted.as_object_mut().unwrap().remove("drives");
        assert!(serde_json::from_value::<PinnedRequired>(omitted).is_err());
    }

    /// AC-T7.4.1: deadline precision/overflow and missing finite reservations refuse as data.
    #[test]
    fn invalid_vm_deadline_or_reservation_refuses_before_native_effects() {
        let mut config = config();
        config.boot_timeout = Duration::from_nanos(1);
        assert!(config.check().is_err());
        config.boot_timeout = Duration::from_millis(u64::from(u32::MAX) + 1);
        assert!(config.check().is_err());
        config.boot_timeout = Duration::from_secs(1);
        let mut spec = spec();
        spec.allocation = None;
        assert!(
            config
                .machine(&spec, Path::new("/private/vsock"), &[])
                .is_err()
        );
        spec.allocation = Some(
            config
                .cost
                .allocate(hex_api::CapacityResources {
                    cpu_milli: 1500,
                    mem_bytes: 65 << 20,
                    disk_bytes: 0,
                })
                .unwrap(),
        );
        spec.bounds.memory_max = Some(100 << 20);
        assert!(
            config
                .machine(&spec, Path::new("/private/vsock"), &[])
                .is_err()
        );
    }

    use crate::devices::{Catalog, Route};
    use crate::guest::gpu::{Boot, DrmNode, PciCatalog, PciFunction};
    use hex_api::{DeviceId, DeviceKind};

    fn gpu(key: u64) -> DeviceId {
        DeviceId {
            kind: DeviceKind::Gpu,
            key,
        }
    }

    /// The guest's own view of one function its VMM attached: amdgpu's DRM nodes and the native
    /// facts hex-gpu reads for it (synthetic values, not a measurement).
    fn attached(address: &str) -> (PciFunction, hex_gpu::Gpu) {
        let address = hex_gpu::PciAddress::parse(address).unwrap();
        let node = |name: &str, minor| DrmNode {
            name: name.into(),
            major: 226,
            minor,
        };
        (
            PciFunction {
                address,
                nodes: vec![node("card0", 0), node("renderD128", 128)],
            },
            hex_gpu::Gpu {
                vendor: hex_gpu::Vendor::Amd,
                model: "1002:13c0 rev e5".into(),
                memory_total_bytes: 512 << 20,
                identity: hex_gpu::Identity::Pci(address),
                driver_version: "6.18.48".into(),
            },
        )
    }

    /// What the guest's PID 1 writes for `boot` when its kernel lists `functions`.
    fn guest_catalog(
        boot: &str,
        owner: &BoxId,
        functions: &[(PciFunction, hex_gpu::Gpu)],
    ) -> Result<PciCatalog, Refusal> {
        match Boot::parse(boot)? {
            Some(Boot::Pci {
                owner: granted,
                count,
            }) if granted == *owner => {
                let (functions, facts): (Vec<_>, Vec<_>) = functions.iter().cloned().unzip();
                crate::guest::gpu::pci_catalog(
                    count,
                    &functions,
                    &facts,
                    "/run/hex/state/firecracker-gpu.cdi",
                )
            }
            other => panic!("a PCI guest's boot line granted {other:?}"),
        }
    }

    /// The guest Node's OCI capacity from its catalog, as `Node::capacity` makes it.
    fn guest_capacity(catalog: &PciCatalog) -> hex_api::Capacity {
        use hex_api::{CapacityResources, ImageArch, ImageOs, ImagePlatform};
        let catalog = Catalog::parse(catalog.catalog.as_bytes()).unwrap();
        let total = CapacityResources {
            cpu_milli: 1_000,
            mem_bytes: 128 << 20,
            disk_bytes: 0,
        };
        let mut capacity = hex_api::Capacity {
            id: "the guest's own node".into(),
            total,
            free: total,
            devices: catalog
                .entries_for(Route::Cdi)
                .map(|entry| entry.report(None).unwrap())
                .collect(),
            cost: None,
            routes: Vec::new(),
            device_routes: Vec::new(),
            backings: Vec::new(),
            unavailable: Vec::new(),
            learned: None,
        };
        crate::devices::native_routes(
            &mut capacity,
            hex_api::Class::Oci,
            ImagePlatform {
                os: ImageOs::Linux,
                arch: ImageArch::Amd64,
            },
        )
        .unwrap();
        capacity
    }

    /// The guest Node's own facts report for the GPUs it reads.
    fn guest_report(guests: &[DeviceId]) -> hex_api::PhysicalReport {
        hex_api::PhysicalReport {
            unavailable: Vec::new(),
            authority: "the guest's own node".into(),
            devices: guests
                .iter()
                .map(|id| hex_api::PhysicalDevice {
                    info: hex_api::DeviceInfo {
                        id: *id,
                        model: "1002:13c0 rev e5".into(),
                        size: 512 << 20,
                        holder: None,
                    },
                    busy_per_mille: None,
                    memory_used_bytes: None,
                })
                .collect(),
        }
    }

    /// The boot line this adapter writes for `spec` with `devices` passed through.
    fn boot_line(spec: &BoxSpec, devices: &[Passthrough]) -> String {
        let vm = serde_json::to_value(
            config()
                .machine(spec, Path::new("/private/owned/vsock"), devices)
                .unwrap(),
        )
        .unwrap();
        vm["boot-source"]["boot_args"].as_str().unwrap().to_owned()
    }

    fn passthrough(id: &str, sbdf: &str, graphics: bool) -> Passthrough {
        Passthrough {
            id: id.into(),
            sbdf: sbdf.into(),
            graphics,
        }
    }

    /// AC-T7.4.1: original. One VM given one PCI GPU (host 0000:79:00.0, which its VMM attaches at
    /// guest 0000:00:03.0). Do: take the boot line this adapter writes, catalog what the guest's
    /// PID 1 writes from it, make the guest Node's OCI capacity from that catalog as the Node does,
    /// and check the binding as the observer does. Expect: the adapter accepts the GPU it passed
    /// through. (Red at fd6d126b: the line carried no GPU words and the guest wrote no catalog.)
    #[test]
    fn original_adapter_accepts_the_pci_gpu_it_passed_to_its_guest() {
        let (host, guest) = (gpu(0x79 << 8), gpu(3 << 3));
        let mut spec = spec();
        spec.devices = vec![host];
        let boot = boot_line(&spec, &[passthrough("pci_7900", "0000:79:00.0", true)]);
        let catalog = guest_catalog(&boot, &spec.id, &[attached("0000:00:03.0")]).unwrap();
        super::super::observations::check_bindings(
            &[super::super::observations::Assignment { host, guest }],
            &guest_report(&[guest]),
            &guest_capacity(&catalog),
            Some(&spec.id),
        )
        .unwrap();
    }

    /// AC-T7.4.1: do write a VM's boot line with one GPU function and its audio companion passed
    /// through; expect only the owner and the count of GPU functions, never a host identity, and
    /// no GPU words for a VM given no GPU function.
    #[test]
    fn pci_boot_words_carry_only_the_owner_and_the_count_of_gpu_functions() {
        let mut spec = spec();
        spec.devices = vec![gpu(0x79 << 8)];
        let boot = boot_line(
            &spec,
            &[
                passthrough("pci_7900", "0000:79:00.0", true),
                passthrough("pci_7901", "0000:79:00.1", false),
            ],
        );
        assert!(
            boot.ends_with(" hex.gpu.box=fixture hex.gpu.pci=1"),
            "{boot}"
        );
        assert_eq!(
            Boot::parse(&boot).unwrap(),
            Some(Boot::Pci {
                owner: spec.id.clone(),
                count: 1
            })
        );
        assert!(!boot.contains("79:00") && !boot.contains(&(0x79_u64 << 8).to_string()));
        for devices in [
            &[][..],
            &[passthrough("pci_7901", "0000:79:00.1", false)][..],
        ] {
            assert_eq!(Boot::parse(&boot_line(&spec, devices)).unwrap(), None);
        }
    }

    /// AC-T7.4.1: do have the guest's kernel list a GPU function at an address its VMM's witness
    /// does not name (the right count, the wrong function); expect the binding check to refuse it.
    #[test]
    fn a_guest_gpu_outside_the_vmm_witness_is_refused() {
        let (host, witnessed, listed) = (gpu(0x79 << 8), gpu(3 << 3), gpu(4 << 3));
        let mut spec = spec();
        spec.devices = vec![host];
        let boot = boot_line(&spec, &[passthrough("pci_7900", "0000:79:00.0", true)]);
        let catalog = guest_catalog(&boot, &spec.id, &[attached("0000:00:04.0")]).unwrap();
        assert!(
            super::super::observations::check_bindings(
                &[super::super::observations::Assignment {
                    host,
                    guest: witnessed
                }],
                &guest_report(&[listed]),
                &guest_capacity(&catalog),
                Some(&spec.id),
            )
            .is_err()
        );
    }

    /// AC-T7.4.1: do pass two GPUs and have the guest serve both while the VMM witnesses only
    /// one; expect the binding check to refuse the GPU without a witness.
    #[test]
    fn a_missing_witness_entry_is_refused() {
        let hosts = [gpu(0x79 << 8), gpu(0x7a << 8)];
        let guests = [gpu(3 << 3), gpu(4 << 3)];
        let mut spec = spec();
        spec.devices = hosts.to_vec();
        let boot = boot_line(
            &spec,
            &[
                passthrough("pci_7900", "0000:79:00.0", true),
                passthrough("pci_7a00", "0000:7a:00.0", true),
            ],
        );
        let catalog = guest_catalog(
            &boot,
            &spec.id,
            &[attached("0000:00:03.0"), attached("0000:00:04.0")],
        )
        .unwrap();
        let (capacity, report) = (guest_capacity(&catalog), guest_report(&guests));
        let check = |witnessed: &[super::super::observations::Assignment]| {
            super::super::observations::check_bindings(
                witnessed,
                &report,
                &capacity,
                Some(&spec.id),
            )
        };
        let both = [
            super::super::observations::Assignment {
                host: hosts[0],
                guest: guests[0],
            },
            super::super::observations::Assignment {
                host: hosts[1],
                guest: guests[1],
            },
        ];
        check(&both).unwrap();
        assert!(check(&both[..1]).is_err());
    }

    fn json() -> serde_json::Value {
        serde_json::json!({
            "binary": "/private/firecracker", "binary_sha256": "02".repeat(32),
            "kernel": "/private/vmlinux", "initrd": "/private/initrd",
            "state_dir": "/private/owned", "pool_id": "one-physical-host",
            "boot_ms": 3000, "stop_ms": 1000, "operation_ms": 1000,
            "cost_evidence": "/private/cost-evidence", "cost": cost(),
            "staging": {"entries": 10, "bytes": 1048576, "path": 100}
        })
    }

    /// AC-T7.4.1: one owned grammar requires every input and rejects unknown/duplicate fields.
    #[test]
    fn explicit_config_parser_has_no_default_budget_or_ambiguous_field() {
        let original = json();
        let bytes = serde_json::to_vec(&original).unwrap();
        let parsed = Config::parse(&bytes).unwrap();
        assert_eq!(parsed.cost, cost());
        assert_eq!(parsed.operation_timeout, Duration::from_secs(1));
        for key in original.as_object().unwrap().keys() {
            let mut absent = original.clone();
            absent.as_object_mut().unwrap().remove(key);
            assert!(
                Config::parse(&serde_json::to_vec(&absent).unwrap()).is_err(),
                "missing {key}"
            );
        }
        let duplicate =
            String::from_utf8(bytes.clone())
                .unwrap()
                .replacen('{', "{\"boot_ms\":1,", 1);
        assert!(Config::parse(duplicate.as_bytes()).is_err());
        let duplicate = String::from_utf8(bytes)
            .unwrap()
            .replace("\"entries\":10", "\"entries\":1,\"entries\":10");
        assert!(Config::parse(duplicate.as_bytes()).is_err());
        let mut unknown = original.clone();
        unknown["unrecognized"] = serde_json::json!(1);
        assert!(Config::parse(&serde_json::to_vec(&unknown).unwrap()).is_err());
        unknown = original.clone();
        unknown["staging"]["unrecognized"] = serde_json::json!(1);
        assert!(Config::parse(&serde_json::to_vec(&unknown).unwrap()).is_err());
        for key in ["boot_ms", "stop_ms", "operation_ms"] {
            for value in [0u64, u64::from(u32::MAX) + 1] {
                let mut invalid = original.clone();
                invalid[key] = serde_json::json!(value);
                assert!(Config::parse(&serde_json::to_vec(&invalid).unwrap()).is_err());
            }
        }
        let mut invalid = original;
        invalid["binary_sha256"] = serde_json::json!("private-invalid-pin");
        let error = Config::parse(&serde_json::to_vec(&invalid).unwrap()).unwrap_err();
        assert!(!error.to_string().contains("private-invalid-pin"));
    }

    /// AC-T7.4.1: configuration bytes are bounded and exact pinned inputs are checked before use.
    #[test]
    fn file_config_and_cost_inputs_refuse_changed_or_oversized_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, serde_json::to_vec(&json()).unwrap()).unwrap();
        let mut config = Config::from_file(&path).unwrap();
        config.binary = dir.path().join("binary");
        config.cost_evidence = dir.path().join("cost");
        std::fs::write(&config.binary, b"fixture-executable-bytes").unwrap();
        std::fs::write(&config.cost_evidence, b"fixture-evidence-bytes-unmeasured").unwrap();
        config.binary_sha256 = Sha256::digest(b"fixture-executable-bytes").into();
        config.cost.id = Sha256::digest(b"fixture-evidence-bytes-unmeasured").into();
        config.verify_inputs().unwrap();
        std::fs::write(&config.binary, b"changed").unwrap();
        assert!(config.verify_inputs().is_err());
        std::fs::write(&config.binary, b"fixture-executable-bytes").unwrap();
        std::fs::write(&config.cost_evidence, b"changed").unwrap();
        assert!(config.verify_inputs().is_err());
        std::fs::write(&path, vec![b' '; hex_core::bundle::MAX_CONFIG_BYTES + 1]).unwrap();
        assert!(Config::from_file(&path).is_err());
    }

    /// AC-T7.4.1 (original: r690's guest amdgpu said "Unable to locate a BIOS ROM" for 79:00.0, an
    /// integrated GPU whose image only its platform's VFCT carries). Do: configure a video BIOS image
    /// for one of two passthrough functions and write the VM's configuration. Expect that function's
    /// entry to name the image for its VMM (vendor patch 0004's VFCT), the other's to name none.
    #[test]
    fn original_a_configured_vbios_image_goes_to_its_functions_vmm_entry_alone() {
        let mut value = json();
        value["vbios"] = serde_json::json!([{
            "sbdf": "0000:79:00.0", "path": "/private/vbios-1002-13c0.rom", "sha256": "03".repeat(32)
        }]);
        let config = Config::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            config.vbios,
            vec![Vbios {
                sbdf: "0000:79:00.0".into(),
                path: "/private/vbios-1002-13c0.rom".into(),
                sha256: [3; 32],
            }]
        );
        let devices = [
            passthrough("pci_7900", "0000:79:00.0", true),
            passthrough("pci_8100", "0000:81:00.0", true),
        ];
        let vm = serde_json::to_value(
            config
                .machine(&spec(), Path::new("/private/owned/vsock"), &devices)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            vm["device-passthrough"],
            serde_json::json!([
                {"id": "pci_7900", "sbdf": "0000:79:00.0", "vbios_path": "/private/vbios-1002-13c0.rom"},
                {"id": "pci_8100", "sbdf": "0000:81:00.0"},
            ])
        );
    }

    /// AC-T7.4.1: an image is named canonically, once a function, by an absolute path and an exact
    /// pin; at launch it must be there and be that pin's bytes. Do: parse malformed entries; then
    /// verify a well-formed one whose file is absent, then holds other bytes, then its own. Expect each
    /// malformed entry refused; the absent file refused as the OS's, the other bytes refused as a pin
    /// mismatch naming the function, and its own bytes verified.
    #[test]
    fn a_vbios_image_malformed_absent_or_off_its_pin_is_refused_by_type() {
        let entry = |sbdf: &str, path: &str, sha256: &str| serde_json::json!({"sbdf": sbdf, "path": path, "sha256": sha256});
        let pin = "03".repeat(32);
        for images in [
            serde_json::json!([entry("79:00.0", "/private/rom", &pin)]),
            serde_json::json!([entry("0000:79:0.0", "/private/rom", &pin)]),
            serde_json::json!([entry("0000:79:00.0", "relative/rom", &pin)]),
            serde_json::json!([entry("0000:79:00.0", "/private/rom", "not-a-pin")]),
            serde_json::json!([
                entry("0000:79:00.0", "/private/rom", &pin),
                entry("0000:79:00.0", "/private/other", &pin)
            ]),
            serde_json::json!([{"sbdf": "0000:79:00.0", "path": "/private/rom"}]),
            serde_json::json!([{"sbdf": "0000:79:00.0", "path": "/private/rom", "sha256": pin, "x": 1}]),
        ] {
            let mut value = json();
            value["vbios"] = images.clone();
            assert!(
                Config::parse(&serde_json::to_vec(&value).unwrap()).is_err(),
                "{images}"
            );
        }

        let dir = tempfile::tempdir().unwrap();
        let mut config = config();
        config.binary = dir.path().join("binary");
        config.cost_evidence = dir.path().join("cost");
        std::fs::write(&config.binary, b"fixture-executable-bytes").unwrap();
        std::fs::write(&config.cost_evidence, b"fixture-evidence-bytes-unmeasured").unwrap();
        config.binary_sha256 = Sha256::digest(b"fixture-executable-bytes").into();
        config.cost.id = Sha256::digest(b"fixture-evidence-bytes-unmeasured").into();
        let rom = dir.path().join("vbios.rom");
        config.vbios = vec![Vbios {
            sbdf: "0000:79:00.0".into(),
            path: rom.clone(),
            sha256: Sha256::digest(b"\x55\xaa image").into(),
        }];
        assert!(matches!(
            config.verify_inputs(),
            Err(Refusal::Os {
                op: "opening pinned Firecracker input",
                ..
            })
        ));
        std::fs::write(&rom, b"\x55\xaa other").unwrap();
        assert!(matches!(
            config.verify_inputs(),
            Err(Refusal::Invalid { why, .. }) if why.contains("video BIOS image for 0000:79:00.0")
        ));
        std::fs::write(&rom, b"\x55\xaa image").unwrap();
        config.verify_inputs().unwrap();
    }
}
