//! AC-T7.3.1/AC-T7.4.1: native fixture discovery, independent of attachment validation.
//! Only ROOT runs the hardware consumer. See docs/hex-native-device-discovery/README.md.

use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

use hex_api::{DeviceId, DeviceKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Vendor {
    Nvidia,
    Amd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NativeGpu {
    pub vendor: Vendor,
    pub id: DeviceId,
    pub pci: String,
    pub model: String,
    pub memory_bytes: u64,
    pub node: PathBuf,
    pub major: u32,
    pub minor: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug)]
pub(super) enum DiscoveryError {
    Probe(hex_api::Refusal),
    Unavailable(String),
    Malformed(&'static str),
    Missing { what: &'static str, pci: String },
    Ambiguous(&'static str),
    Exhausted(&'static str),
    Io { path: PathBuf, error: io::Error },
    Cdi(hex_core::cdi::Error),
    Catalog(hex_api::Refusal),
    Json(serde_json::Error),
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Probe(why) | Self::Catalog(why) => write!(f, "native GPU fixture: {why}"),
            Self::Unavailable(why) => write!(f, "native GPU fixture unavailable: {why}"),
            Self::Malformed(what) => write!(f, "malformed native GPU {what}"),
            Self::Missing { what, pci } => write!(f, "native GPU {pci} has no {what}"),
            Self::Ambiguous(what) => write!(f, "ambiguous native GPU {what}"),
            Self::Exhausted(what) => write!(f, "native GPU fixture exhausted {what}"),
            Self::Io { path, error } => write!(f, "native GPU {}: {error}", path.display()),
            Self::Cdi(why) => write!(f, "native GPU CDI: {why}"),
            Self::Json(why) => write!(f, "native GPU fixture JSON: {why}"),
        }
    }
}

impl std::error::Error for DiscoveryError {}

fn io_at(path: &Path, error: io::Error) -> DiscoveryError {
    DiscoveryError::Io {
        path: path.to_owned(),
        error,
    }
}

// The same documented PCI shape hex-gpu exposes through Prober, with no direct new dependency.
// Domain32/bus8/device5/function3 is the existing hex-api physical-key representation.
fn pci_key(text: &str) -> Result<u64, DiscoveryError> {
    let bad = || DiscoveryError::Malformed("PCI address");
    let (domain, rest) = text.split_once(':').ok_or_else(bad)?;
    let (bus, rest) = rest.split_once(':').ok_or_else(bad)?;
    let (device, function) = rest.split_once('.').ok_or_else(bad)?;
    let hex = |value: &str, min, max| {
        if value.len() < min || value.len() > max || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad());
        }
        u32::from_str_radix(value, 16).map_err(|_| bad())
    };
    let (domain, bus, device, function) = (
        hex(domain, 1, 8)?,
        hex(bus, 2, 2)?,
        hex(device, 2, 2)?,
        hex(function, 1, 1)?,
    );
    if device >= 32 || function >= 8 {
        return Err(bad());
    }
    Ok((u64::from(domain) << 16)
        | (u64::from(bus) << 8)
        | (u64::from(device) << 3)
        | u64::from(function))
}

fn pci_text(key: u64) -> String {
    format!(
        "{:04x}:{:02x}:{:02x}.{:x}",
        key >> 16,
        (key >> 8) & 255,
        (key >> 3) & 31,
        key & 7
    )
}

fn decimal(text: &str) -> Result<u32, DiscoveryError> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(DiscoveryError::Malformed("decimal device number"));
    }
    text.parse()
        .map_err(|_| DiscoveryError::Malformed("decimal device number"))
}

fn dev_numbers(text: &str) -> Result<(u32, u32), DiscoveryError> {
    let (major, minor) = text
        .trim_end_matches('\n')
        .split_once(':')
        .ok_or(DiscoveryError::Malformed("sysfs dev numbers"))?;
    Ok((decimal(major)?, decimal(minor)?))
}

// NVIDIA e6a2e015 kernel-open/common/inc/nv-chardev-numbers.h: reserved GPU char major and
// regular-device minor range. These are ABI values; the actual minor is read, never its ordinal.
const NVIDIA_MAJOR: u32 = 195;
const NVIDIA_LAST_GPU_MINOR: u32 = 247;
// nv-procfs.c's two required printf records, at their full u32/PCI decimal/hex widths.
const INFO_LINE: usize = b"Bus Location: \t ffffffff:ff:1f.7\n".len();

