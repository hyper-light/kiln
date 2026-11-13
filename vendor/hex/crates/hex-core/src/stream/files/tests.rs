//! The staging stream's tests (docs/design/shards-guest.md §4a, HEX-GUEST-13; native plan step 5): a
//! stage's entries read back as written however the stream cuts them; hostile names, kinds, modes and
//! fields are refused by type; and every count past its bound is refused before its bytes are held.
//! §4a: an entry's owner, mtime and extended attributes, and hard links, read back as written, hostile
//! ones refused by type; a stream of any version but 2 refused by name.

use super::*;
use proptest::prelude::*;

fn entry(path: &str, what: What) -> Entry {
    Entry {
        path: path.to_owned(),
        mode: 0o755,
        what,
        meta: None,
    }
}

/// A stage of `entries`, a file's bytes after its head as `bytes` gives them.
fn stage(id: &str, entries: &[(Entry, Vec<u8>)]) -> Vec<u8> {
    let mut out = open(Op::Stage, id).unwrap();
    for (e, bytes) in entries {
        out.extend(head(e, &LIMITS).unwrap());
        out.extend(bytes);
    }
    out.extend(END);
    out
}

/// What an unpacker reads from `input`, fed `cut` bytes at a time: its steps, a file's bytes gathered
/// into the entry before them, or the first refusal.
fn read(limits: Limits, input: &[u8], cut: usize) -> Result<Vec<(Step, Vec<u8>)>, Refused> {
    let mut u = Unpack::new(limits);
    let mut out: Vec<(Step, Vec<u8>)> = Vec::new();
    for mut piece in input.chunks(cut.max(1)) {
        while !piece.is_empty() {
            let (took, step) = u.take(piece)?;
            assert!(took > 0, "take made no progress");
            match step {
                Some(Step::Data { .. }) => out.last_mut().unwrap().1.extend(&piece[..took]),
                Some(step) => out.push((step, Vec::new())),
                None => {}
            }
            piece = &piece[took..];
        }
    }
    u.finish()?;
    Ok(out)
}

fn sample() -> Vec<(Entry, Vec<u8>)> {
    vec![
        (entry("root", What::Dir), vec![]),
        (entry("root/bin", What::Dir), vec![]),
        (
            entry("root/bin/sh", What::File { size: 5 }),
            b"#!sh\n".to_vec(),
        ),
        (entry("root/empty", What::File { size: 0 }), vec![]),
        (
            entry(
                "root/lib",
                What::Symlink {
                    target: "/usr/lib".to_owned(),
                },
            ),
            vec![],
        ),
    ]
}

/// HEX-GUEST-13: stage a directory tree with a file, an empty file and a symlink, read it back fed a
/// byte at a time and whole: expect the opening, each entry with its bytes, and the end, the same both
/// ways.
#[test]
fn a_stage_reads_back_as_written_however_it_is_cut() {
    let entries = sample();
    let bytes = stage("box-1", &entries);
    let mut want = vec![(
        Step::Open {
            op: Op::Stage,
            id: "box-1".to_owned(),
            layers: vec![],
        },
        vec![],
    )];
    want.extend(entries.into_iter().map(|(e, b)| (Step::Entry(e), b)));
    want.push((Step::End, vec![]));
    for cut in [1, 3, 16, 17, bytes.len()] {
        assert_eq!(read(LIMITS, &bytes, cut).unwrap(), want, "cut {cut}");
    }
}

/// HEX-GUEST-13: a remove is its opening alone: expect it read back, and a byte after it refused as
/// trailing.
#[test]
fn a_remove_is_its_opening_alone() {
    let bytes = open(Op::Remove, "box-1").unwrap();
    assert_eq!(
        read(LIMITS, &bytes, 1).unwrap(),
        vec![(
            Step::Open {
                op: Op::Remove,
                id: "box-1".to_owned(),
                layers: vec![],
            },
            vec![]
        )]
    );
    let mut more = bytes;
    more.push(0);
    assert_eq!(read(LIMITS, &more, 64), Err(Refused::Trailing(1)));
}

/// HEX-GUEST-13: end the stream before its end head, in a head and in a file: expect each refused as
/// truncated, never taken as a whole stage.
#[test]
fn a_stage_cut_short_is_refused_as_truncated() {
    let bytes = stage("b", &sample());
    for cut in [
        3,
        OPEN_BYTES + 1,
        OPEN_BYTES + 5,
        bytes.len() - HEAD_BYTES,
        bytes.len() - 1,
    ] {
        assert!(
            matches!(read(LIMITS, &bytes[..cut], 7), Err(Refused::Truncated(_))),
            "cut at {cut}"
        );
    }
}

