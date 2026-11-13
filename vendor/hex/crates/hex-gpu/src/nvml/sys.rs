//! The NVML calls hex-gpu makes, which Rust can't make safely (CLAUDE.md §12): NVML loaded at run time
//! (`dlopen` and `dlsym` on Linux; `LoadLibraryExW` from System32 and `GetProcAddress` on Windows), so a
//! node without NVIDIA's driver starts and says why it reads no NVIDIA GPU, and the eight entry points
//! called through the pointers resolved there. Its `loader` and `export!` load AMD's ADL on Windows
//! too (`crate::adl::sys`). Their signatures and structures are NVIDIA's NVML
//! reference (`nvml.h`, NVML 13.4.92: NVIDIA/go-nvml's `gen/nvml/nvml.h` at f83235b8). Each `unsafe`
//! block states the invariant that makes it sound.

// hex-gpu's one place `unsafe` is allowed (CLAUDE.md §12); every block below carries its SAFETY comment.
#![allow(unsafe_code)]

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::ptr::null_mut;

use super::load::DynLoadError;
use super::{BUS_ID_BYTES, Calls, DRIVER_VERSION_BYTES, Memory, NAME_BYTES};

/// `nvmlReturn_t`, a C enum.
type Return = c_int;
/// `nvmlDevice_t`: `struct nvmlDevice_st*`, opaque.
type RawDevice = *mut c_void;

/// `NVML_SUCCESS`.
const SUCCESS: Return = 0;
/// `NVML_ERROR_INVALID_ARGUMENT`: what a buffer length past `unsigned int` would be.
const INVALID_ARGUMENT: Return = 2;

/// `nvmlMemory_t`.
#[repr(C)]
struct NvmlMemory {
    total: u64,
    _free: u64,
    used: u64,
}

/// `nvmlUtilization_t`.
#[repr(C)]
struct NvmlUtilization {
    gpu: c_uint,
    _memory: c_uint,
}

/// `nvmlPciInfo_t`, which `nvmlDeviceGetPciInfo_v3` fills. Its `char` arrays are declared as bytes:
/// the same layout, read as the bytes they are. Hex reads `busId` alone, which names the function too.
#[repr(C)]
struct NvmlPciInfo {
    _bus_id_legacy: [u8; 16],
    _domain: c_uint,
    _bus: c_uint,
    _device: c_uint,
    _pci_device_id: c_uint,
    _pci_sub_system_id: c_uint,
    bus_id: [u8; BUS_ID_BYTES],
}

type Init = unsafe extern "C" fn() -> Return;
type DriverVersion = unsafe extern "C" fn(*mut c_char, c_uint) -> Return;
type Count = unsafe extern "C" fn(*mut c_uint) -> Return;
type HandleByIndex = unsafe extern "C" fn(c_uint, *mut RawDevice) -> Return;
type Name = unsafe extern "C" fn(RawDevice, *mut c_char, c_uint) -> Return;
type PciInfo = unsafe extern "C" fn(RawDevice, *mut NvmlPciInfo) -> Return;
type MemoryInfo = unsafe extern "C" fn(RawDevice, *mut NvmlMemory) -> Return;
type UtilizationRates = unsafe extern "C" fn(RawDevice, *mut NvmlUtilization) -> Return;

/// NVML's entry points Hex calls, resolved once. The library is never closed, so they stay valid for
/// the process.
#[derive(Debug)]
pub(crate) struct Api {
    init: Init,
    driver_version: DriverVersion,
    count: Count,
    handle_by_index: HandleByIndex,
    name: Name,
    pci_info: PciInfo,
    memory_info: MemoryInfo,
    utilization_rates: UtilizationRates,
}

/// A device handle from `nvmlDeviceGetHandleByIndex_v2`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Handle(RawDevice);

// SAFETY: a handle is an opaque name NVML resolves on each call, never dereferenced here. nvml.h: "NVML
// is thread-safe so it is safe to make simultaneous NVML calls from multiple threads", and a handle stays
// valid until nvmlShutdown, which Hex never calls (`super::load::nvml` keeps NVML for the process).
unsafe impl Send for Handle {}
// SAFETY: as for Send: NVML is thread-safe, so one handle may be named from several threads at once.
unsafe impl Sync for Handle {}

