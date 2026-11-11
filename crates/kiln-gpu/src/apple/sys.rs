//! Apple's OS interfaces, isolated per CLAUDE.md §12 and Ada's scoped authorization.
//! IOReport declarations, GPUPH selection and residency formula adapted from all-smi
//! fe479ae675e39475a2eab1cfbb3f5432f705e12b, macos_native/ioreport.rs and channel_filter.rs.
//! Copyright 2025 Lablup Inc. and Jeongkyu Shin; licensed under Apache-2.0.
//! Changes: GPU-only owned subscription, checked integer rates, caller-defined window, no
//! process cache/worker/timer, native Metal identity/budget and exact-entry IOKit memory.
//! See docs/research/measurements/hex-macos-gpu/README.md for sources and remaining native proof.

#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_void};
use std::ptr::{NonNull, null, null_mut};

use crate::{Absent, Gpu, Identity, Load, Skipped, Unread, Vendor};

type Raw = *const c_void;

#[repr(C)]
struct Range {
    location: isize,
    length: isize,
}

#[link(name = "Metal", kind = "framework")]
unsafe extern "C" {
    fn MTLCopyAllDevices() -> Raw;
}

#[link(name = "objc")]
unsafe extern "C" {
    fn sel_registerName(name: *const c_char) -> Raw;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: Raw);
    fn CFGetTypeID(value: Raw) -> usize;
    fn CFEqual(left: Raw, right: Raw) -> u8;
    fn CFStringGetTypeID() -> usize;
    fn CFStringCreateWithBytes(
        allocator: Raw,
        bytes: *const u8,
        count: isize,
        encoding: u32,
        external: u8,
    ) -> Raw;
    fn CFStringGetLength(value: Raw) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetBytes(
        value: Raw,
        range: Range,
        encoding: u32,
        loss: u8,
        external: u8,
        buffer: *mut u8,
        capacity: isize,
        used: *mut isize,
    ) -> isize;
    fn CFStringFind(value: Raw, find: Raw, flags: usize) -> Range;
    fn CFArrayGetTypeID() -> usize;
    fn CFArrayGetCount(value: Raw) -> isize;
    fn CFArrayGetValueAtIndex(value: Raw, index: isize) -> Raw;
    fn CFArrayCreateMutable(allocator: Raw, capacity: isize, callbacks: Raw) -> Raw;
    fn CFArrayAppendValue(array: *mut c_void, value: Raw);
    // Opaque: only its address is passed to CF, never read using this Rust declaration.
    static kCFTypeArrayCallBacks: c_void;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFDictionaryGetValue(dictionary: Raw, key: Raw) -> Raw;
    fn CFDictionaryCreateMutableCopy(allocator: Raw, capacity: isize, dictionary: Raw) -> Raw;
    fn CFDictionarySetValue(dictionary: *mut c_void, key: Raw, value: Raw);
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(number: Raw, kind: isize, value: *mut c_void) -> u8;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegistryEntryIDMatching(id: u64) -> Raw;
    fn IOServiceGetMatchingService(port: u32, matching: Raw) -> u32;
    fn IORegistryEntryGetRegistryEntryID(entry: u32, id: *mut u64) -> i32;
    fn IORegistryEntryInPlane(entry: u32, plane: *const c_char) -> u32;
    fn IORegistryEntryCreateCFProperty(entry: u32, key: Raw, allocator: Raw, options: u32) -> Raw;
    fn IOObjectRelease(object: u32) -> i32;
}

#[link(name = "IOReport")]
unsafe extern "C" {
    fn IOReportCopyChannelsInGroup(group: Raw, subgroup: Raw, a: u64, b: u64, c: u64) -> Raw;
    fn IOReportCreateSubscription(
        allocator: Raw,
        desired: *mut c_void,
        subscribed: *mut *mut c_void,
        channel_id: u64,
        options: Raw,
    ) -> Raw;
    fn IOReportCreateSamples(subscription: Raw, channels: *mut c_void, options: Raw) -> Raw;
    fn IOReportCreateSamplesDelta(previous: Raw, current: Raw, options: Raw) -> Raw;
    fn IOReportChannelGetChannelName(channel: Raw) -> Raw;
    fn IOReportStateGetCount(channel: Raw) -> i32;
    fn IOReportStateGetNameForIndex(channel: Raw, index: i32) -> Raw;
    fn IOReportStateGetResidency(channel: Raw, index: i32) -> i64;
}

