//! The capacity view's messages on their lane (joint plan step 13, `docs/design/capacity-view.md` CV9): each
//! [`ToKeeper`] and [`ToHolder`] one JSON message framed by its length ([`stream::frame`]), at most
//! [`MESSAGE_BOUND`] bytes, the wire's, on a holder's connection to the keeper's port
//! ([`crate::lanes::CAPACITY_PORT`]). Pure: the lane's shell (`hex-quic`'s `capacity`) reads and writes the bytes.

use hex_api::capacity::{Amounts, Fence, Held, ReservationId, ToKeeper};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::stream::{self, Deframer};
use crate::wire::{self, Fault};

/// One message's bound: the wire's.
pub const MESSAGE_BOUND: usize = wire::MAX_MESSAGE;

fn json(why: serde_json::Error) -> Fault {
    Fault::value("a capacity message", why.to_string())
}

/// `msg` as its lane carries it: its JSON, framed; refused past the bound.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, Fault> {
    let bytes = serde_json::to_vec(msg).map_err(json)?;
    stream::frame(&bytes, MESSAGE_BOUND)
}

/// A whole message's bytes, as the lane's [`deframer`] gave them.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Fault> {
    if bytes.len() > MESSAGE_BOUND {
        return Err(Fault::Oversized {
            bytes: bytes.len(),
            bound: MESSAGE_BOUND,
        });
    }
    serde_json::from_slice(bytes).map_err(json)
}

/// A reader of the lane's messages as their bytes come.
pub fn deframer() -> Deframer {
    Deframer::new(MESSAGE_BOUND)
}