fn check(code: Return) -> Result<(), i32> {
    if code == SUCCESS { Ok(()) } else { Err(code) }
}

/// A vendor's library loaded at run time and never unloaded, and its exports.
#[cfg(target_os = "linux")]
pub(crate) mod loader {
    use std::ffi::{CStr, CString, c_void};

    use super::DynLoadError;

    pub(crate) type Library = *mut c_void;

    pub(crate) fn load(name: &'static str) -> Result<Library, DynLoadError> {
        let failed = |source| DynLoadError::LoadFailed { names: &[], source };
        let path = CString::new(name).map_err(|_| failed(format!("{name}: holds a NUL")))?;
        // SAFETY: path is NUL-terminated and outlives the call. RTLD_NOW binds the library's own symbols
        // before dlopen returns; RTLD_LOCAL keeps them out of the process's global scope. A null return
        // is an error, read from dlerror.
        let library = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if library.is_null() {
            return Err(failed(error()));
        }
        Ok(library)
    }

    pub(crate) fn symbol(library: Library, name: &'static CStr) -> Option<*mut c_void> {
        // SAFETY: library is what dlopen returned, never closed; name is NUL-terminated. dlsym returns
        // null for a symbol the library lacks, checked.
        let f = unsafe { libc::dlsym(library, name.as_ptr()) };
        if f.is_null() { None } else { Some(f) }
    }

    fn error() -> String {
        // SAFETY: dlerror returns null or a NUL-terminated message valid until this thread's next dl*
        // call; it is copied before any.
        let message = unsafe { libc::dlerror() };
        if message.is_null() {
            return String::from("dlopen returned no library and no error");
        }
        // SAFETY: message is non-null and NUL-terminated (above), and nothing frees it while it is copied.
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    }
}

