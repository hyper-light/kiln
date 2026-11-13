//! The check's readers on the kernel's texts and bytes, and on hostile ones (CLAUDE.md §6: hostile input to
//! every parser; goal 5 extended, step 6). The check against real namespaces runs in
//! `crates/hex-cli/tests/oci_reuse.rs`, as root.

use proptest::prelude::*;

use super::*;

/// A clean namespace's `/proc/net/sockstat`, as Linux 6.19 writes it (net/ipv4/proc.c:52-76).
const CLEAN_SOCKSTAT: &str = "sockets: used 0\nTCP: inuse 0 orphan 3 tw 0 alloc 9 mem 1\nUDP: inuse 0 mem 0\n\
UDPLITE: inuse 0\nRAW: inuse 0\nFRAG: inuse 0 memory 0\n";

#[test]
fn sockstat_says_whether_a_time_wait_connection_is_left() {
    assert_eq!(time_wait(CLEAN_SOCKSTAT), Ok(()));
    let tw = CLEAN_SOCKSTAT.replace("tw 0", "tw 2");
    assert_eq!(time_wait(&tw), Err(Refused::TimeWait));
    // TIME_WAIT alone: the sockets' count is `used`'s, and the host-wide orphan and allocation counts aren't the
    // namespace's.
    assert_eq!(
        time_wait(&CLEAN_SOCKSTAT.replace("used 0", "used 3")),
        Ok(())
    );
    assert_eq!(
        time_wait(&CLEAN_SOCKSTAT.replace("orphan 3", "orphan 900")),
        Ok(())
    );
    for broken in [
        "",
        "TCP: inuse 0 tw x\n",
        "TCP: inuse 0\n",
        "sockets: used 0\n",
        "UDP: tw 0\n",
    ] {
        assert_eq!(time_wait(broken), Err(Refused::Unreadable), "{broken:?}");
    }
}

/// `/proc/net/protocols` as Linux 6.19 writes it (net/core/sock.c:4313-4359), cut to a few protocols.
const PROTOCOLS: &str = "protocol  size sockets  memory press maxhdr  slab module     cl co di ac io in de sh ss gs se re bi br ha uh gp em\n\
PACKET    1664      0      -1   NI       0   no   kernel      n  n  n  n  n  n  n  n  n  n  n  n  n  n  n  n  n  n\n\
UDPv6     1472      0       0   NI       0   yes  kernel      y  y  y  n  y  y  y  n  y  y  y  y  y  n  n  y  y  n\n\
TCPv6     2624      0     881   no     320   yes  kernel      y  y  y  y  y  y  y  y  y  y  y  y  y  n  y  y  y  y\n\
UNIX      1152      0      -1   NI       0   yes  kernel      n  n  n  n  n  n  n  n  n  n  n  n  n  n  n  n  n  n\n\
UDP       1344      0       0   NI       0   yes  kernel      y  y  y  n  y  y  y  n  y  y  y  y  y  n  n  y  y  n\n\
TCP       2496      0     881   no     320   yes  kernel      y  y  y  y  y  y  y  y  y  y  y  y  y  n  y  y  y  y\n\
NETLINK   1112      0      -1   NI       0   no   kernel      n  n  n  n  n  n  n  n  n  n  n  n  n  n  n  n  n  n\n";

/// `sockstat`'s count of every socket in the namespace, read before the protocols' (`sockets_left`).
#[test]
fn sockstat_counts_the_namespaces_sockets() {
    assert_eq!(used(CLEAN_SOCKSTAT), Ok(0));
    assert_eq!(used(&CLEAN_SOCKSTAT.replace("used 0", "used 1")), Ok(1));
    for broken in [
        "",
        "sockets: used\n",
        "sockets: used x\n",
        "sockets: used -1\n",
        "TCP: inuse 0 orphan 0 tw 0\n",
    ] {
        assert_eq!(used(broken), Err(Refused::Unreadable), "{broken:?}");
    }
}

