//! The ADL calls hex-gpu makes on Windows, which Rust can't make safely (CLAUDE.md §12): AMD's
//! `atiadlxx.dll` loaded at run time from System32 through `crate::nvml::sys`'s loader, so a node
//! without AMD's driver (an ARM64 Windows machine among them) starts and says why it reads no AMD GPU;
//! one ADL context made per [`Session`] and ended with it; and the six entry points called through the
//! pointers resolved there. Their signatures and structures are AMD's ADL SDK's (`adl_sdk.h`,
//! `adl_structures.h`, `adapter.h`, `overdrive8.h`, `graphics.h`: GPUOpen-LibrariesAndSDKs/display-library
//! at `84c881cb`), as `super::ffi` declares them. Each `unsafe` block states the invariant that makes it
//! sound.

// hex-gpu's second place `unsafe` is allowed (CLAUDE.md §12); every block below carries its SAFETY
// comment.
#![allow(unsafe_code)]

use std::ffi::{c_int, c_void};
use std::ptr::null_mut;
use std::sync::OnceLock;

use super::Calls;
use super::ffi::{
    ADL_OK, AdapterInfoArray, Adl2AdapterAdapterInfoGet, Adl2AdapterMemoryInfo2Get,
    Adl2AdapterNumberOfAdaptersGet, Adl2AdapterVramUsageGet, Adl2GraphicsVersionsX2Get,
    Adl2MainControlCreate, Adl2MainControlDestroy, Adl2NewQueryPmLogDataGet, AdlContextHandle,
    AdlMemoryInfo2, AdlPmLogDataBuffer, AdlVersionsInfoX2,
};
use crate::Absent;
use crate::nvml::load::{DynLoadError, load_api};
use crate::nvml::sys::{export, loader};

/// ADL's 64-bit library, which AMD's driver installs in System32.
const ADL_LIB_NAMES: &[&str] = &["atiadlxx.dll"];

/// `ADL_ERR_INVALID_PARAM`: what a buffer past `int` would be.
const INVALID_PARAM: c_int = -3;

/// ADL's entry points Hex calls, resolved once. The library is never unloaded, so they stay valid for
/// the process.
#[derive(Debug)]
pub(crate) struct Api {
    create: Adl2MainControlCreate,
    destroy: Adl2MainControlDestroy,
    count: Adl2AdapterNumberOfAdaptersGet,
    adapter_info: Adl2AdapterAdapterInfoGet,
    memory_info: Adl2AdapterMemoryInfo2Get,
    versions: Adl2GraphicsVersionsX2Get,
    pmlog: Adl2NewQueryPmLogDataGet,
    vram_usage: Adl2AdapterVramUsageGet,
}

impl Api {
    /// Loads the library `name` names and resolves every entry point Hex calls.
    pub(super) fn open(name: &'static str) -> Result<Api, DynLoadError> {
        let library = loader::load(name)?;
        Ok(Api {
            create: export!(library, c"ADL2_Main_Control_Create", Adl2MainControlCreate),
            destroy: export!(
                library,
                c"ADL2_Main_Control_Destroy",
                Adl2MainControlDestroy
            ),
            count: export!(
                library,
                c"ADL2_Adapter_NumberOfAdapters_Get",
                Adl2AdapterNumberOfAdaptersGet
            ),
            adapter_info: export!(
                library,
                c"ADL2_Adapter_AdapterInfo_Get",
                Adl2AdapterAdapterInfoGet
            ),
            memory_info: export!(
                library,
                c"ADL2_Adapter_MemoryInfo2_Get",
                Adl2AdapterMemoryInfo2Get
            ),
            versions: export!(
                library,
                c"ADL2_Graphics_VersionsX2_Get",
                Adl2GraphicsVersionsX2Get
            ),
            pmlog: export!(
                library,
                c"ADL2_New_QueryPMLogData_Get",
                Adl2NewQueryPmLogDataGet
            ),
            vram_usage: export!(
                library,
                c"ADL2_Adapter_VRAMUsage_Get",
                Adl2AdapterVramUsageGet
            ),
        })
    }
}

/// Set once ADL has loaded; never unloaded. A failure isn't kept: the next open tries again.
static ADL: OnceLock<Api> = OnceLock::new();

/// ADL, loaded once for the process.
pub(crate) fn adl() -> Result<&'static Api, DynLoadError> {
    if let Some(api) = ADL.get() {
        return Ok(api);
    }
    let api = load_api(ADL_LIB_NAMES, Api::open)?;
    Ok(ADL.get_or_init(|| api))
}

