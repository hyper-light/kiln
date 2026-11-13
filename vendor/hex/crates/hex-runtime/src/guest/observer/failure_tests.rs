//! Original AC-T7.4.1 failure-fact control. This uses the actual guest Client,
//! named sockets and RemoteProc. It starts no VM/GPU/host workload or reaper.

use super::*;
use crate::guest::{Booted, GuestHandle, managed, pool, selected};
use hex_api::{
    BackingKey, BackingPhase, Class, ClassSpec, DeviceId, DeviceKind, Exit, LifecycleCause,
};
use hex_client::guest::{Client, DuplexOwner, Link, own_duplex, own_link};
use hex_core::{record, stream, wire};
use std::collections::VecDeque;
use std::io::{Read as _, Write as _};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct LocalLink {
    control: Mutex<Option<UnixStream>>,
    streams: Mutex<VecDeque<UnixStream>>,
}

impl Link for LocalLink {
    fn dial(&self, port: u32) -> Result<DuplexOwner, Refusal> {
        let stream = match port {
            stream::CONTROL_PORT => self.control.lock().unwrap().take(),
            stream::STDIO_PORT => self.streams.lock().unwrap().pop_front(),
            _ => None,
        }
        .ok_or(Refusal::Full(0))?;
        own_duplex(stream)
    }

    fn dial_before(&self, port: u32, at: Option<Instant>) -> Result<DuplexOwner, Refusal> {
        if at.is_some_and(|at| Instant::now() >= at) {
            return Err(Refusal::os(
                "original model socket dial",
                std::io::ErrorKind::TimedOut,
            ));
        }
        self.dial(port)
    }
}

/// Booted's native field is not used by observe. A call here fails the separate
/// zero-native-effect oracle rather than inventing a usable native identity.
struct NoNative(Arc<AtomicUsize>);
impl NoNative {
    fn refused<T>(&self) -> Result<T, shards_api::Refusal> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(shards_api::Refusal::Launch(
            shards_api::LaunchError::Observer,
        ))
    }
}
impl shards_api::Guest for NoNative {
    fn process_exited(&self) -> Result<bool, shards_api::Refusal> {
        self.refused()
    }
    fn identity(&self) -> Result<shards_api::Identity, shards_api::Refusal> {
        self.refused()
    }
    fn connect(
        &self,
        _: u32,
        _: Duration,
    ) -> Result<Box<dyn shards_api::Stream>, shards_api::Refusal> {
        self.refused()
    }
    fn connect_before(
        &self,
        _: u32,
        _: Instant,
    ) -> Result<Box<dyn shards_api::Stream>, shards_api::Refusal> {
        self.refused()
    }
    fn stop(&self) -> Result<(), shards_api::Refusal> {
        self.refused()
    }
    fn wait_for(&self, _: Duration) -> Result<Option<shards_api::End>, shards_api::Refusal> {
        self.refused()
    }
}

fn read_exact(
    stream: &mut UnixStream,
    mut bytes: &mut [u8],
    budget: managed::Budget,
) -> Result<(), String> {
    while !bytes.is_empty() {
        stream
            .set_read_timeout(Some(budget.remaining().map_err(|why| why.to_string())?))
            .map_err(|why| why.to_string())?;
        match stream.read(bytes) {
            Ok(0) => return Err("model peer read EOF".into()),
            Ok(n) => bytes = bytes.get_mut(n..).ok_or("model read exceeded its buffer")?,
            Err(why) if why.kind() == std::io::ErrorKind::Interrupted => {}
            Err(why) => return Err(why.to_string()),
        }
    }
    budget.remaining().map_err(|why| why.to_string())?;
    Ok(())
}

fn write_all(
    stream: &mut UnixStream,
    mut bytes: &[u8],
    budget: managed::Budget,
) -> Result<(), String> {
    while !bytes.is_empty() {
        stream
            .set_write_timeout(Some(budget.remaining().map_err(|why| why.to_string())?))
            .map_err(|why| why.to_string())?;
        match stream.write(bytes) {
            Ok(0) => return Err("model peer wrote zero bytes".into()),
            Ok(n) => bytes = bytes.get(n..).ok_or("model write exceeded its buffer")?,
            Err(why) if why.kind() == std::io::ErrorKind::Interrupted => {}
            Err(why) => return Err(why.to_string()),
        }
    }
    budget.remaining().map_err(|why| why.to_string())?;
    Ok(())
}

