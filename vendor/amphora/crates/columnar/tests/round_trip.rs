//! T-CS.3.1: round trips through both codecs on random and real-like columns (integers, strings,
//! doubles, nulls): every value equal, bit for bit, and every null kept.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use amphora_columnar::cascade::{Node, Tree};
use amphora_columnar::pack::{self, Layout};
use amphora_columnar::{Codec, CodecError, Column, Strings, Values, encode};

/// SplitMix64: a seeded generator, so every failure replays.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

const SIZES: [usize; 11] = [0, 1, 2, 15, 63, 64, 65, 1023, 1024, 1025, 5000];
const CODECS: [Codec; 2] = [Codec::BtrBlocks, Codec::FastLanes];

fn round_trip(column: &Column, what: &str) {
    for codec in CODECS {
        let encoded =
            encode(codec, column).unwrap_or_else(|e| panic!("{what} {codec:?}: encode: {e}"));
        let decoded = encoded
            .decode()
            .unwrap_or_else(|e| panic!("{what} {codec:?}: decode: {e}"));
        assert!(
            column.equal_rows(&decoded),
            "{what} {codec:?}: rows differ after a round trip"
        );
        assert_eq!(decoded.len(), column.len(), "{what} {codec:?}");
    }
}

fn ints(v: Vec<i64>) -> Column {
    Column {
        values: Values::Int(v),
        valid: None,
    }
}

fn with_nulls(mut c: Column, rng: &mut Rng, one_in: u64) -> Column {
    c.valid = Some((0..c.len()).map(|_| rng.below(one_in) != 0).collect());
    c
}

fn strings<'a>(v: impl IntoIterator<Item = &'a [u8]>) -> Strings {
    let mut s = Strings::new();
    for e in v {
        s.push(e);
    }
    s
}

#[test]
fn t_cs_3_1_pack_every_width_both_layouts() {
    let mut rng = Rng(1);
    for layout in [Layout::Horizontal, Layout::Interleaved] {
        for width in 0..=64u32 {
            for n in [0usize, 1, 63, 64, 65, 1024, 1500, 2048] {
                let mask = if width == 0 {
                    0
                } else {
                    u64::MAX >> (64 - width)
                };
                let d: Vec<u64> = (0..n)
                    .map(|i| if i == 0 { mask } else { rng.next() & mask })
                    .collect();
                let base = rng.next();
                let p = pack::pack(layout, base, &d).unwrap();
                assert!(p.width <= width);
                let mut out = Vec::new();
                pack::unpack(&p, &mut out).unwrap();
                let want: Vec<u64> = d.iter().map(|x| x.wrapping_add(base)).collect();
                assert_eq!(out, want, "{layout:?} width {width} n {n}");
            }
        }
    }
}

#[test]
fn t_cs_3_1_delta_lanes_round_trip() {
    let mut rng = Rng(2);
    for n in SIZES {
        let v: Vec<u64> = (0..n).map(|_| rng.next()).collect();
        let (bases, deltas) = pack::delta_encode(&v);
        let mut back = deltas.clone();
        pack::delta_decode(&mut back, &bases).unwrap();
        assert_eq!(back, v, "n {n}");
    }
}

#[test]
fn t_cs_3_1_random_integers() {
    let mut rng = Rng(3);
    for n in SIZES {
        for bits in [1u32, 7, 13, 31, 63, 64] {
            let mask = if bits == 64 {
                u64::MAX
            } else {
                (1u64 << bits) - 1
            };
            let v: Vec<i64> = (0..n).map(|_| (rng.next() & mask) as i64).collect();
            round_trip(&ints(v.clone()), &format!("random {bits} bits n {n}"));
            round_trip(
                &with_nulls(ints(v), &mut rng, 4),
                &format!("random {bits} bits with nulls n {n}"),
            );
        }
        let extremes: Vec<i64> = (0..n)
            .map(|i| [i64::MIN, i64::MAX, 0, -1, 1][i % 5])
            .collect();
        round_trip(&ints(extremes), &format!("extremes n {n}"));
        let runs: Vec<i64> = (0..n).map(|i| (i / 37) as i64 * 1000 - 50).collect();
        round_trip(&ints(runs), &format!("runs n {n}"));
        let few: Vec<i64> = (0..n)
            .map(|_| [7, -3, 1 << 40][rng.below(3) as usize])
            .collect();
        round_trip(&ints(few), &format!("three values n {n}"));
        round_trip(&ints(vec![42; n]), &format!("one value n {n}"));
    }
}

