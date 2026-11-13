//! The seed format's tests (decision 3, goal 3 plan step 2: HEX-SEED-1 and HEX-SEED-2; decision 5, goal 5
//! plan step 4: HEX-SEED-3): every request and answer reads back as written; version 1's bytes are the
//! ones recorded here, so a change to them fails (append-only); hostile bytes, at every truncation,
//! every bit flip and at random, are refused by type and never panic; and a create reads as one only
//! with the descriptors it says it carries.

use super::*;
use proptest::prelude::*;

fn full() -> Request {
    Request {
        init_pid: 4242,
        bundle: "/run/hex/oci/box-1".to_owned(),
        cgroup: Some("/sys/fs/cgroup/hex/box-1".to_owned()),
        args: vec!["/usr/bin/env".to_owned(), "x y".to_owned()],
        cwd: Some("/work".to_owned()),
        env: vec!["PATH=/usr/bin".to_owned(), "HOME=/root".to_owned()],
        user: Some((1000, Some(100))),
        dir: None,
    }
}

fn minimal() -> Request {
    Request {
        init_pid: 1,
        bundle: "/b".to_owned(),
        args: vec!["true".to_owned()],
        ..Request::default()
    }
}

fn answers() -> Vec<Answer> {
    vec![
        Answer::Started { pid: 4243 },
        Answer::Refused {
            failed: Failed::Start,
            why: "EAGAIN: the container's pids bound".to_owned(),
        },
        Answer::Refused {
            failed: Failed::Request,
            why: String::new(),
        },
    ]
}

