//! The node's lessors over its wire, over a line the test answers (docs/design/credential-grants.md C7): a box's runs
//! lease from the connection that created it, each ask told on it and answered there, each give-back and run's end told;
//! a box no leasing connection created leases from no one; and a line that has ended refuses typed.

use std::sync::atomic::{AtomicBool, Ordering};

use hex_api::lease::Value;

use super::*;

/// A connection's line in the test's hands: what the node told it, and each ask's slot for the test to fill.
#[derive(Default)]
struct TestLine {
    told: Mutex<Vec<(u64, Answer)>>,
    slots: Mutex<BTreeMap<u64, Arc<Slot>>>,
    next: Mutex<u64>,
    ended: AtomicBool,
}

impl Line for TestLine {
    fn ask(&self) -> Result<(u64, Arc<Slot>), Refusal> {
        if self.ended.load(Ordering::SeqCst) {
            return Err(Refusal::Broken("the test's line has ended".to_owned()));
        }
        let mut next = self.next.lock().unwrap();
        *next += 1;
        let slot = Slot::new();
        self.slots.lock().unwrap().insert(*next, Arc::clone(&slot));
        Ok((*next, slot))
    }

    fn tell(&self, id: u64, answer: &Answer) -> Result<(), Refusal> {
        self.told
            .lock()
            .unwrap()
            .push((id, answer.clone_for_test()));
        Ok(())
    }

    fn forget(&self, id: u64) {
        self.slots.lock().unwrap().remove(&id);
    }
}

impl TestLine {
    /// Answers ask `id` with `reply` once it has been told, as the connection's reader does.
    fn answer_when_told(self: &Arc<Self>, id: u64, reply: Reply) -> std::thread::JoinHandle<()> {
        let line = Arc::clone(self);
        std::thread::spawn(move || {
            loop {
                if let Some(slot) = line.slots.lock().unwrap().remove(&id) {
                    slot.fill(reply);
                    return;
                }
                std::thread::yield_now();
            }
        })
    }

    /// The connection ends: every ask waiting hears it, as the socket's close fills them.
    fn end(&self) {
        self.ended.store(true, Ordering::SeqCst);
        for slot in std::mem::take(&mut *self.slots.lock().unwrap()).into_values() {
            slot.fill(Reply::Ended);
        }
    }
}

/// What a test compares of an answer told: the lease kinds' alone, which carry no value.
trait CloneForTest {
    fn clone_for_test(&self) -> Answer;
}

impl CloneForTest for Answer {
    fn clone_for_test(&self) -> Answer {
        match self {
            Answer::LessorAsk(id) => Answer::LessorAsk(id.clone()),
            Answer::LeaseAsk {
                run,
                reference,
                key,
            } => Answer::LeaseAsk {
                run: *run,
                reference: reference.clone(),
                key: *key,
            },
            Answer::LeaseGiven(lease) => Answer::LeaseGiven(lease.clone()),
            Answer::LessorEnded => Answer::LessorEnded,
            other => panic!("the node told a line {other:?}"),
        }
    }
}

fn b(name: &str) -> BoxId {
    BoxId::new(name).unwrap()
}

/// Box `id` routed to `line` on connection 7: its weak hold lives while the test holds `line`, as a connection's while
/// the connection stands.
fn routed(wire: &WireLessors, id: &BoxId, line: &Arc<TestLine>) {
    let dynamic: Arc<dyn Line> = Arc::clone(line) as Arc<dyn Line>;
    wire.room(id).unwrap();
    wire.route(id, 7, Some(Arc::downgrade(&dynamic)));
}

/// docs/design/credential-grants.md C7: a run of a box its leasing connection created asks that connection for its lessor,
/// then for each lease, and its give-back and its end are told there, each under the run's id.
#[test]
fn s33_a_routed_boxs_run_leases_over_its_creators_line_and_gives_back_there() {
    let wire = WireLessors::new();
    let line = Arc::new(TestLine::default());
    let id = b("harness-1");
    routed(&wire, &id, &line);

    let answering = line.answer_when_told(1, Reply::Lessor(Ok(())));
    let lessor = wire.lessor(&id).unwrap();
    answering.join().unwrap();

    let answering = line.answer_when_told(
        2,
        Reply::Leased(Ok(Lease {
            lease: "lease-1".to_owned(),
            value: Value::new(b"granted".to_vec()),
            life: None,
        })),
    );
    let lease = lessor.lease("providers/anthropic", None).unwrap();
    answering.join().unwrap();
    assert_eq!(lease.lease, "lease-1");
    assert_eq!(lease.value.expose(), b"granted");
    lessor.release("lease-1");
    drop(lessor);

    let told = line.told.lock().unwrap();
    assert!(
        matches!(&told[..], [
            (1, Answer::LessorAsk(asked)),
            (2, Answer::LeaseAsk { run: 1, reference, key: None }),
            (1, Answer::LeaseGiven(given)),
            (1, Answer::LessorEnded),
        ] if *asked == id && reference == "providers/anthropic" && given == "lease-1"),
        "{told:?}"
    );
}

/// docs/design/credential-grants.md C7 (default deny): a box no leasing connection created leases from no one, refused
/// `Unconfigured`; so does one whose route a later create of its id, or its delete, took away.
#[test]
fn s33_a_box_no_leasing_connection_created_leases_from_no_one() {
    let wire = WireLessors::new();
    let line = Arc::new(TestLine::default());
    let (id, other) = (b("harness-2"), b("harness-3"));
    assert!(matches!(
        wire.lessor(&id),
        Err(Refusal::Unconfigured { .. })
    ));
    routed(&wire, &id, &line);
    wire.route(&id, 8, None);
    assert!(matches!(
        wire.lessor(&id),
        Err(Refusal::Unconfigured { .. })
    ));
    routed(&wire, &other, &line);
    wire.deleted(&other);
    assert!(matches!(
        wire.lessor(&other),
        Err(Refusal::Unconfigured { .. })
    ));
    assert!(
        line.told.lock().unwrap().is_empty(),
        "nothing was asked of the line"
    );
}

/// docs/design/credential-grants.md C7: a connection that ends refuses its waiting lease ask, typed, naming the end, and
/// its routes go with it, so its boxes' runs lease from no one.
#[test]
fn s33_a_lease_asked_on_a_line_that_ends_is_refused_by_its_end() {
    let wire = WireLessors::new();
    let line = Arc::new(TestLine::default());
    let id = b("harness-4");
    routed(&wire, &id, &line);
    let answering = line.answer_when_told(1, Reply::Lessor(Ok(())));
    let lessor = wire.lessor(&id).unwrap();
    answering.join().unwrap();

    let ending = {
        let line = Arc::clone(&line);
        std::thread::spawn(move || {
            while line.slots.lock().unwrap().is_empty() {
                std::thread::yield_now();
            }
            line.end();
        })
    };
    let refused = lessor.lease("providers/openai", None);
    ending.join().unwrap();
    match refused {
        Err(LeaseRefused::Unavailable(why)) => assert!(why.contains("has ended"), "{why}"),
        other => panic!("a lease asked of an ended line: {other:?}"),
    }
    wire.closed(7);
    assert!(matches!(
        wire.lessor(&id),
        Err(Refusal::Unconfigured { .. })
    ));
}