#[test]
fn protocols_say_whether_a_socket_is_left_against_a_fresh_namespaces() {
    let fresh = in_use(PROTOCOLS).unwrap();
    assert_eq!(fresh.len(), 7);
    assert_eq!(fresh.get("UDP"), Some(&0));
    assert_eq!(sockets(PROTOCOLS, &fresh), Ok(()));
    let udp = PROTOCOLS.replace("UDP       1344      0", "UDP       1344      1");
    assert_eq!(
        sockets(&udp, &fresh),
        Err(Refused::Sockets),
        "a bound UDP socket held outside"
    );
    let unix = PROTOCOLS.replace("UNIX      1152      0", "UNIX      1152      2");
    assert_eq!(sockets(&unix, &fresh), Err(Refused::Sockets));
    // The check's own netlink socket, kept in the namespace, is no socket left; a second is.
    let netlink =
        |n: u32| PROTOCOLS.replace("NETLINK   1112      0", &format!("NETLINK   1112      {n}"));
    assert_eq!(sockets(&netlink(1), &fresh), Ok(()));
    assert_eq!(sockets(&netlink(2), &fresh), Err(Refused::Sockets));
    // A protocol a module brought after the fresh namespace was read: none of its sockets, or one.
    let sctp = |n: u32| {
        format!(
            "{PROTOCOLS}SCTP      1600      {n}       0   no       0   yes  sctp        y  y  y  y  y  y  y  y  y  y  y  y  y  y  y  y  y  y\n"
        )
    };
    assert_eq!(sockets(&sctp(0), &fresh), Ok(()));
    assert_eq!(sockets(&sctp(1), &fresh), Err(Refused::Sockets));
    // Fewer than a fresh namespace's is no socket left.
    let mut more = fresh.clone();
    more.insert("NETLINK".to_owned(), 1);
    assert_eq!(sockets(PROTOCOLS, &more), Ok(()));
    for broken in [
        "",
        "protocol  size\n",
        "proto  size sockets\nTCP 2496 0\n",
        "protocol  size sockets  memory\n",
        "protocol  size sockets  memory\nTCP 2496\n",
        "protocol  size sockets  memory\nTCP 2496 -1\n",
    ] {
        assert_eq!(in_use(broken), Err(Refused::Unreadable), "{broken:?}");
        assert_eq!(
            sockets(broken, &fresh),
            Err(Refused::Unreadable),
            "{broken:?}"
        );
    }
}

/// `/proc/net/dev` as a fresh namespace has it.
const LO_ONLY: &str = "Inter-|   Receive                                                |  Transmit\n \
face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    \
lo:       0       0    0    0    0     0          0         0        0       0    0    0    0     0       0          0\n";

#[test]
fn net_dev_says_whether_a_link_besides_loopback_is_there() {
    assert_eq!(links(LO_ONLY), Ok(()));
    let more = format!("{LO_ONLY}  eth0: 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16\n");
    assert_eq!(links(&more), Err(Refused::Links));
    let none = LO_ONLY.lines().take(2).collect::<Vec<_>>().join("\n");
    assert_eq!(links(&none), Err(Refused::Links), "no loopback");
}

