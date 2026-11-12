//! The packer's tarball roots (docs/design/shards-guest.md §4a, HEX-GUEST-15; native plan step 5): a root
//! given as a `.tar` or `.tar.gz` packs to its own entries, Linux modes and symlink texts exact; and
//! hostile archives are refused by name before anything past the fault is sent: absolute names, `..`,
//! names and counts past the bounds, device, FIFO and hard-link entries, an unknown entry type, an entry
//! before its directory, a gzip stream whose trailer lies, an archive cut short; and, from the lead's list,
//! an entry written through a link the archive made (now packed, for the relay to resolve within the root:
//! the lead's ruling, §4a), hard links whatever their target, GNU and PAX long
//! names (whole within the path bound, refused past it), a PAX size, a dangling long-name record, sparse
//! entries. Tar has no entry type for a socket, so an unknown typeflag stands in for one.

use std::collections::BTreeMap;

use super::tests::{id, layers_of, pack, unpack};
use super::*;
use hex_core::stream::files::LIMITS;

/// A raw tar header for `name`, of `kind` (its typeflag byte), `mode`, `size`, linking to `link`.
fn header(name: &[u8], kind: u8, mode: u32, size: u64, link: &[u8]) -> [u8; 512] {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name);
    let octal = |field: &mut [u8], v: u64| {
        let s = format!("{:0width$o}\0", v, width = field.len() - 1);
        field.copy_from_slice(s.as_bytes());
    };
    octal(&mut h[100..108], u64::from(mode));
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], 0);
    h[156] = kind;
    h[157..157 + link.len()].copy_from_slice(link);
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
    let s = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(s.as_bytes());
    h
}

