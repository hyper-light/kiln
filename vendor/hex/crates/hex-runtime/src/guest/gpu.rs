//! The dedicated VM's boot authority and the guest GPU catalog it grants. Only host-created
//! kernel parameters grant this mapping; a box payload, environment or CDI label
//! cannot assert that an arbitrary character device is a selected physical GPU.
//! A Shards VM's words name the host's exact keys, which its virtio render nodes serve. A
//! Firecracker VM's words name only its owner and how many PCI GPU functions its VMM attached:
//! each function's key is the guest's own PCI identity, which the host joins to its own through
//! the VMM's attachment witness (`firecracker::observations`).

#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};

use hex_api::{BoxId, BoxSpec, DeviceId, DeviceKind, Refusal};

/// Linux COMMAND_LINE_SIZE on supported x86_64/arm64, including its terminating
/// byte (arch/{x86,arm64}/include/{asm,uapi/asm}/setup.h).
pub const CMDLINE_BYTES: usize = 2048;
pub const CDI_KIND: &str = "hex.shards/gpu";
/// A Firecracker guest's own PCI GPUs, as a CDI 1.1 `vendor/class` kind.
pub const PCI_CDI_KIND: &str = "hex.firecracker/gpu";

/// Writes within the kernel's whole command-line budget, which `reserve` takes before formatting.
/// A failed write cannot grow storage or launch a partially authorized guest.
struct Bounded<'a>(&'a mut String);

impl std::fmt::Write for Bounded<'_> {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        if self
            .0
            .len()
            .checked_add(value.len())
            .is_none_or(|len| len >= CMDLINE_BYTES)
        {
            return Err(std::fmt::Error);
        }
        self.0.push_str(value);
        Ok(())
    }
}

fn reserve(cmdline: &mut String) -> Result<Bounded<'_>, Refusal> {
    if Boot::parse(cmdline)?.is_some() || cmdline.split_whitespace().any(|word| word == "--") {
        return Err(invalid(
            "base command line already contains boot authority or init arguments",
        ));
    }
    let room = CMDLINE_BYTES
        .checked_sub(cmdline.len())
        .ok_or_else(|| invalid("base command line is oversized"))?;
    cmdline
        .try_reserve_exact(room)
        .map_err(|_| Refusal::Exhausted {
            what: "guest kernel command line",
            bound: CMDLINE_BYTES,
        })?;
    Ok(Bounded(cmdline))
}

/// Firecracker's words: the VM's owner and how many PCI GPU functions its VMM attaches, never
/// their identities, which the VMM assigns as it attaches them.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn append_pci_authority(
    cmdline: &mut String,
    owner: &BoxId,
    count: usize,
) -> Result<(), Refusal> {
    use std::fmt::Write as _;
    let mut text = reserve(cmdline)?;
    write!(text, " hex.gpu.box={owner} hex.gpu.pci={count}")
        .map_err(|_| invalid("GPU boot authority exceeds kernel command line"))?;
    match Boot::parse(text.0)? {
        Some(Boot::Pci {
            owner: parsed,
            count: counted,
        }) if parsed == *owner && counted == count => Ok(()),
        _ => Err(invalid(
            "GPU boot authority is not the exact canonical grant",
        )),
    }
}

