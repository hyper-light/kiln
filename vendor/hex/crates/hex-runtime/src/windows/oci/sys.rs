//! The Windows calls the `oci` class makes that Rust can't make safely (proposed for CLAUDE.md §12): the
//! Host Compute Service's (`computecore.dll`), its storage calls (`computestorage.dll`) and the virtual-disk
//! calls (`virtdisk.dll`), each resolved at run time with `LoadLibraryExW` and `GetProcAddress` because
//! windows-sys links them at load time and a host without the Containers feature would then fail to start
//! the node at all (docs/research/runtime/windows-hcs.md §7); and, through windows-sys, opening a process by
//! pid, duplicating a handle into it, the host's version (`RtlGetVersion`), a file's version resource, and
//! a device interface class's members (`CM_Get_Device_Interface_List*`). Each `unsafe` block states the
//! invariant that makes it sound.

// The oci class's one place `unsafe` is allowed (proposed for CLAUDE.md §12); every block below carries
// its SAFETY comment.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use std::time::Duration;

use windows_sys::Wdk::Graphics::Direct3D::{
    D3DKMT_CLOSEADAPTER, D3DKMT_OPENADAPTERFROMDEVICENAME, D3DKMT_PHYSICAL_ADAPTER_COUNT,
    D3DKMT_QUERYADAPTERINFO, KMTQAITYPE_PHYSICALADAPTERCOUNT,
};
use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CR_SUCCESS,
};
use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, GENERIC_ALL, GetLastError, HANDLE, HLOCAL, HMODULE,
    INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::SECURITY_DESCRIPTOR;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileVersionInfoSizeW, GetFileVersionInfoW,
    OPEN_EXISTING, VS_FIXEDFILEINFO, VerQueryValueW,
};
use windows_sys::Win32::System::HostComputeSystem::{
    HCS_OPERATION, HCS_OPERATION_COMPLETION, HCS_PROCESS, HCS_PROCESS_INFORMATION, HCS_SYSTEM,
};
use windows_sys::Win32::System::JobObjects::IsProcessInJob;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOEXW;
use windows_sys::Win32::System::SystemServices::JOB_OBJECT_ASSIGN_PROCESS;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, TerminateProcess,
};
use windows_sys::core::{GUID, HRESULT, PCWSTR, PWSTR};

use hex_api::Refusal;
use hex_core::hcs::{self, Host, HostKind};

use crate::windows::sys::Handle;

/// `VER_NT_WORKSTATION`: a client SKU's product type (`OSVERSIONINFOEXW.wProductType`).
const VER_NT_WORKSTATION: u8 = 1;

/// The longest string Hex reads back from HCS, in UTF-16 units (a result or error document): far past any
/// document HCS returns for one container, bounded so a malformed one is never walked unbounded (§2).
const MAX_RESULT_UNITS: usize = 1 << 22;

/// The longest device-interface list Hex reads, in UTF-16 units (a probe's list of one class's members).
const MAX_INTERFACE_UNITS: u32 = 1 << 20;

/// A Windows error as an `io::Error`.
fn last_error() -> io::Error {
    // SAFETY: GetLastError reads the calling thread's own last-error value and returns it.
    io::Error::from_raw_os_error(i32::try_from(unsafe { GetLastError() }).unwrap_or(-1))
}

/// `s` as a NUL-terminated wide string. The caller has refused a NUL inside `s`.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// An HCS failure: its HRESULT, as `u32`, and the message its result document gave, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HcsFailure {
    pub code: u32,
    pub message: Option<String>,
}

impl HcsFailure {
    fn of(hr: HRESULT, doc: Option<String>) -> Self {
        Self {
            code: u32::from_ne_bytes(hr.to_ne_bytes()),
            message: doc.map(|d| hcs::result_message(&d)),
        }
    }
}

type CreateOperation =
    unsafe extern "system" fn(*const c_void, HCS_OPERATION_COMPLETION) -> HCS_OPERATION;
type CloseOperation = unsafe extern "system" fn(HCS_OPERATION);
type CancelOperation = unsafe extern "system" fn(HCS_OPERATION) -> HRESULT;
type WaitResult = unsafe extern "system" fn(HCS_OPERATION, u32, *mut PWSTR) -> HRESULT;
type WaitResultAndProcess = unsafe extern "system" fn(
    HCS_OPERATION,
    u32,
    *mut HCS_PROCESS_INFORMATION,
    *mut PWSTR,
) -> HRESULT;
type CreateSystem = unsafe extern "system" fn(
    PCWSTR,
    PCWSTR,
    HCS_OPERATION,
    *const SECURITY_DESCRIPTOR,
    *mut HCS_SYSTEM,
) -> HRESULT;
type OpenSystem = unsafe extern "system" fn(PCWSTR, u32, *mut HCS_SYSTEM) -> HRESULT;
type SystemOp = unsafe extern "system" fn(HCS_SYSTEM, HCS_OPERATION, PCWSTR) -> HRESULT;
type CloseSystem = unsafe extern "system" fn(HCS_SYSTEM);
type CreateProcess = unsafe extern "system" fn(
    HCS_SYSTEM,
    PCWSTR,
    HCS_OPERATION,
    *const SECURITY_DESCRIPTOR,
    *mut HCS_PROCESS,
) -> HRESULT;
type CloseProcess = unsafe extern "system" fn(HCS_PROCESS);
type ServiceProperties = unsafe extern "system" fn(PCWSTR, *mut PWSTR) -> HRESULT;
/// vmcompute's `DRIVER_INFO`: the filter driver (flavour 1) and an empty home folder, as hcsshim passes it
/// (`hcsshim/internal/wclayer/layerutils.go:35-43`).
#[repr(C)]
struct DriverInfo {
    flavour: i32,
    home: PCWSTR,
}

/// vmcompute's `WC_LAYER_DESCRIPTOR` (`hcsshim/internal/wclayer/layerutils.go:45-80`).
#[repr(C)]
struct LayerDescriptor {
    id: GUID,
    flags: u32,
    path: PCWSTR,
}

type LayerOp = unsafe extern "system" fn(*const DriverInfo, PCWSTR) -> HRESULT;
type PrepareLayer =
    unsafe extern "system" fn(*const DriverInfo, PCWSTR, *const LayerDescriptor, u32) -> HRESULT;
type LayerMountPath =
    unsafe extern "system" fn(*const DriverInfo, PCWSTR, *mut usize, *mut u16) -> HRESULT;

/// The HCS calls and vmcompute's layer calls, resolved once. The modules are never unloaded, so each pointer
/// stays valid for the process's life.
struct Api {
    create_operation: CreateOperation,
    close_operation: CloseOperation,
    cancel_operation: CancelOperation,
    wait_result: WaitResult,
    wait_result_and_process: WaitResultAndProcess,
    create_system: CreateSystem,
    open_system: OpenSystem,
    start_system: SystemOp,
    terminate_system: SystemOp,
    close_system: CloseSystem,
    create_process: CreateProcess,
    close_process: CloseProcess,
    service_properties: ServiceProperties,
    activate_layer: LayerOp,
    prepare_layer: PrepareLayer,
    layer_mount_path: LayerMountPath,
    unprepare_layer: LayerOp,
    deactivate_layer: LayerOp,
}

