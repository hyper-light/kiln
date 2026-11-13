use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use hex_api::{
    Destination, Grant, Member, MemberKind, Ports, Proto, Refusal, ShardsImage, Upstream,
};
use proptest::prelude::*;
use shards_api::image::{Keep, MemberReach, Narrowing};

use super::*;

/// Nothing past the microVM kept, and every member held to reaching nothing of it (D246: a member the request names in
/// no entry reaches nothing of the names kept).
fn nothing_kept() -> Narrowing {
    Narrowing {
        egress: Keep::Only(Vec::new()),
        mcp: Keep::Only(Vec::new()),
        destinations: Keep::Only(Vec::new()),
        dns: false,
        members: Some(Vec::new()),
    }
}

fn codex() -> Member {
    Member {
        kind: MemberKind::Harness,
        name: "codex".to_owned(),
    }
}

/// An image VM of the members alone (no command of its run's own), granted `grants`.
fn members_alone(grants: &[Grant]) -> ShardsImage {
    ShardsImage {
        grants: grants.to_vec(),
        ..ShardsImage::new(hex_api::sim::SIM_IMAGE)
    }
}

/// An image VM whose run has a command of its own, granted `grants`.
fn commanded(grants: &[Grant]) -> ShardsImage {
    ShardsImage {
        command: Some(vec!["serve".to_owned()]),
        ..members_alone(grants)
    }
}

/// Joint plan step 15, T-T7.7.1: do program no grant at all; expect no publish and the image's every grant past the
/// microVM dropped, its destinations by name (D236) among them, never the image's own grants that
/// `Narrowing::default()` keeps, and every member held apart, reaching nothing (D246: the request names every member's
/// reach, none named reaching nothing), so nothing passes the microVM (no network, or one whose network process lets
/// nothing past, D250).
#[test]
fn no_grant_is_programmed_as_no_network() {
    let made = program(&members_alone(&[])).unwrap();
    assert!(made.publish.is_empty());
    assert_eq!(made.egress, nothing_kept());
    assert_eq!(made.egress.destinations, Keep::Only(Vec::new()));
    assert_eq!(made.egress.members, Some(Vec::new()));
    assert_eq!(Narrowing::default().destinations, Keep::All);
    assert_ne!(made.egress, Narrowing::default());
}

/// A member's reach as the boot's narrowing names it (`Narrowing::members`).
fn reach(kind: shards_api::image::MemberKind, name: &str, destinations: &[&str]) -> MemberReach {
    MemberReach {
        kind,
        name: name.to_owned(),
        destinations: destinations.iter().map(|d| (*d).to_owned()).collect(),
    }
}