/// An archive of `entries` (each a header and its data), ended with its two zero blocks.
fn archive(entries: &[([u8; 512], &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (h, data) in entries {
        out.extend_from_slice(h);
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.extend_from_slice(&[0u8; 1024]);
    out
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(bytes).unwrap();
    gz.finish().unwrap()
}

/// Packs `bytes` as the tarball `file` (its name says whether it's gzip) under the item `root`.
fn pack_tar(file: &str, bytes: &[u8], limits: &Limits) -> Result<Vec<u8>, Refusal> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(file);
    std::fs::write(&path, bytes).unwrap();
    let items = [Item {
        name: "root".to_owned(),
        source: Source::Tar(path),
    }];
    let mut out = Vec::new();
    pack(&mut out, &id(), &items, limits).map(|()| out)
}

fn refused(file: &str, bytes: &[u8]) -> String {
    pack_tar(file, bytes, &LIMITS).unwrap_err().to_string()
}

fn sample() -> Vec<u8> {
    archive(&[
        (header(b"./", b'5', 0o755, 0, b""), b""),
        (header(b"./bin/", b'5', 0o755, 0, b""), b""),
        (header(b"./bin/busybox", b'0', 0o4755, 5, b""), b"#!bb\n"),
        (
            header(b"./bin/ls", b'2', 0o777, 0, b"/usr/bin/busybox"),
            b"",
        ),
        (header(b"./etc/", b'5', 0o700, 0, b""), b""),
        (header(b"./etc/empty", b'0', 0o600, 0, b""), b""),
    ])
}

/// HEX-GUEST-15: pack a root given as a tarball, plain and gzip: expect its entries under the item's name,
/// each with the archive's own mode, the file's bytes, and the symlink's text exactly as written.
#[test]
fn a_tarball_root_packs_to_its_own_entries_modes_and_link_texts_exact() {
    for (file, bytes) in [("root.tar", sample()), ("root.tar.gz", gzip(&sample()))] {
        let entries = unpack(&pack_tar(file, &bytes, &LIMITS).unwrap());
        let by: BTreeMap<&str, &(Entry, Vec<u8>)> = entries
            .iter()
            .filter(|e| e.0.what != What::Layer)
            .map(|e| (e.0.path.as_str(), e))
            .collect();
        assert_eq!(by["root"].0.what, What::Dir, "{file}");
        assert_eq!(by["root/bin/busybox"].0.mode, 0o4755, "{file}");
        assert_eq!(by["root/bin/busybox"].1, b"#!bb\n", "{file}");
        assert_eq!(
            by["root/bin/ls"].0.what,
            What::Symlink {
                target: "/usr/bin/busybox".to_owned()
            },
            "{file}"
        );
        assert_eq!(by["root/etc"].0.mode, 0o700, "{file}");
        assert_eq!(
            by["root/etc/empty"].0.what,
            What::File { size: 0 },
            "{file}"
        );
        assert_eq!(by["root"].0.what, What::Dir, "{file}");
        assert_eq!(entries[1].0.what, What::Layer, "{file}: the layer's marker");
        assert_eq!(entries.len(), 7, "{file}");
    }
}

/// HEX-GUEST-15: pack archives naming their way out (an absolute name, a `..` component): expect each
/// refused naming the archive's entry and why; and one whose entry's directory no entry makes: expect it
/// packed as named, for the relay to make its parents as containerd's `mkparent` does (§4a).
#[test]
fn a_tarball_naming_its_way_out_is_refused_by_name() {
    for (name, why) in [
        (&b"/etc/passwd"[..], "is absolute"),
        (b"a/../../etc/passwd", "has a '..' component"),
    ] {
        let bytes = archive(&[(header(name, b'0', 0o644, 1, b""), b"x")]);
        let got = refused("r.tar", &bytes);
        assert!(got.contains(why), "{name:?}: {got}");
    }
    let bytes = archive(&[(header(b"nodir/file", b'0', 0o644, 1, b""), &b"x"[..])]);
    let packed = unpack(&pack_tar("r.tar", &bytes, &LIMITS).unwrap());
    assert_eq!(packed[2].0.path, "root/nodir/file");
}

/// HEX-GUEST-15: pack archives holding a character device, a block device, a FIFO, a hard link and an entry
/// of a type tar doesn't define (as a socket would be, which tar can't carry): expect each refused by name.
#[test]
fn a_tarball_with_a_device_fifo_hard_link_or_unknown_kind_is_refused_by_name() {
    for (kind, why) in [
        (b'3', "a device node is never staged"),
        (b'4', "a device node is never staged"),
        (b'6', "a FIFO is never staged"),
        (b'1', "which is no earlier file or link this layer made"),
        (b'Z', "an entry of a kind staging carries not"),
    ] {
        let bytes = archive(&[(header(b"x", kind, 0o644, 0, b"y"), b"")]);
        let got = refused("r.tar", &bytes);
        assert!(got.contains(why), "{}: {got}", kind as char);
    }
}

/// HEX-GUEST-15: pack a tarball past the entries bound, past the bytes bound, and with a name past the path
/// bound: expect each refused naming the bound, before the entry past it is sent.
#[test]
fn a_tarball_past_its_bounds_is_refused() {
    let three = archive(&[
        (header(b"a", b'0', 0o644, 4, b""), b"1234"),
        (header(b"b", b'0', 0o644, 4, b""), b"1234"),
        (header(b"c", b'0', 0o644, 4, b""), b"1234"),
    ]);
    let few = Limits {
        entries: 3,
        ..LIMITS
    };
    assert_eq!(
        pack_tar("r.tar", &three, &few),
        Err(Refusal::Exhausted {
            what: "entries in one stage",
            bound: 3
        })
    );
    let small = Limits { bytes: 7, ..LIMITS };
    assert_eq!(
        pack_tar("r.tar", &three, &small),
        Err(Refusal::Exhausted {
            what: "file and xattr bytes in one stage",
            bound: 7
        })
    );
    let short = Limits { path: 8, ..LIMITS };
    let long = archive(&[(header(b"abcdefghij", b'0', 0o644, 0, b""), b"")]);
    assert!(pack_tar("r.tar", &long, &short).is_err());
}

/// HEX-GUEST-15 (§4a; the lead's rule, 2026-11-06: a layer is hashed only as it crosses, to verify it):
/// pack an image's two layers as its blobs, named by their digests and with no extension, the lower gzip and
/// the upper plain: expect both packed, each read as its own bytes' magic says. The upper's bytes changed
/// after its digest was taken: expect the stage refused naming the blob, its bytes' sha256 and the digest it
/// was given, and nothing sent after its entries (no next layer's marker, no end: the relay never keeps
/// it). A zstd blob: refused by name, as no compression the packer reads.
#[test]
fn layer_blobs_are_read_by_their_magic_and_refused_when_their_bytes_arent_their_digest() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = dir.path().join("blobs/sha256");
    std::fs::create_dir_all(&blobs).unwrap();
    let lower = gzip(&archive(&[(header(b"a", b'0', 0o644, 1, b""), &b"a"[..])]));
    let upper = archive(&[(header(b"b", b'0', 0o644, 1, b""), &b"b"[..])]);
    let blob = |bytes: &[u8]| {
        let digest: files::Digest = <sha2::Sha256 as sha2::Digest>::digest(bytes).into();
        let path = blobs.join(files::digest_hex(&digest));
        std::fs::write(&path, bytes).unwrap();
        Layer {
            path,
            digest: Some(digest),
        }
    };
    let layers = vec![blob(&lower), blob(&upper)];
    let pack_layers = |layers: &[Layer]| {
        let items = [Item {
            name: "root".to_owned(),
            source: Source::Layers(layers.to_vec()),
        }];
        let mut out = Vec::new();
        let packed = pack(&mut out, &id(), &items, &LIMITS);
        (packed, out)
    };
    let (packed, out) = pack_layers(&layers);
    packed.unwrap();
    let paths: Vec<String> = unpack(&out).into_iter().map(|e| e.0.path).collect();
    assert_eq!(paths, ["root", "root/a", "root", "root/b"]);

    // The upper blob's bytes, changed after its digest was taken.
    let changed = archive(&[(header(b"b", b'0', 0o644, 1, b""), &b"c"[..])]);
    std::fs::write(&layers[1].path, &changed).unwrap();
    let got = files::digest_hex(&<sha2::Sha256 as sha2::Digest>::digest(&changed).into());
    let (packed, out) = pack_layers(&layers);
    let why = packed.unwrap_err().to_string();
    assert!(
        why.contains(&layers[1].path.display().to_string())
            && why.contains(&got)
            && why.contains(&files::digest_hex(&layers[1].digest.unwrap())),
        "{why}"
    );
    assert!(!out.ends_with(&files::END), "an end was sent after it");

    // A zstd frame's magic (RFC 8878 §3.1.1): no compression the packer reads.
    let zstd = blob(&[0x28, 0xb5, 0x2f, 0xfd, 0, 0, 0, 0]);
    let (packed, _) = pack_layers(std::slice::from_ref(&zstd));
    let why = packed.unwrap_err().to_string();
    assert!(
        why.contains(&zstd.path.display().to_string()) && why.contains("zstd"),
        "{why}"
    );
}

/// HEX-GUEST-15 (§4a; the lead's rule after HEX-GUEST-22's 2x diagnosis: verify only what the store may
/// admit): pack a layer whose bytes aren't its digest for a stage that admits nothing (every layer into the
/// box, as whole-root staging, which checked none): expect it packed, unchecked; for a stage that may admit:
/// expect it refused, its digest checked.
#[test]
fn only_a_stage_that_may_admit_checks_its_layers_digests() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("layer.tar");
    std::fs::write(
        &path,
        archive(&[(header(b"a", b'0', 0o644, 1, b""), &b"a"[..])]),
    )
    .unwrap();
    let items = [Item {
        name: "root".to_owned(),
        source: Source::Layers(vec![Layer {
            path,
            digest: Some([0x0b; 32]),
        }]),
    }];
    assert_eq!(
        pack_items(&mut Vec::new(), &items, &LIMITS, &[false], false),
        Ok(vec![])
    );
    let why = pack_items(&mut Vec::new(), &items, &LIMITS, &[false], true)
        .unwrap_err()
        .to_string();
    assert!(why.contains(&files::digest_hex(&[0x0b; 32])), "{why}");
}

