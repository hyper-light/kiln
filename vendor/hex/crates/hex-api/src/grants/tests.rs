use std::net::IpAddr;

use super::*;

fn member(kind: MemberKind, name: &str) -> Member {
    Member {
        kind,
        name: name.to_owned(),
    }
}

fn codex() -> Member {
    member(MemberKind::Harness, "codex")
}

fn tcp(first: u16, last: u16) -> Ports {
    Ports {
        proto: Proto::Tcp,
        first,
        last,
    }
}

fn ip(text: &str) -> IpAddr {
    text.parse().unwrap()
}

fn to_name(name: &str) -> Grant {
    Grant::Egress {
        member: codex(),
        to: Destination::Name(name.to_owned()),
        ports: tcp(443, 443),
    }
}

fn to_address(address: &str) -> Grant {
    Grant::Egress {
        member: codex(),
        to: Destination::Address(ip(address)),
        ports: tcp(443, 443),
    }
}

fn publish(member: Member, port: u16, host: &str, host_port: Option<u16>) -> Grant {
    Grant::Publish {
        member,
        proto: Proto::Tcp,
        port,
        host: ip(host),
        host_port,
    }
}

/// A harness's grants as kiln would give them: its providers by name, kiln by address and on its host, and one
/// port published on the host's loopback.
fn harness_grants() -> Vec<Grant> {
    vec![
        to_name("api.openai.com"),
        to_name("chatgpt.com"),
        Grant::Egress {
            member: codex(),
            to: Destination::Address(ip("100.64.0.7")),
            ports: tcp(7070, 7070),
        },
        Grant::Host {
            member: codex(),
            ports: tcp(7070, 7070),
        },
        Grant::Host {
            member: codex(),
            ports: Ports {
                proto: Proto::Udp,
                first: 7071,
                last: 7071,
            },
        },
        publish(codex(), 1455, "127.0.0.1", None),
        publish(codex(), 1455, "::1", Some(18080)),
    ]
}

fn refused_why(grants: &[Grant]) -> String {
    match check_grants(grants) {
        Err(Refusal::Invalid {
            what: "grants",
            why,
        }) => why,
        other => panic!("{grants:?} was not refused as invalid grants: {other:?}"),
    }
}

/// Joint plan step 15, AC-T7.7.2: do check a harness's providers by name, kiln by address and on its host, and a
/// loopback publish, then no grant at all (default deny); expect both to pass.
#[test]
fn a_harness_grant_set_and_the_empty_set_pass() {
    assert_eq!(check_grants(&harness_grants()), Ok(()));
    assert_eq!(check_grants(&[]), Ok(()));
}