/// A module from System32, or what is missing.
fn module(name: &str) -> Result<HMODULE, String> {
    let w = wide(name);
    // SAFETY: w is a NUL-terminated wide string that outlives the call; no file handle is passed, and
    // LOAD_LIBRARY_SEARCH_SYSTEM32 searches System32 alone. A null return is an error, checked.
    let m = unsafe { LoadLibraryExW(w.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if m.is_null() {
        return Err(format!("{name} isn't on this machine ({})", last_error()));
    }
    Ok(m)
}

/// Resolves export `name` of `module` as a function of type `$t` (its documented signature).
macro_rules! export {
    ($module:expr, $dll:literal, $name:literal, $t:ty) => {{
        // SAFETY: $name is a NUL-terminated ASCII export name and $module a module loaded above, never
        // freed; GetProcAddress returns null for a missing export, checked.
        let f = unsafe { GetProcAddress($module, concat!($name, "\0").as_ptr()) };
        match f {
            // SAFETY: the export is the documented function of signature $t (the HCS and virtdisk
            // references, docs/research/runtime/windows-hcs.md); a function pointer of one signature
            // transmuted to another of the same size is sound when the target is that function's true
            // signature, which the documentation states.
            Some(f) => unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, $t>(f) },
            None => return Err(format!("{} has no {} ({})", $dll, $name, last_error())),
        }
    }};
}

impl Api {
    fn load() -> Result<Api, String> {
        let core = module("computecore.dll")?;
        let layers = module("vmcompute.dll")?;
        Ok(Api {
            create_operation: export!(
                core,
                "computecore.dll",
                "HcsCreateOperation",
                CreateOperation
            ),
            close_operation: export!(core, "computecore.dll", "HcsCloseOperation", CloseOperation),
            cancel_operation: export!(
                core,
                "computecore.dll",
                "HcsCancelOperation",
                CancelOperation
            ),
            wait_result: export!(
                core,
                "computecore.dll",
                "HcsWaitForOperationResult",
                WaitResult
            ),
            wait_result_and_process: export!(
                core,
                "computecore.dll",
                "HcsWaitForOperationResultAndProcessInfo",
                WaitResultAndProcess
            ),
            create_system: export!(
                core,
                "computecore.dll",
                "HcsCreateComputeSystem",
                CreateSystem
            ),
            open_system: export!(core, "computecore.dll", "HcsOpenComputeSystem", OpenSystem),
            start_system: export!(core, "computecore.dll", "HcsStartComputeSystem", SystemOp),
            terminate_system: export!(
                core,
                "computecore.dll",
                "HcsTerminateComputeSystem",
                SystemOp
            ),
            close_system: export!(
                core,
                "computecore.dll",
                "HcsCloseComputeSystem",
                CloseSystem
            ),
            create_process: export!(core, "computecore.dll", "HcsCreateProcess", CreateProcess),
            close_process: export!(core, "computecore.dll", "HcsCloseProcess", CloseProcess),
            service_properties: export!(
                core,
                "computecore.dll",
                "HcsGetServiceProperties",
                ServiceProperties
            ),
            activate_layer: export!(layers, "vmcompute.dll", "ActivateLayer", LayerOp),
            prepare_layer: export!(layers, "vmcompute.dll", "PrepareLayer", PrepareLayer),
            layer_mount_path: export!(layers, "vmcompute.dll", "GetLayerMountPath", LayerMountPath),
            unprepare_layer: export!(layers, "vmcompute.dll", "UnprepareLayer", LayerOp),
            deactivate_layer: export!(layers, "vmcompute.dll", "DeactivateLayer", LayerOp),
        })
    }
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

/// The calls, loaded on first use; `Err` says which module or export is missing.
fn api() -> Result<&'static Api, &'static str> {
    API.get_or_init(Api::load).as_ref().map_err(String::as_str)
}

pub(super) fn initialize_api() {
    let _ = api();
}

/// DLL/export initialization only. This does not claim HcsGetServiceProperties answered.
pub(super) fn initialized_api_loaded() -> Option<bool> {
    API.get().map(Result::is_ok)
}

/// Whether HCS is here and answering: its calls load and `HcsGetServiceProperties` succeeds. `Err` says
/// what is missing, for the class's refusal.
pub fn available() -> Result<(), String> {
    let api = api().map_err(str::to_owned)?;
    let query = wide(r#"{"PropertyTypes":["Basic"]}"#);
    let mut result: PWSTR = null_mut();
    // SAFETY: query is a NUL-terminated wide string; result receives an HCS-allocated string, freed below.
    let hr = unsafe { (api.service_properties)(query.as_ptr(), &mut result) };
    let doc = take(result);
    if hr < 0 {
        return Err(format!(
            "the Host Compute Service doesn't answer ({:#010x}{})",
            u32::from_ne_bytes(hr.to_ne_bytes()),
            doc.map(|d| format!(": {d}")).unwrap_or_default()
        ));
    }
    Ok(())
}

/// Reads and frees a string HCS allocated (`LocalFree`, `HcsWaitForOperationResult.md:53`); `None` for null.
fn take(s: PWSTR) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let mut len = 0usize;
    // SAFETY: s is a NUL-terminated wide string HCS allocated; reading stops at its NUL or at the bound,
    // within the allocation either way (a string past the bound is longer than the bound).
    while len < MAX_RESULT_UNITS && unsafe { *s.add(len) } != 0 {
        len = len.saturating_add(1);
    }
    // SAFETY: the len units before the NUL (or the bound) are initialized, read above.
    let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s, len) });
    // SAFETY: s was allocated by HCS for the caller to free with LocalFree, once, here.
    unsafe { LocalFree(s as HLOCAL) };
    Some(text)
}

/// Milliseconds of `d` for an HCS wait, capped below `INFINITE`.
fn millis(d: Duration) -> u32 {
    u32::try_from(d.as_millis())
        .unwrap_or(0xFFFF_FFFE)
        .min(0xFFFF_FFFE)
}

/// One HCS operation: made, handed to `call`, waited on for at most `patience`. A wait that runs out is
/// cancelled (never abandoned, note §2) and waited on once more for the same time; an operation that still
/// hasn't finished is left open rather than closed under HCS's feet, and refused as timed out. Returns the
/// result document and, when `process` is asked for, the process information.
fn operation(
    api: &Api,
    patience: Duration,
    process: Option<&mut HCS_PROCESS_INFORMATION>,
    call: impl FnOnce(HCS_OPERATION) -> HRESULT,
) -> Result<Option<String>, HcsFailure> {
    // SAFETY: no context or callback: the operation is waited on synchronously below.
    let op = unsafe { (api.create_operation)(null(), None) };
    if op.is_null() {
        return Err(HcsFailure {
            code: 0x8037_011A,
            message: Some(format!("HcsCreateOperation failed: {}", last_error())),
        });
    }
    let hr = call(op);
    if hr < 0 {
        // SAFETY: the call failed synchronously, so the operation was never started; op is ours to close.
        unsafe { (api.close_operation)(op) };
        return Err(HcsFailure::of(hr, None));
    }
    let info: *mut HCS_PROCESS_INFORMATION = process.map_or(null_mut(), std::ptr::from_mut);
    let wait = || {
        let mut doc: PWSTR = null_mut();
        let hr = if info.is_null() {
            // SAFETY: op is a started operation; doc is a valid out-pointer.
            unsafe { (api.wait_result)(op, millis(patience), &mut doc) }
        } else {
            // SAFETY: op is a started operation; info points at the caller's HCS_PROCESS_INFORMATION,
            // borrowed for this call, and doc is a valid out-pointer.
            unsafe { (api.wait_result_and_process)(op, millis(patience), info, &mut doc) }
        };
        (hr, take(doc))
    };
    let timed_out = |hr: HRESULT| {
        hex_core::hcs::hcs_error(u32::from_ne_bytes(hr.to_ne_bytes()))
            == hex_core::hcs::HcsError::Timeout
    };
    let (mut hr, mut doc) = wait();
    if timed_out(hr) {
        // SAFETY: op is a started operation; cancelling one is the documented way to bound it.
        unsafe { (api.cancel_operation)(op) };
        (hr, doc) = wait();
        if timed_out(hr) {
            // Still running inside HCS after its cancel: closing it now could free what the call uses, so
            // the operation handle is left open (one per such failure) and the call refused.
            return Err(HcsFailure::of(hr, doc));
        }
    }
    // SAFETY: the operation has finished (its wait returned other than a timeout); op is ours to close.
    unsafe { (api.close_operation)(op) };
    if hr < 0 {
        return Err(HcsFailure::of(hr, doc));
    }
    Ok(doc)
}

/// A compute system, closed on drop (and, created with `ShouldTerminateOnLastHandleClosed`, ended then).
#[derive(Debug)]
pub struct System(HCS_SYSTEM);

// SAFETY: an HCS_SYSTEM handle is valid in any thread of the process; HCS serializes calls on it.
unsafe impl Send for System {}
// SAFETY: as above; Hex shares one only behind its own lock or for HCS calls HCS serializes.
unsafe impl Sync for System {}

impl Drop for System {
    fn drop(&mut self) {
        if let Ok(api) = api() {
            // SAFETY: self owns the handle and Drop runs once; HcsCloseComputeSystem drains in-flight calls.
            unsafe { (api.close_system)(self.0) };
        }
    }
}

/// Creates compute system `id` from `document`, waiting at most `patience`.
pub fn create_system(id: &str, document: &str, patience: Duration) -> Result<System, HcsFailure> {
    let api = api().map_err(|m| HcsFailure {
        code: 0x8037_0114,
        message: Some(m.to_owned()),
    })?;
    let (id, doc) = (wide(id), wide(document));
    let mut system: HCS_SYSTEM = null_mut();
    operation(api, patience, None, |op| {
        // SAFETY: id and doc are NUL-terminated wide strings that outlive the operation's wait; the
        // security descriptor is reserved (null); system receives the handle.
        unsafe { (api.create_system)(id.as_ptr(), doc.as_ptr(), op, null(), &mut system) }
    })
    .inspect_err(|_| {
        if !system.is_null() {
            // SAFETY: a handle HCS gave back before the create failed is ours to close.
            unsafe { (api.close_system)(system) };
        }
    })?;
    Ok(System(system))
}

/// The assigned-device create owns its operation, system and input storage until a terminal fact.
/// Other HCS operations keep their existing implementation above.
pub(crate) struct PendingCreate {
    context: std::ptr::NonNull<CreateContext>,
}

struct CreateContext {
    // Exactly two physical owners: PendingCreate and the registered HCS completion receipt.
    owners: std::sync::atomic::AtomicUsize,
    callback_owned: std::sync::atomic::AtomicBool,
    completed: std::sync::atomic::AtomicBool,
    calls: CreateCalls,
    data: std::sync::Mutex<CreateData>,
}

struct CreateInputs {
    id: Vec<u16>,
    document: Vec<u16>,
}

struct CreateData {
    operation: Option<HCS_OPERATION>,
    inputs: Option<CreateInputs>,
    system: Option<System>,
    state: CreateOperationState,
    cancellation: CreateCancellation,
    durable: Option<super::service::TerminalReceipt>,
}

enum CreateOperationState {
    NotStarted,
    Active,
    Terminal(Result<(), HcsFailure>),
    Released,
}

#[derive(Clone, Copy)]
struct CreateCalls {
    wait: WaitResult,
    cancel: CancelOperation,
    close: CloseOperation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreateCancellation {
    NotRequested,
    Requested,
    Refused(u32),
}

pub(crate) struct TerminalCreate {
    pub system: Option<System>,
    pub result: Result<(), HcsFailure>,
}

// SAFETY: HCS handles are process-wide. CreateData is accessed only under its context mutex;
// the native callback uses try_lock, so an inline completion never waits on the creating thread.
unsafe impl Send for CreateData {}
// SAFETY: this unique runtime receipt owns a reference to that synchronized stable context.
unsafe impl Send for PendingCreate {}

fn create_fault(code: HRESULT) -> HcsFailure {
    HcsFailure {
        code: u32::from_ne_bytes(code.to_ne_bytes()),
        message: None,
    }
}

fn create_wide(text: &str) -> Result<Vec<u16>, HcsFailure> {
    let exhausted = || create_fault(windows_sys::Win32::Foundation::E_OUTOFMEMORY);
    let units = text
        .encode_utf16()
        .count()
        .checked_add(1)
        .ok_or_else(exhausted)?;
    let mut wide = Vec::new();
    wide.try_reserve_exact(units).map_err(|_| exhausted())?;
    wide.extend(text.encode_utf16());
    wide.push(0);
    Ok(wide)
}

fn create_incomplete(hr: HRESULT) -> bool {
    hcs::hcs_error(u32::from_ne_bytes(hr.to_ne_bytes())) == hcs::HcsError::Timeout
        || hr == windows_sys::Win32::Foundation::HCS_E_OPERATION_PENDING
}

impl CreateData {
    fn publish(&mut self) {
        if let CreateOperationState::Terminal(result) = &self.state
            && let Some(durable) = self.durable.as_mut()
        {
            if self.system.is_some() {
                durable.owns_system();
            }
            // A failed durable write leaves Dispatched on disk and signals explicit uncertainty.
            // Native completion is never fabricated from that failure or from creator death.
            let _ = durable.complete(result);
        }
    }

