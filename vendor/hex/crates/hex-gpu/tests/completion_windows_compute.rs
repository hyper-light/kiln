//! AC-T7.3.1: ROOT-only exact-adapter native compute, independent of HCS/VM attachment proof.
//! Required inputs, native lifetime failures and mutation seams: docs/hex-windows-gpu-compute/.
//! Implementers may cross-Clippy this leaf ONLY; never invoke its ignored native test.

#![cfg(target_os = "windows")]
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
use std::ffi::c_void;
use std::fmt;
use std::mem::ManuallyDrop;
use std::ptr::null_mut;

use windows::Wdk::Graphics::Direct3D::{
    D3DKMT_CLOSEADAPTER, D3DKMT_OPENADAPTERFROMDEVICENAME, D3DKMTCloseAdapter,
    D3DKMTOpenAdapterFromDeviceName,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_ENABLE_STRICTNESS, D3DCompile};
use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL_11_0, ID3DBlob, ID3DInclude};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter1, IDXGIFactory4,
};
use windows::Win32::System::SystemInformation::{GetNativeSystemInfo, SYSTEM_INFO};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::{Interface, PCWSTR, s};

// ROOT missing-write mutation seam: remove ONLY this output store in a private source copy.
// The output buffer's u32::MAX prefill must then fail the independent oracle after fence completion.
const SHADER: &[u8] = br#"
StructuredBuffer<uint> input_words : register(t0);
RWStructuredBuffer<uint> output_words : register(u0);
[numthreads(1,1,1)]
void main(uint3 thread_id : SV_DispatchThreadID) {
    output_words[thread_id.x] = input_words[thread_id.x] + thread_id.x + 1u;
}
"#;

#[derive(Debug)]
enum Failure {
    Input(&'static str, String),
    Native(&'static str, windows::core::Error),
    Nt(&'static str, i32),
    Check(&'static str, String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(field, why) => write!(f, "UNVERIFIED input {field}: {why}"),
            Self::Native(phase, error) => write!(f, "{phase}: {error:?}"),
            Self::Nt(phase, status) => write!(f, "{phase}: NTSTATUS={status:#x}"),
            Self::Check(phase, why) => write!(f, "{phase}: {why}"),
        }
    }
}

type Result<T> = std::result::Result<T, Failure>;

fn native<T>(phase: &'static str, result: windows::core::Result<T>) -> Result<T> {
    result.map_err(|error| {
        let failure = Failure::Native(phase, error);
        // Preserve the original error even if pending native work forces process-exit cleanup.
        eprintln!("windows_compute_error {failure}");
        failure
    })
}

fn require(ok: bool, phase: &'static str, why: impl Into<String>) -> Result<()> {
    if ok {
        Ok(())
    } else {
        let failure = Failure::Check(phase, why.into());
        eprintln!("windows_compute_error {failure}");
        Err(failure)
    }
}

fn env(name: &'static str) -> Result<String> {
    std::env::var(name).map_err(|error| Failure::Input(name, error.to_string()))
}

struct Inputs {
    interface: String,
    expected: u64,
    wait_ms: u32,
}

impl Inputs {
    fn read() -> Result<Self> {
        let interface = env("HEX_WINDOWS_GPU_COMPUTE_INTERFACE")?;
        if !interface.starts_with(r"\\?\")
            || interface.contains('\0')
            || !interface
                .to_ascii_lowercase()
                .ends_with("#{1ca05180-a699-450a-9a0c-de4fbe3ddd89}")
        {
            return Err(Failure::Input(
                "HEX_WINDOWS_GPU_COMPUTE_INTERFACE",
                "require the actual OS-enumerated adapter-arrival symbolic link, not a manufactured/view/setup-class path".into(),
            ));
        }
        let expected = env("HEX_WINDOWS_GPU_COMPUTE_LUID")?;
        if expected.len() != 16 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Failure::Input(
                "HEX_WINDOWS_GPU_COMPUTE_LUID",
                "require exactly sixteen hexadecimal digits of independently observed native LUID bits".into(),
            ));
        }
        let expected = u64::from_str_radix(&expected, 16)
            .map_err(|e| Failure::Input("HEX_WINDOWS_GPU_COMPUTE_LUID", e.to_string()))?;
        let wait_ms = env("HEX_WINDOWS_GPU_COMPUTE_WAIT_MS")?
            .parse::<u32>()
            .map_err(|e| Failure::Input("HEX_WINDOWS_GPU_COMPUTE_WAIT_MS", e.to_string()))?;
        if wait_ms == 0 || wait_ms == INFINITE {
            return Err(Failure::Input(
                "HEX_WINDOWS_GPU_COMPUTE_WAIT_MS",
                "require a positive finite caller budget below Win32 INFINITE".into(),
            ));
        }
        Ok(Self {
            interface,
            expected,
            wait_ms,
        })
    }
}

fn luid_bits(luid: LUID) -> u64 {
    let high = u32::from_ne_bytes(luid.HighPart.to_ne_bytes());
    (u64::from(high) << 32) | u64::from(luid.LowPart)
}

fn same_luid(phase: &'static str, expected: u64, actual: LUID) -> Result<()> {
    require(
        luid_bits(actual) == expected,
        phase,
        format!("expected={expected:016x} actual={:016x}", luid_bits(actual)),
    )
}

#[derive(Default)]
struct Cleanup {
    kmt_open: Cell<u32>,
    kmt_closed: Cell<u32>,
    event_open: Cell<u32>,
    event_closed: Cell<u32>,
    failed: Cell<bool>,
}

struct Kmt<'a> {
    handle: u32,
    cleanup: &'a Cleanup,
}

