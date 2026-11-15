//! The guest channel's tests (docs/design/shards-guest.md §3, HEX-GUEST-1..3; native plan step 5, serving
//! AC-T7.1.4 and HEX-RT-1..13 in the guest): a framed message reads back whole however the stream cuts
//! it; a frame over its bound is refused before its body is buffered; a stdio stream's name reads back
//! as written and hostile names are refused by type; and an exec's streams are gathered in either order,
//! within their bound, the strays refused.
//!
//! The bounds and sizes given here (a bound of 100 or 256, inputs up to 512 bytes, four sets) are inputs
//! chosen to exercise each bound, not tuning: any values do.

use super::*;
use proptest::prelude::*;

/// AC-T7.4.1: Firecracker rejects guest-origin connections in its host-origin port
/// range. Readiness therefore has a separate nonprivileged port and exact versioned bytes.
#[test]
fn readiness_uses_a_guest_origin_port_and_complete_versioned_receipt() {
    assert!((1u32 << 30..1u32 << 31).contains(&CONTROL_PORT));
    assert!(!(1u32 << 30..1u32 << 31).contains(&GUEST_READY_PORT));
    assert!(!(0..1024).contains(&GUEST_READY_PORT));
    assert_eq!(GUEST_READY, b"HEX_READY 1\n");
    assert_eq!(GUEST_READY.len(), 12);
}

fn message(n: usize) -> Vec<u8> {
    (0..n).map(|i| u8::try_from(i % 251).unwrap()).collect()
}

fn read_all(d: &mut Deframer, mut input: &[u8]) -> Result<Vec<Vec<u8>>, Fault> {
    let mut out = Vec::new();
    while !input.is_empty() {
        let (took, whole) = d.take(input)?;
        assert!(took > 0, "take made no progress on {} bytes", input.len());
        input = &input[took..];
        out.extend(whole);
    }
    Ok(out)
}

/// HEX-GUEST-1: frame two messages, read them back in one piece: expect both, whole, in order, and
/// the stream at a boundary after.
#[test]
fn framed_messages_read_back_whole_and_in_order() {
    let (a, b) = (message(wire::HEADER_BYTES), message(4096));
    let mut bytes = frame(&a, wire::MAX_MESSAGE).unwrap();
    bytes.extend(frame(&b, wire::MAX_MESSAGE).unwrap());
    let mut d = Deframer::new(wire::MAX_MESSAGE);
    assert_eq!(read_all(&mut d, &bytes).unwrap(), vec![a, b]);
    assert!(d.at_boundary());
}

/// HEX-GUEST-1: cut a framed message at every byte, feeding the pieces apart: expect it whole once,
/// and the stream mid-frame until the last byte.
#[test]
fn a_frame_cut_anywhere_reads_back_whole() {
    let m = message(300);
    let bytes = frame(&m, wire::MAX_MESSAGE).unwrap();
    for cut in 1..bytes.len() {
        let mut d = Deframer::new(wire::MAX_MESSAGE);
        let (head, tail) = bytes.split_at(cut);
        assert!(read_all(&mut d, head).unwrap().is_empty(), "cut {cut}");
        assert!(!d.at_boundary(), "cut {cut}");
        assert_eq!(
            read_all(&mut d, tail).unwrap(),
            vec![m.clone()],
            "cut {cut}"
        );
        assert!(d.at_boundary());
    }
}

/// HEX-GUEST-1: frame a message over the bound: expect a typed refusal, nothing written.
#[test]
fn a_message_over_the_bound_is_refused_before_it_is_framed() {
    assert_eq!(
        frame(&message(101), 100),
        Err(Fault::Oversized {
            bytes: 101,
            bound: 100
        })
    );
}

/// HEX-GUEST-1: read a length prefix over the bound: expect a typed refusal from the prefix alone,
/// before any body is buffered (CLAUDE.md §2), and every later read refused the same way.
#[test]
fn a_frame_announced_over_the_bound_is_refused_from_its_prefix() {
    let mut d = Deframer::new(100);
    let fault = Fault::Oversized {
        bytes: 101,
        bound: 100,
    };
    assert_eq!(d.take(&101u32.to_be_bytes()), Err(fault.clone()));
    assert_eq!(d.take(&[0]), Err(fault));
}