/// Joint plan step 15 (N4; ROOT's rulings of 2026-11-12): do program each destination a harness's grants name by TCP
/// (a provider by name, for a harness and an agent; kiln by its IPv4 address; kiln on the VM's host, its Shards name
/// `host.docker.internal`). Expect each kept as D236 writes it (`host:port`), and its member held to it alone
/// (`Narrowing::members`), which the run host holds every member of the image to before the VM starts. Do program a
/// harness granted a provider over two ports and kiln on its host beside an agent granted that provider's first port,
/// and a publish. Expect every destination kept once, sorted, each member held to its own, sorted, and the publish
/// programmed. Do program what D236 can't express at all (a name by UDP; an IPv6 address; an IPv4 address by UDP; the
/// host's service by UDP); expect each refused naming its own capability, after a destination too.
#[test]
fn each_destination_is_kept_and_its_member_held_to_it() {
    use shards_api::image::MemberKind as Kind;
    let tcp = |port| Ports {
        proto: Proto::Tcp,
        first: port,
        last: port,
    };
    let udp = |port| Ports {
        proto: Proto::Udp,
        ..tcp(port)
    };
    let agent = Member {
        kind: MemberKind::Agent,
        name: "x".to_owned(),
    };
    let egress = |member: &Member, to: Destination, ports| Grant::Egress {
        member: member.clone(),
        to,
        ports,
    };
    let name = |text: &str| Destination::Name(text.to_owned());
    let address = |text: &str| Destination::Address(text.parse().unwrap());
    let host = |member: &Member, ports| Grant::Host {
        member: member.clone(),
        ports,
    };
    for (grant, kept, held) in [
        (
            egress(&codex(), name("api.openai.com"), tcp(443)),
            "api.openai.com:443",
            reach(Kind::Harness, "codex", &["api.openai.com:443"]),
        ),
        (
            egress(&agent, name("mcp.example.com"), tcp(443)),
            "mcp.example.com:443",
            reach(Kind::Agent, "x", &["mcp.example.com:443"]),
        ),
        (
            egress(&codex(), address("10.0.0.5"), tcp(7070)),
            "10.0.0.5:7070",
            reach(Kind::Harness, "codex", &["10.0.0.5:7070"]),
        ),
        (
            host(&codex(), tcp(7070)),
            "host.docker.internal:7070",
            reach(Kind::Harness, "codex", &["host.docker.internal:7070"]),
        ),
    ] {
        let made = program(&members_alone(std::slice::from_ref(&grant))).unwrap();
        assert_eq!(
            made.egress,
            Narrowing {
                destinations: Keep::Only(vec![kept.to_owned()]),
                members: Some(vec![held]),
                ..nothing_kept()
            },
            "{grant:?}"
        );
        assert!(made.publish.is_empty());
    }
    let publish = Grant::Publish {
        member: codex(),
        proto: Proto::Tcp,
        port: 1455,
        host: "127.0.0.1".parse().unwrap(),
        host_port: None,
    };
    let list = [
        egress(
            &codex(),
            name("api.openai.com"),
            Ports {
                proto: Proto::Tcp,
                first: 443,
                last: 444,
            },
        ),
        host(&codex(), tcp(7070)),
        egress(&agent, name("api.openai.com"), tcp(443)),
        publish,
    ];
    let made = program(&members_alone(&list)).unwrap();
    assert_eq!(
        made.egress,
        Narrowing {
            destinations: Keep::Only(vec![
                "api.openai.com:443".to_owned(),
                "api.openai.com:444".to_owned(),
                "host.docker.internal:7070".to_owned(),
            ]),
            members: Some(vec![
                reach(Kind::Agent, "x", &["api.openai.com:443"]),
                reach(
                    Kind::Harness,
                    "codex",
                    &[
                        "api.openai.com:443",
                        "api.openai.com:444",
                        "host.docker.internal:7070",
                    ],
                ),
            ]),
            ..nothing_kept()
        }
    );
    assert_eq!(made.publish.len(), 1);
    let beyond_d236 = [
        (
            egress(&codex(), name("api.openai.com"), udp(443)),
            Upstream::MemberGrantsByName,
        ),
        (
            egress(&codex(), address("2001:db8::5"), tcp(7070)),
            Upstream::MemberGrantsByAddress,
        ),
        (
            egress(&codex(), address("10.0.0.5"), udp(7070)),
            Upstream::MemberGrantsByAddress,
        ),
        (host(&codex(), udp(7070)), Upstream::MemberHostService),
    ];
    for (grant, needs) in &beyond_d236 {
        assert_eq!(
            program(&members_alone(std::slice::from_ref(grant))),
            Err(Refusal::Awaits { needs: *needs }),
            "{grant:?}"
        );
        let after = [list[0].clone(), grant.clone()];
        assert_eq!(
            program(&members_alone(&after)),
            Err(Refusal::Awaits { needs: *needs }),
            "{after:?}"
        );
    }
    // A malformed grant is the check's refusal, before anything is programmed.
    assert!(matches!(
        program(&members_alone(&[Grant::Host {
            member: codex(),
            ports: tcp(0),
        }])),
        Err(Refusal::Invalid { what: "grants", .. })
    ));
}