fn creates() -> Vec<Create> {
    vec![
        Create {
            bundle: "/srv/bundles/box-1".to_owned(),
            dir: "/run/hex-node/0123456789abcdef-box-1".to_owned(),
            cgroup: Some("/sys/fs/cgroup/hex/0123456789abcdef-box-1".to_owned()),
            network: true,
            pruned: false,
            binds: 0,
            path_joins: 0,
            dev_mount: None,
            ipc_uts: false,
            config: false,
            mount_namespace: MountNamespace::Copy,
            proc_mount: None,
            masked_directory: false,
            fixed: Fixed::All,
        },
        Create {
            bundle: "/b".to_owned(),
            dir: "/d".to_owned(),
            cgroup: None,
            network: false,
            pruned: false,
            binds: 0,
            path_joins: 0,
            dev_mount: None,
            ipc_uts: false,
            config: false,
            mount_namespace: MountNamespace::Copy,
            proc_mount: None,
            masked_directory: false,
            fixed: Fixed::All,
        },
        // A pruned create (goal 5 plan step 2): no network, four fixed descriptors, two bind trees and
        // one namespace joined by path.
        Create {
            bundle: "/srv/bundles/box-2".to_owned(),
            dir: "/run/hex-node/fedcba9876543210-box-2".to_owned(),
            cgroup: Some("/sys/fs/cgroup/hex/fedcba9876543210-box-2".to_owned()),
            network: false,
            pruned: true,
            binds: 2,
            path_joins: 1,
            dev_mount: None,
            ipc_uts: false,
            config: false,
            mount_namespace: MountNamespace::Copy,
            proc_mount: None,
            masked_directory: false,
            fixed: Fixed::All,
        },
    ]
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// HEX-SEED-2: each request and answer reads back as the one written.
#[test]
fn every_message_reads_back_as_written() {
    for want in [full(), minimal()] {
        let bytes = request(&want).unwrap();
        assert_eq!(read_request(&bytes, bytes.len()).unwrap(), want);
    }
    for want in answers() {
        let bytes = answer(&want).unwrap();
        assert_eq!(read_answer(&bytes, bytes.len(), want.fds()).unwrap(), want);
    }
}

/// HEX-SEED-2, append-only: version 2's bytes for these messages are the ones recorded here. A seed
/// and an agent of different builds meet (a node spawns its seeds from the `hex` binary it is given),
/// so a change that alters any of them is a new version, never an edit of this one. Version 2 added a
/// create's pruned-root fields (goal 5 plan step 2), so the version byte and the create's body differ
/// from version 1's; the exec request's fields are unchanged beneath the new version byte.
#[test]
fn version_2_messages_are_the_bytes_recorded() {
    let golden: [(&str, Vec<u8>); 4] = [
        (GOLDEN_FULL, request(&full()).unwrap()),
        (GOLDEN_MINIMAL, request(&minimal()).unwrap()),
        (GOLDEN_STARTED, answer(&answers()[0]).unwrap()),
        (GOLDEN_REFUSED, answer(&answers()[1]).unwrap()),
    ];
    for (want, got) in golden {
        assert_eq!(hex(&got), want);
    }
}

/// HEX-SEED-3: a create reads back as the one written, through [`read_create`] and [`read_message`] with
/// the descriptors it carries; an exec reads through [`read_message`] as through [`read_request`]; and the
/// create's bytes are the ones recorded here, the exec's beside them unchanged.
#[test]
fn a_create_reads_back_as_written_and_its_bytes_are_recorded() {
    for want in creates() {
        let bytes = create(&want).unwrap();
        assert_eq!(read_create(&bytes, bytes.len()).unwrap(), want);
        assert_eq!(
            read_message(&bytes, bytes.len(), want.fds()).unwrap(),
            Message::Create(want.clone())
        );
    }
    let exec = request(&full()).unwrap();
    assert_eq!(
        read_message(&exec, exec.len(), REQUEST_FDS).unwrap(),
        Message::Exec(full())
    );
    assert_eq!(hex(&create(&creates()[0]).unwrap()), GOLDEN_CREATE);
    assert_eq!(hex(&create(&creates()[2]).unwrap()), GOLDEN_CREATE_PRUNED);
    assert_eq!(
        creates()[0].fds(),
        CREATE_FDS + 1,
        "network adds the made-ahead netns"
    );
    assert_eq!(creates()[1].fds(), CREATE_FDS);
    assert_eq!(
        creates()[2].fds(),
        CREATE_FDS + PRUNED_FIXED_FDS + 2 + 1,
        "pruned: the four fixed descriptors, two bind trees and one path-joined namespace, no network"
    );
    let unable = answer(&Answer::Refused {
        failed: Failed::Unable,
        why: "clone3: ENOSYS".to_owned(),
    })
    .unwrap();
    assert_eq!(
        read_answer(&unable, unable.len(), 0).unwrap(),
        Answer::Refused {
            failed: Failed::Unable,
            why: "clone3: ENOSYS".to_owned()
        }
    );
}

/// HEX-SEED-3: a create or an exec that came with another count of descriptors than it says it carries,
/// a create that names no bundle or no state directory, one whose `network` byte is neither 0 nor 1,
/// and a message of an answer's kind are each refused by type; every truncation of a create is refused,
/// and its bit flips never panic.
#[test]
fn a_malformed_create_is_refused_naming_what_is_wrong() {
    let with_network = create(&creates()[0]).unwrap();
    for got in [0, 1, 2, 4] {
        assert!(
            matches!(
                read_message(&with_network, with_network.len(), got),
                Err(Fault::Descriptors { want: 3, .. })
            ),
            "{got} descriptors"
        );
    }
    let exec = request(&minimal()).unwrap();
    assert!(matches!(
        read_message(&exec, exec.len(), 2),
        Err(Fault::Descriptors {
            want: 4,
            got: 2,
            ..
        })
    ));
    for nameless in [
        Create {
            bundle: String::new(),
            ..creates()[1].clone()
        },
        Create {
            dir: String::new(),
            ..creates()[1].clone()
        },
    ] {
        let bytes = create(&nameless).unwrap();
        assert!(matches!(
            read_create(&bytes, bytes.len()),
            Err(Fault::Value { what: "create", .. })
        ));
    }
    // A create ends with the network bool, the pruned bool, the binds u32 and the path_joins u32 (ten
    // bytes), so the network byte is the tenth from the end and the pruned byte the ninth, whatever the
    // names' lengths.
    let mut flag = create(&creates()[1]).unwrap();
    let network_at = flag.len() - 10;
    flag[network_at] = 2;
    assert!(matches!(
        read_create(&flag, flag.len()),
        Err(Fault::Value {
            what: "network",
            ..
        })
    ));
    let mut flag = create(&creates()[1]).unwrap();
    let pruned_at = flag.len() - 9;
    flag[pruned_at] = 2;
    assert!(matches!(
        read_create(&flag, flag.len()),
        Err(Fault::Value { what: "pruned", .. })
    ));
    // Bind trees and path-joined namespaces past their bounds, and a non-pruned create that claims any,
    // are refused by type.
    let too_many = create(&Create {
        binds: u32::try_from(MAX_BINDS + 1).expect("MAX_BINDS + 1 fits in u32"),
        ..creates()[2].clone()
    })
    .unwrap();
    assert!(matches!(
        read_create(&too_many, too_many.len()),
        Err(Fault::Value { what: "binds", .. })
    ));
    let too_many_joins = create(&Create {
        path_joins: u32::try_from(MAX_PATH_JOINS + 1).expect("MAX_PATH_JOINS + 1 fits in u32"),
        ..creates()[2].clone()
    })
    .unwrap();
    assert!(matches!(
        read_create(&too_many_joins, too_many_joins.len()),
        Err(Fault::Value {
            what: "path_joins",
            ..
        })
    ));
    let not_pruned_with_binds = create(&Create {
        pruned: false,
        binds: 1,
        ..creates()[1].clone()
    })
    .unwrap();
    assert!(matches!(
        read_create(&not_pruned_with_binds, not_pruned_with_binds.len()),
        Err(Fault::Value { what: "binds", .. })
    ));
    let started = answer(&answers()[0]).unwrap();
    assert!(matches!(
        read_message(&started, started.len(), 1),
        Err(Fault::Kind { kind: 0x8001, .. })
    ));
    assert!(matches!(
        read_create(&exec, exec.len()),
        Err(Fault::Kind { kind: 0x0001, .. })
    ));
    assert!(matches!(
        read_create(&with_network, MAX_REQUEST + 1),
        Err(Fault::Oversized { .. })
    ));
    for n in 0..with_network.len() {
        assert!(
            read_create(&with_network[..n], n).is_err(),
            "{n} bytes read"
        );
    }
    for bit in 0..with_network.len() * 8 {
        let mut flipped = with_network.clone();
        flipped[bit / 8] ^= 1 << (bit % 8);
        let _ = read_message(&flipped, flipped.len(), 3);
    }
}

const GOLDEN_CREATE: &str = "4845585302000200120000002f7372762f62756e646c65732f626f782d31240000002f72756e2f6865782d6e6f64652f303132333435363738396162636465662d626f782d3101290000002f7379732f66732f6367726f75702f6865782f303132333435363738396162636465662d626f782d3101000000000000000000";
const GOLDEN_CREATE_PRUNED: &str = "4845585302000200120000002f7372762f62756e646c65732f626f782d32240000002f72756e2f6865782d6e6f64652f666564636261393837363534333231302d626f782d3201290000002f7379732f66732f6367726f75702f6865782f666564636261393837363534333231302d626f782d3200010200000001000000";

const GOLDEN_FULL: &str = "484558530200010092100000120000002f72756e2f6865782f6f63692f626f782d3101180000002f7379732f66732f6367726f75702f6865782f626f782d31020000000c0000002f7573722f62696e2f656e760300000078207901050000002f776f726b020000000d000000504154483d2f7573722f62696e0a000000484f4d453d2f726f6f7401e80300000164000000";
const GOLDEN_MINIMAL: &str =
    "484558530200010001000000020000002f6200010000000400000074727565000000000000";
const GOLDEN_STARTED: &str = "484558530200018093100000";
const GOLDEN_REFUSED: &str = "484558530200028002002200000045414741494e3a2074686520636f6e7461696e65722773207069647320626f756e64";

/// HEX-SEED-1: every truncation of a valid message is refused by type, never a panic.
#[test]
fn every_truncation_is_refused() {
    let bytes = request(&full()).unwrap();
    for n in 0..bytes.len() {
        assert!(read_request(&bytes[..n], n).is_err(), "{n} bytes read");
    }
    for want in answers() {
        let bytes = answer(&want).unwrap();
        for n in 0..bytes.len() {
            assert!(
                read_answer(&bytes[..n], n, want.fds()).is_err(),
                "{n} bytes of {want:?} read"
            );
        }
    }
}

/// HEX-SEED-1: every single bit flipped in a valid message reads as some message or is refused, and
/// never panics; bytes past a message's end are refused.
#[test]
fn bit_flips_never_panic_and_trailing_bytes_are_refused() {
    let bytes = request(&full()).unwrap();
    for bit in 0..bytes.len() * 8 {
        let mut flipped = bytes.clone();
        flipped[bit / 8] ^= 1 << (bit % 8);
        let _ = read_request(&flipped, flipped.len());
    }
    let mut long = bytes;
    long.push(0);
    assert!(matches!(
        read_request(&long, long.len()),
        Err(Fault::Trailing { bytes: 1, .. })
    ));
    let mut long = answer(&answers()[0]).unwrap();
    long.push(0);
    assert!(matches!(
        read_answer(&long, long.len(), 1),
        Err(Fault::Trailing { bytes: 1, .. })
    ));
}

/// HEX-SEED-1: a message that isn't a seed's, of another version or kind, without a program, past the
/// size bound, or an answer with the wrong count of descriptors or an unknown refusal code, is refused
/// by type.
#[test]
fn each_malformed_message_is_refused_naming_what_is_wrong() {
    let good = request(&minimal()).unwrap();
    let with = |at: usize, b: u8| {
        let mut m = good.clone();
        m[at] = b;
        m
    };
    let wire = with(3, b'W');
    assert!(matches!(
        read_request(&wire, wire.len()),
        Err(Fault::Value {
            what: "message",
            ..
        })
    ));
    let v3 = with(4, 3);
    assert!(matches!(
        read_request(&v3, v3.len()),
        Err(Fault::Version { got: 3, .. })
    ));
    let started = with(7, 0x80);
    assert!(matches!(
        read_request(&started, started.len()),
        Err(Fault::Kind { kind: 0x8001, .. })
    ));
    let none = request(&Request {
        args: Vec::new(),
        ..minimal()
    })
    .unwrap();
    assert!(matches!(
        read_request(&none, none.len()),
        Err(Fault::Value { what: "args", .. })
    ));
    assert!(matches!(
        read_request(&good, MAX_REQUEST + 1),
        Err(Fault::Oversized { .. })
    ));
    let huge = Request {
        env: vec!["X".repeat(MAX_REQUEST)],
        ..minimal()
    };
    assert!(matches!(request(&huge), Err(Fault::Oversized { .. })));
    let started = answer(&answers()[0]).unwrap();
    assert!(matches!(
        read_answer(&started, started.len(), 0),
        Err(Fault::Descriptors {
            want: 1,
            got: 0,
            ..
        })
    ));
    let mut code = answer(&answers()[1]).unwrap();
    code[8] = 99;
    assert!(matches!(
        read_answer(&code, code.len(), 0),
        Err(Fault::Value {
            what: "refusal",
            ..
        })
    ));
}

/// A refusal whose reason is past [`MAX_WHY`] is sent with the reason cut at a character's boundary,
/// so a seed can always refuse.
#[test]
fn a_long_reason_is_cut_to_fit_at_a_character() {
    let why = "é".repeat(MAX_WHY);
    let bytes = answer(&Answer::Refused {
        failed: Failed::Seed,
        why,
    })
    .unwrap();
    assert!(bytes.len() <= MAX_ANSWER);
    match read_answer(&bytes, bytes.len(), 0).unwrap() {
        Answer::Refused { why, .. } => {
            assert!(why.len() <= MAX_WHY && why.len() > MAX_WHY - 2);
            assert!(why.chars().all(|c| c == 'é'));
        }
        other => panic!("{other:?}"),
    }
}

proptest! {
    /// HEX-SEED-1 and HEX-SEED-3: random bytes, as a request of either kind or as an answer, never panic.
    #[test]
    fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..256), fds in 0usize..5) {
        let _ = read_request(&bytes, bytes.len());
        let _ = read_message(&bytes, bytes.len(), fds);
        let _ = read_answer(&bytes, bytes.len(), fds);
    }

    /// HEX-SEED-1: random bytes after a valid header and kind never panic either.
    #[test]
    fn random_bodies_never_panic(body in proptest::collection::vec(any::<u8>(), 0..256)) {
        let mut bytes = request(&minimal()).unwrap();
        bytes.truncate(HEADER_BYTES);
        bytes.extend(body);
        let _ = read_request(&bytes, bytes.len());
    }
}

