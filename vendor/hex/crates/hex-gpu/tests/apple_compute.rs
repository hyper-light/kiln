//! AC-T7.3.1: ROOT-only actual Metal compute witness, not telemetry/attachment proof by itself.
//! Required input and external deadline contract: docs/hex-apple-compute/README.md.
//! Compile-only on implementer lanes; NEVER invoke this test there, even with --ignored.

#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
#![allow(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::cell::Cell;
use std::ffi::{CStr, c_char, c_void};
use std::ptr::{NonNull, null_mut};

use hex_gpu::{Gpus, Identity, Vendor};

type Raw = *mut c_void;

#[link(name = "Metal", kind = "framework")]
unsafe extern "C" {
    fn MTLCopyAllDevices() -> Raw;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: Raw);
    fn CFArrayGetCount(array: Raw) -> isize;
    fn CFArrayGetValueAtIndex(array: Raw, index: isize) -> Raw;
    fn CFStringCreateWithBytes(
        allocator: Raw,
        bytes: *const u8,
        length: isize,
        encoding: u32,
        external: u8,
    ) -> Raw;
}

#[link(name = "objc")]
unsafe extern "C" {
    fn sel_registerName(name: *const c_char) -> Raw;
    fn objc_msgSend();
    fn objc_retain(value: Raw) -> Raw;
    fn objc_release(value: Raw);
    fn objc_autoreleasePoolPush() -> Raw;
    fn objc_autoreleasePoolPop(pool: Raw);
}

#[derive(Clone, Copy)]
enum Release {
    Foundation,
    Object,
    Pool,
}

#[derive(Default)]
struct Counts {
    acquired: Cell<usize>,
    released: Cell<usize>,
}

/// One test-owned native reference or autorelease pool. No thread or process-global state.
struct Owned<'a> {
    pointer: NonNull<c_void>,
    release: Release,
    counts: &'a Counts,
}

impl<'a> Owned<'a> {
    fn take(raw: Raw, release: Release, counts: &'a Counts, phase: &str) -> Self {
        let pointer = NonNull::new(raw).unwrap_or_else(|| panic!("Metal {phase} returned null"));
        counts.acquired.set(counts.acquired.get() + 1);
        Self {
            pointer,
            release,
            counts,
        }
    }

    fn retained(raw: Raw, counts: &'a Counts, phase: &str) -> Self {
        assert!(!raw.is_null(), "Metal {phase} returned null");
        // SAFETY: raw is a borrowed live Objective-C object returned by Metal in the current
        // autorelease pool/retained array. Retain makes exactly one reference owned by this guard.
        Self::take(unsafe { objc_retain(raw) }, Release::Object, counts, phase)
    }

    fn raw(&self) -> Raw {
        self.pointer.as_ptr()
    }
}

impl Drop for Owned<'_> {
    fn drop(&mut self) {
        // SAFETY: each guard owns exactly one matching create/copy/retain reference, or a pool
        // pushed on this same thread. Reverse lexical drop keeps the pool alive until objects drop.
        unsafe {
            match self.release {
                Release::Foundation => CFRelease(self.raw()),
                Release::Object => objc_release(self.raw()),
                Release::Pool => objc_autoreleasePoolPop(self.raw()),
            }
        }
        self.counts.released.set(self.counts.released.get() + 1);
    }
}

fn selector(name: &CStr) -> Raw {
    // SAFETY: name is NUL-terminated; the runtime interns/retains the selector.
    let selector = unsafe { sel_registerName(name.as_ptr()) };
    assert!(!selector.is_null(), "selector registration failed");
    selector
}

/// Callers use documented object/contents-pointer-returning, no-argument Metal methods.
fn object(receiver: Raw, name: &CStr) -> Raw {
    assert!(!receiver.is_null());
    // SAFETY: Objective-C dispatch on supported Apple arm64 uses this ordinary pointer-return ABI.
    let call: unsafe extern "C" fn(Raw, Raw) -> Raw =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // SAFETY: receiver is held by a guard/array; name is a no-argument pointer-returning method.
    unsafe { call(receiver, selector(name)) }
}

