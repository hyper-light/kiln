//! The capacity view's cores, each test "do X, expect Y" (AC-T7.4.1; `docs/design/capacity-view.md`, the
//! decision each cites).

use std::collections::{BTreeMap, VecDeque};

use super::*;

const BUDGET: Budget = Budget {
    holders: 2,
    reservations: 8,
    name_bytes: 32,
};

fn cpu(milli: u64) -> Amounts {
    Amounts {
        cpu_milli: milli,
        ..Amounts::ZERO
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Mem(BTreeMap<String, String>);

impl Records for Mem {
    fn put(&mut self, name: &str, value: &str) -> Result<(), Stop> {
        self.0.insert(name.to_owned(), value.to_owned());
        Ok(())
    }
    fn remove(&mut self, name: &str) -> Result<(), Stop> {
        self.0.remove(name);
        Ok(())
    }
    fn all(&mut self) -> Result<Vec<(String, String)>, Stop> {
        Ok(self.0.clone().into_iter().collect())
    }
}

/// The machine's records as a crash nemesis sees them: writes counted across calls, and the `at`-th
/// crashing, before it lands or once it has (`after`).
struct Counted<'a> {
    inner: &'a mut Mem,
    writes: &'a mut u64,
    crash: Option<(u64, bool)>,
}

impl Counted<'_> {
    fn strike(&mut self, write: impl FnOnce(&mut Mem) -> Result<(), Stop>) -> Result<(), Stop> {
        let n = *self.writes;
        *self.writes += 1;
        match self.crash {
            Some((at, after)) if at == n => {
                if after {
                    write(self.inner)?;
                }
                Err(Stop::Crashed)
            }
            _ => write(self.inner),
        }
    }
}

impl Records for Counted<'_> {
    fn put(&mut self, name: &str, value: &str) -> Result<(), Stop> {
        self.strike(|m| m.put(name, value))
    }
    fn remove(&mut self, name: &str) -> Result<(), Stop> {
        self.strike(|m| m.remove(name))
    }
    fn all(&mut self) -> Result<Vec<(String, String)>, Stop> {
        self.inner.all()
    }
}

/// One machine: its keeper, the keeper's records, and what its host admitted itself.
struct Machine {
    keeper: Keeper,
    records: Mem,
    local: Amounts,
    writes: u64,
    crash: Option<(u64, bool)>,
}

impl Machine {
    fn new(room: Amounts) -> Self {
        let mut records = Mem::default();
        let mut keeper = Keeper::boot(BUDGET, &mut records).unwrap();
        keeper.set_room(room);
        Self {
            keeper,
            records,
            local: Amounts::ZERO,
            writes: 0,
            crash: None,
        }
    }

    fn reboot(&mut self) {
        let room = self.keeper.room();
        self.keeper = Keeper::boot(BUDGET, &mut self.records).unwrap();
        self.keeper.set_room(room);
    }

    fn try_hear(&mut self, msg: ToKeeper) -> Result<ToHolder, Stop> {
        let mut records = Counted {
            inner: &mut self.records,
            writes: &mut self.writes,
            crash: self.crash,
        };
        self.keeper.hear(msg, self.local, &mut records)
    }

    fn hear(&mut self, msg: ToKeeper) -> ToHolder {
        self.try_hear(msg).unwrap()
    }

    /// Says `msgs` to the keeper and its answers to `holder` until neither has more to say.
    fn talk(&mut self, holder: &mut Holder, msgs: Vec<ToKeeper>) -> Vec<Event> {
        let mut queue: VecDeque<ToKeeper> = msgs.into();
        let mut events = Vec::new();
        while let Some(msg) = queue.pop_front() {
            let answer = self.hear(msg);
            let mut out = Out::default();
            holder.hear(answer, &mut out);
            queue.extend(out.send);
            events.extend(out.events);
        }
        events
    }

    fn join(&mut self, holder: &mut Holder) -> Vec<Event> {
        let hello = holder.connect().unwrap();
        self.talk(holder, vec![hello])
    }

    fn reserve(&mut self, holder: &mut Holder, amount: Amounts) -> (ReservationId, Vec<Event>) {
        let (id, msg) = holder.reserve(amount).unwrap();
        (id, self.talk(holder, vec![msg]))
    }

    fn release(&mut self, holder: &mut Holder, id: ReservationId) -> Vec<Event> {
        let mut out = Out::default();
        holder.release(id, &mut out);
        let mut events = out.events;
        events.extend(self.talk(holder, out.send));
        events
    }

    /// A raw session: hello, then (if `sync`) an empty sync; its fence.
    fn session(&mut self, name: &str, epoch: u64, sync: bool) -> Fence {
        let ToHolder::Welcome { fence, .. } = self.hear(ToKeeper::Hello {
            holder: name.to_owned(),
            epoch,
            scope: None,
        }) else {
            panic!("no welcome");
        };
        if sync {
            let synced = self.hear(ToKeeper::Sync {
                fence,
                held: Vec::new(),
            });
            assert!(matches!(synced, ToHolder::Synced { .. }), "{synced:?}");
        }
        fence
    }
}

