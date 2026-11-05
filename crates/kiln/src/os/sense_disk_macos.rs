//! macOS: the work volume's device, found and read without privilege (docs/needs.md;
//! research-disk.md). `statfs` names the volume's BSD device (`/dev/disk3s5`);
//! `IOBSDNameMatching` finds its IOMedia; its IOService parents lead, through APFS's container
//! and the physical store's partition scheme, to the physical disk's `IOBlockStorageDriver`,
//! whose provider is the device (`IONVMeBlockStorageDevice`, `IOAHCIBlockStorageDevice`, …) and
//! says what it is (`Device Characteristics`, `Protocol Characteristics`).
//!
//! - Counters: the driver's `Statistics` (bytes, operations, total time in ns, errors, retries,
//!   per direction). APFS's synthesized disk has a driver of its own on the way up, which counts
//!   only bytes and operations: the walk goes on to the physical one, below the device.
//! - Health: NVMe's SMART / Health Information log page through NVMeSMARTLib's user client
//!   (`SMARTReadData`), on the device that says `NVMe SMART Capable`; it needs no privilege.

#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_void};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use kiln_ci::facts::{DeviceHealth, DeviceKind, Storage, StorageDevice};

use super::sense::cf;
use crate::sense::disk::{Counters, Dir, Errors};

type IoObject = u32;
type KernReturn = i32;
/// `kIOMainPortDefault`: the default main port.
const MAIN_PORT: u32 = 0;
const SERVICE_PLANE: &CStr = c"IOService";

/// `CFUUIDBytes`.
#[repr(C)]
#[derive(Clone, Copy)]
struct UuidBytes([u8; 16]);

/// `IUNKNOWN_C_GUTS`, the head of every CFPlugIn interface's function table.
#[repr(C)]
struct Unknown {
    _reserved: *mut c_void,
    query_interface: unsafe extern "C" fn(this: *mut c_void, iid: UuidBytes, out: *mut *mut c_void) -> i32,
    _add_ref: unsafe extern "C" fn(this: *mut c_void) -> u32,
    release: unsafe extern "C" fn(this: *mut c_void) -> u32,
}

/// `IONVMeSMARTInterface` (`IOKit/storage/nvme/NVMeSMARTLibExternal.h`), as far as
/// `SMARTReadData`.
#[repr(C)]
struct SmartInterface {
    unknown: Unknown,
    _version: u16,
    _revision: u16,
    smart_read_data: unsafe extern "C" fn(this: *mut c_void, data: *mut u8) -> KernReturn,
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOBSDNameMatching(main: u32, options: u32, name: *const c_char) -> cf::CFTypeRef;
    fn IOServiceGetMatchingService(main: u32, matching: cf::CFTypeRef) -> IoObject;
    fn IORegistryEntryGetParentEntry(entry: IoObject, plane: *const c_char, parent: *mut IoObject) -> KernReturn;
    fn IOObjectConformsTo(object: IoObject, class: *const c_char) -> i32;
    fn IOObjectRetain(object: IoObject) -> KernReturn;
    fn IOObjectRelease(object: IoObject) -> KernReturn;
    fn IORegistryEntryCreateCFProperty(entry: IoObject, key: cf::CFStringRef, alloc: cf::CFTypeRef, options: u32) -> cf::CFTypeRef;
    fn IOCreatePlugInInterfaceForService(service: IoObject, plugin_type: cf::CFTypeRef, interface_type: cf::CFTypeRef, out: *mut *mut *mut Unknown, score: *mut i32) -> KernReturn;
    fn IODestroyPlugInInterface(interface: *mut *mut Unknown) -> KernReturn;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    #[allow(clippy::too_many_arguments)]
    fn CFUUIDGetConstantUUIDWithBytes(alloc: cf::CFTypeRef, b0: u8, b1: u8, b2: u8, b3: u8, b4: u8, b5: u8, b6: u8, b7: u8, b8: u8, b9: u8, b10: u8, b11: u8, b12: u8, b13: u8, b14: u8, b15: u8) -> cf::CFTypeRef;
    fn CFUUIDGetUUIDBytes(uuid: cf::CFTypeRef) -> UuidBytes;
}

/// A constant CFUUID (never released: Core Foundation keeps constant UUIDs for good).
fn uuid(b: [u8; 16]) -> cf::CFTypeRef {
    let [b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15] = b;
    // SAFETY: plain bytes; the default allocator.
    unsafe { CFUUIDGetConstantUUIDWithBytes(std::ptr::null(), b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15) }
}