fn number(receiver: Raw, name: &CStr) -> u64 {
    assert!(!receiver.is_null());
    // SAFETY: callers use registryID (uint64_t) or NSUInteger properties, both u64 on Apple arm64.
    let call: unsafe extern "C" fn(Raw, Raw) -> u64 =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // SAFETY: receiver is live; the named scalar method takes no further arguments.
    unsafe { call(receiver, selector(name)) }
}

fn error_code(error: Raw) -> Option<i64> {
    if error.is_null() {
        return None;
    }
    // SAFETY: Metal supplies a borrowed NSError in this live pool; code is signed NSInteger.
    let call: unsafe extern "C" fn(Raw, Raw) -> i64 =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // SAFETY: the NSError is alive throughout this scalar query, and its signature is exact.
    Some(unsafe { call(error, selector(c"code")) })
}

fn void(receiver: Raw, name: &CStr) {
    assert!(!receiver.is_null());
    // SAFETY: callers use only endEncoding, commit and waitUntilCompleted (void/no arguments).
    let call: unsafe extern "C" fn(Raw, Raw) =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // SAFETY: receiver is held, selector has the no-argument void signature stated above.
    unsafe { call(receiver, selector(name)) };
}

fn cf_string<'a>(bytes: &[u8], counts: &'a Counts) -> Owned<'a> {
    // SAFETY: bytes is readable for length and copied by CF. 0x08000100 is kCFStringEncodingUTF8;
    // null allocator / external=false are CF's documented ordinary-string arguments.
    let raw = unsafe {
        CFStringCreateWithBytes(
            null_mut(),
            bytes.as_ptr(),
            isize::try_from(bytes.len()).unwrap(),
            0x0800_0100,
            0,
        )
    };
    Owned::take(raw, Release::Foundation, counts, "CFString")
}

fn named_function<'a>(library: &Owned<'_>, name: &Owned<'_>, counts: &'a Counts) -> Owned<'a> {
    // SAFETY: MTLLibrary.newFunctionWithName: takes NSString* and returns an owned MTLFunction.
    let call: unsafe extern "C" fn(Raw, Raw, Raw) -> Raw =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // SAFETY: the library and toll-free bridged CFString name are held during the call.
    Owned::take(
        unsafe { call(library.raw(), selector(c"newFunctionWithName:"), name.raw()) },
        Release::Object,
        counts,
        "function",
    )
}

fn library<'a>(device: &Owned<'_>, source: &Owned<'_>, counts: &'a Counts) -> Owned<'a> {
    let mut error = null_mut();
    // SAFETY: newLibraryWithSource:options:error: has NSString*, nullable MTLCompileOptions* and
    // an NSError** out argument, returning an owned MTLLibrary. All arguments match that ABI.
    let call: unsafe extern "C" fn(Raw, Raw, Raw, Raw, *mut Raw) -> Raw =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // SAFETY: device/source are held; null selects Metal's documented default compiler options;
    // error is valid writable storage, borrowed NSError (if any) lives in the current pool.
    let raw = unsafe {
        call(
            device.raw(),
            selector(c"newLibraryWithSource:options:error:"),
            source.raw(),
            null_mut(),
            &mut error,
        )
    };
    let result = (!raw.is_null()).then(|| Owned::take(raw, Release::Object, counts, "library"));
    assert!(
        error.is_null(),
        "Metal shader compilation NSError code {:?}",
        error_code(error)
    );
    result.expect("Metal shader compilation returned no library")
}

fn pipeline<'a>(device: &Owned<'_>, function: &Owned<'_>, counts: &'a Counts) -> Owned<'a> {
    let mut error = null_mut();
    // SAFETY: newComputePipelineStateWithFunction:error: takes MTLFunction* and NSError** and
    // returns an owned MTLComputePipelineState using the ordinary object-return ABI.
    let call: unsafe extern "C" fn(Raw, Raw, Raw, *mut Raw) -> Raw =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // SAFETY: both inputs are held; error is writable and its optional value remains pool-held.
    let raw = unsafe {
        call(
            device.raw(),
            selector(c"newComputePipelineStateWithFunction:error:"),
            function.raw(),
            &mut error,
        )
    };
    let result = (!raw.is_null()).then(|| Owned::take(raw, Release::Object, counts, "pipeline"));
    assert!(
        error.is_null(),
        "Metal pipeline NSError code {:?}",
        error_code(error)
    );
    result.expect("Metal returned no pipeline")
}

