//! The capacity view's contract as cases every implementation passes (CLAUDE.md §9): the simulator in
//! this crate's tests, Hex's node in its own, and any keeper kiln hosts. A case drives the view only
//! through its links ([`CapacityView`]) and its host ([`Host`]); each reads "do X, expect Y", cites the
//! decision it holds (`docs/design/capacity-view.md`), and [`view`] returns the cases that failed and why.

use super::{
    Amounts, Close, Fence, Held, Host, ReservationId, Scope, ToHolder, ToKeeper, View, Why,
};
use crate::Refusal;

/// One part of the contract.
pub struct Case {
    pub id: &'static str,
    pub says: &'static str,
    run: fn(&mut dyn Host) -> Result<(), String>,
}

/// A case that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub case: &'static str,
    pub why: String,
}

/// The room every case's machine starts with: eight CPUs, memory and disk enough for any case's asks.
pub const ROOM: Amounts = Amounts {
    cpu_milli: 8_000,
    mem_bytes: 1 << 34,
    disk_bytes: 1 << 34,
    gpus: 1,
};

pub const CASES: &[Case] = &[
    Case {
        id: "HEX-CV-1",
        says: "CV3: a reserve is granted only where it fits the room beside the host's own admissions and every holder's; a refusal says Room with the view",
        run: fits,
    },
    Case {
        id: "HEX-CV-2",
        says: "CV4: a hello under an older epoch is closed; a newer one closes the older process's session, and nothing said under a closed session changes the books",
        run: fences,
    },
    Case {
        id: "HEX-CV-3",
        says: "CV5: a dropped link releases nothing; the rejoin's sync releases what it doesn't list, adopts what the keeper lacks, and says the counts",
        run: syncs,
    },
    Case {
        id: "HEX-CV-4",
        says: "CV2: a keeper restarted from what it kept holds every reservation it granted, closes every earlier session, and agrees with its holder after the rejoin",
        run: restarts,
    },
    Case {
        id: "HEX-CV-5",
        says: "CV5: releasing a reservation twice, or one never held, answers Released and frees it once",
        run: releases,
    },
    Case {
        id: "HEX-CV-6",
        says: "CV7: a change another holder or the host makes owes every other synced holder one hint with the view as it is",
        run: hints,
    },
    Case {
        id: "HEX-CV-7",
        says: "CV3: a room measured below what is held admits nothing until a release makes the reserve fit",
        run: over,
    },
    Case {
        id: "HEX-CV-8",
        says: "CV3, CV8: GPUs, nothing, a reserve before the sync, and a name not visible ASCII are refused, typed",
        run: refusals,
    },
    Case {
        id: "HEX-CV-9",
        says: "CV12, withdrawn: a hello naming its work's scope is served as one naming none, and the room is the measured room alone: a holder's use is never credited",
        run: scope_earns_nothing,
    },
    Case {
        id: "HEX-CV-10",
        says: "CV4: a link speaks for its own session alone: a fence of another link's session is closed as no session, and changes nothing",
        run: own_session,
    },
];

/// Runs every case on a fresh machine from `make`; returns the failures.
pub fn view(make: &mut dyn FnMut() -> Result<Box<dyn Host>, Refusal>) -> Vec<Failure> {
    CASES
        .iter()
        .filter_map(|case| {
            let result = make()
                .map_err(|e| format!("making the view: {e}"))
                .and_then(|mut host| (case.run)(host.as_mut()));
            result.err().map(|why| Failure { case: case.id, why })
        })
        .collect()
}

fn ensure(ok: bool, why: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(why()) }
}

fn cpu(milli: u64) -> Amounts {
    Amounts {
        cpu_milli: milli,
        ..Amounts::ZERO
    }
}

fn hear(host: &mut dyn Host, link: u64, msg: ToKeeper) -> Result<ToHolder, String> {
    host.hear(link, msg).map_err(|e| format!("hear: {e}"))
}