#[test]
fn t_cs_3_1_random_doubles() {
    let mut rng = Rng(4);
    for n in SIZES {
        let any: Vec<f64> = (0..n).map(|_| f64::from_bits(rng.next())).collect();
        round_trip(
            &Column {
                values: Values::Double(any),
                valid: None,
            },
            &format!("random bits n {n}"),
        );
        let specials = [
            0.0,
            -0.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MIN_POSITIVE,
            5e-324,
            f64::MAX,
            1e22,
            1e23,
            0.1,
            -123.456,
        ];
        let special: Vec<f64> = (0..n).map(|i| specials[i % specials.len()]).collect();
        round_trip(
            &Column {
                values: Values::Double(special),
                valid: None,
            },
            &format!("specials n {n}"),
        );
        let decimals: Vec<f64> = (0..n)
            .map(|_| (rng.below(1_000_000) as f64) / 100.0)
            .collect();
        let c = Column {
            values: Values::Double(decimals),
            valid: None,
        };
        round_trip(&c, &format!("two-place decimals n {n}"));
        round_trip(
            &with_nulls(c, &mut rng, 3),
            &format!("decimals with nulls n {n}"),
        );
        let few: Vec<f64> = (0..n)
            .map(|_| [0.5, 99.99, -1.25][rng.below(3) as usize])
            .collect();
        round_trip(
            &Column {
                values: Values::Double(few),
                valid: None,
            },
            &format!("three doubles n {n}"),
        );
    }
}

#[test]
fn t_cs_3_1_random_strings() {
    let mut rng = Rng(5);
    for n in SIZES {
        let owned: Vec<Vec<u8>> = (0..n)
            .map(|_| (0..rng.below(40)).map(|_| rng.next() as u8).collect())
            .collect();
        let c = Column {
            values: Values::Str(strings(owned.iter().map(Vec::as_slice))),
            valid: None,
        };
        round_trip(&c, &format!("random bytes n {n}"));
        round_trip(
            &with_nulls(c, &mut rng, 5),
            &format!("random bytes with nulls n {n}"),
        );
        let words: [&[u8]; 4] = [b"queued", b"running", b"", b"failed"];
        let few = strings((0..n).map(|_| words[rng.below(4) as usize]));
        round_trip(
            &Column {
                values: Values::Str(few),
                valid: None,
            },
            &format!("four words n {n}"),
        );
    }
}

/// Columns shaped like kiln's `events` table (bench/src/schema.rs): its sequence, its millisecond
/// timestamps, its kinds and levels (some null), its tools (mostly null), its ids, and a duration.
#[test]
fn t_cs_3_1_real_like_columns() {
    let mut rng = Rng(6);
    for n in SIZES {
        let seq: Vec<i64> = (0..n as i64).collect();
        let mut t = 1_795_000_000_000i64;
        let at: Vec<i64> = (0..n)
            .map(|_| {
                t += rng.below(2_000) as i64;
                t
            })
            .collect();
        let kinds: [&[u8]; 6] = [
            b"message",
            b"tool_use",
            b"tool_result",
            b"status",
            b"thinking",
            b"subagent",
        ];
        let kind = strings((0..n).map(|_| kinds[rng.below(6) as usize]));
        let levels: [&[u8]; 3] = [b"info", b"warn", b"error"];
        let level = Column {
            values: Values::Str(strings((0..n).map(|_| levels[rng.below(3) as usize]))),
            valid: Some((0..n).map(|_| rng.below(3) != 0).collect()),
        };
        let tools: [&[u8]; 4] = [b"Bash", b"Read", b"Edit", b"mcp__kiln__run"];
        let tool = Column {
            values: Values::Str(strings((0..n).map(|_| tools[rng.below(4) as usize]))),
            valid: Some((0..n).map(|_| rng.below(5) == 0).collect()),
        };
        let ids: Vec<Vec<u8>> = (0..n)
            .map(|_| format!("01a1{:028x}", rng.next() as u128 * 3).into_bytes())
            .collect();
        let id = strings(ids.iter().map(Vec::as_slice));
        let seconds: Vec<f64> = (0..n)
            .map(|_| (rng.below(600_000) as f64) / 1000.0)
            .collect();
        round_trip(&ints(seq), &format!("seq n {n}"));
        round_trip(&ints(at), &format!("at n {n}"));
        round_trip(
            &Column {
                values: Values::Str(kind),
                valid: None,
            },
            &format!("kind n {n}"),
        );
        round_trip(&level, &format!("level n {n}"));
        round_trip(&tool, &format!("tool n {n}"));
        round_trip(
            &Column {
                values: Values::Str(id),
                valid: None,
            },
            &format!("id n {n}"),
        );
        round_trip(
            &Column {
                values: Values::Double(seconds),
                valid: None,
            },
            &format!("seconds n {n}"),
        );
    }
}

