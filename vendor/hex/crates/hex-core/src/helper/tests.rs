//! The helper protocol's round-trips and its hostile-input test (CLAUDE.md §6): every request and answer
//! survives encode→read unchanged, and no bytes — random or structured-but-wrong — make the parser panic;
//! each is a typed [`Fault`], and a kind the protocol doesn't define is refused as one (the helper "does
//! nothing else").

use hex_api::{BoxId, Exit, Program, User};
use proptest::prelude::*;

use super::*;
use crate::meet::{cases_to_meet, sized_runner};

fn box_id(s: &str) -> BoxId {
    BoxId::new(s).expect("a valid box id")
}

fn a_program() -> Program {
    Program {
        args: vec!["/bin/echo".to_owned(), "hi".to_owned()],
        env: vec![("PATH".to_owned(), "/usr/bin".to_owned())],
        cwd: Some("/tmp".to_owned()),
        user: Some(User {
            uid: 501,
            gid: 20,
            groups: vec![12, 61],
        }),
    }
}

/// Every request round-trips: encode in this version, read back the same (HEX-MAC-10).
#[test]
fn requests_round_trip() {
    let requests = [
        Request::Hello { version: VERSION },
        Request::Acquire {
            box_id: box_id("b1"),
            bound: 256 << 20,
            reserve: 16 << 20,
            pids_max: None,
        },
        Request::Acquire {
            box_id: box_id("b1"),
            bound: 256 << 20,
            reserve: 16 << 20,
            pids_max: Some(4096),
        },
        Request::Start {
            box_id: box_id("b1"),
            program: a_program(),
            leader: true,
        },
        Request::Start {
            box_id: box_id("b2"),
            program: Program {
                args: vec!["x".to_owned()],
                env: vec![],
                cwd: None,
                user: None,
            },
            leader: false,
        },
        Request::End {
            box_id: box_id("b1"),
        },
        Request::Inspect {
            box_id: box_id("b1"),
        },
        Request::Signal {
            box_id: box_id("b1"),
            signal: 15,
        },
        Request::Wait {
            box_id: box_id("b1"),
            pid: 4242,
        },
        Request::Kill {
            box_id: box_id("b1"),
            pid: 4242,
        },
    ];
    for req in &requests {
        let bytes = encode_request(VERSION, 7, req).expect("encode");
        let (v, id, got) = read_request(&bytes, req.descriptors()).expect("read");
        assert_eq!(v, VERSION);
        assert_eq!(id, 7);
        assert_eq!(&got, req);
    }
}

/// The wire changes append-only (CLAUDE.md, Working here): an acquire naming no process limit encodes
/// as the first version's kind with the same bytes, so a helper that predates the limit still reads it;
/// one naming a limit (the box's `pids_max`, HEX-MAC-10) is a kind of its own, which that helper refuses
/// by kind rather than misreading. Do encode both; expect kinds 0x0002 and 0x0008, and the first's body
/// exactly box id, bound and reserve.
#[test]
fn an_acquire_with_a_process_limit_is_a_kind_of_its_own() {
    let plain = Request::Acquire {
        box_id: box_id("b1"),
        bound: 256 << 20,
        reserve: 16 << 20,
        pids_max: None,
    };
    let limited = Request::Acquire {
        box_id: box_id("b1"),
        bound: 256 << 20,
        reserve: 16 << 20,
        pids_max: Some(64),
    };
    let kind_of = |bytes: &[u8]| u16::from_le_bytes([bytes[6], bytes[7]]);
    let plain_bytes = encode_request(VERSION, 1, &plain).expect("encode");
    let limited_bytes = encode_request(VERSION, 1, &limited).expect("encode");
    assert_eq!(kind_of(&plain_bytes), 0x0002);
    assert_eq!(kind_of(&limited_bytes), 0x0008);
    // Header, then the box id (u32 length and its bytes), bound and reserve: nothing more.
    assert_eq!(plain_bytes.len(), HEADER_BYTES + 4 + 2 + 8 + 8);
    assert_eq!(limited_bytes.len(), plain_bytes.len() + 4);
}

/// HEX-RT-16 on a box-user box (the lead, 2026-11-09): a kill of one program in a box is a kind of its own,
/// appended after every earlier one, so a helper that predates it refuses it by kind rather than reading it as
/// another; it names the box and the program by pid, and rides with no descriptor. Do encode one; expect kind
/// 0x0009, its body exactly the box id and the pid, and none of the start's descriptors.
#[test]
fn a_kill_of_one_program_is_a_kind_of_its_own() {
    let kill = Request::Kill {
        box_id: box_id("b1"),
        pid: 4242,
    };
    let bytes = encode_request(VERSION, 1, &kill).expect("encode");
    assert_eq!(u16::from_le_bytes([bytes[6], bytes[7]]), 0x0009);
    // Header, then the box id (u32 length and its bytes), then the pid.
    assert_eq!(bytes.len(), HEADER_BYTES + 4 + 2 + 4);
    assert_eq!(kill.descriptors(), 0);
}