/// Joint plan step 15, CLAUDE.md §3: do check a grant with each malformed field (hostile names, special addresses,
/// port 0, an unbound publish); expect each refused, typed, by the field it names, and none passed.
#[test]
fn a_malformed_grant_is_refused_by_the_field_it_names() {
    let long_member = "c".repeat(Member::MAX_NAME + 1);
    let long_label = format!("{}.example", "a".repeat(Destination::MAX_LABEL + 1));
    let long_name = format!(
        "{}.example",
        ["abcdefghi"; 25].join(".") // 249 characters before ".example": 257 in all
    );
    let with_member = |name: &str| Grant::Host {
        member: member(MemberKind::Agent, name),
        ports: tcp(7070, 7070),
    };
    let cases: Vec<(Grant, &str)> = vec![
        (
            with_member(""),
            "grants[0].member.name \"\" isn't 1 to 255 bytes",
        ),
        (with_member(&long_member), "isn't 1 to 255 bytes"),
        (
            with_member("Codex"),
            "doesn't start with a lowercase letter",
        ),
        (
            with_member("1codex"),
            "doesn't start with a lowercase letter",
        ),
        (with_member("co dex"), "holds ' '"),
        (with_member("codex/bin"), "holds '/'"),
        (
            to_name(""),
            "grants[0].to name \"\" isn't 1 to 253 characters",
        ),
        (to_name(&long_name), "isn't 1 to 253 characters"),
        (to_name("api.openai.com."), "root's dot"),
        (to_name("API.openai.com"), "holds 'A'"),
        (to_name("*.openai.com"), "is a pattern"),
        (to_name("api.*.com"), "is a pattern"),
        (to_name("api..openai.com"), "label of 0 octets"),
        (to_name(&long_label), "label of 64 octets"),
        (to_name("-api.openai.com"), "starts or ends with a hyphen"),
        (to_name("api-.openai.com"), "starts or ends with a hyphen"),
        (to_name("api_x.openai.com"), "holds '_'"),
        (to_name("bücher.example"), "holds 'ü'"),
        (to_name("kiln"), "has one label"),
        (to_name("localhost"), "has one label"),
        (to_name("10.0.0.5"), "all-numeric label"),
        (to_name("kiln.localhost"), "under localhost"),
        (
            to_address("0.0.0.0"),
            "grants[0].to address 0.0.0.0 is in 0.0.0.0/8",
        ),
        (to_address("0.1.2.3"), "is in 0.0.0.0/8"),
        (to_address("127.0.0.1"), "is loopback"),
        (to_address("127.8.9.1"), "is loopback"),
        (to_address("169.254.169.254"), "is link-local"),
        (to_address("224.0.0.1"), "is multicast"),
        (to_address("255.255.255.255"), "is the limited broadcast"),
        (to_address("::"), "is unspecified"),
        (to_address("::1"), "is loopback"),
        (to_address("fe80::1"), "is link-local"),
        (to_address("ff02::1"), "is multicast"),
        (to_address("::ffff:10.0.0.5"), "is IPv4-mapped"),
        (
            Grant::Host {
                member: codex(),
                ports: tcp(0, 10),
            },
            "grants[0].ports.first is 0",
        ),
        (
            Grant::Egress {
                member: codex(),
                to: Destination::Name("api.openai.com".to_owned()),
                ports: tcp(444, 443),
            },
            "grants[0].ports run from 444 down to 443",
        ),
        (
            publish(codex(), 0, "127.0.0.1", None),
            "grants[0].port is 0",
        ),
        (
            publish(codex(), 1455, "127.0.0.1", Some(0)),
            "grants[0].host_port is 0",
        ),
        (publish(codex(), 1455, "0.0.0.0", None), "is every address"),
        (publish(codex(), 1455, "::", None), "is every address"),
        (publish(codex(), 1455, "224.0.0.1", None), "is multicast"),
        (publish(codex(), 1455, "169.254.1.1", None), "is link-local"),
        (publish(codex(), 1455, "fe80::1", None), "is link-local"),
        (
            publish(codex(), 1455, "::ffff:127.0.0.1", None),
            "is IPv4-mapped",
        ),
    ];
    for (grant, expected) in cases {
        let why = refused_why(std::slice::from_ref(&grant));
        assert!(
            why.contains(expected),
            "{grant:?}: refused with {why:?}, not one holding {expected:?}"
        );
    }
    // The index names the grant that failed, not the first.
    let mut grants = harness_grants();
    grants.push(to_address("127.0.0.1"));
    let why = refused_why(&grants);
    assert!(why.starts_with("grants[7].to address"), "{why}");
}

/// Joint plan step 15 (N1; Shards D236): do check egress to `host.docker.internal`, the name Shards' destinations give
/// the VM's own host, which its network process takes to the host's loopback. Expect it refused by its field, naming
/// the host grant, since the host is reached through a `Host` grant alone; and another name under `.internal`, which
/// no Shards rule makes the host, passed.
#[test]
fn the_vms_host_by_its_shards_name_is_a_host_grant_never_an_egress_one() {
    let why = refused_why(&[to_name("host.docker.internal")]);
    assert!(
        why.starts_with("grants[0].to name \"host.docker.internal\"") && why.contains("host grant"),
        "{why}"
    );
    assert_eq!(check_grants(&[to_name("kiln.internal")]), Ok(()));
}

