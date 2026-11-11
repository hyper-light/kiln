// Copyright 2025 Lablup Inc. and Jeongkyu Shin
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// Changed by Hex (github.com/hyper-light/hex): only what hex-gpu's Windows AMD reading calls is kept,
// with the three ADL calls it adds declared beside the rest; hex-gpu includes this file by path
// (crates/hex-gpu/src/adl.rs). Each change and why is a row of vendor/all-smi/UPSTREAM.md.

//! Hand-declared ADL types and entry points.
//!
//! Only the symbols hex-gpu calls are declared. The signatures and layouts are transcribed from AMD's
//! public `adl_sdk.h`, `adl_structures.h` and `adl_defines.h` (the ADL SDK,
//! GPUOpen-LibrariesAndSDKs/display-library at `84c881cb`, MIT) and checked there; each size and
//! load-bearing offset is pinned below at compile time, and jupiter's driver (Radeon 8060S, ADL of
//! driver 25.20.32.06) filled each as declared.
//!
//! ## Buffers ADL writes by its own size
//!
//! `ADL2_New_QueryPMLogData_Get` takes a bare pointer with no length, so ADL sizes its write by its own
//! `sizeof` and a short buffer is a memory smash rather than an error. [`AdlPmLogDataBuffer`] gives a
//! larger table room to land, and [`AdlPmLogDataBuffer::validated`] refuses a response whose reported
//! size isn't this layout's. `ADL2_Adapter_AdapterInfo_Get` is told the buffer's size, and
//! [`AdapterInfoArray`] gives it the same headroom.
//!
//! The declared `AdapterInfo` is the **Windows** layout: `adl_structures.h` appends `iExist`, the two
//! driver-path strings, `strPNPString` and `iOSDisplayIndex` under `#if defined(_WIN32) ||
//! defined(_WIN64)`, so the struct is 1572 bytes here.
//!
//! ## Calling convention
//!
//! AMD declares the `ADL2_*` entry points as cdecl and only `ADL_MAIN_MALLOC_CALLBACK` as `__stdcall`,
//! so the entry points use `extern "C"` and the callback `extern "system"`. The two coincide on x86-64
//! and aarch64, Windows' targets.

use std::ffi::c_int;
#[cfg(target_os = "windows")]
use std::ffi::c_void;

/// `ADL_MAX_PATH` from `adl_defines.h`: the length of every string field declared here.
pub const ADL_MAX_PATH: usize = 256;

/// `ADL_PMLOG_MAX_SENSORS` from `adl_defines.h`.
pub const ADL_PMLOG_MAX_SENSORS: usize = 256;

/// `ADL_MAX_ADAPTERS` from `adl_defines.h`: the most adapter indices ADL numbers.
pub const ADL_MAX_ADAPTERS: c_int = 250;

/// `ADL_OK`, the success status shared by every ADL entry point; the positive `ADL_OK_*` codes
/// (warning, mode change, restart, wait) are successes too.
#[cfg(target_os = "windows")]
pub const ADL_OK: c_int = 0;

/// Opaque `ADL_CONTEXT_HANDLE`.
#[cfg(target_os = "windows")]
pub type AdlContextHandle = *mut c_void;

/// `ADL_MAIN_MALLOC_CALLBACK`.
#[cfg(target_os = "windows")]
pub type AdlMainMallocCallback = unsafe extern "system" fn(c_int) -> *mut c_void;

/// One entry of `ADLPMLogDataOutput::sensors`.
///
/// `supported` is a boolean flag; `value` is the reading, whose unit depends on the sensor index.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AdlSingleSensorData {
    pub supported: c_int,
    pub value: c_int,
}

/// `ADLPMLogDataOutput`: a fixed-size, sensor-index-addressed table.
///
/// The array is indexed by `ADLSensorType`, so entry `n` is the sensor whose enum value is `n`.
/// Entries the card does not publish have `supported == 0`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AdlPmLogDataOutput {
    /// `size` in the header.
    pub ul_size: c_int,
    pub sensors: [AdlSingleSensorData; ADL_PMLOG_MAX_SENSORS],
}

impl Default for AdlPmLogDataOutput {
    fn default() -> Self {
        Self {
            ul_size: 0,
            sensors: [AdlSingleSensorData::default(); ADL_PMLOG_MAX_SENSORS],
        }
    }
}

// Layout checks, at compile time: an array whose length is the declared size is assigned one whose
// length is the type's, so any difference fails the build.
const _: [(); 8] = [(); std::mem::size_of::<AdlSingleSensorData>()];
const _: [(); 4] = [(); std::mem::align_of::<AdlSingleSensorData>()];
const _: [(); 2052] = [(); std::mem::size_of::<AdlPmLogDataOutput>()];

