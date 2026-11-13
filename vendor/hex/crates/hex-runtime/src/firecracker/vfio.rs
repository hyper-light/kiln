//! Native prerequisites for exact prebound PCI functions and IOMMU groups.
//! No driver mutation, module loading or host configuration occurs here.

use super::native::{path, text};
use super::{config::Passthrough, exhausted, invalid};
use crate::devices::{Binding, Catalog, Entry, Route};
use hex_api::{DeviceId, DeviceKind, Refusal};
use hex_core::record::ProcId;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Where the adapter reads the host's PCI functions and IOMMU groups, opens its VFIO interfaces and reads what a VMM it
/// starts may pin: the host's own `/sys`, `/dev`, open and process, or a test's stand-ins for them.
pub(super) struct Host {
    pub(super) sys: PathBuf,
    pub(super) dev: PathBuf,
    /// Opens a VFIO interface for reading and writing, answering as the kernel does: a group another process holds
    /// answers EBUSY (vfio's group open admits one opener at a time).
    pub(super) open: fn(&Path) -> std::io::Result<fs::File>,
    /// What a VMM this node starts pins its guest's RAM under, read as each VM given a device whole is created: this
    /// process's ([`Memlock::process`]), or a stand-in host's, whatever the machine's own (AC-T7.4.1). Read then, not
    /// once at the node's start: the VMM inherits the node's limit as it is at its launch, which another process may
    /// change while the node runs (prlimit(2)).
    pub(super) memlock: Box<dyn Fn() -> Result<Memlock, Refusal> + Send + Sync>,
}

impl Host {
    pub(super) fn system() -> Result<Self, Refusal> {
        Ok(Self {
            sys: path(Path::new("/"), "sys")?,
            dev: path(Path::new("/"), "dev")?,
            open: open_read_write,
            memlock: Box::new(Memlock::process),
        })
    }

    /// The RLIMIT_MEMLOCK the VMM of a VM given a device whole, its guest's RAM `needed` bytes, runs under, by this
    /// host's [`Memlock`] ([`dma_memlock`]). A VM given none maps no RAM for DMA, and its VMM runs under the node's own.
    pub(super) fn dma_memlock(
        &self,
        needed: u64,
    ) -> Result<Option<rustix::process::Rlimit>, Refusal> {
        let Memlock { limit, ipc_lock } = (self.memlock)()?;
        dma_memlock(needed, limit, ipc_lock)
    }

    /// `/sys/bus/pci/devices`: each PCI function's directory, named by its SBDF.
    fn functions(&self) -> Result<PathBuf, Refusal> {
        path(&self.sys, "bus/pci/devices")
    }

    /// `/dev/vfio/<name>`: the container (`vfio`) or an IOMMU group's interface.
    fn interface(&self, name: &str) -> Result<PathBuf, Refusal> {
        path(&path(&self.dev, "vfio")?, name)
    }
}

fn open_read_write(path: &Path) -> std::io::Result<fs::File> {
    OpenOptions::new().read(true).write(true).open(path)
}

/// The methods by which the kernel resets one PCI function alone (drivers/pci/pci.c at v7.3, `pci_reset_fn_methods`).
const FUNCTION_RESETS: [&str; 5] = ["device_specific", "acpi", "flr", "af_flr", "pm"];
/// Its methods that reset a function's whole bus.
const BUS_RESETS: [&str; 2] = ["bus", "cxl_bus"];
/// A sysfs attribute's text fits one page (`sysfs_emit`), and the x86_64 and aarch64 kernels' least is 4 KiB; the
/// kernel's list of reset methods is a few names within it.
const RESET_METHOD_BYTES: usize = 4096;

/// Whether vfio-pci's reset at a function's release clears it for its next VM, by the kernel's rules at v7.3. It resets
/// the function by its first method that works (pci.c `__pci_reset_function_locked`, over its `reset_method`): one of
/// its own resets it alone; a bus reset refuses wherever another device is on its bus (`pci_parent_bus_reset`), though
/// the kernel lists `bus` for a bus's first function, probed before its siblings were added (`pci_init_reset_methods`).
/// vfio-pci resets a bus whole at its last release only where every device on it is vfio-pci's and released
/// (vfio_pci_core.c `vfio_pci_dev_set_resettable`, `vfio_pci_dev_set_needs_reset`). `methods` is the function's
/// `reset_method` text, `bus_reset` whether a function of its VM on its bus lists a bus reset, and `shared` whether a
/// device on its bus goes to no VM with it.
fn cleared(methods: &str, bus_reset: bool, shared: bool) -> bool {
    methods
        .split_ascii_whitespace()
        .any(|method| FUNCTION_RESETS.contains(&method))
        || (bus_reset && !shared)
}

/// The bus a canonical SBDF names: its `domain:bus`, the text before its last colon.
fn bus(sbdf: &str) -> Option<&str> {
    sbdf.rsplit_once(':').map(|(bus, _)| bus)
}