/// The stage of one entry whose head is written raw: `kind`, `mode`, `path` and `target`, `size`.
fn raw(kind: u8, pad: u8, mode: u16, path: &[u8], target: &[u8], size: u64) -> Vec<u8> {
    let mut out = open(Op::Stage, "b").unwrap();
    out.push(kind);
    out.push(pad);
    out.extend(mode.to_le_bytes());
    out.extend(u16::try_from(path.len()).unwrap().to_le_bytes());
    out.extend(u16::try_from(target.len()).unwrap().to_le_bytes());
    out.extend(size.to_le_bytes());
    out.extend(path);
    out.extend(target);
    out
}

fn refused_name(path: &[u8]) -> Refused {
    read(LIMITS, &raw(1, 0, 0o755, path, b"", 0), 64).unwrap_err()
}

/// HEX-GUEST-13: send names that would leave the staged root, or aren't names: expect each refused as
/// [`Refused::Name`] naming the path and why, before anything is made.
#[test]
fn hostile_names_are_refused_by_name() {
    for (path, why) in [
        (&b"/etc/passwd"[..], "it is absolute"),
        (b"..", "it has a '..' component"),
        (b"a/../../etc", "it has a '..' component"),
        (b"a/./b", "it has a '.' component"),
        (b"a//b", "it has an empty component"),
        (b"a/", "it has an empty component"),
        (b"", "it is empty"),
        (b"a\0b", "it holds a NUL"),
        (b"\xff\xfe", "it isn't UTF-8"),
    ] {
        match refused_name(path) {
            Refused::Name { why: got, .. } => assert_eq!(got, why, "{path:?}"),
            other => panic!("{path:?}: {other:?}"),
        }
    }
    let long = "a".repeat(NAME_MAX + 1);
    assert!(matches!(
        refused_name(long.as_bytes()),
        Refused::Name { .. }
    ));
}

/// HEX-GUEST-13: send a device node's, a FIFO's and a socket's kind (there is none on the stream), a
/// mode with type bits, a pad byte set, a directory with a size, a file with a target, and a symlink
/// with no target: expect each refused by its type.
#[test]
fn hostile_kinds_modes_and_fields_are_refused_by_type() {
    for kind in [8, 9, 0xff] {
        assert_eq!(
            read(LIMITS, &raw(kind, 0, 0o644, b"dev/null", b"", 0), 64),
            Err(Refused::Kind(kind))
        );
    }
    assert!(matches!(
        read(LIMITS, &raw(2, 0, 0o10_644, b"f", b"", 0), 64),
        Err(Refused::Mode { .. })
    ));
    assert_eq!(
        read(LIMITS, &raw(1, 9, 0o755, b"d", b"", 0), 64),
        Err(Refused::Pad(9))
    );
    assert!(matches!(
        read(LIMITS, &raw(1, 0, 0o755, b"d", b"", 1), 64),
        Err(Refused::Field { field: "size", .. })
    ));
    assert!(matches!(
        read(LIMITS, &raw(2, 0, 0o644, b"f", b"x", 0), 64),
        Err(Refused::Field {
            field: "target",
            ..
        })
    ));
    assert!(matches!(
        read(LIMITS, &raw(3, 0, 0o777, b"l", b"", 0), 64),
        Err(Refused::Target { .. })
    ));
    let mut end = open(Op::Stage, "b").unwrap();
    let mut bad_end = END;
    bad_end[8] = 1;
    end.extend(bad_end);
    assert!(matches!(read(LIMITS, &end, 64), Err(Refused::Field { .. })));
}

/// HEX-GUEST-13: open with the wrong magic, version, op or pad, and with an empty or over-long id:
/// expect each refused by its type.
#[test]
fn hostile_openings_are_refused_by_type() {
    let good = open(Op::Stage, "b").unwrap();
    let with = |at: usize, byte: u8| {
        let mut b = good.clone();
        b[at] = byte;
        read(LIMITS, &b, 64).unwrap_err()
    };
    assert_eq!(with(0, b'X'), Refused::Magic);
    assert_eq!(with(4, 3), Refused::Version { got: 3 });
    assert_eq!(with(6, 3), Refused::Op(3));
    assert_eq!(with(7, 1), Refused::Pad(1));
    let mut empty = good.clone();
    empty.truncate(OPEN_BYTES);
    empty[8] = 0;
    empty[9] = 0;
    assert!(matches!(read(LIMITS, &empty, 64), Err(Refused::Id(_))));
    let long = open(Op::Stage, &"a".repeat(129)).unwrap();
    assert!(matches!(read(LIMITS, &long, 64), Err(Refused::Id(_))));
}