    fn advance(&mut self, calls: CreateCalls, patience: Duration) {
        if !matches!(self.state, CreateOperationState::Active) {
            return;
        }
        let Some(operation) = self.operation else {
            return;
        };
        let mut document: PWSTR = null_mut();
        // SAFETY: this still-owned operation is serialized by the context mutex; output is valid.
        let hr = unsafe { (calls.wait)(operation, millis(patience), &mut document) };
        let document = take(document);
        if !create_incomplete(hr) {
            self.state = CreateOperationState::Terminal(if hr < 0 {
                Err(HcsFailure::of(hr, document))
            } else {
                Ok(())
            });
            self.publish();
        }
    }

    fn close(&mut self, calls: CreateCalls) {
        if let Some(operation) = self.operation.take() {
            // SAFETY: called only after the completion callback's terminal fact, or a synchronous
            // refusal before the operation started. Microsoft permits closing inside this callback.
            unsafe { (calls.close)(operation) };
        }
        self.inputs = None;
    }

    fn cancel(&mut self, calls: CreateCalls) -> CreateCancellation {
        if self.cancellation == CreateCancellation::NotRequested
            && matches!(self.state, CreateOperationState::Active)
            && let Some(operation) = self.operation
        {
            // SAFETY: the context owns this active operation; cancellation is only a request.
            let hr = unsafe { (calls.cancel)(operation) };
            self.cancellation = if hr < 0 {
                CreateCancellation::Refused(u32::from_ne_bytes(hr.to_ne_bytes()))
            } else {
                CreateCancellation::Requested
            };
        }
        self.cancellation
    }
}

impl CreateContext {
    fn allocate(inputs: CreateInputs, calls: CreateCalls) -> Result<PendingCreate, HcsFailure> {
        let layout = std::alloc::Layout::new::<Self>();
        // SAFETY: this exact nonzero layout is deallocated once by release_create's last owner.
        let raw = unsafe { std::alloc::alloc(layout) }.cast::<Self>();
        let pointer = std::ptr::NonNull::new(raw)
            .ok_or_else(|| create_fault(windows_sys::Win32::Foundation::E_OUTOFMEMORY))?;
        // SAFETY: pointer owns suitably aligned, uninitialized storage for exactly one Self.
        unsafe {
            pointer.as_ptr().write(Self {
                owners: std::sync::atomic::AtomicUsize::new(2),
                callback_owned: std::sync::atomic::AtomicBool::new(true),
                completed: std::sync::atomic::AtomicBool::new(false),
                calls,
                data: std::sync::Mutex::new(CreateData {
                    operation: None,
                    inputs: Some(inputs),
                    system: None,
                    state: CreateOperationState::NotStarted,
                    cancellation: CreateCancellation::NotRequested,
                    durable: None,
                }),
            })
        };
        Ok(PendingCreate { context: pointer })
    }
}

/// Releases one of the two non-clonable physical receipts. No general reference-counted manager is
/// introduced; checked subtraction never wraps, and only its last owner frees the exact allocation.
fn release_create(pointer: std::ptr::NonNull<CreateContext>) {
    // SAFETY: the caller holds one of the runtime/callback receipts through this final access.
    let context = unsafe { pointer.as_ref() };
    let previous = context.owners.try_update(
        std::sync::atomic::Ordering::AcqRel,
        std::sync::atomic::Ordering::Acquire,
        |owners| owners.checked_sub(1),
    );
    if previous == Ok(1) {
        #[cfg(test)]
        create_tests::released();
        // SAFETY: both receipts are released; no caller/callback may access the context again.
        unsafe {
            std::ptr::drop_in_place(pointer.as_ptr());
            std::alloc::dealloc(
                pointer.as_ptr().cast(),
                std::alloc::Layout::new::<CreateContext>(),
            );
        }
    }
}

fn release_create_callback(pointer: std::ptr::NonNull<CreateContext>) {
    // SAFETY: callback or the not-started creator still owns the callback receipt at entry.
    let context = unsafe { pointer.as_ref() };
    if context
        .callback_owned
        .swap(false, std::sync::atomic::Ordering::AcqRel)
    {
        release_create(pointer);
    }
}

unsafe extern "system" fn create_completed(_: HCS_OPERATION, opaque: *const c_void) {
    let Some(pointer) = std::ptr::NonNull::new(opaque.cast_mut().cast::<CreateContext>()) else {
        return;
    };
    // SAFETY: HcsCreateOperation received this stable context and owns its completion receipt.
    let context = unsafe { pointer.as_ref() };
    context
        .completed
        .store(true, std::sync::atomic::Ordering::Release);
    match context.data.try_lock() {
        Ok(mut data) => {
            data.advance(context.calls, Duration::ZERO);
            data.close(context.calls);
        }
        Err(std::sync::TryLockError::Poisoned(error)) => {
            let mut data = error.into_inner();
            data.advance(context.calls, Duration::ZERO);
            data.close(context.calls);
        }
        // An inline callback or a simultaneous query/drop never blocks. Its live runtime owner
        // observes completed after its native call returns, or at the context's final drop.
        Err(std::sync::TryLockError::WouldBlock) => {}
    }
    // Last access: this can free the context if its runtime was already dropped.
    release_create_callback(pointer);
}

impl Drop for CreateContext {
    fn drop(&mut self) {
        let data = match self.data.get_mut() {
            Ok(data) => data,
            Err(error) => error.into_inner(),
        };
        if self.completed.load(std::sync::atomic::Ordering::Acquire) {
            data.publish();
            // The native completion receipt supplies the terminal fact even if it ran inline
            // while the creating/querying thread held the mutex.
            data.close(self.calls);
        }
    }
}

impl PendingCreate {
    fn context(&self) -> &CreateContext {
        // SAFETY: this unique runtime receipt keeps the stable context alive until Drop.
        unsafe { self.context.as_ref() }
    }

    /// The existing keeper keeps this runtime receipt until the terminal result is durable.
    /// A failed write keeps both that exact result and its file-lock owner for a later external wake.
    pub(crate) fn publish_terminal(&mut self) -> Result<bool, Refusal> {
        let context = self.context();
        let mut data = context
            .data
            .lock()
            .map_err(|_| Refusal::Broken("native create owner lock was poisoned".to_owned()))?;
        data.advance(context.calls, Duration::ZERO);
        data.publish();
        if context.completed.load(std::sync::atomic::Ordering::Acquire) {
            data.close(context.calls);
        }
        Ok(matches!(data.state, CreateOperationState::Terminal(_))
            && data.durable.as_ref().is_some_and(|owner| owner.published()))
    }

    pub(crate) fn cancel(&mut self) -> CreateCancellation {
        let context = self.context();
        match context.data.lock() {
            Ok(mut data) => data.cancel(context.calls),
            Err(_) => CreateCancellation::Refused(u32::from_ne_bytes(
                windows_sys::Win32::Foundation::E_UNEXPECTED.to_ne_bytes(),
            )),
        }
    }

    pub(crate) fn cancellation(&self) -> CreateCancellation {
        match self.context().data.lock() {
            Ok(data) => data.cancellation,
            Err(_) => CreateCancellation::Refused(u32::from_ne_bytes(
                windows_sys::Win32::Foundation::E_UNEXPECTED.to_ne_bytes(),
            )),
        }
    }

