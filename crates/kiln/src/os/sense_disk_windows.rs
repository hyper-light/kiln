//! Windows: the work volume and the physical drives under it, found and read without privilege
//! (docs/needs.md; research-disk.md). Every call here works with a restricted token: the drives
//! are opened with no access at all (`CreateFileW` with access 0), which the `FILE_ANY_ACCESS`
//! I/O controls below accept.
//!
//! - The volume: `GetVolumePathNameW` (its root, `C:\`), `GetVolumeNameForVolumeMountPointW`
//!   (`\\?\Volume{…}\`), `GetDriveTypeW` (`DRIVE_REMOTE`: a network share, no drive of its own),
//!   `GetVolumeInformationW` (its file system: NTFS, ReFS).
//! - Its drives: `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` on the volume, then `\\.\PhysicalDriveN`.
//! - Each drive's kind: `IOCTL_STORAGE_QUERY_PROPERTY`'s `StorageDeviceProperty` (its bus type,
//!   vendor and product) first, `StorageDeviceSeekPenaltyProperty` (whether it rotates) after;
//!   its size from `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`.
//! - An NVMe drive's health: its SMART / Health Information log page, through the same query's
//!   `StorageDeviceProtocolSpecificProperty` (Microsoft, "Working with NVMe drives").
//! - Counters: `IOCTL_DISK_PERFORMANCE` (bytes, operations, read and write time, idle time). Its
//!   time unit is undocumented, so it's calibrated: the structure's own timestamp (`QueryTime`)
//!   is read against the monotonic clock from the drive's first read on, and every time is
//!   converted at that rate (its error is one tick of `QueryTime` over the time since the first
//!   read, and shrinks as that grows). The idle time is taken to keep `QueryTime`'s unit. Until a
//!   second read there is no rate, so no counters.
//!
//! What stays unread without administrator rights: error counters (`StorageReliabilityCounter`
//! needs them, and Windows keeps no unprivileged per-drive error count), so `errors` is left
//! empty; Windows' performance counters through PDH, which a non-interactive session can't open.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use std::time::Instant;

use kiln_ci::facts::{DeviceHealth, DeviceKind, Storage, StorageDevice};

use crate::sense::disk::{Counters, Dir, Errors};

const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const OPEN_EXISTING: u32 = 3;
const INVALID_HANDLE_VALUE: isize = -1;
const DRIVE_REMOTE: u32 = 4;
const ERROR_MORE_DATA: i32 = 234;
const ERROR_INSUFFICIENT_BUFFER: i32 = 122;

/// `CTL_CODE(IOCTL_VOLUME_BASE, 0, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;
/// `CTL_CODE(IOCTL_DISK_BASE, 0x0008, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
const IOCTL_DISK_PERFORMANCE: u32 = 0x0007_0020;
/// `CTL_CODE(IOCTL_DISK_BASE, 0x0028, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
const IOCTL_DISK_GET_DRIVE_GEOMETRY_EX: u32 = 0x0007_00A0;
/// `CTL_CODE(IOCTL_STORAGE_BASE, 0x0500, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;

/// `STORAGE_PROPERTY_ID`s and `STORAGE_QUERY_TYPE::PropertyStandardQuery`.
const STORAGE_DEVICE_PROPERTY: u32 = 0;
const STORAGE_DEVICE_SEEK_PENALTY_PROPERTY: u32 = 7;
const STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY: u32 = 50;
const PROPERTY_STANDARD_QUERY: u32 = 0;
/// `STORAGE_PROTOCOL_TYPE::ProtocolTypeNvme`, `STORAGE_PROTOCOL_NVME_DATA_TYPE::NVMeDataTypeLogPage`,
/// `NVME_LOG_PAGE_HEALTH_INFO`, and the page's size (`sizeof(NVME_HEALTH_INFO_LOG)`).
const PROTOCOL_TYPE_NVME: u32 = 3;
const NVME_DATA_TYPE_LOG_PAGE: u32 = 2;
const NVME_LOG_PAGE_HEALTH_INFO: u32 = 2;
const NVME_HEALTH_INFO_LOG_SIZE: usize = 512;
/// `STORAGE_BUS_TYPE::BusTypeNvme`.
const BUS_TYPE_NVME: u32 = 17;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetVolumePathNameW(file: *const u16, volume: *mut u16, len: u32) -> i32;
    fn GetVolumeNameForVolumeMountPointW(mount: *const u16, name: *mut u16, len: u32) -> i32;
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn GetVolumeInformationW(root: *const u16, name: *mut u16, name_len: u32, serial: *mut u32, max_component: *mut u32, flags: *mut u32, fs: *mut u16, fs_len: u32) -> i32;
    fn CreateFileW(name: *const u16, access: u32, share: u32, security: *mut c_void, disposition: u32, flags: u32, template: *mut c_void) -> *mut c_void;
    fn DeviceIoControl(device: *mut c_void, code: u32, input: *const c_void, input_len: u32, output: *mut c_void, output_len: u32, returned: *mut u32, overlapped: *mut c_void) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