/// Opens a synced session for `name` on `link`: its fence.
fn joined(host: &mut dyn Host, link: u64, name: &str, epoch: u64) -> Result<Fence, String> {
    let hello = ToKeeper::Hello {
        holder: name.to_owned(),
        epoch,
        scope: None,
    };
    let ToHolder::Welcome { fence, .. } = hear(host, link, hello)? else {
        return Err(format!("{name}: no welcome"));
    };
    match hear(
        host,
        link,
        ToKeeper::Sync {
            fence,
            held: Vec::new(),
        },
    )? {
        ToHolder::Synced { .. } => Ok(fence),
        other => Err(format!("{name}: sync answered {other:?}")),
    }
}

fn id(epoch: u64, seq: u64) -> ReservationId {
    ReservationId { epoch, seq }
}

fn reserve(
    host: &mut dyn Host,
    link: u64,
    fence: Fence,
    id: ReservationId,
    amount: Amounts,
) -> Result<ToHolder, String> {
    hear(host, link, ToKeeper::Reserve { fence, id, amount })
}

fn granted(answer: &ToHolder) -> bool {
    matches!(answer, ToHolder::Granted { .. })
}

fn refused(answer: &ToHolder) -> Option<(Why, View)> {
    match answer {
        ToHolder::Refused { why, view, .. } => Some((*why, *view)),
        _ => None,
    }
}

fn synced(answer: &ToHolder) -> Option<(u64, Amounts, u64, u64)> {
    match answer {
        ToHolder::Synced {
            count,
            sum,
            released,
            adopted,
            ..
        } => Some((*count, *sum, *released, *adopted)),
        _ => None,
    }
}

fn fits(host: &mut dyn Host) -> Result<(), String> {
    host.set_local(cpu(3_000)).map_err(|e| e.to_string())?;
    let a = joined(host, 1, "conformance-a", 1)?;
    let b = joined(host, 2, "conformance-b", 1)?;
    let first = reserve(host, 1, a, id(1, 0), cpu(4_000))?;
    ensure(granted(&first), || {
        format!("4 of the 5 CPUs left: {first:?}")
    })?;
    let past = reserve(host, 2, b, id(1, 0), cpu(2_000))?;
    let room = View {
        room: ROOM,
        held: cpu(7_000),
    };
    ensure(refused(&past) == Some((Why::Room, room)), || {
        format!("2 CPUs past the 1 left: {past:?}")
    })?;
    let last = reserve(host, 2, b, id(1, 1), cpu(1_000))?;
    ensure(granted(&last), || format!("the 1 CPU left: {last:?}"))
}

fn fences(host: &mut dyn Host) -> Result<(), String> {
    let old = joined(host, 1, "conformance-a", 5)?;
    let first = reserve(host, 1, old, id(5, 0), cpu(1_000))?;
    ensure(granted(&first), || format!("first reserve: {first:?}"))?;
    let new = ToKeeper::Hello {
        holder: "conformance-a".to_owned(),
        epoch: 6,
        scope: None,
    };
    let ToHolder::Welcome { fence: newer, .. } = hear(host, 2, new)? else {
        return Err("the newer process got no welcome".to_owned());
    };
    let listed = vec![Held {
        id: id(5, 0),
        amount: cpu(1_000),
    }];
    let answer = hear(
        host,
        2,
        ToKeeper::Sync {
            fence: newer,
            held: listed,
        },
    )?;
    ensure(synced(&answer).is_some(), || {
        format!("newer sync: {answer:?}")
    })?;
    let late = hear(
        host,
        1,
        ToKeeper::Release {
            fence: old,
            id: id(5, 0),
        },
    )?;
    ensure(
        matches!(
            late,
            ToHolder::Closed {
                why: Close::Session,
                ..
            }
        ),
        || format!("the older process's release: {late:?}"),
    )?;
    let older = ToKeeper::Hello {
        holder: "conformance-a".to_owned(),
        epoch: 5,
        scope: None,
    };
    let answer = hear(host, 1, older)?;
    ensure(
        answer
            == ToHolder::Closed {
                rejected: None,
                why: Close::OlderEpoch { current: 6 },
            },
        || format!("the older process's hello: {answer:?}"),
    )?;
    let resync = hear(
        host,
        2,
        ToKeeper::Sync {
            fence: newer,
            held: vec![Held {
                id: id(5, 0),
                amount: cpu(1_000),
            }],
        },
    )?;
    ensure(
        synced(&resync).map(|(count, sum, _, _)| (count, sum)) == Some((1, cpu(1_000))),
        || format!("the reservation kept through the older process's words: {resync:?}"),
    )
}