/// Every answer round-trips.
#[test]
fn answers_round_trip() {
    let answers = [
        Answer::Hello { version: VERSION },
        Answer::Acquired { uid: 502 },
        Answer::Started { pid: 4242 },
        Answer::Ended,
        Answer::Signalled,
        Answer::State(State {
            members: 3,
            footprint: 200 << 20,
            peak: 240 << 20,
            processes: 4,
            crossings: 2,
            ended_over: false,
        }),
        Answer::Refused(Refused::UnknownBox(box_id("b9"))),
        Answer::Refused(Refused::AlreadyAcquired(box_id("b9"))),
        Answer::Refused(Refused::NoFreeUser),
        Answer::Refused(Refused::StartFailed("ENOENT".to_owned())),
        Answer::Refused(Refused::NotPermitted("exec arbitrary".to_owned())),
        Answer::Refused(Refused::Malformed("bad id".to_owned())),
        Answer::Refused(Refused::NotRunning(box_id("b9"))),
        Answer::Refused(Refused::GrowFailed("dscl: eDSPermissionError".to_owned())),
        Answer::Waited(None),
        Answer::Waited(Some(Exit::Code(3))),
        Answer::Waited(Some(Exit::Signal(9))),
        Answer::Waited(Some(Exit::Unknown)),
        Answer::Killed,
    ];
    for ans in &answers {
        let bytes = encode_answer(VERSION, 9, ans).expect("encode");
        let (v, id, got) = read_answer(&bytes).expect("read");
        assert_eq!(v, VERSION);
        assert_eq!(id, 9);
        assert_eq!(&got, ans);
    }
}

/// HEX-MAC-11: the helper refuses a kind the protocol doesn't define — "does nothing else" (CLAUDE.md §3).
/// An unknown request kind reads as a typed [`Fault::Kind`], not a panic and not a silent accept.
#[test]
fn an_unknown_kind_is_refused_by_name() {
    let mut bytes = encode_request(
        VERSION,
        1,
        &Request::End {
            box_id: box_id("b1"),
        },
    )
    .unwrap();
    // Overwrite the kind (bytes 6..8) with one no version defines.
    bytes[6] = 0x77;
    bytes[7] = 0x77;
    match read_request(&bytes, 0) {
        Err(Fault::Kind { kind, expected, .. }) => {
            assert_eq!(kind, 0x7777);
            assert_eq!(expected, "a helper request");
        }
        other => panic!("expected a Kind fault, got {other:?}"),
    }
}

/// A `Start` that doesn't carry exactly its three descriptors is refused (its stdio can't be dropped or
/// forged, CLAUDE.md §3).
#[test]
fn a_start_without_its_three_descriptors_is_refused() {
    let req = Request::Start {
        box_id: box_id("b1"),
        program: a_program(),
        leader: true,
    };
    let bytes = encode_request(VERSION, 1, &req).unwrap();
    assert!(read_request(&bytes, 3).is_ok());
    for n in [0usize, 1, 2, 4] {
        match read_request(&bytes, n) {
            Err(Fault::Descriptors { want, got, .. }) => {
                assert_eq!(want, 3);
                assert_eq!(got, n);
            }
            other => panic!("expected a Descriptors fault for {n}, got {other:?}"),
        }
    }
}

/// A non-Start request that arrives carrying descriptors is refused: only a Start takes them.
#[test]
fn a_non_start_with_descriptors_is_refused() {
    let bytes = encode_request(
        VERSION,
        1,
        &Request::Inspect {
            box_id: box_id("b1"),
        },
    )
    .unwrap();
    assert!(matches!(
        read_request(&bytes, 1),
        Err(Fault::Descriptors {
            want: 0,
            got: 1,
            ..
        })
    ));
}