/// The choice follows the data: each codec picks what its sample shows is smallest.
#[test]
fn t_cs_3_1_schemes_follow_the_data() {
    let first = |codec, c: &Column| {
        encode(codec, c)
            .unwrap()
            .tree
            .nodes
            .into_iter()
            .next()
            .unwrap()
    };
    let seq = ints((0..100_000).collect());
    assert!(
        matches!(first(Codec::FastLanes, &seq), Node::IntDelta { .. }),
        "sorted integers take DELTA in FastLanes"
    );
    assert!(
        matches!(first(Codec::BtrBlocks, &seq), Node::IntPacked(_)),
        "sorted integers pack in BtrBlocks, which has no delta"
    );
    let constant = ints(vec![9; 10_000]);
    for codec in CODECS {
        assert!(matches!(first(codec, &constant), Node::IntOne { .. }));
    }
    let words: [&[u8]; 3] = [b"queued", b"running", b"succeeded"];
    let low = Column {
        values: Values::Str(strings((0..10_000).map(|i| words[i % 3]))),
        valid: None,
    };
    for codec in CODECS {
        assert!(matches!(first(codec, &low), Node::StrDict { .. }));
    }
    let runs = ints((0..10_000).map(|i| i / 500).collect());
    assert!(matches!(
        first(Codec::BtrBlocks, &runs),
        Node::IntRle { .. }
    ));
    // A column that is one value except where the sample can't see: One must not be taken.
    let mut almost: Vec<i64> = vec![5; 100_000];
    almost[77_777] = 6;
    round_trip(&ints(almost), "one value but one");
    // The cascade keeps going while its samples say so: a bit width per value well under raw.
    let e = encode(Codec::BtrBlocks, &low).unwrap();
    assert!(
        e.bits_per_value() < 8.0,
        "three words take {} bits a value",
        e.bits_per_value()
    );
}

/// Hostile encodings are refused with a typed error, never a panic (CLAUDE.md §1, §3).
#[test]
fn t_cs_3_1_corrupt_encodings_refused() {
    let good = encode(Codec::BtrBlocks, &ints((0..1000).map(|i| i % 7).collect())).unwrap();
    let mut bad = good.clone();
    bad.len += 1;
    assert!(bad.decode().is_err());
    let mut cyclic = good.clone();
    cyclic.tree = Tree {
        nodes: vec![Node::IntDict { dict: 0, codes: 0 }],
    };
    assert!(matches!(cyclic.decode(), Err(CodecError::Corrupt(_))));
    let mut short = good;
    for node in &mut short.tree.nodes {
        if let Node::IntPacked(p) = node {
            p.words.pop();
        }
        if let Node::IntRle { len, .. } = node {
            *len += 1;
        }
    }
    let empty_dict = Tree {
        nodes: vec![
            Node::IntDict { dict: 1, codes: 2 },
            Node::IntRaw(vec![]),
            Node::IntRaw(vec![0]),
        ],
    };
    assert!(empty_dict.decode().is_err());
    let wide = Tree {
        nodes: vec![Node::IntPacked(pack::Packed {
            layout: Layout::Interleaved,
            width: 65,
            base: 0,
            len: 1,
            words: vec![0; 65 * 16],
        })],
    };
    assert!(wide.decode().is_err());
    let _ = short.decode();
}