/// A PMLog output buffer with trailing headroom: room for a table four times the declared one, so a
/// driver that writes a larger table writes into this buffer rather than past it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AdlPmLogDataBuffer {
    pub output: AdlPmLogDataOutput,
    /// Never read. Present so an oversized write lands in this buffer instead of in whatever
    /// follows it.
    _headroom: [AdlSingleSensorData; ADL_PMLOG_MAX_SENSORS * 3],
}

impl Default for AdlPmLogDataBuffer {
    fn default() -> Self {
        Self {
            output: AdlPmLogDataOutput::default(),
            _headroom: [AdlSingleSensorData::default(); ADL_PMLOG_MAX_SENSORS * 3],
        }
    }
}

impl AdlPmLogDataBuffer {
    /// The filled-in table, or `None` when the driver reported a size that does not correspond to the
    /// layout declared here. Drivers in the field have been observed leaving `size` at zero, so a zero
    /// is accepted; only a positive value that disagrees with this layout is refused, the signal that
    /// this file's ABI no longer matches the driver's.
    pub fn validated(&self) -> Option<&AdlPmLogDataOutput> {
        let reported = usize::try_from(self.output.ul_size).ok()?;
        if reported == 0 || reported == std::mem::size_of::<AdlPmLogDataOutput>() {
            return Some(&self.output);
        }
        None
    }
}

/// `AdapterInfo` from `adl_structures.h`, **Windows layout**.
///
/// Everything is a 32-bit `int` or a `char[ADL_MAX_PATH]` array, so the natural alignment is 4 and
/// there is no padding anywhere; the compile-time checks below pin every load-bearing offset and the
/// 1572-byte total. The string fields are declared `[u8; ADL_MAX_PATH]` rather than C's `char`: the two
/// have identical size and alignment.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterInfo {
    /// Size of the structure. Some ADL revisions fill it in, some leave the caller's zero.
    pub i_size: c_int,
    /// The ADL adapter index this row describes.
    pub i_adapter_index: c_int,
    /// Unique driver-assigned device identifier.
    pub str_udid: [u8; ADL_MAX_PATH],
    /// PCI bus number.
    pub i_bus_number: c_int,
    /// PCI device number.
    pub i_device_number: c_int,
    /// PCI function number.
    pub i_function_number: c_int,
    /// The PCI vendor id, written in decimal digits: AMD's is `1002` (AMD's own samples compare it so).
    pub i_vendor_id: c_int,
    /// Marketing name, e.g. "AMD Radeon(TM) 8060S Graphics".
    pub str_adapter_name: [u8; ADL_MAX_PATH],
    /// OS display name, e.g. `\\.\DISPLAY1`.
    pub str_display_name: [u8; ADL_MAX_PATH],
    pub _i_present: c_int,
    // --- Windows-only tail below ---
    pub _i_exist: c_int,
    pub _str_driver_path: [u8; ADL_MAX_PATH],
    pub _str_driver_path_ext: [u8; ADL_MAX_PATH],
    /// The Windows PNP device instance path, e.g. `PCI\VEN_1002&DEV_1586&SUBSYS_...\4&...&0041`.
    pub str_pnp_string: [u8; ADL_MAX_PATH],
    pub _i_os_display_index: c_int,
}

impl Default for AdapterInfo {
    fn default() -> Self {
        Self {
            i_size: 0,
            i_adapter_index: 0,
            str_udid: [0; ADL_MAX_PATH],
            i_bus_number: 0,
            i_device_number: 0,
            i_function_number: 0,
            i_vendor_id: 0,
            str_adapter_name: [0; ADL_MAX_PATH],
            str_display_name: [0; ADL_MAX_PATH],
            _i_present: 0,
            _i_exist: 0,
            _str_driver_path: [0; ADL_MAX_PATH],
            _str_driver_path_ext: [0; ADL_MAX_PATH],
            str_pnp_string: [0; ADL_MAX_PATH],
            _i_os_display_index: 0,
        }
    }
}