/// HEX-GUEST-1: read a length prefix shorter than a wire header: expect a typed refusal.
#[test]
fn a_frame_shorter_than_a_header_is_refused() {
    let mut d = Deframer::new(wire::MAX_MESSAGE);
    assert_eq!(
        d.take(&3u32.to_be_bytes()),
        Err(Fault::Truncated {
            need: wire::HEADER_BYTES,
            have: 3
        })
    );
}

/// HEX-GUEST-2: name each stream of an exec and read the names back: expect what was written.
#[test]
fn a_stdio_name_reads_back_as_written() {
    for stream in [Stdio::In, Stdio::Out, Stdio::Err] {
        let named = Named {
            exec: Exec {
                conn: 0x1112_1314_1516_1718,
                id: 0x0102_0304_0506_0708,
            },
            stream,
        };
        assert_eq!(read_name(&name(named)), Ok(named));
    }
}

/// HEX-GUEST-2: read names with a wrong magic, an unknown version, an unknown stream or a set pad
/// byte: expect each refused by type.
#[test]
fn a_hostile_stdio_name_is_refused() {
    let good = name(Named {
        exec: x(7),
        stream: Stdio::Out,
    });
    let mut magic = good;
    magic[0] = b'X';
    assert_eq!(read_name(&magic), Err(Fault::Magic));
    let mut version = good;
    version[4] = 9;
    assert!(matches!(read_name(&version), Err(Fault::Version { .. })));
    let mut stream = good;
    stream[6] = 3;
    assert!(matches!(read_name(&stream), Err(Fault::Value { .. })));
    let mut pad = good;
    pad[7] = 1;
    assert!(matches!(read_name(&pad), Err(Fault::Value { .. })));
}

/// Exec `id` of one control connection.
fn x(id: u64) -> Exec {
    Exec { conn: 1, id }
}

/// HEX-GUEST-3: the same request id on two control connections: expect two execs, each gathered apart.
#[test]
fn the_same_request_id_on_two_connections_is_two_execs() {
    let mut g = Gather::new(4);
    let (a, b) = (Exec { conn: 1, id: 5 }, Exec { conn: 2, id: 5 });
    g.expect(a, streams(true, false, false)).unwrap();
    g.expect(b, streams(true, false, false)).unwrap();
    assert_eq!(
        g.arrive(
            Named {
                exec: b,
                stream: Stdio::In
            },
            "b"
        ),
        Ok(Some((b, vec!["b"])))
    );
    assert_eq!(
        g.arrive(
            Named {
                exec: a,
                stream: Stdio::In
            },
            "a"
        ),
        Ok(Some((a, vec!["a"])))
    );
}

fn streams(stdin: bool, stdout: bool, stderr: bool) -> wire::Streams {
    wire::Streams {
        stdin,
        stdout,
        stderr,
    }
}

/// HEX-GUEST-3: expect an exec's stdout and stderr, then have them arrive out of order: expect the
/// set complete on the last, in stdin-stdout-stderr order.
#[test]
fn an_execs_streams_are_gathered_in_order_after_its_exec() {
    let mut g = Gather::new(4);
    g.expect(x(9), streams(false, true, true)).unwrap();
    assert_eq!(
        g.arrive(
            Named {
                exec: x(9),
                stream: Stdio::Err
            },
            "err"
        ),
        Ok(None)
    );
    assert_eq!(
        g.arrive(
            Named {
                exec: x(9),
                stream: Stdio::Out
            },
            "out"
        ),
        Ok(Some((x(9), vec!["out", "err"])))
    );
    assert!(g.is_empty());
}

