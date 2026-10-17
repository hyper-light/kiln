//! Windows 10 1809+: DNS-SD through dnsapi (`DnsServiceRegister`, `DnsServiceBrowse`,
//! `DnsServiceResolve`), declared by hand. The DNS Client service does the multicast DNS (its
//! socket, its firewall rules); kiln registers its service with it, browses for the others and
//! resolves them, with the same service type, instance names and TXT keys as kiln's own
//! responder, so both kinds see each other.
//!
//! The functions are looked up at run time (`LoadLibraryExW` + `GetProcAddress`): Windows before
//! 1809 lacks them, and a binary that imported them directly would not start there at all; kiln
//! falls back to its own multicast DNS instead. Callbacks arrive on the system's thread pool and
//! only hand [`Event`]s to the caller's sink (a channel into hyper-rt).
//!
//! Self-contained (std only), so it type-checks for Windows from any host:
//! `rustc --edition 2024 --crate-type lib --target x86_64-pc-windows-gnu`. Structure layouts on
//! 64-bit Windows are asserted at compile time against the values of the Windows SDK headers.

#![allow(unsafe_code)]

use std::ffi::{c_char, c_void};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Mutex, OnceLock, PoisonError};

pub const LABEL: &str = "Windows DNS-SD";

/// What to register.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Service {
    /// The instance label, e.g. `kiln server on box (1a2b3c4d)`.
    pub label: String,
    pub port: u16,
    pub txt: Vec<(String, String)>,
    /// The host name the SRV record points at (`kiln-1a2b3c4d.local`) and its addresses.
    pub host: String,
    pub addrs: Vec<IpAddr>,
}

/// What the system reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The browse is running.
    Started,
    /// The registration is live under this name.
    Registered { name: String },
    Found { label: String },
    Lost { label: String },
    Resolved { label: String, host: String, port: u16, txt: Vec<(String, String)>, addrs: Vec<IpAddr> },
    /// A call failed with a Win32 or DNS error code (`what`: start, browse, register, resolve).
    Failed { what: &'static str, code: i32 },
}

pub type Sink = Box<dyn Fn(Event) + Send + Sync + 'static>;

const DNS_REQUEST_PENDING: u32 = 9506;
const DNS_QUERY_REQUEST_VERSION1: u32 = 1;
const DNS_FREE_RECORD_LIST: u32 = 1;
const DNS_TYPE_PTR: u16 = 12;
const ERROR_CANCELLED: u32 = 1223;
const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x800;
/// Most records read from one browse callback.
const MAX_RECORDS: usize = 64;

/// `IP6_ADDRESS`.
#[repr(C, align(4))]
#[derive(Clone, Copy)]
struct Ip6Address {
    bytes: [u8; 16],
}

/// `DNS_SERVICE_INSTANCE`.
#[repr(C)]
struct Instance {
    instance_name: *mut u16,
    host_name: *mut u16,
    ip4: *mut u32,
    ip6: *mut Ip6Address,
    port: u16,
    priority: u16,
    weight: u16,
    property_count: u32,
    keys: *mut *mut u16,
    values: *mut *mut u16,
    interface_index: u32,
}

#[repr(C)]
struct Cancel {
    reserved: *mut c_void,
}

type RegisterComplete = unsafe extern "system" fn(u32, *mut c_void, *mut Instance);
type BrowseCallback = unsafe extern "system" fn(u32, *mut c_void, *mut DnsRecord);
type ResolveComplete = unsafe extern "system" fn(u32, *mut c_void, *mut Instance);

/// `DNS_SERVICE_REGISTER_REQUEST`.
#[repr(C)]
struct RegisterRequest {
    version: u32,
    interface_index: u32,
    instance: *mut Instance,
    callback: Option<RegisterComplete>,
    context: *mut c_void,
    credentials: *mut c_void,
    unicast_enabled: i32,
}

/// `DNS_SERVICE_BROWSE_REQUEST` (version 1: the callback gets the records).
#[repr(C)]
struct BrowseRequest {
    version: u32,
    interface_index: u32,
    query_name: *const u16,
    callback: Option<BrowseCallback>,
    context: *mut c_void,
}

/// `DNS_SERVICE_RESOLVE_REQUEST`.
#[repr(C)]
struct ResolveRequest {
    version: u32,
    interface_index: u32,
    query_name: *mut u16,
    callback: Option<ResolveComplete>,
    context: *mut c_void,
}

