//! The group's class, stated once in its log (docs/design/replication.md, "The class stated once
//! per group"; Amphora goal 11209e8d, AC-T1.8.5): a class entry, the record a member keeps of the
//! last compatible one, and the compatibility every member holds itself to.
use crate::db::Db;
use amphora_api::{self as api, Refusal, replication::Durability};

/// A class entry's prefix: a log entry kind of its own, as the seed's and the baseline delta's.
const PREFIX: &[u8] = b"amphora.class.v1\0";
const TABLE: &str = "CREATE TABLE IF NOT EXISTS __amphora_class (id INTEGER PRIMARY KEY CHECK(id = 1), class BLOB NOT NULL)";
/// A class entry's bytes at most: what the largest class a replica takes encodes to. Postcard's
/// wire format (postcard.jamesmunns.com, "Wire Format") writes an enum's tag and a u32 as
/// varints of at most 5 bytes, a length and a u64 of at most 10. So: the prefix, `Replicated`'s
/// tag, the fates' length, a (member, fate) pair of two u64s for each of the most members
/// (`replicated::MEMBERS`), and the span. No class names more: the record is restricted to the
/// configuration, hyper-raft refuses a change whose configuration names more than
/// `limits.members` (raft.rs:3633) and `Fates::validate` holds to the same limit (raft.rs:431),
/// which the owner's settings set to `MEMBERS`.
const ENTRY_BYTES: usize = PREFIX
    .len()
    .saturating_add(5)
    .saturating_add(10)
    .saturating_add(5)
    .saturating_add(crate::database::replicated::MEMBERS.saturating_mul(20));

/// `durability` with its fates in member order: the same class stated in any order is one class.
pub(crate) fn canonical(durability: &Durability) -> Durability {
    match durability {
        Durability::Replicated(fates) => {
            let mut fates = fates.clone();
            fates.of.sort_unstable();
            Durability::Replicated(fates)
        }
        Durability::Persisted => Durability::Persisted,
    }
}

/// Whether two classes can be one group's: the same kind, the same fates to span, and the same
/// fate for every member both name. A member only one names is a joiner's fate added or a
/// departed member's dropped.
pub(crate) fn compatible(a: &Durability, b: &Durability) -> bool {
    match (a, b) {
        (Durability::Persisted, Durability::Persisted) => true,
        (Durability::Replicated(a), Durability::Replicated(b)) => {
            a.spanning == b.spanning
                && a.of.iter().all(|(member, fate)| {
                    b.of.iter()
                        .filter(|(other, _)| other == member)
                        .all(|(_, other)| other == fate)
                })
        }
        _ => false,
    }
}

/// The class a leader proposes: `record` joined with `stated`, the fates of both (they agree
/// where both name a member, as they are compatible), to the members `named`.
pub(crate) fn joined(
    record: Option<&Durability>,
    stated: &Durability,
    named: &[u64],
) -> Durability {
    match (record, stated) {
        (Some(Durability::Replicated(record)), Durability::Replicated(stated)) => {
            let mut of: Vec<(u64, u64)> = record
                .of
                .iter()
                .chain(&stated.of)
                .filter(|(member, _)| named.contains(member))
                .copied()
                .collect();
            of.sort_unstable();
            of.dedup();
            Durability::Replicated(api::replication::Fates {
                of,
                spanning: stated.spanning,
            })
        }
        (None, Durability::Replicated(stated)) => {
            let mut of: Vec<(u64, u64)> = stated
                .of
                .iter()
                .filter(|(member, _)| named.contains(member))
                .copied()
                .collect();
            of.sort_unstable();
            Durability::Replicated(api::replication::Fates {
                of,
                spanning: stated.spanning,
            })
        }
        _ => canonical(stated),
    }
}

/// Whether `record` holds every fate `stated` gives a member of `named`: the same kind and
/// span, and each such fate recorded (replication.md, "The class stated once per group").
pub(crate) fn recorded_all(record: &Durability, stated: &Durability, named: &[u64]) -> bool {
    match (record, stated) {
        (Durability::Persisted, Durability::Persisted) => true,
        (Durability::Replicated(record), Durability::Replicated(stated)) => {
            record.spanning == stated.spanning
                && stated
                    .of
                    .iter()
                    .filter(|(member, _)| named.contains(member))
                    .all(|fate| record.of.contains(fate))
        }
        _ => false,
    }
}

/// The log entry that states `durability` for the group.
pub(crate) fn entry(durability: &Durability) -> Result<Vec<u8>, Refusal> {
    let mut bytes = PREFIX.to_vec();
    bytes.extend(postcard::to_allocvec(&canonical(durability)).map_err(unavailable)?);
    api::bound("class entry", bytes.len(), ENTRY_BYTES)?;
    Ok(bytes)
}