/// CoreFoundation's kCFStringEncodingUTF8 (CFStringEncodingExt.h).
const UTF8: u32 = 0x0800_0100;
/// MTLGPUFamilyApple1, supported by Apple Silicon's Apple-family Metal devices (MTLDevice.h).
const APPLE_FAMILY: u64 = 1001;
/// CFNumber's kCFNumberSInt64Type (CFNumber.h).
const SIGNED_64: isize = 4;

#[derive(Debug)]
struct Cf(NonNull<c_void>);

impl Cf {
    /// `raw` is an owned (+1) CF reference from a create/copy call in this module.
    fn own(raw: Raw, missing: Unread) -> Result<Self, Unread> {
        NonNull::new(raw.cast_mut()).map(Self).ok_or(missing)
    }

    fn raw(&self) -> Raw {
        self.0.as_ptr()
    }

    fn mutable(&self) -> *mut c_void {
        self.0.as_ptr()
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: each Cf owns one non-null create/copy reference, released exactly once.
        unsafe { CFRelease(self.raw()) };
    }
}

#[derive(Debug)]
struct Pool(NonNull<c_void>);

impl Pool {
    fn open() -> Result<Self, Unread> {
        // SAFETY: the Objective-C runtime returns this thread's new autorelease-pool token.
        NonNull::new(unsafe { objc_autoreleasePoolPush() })
            .map(Self)
            .ok_or(Unread::Allocation)
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // SAFETY: this token was pushed on this thread; Pool never escapes open's stack frame.
        unsafe { objc_autoreleasePoolPop(self.0.as_ptr()) };
    }
}

fn string(bytes: &[u8]) -> Result<Cf, Unread> {
    let length = isize::try_from(bytes.len()).map_err(|_| Unread::Malformed)?;
    // SAFETY: bytes is readable for length; CF copies it. Null allocator and external=false are
    // the CF default allocator and an ordinary UTF-8 string, not an external representation.
    let raw = unsafe { CFStringCreateWithBytes(null(), bytes.as_ptr(), length, UTF8, 0) };
    Cf::own(raw, Unread::Allocation)
}

fn is_type(value: Raw, kind: unsafe extern "C" fn() -> usize) -> bool {
    // SAFETY: every non-null value passed here is a CF get/create/copy result held by its owner.
    !value.is_null() && unsafe { CFGetTypeID(value) == kind() }
}

fn text(value: Raw) -> Result<String, Unread> {
    if !is_type(value, CFStringGetTypeID) {
        return Err(Unread::Malformed);
    }
    // SAFETY: value is a type-checked live CFString. Both functions only query its length.
    let units = unsafe { CFStringGetLength(value) };
    // SAFETY: units came from a live CFString and UTF8 is the native encoding identifier.
    let capacity = unsafe { CFStringGetMaximumSizeForEncoding(units, UTF8) };
    let length = usize::try_from(capacity).map_err(|_| Unread::Malformed)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| Unread::Allocation)?;
    bytes.resize(length, 0);
    let mut used = 0;
    // SAFETY: the checked CFString has units UTF-16 code units. The buffer has capacity initialized
    // bytes; used is writable. No lossy replacement is allowed, and the complete range must convert.
    let converted = unsafe {
        CFStringGetBytes(
            value,
            Range {
                location: 0,
                length: units,
            },
            UTF8,
            0,
            0,
            bytes.as_mut_ptr(),
            capacity,
            &mut used,
        )
    };
    if converted != units {
        return Err(Unread::Malformed);
    }
    let used = usize::try_from(used).map_err(|_| Unread::Malformed)?;
    if used > bytes.len() {
        return Err(Unread::Malformed);
    }
    bytes.truncate(used);
    String::from_utf8(bytes).map_err(|_| Unread::Malformed)
}

fn selector(name: &CStr) -> Result<Raw, Unread> {
    // SAFETY: name is NUL-terminated and the runtime retains an interned selector for it.
    let sel = unsafe { sel_registerName(name.as_ptr()) };
    if sel.is_null() {
        Err(Unread::Allocation)
    } else {
        Ok(sel)
    }
}