/// `DISK_PERFORMANCE` (winioctl.h).
// Laid out as the SDK's, for its offsets: not every field is read.
#[allow(dead_code)]
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DiskPerformance {
    bytes_read: i64,
    bytes_written: i64,
    read_time: i64,
    write_time: i64,
    idle_time: i64,
    read_count: u32,
    write_count: u32,
    queue_depth: u32,
    split_count: u32,
    query_time: i64,
    storage_device_number: u32,
    storage_manager_name: [u16; 8],
}

const _: () = {
    assert!(size_of::<DiskPerformance>() == 88);
    assert!(offset_of!(DiskPerformance, idle_time) == 32);
    assert!(offset_of!(DiskPerformance, read_count) == 40);
    assert!(offset_of!(DiskPerformance, query_time) == 56);
    assert!(offset_of!(DiskPerformance, storage_device_number) == 64);
    assert!(offset_of!(DiskPerformance, storage_manager_name) == 68);
};

/// `DISK_EXTENT` and the head of `VOLUME_DISK_EXTENTS` (its extents follow, from `Extents`'s
/// offset, each a `DISK_EXTENT`).
// Laid out as the SDK's, for its offsets: not every field is read.
#[allow(dead_code)]
#[repr(C)]
struct DiskExtent {
    disk_number: u32,
    starting_offset: i64,
    extent_length: i64,
}

// Laid out as the SDK's, for its offsets: not every field is read.
#[allow(dead_code)]
#[repr(C)]
struct VolumeDiskExtents {
    number_of_disk_extents: u32,
    extents: [DiskExtent; 1],
}

const _: () = {
    assert!(size_of::<DiskExtent>() == 24);
    assert!(offset_of!(DiskExtent, disk_number) == 0);
    assert!(size_of::<VolumeDiskExtents>() == 32);
    assert!(offset_of!(VolumeDiskExtents, extents) == 8);
};

/// `STORAGE_PROPERTY_QUERY`: its `AdditionalParameters` is where a protocol query's
/// `STORAGE_PROTOCOL_SPECIFIC_DATA` goes.
// Laid out as the SDK's, for its offsets: not every field is read.
#[allow(dead_code)]
#[repr(C)]
struct StoragePropertyQuery {
    property_id: u32,
    query_type: u32,
    additional_parameters: [u8; 1],
}

/// `STORAGE_DEVICE_DESCRIPTOR`'s fixed part.
// Laid out as the SDK's, for its offsets: not every field is read.
#[allow(dead_code)]
#[repr(C)]
struct StorageDeviceDescriptor {
    version: u32,
    size: u32,
    device_type: u8,
    device_type_modifier: u8,
    removable_media: u8,
    command_queueing: u8,
    vendor_id_offset: u32,
    product_id_offset: u32,
    product_revision_offset: u32,
    serial_number_offset: u32,
    bus_type: u32,
    raw_properties_length: u32,
    raw_device_properties: [u8; 1],
}

/// `DEVICE_SEEK_PENALTY_DESCRIPTOR`.
// Laid out as the SDK's, for its offsets: not every field is read.
#[allow(dead_code)]
#[repr(C)]
#[derive(Default)]
struct SeekPenalty {
    version: u32,
    size: u32,
    incurs_seek_penalty: u8,
}