fn buffer<'a>(device: &Owned<'_>, bytes: usize, counts: &'a Counts) -> Owned<'a> {
    // SAFETY: newBufferWithLength:options: takes NSUInteger length and MTLResourceOptions and
    // returns an owned MTLBuffer. Both integers are usize on supported Apple arm64.
    let call: unsafe extern "C" fn(Raw, Raw, usize, usize) -> Raw =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // SAFETY: bytes was checked against device.maxBufferLength. Zero is the SDK's
    // MTLResourceStorageModeShared (CPU/GPU visible), not private/uninitialized device-only storage.
    Owned::take(
        unsafe {
            call(
                device.raw(),
                selector(c"newBufferWithLength:options:"),
                bytes,
                0,
            )
        },
        Release::Object,
        counts,
        "shared buffer",
    )
}

fn words(buffer: &Owned<'_>, elements: usize) -> *mut u32 {
    let bytes = elements.checked_mul(size_of::<u32>()).unwrap();
    assert!(number(buffer.raw(), c"length") >= u64::try_from(bytes).unwrap());
    let pointer = object(buffer.raw(), c"contents").cast::<u32>();
    assert!(
        !pointer.is_null() && pointer.is_aligned(),
        "invalid shared-buffer address"
    );
    pointer
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Size {
    width: usize,
    height: usize,
    depth: usize,
}

#[repr(C)]
struct Arguments {
    elements: u32,
    rounds: u32,
}

fn encode(
    encoder: &Owned<'_>,
    pipeline: &Owned<'_>,
    input: &Owned<'_>,
    output: &Owned<'_>,
    args: &Arguments,
) {
    // SAFETY: these exact signatures are setComputePipelineState:, setBuffer:offset:atIndex:,
    // setBytes:length:atIndex: and dispatchThreadgroups:threadsPerThreadgroup: from Apple's SDK.
    let set_pipeline: unsafe extern "C" fn(Raw, Raw, Raw) =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // SAFETY: setBuffer's object and two NSUInteger arguments match the method ABI.
    let set_buffer: unsafe extern "C" fn(Raw, Raw, Raw, usize, usize) =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // SAFETY: setBytes copies a const void* of NSUInteger bytes at an NSUInteger argument index.
    let set_bytes: unsafe extern "C" fn(Raw, Raw, *const c_void, usize, usize) =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    // SAFETY: Size is the SDK's MTLSize (three NSUInteger), passed by value twice, returning void.
    let dispatch: unsafe extern "C" fn(Raw, Raw, Size, Size) =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    let elements = usize::try_from(args.elements).unwrap();
    let width = usize::try_from(number(pipeline.raw(), c"maxTotalThreadsPerThreadgroup"))
        .unwrap()
        .min(elements);
    assert!(width > 0, "Metal supplied no legal pipeline threadgroup");
    let groups = elements
        .checked_add(width - 1)
        .unwrap()
        .checked_div(width)
        .unwrap();
    let launched = groups.checked_mul(width).unwrap();
    assert!(
        u64::try_from(launched).unwrap() <= u64::from(u32::MAX) + 1,
        "padded grid exceeds the shader's uint index address space"
    );
    // SAFETY: all objects are live, same-device, encoder is not ended/committed. Buffer offsets are
    // zero, indices 0/1/2 match explicit MSL bindings. Arguments is repr(C), two u32 (8 bytes), copied
    // immediately by setBytes (below Apple's 4KiB ABI bound). Native pipeline limits bound width;
    // the shader guards padding before any buffer access. Dispatch encodes, it does not run yet.
    unsafe {
        set_pipeline(
            encoder.raw(),
            selector(c"setComputePipelineState:"),
            pipeline.raw(),
        );
        set_buffer(
            encoder.raw(),
            selector(c"setBuffer:offset:atIndex:"),
            input.raw(),
            0,
            0,
        );
        set_buffer(
            encoder.raw(),
            selector(c"setBuffer:offset:atIndex:"),
            output.raw(),
            0,
            1,
        );
        set_bytes(
            encoder.raw(),
            selector(c"setBytes:length:atIndex:"),
            std::ptr::from_ref(args).cast(),
            size_of::<Arguments>(),
            2,
        );
        dispatch(
            encoder.raw(),
            selector(c"dispatchThreadgroups:threadsPerThreadgroup:"),
            Size {
                width: groups,
                height: 1,
                depth: 1,
            },
            Size {
                width,
                height: 1,
                depth: 1,
            },
        );
    }
    eprintln!(
        "metal_dispatch elements={} rounds={} groups={groups} group_width={width} launched_threads={launched}",
        args.elements, args.rounds
    );
}

const SHADER: &[u8] = br#"
#include <metal_stdlib>
using namespace metal;
struct Arguments { uint elements; uint rounds; };
kernel void hex_witness(device const uint* input [[buffer(0)]],
                        device uint* output [[buffer(1)]],
                        constant Arguments& args [[buffer(2)]],
                        uint i [[thread_position_in_grid]]) {
    if (i >= args.elements) return;
    uint value = input[i];
    for (uint r = 0; r < args.rounds; ++r) {
        value = ((value << 1u) | (value >> 31u)) ^ (i + r);
    }
    output[i] = value ^ 0xffffffffu;
}
"#;

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|error| panic!("ROOT must supply {name}: {error}"))
}

