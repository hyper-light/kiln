//! macOS: Bonjour through the dns_sd API of libSystem, declared by hand. mDNSResponder does the
//! multicast DNS (its socket, its firewall exemption, its conflict handling); kiln registers its
//! service with it, browses for the others and resolves them, with the same service type, instance
//! names and TXT keys as kiln's own responder, so both kinds see each other.
//!
//! One thread owns every `DNSServiceRef`: it polls their sockets (`DNSServiceRefSockFD`) and calls
//! `DNSServiceProcessResult`, which runs the callbacks on that thread. The callbacks only queue
//! what they saw; the thread then hands [`Event`]s to the caller's sink (a channel into hyper-rt).
//! Commands (register, resolve, stop) reach the thread through a channel it checks between polls.
//!
//! Self-contained (std only), so it type-checks for macOS from any host:
//! `rustc --edition 2024 --crate-type lib --target aarch64-apple-darwin`.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const LABEL: &str = "Bonjour (macOS)";

/// What to register.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Service {
    /// The instance label, e.g. `kiln server on box (1a2b3c4d)`.
    pub label: String,
    pub port: u16,
    pub txt: Vec<(String, String)>,
}

/// What the thread reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// mDNSResponder took the browse and the registration (if any).
    Started,
    /// The registration is live under this name.
    Registered { name: String },
    Found { label: String },
    Lost { label: String },
    Resolved { label: String, host: String, port: u16, txt: Vec<(String, String)>, addrs: Vec<IpAddr> },
    /// A call failed with a dns_sd error code (`what`: start, browse, register, resolve).
    Failed { what: &'static str, code: i32 },
}

pub type Sink = Box<dyn Fn(Event) + Send + 'static>;

/// `kDNSServiceErr_ServiceNotRunning`: mDNSResponder is not there.
pub const ERR_SERVICE_NOT_RUNNING: i32 = -65563;

type DnsServiceRef = *mut c_void;
type RegisterReply = unsafe extern "C" fn(DnsServiceRef, u32, i32, *const c_char, *const c_char, *const c_char, *mut c_void);
type BrowseReply = unsafe extern "C" fn(DnsServiceRef, u32, u32, i32, *const c_char, *const c_char, *const c_char, *mut c_void);
type ResolveReply = unsafe extern "C" fn(DnsServiceRef, u32, u32, i32, *const c_char, *const c_char, u16, u16, *const u8, *mut c_void);
type AddrInfoReply = unsafe extern "C" fn(DnsServiceRef, u32, u32, i32, *const c_char, *const u8, u32, *mut c_void);

const FLAG_MORE_COMING: u32 = 0x1;
const FLAG_ADD: u32 = 0x2;
const PROTOCOL_IPV4: u32 = 0x01;
const PROTOCOL_IPV6: u32 = 0x02;
const AF_INET: u8 = 2;
const AF_INET6: u8 = 30;
const POLLIN: i16 = 0x0001;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
const ADDRESS_WAIT: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_secs(5);
/// Most resolutions in flight.
const MAX_JOBS: usize = 32;

/// `TXTRecordRef`: 16 opaque bytes, pointer-aligned.
#[repr(C)]
struct TxtRecordRef {
    data: [u8; 16],
    _align: [*mut c_char; 0],
}

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