/// `STORAGE_PROTOCOL_SPECIFIC_DATA` (as of the Windows 10 1903 SDK; the same 40 bytes before it,
/// its last three fields then reserved).
// Laid out as the SDK's, for its offsets: not every field is read.
#[allow(dead_code)]
#[repr(C)]
struct StorageProtocolSpecificData {
    protocol_type: u32,
    data_type: u32,
    protocol_data_request_value: u32,
    protocol_data_request_sub_value: u32,
    protocol_data_offset: u32,
    protocol_data_length: u32,
    fixed_protocol_return_data: u32,
    protocol_data_request_sub_value2: u32,
    protocol_data_request_sub_value3: u32,
    protocol_data_request_sub_value4: u32,
}

/// `STORAGE_PROTOCOL_DATA_DESCRIPTOR`: the reply to a protocol query.
// Laid out as the SDK's, for its offsets: not every field is read.
#[allow(dead_code)]
#[repr(C)]
struct StorageProtocolDataDescriptor {
    version: u32,
    size: u32,
    protocol_specific_data: StorageProtocolSpecificData,
}

/// `DISK_GEOMETRY_EX`'s head: `DISK_GEOMETRY`, then the disk's size.
// Laid out as the SDK's, for its offsets: not every field is read.
#[allow(dead_code)]
#[repr(C)]
struct DiskGeometryEx {
    cylinders: i64,
    media_type: u32,
    tracks_per_cylinder: u32,
    sectors_per_track: u32,
    bytes_per_sector: u32,
    disk_size: i64,
    data: [u8; 1],
}

const _: () = {
    assert!(size_of::<StoragePropertyQuery>() == 12);
    assert!(offset_of!(StoragePropertyQuery, additional_parameters) == 8);
    assert!(offset_of!(StorageDeviceDescriptor, vendor_id_offset) == 12);
    assert!(offset_of!(StorageDeviceDescriptor, product_id_offset) == 16);
    assert!(offset_of!(StorageDeviceDescriptor, bus_type) == 28);
    assert!(size_of::<StorageDeviceDescriptor>() == 40);
    assert!(size_of::<SeekPenalty>() == 12);
    assert!(offset_of!(SeekPenalty, incurs_seek_penalty) == 8);
    assert!(size_of::<StorageProtocolSpecificData>() == 40);
    assert!(offset_of!(StorageProtocolSpecificData, protocol_data_offset) == 16);
    assert!(offset_of!(StorageProtocolSpecificData, protocol_data_length) == 20);
    assert!(size_of::<StorageProtocolDataDescriptor>() == 48);
    assert!(offset_of!(StorageProtocolDataDescriptor, protocol_specific_data) == 8);
    assert!(offset_of!(DiskGeometryEx, disk_size) == 24);
};

/// A NUL-terminated UTF-16 string.
fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// UTF-16 up to its first NUL.
fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(buf.get(..end).unwrap_or_default())
}

fn u32_at(buf: &[u8], at: usize) -> Option<u32> {
    buf.get(at..at + 4).and_then(|s| s.try_into().ok()).map(u32::from_le_bytes)
}

fn i64_at(buf: &[u8], at: usize) -> Option<i64> {
    buf.get(at..at + 8).and_then(|s| s.try_into().ok()).map(i64::from_le_bytes)
}