/// `kIONVMeSMARTUserClientTypeID`: AA0FA6F9-C2D6-457F-B10B-59A13253292F.
const SMART_USER_CLIENT: [u8; 16] = [0xAA, 0x0F, 0xA6, 0xF9, 0xC2, 0xD6, 0x45, 0x7F, 0xB1, 0x0B, 0x59, 0xA1, 0x32, 0x53, 0x29, 0x2F];
/// `kIONVMeSMARTInterfaceID`: CCD1DB19-FD9A-4DAF-BF95-12454B230AB6.
const SMART_INTERFACE: [u8; 16] = [0xCC, 0xD1, 0xDB, 0x19, 0xFD, 0x9A, 0x4D, 0xAF, 0xBF, 0x95, 0x12, 0x45, 0x4B, 0x23, 0x0A, 0xB6];
/// `kIOCFPlugInInterfaceID`: C244E858-109C-11D4-91D4-0050E4C6426F.
const PLUGIN_INTERFACE: [u8; 16] = [0xC2, 0x44, 0xE8, 0x58, 0x10, 0x9C, 0x11, 0xD4, 0x91, 0xD4, 0x00, 0x50, 0xE4, 0xC6, 0x42, 0x6F];

/// An IOKit object this holds one reference to, released when dropped.
struct Obj(IoObject);

impl Drop for Obj {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: an object this holds a reference to, released once.
            unsafe { IOObjectRelease(self.0) };
        }
    }
}

impl Obj {
    fn retained(&self) -> Obj {
        // SAFETY: a live object; the new reference is the returned `Obj`'s.
        unsafe { IOObjectRetain(self.0) };
        Obj(self.0)
    }

    fn conforms(&self, class: &CStr) -> bool {
        // SAFETY: a live object and a NUL-terminated class name.
        unsafe { IOObjectConformsTo(self.0, class.as_ptr()) != 0 }
    }

    fn parent(&self) -> Option<Obj> {
        let mut p: IoObject = 0;
        // SAFETY: a live object, the plane's NUL-terminated name, a place for the parent (a
        // reference this then holds).
        let r = unsafe { IORegistryEntryGetParentEntry(self.0, SERVICE_PLANE.as_ptr(), &raw mut p) };
        (r == 0 && p != 0).then_some(Obj(p))
    }

    /// Its registry property `key`, owned.
    fn property(&self, key: &CStr) -> cf::Owned {
        let k = cf::string(key);
        if k.0.is_null() {
            return cf::Owned(std::ptr::null());
        }
        // SAFETY: a live object and a live CFString; the default allocator; the result (a +1
        // reference, or null) is released by `Owned`.
        cf::Owned(unsafe { IORegistryEntryCreateCFProperty(self.0, k.0, std::ptr::null(), 0) })
    }
}