fn syncs(host: &mut dyn Host) -> Result<(), String> {
    let a = joined(host, 1, "conformance-a", 1)?;
    for seq in 0..2 {
        let answer = reserve(host, 1, a, id(1, seq), cpu(1_000))?;
        ensure(granted(&answer), || format!("reserve {seq}: {answer:?}"))?;
    }
    host.link_down(1).map_err(|e| e.to_string())?;
    let b = joined(host, 2, "conformance-b", 1)?;
    let held = reserve(host, 2, b, id(1, 0), cpu(7_000))?;
    ensure(
        refused(&held).map(|(why, _)| why) == Some(Why::Room),
        || format!("a dropped link released something: {held:?}"),
    )?;
    let hello = ToKeeper::Hello {
        holder: "conformance-a".to_owned(),
        epoch: 1,
        scope: None,
    };
    let ToHolder::Welcome { fence, .. } = hear(host, 3, hello)? else {
        return Err("no welcome on the rejoin".to_owned());
    };
    let listed = vec![
        Held {
            id: id(1, 0),
            amount: cpu(1_000),
        },
        Held {
            id: id(0, 9),
            amount: cpu(500),
        },
    ];
    let answer = hear(
        host,
        3,
        ToKeeper::Sync {
            fence,
            held: listed,
        },
    )?;
    ensure(synced(&answer) == Some((2, cpu(1_500), 1, 1)), || {
        format!("released 1, adopted 1, holding 2 of 1.5 CPUs: {answer:?}")
    })
}

fn restarts(host: &mut dyn Host) -> Result<(), String> {
    let a = joined(host, 1, "conformance-a", 1)?;
    for seq in 0..3 {
        let answer = reserve(host, 1, a, id(1, seq), cpu(1_000))?;
        ensure(granted(&answer), || format!("reserve {seq}: {answer:?}"))?;
    }
    let released = hear(
        host,
        1,
        ToKeeper::Release {
            fence: a,
            id: id(1, 1),
        },
    )?;
    ensure(matches!(released, ToHolder::Released { .. }), || {
        format!("release: {released:?}")
    })?;
    host.restart().map_err(|e| format!("restart: {e}"))?;
    let stale = hear(
        host,
        1,
        ToKeeper::Release {
            fence: a,
            id: id(1, 0),
        },
    )?;
    ensure(
        matches!(
            stale,
            ToHolder::Closed {
                why: Close::Session,
                ..
            }
        ),
        || format!("a session from before the restart: {stale:?}"),
    )?;
    let b = joined(host, 2, "conformance-b", 1)?;
    let probe = reserve(host, 2, b, id(1, 0), cpu(6_001))?;
    ensure(
        refused(&probe).map(|(why, _)| why) == Some(Why::Room),
        || format!("the restarted keeper forgot a grant: {probe:?}"),
    )?;
    let hello = ToKeeper::Hello {
        holder: "conformance-a".to_owned(),
        epoch: 1,
        scope: None,
    };
    let ToHolder::Welcome { fence, .. } = hear(host, 3, hello)? else {
        return Err("no welcome after the restart".to_owned());
    };
    let listed = [0, 2]
        .into_iter()
        .map(|seq| Held {
            id: id(1, seq),
            amount: cpu(1_000),
        })
        .collect();
    let answer = hear(
        host,
        3,
        ToKeeper::Sync {
            fence,
            held: listed,
        },
    )?;
    ensure(synced(&answer) == Some((2, cpu(2_000), 0, 0)), || {
        format!("the books after the restart: {answer:?}")
    })
}