/// A function's `reset_method` text, as the kernel lists it in the function's sysfs directory `function`: empty where it
/// has no method to list, as the kernel then hides the file (pci-sysfs.c `reset_method_attr_is_visible`).
fn reset_methods(function: &Path) -> Result<String, Refusal> {
    let file = match fs::File::open(path(function, "reset_method")?) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(Refusal::os("reading a PCI function's reset methods", e)),
    };
    let bound = RESET_METHOD_BYTES
        .checked_add(1)
        .ok_or_else(|| invalid("reset method bound overflows"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(bound)
        .map_err(|_| exhausted("PCI reset method bytes", bound))?;
    let limit = u64::try_from(bound).map_err(|_| invalid("reset method bound overflows"))?;
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|e| Refusal::os("reading a PCI function's reset methods", e))?;
    if bytes.len() > RESET_METHOD_BYTES {
        return Err(invalid(
            "a PCI function's reset methods exceed one sysfs page",
        ));
    }
    String::from_utf8(bytes).map_err(|_| invalid("a PCI function's reset methods aren't text"))
}

/// The functions among `given`, one VM's (by SBDF), that no reset clears at their release ([`cleared`]), as `host`
/// lists them: each one's reset methods and every device on its bus. Such a function goes to one VM per host boot
/// (`super::single_use`).
pub(super) fn once_per_boot<'a>(host: &Host, given: &[&'a str]) -> Result<Vec<&'a str>, Refusal> {
    let functions = host.functions()?;
    let mut methods = Vec::new();
    reserve(&mut methods, given.len())?;
    for function in given {
        methods.push(reset_methods(&path(&functions, function)?)?);
    }
    // Which of the VM's functions share their bus with a device that goes to no VM with them.
    let mut shared = Vec::new();
    reserve(&mut shared, given.len())?;
    shared.resize(given.len(), false);
    let mut count = 0usize;
    for entry in fs::read_dir(&functions).map_err(|e| Refusal::os("listing PCI functions", e))? {
        if count >= hex_api::MAX_DEVICES {
            return Err(exhausted("host PCI functions", hex_api::MAX_DEVICES));
        }
        count = count
            .checked_add(1)
            .ok_or_else(|| invalid("PCI function count overflow"))?;
        let entry = entry.map_err(|e| Refusal::os("listing PCI functions", e))?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| invalid("native PCI function is not UTF-8"))?;
        if given.contains(&name) {
            continue;
        }
        for (function, shares) in given.iter().zip(shared.iter_mut()) {
            if bus(function).is_some_and(|own| bus(name) == Some(own)) {
                *shares = true;
            }
        }
    }
    let mut once = Vec::new();
    reserve(&mut once, given.len())?;
    for ((function, own), shares) in given.iter().zip(&methods).zip(&shared) {
        let bus_reset = given.iter().zip(&methods).any(|(other, listed)| {
            bus(other) == bus(function)
                && listed
                    .split_ascii_whitespace()
                    .any(|method| BUS_RESETS.contains(&method))
        });
        if !cleared(own, bus_reset, *shares) {
            once.push(*function);
        }
    }
    Ok(once)
}

/// The SBDFs of a VM's `plan`.
pub(super) fn functions(plan: &[Passthrough]) -> Result<Vec<&str>, Refusal> {
    let mut functions = Vec::new();
    reserve(&mut functions, plan.len())?;
    functions.extend(plan.iter().map(|device| device.sbdf.as_str()));
    Ok(functions)
}

/// The functions a catalogued VFIO entry names, by SBDF: its own and its companions, those a VM given it gets.
pub(super) fn declared(entry: &Entry) -> Result<Vec<&str>, Refusal> {
    let Some(Binding::Vfio {
        sbdf, companions, ..
    }) = entry.binding(Route::Vfio)
    else {
        return Err(invalid("Firecracker requires VFIO native bindings"));
    };
    let mut functions = Vec::new();
    let count = companions
        .len()
        .checked_add(1)
        .ok_or_else(|| exhausted("VFIO function metadata", hex_api::MAX_DEVICES))?;
    reserve(&mut functions, count)?;
    functions.push(sbdf.as_str());
    functions.extend(companions.iter().map(String::as_str));
    Ok(functions)
}

fn pci(sbdf: &str) -> Result<hex_gpu::PciAddress, Refusal> {
    let pci =
        hex_gpu::PciAddress::parse(sbdf).ok_or_else(|| invalid("invalid native PCI address"))?;
    if pci.domain > u32::from(u16::MAX) {
        return Err(invalid("PCI function must use the canonical native SBDF"));
    }
    // Linux SBDF is domain16/bus8/device5/function3 with these fixed separators.
    let mut canonical = String::new();
    canonical
        .try_reserve_exact("ffff:ff:1f.7".len())
        .map_err(|_| exhausted("native PCI address", "ffff:ff:1f.7".len()))?;
    write!(&mut canonical, "{pci}").map_err(|_| invalid("native PCI address cannot be encoded"))?;
    if canonical != sbdf {
        return Err(invalid("PCI function must use the canonical native SBDF"));
    }
    Ok(pci)
}

fn group_name(group: u32) -> Result<String, Refusal> {
    // IOMMU group numbers are u32; ten decimal digits cover every representable group.
    let mut name = String::new();
    name.try_reserve_exact(10)
        .map_err(|_| exhausted("IOMMU group name", 10))?;
    write!(&mut name, "{group}").map_err(|_| invalid("IOMMU group cannot be encoded"))?;
    Ok(name)
}