fn metal_number(device: Raw, property: &CStr) -> Result<u64, Unread> {
    let sel = selector(property)?;
    // SAFETY: used only with live MTLDevice objects and documented uint64_t no-argument
    // properties (registryID/recommendedMaxWorkingSetSize). Apple arm64 uses objc_msgSend's
    // ordinary scalar-return ABI for both; the function-pointer signature matches those methods.
    let call: unsafe extern "C" fn(Raw, Raw) -> u64 =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // SAFETY: device is retained by MTLCopyAllDevices' live array; sel has the signature above.
    Ok(unsafe { call(device, sel) })
}

fn metal_name(device: Raw) -> Result<String, Unread> {
    let sel = selector(c"name")?;
    // SAFETY: MTLDevice.name returns a borrowed NSString*, toll-free bridged to CFString.
    let call: unsafe extern "C" fn(Raw, Raw) -> Raw =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // SAFETY: device is held by the live array; the borrowed name stays live during text's copy.
    let name = text(unsafe { call(device, sel) })?;
    if name.is_empty() || name.contains('\0') {
        Err(Unread::Malformed)
    } else {
        Ok(name)
    }
}

fn is_apple(device: Raw) -> Result<bool, Unread> {
    let sel = selector(c"supportsFamily:")?;
    // SAFETY: supportsFamily: has the documented MTLGPUFamily (NSUInteger) argument and BOOL
    // return; on supported macOS arm64 BOOL is C bool. The live receiver is an MTLDevice.
    let call: unsafe extern "C" fn(Raw, Raw, u64) -> bool =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // SAFETY: receiver and selector are as above, and APPLE_FAMILY is the SDK's Apple1 value.
    Ok(unsafe { call(device, sel, APPLE_FAMILY) })
}

#[derive(Debug)]
struct Service(u32);

impl Service {
    fn open(id: u64) -> Result<Self, Unread> {
        // SAFETY: this creates an owned matching dictionary for the full native registry ID.
        let matching = unsafe { IORegistryEntryIDMatching(id) };
        if matching.is_null() {
            return Err(Unread::Allocation);
        }
        // SAFETY: IOKit consumes matching even on failure. Port 0 is kIOMainPortDefault.
        let service = Self(unsafe { IOServiceGetMatchingService(0, matching) });
        if service.0 == 0 {
            return Err(Unread::Gone);
        }
        let mut actual = 0;
        // SAFETY: service is an owned live IOKit reference; actual is a writable uint64_t.
        if unsafe { IORegistryEntryGetRegistryEntryID(service.0, &mut actual) } != 0 || actual != id
        {
            return Err(Unread::Malformed);
        }
        Ok(service)
    }

    fn present(&self) -> bool {
        // SAFETY: self owns the reference; IOService is the SDK's NUL-terminated registry plane.
        unsafe { IORegistryEntryInPlane(self.0, c"IOService".as_ptr()) != 0 }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: this nonzero reference came from GetMatchingService and is released once.
            let _ = unsafe { IOObjectRelease(self.0) };
        }
    }
}

#[derive(Debug)]
struct Card {
    service: Service,
    statistics: Cf,
    allocated: Cf,
}

impl Card {
    fn memory(&self) -> Result<u64, Unread> {
        if !self.service.present() {
            return Err(Unread::Gone);
        }
        // SAFETY: service owns the exact Metal registry entry. Create returns an owned snapshot;
        // key is a live CFString, allocator/options are their documented defaults.
        let statistics = Cf::own(
            unsafe {
                IORegistryEntryCreateCFProperty(self.service.0, self.statistics.raw(), null(), 0)
            },
            Unread::Unsupported,
        )?;
        if !is_type(statistics.raw(), CFDictionaryGetTypeID) {
            return Err(Unread::Malformed);
        }
        // SAFETY: statistics is a checked dictionary; the result is borrowed until it drops.
        let allocated = unsafe { CFDictionaryGetValue(statistics.raw(), self.allocated.raw()) };
        if allocated.is_null() {
            return Err(Unread::Unsupported);
        }
        if !is_type(allocated, CFNumberGetTypeID) {
            return Err(Unread::Malformed);
        }
        let mut bytes: i64 = 0;
        // SAFETY: allocated is a checked CFNumber; SIGNED_64 matches bytes' storage. CF refuses
        // values not exactly representable, and the signed conversion below refuses negatives.
        if unsafe { CFNumberGetValue(allocated, SIGNED_64, (&raw mut bytes).cast()) } == 0 {
            return Err(Unread::Malformed);
        }
        u64::try_from(bytes).map_err(|_| Unread::Malformed)
    }
}

