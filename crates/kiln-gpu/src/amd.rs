//! AMD's GPUs on Linux through amdgpu's sysfs counters (`docs/design/gpu-telemetry.md` G2). [`open`]
//! lists the DRM class's cards that amdgpu drives, reads each one's identity once and opens its two
//! counters; [`Card::sample`] then makes one `pread` per counter. No ioctl, no device node, no library:
//! the counters are world-readable sysfs attributes.

#[path = "../../../vendor/all-smi/crates/all-smi-amd-plugin/src/reader.rs"]
mod counter;
#[cfg(test)]
mod tests;

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::{Absent, Gpu, Identity, Load, PciAddress, Skipped, Unread, Vendor};

/// The kernel's longest release (`__NEW_UTS_LEN`, include/uapi/linux/utsname.h) and its newline: an
/// in-tree amdgpu's version is the kernel release, and a DKMS one's is shorter. A longer version is
/// refused as malformed.
const VERSION_LINE: usize = 65;
/// A sysfs id line: `0x` and four hex digits, and the newline (`device`, `revision`).
const ID_LINE: usize = 7;
/// The longest model name Hex keeps: the 95 bytes NVML's name buffer holds for NVIDIA's
/// (`NVML_DEVICE_NAME_V2_BUFFER_SIZE`, its NUL less). A longer name in libdrm's table isn't taken.
const NAME_MAX: usize = 95;
/// One line of libdrm's table that names a card: four hex digits of device id, two of revision, the
/// separators, a name up to [`NAME_MAX`] and the newline. A longer line names nothing Hex can use.
const TABLE_LINE: u64 = 106;
/// PCI's vendor id for AMD (ATI's, which amdgpu answers to).
const AMD: u16 = 0x1002;

/// Where the kernel and libdrm keep what Hex reads: [`Paths::system`] on a node, a fixture's in the
/// tests.
pub(crate) struct Paths<'a> {
    /// The DRM class: one `card<N>` per GPU a DRM driver drives.
    pub(crate) drm: &'a Path,
    /// libdrm's table of AMD's marketing names, which all-smi's AMD reader shows (through
    /// libamdgpu_top and libdrm's `amdgpu_get_marketing_name`).
    pub(crate) ids: &'a Path,
    /// A DKMS-built amdgpu's version; the in-tree driver has none.
    pub(crate) module_version: &'a Path,
    /// The kernel's release, the in-tree driver's version.
    pub(crate) os_release: &'a Path,
}

impl Paths<'static> {
    /// Where they are on a node.
    pub(crate) fn system() -> Paths<'static> {
        Paths {
            drm: Path::new("/sys/class/drm"),
            ids: Path::new("/usr/share/libdrm/amdgpu.ids"),
            module_version: Path::new("/sys/module/amdgpu/version"),
            os_release: Path::new("/proc/sys/kernel/osrelease"),
        }
    }
}

/// One card's counters, opened once and kept.
#[derive(Debug)]
pub(crate) struct Card<F> {
    busy: F,
    vram_used: F,
}

impl<F: FileExt> Card<F> {
    /// This GPU's load now: one read of `gpu_busy_percent`, one of `mem_info_vram_used`; a card the
    /// first finds gone isn't read again.
    pub(crate) fn sample(&self) -> Load {
        let busy_percent = counter::read_gpu_busy_percent(&self.busy);
        let memory_used_bytes = match busy_percent {
            Err(Unread::Gone) => Err(Unread::Gone),
            _ => counter::read_vram_bytes(&self.vram_used),
        };
        Load {
            busy_percent,
            memory_used_bytes,
        }
    }
}

/// What [`open`] found: the GPUs it reads, in PCI order, each beside its open counters, and the amdgpu
/// cards it can't read.
pub(crate) struct Found {
    pub(crate) gpus: Vec<(Gpu, Card<File>)>,
    pub(crate) skipped: Vec<Skipped>,
}