/// HEX-GUEST-15 (§4a; the lead's check, 2026-11-06: what whole-root staging took, the store takes): pack a
/// layer as `docker save` writes it, `<id>/layer.tar`, its name no digest and none known: expect it packed,
/// and its digest learned as it crossed, its bytes' sha256, with its file's identity, for the next create
/// to name it by without reading it.
#[test]
fn a_layer_of_no_known_digest_is_hashed_as_it_crosses_and_its_digest_learned() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("0f3e").join("layer.tar");
    std::fs::create_dir(path.parent().unwrap()).unwrap();
    let bytes = archive(&[(header(b"a", b'0', 0o644, 1, b""), &b"a"[..])]);
    std::fs::write(&path, &bytes).unwrap();
    let items = [Item {
        name: "root".to_owned(),
        source: Source::Layers(vec![Layer {
            path: path.clone(),
            digest: None,
        }]),
    }];
    let mut out = Vec::new();
    let learned = pack_items(&mut out, &items, &LIMITS, &[false], false).unwrap();
    assert_eq!(
        learned,
        [Learned {
            path: path.clone(),
            digest: <sha2::Sha256 as sha2::Digest>::digest(&bytes).into(),
            identity: identity(&std::fs::metadata(&path).unwrap()),
        }]
    );
    let paths: Vec<String> = unpack(
        &[
            files::open_stage("box-1", &[None]).unwrap(),
            out,
            files::END.to_vec(),
        ]
        .concat(),
    )
    .into_iter()
    .map(|e| e.0.path)
    .collect();
    assert_eq!(paths, ["root", "root/a"]);
}

/// HEX-GUEST-15: pack a gzip tarball whose trailer lies (its length, then its CRC), an archive cut in the
/// middle of a file, and one cut before its end-of-archive block: expect each refused, never staged as whole.
#[test]
fn a_lying_or_truncated_tarball_is_refused() {
    let good = gzip(&sample());
    let n = good.len();
    for at in [n - 1, n - 5] {
        let mut lies = good.clone();
        lies[at] ^= 0x01;
        let got = refused("r.tar.gz", &lies);
        assert!(got.contains("r.tar.gz"), "trailer byte {at}: {got}");
    }
    let plain = sample();
    let mid_file = &plain[..512 * 3 + 2];
    assert!(refused("r.tar", mid_file).contains("r.tar"));
    let no_end = &plain[..plain.len() - 1024];
    let got = refused("r.tar", no_end);
    assert!(got.contains("end-of-archive"), "{got}");
    let cut_gz = &good[..n / 2];
    assert!(refused("r.tar.gz", cut_gz).contains("r.tar.gz"));
}

/// A PAX record (`%d key=value\n`, its length counting itself) of `pairs`.
fn pax(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in pairs {
        let body = format!(" {k}={v}\n");
        let mut n = body.len() + 1;
        while format!("{n}").len() + body.len() != n {
            n += 1;
        }
        out.extend(format!("{n}{body}").into_bytes());
    }
    out
}

/// HEX-GUEST-15 (the lead's ruling, §4a): pack an entry written through a symbolic link the archive made
/// (`a` to `/`, to `../..`, and through a chain `a` to `b` to `/`): expect each packed as named, for the
/// relay to resolve within the root as containerd does.
#[test]
fn an_entry_written_through_a_link_in_the_archive_is_packed_for_the_relay_to_resolve() {
    for (entries, under) in [
        (
            vec![
                (header(b"a", b'2', 0o777, 0, b"/"), &b""[..]),
                (header(b"a/x/", b'5', 0o755, 0, b""), b""),
            ],
            What::Dir,
        ),
        (
            vec![
                (header(b"a", b'2', 0o777, 0, b"../.."), &b""[..]),
                (header(b"a/x", b'0', 0o644, 1, b""), b"x"),
            ],
            What::File { size: 1 },
        ),
        (
            vec![
                (header(b"b", b'2', 0o777, 0, b"/"), &b""[..]),
                (header(b"a", b'2', 0o777, 0, b"b"), b""),
                (header(b"a/x", b'0', 0o644, 1, b""), b"x"),
            ],
            What::File { size: 1 },
        ),
    ] {
        let packed = unpack(&pack_tar("r.tar", &archive(&entries), &LIMITS).unwrap());
        let x = packed.iter().find(|e| e.0.path == "root/a/x").unwrap();
        assert_eq!(x.0.what, under);
    }
}