pub(super) fn append_authority(
    cmdline: &mut String,
    owner: &BoxId,
    devices: &[DeviceId],
) -> Result<(), Refusal> {
    use std::fmt::Write as _;
    let mut text = reserve(cmdline)?;
    write!(text, " hex.gpu.box={owner} hex.gpu=")
        .map_err(|_| invalid("GPU boot authority exceeds kernel command line"))?;
    for (index, device) in devices.iter().enumerate() {
        if index != 0 {
            text.write_char(',')
                .map_err(|_| invalid("GPU boot authority exceeds kernel command line"))?;
        }
        write!(text, "{}", device.key)
            .map_err(|_| invalid("GPU boot authority exceeds kernel command line"))?;
    }
    let parsed = Authority::parse(text.0)?.ok_or_else(|| invalid("GPU boot authority is empty"))?;
    if parsed.owner != *owner || parsed.devices != devices {
        return Err(invalid(
            "GPU boot authority is not the exact canonical selection",
        ));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub struct Authority {
    pub owner: BoxId,
    pub devices: Vec<DeviceId>,
}

/// What the host's boot words grant this guest.
#[derive(Debug, PartialEq, Eq)]
pub enum Boot {
    /// Shards: virtio renderers serving the host's exact physical keys.
    Renderer(Authority),
    /// Firecracker: its VMM attached exactly `count` PCI GPU functions to `owner`'s VM.
    Pci { owner: BoxId, count: usize },
}

fn invalid(why: &'static str) -> Refusal {
    Refusal::Invalid {
        what: "guest GPU boot authority",
        why: why.into(),
    }
}

impl Boot {
    pub fn parse(cmdline: &str) -> Result<Option<Self>, Refusal> {
        if cmdline.len() >= CMDLINE_BYTES || cmdline.contains('\0') {
            return Err(invalid("kernel command line is oversized or contains NUL"));
        }
        let (mut owner, mut list, mut pci) = (None, None, None);
        for word in cmdline.split_whitespace().take_while(|word| *word != "--") {
            if let Some(value) = word.strip_prefix("hex.gpu.box=") {
                if owner.replace(value).is_some() {
                    return Err(invalid("repeated GPU box parameter"));
                }
            } else if let Some(value) = word.strip_prefix("hex.gpu.pci=") {
                if pci.replace(value).is_some() {
                    return Err(invalid("repeated PCI GPU count parameter"));
                }
            } else if let Some(value) = word.strip_prefix("hex.gpu=")
                && list.replace(value).is_some()
            {
                return Err(invalid("repeated GPU selection parameter"));
            }
        }
        match (owner, list, pci) {
            (None, None, None) => Ok(None),
            (Some(owner), Some(list), None) if !list.is_empty() => {
                Ok(Some(Self::Renderer(Authority {
                    owner: BoxId::new(owner)?,
                    devices: keys(list)?,
                })))
            }
            (Some(owner), None, Some(count)) => Ok(Some(Self::Pci {
                owner: BoxId::new(owner)?,
                count: pci_count(count)?,
            })),
            _ => Err(invalid(
                "GPU box and exactly one nonempty grant must appear together",
            )),
        }
    }
}

impl Boot {
    /// The guest's grant for `spec` at render, before any device effect: a Shards renderer's exact
    /// owner and set ([`Authority::check`]); a Firecracker VM's owner, with every PCI GPU its VMM
    /// attached.
    pub fn check(&self, spec: &BoxSpec) -> Result<(), Refusal> {
        match self {
            Self::Renderer(authority) => authority.check(spec),
            Self::Pci { owner, count } => {
                spec.check()?;
                let gpus = spec
                    .devices
                    .iter()
                    .filter(|device| device.kind == DeviceKind::Gpu)
                    .count();
                if spec.class.class() != hex_api::Class::Oci || spec.id != *owner || gpus != *count
                {
                    return Err(invalid(
                        "request disagrees with the Firecracker VM's owner or its attached GPUs",
                    ));
                }
                Ok(())
            }
        }
    }
}

fn keys(list: &str) -> Result<Vec<DeviceId>, Refusal> {
    let mut devices = Vec::new();
    for key in list.split(',') {
        if devices.len() >= hex_api::MAX_DEVICES {
            return Err(invalid("too many GPU identities"));
        }
        if key.is_empty()
            || !key.bytes().all(|byte| byte.is_ascii_digit())
            || (key.len() > 1 && key.starts_with('0'))
        {
            return Err(invalid("GPU key is not canonical unsigned decimal"));
        }
        let key = key
            .parse::<u64>()
            .map_err(|_| invalid("GPU key is not an unsigned native identity"))?;
        let id = DeviceId {
            kind: DeviceKind::Gpu,
            key,
        };
        if devices.last().is_some_and(|previous| *previous >= id) {
            return Err(invalid(
                "GPU identities are repeated or not in canonical order",
            ));
        }
        devices.try_reserve(1).map_err(|_| Refusal::Exhausted {
            what: "guest GPU identities",
            bound: hex_api::MAX_DEVICES,
        })?;
        devices.push(id);
    }
    Ok(devices)
}

/// A positive count in canonical decimal, within the scheduler's device index space.
fn pci_count(text: &str) -> Result<usize, Refusal> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) || text.starts_with('0') {
        return Err(invalid("PCI GPU count is not a canonical positive decimal"));
    }
    text.parse::<usize>()
        .ok()
        .filter(|count| *count <= hex_api::MAX_DEVICES)
        .ok_or_else(|| invalid("PCI GPU count exceeds the device index space"))
}

impl Authority {
    /// The Shards renderer grant on this line, if it carries one.
    pub fn parse(cmdline: &str) -> Result<Option<Self>, Refusal> {
        Ok(match Boot::parse(cmdline)? {
            Some(Boot::Renderer(authority)) => Some(authority),
            Some(Boot::Pci { .. }) | None => None,
        })
    }

    pub fn check(&self, spec: &BoxSpec) -> Result<(), Refusal> {
        spec.check()?;
        if spec.class.class() != hex_api::Class::Oci
            || spec.id != self.owner
            || spec.devices.len() != self.devices.len()
            || spec.devices.iter().any(|id| !self.devices.contains(id))
        {
            return Err(invalid(
                "request disagrees with the dedicated VM's exact owner or physical set",
            ));
        }
        Ok(())
    }
}

/// The Shards renderer grant on this guest's kernel command line, if it carries one.
#[cfg(target_os = "linux")]
pub fn boot_authority() -> Result<Option<Authority>, Refusal> {
    Ok(match boot()? {
        Some(Boot::Renderer(authority)) => Some(authority),
        Some(Boot::Pci { .. }) | None => None,
    })
}

#[cfg(target_os = "linux")]
pub fn boot() -> Result<Option<Boot>, Refusal> {
    use std::io::Read;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(CMDLINE_BYTES)
        .map_err(|_| Refusal::Exhausted {
            what: "guest kernel command line",
            bound: CMDLINE_BYTES,
        })?;
    bytes.resize(CMDLINE_BYTES, 0);
    let mut file = std::fs::File::open("/proc/cmdline")
        .map_err(|e| Refusal::os("opening guest kernel command line", e))?;
    let mut used = 0usize;
    while used < CMDLINE_BYTES {
        let tail = bytes
            .get_mut(used..)
            .ok_or_else(|| invalid("kernel command line length overflows"))?;
        let count = file
            .read(tail)
            .map_err(|e| Refusal::os("reading guest kernel command line", e))?;
        if count == 0 {
            break;
        }
        used = used
            .checked_add(count)
            .ok_or_else(|| invalid("kernel command line length overflows"))?;
    }
    let text = std::str::from_utf8(
        bytes
            .get(..used)
            .ok_or_else(|| invalid("invalid command line length"))?,
    )
    .map_err(|_| invalid("kernel command line is not UTF-8"))?;
    Boot::parse(text)
}

#[cfg(target_os = "linux")]
fn path_join(base: &Path, tail: &str) -> Result<PathBuf, Refusal> {
    let bytes = base
        .as_os_str()
        .len()
        .checked_add(tail.len())
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| invalid("native GPU path length overflows"))?;
    let mut path = PathBuf::new();
    path.try_reserve_exact(bytes)
        .map_err(|_| invalid("native GPU path storage unavailable"))?;
    path.push(base);
    path.push(tail);
    Ok(path)
}