impl Drop for Kmt<'_> {
    fn drop(&mut self) {
        let close = D3DKMT_CLOSEADAPTER {
            hAdapter: self.handle,
        };
        // SAFETY: this is the nonzero handle returned by our successful KMT open, closed once.
        let status = unsafe { D3DKMTCloseAdapter(&close) };
        if status.0 == 0 {
            self.cleanup
                .kmt_closed
                .set(self.cleanup.kmt_closed.get() + 1);
        } else {
            self.cleanup.failed.set(true);
        }
        eprintln!("windows_compute_kmt_close status={:#x}", status.0);
    }
}

struct Event<'a> {
    handle: HANDLE,
    cleanup: &'a Cleanup,
}

impl Drop for Event<'_> {
    fn drop(&mut self) {
        // SAFETY: our anonymous non-inheritable event is owned here, never duplicated or closed elsewhere.
        match unsafe { CloseHandle(self.handle) } {
            Ok(()) => self
                .cleanup
                .event_closed
                .set(self.cleanup.event_closed.get() + 1),
            Err(error) => {
                self.cleanup.failed.set(true);
                eprintln!("windows_compute_event_close_error {error:?}");
            }
        }
    }
}

fn device(args: &Inputs, cleanup: &Cleanup) -> Result<ID3D12Device> {
    let path: Vec<u16> = args.interface.encode_utf16().chain([0]).collect();
    let luid = {
        let mut open = D3DKMT_OPENADAPTERFROMDEVICENAME {
            pDeviceName: PCWSTR(path.as_ptr()),
            ..Default::default()
        };
        // SAFETY: SDK layout, writable out-structure and NUL-terminated path remain live through the call.
        let status = unsafe { D3DKMTOpenAdapterFromDeviceName(&mut open) };
        eprintln!(
            "windows_compute_kmt_open interface={:?} status={:#x}",
            args.interface, status.0
        );
        if status.0 != 0 {
            return Err(Failure::Nt("KMT open", status.0));
        }
        require(
            open.hAdapter != 0,
            "KMT open",
            "successful open returned a zero handle",
        )?;
        cleanup.kmt_open.set(cleanup.kmt_open.get() + 1);
        let _adapter = Kmt {
            handle: open.hAdapter,
            cleanup,
        };
        eprintln!(
            "windows_compute_kmt_luid actual={:016x}",
            luid_bits(open.AdapterLuid)
        );
        // ROOT wrong-ID control: supply a different expected LUID; refusal precedes DXGI/GPU work.
        same_luid("KMT identity", args.expected, open.AdapterLuid)?;
        open.AdapterLuid
    };
    require(
        !cleanup.failed.get(),
        "KMT cleanup",
        "close failed; no GPU work will be submitted",
    )?;
    // SAFETY: SDK creates a new owned factory. No ordinal/default/WARP enumeration is used.
    let factory: IDXGIFactory4 = native("DXGI factory", unsafe { CreateDXGIFactory1() })?;
    // SAFETY: the LUID came from our exact interface; returned COM reference is owned by adapter.
    let adapter: IDXGIAdapter1 = native("DXGI exact LUID", unsafe {
        factory.EnumAdapterByLuid(luid)
    })?;
    // SAFETY: adapter is a live owned SDK COM interface; GetDesc1 writes its own descriptor.
    let description = native("DXGI descriptor", unsafe { adapter.GetDesc1() })?;
    same_luid("DXGI identity", args.expected, description.AdapterLuid)?;
    require(
        description.Flags & u32::try_from(DXGI_ADAPTER_FLAG_SOFTWARE.0).unwrap() == 0,
        "DXGI hardware",
        "software adapter is not physical GPU proof",
    )?;
    let mut selected = None;
    // SAFETY: explicit live adapter, SDK ABI, and an owned output slot. This actually creates a device.
    native("D3D12 device", unsafe {
        D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut selected)
    })?;
    let selected: ID3D12Device =
        selected.ok_or_else(|| Failure::Check("D3D12 device", "null result".into()))?;
    // SAFETY: live D3D12 device; both calls return plain native values and retain no pointers.
    let (actual, nodes) = unsafe { (selected.GetAdapterLuid(), selected.GetNodeCount()) };
    same_luid("D3D12 identity", args.expected, actual)?;
    require(
        nodes == 1,
        "D3D12 physical node",
        format!("nodes={nodes}; exact linked-node mapping is UNVERIFIED"),
    )?;
    eprintln!(
        "windows_compute_selected luid={:016x} nodes={nodes} dxgi_flags={}",
        luid_bits(actual),
        description.Flags
    );
    Ok(selected)
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: the owned blob retains its immutable buffer for this borrow; SDK supplies its extent.
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()) }
}