/// HEX-SEED-2/3, AC-T7.1.4: carry a prepared mount in a new kind without changing CREATE bytes.
/// Expect old payload intact, exactly one appended index/fd, every truncation and excess refused.
#[test]
fn a_prepared_create_appends_only_in_its_new_kind() {
    let ordinary = creates()[2].clone();
    let old = create(&ordinary).unwrap();
    let prepared = Create {
        dev_mount: Some(7),
        ..ordinary.clone()
    };
    let bytes = create(&prepared).unwrap();
    assert_eq!(&bytes[..6], &old[..6]);
    assert_eq!(u16::from_le_bytes(bytes[6..8].try_into().unwrap()), 3);
    assert_eq!(&bytes[8..old.len()], &old[8..]);
    assert_eq!(&bytes[old.len()..], &7u32.to_le_bytes());
    assert_eq!(prepared.fds(), ordinary.fds() + 1);
    assert_eq!(
        read_message(&bytes, bytes.len(), prepared.fds()).unwrap(),
        Message::Create(prepared.clone())
    );
    for len in 0..bytes.len() {
        assert!(read_create(&bytes[..len], len).is_err());
    }
    assert!(matches!(
        read_message(&bytes, bytes.len(), ordinary.fds()),
        Err(Fault::Descriptors { .. })
    ));
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(read_create(&trailing, trailing.len()).is_err());
    let over = create(&Create {
        binds: u32::try_from(MAX_BINDS).unwrap(),
        path_joins: u32::try_from(MAX_PATH_JOINS).unwrap(),
        network: true,
        dev_mount: Some(1),
        ..ordinary.clone()
    })
    .unwrap();
    assert!(matches!(
        read_create(&over, over.len()),
        Err(Fault::Value {
            what: "dev_mount",
            ..
        })
    ));
    let wrong_path = create(&Create {
        pruned: false,
        binds: 0,
        path_joins: 0,
        dev_mount: Some(1),
        ..ordinary
    })
    .unwrap();
    assert!(read_create(&wrong_path, wrong_path.len()).is_err());
}