/// Revalidate a resolved CDI node through the kernel's device identity, not its
/// path name or a copied ordinal. Used only with a matching dedicated authority.
#[cfg(target_os = "linux")]
pub fn check_render_node(major: u32, minor: u32) -> Result<(), Refusal> {
    use std::fmt::Write as _;
    let mut text = String::new();
    // Two u32 decimal components and one separator, not a tuned path allowance.
    let bytes = "/sys/dev/char/"
        .len()
        .checked_add("4294967295:4294967295".len())
        .ok_or_else(|| invalid("native char device path length overflows"))?;
    text.try_reserve_exact(bytes)
        .map_err(|_| invalid("native char device path storage unavailable"))?;
    write!(text, "/sys/dev/char/{major}:{minor}")
        .map_err(|_| invalid("native char device path cannot be encoded"))?;
    let base = PathBuf::from(text);
    let subsystem = std::fs::canonicalize(path_join(&base, "subsystem")?)
        .map_err(|e| Refusal::os("resolving guest render node subsystem", e))?;
    let driver = std::fs::canonicalize(path_join(&base, "device/driver")?)
        .map_err(|e| Refusal::os("resolving guest render node driver", e))?;
    if subsystem.file_name().is_none_or(|name| name != "drm")
        || driver.file_name().is_none_or(|name| name != "virtio_gpu")
    {
        return Err(invalid(
            "selected guest node is not the virtio GPU DRM renderer",
        ));
    }
    let device =
        std::fs::canonicalize(&base).map_err(|e| Refusal::os("resolving guest DRM node", e))?;
    let render = device
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("renderD"));
    if render.is_none_or(|suffix| suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(invalid("selected guest node is not a DRM render node"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
struct Node {
    path: String,
    major: u32,
    minor: u32,
}

struct Text(String);

impl Text {
    fn new() -> Result<Self, Refusal> {
        let mut text = String::new();
        text.try_reserve_exact(hex_core::bundle::MAX_CONFIG_BYTES)
            .map_err(|_| Refusal::Exhausted {
                what: "guest GPU configuration",
                bound: hex_core::bundle::MAX_CONFIG_BYTES,
            })?;
        Ok(Self(text))
    }
}

impl std::fmt::Write for Text {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        if self
            .0
            .len()
            .checked_add(value.len())
            .is_none_or(|len| len > hex_core::bundle::MAX_CONFIG_BYTES)
        {
            return Err(std::fmt::Error);
        }
        self.0.push_str(value);
        Ok(())
    }
}

/// One PCI function of display (0x03) or processing-accelerator (0x12) base class, the classes the
/// host's VFIO plan counts as GPU functions, that a Firecracker guest's kernel lists, with the DRM
/// nodes its driver made for it (none when no driver bound it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PciFunction {
    pub address: hex_gpu::PciAddress,
    pub nodes: Vec<DrmNode>,
}

/// A DRM character device under `/dev/dri`, named as the kernel names it (`card0`, `renderD128`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrmNode {
    pub name: String,
    pub major: u32,
    pub minor: u32,
}

/// The `hex-devices-1` catalog and the CDI 1.1 spec a Firecracker guest's PID 1 writes.
#[derive(Debug, PartialEq, Eq)]
pub struct PciCatalog {
    pub catalog: String,
    pub cdi: String,
}

