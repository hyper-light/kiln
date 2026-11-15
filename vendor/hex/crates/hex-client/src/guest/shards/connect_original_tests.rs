//! Originals for typed cause loss at the real ShardsLink/wire seam: CONNECT's, and the guest's own failures (the
//! review's B-15).

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hex_api::Refusal;
use hex_core::{stream, wire};
use shards_api::{HandshakeError, HandshakePhase, Refusal as SdkRefusal};

use super::{Link, ShardsLink, refusal};

#[derive(Debug, PartialEq, Eq)]
struct Attempt {
    port: u32,
    deadline: Instant,
}

#[derive(Debug, Default)]
struct Trace {
    absolute: Vec<Attempt>,
    other: Vec<&'static str>,
}

struct ModelGuest {
    refusal: SdkRefusal,
    trace: Mutex<Trace>,
}

impl ModelGuest {
    fn new(refusal: SdkRefusal) -> Arc<Self> {
        Arc::new(Self {
            refusal,
            trace: Mutex::new(Trace::default()),
        })
    }

    fn unexpected(&self, method: &'static str) -> SdkRefusal {
        self.trace.lock().unwrap().other.push(method);
        SdkRefusal::Unsupported {
            what: "CONNECT original model",
            why: format!("unexpected Guest::{method}"),
        }
    }
}

impl shards_api::Guest for ModelGuest {
    fn process_exited(&self) -> Result<bool, SdkRefusal> {
        Err(self.unexpected("process_exited"))
    }

    fn connect(&self, _: u32, _: Duration) -> Result<Box<dyn shards_api::Stream>, SdkRefusal> {
        Err(self.unexpected("connect"))
    }

    fn connect_before(
        &self,
        port: u32,
        deadline: Instant,
    ) -> Result<Box<dyn shards_api::Stream>, SdkRefusal> {
        self.trace
            .lock()
            .unwrap()
            .absolute
            .push(Attempt { port, deadline });
        Err(self.refusal.clone())
    }

    fn stop(&self) -> Result<(), SdkRefusal> {
        Err(self.unexpected("stop"))
    }

    fn wait_for(&self, _: Duration) -> Result<Option<shards_api::End>, SdkRefusal> {
        Err(self.unexpected("wait_for"))
    }

    fn identity(&self) -> Result<shards_api::Identity, SdkRefusal> {
        Err(self.unexpected("identity"))
    }
}

struct Roundtrip {
    decoded: Refusal,
    bytes: Vec<u8>,
}

fn roundtrip(label: &str, model: &Arc<ModelGuest>, caller: Instant) -> Roundtrip {
    // The relative dial path must remain unused; this model performs no I/O.
    let link = ShardsLink::new(model.clone(), None);
    let result = link.dial_before(stream::CONTROL_PORT, Some(caller));
    let trace = model.trace.lock().unwrap();
    eprintln!(
        "shards_connect_original label={label} caller={caller:?} input={:?} trace={trace:?}",
        model.refusal
    );
    assert_eq!(
        trace.absolute,
        [Attempt {
            port: stream::CONTROL_PORT,
            deadline: caller,
        }],
        "precondition: actual SDK connect_before must receive the exact port/deadline"
    );
    assert!(trace.other.is_empty(), "unexpected SDK effects: {trace:?}");
    drop(trace);
    let Err(adapted) = result else {
        panic!("model refusal unexpectedly became a stream");
    };
    let bytes = wire::snapshot_refusal(&adapted, wire::MAX_MESSAGE).unwrap();
    let received = wire::Received {
        bytes: &bytes,
        size: bytes.len(),
        descriptors: 0,
        descriptors_cut: false,
    };
    let (header, answer) = wire::read_answer(&received, wire::MAX_MESSAGE).unwrap();
    assert_eq!(header.id, 0);
    let wire::Answer::Refused(decoded) = answer else {
        panic!("snapshot did not decode as an actual refused answer");
    };
    assert_eq!(
        decoded, adapted,
        "wire roundtrip changed the adapter result"
    );
    assert_eq!(
        wire::snapshot_refusal(&decoded, wire::MAX_MESSAGE).unwrap(),
        bytes
    );
    eprintln!(
        "shards_connect_original label={label} adapted={adapted:?} decoded={decoded:?} bytes={bytes:?}"
    );
    Roundtrip { decoded, bytes }
}