fn put_u32(buf: &mut [u8], at: usize, v: u32) {
    if let Some(s) = buf.get_mut(at..at + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

/// An ASCII string at `offset` in a descriptor (none at offset 0), trimmed.
fn ascii_at(buf: &[u8], offset: u32) -> Option<String> {
    let start = usize::try_from(offset).ok().filter(|o| *o > 0)?;
    let rest = buf.get(start..)?;
    let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
    let s = String::from_utf8_lossy(rest.get(..end)?).trim().to_owned();
    (!s.is_empty()).then_some(s)
}

/// A device handle opened with no access, closed when dropped. Kept as its integer value, so a
/// reader holding it can move between threads (the kernel handle is process-wide).
struct Handle(isize);

impl Handle {
    /// `name` (`\\?\Volume{…}`, `\\.\PhysicalDrive0`) opened for I/O controls alone.
    fn open(name: &str) -> Option<Handle> {
        let w = wide(std::ffi::OsStr::new(name));
        // SAFETY: a NUL-terminated name; no access asked, sharing both ways, no security
        // attributes or template.
        let h = unsafe { CreateFileW(w.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null_mut(), OPEN_EXISTING, 0, std::ptr::null_mut()) } as isize;
        (h != INVALID_HANDLE_VALUE && h != 0).then_some(Handle(h))
    }

    /// The handle as the calls take it.
    fn raw(&self) -> *mut c_void {
        self.0 as *mut c_void
    }

    /// `DeviceIoControl(code)` with `input`, into `output`: the bytes returned, or the error.
    fn control(&self, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize, i32> {
        let mut returned = 0u32;
        let (in_len, out_len) = (u32::try_from(input.len()).unwrap_or(0), u32::try_from(output.len()).unwrap_or(0));
        // SAFETY: an open handle; buffers with their lengths; no OVERLAPPED (a synchronous call).
        let ok = unsafe { DeviceIoControl(self.raw(), code, input.as_ptr().cast(), in_len, output.as_mut_ptr().cast(), out_len, &raw mut returned, std::ptr::null_mut()) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
        }
        Ok(usize::try_from(returned).unwrap_or(0))
    }

    fn performance(&self) -> Option<DiskPerformance> {
        let mut p = DiskPerformance::default();
        let mut returned = 0u32;
        let len = u32::try_from(size_of::<DiskPerformance>()).ok()?;
        // SAFETY: an open handle; a whole DISK_PERFORMANCE as the output, no input.
        let ok = unsafe { DeviceIoControl(self.raw(), IOCTL_DISK_PERFORMANCE, std::ptr::null(), 0, (&raw mut p).cast(), len, &raw mut returned, std::ptr::null_mut()) };
        (ok != 0 && usize::try_from(returned).ok() == Some(size_of::<DiskPerformance>())).then_some(p)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a handle this owns, closed once.
        unsafe { CloseHandle(self.raw()) };
    }
}

/// The work volume: its root, its volume name, whether it's a network share, its file system.
struct Volume {
    root: String,
    name: Option<String>,
    remote: bool,
    fs: Option<String>,
}

/// `\\?\C:\…` as the plain `C:\…` the volume calls take; a `\\?\UNC\server\share` as
/// `\\server\share`.
fn plain(path: &Path) -> std::ffi::OsString {
    let s = path.as_os_str().to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => match rest.strip_prefix(r"UNC\") {
            Some(unc) => format!(r"\\{unc}").into(),
            None => rest.to_owned().into(),
        },
        None => path.as_os_str().to_owned(),
    }
}

fn volume(dir: &Path) -> Option<Volume> {
    let path = wide(&plain(dir));
    let mut root = [0u16; 1024];
    // SAFETY: a NUL-terminated path, and a buffer of its stated length.
    if unsafe { GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), u32::try_from(root.len()).ok()?) } == 0 {
        return None;
    }
    let root_s = from_wide(&root);
    let root_w = wide(std::ffi::OsStr::new(&root_s));
    // SAFETY: a NUL-terminated root ending in a backslash.
    let remote = unsafe { GetDriveTypeW(root_w.as_ptr()) } == DRIVE_REMOTE;
    let mut name = [0u16; 128];
    // SAFETY: the root (with its trailing backslash), and a buffer of its stated length.
    let named = !remote && unsafe { GetVolumeNameForVolumeMountPointW(root_w.as_ptr(), name.as_mut_ptr(), u32::try_from(name.len()).ok()?) } != 0;
    let mut fs = [0u16; 64];
    // SAFETY: the root, and the file system name's buffer with its length; the rest not asked.
    let has_fs = unsafe { GetVolumeInformationW(root_w.as_ptr(), std::ptr::null_mut(), 0, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), fs.as_mut_ptr(), u32::try_from(fs.len()).ok()?) } != 0;
    Some(Volume { root: root_s, name: named.then(|| from_wide(&name)), remote, fs: has_fs.then(|| from_wide(&fs)).filter(|f| !f.is_empty()) })
}