/// Joint plan step 15 (N6): do program a publish of a harness's port for a run with a command of its own, and for a
/// run of the members alone. Expect it programmed alike for both, nothing kept past the VM: Shards names each
/// published port's receivers as the boot returns (D231's `Bound::to`), and the run is held to them there
/// ([`bound`]). Do program it beside a provider by name on the commanded run, and no grant at all; expect the
/// provider kept for its harness alone beside the publish, and no grant still no network.
#[test]
fn a_publish_on_a_run_with_its_own_command_is_programmed() {
    let publish = Grant::Publish {
        member: codex(),
        proto: Proto::Tcp,
        port: 1455,
        host: "127.0.0.1".parse().unwrap(),
        host_port: None,
    };
    let commanded_run = program(&commanded(std::slice::from_ref(&publish))).unwrap();
    let members_run = program(&members_alone(std::slice::from_ref(&publish))).unwrap();
    assert_eq!(commanded_run, members_run);
    assert_eq!(commanded_run.publish.len(), 1);
    assert_eq!(commanded_run.egress, nothing_kept());
    let by_name = Grant::Egress {
        member: codex(),
        to: Destination::Name("api.openai.com".to_owned()),
        ports: Ports {
            proto: Proto::Tcp,
            first: 443,
            last: 443,
        },
    };
    let beside = program(&commanded(&[publish, by_name])).unwrap();
    assert_eq!(beside.publish, commanded_run.publish);
    assert_eq!(
        beside.egress.members,
        Some(vec![reach(
            shards_api::image::MemberKind::Harness,
            "codex",
            &["api.openai.com:443"]
        )])
    );
    let none = program(&commanded(&[])).unwrap();
    assert!(none.publish.is_empty());
    assert_eq!(none.egress, nothing_kept());
}

/// What a list of grants awaits, by a model of N4 apart from the program's: the first grant D236's destinations can't
/// express at all (UDP; an IPv6 address) awaits its own capability; nothing else awaits Shards.
fn awaited(list: &[Grant]) -> Option<Upstream> {
    list.iter().find_map(|grant| match grant {
        Grant::Egress {
            to: Destination::Name(_),
            ports,
            ..
        } if ports.proto == Proto::Udp => Some(Upstream::MemberGrantsByName),
        Grant::Egress {
            to: Destination::Address(IpAddr::V6(_)),
            ..
        } => Some(Upstream::MemberGrantsByAddress),
        Grant::Egress { ports, .. } if ports.proto == Proto::Udp => {
            Some(Upstream::MemberGrantsByAddress)
        }
        Grant::Host { ports, .. } if ports.proto == Proto::Udp => Some(Upstream::MemberHostService),
        _ => None,
    })
}

/// The destinations a list names by TCP, as D236 writes each (`host:port`, every port of a grant's range), and each
/// member's: what a program must keep, and hold each member to, by a model apart from the program's. No remote MCP
/// server is kept, so of D246's names kept (destinations and remote MCP servers alike) every one is a destination the
/// list names; and each member has one entry, so the run host's refusal of a member's second entry (D246) never
/// meets a program.
fn reach_of(list: &[Grant]) -> Narrowing {
    let mut kept = BTreeSet::new();
    let mut held: BTreeMap<(MemberKind, String), BTreeSet<String>> = BTreeMap::new();
    for grant in list {
        let (member, host, ports) = match grant {
            Grant::Egress {
                member,
                to: Destination::Name(name),
                ports,
            } => (member, name.clone(), ports),
            Grant::Egress {
                member,
                to: Destination::Address(address),
                ports,
            } => (member, address.to_string(), ports),
            Grant::Host { member, ports } => (member, "host.docker.internal".to_owned(), ports),
            Grant::Publish { .. } => continue,
        };
        for port in ports.first..=ports.last {
            let at = format!("{host}:{port}");
            kept.insert(at.clone());
            held.entry((member.kind, member.name.clone()))
                .or_default()
                .insert(at);
        }
    }
    let members = held
        .into_iter()
        .map(|((kind, name), at)| MemberReach {
            kind: match kind {
                MemberKind::Agent => shards_api::image::MemberKind::Agent,
                MemberKind::Harness => shards_api::image::MemberKind::Harness,
            },
            name,
            destinations: at.into_iter().collect(),
        })
        .collect::<Vec<_>>();
    Narrowing {
        destinations: Keep::Only(kept.into_iter().collect()),
        members: Some(members),
        ..nothing_kept()
    }
}