/// HEX-GUEST-15 (§4a): pack two layers, the second whiting out `etc/a`, making `d` opaque and replacing
/// `f`: expect a layer marker before each layer's entries, the whiteout and opaque marker as kinds of
/// their own (never files), a hard link to what the whiteout or opaque marker removed, or to an earlier
/// layer's file, refused (overlay's unpack fails it), and to this layer's file staged; and a whiteout
/// naming nothing refused.
#[test]
fn layers_pack_in_order_with_whiteouts_and_opaque_markers_as_their_own_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let one = archive(&[
        (header(b"etc/", b'5', 0o755, 0, b""), &b""[..]),
        (header(b"etc/a", b'0', 0o644, 1, b""), b"a"),
        (header(b"etc/b", b'0', 0o644, 1, b""), b"b"),
        (header(b"d/", b'5', 0o755, 0, b""), b""),
        (header(b"d/old", b'0', 0o644, 1, b""), b"o"),
        (header(b"f", b'0', 0o644, 1, b""), b"f"),
    ]);
    let pack_layers = |two: Vec<([u8; 512], &[u8])>| {
        std::fs::write(dir.path().join("1.tar"), &one).unwrap();
        std::fs::write(dir.path().join("2.tar"), archive(&two)).unwrap();
        let items = [Item {
            name: "root".to_owned(),
            source: Source::Layers(layers_of(&[
                dir.path().join("1.tar"),
                dir.path().join("2.tar"),
            ])),
        }];
        let mut out = Vec::new();
        pack(&mut out, &id(), &items, &LIMITS).map(|()| out)
    };
    let two = vec![
        (header(b"etc/.wh.a", b'0', 0o644, 0, b""), &b""[..]),
        (header(b"d/", b'5', 0o755, 0, b""), b""),
        (header(b"d/.wh..wh..opq", b'0', 0o644, 0, b""), b""),
        (header(b"d/new", b'0', 0o644, 1, b""), b"n"),
        (header(b"f/", b'5', 0o755, 0, b""), b""),
        (header(b"h", b'1', 0o644, 0, b"d/new"), b""),
    ];
    let packed = unpack(&pack_layers(two).unwrap());
    let kinds: Vec<(&str, &What)> = packed
        .iter()
        .map(|e| (e.0.path.as_str(), &e.0.what))
        .collect();
    let layers: Vec<usize> = kinds
        .iter()
        .enumerate()
        .filter(|(_, k)| *k.1 == What::Layer)
        .map(|(i, _)| i)
        .collect();
    // A layered root's layers go to the guest's store: no directory of the box's comes before them.
    assert_eq!(layers, [0, 7], "{kinds:?}");
    assert_eq!(kinds[8], ("root/etc/a", &What::Whiteout));
    assert_eq!(kinds[9], ("root/d", &What::Dir));
    assert_eq!(kinds[10], ("root/d", &What::Opaque));
    assert_eq!(kinds[12], ("root/f", &What::Dir));
    assert_eq!(
        kinds[13],
        (
            "root/h",
            &What::Hardlink {
                target: "root/d/new".to_owned()
            }
        )
    );
    // What a whiteout or opaque marker removed, and a file an earlier layer made and nothing removed:
    // overlay's unpack fails a hard link to any of them.
    for gone in [&b"etc/a"[..], b"d/old", b"f", b"etc/b"] {
        let mut two = vec![
            (header(b"etc/.wh.a", b'0', 0o644, 0, b""), &b""[..]),
            (header(b"d/.wh..wh..opq", b'0', 0o644, 0, b""), b""),
            (header(b"f/", b'5', 0o755, 0, b""), b""),
        ];
        two.push((header(b"h", b'1', 0o644, 0, gone), b""));
        let got = pack_layers(two).unwrap_err().to_string();
        let why = if gone == b"etc/b" {
            "a file an earlier layer made"
        } else {
            "which is no earlier file or link this layer made"
        };
        assert!(got.contains(why), "{gone:?}: {got}");
    }
    let got = pack_layers(vec![(header(b"etc/.wh.", b'0', 0o644, 0, b""), &b""[..])])
        .unwrap_err()
        .to_string();
    assert!(got.contains("a whiteout that names nothing"), "{got}");
}

/// HEX-GUEST-15 (2, §4a as amended): pack a hard link to an earlier regular file of the root: expect it
/// staged as another name for that file, carrying nothing of its own; and one whose target escapes the
/// root, names a later entry or none, or a directory: expect each refused naming the target, whatever it is.
#[test]
fn a_hard_link_stages_to_an_earlier_file_and_is_refused_to_anything_else() {
    let ok = archive(&[
        (header(b"usr/", b'5', 0o755, 0, b""), &b""[..]),
        (header(b"usr/perl5.36.0", b'0', 0o755, 4, b""), b"perl"),
        (header(b"usr/perl", b'1', 0o755, 0, b"usr/perl5.36.0"), b""),
    ]);
    let entries = unpack(&pack_tar("r.tar", &ok, &LIMITS).unwrap());
    let link = entries
        .iter()
        .find(|e| e.0.path == "root/usr/perl")
        .unwrap();
    assert_eq!(
        link.0.what,
        What::Hardlink {
            target: "root/usr/perl5.36.0".to_owned()
        }
    );
    assert_eq!(
        link.0.meta, None,
        "a hard link carries its target's, not its own"
    );
    for (target, why) in [(&b"/etc/passwd"[..]), b"../x"]
        .into_iter()
        .map(|t| (t, "which names its way out"))
    {
        let all = vec![(header(b"x", b'1', 0o644, 0, target), &b""[..])];
        let got = refused("r.tar", &archive(&all));
        assert!(got.contains(why), "{target:?}: {got}");
    }
    for (target, entries) in [
        (
            &b"z"[..],
            vec![(header(b"z", b'0', 0o644, 1, b""), &b"z"[..])],
        ),
        (b"nope", vec![]),
        (b"d", vec![]),
    ] {
        let mut all = vec![(header(b"d/", b'5', 0o755, 0, b""), &b""[..])];
        all.push((header(b"x", b'1', 0o644, 0, target), b""));
        all.extend(entries);
        let got = refused("r.tar", &archive(&all));
        assert!(
            got.contains("which is no earlier file or link this layer made"),
            "{target:?}: {got}"
        );
    }
}