/// The physical drives a volume (`\\?\Volume{…}\`) lies on, by number.
fn drives_of(volume_name: &str) -> Vec<u32> {
    let Some(h) = Handle::open(volume_name.trim_end_matches('\\')) else { return Vec::new() };
    let mut buf = vec![0u8; size_of::<VolumeDiskExtents>()];
    let returned = loop {
        match h.control(IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut buf) {
            Ok(n) => break n,
            // A volume over several drives: room for every extent its count says.
            Err(ERROR_MORE_DATA | ERROR_INSUFFICIENT_BUFFER) => {
                let count = u32_at(&buf, 0).and_then(|c| usize::try_from(c).ok()).unwrap_or(0);
                let want = offset_of!(VolumeDiskExtents, extents) + count.max(1) * size_of::<DiskExtent>();
                if count == 0 || want <= buf.len() {
                    return Vec::new();
                }
                buf.resize(want, 0);
            }
            Err(_) => return Vec::new(),
        }
    };
    let buf = buf.get(..returned).unwrap_or_default();
    let count = u32_at(buf, 0).and_then(|c| usize::try_from(c).ok()).unwrap_or(0);
    let mut out: Vec<u32> = (0..count).filter_map(|i| u32_at(buf, offset_of!(VolumeDiskExtents, extents) + i * size_of::<DiskExtent>() + offset_of!(DiskExtent, disk_number))).collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn drive_name(n: u32) -> String {
    format!(r"\\.\PhysicalDrive{n}")
}

/// A drive's kind and interconnect from its bus type (`STORAGE_BUS_TYPE`) first, whether it
/// incurs a seek penalty after: a virtual disk's seek penalty says nothing of what's under it.
pub fn classify(bus_type: u32, seek_penalty: Option<bool>) -> (DeviceKind, Option<String>) {
    let name = match bus_type {
        1 => "SCSI",
        2 => "ATAPI",
        3 => "ATA",
        4 => "1394",
        5 => "SSA",
        6 => "Fibre",
        7 => "USB",
        8 => "RAID",
        9 => "iSCSI",
        10 => "SAS",
        11 => "SATA",
        12 => "SD",
        13 => "MMC",
        14 => "Virtual",
        15 => "FileBackedVirtual",
        16 => "Spaces",
        17 => "NVMe",
        18 => "SCM",
        19 => "UFS",
        _ => "",
    };
    let by_medium = match seek_penalty {
        Some(true) => DeviceKind::Hdd,
        Some(false) => DeviceKind::Ssd,
        None => DeviceKind::Unknown,
    };
    let kind = match bus_type {
        BUS_TYPE_NVME => DeviceKind::Nvme,
        14 | 15 => DeviceKind::Virtual,
        6 | 9 => DeviceKind::Network,
        18 => DeviceKind::Ram,
        12 | 13 => DeviceKind::Ssd,
        _ => by_medium,
    };
    (kind, (!name.is_empty()).then(|| name.to_owned()))
}

/// A standard property query's reply, into `out`.
fn query(h: &Handle, property: u32, out: &mut [u8]) -> Option<usize> {
    let mut q = [0u8; size_of::<StoragePropertyQuery>()];
    put_u32(&mut q, offset_of!(StoragePropertyQuery, property_id), property);
    put_u32(&mut q, offset_of!(StoragePropertyQuery, query_type), PROPERTY_STANDARD_QUERY);
    h.control(IOCTL_STORAGE_QUERY_PROPERTY, &q, out).ok()
}

/// An NVMe drive's SMART / Health Information log page, as Microsoft's "Working with NVMe
/// drives" (learn.microsoft.com/windows/win32/fileio/working-with-nvme-devices, "Example: NVMe
/// Get Log Pages query") lays out the buffer: `STORAGE_PROPERTY_QUERY` up to its
/// `AdditionalParameters`, which holds `STORAGE_PROTOCOL_SPECIFIC_DATA` (log page, health
/// information, its data right after the structure, 512 bytes); the reply is a
/// `STORAGE_PROTOCOL_DATA_DESCRIPTOR` whose `ProtocolSpecificData` says where in it the page is.
fn nvme_health(h: &Handle) -> Option<DeviceHealth> {
    let spec = offset_of!(StoragePropertyQuery, additional_parameters);
    let data_size = size_of::<StorageProtocolSpecificData>();
    let mut buf = vec![0u8; spec + data_size + NVME_HEALTH_INFO_LOG_SIZE];
    put_u32(&mut buf, offset_of!(StoragePropertyQuery, property_id), STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY);
    put_u32(&mut buf, offset_of!(StoragePropertyQuery, query_type), PROPERTY_STANDARD_QUERY);
    put_u32(&mut buf, spec + offset_of!(StorageProtocolSpecificData, protocol_type), PROTOCOL_TYPE_NVME);
    put_u32(&mut buf, spec + offset_of!(StorageProtocolSpecificData, data_type), NVME_DATA_TYPE_LOG_PAGE);
    put_u32(&mut buf, spec + offset_of!(StorageProtocolSpecificData, protocol_data_request_value), NVME_LOG_PAGE_HEALTH_INFO);
    put_u32(&mut buf, spec + offset_of!(StorageProtocolSpecificData, protocol_data_request_sub_value), 0);
    put_u32(&mut buf, spec + offset_of!(StorageProtocolSpecificData, protocol_data_offset), u32::try_from(data_size).ok()?);
    put_u32(&mut buf, spec + offset_of!(StorageProtocolSpecificData, protocol_data_length), u32::try_from(NVME_HEALTH_INFO_LOG_SIZE).ok()?);
    let input = buf.clone();
    let n = h.control(IOCTL_STORAGE_QUERY_PROPERTY, &input, &mut buf).ok()?;
    let reply = buf.get(..n)?;
    nvme_page(reply)
}

/// The health page in a `STORAGE_PROTOCOL_DATA_DESCRIPTOR` reply, checked as the sample checks
/// it: the descriptor's version and size, and an offset and length that hold a whole page.
fn nvme_page(reply: &[u8]) -> Option<DeviceHealth> {
    let descriptor = u32::try_from(size_of::<StorageProtocolDataDescriptor>()).ok()?;
    if u32_at(reply, offset_of!(StorageProtocolDataDescriptor, version))? != descriptor || u32_at(reply, offset_of!(StorageProtocolDataDescriptor, size))? != descriptor {
        return None;
    }
    let data = offset_of!(StorageProtocolDataDescriptor, protocol_specific_data);
    let offset = usize::try_from(u32_at(reply, data + offset_of!(StorageProtocolSpecificData, protocol_data_offset))?).ok()?;
    let length = usize::try_from(u32_at(reply, data + offset_of!(StorageProtocolSpecificData, protocol_data_length))?).ok()?;
    if offset < size_of::<StorageProtocolSpecificData>() || length < NVME_HEALTH_INFO_LOG_SIZE {
        return None;
    }
    DeviceHealth::of_nvme_log(reply.get(data + offset..data + offset + NVME_HEALTH_INFO_LOG_SIZE)?)
}

/// One physical drive's facts.
fn drive_facts(n: u32) -> StorageDevice {
    let name = format!("PhysicalDrive{n}");
    let Some(h) = Handle::open(&drive_name(n)) else { return StorageDevice { name, ..StorageDevice::default() } };
    let mut d = vec![0u8; 1024];
    let got = query(&h, STORAGE_DEVICE_PROPERTY, &mut d).unwrap_or(0);
    let d = d.get(..got).unwrap_or_default();
    let bus = u32_at(d, offset_of!(StorageDeviceDescriptor, bus_type));
    let vendor = u32_at(d, offset_of!(StorageDeviceDescriptor, vendor_id_offset)).and_then(|o| ascii_at(d, o));
    let product = u32_at(d, offset_of!(StorageDeviceDescriptor, product_id_offset)).and_then(|o| ascii_at(d, o));
    let mut p = [0u8; size_of::<SeekPenalty>()];
    let rotational = query(&h, STORAGE_DEVICE_SEEK_PENALTY_PROPERTY, &mut p).filter(|n| *n >= size_of::<SeekPenalty>()).and_then(|_| p.get(offset_of!(SeekPenalty, incurs_seek_penalty)).map(|b| *b != 0));
    let (kind, interconnect) = classify(bus.unwrap_or(0), rotational);
    let model = match (vendor, product) {
        (Some(v), Some(p)) if !p.starts_with(&v) => Some(format!("{v} {p}")),
        (v, p) => p.or(v),
    };
    let mut g = [0u8; size_of::<DiskGeometryEx>()];
    let size = h.control(IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut g).ok().filter(|n| *n >= offset_of!(DiskGeometryEx, disk_size) + 8).and_then(|_| i64_at(&g, offset_of!(DiskGeometryEx, disk_size))).and_then(|s| u64::try_from(s).ok());
    let health = (bus == Some(BUS_TYPE_NVME)).then(|| nvme_health(&h)).flatten();
    StorageDevice { name, kind, interconnect, rotational, model, size, location: None, health }
}

/// The work directory's storage (docs/needs.md, W13-R01): its volume's file system, and each
/// physical drive under it.
pub fn facts(work_dir: &Path) -> Option<Storage> {
    let v = volume(work_dir)?;
    let devices = if v.remote {
        vec![StorageDevice { name: v.root.clone(), kind: DeviceKind::Network, interconnect: v.fs.clone(), ..StorageDevice::default() }]
    } else {
        v.name.as_deref().map(drives_of).unwrap_or_default().into_iter().map(drive_facts).collect()
    };
    Some(Storage { fs: v.fs, mount: Some(v.root), source: v.name, network: v.remote, devices })
}

/// One drive's counters as read, and what calibrates them.
struct Drive {
    handle: Handle,
    /// The first read's `QueryTime` and idle time, and when it was made.
    anchor: Option<(i64, i64, Instant)>,
    /// The 32-bit operation counts as last read, and their sums since the first read.
    counts: Option<(u32, u32)>,
    reads: u64,
    writes: u64,
}

/// The work volume's drives, as found once: their handles.
pub struct Device {
    drives: Vec<Drive>,
}

impl Device {
    pub fn of(work_dir: &Path) -> Option<Device> {
        let v = volume(work_dir).filter(|v| !v.remote)?;
        let drives: Vec<Drive> = drives_of(v.name.as_deref()?).into_iter().filter_map(|n| Handle::open(&drive_name(n))).map(|handle| Drive { handle, anchor: None, counts: None, reads: 0, writes: 0 }).collect();
        (!drives.is_empty()).then_some(Device { drives })
    }

    /// The drives' counters now, summed (a volume over several), with times in ns at their
    /// calibrated rate; none until each drive has been read twice (no rate yet).
    pub fn counters(&mut self) -> Option<Counters> {
        let single = self.drives.len() == 1;
        let mut total = Counters { busy_ns: if single { Some(0) } else { None }, ..Counters::default() };
        let mut calibrated = true;
        for d in &mut self.drives {
            let p = d.handle.performance()?;
            let now = Instant::now();
            // The 32-bit counts wrap: their sums since the first read are kept in 64 bits.
            if let Some((r, w)) = d.counts {
                d.reads += u64::from(p.read_count.wrapping_sub(r));
                d.writes += u64::from(p.write_count.wrapping_sub(w));
            }
            d.counts = Some((p.read_count, p.write_count));
            let Some((q0, idle0, t0)) = d.anchor else {
                d.anchor = Some((p.query_time, p.idle_time, now));
                calibrated = false;
                continue;
            };
            let (dq, dt) = (p.query_time.saturating_sub(q0), now.duration_since(t0).as_secs_f64());
            if dq <= 0 || dt <= 0.0 {
                calibrated = false;
                continue;
            }
            // Units per second of QueryTime, and so of the times kept with it.
            let rate = dq as f64 / dt;
            let ns = |units: i64| (u64::try_from(units).unwrap_or(0) as f64 * 1e9 / rate) as u64;
            let bytes = |b: i64| u64::try_from(b).unwrap_or(0);
            total.read = Dir { ops: total.read.ops + d.reads, bytes: total.read.bytes + bytes(p.bytes_read), time_ns: total.read.time_ns + ns(p.read_time) };
            total.write = Dir { ops: total.write.ops + d.writes, bytes: total.write.bytes + bytes(p.bytes_written), time_ns: total.write.time_ns + ns(p.write_time) };
            // Busy: all time since the first read less the idle time since.
            let busy = dq.saturating_sub(p.idle_time.saturating_sub(idle0)).max(0);
            total.busy_ns = total.busy_ns.map(|b| b + ns(busy));
        }
        total.errors = Errors::default();
        calibrated.then_some(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docs/needs.md: a drive's kind is its bus's before its seek penalty's: a virtual disk that
    /// says it seeks is virtual, an NVMe drive is NVMe, an iSCSI one is the network's.
    #[test]
    fn w13_r01_drive_kind_by_bus() {
        assert_eq!(classify(17, Some(false)), (DeviceKind::Nvme, Some("NVMe".into())));
        assert_eq!(classify(14, Some(true)), (DeviceKind::Virtual, Some("Virtual".into())));
        assert_eq!(classify(15, None), (DeviceKind::Virtual, Some("FileBackedVirtual".into())));
        assert_eq!(classify(9, Some(true)), (DeviceKind::Network, Some("iSCSI".into())));
        assert_eq!(classify(11, Some(true)), (DeviceKind::Hdd, Some("SATA".into())));
        assert_eq!(classify(11, Some(false)), (DeviceKind::Ssd, Some("SATA".into())));
        assert_eq!(classify(7, None), (DeviceKind::Unknown, Some("USB".into())));
        assert_eq!(classify(99, None), (DeviceKind::Unknown, None));
    }

    /// The health page is found where the reply's descriptor says, and only a whole one.
    #[test]
    fn w13_r01_nvme_page_from_a_protocol_reply() {
        let data = size_of::<StorageProtocolSpecificData>();
        let mut reply = vec![0u8; 8 + data + NVME_HEALTH_INFO_LOG_SIZE];
        let d = u32::try_from(size_of::<StorageProtocolDataDescriptor>()).unwrap_or(0);
        put_u32(&mut reply, 0, d);
        put_u32(&mut reply, 4, d);
        put_u32(&mut reply, 8 + offset_of!(StorageProtocolSpecificData, protocol_data_offset), u32::try_from(data).unwrap_or(0));
        put_u32(&mut reply, 8 + offset_of!(StorageProtocolSpecificData, protocol_data_length), 512);
        if let Some(b) = reply.get_mut(8 + data + 5) {
            *b = 7;
        }
        assert_eq!(nvme_page(&reply).and_then(|h| h.percentage_used), Some(7));
        put_u32(&mut reply, 0, 1);
        assert_eq!(nvme_page(&reply), None, "another version");
    }

    #[test]
    fn w13_r01_paths_for_the_volume_calls() {
        assert_eq!(plain(Path::new(r"\\?\C:\work")), std::ffi::OsString::from(r"C:\work"));
        assert_eq!(plain(Path::new(r"\\?\UNC\srv\share\w")), std::ffi::OsString::from(r"\\srv\share\w"));
        assert_eq!(plain(Path::new(r"D:\w")), std::ffi::OsString::from(r"D:\w"));
    }

    /// This machine's work volume: a file system, and drives with a kind (on Windows, natively).
    #[test]
    fn w13_r01_this_machines_storage() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let s = facts(dir.path()).ok_or_else(|| anyhow::anyhow!("no storage for {}", dir.path().display()))?;
        assert!(s.fs.is_some() && s.mount.is_some(), "{s:?}");
        if let Some(mut d) = Device::of(dir.path()) {
            let _ = d.counters();
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(d.counters().is_some(), "counters once calibrated, for {s:?}");
        }
        Ok(())
    }
}