fn request(
    stream: &mut UnixStream,
    budget: managed::Budget,
) -> Result<(wire::Header, wire::Request), String> {
    let mut prefix = [0u8; size_of::<u32>()];
    read_exact(stream, &mut prefix, budget)?;
    let length = usize::try_from(u32::from_be_bytes(prefix)).map_err(|why| why.to_string())?;
    if length > wire::MAX_MESSAGE {
        return Err("model frame exceeded the actual wire bound".into());
    }
    let mut bytes = vec![0u8; length];
    read_exact(stream, &mut bytes, budget)?;
    wire::read_request_apart(&bytes, wire::MAX_MESSAGE).map_err(|why| format!("{why:?}"))
}

fn answer(
    stream: &mut UnixStream,
    id: u64,
    value: &wire::Answer,
    budget: managed::Budget,
) -> Result<(), String> {
    let bytes = wire::answer(id, value, wire::MAX_MESSAGE).map_err(|why| format!("{why:?}"))?;
    let bytes = stream::frame(&bytes, wire::MAX_MESSAGE).map_err(|why| format!("{why:?}"))?;
    write_all(stream, &bytes, budget)
}

#[derive(Debug)]
struct Seen {
    run: hex_api::BoxId,
    deletes: usize,
    stdout: stream::Named,
    stderr: stream::Named,
    // Keep the endpoint in the worker's return value until the caller captures
    // the typed refusal. An immediate peer EOF must not replace that cause.
    control: Option<UnixStream>,
}

/// The guest's side of one observer run whose Program writes a refusal to stderr and exits 7; its
/// Delete is answered done where `deleted`, else refused as uncertain.
fn failed_workload(
    mut control: UnixStream,
    mut stdout: UnixStream,
    mut stderr: UnixStream,
    budget: managed::Budget,
    deleted: bool,
) -> Result<Seen, String> {
    write_all(&mut control, &7u64.to_le_bytes(), budget)?;
    let (hello, request_hello) = request(&mut control, budget)?;
    if !matches!(request_hello, wire::Request::Hello { .. }) {
        return Err("model expected actual Client Hello".into());
    }
    answer(
        &mut control,
        hello.id,
        &wire::Answer::Hello(wire::Hello {
            build: wire::build(),
            classes: vec![Class::Process],
            limits: wire::Limits {
                connections: 1,
                message_bytes: wire::MAX_MESSAGE,
                in_flight: 1,
                programs: 1,
            },
            budgets: wire::Budgets {
                in_flight: 1,
                programs: 1,
            },
        }),
        budget,
    )?;
    let (run, run_request) = request(&mut control, budget)?;
    let wire::Request::Call(wire::Call::Run { spec, .. }) = run_request else {
        return Err("model expected actual Client Run".into());
    };
    if !matches!(spec.class, ClassSpec::Process)
        || spec.route.is_some()
        || spec.allocation.is_some()
        || !spec.devices.is_empty()
    {
        return Err("observer leaked outer host route/allocation/physical authority".into());
    }
    let id = spec.id.try_clone().map_err(|why| why.to_string())?;
    let mut out_name = [0u8; stream::NAME_BYTES];
    let mut err_name = [0u8; stream::NAME_BYTES];
    read_exact(&mut stdout, &mut out_name, budget)?;
    read_exact(&mut stderr, &mut err_name, budget)?;
    let out_name = stream::read_name(&out_name).map_err(|why| format!("{why:?}"))?;
    let err_name = stream::read_name(&err_name).map_err(|why| format!("{why:?}"))?;
    if out_name.exec.id != run.id
        || err_name.exec.id != run.id
        || out_name.exec.conn != 7
        || err_name.exec.conn != 7
    {
        return Err("model stdio did not name the SAME actual Run".into());
    }
    answer(
        &mut control,
        run.id,
        &wire::Answer::Started { pid: 7 },
        budget,
    )?;
    write_all(&mut stderr, b"vkCreateDevice refused\n", budget)?;
    stdout
        .shutdown(Shutdown::Write)
        .map_err(|why| why.to_string())?;
    stderr
        .shutdown(Shutdown::Write)
        .map_err(|why| why.to_string())?;
    drop((stdout, stderr));
    answer(
        &mut control,
        run.id,
        &wire::Answer::Exited(Exit::Code(7)),
        budget,
    )?;
    // At most the one in-flight and one program acknowledgements may precede
    // the one Delete in this declared 1/1 fixture. No unbounded reader loop.
    for _ in 0..3 {
        let (header, next) = request(&mut control, budget)?;
        match next {
            wire::Request::Taken | wire::Request::StreamsReady => {}
            wire::Request::Call(wire::Call::Delete(gone)) if gone == id => {
                let done = if deleted {
                    wire::Answer::Done
                } else {
                    wire::Answer::Refused(Refusal::Lifecycle {
                        id: id.try_clone().map_err(|why| why.to_string())?,
                        cause: LifecycleCause::Uncertain,
                    })
                };
                answer(&mut control, header.id, &done, budget)?;
                return Ok(Seen {
                    run: id,
                    deletes: 1,
                    stdout: out_name,
                    stderr: err_name,
                    control: Some(control),
                });
            }
            _ => return Err("model expected exact-ID typed failed Delete".into()),
        }
    }
    Err("model exhausted the declared acknowledgement/one-delete bound".into())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    for byte in bytes {
        write!(&mut text, "{byte:02x}").unwrap();
    }
    text
}