/// HEX-GUEST-15 (3): pack names and link targets longer than a header holds, as GNU long-name ('L') and
/// long-link ('K') records and as a PAX record's `path` and `linkpath`: expect each staged with its whole
/// text exact; past the path bound, refused by name; a PAX `size` refused; and a long-name record with no
/// entry after it refused.
#[test]
fn long_names_and_targets_stage_whole_within_the_bound_and_are_refused_past_it() {
    let long = format!("dir-{}", "n".repeat(180));
    let target = format!("/{}", "t".repeat(200));
    let gnu = archive(&[
        (
            header(b"././@LongLink", b'L', 0o644, long.len() as u64 + 1, b""),
            format!("{long}\0").as_bytes(),
        ),
        (header(b"short", b'5', 0o755, 0, b""), b""),
        (
            header(b"././@LongLink", b'K', 0o644, target.len() as u64, b""),
            target.as_bytes(),
        ),
        (header(b"link", b'2', 0o777, 0, b"short"), b""),
    ]);
    let px = archive(&[
        (
            header(
                b"PaxHeaders/x",
                b'x',
                0o644,
                pax(&[("path", &long)]).len() as u64,
                b"",
            ),
            &pax(&[("path", &long)]),
        ),
        (header(b"short", b'5', 0o755, 0, b""), b""),
        (
            header(
                b"PaxHeaders/l",
                b'x',
                0o644,
                pax(&[("linkpath", &target)]).len() as u64,
                b"",
            ),
            &pax(&[("linkpath", &target)]),
        ),
        (header(b"link", b'2', 0o777, 0, b"short"), b""),
    ]);
    for (what, bytes) in [("gnu", gnu), ("pax", px)] {
        let entries = unpack(&pack_tar("r.tar", &bytes, &LIMITS).unwrap());
        let by: BTreeMap<&str, &(Entry, Vec<u8>)> =
            entries.iter().map(|e| (e.0.path.as_str(), e)).collect();
        assert_eq!(
            by[format!("root/{long}").as_str()].0.what,
            What::Dir,
            "{what}"
        );
        assert_eq!(
            by["root/link"].0.what,
            What::Symlink {
                target: target.clone()
            },
            "{what}"
        );
    }
    let short = Limits {
        path: 100,
        ..LIMITS
    };
    let past = archive(&[
        (
            header(b"././@LongLink", b'L', 0o644, long.len() as u64 + 1, b""),
            format!("{long}\0").as_bytes(),
        ),
        (header(b"short", b'5', 0o755, 0, b""), b""),
    ]);
    let got = pack_tar("r.tar", &past, &short).unwrap_err().to_string();
    assert!(got.contains("past the path bound"), "{got}");
    let sized = archive(&[
        (
            header(
                b"PaxHeaders/s",
                b'x',
                0o644,
                pax(&[("size", "100")]).len() as u64,
                b"",
            ),
            &pax(&[("size", "100")]),
        ),
        (header(b"f", b'0', 0o644, 1, b""), b"x"),
    ]);
    assert!(refused("r.tar", &sized).contains("a PAX size"), "pax size");
    let dangling = archive(&[(header(b"././@LongLink", b'L', 0o644, 6, b""), b"name\0\0")]);
    assert!(
        refused("r.tar", &dangling).contains("no entry after it"),
        "dangling long name"
    );
}

/// HEX-GUEST-15 (4): pack a GNU sparse entry ('S') and a PAX one (`GNU.sparse.*`): expect each refused by
/// name; none of the roots it serves holds one (`tar -tv`, docs/design/shards-guest.md §4a).
#[test]
fn a_sparse_entry_is_refused_by_name() {
    let gnu = archive(&[(header(b"s", b'S', 0o644, 0, b""), &b""[..])]);
    assert!(refused("r.tar", &gnu).contains("a sparse entry"), "gnu");
    let record = pax(&[("GNU.sparse.major", "1"), ("GNU.sparse.minor", "0")]);
    let px = archive(&[
        (
            header(b"PaxHeaders/s", b'x', 0o644, record.len() as u64, b""),
            &record,
        ),
        (header(b"s", b'0', 0o644, 0, b""), b""),
    ]);
    assert!(refused("r.tar", &px).contains("a sparse entry"), "pax");
}

/// A global PAX record ('g') of `pairs`, then one plain file, as an archive.
fn global(pairs: &[(&str, &str)]) -> Vec<u8> {
    let record = pax(pairs);
    archive(&[
        (
            header(b"pax_global_header", b'g', 0o644, record.len() as u64, b""),
            &record,
        ),
        (header(b"f", b'0', 0o644, 1, b""), b"x"),
    ])
}