/// HEX-GUEST-13: stage one entry past the entries bound, a file past the bytes bound, and a path and a
/// target past the path bound: expect each refused as over its bound from its head, before its path
/// or bytes are held.
#[test]
fn every_count_past_its_bound_is_refused_from_its_head() {
    let tight = Limits {
        entries: 2,
        bytes: 10,
        path: 8,
    };
    let three: Vec<(Entry, Vec<u8>)> = ["a", "b", "c"]
        .iter()
        .map(|p| (entry(p, What::Dir), vec![]))
        .collect();
    assert_eq!(
        read(tight, &stage("b", &three), 64),
        Err(Refused::Over {
            what: "the stage's entries",
            bound: 2
        })
    );
    // The opening (its fixed part, the id `b`, a stage's count of layers) and the head alone, its 11 bytes
    // never sent: refused as soon as the head is whole.
    let big = raw(2, 0, 0o644, b"f", b"", 11);
    let head_only = &big[..OPEN_BYTES + 1 + 2 + HEAD_BYTES];
    assert_eq!(
        read(tight, head_only, 64),
        Err(Refused::Over {
            what: "the stage's file and xattr bytes",
            bound: 10
        })
    );
    let long_path = raw(1, 0, 0o755, b"abcdefghi", b"", 0);
    assert_eq!(
        read(tight, &long_path[..OPEN_BYTES + 1 + 2 + HEAD_BYTES], 64),
        Err(Refused::Over {
            what: "a path",
            bound: 8
        })
    );
    let long_target = raw(3, 0, 0o777, b"l", b"/abcdefgh", 0);
    assert_eq!(
        read(tight, &long_target[..OPEN_BYTES + 1 + 2 + HEAD_BYTES], 64),
        Err(Refused::Over {
            what: "a symbolic link's target",
            bound: 8
        })
    );
    // The packer refuses what the unpacker would.
    assert!(head(&entry("abcdefghi", What::Dir), &tight).is_err());
    assert!(head(&entry("../x", What::Dir), &LIMITS).is_err());
}

proptest! {
    /// HEX-GUEST-13: feed any bytes after a stage's opening: expect a refusal, steps, or a wait for
    /// more, never a panic, and never a name that leaves the root.
    #[test]
    fn any_bytes_are_read_or_refused(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let mut input = open(Op::Stage, "b").unwrap();
        input.extend(bytes);
        let mut u = Unpack::new(LIMITS);
        let mut rest = &input[..];
        while !rest.is_empty() {
            match u.take(rest) {
                Ok((took, step)) => {
                    if let Some(Step::Entry(e)) = step {
                        prop_assert!(check_path(&e.path, &LIMITS).is_ok());
                    }
                    rest = &rest[took..];
                }
                Err(_) => break,
            }
        }
    }
}

fn meta(uid: u32, gid: u32, xattrs: &[(&str, &[u8])]) -> Meta {
    Meta {
        uid,
        gid,
        mtime: Mtime {
            secs: 1_700_000_000,
            nanos: 500,
        },
        atime: Mtime {
            secs: 1_700_000_001,
            nanos: 0,
        },
        xattrs: xattrs
            .iter()
            .map(|(n, v)| Xattr {
                name: (*n).to_owned(),
                value: v.to_vec(),
            })
            .collect(),
    }
}