fn releases(host: &mut dyn Host) -> Result<(), String> {
    let a = joined(host, 1, "conformance-a", 1)?;
    let answer = reserve(host, 1, a, id(1, 0), cpu(8_000))?;
    ensure(granted(&answer), || format!("reserve: {answer:?}"))?;
    for which in [id(1, 0), id(1, 0), id(1, 7)] {
        let answer = hear(
            host,
            1,
            ToKeeper::Release {
                fence: a,
                id: which,
            },
        )?;
        ensure(matches!(answer, ToHolder::Released { .. }), || {
            format!("release {which:?}: {answer:?}")
        })?;
    }
    let again = reserve(host, 1, a, id(1, 1), cpu(8_000))?;
    ensure(granted(&again), || {
        format!("the room freed once: {again:?}")
    })
}

fn hints(host: &mut dyn Host) -> Result<(), String> {
    let a = joined(host, 1, "conformance-a", 1)?;
    joined(host, 2, "conformance-b", 1)?;
    for seq in 0..2 {
        reserve(host, 1, a, id(1, seq), cpu(1_000))?;
    }
    let hints = host.hints().map_err(|e| e.to_string())?;
    ensure(
        matches!(hints.as_slice(), [(2, ToHolder::Hint { view, .. })] if view.held == cpu(2_000)),
        || format!("one hint to the other holder, the latest view: {hints:?}"),
    )?;
    let none = host.hints().map_err(|e| e.to_string())?;
    ensure(none.is_empty(), || format!("hints said twice: {none:?}"))?;
    host.set_local(cpu(500)).map_err(|e| e.to_string())?;
    let both = host.hints().map_err(|e| e.to_string())?;
    ensure(both.len() == 2, || {
        format!("the host's change to both: {both:?}")
    })
}

fn over(host: &mut dyn Host) -> Result<(), String> {
    let a = joined(host, 1, "conformance-a", 1)?;
    for seq in 0..2 {
        reserve(host, 1, a, id(1, seq), cpu(3_000))?;
    }
    host.set_room(Amounts {
        cpu_milli: 5_000,
        ..ROOM
    })
    .map_err(|e| e.to_string())?;
    let small = reserve(host, 1, a, id(1, 2), cpu(1))?;
    ensure(
        refused(&small).map(|(why, _)| why) == Some(Why::Room),
        || format!("admitted while over: {small:?}"),
    )?;
    hear(
        host,
        1,
        ToKeeper::Release {
            fence: a,
            id: id(1, 1),
        },
    )?;
    let fits = reserve(host, 1, a, id(1, 3), cpu(2_000))?;
    ensure(granted(&fits), || {
        format!("admitted once it fits: {fits:?}")
    })
}

fn refusals(host: &mut dyn Host) -> Result<(), String> {
    let hello = ToKeeper::Hello {
        holder: "has space".to_owned(),
        epoch: 1,
        scope: None,
    };
    let answer = hear(host, 1, hello)?;
    ensure(
        matches!(answer, ToHolder::Closed { rejected: None, .. }),
        || format!("a name with a space: {answer:?}"),
    )?;
    let hello = ToKeeper::Hello {
        holder: "conformance-a".to_owned(),
        epoch: 1,
        scope: None,
    };
    let ToHolder::Welcome { fence, .. } = hear(host, 2, hello)? else {
        return Err("no welcome".to_owned());
    };
    let early = reserve(host, 2, fence, id(1, 0), cpu(1))?;
    ensure(
        matches!(
            early,
            ToHolder::Closed {
                why: Close::NotSynced,
                ..
            }
        ),
        || format!("a reserve before the sync: {early:?}"),
    )?;
    let a = joined(host, 3, "conformance-a", 1)?;
    let gpu = reserve(host, 3, a, id(1, 0), Amounts { gpus: 1, ..cpu(1) })?;
    ensure(
        refused(&gpu).map(|(why, _)| why) == Some(Why::Devices),
        || format!("a GPU by count: {gpu:?}"),
    )?;
    let empty = reserve(host, 3, a, id(1, 1), Amounts::ZERO)?;
    ensure(
        refused(&empty).map(|(why, _)| why) == Some(Why::Empty),
        || format!("nothing: {empty:?}"),
    )
}