/// What one observer run against `failed_workload` left: its result, the time left, its receipt as
/// read back and as written, what the model saw, the run's scope and allocation, and native calls.
struct Observed {
    result: Result<(), Refusal>,
    remaining: Result<Duration, Refusal>,
    retained: Result<Option<pool::Receipt>, Refusal>,
    durable: std::io::Result<String>,
    seen: Seen,
    scope: record::NativeScope,
    allocation: hex_api::Allocation,
    native_calls: usize,
}

/// One observer run through the actual Client, named sockets and RemoteProc against
/// `failed_workload`, its Delete answered as `deleted` says. Every peer is shut before it returns.
fn observed(dir: &std::path::Path, deleted: bool) -> Observed {
    let (config, input, allocation) = config::tests::fixture(dir);
    let configured = config::GpuObserver::new(input, &config, allocation).unwrap();
    let original = record::PoolRecord {
        version: record::RECORD_VERSION,
        key: BackingKey {
            pool: [1; 32],
            generation: [7; 16],
        },
        execution: hex_api::Execution::ShardsDedicated,
        cost: allocation.cost,
        devices: vec![DeviceId {
            kind: DeviceKind::Gpu,
            key: 256,
        }],
        allocation: Some(allocation),
        phase: BackingPhase::Granted,
        init: None,
        exit: None,
    };
    let scope = original.scope();
    let mut receipt = pool::Receipt::intent(
        original,
        vec![shards_api::PhysicalGpu::Pci {
            domain: 0,
            bus: 1,
            device: 0,
            function: 0,
        }],
        &config,
    )
    .unwrap();
    // Model input only. The native field below is deliberately unusable and the
    // zero-effect oracle proves this control never claims a native VM identity.
    receipt.phase = selected::Phase::Running(shards_api::Identity {
        pid: 42,
        started: 7,
        vsock: "/model/v".into(),
    });
    let path = dir.join("receipt");
    let (control, peer) = UnixStream::pair().unwrap();
    let closer = control.try_clone().unwrap();
    let (out, stdout) = UnixStream::pair().unwrap();
    let (err, stderr) = UnixStream::pair().unwrap();
    let link = own_link(LocalLink {
        control: Mutex::new(Some(control)),
        streams: Mutex::new(VecDeque::from([out, err])),
    })
    .unwrap();
    let native_calls = Arc::new(AtomicUsize::new(0));
    // One observer attempt's budget, as the node gives one before any boot (`first_dial_bound`).
    let budget = managed::Budget::new(crate::guest::recorded_dial_bound().unwrap()).unwrap();
    let worker = std::thread::Builder::new()
        .name("observer-known-facts".into())
        .spawn(move || failed_workload(peer, stdout, stderr, budget, deleted))
        .unwrap();
    let connected = Client::connect_before(triomphe::Arc::clone(&link), budget.deadline());
    let setup_ok = connected.is_ok();
    let result = match connected {
        Ok(client) => {
            let booted = Booted {
                guest: GuestHandle::Shared(Arc::new(NoNative(native_calls.clone()))),
                link,
                client,
                evented: Mutex::new(None),
                owner: None,
                backing: None,
            };
            observe(&configured, &booted, &mut receipt, &path, budget)
        }
        Err(why) => Err(why),
    };
    let remaining = budget.remaining();
    let retained = pool::read(&path);
    let durable = std::fs::read_to_string(&path);
    // Cleanup precedes every oracle assertion, including an original setup or
    // protocol failure. Peer I/O also carries the original finite deadline.
    let _ = closer.shutdown(Shutdown::Read);
    let _ = closer.shutdown(Shutdown::Write);
    drop(closer);
    let mut joined = worker.join();
    if let Ok(Ok(seen)) = &mut joined {
        drop(seen.control.take());
    }
    assert!(
        setup_ok,
        "actual Client setup failed before the observer behavior: {result:?}"
    );
    Observed {
        result,
        remaining,
        retained,
        durable,
        seen: joined.unwrap().unwrap(),
        scope,
        allocation,
        native_calls: native_calls.load(Ordering::SeqCst),
    }
}