/// The most reservations one sync lists within the bound: what one message holds of them, each at its widest
/// (every number `u64::MAX`), beside the sync's own fields at theirs. A keeper holds a holder to it (CV8), so
/// every sync fits one message.
pub fn most_held() -> Result<usize, Fault> {
    let widest = Held {
        id: ReservationId {
            epoch: u64::MAX,
            seq: u64::MAX,
        },
        amount: Amounts::from_array([u64::MAX; 4]),
    };
    let fence = Fence {
        keeper: u64::MAX,
        session: u64::MAX,
        holder: u64::MAX,
    };
    let empty = serde_json::to_vec(&ToKeeper::Sync {
        fence,
        held: Vec::new(),
    })
    .map_err(json)?
    .len();
    let one = serde_json::to_vec(&widest).map_err(json)?.len();
    // n entries take n of `one` and the n - 1 commas between them: n * (one + 1) - 1 bytes beside the sync's own, which
    // fit the bound's room past those while n * (one + 1) <= room + 1. The frame's prefix is outside the bound.
    MESSAGE_BOUND
        .checked_sub(empty)
        .and_then(|room| room.checked_add(1))
        .zip(one.checked_add(1))
        .and_then(|(room, each)| room.checked_div(each))
        .ok_or_else(|| Fault::value("a capacity sync", "its own fields pass the message bound"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_api::capacity::{Close, ToHolder, View, Why};

    /// A framed message read back as its lane's reader takes it, in as many takes as it needs.
    fn whole(framed: &[u8]) -> Vec<u8> {
        let mut deframer = deframer();
        let mut input = framed;
        loop {
            let (taken, message) = deframer.take(input).unwrap();
            input = &input[taken..];
            if let Some(message) = message {
                assert!(input.is_empty());
                return message;
            }
        }
    }

    /// AC-T7.4.1, CV9: frame a sync, a reserve and each answer, and read them back; expect each the same.
    #[test]
    fn capacity_messages_round_trip_through_their_frames() {
        let fence = Fence {
            keeper: 1,
            session: 2,
            holder: 3,
        };
        let id = ReservationId { epoch: 3, seq: 4 };
        let amount = Amounts::from_array([1000, 1 << 30, 0, 0]);
        let to_keeper = [
            ToKeeper::Hello {
                holder: "kiln-runner".to_owned(),
                epoch: 3,
                scope: None,
            },
            ToKeeper::Sync {
                fence,
                held: vec![Held { id, amount }],
            },
            ToKeeper::Reserve { fence, id, amount },
            ToKeeper::Release { fence, id },
        ];
        for msg in to_keeper {
            let back: ToKeeper = decode(&whole(&encode(&msg).unwrap())).unwrap();
            assert_eq!(back, msg);
        }
        let view = View {
            room: amount,
            held: Amounts::ZERO,
        };
        let to_holder = [
            ToHolder::Refused {
                fence,
                id,
                why: Why::Room,
                view,
            },
            ToHolder::Closed {
                rejected: None,
                why: Close::OlderEpoch { current: 9 },
            },
        ];
        for msg in to_holder {
            let back: ToHolder = decode(&whole(&encode(&msg).unwrap())).unwrap();
            assert_eq!(back, msg);
        }
    }

    /// AC-T7.4.1, CV8 (R03: the bound pinned at its edge): list the most reservations a sync may, each at its widest;
    /// expect the sync to fit one message, and one more not to.
    #[test]
    fn the_most_held_fit_one_message() {
        let most = most_held().unwrap();
        assert!(most > 0);
        let widest = |seq| Held {
            id: ReservationId {
                epoch: u64::MAX,
                seq,
            },
            amount: Amounts::from_array([u64::MAX; 4]),
        };
        let fence = Fence {
            keeper: u64::MAX,
            session: u64::MAX,
            holder: u64::MAX,
        };
        let sync = |n: usize| ToKeeper::Sync {
            fence,
            held: (0..n).map(|_| widest(u64::MAX)).collect(),
        };
        assert!(encode(&sync(most)).is_ok());
        assert!(matches!(
            encode(&sync(most + 1)),
            Err(Fault::Oversized { .. })
        ));
    }

    /// AC-T7.4.1, CLAUDE.md §6: read hostile bytes as a message: garbage, an unknown kind, a field of the
    /// wrong type, a deep nesting, a message past the bound; expect each refused, typed, never a panic.
    #[test]
    fn hostile_capacity_messages_are_refused_typed() {
        let deep = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
        let cases: [&[u8]; 5] = [
            b"\xff\xfe garbage",
            br#"{"Unknown":{}}"#,
            br#"{"Hello":{"holder":7,"epoch":"x"}}"#,
            deep.as_bytes(),
            br#"{"Reserve":{"fence":{"keeper":1,"session":2,"holder":3}}}"#,
        ];
        for bytes in cases {
            assert!(decode::<ToKeeper>(bytes).is_err(), "{bytes:?}");
        }
        let past = vec![b' '; MESSAGE_BOUND + 1];
        assert!(matches!(
            decode::<ToKeeper>(&past),
            Err(Fault::Oversized { .. })
        ));
    }
}

#[cfg(test)]
mod contract_tests {
    use hex_api::capacity::{
        Close, Fence, Invalid, ReservationId, ScopeRefused, ToHolder, ToKeeper, View, Why,
    };

    use crate::lanes::CAPACITY_PORT;

    /// The section of capacity-view.md that says what kiln's runner implements as a holder.
    fn contract() -> String {
        let doc = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/design/capacity-view.md"
        ))
        .unwrap();
        let (_, rest) = doc
            .split_once("## What kiln's runner implements as a holder on `CAPACITY_PORT`")
            .expect("capacity-view.md has no holder contract");
        rest.split("\n## ").next().unwrap_or_default().to_owned()
    }

    /// Each word's name, by a match the compiler holds to every variant: one more is a doc to write.
    fn keeper_words() -> Vec<&'static str> {
        let fence = Fence {
            keeper: 0,
            session: 0,
            holder: 0,
        };
        let id = ReservationId { epoch: 0, seq: 0 };
        [
            ToKeeper::Hello {
                holder: String::new(),
                epoch: 0,
                scope: None,
            },
            ToKeeper::Sync {
                fence,
                held: Vec::new(),
            },
            ToKeeper::Reserve {
                fence,
                id,
                amount: hex_api::capacity::Amounts::ZERO,
            },
            ToKeeper::Release { fence, id },
        ]
        .iter()
        .map(|word| match word {
            ToKeeper::Hello { .. } => "Hello",
            ToKeeper::Sync { .. } => "Sync",
            ToKeeper::Reserve { .. } => "Reserve",
            ToKeeper::Release { .. } => "Release",
        })
        .collect()
    }

    fn holder_words() -> Vec<&'static str> {
        let fence = Fence {
            keeper: 0,
            session: 0,
            holder: 0,
        };
        let id = ReservationId { epoch: 0, seq: 0 };
        let view = View::default();
        let zero = hex_api::capacity::Amounts::ZERO;
        [
            ToHolder::Welcome { fence, view },
            ToHolder::Synced {
                fence,
                view,
                count: 0,
                sum: zero,
                released: 0,
                adopted: 0,
                disagreed: 0,
            },
            ToHolder::Granted {
                fence,
                id,
                amount: zero,
                view,
            },
            ToHolder::Refused {
                fence,
                id,
                why: Why::Room,
                view,
            },
            ToHolder::Released { fence, id, view },
            ToHolder::Hint { fence, view },
            ToHolder::Closed {
                rejected: None,
                why: Close::Session,
            },
        ]
        .iter()
        .map(|word| match word {
            ToHolder::Welcome { .. } => "Welcome",
            ToHolder::Synced { .. } => "Synced",
            ToHolder::Granted { .. } => "Granted",
            ToHolder::Refused { .. } => "Refused",
            ToHolder::Released { .. } => "Released",
            ToHolder::Hint { .. } => "Hint",
            ToHolder::Closed { .. } => "Closed",
        })
        .collect()
    }

    fn closes() -> Vec<&'static str> {
        [
            Close::OlderEpoch { current: 0 },
            Close::Session,
            Close::NotSynced,
            Close::HoldersFull { bound: 0 },
            Close::Invalid(Invalid::Name),
            Close::Scope(ScopeRefused::NotOwned),
            Close::NotAuthorized,
        ]
        .iter()
        .map(|why| match why {
            Close::OlderEpoch { .. } => "OlderEpoch",
            Close::Session => "Session",
            Close::NotSynced => "NotSynced",
            Close::HoldersFull { .. } => "HoldersFull",
            Close::Invalid(_) => "Invalid",
            Close::Scope(_) => "Scope",
            Close::NotAuthorized => "NotAuthorized",
        })
        .collect()
    }

    fn refusals() -> Vec<&'static str> {
        [
            Why::Room,
            Why::Full { bound: 0 },
            Why::Empty,
            Why::Devices,
            Why::Disagrees,
        ]
        .iter()
        .map(|why| match why {
            Why::Room => "Room",
            Why::Full { .. } => "Full",
            Why::Empty => "Empty",
            Why::Devices => "Devices",
            Why::Disagrees => "Disagrees",
        })
        .collect()
    }

    /// AC-T7.4.1, CV9 (doc truth): read capacity-view.md's holder contract; expect it to number the port as the code
    /// does and to name every word a holder says, every answer it hears, and every close and refusal it is told.
    #[test]
    fn the_holder_contract_names_the_port_and_every_word() {
        let contract = contract();
        let port = format!(
            "0x{:04x}_{:04x}",
            CAPACITY_PORT >> 16,
            CAPACITY_PORT & 0xffff
        );
        assert!(contract.contains(&port), "the contract numbers no {port}");
        for name in keeper_words()
            .into_iter()
            .chain(holder_words())
            .chain(closes())
            .chain(refusals())
        {
            assert!(
                contract.contains(&format!("`{name}")) || contract.contains(&format!("{name} {{")),
                "the holder contract doesn't name {name}"
            );
        }
    }
}