fn nvidia_information(mut reader: impl BufRead, path: &Path) -> Result<(u64, u32), DiscoveryError> {
    let mut line = Vec::new();
    line.try_reserve_exact(INFO_LINE + 1)
        .map_err(|_| DiscoveryError::Exhausted("information line"))?;
    let (mut pci, mut minor) = (None, None);
    loop {
        line.clear();
        let read = (&mut reader)
            .take(
                u64::try_from(INFO_LINE + 1)
                    .map_err(|_| DiscoveryError::Exhausted("information line"))?,
            )
            .read_until(b'\n', &mut line)
            .map_err(|e| io_at(path, e))?;
        if read == 0 {
            break;
        }
        let required = line.starts_with(b"Bus Location:") || line.starts_with(b"Device Minor:");
        if !line.ends_with(b"\n") || line.len() > INFO_LINE {
            if required {
                return Err(DiscoveryError::Malformed("NVIDIA information line"));
            }
            // Irrelevant model/UUID/debug lines are streamed past, never retained or logged.
            if !line.ends_with(b"\n") {
                loop {
                    let bytes = reader.fill_buf().map_err(|e| io_at(path, e))?;
                    if bytes.is_empty() {
                        break;
                    }
                    let newline = bytes.iter().position(|b| *b == b'\n');
                    let used = newline.map_or(bytes.len(), |n| n + 1);
                    reader.consume(used);
                    if newline.is_some() {
                        break;
                    }
                }
            }
            continue;
        }
        if !required {
            continue;
        }
        let text = std::str::from_utf8(&line)
            .map_err(|_| DiscoveryError::Malformed("NVIDIA information encoding"))?;
        if let Some(value) = text.strip_prefix("Bus Location:") {
            if pci.replace(pci_key(value.trim())?).is_some() {
                return Err(DiscoveryError::Ambiguous("NVIDIA Bus Location"));
            }
        } else if let Some(value) = text.strip_prefix("Device Minor:") {
            let found = decimal(value.trim())?;
            if found > NVIDIA_LAST_GPU_MINOR {
                return Err(DiscoveryError::Malformed("NVIDIA regular-device minor"));
            }
            if minor.replace(found).is_some() {
                return Err(DiscoveryError::Ambiguous("NVIDIA Device Minor"));
            }
        }
    }
    Ok((
        pci.ok_or(DiscoveryError::Malformed("missing NVIDIA Bus Location"))?,
        minor.ok_or(DiscoveryError::Malformed("missing NVIDIA Device Minor"))?,
    ))
}