#[derive(Debug)]
struct Report {
    subscription: Cf,
    channels: Cf,
    previous: Cf,
    channels_key: Cf,
    gpuph: Cf,
    idle: Cf,
    off: Cf,
    down: Cf,
}

fn channels(dictionary: Raw, key: &Cf) -> Result<Raw, Unread> {
    if !is_type(dictionary, CFDictionaryGetTypeID) {
        return Err(Unread::Malformed);
    }
    // SAFETY: dictionary and key are live, type-checked CF values. The caller holds the owner.
    let array = unsafe { CFDictionaryGetValue(dictionary, key.raw()) };
    if is_type(array, CFArrayGetTypeID) {
        Ok(array)
    } else {
        Err(Unread::Malformed)
    }
}

fn gpuph(dictionary: Raw, key: &Cf, name: &Cf) -> Result<Raw, Unread> {
    let array = channels(dictionary, key)?;
    // SAFETY: array is a live, checked CFArray owned by dictionary's caller.
    let count = unsafe { CFArrayGetCount(array) };
    if count < 0 {
        return Err(Unread::Malformed);
    }
    let mut selected = None;
    for index in 0..count {
        // SAFETY: index is strictly within the just-read array count, which this code never mutates.
        let channel = unsafe { CFArrayGetValueAtIndex(array, index) };
        if !is_type(channel, CFDictionaryGetTypeID) {
            return Err(Unread::Malformed);
        }
        // SAFETY: IOReport supplied this checked channel dictionary; the name is borrowed from it.
        let channel_name = unsafe { IOReportChannelGetChannelName(channel) };
        if !is_type(channel_name, CFStringGetTypeID) {
            return Err(Unread::Malformed);
        }
        // SAFETY: both arguments are live CFStrings. No textual conversion/allocation is needed.
        if unsafe { CFEqual(channel_name, name.raw()) } != 0 && selected.replace(channel).is_some()
        {
            return Err(Unread::Ambiguous);
        }
    }
    selected.ok_or(Unread::Unsupported)
}

impl Report {
    fn open(devices: usize) -> Result<Self, Unread> {
        super::package_counter(devices)?;
        let group = string(b"GPU Stats")?;
        let subgroup = string(b"GPU Performance States")?;
        let channels_key = string(b"IOReportChannels")?;
        let name = string(b"GPUPH")?;
        // SAFETY: strings live through the call. The three zero query options match the pinned
        // IOReport declaration; this queries only GPU performance states, not CPU/energy sensors.
        let description = Cf::own(
            unsafe { IOReportCopyChannelsInGroup(group.raw(), subgroup.raw(), 0, 0, 0) },
            Unread::IoReport,
        )?;
        let channel = gpuph(description.raw(), &channels_key, &name)?;
        // SAFETY: the opaque global is passed only by address as CF's standard retaining
        // callbacks; one is the exact validated GPUPH channel count, not an inventory ceiling.
        let array = Cf::own(
            unsafe { CFArrayCreateMutable(null(), 1, &raw const kCFTypeArrayCallBacks) },
            Unread::Allocation,
        )?;
        // SAFETY: array is mutable and empty; channel is live and callbacks retain it.
        unsafe { CFArrayAppendValue(array.mutable(), channel) };
        // SAFETY: description is a checked dictionary; zero lets CF choose capacity for its copy.
        let desired = Cf::own(
            unsafe { CFDictionaryCreateMutableCopy(null(), 0, description.raw()) },
            Unread::Allocation,
        )?;
        // SAFETY: desired is mutable; key and array are live and the dictionary retains them.
        unsafe { CFDictionarySetValue(desired.mutable(), channels_key.raw(), array.raw()) };
        let mut subscribed = null_mut();
        // SAFETY: desired is the owned mutable channel description, subscribed is writable,
        // and null/zero arguments are IOReport's allocator/channel/options defaults.
        let raw = unsafe {
            IOReportCreateSubscription(null(), desired.mutable(), &mut subscribed, 0, null())
        };
        // Take both create-rule results before testing either, so partial creation releases both.
        let subscription = Cf::own(raw, Unread::IoReport);
        let subscribed = Cf::own(subscribed, Unread::IoReport);
        let subscription = subscription?;
        let subscribed = subscribed?;
        gpuph(subscribed.raw(), &channels_key, &name)?;
        // SAFETY: the subscription and its actual subscribed channels are live and owned here.
        let previous = Cf::own(
            unsafe { IOReportCreateSamples(subscription.raw(), subscribed.mutable(), null()) },
            Unread::IoReport,
        )?;
        Ok(Self {
            subscription,
            channels: subscribed,
            previous,
            channels_key,
            gpuph: name,
            idle: string(b"IDLE")?,
            off: string(b"OFF")?,
            down: string(b"DOWN")?,
        })
    }