fn reserve<T>(items: &mut Vec<T>, more: usize) -> Result<(), Refusal> {
    if items
        .len()
        .checked_add(more)
        .is_none_or(|n| n > hex_api::MAX_DEVICES)
    {
        return Err(exhausted("VFIO function metadata", hex_api::MAX_DEVICES));
    }
    items
        .try_reserve(more)
        .map_err(|_| exhausted("VFIO function metadata", hex_api::MAX_DEVICES))
}

/// What a VMM pins its guest's RAM for DMA under: the RLIMIT_MEMLOCK it starts with, and whether it holds
/// CAP_IPC_LOCK, which exempts it from that limit ([`dma_memlock`]).
pub(super) struct Memlock {
    pub(super) limit: rustix::process::Rlimit,
    pub(super) ipc_lock: bool,
}

impl Memlock {
    /// This process's: the RLIMIT_MEMLOCK a VMM it starts inherits, and whether that VMM holds CAP_IPC_LOCK.
    fn process() -> Result<Self, Refusal> {
        Ok(Self {
            limit: rustix::process::getrlimit(rustix::process::Resource::Memlock),
            ipc_lock: vmm_holds_ipc_lock()?,
        })
    }
}

/// The RLIMIT_MEMLOCK a VMM that maps `needed` bytes of its guest's RAM for DMA runs under: vfio's type1 IOMMU charges
/// every page it pins to the VMM's RLIMIT_MEMLOCK unless the VMM holds CAP_IPC_LOCK
/// (drivers/vfio/vfio_iommu_type1.c, `vfio_pin_pages_remote`), and Firecracker maps all of a VM's RAM once a device is
/// given it whole. `Ok(None)`: the VMM runs under the node's own limit, which holds it, or holds the capability.
/// `Ok(Some(limit))`: its soft limit raised to its hard one, which holds the RAM where the soft doesn't. Refused by
/// type where neither holds it and the VMM won't hold CAP_IPC_LOCK (`ipc_lock`), naming the bytes, both limits and the
/// capability, so the create is refused before anything is launched rather than the VMM ending on its DMA map's ENOMEM
/// (the lead, 2026-11-11, from the GPU run r690's `IommuDmaMap(Error(12))`).
fn dma_memlock(
    needed: u64,
    limit: rustix::process::Rlimit,
    ipc_lock: bool,
) -> Result<Option<rustix::process::Rlimit>, Refusal> {
    let holds = |bound: Option<u64>| bound.is_none_or(|bound| bound >= needed);
    if ipc_lock || holds(limit.current) {
        return Ok(None);
    }
    if holds(limit.maximum) {
        return Ok(Some(rustix::process::Rlimit {
            current: limit.maximum,
            maximum: limit.maximum,
        }));
    }
    let shown =
        |bound: Option<u64>| bound.map_or_else(|| "unlimited".to_owned(), |b| b.to_string());
    Err(Refusal::Platform {
        what: "a VM given a device whole (VFIO)",
        platform: "linux",
        why: format!(
            "its VMM pins all {needed} bytes of its guest's RAM for DMA, past RLIMIT_MEMLOCK: soft {}, hard {}, and \
             its VMM won't hold CAP_IPC_LOCK; give the node a memlock hard limit of {needed} bytes or more \
             (LimitMEMLOCK=), or CAP_IPC_LOCK",
            shown(limit.current),
            shown(limit.maximum)
        ),
    })
}

/// Whether a VMM this process starts holds CAP_IPC_LOCK where vfio looks for it, the initial user namespace's
/// (`capable(CAP_IPC_LOCK)`): a process of the initial user namespace whose child keeps it across its execve, a root
/// one's from its bounding set (capabilities(7), the root exemption), any other's from its ambient set alone.
fn vmm_holds_ipc_lock() -> Result<bool, Refusal> {
    use rustix::thread::CapabilitySet;
    let map = fs::read_to_string("/proc/self/uid_map")
        .map_err(|e| Refusal::os("reading this process's user namespace map", e))?;
    let initial = map.split_whitespace().collect::<Vec<_>>() == ["0", "0", "4294967295"];
    if !initial {
        return Ok(false);
    }
    let held = if rustix::process::geteuid().is_root() {
        rustix::thread::capability_is_in_bounding_set(CapabilitySet::IPC_LOCK)
    } else {
        rustix::thread::capability_is_in_ambient_set(CapabilitySet::IPC_LOCK)
    };
    held.map_err(|e| Refusal::os("reading this process's CAP_IPC_LOCK", e))
}