/// Joint plan step 15, AC-T7.7.2: do check grants that name one flow twice (overlapping ports to one destination or
/// the host, one published port for two members, one publish twice, one host socket for two ports); expect each
/// refused naming both grants, while grants that only look alike (another protocol, adjacent ports, another member,
/// another host address) pass.
#[test]
fn a_flow_named_by_two_grants_is_refused_and_one_named_once_passes() {
    let ranged = |member: Member, to: Option<&str>, ports: Ports| match to {
        Some(name) => Grant::Egress {
            member,
            to: Destination::Name(name.to_owned()),
            ports,
        },
        None => Grant::Host { member, ports },
    };
    let twice: Vec<(Vec<Grant>, &str)> = vec![
        (
            vec![
                ranged(codex(), Some("api.openai.com"), tcp(443, 443)),
                ranged(codex(), Some("api.openai.com"), tcp(400, 500)),
            ],
            "grants[1] and grants[0] both let harness codex reach api.openai.com on tcp/443",
        ),
        (
            vec![
                ranged(codex(), None, tcp(7000, 7100)),
                ranged(codex(), None, tcp(7100, 7100)),
            ],
            "both let harness codex reach its host on tcp/7100",
        ),
        (
            vec![
                ranged(codex(), Some("api.openai.com"), tcp(1, 10)),
                ranged(
                    member(MemberKind::Agent, "x"),
                    Some("api.openai.com"),
                    tcp(1, 10),
                ),
                ranged(codex(), Some("api.openai.com"), tcp(5, 6)),
            ],
            "grants[0] and grants[2]",
        ),
        (
            vec![
                publish(codex(), 1455, "127.0.0.1", None),
                publish(member(MemberKind::Agent, "x"), 1455, "::1", None),
            ],
            "publish tcp/1455 to agent x and harness codex: a published port has one receiver",
        ),
        (
            vec![
                publish(codex(), 1455, "127.0.0.1", None),
                publish(codex(), 1455, "127.0.0.1", None),
            ],
            "publish harness codex's tcp/1455 at 127.0.0.1 alike",
        ),
        (
            vec![
                publish(codex(), 1455, "127.0.0.1", Some(8080)),
                publish(codex(), 1456, "127.0.0.1", Some(8080)),
            ],
            "both publish at 127.0.0.1 tcp/8080: a host socket serves one port",
        ),
    ];
    for (grants, expected) in twice {
        let why = refused_why(&grants);
        assert!(
            why.contains(expected),
            "{grants:?}: {why:?}, not {expected:?}"
        );
    }
    let once = vec![
        ranged(codex(), Some("api.openai.com"), tcp(443, 443)),
        ranged(
            codex(),
            Some("api.openai.com"),
            Ports {
                proto: Proto::Udp,
                first: 443,
                last: 443,
            },
        ),
        ranged(codex(), Some("chatgpt.com"), tcp(400, 442)),
        ranged(codex(), Some("chatgpt.com"), tcp(443, 500)),
        ranged(
            member(MemberKind::Agent, "x"),
            Some("api.openai.com"),
            tcp(443, 443),
        ),
        Grant::Egress {
            member: codex(),
            to: Destination::Address(ip("104.18.0.1")),
            ports: tcp(443, 443),
        },
        ranged(codex(), None, tcp(443, 443)),
        publish(codex(), 1455, "127.0.0.1", None),
        publish(codex(), 1455, "::1", None),
        publish(codex(), 1455, "127.0.0.1", Some(8080)),
        publish(
            member(MemberKind::Agent, "x"),
            1456,
            "127.0.0.1",
            Some(8081),
        ),
    ];
    assert_eq!(check_grants(&once), Ok(()));
}

/// The conflicts a grant pair has, checked pair by pair: the model the sorted check is held to.
fn conflict(a: &Grant, b: &Grant) -> bool {
    let meet = |x: &Ports, y: &Ports| x.proto == y.proto && x.first <= y.last && y.first <= x.last;
    match (a, b) {
        (
            Grant::Egress {
                member: m,
                to: t,
                ports: p,
            },
            Grant::Egress {
                member: n,
                to: u,
                ports: q,
            },
        ) => m == n && t == u && meet(p, q),
        (
            Grant::Host {
                member: m,
                ports: p,
            },
            Grant::Host {
                member: n,
                ports: q,
            },
        ) => m == n && meet(p, q),
        (
            Grant::Publish {
                member: m,
                proto: p,
                port: g,
                host: h,
                host_port: k,
            },
            Grant::Publish {
                member: n,
                proto: q,
                port: f,
                host: i,
                host_port: l,
            },
        ) => {
            (p == q && g == f && (m != n || (h == i && k == l)))
                || (p == q && h == i && k.is_some() && k == l)
        }
        _ => false,
    }
}