/// Lists the cards amdgpu drives. The listing is the kernel's, one entry per DRM node (DRM numbers at
/// most 2^20 cards: `DRM_EXTENDED_MINOR_LIMIT`, drivers/gpu/drm/drm_drv.c), and each card found holds
/// two descriptors against the process's `RLIMIT_NOFILE`, an open past which the OS refuses (`EMFILE`),
/// that card skipped with why.
pub(crate) fn open(at: &Paths) -> Result<Found, Absent> {
    let entries = fs::read_dir(at.drm).map_err(|e| Absent::Os { errno: errno(&e) })?;
    let mut gpus = Vec::new();
    let mut skipped = Vec::new();
    let mut driver_version = None;
    for entry in entries {
        let entry = entry.map_err(|e| Absent::Os { errno: errno(&e) })?;
        // Connectors (`card1-DP-1`), render nodes and the class's own files aren't cards.
        let Some(card) = card_number(&entry.file_name()) else {
            continue;
        };
        let device = entry.path().join("device");
        match driven_by_amdgpu(&device) {
            Ok(true) => {}
            // Another vendor's card (NVIDIA's are NVML's) or a firmware framebuffer.
            Ok(false) => continue,
            Err(why) => {
                skipped.push(Skipped::Drm { card, why });
                continue;
            }
        }
        match identify(&device, at, &mut driver_version) {
            Ok(found) => gpus.push(found),
            Err(why) => skipped.push(Skipped::Drm { card, why }),
        }
    }
    gpus.sort_by_key(|(gpu, _)| gpu.identity);
    Ok(Found { gpus, skipped })
}

/// `card<N>` alone, its number.
fn card_number(name: &std::ffi::OsStr) -> Option<u32> {
    let digits = name.to_str()?.strip_prefix("card")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Whether the card's device is bound to amdgpu, by its driver link's name. amdgpu drives AMD's GPUs
/// alone, and only it offers the counters read here.
fn driven_by_amdgpu(device: &Path) -> Result<bool, Unread> {
    match fs::read_link(device.join("driver")) {
        Ok(driver) => Ok(driver.file_name().is_some_and(|name| name == "amdgpu")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(os(&e)),
    }
}

/// The card's identity, read once, and its counters, opened once. The driver's version is read at the
/// first card and kept for the rest.
fn identify(
    device: &Path,
    at: &Paths,
    driver_version: &mut Option<String>,
) -> Result<(Gpu, Card<File>), Unread> {
    // The device link names the PCI device: `../../../0000:79:00.0`.
    let link = fs::read_link(device).map_err(|e| os(&e))?;
    let pci = link
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(PciAddress::parse)
        .ok_or(Unread::Malformed)?;
    let device_id =
        u16::try_from(hex_id(&device.join("device"))?).map_err(|_| Unread::Malformed)?;
    let revision =
        u8::try_from(hex_id(&device.join("revision"))?).map_err(|_| Unread::Malformed)?;
    let total = open_counter(&device.join("mem_info_vram_total"))?;
    let memory_total_bytes = counter::read_vram_bytes(&total)?;
    let card = Card {
        busy: open_counter(&device.join("gpu_busy_percent"))?,
        vram_used: open_counter(&device.join("mem_info_vram_used"))?,
    };
    let driver_version = match driver_version {
        Some(version) => version.clone(),
        None => driver_version.insert(version(at)?).clone(),
    };
    let gpu = Gpu {
        vendor: Vendor::Amd,
        model: model(at.ids, device_id, revision),
        memory_total_bytes,
        identity: Identity::Pci(pci),
        driver_version,
    };
    Ok((gpu, card))
}

/// A counter's file, kept open. A kernel or card without it doesn't offer it.
fn open_counter(path: &Path) -> Result<File, Unread> {
    File::open(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => Unread::Unsupported,
        _ => os(&e),
    })
}

/// A sysfs id: `0x1002`.
fn hex_id(path: &Path) -> Result<u32, Unread> {
    let mut line = [0; ID_LINE];
    let text = read_line(path, &mut line)?;
    let digits = text.strip_prefix("0x").ok_or(Unread::Malformed)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Unread::Malformed);
    }
    u32::from_str_radix(digits, 16).map_err(|_| Unread::Malformed)
}

