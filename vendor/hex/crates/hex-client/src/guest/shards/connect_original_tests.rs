//! Original for typed CONNECT cause loss at the real ShardsLink/wire seam.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hex_api::Refusal;
use hex_core::{stream, wire};
use shards_api::{HandshakeError, HandshakePhase, Refusal as SdkRefusal};

use super::{Link, ShardsLink};

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
    let link = ShardsLink::new(model.clone(), Duration::ZERO);
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