/// Why ADL gave no GPU, from why it didn't load.
pub(crate) fn absent(error: DynLoadError) -> Absent {
    match error {
        DynLoadError::LoadFailed { names, source } => Absent::NoLibrary {
            tried: names,
            error: source,
        },
        DynLoadError::MissingSymbol { name } => Absent::NoSymbol { name },
        DynLoadError::InitFailed { code } => Absent::Adl { code },
    }
}

/// The allocator ADL is handed for memory it returns to its caller. Every call Hex makes fills a
/// buffer Hex passes, so it refuses: a call that would need it fails rather than leaving memory no one
/// frees. On jupiter ADL never asked it (2026-11-09).
extern "system" fn refuse(_bytes: c_int) -> *mut c_void {
    null_mut()
}

fn check(code: c_int) -> Result<(), c_int> {
    if code >= ADL_OK { Ok(()) } else { Err(code) }
}

/// One ADL context, made at open and ended when the [`Gpus`](crate::Gpus) holding it is dropped.
#[derive(Debug)]
pub(crate) struct Session {
    api: &'static Api,
    context: AdlContextHandle,
}

// SAFETY: an ADL2 context is the state of one ADL client (adl_sdk.h: a client "has to pass" its
// context "to each subsequent ADL2 call"), tied to no thread; every call through it takes `&mut
// Session` (`Calls`), so moving it to another thread moves its only caller with it.
unsafe impl Send for Session {}
// SAFETY: every call into ADL takes `&mut Session`, so a shared reference reaches none: two threads
// holding `&Session` can't call ADL on one context at once.
unsafe impl Sync for Session {}

impl Session {
    /// A context of ADL's own, listing the adapters present and enabled (`iEnumConnectedAdapters` 1,
    /// as AMD's samples make theirs), or ADL's code when it can't make one.
    pub(crate) fn open(api: &'static Api) -> Result<Session, c_int> {
        let mut context: AdlContextHandle = null_mut();
        // SAFETY: refuse has ADL_MAIN_MALLOC_CALLBACK's signature and touches nothing; context is a
        // writable ADL_CONTEXT_HANDLE that ADL sets on success.
        check(unsafe { (api.create)(refuse, 1, &mut context) })?;
        Ok(Session { api, context })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // ADL documents no failure of a context's end that leaves its caller anything to do, and a drop
        // has no one to tell; the context is gone either way.
        // SAFETY: context is the one Session::open made, ended once, here; no call on it can follow.
        let _ = unsafe { (self.api.destroy)(self.context) };
    }
}

// Every call below is made on the context Session::open made, with pointers to the caller's own
// values, each valid and writable for the call's duration.
impl Calls for Session {
    fn count(&mut self) -> Result<c_int, c_int> {
        let mut count: c_int = 0;
        // SAFETY: count is a writable int.
        check(unsafe { (self.api.count)(self.context, &mut count) })?;
        Ok(count)
    }

    fn adapters(&mut self, rows: &mut AdapterInfoArray) -> Result<(), c_int> {
        let size = rows.input_size().ok_or(INVALID_PARAM)?;
        // SAFETY: rows holds the requested rows and as many more again, writable; ADL is told the
        // requested rows' size and writes at most that.
        check(unsafe { (self.api.adapter_info)(self.context, rows.as_mut_ptr(), size) })
    }

    fn memory(&mut self, adapter: c_int, out: &mut AdlMemoryInfo2) -> Result<(), c_int> {
        // SAFETY: out is a writable ADLMemoryInfo2; an adapter index ADL doesn't number is refused by
        // ADL, not read.
        check(unsafe { (self.api.memory_info)(self.context, adapter, out) })
    }

    fn versions(&mut self, out: &mut AdlVersionsInfoX2) -> Result<(), c_int> {
        // SAFETY: out is a writable ADLVersionsInfoX2.
        check(unsafe { (self.api.versions)(self.context, out) })
    }

    fn pmlog(&mut self, adapter: c_int, out: &mut AdlPmLogDataBuffer) -> Result<(), c_int> {
        // The pointer is the whole buffer's, its table first (repr(C)), so a driver writing a larger
        // table than the header's writes inside memory the pointer may reach.
        let table = std::ptr::from_mut(out).cast();
        // SAFETY: table is a writable ADLPMLogDataOutput with three more tables' room behind it, all of
        // one allocation the pointer reaches.
        check(unsafe { (self.api.pmlog)(self.context, adapter, table) })
    }

    fn vram_used_mb(&mut self, adapter: c_int) -> Result<c_int, c_int> {
        let mut megabytes: c_int = 0;
        // SAFETY: megabytes is a writable int.
        check(unsafe { (self.api.vram_usage)(self.context, adapter, &mut megabytes) })?;
        Ok(megabytes)
    }
}