/// Whether a log entry is a class entry.
pub(super) fn is_class(data: &[u8]) -> bool {
    data.starts_with(PREFIX)
}

/// The class a class entry states.
pub(super) fn decode(data: &[u8]) -> Result<Durability, Refusal> {
    api::bound("class entry", data.len(), ENTRY_BYTES)?;
    let payload = data
        .strip_prefix(PREFIX)
        .ok_or_else(|| api::invalid("class entry", "the entry is not a class entry"))?;
    let durability: Durability = postcard::from_bytes(payload).map_err(unavailable)?;
    Ok(canonical(&durability))
}

/// The class this database's group recorded, if any.
pub(super) fn recorded(db: &Db) -> anyhow::Result<Option<Durability>> {
    let mut table = false;
    db.query(
        "SELECT 1 FROM sqlite_schema WHERE name = '__amphora_class'",
        [],
        |_| {
            table = true;
            Ok(false)
        },
    )?;
    if !table {
        return Ok(None);
    }
    let mut class = None;
    db.query(
        "SELECT class FROM __amphora_class WHERE id = 1",
        [],
        |row| {
            let bytes = row
                .get_value(0)
                .to_blob()
                .ok_or_else(|| anyhow::anyhow!("the recorded class is not a blob"))?;
            class = Some(canonical(&postcard::from_bytes::<Durability>(bytes)?));
            Ok(false)
        },
    )?;
    Ok(class)
}

/// Records `durability` as the group's class, in the transaction under way.
pub(super) fn record(db: &mut Db, durability: &Durability) -> anyhow::Result<()> {
    db.exec(TABLE)?;
    let bytes = postcard::to_allocvec(&canonical(durability))?;
    db.query(
        "INSERT INTO __amphora_class(id, class) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET class = excluded.class",
        [amphora_engine::Value::from_slice(&bytes)?],
        |_| Ok(true),
    )
}