/// HEX-GUEST-15 (the lead's question on global PAX records): pack a global record whose comment is past
/// its key's longest line, one with GNU sparse keys, and one setting `path`, `linkpath` or `size`: expect
/// each refused by name, never held whole nor ignored; and one with only keys that don't change what's
/// staged (times, ids, names, a comment, charsets) packed.
#[test]
fn a_global_pax_record_is_bounded_and_refused_when_it_sets_what_staging_applies() {
    let big = "c".repeat(70 << 10);
    let got = refused("r.tar", &global(&[("comment", &big)]));
    assert!(
        got.contains("a PAX record")
            && got.contains("its comment line of")
            && got.contains("past the"),
        "{got}"
    );
    let got = refused("r.tar", &global(&[("GNU.sparse.major", "1")]));
    assert!(got.contains("a sparse entry"), "{got}");
    for key in ["path", "linkpath", "size", "uid", "gid", "mtime"] {
        let got = refused("r.tar", &global(&[(key, "1")]));
        assert!(
            got.contains(&format!(
                "a global PAX record sets {key}, which staging doesn't apply"
            )),
            "{key}: {got}"
        );
    }
    let quiet = global(&[
        ("atime", "1700000000"),
        ("ctime", "1700000000"),
        ("uname", "root"),
        ("gname", "root"),
        ("comment", "made by git archive"),
        ("charset", "ISO-IR 10646 2000 UTF-8"),
        ("hdrcharset", "ISO-IR 10646 2000 UTF-8"),
    ]);
    let entries = unpack(&pack_tar("r.tar", &quiet, &LIMITS).unwrap());
    assert_eq!(
        entries.len(),
        3,
        "the root, its layer's marker and its file"
    );
}

/// HEX-GUEST-15 (the lead's 'g' list, 5): pack a global record setting a key on neither list (neither one
/// staging applies nor one it may ignore): expect it refused naming that key, so a real archive that
/// meets one shows exactly what to weigh.
#[test]
fn a_global_pax_record_with_an_unknown_key_is_refused_naming_it() {
    let got = refused("r.tar", &global(&[("comment", "ok"), ("HEX.made.up", "1")]));
    assert!(
        got.contains("a global PAX record sets HEX.made.up, which staging doesn't apply"),
        "{got}"
    );
}

/// HEX-GUEST-15 (§4a as amended): pack entries owned 472:0 and 1001:1001 by their headers, one whose PAX
/// record sets a uid, gid, fractional mtime and later atime past what a header holds, and xattrs as SCHILY (raw) and
/// LIBARCHIVE (percent-encoded name, base64 value), with one in macOS's namespace: expect each staged with
/// its owner, mtime and Linux xattrs exact, and the macOS one left out.
#[test]
fn a_tarballs_owners_times_and_xattrs_are_carried() {
    let mut owned = header(b"data", b'5', 0o750, 0, b"");
    let octal = |field: &mut [u8], v: u64| {
        let s = format!("{:0width$o}\0", v, width = field.len() - 1);
        field.copy_from_slice(s.as_bytes());
    };
    octal(&mut owned[108..116], 472);
    octal(&mut owned[136..148], 1_700_000_123);
    let resum = |h: &mut [u8; 512]| {
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    };
    resum(&mut owned);
    let record = pax(&[
        ("uid", "70000"),
        ("gid", "1001"),
        ("mtime", "1700000000.25"),
        ("atime", "1800000000.5"),
        ("SCHILY.xattr.security.capability", "\u{1}\0\0\u{2}"),
        ("LIBARCHIVE.xattr.user.n%61me", "dmFsdWU="),
        ("SCHILY.xattr.com.apple.provenance", "x"),
    ]);
    let bytes = archive(&[
        (owned, &b""[..]),
        (
            header(b"PaxHeaders/caddy", b'x', 0o644, record.len() as u64, b""),
            &record,
        ),
        (header(b"caddy", b'0', 0o755, 3, b""), b"bin"),
    ]);
    let entries = unpack(&pack_tar("r.tar", &bytes, &LIMITS).unwrap());
    let by: BTreeMap<&str, &(Entry, Vec<u8>)> =
        entries.iter().map(|e| (e.0.path.as_str(), e)).collect();
    let data = by["root/data"].0.meta.clone().unwrap();
    assert_eq!((data.uid, data.gid), (472, 0));
    assert_eq!(
        data.mtime,
        Mtime {
            secs: 1_700_000_123,
            nanos: 0
        }
    );
    assert_eq!(
        data.atime, data.mtime,
        "no atime: the mtime, as containerd's"
    );
    assert!(data.xattrs.is_empty());
    let caddy = by["root/caddy"].0.meta.clone().unwrap();
    assert_eq!((caddy.uid, caddy.gid), (70_000, 1001));
    assert_eq!(
        caddy.mtime,
        Mtime {
            secs: 1_700_000_000,
            nanos: 250_000_000
        }
    );
    assert_eq!(
        caddy.atime,
        Mtime {
            secs: 1_800_000_000,
            nanos: 500_000_000
        },
        "a later atime is kept"
    );
    assert_eq!(
        caddy.xattrs,
        vec![
            Xattr {
                name: "security.capability".to_owned(),
                value: vec![1, 0, 0, 2],
            },
            Xattr {
                name: "user.name".to_owned(),
                value: b"value".to_vec(),
            },
        ]
    );
}