// AdapterInfo's layout: 9 ints (36 bytes) and 6 char[256] arrays (1536 bytes) are 1572, every field
// 4-aligned, no padding; the offsets the reading uses are pinned one by one.
const _: [(); 1572] = [(); std::mem::size_of::<AdapterInfo>()];
const _: [(); 4] = [(); std::mem::align_of::<AdapterInfo>()];
const _: [(); 8] = [(); std::mem::offset_of!(AdapterInfo, str_udid)];
const _: [(); 264] = [(); std::mem::offset_of!(AdapterInfo, i_bus_number)];
const _: [(); 268] = [(); std::mem::offset_of!(AdapterInfo, i_device_number)];
const _: [(); 272] = [(); std::mem::offset_of!(AdapterInfo, i_function_number)];
const _: [(); 276] = [(); std::mem::offset_of!(AdapterInfo, i_vendor_id)];
const _: [(); 280] = [(); std::mem::offset_of!(AdapterInfo, str_adapter_name)];
const _: [(); 536] = [(); std::mem::offset_of!(AdapterInfo, str_display_name)];
const _: [(); 792] = [(); std::mem::offset_of!(AdapterInfo, _i_present)];
const _: [(); 1312] = [(); std::mem::offset_of!(AdapterInfo, str_pnp_string)];
const _: [(); 1568] = [(); std::mem::offset_of!(AdapterInfo, _i_os_display_index)];

/// The NUL-terminated printable-ASCII prefix of an ADL string field, or `None` when the bytes do not
/// look like a string at all: no terminator anywhere, or a non-printable byte before it. Garbage here
/// is the signature of a misdeclared layout. An empty string (NUL at offset 0) is a valid string.
pub fn adl_string(bytes: &[u8]) -> Option<&str> {
    let (prefix, _) = bytes.split_at_checked(bytes.iter().position(|&b| b == 0)?)?;
    if !prefix.iter().all(|&b| (0x20..=0x7E).contains(&b)) {
        return None;
    }
    // Printable ASCII is valid UTF-8 by construction.
    std::str::from_utf8(prefix).ok()
}

impl AdapterInfo {
    /// Whether a written row is consistent with the layout declared above, by three independent
    /// signals a correct layout passes and a wrong one fails almost surely: the four identity strings
    /// are NUL-terminated printable ASCII (a shifted layout puts binary data there); `iSize`, when the
    /// driver fills it in, is this layout's; `iAdapterIndex` is an index ADL can number rather than
    /// the bytes of a string that landed there.
    pub fn looks_sane(&self) -> bool {
        let size_ok = self.i_size == 0
            || usize::try_from(self.i_size).ok() == Some(std::mem::size_of::<Self>());
        let index_ok = (0..ADL_MAX_ADAPTERS).contains(&self.i_adapter_index);
        size_ok
            && index_ok
            && adl_string(&self.str_udid).is_some()
            && adl_string(&self.str_adapter_name).is_some()
            && adl_string(&self.str_display_name).is_some()
            && adl_string(&self.str_pnp_string).is_some()
    }
}

/// An `AdapterInfo` array with trailing headroom for `ADL2_Adapter_AdapterInfo_Get`.
///
/// That entry point takes an `iInputSize` parameter, so the driver is *told* how large the buffer is.
/// The headroom (a full second copy of the requested rows, plus slack) is kept anyway: a driver that
/// sizes its write by its own larger `sizeof` in disregard of `iInputSize` still lands inside this
/// allocation instead of behind it.
pub struct AdapterInfoArray {
    entries: Vec<AdapterInfo>,
    requested: usize,
}

impl AdapterInfoArray {
    /// Room for `requested` rows and the headroom, or `None` past what memory can hold.
    pub fn for_count(requested: usize) -> Option<Self> {
        let allocated = requested.checked_mul(2)?.checked_add(4)?;
        Some(Self {
            entries: vec![AdapterInfo::default(); allocated],
            requested,
        })
    }

    #[cfg(target_os = "windows")]
    pub fn as_mut_ptr(&mut self) -> *mut AdapterInfo {
        self.entries.as_mut_ptr()
    }

    /// The `iInputSize` to pass: the requested rows' bytes, or `None` past `int`.
    #[cfg(target_os = "windows")]
    pub fn input_size(&self) -> Option<c_int> {
        self.requested
            .checked_mul(std::mem::size_of::<AdapterInfo>())
            .and_then(|bytes| c_int::try_from(bytes).ok())
    }

    /// The rows asked for.
    pub fn requested_entries(&self) -> &[AdapterInfo] {
        self.entries.get(..self.requested).unwrap_or_default()
    }

    /// The rows asked for, to fill: a recorded driver's answer, in the tests.
    #[cfg(test)]
    pub fn requested_entries_mut(&mut self) -> &mut [AdapterInfo] {
        let requested = self.requested;
        self.entries.get_mut(..requested).unwrap_or_default()
    }
}