fn shader() -> Result<ID3DBlob> {
    let mut code = None;
    let mut diagnostics = None;
    // SAFETY: immutable embedded source and literal strings remain live; no include callback/files.
    let compiled = unsafe {
        D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
            s!("hex_windows_compute"),
            None,
            None::<&ID3DInclude>,
            s!("main"),
            s!("cs_5_0"),
            D3DCOMPILE_ENABLE_STRICTNESS,
            0,
            &mut code,
            Some(&mut diagnostics),
        )
    };
    if let Some(diagnostics) = diagnostics {
        eprintln!(
            "windows_compute_shader_diagnostics {}",
            String::from_utf8_lossy(blob_bytes(&diagnostics))
        );
    }
    native("shader compilation", compiled)?;
    code.ok_or_else(|| Failure::Check("shader compilation", "null bytecode".into()))
}

fn pipeline(device: &ID3D12Device) -> Result<(ID3D12RootSignature, ID3D12PipelineState)> {
    let parameters = [D3D12_ROOT_PARAMETER_TYPE_SRV, D3D12_ROOT_PARAMETER_TYPE_UAV].map(|kind| {
        D3D12_ROOT_PARAMETER {
            ParameterType: kind,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        }
    });
    let description = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: u32::try_from(parameters.len()).unwrap(),
        pParameters: parameters.as_ptr(),
        ..Default::default()
    };
    let mut serialized = None;
    let mut diagnostics = None;
    // SAFETY: both SDK root descriptors and output slots remain live until serialization returns.
    let result = unsafe {
        D3D12SerializeRootSignature(
            &description,
            D3D_ROOT_SIGNATURE_VERSION_1,
            &mut serialized,
            Some(&mut diagnostics),
        )
    };
    if let Some(diagnostics) = diagnostics {
        eprintln!(
            "windows_compute_root_diagnostics {}",
            String::from_utf8_lossy(blob_bytes(&diagnostics))
        );
    }
    native("root serialization", result)?;
    let serialized =
        serialized.ok_or_else(|| Failure::Check("root serialization", "null result".into()))?;
    // SAFETY: immutable serialized bytes remain held through the creation call; one physical node was checked.
    let root: ID3D12RootSignature = native("root signature", unsafe {
        device.CreateRootSignature(0, blob_bytes(&serialized))
    })?;
    let shader = shader()?;
    let mut description = D3D12_COMPUTE_PIPELINE_STATE_DESC {
        pRootSignature: ManuallyDrop::new(Some(root.clone())),
        CS: D3D12_SHADER_BYTECODE {
            pShaderBytecode: blob_bytes(&shader).as_ptr().cast(),
            BytecodeLength: blob_bytes(&shader).len(),
        },
        ..Default::default()
    };
    // SAFETY: root/bytecode remain live; SDK returns an independently owned pipeline reference.
    let result = unsafe { device.CreateComputePipelineState(&description) };
    // SAFETY: this is exactly the one cloned reference placed into the SDK's non-dropping descriptor.
    unsafe { ManuallyDrop::drop(&mut description.pRootSignature) };
    Ok((root, native("compute pipeline", result)?))
}