/// The VFIO functions a VM given `ids` gets: every function of their IOMMU groups, each checked as `host` lists it.
/// `vmms`: the VMMs of this node's own that hold the groups now, where any does (a pool's observer before its handoff).
pub(super) fn plan(
    host: &Host,
    catalog: &Catalog,
    ids: &[DeviceId],
    vmms: &[ProcId],
) -> Result<Vec<Passthrough>, Refusal> {
    let mut selected = Vec::new();
    reserve(&mut selected, ids.len())?;
    selected.extend_from_slice(ids);
    selected.sort_unstable();
    let mut indexed = Vec::new();
    reserve(&mut indexed, catalog.entries().len())?;
    indexed.extend(catalog.entries().iter());
    indexed.sort_unstable_by_key(|entry| entry.info.id);
    let mut groups = Vec::new();
    reserve(&mut groups, selected.len())?;
    for id in &selected {
        let at = indexed
            .binary_search_by_key(id, |entry| entry.info.id)
            .map_err(|_| invalid("selected physical identity has no VFIO binding"))?;
        let entry = indexed
            .get(at)
            .ok_or_else(|| invalid("physical identity index disagrees"))?;
        validate(host, entry, vmms)?;
        let Some(Binding::Vfio { group, .. }) = entry.binding(Route::Vfio) else {
            return Err(invalid("selected native binding is not VFIO"));
        };
        groups.push(*group);
    }
    groups.sort_unstable();
    groups.dedup();
    // Collect and sort transient administrator authority once. Companion declarations never
    // supply the independent GPU selection; that is checked against the actual function key.
    let mut functions = Vec::new();
    for entry in &indexed {
        let Some(Binding::Vfio {
            sbdf,
            group,
            companions,
        }) = entry.binding(Route::Vfio)
        else {
            continue;
        };
        if groups.binary_search(group).is_err() {
            continue;
        }
        for name in std::iter::once(sbdf).chain(companions) {
            reserve(&mut functions, 1)?;
            functions.push((
                *group,
                hex_gpu::Identity::Pci(pci(name)?).key(),
                name.as_str(),
            ));
        }
    }
    functions.sort_unstable_by_key(|function| (function.0, function.1));
    functions.dedup_by_key(|function| (function.0, function.1));
    let mut devices = Vec::new();
    let iommu_groups = path(&host.sys, "kernel/iommu_groups")?;
    for group in groups {
        let group_path = path(&iommu_groups, &group_name(group)?)?;
        let group_path = path(&group_path, "devices")?;
        let mut count = 0usize;
        for member in
            fs::read_dir(&group_path).map_err(|e| Refusal::os("reading VFIO isolation group", e))?
        {
            if count >= hex_api::MAX_DEVICES {
                return Err(exhausted("VFIO group functions", hex_api::MAX_DEVICES));
            }
            count = count
                .checked_add(1)
                .ok_or_else(|| invalid("VFIO function count overflow"))?;
            let member = member.map_err(|e| Refusal::os("reading VFIO group member", e))?;
            let name = member.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| invalid("native VFIO function is not UTF-8"))?;
            let key = hex_gpu::Identity::Pci(pci(name)?).key();
            if functions
                .binary_search_by_key(&(group, key), |function| (function.0, function.1))
                .is_err()
            {
                return Err(invalid(
                    "IOMMU group includes an unauthorized physical function",
                ));
            }
            let class = validate_function(host, name, group)?;
            authorize_function(&indexed, &selected, group, key, class)?;
            reserve(&mut devices, 1)?;
            devices.push(Passthrough {
                id: function_id(name)?,
                sbdf: text(name)?,
                graphics: graphics(class),
            });
        }
        if count == 0 {
            return Err(invalid(
                "selected isolation group has no physical functions",
            ));
        }
    }
    devices.sort_unstable_by(|a, b| a.sbdf.cmp(&b.sbdf));
    // A hotplug/group change during the finite snapshot must not omit a selected primary while
    // leaving only one of its companions in an otherwise nonempty passthrough group.
    for id in &selected {
        let entry = indexed
            .binary_search_by_key(id, |entry| entry.info.id)
            .ok()
            .and_then(|index| indexed.get(index))
            .ok_or_else(|| invalid("selected identity disappeared from its index"))?;
        let Some(Binding::Vfio { sbdf, .. }) = entry.binding(Route::Vfio) else {
            return Err(invalid("selected identity lost its VFIO route"));
        };
        if devices
            .binary_search_by(|device| device.sbdf.as_str().cmp(sbdf))
            .is_err()
        {
            return Err(invalid(
                "selected physical function disappeared from its IOMMU group",
            ));
        }
    }
    Ok(devices)
}

fn graphics(class: u32) -> bool {
    // Linux PCI_BASE_CLASS_DISPLAY=0x03 and PROCESSING_ACCELERATOR=0x12; GPU compute functions
    // need independent authority even if an administrator lists them as another GPU's companion.
    matches!(class >> 16, 0x03 | 0x12)
}

fn authorize_function(
    indexed: &[&Entry],
    selected: &[DeviceId],
    group: u32,
    key: u64,
    class: u32,
) -> Result<(), Refusal> {
    if !graphics(class)
        || selected
            .binary_search(&DeviceId {
                kind: DeviceKind::Vfio,
                key: u64::from(group),
            })
            .is_ok()
    {
        return Ok(());
    }
    let id = DeviceId {
        kind: DeviceKind::Gpu,
        key,
    };
    let at = indexed
        .binary_search_by_key(&id, |entry| entry.info.id)
        .map_err(|_| invalid("actual GPU function is not independently catalogued"))?;
    let entry = indexed
        .get(at)
        .ok_or_else(|| invalid("GPU authority index disagrees"))?;
    if selected.binary_search(&id).is_err()
        || !matches!(entry.binding(Route::Vfio), Some(Binding::Vfio { group: configured, .. }) if *configured == group)
    {
        return Err(invalid(
            "IOMMU group includes an independently unselected GPU",
        ));
    }
    Ok(())
}