fn registry_id() -> u64 {
    let value = required("HEX_METAL_REGISTRY_ID");
    if let Some(hex) = value.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).expect("invalid hexadecimal registryID")
    } else {
        value.parse().expect("invalid decimal registryID")
    }
}

fn work() -> Arguments {
    let positive = |name| {
        let value = required(name)
            .parse::<u32>()
            .unwrap_or_else(|_| panic!("invalid u32 {name}"));
        assert!(value > 0, "{name} must be positive");
        value
    };
    Arguments {
        elements: positive("HEX_METAL_ELEMENTS"),
        rounds: positive("HEX_METAL_ROUNDS"),
    }
}

fn expected(input: u32, index: u32, rounds: u32) -> u32 {
    let mut value = input;
    for round in 0..rounds {
        value = value.rotate_left(1) ^ index.wrapping_add(round);
    }
    value ^ u32::MAX
}

fn run(target: u64, args: &Arguments, counts: &Counts) {
    // SAFETY: creates this thread's pool; first guard created, so it is the last native guard dropped.
    let _pool = Owned::take(
        unsafe { objc_autoreleasePoolPush() },
        Release::Pool,
        counts,
        "pool",
    );
    let inventory = Gpus::open();
    let gpus = inventory
        .inventory()
        .unwrap_or_else(|why| panic!("required Metal inventory failed: {why}"));
    let matches = gpus
        .iter()
        .filter(|gpu| gpu.vendor == Vendor::Apple && gpu.identity == Identity::Metal(target))
        .count();
    assert_eq!(
        matches, 1,
        "required native registryID {target} missing/duplicated in Gpus inventory: {gpus:?}"
    );
    // SAFETY: returns an owned NSArray, toll-free bridged to CFArray, retaining its Metal devices.
    let devices = Owned::take(
        unsafe { MTLCopyAllDevices() },
        Release::Foundation,
        counts,
        "device array",
    );
    // SAFETY: devices holds the native immutable array during all bounded indexed lookups.
    let count = unsafe { CFArrayGetCount(devices.raw()) };
    assert!(count > 0, "Metal enumerated no devices");
    let mut chosen = None;
    for index in 0..count {
        // SAFETY: index is within the just-read immutable array count; array retains this MTLDevice.
        let device = unsafe { CFArrayGetValueAtIndex(devices.raw(), index) };
        assert!(!device.is_null(), "Metal returned a null device row");
        let id = number(device, c"registryID");
        eprintln!("metal_enumerated registry_id={id}");
        if id == target {
            assert!(chosen.is_none(), "duplicate native registryID {target}");
            chosen = Some(Owned::retained(device, counts, "selected device"));
        }
    }
    let device =
        chosen.expect("required registryID absent from independent native Metal enumeration");
    let elements = usize::try_from(args.elements).unwrap();
    let bytes = elements
        .checked_mul(size_of::<u32>())
        .expect("work byte count overflow");
    assert!(
        u64::try_from(bytes).unwrap() <= number(device.raw(), c"maxBufferLength"),
        "ROOT work exceeds native maxBufferLength"
    );
    let iterations = u64::from(args.elements)
        .checked_mul(u64::from(args.rounds))
        .unwrap();
    eprintln!(
        "metal_selected registry_id={target} elements={} rounds={} thread_iterations={iterations} buffer_bytes={bytes}",
        args.elements, args.rounds
    );

    let source = cf_string(SHADER, counts);
    let name = cf_string(b"hex_witness", counts);
    let library = library(&device, &source, counts);
    let function = named_function(&library, &name, counts);
    let pipeline = pipeline(&device, &function, counts);
    let queue = Owned::take(
        object(device.raw(), c"newCommandQueue"),
        Release::Object,
        counts,
        "queue",
    );
    let input = buffer(&device, bytes, counts);
    let output = buffer(&device, bytes, counts);
    let mut oracle = Vec::new();
    oracle
        .try_reserve_exact(elements)
        .expect("ROOT oracle allocation refused");
    let input_pointer = words(&input, elements);
    let output_pointer = words(&output, elements);
    // SAFETY: these are distinct live shared buffers of at least elements u32, aligned/non-null,
    // and no command has been committed. Exclusive CPU slices end before any GPU submission.
    unsafe {
        let input = std::slice::from_raw_parts_mut(input_pointer, elements);
        let output = std::slice::from_raw_parts_mut(output_pointer, elements);
        for (index, (input, output)) in input.iter_mut().zip(output).enumerate() {
            let index = u32::try_from(index).unwrap();
            // Fixed fixture bit-pattern, including high bits; not a tuning or random-generator claim.
            *input = index.rotate_left(16) ^ 0x8000_0001;
            let expected = expected(*input, index, args.rounds);
            oracle.push(expected);
            *output = expected ^ 1; // Every initial output is provably incorrect, never a fake pass.
        }
    }
    for resource in [&queue, &pipeline, &input, &output] {
        assert_eq!(
            number(object(resource.raw(), c"device"), c"registryID"),
            target,
            "Metal resource belongs to a different GPU"
        );
    }
    let command = Owned::retained(
        object(queue.raw(), c"commandBuffer"),
        counts,
        "command buffer",
    );
    let encoder = Owned::retained(
        object(command.raw(), c"computeCommandEncoder"),
        counts,
        "encoder",
    );
    encode(&encoder, &pipeline, &input, &output, args);
    void(encoder.raw(), c"endEncoding");
    void(command.raw(), c"commit");
    // This OS wait has NO deadline or cancellation guarantee. ROOT MUST externally guard the
    // entire invocation (CPU oracle, compilation and GPU wait). A timed-out/killed witness does
    // not prove completion or native cleanup. No polling/worker/deadline framework is added here.
    void(command.raw(), c"waitUntilCompleted");
    let status = number(command.raw(), c"status");
    let error = object(command.raw(), c"error");
    eprintln!(
        "metal_command registry_id={target} status={status} error_code={:?}",
        error_code(error)
    );
    assert_eq!(
        status, 4,
        "Metal command did not reach MTLCommandBufferStatusCompleted"
    );
    assert!(
        error.is_null(),
        "Metal completed with NSError code {:?}",
        error_code(error)
    );
    // SAFETY: wait completed successfully; the GPU has finished writing and the live shared
    // output buffer is CPU-coherent. Its alignment/length were checked before submission.
    let actual = unsafe { std::slice::from_raw_parts(output_pointer, elements) };
    assert_eq!(
        actual,
        oracle.as_slice(),
        "GPU output disagrees with the independent CPU oracle"
    );
    eprintln!(
        "metal_verified registry_id={target} elements={} mismatches=0",
        args.elements
    );
}

/// AC-T7.3.1: do select ROOT's exact inventoried registryID, execute caller-bounded deterministic
/// Metal work, then expect successful completion and every output word changed to its CPU oracle.
#[test]
#[ignore = "ROOT-only actual Metal GPU effect; required env and external process guard; never run on implementer lanes"]
fn root_selected_metal_device_executes_checked_kernel() {
    let target = registry_id();
    let args = work();
    let counts = Counts::default();
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(target, &args, &counts)));
    assert_eq!(
        counts.acquired.get(),
        counts.released.get(),
        "fixture native retain/release calls unbalanced"
    );
    eprintln!(
        "metal_fixture_cleanup registry_id={target} acquired={} released={} live=0 (wrapper calls, not OS leak proof)",
        counts.acquired.get(),
        counts.released.get()
    );
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