/// HEX-GUEST-3: have an exec's stream arrive before its exec does: expect it held, and the set
/// complete once the exec says what it wants.
#[test]
fn a_stream_before_its_exec_is_held_until_the_exec_comes() {
    let mut g = Gather::new(4);
    assert_eq!(
        g.arrive(
            Named {
                exec: x(3),
                stream: Stdio::In
            },
            "in"
        ),
        Ok(None)
    );
    assert_eq!(
        g.expect(x(3), streams(true, false, false)),
        Ok(Some((x(3), vec!["in"])))
    );
    assert!(g.is_empty());
}

/// HEX-GUEST-3: an exec with no streams is complete at once.
#[test]
fn an_exec_with_no_streams_is_complete_at_once() {
    let mut g = Gather::<&str>::new(1);
    assert_eq!(
        g.expect(x(1), streams(false, false, false)),
        Ok(Some((x(1), vec![])))
    );
}

/// HEX-GUEST-3: name a stream twice, a stream the exec didn't ask for, or an exec twice: expect each
/// refused by type, with the stray connection handed back to close.
#[test]
fn duplicate_and_unwanted_streams_are_refused() {
    let mut g = Gather::new(4);
    g.expect(x(5), streams(true, true, false)).unwrap();
    assert_eq!(
        g.arrive(
            Named {
                exec: x(5),
                stream: Stdio::In
            },
            "a"
        ),
        Ok(None)
    );
    assert_eq!(
        g.arrive(
            Named {
                exec: x(5),
                stream: Stdio::In
            },
            "b"
        ),
        Err(Stray {
            conn: "b",
            why: Why::Twice
        })
    );
    assert_eq!(
        g.arrive(
            Named {
                exec: x(5),
                stream: Stdio::Err
            },
            "c"
        ),
        Err(Stray {
            conn: "c",
            why: Why::Unwanted
        })
    );
    assert_eq!(
        g.expect(x(5), streams(true, true, false)),
        Err(Refused {
            why: Why::Twice,
            conns: vec![]
        })
    );
}

/// HEX-GUEST-3: have a stream arrive that its exec, coming after, doesn't ask for: expect the exec
/// refused by type and the early stream handed back to close (CLAUDE.md §3).
#[test]
fn an_early_stream_its_exec_doesnt_want_is_handed_back() {
    let mut g = Gather::new(4);
    g.arrive(
        Named {
            exec: x(4),
            stream: Stdio::Err,
        },
        "err",
    )
    .unwrap();
    assert_eq!(
        g.expect(x(4), streams(true, false, false)),
        Err(Refused {
            why: Why::Unwanted,
            conns: vec!["err"]
        })
    );
    assert!(g.is_empty());
}

/// HEX-GUEST-3: gather more execs at once than the bound: expect the next refused by type, both as an
/// exec and as an early stream (CLAUDE.md §2).
#[test]
fn execs_past_the_bound_are_refused() {
    let mut g = Gather::new(1);
    g.expect(x(1), streams(true, false, false)).unwrap();
    assert_eq!(
        g.expect(x(2), streams(true, false, false)),
        Err(Refused {
            why: Why::Full { bound: 1 },
            conns: vec![]
        })
    );
    assert_eq!(
        g.arrive(
            Named {
                exec: x(3),
                stream: Stdio::In
            },
            "x"
        ),
        Err(Stray {
            conn: "x",
            why: Why::Full { bound: 1 }
        })
    );
}

/// HEX-GUEST-3 (CLAUDE.md §4: a fact, not a clock): leave sets incomplete on two control connections,
/// then end one: expect that connection's sets, the exec waiting and the streams that came early,
/// handed back with their connections to close, and the other connection's set kept.
#[test]
fn a_control_connections_end_hands_back_its_incomplete_sets() {
    let on = |conn, id| Exec { conn, id };
    let mut g = Gather::new(4);
    g.expect(on(1, 1), streams(true, true, false)).unwrap();
    g.arrive(
        Named {
            exec: on(1, 1),
            stream: Stdio::Out,
        },
        "out",
    )
    .unwrap();
    g.arrive(
        Named {
            exec: on(1, 2),
            stream: Stdio::In,
        },
        "early",
    )
    .unwrap();
    g.arrive(
        Named {
            exec: on(2, 1),
            stream: Stdio::In,
        },
        "other",
    )
    .unwrap();
    assert_eq!(
        g.forget(1),
        vec![(on(1, 1), vec!["out"]), (on(1, 2), vec!["early"])]
    );
    assert!(!g.is_empty());
    assert_eq!(g.forget(2), vec![(on(2, 1), vec!["other"])]);
    assert!(g.is_empty());
}

