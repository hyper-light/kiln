//! AC-T2.1.5: a slow subscriber gets a typed lag notice and never delays another subscriber, shown
//! with one stalled subscriber among 100. Ninety-nine subscriptions each take every message on a
//! thread of their own while one is never taken from; every publish completes at once, the 99
//! take every message in order with no lag notice, and the stalled one, when it goes on at last,
//! takes one lag notice naming exactly what it missed, then what's kept.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use event_listener::{Event, Listener};
use futures::FutureExt as _;
use quiver::{Node, NodeConfig};
use quiver_api::{Class, Next, Start, Topic, Topics};

const SUBSCRIBERS: usize = 100;
const ROUNDS: u64 = 20;
const BURST: u64 = 100;
const RETAIN: u64 = 1000;
const TOTAL: u64 = ROUNDS * BURST;

/// What one subscriber that keeps up saw.
#[derive(Debug, Default)]
struct Seen {
    offsets_in_order: bool,
    messages: u64,
    lags: u64,
}

#[test]
fn a_stalled_subscriber_among_100_gets_a_lag_notice_and_delays_no_other() {
    let node = Node::new(NodeConfig {
        memory: 256 << 20,
        topics: 10,
        subscriptions: 1000,
    })
    .unwrap();
    let topic = Topic {
        class: Class::Ephemeral,
        retain_messages: RETAIN,
        retain_bytes: 64 << 20,
    };
    node.create("feed", topic).now_or_never().unwrap().unwrap();

    let mut stalled = node
        .subscribe("feed", Start::Oldest)
        .now_or_never()
        .unwrap()
        .unwrap();
    let keeping_up: Vec<_> = (1..SUBSCRIBERS)
        .map(|_| {
            node.subscribe("feed", Start::Oldest)
                .now_or_never()
                .unwrap()
                .unwrap()
        })
        .collect();
    assert_eq!(node.stats().subscriptions, SUBSCRIBERS);

    // Each keeping-up subscriber says how far it has taken; the publisher waits on that fact.
    let taken: Arc<Vec<AtomicU64>> =
        Arc::new((1..SUBSCRIBERS).map(|_| AtomicU64::new(0)).collect());
    let progress = Arc::new(Event::new());

    let seen = std::thread::scope(|scope| {
        let handles: Vec<_> = keeping_up
            .into_iter()
            .enumerate()
            .map(|(i, mut sub)| {
                let taken = Arc::clone(&taken);
                let progress = Arc::clone(&progress);
                scope.spawn(move || {
                    let mut seen = Seen {
                        offsets_in_order: true,
                        ..Seen::default()
                    };
                    let mut want = 0_u64;
                    while want < TOTAL {
                        futures::executor::block_on(sub.ready());
                        while let Some(next) = sub.take() {
                            match next {
                                Next::Message(m) => {
                                    seen.offsets_in_order &=
                                        m.offset == want && m.payload == want.to_le_bytes();
                                    seen.messages += 1;
                                    want = m.offset + 1;
                                }
                                Next::Lag { .. } => seen.lags += 1,
                                Next::Ended => return seen,
                            }
                        }
                        taken[i].store(want, Ordering::Release);
                        progress.notify(usize::MAX);
                    }
                    seen
                })
            })
            .collect();

        let mut published = 0_u64;
        for _ in 0..ROUNDS {
            for _ in 0..BURST {
                // Never delayed: each publish is done the moment it's asked, whatever the stalled
                // subscriber holds.
                let offset = node
                    .publish("feed", None, &published.to_le_bytes())
                    .now_or_never();
                assert_eq!(
                    offset,
                    Some(Ok(published)),
                    "publish {published} didn't complete at once"
                );
                published += 1;
            }
            // The next burst once the 99 have taken this one (retention covers ten bursts, so
            // none of them can fall behind it); the stalled one is never waited for.
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                let listener = progress.listen();
                if taken.iter().all(|t| t.load(Ordering::Acquire) >= published) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "the 99 didn't take burst {published} (a failure, not a wait: the deadline only stops a hang)"
                );
                let _ = listener.wait_timeout(Duration::from_secs(1));
            }
            assert_eq!(
                stalled.position(),
                0,
                "the stalled subscriber took something"
            );
        }
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<Seen>>()
    });

    assert_eq!(seen.len(), SUBSCRIBERS - 1);
    for (i, s) in seen.iter().enumerate() {
        assert!(
            s.offsets_in_order,
            "subscriber {i} took messages out of order"
        );
        assert_eq!(
            s.messages, TOTAL,
            "subscriber {i} took {} of {TOTAL}",
            s.messages
        );
        assert_eq!(s.lags, 0, "subscriber {i}, keeping up, was told it lagged");
    }

    // The stalled one goes on at last: one lag notice naming what it missed, then what's kept.
    match stalled.take() {
        Some(Next::Lag { from, to }) => assert_eq!((from, to), (0, TOTAL - RETAIN)),
        other => panic!("the stalled subscriber took {other:?}, not a lag notice"),
    }
    let mut kept = Vec::new();
    while let Some(Next::Message(m)) = stalled.take() {
        kept.push(m.offset);
    }
    assert_eq!(kept, (TOTAL - RETAIN..TOTAL).collect::<Vec<_>>());
    assert_eq!(node.stats().lags, 1, "the node counts the one lag notice");
    assert_eq!(node.stats().published, TOTAL);
}