/// amdgpu's version: a DKMS-built module's own, else the kernel's release: the in-tree driver has no
/// version of its own (no `/sys/module/amdgpu/version`), being built with the kernel.
fn version(at: &Paths) -> Result<String, Unread> {
    let mut line = [0; VERSION_LINE];
    match read_line(at.module_version, &mut line) {
        Ok(version) => Ok(version.to_owned()),
        Err(Unread::Unsupported) => {
            let mut line = [0; VERSION_LINE];
            read_line(at.os_release, &mut line).map(str::to_owned)
        }
        Err(why) => Err(why),
    }
}

/// A short text file's one line, read whole into `buffer`: its text without the newline, UTF-8 and
/// not empty. A file that fills the buffer without a newline is too long, refused as malformed.
fn read_line<'b>(path: &Path, buffer: &'b mut [u8]) -> Result<&'b str, Unread> {
    let mut file = open_counter(path)?;
    let mut filled = 0;
    // A short file arrives in one read; the loop only carries a read the OS split, each one filling
    // more of the buffer, so it ends within the buffer's length.
    while let Some(rest) = buffer.get_mut(filled..).filter(|rest| !rest.is_empty()) {
        match file.read(rest) {
            Ok(0) => break,
            Ok(read) => filled = filled.checked_add(read).ok_or(Unread::Malformed)?,
            Err(e) => return Err(os(&e)),
        }
    }
    let (line, _) = buffer.split_at_checked(filled).ok_or(Unread::Malformed)?;
    let text = line.strip_suffix(b"\n").ok_or(Unread::Malformed)?;
    std::str::from_utf8(text)
        .ok()
        .filter(|t| !t.is_empty() && !t.contains('\n'))
        .ok_or(Unread::Malformed)
}

/// The card's marketing name from libdrm's table (`device_id,\trevision_id,\tproduct_name`), else its
/// PCI ids as lspci prints them (`1002:13c0 rev e5`): the table may lag a new part, or be absent where
/// libdrm isn't installed.
fn model(ids: &Path, device: u16, revision: u8) -> String {
    match named(ids, device, revision) {
        Some(name) => name,
        None => format!("{AMD:04x}:{device:04x} rev {revision:02x}"),
    }
}

/// The table's name for (device, revision), streamed a line at a time through a fixed buffer, so the
/// file's size costs time, never memory.
fn named(ids: &Path, device: u16, revision: u8) -> Option<String> {
    let mut table = BufReader::new(File::open(ids).ok()?);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut table)
            .take(TABLE_LINE)
            .read_until(b'\n', &mut line)
            .ok()?;
        if read == 0 {
            return None;
        }
        if line.last() != Some(&b'\n') {
            // Longer than any line that names a card: drop the rest of it.
            skip_line(&mut table)?;
            continue;
        }
        if let Some(name) = entry(&line, device, revision) {
            return Some(name.to_owned());
        }
    }
}

/// Reads past the end of the current line, through the reader's own buffer.
fn skip_line(table: &mut impl BufRead) -> Option<()> {
    loop {
        let buffered = table.fill_buf().ok()?;
        if buffered.is_empty() {
            return Some(());
        }
        let (consumed, ended) = match buffered.iter().position(|b| *b == b'\n') {
            Some(at) => (at.checked_add(1)?, true),
            None => (buffered.len(), false),
        };
        table.consume(consumed);
        if ended {
            return Some(());
        }
    }
}

/// One table line's name when it names (device, revision): `13C0,\tE5,\tAMD Radeon 610M\n`. Comments,
/// the version line and every other card's line name nothing.
fn entry(line: &[u8], device: u16, revision: u8) -> Option<&str> {
    let line = std::str::from_utf8(line.strip_suffix(b"\n")?).ok()?;
    let mut fields = line.splitn(3, ',');
    let id = fields.next()?.trim();
    let rev = fields.next()?.trim();
    let name = fields.next()?.trim();
    let hex = |s: &str, digits: usize| {
        (s.len() == digits && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| u32::from_str_radix(s, 16).ok())
            .flatten()
    };
    let named = hex(id, 4)? == u32::from(device)
        && hex(rev, 2)? == u32::from(revision)
        && !name.is_empty()
        && name.len() <= NAME_MAX;
    if named { Some(name) } else { None }
}

fn errno(error: &io::Error) -> i32 {
    error.raw_os_error().unwrap_or_default()
}

fn os(error: &io::Error) -> Unread {
    Unread::Os {
        errno: errno(error),
    }
}