/// AC-T7.4.1 / R04: an owning absolute CONNECT failure must remain distinguishable
/// from a legacy text-only Channel refusal with the identical presentation.
#[test]
fn shards_link_connect_cause_remains_distinct_from_legacy_channel_text_on_wire() {
    // These are bounded model inputs, not an observed host errno or native fault.
    let failure = HandshakeError::Io {
        phase: HandshakePhase::Read,
        kind: io::ErrorKind::ConnectionRefused,
        code: Some(111),
    };
    let connect = SdkRefusal::Connect {
        port: stream::CONTROL_PORT,
        failure: failure.clone(),
    };
    let legacy = SdkRefusal::Channel {
        port: stream::CONTROL_PORT,
        why: failure.to_string(),
    };
    let changed_phase = SdkRefusal::Connect {
        port: stream::CONTROL_PORT,
        failure: HandshakeError::Io {
            phase: HandshakePhase::Write,
            kind: io::ErrorKind::ConnectionRefused,
            code: Some(111),
        },
    };
    assert_ne!(connect, legacy);
    assert_eq!(connect.to_string(), legacy.to_string());
    assert_ne!(connect, changed_phase);
    let connect = ModelGuest::new(connect);
    let legacy = ModelGuest::new(legacy);
    let changed_phase = ModelGuest::new(changed_phase);
    // Match the adjacent existing ShardsLink model's one-second caller input.
    // No actor, retry, stream or wait consumes it; every dial receives this same Instant.
    let caller = Instant::now().checked_add(Duration::from_secs(1)).unwrap();
    let connect = roundtrip("typed_connect", &connect, caller);
    let legacy = roundtrip("legacy_same_text", &legacy, caller);
    let changed_phase = roundtrip("typed_changed_phase", &changed_phase, caller);

    // Positive nonvacuity: the real adapter/codec observes a changed SDK phase.
    assert_ne!(connect.decoded, changed_phase.decoded);
    assert_ne!(connect.bytes, changed_phase.bytes);
    eprintln!(
        "shards_connect_original phase_witnessed=true wire_roundtrips=3 sdk_absolute_calls=3 other_calls=0"
    );
    assert_ne!(
        connect.bytes, legacy.bytes,
        "typed cause loss: SDK Connect(Read, ConnectionRefused, Some(111)) and legacy Channel text became the same canonical Hex refusal; decoded Connect={:?}, Channel={:?}",
        connect.decoded, legacy.decoded
    );
}

/// `adapted` through the node's wire as a refused answer, as a client reads it.
fn on_the_wire(adapted: &Refusal) -> Refusal {
    let bytes = wire::snapshot_refusal(adapted, wire::MAX_MESSAGE).unwrap();
    let received = wire::Received {
        bytes: &bytes,
        size: bytes.len(),
        descriptors: 0,
        descriptors_cut: false,
    };
    let (_, answer) = wire::read_answer(&received, wire::MAX_MESSAGE).unwrap();
    let wire::Answer::Refused(decoded) = answer else {
        panic!("snapshot did not decode as a refused answer");
    };
    decoded
}

/// The review's B-15 (AC-T7.4.1; Shards R04): each of Shards' own kinds of a guest's failure is its own typed refusal,
/// never an OS error's text: its VM stalled (Part B's judge), it ended, it went, or its VM process answered a rate or a
/// use none of the protocol's. Do: map each, a stall found by Shards' own judge, through the adapter and the node's
/// wire. Expect: none an `Os` refusal, and each the same refusal after the wire.
#[test]
fn b15_each_of_shards_guest_failures_is_its_own_typed_refusal() {
    let judge = shards_api::Record::new();
    judge.teach(Duration::from_nanos(1), Duration::ZERO);
    let since = Instant::now();
    let stall = judge
        .stalled(since, since.checked_add(Duration::from_millis(1)).unwrap())
        .expect("a learned judge finds a whole W* with no progress a stall");
    let kinds = [
        SdkRefusal::Stalled(stall),
        SdkRefusal::Ended(shards_api::End::Failed("its VMM exited".to_owned())),
        SdkRefusal::Ended(shards_api::End::Stopped),
        SdkRefusal::Gone {
            pid: 4242,
            why: "no process has its pid".to_owned(),
        },
        SdkRefusal::Rate("its governor's thread wasn't made".to_owned()),
        SdkRefusal::HostUse(shards_abi::rate::UseBroken::Word),
    ];
    let mut told = Vec::new();
    for kind in &kinds {
        let adapted = refusal("booting the node's guest", kind);
        assert!(
            !matches!(adapted, Refusal::Os { .. }),
            "Shards' {kind:?} became an OS error: {adapted:?}"
        );
        assert_eq!(on_the_wire(&adapted), adapted, "{kind:?}");
        let Refusal::Guest { op, failure } = adapted else {
            panic!("Shards' {kind:?} is no guest's failure: {adapted:?}");
        };
        assert_eq!(op, "booting the node's guest");
        told.push(failure);
    }
    assert_eq!(
        told[0],
        hex_api::GuestFailure::Stalled {
            waited: stall.waited(),
            over: stall.over(),
            slowest: stall.slowest(),
        },
        "the stall as Shards' judge found it"
    );
    let distinct: std::collections::BTreeSet<String> = told
        .iter()
        .map(|failure| format!("{:?}", std::mem::discriminant(failure)))
        .collect();
    assert_eq!(
        distinct.len(),
        5,
        "Shards' five kinds, each its own: {told:?}"
    );
}