/// Goal 5 extended step 6, the lead's fourth condition for a kept namespace's files: a read of one that fails is
/// no verdict: the check drops them and looks as it does without them, counted, and refuses nothing on its own. Do:
/// hold ends of pipes for them, which can't be read at an offset (ESPIPE), and check. Expect: no verdict through
/// them, and the check counted as having failed them, holding none after.
#[test]
fn a_held_file_whose_read_fails_is_no_verdict_and_the_check_falls_back_counted() {
    let end = || {
        rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
            .unwrap()
            .0
    };
    let mut files = Some(Files {
        sockstat: end(),
        dev: end(),
        tables: [Some(end()), Some(end())],
    });
    let fresh = Fresh {
        protocols: BTreeMap::new(),
        dumps: Vec::new(),
    };
    let mut scratch = Scratch::default();
    let socket = end();
    let proc_net = ProcNet::of_this_thread().unwrap();
    assert!(matches!(
        look_held(
            files.as_ref().unwrap(),
            &socket,
            &proc_net,
            &fresh,
            &mut scratch
        ),
        HeldLook::Failed
    ));
    let own = || {
        rustix::fs::open(
            "/proc/self/ns/net",
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap()
    };
    let (ns, home) = (own(), own());
    let checked = check(
        ns.as_fd(),
        home.as_fd(),
        &proc_net,
        &fresh,
        &mut scratch,
        &mut Some(socket),
        Some(&mut files),
    );
    assert!(checked.files_failed && !checked.through_files);
    assert!(files.is_none());
}

/// An iptables file is absent while its module isn't loaded, and comes to be in every namespace when it loads, so a
/// namespace holds those there are, and a check looks by name, where it stands, for one it doesn't hold: fedora's kernel
/// has `ip_tables_names` and no `ip6_tables_names`. Do: hold a `/proc/net` with neither, look, then make one, and look.
/// Expect: past them to the dumps, whose socket here refuses; then no verdict, and the check sent in.
#[test]
fn an_iptables_file_not_held_is_looked_for_and_one_come_to_be_sends_the_check_in() {
    let dir = tempfile::tempdir().unwrap();
    let sockstat = CLEAN_SOCKSTAT.replace("sockets: used 0", "sockets: used 1");
    std::fs::write(dir.path().join("sockstat"), sockstat).unwrap();
    std::fs::write(dir.path().join("dev"), LO_ONLY).unwrap();
    let proc_net = ProcNet(
        rustix::fs::open(
            dir.path(),
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap(),
    );
    let files = Files::open(&proc_net).unwrap().unwrap();
    assert!(files.tables.iter().all(Option::is_none));
    let fresh = Fresh {
        protocols: BTreeMap::new(),
        dumps: Vec::new(),
    };
    let mut scratch = Scratch::default();
    let socket = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
        .unwrap()
        .0;
    assert!(matches!(
        look_held(&files, &socket, &proc_net, &fresh, &mut scratch),
        HeldLook::Done(Err(_))
    ));
    std::fs::write(dir.path().join("ip6_tables_names"), b"").unwrap();
    assert!(matches!(
        look_held(&files, &socket, &proc_net, &fresh, &mut scratch),
        HeldLook::Failed
    ));
}

/// A file of a namespace's `/proc/net` is read whole into the thread's held room, again and again into the same
/// room; one too long for the bound is refused, and one the kernel hasn't is none (CLAUDE.md §6: hostile input).
#[test]
fn a_proc_net_file_is_read_whole_into_the_held_room_and_one_past_the_bound_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let proc_net = ProcNet(
        rustix::fs::open(
            dir.path(),
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap(),
    );
    let mut scratch = Scratch::default();
    std::fs::write(dir.path().join("protocols"), PROTOCOLS).unwrap();
    std::fs::write(dir.path().join("sockstat"), CLEAN_SOCKSTAT).unwrap();
    for _ in 0..2 {
        assert_eq!(
            proc_net.file("protocols", &mut scratch.text),
            Ok(Some(PROTOCOLS))
        );
        assert_eq!(
            proc_net.file("sockstat", &mut scratch.text),
            Ok(Some(CLEAN_SOCKSTAT)),
            "a shorter file after a longer one is itself alone"
        );
    }
    assert_eq!(
        proc_net.file("ip6_tables_names", &mut scratch.text),
        Ok(None)
    );
    std::fs::write(dir.path().join("empty"), "").unwrap();
    assert_eq!(proc_net.file("empty", &mut scratch.text), Ok(Some("")));
    let at_bound = "x".repeat(MAX_PROC_FILE);
    std::fs::write(dir.path().join("at"), &at_bound).unwrap();
    assert_eq!(
        proc_net.file("at", &mut scratch.text),
        Ok(Some(at_bound.as_str()))
    );
    std::fs::write(dir.path().join("past"), "x".repeat(MAX_PROC_FILE + 1)).unwrap();
    assert_eq!(
        proc_net.file("past", &mut scratch.text),
        Err(Refused::Unreadable)
    );
    std::fs::write(dir.path().join("binary"), b"\xff\xfe\x00").unwrap();
    assert_eq!(
        proc_net.file("binary", &mut scratch.text),
        Err(Refused::Unreadable),
        "not text"
    );
}

/// The sockets' check reads `sockstat` first, and the protocols only past a count of the check's own socket: a count
/// of 1 needs no protocols file, and any other count reads it, said in `protocols` for the node to count, which tells
/// a box's netlink socket waiting out its grace period from a socket held outside; TIME_WAIT refuses whatever the
/// count (CLAUDE.md §6). Goal 5 extended step 6, the lead's conditions for the kept socket (2026-11-03T17:36Z): a
/// count past 1 from the RCU lag (a closed netlink socket counted until its grace period has passed, Linux v6.19
/// net/netlink/af_netlink.c:794-796, :703-715) reads the protocols, counted, and is never refused for it.
#[test]
fn the_sockets_are_read_from_the_protocols_only_past_the_checks_own_socket() {
    let dir = tempfile::tempdir().unwrap();
    let proc_net = ProcNet(
        rustix::fs::open(
            dir.path(),
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap(),
    );
    let fresh = Fresh {
        protocols: in_use(PROTOCOLS).unwrap(),
        dumps: Vec::new(),
    };
    let mut text = Scratch::default().text;
    let mut left = || {
        let mut protocols = false;
        let verdict = sockets_left(&proc_net, &fresh, &mut text, &mut protocols);
        (verdict, protocols)
    };
    let write = |name: &str, body: &str| std::fs::write(dir.path().join(name), body).unwrap();
    write("sockstat", &CLEAN_SOCKSTAT.replace("used 0", "used 1"));
    assert_eq!(left(), (Ok(()), false), "the check's own socket alone");
    write("sockstat", &CLEAN_SOCKSTAT.replace("used 0", "used 2"));
    assert_eq!(
        left(),
        (Err(Refused::Unreadable), true),
        "past it the protocols are read, and there are none here"
    );
    write(
        "protocols",
        &PROTOCOLS.replace("NETLINK   1112      0", "NETLINK   1112      1"),
    );
    assert_eq!(
        left(),
        (Ok(()), true),
        "a box's netlink socket still counted in sockstat, gone from the protocols"
    );
    write(
        "protocols",
        &PROTOCOLS.replace("UDP       1344      0", "UDP       1344      1"),
    );
    assert_eq!(
        left(),
        (Err(Refused::Sockets), true),
        "a bound UDP socket held outside"
    );
    write(
        "sockstat",
        &CLEAN_SOCKSTAT
            .replace("used 0", "used 1")
            .replace("tw 0", "tw 1"),
    );
    assert_eq!(left(), (Err(Refused::TimeWait), false));
}

#[test]
fn a_dump_request_is_a_header_and_its_family_header() {
    let m = request(RTM_GETROUTE, AF_INET6, RTMSG, 7);
    assert_eq!(m.len(), NLMSG_HDR + RTMSG);
    assert_eq!(u32_at(&m, 0), Ok(28), "nlmsg_len");
    assert_eq!(u16_at(&m, 4), Ok(RTM_GETROUTE));
    assert_eq!(u16_at(&m, 6), Ok(DUMP), "NLM_F_REQUEST | NLM_F_DUMP");
    assert_eq!(u32_at(&m, 8), Ok(7), "seq");
    assert_eq!(m[16], AF_INET6, "rtm_family");
    assert!(m[17..].iter().all(|b| *b == 0));
}

/// A netlink message of `kind` and `seq` with `payload`, padded to 4.
fn msg(kind: u16, seq: u32, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(NLMSG_HDR + payload.len()).unwrap();
    let mut m = Vec::new();
    m.extend_from_slice(&len.to_ne_bytes());
    m.extend_from_slice(&kind.to_ne_bytes());
    m.extend_from_slice(&2u16.to_ne_bytes());
    m.extend_from_slice(&seq.to_ne_bytes());
    m.extend_from_slice(&0u32.to_ne_bytes());
    m.extend_from_slice(payload);
    while m.len() % 4 != 0 {
        m.push(0);
    }
    m
}

fn attr(kind: u16, value: &[u8]) -> Vec<u8> {
    let mut a = Vec::new();
    a.extend_from_slice(&u16::try_from(4 + value.len()).unwrap().to_ne_bytes());
    a.extend_from_slice(&kind.to_ne_bytes());
    a.extend_from_slice(value);
    while a.len() % 4 != 0 {
        a.push(0);
    }
    a
}

/// An `RTM_NEWADDR` (20) for 127.0.0.1/8 on interface 1, its cache info `cache`.
fn lo_address(seq: u32, cache: u8) -> Vec<u8> {
    let mut p = vec![AF_INET, 8, 0x80, 254, 1, 0, 0, 0];
    p.extend(attr(1, &[127, 0, 0, 1]));
    p.extend(attr(IFA_CACHEINFO, &[cache; 16]));
    msg(20, seq, &p)
}

#[test]
fn a_dump_is_compared_without_what_changes_with_time() {
    let d = &DUMPS[0];
    let done = msg(NLMSG_DONE, 3, &[0, 0, 0, 0]);
    let one = [lo_address(3, 1), done.clone()].concat();
    let later = [lo_address(3, 9), done.clone()].concat();
    let a = normalized(&one, d, 3).unwrap();
    let b = normalized(&later, d, 3).unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(a, b, "the cache info is dropped");
    let mut other = vec![AF_INET, 8, 0x80, 254, 1, 0, 0, 0];
    other.extend(attr(1, &[10, 0, 0, 1]));
    let other = [msg(20, 3, &other), done.clone()].concat();
    assert_ne!(
        normalized(&other, d, 3).unwrap(),
        a,
        "another address differs"
    );
    assert!(ends(&done).unwrap());
    assert!(!ends(&lo_address(3, 1)).unwrap());
}

#[test]
fn an_error_another_requests_answer_or_a_torn_message_is_unreadable() {
    let d = &DUMPS[0];
    let error = msg(NLMSG_ERROR, 3, &(-95i32).to_ne_bytes());
    assert_eq!(normalized(&error, d, 3), Err(Refused::Unreadable));
    assert_eq!(
        normalized(&lo_address(4, 1), d, 3),
        Err(Refused::Unreadable),
        "another request's"
    );
    let whole = lo_address(3, 1);
    for cut in [1, NLMSG_HDR - 1, NLMSG_HDR + 3, whole.len() - 1] {
        assert_eq!(
            normalized(&whole[..cut], d, 3),
            Err(Refused::Unreadable),
            "cut at {cut}"
        );
    }
}

proptest! {
    /// Any bytes give a verdict or a refusal, never a panic. 512 bytes is a fixture: room for headers, attributes
    /// and lengths that run past the end, which is what the readers must refuse.
    #[test]
    fn any_answer_or_text_is_read_without_a_panic(
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
        text in "\\PC*",
        seq in any::<u32>(),
    ) {
        for d in &DUMPS {
            let _ = normalized(&bytes, d, seq);
        }
        let _ = ends(&bytes);
        let _ = time_wait(&text);
        let _ = used(&text);
        let _ = in_use(&text);
        let _ = sockets(&text, &BTreeMap::new());
        let _ = links(&text);
    }
}

/// CLAUDE.md §2; Ada's 2026-11-06 rule (no picked constants): a dump's reads are bounded by its bytes, each read
/// that isn't refused holding at least a message's header, not by a count picked beside them. Do: answer a dump in
/// 65 reads of one header-only message each (one past the 64 once picked), then `NLMSG_DONE`, through a
/// socket pair that keeps each read apart. Expect: the answer read whole, every message of it kept.
#[test]
fn a_dumps_reads_are_bounded_by_its_bytes() {
    let (ours, theirs) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    let reads = 65;
    for _ in 0..reads {
        rustix::net::send(&theirs, &msg(20, 1, &[]), SendFlags::empty()).unwrap();
    }
    let done = msg(NLMSG_DONE, 1, &[0, 0, 0, 0]);
    rustix::net::send(&theirs, &done, SendFlags::empty()).unwrap();
    let mut scratch = Scratch::default();
    assert_eq!(
        ask(
            &ours,
            &[0; NLMSG_HDR],
            &mut scratch,
            &Patience::measuring("test")
        ),
        Ok(())
    );
    assert_eq!(scratch.answer.len(), reads * NLMSG_HDR + done.len());
}

/// The lead's failure bound (2026-11-06), with decision 9's rule (d6bfb3f0): measuring, with no record, a dump is
/// waited for with no bound, and its answers are recorded for the census that seeds it, never arming one, so no first
/// answer becomes the bound. Do: dump, with nothing recorded, through a socket pair whose other end has answered each of
/// the five dumps with its end. Expect: the dumps read, their answers seen, and still no bound.
#[test]
fn a_measuring_dump_is_waited_for_and_its_answers_recorded_unarmed() {
    let (ours, theirs) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    for seq in 1..=5 {
        let done = msg(NLMSG_DONE, seq, &[0, 0, 0, 0]);
        rustix::net::send(&theirs, &done, SendFlags::empty()).unwrap();
    }
    let slowest = Patience::measuring("test");
    let mut scratch = Scratch::default();
    assert_eq!(
        dumps(&ours, &mut scratch, &slowest),
        Ok(vec![Vec::new(); DUMPS.len()])
    );
    assert!(
        slowest.slowest().is_some(),
        "the dumps' answers are recorded"
    );
    assert_eq!(slowest.deadline(), None, "a measuring wait is never armed");
}

/// The lead's failure bound (2026-11-06): a read unanswered past twice the slowest answer recorded is refused, the
/// namespace thrown away, and it is no sample (Karn's point), but its answer is slower than the bound it ran out at,
/// so the next read's bound backs off to twice that (AC-T7.4.1: `Patience::ran_out`, RFC 6298 §5.5). Do: seed the
/// wait at 1 ms, then dump through a socket pair whose other end never answers. Expect: refused as unreadable once
/// 2 ms have passed, not held for good, and the bound 4 ms after.
#[test]
fn a_dump_unanswered_past_its_bound_is_refused_and_no_sample() {
    let (ours, _theirs) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    let slowest = Patience::seeded("test", Duration::from_millis(1));
    let mut scratch = Scratch::default();
    assert_eq!(
        dumps(&ours, &mut scratch, &slowest),
        Err(Refused::Unreadable)
    );
    assert_eq!(slowest.deadline(), Some(Duration::from_millis(4)));
}