fn holder(name: &str, epoch: u64) -> Holder {
    Holder::start(name.to_owned(), epoch, BUDGET.reservations, &[]).unwrap()
}

fn sum(held: impl Iterator<Item = Held>) -> Amounts {
    held.fold(Amounts::ZERO, |sum, h| sum.checked_add(h.amount).unwrap())
}

/// AC-T7.4.1, CV3: reserve beside the host's own admissions and another holder's; expect a grant only
/// for what fits the room left, and the host's own budget to be the room less every holder's.
#[test]
fn cv3_a_holder_is_granted_only_what_fits_beside_the_hosts_own_and_other_holders() {
    let mut m = Machine::new(cpu(8000));
    m.local = cpu(3000);
    let (mut a, mut b) = (holder("kiln-a", 1), holder("kiln-b", 1));
    m.join(&mut a);
    m.join(&mut b);
    let (ra, events) = m.reserve(&mut a, cpu(4000));
    assert_eq!(events, vec![Event::Granted(ra)]);
    let (rb, events) = m.reserve(&mut b, cpu(2000));
    assert_eq!(events, vec![Event::Refused(rb, Why::Room)]);
    assert!(!b.may_run(rb));
    let (rb, events) = m.reserve(&mut b, cpu(1000));
    assert_eq!(events, vec![Event::Granted(rb)]);
    assert!(a.may_run(ra) && b.may_run(rb));
    assert_eq!(m.keeper.view(m.local).free(), Amounts::ZERO);
    assert_eq!(m.keeper.room_for_local(), cpu(3000));
    assert_eq!(m.keeper.foreign(), cpu(5000));
}

/// AC-T7.4.1, CV7: refuse a reserve for room; expect the refusal to carry the view, everything held
/// beside the room, so the next decision is made on the room there is.
#[test]
fn cv7_a_refusal_carries_the_view() {
    let mut m = Machine::new(cpu(4000));
    m.local = cpu(1000);
    let mut a = holder("kiln-a", 1);
    m.join(&mut a);
    m.reserve(&mut a, cpu(2000));
    let (_, msg) = a.reserve(cpu(2000)).unwrap();
    let ToHolder::Refused { why, view, .. } = m.hear(msg) else {
        panic!("not refused");
    };
    assert_eq!(why, Why::Room);
    assert_eq!(
        view,
        View {
            room: cpu(4000),
            held: cpu(3000)
        }
    );
    assert_eq!(view.free(), cpu(1000));
}

/// AC-T7.4.1, CV3 and CV8: ask GPUs, nothing, and past the holder's bound; expect each refused, typed.
#[test]
fn cv3_gpus_nothing_and_past_the_bound_are_refused_typed() {
    let mut m = Machine::new(Amounts::from_array([64_000, 1 << 40, 1 << 40, 8]));
    let fence = m.session("kiln-a", 1, true);
    let id = |seq| ReservationId { epoch: 1, seq };
    let gpu = Amounts::from_array([1000, 0, 0, 1]);
    let refused = |answer: ToHolder| match answer {
        ToHolder::Refused { why, .. } => why,
        other => panic!("{other:?}"),
    };
    let ask = |m: &mut Machine, seq, amount| {
        m.hear(ToKeeper::Reserve {
            fence,
            id: id(seq),
            amount,
        })
    };
    assert_eq!(refused(ask(&mut m, 0, gpu)), Why::Devices);
    assert_eq!(refused(ask(&mut m, 1, Amounts::ZERO)), Why::Empty);
    for seq in 2..10 {
        let answer = ask(&mut m, seq, cpu(1));
        assert!(matches!(answer, ToHolder::Granted { .. }), "{answer:?}");
    }
    assert_eq!(refused(ask(&mut m, 10, cpu(1))), Why::Full { bound: 8 });
    let mut b = holder("kiln-b", 1);
    m.join(&mut b);
    assert_eq!(b.reserve(Amounts::ZERO).unwrap_err(), NotNow::Empty);
}

/// AC-T7.4.1, CV3: shrink the room below what is held; expect the view over, nothing admitted, running
/// reservations kept, and admission again once a release makes it fit.
#[test]
fn cv3_a_room_shrunk_below_what_is_held_admits_nothing_until_it_fits() {
    let mut m = Machine::new(cpu(8000));
    let mut a = holder("kiln-a", 1);
    m.join(&mut a);
    let (r1, _) = m.reserve(&mut a, cpu(3000));
    let (r2, _) = m.reserve(&mut a, cpu(3000));
    m.keeper.set_room(cpu(5000));
    let view = m.keeper.view(m.local);
    assert_eq!((view.over(), view.free()), (cpu(1000), Amounts::ZERO));
    let (r3, events) = m.reserve(&mut a, cpu(1));
    assert_eq!(events, vec![Event::Refused(r3, Why::Room)]);
    assert!(a.may_run(r1) && a.may_run(r2));
    m.release(&mut a, r2);
    let (r4, events) = m.reserve(&mut a, cpu(2000));
    assert_eq!(events, vec![Event::Granted(r4)]);
}