/// HEX-GUEST-13 (version 2): stage a directory, a file and a symlink each with an owner, an mtime and
/// extended attributes, and a hard link to the file: expect each read back as written, cut anywhere.
#[test]
fn version_2_entries_read_back_with_owners_times_xattrs_and_hard_links() {
    let with = |mut e: Entry, m: Meta| {
        e.meta = Some(m);
        e
    };
    let entries = vec![
        (with(entry("root", What::Dir), meta(472, 0, &[])), vec![]),
        (
            with(
                entry("root/caddy", What::File { size: 3 }),
                meta(
                    1001,
                    1001,
                    &[("security.capability", &[1, 0, 0, 2, 0, 4, 0, 0])],
                ),
            ),
            b"bin".to_vec(),
        ),
        (
            entry(
                "root/perl5",
                What::Hardlink {
                    target: "root/caddy".to_owned(),
                },
            ),
            vec![],
        ),
        (
            with(
                entry(
                    "root/lib",
                    What::Symlink {
                        target: "/usr/lib".to_owned(),
                    },
                ),
                meta(0, 42, &[("trusted.x", b"y"), ("user.a", b"")]),
            ),
            vec![],
        ),
    ];
    let bytes = stage("b", &entries);
    let mut want = vec![(
        Step::Open {
            op: Op::Stage,
            id: "b".to_owned(),
            layers: vec![],
        },
        vec![],
    )];
    want.extend(entries.into_iter().map(|(e, b)| (Step::Entry(e), b)));
    want.push((Step::End, vec![]));
    for cut in [1, 7, 29, bytes.len()] {
        assert_eq!(read(LIMITS, &bytes, cut).unwrap(), want, "cut {cut}");
    }
}

/// HEX-GUEST-13 (version 2): refuse, by type, an entry's id of u32::MAX, a time's nanoseconds of a second,
/// names together past Linux's `XATTR_LIST_MAX`, a name past 255 bytes, one in no Linux namespace, a value
/// past `XATTR_SIZE_MAX`, a hard link to itself or to no staged path, and a hard link with a size; each
/// refused by the packer, as the unpacker would.
#[test]
fn hostile_version_2_metadata_and_hard_links_are_refused_by_type() {
    let file = |m: Meta| Entry {
        meta: Some(m),
        ..entry("f", What::File { size: 0 })
    };
    let big = vec![0u8; XATTR_SIZE_MAX + 1];
    let long = format!("user.{}", "n".repeat(XATTR_NAME_MAX));
    // Names of XATTR_NAME_MAX bytes, each with its NUL, one more than XATTR_LIST_MAX holds.
    let many: Vec<(String, Vec<u8>)> = (0..=XATTR_LIST_MAX / (XATTR_NAME_MAX + 1))
        .map(|i| (format!("user.{i:0>250}"), vec![]))
        .collect();
    let many: Vec<(&str, &[u8])> = many
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_slice()))
        .collect();
    let mut late = meta(0, 0, &[]);
    late.mtime.nanos = 1_000_000_000;
    for m in [
        meta(u32::MAX, 0, &[]),
        meta(0, u32::MAX, &[]),
        late,
        meta(0, 0, &many),
        meta(0, 0, &[(long.as_str(), b"")]),
        meta(0, 0, &[("com.apple.provenance", b"x")]),
        meta(0, 0, &[("user.big", &big)]),
    ] {
        assert!(
            matches!(head(&file(m.clone()), &LIMITS), Err(Refused::Meta { .. })),
            "{m:?}"
        );
    }
    let link = |target: &str| {
        entry(
            "a",
            What::Hardlink {
                target: target.to_owned(),
            },
        )
    };
    for target in ["a", "../x", "/etc/passwd", ""] {
        assert!(
            matches!(head(&link(target), &LIMITS), Err(Refused::Target { .. })),
            "{target}"
        );
    }
    // A hard link with a size is refused from its head.
    let mut raw = open(Op::Stage, "b").unwrap();
    raw.extend([4u8, 0]);
    raw.extend(0o644u16.to_le_bytes());
    raw.extend(1u16.to_le_bytes());
    raw.extend(1u16.to_le_bytes());
    raw.extend(5u64.to_le_bytes());
    assert!(matches!(
        read(LIMITS, &raw, 64),
        Err(Refused::Field { field: "size", .. })
    ));
}

/// HEX-GUEST-13 (Ada's rule: no legacy kept without a named consumer): open a stream as version 1, which was
/// never released (packer and relay ship from one tree in one image), and as version 3, which isn't one:
/// expect each refused by name, saying version 2 is the one this node speaks, before any entry is read.
#[test]
fn a_stream_of_any_version_but_2_is_refused_by_name() {
    for version in [1u16, 3, 0] {
        let mut bytes = open(Op::Stage, "b").unwrap();
        bytes[4..6].copy_from_slice(&version.to_le_bytes());
        let mut plain = head(&entry("d", What::Dir), &LIMITS).unwrap();
        plain.extend(END);
        bytes.extend(plain);
        let got = read(LIMITS, &bytes, 64);
        assert_eq!(got, Err(Refused::Version { got: version }));
        let why = got.unwrap_err().to_string();
        assert!(
            why.contains(&format!("staging stream version {version} isn't read"))
                && why.contains("this node speaks version 2"),
            "{why}"
        );
    }
}