    pub(crate) fn query(&mut self) -> Option<TerminalCreate> {
        let context = self.context();
        let mut data = context.data.lock().ok()?;
        data.advance(context.calls, Duration::ZERO);
        if context.completed.load(std::sync::atomic::Ordering::Acquire) {
            data.close(context.calls);
        }
        if !matches!(data.state, CreateOperationState::Terminal(_)) {
            return None;
        }
        match std::mem::replace(&mut data.state, CreateOperationState::Released) {
            CreateOperationState::Terminal(result) => Some(TerminalCreate {
                system: data.system.take(),
                result,
            }),
            state => {
                data.state = state;
                None
            }
        }
    }
}

impl Drop for PendingCreate {
    fn drop(&mut self) {
        release_create(self.context);
    }
}

/// Only the assigned-create path uses the callback; legacy HCS operations remain unchanged.
pub(crate) fn create_system_owned(
    id: &str,
    document: &str,
    patience: Duration,
) -> Result<PendingCreate, HcsFailure> {
    create_system_inner(id, document, patience, &mut None)
}

pub(crate) fn create_system_durable(
    id: &str,
    document: &str,
    durable: &mut Option<super::service::TerminalReceipt>,
) -> Result<PendingCreate, HcsFailure> {
    create_system_inner(id, document, Duration::ZERO, durable)
}

fn create_system_inner(
    id: &str,
    document: &str,
    patience: Duration,
    durable: &mut Option<super::service::TerminalReceipt>,
) -> Result<PendingCreate, HcsFailure> {
    let api = api().map_err(|m| HcsFailure {
        code: 0x8037_0114,
        message: Some(m.to_owned()),
    })?;
    let inputs = CreateInputs {
        id: create_wide(id)?,
        document: create_wide(document)?,
    };
    let owned = CreateContext::allocate(
        inputs,
        CreateCalls {
            wait: api.wait_result,
            cancel: api.cancel_operation,
            close: api.close_operation,
        },
    )?;
    match owned.context().data.lock() {
        Ok(mut data) => data.durable = durable.take(),
        Err(_) => {
            release_create_callback(owned.context);
            return Err(create_fault(windows_sys::Win32::Foundation::E_UNEXPECTED));
        }
    }
    // SAFETY: this stable context owns a distinct callback receipt, released exactly once at completion
    // or below when allocation/start failed and no operation/callback was started.
    let operation =
        unsafe { (api.create_operation)(owned.context.as_ptr().cast(), Some(create_completed)) };
    if operation.is_null() {
        if let Ok(mut data) = owned.context().data.lock() {
            data.state = CreateOperationState::Terminal(Err(HcsFailure {
                code: 0x8037_011A,
                message: None,
            }));
            data.publish();
        }
        release_create_callback(owned.context);
        return Ok(owned);
    }
    submit_create(&owned, api, operation, patience);
    Ok(owned)
}

// The lock result and callback work borrow the pending owner only inside this operation.
fn submit_create(owned: &PendingCreate, api: &Api, operation: HCS_OPERATION, patience: Duration) {
    let context = owned.context();
    let mut data = match context.data.lock() {
        Ok(data) => data,
        Err(error) => {
            // SAFETY: HcsCreateComputeSystem has not been called; no completion can be pending.
            unsafe { (api.close_operation)(operation) };
            let mut data = error.into_inner();
            data.state = CreateOperationState::Terminal(Err(create_fault(
                windows_sys::Win32::Foundation::E_UNEXPECTED,
            )));
            data.publish();
            drop(data);
            release_create_callback(owned.context);
            return;
        }
    };
    data.operation = Some(operation);
    let Some((id, document)) = data
        .inputs
        .as_ref()
        .map(|inputs| (inputs.id.as_ptr(), inputs.document.as_ptr()))
    else {
        data.state = CreateOperationState::Terminal(Err(create_fault(
            windows_sys::Win32::Foundation::E_UNEXPECTED,
        )));
        data.publish();
        data.close(context.calls);
        drop(data);
        release_create_callback(owned.context);
        return;
    };
    if let Some(durable) = data.durable.as_mut() {
        durable.submitted();
    }
    let mut system: HCS_SYSTEM = null_mut();
    // SAFETY: inputs remain stable through terminal completion; the reserved descriptor is null;
    // system receives its handle on this call's return. An inline callback uses try_lock only.
    let hr = unsafe { (api.create_system)(id, document, operation, null(), &mut system) };
    if !system.is_null() {
        data.system = Some(System(system));
    }
    if hr < 0 {
        // A synchronous refusal did not start the operation, so no future completion is registered.
        data.state = CreateOperationState::Terminal(Err(HcsFailure::of(hr, None)));
        data.publish();
        data.close(context.calls);
        drop(data);
        release_create_callback(owned.context);
    } else {
        data.state = CreateOperationState::Active;
        data.advance(context.calls, patience);
        if matches!(data.state, CreateOperationState::Active) {
            data.cancel(context.calls);
            data.advance(context.calls, patience);
        }
        if context.completed.load(std::sync::atomic::Ordering::Acquire) {
            data.close(context.calls);
        }
        drop(data);
    }
}

/// Opens compute system `id` a node before this one made, where it still runs (`HcsOpenComputeSystem`,
/// with `GENERIC_ALL` as HCS's own sample reopens one: `VD/hcs/Reference/ComputeSystemSample.md:24-38`);
/// `None` where HCS has no system of that id. The handle takes every call the creating node's did.
pub fn open_system(id: &str) -> Result<Option<System>, HcsFailure> {
    let api = api().map_err(|m| HcsFailure {
        code: 0x8037_0114,
        message: Some(m.to_owned()),
    })?;
    let id = wide(id);
    let mut system: HCS_SYSTEM = null_mut();
    // SAFETY: id is a NUL-terminated wide string that outlives the call, which is synchronous (no
    // operation); system receives the handle, owned below on success.
    let hr = unsafe { (api.open_system)(id.as_ptr(), GENERIC_ALL, &mut system) };
    if hr < 0 {
        let f = HcsFailure::of(hr, None);
        return match hcs::hcs_error(f.code) {
            hcs::HcsError::NotFound => Ok(None),
            _ => Err(f),
        };
    }
    Ok(Some(System(system)))
}

impl System {
    fn op(&self, f: fn(&Api) -> SystemOp, patience: Duration) -> Result<(), HcsFailure> {
        let api = api().map_err(|m| HcsFailure {
            code: 0x8037_0114,
            message: Some(m.to_owned()),
        })?;
        let call = f(api);
        operation(api, patience, None, |op| {
            // SAFETY: self.0 is a live compute-system handle; options are null (none, documented).
            unsafe { call(self.0, op, null()) }
        })
        .map(|_| ())
    }

    /// Starts it (`HcsStartComputeSystem`).
    pub fn start(&self, patience: Duration) -> Result<(), HcsFailure> {
        self.op(|a| a.start_system, patience)
    }

    /// Ends it forcefully (`HcsTerminateComputeSystem`).
    pub fn terminate(&self, patience: Duration) -> Result<(), HcsFailure> {
        self.op(|a| a.terminate_system, patience)
    }