/// A vendor's library loaded at run time and never unloaded, and its exports.
#[cfg(target_os = "windows")]
pub(crate) mod loader {
    use std::ffi::{CStr, c_void};
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{GetLastError, HMODULE};
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
    };

    use super::DynLoadError;

    pub(crate) type Library = HMODULE;

    pub(crate) fn load(name: &'static str) -> Result<Library, DynLoadError> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: wide is NUL-terminated and outlives the call; no file handle is passed, and
        // LOAD_LIBRARY_SEARCH_SYSTEM32 searches System32 alone. A null return is an error, checked.
        let library =
            unsafe { LoadLibraryExW(wide.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if library.is_null() {
            // SAFETY: reads this thread's last error, which the failed call above set; nothing intervened.
            let code = unsafe { GetLastError() };
            return Err(DynLoadError::LoadFailed {
                names: &[],
                source: format!("{name}: Windows error {code}"),
            });
        }
        Ok(library)
    }

    pub(crate) fn symbol(library: Library, name: &'static CStr) -> Option<*mut c_void> {
        // SAFETY: library is a module LoadLibraryExW returned, never freed; name is NUL-terminated ASCII.
        // GetProcAddress returns None for an export the module lacks, checked.
        let f = unsafe { GetProcAddress(library, name.as_ptr().cast()) }?;
        Some(f as *mut c_void)
    }
}

/// Resolves a loaded library's export `$name` as a function of type `$t`, or returns the missing
/// symbol's error.
macro_rules! export {
    ($library:expr, $name:literal, $t:ty) => {{
        let Some(f) = $crate::nvml::sys::loader::symbol($library, $name) else {
            return Err($crate::nvml::load::DynLoadError::MissingSymbol {
                name: match $name.to_str() {
                    Ok(name) => name,
                    Err(_) => "a name that isn't UTF-8",
                },
            });
        };
        // SAFETY: the export is the library's function of signature $t, as its vendor's header declares
        // it (the calling module's comment names the header); a function's address read as a data
        // pointer and transmuted back to that function's own type is that function, and the two
        // pointers have one size on every target Hex builds for.
        unsafe { ::std::mem::transmute::<*mut ::std::ffi::c_void, $t>(f) }
    }};
}
// ADL's loading on Windows resolves its exports through it too (`crate::adl::sys`).
#[cfg(target_os = "windows")]
pub(crate) use export;

impl Api {
    /// Loads the library `name` names and resolves every entry point Hex calls.
    pub(crate) fn open(name: &'static str) -> Result<Api, DynLoadError> {
        let library = loader::load(name)?;
        Ok(Api {
            init: export!(library, c"nvmlInit_v2", Init),
            driver_version: export!(library, c"nvmlSystemGetDriverVersion", DriverVersion),
            count: export!(library, c"nvmlDeviceGetCount_v2", Count),
            handle_by_index: export!(library, c"nvmlDeviceGetHandleByIndex_v2", HandleByIndex),
            name: export!(library, c"nvmlDeviceGetName", Name),
            pci_info: export!(library, c"nvmlDeviceGetPciInfo_v3", PciInfo),
            memory_info: export!(library, c"nvmlDeviceGetMemoryInfo", MemoryInfo),
            utilization_rates: export!(library, c"nvmlDeviceGetUtilizationRates", UtilizationRates),
        })
    }

    /// `nvmlInit_v2`, once for the process (`super::load::nvml`): NVML counts initialisations, and
    /// Hex keeps this one.
    pub(crate) fn init(&self) -> Result<(), i32> {
        // SAFETY: nvmlInit_v2 takes no arguments and may be called from any thread (nvml.h).
        check(unsafe { (self.init)() })
    }
}

/// A buffer's length as NVML's `unsigned int`.
fn length(buffer: &[u8]) -> Result<c_uint, i32> {
    c_uint::try_from(buffer.len()).map_err(|_| INVALID_ARGUMENT)
}

// Every call below is made on an NVML `super::load::nvml` initialised and never shuts down, with
// pointers to this frame's own values, each valid and writable for the call's duration.
impl Calls for &'static Api {
    type Device = Handle;

    fn driver_version(self, out: &mut [u8; DRIVER_VERSION_BYTES]) -> Result<(), i32> {
        let len = length(out)?;
        // SAFETY: out is writable for len bytes; NVML writes at most len, its NUL included (nvml.h,
        // nvmlSystemGetDriverVersion).
        check(unsafe { (self.driver_version)(out.as_mut_ptr().cast(), len) })
    }

    fn count(self) -> Result<u32, i32> {
        let mut count: c_uint = 0;
        // SAFETY: count is a writable unsigned int.
        check(unsafe { (self.count)(&mut count) })?;
        Ok(count)
    }

    fn device(self, index: u32) -> Result<Handle, i32> {
        let mut device: RawDevice = null_mut();
        // SAFETY: device is a writable nvmlDevice_t; an index past the count is refused by NVML, not read.
        check(unsafe { (self.handle_by_index)(index, &mut device) })?;
        Ok(Handle(device))
    }

    fn name(self, device: Handle, out: &mut [u8; NAME_BYTES]) -> Result<(), i32> {
        let len = length(out)?;
        // SAFETY: device is a handle NVML gave; out is writable for len bytes, and NVML writes at most
        // len, its NUL included (nvml.h, nvmlDeviceGetName).
        check(unsafe { (self.name)(device.0, out.as_mut_ptr().cast(), len) })
    }

    fn bus_id(self, device: Handle, out: &mut [u8; BUS_ID_BYTES]) -> Result<(), i32> {
        let mut pci = NvmlPciInfo {
            _bus_id_legacy: [0; 16],
            _domain: 0,
            _bus: 0,
            _device: 0,
            _pci_device_id: 0,
            _pci_sub_system_id: 0,
            bus_id: [0; BUS_ID_BYTES],
        };
        // SAFETY: device is a handle NVML gave; pci is a writable nvmlPciInfo_t.
        check(unsafe { (self.pci_info)(device.0, &mut pci) })?;
        *out = pci.bus_id;
        Ok(())
    }

    fn memory(self, device: Handle) -> Result<Memory, i32> {
        let mut memory = NvmlMemory {
            total: 0,
            _free: 0,
            used: 0,
        };
        // SAFETY: device is a handle NVML gave; memory is a writable nvmlMemory_t.
        check(unsafe { (self.memory_info)(device.0, &mut memory) })?;
        Ok(Memory {
            total: memory.total,
            used: memory.used,
        })
    }

    fn utilization(self, device: Handle) -> Result<u32, i32> {
        let mut utilization = NvmlUtilization { gpu: 0, _memory: 0 };
        // SAFETY: device is a handle NVML gave; utilization is a writable nvmlUtilization_t.
        check(unsafe { (self.utilization_rates)(device.0, &mut utilization) })?;
        Ok(utilization.gpu)
    }
}