/// Checks a catalogued VFIO function as `host` lists it, and that this process may open its container and group.
/// `vmms`: the VMMs of this node's own that may hold its group now, a recorded VM's or pool observer's. vfio gives a
/// group to one opener at a time (a second open answers EBUSY), so a group one of them is seen holding ([`holds`]) is
/// in use by this node's own VM and isn't opened; held by any other process, it is refused by type, naming that
/// process ([`held`]).
pub(super) fn validate(host: &Host, entry: &Entry, vmms: &[ProcId]) -> Result<(), Refusal> {
    let Some(Binding::Vfio { sbdf, group, .. }) = entry.binding(Route::Vfio) else {
        return Err(invalid("Firecracker requires VFIO native bindings"));
    };
    let key = hex_gpu::Identity::Pci(pci(sbdf)?).key();
    if (entry.info.id.kind == DeviceKind::Gpu && key != entry.info.id.key)
        || (entry.info.id.kind == DeviceKind::Vfio && entry.info.id.key != u64::from(*group))
    {
        return Err(invalid(
            "VFIO binding differs from its selected physical identity",
        ));
    }
    let class = validate_function(host, sbdf, *group)?;
    if entry.info.id.kind == DeviceKind::Gpu && !graphics(class) {
        return Err(invalid(
            "selected native function is not a GPU/compute accelerator",
        ));
    }
    let _container = (host.open)(&host.interface("vfio")?)
        .map_err(|e| Refusal::os("opening selected VFIO interface", e))?;
    let interface = host.interface(&group_name(*group)?)?;
    if !vmms.is_empty()
        && let Ok(named) = fs::canonicalize(&interface)
    {
        let descriptors = kernel_bound("fs/nr_open")?;
        for vmm in vmms {
            if holds(*vmm, &named, descriptors)? {
                return Ok(());
            }
        }
    }
    match (host.open)(&interface) {
        Ok(_access) => Ok(()),
        Err(e) if e.raw_os_error() == Some(rustix::io::Errno::BUSY.raw_os_error()) => {
            Err(held(&interface, &e)?)
        }
        Err(e) => Err(Refusal::os("opening selected VFIO interface", e)),
    }
}

/// A kernel limit `/proc/sys` names (`kernel/pid_max`, `fs/nr_open`), the bound of a walk over what it limits.
fn kernel_bound(name: &str) -> Result<usize, Refusal> {
    let text = fs::read_to_string(path(Path::new("/proc/sys"), name)?)
        .map_err(|e| Refusal::os("reading a kernel limit", e))?;
    text.trim()
        .parse()
        .map_err(|_| invalid("a kernel limit isn't a count"))
}

/// Whether process `vmm`, the one its pid and start time name, holds `interface` (resolved) open: a process that
/// ended, or another given its pid since, holds nothing of this node's.
fn holds(vmm: ProcId, interface: &Path, descriptors: usize) -> Result<bool, Refusal> {
    let Ok(pid) = i32::try_from(vmm.pid) else {
        return Ok(false);
    };
    if crate::linux::oci::state::start_time(pid) != Some(vmm.start_ticks) {
        return Ok(false);
    }
    names(vmm.pid, interface, descriptors)
}