fn buffer(
    device: &ID3D12Device,
    bytes: usize,
    heap: D3D12_HEAP_TYPE,
    state: D3D12_RESOURCE_STATES,
    flags: D3D12_RESOURCE_FLAGS,
) -> Result<ID3D12Resource> {
    let properties = D3D12_HEAP_PROPERTIES {
        Type: heap,
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
        ..Default::default()
    };
    let description = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: u64::try_from(bytes).unwrap(),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    };
    let mut resource = None;
    // SAFETY: SDK descriptors name a byte buffer on the single checked node; output is owned.
    native("buffer creation", unsafe {
        device.CreateCommittedResource(
            &properties,
            D3D12_HEAP_FLAG_NONE,
            &description,
            state,
            None,
            &mut resource,
        )
    })?;
    resource.ok_or_else(|| Failure::Check("buffer creation", "null resource".into()))
}

struct Mapping<'a> {
    resource: &'a ID3D12Resource,
    pointer: *mut u32,
    words: usize,
    written: D3D12_RANGE,
}

impl<'a> Mapping<'a> {
    fn open(resource: &'a ID3D12Resource, bytes: usize, writing: bool) -> Result<Self> {
        let range = D3D12_RANGE {
            Begin: 0,
            End: if writing { 0 } else { bytes },
        };
        let mut pointer: *mut c_void = null_mut();
        // SAFETY: resource owns this known byte buffer; range/output are writable and in bounds.
        native("buffer map", unsafe {
            resource.Map(0, Some(&range), Some(&mut pointer))
        })?;
        let mapped = Self {
            resource,
            pointer: pointer.cast(),
            words: bytes / size_of::<u32>(),
            written: D3D12_RANGE {
                Begin: 0,
                End: if writing { bytes } else { 0 },
            },
        };
        require(
            !pointer.is_null() && mapped.pointer.is_aligned(),
            "buffer map",
            "null or unaligned mapped buffer",
        )?;
        Ok(mapped)
    }