/// `statfs` of `dir`: its file system type, mount point, source, and whether it is local.
fn statfs(dir: &Path) -> Option<(String, String, String, bool)> {
    use nix::libc;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: statfs is plain integers and byte arrays, for which zero is a value.
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and a place for the result.
    if unsafe { libc::statfs(path.as_ptr(), &raw mut s) } != 0 {
        return None;
    }
    let text = |a: &[c_char]| {
        let bytes: Vec<u8> = a.iter().map(|c| *c as u8).take_while(|b| *b != 0).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    Some((text(&s.f_fstypename), text(&s.f_mntonname), text(&s.f_mntfromname), i64::from(s.f_flags) & i64::from(libc::MNT_LOCAL) != 0))
}

/// The way from a volume's media up to its physical disk.
struct Found {
    /// The physical disk's `IOBlockStorageDriver`.
    driver: Obj,
    /// Its provider, the device, when one says what it is.
    device: Option<Obj>,
    /// The whole disk's IOMedia, the driver's child.
    media: Option<Obj>,
}

fn find(source: &str) -> Option<Found> {
    let bsd = std::ffi::CString::new(source.strip_prefix("/dev/").unwrap_or(source)).ok()?;
    // SAFETY: the default main port and a NUL-terminated name; the dictionary it returns is
    // consumed by IOServiceGetMatchingService, which returns a reference this then holds.
    let start = unsafe {
        let matching = IOBSDNameMatching(MAIN_PORT, 0, bsd.as_ptr());
        if matching.is_null() {
            return None;
        }
        IOServiceGetMatchingService(MAIN_PORT, matching)
    };
    if start == 0 {
        return None;
    }
    let mut cur = Obj(start);
    let mut media: Option<Obj> = None;
    let mut driver: Option<(Obj, Option<Obj>)> = None;
    loop {
        if cur.conforms(c"IOBlockStorageDriver") {
            driver = Some((cur.retained(), media.take()));
        } else if cur.conforms(c"IOMedia") {
            media = Some(cur.retained());
        }
        // The device below a driver, saying what it is: the physical disk (APFS's own driver on
        // the way has none above it).
        if driver.is_some() && !cur.property(c"Protocol Characteristics").0.is_null() {
            let (driver, media) = driver?;
            return Some(Found { driver, device: Some(cur), media });
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => break,
        }
    }
    driver.map(|(driver, media)| Found { driver, device: None, media })
}

/// The work volume's device, as found once: its driver, whose counters are read on demand.
pub struct Device {
    driver: Obj,
}

impl Device {
    pub fn of(work_dir: &Path) -> Option<Device> {
        let (_, _, source, local) = statfs(work_dir)?;
        if !local {
            return None;
        }
        find(&source).map(|f| Device { driver: f.driver })
    }

    /// The driver's `Statistics` now.
    pub fn counters(&mut self) -> Option<Counters> {
        let stats = self.driver.property(c"Statistics");
        if stats.0.is_null() {
            return None;
        }
        let n = |k: &CStr| cf::number(cf::get(stats.0, k)).and_then(|v| u64::try_from(v).ok());
        let both = |a: &CStr, b: &CStr| match (n(a), n(b)) {
            (None, None) => None,
            (x, y) => Some(x.unwrap_or(0).saturating_add(y.unwrap_or(0))),
        };
        Some(Counters {
            read: Dir { ops: n(c"Operations (Read)")?, bytes: n(c"Bytes (Read)")?, time_ns: n(c"Total Time (Read)").unwrap_or(0) },
            write: Dir { ops: n(c"Operations (Write)")?, bytes: n(c"Bytes (Write)")?, time_ns: n(c"Total Time (Write)").unwrap_or(0) },
            busy_ns: None,
            in_flight_ns: None,
            errors: Errors { errors: both(c"Errors (Read)", c"Errors (Write)"), timeouts: None, retries: both(c"Retries (Read)", c"Retries (Write)"), resets: None },
        })
    }
}

/// A device's kind from what IOKit says of it: its interconnect first (a disk image says it is
/// solid state), its location (`File` for a disk image), then its medium.
pub fn classify(interconnect: Option<&str>, location: Option<&str>, medium: Option<&str>) -> DeviceKind {
    let i = interconnect.unwrap_or_default().to_ascii_lowercase();
    if location.is_some_and(|l| l.eq_ignore_ascii_case("file")) || i.contains("virtual") {
        return DeviceKind::Virtual;
    }
    if i.contains("apple fabric") || i.contains("pci") || i.contains("nvme") {
        return DeviceKind::Nvme;
    }
    if ["ethernet", "fibre", "iscsi", "network"].iter().any(|n| i.contains(n)) {
        return DeviceKind::Network;
    }
    match medium {
        Some(m) if m.eq_ignore_ascii_case("rotational") => DeviceKind::Hdd,
        Some(m) if m.eq_ignore_ascii_case("solid state") => DeviceKind::Ssd,
        _ => DeviceKind::Unknown,
    }
}

/// NVMe's SMART / Health Information log page through NVMeSMARTLib, from `service` (the device
/// saying `NVMe SMART Capable`).
fn nvme_health(service: &Obj) -> Option<DeviceHealth> {
    if cf::number(service.property(c"NVMe SMART Capable").0) != Some(1) {
        return None;
    }
    let mut plugin: *mut *mut Unknown = std::ptr::null_mut();
    let mut score = 0i32;
    // SAFETY: a live service, the constant UUIDs, places for the plug-in interface (released
    // below with IODestroyPlugInInterface) and its score.
    let r = unsafe { IOCreatePlugInInterfaceForService(service.0, uuid(SMART_USER_CLIENT), uuid(PLUGIN_INTERFACE), &raw mut plugin, &raw mut score) };
    if r != 0 || plugin.is_null() {
        return None;
    }
    let mut iface: *mut c_void = std::ptr::null_mut();
    // SAFETY: `plugin` points at a live plug-in interface, whose table starts with IUnknown's;
    // the interface ID's bytes; a place for the SMART interface (released below).
    let q = unsafe {
        let table = *plugin;
        if table.is_null() {
            IODestroyPlugInInterface(plugin);
            return None;
        }
        ((*table).query_interface)(plugin.cast(), CFUUIDGetUUIDBytes(uuid(SMART_INTERFACE)), &raw mut iface)
    };
    let mut page = [0u8; 512];
    let mut ok = false;
    if q == 0 && !iface.is_null() {
        let smart = iface.cast::<*mut SmartInterface>();
        // SAFETY: `smart` points at a live IONVMeSMARTInterface (the query succeeded): its table
        // has SMARTReadData, which fills the 512-byte NVMeSMARTData given it; then its one
        // reference is released.
        unsafe {
            let table = *smart;
            if !table.is_null() {
                ok = ((*table).smart_read_data)(iface, page.as_mut_ptr()) == 0;
                ((*table).unknown.release)(iface);
            }
        }
    }
    // SAFETY: the plug-in interface IOCreatePlugInInterfaceForService gave, destroyed once.
    unsafe { IODestroyPlugInInterface(plugin) };
    if ok { DeviceHealth::of_nvme_log(&page) } else { None }
}

/// The work directory's storage (docs/needs.md): its file system, and the physical device under
/// it.
pub fn facts(work_dir: &Path) -> Option<Storage> {
    let (fs, mount, source, local) = statfs(work_dir)?;
    let mut storage = Storage { fs: Some(fs.clone()), mount: Some(mount), source: Some(source.clone()).filter(|s| !s.is_empty()), network: !local, devices: Vec::new() };
    if !local {
        storage.devices.push(StorageDevice { name: source, kind: DeviceKind::Network, interconnect: Some(fs), ..StorageDevice::default() });
        return Some(storage);
    }
    let Some(found) = find(&source) else { return Some(storage) };
    let name = found.media.as_ref().and_then(|m| cf::text(m.property(c"BSD Name").0)).unwrap_or_default();
    let size = found.media.as_ref().and_then(|m| cf::number(m.property(c"Size").0)).and_then(|s| u64::try_from(s).ok());
    let Some(device) = found.device else {
        storage.devices.push(StorageDevice { name, size, ..StorageDevice::default() });
        return Some(storage);
    };
    let characteristics = device.property(c"Device Characteristics");
    let protocol = device.property(c"Protocol Characteristics");
    let medium = cf::text(cf::get(characteristics.0, c"Medium Type"));
    let model = cf::text(cf::get(characteristics.0, c"Product Name")).map(|m| m.trim().to_owned()).filter(|m| !m.is_empty());
    let interconnect = cf::text(cf::get(protocol.0, c"Physical Interconnect"));
    let location = cf::text(cf::get(protocol.0, c"Physical Interconnect Location"));
    let kind = classify(interconnect.as_deref(), location.as_deref(), medium.as_deref());
    let rotational = medium.as_deref().and_then(|m| match m {
        "Rotational" => Some(true),
        "Solid State" => Some(false),
        _ => None,
    });
    storage.devices.push(StorageDevice { name, kind, interconnect, rotational, model, size, location: location.map(|l| l.to_ascii_lowercase()), health: nvme_health(&device) });
    Some(storage)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W13-R01 (docs/needs.md): the interconnect says the kind before the medium does.
    #[test]
    fn w13_r01_macos_device_kind_by_interconnect() {
        assert_eq!(classify(Some("Apple Fabric"), Some("Internal"), Some("Solid State")), DeviceKind::Nvme);
        assert_eq!(classify(Some("PCI-Express"), Some("Internal"), Some("Solid State")), DeviceKind::Nvme);
        assert_eq!(classify(Some("Virtual Interface"), Some("File"), Some("Solid State")), DeviceKind::Virtual, "a disk image");
        assert_eq!(classify(Some("USB"), Some("External"), Some("Rotational")), DeviceKind::Hdd);
        assert_eq!(classify(Some("SATA"), Some("Internal"), Some("Solid State")), DeviceKind::Ssd);
        assert_eq!(classify(None, None, None), DeviceKind::Unknown);
    }

    /// This Mac's work volume: APFS on its physical disk, whose counters move, with its health
    /// where it's NVMe.
    #[test]
    fn w13_r01_this_macs_storage() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = crate::sense::canonical(dir.path());
        let s = facts(&path).ok_or_else(|| anyhow::anyhow!("no storage"))?;
        assert_eq!(s.fs.as_deref(), Some("apfs"), "{s:?}");
        assert!(!s.network && !s.devices.is_empty(), "{s:?}");
        if cfg!(target_arch = "aarch64") {
            let nvme = s.devices.iter().find(|d| d.kind == DeviceKind::Nvme);
            assert!(nvme.is_some_and(|d| d.name.starts_with("disk") && d.size.is_some_and(|z| z > 0)), "{s:?}");
            assert!(nvme.and_then(|d| d.health.as_ref()).is_some_and(|h| h.available_spare.is_some() && h.percentage_used.is_some()), "{s:?}");
        }
        let c = Device::of(&path).and_then(|mut d| d.counters());
        assert!(c.is_some_and(|c| c.read.ops > 0 && c.read.time_ns > 0 && c.errors.errors.is_some()), "{c:?}");
        Ok(())
    }
}