/// HEX-IPCUTS-7 (goal 5 extended, step 4): a create with a pair is a kind of its own, every older kind's bytes as they
/// were. Do: write a create with a pair, with and without a prepared `/dev`, read each back, and read hostile ones.
/// Expect: its bytes are the ordinary create's, its kind apart, with an option appended; it says two descriptors more;
/// it reads back whole; a cut one, one with trailing bytes, one short of its descriptors, one that isn't pruned and one
/// past `SCM_MAX_FD` are each refused by type; and one without a pair still writes the older kinds.
#[test]
fn a_create_with_a_pair_appends_only_in_its_new_kind() {
    let ordinary = creates()[2].clone();
    let old = create(&ordinary).unwrap();
    let ahead = Create {
        ipc_uts: true,
        ..ordinary.clone()
    };
    let bytes = create(&ahead).unwrap();
    assert_eq!(&bytes[..6], &old[..6]);
    assert_eq!(u16::from_le_bytes(bytes[6..8].try_into().unwrap()), 4);
    assert_eq!(&bytes[8..old.len()], &old[8..]);
    assert_eq!(&bytes[old.len()..], &[0u8]);
    assert_eq!(ahead.fds(), ordinary.fds() + IPC_UTS_FDS);
    assert_eq!(
        read_message(&bytes, bytes.len(), ahead.fds()).unwrap(),
        Message::Create(ahead.clone())
    );
    for len in 0..bytes.len() {
        assert!(read_create(&bytes[..len], len).is_err());
    }
    assert!(matches!(
        read_message(&bytes, bytes.len(), ordinary.fds()),
        Err(Fault::Descriptors { .. })
    ));
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(read_create(&trailing, trailing.len()).is_err());

    let both = Create {
        dev_mount: Some(7),
        ..ahead.clone()
    };
    let bytes = create(&both).unwrap();
    assert_eq!(u16::from_le_bytes(bytes[6..8].try_into().unwrap()), 4);
    assert_eq!(both.fds(), ordinary.fds() + IPC_UTS_FDS + 1);
    assert_eq!(
        read_message(&bytes, bytes.len(), both.fds()).unwrap(),
        Message::Create(both)
    );

    let not_pruned = create(&Create {
        pruned: false,
        binds: 0,
        path_joins: 0,
        ipc_uts: true,
        ..ordinary.clone()
    })
    .unwrap();
    assert!(matches!(
        read_create(&not_pruned, not_pruned.len()),
        Err(Fault::Value {
            what: "ipc_uts",
            ..
        })
    ));
    let over = create(&Create {
        binds: u32::try_from(MAX_BINDS).unwrap(),
        path_joins: u32::try_from(MAX_PATH_JOINS).unwrap(),
        network: true,
        ipc_uts: true,
        ..ordinary.clone()
    })
    .unwrap();
    assert!(read_create(&over, over.len()).is_err());

    let prepared = create(&Create {
        dev_mount: Some(7),
        ..ordinary.clone()
    })
    .unwrap();
    assert_eq!(u16::from_le_bytes(prepared[6..8].try_into().unwrap()), 3);
    assert_eq!(u16::from_le_bytes(old[6..8].try_into().unwrap()), 2);
}