    fn initialize(&mut self, elements: usize) -> Vec<u32> {
        // SAFETY: upload Map succeeded for two full pages, checked aligned/non-null; GPU is not submitted.
        let words = unsafe { std::slice::from_raw_parts_mut(self.pointer, self.words) };
        let mut oracle = Vec::with_capacity(elements);
        for index in 0..elements {
            let value = u32::try_from(index).unwrap();
            words[index] = value;
            words[elements + index] = u32::MAX;
            oracle.push(value.checked_mul(2).and_then(|v| v.checked_add(1)).unwrap());
        }
        oracle
    }

    fn verify(&self, expected: &[u32]) -> Result<()> {
        // SAFETY: readback Map succeeded after fence completion and device health checks; GPU writes ended.
        let words = unsafe { std::slice::from_raw_parts(self.pointer, self.words) };
        for (index, (actual, expected)) in words.iter().zip(expected).enumerate() {
            require(
                actual == expected,
                "GPU oracle",
                format!("index={index} expected={expected} actual={actual}"),
            )?;
        }
        require(
            words.len() == expected.len(),
            "GPU oracle",
            "output extent mismatch",
        )
    }
}

impl Drop for Mapping<'_> {
    fn drop(&mut self) {
        // SAFETY: this guard owns the only matching Map; range identifies actual CPU writes, none for readback.
        unsafe { self.resource.Unmap(0, Some(&self.written)) };
    }
}

fn transition(
    list: &ID3D12GraphicsCommandList,
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    let mut barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    };
    // SAFETY: transition union matches Type; resource remains live, with one known before/after state.
    unsafe { list.ResourceBarrier(std::slice::from_ref(&barrier)) };
    // SAFETY: only Transition was initialized; release exactly its temporary cloned reference once.
    unsafe { ManuallyDrop::drop(&mut (*barrier.Anonymous.Transition).pResource) };
}

struct Work<'a> {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    _allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    _root: ID3D12RootSignature,
    _pipeline: ID3D12PipelineState,
    _input: ID3D12Resource,
    _output: ID3D12Resource,
    _upload: ID3D12Resource,
    readback: ID3D12Resource,
    fence: ID3D12Fence,
    event: Event<'a>,
    submitted: bool,
    complete: bool,
}

impl Drop for Work<'_> {
    fn drop(&mut self) {
        if self.submitted && !self.complete {
            // SAFETY: fence/device are still owned; a single cleanup observation is not a polling loop.
            let (completed, healthy) = unsafe {
                (
                    self.fence.GetCompletedValue(),
                    self.device.GetDeviceRemovedReason(),
                )
            };
            if completed == 1 && healthy.is_ok() {
                eprintln!("windows_compute_cleanup completed_after_error=true");
            } else {
                eprintln!(
                    "windows_compute_cleanup_uncertain submitted=true fence={completed} device={healthy:?}; aborting ONLY own fixture process before releasing in-flight resources; ROOT must verify process/native cleanup"
                );
                // D3D12 command lists retain no resource references. Returning/unwinding here could
                // free GPU-visible memory still in flight. ROOT owns this disposable test process;
                // process exit preserves the original logged error without a wait/retry/leak claim.
                std::process::abort();
            }
        }
    }
}

