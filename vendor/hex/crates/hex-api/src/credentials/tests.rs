use super::*;
use crate::{MemberKind, Ports};

fn member(kind: MemberKind, name: &str) -> Member {
    Member {
        kind,
        name: name.to_owned(),
    }
}

fn claude() -> Member {
    member(MemberKind::Harness, "claude")
}

fn egress(member: Member, name: &str, proto: Proto) -> Grant {
    Grant::Egress {
        member,
        to: Destination::Name(name.to_owned()),
        ports: Ports {
            proto,
            first: 443,
            last: 443,
        },
    }
}

fn credential(member: Member, reference: &str, toward: &[&str]) -> CredentialGrant {
    CredentialGrant {
        member,
        reference: reference.to_owned(),
        toward: toward.iter().map(|h| (*h).to_owned()).collect(),
    }
}

/// Claude Code's harness as kiln would grant it (joint plan step 33's example): its providers by name, by TCP, and
/// its account's credential toward both.
fn harness() -> (Vec<Grant>, Vec<CredentialGrant>) {
    (
        vec![
            egress(claude(), "api.anthropic.com", Proto::Tcp),
            egress(claude(), "platform.claude.com", Proto::Tcp),
        ],
        vec![credential(
            claude(),
            "anthropic/ada",
            &["api.anthropic.com", "platform.claude.com"],
        )],
    )
}

fn refused(credentials: &[CredentialGrant], grants: &[Grant]) -> String {
    match check_credentials(credentials, grants) {
        Err(Refusal::Invalid {
            what: "credentials",
            why,
        }) => why,
        other => panic!("not refused as invalid credentials: {other:?}"),
    }
}

/// AC-T7.7.2, joint plan step 33 (C2): do check a harness's credential toward the providers its own egress grants
/// reach by name, by TCP, and a box granted none. Expect both taken.
#[test]
fn w33_a_credential_toward_its_members_own_egress_hosts_is_taken() {
    let (grants, credentials) = harness();
    assert_eq!(check_credentials(&credentials, &grants), Ok(()));
    assert_eq!(check_credentials(&[], &grants), Ok(()));
    assert_eq!(check_credentials(&[], &[]), Ok(()));
}

/// AC-T7.7.2 (C2): do check a credential with each field malformed in turn. Expect each refused as invalid
/// credentials naming its field: a member's name no Agentfile takes, a reference no store could hold (empty, past
/// REFERENCE_MAX, a space), no host, a host named twice.
#[test]
fn w33_each_malformed_field_is_refused_by_name() {
    let (grants, _) = harness();
    let at_most = "r".repeat(REFERENCE_MAX);
    assert_eq!(
        check_credentials(
            &[credential(claude(), &at_most, &["api.anthropic.com"])],
            &grants
        ),
        Ok(())
    );
    let past = "r".repeat(REFERENCE_MAX + 1);
    let cases = [
        (
            credential(
                member(MemberKind::Harness, "Claude"),
                "anthropic/ada",
                &["api.anthropic.com"],
            ),
            "credentials[0].member.name",
        ),
        (
            credential(claude(), "", &["api.anthropic.com"]),
            "credentials[0].reference",
        ),
        (
            credential(claude(), &past, &["api.anthropic.com"]),
            "credentials[0].reference",
        ),
        (
            credential(claude(), "anthropic ada", &["api.anthropic.com"]),
            "credentials[0].reference",
        ),
        (credential(claude(), "anthropic/ada", &[]), "toward no host"),
        (
            credential(
                claude(),
                "anthropic/ada",
                &["api.anthropic.com", "api.anthropic.com"],
            ),
            "credentials[0].toward names \"api.anthropic.com\" twice",
        ),
    ];
    for (grant, says) in cases {
        let why = refused(&[grant], &grants);
        assert!(why.contains(says), "{says}: {why}");
    }
}