/// HEX-GUEST-15 (§4a): pack an entry whose PAX record's xattr names together pass Linux's
/// `XATTR_LIST_MAX`, one whose LIBARCHIVE value isn't base64, and one whose uid isn't a number: expect
/// each refused by name.
#[test]
fn hostile_owners_and_xattrs_in_a_tarball_are_refused_by_name() {
    let many: Vec<(String, String)> = (0..=files::XATTR_LIST_MAX / (files::XATTR_NAME_MAX + 1))
        .map(|i| (format!("SCHILY.xattr.user.{i:0>250}"), "v".to_owned()))
        .collect();
    let many: Vec<(&str, &str)> = many.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    for (pairs, why) in [
        (many, "XATTR_LIST_MAX"),
        (vec![("LIBARCHIVE.xattr.user.x", "%%%")], "isn't base64"),
        (vec![("uid", "root")], "its uid isn't an id"),
    ] {
        let record = pax(&pairs);
        let bytes = archive(&[
            (
                header(b"PaxHeaders/f", b'x', 0o644, record.len() as u64, b""),
                &record,
            ),
            (header(b"f", b'0', 0o644, 1, b""), b"x"),
        ]);
        let got = refused("r.tar", &bytes);
        assert!(got.contains(why), "{why}: {got}");
    }
}

/// Runs `git` in `dir` with `args`, feeding it `input`; its stdout. A commit is made by a fixed author with
/// signing off, so it runs on any host's git as configured.
fn git(dir: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut child = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=hex-test",
            "-c",
            "user.email=hex-test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "git {args:?} failed");
    out.stdout
}

/// HEX-GUEST-15 (the lead's 'g' list, 4, generated): make a scratch repo with an executable, a file and a
/// symlink (written as git objects, so no host needs filesystem symlinks or modes), commit it, and stage
/// `git archive --format=tar HEAD` and `--format=tar.gz`, each beginning with a global record holding only
/// `comment=<commit>`: expect every entry staged exact, its mode as git writes it (tar.umask 002), its bytes
/// and its link text. Every host the gates run on has `git` (the gates' vendor check needs it), so this
/// test needs it too.
#[test]
fn a_git_archive_made_here_stages_with_every_entry_exact() {
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    git(dir, &["init", "-q", "-b", "main", "."], b"");
    let script = git(
        dir,
        &["hash-object", "-w", "--stdin"],
        b"#!/bin/sh\necho hi\n",
    );
    let conf = git(dir, &["hash-object", "-w", "--stdin"], b"name=lab\n");
    let link = git(dir, &["hash-object", "-w", "--stdin"], b"../bin/hello");
    for (mode, blob, path) in [
        ("100755", &script, "bin/hello"),
        ("100644", &conf, "etc/conf"),
        ("120000", &link, "link"),
    ] {
        let blob = String::from_utf8(blob.clone()).unwrap();
        let info = format!("{mode},{},{path}", blob.trim());
        git(dir, &["update-index", "--add", "--cacheinfo", &info], b"");
    }
    git(dir, &["commit", "-q", "-m", "fixture"], b"");
    let commit = String::from_utf8(git(dir, &["rev-parse", "HEAD"], b"")).unwrap();
    let want = vec![
        ("root".to_owned(), 0o755, What::Dir, vec![]),
        ("root".to_owned(), 0, What::Layer, vec![]),
        ("root/bin".to_owned(), 0o775, What::Dir, vec![]),
        (
            "root/bin/hello".to_owned(),
            0o775,
            What::File { size: 18 },
            b"#!/bin/sh\necho hi\n".to_vec(),
        ),
        ("root/etc".to_owned(), 0o775, What::Dir, vec![]),
        (
            "root/etc/conf".to_owned(),
            0o664,
            What::File { size: 9 },
            b"name=lab\n".to_vec(),
        ),
        (
            "root/link".to_owned(),
            0o777,
            What::Symlink {
                target: "../bin/hello".to_owned(),
            },
            vec![],
        ),
    ];
    for (format, file) in [("tar", "r.tar"), ("tar.gz", "r.tar.gz")] {
        let bytes = git(
            dir,
            &["archive", &format!("--format={format}"), "HEAD"],
            b"",
        );
        let plain = if format == "tar" {
            bytes.clone()
        } else {
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&bytes[..]), &mut out)
                .unwrap();
            out
        };
        assert_eq!(
            plain[156], b'g',
            "{format}: git's first header is its global record"
        );
        let record = String::from_utf8_lossy(&plain[512..1024]).into_owned();
        assert!(
            record.contains(&format!("comment={}", commit.trim())),
            "{format}: {record}"
        );
        let entries = unpack(&pack_tar(file, &bytes, &LIMITS).unwrap());
        let got: Vec<(String, u16, What, Vec<u8>)> = entries
            .into_iter()
            .map(|(e, data)| (e.path, e.mode, e.what, data))
            .collect();
        assert_eq!(got, want, "{format}");
    }
}

/// A PAX line, `<len> <key>=<value>\n`, its length counting itself.
fn pax_line(key: &str, value: &[u8]) -> Vec<u8> {
    let body = key.len() + value.len() + 3;
    let mut n = body + 1;
    while format!("{n}").len() + body != n {
        n += 1;
    }
    let mut out = format!("{n} {key}=").into_bytes();
    out.extend_from_slice(value);
    out.push(b'\n');
    out
}

fn read_pax(record: &[u8], global: bool) -> Result<Pax, String> {
    let bad = |what: &str, why: &dyn std::fmt::Display| Refusal::Invalid {
        what: "staged files",
        why: format!("{what}: {why}"),
    };
    pax_record(&mut &record[..], global, &bad).map_err(|e| e.to_string())
}