/// AC-T7.4.1, CV4: start a newer process of a holder beside its older one; expect the older one's
/// session closed, its hello refused as older, and nothing it says to change the books.
#[test]
fn cv4_a_newer_process_fences_the_older_one() {
    let mut m = Machine::new(cpu(8000));
    let mut old = holder("kiln-a", 5);
    m.join(&mut old);
    let (r1, _) = m.reserve(&mut old, cpu(1000));
    let survivor = Held {
        id: r1,
        amount: cpu(1000),
    };
    let mut new = Holder::start("kiln-a".to_owned(), 6, BUDGET.reservations, &[survivor]).unwrap();
    let events = m.join(&mut new);
    assert!(
        matches!(events.as_slice(), [Event::Synced { agree: true, .. }]),
        "{events:?}"
    );
    let (r2, msg) = old.reserve(cpu(1000)).unwrap();
    let events = m.talk(&mut old, vec![msg]);
    assert_eq!(
        events,
        vec![Event::Unanswered(r2), Event::Closed(Close::Session)]
    );
    let events = m.join(&mut old);
    assert_eq!(
        events,
        vec![Event::Closed(Close::OlderEpoch { current: 6 })]
    );
    assert_eq!(m.keeper.foreign(), cpu(1000));
    assert_eq!(
        m.keeper.held_by("kiln-a").collect::<Vec<_>>(),
        vec![survivor]
    );
}

/// AC-T7.4.1, CV4: say a release under a session whose link has dropped; expect it closed and the
/// reservation kept.
#[test]
fn cv4_a_message_under_a_dropped_session_changes_nothing() {
    let mut m = Machine::new(cpu(8000));
    let fence = m.session("kiln-a", 1, true);
    let id = ReservationId { epoch: 1, seq: 0 };
    let granted = m.hear(ToKeeper::Reserve {
        fence,
        id,
        amount: cpu(1000),
    });
    assert!(matches!(granted, ToHolder::Granted { .. }));
    m.keeper.link_down(fence.session);
    let answer = m.hear(ToKeeper::Release { fence, id });
    assert_eq!(
        answer,
        ToHolder::Closed {
            rejected: Some(fence),
            why: Close::Session
        }
    );
    assert_eq!(m.keeper.foreign(), cpu(1000));
}

/// AC-T7.4.1, R04: reserve before the session's sync; expect it closed as not synced, the session ended,
/// and nothing held.
#[test]
fn r04_a_reserve_before_the_sync_is_closed_not_synced() {
    let mut m = Machine::new(cpu(8000));
    let fence = m.session("kiln-a", 1, false);
    let reserve = ToKeeper::Reserve {
        fence,
        id: ReservationId { epoch: 1, seq: 0 },
        amount: cpu(1),
    };
    let closed = |why| ToHolder::Closed {
        rejected: Some(fence),
        why,
    };
    assert_eq!(m.hear(reserve.clone()), closed(Close::NotSynced));
    assert_eq!(m.hear(reserve), closed(Close::Session));
    assert_eq!(m.keeper.foreign(), Amounts::ZERO);
}

/// AC-T7.4.1, CV5: release one reservation twice and an id never held; expect each answered released and
/// the books changed once.
#[test]
fn cv5_release_is_idempotent() {
    let mut m = Machine::new(cpu(8000));
    let fence = m.session("kiln-a", 1, true);
    let id = ReservationId { epoch: 1, seq: 0 };
    m.hear(ToKeeper::Reserve {
        fence,
        id,
        amount: cpu(1000),
    });
    for id in [id, id, ReservationId { epoch: 1, seq: 9 }] {
        let answer = m.hear(ToKeeper::Release { fence, id });
        assert!(matches!(answer, ToHolder::Released { .. }), "{answer:?}");
    }
    assert_eq!(m.keeper.foreign(), Amounts::ZERO);
    assert!(m.records.0.keys().all(|name| !name.starts_with("res-")));
}

/// AC-T7.4.1, CV5: drop the link while a holder holds two reservations, and end one's work while it is
/// down; expect the keeper to keep both while it is down, and the rejoin's sync to release the ended one
/// with the counts agreeing.
#[test]
fn cv5_a_dropped_link_releases_nothing_and_the_next_sync_releases_what_ended() {
    let mut m = Machine::new(cpu(8000));
    let mut a = holder("kiln-a", 1);
    m.join(&mut a);
    let (r1, _) = m.reserve(&mut a, cpu(1000));
    let (r2, _) = m.reserve(&mut a, cpu(2000));
    // The keeper's first session of its epoch is 1.
    m.keeper.link_down(1);
    let mut out = Out::default();
    a.link_down(&mut out);
    assert!(out.events.is_empty());
    assert_eq!(m.keeper.foreign(), cpu(3000));
    m.release(&mut a, r2);
    assert_eq!(m.keeper.foreign(), cpu(3000));
    let events = m.join(&mut a);
    assert_eq!(
        events,
        vec![Event::Synced {
            agree: true,
            released: 1,
            adopted: 0,
            disagreed: 0
        }]
    );
    assert!(a.may_run(r1) && !a.may_run(r2));
    assert_eq!(m.keeper.foreign(), cpu(1000));
}

