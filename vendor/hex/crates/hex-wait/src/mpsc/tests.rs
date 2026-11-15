//! The channel, on every platform (AC-T7.4.1). Each wait here is for its fact alone (a thread's end, joined), but
//! where a deadline is the claim under test, which says so. How late a receive returns past its deadline is the
//! host's, which the client's mailbox test tells the census and holds to no bound; its receive waits on the
//! condition variable, whose deadline wait on macOS the kernel's read-back holds (`macos::tests`).

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// The deadline of the receives whose deadline is the claim under test: the client budget tests' caller cap.
const DEADLINE: Duration = Duration::from_millis(25);

/// Do send two messages into a channel of two, then a third; expect the third refused full and given back, and the
/// two taken in order.
#[test]
fn ac_t7_4_1_messages_come_in_order_and_a_full_channel_refuses_one_more() {
    let (tx, rx) = sync_channel(2).unwrap();
    tx.try_send(1).unwrap();
    tx.try_send(2).unwrap();
    assert!(matches!(tx.try_send(3), Err(TrySendError::Full(3))));
    assert_eq!(rx.try_recv().unwrap(), 1);
    assert_eq!(rx.recv().unwrap(), 2);
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

/// Do receive with a deadline on a channel nothing is sent on; expect a timeout no sooner than the deadline.
#[test]
fn ac_t7_4_1_a_receive_nothing_comes_to_times_out_at_its_deadline() {
    let (tx, rx) = sync_channel::<()>(1).unwrap();
    let began = Instant::now();
    assert!(matches!(
        rx.recv_timeout(DEADLINE),
        Err(RecvTimeoutError::Timeout)
    ));
    assert!(began.elapsed() >= DEADLINE);
    drop(tx);
}

/// Do send to a receiver waiting with a deadline, through a rendezvous, which takes a message only while its receiver
/// waits; expect the receive to end with the message, or, where the deadline passed before the send could go, timed
/// out. The deadline is the claim's.
#[test]
fn ac_t7_4_1_a_send_ends_a_timed_receive() {
    let (tx, rx) = sync_channel(0).unwrap();
    let waiter = std::thread::spawn(move || rx.recv_timeout(DEADLINE));
    let mut message = 7;
    let went = loop {
        match tx.try_send(message) {
            Ok(()) => break true,
            Err(TrySendError::Full(m)) => message = m,
            Err(TrySendError::Disconnected(_)) => break false,
        }
        std::thread::yield_now();
    };
    let got = waiter.join().unwrap();
    if went {
        assert_eq!(got.unwrap(), 7);
    } else {
        assert!(matches!(got, Err(RecvTimeoutError::Timeout)));
    }
}

/// Do drop the only sender of a channel holding a message; expect the message taken, then the channel disconnected,
/// a waiting receive among them.
#[test]
fn ac_t7_4_1_the_last_sender_gone_disconnects_once_what_is_left_is_taken() {
    let (tx, rx) = sync_channel(1).unwrap();
    tx.try_send(1).unwrap();
    let theirs = tx.clone();
    drop(tx);
    assert_eq!(rx.recv().unwrap(), 1);
    let waiter = std::thread::spawn(move || rx.recv());
    drop(theirs);
    assert!(matches!(
        waiter.join().unwrap(),
        Err(RecvError::Disconnected)
    ));
}

/// Do send on a rendezvous channel with and without a receiver waiting; expect it refused full until the receiver
/// waits, then taken.
#[test]
fn ac_t7_4_1_a_rendezvous_takes_a_message_only_while_its_receiver_waits() {
    let (tx, rx) = sync_channel(0).unwrap();
    assert!(matches!(tx.try_send(1), Err(TrySendError::Full(1))));
    let waiter = std::thread::spawn(move || rx.recv());
    let mut message = 2;
    loop {
        match tx.try_send(message) {
            Ok(()) => break,
            Err(TrySendError::Full(m)) => message = m,
            Err(TrySendError::Disconnected(_)) => panic!("the receiver went"),
        }
        std::thread::yield_now();
    }
    assert_eq!(waiter.join().unwrap().unwrap(), 2);
}

/// Do drop a receiver while a message it hadn't taken holds something; expect the message dropped with it, and a later
/// send refused disconnected, given back.
#[test]
fn ac_t7_4_1_the_receiver_gone_drops_what_it_left_and_refuses_sends() {
    struct Held(Arc<AtomicBool>);
    impl Drop for Held {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let (tx, rx) = sync_channel(1).unwrap();
    tx.try_send(Held(Arc::clone(&dropped))).unwrap();
    drop(rx);
    assert!(dropped.load(Ordering::SeqCst));
    let again = Arc::new(AtomicBool::new(false));
    assert!(matches!(
        tx.try_send(Held(Arc::clone(&again))),
        Err(TrySendError::Disconnected(_))
    ));
    assert!(
        again.load(Ordering::SeqCst),
        "the refused message came back, and went"
    );
}