/// AC-T7.7.2, Shards' D242 (C2): a credential grant grants no egress. Do grant a credential toward a host its member
/// reaches by no egress grant, by UDP alone, by address alone, by another member's grant alone, or written in another
/// case than its grant's. Expect each refused, naming the host its member's egress doesn't reach by name, by TCP.
#[test]
fn w33_a_credential_toward_a_host_its_members_egress_does_not_reach_by_name_is_refused() {
    let agent = member(MemberKind::Agent, "helper");
    let unreached: [(Vec<Grant>, &str); 5] = [
        (Vec::new(), "api.anthropic.com"),
        (
            vec![egress(claude(), "api.anthropic.com", Proto::Udp)],
            "api.anthropic.com",
        ),
        (
            vec![Grant::Egress {
                member: claude(),
                to: Destination::Address([203, 0, 113, 7].into()),
                ports: Ports {
                    proto: Proto::Tcp,
                    first: 443,
                    last: 443,
                },
            }],
            "203.0.113.7",
        ),
        (
            vec![egress(agent, "api.anthropic.com", Proto::Tcp)],
            "api.anthropic.com",
        ),
        (
            vec![egress(claude(), "api.anthropic.com", Proto::Tcp)],
            "API.anthropic.com",
        ),
    ];
    for (grants, host) in unreached {
        let why = refused(&[credential(claude(), "anthropic/ada", &[host])], &grants);
        assert!(
            why.contains(&format!(
                "{host:?} is no host an egress grant of harness claude reaches"
            )),
            "{host} over {grants:?}: {why}"
        );
    }
}

/// AC-T7.7.2, Shards' D242 (C2): do grant one member a credential twice, and two credentials toward one host. Expect
/// each refused: a member's credential is one grant's, and a host one credential's alone, as Shards' injector fills in
/// the first credential a host's grants name.
#[test]
fn w33_a_credential_granted_twice_or_a_second_toward_one_host_is_refused() {
    let (grants, _) = harness();
    let twice = [
        credential(claude(), "anthropic/ada", &["api.anthropic.com"]),
        credential(claude(), "anthropic/ada", &["platform.claude.com"]),
    ];
    let why = refused(&twice, &grants);
    assert!(
        why.contains("credentials[0] and credentials[1] both grant harness claude anthropic/ada"),
        "{why}"
    );
    let two = [
        credential(claude(), "anthropic/ada", &["api.anthropic.com"]),
        credential(claude(), "anthropic/team", &["api.anthropic.com"]),
    ];
    let why = refused(&two, &grants);
    assert!(
        why.contains("fill anthropic/ada and anthropic/team in toward api.anthropic.com"),
        "{why}"
    );
}

/// AC-T7.7.2, Shards' D242's "Who" (C2): Shards' injector ends the TLS of every flow to a credential's host. Do let a
/// second member reach a credential's host by its own egress grant, ungranted the credential, then granted it too.
/// Expect the first refused, naming the member and the host, and the second taken; a member reaching that host by UDP
/// alone (no TLS) is no such member.
#[test]
fn w33_every_member_reaching_a_credentials_host_holds_it() {
    let agent = member(MemberKind::Agent, "helper");
    let (mut grants, mut credentials) = harness();
    grants.push(egress(agent.clone(), "api.anthropic.com", Proto::Tcp));
    let why = refused(&credentials, &grants);
    assert!(
        why.contains("grants[2] lets agent helper reach api.anthropic.com, toward which credential anthropic/ada"),
        "{why}"
    );
    credentials.push(credential(
        agent.clone(),
        "anthropic/ada",
        &["api.anthropic.com"],
    ));
    assert_eq!(check_credentials(&credentials, &grants), Ok(()));
    let (mut grants, credentials) = harness();
    grants.push(egress(agent, "api.anthropic.com", Proto::Udp));
    assert_eq!(check_credentials(&credentials, &grants), Ok(()));
}

/// Joint plan step 33 (every credential grant in Hex's record of the VM): do write a credential grant as the record's
/// JSON and read it back. Expect it whole, each field by its name.
#[test]
fn w33_a_credential_grant_round_trips_through_the_records_json() {
    let (_, credentials) = harness();
    let json = serde_json::to_string(&credentials).unwrap();
    assert_eq!(
        json,
        r#"[{"member":{"kind":"harness","name":"claude"},"reference":"anthropic/ada","toward":["api.anthropic.com","platform.claude.com"]}]"#
    );
    let back: Vec<CredentialGrant> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, credentials);
}