/// AC-T7.4.1, CV5: list in a sync an id the keeper lacks and one whose amount differs; expect the first
/// adopted (its work runs), the larger amount kept for the second, and the counts said.
#[test]
fn cv5_a_sync_adopts_what_the_keeper_lacks_and_keeps_the_larger_amount() {
    let mut m = Machine::new(cpu(8000));
    let fence = m.session("kiln-a", 1, true);
    let (known, unknown) = (
        ReservationId { epoch: 1, seq: 0 },
        ReservationId { epoch: 0, seq: 7 },
    );
    m.hear(ToKeeper::Reserve {
        fence,
        id: known,
        amount: cpu(3000),
    });
    let answer = m.hear(ToKeeper::Sync {
        fence,
        held: vec![
            Held {
                id: known,
                amount: cpu(1000),
            },
            Held {
                id: unknown,
                amount: cpu(500),
            },
        ],
    });
    let ToHolder::Synced {
        count,
        sum,
        released,
        adopted,
        disagreed,
        ..
    } = answer
    else {
        panic!("{answer:?}");
    };
    assert_eq!((count, sum), (2, cpu(3500)));
    assert_eq!((released, adopted, disagreed), (0, 1, 1));
    let held: Vec<Held> = m.keeper.held_by("kiln-a").collect();
    assert_eq!(
        held,
        vec![
            Held {
                id: unknown,
                amount: cpu(500)
            },
            Held {
                id: known,
                amount: cpu(3000)
            }
        ]
    );
    assert_eq!(m.keeper.foreign(), cpu(3500));
}

/// AC-T7.4.1, CV5: rejoin a holder whose reservation the keeper's records hold larger; expect the holder
/// told the counts don't agree.
#[test]
fn cv5_a_holder_told_counts_that_differ_says_they_dont_agree() {
    let mut m = Machine::new(cpu(8000));
    let mut a = holder("kiln-a", 2);
    m.join(&mut a);
    let (id, _) = m.reserve(&mut a, cpu(1000));
    // The keeper's record says more than the holder holds (written by hand: no keeper grants so).
    let name = format!("res-0-{}-{}", id.epoch, id.seq);
    m.records.put(&name, "4000 0 0 0").unwrap();
    m.reboot();
    let mut out = Out::default();
    a.link_down(&mut out);
    let events = m.join(&mut a);
    assert_eq!(
        events,
        vec![Event::Synced {
            agree: false,
            released: 0,
            adopted: 0,
            disagreed: 1
        }]
    );
}

/// AC-T7.4.1, CV5 and CV6: let a reserve's wait run out before its answer; expect its work never to run,
/// its release said at once, and the keeper to hold nothing once both arrive.
#[test]
fn cv5_a_reserve_whose_wait_ran_out_never_runs_and_is_freed() {
    let mut m = Machine::new(cpu(8000));
    let mut a = holder("kiln-a", 1);
    m.join(&mut a);
    let (id, reserve) = a.reserve(cpu(1000)).unwrap();
    let mut out = Out::default();
    a.ran_out(id, &mut out);
    assert_eq!(out.events, vec![Event::Unanswered(id)]);
    assert!(!a.may_run(id));
    let mut msgs = vec![reserve];
    msgs.extend(out.send);
    let events = m.talk(&mut a, msgs);
    assert!(events.is_empty(), "{events:?}");
    assert!(!a.may_run(id));
    assert_eq!(m.keeper.foreign(), Amounts::ZERO);
}

/// AC-T7.4.1, CV5: grant a reserve whose answer the dropped link lost; expect the holder to abandon it and
/// leave it out of its next sync, which releases it.
#[test]
fn cv5_a_grant_lost_with_the_link_is_released_by_the_next_sync() {
    let mut m = Machine::new(cpu(8000));
    let mut a = holder("kiln-a", 1);
    m.join(&mut a);
    let (id, reserve) = a.reserve(cpu(1000)).unwrap();
    let ToHolder::Granted { fence, .. } = m.hear(reserve) else {
        panic!("not granted");
    };
    m.keeper.link_down(fence.session);
    let mut out = Out::default();
    a.link_down(&mut out);
    assert_eq!(out.events, vec![Event::Unanswered(id)]);
    let events = m.join(&mut a);
    assert_eq!(
        events,
        vec![Event::Synced {
            agree: true,
            released: 1,
            adopted: 0,
            disagreed: 0
        }]
    );
    assert_eq!(m.keeper.foreign(), Amounts::ZERO);
}

/// AC-T7.4.1, CV5: end a reservation's work while its sync is in flight; expect its release said once the
/// sync is answered, and the books then empty.
#[test]
fn cv5_a_release_while_syncing_is_said_once_synced() {
    let mut m = Machine::new(cpu(8000));
    let mut a = holder("kiln-a", 1);
    m.join(&mut a);
    let (id, _) = m.reserve(&mut a, cpu(1000));
    let mut out = Out::default();
    a.link_down(&mut out);
    let hello = a.connect().unwrap();
    let welcome = m.hear(hello);
    let mut out = Out::default();
    a.hear(welcome, &mut out);
    let sync = out.send;
    let mut out = Out::default();
    a.release(id, &mut out);
    assert!(out.send.is_empty());
    let events = m.talk(&mut a, sync);
    assert_eq!(
        events,
        vec![
            Event::Synced {
                agree: true,
                released: 0,
                adopted: 0,
                disagreed: 0
            },
            Event::Released(id)
        ]
    );
    assert_eq!(m.keeper.foreign(), Amounts::ZERO);
}