fn any_publish() -> impl Strategy<Value = Grant> {
    (
        any_member(),
        any::<bool>(),
        1u16..=u16::MAX,
        prop_oneof![Just("127.0.0.1"), Just("::1"), Just("10.0.0.2")],
        proptest::option::of(1u16..=u16::MAX),
    )
        .prop_map(|(member, udp, port, host, host_port)| Grant::Publish {
            member,
            proto: if udp { Proto::Udp } else { Proto::Tcp },
            port,
            host: host.parse().unwrap(),
            host_port,
        })
}

fn any_member() -> impl Strategy<Value = Member> {
    (any::<bool>(), "[a-z][a-z0-9._-]{0,6}").prop_map(|(agent, name)| Member {
        kind: if agent {
            MemberKind::Agent
        } else {
            MemberKind::Harness
        },
        name,
    })
}

fn any_grant() -> impl Strategy<Value = Grant> {
    let ports = (any::<bool>(), 1u16..=u16::MAX, 0u16..64).prop_map(|(udp, first, span)| Ports {
        proto: if udp { Proto::Udp } else { Proto::Tcp },
        first,
        last: first.saturating_add(span),
    });
    let to = prop_oneof![
        "[a-z]([a-z0-9-]{0,6}[a-z0-9])?\\.[a-z]{2,6}".prop_map(Destination::Name),
        any::<[u8; 4]>().prop_map(|a| Destination::Address(IpAddr::from(a))),
    ];
    // Publishes weigh twice, so lists of publishes alone, which program, are common.
    prop_oneof![
        (any_member(), to, ports.clone()).prop_map(|(member, to, ports)| Grant::Egress {
            member,
            to,
            ports
        }),
        (any_member(), ports).prop_map(|(member, ports)| Grant::Host { member, ports }),
        any_publish(),
        any_publish(),
    ]
    .prop_filter("a well-formed grant", |g| {
        hex_api::check_grants(std::slice::from_ref(g)).is_ok()
    })
}

proptest! {
    /// AC-T7.7.2 (joint plan step 15): do program any well-formed list of grants of every way, for a run of the
    /// members alone and for one with a command of its own; expect either a typed `Awaits` naming what the list
    /// awaits (`awaited`, N4), or a program that keeps of the image's grants past the microVM exactly the list's
    /// destinations and no remote MCP server (D246's names kept), each member held to its own in one entry
    /// (`reach_of`), and publishes exactly the list's publish grants, in order, each at the host address and port it
    /// names. So the flows a program allows that no grant names are exactly 0.
    #[test]
    fn a_program_allows_no_flow_its_grants_do_not_name(
        list in proptest::collection::vec(any_grant(), 0..6)
            .prop_filter("each flow named once", |l| hex_api::check_grants(l).is_ok()),
        with_command in any::<bool>(),
    ) {
        let image = if with_command { commanded(&list) } else { members_alone(&list) };
        match program(&image) {
            Ok(made) => {
                prop_assert_eq!(awaited(&list), None);
                prop_assert_eq!(&made.egress, &reach_of(&list));
                let publishes: Vec<&Grant> =
                    list.iter().filter(|g| matches!(g, Grant::Publish { .. })).collect();
                prop_assert_eq!(made.publish.len(), publishes.len());
                for (published, grant) in made.publish.iter().zip(publishes) {
                    let Grant::Publish { proto, port, host, host_port, .. } = grant else {
                        return Err(TestCaseError::fail("a published port no publish grant names"));
                    };
                    prop_assert_eq!(published.guest_port, *port);
                    prop_assert_eq!(published.host, Some(*host));
                    prop_assert_eq!(published.host_port, host_port.unwrap_or(0));
                    prop_assert_eq!(
                        published.proto,
                        match proto {
                            Proto::Tcp => shards_api::image::Proto::Tcp,
                            Proto::Udp => shards_api::image::Proto::Udp,
                        }
                    );
                }
            }
            Err(Refusal::Awaits { needs }) => {
                prop_assert_eq!(Some(needs), awaited(&list));
            }
            Err(other) => return Err(TestCaseError::fail(format!("{other:?}"))),
        }
    }
}