    /// Starts a process in it from `parameters` (a `ProcessParameters` document), returning HCS's handle
    /// for it, its pid, and the stdin and stdout pipes HCS made for it.
    pub fn create_process(
        &self,
        parameters: &str,
        patience: Duration,
    ) -> Result<HcsProcess, HcsFailure> {
        let api = api().map_err(|m| HcsFailure {
            code: 0x8037_0114,
            message: Some(m.to_owned()),
        })?;
        let params = wide(parameters);
        let mut process: HCS_PROCESS = null_mut();
        // SAFETY: HCS_PROCESS_INFORMATION is plain data; all-zero is a valid empty value.
        let mut info: HCS_PROCESS_INFORMATION = unsafe { zeroed() };
        let made = operation(api, patience, Some(&mut info), |op| {
            // SAFETY: self.0 is a live system; params is NUL-terminated and outlives the wait; the
            // security descriptor is reserved (null); process receives the handle.
            unsafe { (api.create_process)(self.0, params.as_ptr(), op, null(), &mut process) }
        });
        // The pipes and the process handle are ours from here, whatever the outcome, so they are owned
        // (and closed) either way.
        let handle = HcsProcessHandle(process);
        let own = |h: HANDLE| {
            (!h.is_null()).then(|| {
                // SAFETY: each non-null handle in info is a pipe end HCS made for this process, ours to
                // own, and each is taken once.
                unsafe { OwnedHandle::from_raw_handle(h as RawHandle) }
            })
        };
        let (stdin, stdout, _stderr) =
            (own(info.StdInput), own(info.StdOutput), own(info.StdError));
        made?;
        match (stdin, stdout) {
            (Some(stdin), Some(stdout)) => Ok(HcsProcess {
                handle,
                pid: info.ProcessId,
                stdin,
                stdout,
            }),
            _ => Err(HcsFailure {
                code: 0x8037_011D,
                message: Some("HCS gave the launcher no stdin or stdout pipe".to_owned()),
            }),
        }
    }
}

/// HCS's handle for a process it started, closed on drop.
#[derive(Debug)]
pub struct HcsProcessHandle(HCS_PROCESS);

/// What HCS gave back for a process it started.
#[derive(Debug)]
pub struct HcsProcess {
    pub handle: HcsProcessHandle,
    pub pid: u32,
    pub stdin: OwnedHandle,
    pub stdout: OwnedHandle,
}

// SAFETY: an HCS_PROCESS handle is valid in any thread of the process.
unsafe impl Send for HcsProcessHandle {}

impl Drop for HcsProcessHandle {
    fn drop(&mut self) {
        if !self.0.is_null()
            && let Ok(api) = api()
        {
            // SAFETY: self owns the handle and Drop runs once.
            unsafe { (api.close_process)(self.0) };
        }
    }
}

/// The longest mount path `GetLayerMountPath` may give, in UTF-16 units: a volume GUID path is 49, and
/// the longest Windows path 32,767 (CLAUDE.md §2: a longer answer is refused, not allocated).
const MAX_MOUNT_PATH: usize = 32_767;

fn layer_call(f: impl FnOnce(&Api, &DriverInfo) -> HRESULT) -> Result<(), HcsFailure> {
    let api = api().map_err(|m| HcsFailure {
        code: 0x8037_0114,
        message: Some(m.to_owned()),
    })?;
    let home = [0u16];
    let info = DriverInfo {
        flavour: 1,
        home: home.as_ptr(),
    };
    let hr = f(api, &info);
    if hr < 0 {
        return Err(HcsFailure::of(hr, None));
    }
    Ok(())
}

/// `ActivateLayer`: attaches scratch folder `scratch`'s `sandbox.vhdx` as a volume (note §3).
pub fn activate_layer(scratch: &str) -> Result<(), HcsFailure> {
    let id = wide(scratch);
    // SAFETY: info and id are valid for the call: a DRIVER_INFO whose home is a NUL-terminated empty wide
    // string, and a NUL-terminated wide string.
    layer_call(|api, info| unsafe { (api.activate_layer)(info, id.as_ptr()) })
}

/// `PrepareLayer`: puts the storage filter over activated scratch `scratch`, its parents `parents`
/// topmost first, each named by [`hex_core::hcs::layer_guid`] as the container's document names it.
pub fn prepare_layer(scratch: &str, parents: &[String]) -> Result<(), HcsFailure> {
    let id = wide(scratch);
    let paths: Vec<Vec<u16>> = parents.iter().map(|p| wide(p)).collect();
    let descriptors: Vec<LayerDescriptor> = parents
        .iter()
        .zip(&paths)
        .map(|(p, w)| LayerDescriptor {
            id: GUID::from_u128(hex_core::hcs::layer_guid(p)),
            flags: 0,
            path: w.as_ptr(),
        })
        .collect();
    let count = u32::try_from(descriptors.len()).map_err(|_| HcsFailure {
        code: 0x8007_0057,
        message: Some(format!(
            "{} layers is more than PrepareLayer takes",
            descriptors.len()
        )),
    })?;
    // SAFETY: info and id as in activate_layer; descriptors holds count descriptors whose paths point into
    // paths, NUL-terminated wide strings, all of which outlive the call.
    layer_call(|api, info| unsafe {
        (api.prepare_layer)(info, id.as_ptr(), descriptors.as_ptr(), count)
    })
}

/// `GetLayerMountPath`: the prepared scratch's volume path, `\\?\Volume{GUID}` (its root without the
/// trailing backslash; `hex_core::hcs::volume_root` adds it).
pub fn layer_mount_path(scratch: &str) -> Result<String, HcsFailure> {
    let id = wide(scratch);
    let mut len = 0usize;
    // SAFETY: info and id as in activate_layer; len is a valid out-pointer, and a null buffer asks for
    // the length alone (hcsshim/internal/wclayer/getlayermountpath.go:26-30).
    layer_call(|api, info| unsafe {
        (api.layer_mount_path)(info, id.as_ptr(), &mut len, null_mut())
    })?;
    if len == 0 || len > MAX_MOUNT_PATH {
        return Err(HcsFailure {
            code: 0x8007_000D,
            message: Some(format!("GetLayerMountPath gave a length of {len}")),
        });
    }
    let mut buf = vec![0u16; len];
    // SAFETY: as above, with buf a buffer of len UTF-16 units, the length the call just gave.
    layer_call(|api, info| unsafe {
        (api.layer_mount_path)(info, id.as_ptr(), &mut len, buf.as_mut_ptr())
    })?;
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16(buf.get(..end).unwrap_or_default()).map_err(|_| HcsFailure {
        code: 0x8007_000D,
        message: Some("GetLayerMountPath gave a path that isn't UTF-16".to_owned()),
    })
}

/// `UnprepareLayer`: takes the storage filter off scratch `scratch`.
pub fn unprepare_layer(scratch: &str) -> Result<(), HcsFailure> {
    let id = wide(scratch);
    // SAFETY: info and id as in activate_layer.
    layer_call(|api, info| unsafe { (api.unprepare_layer)(info, id.as_ptr()) })
}

/// `DeactivateLayer`: detaches scratch `scratch`'s volume.
pub fn deactivate_layer(scratch: &str) -> Result<(), HcsFailure> {
    let id = wide(scratch);
    // SAFETY: info and id as in activate_layer.
    layer_call(|api, info| unsafe { (api.deactivate_layer)(info, id.as_ptr()) })
}

/// Opens process `pid` for `access`.
pub fn open_process(pid: u32, access: u32) -> io::Result<Handle> {
    // SAFETY: OpenProcess takes plain values and returns a handle or null, checked.
    let h = unsafe { OpenProcess(access, 0, pid) };
    if h.is_null() {
        return Err(last_error());
    }
    // SAFETY: h is the process handle just opened, owned by nothing else.
    Ok(unsafe { Handle::own(h) })
}

/// The access the node opens a launcher with: to duplicate handles into it and to end it.
pub const LAUNCHER_ACCESS: u32 =
    PROCESS_DUP_HANDLE | windows_sys::Win32::System::Threading::PROCESS_TERMINATE;
/// The access the node holds a member with: to wait on it, read its exit and job, and end it.
pub const MEMBER_ACCESS: u32 = PROCESS_SYNCHRONIZE
    | PROCESS_QUERY_LIMITED_INFORMATION
    | windows_sys::Win32::System::Threading::PROCESS_TERMINATE;
/// The access the launcher gets to the box's job: to start a member in it, and nothing else.
pub const JOB_ACCESS: u32 = JOB_OBJECT_ASSIGN_PROCESS;

/// Duplicates `handle` into `target` with `access` (or the same access where `None`), not inheritable;
/// returns its value in `target`.
pub fn duplicate_into(
    target: &Handle,
    handle: BorrowedHandle<'_>,
    access: Option<u32>,
) -> io::Result<u64> {
    let mut out: HANDLE = null_mut();
    // SAFETY: handle is a live handle of this process (borrowed for the call), target a process handle
    // with PROCESS_DUP_HANDLE; GetCurrentProcess is the pseudo-handle for this process; out receives the
    // value in target.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle.as_raw_handle() as HANDLE,
            target.raw(),
            &mut out,
            access.unwrap_or(0),
            0,
            if access.is_some() {
                0
            } else {
                DUPLICATE_SAME_ACCESS
            },
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(out as usize as u64)
}

/// Ends `process` with `code` (a launcher that never replied).
pub fn terminate_process(process: &Handle, code: u32) -> io::Result<()> {
    // SAFETY: process is a live process handle with PROCESS_TERMINATE.
    if unsafe { TerminateProcess(process.raw(), code) } == 0 {
        return Err(last_error());
    }
    Ok(())
}

/// Whether `process` is in `job`.
pub fn in_job(process: &Handle, job: BorrowedHandle<'_>) -> io::Result<bool> {
    let mut result = 0;
    // SAFETY: process and job are live handles (job borrowed for the call); result is a valid out-pointer.
    if unsafe { IsProcessInJob(process.raw(), job.as_raw_handle() as HANDLE, &mut result) } == 0 {
        return Err(last_error());
    }
    Ok(result != 0)
}

/// Takes ownership, in the launcher, of the box's job and the member's three standard handles: the values
/// the node duplicated into this process for its one request (`super::launch`). Only the launcher calls
/// this, once, with the values its request carried; a value of 0 is refused.
pub fn adopt(job: u64, stdio: [u64; 3]) -> io::Result<(OwnedHandle, [OwnedHandle; 3])> {
    let own = |value: u64| {
        let raw = usize::try_from(value).map_err(io::Error::other)? as RawHandle;
        if raw.is_null() {
            return Err(io::Error::other("a null handle value"));
        }
        // SAFETY: the node duplicated this value into the launcher for this request alone (DuplicateHandle
        // into the launcher's process, `super::launch::start`), so it is a live handle here that nothing
        // else owns, and the launcher adopts each once.
        Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
    };
    let [i, o, e] = stdio;
    Ok((own(job)?, [own(i)?, own(o)?, own(e)?]))
}

/// The host's build and kind (`RtlGetVersion`, which, unlike `GetVersionEx`, isn't shimmed by a manifest).
pub fn host() -> Host {
    // SAFETY: OSVERSIONINFOEXW is plain data; all-zero then its size set is the documented input.
    let mut v: OSVERSIONINFOEXW = unsafe { zeroed() };
    v.dwOSVersionInfoSize = u32::try_from(size_of::<OSVERSIONINFOEXW>()).unwrap_or(0);
    // SAFETY: v is an OSVERSIONINFOEXW with its size set, which RtlGetVersion accepts through its
    // OSVERSIONINFOW pointer (the structure begins with it); it fills v and returns STATUS_SUCCESS.
    unsafe { RtlGetVersion((&raw mut v).cast()) };
    Host {
        build: v.dwBuildNumber,
        kind: if v.wProductType == VER_NT_WORKSTATION {
            HostKind::Client
        } else {
            HostKind::Server
        },
    }
}

/// The build in `path`'s version resource (`VS_FIXEDFILEINFO.dwFileVersionLS`'s high word): for a base
/// layer's `ntdll.dll`, the image's Windows build.
pub fn file_build(path: &str) -> io::Result<u32> {
    let p = wide(path);
    let mut ignored = 0u32;
    // SAFETY: p is a NUL-terminated wide string; ignored is a valid out-pointer.
    let size = unsafe { GetFileVersionInfoSizeW(p.as_ptr(), &mut ignored) };
    if size == 0 {
        return Err(last_error());
    }
    let mut block = vec![0u8; usize::try_from(size).map_err(io::Error::other)?];
    // SAFETY: block holds size bytes, as GetFileVersionInfoW was told.
    if unsafe { GetFileVersionInfoW(p.as_ptr(), 0, size, block.as_mut_ptr().cast()) } == 0 {
        return Err(last_error());
    }
    let root = wide("\\");
    let mut info: *mut c_void = null_mut();
    let mut len = 0u32;
    // SAFETY: block is the version resource just read; VerQueryValueW points info into it.
    if unsafe { VerQueryValueW(block.as_ptr().cast(), root.as_ptr(), &mut info, &mut len) } == 0
        || info.is_null()
        || usize::try_from(len).unwrap_or(0) < size_of::<VS_FIXEDFILEINFO>()
    {
        return Err(io::Error::other("no fixed version information"));
    }
    // SAFETY: info points at a VS_FIXEDFILEINFO of len bytes inside block, which is alive; it may be
    // unaligned, so it is read unaligned.
    let fixed = unsafe { info.cast::<VS_FIXEDFILEINFO>().read_unaligned() };
    Ok(fixed.dwFileVersionLS >> 16)
}

/// The present devices implementing interface class `class` as seen from where this process runs: how
/// many `CM_Get_Device_Interface_List*` lists, and how many of those open. The probe for "a device not
/// granted isn't visible", read as reachable (the lead's ruling, 2026-11-06).
pub fn device_interfaces(class: &GUID) -> io::Result<DeviceView> {
    let mut len = 0u32;
    // SAFETY: class is a valid GUID; len is a valid out-pointer; no device instance filter.
    let cr = unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &mut len,
            class,
            null(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS {
        return Err(io::Error::other(format!(
            "CM_Get_Device_Interface_List_SizeW: {cr}"
        )));
    }
    if len > MAX_INTERFACE_UNITS {
        return Err(io::Error::other(format!(
            "{len} units of device interfaces is past the bound"
        )));
    }
    let mut buf = vec![0u16; usize::try_from(len).map_err(io::Error::other)?];
    // SAFETY: buf holds len units, as told; class is a valid GUID.
    let cr = unsafe {
        CM_Get_Device_Interface_ListW(
            class,
            null(),
            buf.as_mut_ptr(),
            len,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS {
        return Err(io::Error::other(format!(
            "CM_Get_Device_Interface_ListW: {cr}"
        )));
    }
    // A double-NUL-terminated list of NUL-terminated names: each one listed, and opened to see whether
    // it is reachable from here.
    let mut view = DeviceView::default();
    for name in buf.split(|u| *u == 0).filter(|s| !s.is_empty()) {
        view.listed = view.listed.saturating_add(1);
        let path: Vec<u16> = name.iter().copied().chain([0]).collect();
        // A query open (no access asked): it opens a plumbed device and is refused one the container
        // wasn't given (jupiter, 2026-11-06: USB, keyboard, mouse and disk classes, 0 of n opened ungranted,
        // n of n granted). SAFETY: path is a NUL-terminated wide string that outlives the call; no security
        // attributes or template; the result is a handle or INVALID_HANDLE_VALUE, checked.
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE && !h.is_null() {
            // SAFETY: h is the handle just opened, owned by nothing else; dropping it closes it.
            drop(unsafe { Handle::own(h) });
            view.reachable = view.reachable.saturating_add(1);
        }
    }
    Ok(view)
}

type OpenAdapter = unsafe extern "system" fn(*mut D3DKMT_OPENADAPTERFROMDEVICENAME) -> i32;
type CloseAdapter = unsafe extern "system" fn(*const D3DKMT_CLOSEADAPTER) -> i32;
type QueryAdapter = unsafe extern "system" fn(*mut D3DKMT_QUERYADAPTERINFO) -> i32;

struct AdapterCalls {
    open: OpenAdapter,
    query: QueryAdapter,
    close: CloseAdapter,
}

#[derive(Clone, Copy)]
struct AdapterLoadFailure {
    op: &'static str,
    code: u32,
}

fn adapter_failure(op: &'static str, namespace: &'static str, code: u32) -> Refusal {
    use std::fmt::Write;
    // Prefix plus " 0x" and the complete eight hexadecimal digits of a Windows status.
    let Some(bytes) = namespace
        .len()
        .checked_add(" 0x".len())
        .and_then(|n| n.checked_add(size_of::<u32>().checked_mul(2)?))
    else {
        return Refusal::Exhausted {
            what: "native GPU status bytes",
            bound: usize::MAX,
        };
    };
    let mut why = String::new();
    if why.try_reserve_exact(bytes).is_err() || write!(why, "{namespace} {code:#010x}").is_err() {
        return Refusal::Exhausted {
            what: "native GPU status bytes",
            bound: bytes,
        };
    }
    Refusal::Os { op, why }
}

fn adapter_invalid(message: &'static str) -> Refusal {
    let mut why = String::new();
    if why.try_reserve_exact(message.len()).is_err() {
        return Refusal::Exhausted {
            what: "native GPU refusal bytes",
            bound: message.len(),
        };
    }
    why.push_str(message);
    Refusal::Invalid {
        what: "GPU PNP identity",
        why,
    }
}

impl AdapterCalls {
    fn load() -> Result<Self, AdapterLoadFailure> {
        // SAFETY: a static terminated DLL name, System32 only, no inherited file handle.
        let library = unsafe {
            LoadLibraryExW(
                windows_sys::core::w!("gdi32.dll"),
                null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if library.is_null() {
            return Err(AdapterLoadFailure {
                op: "loading native GPU adapter queries",
                // SAFETY: captures this thread's last error immediately after the failed load.
                code: unsafe { GetLastError() },
            });
        }
        // SAFETY: both static names are terminated and library remains loaded for the function
        // pointers' whole process lifetime, as the existing HCS loader does.
        let open = unsafe {
            GetProcAddress(
                library,
                windows_sys::core::s!("D3DKMTOpenAdapterFromDeviceName"),
            )
        };
        let Some(open) = open else {
            return Err(AdapterLoadFailure {
                op: "resolving D3DKMTOpenAdapterFromDeviceName",
                // SAFETY: immediate capture of this thread's export lookup failure.
                code: unsafe { GetLastError() },
            });
        };
        // SAFETY: same static name and module lifetime as above.
        let close = unsafe { GetProcAddress(library, windows_sys::core::s!("D3DKMTCloseAdapter")) };
        let Some(close) = close else {
            return Err(AdapterLoadFailure {
                op: "resolving D3DKMTCloseAdapter",
                // SAFETY: immediate capture of this thread's export lookup failure.
                code: unsafe { GetLastError() },
            });
        };
        // SAFETY: same static name and module lifetime as above.
        let query =
            unsafe { GetProcAddress(library, windows_sys::core::s!("D3DKMTQueryAdapterInfo")) };
        let Some(query) = query else {
            return Err(AdapterLoadFailure {
                op: "resolving D3DKMTQueryAdapterInfo",
                // SAFETY: immediate capture of this thread's export lookup failure.
                code: unsafe { GetLastError() },
            });
        };
        Ok(Self {
            // SAFETY: documented Gdi32 signatures, using Microsoft's generated SDK structures.
            open: unsafe {
                std::mem::transmute::<unsafe extern "system" fn() -> isize, OpenAdapter>(open)
            },
            // SAFETY: documented Gdi32 query signature, using the generated SDK structure.
            query: unsafe {
                std::mem::transmute::<unsafe extern "system" fn() -> isize, QueryAdapter>(query)
            },
            // SAFETY: documented Gdi32 close signature; a KMT handle is never a Win32 HANDLE.
            close: unsafe {
                std::mem::transmute::<unsafe extern "system" fn() -> isize, CloseAdapter>(close)
            },
        })
    }
}

/// Re-resolves one exact PNP instance to its current-boot adapter LUID. No catalog, holder or
/// ordinal is inferred here. The initialized provider must also prove full PCI/UUID correspondence.
/// Microsoft GUID_DISPLAY_DEVICE_ARRIVAL explicitly supports this D3DKMT open; the similarly named
/// 5B452 display-view interface is not substituted (docs/hex-windows-gpu-compute-mapping).
pub fn adapter_luid_for_instance(instance: &str) -> Result<[u8; 8], Refusal> {
    const ARRIVAL: GUID = GUID::from_u128(0x1ca05180_a699_450a_9a0c_de4fbe3ddd89);
    if instance.is_empty() || instance.contains('\0') {
        return Err(adapter_invalid(
            "an exact nonempty instance without NUL is required",
        ));
    }
    let bound = usize::try_from(MAX_INTERFACE_UNITS).map_err(|_| Refusal::Exhausted {
        what: "GPU interface units",
        bound: usize::MAX,
    })?;
    let units = instance
        .encode_utf16()
        .count()
        .checked_add(1)
        .filter(|n| *n <= bound)
        .ok_or(Refusal::Exhausted {
            what: "GPU instance units",
            bound,
        })?;
    let mut identity = Vec::new();
    identity
        .try_reserve_exact(units)
        .map_err(|_| Refusal::Exhausted {
            what: "GPU instance units",
            bound,
        })?;
    identity.extend(instance.encode_utf16());
    identity.push(0);
    let mut len = 0u32;
    // SAFETY: initialized output and static interface GUID; this non-null terminated instance
    // filters the query to that exact device rather than enumerating all graphics adapters.
    let status = unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &mut len,
            &ARRIVAL,
            identity.as_ptr(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if status != CR_SUCCESS {
        return Err(adapter_failure(
            "sizing exact GPU interfaces",
            "CONFIGRET",
            status,
        ));
    }
    if len > MAX_INTERFACE_UNITS {
        return Err(Refusal::Exhausted {
            what: "GPU interface units",
            bound,
        });
    }
    let len_usize = usize::try_from(len).map_err(|_| Refusal::Exhausted {
        what: "GPU interface units",
        bound,
    })?;
    let mut names = Vec::new();
    names
        .try_reserve_exact(len_usize)
        .map_err(|_| Refusal::Exhausted {
            what: "GPU interface units",
            bound,
        })?;
    names.resize(len_usize, 0u16);
    if names.len() < [0u16, 0].len() {
        return Err(adapter_invalid(
            "native interface list cannot hold a double NUL",
        ));
    }
    // SAFETY: same exact filter and GUID, a fully initialized output sized by CM. A changing
    // list returns its raw error, with no retry or substitute adapter.
    let status = unsafe {
        CM_Get_Device_Interface_ListW(
            &ARRIVAL,
            identity.as_ptr(),
            names.as_mut_ptr(),
            len,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if status != CR_SUCCESS {
        return Err(adapter_failure(
            "reading exact GPU interfaces",
            "CONFIGRET",
            status,
        ));
    }
    static CALLS: OnceLock<Result<AdapterCalls, AdapterLoadFailure>> = OnceLock::new();
    let calls = CALLS
        .get_or_init(AdapterCalls::load)
        .as_ref()
        .map_err(|f| adapter_failure(f.op, "Win32", f.code))?;
    adapter_luid_in(&names, calls)
}

fn adapter_luid_in(names: &[u16], calls: &AdapterCalls) -> Result<[u8; 8], Refusal> {
    if !names.ends_with(&[0, 0]) {
        return Err(adapter_invalid(
            "native interface list is not double-NUL terminated",
        ));
    }
    let count_bytes = u32::try_from(size_of::<D3DKMT_PHYSICAL_ADAPTER_COUNT>())
        .map_err(|_| adapter_invalid("physical adapter count exceeds its query size"))?;
    let mut found = None;
    let mut at = 0usize;
    while at < names.len() {
        let tail = names
            .get(at..)
            .ok_or_else(|| adapter_invalid("native interface offset escapes its buffer"))?;
        let Some(end) = tail.iter().position(|unit| *unit == 0) else {
            return Err(adapter_invalid("native interface has no terminating NUL"));
        };
        if end == 0 {
            if tail.iter().any(|unit| *unit != 0) {
                return Err(adapter_invalid(
                    "native interface list has data after its terminator",
                ));
            }
            break;
        }
        let mut adapter = D3DKMT_OPENADAPTERFROMDEVICENAME {
            pDeviceName: tail.as_ptr(),
            ..Default::default()
        };
        // SAFETY: tail contains a terminating NUL and outlives this synchronous query; the
        // generated SDK structure holds all initialized input and output fields.
        let status = unsafe { (calls.open)(&mut adapter) };
        if status < 0 {
            return Err(adapter_failure(
                "mapping exact GPU interface",
                "NTSTATUS",
                u32::from_ne_bytes(status.to_ne_bytes()),
            ));
        }
        if adapter.hAdapter == 0 {
            return Err(adapter_failure(
                "mapping exact GPU interface returned no handle",
                "NTSTATUS",
                u32::from_ne_bytes(status.to_ne_bytes()),
            ));
        }
        let luid = adapter.AdapterLuid;
        // A linked display adapter's one LUID covers each of its physical adapters (WDDM 2.0
        // KMTQAITYPE_PHYSICALADAPTERCOUNT, d3dkmthk.h). Until a node map joins this PNP device to
        // its node, only a single physical adapter's LUID names this exact GPU.
        let mut nodes = D3DKMT_PHYSICAL_ADAPTER_COUNT::default();
        let mut count = D3DKMT_QUERYADAPTERINFO {
            hAdapter: adapter.hAdapter,
            Type: KMTQAITYPE_PHYSICALADAPTERCOUNT,
            pPrivateDriverData: (&raw mut nodes).cast(),
            PrivateDriverDataSize: count_bytes,
        };
        // SAFETY: the handle this open just returned, and an initialized output of exactly the
        // stated size that outlives this synchronous query.
        let queried = unsafe { (calls.query)(&mut count) };
        // SAFETY: this successful open owns exactly this KMT handle; it is closed once through
        // D3DKMTCloseAdapter, never CloseHandle, whatever the query returned. Failure is surfaced,
        // not a successful mapping.
        let closed = unsafe {
            (calls.close)(&D3DKMT_CLOSEADAPTER {
                hAdapter: adapter.hAdapter,
            })
        };
        if closed < 0 {
            return Err(adapter_failure(
                "closing exact GPU adapter query",
                "NTSTATUS",
                u32::from_ne_bytes(closed.to_ne_bytes()),
            ));
        }
        if queried < 0 {
            return Err(adapter_failure(
                "counting the exact GPU adapter's physical adapters",
                "NTSTATUS",
                u32::from_ne_bytes(queried.to_ne_bytes()),
            ));
        }
        if nodes.Count != 1 {
            return Err(adapter_invalid(
                "the exact GPU's adapter is not one physical adapter, and no node map joins it",
            ));
        }
        let [a, b, c, d] = luid.LowPart.to_le_bytes();
        let [e, f, g, h] = luid.HighPart.to_le_bytes();
        let current = [a, b, c, d, e, f, g, h];
        if found.is_some_and(|previous| previous != current) {
            return Err(adapter_invalid(
                "present aliases disagree on the complete current adapter LUID",
            ));
        }
        found = Some(current);
        at = at
            .checked_add(end)
            .and_then(|n| n.checked_add(1))
            .ok_or(Refusal::Exhausted {
                what: "GPU interface units",
                bound: names.len(),
            })?;
    }
    found.ok_or_else(|| {
        adapter_invalid("the exact instance has no present adapter-arrival interface")
    })
}

/// A device interface class as seen from where this process runs: how many of its interfaces are listed,
/// and how many of those open (a process-isolated container lists the host's, and opens only those its
/// grant plumbed in: the lead's ruling on "visible", 2026-11-06).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeviceView {
    pub listed: usize,
    pub reachable: usize,
}

#[cfg(all(test, windows))]
#[path = "assigned_native_tests.rs"]
mod assigned_native_tests;

#[cfg(test)]
mod create_tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct Calls {
        waits: VecDeque<HRESULT>,
        budgets: Vec<u32>,
        closed: usize,
        cancelled: usize,
        released: usize,
    }
    thread_local! { static CALLS: RefCell<Calls> = RefCell::new(Calls::default()); }

    pub(super) fn released() {
        CALLS.with_borrow_mut(|calls| calls.released += 1);
    }

    unsafe extern "system" fn wait(_: HCS_OPERATION, budget: u32, _: *mut PWSTR) -> HRESULT {
        CALLS.with_borrow_mut(|calls| {
            calls.budgets.push(budget);
            calls.waits.pop_front().unwrap()
        })
    }
    unsafe extern "system" fn cancel(_: HCS_OPERATION) -> HRESULT {
        CALLS.with_borrow_mut(|calls| calls.cancelled += 1);
        windows_sys::Win32::Foundation::E_NOTIMPL
    }
    unsafe extern "system" fn close(_: HCS_OPERATION) {
        CALLS.with_borrow_mut(|calls| calls.closed += 1);
    }

    fn pending(waits: &[HRESULT]) -> PendingCreate {
        CALLS.with_borrow_mut(|calls| {
            *calls = Calls {
                waits: waits.iter().copied().collect(),
                ..Calls::default()
            }
        });
        let owned = CreateContext::allocate(
            CreateInputs {
                id: create_wide("owned-synthetic-id").unwrap(),
                document: create_wide("synthetic-private-document").unwrap(),
            },
            CreateCalls {
                wait,
                cancel,
                close,
            },
        )
        .unwrap();
        {
            let mut data = owned.context().data.lock().unwrap();
            data.operation = Some(std::ptr::without_provenance_mut(1)); // Synthetic callback ID only.
            data.state = CreateOperationState::Active;
        }
        owned
    }

    fn complete(pointer: std::ptr::NonNull<CreateContext>) {
        // SAFETY: the fixture owns the one synthetic native completion receipt, with fake calls only.
        unsafe { create_completed(std::ptr::without_provenance_mut(1), pointer.as_ptr().cast()) };
    }

    /// T-T7.1.2: fake existing calls, no native HCS; completion alone releases owned input/operation.
    #[test]
    fn device_create_pending_keeps_inputs_and_closes_once_after_terminal_result() {
        let mut create = pending(&[
            windows_sys::Win32::Foundation::HCS_E_OPERATION_TIMEOUT,
            windows_sys::Win32::Foundation::HCS_E_OPERATION_PENDING,
            windows_sys::Win32::Foundation::E_ABORT,
        ]);
        assert!(create.query().is_none());
        assert!(create.query().is_none());
        {
            let data = create.context().data.lock().unwrap();
            assert!(matches!(data.state, CreateOperationState::Active));
            let inputs = data.inputs.as_ref().unwrap();
            assert_eq!(inputs.id, create_wide("owned-synthetic-id").unwrap());
            assert_eq!(
                inputs.document,
                create_wide("synthetic-private-document").unwrap()
            );
        }
        CALLS.with_borrow(|calls| assert_eq!(calls.closed, 0));
        let terminal = create.query().unwrap();
        assert!(terminal.result.is_err());
        assert!(terminal.system.is_none());
        assert!(create.query().is_none());
        CALLS.with_borrow(|calls| assert_eq!(calls.closed, 0)); // A query must not cancel a queued callback.
        complete(create.context);
        drop(create);
        CALLS.with_borrow(|calls| {
            assert_eq!(calls.closed, 1);
            assert_eq!(calls.released, 1);
            assert_eq!(calls.budgets, vec![0, 0, 0]);
        });
    }

    #[test]
    fn device_create_cancel_refusal_is_not_completion_and_does_not_repeat_or_close_pending() {
        let mut create = pending(&[
            windows_sys::Win32::Foundation::HCS_E_OPERATION_TIMEOUT,
            windows_sys::Win32::Foundation::E_ABORT,
        ]);
        let code = u32::from_ne_bytes(windows_sys::Win32::Foundation::E_NOTIMPL.to_ne_bytes());
        assert_eq!(create.cancel(), CreateCancellation::Refused(code));
        assert_eq!(create.cancel(), CreateCancellation::Refused(code));
        assert!(create.query().is_none());
        CALLS.with_borrow(|calls| assert_eq!((calls.cancelled, calls.closed), (1, 0)));
        assert!(create.query().unwrap().result.is_err());
        complete(create.context);
        drop(create);
        CALLS.with_borrow(|calls| assert_eq!((calls.cancelled, calls.closed), (1, 1)));
    }

    #[test]
    fn device_create_callback_before_native_return_never_waits_on_creator_mutex() {
        let mut create = pending(&[windows_sys::Win32::Foundation::E_ABORT]);
        let pointer = create.context;
        {
            let mut data = create.context().data.lock().unwrap();
            data.state = CreateOperationState::NotStarted;
            complete(pointer); // Inline native callback while the creating thread owns its inputs.
            assert!(data.inputs.is_some());
            CALLS.with_borrow(|calls| assert_eq!((calls.closed, calls.released), (0, 0)));
            data.state = CreateOperationState::Active; // The native create call has now returned.
        }
        assert!(create.query().unwrap().result.is_err());
        drop(create);
        CALLS.with_borrow(|calls| assert_eq!((calls.closed, calls.released), (1, 1)));
    }

    #[test]
    fn device_create_drop_before_completion_keeps_callback_owner_then_reclaims_exact_context() {
        let create = pending(&[windows_sys::Win32::Foundation::E_ABORT]);
        let pointer = create.context;
        drop(create);
        CALLS.with_borrow(|calls| assert_eq!((calls.closed, calls.released), (0, 0)));
        complete(pointer);
        CALLS.with_borrow(|calls| {
            assert_eq!((calls.closed, calls.released), (1, 1));
            assert_eq!(calls.budgets, vec![0]);
        });
    }

    #[test]
    fn device_create_synchronous_refusal_releases_unused_completion_receipt_once() {
        let mut create = pending(&[]);
        let pointer = create.context;
        {
            let context = create.context();
            let mut data = context.data.lock().unwrap();
            data.state = CreateOperationState::Terminal(Err(create_fault(
                windows_sys::Win32::Foundation::E_INVALIDARG,
            )));
            data.close(context.calls);
        }
        release_create_callback(pointer); // Native create never started, so no future callback exists.
        assert!(create.query().unwrap().result.is_err());
        drop(create);
        CALLS.with_borrow(|calls| {
            assert_eq!((calls.closed, calls.released), (1, 1));
            assert!(calls.budgets.is_empty());
        });
    }

    /// Actual Runtime/Holding callbacks over a synthetic HCS operation and one ordinary owned Job.
    /// No HCS, WMI, container, GPU, child program or keeper is launched.
    #[test]
    fn device_create_pending_runtime_retains_claims_then_terminal_cleanup_releases_marker() {
        use crate::devices::Catalog;
        use crate::windows as win;
        use hex_api::{BoxId, BoxSpec, Class, ClassSpec, DeviceId, DeviceKind, Runtime};
        use std::sync::{Arc, Mutex};
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("bundle");
        std::fs::create_dir(&bundle).unwrap();
        let config = serde_json::json!({
            "ociVersion":"1.2.0", "root":{"path":r"\\?\Volume{ec84d99e-3f02-11e7-ac6c-00155d7682cf}\"},
            "process":{"args":["cmd.exe"],"cwd":"C:\\"},
            "windows":{"layerFolders":["C:\\fixture\\base","C:\\fixture\\caller-scratch"]}
        });
        std::fs::write(
            bundle.join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let rt = win::WinRuntime {
            processors: std::num::NonZeroU32::MIN,
            catalog: win::NativeCatalog::Shared(
                crate::devices::SharedCatalog::try_new(
                    Catalog::parse(
                        b"hex-devices-1\ngpu\t256\tfixture\t4096\thcs\tPCIROOT(0)#PCI(0100)\t\n",
                    )
                    .unwrap(),
                )
                .unwrap(),
            ),
            scratch_root: root.path().to_path_buf(),
            launcher: None,
            holding: Arc::new(win::Holding {
                boxes: Mutex::new(Vec::new()),
                keeper: None,
            }),
            node: None,
            features_off: OnceLock::from(std::collections::BTreeSet::new()),
            feature_reads: std::sync::atomic::AtomicU32::new(0),
            reclaim_failures: Vec::new(),
        };
        let spec = BoxSpec {
            allocation: None,
            route: None,
            id: BoxId::new("pending-runtime").unwrap(),
            bounds: hex_api::Bounds::default(),
            owner: None,
            class: ClassSpec::Bundle(bundle),
            devices: vec![DeviceId {
                kind: DeviceKind::Gpu,
                key: 256,
            }],
        };
        let bindings = rt.bindings(&spec).unwrap();
        let container = rt.container(&spec).unwrap();
        let marker = rt.marker(&spec.id).unwrap();
        let mut mounted = super::super::scratch(
            &container,
            "unused-borrowed-base",
            Some(marker.clone()),
            &spec.id,
            hcs::Lifetime::EndsWithNode,
            &bindings,
        )
        .unwrap();
        mounted.creation(hcs::NativeCreate::Pending).unwrap();
        mounted.owner = super::super::ScratchOwner::Uncertain;
        let bytes = std::fs::read(&marker).unwrap();
        let create = pending(&[
            windows_sys::Win32::Foundation::HCS_E_OPERATION_TIMEOUT,
            windows_sys::Win32::Foundation::HCS_E_OPERATION_TIMEOUT,
            windows_sys::Win32::Foundation::E_ABORT,
        ]);
        let completion = create.context;
        let job =
            win::process::JobBox::create(spec.id.clone(), &spec.bounds, std::num::NonZeroU32::MIN)
                .unwrap();
        let boxed = super::super::OciBox {
            job,
            system: Mutex::new(super::super::NativeSystem::Creating(create)),
            mounted: Mutex::new(Some(mounted)),
            user: None,
            init: container.init,
            life: Mutex::new(super::super::Life {
                started: false,
                init: None,
                exit: None,
            }),
            devices: bindings,
        };
        rt.holding
            .reserve(
                hex_core::record::OwnerRef::Box(&spec.id),
                &spec.devices,
                Some(marker.clone()),
                None,
            )
            .unwrap();
        let error = rt
            .finish_make(&spec.id, Ok(win::Held::Oci(Arc::new(boxed))))
            .unwrap_err();
        assert!(!error.to_string().contains("synthetic-private-document"));
        assert!(!matches!(
            rt.inspect(&spec.id),
            Err(hex_api::Refusal::NoSuchBox(_))
        ));
        assert_eq!(std::fs::read(&marker).unwrap(), bytes);
        let offer = rt.capacity(Class::Oci).unwrap().unwrap();
        assert_eq!(
            offer.devices[0].holder,
            Some(hex_api::DeviceHolder::Box(spec.id.clone()))
        );
        assert!(
            rt.holding
                .reserve(
                    hex_core::record::OwnerRef::Box(&BoxId::new("contender").unwrap()),
                    &spec.devices,
                    None,
                    None
                )
                .is_err()
        );
        CALLS.with_borrow(|calls| assert_eq!((calls.closed, calls.cancelled), (0, 1)));

        let sibling = BoxSpec {
            allocation: None,
            route: None,
            id: BoxId::new("independent-empty-job").unwrap(),
            bounds: hex_api::Bounds::default(),
            owner: None,
            class: ClassSpec::Process,
            devices: Vec::new(),
        };
        rt.create(sibling.clone()).unwrap();
        assert_eq!(
            rt.inspect(&sibling.id).unwrap().state,
            hex_api::BoxState::Empty
        );
        rt.delete(&spec.id).unwrap(); // The next fake observation is terminal E_ABORT, with no system.
        complete(completion);
        assert!(!marker.exists());
        assert_eq!(
            rt.capacity(Class::Oci).unwrap().unwrap().devices[0].holder,
            None
        );
        assert!(matches!(
            rt.inspect(&spec.id),
            Err(hex_api::Refusal::NoSuchBox(_))
        ));
        rt.delete(&sibling.id).unwrap();
        assert!(rt.boxes().unwrap().is_empty());
        CALLS.with_borrow(|calls| {
            assert_eq!((calls.closed, calls.cancelled), (1, 1));
            assert_eq!(calls.released, 1);
            assert_eq!(calls.budgets, vec![0, 0, 0]);
        });
    }
}

#[cfg(test)]
mod adapter_tests {
    use super::*;
    use std::cell::RefCell;

    /// One fake adapter: the physical adapter count and query status it reports.
    #[derive(Default)]
    struct Adapter {
        nodes: u32,
        query: i32,
        opened: usize,
        queried: usize,
        closed: usize,
    }
    thread_local! { static ADAPTER: RefCell<Adapter> = RefCell::new(Adapter::default()); }

    unsafe extern "system" fn open(adapter: *mut D3DKMT_OPENADAPTERFROMDEVICENAME) -> i32 {
        ADAPTER.with_borrow_mut(|fake| fake.opened += 1);
        // SAFETY: adapter_luid_in passes its own initialized, exclusively borrowed structure.
        let adapter = unsafe { &mut *adapter };
        adapter.hAdapter = 7;
        adapter.AdapterLuid.LowPart = 9;
        0
    }
    unsafe extern "system" fn query(info: *mut D3DKMT_QUERYADAPTERINFO) -> i32 {
        // SAFETY: adapter_luid_in passes its own initialized, exclusively borrowed structure.
        let info = unsafe { &mut *info };
        assert_eq!(
            info.hAdapter, 7,
            "the count is asked of the adapter just opened"
        );
        assert_eq!(info.Type, KMTQAITYPE_PHYSICALADAPTERCOUNT);
        assert_eq!(
            usize::try_from(info.PrivateDriverDataSize).unwrap(),
            size_of::<D3DKMT_PHYSICAL_ADAPTER_COUNT>()
        );
        ADAPTER.with_borrow_mut(|fake| {
            fake.queried += 1;
            // SAFETY: the output above is a live D3DKMT_PHYSICAL_ADAPTER_COUNT of the checked size.
            unsafe {
                (*info
                    .pPrivateDriverData
                    .cast::<D3DKMT_PHYSICAL_ADAPTER_COUNT>())
                .Count = fake.nodes
            };
            fake.query
        })
    }
    unsafe extern "system" fn close(adapter: *const D3DKMT_CLOSEADAPTER) -> i32 {
        // SAFETY: adapter_luid_in passes its own initialized structure.
        assert_eq!(unsafe { (*adapter).hAdapter }, 7);
        ADAPTER.with_borrow_mut(|fake| fake.closed += 1);
        0
    }

    fn join(nodes: u32, query_status: i32) -> (Result<[u8; 8], Refusal>, Adapter) {
        ADAPTER.with_borrow_mut(|fake| {
            *fake = Adapter {
                nodes,
                query: query_status,
                ..Adapter::default()
            }
        });
        let names: Vec<u16> = "\\\\?\\exact-interface\0\0".encode_utf16().collect();
        let calls = AdapterCalls { open, query, close };
        let joined = adapter_luid_in(&names, &calls);
        (joined, ADAPTER.take())
    }

    /// AC-T7.4.1: join an exact PNP interface whose adapter reports one physical adapter, then
    /// two (a linked display adapter) and then a failed count; expect only the first to give its
    /// LUID, and the adapter closed once in every case.
    #[test]
    fn only_a_single_physical_adapter_joins_its_luid() {
        let (joined, fake) = join(1, 0);
        assert_eq!(joined.unwrap(), [9, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!((fake.opened, fake.queried, fake.closed), (1, 1, 1));

        let (joined, fake) = join(2, 0);
        assert!(
            matches!(
                joined,
                Err(Refusal::Invalid {
                    what: "GPU PNP identity",
                    ..
                })
            ),
            "{joined:?}"
        );
        assert_eq!((fake.opened, fake.queried, fake.closed), (1, 1, 1));

        // STATUS_INVALID_PARAMETER: a refused count leaves the node count unknown, never one.
        let (joined, fake) = join(1, i32::from_ne_bytes(0xC000_000Du32.to_ne_bytes()));
        assert!(matches!(joined, Err(Refusal::Os { .. })), "{joined:?}");
        assert_eq!((fake.opened, fake.queried, fake.closed), (1, 1, 1));
    }
}