fn gib(n: u64) -> Amounts {
    Amounts {
        mem_bytes: n << 30,
        ..Amounts::ZERO
    }
}

/// A synced session on `link` for a holder whose hello names `scope`: its fence.
fn scoped(host: &mut dyn Host, link: u64, scope: Scope) -> Result<Fence, String> {
    let hello = ToKeeper::Hello {
        holder: "conformance-a".to_owned(),
        epoch: 1,
        scope: Some(scope),
    };
    let ToHolder::Welcome { fence, .. } = hear(host, link, hello)? else {
        return Err("no welcome".to_owned());
    };
    let synced = hear(
        host,
        link,
        ToKeeper::Sync {
            fence,
            held: Vec::new(),
        },
    )?;
    ensure(matches!(synced, ToHolder::Synced { .. }), || {
        format!("sync answered {synced:?}")
    })?;
    Ok(fence)
}

/// HEX-CV-9: a holder whose hello names a cgroup reserves 4 GiB, and the host measures the room 3 GiB short (its work's use,
/// missing from free memory); expect the hello welcomed, a reserve of all the measured room leaves granted, and a GiB more
/// refused for room in the measured room: its use counted twice, never credited (CV12, withdrawn).
fn scope_earns_nothing(host: &mut dyn Host) -> Result<(), String> {
    let fence = scoped(host, 1, Scope::Cgroup("conformance/a".to_owned()))?;
    ensure(granted(&reserve(host, 1, fence, id(1, 0), gib(4))?), || {
        "4 GiB refused".to_owned()
    })?;
    let measured = ROOM.less(gib(3));
    host.set_room(measured)
        .map_err(|e| format!("set_room: {e}"))?;
    let rest = Amounts {
        mem_bytes: measured.mem_bytes.saturating_sub(4 << 30),
        ..Amounts::ZERO
    };
    ensure(granted(&reserve(host, 1, fence, id(1, 1), rest)?), || {
        "all the measured room leaves refused".to_owned()
    })?;
    match reserve(host, 1, fence, id(1, 2), gib(1))? {
        ToHolder::Refused {
            why: Why::Room,
            view,
            ..
        } if view.room == measured => Ok(()),
        other => Err(format!("a GiB past the measured room: {other:?}")),
    }
}

/// HEX-CV-10: two holders joined on links 1 and 2; link 2 reserves under link 1's fence, then link 1 reserves the whole
/// room; expect link 2's word closed as no session (naming the fence it said) and link 1's reserve granted.
fn own_session(host: &mut dyn Host) -> Result<(), String> {
    let first = joined(host, 1, "conformance-a", 1)?;
    joined(host, 2, "conformance-b", 1)?;
    match reserve(host, 2, first, id(1, 0), cpu(1000))? {
        ToHolder::Closed {
            rejected: Some(fence),
            why: Close::Session,
        } if fence == first => {}
        other => return Err(format!("another link's fence: {other:?}")),
    }
    let whole = Amounts {
        cpu_milli: ROOM.cpu_milli,
        ..Amounts::ZERO
    };
    ensure(granted(&reserve(host, 1, first, id(1, 0), whole)?), || {
        "the whole room refused after another link said its fence".to_owned()
    })
}