/// Wrong magic, a bad version, trailing bytes and truncation are each a typed fault.
#[test]
fn malformed_headers_and_bodies_are_typed_faults() {
    let good = encode_request(
        VERSION,
        1,
        &Request::End {
            box_id: box_id("b1"),
        },
    )
    .unwrap();

    // Not a helper message.
    let mut wrong_magic = good.clone();
    wrong_magic[0] = b'X';
    assert!(matches!(read_request(&wrong_magic, 0), Err(Fault::Magic)));

    // A version this side doesn't speak, and version 0.
    let mut bad_version = good.clone();
    bad_version[4] = 0xff;
    bad_version[5] = 0xff;
    assert!(matches!(
        read_request(&bad_version, 0),
        Err(Fault::Version { .. })
    ));
    let mut zero_version = good.clone();
    zero_version[4] = 0;
    zero_version[5] = 0;
    assert!(matches!(
        read_request(&zero_version, 0),
        Err(Fault::Version { got: 0, .. })
    ));

    // Bytes past what the kind defines are refused, not skipped.
    let mut trailing = good.clone();
    trailing.push(0);
    assert!(matches!(
        read_request(&trailing, 0),
        Err(Fault::Trailing { .. })
    ));

    // Truncated: every prefix short of the whole is a fault, never a panic.
    for cut in 0..good.len() {
        assert!(
            read_request(&good[..cut], 0).is_err(),
            "a {cut}-byte prefix read as a whole request"
        );
    }

    // A message past the bound is refused before it is parsed.
    let huge = vec![0u8; MAX_MESSAGE + 1];
    assert!(matches!(
        read_request(&huge, 0),
        Err(Fault::Oversized { .. })
    ));
}

/// HEX-MAC-11 (hostile input, CLAUDE.md §6): no bytes make the parser panic. Random bytes, and valid
/// framings with a random body, each read as a request or a typed fault — never a crash, never a silent
/// accept of trailing bytes.
///
/// Each run's count follows from its strategy ([`crate::meet`]): any bytes are shorter than a header in 16
/// of their 2048 lengths, the rarer kind of that run; a framed message names one of the protocol's 15 kinds
/// (7 requests, their 7 answers, a refusal) in 15 of its 0x9000 kinds, the rarer kind of that one.
#[test]
fn the_parser_never_panics_on_hostile_input() {
    let short = cases_to_meet(16.0 / 2048.0);
    let known_kind = cases_to_meet(15.0 / f64::from(0x9000_u32));
    let mut runner = sized_runner(short);
    // Any bytes at all.
    runner
        .run(&prop::collection::vec(any::<u8>(), 0..2048), |bytes| {
            let _ = read_request(&bytes, 0);
            let _ = read_request(&bytes, 3);
            let _ = read_answer(&bytes);
            Ok(())
        })
        .unwrap();
    // A well-formed header (so the magic and version pass) over a random kind and body: exercises the
    // per-kind readers, not just the header's rejection.
    let framed =
        (0u16..0x9000u16, prop::collection::vec(any::<u8>(), 0..512)).prop_map(|(kind, body)| {
            let mut m = Vec::with_capacity(HEADER_BYTES + body.len());
            m.extend_from_slice(&MAGIC);
            m.extend_from_slice(&VERSION.to_le_bytes());
            m.extend_from_slice(&kind.to_le_bytes());
            m.extend_from_slice(&3u64.to_le_bytes());
            m.extend_from_slice(&body);
            m
        });
    let mut runner = sized_runner(known_kind);
    runner
        .run(&framed, |bytes| {
            let _ = read_request(&bytes, 0);
            let _ = read_answer(&bytes);
            Ok(())
        })
        .unwrap();
}

/// HEX-MAC-10, CLAUDE.md §5 (the bound is macOS's, not picked): do encode the largest Start a macOS exec
/// would take: an argument filling `ARG_MAX` as the kernel charges it (its bytes, its NUL and its argv
/// pointer, `bsd/kern/kern_exec.c:6108-6112`), a `PATH_MAX` working folder, `NGROUPS_MAX` groups and a
/// longest box id; expect it within the message bound and read back unchanged. A program past that is
/// one exec refuses (`E2BIG`), so the helper's bound need hold no more.
#[test]
fn the_largest_program_a_macos_exec_takes_fits_one_message() {
    let pointer = 8;
    let start = Request::Start {
        box_id: box_id(&"b".repeat(BoxId::MAX_LEN)),
        program: Program {
            args: vec!["a".repeat(ARG_MAX - 1 - pointer)],
            env: vec![],
            cwd: Some("c".repeat(PATH_MAX)),
            user: Some(User {
                uid: 501,
                gid: 20,
                groups: vec![12; NGROUPS_MAX],
            }),
        },
        leader: true,
    };
    let bytes = encode_request(VERSION, 1, &start).expect("within the bound");
    assert!(bytes.len() <= MAX_MESSAGE);
    let (_, _, got) = read_request(&bytes, START_FDS).expect("read back");
    assert_eq!(got, start);
}