proptest! {
    /// HEX-GUEST-1: any bytes, fed in any cuts: the deframer never panics, never buffers past its
    /// bound, and returns only messages its prefixes announced within it.
    #[test]
    fn hostile_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512), cut in 1usize..64) {
        let mut d = Deframer::new(256);
        for piece in bytes.chunks(cut) {
            let mut piece = piece;
            while !piece.is_empty() {
                match d.take(piece) {
                    Ok((took, whole)) => {
                        prop_assert!(took > 0 && took <= piece.len());
                        if let Some(m) = whole {
                            prop_assert!(m.len() >= wire::HEADER_BYTES && m.len() <= 256);
                        }
                        piece = &piece[took..];
                    }
                    Err(_) => return Ok(()),
                }
                prop_assert!(d.buffered() <= 256);
            }
        }
    }

    /// HEX-GUEST-2: any name's worth of bytes, read as a name, never panics.
    #[test]
    fn hostile_names_never_panic(bytes in proptest::array::uniform24(any::<u8>())) {
        let _ = read_name(&bytes);
    }
}

/// The rows of the markdown table in `doc` that follows the line starting with `heading`: each row's cells.
fn table_after<'a>(doc: &'a str, heading: &str) -> Vec<Vec<&'a str>> {
    doc.lines()
        .skip_while(|line| !line.starts_with(heading))
        .skip(1)
        .skip_while(|line| !line.starts_with('|'))
        .take_while(|line| line.starts_with('|'))
        .skip(2)
        .map(|row| row.trim_matches('|').split('|').map(str::trim).collect())
        .collect()
}

/// HEX-GUEST-30: read §3a's format tables from the design doc: expect each port and width the code
/// uses to be the one written there, and the stdio name's fields to sum to `NAME_BYTES`.
#[test]
fn the_channel_formats_are_the_design_docs() {
    let doc = include_str!("../../../../docs/design/shards-guest.md");
    let ports = table_after(doc, "**Ports.**");
    let port = |name: &str| {
        let row = ports.iter().find(|r| r[0] == format!("`{name}`")).unwrap();
        let hex = row[1].trim_matches('`').replace('_', "");
        u32::from_str_radix(hex.strip_prefix("0x").unwrap(), 16).unwrap()
    };
    assert_eq!(port("CONTROL_PORT"), CONTROL_PORT);
    assert_eq!(port("STDIO_PORT"), STDIO_PORT);
    assert_eq!(port("FILES_PORT"), files::FILES_PORT);
    assert_eq!(port("STAMPS_PORT"), STAMPS_PORT);
    // Each port's listener as the relay's `--unix` names it, the doc's socket column.
    for row in &ports {
        let name = row[0].trim_matches('`');
        assert_eq!(
            socket_name(port(name)),
            Some(row[2].trim_matches('`')),
            "{name}'s socket"
        );
    }
    assert_eq!(
        socket_name(GUEST_READY_PORT),
        None,
        "a port the relay doesn't listen on"
    );
    let control = table_after(doc, "**Control.**");
    assert_eq!(control[0][1], TOKEN_BYTES.to_string());
    assert_eq!(control[1][1], PREFIX.to_string());
    let name = table_after(doc, "**Stdio name**");
    let sum: usize = name.iter().map(|r| r[1].parse::<usize>().unwrap()).sum();
    assert_eq!(sum, NAME_BYTES);
    assert!(doc.contains(&format!("`NAME_BYTES` = {NAME_BYTES}")));
}