fn unavailable(error: impl std::fmt::Display) -> Refusal {
    Refusal::Unavailable(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use amphora_api::replication::Fates;

    fn class(of: Vec<(u64, u64)>, spanning: u32) -> Durability {
        Durability::Replicated(Fates { of, spanning })
    }

    /// AC-T1.8.5 (replication.md, "The class stated once per group", the amendment): which classes
    /// can be one group's, and what a leader proposes. Do: compare classes of each kind, and join
    /// records with stated classes. Expect: a joiner's fate added or a departed member's dropped
    /// compatible; another fate for a member both name, another span or another kind not; the
    /// same fates in another order one class; a join that only grows, to the members named; an
    /// entry read back as the class it states, and anything else refused.
    #[test]
    fn classes_are_compatible_only_on_every_member_both_name() {
        let three = class(vec![(1, 10), (2, 20), (3, 30)], 2);
        assert!(compatible(
            &three,
            &class(vec![(1, 10), (2, 20), (3, 30), (4, 40)], 2)
        ));
        assert!(compatible(&three, &class(vec![(1, 10), (2, 20)], 2)));
        assert!(!compatible(
            &three,
            &class(vec![(1, 10), (2, 10), (3, 30)], 2)
        ));
        assert!(!compatible(
            &three,
            &class(vec![(1, 10), (2, 20), (3, 30)], 3)
        ));
        assert!(!compatible(&three, &Durability::Persisted));
        assert!(!compatible(&Durability::Persisted, &three));
        assert!(compatible(&Durability::Persisted, &Durability::Persisted));
        let shuffled = class(vec![(3, 30), (1, 10), (2, 20)], 2);
        assert_eq!(canonical(&shuffled), three);
        assert_eq!(decode(&entry(&shuffled).unwrap()), Ok(three.clone()));
        let four = class(vec![(1, 10), (2, 20), (3, 30), (4, 40)], 2);
        let two = class(vec![(1, 10), (2, 20)], 2);
        assert_eq!(
            joined(Some(&three), &two, &[1, 2, 3, 4]),
            three,
            "a subset adds nothing"
        );
        assert_eq!(
            joined(Some(&three), &four, &[1, 2, 3, 4]),
            four,
            "a joiner's fate added"
        );
        assert_eq!(
            joined(Some(&three), &four, &[1, 2, 3]),
            three,
            "only members named"
        );
        assert_eq!(
            joined(Some(&four), &three, &[1, 2, 3]),
            three,
            "a departed member dropped"
        );
        assert_eq!(joined(None, &four, &[1, 2, 3]), three);
        assert_eq!(
            joined(None, &Durability::Persisted, &[1]),
            Durability::Persisted
        );
        assert!(decode(b"amphora.seed.v1\0").is_err());
        assert!(decode(PREFIX).is_err());
    }

    /// AC-T1.8.5, T-T1.8.C4 (replication.md, "The class stated once per group"): the record
    /// travels with the state and holds a member to it. Do: a machine stating one class applies
    /// an entry stating it, then one stating another (a member's fate changed); its image
    /// installed on a machine stating the same class and on one stating another; the first
    /// opened again stating the other class. Expect: the second entry inert, the record the
    /// first's; the image's member agreeing with it; the other refused as it installs; the
    /// reopening refused Invalid, naming the class.
    #[test]
    fn a_class_entry_survives_an_image_and_a_restart() {
        use hyper_durable::{EntryRef, StateMachine};
        use hyper_raft::proto::{ConfState, EntryType};
        let dir = tempfile::tempdir().unwrap();
        let conf = ConfState {
            voters: vec![1, 2, 3],
            ..ConfState::default()
        };
        let group = class(vec![(1, 10), (2, 20), (3, 30)], 2);
        let other = class(vec![(1, 10), (2, 10), (3, 30)], 2);
        let open = |name: &str, stated: &Durability| {
            let mut machine =
                super::super::Machine::open(&dir.path().join(name), conf.clone()).unwrap();
            machine.state_class(stated).map(|()| machine)
        };
        let mut first = open("first.db", &group).unwrap();
        assert!(!first.class_agreed());
        for (index, stated) in [(1, &group), (2, &other)] {
            let data = entry(stated).unwrap();
            first
                .apply(
                    &EntryRef {
                        index,
                        term: 1,
                        kind: EntryType::EntryNormal,
                        context: b"class",
                        data: &data,
                    },
                    &mut Vec::new(),
                )
                .unwrap();
            assert!(
                first.class_agreed(),
                "entry {index} took the group's record"
            );
        }
        assert!(first.class_proposal().is_none());

        let mut image = Vec::new();
        let (point, configuration) = first.image(&mut image).unwrap();
        let mut agreeing = open("agreeing.db", &group).unwrap();
        agreeing.install(&image, point, &configuration).unwrap();
        assert!(agreeing.class_agreed());
        let mut differing = open("differing.db", &other).unwrap();
        assert!(differing.install(&image, point, &configuration).is_err());

        drop(first);
        match open("first.db", &other) {
            Err(Refusal::Invalid { what, .. }) => assert_eq!(what, "replicated class"),
            Err(refusal) => panic!("refused, but not Invalid: {refusal:?}"),
            Ok(_) => panic!("a member opened stating another class than its group's"),
        }
        assert!(open("first.db", &group).unwrap().class_agreed());
    }

    /// AC-T1.8.5 (replication.md, "The class stated once per group", the code review at 552c0a1):
    /// a member counts held answers only once every fate it states for a member of its
    /// configuration is the record's. Do: a machine of voters 1 to 3 and learner 4 records the
    /// three voters' fates; it states those and a fate for 4; then an entry recording 4's fate
    /// too. Expect: not agreed while 4's fate is its own alone (another member may state another
    /// fate for 4, and each would count by its own); agreed once the record holds it.
    #[test]
    fn a_fate_not_yet_recorded_opens_no_held_count() {
        use hyper_durable::{EntryRef, StateMachine};
        use hyper_raft::proto::{ConfState, EntryType};
        let dir = tempfile::tempdir().unwrap();
        let conf = ConfState {
            voters: vec![1, 2, 3],
            learners: vec![4],
            ..ConfState::default()
        };
        let three = class(vec![(1, 10), (2, 20), (3, 30)], 2);
        let four = class(vec![(1, 10), (2, 20), (3, 30), (4, 40)], 2);
        let mut machine = super::super::Machine::open(&dir.path().join("state.db"), conf).unwrap();
        machine.state_class(&four).unwrap();
        for (index, recorded, agreed) in [(1, &three, false), (2, &four, true)] {
            let data = entry(recorded).unwrap();
            machine
                .apply(
                    &EntryRef {
                        index,
                        term: 1,
                        kind: EntryType::EntryNormal,
                        context: b"class",
                        data: &data,
                    },
                    &mut Vec::new(),
                )
                .unwrap();
            assert_eq!(machine.class_agreed(), agreed, "after entry {index}");
        }
    }

    /// AC-T1.8.5: a class entry's bound is what the largest class encodes to. Do: encode a class of
    /// the most members a replica takes, every id, fate and span at its type's largest, then one
    /// member more. Expect: the first an entry read back as its class; the one more refused as a
    /// class entry, where a picked 64 KiB took it (2026-11-11: no picked bounds).
    #[test]
    fn a_class_entry_holds_the_most_members_and_no_more() {
        let most = crate::database::replicated::MEMBERS;
        let wide = |members: usize| {
            let of = (0..members)
                .map(|m| (u64::MAX - m as u64, u64::MAX))
                .collect();
            class(of, u32::MAX)
        };
        let full = wide(most);
        assert_eq!(decode(&entry(&full).unwrap()).unwrap(), canonical(&full));
        assert!(entry(&wide(most + 1)).is_err(), "a class past the members");
    }
}