/// AC-T7.4.1, CV5: restart a holder whose work outlived it, once listing that work and once not; expect
/// the keeper to keep its room in the first and release it in the second.
#[test]
fn cv5_a_restarted_holder_keeps_what_it_lists_and_releases_what_it_doesnt() {
    for lists in [true, false] {
        let mut m = Machine::new(cpu(8000));
        let mut a = holder("kiln-a", 1);
        m.join(&mut a);
        let (id, _) = m.reserve(&mut a, cpu(1000));
        let running = [Held {
            id,
            amount: cpu(1000),
        }];
        let running: &[Held] = if lists { &running } else { &[] };
        let mut restarted =
            Holder::start("kiln-a".to_owned(), 2, BUDGET.reservations, running).unwrap();
        let events = m.join(&mut restarted);
        assert_eq!(
            events,
            vec![Event::Synced {
                agree: true,
                released: u64::from(!lists),
                adopted: 0,
                disagreed: 0
            }]
        );
        let expected = if lists { cpu(1000) } else { Amounts::ZERO };
        assert_eq!(m.keeper.foreign(), expected);
        assert_eq!(restarted.may_run(id), lists);
    }
}

/// The conversation the crash test repeats: join, three reserves, then a release of the first.
fn script(m: &mut Machine, a: &mut Holder) -> Result<(), Stop> {
    let mut queue: VecDeque<ToKeeper> = a.connect().into_iter().collect();
    let mut granted = Vec::new();
    for step in 0..5 {
        while let Some(msg) = queue.pop_front() {
            let answer = m.try_hear(msg)?;
            let mut out = Out::default();
            a.hear(answer, &mut out);
            queue.extend(out.send);
            granted.extend(out.events.iter().filter_map(|event| match event {
                Event::Granted(id) => Some(*id),
                _ => None,
            }));
        }
        match step {
            0..=2 => queue.push_back(a.reserve(cpu(1000 * (step + 1))).unwrap().1),
            3 => {
                let mut out = Out::default();
                a.release(granted[0], &mut out);
                queue.extend(out.send);
            }
            _ => {}
        }
    }
    Ok(())
}

/// AC-T7.4.1, CV2: crash the keeper at every write of a conversation, before and after the write lands,
/// and boot it again; expect every reservation the holder heard granted to be in the rebooted keeper's
/// books before any sync, and the books to equal the holder's once it rejoins, the counts agreeing.
#[test]
fn cv2_a_keeper_crashed_at_any_write_keeps_every_grant_it_answered() {
    let mut m = Machine::new(cpu(64_000));
    script(&mut m, &mut holder("kiln-a", 1)).unwrap();
    let writes = m.writes;
    assert_eq!(
        writes, 5,
        "hello, three reserves and a release each write once"
    );
    for at in 0..writes {
        for after in [false, true] {
            let mut m = Machine::new(cpu(64_000));
            let mut a = holder("kiln-a", 1);
            m.crash = Some((at, after));
            assert_eq!(script(&mut m, &mut a), Err(Stop::Crashed), "write {at}");
            m.crash = None;
            let before = m.keeper.epoch();
            m.reboot();
            assert_eq!(m.keeper.epoch(), before + 1);
            let kept: Vec<Held> = m.keeper.held_by("kiln-a").collect();
            for held in a.held() {
                assert!(
                    kept.contains(&held),
                    "{held:?} granted, not kept (write {at}, after {after})"
                );
            }
            let mut out = Out::default();
            a.link_down(&mut out);
            let events = m.join(&mut a);
            assert!(
                matches!(events.as_slice(), [Event::Synced { agree: true, .. }]),
                "write {at}, after {after}: {events:?}"
            );
            let books: Vec<Held> = m.keeper.held_by("kiln-a").collect();
            assert_eq!(books, a.held().collect::<Vec<_>>());
            assert_eq!(m.keeper.foreign(), sum(a.held()));
        }
    }
}

/// AC-T7.4.1, CV7: change the view by one holder's reserves, the host's own admission and a room
/// measured anew; expect one hint owed to every other synced holder however many changes, none to the
/// holder that caused it, and none for a room measured the same.
#[test]
fn cv7_hints_are_owed_once_to_every_other_synced_holder() {
    let mut m = Machine::new(cpu(8000));
    let (mut a, mut b) = (holder("kiln-a", 1), holder("kiln-b", 1));
    m.join(&mut a);
    m.join(&mut b);
    m.reserve(&mut a, cpu(1000));
    m.reserve(&mut a, cpu(1000));
    let hints = m.keeper.hints(m.local);
    assert_eq!(hints.len(), 1);
    let (_, hint) = hints.into_iter().next().unwrap();
    let mut out = Out::default();
    b.hear(hint, &mut out);
    assert_eq!(b.view().map(|view| view.held), Some(cpu(2000)));
    assert_eq!(b.stale(), 0);
    assert!(m.keeper.hints(m.local).is_empty());
    m.keeper.set_room(cpu(8000));
    assert!(m.keeper.hints(m.local).is_empty());
    m.local = cpu(500);
    m.keeper.local_changed();
    assert_eq!(m.keeper.hints(m.local).len(), 2);
}