fn drm_name(name: &str) -> bool {
    name.strip_prefix("renderD")
        .or_else(|| name.strip_prefix("card"))
        .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

/// What a Firecracker guest serves of the PCI GPUs its VMM attached: every such function its
/// kernel lists, keyed by the guest's own PCI identity, with its own DRM nodes and its native
/// facts' model and memory. The count the boot words carry must match exactly: a function the VMM
/// did not attach, or one this kernel lost, refuses rather than serving a subset.
pub fn pci_catalog(
    count: usize,
    functions: &[PciFunction],
    facts: &[hex_gpu::Gpu],
    cdi_file: &str,
) -> Result<PciCatalog, Refusal> {
    use std::fmt::Write as _;
    let encode = |_| invalid("guest PCI GPU catalog exceeds native config bytes");
    let full = |_| Refusal::Exhausted {
        what: "guest PCI GPU functions",
        bound: hex_api::MAX_DEVICES,
    };
    if functions.len() != count {
        return Err(invalid(
            "the guest lists another number of PCI GPU functions than its VMM attached",
        ));
    }
    if !cdi_file.starts_with('/') || cdi_file.contains(['\t', '\n', '\r', '\0', '"', '\\']) {
        return Err(invalid("guest catalog path is not an absolute flat field"));
    }
    let mut ordered = Vec::new();
    ordered.try_reserve_exact(functions.len()).map_err(full)?;
    ordered.extend(functions);
    ordered.sort_unstable_by_key(|function| function.address);
    if ordered
        .windows(2)
        .any(|pair| pair.first().map(|f| f.address) == pair.last().map(|f| f.address))
    {
        return Err(invalid("one PCI GPU function is listed twice"));
    }
    let mut catalog = Text::new()?;
    catalog.write_str("hex-devices-1\n").map_err(encode)?;
    let mut cdi = Text::new()?;
    write!(
        cdi,
        "{{\"cdiVersion\":\"1.1.0\",\"kind\":\"{PCI_CDI_KIND}\",\"devices\":["
    )
    .map_err(encode)?;
    for (index, function) in ordered.iter().enumerate() {
        let identity = hex_gpu::Identity::Pci(function.address);
        let mut found = facts.iter().filter(|gpu| gpu.identity == identity);
        let fact = match (found.next(), found.next()) {
            (Some(fact), None) => fact,
            (None, _) => return Err(invalid("an attached PCI GPU has no native facts")),
            (Some(_), Some(_)) => return Err(invalid("native facts name one PCI GPU twice")),
        };
        if fact.memory_total_bytes == 0 {
            return Err(invalid("an attached PCI GPU reports no memory"));
        }
        if fact.model.is_empty() || fact.model.contains(['\t', '\n', '\r', '\0']) {
            return Err(invalid(
                "an attached PCI GPU's model is not a flat catalog field",
            ));
        }
        let mut nodes = Vec::new();
        nodes
            .try_reserve_exact(function.nodes.len())
            .map_err(full)?;
        nodes.extend(&function.nodes);
        nodes.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        if !nodes.iter().all(|node| drm_name(&node.name))
            || nodes
                .windows(2)
                .any(|pair| pair.first().map(|n| &n.name) == pair.last().map(|n| &n.name))
        {
            return Err(invalid("a guest DRM node name is malformed or repeated"));
        }
        if !nodes.iter().any(|node| node.name.starts_with("renderD")) {
            return Err(invalid(
                "an attached PCI GPU has no DRM render node: no driver bound it",
            ));
        }
        let key = identity.key();
        writeln!(
            catalog,
            "gpu\t{key}\t{}\t{}\tcdi\t{cdi_file}\t{PCI_CDI_KIND}={key}",
            fact.model, fact.memory_total_bytes
        )
        .map_err(encode)?;
        if index != 0 {
            cdi.write_char(',').map_err(encode)?;
        }
        write!(
            cdi,
            "{{\"name\":\"{key}\",\"containerEdits\":{{\"deviceNodes\":["
        )
        .map_err(encode)?;
        for (index, node) in nodes.iter().enumerate() {
            if index != 0 {
                cdi.write_char(',').map_err(encode)?;
            }
            write!(
                cdi,
                "{{\"path\":\"/dev/dri/{}\",\"type\":\"c\",\"major\":{},\"minor\":{},\"permissions\":\"rw\"}}",
                node.name, node.major, node.minor
            )
            .map_err(encode)?;
        }
        cdi.write_str("]}}").map_err(encode)?;
    }
    cdi.write_str("]}").map_err(encode)?;
    check_written(&catalog.0, &cdi.0, ordered.len(), cdi_file)?;
    Ok(PciCatalog {
        catalog: catalog.0,
        cdi: cdi.0,
    })
}

/// The Node reads both back: each row must be a CDI binding to this spec that selects a device.
fn check_written(catalog: &str, cdi: &str, rows: usize, cdi_file: &str) -> Result<(), Refusal> {
    use crate::devices::{Binding, Catalog, Route};
    let parsed = Catalog::parse(catalog.as_bytes())?;
    let spec = hex_core::cdi::Spec::parse(cdi.as_bytes(), hex_core::bundle::MAX_CONFIG_BYTES)
        .map_err(|_| invalid("guest PCI GPU spec is not valid CDI"))?;
    if parsed.entries().len() != rows {
        return Err(invalid("guest PCI GPU catalog lost a row"));
    }
    for entry in parsed.entries() {
        match entry.binding(Route::Cdi) {
            Some(Binding::Cdi { file, qualified }) if file == cdi_file => {
                spec.select(&[qualified.as_str()], 1)
                    .map_err(|_| invalid("guest PCI GPU row selects no CDI device"))?;
            }
            _ => {
                return Err(invalid(
                    "guest PCI GPU row is not a binding to its own spec",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn render_nodes() -> Result<Vec<Node>, Refusal> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let mut nodes = Vec::new();
    let entries = std::fs::read_dir("/sys/class/drm")
        .map_err(|e| Refusal::os("listing guest DRM devices", e))?;
    for (seen, entry) in entries.enumerate() {
        if seen >= hex_api::MAX_DEVICES {
            return Err(invalid("guest DRM listing exceeds the device index space"));
        }
        let entry = entry.map_err(|e| Refusal::os("reading guest DRM entry", e))?;
        let name = entry.file_name();
        let Some(suffix) = name.to_str().and_then(|name| name.strip_prefix("renderD")) else {
            continue;
        };
        if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid("malformed DRM render node name"));
        }
        let mut path = String::new();
        path.try_reserve_exact(
            "/dev/dri/renderD"
                .len()
                .checked_add(suffix.len())
                .ok_or_else(|| invalid("render path length overflows"))?,
        )
        .map_err(|_| invalid("render path storage unavailable"))?;
        path.push_str("/dev/dri/renderD");
        path.push_str(suffix);
        let metadata =
            std::fs::metadata(&path).map_err(|e| Refusal::os("reading guest render node", e))?;
        if !metadata.file_type().is_char_device() {
            return Err(invalid("guest render path is not a character device"));
        }
        let major = rustix::fs::major(metadata.rdev());
        let minor = rustix::fs::minor(metadata.rdev());
        check_render_node(major, minor)?;
        nodes.try_reserve(1).map_err(|_| Refusal::Exhausted {
            what: "guest render nodes",
            bound: hex_api::MAX_DEVICES,
        })?;
        nodes.push(Node { path, major, minor });
    }
    if nodes.is_empty() {
        return Err(invalid(
            "dedicated GPU guest has no initialized virtio render node",
        ));
    }
    nodes.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    Ok(nodes)
}

/// A sysfs attribute shorter than `buffer`; a longer one is refused, never truncated.
#[cfg(target_os = "linux")]
fn read_small<'b>(path: &Path, buffer: &'b mut [u8]) -> Result<&'b str, Refusal> {
    use std::io::Read;
    let mut file =
        std::fs::File::open(path).map_err(|e| Refusal::os("opening a guest PCI attribute", e))?;
    let mut used = 0usize;
    while used < buffer.len() {
        let tail = buffer
            .get_mut(used..)
            .ok_or_else(|| invalid("guest attribute length overflows"))?;
        let count = file
            .read(tail)
            .map_err(|e| Refusal::os("reading a guest PCI attribute", e))?;
        if count == 0 {
            break;
        }
        used = used
            .checked_add(count)
            .ok_or_else(|| invalid("guest attribute length overflows"))?;
    }
    if used >= buffer.len() {
        return Err(invalid("guest PCI attribute exceeds its protocol width"));
    }
    let bytes = buffer
        .get(..used)
        .ok_or_else(|| invalid("guest attribute length overflows"))?;
    std::str::from_utf8(bytes).map_err(|_| invalid("guest PCI attribute is not UTF-8"))
}

/// The PCI GPU functions under `sys` (`/sys` in a guest), each with its DRM nodes. A Firecracker
/// guest's every PCI function is one its VMM attached or emulates.
#[cfg(target_os = "linux")]
pub fn pci_functions(sys: &Path) -> Result<Vec<PciFunction>, Refusal> {
    let devices = path_join(sys, "bus/pci/devices")?;
    let entries =
        std::fs::read_dir(&devices).map_err(|e| Refusal::os("listing guest PCI functions", e))?;
    let mut functions = Vec::new();
    for (seen, entry) in entries.enumerate() {
        if seen >= hex_api::MAX_DEVICES {
            return Err(invalid("guest PCI listing exceeds the device index space"));
        }
        let entry = entry.map_err(|e| Refusal::os("reading a guest PCI function", e))?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| invalid("guest PCI function name is not UTF-8"))?;
        let address = hex_gpu::PciAddress::parse(name)
            .ok_or_else(|| invalid("malformed guest PCI function name"))?;
        let function = path_join(&devices, name)?;
        // sysfs writes the 24-bit class as 0x, six hex digits and a newline; one more byte shows
        // an object that is not that attribute.
        let mut buffer = [0u8; "0xffffff\n".len() + 1];
        let class = read_small(&path_join(&function, "class")?, &mut buffer)?
            .trim()
            .strip_prefix("0x")
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .filter(|class| *class <= 0xff_ffff)
            .ok_or_else(|| invalid("malformed guest PCI class"))?;
        if !matches!(class >> 16, 0x03 | 0x12) {
            continue;
        }
        functions.try_reserve(1).map_err(|_| Refusal::Exhausted {
            what: "guest PCI GPU functions",
            bound: hex_api::MAX_DEVICES,
        })?;
        functions.push(PciFunction {
            address,
            nodes: drm_nodes(&function)?,
        });
    }
    Ok(functions)
}

#[cfg(target_os = "linux")]
fn drm_nodes(function: &Path) -> Result<Vec<DrmNode>, Refusal> {
    let drm = path_join(function, "drm")?;
    let entries = match std::fs::read_dir(&drm) {
        Ok(entries) => entries,
        // A function no driver bound has made no DRM nodes; the catalog refuses it by name.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Refusal::os("listing a guest GPU's DRM nodes", error)),
    };
    let mut nodes = Vec::new();
    for (seen, entry) in entries.enumerate() {
        if seen >= hex_api::MAX_DEVICES {
            return Err(invalid("guest DRM listing exceeds the device index space"));
        }
        let entry = entry.map_err(|e| Refusal::os("reading a guest GPU's DRM node", e))?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| invalid("guest DRM node name is not UTF-8"))?;
        if !drm_name(name) {
            continue;
        }
        // Two u32 decimal components, a separator and a newline, plus the byte that shows more.
        let mut buffer = [0u8; "4294967295:4294967295\n".len() + 1];
        let (major, minor) = read_small(&path_join(&path_join(&drm, name)?, "dev")?, &mut buffer)?
            .trim()
            .split_once(':')
            .and_then(|(major, minor)| Some((major.parse().ok()?, minor.parse().ok()?)))
            .ok_or_else(|| invalid("malformed guest DRM device number"))?;
        let mut owned = String::new();
        owned
            .try_reserve_exact(name.len())
            .map_err(|_| invalid("guest DRM node name storage unavailable"))?;
        owned.push_str(name);
        nodes.try_reserve(1).map_err(|_| Refusal::Exhausted {
            what: "guest DRM nodes",
            bound: hex_api::MAX_DEVICES,
        })?;
        nodes.push(DrmNode {
            name: owned,
            major,
            minor,
        });
    }
    Ok(nodes)
}