/// The head of `DNS_RECORDW`, as far as a PTR record needs: the record data starts with the PTR
/// target (`DNS_PTR_DATAW::pNameHost`). Only ever read through pointers the system hands out.
#[repr(C)]
struct DnsRecord {
    next: *mut DnsRecord,
    name: *mut u16,
    rtype: u16,
    data_length: u16,
    flags: u32,
    ttl: u32,
    reserved: u32,
    ptr_target: *mut u16,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<Instance>() == 72);
    assert!(offset_of!(Instance, port) == 32);
    assert!(offset_of!(Instance, property_count) == 40);
    assert!(offset_of!(Instance, keys) == 48);
    assert!(offset_of!(Instance, interface_index) == 64);
    assert!(size_of::<RegisterRequest>() == 48);
    assert!(offset_of!(RegisterRequest, unicast_enabled) == 40);
    assert!(size_of::<BrowseRequest>() == 32);
    assert!(size_of::<ResolveRequest>() == 32);
    assert!(offset_of!(DnsRecord, rtype) == 16);
    assert!(offset_of!(DnsRecord, ttl) == 24);
    assert!(offset_of!(DnsRecord, ptr_target) == 32);
};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
}

type ConstructFn =
    unsafe extern "system" fn(*const u16, *const u16, *const u32, *const Ip6Address, u16, u16, u16, u32, *const *const u16, *const *const u16) -> *mut Instance;
type FreeInstanceFn = unsafe extern "system" fn(*mut Instance);
type RegisterFn = unsafe extern "system" fn(*mut RegisterRequest, *mut Cancel) -> u32;
type BrowseFn = unsafe extern "system" fn(*mut BrowseRequest, *mut Cancel) -> u32;
type CancelFn = unsafe extern "system" fn(*mut Cancel) -> u32;
type ResolveFn = unsafe extern "system" fn(*mut ResolveRequest, *mut Cancel) -> u32;
type FreeFn = unsafe extern "system" fn(*mut c_void, u32);

