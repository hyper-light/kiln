//! AC-T7.4.1: a CDI name is configuration, never evidence of a GPU's PCI identity.
//! Linux sysfs device links and NVIDIA's per-PCI Device Minor report establish the mapping.

use super::*;
use std::io::Read;

fn invalid(why: &'static str) -> Refusal {
    Refusal::Invalid {
        what: "native GPU binding",
        why: why.into(),
    }
}

fn pci_key(pci: hex_gpu::PciAddress) -> u64 {
    (u64::from(pci.domain) << 16)
        | (u64::from(pci.bus) << 8)
        | (u64::from(pci.device) << 3)
        | u64::from(pci.function)
}

fn path_key(path: &Path) -> Option<u64> {
    path.ancestors().find_map(|part| {
        part.file_name()
            .and_then(|name| name.to_str())
            .and_then(hex_gpu::PciAddress::parse)
            .map(pci_key)
    })
}

/// The source driver writes exactly these fields in nv_procfs_read_gpu_info; no minor is guessed.
fn nvidia_minor(text: &str, expected: u64) -> Result<u32, Refusal> {
    let mut pci = None;
    let mut minor = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("Bus Location:") {
            if pci.is_some() {
                return Err(invalid("duplicate NVIDIA PCI identity field"));
            }
            pci = Some(
                hex_gpu::PciAddress::parse(value.trim())
                    .map(pci_key)
                    .ok_or_else(|| invalid("malformed NVIDIA PCI identity"))?,
            );
        } else if let Some(value) = line.strip_prefix("Device Minor:") {
            if minor.is_some() {
                return Err(invalid("duplicate NVIDIA device minor field"));
            }
            minor = Some(
                value
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| invalid("malformed NVIDIA device minor"))?,
            );
        }
    }
    if pci != Some(expected) {
        return Err(invalid("NVIDIA PCI report disagrees with its directory"));
    }
    minor.ok_or_else(|| invalid("NVIDIA PCI report has no device minor"))
}

fn bounded_text(path: &Path) -> Result<String, Refusal> {
    let bound = bundle::MAX_CONFIG_BYTES;
    let room = bound.checked_add(1).ok_or_else(|| full(bound))?;
    let mut file = fs::File::open(path)
        .map_err(|error| Refusal::os("opening a GPU identity report", error))?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(room).map_err(|_| full(room))?;
    bytes.resize(room, 0);
    let mut used = 0usize;
    while used < room {
        let tail = bytes.get_mut(used..).ok_or_else(|| full(room))?;
        let count = file
            .read(tail)
            .map_err(|error| Refusal::os("reading a GPU identity report", error))?;
        if count == 0 {
            break;
        }
        used = used.checked_add(count).ok_or_else(|| full(room))?;
    }
    if used > bound {
        return Err(full(bound));
    }
    bytes.truncate(used);
    String::from_utf8(bytes).map_err(|_| invalid("GPU identity report is not UTF-8"))
}

/// Temporary create input, not a holder ledger or telemetry cache. Enumerating at most the existing
/// physical device index space bounds procfs work; each report shares the native config byte ceiling.
fn nvidia_nodes() -> Result<Vec<(u64, u32, u32)>, Refusal> {
    let entries = match fs::read_dir("/proc/driver/nvidia/gpus") {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Refusal::os("listing NVIDIA PCI identity reports", error)),
    };
    let mut nodes = Vec::new();
    for (seen, entry) in entries.enumerate() {
        if seen >= hex_api::MAX_DEVICES {
            return Err(full(hex_api::MAX_DEVICES));
        }
        let entry = entry.map_err(|error| Refusal::os("listing NVIDIA PCI identities", error))?;
        let pci = entry
            .file_name()
            .to_str()
            .and_then(hex_gpu::PciAddress::parse)
            .ok_or_else(|| invalid("NVIDIA GPU directory is not a PCI identity"))?;
        let key = pci_key(pci);
        let minor = nvidia_minor(&bounded_text(&entry.path().join("information"))?, key)?;
        let path = format!("/dev/nvidia{minor}");
        let stat = rustix::fs::stat(path.as_str())
            .map_err(|error| Refusal::os("reading an NVIDIA GPU node identity", error))?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode)
            != rustix::fs::FileType::CharacterDevice
        {
            return Err(invalid("NVIDIA GPU object is not a character device"));
        }
        nodes
            .try_reserve(1)
            .map_err(|_| full(hex_api::MAX_DEVICES))?;
        nodes.push((
            key,
            rustix::fs::major(stat.st_rdev),
            rustix::fs::minor(stat.st_rdev),
        ));
    }
    Ok(nodes)
}