fn running(
    kind: shards_api::image::MemberKind,
    name: &str,
    runs: bool,
) -> shards_api::image::MemberInfo {
    shards_api::image::MemberInfo {
        index: 0,
        kind,
        name: name.to_owned(),
        runs,
    }
}

/// A TCP port Shards bound at `at` for guest port `guest_port`, reaching each `(member index, member's port)` of `to`
/// (D231's `Bound::to`; none: the run's own command).
fn bound_at(guest_port: u16, at: &str, to: &[(u32, u16)]) -> shards_api::image::Bound {
    shards_api::image::Bound {
        guest_port,
        proto: shards_api::image::Proto::Tcp,
        host: at.parse().unwrap(),
        to: to
            .iter()
            .map(|&(member, port)| shards_api::image::Receiving { member, port })
            .collect(),
    }
}

/// Joint plan step 15 (N6): do hold a run's report to its grants: two publishes of a running harness (one on a port
/// the OS picked, one on the port its grant names and that its image maps to the harness's own port 8443) beside an
/// egress grant, each reaching that harness alone. Expect: each publish reported once, by its grant's place, at the
/// socket the host bound, its receiver the member Shards names, at the member's own port.
#[test]
fn a_runs_publishes_are_reported_by_their_grants() {
    let grants = vec![
        Grant::Egress {
            member: codex(),
            to: Destination::Name("api.openai.com".to_owned()),
            ports: Ports {
                proto: Proto::Tcp,
                first: 443,
                last: 443,
            },
        },
        Grant::Publish {
            member: codex(),
            proto: Proto::Tcp,
            port: 1455,
            host: "127.0.0.1".parse().unwrap(),
            host_port: None,
        },
        Grant::Publish {
            member: codex(),
            proto: Proto::Tcp,
            port: 8080,
            host: "::1".parse().unwrap(),
            host_port: Some(18080),
        },
    ];
    let members = [running(
        shards_api::image::MemberKind::Harness,
        "codex",
        true,
    )];
    let published = [
        bound_at(8080, "[::1]:18080", &[(0, 8443)]),
        bound_at(1455, "127.0.0.1:49200", &[(0, 1455)]),
    ];
    assert_eq!(
        bound(&grants, &members, &published).unwrap(),
        vec![
            hex_api::Published {
                grant: 1,
                at: "127.0.0.1:49200".parse().unwrap(),
                receiver: hex_api::Receiver::Named { port: 1455 },
            },
            hex_api::Published {
                grant: 2,
                at: "[::1]:18080".parse().unwrap(),
                receiver: hex_api::Receiver::Named { port: 8443 },
            },
        ]
    );
}