/// HEX-GUEST-13 (version 2, §4a's layers): stage two layers of one root, the second with a whiteout and an
/// opaque directory: expect each marker read back as written, cut anywhere; and a marker carrying a size,
/// a target or metadata refused by type.
#[test]
fn layers_whiteouts_and_opaque_markers_read_back_and_carry_nothing_else() {
    let entries = vec![
        (entry("root", What::Layer), vec![]),
        (entry("root/etc", What::Dir), vec![]),
        (
            entry("root/etc/motd", What::File { size: 2 }),
            b"hi".to_vec(),
        ),
        (entry("root", What::Layer), vec![]),
        (entry("root/etc/motd", What::Whiteout), vec![]),
        (entry("root/etc", What::Opaque), vec![]),
    ];
    let bytes = stage("b", &entries);
    let mut want = vec![(
        Step::Open {
            op: Op::Stage,
            id: "b".to_owned(),
            layers: vec![],
        },
        vec![],
    )];
    want.extend(entries.into_iter().map(|(e, b)| (Step::Entry(e), b)));
    want.push((Step::End, vec![]));
    for cut in [1, 5, bytes.len()] {
        assert_eq!(read(LIMITS, &bytes, cut).unwrap(), want, "cut {cut}");
    }
    for (kind, size, target, flags) in [(5u8, 1u64, 0u16, 0u8), (6, 0, 1, 0), (7, 0, 0, HAS_META)] {
        let mut raw = open(Op::Stage, "b").unwrap();
        raw.extend([kind, flags]);
        raw.extend(0u16.to_le_bytes());
        raw.extend(1u16.to_le_bytes());
        raw.extend(target.to_le_bytes());
        raw.extend(size.to_le_bytes());
        raw.push(b'r');
        assert!(
            matches!(read(LIMITS, &raw, 64), Err(Refused::Field { .. })),
            "kind {kind}"
        );
    }
}

/// HEX-GUEST-13 (version 2, Ada's rule on constants): an entry's xattrs are bounded by Linux's own limits
/// alone (include/uapi/linux/limits.h): stage one with more xattrs than the old count of 64, and one whose
/// values together pass `XATTR_SIZE_MAX` while each is within it: expect both read back as written.
#[test]
fn an_entrys_xattrs_are_bounded_by_linux_alone() {
    let names: Vec<String> = (0..65).map(|i| format!("user.{i}")).collect();
    let many: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"v"[..])).collect();
    let half = vec![7u8; XATTR_SIZE_MAX / 2 + 1];
    for m in [
        meta(0, 0, &many),
        meta(0, 0, &[("user.a", &half), ("user.b", &half)]),
    ] {
        let e = Entry {
            meta: Some(m.clone()),
            ..entry("f", What::File { size: 0 })
        };
        let got = read(LIMITS, &stage("b", &[(e.clone(), vec![])]), 4096).unwrap();
        assert_eq!(got[1].0, Step::Entry(e));
    }
}

/// HEX-GUEST-13 (version 2): an entry's xattr bytes spend the stage's byte budget, as file bytes do: stage
/// an entry whose xattrs pass what is left of it: expect it refused as over the budget from its metadata's
/// fixed part, before its xattrs are held.
#[test]
fn xattr_bytes_spend_the_stages_byte_budget() {
    let tight = Limits {
        entries: 4,
        bytes: 10,
        path: 64,
    };
    let e = Entry {
        meta: Some(meta(0, 0, &[("user.a", &[1u8; 6])])),
        ..entry("f", What::File { size: 0 })
    };
    let mut bytes = open(Op::Stage, "b").unwrap();
    bytes.extend(head(&e, &LIMITS).unwrap());
    // Cut at the end of the metadata's fixed part: the refusal comes before any xattr byte is taken.
    let fixed = bytes.len() - (1 + 4 + "user.a".len() + 6);
    assert!(matches!(
        read(tight, &bytes[..fixed], 4096),
        Err(Refused::Over {
            what: "the stage's file and xattr bytes",
            bound: 10
        })
    ));
}