fn witness(args: &Inputs, cleanup: &Cleanup) -> Result<()> {
    let device = device(args, cleanup)?;
    let mut system = SYSTEM_INFO::default();
    // SAFETY: SDK writes one complete SYSTEM_INFO; only its native page-size field is read.
    unsafe { GetNativeSystemInfo(&mut system) };
    let bytes = usize::try_from(system.dwPageSize).unwrap();
    require(
        bytes != 0 && bytes % size_of::<u32>() == 0,
        "OS page",
        "invalid u32 page extent",
    )?;
    let elements = bytes / size_of::<u32>();
    let groups =
        u32::try_from(elements).map_err(|e| Failure::Check("dispatch extent", e.to_string()))?;
    require(
        groups <= D3D12_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION,
        "dispatch extent",
        "native page exceeds D3D12 dispatch limit",
    )?;
    let upload_bytes = bytes
        .checked_mul(2)
        .ok_or_else(|| Failure::Check("upload extent", "overflow".into()))?;
    let (root, pipeline) = pipeline(&device)?;
    let input = buffer(
        &device,
        bytes,
        D3D12_HEAP_TYPE_DEFAULT,
        D3D12_RESOURCE_STATE_COPY_DEST,
        D3D12_RESOURCE_FLAG_NONE,
    )?;
    let output = buffer(
        &device,
        bytes,
        D3D12_HEAP_TYPE_DEFAULT,
        D3D12_RESOURCE_STATE_COPY_DEST,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
    )?;
    let upload = buffer(
        &device,
        upload_bytes,
        D3D12_HEAP_TYPE_UPLOAD,
        D3D12_RESOURCE_STATE_GENERIC_READ,
        D3D12_RESOURCE_FLAG_NONE,
    )?;
    let readback = buffer(
        &device,
        bytes,
        D3D12_HEAP_TYPE_READBACK,
        D3D12_RESOURCE_STATE_COPY_DEST,
        D3D12_RESOURCE_FLAG_NONE,
    )?;
    let oracle = Mapping::open(&upload, upload_bytes, true)?.initialize(elements);
    eprintln!(
        "windows_compute_init page_bytes={bytes} elements={elements} uploaded_words={} output_sentinel={} oracle_words={}",
        elements * 2,
        u32::MAX,
        oracle.len()
    );
    let description = D3D12_COMMAND_QUEUE_DESC {
        Type: D3D12_COMMAND_LIST_TYPE_COMPUTE,
        ..Default::default()
    };
    // SAFETY: SDK creates owned command objects with matching compute type, allocator and pipeline.
    let queue: ID3D12CommandQueue =
        native("queue", unsafe { device.CreateCommandQueue(&description) })?;
    // SAFETY: single owned device; compute allocator remains live in Work until GPU completion.
    let allocator: ID3D12CommandAllocator = native("allocator", unsafe {
        device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COMPUTE)
    })?;
    // SAFETY: allocator/pipeline are live and matching compute objects; this creates an open list.
    let list: ID3D12GraphicsCommandList = native("command list", unsafe {
        device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COMPUTE, &allocator, &pipeline)
    })?;
    // SAFETY: all byte regions are within their actual committed extents, with correct COPY states.
    unsafe {
        list.CopyBufferRegion(&input, 0, &upload, 0, u64::try_from(bytes).unwrap());
        list.CopyBufferRegion(
            &output,
            0,
            &upload,
            u64::try_from(bytes).unwrap(),
            u64::try_from(bytes).unwrap(),
        );
    }
    transition(
        &list,
        &input,
        D3D12_RESOURCE_STATE_COPY_DEST,
        D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
    );
    transition(
        &list,
        &output,
        D3D12_RESOURCE_STATE_COPY_DEST,
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
    );
    // SAFETY: root slots match embedded shader SRV/UAV; buffers remain owned through submission completion.
    unsafe {
        list.SetComputeRootSignature(&root);
        list.SetComputeRootShaderResourceView(0, input.GetGPUVirtualAddress());
        list.SetComputeRootUnorderedAccessView(1, output.GetGPUVirtualAddress());
        list.Dispatch(groups, 1, 1);
    }
    // The UAV->COPY_SOURCE transition orders/completes the shader writes before readback copy.
    transition(
        &list,
        &output,
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
    );
    // SAFETY: output/readback are live full-page buffers in COPY_SOURCE/COPY_DEST respectively.
    unsafe { list.CopyBufferRegion(&readback, 0, &output, 0, u64::try_from(bytes).unwrap()) };
    // SAFETY: exactly this open list has finished recording and is not submitted yet.
    native("command close", unsafe { list.Close() })?;
    // SAFETY: zero initial fence value and no sharing; one queue signal will advance it to one.
    let fence: ID3D12Fence = native("fence creation", unsafe {
        device.CreateFence(0, D3D12_FENCE_FLAG_NONE)
    })?;
    // SAFETY: anonymous initially nonsignaled auto-reset event, no inheritance/security/name.
    let handle = native("event creation", unsafe {
        CreateEventW(None, false, false, PCWSTR::null())
    })?;
    cleanup.event_open.set(cleanup.event_open.get() + 1);
    let mut work = Work {
        device,
        queue,
        _allocator: allocator,
        list,
        _root: root,
        _pipeline: pipeline,
        _input: input,
        _output: output,
        _upload: upload,
        readback,
        fence,
        event: Event { handle, cleanup },
        submitted: false,
        complete: false,
    };
    let command: ID3D12CommandList = native("command interface", work.list.cast())?;
    work.submitted = true;
    // SAFETY: one closed matching list; Work owns all referenced objects through proven completion.
    unsafe { work.queue.ExecuteCommandLists(&[Some(command)]) };
    // SAFETY: live queue/fence; signal follows the list in the same queue and cannot precede its work.
    native("queue fence signal", unsafe {
        work.queue.Signal(&work.fence, 1)
    })?;
    // SAFETY: non-null live event is kept until completion; no blocking null-event variant is used.
    native("fence event", unsafe {
        work.fence.SetEventOnCompletion(1, work.event.handle)
    })?;
    // SAFETY: owned event and caller's validated finite budget. Exactly one wait, no polling/retry.
    let waited = unsafe { WaitForSingleObject(work.event.handle, args.wait_ms) };
    // SAFETY: both objects remain owned; read actual completion/removal before any readback mapping.
    let (completed, healthy) = unsafe {
        (
            work.fence.GetCompletedValue(),
            work.device.GetDeviceRemovedReason(),
        )
    };
    eprintln!(
        "windows_compute_fence wait={:#x} value={completed} device={healthy:?} wait_ms={}",
        waited.0, args.wait_ms
    );
    require(
        waited == WAIT_OBJECT_0 && completed == 1,
        "GPU completion",
        "finite event wait did not prove exact fence completion",
    )?;
    native("GPU device health", healthy)?;
    work.complete = true;
    eprintln!("windows_compute_map readback=true bytes={bytes} fence_complete=true");
    Mapping::open(&work.readback, bytes, false)?.verify(&oracle)?;
    eprintln!(
        "windows_compute_verified luid={:016x} elements={elements} mismatches=0",
        args.expected
    );
    drop(work);
    eprintln!(
        "windows_compute_cleanup com_scope_dropped=true (RAII calls, not OS/driver leak proof)"
    );
    Ok(())
}