/// AC-T7.4.1, CV8: say hello from more holders than the budget; expect the one past it closed, typed.
#[test]
fn cv8_holders_past_the_budget_are_closed() {
    let mut m = Machine::new(cpu(8000));
    m.session("kiln-a", 1, true);
    m.session("kiln-b", 1, true);
    let answer = m.hear(ToKeeper::Hello {
        holder: "kiln-c".to_owned(),
        epoch: 1,
        scope: None,
    });
    assert_eq!(
        answer,
        ToHolder::Closed {
            rejected: None,
            why: Close::HoldersFull { bound: 2 }
        }
    );
}

/// AC-T7.4.1, CV8 and CLAUDE.md §6: send malformed hellos and syncs; expect each closed as invalid and
/// the books unchanged.
#[test]
fn cv8_malformed_hellos_and_syncs_are_invalid() {
    let mut m = Machine::new(cpu(8000));
    for name in ["", "has space", &"x".repeat(33), "tab\t", "caf\u{e9}"] {
        let answer = m.hear(ToKeeper::Hello {
            holder: name.to_owned(),
            epoch: 1,
            scope: None,
        });
        assert_eq!(
            answer,
            ToHolder::Closed {
                rejected: None,
                why: Close::Invalid(Invalid::Name)
            }
        );
    }
    let held = |epoch, seq, milli| Held {
        id: ReservationId { epoch, seq },
        amount: cpu(milli),
    };
    let cases = [
        (
            (0..9).map(|s| held(1, s, 1)).collect(),
            Invalid::SyncTooLong { bound: 8 },
        ),
        (vec![held(1, 0, 1), held(1, 0, 1)], Invalid::DuplicateId),
        (vec![held(4, 0, 1)], Invalid::FutureId),
        (vec![held(1, 0, u64::MAX), held(1, 1, 1)], Invalid::Overflow),
    ];
    for (listed, why) in cases {
        let fence = m.session("kiln-a", 3, false);
        let answer = m.hear(ToKeeper::Sync {
            fence,
            held: listed,
        });
        assert_eq!(
            answer,
            ToHolder::Closed {
                rejected: Some(fence),
                why: Close::Invalid(why)
            }
        );
    }
    assert_eq!(m.keeper.foreign(), Amounts::ZERO);
    assert!(m.records.0.keys().all(|name| !name.starts_with("res-")));
}

/// AC-T7.4.1, CV4: boot a keeper three times; expect its epoch to grow by one each time, and a fence of
/// the first epoch closed.
#[test]
fn cv4_the_keepers_epoch_grows_at_every_boot() {
    let mut m = Machine::new(cpu(8000));
    let fence = m.session("kiln-a", 1, true);
    assert_eq!(m.keeper.epoch(), 1);
    m.reboot();
    m.reboot();
    assert_eq!(m.keeper.epoch(), 3);
    let answer = m.hear(ToKeeper::Release {
        fence,
        id: ReservationId { epoch: 1, seq: 0 },
    });
    assert_eq!(
        answer,
        ToHolder::Closed {
            rejected: Some(fence),
            why: Close::Session
        }
    );
}

/// AC-T7.4.1, CLAUDE.md §6: boot from hostile record text, and from records of a name no keeper writes;
/// expect each refused as damaged or full, never a panic, and the keeper's own records booted.
#[test]
fn hostile_records_are_refused_never_a_panic() {
    let holder = ("holder-0", "1 a");
    let cases: &[&[(&str, &str)]] = &[
        &[("epoch", "x")],
        &[("epoch", "18446744073709551615")],
        &[("holder-0", "1")],
        &[("holder-0", "1 has space")],
        &[("holder-0", "x kiln")],
        &[("holder-1", "1 kiln")],
        &[holder, ("holder-1", "1 b"), ("holder-2", "1 c")],
        &[("res-0-1-0", "1 0 0 0")],
        &[holder, ("res-0-2-0", "1 0 0 0")],
        &[holder, ("res-0-1", "1 0 0 0")],
        &[holder, ("res-0-1-0-0", "1 0 0 0")],
        &[holder, ("res-0-1-0", "1 0 0")],
        &[holder, ("res-0-1-0", "1 0 0 0 0")],
        &[holder, ("res-0-1-0", "-1 0 0 0")],
        &[holder, ("res-0-1-0", "")],
        &[
            holder,
            ("res-0-1-0", "18446744073709551615 0 0 0"),
            ("res-0-1-1", "1 0 0 0"),
        ],
        &[("other", "")],
        &[("own-1-2-3", "x")],
    ];
    for case in cases {
        let records = case
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()));
        let mut records = Mem(records.collect());
        let result = Keeper::boot(BUDGET, &mut records);
        assert!(
            matches!(result, Err(Stop::Damaged(_) | Stop::Full { .. })),
            "{case:?}: {result:?}"
        );
    }
    let kept = [
        ("epoch", "4"),
        ("holder-0", "2 a"),
        ("res-0-2-0", "1 2 3 0"),
    ];
    let records = kept.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned()));
    let mut records = Mem(records.collect());
    let keeper = Keeper::boot(BUDGET, &mut records).unwrap();
    assert_eq!(keeper.epoch(), 5);
    assert_eq!(keeper.foreign(), Amounts::from_array([1, 2, 3, 0]));
}