#[link(name = "System", kind = "dylib")]
unsafe extern "C" {
    fn DNSServiceRegister(
        sd: *mut DnsServiceRef,
        flags: u32,
        interface: u32,
        name: *const c_char,
        regtype: *const c_char,
        domain: *const c_char,
        host: *const c_char,
        port_be: u16,
        txt_len: u16,
        txt: *const c_void,
        callback: Option<RegisterReply>,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceBrowse(
        sd: *mut DnsServiceRef,
        flags: u32,
        interface: u32,
        regtype: *const c_char,
        domain: *const c_char,
        callback: Option<BrowseReply>,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceResolve(
        sd: *mut DnsServiceRef,
        flags: u32,
        interface: u32,
        name: *const c_char,
        regtype: *const c_char,
        domain: *const c_char,
        callback: Option<ResolveReply>,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceGetAddrInfo(
        sd: *mut DnsServiceRef,
        flags: u32,
        interface: u32,
        protocol: u32,
        hostname: *const c_char,
        callback: Option<AddrInfoReply>,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceRefSockFD(sd: DnsServiceRef) -> c_int;
    fn DNSServiceProcessResult(sd: DnsServiceRef) -> i32;
    fn DNSServiceRefDeallocate(sd: DnsServiceRef);
    fn TXTRecordCreate(txt: *mut TxtRecordRef, buffer_len: u16, buffer: *mut c_void);
    fn TXTRecordSetValue(txt: *mut TxtRecordRef, key: *const c_char, value_len: u8, value: *const c_void) -> i32;
    fn TXTRecordGetLength(txt: *const TxtRecordRef) -> u16;
    fn TXTRecordGetBytesPtr(txt: *const TxtRecordRef) -> *const c_void;
    fn TXTRecordDeallocate(txt: *mut TxtRecordRef);
    fn TXTRecordGetValuePtr(txt_len: u16, txt: *const c_void, key: *const c_char, value_len: *mut u8) -> *const c_void;
    fn poll(fds: *mut PollFd, nfds: u32, timeout: c_int) -> c_int;
}

// ------------------------------------------------------------------------------ callbacks

/// What a callback saw, queued for the thread.
enum Seen {
    Registered(String),
    RegisterFailed(i32),
    Found(String),
    Lost(String),
    BrowseFailed(i32),
    Resolved { job: u64, host: String, port: u16, txt: Vec<(String, String)> },
    ResolveFailed { job: u64, code: i32 },
    Address { job: u64, ip: Option<IpAddr>, more: bool },
}

/// A callback's context: the thread's queue, the TXT keys to read, and which job it serves.
struct Ctx {
    queue: Rc<RefCell<Vec<Seen>>>,
    keys: Rc<Vec<CString>>,
    job: u64,
}

/// # Safety
/// `p` is null or a NUL-terminated string valid for the call.
unsafe fn text(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: as the caller promises.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// # Safety
/// `ctx` is the `Ctx` this callback's reference was created with, alive (the thread deallocates the
/// reference before dropping its context, and callbacks only run inside `DNSServiceProcessResult`
/// on the thread).
unsafe fn push(ctx: *mut c_void, seen: Seen) {
    if ctx.is_null() {
        return;
    }
    // SAFETY: as the caller promises.
    let c = unsafe { &*ctx.cast::<Ctx>() };
    c.queue.borrow_mut().push(seen);
}

unsafe extern "C" fn on_register(_: DnsServiceRef, _flags: u32, error: i32, name: *const c_char, _: *const c_char, _: *const c_char, ctx: *mut c_void) {
    // SAFETY: dns_sd passes a valid name (or null) and our context.
    unsafe {
        let seen = if error == 0 { Seen::Registered(text(name)) } else { Seen::RegisterFailed(error) };
        push(ctx, seen);
    }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn on_browse(
    _: DnsServiceRef,
    flags: u32,
    _interface: u32,
    error: i32,
    name: *const c_char,
    _: *const c_char,
    _: *const c_char,
    ctx: *mut c_void,
) {
    // SAFETY: as in `on_register`.
    unsafe {
        let seen = if error != 0 {
            Seen::BrowseFailed(error)
        } else if flags & FLAG_ADD != 0 {
            Seen::Found(text(name))
        } else {
            Seen::Lost(text(name))
        };
        push(ctx, seen);
    }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn on_resolve(
    _: DnsServiceRef,
    _flags: u32,
    _interface: u32,
    error: i32,
    _fullname: *const c_char,
    host: *const c_char,
    port_be: u16,
    txt_len: u16,
    txt: *const u8,
    ctx: *mut c_void,
) {
    if ctx.is_null() {
        return;
    }
    // SAFETY: our context (see `push`).
    let c = unsafe { &*ctx.cast::<Ctx>() };
    if error != 0 {
        c.queue.borrow_mut().push(Seen::ResolveFailed { job: c.job, code: error });
        return;
    }
    let mut pairs = Vec::new();
    for key in c.keys.iter() {
        let mut len = 0u8;
        // SAFETY: `txt` holds `txt_len` bytes of TXT data for the duration of the callback.
        let value = unsafe { TXTRecordGetValuePtr(txt_len, txt.cast(), key.as_ptr(), &mut len) };
        if value.is_null() {
            continue;
        }
        // SAFETY: dns_sd returns a pointer to `len` bytes inside the TXT data.
        let bytes = unsafe { std::slice::from_raw_parts(value.cast::<u8>(), usize::from(len)) };
        pairs.push((key.to_string_lossy().into_owned(), String::from_utf8_lossy(bytes).into_owned()));
    }
    // SAFETY: a valid host name (or null) for the callback's duration.
    let host = unsafe { text(host) };
    c.queue.borrow_mut().push(Seen::Resolved { job: c.job, host, port: u16::from_be(port_be), txt: pairs });
}

/// An IP address from a BSD `sockaddr` (length byte, family byte, then the family's layout).
///
/// # Safety
/// `sa` is null or points at a `sockaddr` whose first byte is its length.
unsafe fn sockaddr_ip(sa: *const u8) -> Option<IpAddr> {
    if sa.is_null() {
        return None;
    }
    // SAFETY: a sockaddr starts with its length byte.
    let len = usize::from(unsafe { *sa });
    if len < 8 {
        return None;
    }
    // SAFETY: `len` bytes are there, as the sockaddr says.
    let b = unsafe { std::slice::from_raw_parts(sa, len) };
    match *b.get(1)? {
        AF_INET => {
            let o: [u8; 4] = b.get(4..8)?.try_into().ok()?;
            Some(IpAddr::V4(Ipv4Addr::from(o)))
        }
        AF_INET6 => {
            let o: [u8; 16] = b.get(8..24)?.try_into().ok()?;
            Some(IpAddr::V6(Ipv6Addr::from(o)))
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn on_address(
    _: DnsServiceRef,
    flags: u32,
    _interface: u32,
    error: i32,
    _host: *const c_char,
    address: *const u8,
    _ttl: u32,
    ctx: *mut c_void,
) {
    if ctx.is_null() {
        return;
    }
    // SAFETY: our context (see `push`).
    let c = unsafe { &*ctx.cast::<Ctx>() };
    // SAFETY: dns_sd passes a sockaddr (or null) valid for the callback.
    let ip = if error == 0 && flags & FLAG_ADD != 0 { unsafe { sockaddr_ip(address) } } else { None };
    c.queue.borrow_mut().push(Seen::Address { job: c.job, ip, more: flags & FLAG_MORE_COMING != 0 });
}

// ------------------------------------------------------------------------------ the thread

/// A `DNSServiceRef` and the context its callbacks get; deallocating it stops the callbacks
/// (and withdraws a registration) before the context goes.
struct Op {
    sd: DnsServiceRef,
    ctx: Box<Ctx>,
}

impl Drop for Op {
    fn drop(&mut self) {
        // SAFETY: `sd` came from a successful dns_sd call and is deallocated exactly once.
        unsafe { DNSServiceRefDeallocate(self.sd) };
    }
}

impl Op {
    fn fd(&self) -> c_int {
        // SAFETY: a live reference.
        unsafe { DNSServiceRefSockFD(self.sd) }
    }

    /// Run the callbacks that are ready. An error means the reference is dead.
    fn process(&self) -> Result<(), i32> {
        // SAFETY: a live reference, on the thread that owns it.
        match unsafe { DNSServiceProcessResult(self.sd) } {
            0 => Ok(()),
            e => Err(e),
        }
    }
}

/// A resolution: SRV and TXT first, then the host's addresses.
struct Job {
    label: String,
    op: Op,
    started: Instant,
    found: Option<(String, u16, Vec<(String, String)>)>,
    addrs: Vec<IpAddr>,
}

enum Command {
    Register(Service),
    Resolve(String),
}

struct Worker {
    service_type: CString,
    domain: CString,
    queue: Rc<RefCell<Vec<Seen>>>,
    keys: Rc<Vec<CString>>,
    browse: Option<Op>,
    want_browse: bool,
    register: Option<Op>,
    service: Option<Service>,
    jobs: Vec<Job>,
    next_job: u64,
    retry_at: Option<Instant>,
    sink: Sink,
}

impl Worker {
    fn ctx(&self, job: u64) -> Box<Ctx> {
        Box::new(Ctx { queue: Rc::clone(&self.queue), keys: Rc::clone(&self.keys), job })
    }

    fn start_browse(&mut self) -> Result<(), i32> {
        let mut ctx = self.ctx(0);
        let mut sd: DnsServiceRef = std::ptr::null_mut();
        let ctx_ptr: *mut Ctx = &mut *ctx;
        // SAFETY: valid out pointer, NUL-terminated strings, and a context that lives as long as the
        // reference (both are kept in the same `Op`).
        let rc = unsafe {
            DNSServiceBrowse(&mut sd, 0, 0, self.service_type.as_ptr(), self.domain.as_ptr(), Some(on_browse), ctx_ptr.cast())
        };
        if rc != 0 || sd.is_null() {
            return Err(if rc == 0 { -65537 } else { rc });
        }
        self.browse = Some(Op { sd, ctx });
        Ok(())
    }

    fn start_register(&mut self) -> Result<(), i32> {
        self.register = None;
        let Some(svc) = self.service.clone() else { return Ok(()) };
        let name = CString::new(svc.label.replace('\0', "")).map_err(|_| -65540)?;
        let mut txt = TxtRecordRef { data: [0; 16], _align: [] };
        // SAFETY: an empty TXT record that dns_sd grows itself (no buffer of ours).
        unsafe { TXTRecordCreate(&mut txt, 0, std::ptr::null_mut()) };
        for (k, v) in &svc.txt {
            let (Ok(key), Ok(len)) = (CString::new(k.as_str()), u8::try_from(v.len())) else { continue };
            // SAFETY: an initialised TXT record, a NUL-terminated key and `len` bytes of value.
            unsafe { TXTRecordSetValue(&mut txt, key.as_ptr(), len, v.as_ptr().cast()) };
        }
        let mut ctx = self.ctx(0);
        let ctx_ptr: *mut Ctx = &mut *ctx;
        let mut sd: DnsServiceRef = std::ptr::null_mut();
        // SAFETY: as in `start_browse`; the TXT bytes are copied by the call (the request goes to
        // mDNSResponder at once), so the record is deallocated right after.
        let rc = unsafe {
            let rc = DNSServiceRegister(
                &mut sd,
                0,
                0,
                name.as_ptr(),
                self.service_type.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                svc.port.to_be(),
                TXTRecordGetLength(&txt),
                TXTRecordGetBytesPtr(&txt),
                Some(on_register),
                ctx_ptr.cast(),
            );
            TXTRecordDeallocate(&mut txt);
            rc
        };
        if rc != 0 || sd.is_null() {
            return Err(if rc == 0 { -65537 } else { rc });
        }
        self.register = Some(Op { sd, ctx });
        Ok(())
    }

    fn resolve(&mut self, label: String) {
        if self.jobs.iter().any(|j| j.label == label) || self.jobs.len() >= MAX_JOBS {
            return;
        }
        let Ok(name) = CString::new(label.replace('\0', "")) else { return };
        self.next_job += 1;
        let id = self.next_job;
        let mut ctx = self.ctx(id);
        let ctx_ptr: *mut Ctx = &mut *ctx;
        let mut sd: DnsServiceRef = std::ptr::null_mut();
        // SAFETY: as in `start_browse`.
        let rc = unsafe {
            DNSServiceResolve(&mut sd, 0, 0, name.as_ptr(), self.service_type.as_ptr(), self.domain.as_ptr(), Some(on_resolve), ctx_ptr.cast())
        };
        if rc != 0 || sd.is_null() {
            (self.sink)(Event::Failed { what: "resolve", code: rc });
            return;
        }
        self.jobs.push(Job { label, op: Op { sd, ctx }, started: Instant::now(), found: None, addrs: Vec::new() });
    }

    /// The SRV/TXT answer is in: look up the host's addresses under the same job number.
    fn lookup_addresses(&mut self, at: usize, host: &str) {
        let Some(id) = self.jobs.get(at).map(job_id) else { return };
        let Ok(h) = CString::new(host) else {
            self.finish(at);
            return;
        };
        let mut ctx = self.ctx(id);
        let ctx_ptr: *mut Ctx = &mut *ctx;
        let mut sd: DnsServiceRef = std::ptr::null_mut();
        // SAFETY: as in `start_browse`.
        let rc = unsafe { DNSServiceGetAddrInfo(&mut sd, 0, 0, PROTOCOL_IPV4 | PROTOCOL_IPV6, h.as_ptr(), Some(on_address), ctx_ptr.cast()) };
        if rc != 0 || sd.is_null() {
            // Reported without addresses (the host name still says who it is).
            self.finish(at);
            return;
        }
        if let Some(job) = self.jobs.get_mut(at) {
            // The resolve reference goes (we are outside its callbacks); the lookup takes its place.
            job.op = Op { sd, ctx };
            job.started = Instant::now();
        }
    }

    fn finish(&mut self, at: usize) {
        if at >= self.jobs.len() {
            return;
        }
        let job = self.jobs.remove(at);
        if let Some((host, port, txt)) = job.found {
            (self.sink)(Event::Resolved { label: job.label, host, port, txt, addrs: job.addrs });
        }
    }

    /// Act on what the callbacks queued.
    fn drain(&mut self) {
        let seen: Vec<Seen> = std::mem::take(&mut *self.queue.borrow_mut());
        for s in seen {
            match s {
                Seen::Registered(name) => (self.sink)(Event::Registered { name }),
                Seen::RegisterFailed(code) => {
                    self.register = None;
                    self.retry_at = Some(Instant::now() + RETRY);
                    (self.sink)(Event::Failed { what: "register", code });
                }
                Seen::Found(label) => (self.sink)(Event::Found { label }),
                Seen::Lost(label) => (self.sink)(Event::Lost { label }),
                Seen::BrowseFailed(code) => {
                    self.browse = None;
                    self.retry_at = Some(Instant::now() + RETRY);
                    (self.sink)(Event::Failed { what: "browse", code });
                }
                Seen::Resolved { job, host, port, txt } => {
                    if let Some(at) = self.jobs.iter().position(|j| job_id(j) == job && j.found.is_none()) {
                        if let Some(j) = self.jobs.get_mut(at) {
                            j.found = Some((host.clone(), port, txt));
                        }
                        self.lookup_addresses(at, &host);
                    }
                }
                Seen::ResolveFailed { job, code } => {
                    if let Some(at) = self.jobs.iter().position(|j| job_id(j) == job) {
                        self.jobs.remove(at);
                        (self.sink)(Event::Failed { what: "resolve", code });
                    }
                }
                Seen::Address { job, ip, more } => {
                    let Some(at) = self.jobs.iter().position(|j| job_id(j) == job) else { continue };
                    let done = match self.jobs.get_mut(at) {
                        Some(j) => {
                            if let Some(ip) = ip
                                && !j.addrs.contains(&ip)
                                && j.addrs.len() < 16
                            {
                                j.addrs.push(ip);
                            }
                            !more && j.addrs.iter().any(IpAddr::is_ipv4)
                        }
                        None => false,
                    };
                    if done {
                        self.finish(at);
                    }
                }
            }
        }
    }

    /// Lapsed jobs: a resolution that never answered is dropped; an address lookup reports what
    /// it has.
    fn expire(&mut self, now: Instant) {
        let mut at = 0;
        while at < self.jobs.len() {
            let lapsed = self.jobs.get(at).map(|j| {
                let limit = if j.found.is_some() { ADDRESS_WAIT } else { RESOLVE_TIMEOUT };
                now.saturating_duration_since(j.started) >= limit
            });
            if lapsed == Some(true) {
                self.finish(at);
            } else {
                at += 1;
            }
        }
    }

    fn run(mut self, rx: mpsc::Receiver<Command>) {
        loop {
            loop {
                match rx.try_recv() {
                    Ok(Command::Register(svc)) => {
                        self.service = Some(svc);
                        if let Err(code) = self.start_register() {
                            self.retry_at = Some(Instant::now() + RETRY);
                            (self.sink)(Event::Failed { what: "register", code });
                        }
                    }
                    Ok(Command::Resolve(label)) => self.resolve(label),
                    Err(mpsc::TryRecvError::Empty) => break,
                    // The handle is gone: withdraw everything (dropping the references does).
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
            let now = Instant::now();
            if self.retry_at.is_some_and(|t| t <= now) {
                self.retry_at = None;
                if self.want_browse && self.browse.is_none() && self.start_browse().is_err() {
                    self.retry_at = Some(now + RETRY);
                }
                if self.service.is_some() && self.register.is_none() && self.start_register().is_err() {
                    self.retry_at = Some(now + RETRY);
                }
            }
            self.expire(now);
            // Every live reference's socket, and which reference it is.
            let ops: Vec<(Which, c_int)> = self
                .browse
                .iter()
                .map(|o| (Which::Browse, o.fd()))
                .chain(self.register.iter().map(|o| (Which::Register, o.fd())))
                .chain(self.jobs.iter().map(|j| (Which::Job(job_id(j)), j.op.fd())))
                .filter(|(_, fd)| *fd >= 0)
                .collect();
            let mut fds: Vec<PollFd> = ops.iter().map(|(_, fd)| PollFd { fd: *fd, events: POLLIN, revents: 0 }).collect();
            let n = u32::try_from(fds.len()).unwrap_or(0);
            // SAFETY: `fds` holds `n` initialised entries for the duration of the call.
            let ready = unsafe { poll(fds.as_mut_ptr(), n, 250) };
            if ready <= 0 {
                if fds.is_empty() {
                    std::thread::sleep(Duration::from_millis(250));
                }
                continue;
            }
            for ((which, _), pfd) in ops.iter().zip(&fds) {
                if pfd.revents == 0 {
                    continue;
                }
                let result = match which {
                    Which::Browse => self.browse.as_ref().map(Op::process),
                    Which::Register => self.register.as_ref().map(Op::process),
                    Which::Job(id) => self.jobs.iter().find(|j| job_id(j) == *id).map(|j| j.op.process()),
                };
                if let Some(Err(code)) = result {
                    match which {
                        Which::Browse => {
                            self.browse = None;
                            self.retry_at = Some(Instant::now() + RETRY);
                            (self.sink)(Event::Failed { what: "browse", code });
                        }
                        Which::Register => {
                            self.register = None;
                            self.retry_at = Some(Instant::now() + RETRY);
                            (self.sink)(Event::Failed { what: "register", code });
                        }
                        Which::Job(id) => self.jobs.retain(|j| job_id(j) != *id),
                    }
                }
            }
            self.drain();
        }
    }
}

#[derive(Clone, Copy)]
enum Which {
    Browse,
    Register,
    Job(u64),
}

fn job_id(j: &Job) -> u64 {
    j.op.ctx.job
}

/// The running thread; dropping it withdraws the registration and stops browsing.
pub struct Handle {
    tx: mpsc::Sender<Command>,
}

impl Handle {
    /// Browse for `service_type` (`_kiln._tcp`) in `local.` when `browse`, reading the TXT
    /// `keys` of what is found. Reports [`Event::Started`] once mDNSResponder accepted the browse,
    /// or [`Event::Failed`] (`what: "start"`) when it is not reachable; the thread then ends.
    pub fn start(service_type: &str, keys: &[&str], browse: bool, sink: Sink) -> Result<Self, String> {
        let service_type = CString::new(service_type).map_err(|e| e.to_string())?;
        let keys: Vec<CString> = keys.iter().filter_map(|k| CString::new(*k).ok()).collect();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("kiln-bonjour".into())
            .spawn(move || {
                let mut w = Worker {
                    service_type,
                    domain: CString::from(c"local."),
                    queue: Rc::new(RefCell::new(Vec::new())),
                    keys: Rc::new(keys),
                    browse: None,
                    want_browse: browse,
                    register: None,
                    service: None,
                    jobs: Vec::new(),
                    next_job: 0,
                    retry_at: None,
                    sink,
                };
                if browse && let Err(code) = w.start_browse() {
                    (w.sink)(Event::Failed { what: "start", code });
                    return;
                }
                (w.sink)(Event::Started);
                w.run(rx);
            })
            .map_err(|e| format!("start the Bonjour thread: {e}"))?;
        Ok(Self { tx })
    }

    /// Register (or re-register, replacing the earlier registration) this service.
    pub fn register(&self, service: Service) {
        let _ = self.tx.send(Command::Register(service));
    }

    /// Look up an instance found by browsing: [`Event::Resolved`] follows.
    pub fn resolve(&self, label: &str) {
        let _ = self.tx.send(Command::Resolve(label.to_owned()));
    }
}