/// HEX-GUEST-13 (version 2, a learned budget): a stage's budget reads back as written, and anything else
/// isn't one; an unpacker that learns a budget refuses an entry past it by type.
#[test]
fn a_stages_learned_budget_bounds_it() {
    let want = Budget {
        entries: 7,
        bytes: 1 << 40,
        present: vec![],
        capacity: 1 << 20,
        admitting: true,
    };
    assert_eq!(read_budget(&budget(&want)), Some(want.clone()));
    assert_eq!(read_budget(b"HEXB"), None);
    assert_eq!(read_budget(&[budget(&want), vec![0]].concat()), None);
    let three: Vec<(Entry, Vec<u8>)> = ["a", "b", "c"]
        .iter()
        .map(|p| (entry(p, What::Dir), vec![]))
        .collect();
    let bytes = stage("b", &three);
    let mut u = Unpack::new(LIMITS);
    u.learn(2, 1 << 20);
    let mut input = bytes.as_slice();
    let refused = loop {
        assert!(
            !input.is_empty(),
            "the stage was read whole past its budget"
        );
        match u.take(input) {
            Ok((took, _)) => input = &input[took..],
            Err(why) => break why,
        }
    };
    assert_eq!(
        refused,
        Refused::Over {
            what: "the stage's entries",
            bound: 2
        }
    );
}

/// HEX-GUEST-13 (§4a, the guest's layer store): open a stage naming its root's 9 layers, 7 by digest and 2
/// unnamed (a tarball whose digest the host doesn't know yet, staged into the box), and read it back fed a
/// byte at a time and whole: expect each layer's digest or none, in order. A layer whose named byte is
/// neither 0 nor 1, and an unnamed one with digest bytes, are refused as hostile; a stage naming more layers
/// than overlayfs stacks (`MAX_LAYERS`) is refused from its count, before any layer is held. A budget answer
/// saying which of 9 layers the guest holds, and how many layers its store can hold, reads back as written.
#[test]
fn a_stages_layers_are_named_by_digest_or_unnamed_and_the_guest_says_which_it_holds() {
    let layers: Vec<Option<Digest>> = (0..9u8)
        .map(|i| {
            if i == 2 || i == 8 {
                None
            } else {
                Some([i; 32])
            }
        })
        .collect();
    let mut bytes = open_stage("b", &layers).unwrap();
    bytes.extend(END);
    for cut in [1, 7, bytes.len()] {
        let got = read(LIMITS, &bytes, cut).unwrap();
        assert_eq!(
            got[0].0,
            Step::Open {
                op: Op::Stage,
                id: "b".to_owned(),
                layers: layers.clone(),
            },
            "cut {cut}"
        );
    }
    // The opening's first layer starts after its count: its named byte, then its digest.
    let first = open_stage("b", &[]).unwrap().len();
    let mut odd = open_stage("b", &layers).unwrap();
    odd[first] = 2;
    assert!(
        matches!(read(LIMITS, &odd, 64), Err(Refused::Layer(_))),
        "{:?}",
        read(LIMITS, &odd, 64)
    );
    let mut stray = open_stage("b", &layers).unwrap();
    let unnamed = first + 2 * (1 + DIGEST_BYTES);
    assert_eq!(stray[unnamed], 0);
    stray[unnamed + 1] = 9;
    assert!(matches!(read(LIMITS, &stray, 64), Err(Refused::Layer(_))));
    let mut over = open_stage("b", &[]).unwrap();
    over.truncate(over.len() - 2);
    over.extend(
        u16::try_from(crate::bundle::MAX_LAYERS + 1)
            .unwrap()
            .to_le_bytes(),
    );
    assert_eq!(
        read(LIMITS, &over, 64),
        Err(Refused::Over {
            what: "the stage's layers",
            bound: u64::try_from(crate::bundle::MAX_LAYERS).unwrap()
        })
    );
    let held = Budget {
        entries: 1,
        bytes: 2,
        present: vec![true, false, true, true, false, false, false, true, true],
        capacity: 3,
        admitting: false,
    };
    assert_eq!(read_budget(&budget(&held)), Some(held.clone()));
    // Its admitting byte is 0 or 1, else it isn't a budget.
    let mut odd = budget(&held);
    *odd.last_mut().unwrap() = 2;
    assert_eq!(read_budget(&odd), None);
    assert_eq!(read_budget(&budget(&held)[..budget(&held).len() - 1]), None);
}