/// The dnsapi functions this module uses.
struct Api {
    construct: ConstructFn,
    free_instance: FreeInstanceFn,
    register: RegisterFn,
    deregister: RegisterFn,
    browse: BrowseFn,
    browse_cancel: CancelFn,
    resolve: ResolveFn,
    free: FreeFn,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// # Safety
/// `p` is null or a NUL-terminated UTF-16 string valid for the call.
unsafe fn from_wide(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut units = Vec::new();
    for i in 0..1024 {
        // SAFETY: NUL-terminated, and the terminator has not been passed.
        let u = unsafe { *p.add(i) };
        if u == 0 {
            break;
        }
        units.push(u);
    }
    String::from_utf16_lossy(&units)
}

fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| {
        let dll = wide("dnsapi.dll");
        // SAFETY: a NUL-terminated name; only the system directory is searched.
        let module = unsafe { LoadLibraryExW(dll.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if module.is_null() {
            return Err("dnsapi.dll is not available".to_owned());
        }
        let find = |name: &std::ffi::CStr| -> Result<*mut c_void, String> {
            // SAFETY: a loaded module and a NUL-terminated name.
            let p = unsafe { GetProcAddress(module, name.as_ptr()) };
            if p.is_null() {
                Err(format!("{} is missing (Windows 10 1809 or later has it)", name.to_string_lossy()))
            } else {
                Ok(p)
            }
        };
        // SAFETY: each pointer is the named dnsapi export, whose signature the types spell out.
        unsafe {
            Ok(Api {
                construct: std::mem::transmute::<*mut c_void, ConstructFn>(find(c"DnsServiceConstructInstance")?),
                free_instance: std::mem::transmute::<*mut c_void, FreeInstanceFn>(find(c"DnsServiceFreeInstance")?),
                register: std::mem::transmute::<*mut c_void, RegisterFn>(find(c"DnsServiceRegister")?),
                deregister: std::mem::transmute::<*mut c_void, RegisterFn>(find(c"DnsServiceDeRegister")?),
                browse: std::mem::transmute::<*mut c_void, BrowseFn>(find(c"DnsServiceBrowse")?),
                browse_cancel: std::mem::transmute::<*mut c_void, CancelFn>(find(c"DnsServiceBrowseCancel")?),
                resolve: std::mem::transmute::<*mut c_void, ResolveFn>(find(c"DnsServiceResolve")?),
                free: std::mem::transmute::<*mut c_void, FreeFn>(find(c"DnsFree")?),
            })
        }
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// Whether this Windows has the DNS-SD functions (1809 or later).
pub fn available() -> Result<(), String> {
    api().map(|_| ())
}

/// What every callback gets: the sink and the service type's suffix (`._kiln._tcp.local`).
struct Shared {
    sink: Sink,
    suffix: String,
}

impl Shared {
    /// The instance label of a full service instance name.
    fn label_of(&self, full: &str) -> Option<String> {
        let full = full.trim_end_matches('.');
        let cut = full.len().checked_sub(self.suffix.len())?;
        let (label, suffix) = (full.get(..cut)?, full.get(cut..)?);
        (suffix.eq_ignore_ascii_case(&self.suffix) && !label.is_empty()).then(|| label.to_owned())
    }
}

unsafe extern "system" fn on_browse(status: u32, ctx: *mut c_void, records: *mut DnsRecord) {
    if ctx.is_null() {
        return;
    }
    // SAFETY: the context is the `Shared` the handle leaked for its callbacks.
    let shared = unsafe { &*ctx.cast::<Shared>() };
    if status != 0 && status != ERROR_CANCELLED {
        (shared.sink)(Event::Failed { what: "browse", code: i32::try_from(status).unwrap_or(i32::MAX) });
    }
    let mut r = records;
    let mut n = 0;
    while !r.is_null() && n < MAX_RECORDS {
        // SAFETY: a record of the list the system passed.
        let rec = unsafe { &*r };
        if rec.rtype == DNS_TYPE_PTR {
            // SAFETY: a PTR record's data starts with its NUL-terminated target.
            let target = unsafe { from_wide(rec.ptr_target) };
            if let Some(label) = shared.label_of(&target) {
                (shared.sink)(if rec.ttl == 0 { Event::Lost { label } } else { Event::Found { label } });
            }
        }
        r = rec.next;
        n += 1;
    }
    if !records.is_null()
        && let Ok(api) = api()
    {
        // SAFETY: the list is ours to free (DnsRecordListFree).
        unsafe { (api.free)(records.cast(), DNS_FREE_RECORD_LIST) };
    }
}

/// # Safety
/// `inst` is a live `DNS_SERVICE_INSTANCE` the system passed.
unsafe fn read_instance(inst: &Instance) -> (String, String, u16, Vec<(String, String)>, Vec<IpAddr>) {
    // SAFETY: the instance's strings, NUL-terminated.
    let (name, host) = unsafe { (from_wide(inst.instance_name), from_wide(inst.host_name)) };
    let mut txt = Vec::new();
    let count = usize::try_from(inst.property_count).unwrap_or(0).min(64);
    if !inst.keys.is_null() && !inst.values.is_null() {
        for i in 0..count {
            // SAFETY: `property_count` keys and values.
            let (k, v) = unsafe { (from_wide(*inst.keys.add(i)), from_wide(*inst.values.add(i))) };
            if !k.is_empty() {
                txt.push((k, v));
            }
        }
    }
    let mut addrs = Vec::new();
    if !inst.ip4.is_null() {
        // SAFETY: an IP4_ADDRESS: four bytes in network order.
        let raw = unsafe { *inst.ip4 };
        let ip = Ipv4Addr::from(raw.to_ne_bytes());
        if !ip.is_unspecified() {
            addrs.push(IpAddr::V4(ip));
        }
    }
    if !inst.ip6.is_null() {
        // SAFETY: an IP6_ADDRESS: sixteen bytes.
        let ip = Ipv6Addr::from(unsafe { (*inst.ip6).bytes });
        if !ip.is_unspecified() {
            addrs.push(IpAddr::V6(ip));
        }
    }
    (name, host, inst.port, txt, addrs)
}

/// A resolution in flight: owned by the system until its completion callback takes it back.
struct Resolving {
    shared: *const Shared,
    label: String,
    name: Vec<u16>,
    request: ResolveRequest,
    cancel: Cancel,
}

unsafe extern "system" fn on_resolve(status: u32, ctx: *mut c_void, inst: *mut Instance) {
    if ctx.is_null() {
        return;
    }
    // SAFETY: the context is the `Resolving` box `Handle::resolve` gave away; the completion runs
    // once, so taking it back here frees it exactly once.
    let job = unsafe { Box::from_raw(ctx.cast::<Resolving>()) };
    // SAFETY: the `Shared` outlives every request (it is leaked).
    let shared = unsafe { &*job.shared };
    if status == 0 && !inst.is_null() {
        // SAFETY: the instance the system passed.
        let (name, host, port, txt, addrs) = unsafe { read_instance(&*inst) };
        let label = shared.label_of(&name).unwrap_or_else(|| job.label.clone());
        (shared.sink)(Event::Resolved { label, host, port, txt, addrs });
    } else if status != ERROR_CANCELLED {
        (shared.sink)(Event::Failed { what: "resolve", code: i32::try_from(status).unwrap_or(i32::MAX) });
    }
    if !inst.is_null()
        && let Ok(api) = api()
    {
        // SAFETY: the instance is ours to free.
        unsafe { (api.free_instance)(inst) };
    }
    drop(job);
}

unsafe extern "system" fn on_register(status: u32, ctx: *mut c_void, inst: *mut Instance) {
    if !ctx.is_null() {
        // SAFETY: the leaked `Shared`.
        let shared = unsafe { &*ctx.cast::<Shared>() };
        if status == 0 {
            // SAFETY: the instance the system passed, if any.
            let name = if inst.is_null() { String::new() } else { unsafe { from_wide((*inst).instance_name) } };
            (shared.sink)(Event::Registered { name: shared.label_of(&name).unwrap_or(name) });
        } else {
            (shared.sink)(Event::Failed { what: "register", code: i32::try_from(status).unwrap_or(i32::MAX) });
        }
    }
    if !inst.is_null()
        && let Ok(api) = api()
    {
        // SAFETY: the instance is ours to free.
        unsafe { (api.free_instance)(inst) };
    }
}

/// A live registration: the request and its instance stay alive while registered.
struct Registration {
    request: Box<RegisterRequest>,
}

// SAFETY: the raw pointers inside are only handed to dnsapi, from whichever thread holds the lock.
unsafe impl Send for Registration {}

/// A running browse.
struct Browse {
    request: Box<BrowseRequest>,
    cancel: Box<Cancel>,
    _name: Vec<u16>,
}

// SAFETY: as for `Registration`.
unsafe impl Send for Browse {}

/// Browsing and registration; dropping it deregisters and cancels the browse.
pub struct Handle {
    api: &'static Api,
    shared: &'static Shared,
    service_type: String,
    browse: Mutex<Option<Browse>>,
    registration: Mutex<Option<Registration>>,
}

// SAFETY: every field is either immutable or behind a mutex; dnsapi may be called from any thread.
unsafe impl Send for Handle {}
// SAFETY: as above.
unsafe impl Sync for Handle {}

impl Handle {
    /// Browse for `service_type` (`_kiln._tcp`) in `local` when `browse`. Fails when this Windows
    /// has no DNS-SD functions or refuses the browse; reports [`Event::Started`] otherwise.
    pub fn start(service_type: &str, _keys: &[&str], browse: bool, sink: Sink) -> Result<Self, String> {
        let api = api()?;
        // Callbacks may still run after a cancel: what they use is never freed.
        let shared: &'static Shared = Box::leak(Box::new(Shared { sink, suffix: format!(".{service_type}.local") }));
        let h = Self { api, shared, service_type: service_type.to_owned(), browse: Mutex::new(None), registration: Mutex::new(None) };
        if browse {
            let name = wide(&format!("{service_type}.local"));
            let mut b = Browse {
                request: Box::new(BrowseRequest {
                    version: DNS_QUERY_REQUEST_VERSION1,
                    interface_index: 0,
                    query_name: name.as_ptr(),
                    callback: Some(on_browse),
                    context: std::ptr::from_ref(shared).cast_mut().cast(),
                }),
                cancel: Box::new(Cancel { reserved: std::ptr::null_mut() }),
                _name: name,
            };
            // SAFETY: the request, its name and the cancel handle live in `b`, kept until the
            // browse is cancelled (and then leaked).
            let rc = unsafe { (api.browse)(&mut *b.request, &mut *b.cancel) };
            if rc != DNS_REQUEST_PENDING {
                return Err(format!("DnsServiceBrowse failed (error {rc})"));
            }
            *h.browse.lock().unwrap_or_else(PoisonError::into_inner) = Some(b);
        }
        (h.shared.sink)(Event::Started);
        Ok(h)
    }

    /// Register (or re-register, replacing the earlier registration) this service.
    pub fn register(&self, service: Service) {
        self.deregister();
        // Windows takes the full instance name; a dot inside the label would split it.
        let full = wide(&format!("{}.{}.local", service.label.replace('.', "-"), self.service_type));
        let host = wide(&service.host);
        let ip4 = service.addrs.iter().find_map(|a| match a {
            IpAddr::V4(v4) => Some(u32::from_ne_bytes(v4.octets())),
            IpAddr::V6(_) => None,
        });
        let ip6 = service.addrs.iter().find_map(|a| match a {
            IpAddr::V6(v6) if (v6.segments().first().copied().unwrap_or(0) & 0xffc0) != 0xfe80 => Some(Ip6Address { bytes: v6.octets() }),
            _ => None,
        });
        let keys: Vec<Vec<u16>> = service.txt.iter().map(|(k, _)| wide(k)).collect();
        let values: Vec<Vec<u16>> = service.txt.iter().map(|(_, v)| wide(v)).collect();
        let key_ptrs: Vec<*const u16> = keys.iter().map(|k| k.as_ptr()).collect();
        let value_ptrs: Vec<*const u16> = values.iter().map(|v| v.as_ptr()).collect();
        let count = u32::try_from(key_ptrs.len()).unwrap_or(0);
        // SAFETY: NUL-terminated strings and arrays of `count` of them, alive for the call; the
        // instance copies them.
        let inst = unsafe {
            (self.api.construct)(
                full.as_ptr(),
                host.as_ptr(),
                ip4.as_ref().map_or(std::ptr::null(), std::ptr::from_ref),
                ip6.as_ref().map_or(std::ptr::null(), std::ptr::from_ref),
                service.port,
                0,
                0,
                count,
                key_ptrs.as_ptr(),
                value_ptrs.as_ptr(),
            )
        };
        if inst.is_null() {
            (self.shared.sink)(Event::Failed { what: "register", code: -1 });
            return;
        }
        let mut request = Box::new(RegisterRequest {
            version: DNS_QUERY_REQUEST_VERSION1,
            interface_index: 0,
            instance: inst,
            callback: Some(on_register),
            context: std::ptr::from_ref(self.shared).cast_mut().cast(),
            credentials: std::ptr::null_mut(),
            unicast_enabled: 0,
        });
        // SAFETY: the request and its instance stay alive while registered (and after).
        let rc = unsafe { (self.api.register)(&mut *request, std::ptr::null_mut()) };
        if rc != DNS_REQUEST_PENDING {
            // SAFETY: never handed over; ours to free.
            unsafe { (self.api.free_instance)(inst) };
            (self.shared.sink)(Event::Failed { what: "register", code: i32::try_from(rc).unwrap_or(i32::MAX) });
            return;
        }
        *self.registration.lock().unwrap_or_else(PoisonError::into_inner) = Some(Registration { request });
    }

    fn deregister(&self) {
        let Some(mut r) = self.registration.lock().unwrap_or_else(PoisonError::into_inner).take() else { return };
        // SAFETY: the request that registered the service; its completion may still refer to it,
        // so it is leaked rather than freed.
        unsafe { (self.api.deregister)(&mut *r.request, std::ptr::null_mut()) };
        std::mem::forget(r);
    }

    /// Look up an instance found by browsing: [`Event::Resolved`] follows.
    pub fn resolve(&self, label: &str) {
        let name = wide(&format!("{label}.{}.local", self.service_type));
        let job = Box::into_raw(Box::new(Resolving {
            shared: self.shared,
            label: label.to_owned(),
            name,
            request: ResolveRequest {
                version: DNS_QUERY_REQUEST_VERSION1,
                interface_index: 0,
                query_name: std::ptr::null_mut(),
                callback: Some(on_resolve),
                context: std::ptr::null_mut(),
            },
            cancel: Cancel { reserved: std::ptr::null_mut() },
        }));
        // SAFETY: `job` is a fresh box: the request points into it, and the completion callback
        // takes it back (a refused request is freed here instead).
        unsafe {
            (*job).request.query_name = (*job).name.as_mut_ptr();
            (*job).request.context = job.cast();
            let rc = (self.api.resolve)(&mut (*job).request, &mut (*job).cancel);
            if rc != DNS_REQUEST_PENDING {
                drop(Box::from_raw(job));
                (self.shared.sink)(Event::Failed { what: "resolve", code: i32::try_from(rc).unwrap_or(i32::MAX) });
            }
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.deregister();
        if let Some(mut b) = self.browse.lock().unwrap_or_else(PoisonError::into_inner).take() {
            // SAFETY: the running browse's cancel handle; the request may still be in use by a
            // late callback, so it is leaked.
            unsafe { (self.api.browse_cancel)(&mut *b.cancel) };
            std::mem::forget(b);
        }
    }
}