/// AC-T7.4.1, CV9: write every message as JSON and read it back; expect each the same, so the wire can
/// carry them.
#[test]
fn messages_round_trip_through_json() {
    let fence = Fence {
        keeper: 2,
        session: 3,
        holder: 4,
    };
    let id = ReservationId { epoch: 4, seq: 5 };
    let view = View {
        room: cpu(8000),
        held: cpu(1000),
    };
    let held = vec![Held { id, amount: cpu(1) }];
    let to_keeper = [
        ToKeeper::Hello {
            holder: "kiln-a".to_owned(),
            epoch: 4,
            scope: None,
        },
        ToKeeper::Sync { fence, held },
        ToKeeper::Reserve {
            fence,
            id,
            amount: cpu(1),
        },
        ToKeeper::Release { fence, id },
    ];
    for msg in to_keeper {
        let text = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<ToKeeper>(&text).unwrap(), msg);
    }
    let to_holder = [
        ToHolder::Welcome { fence, view },
        ToHolder::Synced {
            fence,
            view,
            count: 1,
            sum: cpu(1),
            released: 2,
            adopted: 3,
            disagreed: 4,
        },
        ToHolder::Granted {
            fence,
            id,
            amount: cpu(1),
            view,
        },
        ToHolder::Refused {
            fence,
            id,
            why: Why::Full { bound: 8 },
            view,
        },
        ToHolder::Released { fence, id, view },
        ToHolder::Hint { fence, view },
        ToHolder::Closed {
            rejected: Some(fence),
            why: Close::Invalid(Invalid::SyncTooLong { bound: 8 }),
        },
    ];
    for msg in to_holder {
        let text = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<ToHolder>(&text).unwrap(), msg);
    }
}

/// AC-T7.4.1, CV5: answer a holder's reserve with a grant of another amount than it asked; expect the
/// holder to give it back and its work not to start.
#[test]
fn cv5_a_grant_of_another_amount_is_given_back() {
    let mut a = holder("kiln-a", 1);
    let fence = Fence {
        keeper: 1,
        session: 1,
        holder: 1,
    };
    let view = View::default();
    let mut out = Out::default();
    a.connect();
    a.hear(ToHolder::Welcome { fence, view }, &mut out);
    let synced = ToHolder::Synced {
        fence,
        view,
        count: 0,
        sum: Amounts::ZERO,
        released: 0,
        adopted: 0,
        disagreed: 0,
    };
    a.hear(synced, &mut out);
    let (id, _) = a.reserve(cpu(1000)).unwrap();
    let mut out = Out::default();
    let granted = ToHolder::Granted {
        fence,
        id,
        amount: cpu(2000),
        view,
    };
    a.hear(granted, &mut out);
    assert_eq!(out.send, vec![ToKeeper::Release { fence, id }]);
    assert_eq!(out.events, vec![Event::Refused(id, Why::Disagrees)]);
    assert!(!a.may_run(id));
}

/// AC-T7.4.1, CV6: the person releases a holder that is gone for good; expect it refused while the
/// holder is connected and for a name never seen, and once the holder is away every reservation of its
/// released (its records gone), what was released returned for the log, the other holders hinted, the
/// room free again, and the holder's own later sync adopting what still runs.
#[test]
fn cv6_the_persons_release_of_a_holder_gone_for_good() {
    let mut m = Machine::new(cpu(8000));
    let (mut gone, mut other) = (holder("kiln-a", 3), holder("kiln-b", 1));
    m.join(&mut gone);
    m.join(&mut other);
    let (r1, _) = m.reserve(&mut gone, cpu(3000));
    m.reserve(&mut gone, cpu(2000));
    assert_eq!(
        m.keeper.forget("kiln-a", &mut m.records).unwrap(),
        Err(NotForgotten::Connected)
    );
    assert_eq!(
        m.keeper.forget("kiln-z", &mut m.records).unwrap(),
        Err(NotForgotten::Unknown)
    );
    m.keeper.link_down(1);
    let forgotten = m.keeper.forget("kiln-a", &mut m.records).unwrap();
    assert_eq!(
        forgotten,
        Ok(Forgotten {
            epoch: 3,
            count: 2,
            sum: cpu(5000)
        })
    );
    assert!(m.records.0.keys().all(|name| !name.starts_with("res-")));
    assert_eq!(m.keeper.foreign(), Amounts::ZERO);
    assert_eq!(m.keeper.hints(m.local).len(), 1);
    let (whole, events) = m.reserve(&mut other, cpu(8000));
    assert_eq!(events, vec![Event::Granted(whole)]);
    m.release(&mut other, whole);
    // The person was wrong about one: its work still runs, and the holder's own sync says so.
    let mut out = Out::default();
    gone.link_down(&mut out);
    let ended = gone.held().map(|h| h.id).find(|id| *id != r1).unwrap();
    let mut out = Out::default();
    gone.release(ended, &mut out);
    let events = m.join(&mut gone);
    assert_eq!(
        events,
        vec![Event::Synced {
            agree: true,
            released: 0,
            adopted: 1,
            disagreed: 0
        }]
    );
    assert_eq!(m.keeper.foreign(), cpu(3000));
}