/// AC-T7.4.1: known guest exit and typed failed cleanup survive observer error
/// in the SAME durable VM receipt. This original control adds no v5 implementation.
#[test]
fn known_guest_exit_and_typed_cleanup_failure_survive_the_same_vm_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let Observed {
        result,
        remaining,
        retained,
        durable,
        seen,
        scope,
        allocation,
        native_calls,
    } = observed(dir.path(), false);
    assert_eq!(seen.deletes, 1);
    assert_eq!(seen.stdout.stream, stream::Stdio::Out);
    assert_eq!(seen.stderr.stream, stream::Stdio::Err);
    assert_eq!(
        seen.run.as_str(),
        format!("hex-gpu-observer-{}", hex(scope.bytes()))
    );
    assert!(
        remaining.is_ok(),
        "expiry must not masquerade as known exit failure"
    );
    assert_eq!(native_calls, 0);
    assert!(matches!(
        result,
        Err(Refusal::Backing {
            cause: LifecycleCause::Uncertain,
            ..
        })
    ));
    let retained = retained.unwrap().unwrap();
    let attempt = retained.attempt.as_ref().unwrap();
    assert_eq!(attempt.phase, AttemptPhase::Waiting);
    assert!(attempt.finished_streams());
    assert_eq!(attempt.stderr.bytes().unwrap(), b"vkCreateDevice refused\n");
    assert!(retained.readiness.is_none());
    assert!(matches!(retained.phase, selected::Phase::Running(_)));
    assert_eq!(retained.original.phase, BackingPhase::Granted);
    assert_eq!(retained.original.allocation, Some(allocation));
    // Canonical header-id0 frames retain exact typed facts and exact codec
    // identity. These are durable-fact contract oracles, not v5 production code.
    let exited = wire::answer(0, &wire::Answer::Exited(Exit::Code(7)), wire::MAX_MESSAGE).unwrap();
    let cleanup = wire::answer(
        0,
        &wire::Answer::Refused(Refusal::Lifecycle {
            id: seen.run,
            cause: LifecycleCause::Uncertain,
        }),
        wire::MAX_MESSAGE,
    )
    .unwrap();
    let exit_fact = format!(
        "observer-exit\t1\t{:016x}\t{}\t{}\n",
        wire::IDENTITY,
        exited.len(),
        hex(&exited)
    );
    let cleanup_fact = format!(
        "observer-cleanup\tfailed\t1\t{:016x}\t{}\t{}\n",
        wire::IDENTITY,
        cleanup.len(),
        hex(&cleanup)
    );
    let durable = durable.unwrap();
    assert!(
        durable.contains(&exit_fact),
        "known Exited7 was lost from the durable VM receipt"
    );
    assert!(
        durable.contains(&cleanup_fact),
        "known typed failed Delete was lost from the same receipt"
    );
}

/// AC-T7.4.1: run an observer whose Program writes its refusal to stderr and exits 7, its cleanup
/// done; expect the observer refused with that stderr, in one wire answer, for the person to read.
#[test]
fn an_unsuccessful_observer_is_refused_with_its_programs_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let observed = observed(dir.path(), true);
    assert_eq!(observed.seen.deletes, 1);
    assert!(observed.remaining.is_ok());
    let Err(refusal) = observed.result else {
        panic!("an exit 7 was taken as ready");
    };
    assert_eq!(
        refusal,
        Refusal::Invalid {
            what: "Shards guest GPU readiness",
            why:
                "actual guest observer exited unsuccessfully; its stderr:\nvkCreateDevice refused\n"
                    .into(),
        }
    );
    assert!(wire::answer(0, &wire::Answer::Refused(refusal), wire::MAX_MESSAGE).is_ok());
    let retained = observed.retained.unwrap().unwrap();
    let attempt = retained.attempt.as_ref().unwrap();
    assert_eq!(attempt.stderr.bytes().unwrap(), b"vkCreateDevice refused\n");
}