fn char_identity(
    character: bool,
    actual: (u32, u32),
    expected: (u32, u32),
) -> Result<(), DiscoveryError> {
    if !character || actual != expected {
        return Err(DiscoveryError::Malformed(
            "character rdev disagrees with driver mapping",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn stat_node(node: &Path, expected: (u32, u32)) -> Result<(u32, u32, u32), DiscoveryError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let metadata = fs::metadata(node).map_err(|e| io_at(node, e))?;
    char_identity(
        metadata.file_type().is_char_device(),
        (
            rustix::fs::major(metadata.rdev()),
            rustix::fs::minor(metadata.rdev()),
        ),
        expected,
    )?;
    Ok((metadata.mode() & 0o7777, metadata.uid(), metadata.gid()))
}

#[cfg(target_os = "linux")]
struct Mapping {
    key: u64,
    node: PathBuf,
    major: u32,
    minor: u32,
}

#[cfg(target_os = "linux")]
fn mappings(vendor: Vendor, wanted: &[u64]) -> Result<Vec<Mapping>, DiscoveryError> {
    let root = Path::new(match vendor {
        Vendor::Nvidia => "/proc/driver/nvidia/gpus",
        Vendor::Amd => "/sys/class/drm",
    });
    let mut rows = Vec::new();
    for (count, entry) in fs::read_dir(root).map_err(|e| io_at(root, e))?.enumerate() {
        if count >= hex_api::MAX_DEVICES {
            return Err(DiscoveryError::Exhausted("kernel directory address space"));
        }
        let entry = entry.map_err(|e| io_at(root, e))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(DiscoveryError::Malformed("kernel device name encoding"));
        };
        let (key, node, major, minor) = match vendor {
            Vendor::Nvidia => {
                let key = pci_key(name)?;
                if wanted.binary_search(&key).is_err() {
                    continue;
                }
                let path = entry.path().join("information");
                let file = fs::File::open(&path).map_err(|e| io_at(&path, e))?;
                let (reported, minor) =
                    nvidia_information(io::BufReader::with_capacity(INFO_LINE, file), &path)?;
                if reported != key {
                    return Err(DiscoveryError::Malformed(
                        "NVIDIA directory/Bus Location identity",
                    ));
                }
                (
                    key,
                    PathBuf::from(format!("/dev/nvidia{minor}")),
                    NVIDIA_MAJOR,
                    minor,
                )
            }
            Vendor::Amd => {
                let Some(number) = name.strip_prefix("renderD") else {
                    continue;
                };
                decimal(number)?;
                let device = entry.path().join("device");
                let target = fs::read_link(&device).map_err(|e| io_at(&device, e))?;
                let Some(pci) = target.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                let Ok(key) = pci_key(pci) else {
                    continue;
                }; // Non-PCI render devices are not the measured AMD PCI GPUs.
                if wanted.binary_search(&key).is_err() {
                    continue;
                }
                let path = entry.path().join("dev");
                // Two u32 decimal fields, colon and newline; never infer rdev from renderD's suffix.
                let bound = b"4294967295:4294967295\n".len();
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(bound + 1)
                    .map_err(|_| DiscoveryError::Exhausted("sysfs dev line"))?;
                fs::File::open(&path)
                    .map_err(|e| io_at(&path, e))?
                    .take(
                        u64::try_from(bound + 1)
                            .map_err(|_| DiscoveryError::Exhausted("sysfs dev line"))?,
                    )
                    .read_to_end(&mut bytes)
                    .map_err(|e| io_at(&path, e))?;
                if bytes.len() > bound {
                    return Err(DiscoveryError::Malformed("sysfs dev line length"));
                }
                let (major, minor) = dev_numbers(
                    std::str::from_utf8(&bytes)
                        .map_err(|_| DiscoveryError::Malformed("sysfs dev encoding"))?,
                )?;
                (key, Path::new("/dev/dri").join(name), major, minor)
            }
        };
        stat_node(&node, (major, minor))?;
        rows.try_reserve(1)
            .map_err(|_| DiscoveryError::Exhausted("kernel mappings"))?;
        rows.push(Mapping {
            key,
            node,
            major,
            minor,
        });
    }
    rows.sort_unstable_by_key(|row| row.key);
    if rows
        .windows(2)
        .any(|pair| matches!(pair, [a, b] if a.key == b.key))
    {
        return Err(DiscoveryError::Ambiguous("PCI-to-device mapping"));
    }
    Ok(rows)
}

#[cfg(target_os = "linux")]
pub(super) fn discover(dir: &Path) -> Result<Vec<NativeGpu>, DiscoveryError> {
    let facts = hex_node::facts::Prober::open(dir, hex_node::facts::Declared::default())
        .probe()
        .map_err(DiscoveryError::Probe)?;
    if facts.gpus.is_empty() {
        return Err(DiscoveryError::Unavailable(
            facts
                .no_gpus
                .unwrap_or_else(|| "no measured NVIDIA/AMD GPUs".into()),
        ));
    }
    if facts.gpus.len() > hex_api::MAX_DEVICES {
        return Err(DiscoveryError::Exhausted("GPU address space"));
    }
    let mut found = Vec::new();
    found
        .try_reserve_exact(facts.gpus.len())
        .map_err(|_| DiscoveryError::Exhausted("GPU facts"))?;
    for gpu in facts.gpus {
        let vendor = match gpu.vendor.as_str() {
            "nvidia" => Vendor::Nvidia,
            "amd" => Vendor::Amd,
            _ => return Err(DiscoveryError::Malformed("unsupported measured vendor")),
        };
        let pci = gpu
            .pci
            .ok_or(DiscoveryError::Malformed("missing measured PCI identity"))?;
        let id = DeviceId {
            kind: DeviceKind::Gpu,
            key: pci_key(&pci)?,
        };
        id.check().map_err(DiscoveryError::Catalog)?;
        found.push(NativeGpu {
            vendor,
            id,
            pci: pci_text(id.key),
            model: gpu.model,
            memory_bytes: gpu.memory,
            node: PathBuf::new(),
            major: 0,
            minor: 0,
            mode: 0,
            uid: 0,
            gid: 0,
        });
    }
    validate_ids(&found)?;
    for vendor in [Vendor::Nvidia, Vendor::Amd] {
        let mut wanted: Vec<_> = found
            .iter()
            .filter(|gpu| gpu.vendor == vendor)
            .map(|gpu| gpu.id.key)
            .collect();
        if wanted.is_empty() {
            continue;
        }
        wanted.sort_unstable();
        let rows = mappings(vendor, &wanted)?;
        for gpu in found.iter_mut().filter(|gpu| gpu.vendor == vendor) {
            let at = rows
                .binary_search_by_key(&gpu.id.key, |row| row.key)
                .map_err(|_| DiscoveryError::Missing {
                    what: "verified character device",
                    pci: gpu.pci.clone(),
                })?;
            let row = rows
                .get(at)
                .ok_or(DiscoveryError::Malformed("mapping position"))?;
            let (mode, uid, gid) = stat_node(&row.node, (row.major, row.minor))?;
            gpu.node = row.node.clone();
            gpu.major = row.major;
            gpu.minor = row.minor;
            gpu.mode = mode;
            gpu.uid = uid;
            gpu.gid = gid;
        }
    }
    let mut devices: Vec<_> = found.iter().map(|gpu| (gpu.major, gpu.minor)).collect();
    devices.sort_unstable();
    if devices.windows(2).any(|pair| pair.first() == pair.last()) {
        return Err(DiscoveryError::Ambiguous(
            "distinct PCI GPUs share a character rdev",
        ));
    }
    Ok(found)
}

fn validate_ids(gpus: &[NativeGpu]) -> Result<(), DiscoveryError> {
    if gpus.is_empty() || gpus.len() > hex_api::MAX_DEVICES {
        return Err(DiscoveryError::Malformed("fixture GPU count"));
    }
    let mut ids = Vec::new();
    ids.try_reserve_exact(gpus.len())
        .map_err(|_| DiscoveryError::Exhausted("GPU identity validation"))?;
    for gpu in gpus {
        gpu.id.check().map_err(DiscoveryError::Catalog)?;
        if gpu.id.kind != DeviceKind::Gpu || pci_key(&gpu.pci)? != gpu.id.key {
            return Err(DiscoveryError::Malformed("fixture GPU identity"));
        }
        ids.push(gpu.id);
    }
    ids.sort_unstable();
    if ids.windows(2).any(|pair| pair.first() == pair.last()) {
        return Err(DiscoveryError::Ambiguous("GPU identities"));
    }
    Ok(())
}

#[derive(Debug)]
pub(super) struct FixtureFiles {
    pub cdi: PathBuf,
    pub catalog: PathBuf,
}

fn fixture_bytes(
    cdi_path: &Path,
    gpus: &[NativeGpu],
) -> Result<(Vec<u8>, Vec<u8>), DiscoveryError> {
    validate_ids(gpus)?;
    let source = cdi_path
        .to_str()
        .filter(|_| cdi_path.is_absolute())
        .ok_or(DiscoveryError::Malformed("absolute UTF-8 CDI path"))?;
    if source.contains(['\t', '\n', '\r', '\0']) {
        return Err(DiscoveryError::Malformed("catalog source delimiter"));
    }
    let mut catalog = String::from("hex-devices-1\n");
    let mut devices = Vec::new();
    devices
        .try_reserve_exact(gpus.len())
        .map_err(|_| DiscoveryError::Exhausted("CDI devices"))?;
    for gpu in gpus {
        if gpu.model.contains(['\t', '\n', '\r', '\0']) {
            return Err(DiscoveryError::Malformed("catalog model delimiter"));
        }
        let node = gpu
            .node
            .to_str()
            .filter(|_| gpu.node.is_absolute())
            .ok_or(DiscoveryError::Malformed("absolute UTF-8 GPU node"))?;
        let name = format!("pci-{:012x}", gpu.id.key);
        // Read-only open proof, not CUDA/ROCm computation or shared-control-node isolation.
        devices.push(serde_json::json!({"name":name,"containerEdits":{"deviceNodes":[{"path":"/dev/hex-selected-gpu","hostPath":node,"type":"c","major":gpu.major,"minor":gpu.minor,"fileMode":gpu.mode,"uid":gpu.uid,"gid":gpu.gid,"permissions":"r"}]}}));
        use std::fmt::Write as _;
        writeln!(
            &mut catalog,
            "gpu\t{}\t{}\t{}\tcdi\t{}\thex.test/gpu={name}",
            gpu.id.key, gpu.model, gpu.memory_bytes, source
        )
        .map_err(|_| DiscoveryError::Exhausted("catalog formatting"))?;
    }
    // HostPath needs CDI >=0.5.0; no later-version edits or global grants are generated.
    let cdi = serde_json::to_vec(
        &serde_json::json!({"cdiVersion":"0.5.0","kind":"hex.test/gpu","devices":devices}),
    )
    .map_err(DiscoveryError::Json)?;
    hex_core::cdi::Spec::parse(&cdi, hex_core::bundle::MAX_CONFIG_BYTES)
        .map_err(DiscoveryError::Cdi)?;
    hex_runtime::devices::Catalog::parse(catalog.as_bytes()).map_err(DiscoveryError::Catalog)?;
    Ok((cdi, catalog.into_bytes()))
}

/// Caller supplies a new existing absolute private state directory. Existing files are never replaced.
#[cfg(target_os = "linux")]
pub(super) fn write_fixture(
    state: &Path,
    gpus: &[NativeGpu],
) -> Result<FixtureFiles, DiscoveryError> {
    if !state.is_absolute() || !state.is_dir() {
        return Err(DiscoveryError::Malformed(
            "existing absolute fixture directory",
        ));
    }
    for gpu in gpus {
        let (mode, uid, gid) = stat_node(&gpu.node, (gpu.major, gpu.minor))?;
        if (mode, uid, gid) != (gpu.mode, gpu.uid, gpu.gid) {
            return Err(DiscoveryError::Malformed(
                "GPU stat changed before publication",
            ));
        }
    }
    let paths = FixtureFiles {
        cdi: state.join("native-gpus.json"),
        catalog: state.join("devices"),
    };
    let (cdi, catalog) = fixture_bytes(&paths.cdi, gpus)?;
    for (path, bytes) in [(&paths.cdi, cdi), (&paths.catalog, catalog)] {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| io_at(path, e))?;
        file.write_all(&bytes).map_err(|e| io_at(path, e))?;
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_and_driver_numbers_are_checked_not_guessed_from_order() {
        assert_eq!(
            pci_key("00000000:09:1f.7").unwrap(),
            pci_key("0000:09:1f.7").unwrap()
        );
        assert_eq!(pci_key("ffffffff:ff:1f.7").unwrap(), (1u64 << 48) - 1);
        for bad in [
            "0000:09:20.0",
            "0000:09:00.8",
            "100000000:09:00.0",
            "0:9:00.0",
            "0000:09:00.0/extra",
        ] {
            assert!(pci_key(bad).is_err(), "{bad}");
        }
        let info = b"Model: ignored\nBus Location: \t 0000:09:00.0\nDevice Minor: \t 7\n";
        assert_eq!(
            nvidia_information(io::Cursor::new(info), Path::new("synthetic")).unwrap(),
            (pci_key("0000:09:00.0").unwrap(), 7)
        );
        for body in [
            "Bus Location: 0000:09:00.0\n",
            "Bus Location: 0000:09:00.0\nDevice Minor: 255\n",
            "Bus Location: 0000:09:00.0\nDevice Minor: +0\n",
            "Bus Location: 0000:09:00.0\nDevice Minor: 0\nDevice Minor: 1\n",
        ] {
            assert!(nvidia_information(io::Cursor::new(body), Path::new("synthetic")).is_err());
        }
        assert_eq!(dev_numbers("226:141\n").unwrap(), (226, 141));
        assert!(dev_numbers("226:141extra\n").is_err());
        assert!(char_identity(false, (195, 7), (195, 7)).is_err());
        assert!(char_identity(true, (1, 3), (195, 7)).is_err());
        assert!(char_identity(true, (195, 0), (195, 0)).is_ok());
    }

    #[test]
    fn synthetic_parser_control_emits_only_exact_selected_nodes_and_real_catalog_syntax() {
        let gpu = NativeGpu {
            vendor: Vendor::Nvidia,
            id: DeviceId {
                kind: DeviceKind::Gpu,
                key: pci_key("0000:09:00.0").unwrap(),
            },
            pci: "0000:09:00.0".into(),
            model: "synthetic parser model".into(),
            memory_bytes: 4096,
            node: "/dev/nvidia7".into(),
            major: 195,
            minor: 7,
            mode: 0o660,
            uid: 0,
            gid: 44,
        };
        let (cdi, catalog) = fixture_bytes(
            Path::new("/owned/private/native-gpus.json"),
            std::slice::from_ref(&gpu),
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&cdi).unwrap();
        let node = &json["devices"][0]["containerEdits"]["deviceNodes"][0];
        assert_eq!(node["hostPath"], "/dev/nvidia7");
        assert_eq!(node["path"], "/dev/hex-selected-gpu");
        assert_eq!(
            (node["major"].as_u64(), node["minor"].as_u64()),
            (Some(195), Some(7))
        );
        let parsed = hex_runtime::devices::Catalog::parse(&catalog).unwrap();
        assert_eq!(parsed.entries()[0].info.id, gpu.id);
        assert_eq!(parsed.entries()[0].info.model, gpu.model);
        assert_eq!(parsed.entries()[0].info.size, gpu.memory_bytes);
        assert!(
            fixture_bytes(
                Path::new("/owned/private/native-gpus.json"),
                &[gpu.clone(), gpu]
            )
            .is_err()
        );
    }
}