/// Joint plan step 15 (N6): do hold a run's report to a publish grant when the image lacks the grant's member, has
/// it as files alone, or as an agent of that name; when the host bound nothing for the grant, bound it at another
/// address or on another host port; and when it bound one past the grants. Expect: the member refused Invalid
/// naming the grant's member, the sockets Broken naming what Shards bound, never a report of an ungranted flow.
#[test]
fn a_runs_report_that_breaks_its_grants_is_refused() {
    let grants = vec![Grant::Publish {
        member: codex(),
        proto: Proto::Tcp,
        port: 1455,
        host: "127.0.0.1".parse().unwrap(),
        host_port: Some(18080),
    }];
    let harness = shards_api::image::MemberKind::Harness;
    let agent = shards_api::image::MemberKind::Agent;
    let at_grant = [bound_at(1455, "127.0.0.1:18080", &[(0, 1455)])];
    for members in [
        vec![],
        vec![running(harness, "codex", false)],
        vec![running(agent, "codex", true)],
        vec![running(harness, "other", true)],
    ] {
        assert!(matches!(
            bound(&grants, &members, &at_grant),
            Err(Refusal::Invalid { what: "grants", why }) if why.contains("grants[0].member harness codex")
        ));
    }
    let members = [running(harness, "codex", true)];
    for published in [
        vec![],
        vec![bound_at(1455, "10.0.0.2:18080", &[(0, 1455)])],
        vec![bound_at(1455, "127.0.0.1:18081", &[(0, 1455)])],
        vec![bound_at(1456, "127.0.0.1:18080", &[(0, 1456)])],
    ] {
        assert!(matches!(
            bound(&grants, &members, &published),
            Err(Refusal::Broken(why)) if why.contains("no host socket for grants[0]")
        ));
    }
    let extra = [
        bound_at(1455, "127.0.0.1:18080", &[(0, 1455)]),
        bound_at(22, "0.0.0.0:2222", &[]),
    ];
    assert!(matches!(
        bound(&grants, &members, &extra),
        Err(Refusal::Broken(why)) if why.contains("guest port 22") && why.contains("no grant names")
    ));
    assert_eq!(bound(&grants, &members, &at_grant).unwrap().len(), 1);
}

/// Joint plan step 15 (N6): do hold a run's report to a publish grant of harness codex (declared index 0) when the
/// port Shards bound for it reaches the run's own command (no member), another member alone, codex and another
/// member alike (the one listening takes each connection), or a member index the image doesn't declare. Expect the
/// first three refused Invalid naming the grant and whom the port reaches, the last Broken (Shards named no member of
/// its own image), never a report of a flow to anyone the grant doesn't name. Do bound it reaching codex alone at the
/// port its image maps it to; expect it reported, its receiver codex at that port.
#[test]
fn a_publish_reaching_anyone_but_its_member_is_refused() {
    let grants = vec![Grant::Publish {
        member: codex(),
        proto: Proto::Tcp,
        port: 1455,
        host: "127.0.0.1".parse().unwrap(),
        host_port: None,
    }];
    let harness = shards_api::image::MemberKind::Harness;
    let agent = shards_api::image::MemberKind::Agent;
    let mut helper = running(agent, "helper", true);
    helper.index = 1;
    let members = [running(harness, "codex", true), helper];
    for (to, says) in [
        (vec![], "the run's own command"),
        (vec![(1, 1455)], "agent helper"),
        (vec![(0, 1455), (1, 1455)], "agent helper"),
        (vec![(1, 1455), (0, 1455)], "agent helper"),
    ] {
        let published = [bound_at(1455, "127.0.0.1:49200", &to)];
        assert!(
            matches!(
                bound(&grants, &members, &published),
                Err(Refusal::Invalid { what: "grants", ref why })
                    if why.starts_with("grants[0]") && why.contains(says)
            ),
            "{to:?}: {:?}",
            bound(&grants, &members, &published)
        );
    }
    let unknown = [bound_at(1455, "127.0.0.1:49200", &[(7, 1455)])];
    assert!(
        matches!(
            bound(&grants, &members, &unknown),
            Err(Refusal::Broken(ref why)) if why.contains("member 7")
        ),
        "{:?}",
        bound(&grants, &members, &unknown)
    );
    let mapped = [bound_at(1455, "127.0.0.1:49200", &[(0, 8080)])];
    assert_eq!(
        bound(&grants, &members, &mapped).unwrap(),
        vec![hex_api::Published {
            grant: 0,
            at: "127.0.0.1:49200".parse().unwrap(),
            receiver: hex_api::Receiver::Named { port: 8080 },
        }]
    );
}