/// Standard base64 (RFC 4648 §4) of `bytes`, padded.
fn base64(bytes: &[u8]) -> Vec<u8> {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(A[((n >> (18 - 6 * i)) & 63) as usize]);
            } else {
                out.push(b'=');
            }
        }
    }
    out
}

/// HEX-GUEST-15 (Ada's rule on constants: each PAX line bounded by its key's longest, from cited limits):
/// for every key staging reads, read a record of its largest legal line: expect it accepted; and the same
/// line one byte longer: expect it refused by name, as past what its key allows, before its value is read.
#[test]
fn each_pax_keys_largest_line_is_read_and_one_byte_more_is_refused() {
    let path = vec![b'p'; 4095];
    let value = vec![7u8; files::XATTR_SIZE_MAX];
    let name = format!("user.{}", "n".repeat(files::XATTR_NAME_MAX - 5));
    let cases: Vec<(bool, String, Vec<u8>)> = vec![
        (false, "path".into(), path.clone()),
        (false, "linkpath".into(), path.clone()),
        (false, "uid".into(), b"4294967295".to_vec()),
        (false, "gid".into(), b"4294967295".to_vec()),
        (
            false,
            "mtime".into(),
            b"-9223372036854775808.999999999".to_vec(),
        ),
        (
            false,
            "atime".into(),
            b"-9223372036854775808.999999999".to_vec(),
        ),
        (false, format!("SCHILY.xattr.{name}"), value.clone()),
        (
            false,
            format!(
                "LIBARCHIVE.xattr.{}",
                name.bytes()
                    .map(|b| format!("%{b:02X}"))
                    .collect::<String>()
            ),
            base64(&value),
        ),
        (
            true,
            "atime".into(),
            b"-9223372036854775808.999999999".to_vec(),
        ),
        (
            true,
            "ctime".into(),
            b"-9223372036854775808.999999999".to_vec(),
        ),
        (true, "uname".into(), vec![b'u'; 256]),
        (true, "gname".into(), vec![b'g'; 256]),
        (true, "comment".into(), vec![b'c'; 64]),
        (true, "charset".into(), b"ISO-IR 10646 2000 UTF-8".to_vec()),
        (
            true,
            "hdrcharset".into(),
            b"ISO-IR 10646 2000 UTF-8".to_vec(),
        ),
    ];
    for (global, key, value) in cases {
        let line = pax_line(&key, &value);
        let pax = read_pax(&line, global).unwrap_or_else(|e| panic!("{key}: {e}"));
        assert_eq!(pax.peak, line.len(), "{key}");
        let mut longer = value.clone();
        longer.insert(0, b'0');
        let got = read_pax(&pax_line(&key, &longer), global).unwrap_err();
        assert!(
            got.contains(&format!("its {key} line of")) && got.contains("past the"),
            "{key}: {got}"
        );
    }
}

/// HEX-GUEST-15 (bounded memory): read a record of 40 xattr lines of `XATTR_SIZE_MAX` each, 2.6 MB in all:
/// expect every one read and the reader never holding more than one line.
#[test]
fn a_record_of_many_maximal_lines_streams_a_line_at_a_time() {
    let value = vec![1u8; files::XATTR_SIZE_MAX];
    let record: Vec<u8> = (0..40)
        .flat_map(|i| pax_line(&format!("SCHILY.xattr.user.{i:02}"), &value))
        .collect();
    let pax = read_pax(&record, false).unwrap();
    assert_eq!(pax.xattrs.len(), 40);
    assert_eq!(pax.peak, pax_line("SCHILY.xattr.user.00", &value).len());
    assert!(pax.peak * 40 <= record.len());
}

/// HEX-GUEST-15 (Linux's xattr bounds, each counted): read records whose xattr names together pass
/// `XATTR_LIST_MAX` (names in no Linux namespace count too), that set a key twice, that set a key staging
/// doesn't apply, and whose time is finer than a nanosecond: expect each refused by name.
#[test]
fn pax_records_past_linuxs_bounds_or_repeating_or_unknown_keys_are_refused() {
    let names: Vec<u8> = (0..=files::XATTR_LIST_MAX / (files::XATTR_NAME_MAX + 1))
        .flat_map(|i| pax_line(&format!("SCHILY.xattr.com.apple.{i:0>245}"), b""))
        .collect();
    assert!(
        read_pax(&names, false)
            .unwrap_err()
            .contains("XATTR_LIST_MAX")
    );
    let twice = [pax_line("path", b"a"), pax_line("path", b"b")].concat();
    assert!(
        read_pax(&twice, false)
            .unwrap_err()
            .contains("it sets path twice")
    );
    let twice = [
        pax_line("SCHILY.xattr.user.a", b"1"),
        pax_line("SCHILY.xattr.user.a", b"2"),
    ]
    .concat();
    assert!(read_pax(&twice, false).unwrap_err().contains("twice"));
    for key in ["comment", "ctime", "uname", "SCHILY.dev"] {
        let got = read_pax(&pax_line(key, b"1"), false).unwrap_err();
        assert!(
            got.contains(&format!(
                "a PAX record sets {key}, which staging doesn't apply"
            )),
            "{key}: {got}"
        );
    }
    let fine = pax_line("mtime", b"1.0000000001");
    assert!(
        read_pax(&fine, false)
            .unwrap_err()
            .contains("its mtime isn't a time")
    );
}