/// Whether one of process `pid`'s open descriptors, its `/proc/<pid>/fd` links (at most the kernel's `nr_open`,
/// `descriptors`), names `interface`. A process that ended, or that this node may not read, names none it sees.
fn names(pid: u32, interface: &Path, descriptors: usize) -> Result<bool, Refusal> {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Ok(false);
    };
    let mut count = 0usize;
    for entry in entries {
        if count >= descriptors {
            return Err(exhausted("a process's open descriptors", descriptors));
        }
        count = count
            .checked_add(1)
            .ok_or_else(|| invalid("descriptor count overflow"))?;
        let Ok(entry) = entry else {
            continue;
        };
        if fs::read_link(entry.path()).is_ok_and(|target| target == interface) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The refusal of a VFIO group another process holds: its open's error, naming the first process this node can read
/// that holds it, by pid and command, from a walk of `/proc` (at most the kernel's `pid_max` processes), or saying it
/// sees none: another user's, which this node may not read.
fn held(interface: &Path, error: &std::io::Error) -> Result<Refusal, Refusal> {
    let mut holder = None;
    if let Ok(named) = fs::canonicalize(interface) {
        let processes = kernel_bound("kernel/pid_max")?;
        let descriptors = kernel_bound("fs/nr_open")?;
        let mut count = 0usize;
        for entry in fs::read_dir("/proc").map_err(|e| Refusal::os("listing processes", e))? {
            let Some(pid) = entry
                .ok()
                .and_then(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            else {
                continue;
            };
            if count >= processes {
                return Err(exhausted("processes", processes));
            }
            count = count
                .checked_add(1)
                .ok_or_else(|| invalid("process count overflow"))?;
            if names(pid, &named, descriptors)? {
                let command = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
                holder = Some((pid, command));
                break;
            }
        }
    }
    let why = match holder {
        Some((pid, command)) => format!(
            "{}: {error}: held by pid {pid} ({}), no VM this node records",
            interface.display(),
            command.trim()
        ),
        None => format!(
            "{}: {error}: held by a process this node can't read, no VM this node records",
            interface.display()
        ),
    };
    Ok(Refusal::Os {
        op: "opening selected VFIO interface",
        why,
    })
}

/// The user owning the interface of function `sbdf`'s IOMMU group, `/dev/vfio/<group>`, as `host` lists them: whose
/// nodes may open it.
pub(super) fn group_owner(host: &Host, sbdf: &str) -> Result<u32, Refusal> {
    use std::os::unix::fs::MetadataExt;
    pci(sbdf)?;
    let group = fs::canonicalize(path(&path(&host.functions()?, sbdf)?, "iommu_group")?)
        .map_err(|e| Refusal::os("resolving native IOMMU group", e))?;
    let name = group
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| name.parse::<u32>().is_ok())
        .ok_or_else(|| invalid("a native IOMMU group isn't named by its number"))?;
    fs::metadata(host.interface(name)?)
        .map(|interface| interface.uid())
        .map_err(|e| Refusal::os("reading a VFIO group interface's owner", e))
}

fn validate_function(host: &Host, sbdf: &str, group: u32) -> Result<u32, Refusal> {
    pci(sbdf)?;
    let device_path = path(&host.functions()?, sbdf)?;
    let native_group = fs::canonicalize(path(&device_path, "iommu_group")?)
        .map_err(|e| Refusal::os("resolving native IOMMU group", e))?;
    if native_group
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.parse::<u32>().ok())
        != Some(group)
    {
        return Err(invalid(
            "physical function moved to a different isolation group",
        ));
    }
    let driver = fs::canonicalize(path(&device_path, "driver")?)
        .map_err(|e| Refusal::os("resolving native PCI driver", e))?;
    if driver.file_name().is_none_or(|name| name != "vfio-pci") {
        return Err(invalid("selected function is not prebound to vfio-pci"));
    }
    // PCI class is 24 bits, encoded by sysfs as 0x + six hex digits + newline. One excess
    // byte distinguishes an oversized/changed object; no unbounded string allocation occurs.
    let mut bytes = [0u8; b"0xffffff\n".len() + 1];
    let mut file = fs::File::open(path(&device_path, "class")?)
        .map_err(|e| Refusal::os("reading native PCI class", e))?;
    let mut used = 0usize;
    while used < bytes.len() {
        let count = file
            .read(
                bytes
                    .get_mut(used..)
                    .ok_or_else(|| invalid("PCI class byte count overflow"))?,
            )
            .map_err(|e| Refusal::os("reading native PCI class", e))?;
        if count == 0 {
            break;
        }
        used = used
            .checked_add(count)
            .ok_or_else(|| invalid("PCI class byte count overflow"))?;
    }
    if used >= bytes.len() {
        return Err(invalid("native PCI class exceeds its protocol width"));
    }
    let text = std::str::from_utf8(
        bytes
            .get(..used)
            .ok_or_else(|| invalid("PCI class byte count overflow"))?,
    )
    .map_err(|_| invalid("PCI class is not ASCII"))?;
    text.trim()
        .strip_prefix("0x")
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .filter(|value| *value <= 0xffffff)
        .ok_or_else(|| invalid("malformed native PCI class"))
}

pub(super) fn function_id(sbdf: &str) -> Result<String, Refusal> {
    let key = hex_gpu::Identity::Pci(pci(sbdf)?).key();
    // Prefix plus at most sixteen hexadecimal digits for the existing u64 physical key.
    let bound = "pci_"
        .len()
        .checked_add(16)
        .ok_or_else(|| invalid("VFIO identifier bound overflows"))?;
    let mut id = String::new();
    id.try_reserve_exact(bound)
        .map_err(|_| exhausted("VFIO function identifier", bound))?;
    write!(&mut id, "pci_{key:x}")
        .map_err(|_| invalid("VFIO function identifier cannot be encoded"))?;
    Ok(id)
}

/// A stand-in host for tests: a tree laid out as the host's `/sys` and `/dev` are, where a test adds the PCI functions
/// it needs.
#[cfg(test)]
pub(super) mod stand_in {
    use super::*;

    /// An empty stand-in host under `root`, opening its interfaces with `open`. Its memlock is refused until its test
    /// gives one ([`memlock`]).
    pub(in super::super) fn host(
        root: &Path,
        open: fn(&Path) -> std::io::Result<fs::File>,
    ) -> Host {
        let host = Host {
            sys: root.join("sys"),
            dev: root.join("dev"),
            open,
            memlock: Box::new(|| Err(invalid("a stand-in host's memlock is its test's to give"))),
        };
        fs::create_dir_all(host.dev.join("vfio")).unwrap();
        fs::write(host.dev.join("vfio/vfio"), "").unwrap();
        host
    }

    /// The host's own way of opening an interface, for a stand-in's files.
    pub(in super::super) fn opened(path: &Path) -> std::io::Result<fs::File> {
        open_read_write(path)
    }

    /// A stand-in host's memlock: its VMMs start under an RLIMIT_MEMLOCK of `bytes`, soft and hard, holding no
    /// CAP_IPC_LOCK.
    pub(in super::super) fn memlock(
        bytes: u64,
    ) -> Box<dyn Fn() -> Result<Memlock, Refusal> + Send + Sync> {
        Box::new(move || {
            Ok(Memlock {
                limit: rustix::process::Rlimit {
                    current: Some(bytes),
                    maximum: Some(bytes),
                },
                ipc_lock: false,
            })
        })
    }

