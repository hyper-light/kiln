//! A holder's memory counted once (joint plan step 13, CV12; AC-T7.4.1): what its work uses is missing from the free
//! memory the node's facts measure, and is credited back to the room, up to the holder's reservations, from the
//! sample that read it and while the holder is synced.

use std::sync::Arc;

use hex_api::capacity::{Amounts, Close, Event, Holder, Scope, ScopeRefused, ToHolder};
use hex_api::{Runtime, sim::SimRuntime};
use hex_sim::index::Pick;
use hex_sim::model::{Class, Demand, NodeId, NodeSpec, Resources};
use hex_sim::peer::Budget;

use super::tests::{demand_of, quiesce, started, talk};
use crate::placement::{Driver, Payload, Runtimes};

const GIB: u64 = 1 << 30;

fn machine(mem_bytes: u64) -> Resources {
    Resources {
        cpu_milli: 4000,
        mem_bytes,
        ..Resources::default()
    }
}

fn mem(bytes: u64) -> Amounts {
    Amounts {
        mem_bytes: bytes,
        ..Amounts::ZERO
    }
}

/// A box of one CPU and `bytes` of memory.
fn demand_mem(bytes: u64) -> (Demand, Payload) {
    let (mut demand, mut payload) = demand_of(1000);
    demand.need.mem_bytes = bytes;
    payload.spec.bounds.memory_max = Some(bytes);
    (demand, payload)
}

fn scope() -> Scope {
    Scope::Cgroup("kiln.slice/jobs".into())
}

/// A node of 8 GiB, its view open for one holder, and that holder joined naming its work's cgroup and holding 4 GiB.
fn booted(dir: &std::path::Path) -> (Driver, Holder) {
    let runtime: Arc<dyn Runtime> = Arc::new(SimRuntime::default());
    let driver = Driver::boot(
        NodeId(0),
        &[NodeSpec::new(NodeId(0), machine(8 * GIB))],
        &[],
        Budget { owned: 4, ran: 4 },
        Pick::default(),
        dir,
        Runtimes::new([(Class::Oci, runtime)]).unwrap(),
    )
    .unwrap();
    driver
        .refresh_node(NodeSpec::new(NodeId(0), machine(8 * GIB)))
        .unwrap();
    driver.open_view(&dir.join("capacity"), 1).unwrap();
    let mut kiln = Holder::start("kiln-runner".into(), 1, 4, &[])
        .unwrap()
        .with_scope(scope());
    let hello = kiln.connect().unwrap();
    let joined = talk(&driver, &mut kiln, vec![hello]);
    assert!(
        matches!(joined.as_slice(), [Event::Synced { agree: true, .. }]),
        "{joined:?}"
    );
    let (run, msg) = kiln.reserve(mem(4 * GIB)).unwrap();
    assert_eq!(
        talk(&driver, &mut kiln, vec![msg]),
        vec![Event::Granted(run)]
    );
    assert_eq!(
        driver.view_scopes().unwrap(),
        vec![(1, "kiln-runner".to_owned(), scope())]
    );
    (driver, kiln)
}

/// AC-T7.4.1, CV12: on a machine of 8 GiB a holder naming its work's cgroup holds 4 GiB and its work uses 3, so the
/// node's facts measure 5 free; the node's sample reads the 3 with them, then the node places a box of 4 GiB; then the
/// holder's link drops and the node places a box of 1. Expect the box of 4 started (the holder's 3 GiB counted once),
/// the view's room 8 with none free, the room 5 at once when the link drops, and the box of 1 refused.
#[test]
fn cv12_a_holders_memory_is_counted_once() {
    let dir = tempfile::tempdir().unwrap();
    let (driver, _kiln) = booted(dir.path());
    driver
        .refresh_node_sampled(
            NodeSpec::new(NodeId(0), machine(5 * GIB)),
            &[(1, Ok(mem(3 * GIB)))],
        )
        .unwrap();
    let (d, p) = demand_mem(4 * GIB);
    let (_, step) = driver.submit(d, p).unwrap();
    assert!(
        started(&quiesce(&driver, step)),
        "the node refused 4 GiB the holder's work doesn't use: its 3 GiB counted twice"
    );
    let view = driver.view().unwrap();
    assert_eq!((view.room.mem_bytes, view.free().mem_bytes), (8 * GIB, 0));
    driver.view_link_down(1).unwrap();
    assert_eq!(driver.view().unwrap().room.mem_bytes, 5 * GIB);
    let (d, p) = demand_mem(GIB);
    let (_, step) = driver.submit(d, p).unwrap();
    assert!(!started(&quiesce(&driver, step)));
}

/// AC-T7.4.1, CV12: the node's sample refuses the holder's cgroup as not its user's; expect nothing credited (the
/// node's box of 4 GiB refused) and the view's hints closing the holder's session, typed: Close::Scope(NotOwned).
#[test]
fn cv12_a_scope_not_its_holders_earns_nothing_and_closes_its_session() {
    let dir = tempfile::tempdir().unwrap();
    let (driver, _kiln) = booted(dir.path());
    driver
        .refresh_node_sampled(
            NodeSpec::new(NodeId(0), machine(5 * GIB)),
            &[(1, Err(ScopeRefused::NotOwned))],
        )
        .unwrap();
    let (d, p) = demand_mem(4 * GIB);
    let (_, step) = driver.submit(d, p).unwrap();
    assert!(!started(&quiesce(&driver, step)));
    let hints = driver.view_hints().unwrap();
    assert!(
        matches!(
            hints.as_slice(),
            [(
                1,
                ToHolder::Closed {
                    why: Close::Scope(ScopeRefused::NotOwned),
                    ..
                }
            )]
        ),
        "{hints:?}"
    );
    assert!(driver.view_scopes().unwrap().is_empty());
}