/// AC-T7.4.1, CLAUDE.md §9: run the capacity view's conformance suite against its simulator; expect every
/// case to pass.
#[test]
fn the_simulator_passes_the_conformance_suite() {
    let mut make = || -> Result<Box<dyn Host>, crate::Refusal> {
        Ok(Box::new(sim::SimView::new(BUDGET, conformance::ROOM)?))
    };
    let failures = conformance::view(&mut make);
    assert!(failures.is_empty(), "{failures:#?}");
}

fn gib(n: u64) -> Amounts {
    Amounts {
        mem_bytes: n << 30,
        ..Amounts::ZERO
    }
}

/// The session the keeper's sample reads `name`'s scope under, if it is synced and named one.
fn scoped_session(m: &Machine, name: &str) -> Option<u64> {
    m.keeper
        .scopes()
        .into_iter()
        .find(|(_, holder, _)| holder == name)
        .map(|(session, _, _)| session)
}

/// AC-T7.4.1, CV12: a machine of 8 GiB, a holder whose hello names its work's cgroup reserves 4 GiB and its work
/// uses 3, so the host's measured room is 5; then its work uses 6; then its link drops. Expect the room every
/// admission is decided in to be 8 with 4 free (the holder's 3 GiB counted once, not again as missing from free
/// memory), then 6 with 2 free (its use past its reservations counted against the room), then 5 again at once.
#[test]
fn cv12_a_scoped_holders_use_is_credited_while_synced_up_to_what_it_holds() {
    let mut m = Machine::new(gib(8));
    let mut kiln = holder("kiln-a", 1).with_scope(Scope::Cgroup("kiln.slice/jobs".into()));
    m.join(&mut kiln);
    let (run, events) = m.reserve(&mut kiln, gib(4));
    assert_eq!(events, vec![Event::Granted(run)]);
    let session = scoped_session(&m, "kiln-a").unwrap();
    m.keeper.set_room(gib(5));
    m.keeper.sampled(&[(session, Ok(gib(3)))]);
    let view = m.keeper.view(m.local);
    assert_eq!(
        (view.room, view.free()),
        (gib(8), gib(4)),
        "the holder's 3 GiB counted twice: missing from the measured room and held besides"
    );
    assert_eq!(m.keeper.room_for_local(), gib(4));
    m.keeper.set_room(gib(2));
    m.keeper.sampled(&[(session, Ok(gib(6)))]);
    let view = m.keeper.view(m.local);
    assert_eq!((view.room, view.free()), (gib(6), gib(2)));
    m.keeper.link_down(session);
    assert_eq!(m.keeper.credit(), Amounts::ZERO);
    assert_eq!(m.keeper.view(m.local).room, gib(2));
}

/// AC-T7.4.1, CV12: a sample refuses a synced holder's scope as not its user's; expect no credit, the keeper's next
/// hints to close that holder's session typed (Close::Scope(NotOwned)), and the holder's next hello to name no scope.
#[test]
fn cv12_a_refused_scope_closes_its_session_typed_and_the_holder_drops_it() {
    let mut m = Machine::new(gib(8));
    let mut kiln = holder("kiln-a", 1).with_scope(Scope::Group(4242));
    m.join(&mut kiln);
    let (run, _) = m.reserve(&mut kiln, gib(2));
    let session = scoped_session(&m, "kiln-a").unwrap();
    m.keeper.set_room(gib(6));
    m.keeper.sampled(&[(session, Err(ScopeRefused::NotOwned))]);
    assert_eq!(m.keeper.credit(), Amounts::ZERO);
    let hints = m.keeper.hints(m.local);
    assert!(
        matches!(
            hints.as_slice(),
            [(s, ToHolder::Closed { why: Close::Scope(ScopeRefused::NotOwned), .. })] if *s == session
        ),
        "{hints:?}"
    );
    let mut out = Out::default();
    for (_, msg) in hints {
        kiln.hear(msg, &mut out);
    }
    assert_eq!(
        out.events,
        vec![Event::Closed(Close::Scope(ScopeRefused::NotOwned))]
    );
    assert!(kiln.may_run(run), "a closed session keeps what it holds");
    assert!(m.keeper.scopes().is_empty());
    assert!(matches!(
        kiln.connect(),
        Some(ToKeeper::Hello { scope: None, .. })
    ));
}
