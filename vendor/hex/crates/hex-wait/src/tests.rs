//! The condition variable's waits, on every platform (AC-T7.4.1). Each wait here is for its fact alone (a thread's
//! end, joined), but where a deadline is the claim under test, which says so; how late a wait returns past its
//! deadline is held by the client's queued-sender test against the platform's recorded overshoot
//! (`hex_testkit::waits::OVERSHOOT`).

use super::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The deadline of the waits whose deadline is the claim under test: the client budget tests' caller cap.
const DEADLINE: Duration = Duration::from_millis(25);

/// Do wait for a condition nobody changes, with a deadline; expect the wait to end no sooner than its deadline,
/// timed out, holding its lock.
#[test]
fn ac_t7_4_1_a_timed_wait_nobody_notifies_ends_at_its_deadline_timed_out() {
    let lock = Mutex::new(0u32);
    let cv = Condvar::new();
    let began = Instant::now();
    let (mut held, timed) = cv
        .wait_timeout_while(lock.lock().unwrap(), DEADLINE, |_| true)
        .unwrap();
    assert!(began.elapsed() >= DEADLINE);
    assert!(timed.timed_out());
    // Still its holder: it writes, and once let go the lock is free.
    *held = 1;
    drop(held);
    assert_eq!(*lock.lock().unwrap(), 1);
}

/// Do notify a waiter with no deadline once what it waits for has changed; expect it to wake and see the change.
#[test]
fn ac_t7_4_1_a_notified_waiter_wakes_and_sees_the_change() {
    let shared = Arc::new((Mutex::new(false), Condvar::new()));
    let theirs = Arc::clone(&shared);
    let waiter = std::thread::spawn(move || {
        let (lock, cv) = &*theirs;
        *cv.wait_while(lock.lock().unwrap(), |set| !*set).unwrap()
    });
    let (lock, cv) = &*shared;
    *lock.lock().unwrap() = true;
    cv.notify_one();
    assert!(waiter.join().unwrap());
}

/// Do notify a waiter waiting with a deadline, once it is waiting; expect it to wake not timed out, unless the
/// notification itself came only after its deadline. The deadline is the claim's.
#[test]
fn ac_t7_4_1_a_notification_ends_a_timed_wait_before_its_deadline() {
    // (the waiter is waiting, the notifier has set it)
    let shared = Arc::new((Mutex::new((false, false)), Condvar::new()));
    let theirs = Arc::clone(&shared);
    let waiter = std::thread::spawn(move || {
        let (lock, cv) = &*theirs;
        let mut state = lock.lock().unwrap();
        state.0 = true;
        // No later than the wait's own deadline, which it takes from the clock after this.
        let by = Instant::now() + DEADLINE;
        let (state, timed) = cv
            .wait_timeout_while(state, DEADLINE, |state| !state.1)
            .unwrap();
        drop(state);
        (by, timed.timed_out())
    });
    let (lock, cv) = &*shared;
    let sent = loop {
        let mut state = lock.lock().unwrap();
        // Seen under the lock, so the waiter has let it go in its wait: the notification can't come too soon.
        if state.0 {
            state.1 = true;
            let sent = Instant::now();
            cv.notify_one();
            break sent;
        }
        drop(state);
        std::thread::yield_now();
    };
    let (by, timed_out) = waiter.join().unwrap();
    assert!(
        !timed_out || sent >= by,
        "timed out though notified {:?} before its deadline",
        by - sent
    );
}

/// Do notify all of two waiters, each waiting with no deadline; expect both to wake.
#[test]
fn ac_t7_4_1_notify_all_wakes_every_waiter() {
    // (how many wait, the notifier has set it)
    let shared = Arc::new((Mutex::new((0u32, false)), Condvar::new()));
    let waiters: Vec<_> = (0..2)
        .map(|_| {
            let theirs = Arc::clone(&shared);
            std::thread::spawn(move || {
                let (lock, cv) = &*theirs;
                let mut state = lock.lock().unwrap();
                state.0 += 1;
                cv.wait_while(state, |state| !state.1).unwrap().1
            })
        })
        .collect();
    let (lock, cv) = &*shared;
    loop {
        let mut state = lock.lock().unwrap();
        if state.0 == 2 {
            state.1 = true;
            cv.notify_all();
            break;
        }
        drop(state);
        std::thread::yield_now();
    }
    for waiter in waiters {
        assert!(waiter.join().unwrap());
    }
}

/// Do wait with a mutex a thread poisoned by panicking while it held it; expect the wait refused as poisoned, typed.
#[test]
fn ac_t7_4_1_a_wait_on_a_poisoned_mutex_is_refused_poisoned() {
    let lock = Arc::new(Mutex::new(()));
    let theirs = Arc::clone(&lock);
    let panicked = std::thread::spawn(move || {
        let _held = theirs.lock().unwrap();
        panic!("poisons the lock");
    })
    .join();
    assert!(panicked.is_err());
    let cv = Condvar::new();
    let held = lock.lock().unwrap_err().into_inner();
    let refused = cv.wait_timeout(held, Duration::ZERO).map(|_| ());
    assert!(matches!(refused, Err(Failed::Poisoned)));
}

/// Do rest until a deadline; expect it to return no sooner than the deadline.
#[test]
fn ac_t7_4_1_a_rest_ends_no_sooner_than_its_deadline() {
    let deadline = Instant::now() + DEADLINE;
    rest_until(deadline).unwrap();
    assert!(Instant::now() >= deadline);
}