/// PID 1 calls this after mounting proc/sys/dev and before starting its node. The catalog it
/// writes is internal to this VM. A Shards VM's names the host keys its words carry, served by its
/// virtio render nodes, never by their ordinal; a Firecracker VM's names each PCI GPU function its
/// VMM attached by the guest's own PCI identity. CPU-only guests write no GPU catalog.
#[cfg(target_os = "linux")]
pub fn initialize(state: &Path) -> Result<Option<Boot>, Refusal> {
    let boot = boot()?;
    match &boot {
        None => {}
        Some(Boot::Renderer(authority)) => renderer_catalog(state, authority)?,
        Some(Boot::Pci { count, .. }) => {
            let functions = pci_functions(Path::new("/sys"))?;
            let gpus = hex_gpu::Gpus::open();
            let cdi = path_join(state, "firecracker-gpu.cdi")?;
            let file = cdi
                .to_str()
                .ok_or_else(|| invalid("guest catalog path is not UTF-8"))?;
            let written = pci_catalog(*count, &functions, gpus.inventory().unwrap_or(&[]), file)?;
            std::fs::create_dir_all(state)
                .map_err(|e| Refusal::os("creating guest node state", e))?;
            super::held::replace(&cdi, written.cdi.as_bytes())?;
            super::held::replace(&path_join(state, "devices")?, written.catalog.as_bytes())?;
        }
    }
    Ok(boot)
}