    fn sample(&mut self) -> Result<u8, Unread> {
        // SAFETY: this exclusively borrowed Report owns the live subscription and channel set.
        let current = Cf::own(
            unsafe {
                IOReportCreateSamples(self.subscription.raw(), self.channels.mutable(), null())
            },
            Unread::IoReport,
        )?;
        // SAFETY: both samples stay owned through the delta call, which consumes neither.
        let delta = Cf::own(
            unsafe { IOReportCreateSamplesDelta(self.previous.raw(), current.raw(), null()) },
            Unread::IoReport,
        );
        // The baseline is mathematical history, not a cached reading. Even a zero-tick window
        // retains only this newest raw sample; no timer, minimum interval or stale busy value.
        self.previous = current;
        let delta = delta?;
        let channel = gpuph(delta.raw(), &self.channels_key, &self.gpuph)?;
        // SAFETY: channel is an IOReport-owned checked dictionary held by delta.
        let count = unsafe { IOReportStateGetCount(channel) };
        if count <= 0 {
            return Err(Unread::Malformed);
        }
        let mut residency = super::Residency::default();
        for index in 0..count {
            // SAFETY: index is within IOReport's own count, and delta is retained throughout.
            let state = unsafe { IOReportStateGetNameForIndex(channel, index) };
            if !is_type(state, CFStringGetTypeID) {
                return Err(Unread::Malformed);
            }
            // SAFETY: state is a checked live CFString. Empty state names cannot classify activity.
            if unsafe { CFStringGetLength(state) } <= 0 {
                return Err(Unread::Malformed);
            }
            let idle = [&self.idle, &self.off, &self.down].iter().any(|word| {
                // SAFETY: state and word are live CFStrings; zero means literal case-sensitive
                // search. kCFNotFound is -1, not a residency or sample sentinel.
                unsafe { CFStringFind(state, word.raw(), 0).location != -1 }
            });
            // SAFETY: channel is held by delta and index is below its state count.
            residency.add(idle, unsafe { IOReportStateGetResidency(channel, index) })?;
        }
        residency.percent()
    }
}

#[derive(Debug)]
pub(crate) struct Apple {
    cards: Vec<Card>,
    report: Result<Report, Unread>,
}

// SAFETY: Cf/IOKit references are owned, never exposed, and only moved as part of Apple. All native
// sampling requires &mut Apple; no native call can race through shared access. CF values retained
// between calls are immutable except the exclusively owned subscription's OS-managed sampling.
unsafe impl Send for Apple {}
// SAFETY: &Apple exposes no native operation or handle; sample requires exclusive access, and Drop
// runs only after all shared borrows have ended. There is no thread-affine pool retained in Apple.
unsafe impl Sync for Apple {}

impl Apple {
    pub(crate) fn sample(&mut self, loads: &mut Vec<Load>) {
        let busy = match &mut self.report {
            Ok(report) => report.sample(),
            Err(why) => Err(*why),
        };
        for card in &self.cards {
            let memory = card.memory();
            loads.push(Load {
                busy_percent: if memory == Err(Unread::Gone) {
                    Err(Unread::Gone)
                } else {
                    busy
                },
                memory_used_bytes: memory,
            });
        }
    }
}

pub(crate) struct Found {
    pub(crate) gpus: Vec<Gpu>,
    pub(crate) loads: Vec<Load>,
    pub(crate) skipped: Vec<Skipped>,
    pub(crate) apple: Apple,
}

pub(crate) struct Failure {
    pub(crate) cause: Absent,
    pub(crate) skipped: Vec<Skipped>,
}