/// HEX-SEED-3, append-only (AC-T7.1.1, T-T7.1.4: the spec the runtime checked, never a later read of the bundle).
/// Do: write a create carrying its checked spec, of each shape (ordinary, pruned, with a prepared `/dev` and a pair).
/// Expect: kind 5, its body the older kinds' fields with the `/dev` index an option and the pair a flag; one descriptor
/// more, last; it reads back whole; every cut, trailing bytes and one short of its descriptors are refused by type;
/// an exec reader refuses its kind; and a create without it still writes the older kinds, byte for byte.
#[test]
fn a_create_with_its_checked_spec_appends_only_in_its_new_kind() {
    let shapes = [
        creates()[0].clone(),
        creates()[2].clone(),
        Create {
            dev_mount: Some(7),
            ipc_uts: true,
            ..creates()[2].clone()
        },
    ];
    for older in shapes {
        let old = create(&older).unwrap();
        let checked = Create {
            config: true,
            ..older.clone()
        };
        let bytes = create(&checked).unwrap();
        assert_eq!(&bytes[..6], &old[..6], "magic and version");
        assert_eq!(u16::from_le_bytes(bytes[6..8].try_into().unwrap()), 5);
        assert_eq!(checked.fds(), older.fds() + CONFIG_FDS);
        assert_eq!(read_create(&bytes, bytes.len()).unwrap(), checked);
        assert_eq!(
            read_message(&bytes, bytes.len(), checked.fds()).unwrap(),
            Message::Create(checked.clone())
        );
        for len in 0..bytes.len() {
            assert!(read_create(&bytes[..len], len).is_err(), "cut at {len}");
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(read_create(&trailing, trailing.len()).is_err());
        assert!(matches!(
            read_message(&bytes, bytes.len(), older.fds()),
            Err(Fault::Descriptors { .. })
        ));
        assert!(matches!(
            read_request(&bytes, bytes.len()),
            Err(Fault::Kind { .. })
        ));
    }
    assert_eq!(hex(&create(&creates()[0]).unwrap()), GOLDEN_CREATE);
    assert_eq!(hex(&create(&creates()[2]).unwrap()), GOLDEN_CREATE_PRUNED);
    // Past SCM_MAX_FD with its spec, as with a pair: refused by type.
    let over = create(&Create {
        binds: u32::try_from(MAX_BINDS).unwrap(),
        path_joins: u32::try_from(MAX_PATH_JOINS).unwrap(),
        network: true,
        config: true,
        ..creates()[2].clone()
    })
    .unwrap();
    assert!(read_create(&over, over.len()).is_err());
}

/// HEX-SEED-2, append-only (AC-T7.1.1: an exec's process settings from the spec its create checked and kept, never
/// the bundle as it is now). Do: write an exec naming the container's state directory. Expect: kind 6, its body the
/// older exec's bytes with the directory appended; it reads back whole, through read_request and read_message with
/// the exec's four descriptors; every cut and trailing bytes are refused; a create reader refuses its kind; and an
/// exec naming none still writes the older kind, byte for byte (its recorded bytes).
#[test]
fn an_exec_naming_its_state_directory_appends_only_in_its_new_kind() {
    let older = full();
    let old = request(&older).unwrap();
    let checked = Request {
        dir: Some("/run/hex-node/0123456789abcdef-box-1".to_owned()),
        ..older.clone()
    };
    let bytes = request(&checked).unwrap();
    assert_eq!(&bytes[..6], &old[..6], "magic and version");
    assert_eq!(u16::from_le_bytes(bytes[6..8].try_into().unwrap()), 6);
    assert_eq!(
        &bytes[8..old.len()],
        &old[8..],
        "the older exec's body, unchanged"
    );
    assert_eq!(read_request(&bytes, bytes.len()).unwrap(), checked);
    assert_eq!(
        read_message(&bytes, bytes.len(), REQUEST_FDS).unwrap(),
        Message::Exec(checked.clone())
    );
    for len in 0..bytes.len() {
        assert!(read_request(&bytes[..len], len).is_err(), "cut at {len}");
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(read_request(&trailing, trailing.len()).is_err());
    assert!(matches!(
        read_create(&bytes, bytes.len()),
        Err(Fault::Kind { .. })
    ));
    assert_eq!(hex(&old), GOLDEN_FULL);
}

/// HEX-MNTNS-2/3 (decision 6's update), HEX-SEED-2/3: a pruned create that says how its mount namespace is
/// made, from its tree or empty, does so in a new kind (7) whose bytes are the create carrying its checked spec
/// (kind 5) with the proc's option, the way and the masked directories' flag appended, so a create made the old
/// way keeps its bytes; one that doesn't carry its checked spec isn't written. Expect each to read
/// back as written with the descriptors it always carried, every truncation and a trailing byte refused, an
/// unknown way refused, the old way in the new kind refused (one encoding a value), and a way named by a
/// create that isn't pruned refused.
#[test]
fn a_create_naming_its_mount_namespace_appends_only_in_its_new_kind() {
    let ordinary = Create {
        config: true,
        ..creates()[2].clone()
    };
    let old = create(&ordinary).unwrap();
    assert!(matches!(
        create(&Create {
            config: false,
            mount_namespace: MountNamespace::Empty,
            ..ordinary.clone()
        }),
        Err(Fault::Value { what: "config", .. })
    ));
    for (way, code) in [(MountNamespace::Empty, 1u8), (MountNamespace::FromTree, 2)] {
        let named = Create {
            mount_namespace: way,
            ..ordinary.clone()
        };
        let bytes = create(&named).unwrap();
        assert_eq!(&bytes[..6], &old[..6]);
        assert_eq!(u16::from_le_bytes(bytes[6..8].try_into().unwrap()), 7);
        assert_eq!(&bytes[8..old.len()], &old[8..]);
        assert_eq!(
            &bytes[old.len()..],
            &[0, code, 0],
            "no prepared proc, the way, no masked directory's source"
        );
        assert_eq!(
            named.fds(),
            ordinary.fds(),
            "the namespace rides in the rootfs's place"
        );
        assert_eq!(
            read_message(&bytes, bytes.len(), named.fds()).unwrap(),
            Message::Create(named.clone())
        );
        for len in 0..bytes.len() {
            assert!(read_create(&bytes[..len], len).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(read_create(&trailing, trailing.len()).is_err());
        let with_dev = Create {
            dev_mount: Some(5),
            ..named.clone()
        };
        let bytes = create(&with_dev).unwrap();
        let old_dev = create(&Create {
            mount_namespace: MountNamespace::Copy,
            ..with_dev.clone()
        })
        .unwrap();
        assert_eq!(&bytes[8..old_dev.len()], &old_dev[8..]);
        assert_eq!(&bytes[old_dev.len()..], &[0, code, 0]);
        assert_eq!(with_dev.fds(), ordinary.fds() + 1);
        assert_eq!(read_create(&bytes, bytes.len()).unwrap(), with_dev);
    }
    let mut unknown = create(&Create {
        mount_namespace: MountNamespace::Empty,
        ..ordinary.clone()
    })
    .unwrap();
    let at = unknown.len() - 2;
    for code in [0u8, 3, 0xff] {
        *unknown.get_mut(at).unwrap() = code;
        // The copy is the old kinds' unless a proc rides; 3 and 255 name no way.
        assert!(
            matches!(
                read_create(&unknown, unknown.len()),
                Err(Fault::Value {
                    what: "mount_namespace",
                    ..
                })
            ),
            "way {code} is refused in the new kind"
        );
    }
    let unpruned = create(&Create {
        pruned: false,
        binds: 0,
        path_joins: 0,
        mount_namespace: MountNamespace::FromTree,
        ..ordinary
    })
    .unwrap();
    assert!(matches!(
        read_create(&unpruned, unpruned.len()),
        Err(Fault::Value {
            what: "mount_namespace",
            ..
        })
    ));
}

/// HEX-MNTNS-2/3, HEX-SEED-2: a started answer that says how the init's mount namespace was made reads back
/// as written, in a kind of its own after the started answer's bytes, with its pidfd; an unknown way and
/// every truncation are refused.
#[test]
fn a_started_answer_names_how_its_namespace_was_made() {
    for way in [
        MountNamespace::Copy,
        MountNamespace::Empty,
        MountNamespace::FromTree,
    ] {
        let want = Answer::StartedIn {
            pid: 4243,
            mount_namespace: way,
        };
        let bytes = answer(&want).unwrap();
        let started = answer(&Answer::Started { pid: 4243 }).unwrap();
        assert_eq!(u16::from_le_bytes(bytes[6..8].try_into().unwrap()), 0x8003);
        assert_eq!(&bytes[8..started.len()], &started[8..]);
        assert_eq!(bytes.len(), started.len() + 1);
        assert_eq!(read_answer(&bytes, bytes.len(), STARTED_FDS).unwrap(), want);
        assert!(read_answer(&bytes, bytes.len(), 0).is_err());
        for len in 0..bytes.len() {
            assert!(read_answer(&bytes[..len], len, STARTED_FDS).is_err());
        }
    }
    let mut bad = answer(&Answer::StartedIn {
        pid: 1,
        mount_namespace: MountNamespace::Copy,
    })
    .unwrap();
    *bad.last_mut().unwrap() = 3;
    assert!(read_answer(&bad, bad.len(), STARTED_FDS).is_err());
}

/// HEX-MNTNS-6 (decision 6's update, part 2), HEX-SEED-2/3: a pruned create carrying a proc made ahead, made
/// the old way or another, in the new kind. Expect its index after the `/dev` option, its two descriptors
/// counted, a read back as written, every truncation refused, and a proc on a create that isn't pruned
/// refused.
#[test]
fn a_create_carrying_a_prepared_proc_counts_its_two_descriptors() {
    let ordinary = Create {
        config: true,
        ..creates()[2].clone()
    };
    let old = create(&Create {
        dev_mount: Some(4),
        ..ordinary.clone()
    })
    .unwrap();
    for way in [MountNamespace::Copy, MountNamespace::FromTree] {
        let with_proc = Create {
            proc_mount: Some(3),
            dev_mount: Some(4),
            mount_namespace: way,
            ..ordinary.clone()
        };
        let bytes = create(&with_proc).unwrap();
        assert_eq!(u16::from_le_bytes(bytes[6..8].try_into().unwrap()), 7);
        assert_eq!(&bytes[8..old.len()], &old[8..]);
        assert_eq!(&bytes[old.len()..], &[1, 3, 0, 0, 0, way.code(), 0]);
        assert_eq!(with_proc.fds(), ordinary.fds() + 1 + PROC_FDS);
        assert_eq!(
            read_message(&bytes, bytes.len(), with_proc.fds()).unwrap(),
            Message::Create(with_proc.clone())
        );
        for len in 0..bytes.len() {
            assert!(read_create(&bytes[..len], len).is_err());
        }
    }
    let unpruned = create(&Create {
        pruned: false,
        binds: 0,
        path_joins: 0,
        proc_mount: Some(1),
        ..ordinary
    })
    .unwrap();
    assert!(read_create(&unpruned, unpruned.len()).is_err());
}

/// HEX-MNTNS-8 (decision 6's update), HEX-SEED-2/3: a pruned create carrying the node's empty read-only tmpfs for
/// its masked directories, made the old way, in the new kind. Expect the flag last, its one descriptor counted, a
/// read back as written, every truncation refused, a byte that is neither 0 nor 1 refused, and the flag on a create
/// that isn't pruned refused.
#[test]
fn a_create_carrying_the_masked_directory_source_counts_its_descriptor() {
    let ordinary = Create {
        config: true,
        ..creates()[2].clone()
    };
    let old = create(&ordinary).unwrap();
    let with = Create {
        masked_directory: true,
        ..ordinary.clone()
    };
    let bytes = create(&with).unwrap();
    assert_eq!(u16::from_le_bytes(bytes[6..8].try_into().unwrap()), 7);
    assert_eq!(&bytes[8..old.len()], &old[8..]);
    assert_eq!(&bytes[old.len()..], &[0, 0, 1]);
    assert_eq!(with.fds(), ordinary.fds() + 1);
    assert_eq!(
        read_message(&bytes, bytes.len(), with.fds()).unwrap(),
        Message::Create(with.clone())
    );
    for len in 0..bytes.len() {
        assert!(read_create(&bytes[..len], len).is_err());
    }
    let mut two = bytes.clone();
    *two.last_mut().unwrap() = 2;
    assert!(read_create(&two, two.len()).is_err());
    let unpruned = create(&Create {
        pruned: false,
        binds: 0,
        path_joins: 0,
        masked_directory: true,
        ..ordinary
    })
    .unwrap();
    assert!(read_create(&unpruned, unpruned.len()).is_err());
}

/// HEX-SEED-4 (decision 8, "The p99.9 that stayed"; AC-T7.1.3): a park reads back as written, with its two
/// descriptors and no other count; its bytes are the ones recorded here (append-only: kind 8, after goal 6's 7);
/// every truncation of it and a byte past its end are refused by type.
#[test]
fn a_park_reads_back_as_written_and_its_bytes_are_recorded() {
    // CLONE_NEWNS | CLONE_NEWPID, the namespaces an owner's box on the pruned path makes fresh.
    let want = Park { flags: 0x2002_0000 };
    let bytes = park(&want).unwrap();
    assert_eq!(hex(&bytes), "48455853020008000000022000000000");
    assert_eq!(
        read_message(&bytes, bytes.len(), PARK_FDS).unwrap(),
        Message::Park(want)
    );
    for got in [0, 1, 3] {
        assert!(
            matches!(
                read_message(&bytes, bytes.len(), got),
                Err(Fault::Descriptors { .. })
            ),
            "a park with {got} descriptors"
        );
    }
    for n in 0..bytes.len() {
        assert!(
            read_message(&bytes[..n], n, PARK_FDS).is_err(),
            "{n} bytes read"
        );
    }
    let mut long = bytes.clone();
    long.push(0);
    assert!(read_message(&long, long.len(), PARK_FDS).is_err());
}

/// The lead's (B) of 2026-11-09 (decision 8's claim), HEX-SEED-2/3: a pruned create carrying only the descriptors its init
/// reads does so in a new kind (9), whose bytes are the older kinds' create with its checked spec (kind 5), then the
/// proc's option, the way and the masked directories' flag as kind 7 has them, whatever the way, then whether the cgroup
/// directory rides; a create of the older layout keeps its kind and its bytes. Expect: no map pipe and no bundle directory
/// counted, and the cgroup directory only where it rides (a seed's clone, never a parked init's claim); each reads back as
/// written with those descriptors and with no other count; every truncation, a trailing byte and a cgroup byte neither 0
/// nor 1 refused, and no bit flip panics; a lean create that isn't pruned refused; and one without its checked spec not
/// written.
#[test]
fn a_lean_create_carries_only_what_its_init_reads_in_its_new_kind() {
    let ordinary = Create {
        config: true,
        ..creates()[2].clone()
    };
    let old = create(&ordinary).unwrap();
    for cgroup in [false, true] {
        let lean = Create {
            fixed: Fixed::Lean { cgroup },
            ..ordinary.clone()
        };
        let bytes = create(&lean).unwrap();
        assert_eq!(&bytes[..6], &old[..6]);
        assert_eq!(u16::from_le_bytes(bytes[6..8].try_into().unwrap()), 9);
        assert_eq!(&bytes[8..old.len()], &old[8..]);
        assert_eq!(
            &bytes[old.len()..],
            &[0, 0, 0, u8::from(cgroup)],
            "no prepared proc, the copy, no masked directory's source, then the cgroup directory's flag"
        );
        assert_eq!(
            lean.fds(),
            ordinary.fds() - 2 - usize::from(!cgroup),
            "no map pipe and no bundle directory; the cgroup directory where it rides"
        );
        assert_eq!(
            read_message(&bytes, bytes.len(), lean.fds()).unwrap(),
            Message::Create(lean.clone())
        );
        for got in [lean.fds() - 1, lean.fds() + 1, ordinary.fds()] {
            assert!(
                matches!(
                    read_message(&bytes, bytes.len(), got),
                    Err(Fault::Descriptors { .. })
                ),
                "a lean create with {got} descriptors"
            );
        }
        for len in 0..bytes.len() {
            assert!(read_create(&bytes[..len], len).is_err(), "{len} bytes read");
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(read_create(&trailing, trailing.len()).is_err());
        let mut two = bytes.clone();
        *two.last_mut().unwrap() = 2;
        assert!(read_create(&two, two.len()).is_err());
        for bit in 0..bytes.len() * 8 {
            let mut flipped = bytes.clone();
            flipped[bit / 8] ^= 1 << (bit % 8);
            let _ = read_message(&flipped, flipped.len(), lean.fds());
        }
    }
    // A way named, a prepared `/dev` and proc and the masked directories' source ride as kind 7 has them, the flag after.
    let named = Create {
        mount_namespace: MountNamespace::FromTree,
        dev_mount: Some(4),
        proc_mount: Some(3),
        masked_directory: true,
        ..ordinary.clone()
    };
    let seven = create(&named).unwrap();
    let lean = Create {
        fixed: Fixed::Lean { cgroup: false },
        ..named.clone()
    };
    let bytes = create(&lean).unwrap();
    assert_eq!(u16::from_le_bytes(bytes[6..8].try_into().unwrap()), 9);
    assert_eq!(&bytes[8..seven.len()], &seven[8..]);
    assert_eq!(&bytes[seven.len()..], &[0]);
    assert_eq!(lean.fds(), named.fds() - 3);
    assert_eq!(
        read_message(&bytes, bytes.len(), lean.fds()).unwrap(),
        Message::Create(lean)
    );
    // Only a pruned create is lean: a classic create's init may ask for its maps, or stop for its hooks, on the map pipe.
    let unpruned = create(&Create {
        pruned: false,
        binds: 0,
        path_joins: 0,
        fixed: Fixed::Lean { cgroup: false },
        ..ordinary.clone()
    })
    .unwrap();
    assert!(matches!(
        read_create(&unpruned, unpruned.len()),
        Err(Fault::Value { what: "fixed", .. })
    ));
    // It carries the spec its runtime checked, as every create of this build does.
    assert!(matches!(
        create(&Create {
            config: false,
            fixed: Fixed::Lean { cgroup: true },
            ..ordinary
        }),
        Err(Fault::Value { what: "config", .. })
    ));
}

/// HEX-RT-14 through a seed (CLAUDE.md §3): a seed refuses a program past its exec bound as that, so the
/// agent refuses it as `Invalid` "program", not as a failed start. Do: write a program refusal. Expect: it
/// reads back as written, its code is 5, appended after version 1's four (append-only), and the four keep
/// theirs.
#[test]
fn a_program_refusal_reads_back_with_a_code_of_its_own() {
    let want = Answer::Refused {
        failed: Failed::Program,
        why: "a program of 300000 bytes".to_owned(),
    };
    let bytes = answer(&want).unwrap();
    assert_eq!(read_answer(&bytes, bytes.len(), 0).unwrap(), want);
    assert_eq!(&bytes[HEADER_BYTES..HEADER_BYTES + 2], &5u16.to_le_bytes());
    for (failed, code) in [
        (Failed::Request, 1u16),
        (Failed::Start, 2),
        (Failed::Seed, 3),
        (Failed::Unable, 4),
        (Failed::Program, 5),
    ] {
        assert_eq!(failed.code(), code);
        assert_eq!(Failed::of(code).unwrap(), failed);
    }
}