/// AC-T7.3.1: do select ROOT's exact current native adapter, dispatch one page of real shader work,
/// then expect explicit device identity, successful finite fence completion and every CPU-oracle word.
#[test]
#[ignore = "ROOT-only actual Windows GPU compute; required actual interface/LUID/wait and external process guard; NEVER run on implementer lanes"]
fn root_windows_exact_adapter_executes_checked_kernel() {
    let args = Inputs::read().unwrap_or_else(|why| panic!("{why}"));
    let cleanup = Cleanup::default();
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| witness(&args, &cleanup)));
    let balanced = cleanup.kmt_open.get() == cleanup.kmt_closed.get()
        && cleanup.event_open.get() == cleanup.event_closed.get()
        && !cleanup.failed.get();
    eprintln!(
        "windows_compute_handles kmt={}/{} events={}/{} close_failed={} (owned wrapper calls only)",
        cleanup.kmt_closed.get(),
        cleanup.kmt_open.get(),
        cleanup.event_closed.get(),
        cleanup.event_open.get(),
        cleanup.failed.get()
    );
    match result {
        Err(original) => std::panic::resume_unwind(original),
        Ok(Err(original)) => panic!("{original}; owned_handle_cleanup_balanced={balanced}"),
        Ok(Ok(())) => assert!(balanced, "native handle cleanup failed"),
    }
}
