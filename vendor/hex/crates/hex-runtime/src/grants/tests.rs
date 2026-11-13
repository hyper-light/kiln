use std::net::IpAddr;

use hex_api::{
    Destination, Grant, Member, MemberKind, Ports, Proto, Refusal, ShardsImage, Upstream,
};
use proptest::prelude::*;
use shards_api::image::{Keep, Narrowing};

use super::*;

fn nothing_kept() -> Narrowing {
    Narrowing {
        egress: Keep::Only(Vec::new()),
        mcp: Keep::Only(Vec::new()),
        destinations: Keep::Only(Vec::new()),
        dns: false,
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
/// microVM dropped, so its run has no network (`shards api-run`'s `network none`), never the image's own grants that
/// `Narrowing::default()` keeps.
#[test]
fn no_grant_is_programmed_as_no_network() {
    let made = program(&members_alone(&[])).unwrap();
    assert!(made.publish.is_empty());
    assert_eq!(made.egress, nothing_kept());
    assert_ne!(made.egress, Narrowing::default());
}

/// Joint plan step 15: do program a harness's grants as kiln would give them (a provider by name, kiln by address,
/// kiln on the VM's host, a publish); expect each one Shards 482c5d78 can't hold exactly refused, typed, naming the
/// capability it awaits, and a list of publishes alone programmed at the host address and port each names.
#[test]
fn each_grant_482c5d78_cant_hold_is_refused_naming_what_it_awaits() {
    let tcp = |port| Ports {
        proto: Proto::Tcp,
        first: port,
        last: port,
    };
    for (grant, needs) in [
        (
            Grant::Egress {
                member: codex(),
                to: Destination::Name("api.openai.com".to_owned()),
                ports: tcp(443),
            },
            Upstream::MemberGrantsByName,
        ),
        (
            Grant::Egress {
                member: Member {
                    kind: MemberKind::Agent,
                    name: "x".to_owned(),
                },
                to: Destination::Name("mcp.example.com".to_owned()),
                ports: tcp(443),
            },
            Upstream::MemberGrantsByName,
        ),
        (
            Grant::Egress {
                member: codex(),
                to: Destination::Address("10.0.0.5".parse().unwrap()),
                ports: tcp(7070),
            },
            Upstream::MemberGrantsByAddress,
        ),
        (
            Grant::Host {
                member: codex(),
                ports: tcp(7070),
            },
            Upstream::MemberHostService,
        ),
    ] {
        assert_eq!(
            program(&members_alone(std::slice::from_ref(&grant))),
            Err(Refusal::Awaits { needs }),
            "{grant:?}"
        );
    }
    let publishes = [
        Grant::Publish {
            member: codex(),
            proto: Proto::Tcp,
            port: 1455,
            host: "127.0.0.1".parse().unwrap(),
            host_port: None,
        },
        Grant::Publish {
            member: codex(),
            proto: Proto::Udp,
            port: 1455,
            host: "::1".parse().unwrap(),
            host_port: Some(18080),
        },
    ];
    let made = program(&members_alone(&publishes)).unwrap();
    assert_eq!(made.egress, nothing_kept());
    assert_eq!(
        made.publish,
        vec![
            shards_api::image::Publish {
                guest_port: 1455,
                proto: shards_api::image::Proto::Tcp,
                host: Some("127.0.0.1".parse().unwrap()),
                host_port: 0,
            },
            shards_api::image::Publish {
                guest_port: 1455,
                proto: shards_api::image::Proto::Udp,
                host: Some("::1".parse().unwrap()),
                host_port: 18080,
            },
        ]
    );
    // A malformed grant is the check's refusal, before anything is programmed.
    assert!(matches!(
        program(&members_alone(&[Grant::Host {
            member: codex(),
            ports: tcp(0),
        }])),
        Err(Refusal::Invalid { what: "grants", .. })
    ));
}

/// Joint plan step 15 (N6's receiver gap): do program a publish of a harness's port for a run with a command of its
/// own, alone and after an egress by name, and that run with no grant at all. Expect: the publish refused
/// `Awaits(PublishReceiver)`, since Shards' run host gives such a run's command every port its Agentfile gives no
/// member (D122) and names no port's receiver, so the grant's member can't be held; the first unheld grant of the
/// list refused first; and no grant still no network. Do program the same publish for a run of the members alone;
/// expect it programmed.
#[test]
fn a_publish_on_a_run_with_its_own_command_awaits_its_receivers_name() {
    let publish = Grant::Publish {
        member: codex(),
        proto: Proto::Tcp,
        port: 1455,
        host: "127.0.0.1".parse().unwrap(),
        host_port: None,
    };
    assert_eq!(
        program(&commanded(std::slice::from_ref(&publish))),
        Err(Refusal::Awaits {
            needs: Upstream::PublishReceiver
        })
    );
    let by_name = Grant::Egress {
        member: codex(),
        to: Destination::Name("api.openai.com".to_owned()),
        ports: Ports {
            proto: Proto::Tcp,
            first: 443,
            last: 443,
        },
    };
    assert_eq!(
        program(&commanded(&[by_name, publish.clone()])),
        Err(Refusal::Awaits {
            needs: Upstream::MemberGrantsByName
        })
    );
    let none = program(&commanded(&[])).unwrap();
    assert!(none.publish.is_empty());
    assert_eq!(none.egress, nothing_kept());
    let made = program(&members_alone(std::slice::from_ref(&publish))).unwrap();
    assert_eq!(made.publish.len(), 1);
    assert_eq!(made.egress, nothing_kept());
}

/// The capability a grant 482c5d78 can't hold awaits, on a run with a command of its own or not; none for one it
/// holds.
fn awaited(grant: &Grant, commanded: bool) -> Option<Upstream> {
    match grant {
        Grant::Egress {
            to: Destination::Name(_),
            ..
        } => Some(Upstream::MemberGrantsByName),
        Grant::Egress {
            to: Destination::Address(_),
            ..
        } => Some(Upstream::MemberGrantsByAddress),
        Grant::Host { .. } => Some(Upstream::MemberHostService),
        Grant::Publish { .. } if commanded => Some(Upstream::PublishReceiver),
        Grant::Publish { .. } => None,
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
    /// members alone and for one with a command of its own; expect either a typed `Awaits` naming the capability of
    /// its first grant 482c5d78 can't hold exactly (any publish, on a run with its own command), or a program that
    /// keeps none of the image's grants past the microVM and publishes exactly the list's publish grants, in order,
    /// each at the host address and port it names. So the flows a program allows that no grant names are exactly 0.
    #[test]
    fn a_program_allows_no_flow_its_grants_do_not_name(
        list in proptest::collection::vec(any_grant(), 0..6)
            .prop_filter("each flow named once", |l| hex_api::check_grants(l).is_ok()),
        with_command in any::<bool>(),
    ) {
        let image = if with_command { commanded(&list) } else { members_alone(&list) };
        match program(&image) {
            Ok(made) => {
                prop_assert!(list.iter().all(|g| awaited(g, with_command).is_none()));
                prop_assert_eq!(&made.egress, &nothing_kept());
                prop_assert_eq!(made.publish.len(), list.len());
                for (published, grant) in made.publish.iter().zip(&list) {
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
                prop_assert_eq!(Some(needs), list.iter().find_map(|g| awaited(g, with_command)));
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

fn bound_at(guest_port: u16, at: &str) -> shards_api::image::Bound {
    shards_api::image::Bound {
        guest_port,
        proto: shards_api::image::Proto::Tcp,
        host: at.parse().unwrap(),
        to: Vec::new(),
    }
}

/// Joint plan step 15 (N6): do hold a run's report to its grants: two publishes of a running harness (one on a port
/// the OS picked, one on the port its grant names) beside an egress grant. Expect: each publish reported once, by
/// its grant's place, at the socket the host bound, its receiver the image's declaration.
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
        bound_at(8080, "[::1]:18080"),
        bound_at(1455, "127.0.0.1:49200"),
    ];
    assert_eq!(
        bound(&grants, &members, &published).unwrap(),
        vec![
            hex_api::Published {
                grant: 1,
                at: "127.0.0.1:49200".parse().unwrap(),
                receiver: hex_api::Receiver::Declared,
            },
            hex_api::Published {
                grant: 2,
                at: "[::1]:18080".parse().unwrap(),
                receiver: hex_api::Receiver::Declared,
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
    let at_grant = [bound_at(1455, "127.0.0.1:18080")];
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
        vec![bound_at(1455, "10.0.0.2:18080")],
        vec![bound_at(1455, "127.0.0.1:18081")],
        vec![bound_at(1456, "127.0.0.1:18080")],
    ] {
        assert!(matches!(
            bound(&grants, &members, &published),
            Err(Refusal::Broken(why)) if why.contains("no host socket for grants[0]")
        ));
    }
    let extra = [
        bound_at(1455, "127.0.0.1:18080"),
        bound_at(22, "0.0.0.0:2222"),
    ];
    assert!(matches!(
        bound(&grants, &members, &extra),
        Err(Refusal::Broken(why)) if why.contains("guest port 22") && why.contains("no grant names")
    ));
    assert_eq!(bound(&grants, &members, &at_grant).unwrap().len(), 1);
}
