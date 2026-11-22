//! Follower reads at a closed time (replication.md, "Follower reads at a closed time"; CP-T1.10,
//! AC-T1.10.C1 and C2, T-T1.10.C1 and C2), on the driven shell: reads under leader changes, and a
//! read that waits for its time.

use super::*;
use crate::replication::closed;

/// An hour of a wall clock, in the stamps' microseconds: a member's clock set this far behind
/// another's stands for a machine whose clock is wrong by that much.
const HOUR: u64 = 3_600_000_000;

/// A follower read through `client`: the values of member_items its replica holds at its closed
/// time once that is at least `at_least`, and that time.
fn read_at(client: &Client, at_least: u64, until: Instant) -> Result<(Vec<u8>, u64), Refusal> {
    client.read_closed(at_least, until, |db| {
        db.query_transaction(|db| match closed::recorded(db)? {
            Some(time) if time >= at_least => Ok(Ok((values(db)?, time))),
            found => Ok(Err(found)),
        })
    })
}

/// Every member's follower read now, recorded.
fn read_all(clients: &[Client], reads: &mut Vec<(u64, Vec<u8>)>) {
    for client in clients {
        let (seen, time) = read_at(client, 0, Instant::now() + HANG_READ).unwrap();
        reads.push((time, seen));
    }
}

/// T-T1.10.C1 (AC-T1.10.C1, C2): reads under leader changes. Do: three voters, each member's wall
/// clock an hour behind the one before it; the lead goes from member 1 to 2, to 3 and back to 1,
/// each leader writing the next three values; after each write every member reads at its closed
/// time, once before catching up and once after. Expect: no read sees a version past its
/// timestamp, nor misses one at or before it: every read sees 1..=k for some k, and a read at a
/// later time never sees less (at the same time, the same). The lifted stamps are counted, so
/// the leaders behind their predecessors' clocks were tested: with an entry's time its stamp
/// alone, member 2's writes would take times an hour before member 1's, and a read of them
/// would see more than a read at a later time.
#[test]
fn no_read_under_leader_changes_sees_a_version_past_its_time() {
    let dir = tempfile::tempdir().unwrap();
    let mut driven = Driven::new();
    let clients: Vec<Client> = (1..=3)
        .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
        .collect();
    let wall = driven.owner(0).wall;
    for (owner, behind) in [(1, HOUR), (2, 2 * HOUR)] {
        driven.owner_mut(owner).wall = wall - behind;
    }
    let mut pump = Pump::new(&clients);
    let mut leader = driven.elect(&mut pump, &clients, 0, 0);
    assert_eq!(leader, 0);
    let mut reads = Vec::new();
    let mut value = 0u8;
    for next in [1usize, 2, 0, usize::MAX] {
        for _ in 0..3 {
            value += 1;
            write(&mut driven, &mut pump, clients.get(leader).unwrap(), value);
            read_all(&clients, &mut reads);
            for owner in 0..clients.len() {
                catch_up(&mut driven, &mut pump, &clients, leader, owner);
            }
            read_all(&clients, &mut reads);
        }
        if next != usize::MAX {
            driven.transfer(&mut pump, &clients, leader, next);
            leader = next;
        }
    }
    driven.drain(&mut pump);
    pump.finish();

    for (time, seen) in &reads {
        let k = u8::try_from(seen.len()).unwrap();
        assert_eq!(
            *seen,
            (1..=k).collect::<Vec<u8>>(),
            "a read at {time} saw a gap"
        );
    }
    for (a, seen_a) in &reads {
        for (b, seen_b) in &reads {
            if a < b {
                assert!(
                    seen_a.len() <= seen_b.len(),
                    "a read at {a} saw {seen_a:?}, one at the later {b} saw {seen_b:?}"
                );
            } else if a == b {
                assert_eq!(seen_a, seen_b, "two reads at {a} saw different states");
            }
        }
    }
    assert!(
        reads
            .iter()
            .any(|(_, seen)| seen.len() == usize::from(value)),
        "no read saw every write"
    );
    let lifted: u64 = (0..clients.len())
        .map(|owner| driven.owner(owner).replica.machine().lifted())
        .sum();
    assert!(
        lifted > 0,
        "no leader's stamp was behind its predecessor's time"
    );
}

/// T-T1.10.C2 (AC-T1.10.C2, C3): a read that waits for its time. Do: three voters, 1 written and
/// caught up; a follower read asked at a time past the follower's closed time, on a thread; then
/// 2 written and caught up. Then a read asked an hour past the wall clock, with a short deadline.
/// Expect: the first read is answered once 2 is applied, at a time at least the one it asked,
/// seeing 1 and 2; the second is refused Timeout at its deadline. The follower has measured a lag.
#[test]
fn a_follower_read_waits_for_its_time_and_is_refused_past_its_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let mut driven = Driven::new();
    let clients: Vec<Client> = (1..=3)
        .map(|n| driven.open(dir.path(), n, vec![1, 2, 3], None))
        .collect();
    let mut pump = Pump::new(&clients);
    let leader = driven.elect(&mut pump, &clients, 0, 0);
    let follower = (leader + 1) % clients.len();
    write(&mut driven, &mut pump, clients.get(leader).unwrap(), 1);
    catch_up(&mut driven, &mut pump, &clients, leader, follower);
    let (seen, time) = read_at(
        clients.get(follower).unwrap(),
        0,
        Instant::now() + HANG_READ,
    )
    .unwrap();
    assert_eq!(seen, vec![1]);
    let asked = time + 1;
    let reader = clients.get(follower).unwrap().clone();
    let waiting = std::thread::spawn(move || read_at(&reader, asked, Instant::now() + HANG_READ));
    write(&mut driven, &mut pump, clients.get(leader).unwrap(), 2);
    catch_up(&mut driven, &mut pump, &clients, leader, follower);
    let (seen, answered) = waiting.join().unwrap().unwrap();
    assert_eq!(seen, vec![1, 2]);
    assert!(
        answered >= asked,
        "answered at {answered}, asked at {asked}"
    );
    assert!(
        clients
            .get(follower)
            .unwrap()
            .closed_lag()
            .unwrap()
            .is_some(),
        "the follower measured no lag"
    );
    let ahead = super::wall_micros().unwrap() + HOUR;
    assert_eq!(
        read_at(
            clients.get(follower).unwrap(),
            ahead,
            Instant::now() + Duration::from_millis(50)
        ),
        Err(Refusal::Timeout)
    );
    driven.drain(&mut pump);
    pump.finish();
}