/// `ADLMemoryInfo2` from `adl_structures.h`: the adapter's memory.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdlMemoryInfo2 {
    /// Memory size in bytes.
    pub i_memory_size: i64,
    /// Memory type in string.
    pub _str_memory_type: [u8; ADL_MAX_PATH],
    /// Highest default performance level memory bandwidth in Mbytes/s.
    pub _i_memory_bandwidth: i64,
    /// HyperMemory size in bytes.
    pub _i_hyper_memory_size: i64,
    /// Invisible memory size in bytes.
    pub _i_invisible_memory_size: i64,
    /// Visible memory size in bytes.
    pub _i_visible_memory_size: i64,
}

impl Default for AdlMemoryInfo2 {
    fn default() -> Self {
        Self {
            i_memory_size: 0,
            _str_memory_type: [0; ADL_MAX_PATH],
            _i_memory_bandwidth: 0,
            _i_hyper_memory_size: 0,
            _i_invisible_memory_size: 0,
            _i_visible_memory_size: 0,
        }
    }
}

// ADLMemoryInfo2's layout: a long long, char[256], then four long longs on 8-byte boundaries.
const _: [(); 296] = [(); std::mem::size_of::<AdlMemoryInfo2>()];
const _: [(); 264] = [(); std::mem::offset_of!(AdlMemoryInfo2, _i_memory_bandwidth)];

/// `ADLVersionsInfoX2` from `adl_structures.h`: the driver's versions.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdlVersionsInfoX2 {
    /// Driver release (packaging) version, e.g. "25.20.32.06-251214a-197030C-Asus".
    pub str_driver_ver: [u8; ADL_MAX_PATH],
    /// Catalyst version.
    pub _str_catalyst_version: [u8; ADL_MAX_PATH],
    /// Crimson version, the driver's release: e.g. "25.20.32.06".
    pub str_crimson_version: [u8; ADL_MAX_PATH],
    /// Web link to an XML file about the latest AMD drivers.
    pub _str_catalyst_web_link: [u8; ADL_MAX_PATH],
}

impl Default for AdlVersionsInfoX2 {
    fn default() -> Self {
        Self {
            str_driver_ver: [0; ADL_MAX_PATH],
            _str_catalyst_version: [0; ADL_MAX_PATH],
            str_crimson_version: [0; ADL_MAX_PATH],
            _str_catalyst_web_link: [0; ADL_MAX_PATH],
        }
    }
}

const _: [(); 1024] = [(); std::mem::size_of::<AdlVersionsInfoX2>()];

// The entry points are only callable where the library exists, so they are declared for Windows
// alone. The struct layouts above stay unconditional: their compile-time checks and the fixture
// tests run on every platform.

#[cfg(target_os = "windows")]
pub type Adl2MainControlCreate = unsafe extern "C" fn(
    callback: AdlMainMallocCallback,
    enum_connected_adapters: c_int,
    context: *mut AdlContextHandle,
) -> c_int;

#[cfg(target_os = "windows")]
pub type Adl2MainControlDestroy = unsafe extern "C" fn(context: AdlContextHandle) -> c_int;

#[cfg(target_os = "windows")]
pub type Adl2AdapterNumberOfAdaptersGet =
    unsafe extern "C" fn(context: AdlContextHandle, num_adapters: *mut c_int) -> c_int;

#[cfg(target_os = "windows")]
pub type Adl2NewQueryPmLogDataGet = unsafe extern "C" fn(
    context: AdlContextHandle,
    adapter_index: c_int,
    output: *mut AdlPmLogDataOutput,
) -> c_int;

#[cfg(target_os = "windows")]
pub type Adl2AdapterAdapterInfoGet = unsafe extern "C" fn(
    context: AdlContextHandle,
    info: *mut AdapterInfo,
    input_size: c_int,
) -> c_int;

/// `ADL2_Adapter_VRAMUsage_Get` (`adapter.h`): the adapter's VRAM in use, in megabytes.
#[cfg(target_os = "windows")]
pub type Adl2AdapterVramUsageGet = unsafe extern "C" fn(
    context: AdlContextHandle,
    adapter_index: c_int,
    vram_usage_in_mb: *mut c_int,
) -> c_int;

/// `ADL2_Adapter_MemoryInfo2_Get` (`adapter.h`).
#[cfg(target_os = "windows")]
pub type Adl2AdapterMemoryInfo2Get = unsafe extern "C" fn(
    context: AdlContextHandle,
    adapter_index: c_int,
    memory_info: *mut AdlMemoryInfo2,
) -> c_int;

/// `ADL2_Graphics_VersionsX2_Get` (`graphics.h`).
#[cfg(target_os = "windows")]
pub type Adl2GraphicsVersionsX2Get =
    unsafe extern "C" fn(context: AdlContextHandle, versions: *mut AdlVersionsInfoX2) -> c_int;