/// AC-T7.7.2, CLAUDE.md §6 (oracle): do check every ordered pair and triple of a universe of well-formed grants
/// (two members, a name, an address and the host, both protocols, ranges that meet, nest, touch and part, and
/// publishes that share ports, hosts and host sockets); expect the sorted check to refuse exactly the lists in which
/// some pair conflicts by the pairwise model, so no flow is ever named by two grants, and nothing else is refused.
#[test]
fn the_sorted_check_refuses_exactly_what_the_pairwise_model_does() {
    let members = [codex(), member(MemberKind::Agent, "x")];
    let destinations = [
        Some(Destination::Name("api.openai.com".to_owned())),
        Some(Destination::Address(ip("10.0.0.5"))),
        None,
    ];
    let ranges = [(1, 1), (1, 5), (2, 3), (5, 9), (6, 9)];
    let mut universe = Vec::new();
    for member in &members {
        for to in &destinations {
            for proto in [Proto::Tcp, Proto::Udp] {
                for &(first, last) in &ranges {
                    let ports = Ports { proto, first, last };
                    universe.push(match to {
                        Some(to) => Grant::Egress {
                            member: member.clone(),
                            to: to.clone(),
                            ports,
                        },
                        None => Grant::Host {
                            member: member.clone(),
                            ports,
                        },
                    });
                }
            }
        }
        for port in [1, 2] {
            for host in ["127.0.0.1", "::1"] {
                for host_port in [None, Some(9), Some(10)] {
                    universe.push(publish(member.clone(), port, host, host_port));
                }
            }
        }
    }
    for grant in &universe {
        assert_eq!(
            check_grants(std::slice::from_ref(grant)),
            Ok(()),
            "{grant:?}"
        );
    }
    let mut refused = 0usize;
    let mut passed = 0usize;
    let mut judge = |list: &[&Grant]| {
        let model = list.iter().enumerate().any(|(i, a)| {
            list.iter()
                .skip(i + 1)
                .any(|b| conflict(a, b) || conflict(b, a))
        });
        let owned: Vec<Grant> = list.iter().map(|g| (*g).clone()).collect();
        let got = check_grants(&owned);
        assert_eq!(got.is_err(), model, "{owned:?}: {got:?}");
        if model {
            refused += 1;
        } else {
            passed += 1;
        }
    };
    for a in &universe {
        for b in &universe {
            judge(&[a, b]);
        }
    }
    // Triples over a slice of the universe that holds every kind of conflict: the first member's grants.
    let first = &universe[..universe.len() / 2];
    for a in first {
        for b in first {
            for c in first {
                judge(&[a, b, c]);
            }
        }
    }
    // Non-vacuity: both verdicts occur, many times.
    assert!(
        refused > 1_000 && passed > 1_000,
        "refused {refused}, passed {passed}"
    );
}

/// Joint plan step 15 (every grant visible in Hex's record of the VM): do write a harness's grants as a box's
/// record writes its spec (JSON) and read them back; expect the same grants, each written whole with its member,
/// destination, ports and way, and a grant of an unknown way refused rather than dropped.
#[test]
fn every_grant_is_written_whole_in_the_record_and_read_back() {
    let grants = harness_grants();
    let text = serde_json::to_string(&grants).unwrap();
    assert_eq!(serde_json::from_str::<Vec<Grant>>(&text).unwrap(), grants);
    for part in [
        r#"{"egress":{"member":{"kind":"harness","name":"codex"},"to":{"name":"api.openai.com"},"ports":{"proto":"tcp","first":443,"last":443}}}"#,
        r#"{"egress":{"member":{"kind":"harness","name":"codex"},"to":{"address":"100.64.0.7"},"ports":{"proto":"tcp","first":7070,"last":7070}}}"#,
        r#"{"host":{"member":{"kind":"harness","name":"codex"},"ports":{"proto":"udp","first":7071,"last":7071}}}"#,
        r#"{"publish":{"member":{"kind":"harness","name":"codex"},"proto":"tcp","port":1455,"host":"127.0.0.1"}}"#,
        r#"{"publish":{"member":{"kind":"harness","name":"codex"},"proto":"tcp","port":1455,"host":"::1","host_port":18080}}"#,
    ] {
        assert!(text.contains(part), "{part} not in {text}");
    }
    let unknown = r#"[{"ingress":{"member":{"kind":"harness","name":"codex"},"port":1455}}]"#;
    assert!(serde_json::from_str::<Vec<Grant>>(unknown).is_err());
}

/// Joint plan step 15 (N6): do check reports of published ports against a harness's grants: each publish once at
/// its address, one naming an egress grant, one past the grants, one at another address or host port, and one
/// grant reported twice. Expect: the first passes; each other is refused as invalid published ports, by its place.
#[test]
fn a_report_of_published_ports_is_held_to_the_grants() {
    let grants = harness_grants();
    let published = |grant: u32, at: &str| Published {
        grant,
        at: at.parse().unwrap(),
        receiver: Receiver::Declared,
    };
    assert_eq!(
        check_published(
            &grants,
            &[published(5, "127.0.0.1:49152"), published(6, "[::1]:18080")]
        ),
        Ok(())
    );
    for (report, expected) in [
        (
            vec![published(0, "127.0.0.1:443")],
            "published[0] names grants[0], which is no publish",
        ),
        (vec![published(7, "127.0.0.1:1")], "names grants[7]"),
        (
            vec![published(5, "10.0.0.2:49152")],
            "published[0] is bound at 10.0.0.2:49152",
        ),
        (
            vec![published(6, "[::1]:18081")],
            "not where grants[6] publishes it",
        ),
        (
            vec![published(5, "127.0.0.1:1"), published(5, "127.0.0.1:2")],
            "names one grant twice",
        ),
    ] {
        match check_published(&grants, &report) {
            Err(Refusal::Invalid {
                what: "published",
                why,
            }) => {
                assert!(
                    why.contains(expected),
                    "{report:?}: {why:?}, not {expected:?}"
                )
            }
            other => panic!("{report:?} was not refused: {other:?}"),
        }
    }
}