fn os_version() -> Result<String, Unread> {
    // SAFETY: utsname is a plain C struct of character arrays; zero initializes every field.
    let mut info: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: info is valid writable storage for uname's entire result.
    if unsafe { libc::uname(&mut info) } != 0 {
        return Err(Unread::Os {
            errno: std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        });
    }
    let end = info
        .release
        .iter()
        .position(|b| *b == 0)
        .ok_or(Unread::Malformed)?;
    let mut version = String::new();
    version
        .try_reserve_exact(end)
        .map_err(|_| Unread::Allocation)?;
    for ch in info.release.iter().take(end) {
        let byte = u8::try_from(*ch).map_err(|_| Unread::Malformed)?;
        if !byte.is_ascii() {
            return Err(Unread::Malformed);
        }
        version.push(char::from(byte));
    }
    if version.is_empty() {
        Err(Unread::Malformed)
    } else {
        Ok(version)
    }
}

fn identify(device: Raw) -> Result<(Gpu, Card), Unread> {
    if device.is_null() || !is_apple(device)? {
        return Err(Unread::Unsupported);
    }
    let id = metal_number(device, c"registryID")?;
    let memory_total_bytes = metal_number(device, c"recommendedMaxWorkingSetSize")?;
    if memory_total_bytes == 0 {
        return Err(Unread::Malformed);
    }
    Ok((
        Gpu {
            vendor: Vendor::Apple,
            model: metal_name(device)?,
            memory_total_bytes,
            identity: Identity::Metal(id),
            // Apple's GPU driver ships with macOS. Record its native Darwin release, not a guessed
            // Metal version from a product/OS table; no independent driver-version API is advertised.
            driver_version: os_version()?,
        },
        Card {
            service: Service::open(id)?,
            statistics: string(b"PerformanceStatistics")?,
            allocated: string(b"Alloc system memory")?,
        },
    ))
}

pub(crate) fn open() -> Result<Found, Failure> {
    let mut skipped = Vec::new();
    let result = inventory(&mut skipped);
    match result {
        Ok((gpus, loads, apple)) => Ok(Found {
            gpus,
            loads,
            skipped,
            apple,
        }),
        Err(cause) => Err(Failure { cause, skipped }),
    }
}

fn inventory(skipped: &mut Vec<Skipped>) -> Result<(Vec<Gpu>, Vec<Load>, Apple), Absent> {
    let _pool = Pool::open().map_err(|_| Absent::Allocation)?;
    // SAFETY: Metal returns an owned NSArray, toll-free bridged to CFArray, or null. The array
    // retains every MTLDevice until after all inventory method calls have finished.
    let array =
        Cf::own(unsafe { MTLCopyAllDevices() }, Unread::Malformed).map_err(|_| Absent::Metal)?;
    if !is_type(array.raw(), CFArrayGetTypeID) {
        return Err(Absent::Malformed);
    }
    // SAFETY: array is live and type-checked; its immutable count bounds every subsequent lookup.
    let count = unsafe { CFArrayGetCount(array.raw()) };
    let count_usize = usize::try_from(count).map_err(|_| Absent::Malformed)?;
    let mut found = Vec::new();
    found
        .try_reserve_exact(count_usize)
        .map_err(|_| Absent::Allocation)?;
    skipped
        .try_reserve_exact(count_usize)
        .map_err(|_| Absent::Allocation)?;
    for index in 0..count {
        // SAFETY: index is within Metal's retained immutable array count.
        let device = unsafe { CFArrayGetValueAtIndex(array.raw(), index) };
        match identify(device) {
            Ok(gpu) => found.push(gpu),
            Err(why) => skipped.push(Skipped::Metal {
                index: usize::try_from(index).map_err(|_| Absent::Malformed)?,
                why,
            }),
        }
    }
    if found.is_empty() {
        return Err(Absent::NoGpu);
    }
    found.sort_unstable_by_key(|(gpu, _)| gpu.identity);
    if found.windows(2).any(|pair| {
        pair.first()
            .zip(pair.get(1))
            .is_some_and(|(a, b)| a.0.identity == b.0.identity)
    }) {
        return Err(Absent::Malformed);
    }
    let mut gpus = Vec::new();
    let mut cards = Vec::new();
    let mut loads = Vec::new();
    gpus.try_reserve_exact(found.len())
        .map_err(|_| Absent::Allocation)?;
    cards
        .try_reserve_exact(found.len())
        .map_err(|_| Absent::Allocation)?;
    loads
        .try_reserve_exact(found.len())
        .map_err(|_| Absent::Allocation)?;
    for (gpu, card) in found {
        gpus.push(gpu);
        cards.push(card);
    }
    Ok((
        gpus,
        loads,
        Apple {
            cards,
            report: Report::open(count_usize),
        },
    ))
}