    /// Adds PCI function `sbdf` to `host`: in IOMMU group `group` (its interface in `/dev/vfio`), bound to `driver`,
    /// of PCI class `class`, listing `reset` as its reset methods where `Some` (the kernel hides the file where it lists
    /// none).
    pub(in super::super) fn function(
        host: &Host,
        sbdf: &str,
        group: u32,
        driver: &str,
        class: &str,
        reset: Option<&str>,
    ) {
        let group_dir = host.sys.join(format!("kernel/iommu_groups/{group}"));
        let driver = host.sys.join("bus/pci/drivers").join(driver);
        let function = host.sys.join("bus/pci/devices").join(sbdf);
        for dir in [&group_dir.join("devices"), &driver, &function] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(group_dir.join("devices").join(sbdf), "").unwrap();
        std::os::unix::fs::symlink(&group_dir, function.join("iommu_group")).unwrap();
        std::os::unix::fs::symlink(&driver, function.join("driver")).unwrap();
        fs::write(function.join("class"), format!("{class}\n")).unwrap();
        if let Some(reset) = reset {
            fs::write(function.join("reset_method"), format!("{reset}\n")).unwrap();
        }
        let interface = host.dev.join("vfio").join(group.to_string());
        if !interface.exists() {
            fs::write(interface, "").unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lead's ruling of 2026-11-11 (AC-T7.4.1; the GPU run r690's step a: RLIMIT_MEMLOCK 524,288 KiB against a VM of
    /// 1,536 MiB, `IommuDmaMap(Error(12))`): the RLIMIT_MEMLOCK a VMM that pins its guest's RAM for DMA runs under. Do:
    /// ask for r690's VM under its limit, with and without CAP_IPC_LOCK; under a soft limit short of the RAM and a hard
    /// one that holds it, or none; under a soft limit that holds it, or none. Expect r690's refused by type, naming
    /// the RAM in bytes, both limits and CAP_IPC_LOCK, and with the capability left as the node's; the short soft limit
    /// raised to its hard one; one that holds it left as the node's.
    #[test]
    fn a_vmm_pinning_its_ram_runs_under_a_memlock_that_holds_it_or_is_refused_naming_it() {
        use rustix::process::Rlimit;
        let ram = 1_536u64 << 20;
        let at = |soft: Option<u64>, hard: Option<u64>| Rlimit {
            current: soft,
            maximum: hard,
        };
        let r690 = at(Some(524_288 << 10), Some(524_288 << 10));
        let Err(Refusal::Platform { why, .. }) = dma_memlock(ram, r690, false) else {
            panic!("r690's VM refused before its launch")
        };
        for named in [
            "1610612736 bytes",
            "soft 536870912",
            "hard 536870912",
            "CAP_IPC_LOCK",
        ] {
            assert!(why.contains(named), "{named}: {why}");
        }
        assert_eq!(dma_memlock(ram, r690, true), Ok(None));
        let short = Some(8 << 20);
        for hard in [Some(ram), Some(ram + 1), None] {
            assert_eq!(
                dma_memlock(ram, at(short, hard), false),
                Ok(Some(at(hard, hard))),
                "{hard:?}"
            );
        }
        for soft in [Some(ram), None] {
            assert_eq!(dma_memlock(ram, at(soft, soft), false), Ok(None));
        }
    }

    /// AC-T7.4.1: native address and VMM witness identifiers retain exact canonical physical bits.
    #[test]
    fn native_function_names_keep_canonical_full_physical_identity() {
        assert_eq!(function_id("0000:01:00.0").unwrap(), "pci_100");
        assert_eq!(function_id("ffff:ff:1f.7").unwrap(), "pci_ffffffff");
        assert_eq!(group_name(u32::MAX).unwrap(), "4294967295");
        for invalid in [
            "0:01:00.0",
            "FFFF:ff:1f.7",
            "10000:00:00.0",
            "0000:00:20.0",
            "0000:00:00.8",
        ] {
            assert!(
                function_id(invalid).is_err(),
                "accepted noncanonical {invalid}"
            );
        }
    }

    /// The kernel's answer to opening a VFIO group another process holds (vfio's group open admits one opener).
    fn busy(path: &Path) -> std::io::Result<fs::File> {
        if path.file_name().is_some_and(|name| name == "7") {
            return Err(std::io::Error::from_raw_os_error(
                rustix::io::Errno::BUSY.raw_os_error(),
            ));
        }
        open_read_write(path)
    }

    /// A stand-in host for one function, `0000:01:00.0`, alone in IOMMU group 7, a display controller bound to
    /// vfio-pci, whose group's open answers EBUSY as a held group's does.
    fn held_host(root: &Path) -> Host {
        let host = stand_in::host(root, busy);
        stand_in::function(&host, "0000:01:00.0", 7, "vfio-pci", "0x030000", None);
        host
    }

    /// The lead's report of GPU run r985 (AC-T7.4.1, T-T7.1.2; original): a node SIGKILLed beside its running GPU VM
    /// refused to start again, "opening selected VFIO interface failed: Device or resource busy (os error 16)": its
    /// start checks each catalogued VFIO function by opening its group, which the kernel gives one process at a time,
    /// and its own VM's VMM held it. Do: check a stand-in function whose group's open answers EBUSY while a stand-in
    /// VMM, a child holding the group's file open, holds it: with this node's recorded VMMs naming that child by its
    /// pid and start time, then naming another start time, then none. Expect it in use by this node's own VM, valid
    /// with its group unopened; and refused by type, naming the holder's pid, where no recorded VMM of this node holds
    /// it.
    #[test]
    fn original_a_group_its_own_vms_vmm_holds_is_in_use_not_busy() {
        let root = tempfile::tempdir().unwrap();
        let host = held_host(root.path());
        let catalog =
            Catalog::parse(b"hex-devices-1\ngpu\t256\ta\t1\tvfio\t0000:01:00.0\t7\n").unwrap();
        let entry = &catalog.entries()[0];
        let group = fs::File::open(host.dev.join("vfio/7")).unwrap();
        let mut vmm = std::process::Command::new("sleep")
            .arg("600")
            .stdin(group)
            .spawn()
            .unwrap();
        let pid = vmm.id();
        let own = ProcId {
            pid,
            start_ticks: crate::linux::oci::state::start_time(i32::try_from(pid).unwrap()).unwrap(),
        };
        let checked = validate(&host, entry, &[own]);
        let other = ProcId {
            start_ticks: own.start_ticks + 1,
            ..own
        };
        let foreign = [&[other][..], &[]].map(|recorded| validate(&host, entry, recorded));
        vmm.kill().unwrap();
        vmm.wait().unwrap();
        assert_eq!(checked, Ok(()), "its own VM's VMM holds it");
        for refused in foreign {
            let Err(Refusal::Os { op, why }) = &refused else {
                panic!("refused by type: {refused:?}");
            };
            assert_eq!(*op, "opening selected VFIO interface");
            assert!(
                why.contains(&format!("pid {pid} ")),
                "names the holder: {why}"
            );
        }
    }

    /// The kernel's rules for what clears a function at its VM's end, at v7.3 (AC-T7.4.1; docs/design/
    /// firecracker-gpu-host.md). Do: on a stand-in host, give VMs functions whose kernel lists no method; flr and bus;
    /// pm; only a method this code doesn't know; bus, alone on its bus; bus, beside a function the same VM is given;
    /// and bus, beside a device no VM is given, as 0000:79:00.0 lists it beside its audio function on 00aad8da (r940).
    /// Expect once per boot the one listing none, the unknown one and the last; every other one cleared.
    #[test]
    fn a_function_no_reset_clears_goes_to_one_vm_per_boot() {
        let root = tempfile::tempdir().unwrap();
        let host = stand_in::host(root.path(), stand_in::opened);
        for (sbdf, group, driver, reset) in [
            ("0000:01:00.0", 1, "vfio-pci", None),
            ("0000:02:00.0", 2, "vfio-pci", Some("flr bus")),
            ("0000:03:00.0", 3, "vfio-pci", Some("pm")),
            (
                "0000:04:00.0",
                4,
                "vfio-pci",
                Some("a_later_kernels_method"),
            ),
            ("0000:05:00.0", 5, "vfio-pci", Some("bus")),
            ("0000:06:00.0", 6, "vfio-pci", Some("bus")),
            ("0000:06:00.1", 6, "vfio-pci", None),
            ("0000:79:00.0", 25, "vfio-pci", Some("bus")),
            ("0000:79:00.1", 26, "snd_hda_intel", Some("pm")),
        ] {
            stand_in::function(&host, sbdf, group, driver, "0x030000", reset);
        }
        let check = |given: &[&str], once: &[&str]| {
            assert_eq!(once_per_boot(&host, given).unwrap(), once, "{given:?}");
        };
        check(&["0000:01:00.0"], &["0000:01:00.0"]);
        check(&["0000:02:00.0"], &[]);
        check(&["0000:03:00.0"], &[]);
        check(&["0000:04:00.0"], &["0000:04:00.0"]);
        check(&["0000:05:00.0"], &[]);
        check(&["0000:06:00.0", "0000:06:00.1"], &[]);
        check(&["0000:79:00.0"], &["0000:79:00.0"]);
    }

    #[test]
    fn companion_declaration_cannot_authorize_a_second_physical_gpu() {
        let catalog = Catalog::parse(
            b"hex-devices-1\ngpu\t256\tfixture\t4096\tvfio\t0000:01:00.0,0000:02:00.0\t7\n",
        )
        .unwrap();
        let indexed = catalog.entries().iter().collect::<Vec<_>>();
        let selected = [DeviceId {
            kind: DeviceKind::Gpu,
            key: 256,
        }];
        assert!(authorize_function(&indexed, &selected, 7, 512, 0x030000).is_err());
        assert!(authorize_function(&indexed, &selected, 7, 512, 0x120000).is_err());
        assert!(authorize_function(&indexed, &selected, 7, 512, 0x040300).is_ok());
        assert!(
            authorize_function(
                &indexed,
                &[DeviceId {
                    kind: DeviceKind::Vfio,
                    key: 7
                }],
                7,
                512,
                0x030000
            )
            .is_ok()
        );
    }
}