#[cfg(target_os = "linux")]
fn renderer_catalog(state: &Path, authority: &Authority) -> Result<(), Refusal> {
    use std::fmt::Write as _;
    let nodes = render_nodes()?;
    let cdi = path_join(state, "shards-gpu.cdi")?;
    let file = cdi
        .to_str()
        .filter(|path| path.starts_with('/') && !path.contains(['\t', '\n', '\r', '\0']))
        .ok_or_else(|| invalid("guest catalog path is not an absolute flat UTF-8 field"))?;
    let mut catalog = Text::new()?;
    catalog
        .write_str("hex-devices-1\n")
        .map_err(|_| invalid("guest GPU catalog exceeds native config bytes"))?;
    for id in &authority.devices {
        writeln!(
            catalog,
            "gpu\t{}\tshards-renderer\t0\tguest-gpu\t{file}\t{CDI_KIND}={}",
            id.key, id.key
        )
        .map_err(|_| invalid("cannot encode guest GPU catalog"))?;
    }
    let mut json = Text::new()?;
    json.write_str("{\"cdiVersion\":\"1.1.0\",\"kind\":\"hex.shards/gpu\",\"devices\":[")
        .map_err(|_| invalid("guest GPU CDI exceeds native config bytes"))?;
    for (index, id) in authority.devices.iter().enumerate() {
        if index != 0 {
            json.write_char(',')
                .map_err(|_| invalid("guest GPU CDI exceeds native config bytes"))?;
        }
        write!(
            json,
            "{{\"name\":\"{}\",\"containerEdits\":{{\"deviceNodes\":[",
            id.key
        )
        .map_err(|_| invalid("cannot encode GPU CDI"))?;
        for (index, node) in nodes.iter().enumerate() {
            if index != 0 {
                json.write_char(',')
                    .map_err(|_| invalid("guest GPU CDI exceeds native config bytes"))?;
            }
            write!(json, "{{\"path\":\"{}\",\"type\":\"c\",\"major\":{},\"minor\":{},\"permissions\":\"rw\"}}", node.path, node.major, node.minor)
                .map_err(|_| invalid("cannot encode GPU CDI node"))?;
        }
        json.write_str("]}}")
            .map_err(|_| invalid("guest GPU CDI exceeds native config bytes"))?;
    }
    json.write_str("]}")
        .map_err(|_| invalid("guest GPU CDI exceeds native config bytes"))?;
    std::fs::create_dir_all(state).map_err(|e| Refusal::os("creating guest node state", e))?;
    super::held::replace(&cdi, json.0.as_bytes())?;
    super::held::replace(&path_join(state, "devices")?, catalog.0.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-T7.4.1: only canonical boot parameters before -- bind one exact owner/set.
    #[test]
    fn boot_authority_is_exact_and_not_an_init_argument() {
        let parsed = Authority::parse(
            "console=ttyS0 hex.gpu.box=box-a hex.gpu=256,18446744073709551615 -- init",
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.owner, BoxId::new("box-a").unwrap());
        assert_eq!(
            parsed.devices.iter().map(|id| id.key).collect::<Vec<_>>(),
            [256, u64::MAX]
        );
        assert_eq!(
            Authority::parse("-- hex.gpu.box=foreign hex.gpu=1").unwrap(),
            None
        );
        for text in [
            "hex.gpu=1",
            "hex.gpu.box=a",
            "hex.gpu.box=a hex.gpu=",
            "hex.gpu.box=a hex.gpu=1,1",
            "hex.gpu.box=a hex.gpu=2,1",
            "hex.gpu.box=a hex.gpu=1 hex.gpu=2",
            "hex.gpu.box=a hex.gpu.box=b hex.gpu=1",
            "hex.gpu.box=../a hex.gpu=1",
            "hex.gpu.box=a hex.gpu=+1",
            "hex.gpu.box=a hex.gpu=01",
        ] {
            assert!(Authority::parse(text).is_err(), "{text}");
        }
    }

    /// AC-T7.4.1: a payload cannot forge another box/set; kernel overflow refuses
    /// before any VM effect, rather than truncating the selected authority.
    #[test]
    fn exact_owner_set_and_kernel_budget_are_enforced() {
        use hex_api::{Bounds, ClassSpec, Network, OciSpec, Program, Rootfs};
        let owner = BoxId::new("selected").unwrap();
        let devices = vec![DeviceId {
            kind: DeviceKind::Gpu,
            key: 256,
        }];
        let mut cmdline = "console=ttyS0".to_owned();
        append_authority(&mut cmdline, &owner, &devices).unwrap();
        let authority = Authority::parse(&cmdline).unwrap().unwrap();
        let spec = BoxSpec {
            id: owner,
            bounds: Bounds::default(),
            class: ClassSpec::Oci(Box::new(OciSpec {
                rootfs: Rootfs::Dir("/rootfs".into()),
                init: Program::of(&["/bin/true"]),
                mounts: Vec::new(),
                hostname: None,
                network: Network::Private,
                hosts: Vec::new(),
            })),
            owner: None,
            devices: devices.clone(),
            allocation: None,
            route: None,
            band: 0,
        };
        authority.check(&spec).unwrap();
        let mut wrong_class = spec.clone();
        wrong_class.class = ClassSpec::Process;
        assert!(authority.check(&wrong_class).is_err());
        let mut foreign = spec.clone();
        foreign.id = BoxId::new("foreign").unwrap();
        assert!(authority.check(&foreign).is_err());
        foreign.id = spec.id.clone();
        foreign.devices[0].key = 512;
        assert!(authority.check(&foreign).is_err());
        let mut too_long = "x".repeat(CMDLINE_BYTES - 1);
        assert!(append_authority(&mut too_long, &spec.id, &devices).is_err());
        assert!(too_long.len() < CMDLINE_BYTES);
        assert!(append_authority(&mut cmdline, &spec.id, &devices).is_err());
    }

    fn node(name: &str, minor: u32) -> DrmNode {
        DrmNode {
            name: name.into(),
            major: 226,
            minor,
        }
    }

    /// One function a Firecracker guest's kernel lists and the facts hex-gpu reads for it
    /// (synthetic values, not a measurement).
    fn attached(address: &str, nodes: &[(&str, u32)]) -> (PciFunction, hex_gpu::Gpu) {
        let address = hex_gpu::PciAddress::parse(address).unwrap();
        (
            PciFunction {
                address,
                nodes: nodes
                    .iter()
                    .map(|(name, minor)| node(name, *minor))
                    .collect(),
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

    const FILE: &str = "/run/hex/state/firecracker-gpu.cdi";

    fn catalog(
        count: usize,
        listed: &[(PciFunction, hex_gpu::Gpu)],
    ) -> Result<PciCatalog, Refusal> {
        let (functions, facts): (Vec<_>, Vec<_>) = listed.iter().cloned().unzip();
        pci_catalog(count, &functions, &facts, FILE)
    }

    /// AC-T7.4.1: a Firecracker VM's words grant only an owner and a canonical positive count;
    /// every other form, or both grants at once, refuses before the guest serves a GPU.
    #[test]
    fn pci_boot_words_are_exact_and_refuse_every_other_form() {
        let owner = BoxId::new("fc-box").unwrap();
        assert_eq!(
            Boot::parse("console=ttyS0 hex.gpu.box=fc-box hex.gpu.pci=2 -- init").unwrap(),
            Some(Boot::Pci {
                owner: owner.clone(),
                count: 2
            })
        );
        assert_eq!(
            Boot::parse("-- hex.gpu.box=fc-box hex.gpu.pci=1").unwrap(),
            None
        );
        assert_eq!(
            Authority::parse("hex.gpu.box=fc-box hex.gpu.pci=1").unwrap(),
            None
        );
        for text in [
            "hex.gpu.pci=1",
            "hex.gpu.box=a hex.gpu.pci=",
            "hex.gpu.box=a hex.gpu.pci=0",
            "hex.gpu.box=a hex.gpu.pci=01",
            "hex.gpu.box=a hex.gpu.pci=+1",
            "hex.gpu.box=a hex.gpu.pci=65537",
            "hex.gpu.box=a hex.gpu.pci=1 hex.gpu.pci=1",
            "hex.gpu.box=a hex.gpu=256 hex.gpu.pci=1",
            "hex.gpu.box=../a hex.gpu.pci=1",
        ] {
            assert!(Boot::parse(text).is_err(), "{text}");
        }
        let mut line = "console=ttyS0".to_owned();
        append_pci_authority(&mut line, &owner, 1).unwrap();
        assert_eq!(line, "console=ttyS0 hex.gpu.box=fc-box hex.gpu.pci=1");
        assert!(append_pci_authority(&mut line, &owner, 1).is_err());
        let one = [DeviceId {
            kind: DeviceKind::Gpu,
            key: 1,
        }];
        assert!(append_authority(&mut line, &owner, &one).is_err());
        assert!(append_pci_authority(&mut "console=ttyS0 -- init".to_owned(), &owner, 1).is_err());
        assert!(append_pci_authority(&mut "console=ttyS0".to_owned(), &owner, 0).is_err());
        let mut too_long = "x".repeat(CMDLINE_BYTES - 1);
        assert!(append_pci_authority(&mut too_long, &owner, 1).is_err());
        assert!(too_long.len() < CMDLINE_BYTES);
    }

    /// AC-T7.4.1: do catalog one function its VMM attached at 0000:00:03.0; expect one CDI row
    /// keyed by that guest PCI identity with its native model and memory, whose spec grants
    /// exactly that function's own DRM nodes, by name and number.
    #[test]
    fn firecracker_guest_catalogs_each_attached_function_by_its_own_pci_identity() {
        let written = catalog(
            1,
            &[attached(
                "0000:00:03.0",
                &[("renderD128", 128), ("card0", 0)],
            )],
        )
        .unwrap();
        assert_eq!(
            written.catalog,
            "hex-devices-1\ngpu\t24\t1002:13c0 rev e5\t536870912\tcdi\t\
             /run/hex/state/firecracker-gpu.cdi\thex.firecracker/gpu=24\n"
        );
        assert_eq!(
            written.cdi,
            concat!(
                "{\"cdiVersion\":\"1.1.0\",\"kind\":\"hex.firecracker/gpu\",\"devices\":[",
                "{\"name\":\"24\",\"containerEdits\":{\"deviceNodes\":[",
                "{\"path\":\"/dev/dri/card0\",\"type\":\"c\",\"major\":226,\"minor\":0,",
                "\"permissions\":\"rw\"},",
                "{\"path\":\"/dev/dri/renderD128\",\"type\":\"c\",\"major\":226,\"minor\":128,",
                "\"permissions\":\"rw\"}]}}]}"
            )
        );
        let parsed = crate::devices::Catalog::parse(written.catalog.as_bytes()).unwrap();
        let entry = &parsed.entries()[0];
        assert_eq!(
            (entry.info.id, entry.info.size),
            (
                DeviceId {
                    kind: DeviceKind::Gpu,
                    key: 3 << 3
                },
                512 << 20
            )
        );
        let spec = hex_core::cdi::Spec::parse(written.cdi.as_bytes(), written.cdi.len()).unwrap();
        let edits = spec.select(&["hex.firecracker/gpu=24"], 1).unwrap();
        assert_eq!(
            edits
                .device_nodes
                .iter()
                .map(|node| node.path.as_str())
                .collect::<Vec<_>>(),
            ["/dev/dri/card0", "/dev/dri/renderD128"]
        );
    }

    /// AC-T7.4.1: do catalog with a count unlike the functions listed, a function no driver
    /// bound, one without native facts, and malformed rows; expect each to refuse the whole
    /// catalog rather than serve a subset.
    #[test]
    fn a_count_mismatch_or_an_unready_function_refuses_the_whole_catalog() {
        let one = attached("0000:00:03.0", &[("card0", 0), ("renderD128", 128)]);
        let two = attached("0000:00:04.0", &[("card1", 1), ("renderD129", 129)]);
        catalog(1, std::slice::from_ref(&one)).unwrap();
        catalog(2, &[two.clone(), one.clone()]).unwrap();
        for (count, listed) in [
            (2, vec![one.clone()]),
            (1, vec![one.clone(), two.clone()]),
            (1, vec![]),
            (2, vec![one.clone(), one.clone()]),
        ] {
            assert!(
                catalog(count, &listed).is_err(),
                "{count} of {}",
                listed.len()
            );
        }
        let with_nodes = |nodes: Vec<DrmNode>| {
            (
                PciFunction {
                    address: one.0.address,
                    nodes,
                },
                one.1.clone(),
            )
        };
        for nodes in [
            Vec::new(),
            vec![node("card0", 0)],
            vec![node("renderD128", 128), node("renderD", 1)],
            vec![node("renderD128", 128), node("renderD128", 128)],
        ] {
            assert!(catalog(1, &[with_nodes(nodes)]).is_err());
        }
        assert!(pci_catalog(1, std::slice::from_ref(&one.0), &[], FILE).is_err());
        assert!(
            pci_catalog(
                1,
                std::slice::from_ref(&one.0),
                &[one.1.clone(), one.1.clone()],
                FILE
            )
            .is_err()
        );
        let mut empty = one.1.clone();
        empty.memory_total_bytes = 0;
        assert!(pci_catalog(1, std::slice::from_ref(&one.0), &[empty], FILE).is_err());
        let mut tabbed = one.1.clone();
        tabbed.model = "1002:13c0\trev".into();
        assert!(pci_catalog(1, std::slice::from_ref(&one.0), &[tabbed], FILE).is_err());
        assert!(
            pci_catalog(
                1,
                std::slice::from_ref(&one.0),
                std::slice::from_ref(&one.1),
                "relative.cdi"
            )
            .is_err()
        );
    }

    /// AC-T7.4.1: do list a guest's PCI functions from its sysfs; expect only display and
    /// accelerator functions, each with its own DRM nodes (none where no driver bound it), and an
    /// attribute wider than its kernel format refused rather than read in part.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_guest_lists_its_pci_gpu_functions_with_their_own_drm_nodes() {
        let sys = tempfile::tempdir().unwrap();
        let function = |name: &str, class: &str| {
            let dir = sys.path().join("bus/pci/devices").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("class"), class).unwrap();
            dir
        };
        function("0000:00:01.0", "0x078000\n");
        let gpu = function("0000:00:03.0", "0x030000\n");
        for (name, dev) in [("card0", "226:0\n"), ("renderD128", "226:128\n")] {
            std::fs::create_dir_all(gpu.join("drm").join(name)).unwrap();
            std::fs::write(gpu.join("drm").join(name).join("dev"), dev).unwrap();
        }
        function("0000:00:04.0", "0x120000\n");
        let mut listed = pci_functions(sys.path()).unwrap();
        listed.sort_unstable_by_key(|function| function.address);
        for function in &mut listed {
            function.nodes.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        }
        let address = |text| hex_gpu::PciAddress::parse(text).unwrap();
        assert_eq!(
            listed,
            [
                PciFunction {
                    address: address("0000:00:03.0"),
                    nodes: vec![node("card0", 0), node("renderD128", 128)],
                },
                PciFunction {
                    address: address("0000:00:04.0"),
                    nodes: Vec::new(),
                },
            ]
        );
        std::fs::write(gpu.join("drm/renderD128/dev"), "226:128:1\n").unwrap();
        assert!(pci_functions(sys.path()).is_err());
        std::fs::write(gpu.join("drm/renderD128/dev"), "226:128\n").unwrap();
        std::fs::write(gpu.join("class"), "0x0300000\n").unwrap();
        assert!(pci_functions(sys.path()).is_err());
    }

    /// AC-T7.4.1: original. A Firecracker guest's words name owner `fc-box` and one attached GPU,
    /// which its catalog keys 24. Do: grant GPUs at render to `fc-box`, to another box, to
    /// `fc-box` asking for another number, and to a process box; expect only the owner with the
    /// whole attached set to pass, as a Shards renderer's grant does.
    #[test]
    fn original_a_firecracker_guests_gpus_go_to_its_vm_owner_alone() {
        use hex_api::{Bounds, ClassSpec, Network, OciSpec, Program, Rootfs};
        let boot = Boot::parse("console=ttyS0 hex.gpu.box=fc-box hex.gpu.pci=1")
            .unwrap()
            .unwrap();
        let spec = |id: &str, keys: &[u64]| BoxSpec {
            id: BoxId::new(id).unwrap(),
            bounds: Bounds::default(),
            class: ClassSpec::Oci(Box::new(OciSpec {
                rootfs: Rootfs::Dir("/rootfs".into()),
                init: Program::of(&["/bin/true"]),
                mounts: Vec::new(),
                hostname: None,
                network: Network::Private,
                hosts: Vec::new(),
            })),
            owner: None,
            devices: keys
                .iter()
                .map(|key| DeviceId {
                    kind: DeviceKind::Gpu,
                    key: *key,
                })
                .collect(),
            allocation: None,
            route: None,
            band: 0,
        };
        boot.check(&spec("fc-box", &[24])).unwrap();
        assert!(boot.check(&spec("foreign", &[24])).is_err());
        assert!(boot.check(&spec("fc-box", &[24, 32])).is_err());
        let mut process = spec("fc-box", &[24]);
        process.class = ClassSpec::Process;
        assert!(boot.check(&process).is_err());
    }
}