fn agree(sysfs: Option<u64>, nvidia: Option<u64>) -> Result<Option<u64>, Refusal> {
    match (sysfs, nvidia) {
        (Some(a), Some(b)) if a != b => Err(invalid("GPU identity interfaces disagree")),
        (Some(key), _) | (_, Some(key)) => Ok(Some(key)),
        (None, None) => Ok(None),
    }
}

fn check_claims(devices: &[DeviceId], nodes: &[Option<u64>]) -> Result<(), Refusal> {
    if nodes.iter().flatten().any(|key| {
        !devices
            .iter()
            .any(|device| device.kind == DeviceKind::Gpu && device.key == *key)
    }) {
        return Err(invalid(
            "CDI grants a GPU outside the reserved physical identities",
        ));
    }
    if devices
        .iter()
        .filter(|device| device.kind == DeviceKind::Gpu)
        .any(|device| !nodes.contains(&Some(device.key)))
    {
        return Err(invalid(
            "CDI has no native node for a reserved GPU identity",
        ));
    }
    Ok(())
}

pub(super) fn check(
    nodes: &[bundle::ResolvedNode<'_>],
    devices: &[DeviceId],
) -> Result<(), Refusal> {
    if !devices.iter().any(|device| device.kind == DeviceKind::Gpu) {
        return Ok(());
    }
    let nvidia = nvidia_nodes()?;
    let mut identities = Vec::new();
    identities
        .try_reserve_exact(nodes.len())
        .map_err(|_| full(nodes.len()))?;
    for node in nodes {
        if node.kind != DeviceType::Char {
            continue;
        }
        let path = format!("/sys/dev/char/{}:{}/device", node.major, node.minor);
        let sysfs = match fs::canonicalize(path) {
            Ok(path) => path_key(&path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(Refusal::os(
                    "resolving a selected device's PCI identity",
                    error,
                ));
            }
        };
        let mut nv_key = None;
        for (key, major, minor) in &nvidia {
            if (*major, *minor) == (node.major, node.minor) {
                nv_key = agree(nv_key, Some(*key))?;
            }
        }
        identities.push(agree(sysfs, nv_key)?);
    }
    // Shared vendor control nodes without a PCI function remain CDI operations. This verifies
    // exclusive GPU identity; it does not infer CUDA/ROCm capability or isolation from those nodes.
    check_claims(devices, &identities)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(key: u64) -> DeviceId {
        DeviceId {
            kind: DeviceKind::Gpu,
            key,
        }
    }

    /// AC-T7.4.1: a renamed CDI selector cannot grant the other physical GPU or an extra one.
    #[test]
    fn gpu_nodes_must_cover_exactly_the_reserved_physical_identities() {
        assert!(check_claims(&[gpu(256)], &[Some(256), None]).is_ok());
        assert!(check_claims(&[gpu(256)], &[Some(512), None]).is_err());
        assert!(check_claims(&[gpu(256)], &[Some(256), Some(512)]).is_err());
        assert!(check_claims(&[gpu(256)], &[None]).is_err());
        assert!(check_claims(&[gpu(256), gpu(512)], &[Some(512), Some(256)]).is_ok());
        assert!(check_claims(&[gpu(256), gpu(512)], &[Some(256)]).is_err());
    }

    /// AC-T7.4.1: closest PCI function wins over its bridge; no device filename supplies identity.
    #[test]
    fn sysfs_uses_the_resolved_device_function_and_interfaces_must_agree() {
        let pci = hex_gpu::PciAddress::parse("0000:03:00.1").unwrap();
        assert_eq!(
            path_key(Path::new(
                "/sys/devices/pci0000:00/0000:00:01.0/0000:03:00.1"
            )),
            Some(pci_key(pci))
        );
        assert_eq!(path_key(Path::new("/sys/devices/virtual/nvidia0")), None);
        assert_eq!(agree(Some(256), Some(256)).unwrap(), Some(256));
        assert!(agree(Some(256), Some(512)).is_err());
    }

    /// AC-T7.4.1: use the driver-reported minor only after independent PCI fields agree.
    #[test]
    fn nvidia_report_requires_one_matching_pci_and_minor() {
        let key = pci_key(hex_gpu::PciAddress::parse("0000:01:00.0").unwrap());
        assert_eq!(
            nvidia_minor(
                "Model: fixture\nBus Location: \t 0000:01:00.0\nDevice Minor: \t 7\n",
                key
            )
            .unwrap(),
            7
        );
        for text in [
            "Device Minor: 7\n",
            "Bus Location: 0000:02:00.0\nDevice Minor: 7\n",
            "Bus Location: 0000:01:00.0\nDevice Minor: 7\nDevice Minor: 8\n",
            "Bus Location: 0000:01:00.0\nDevice Minor: 4294967296\n",
        ] {
            assert!(nvidia_minor(text, key).is_err());
        }
    }
}
