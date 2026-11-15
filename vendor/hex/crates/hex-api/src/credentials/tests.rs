use std::fmt::Write as _;

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

/// The placeholder kiln's broker would give here for the credential `reference` names (Shards' D242): its visible
/// marker, then hex, one for each reference, so that grants of one credential share it and two credentials never do.
fn placeholder(reference: &str) -> String {
    reference
        .bytes()
        .fold("kiln-placeholder-".to_owned(), |mut placeholder, b| {
            write!(placeholder, "{b:02x}").unwrap();
            placeholder
        })
}

fn credential(member: Member, reference: &str, toward: &[&str]) -> CredentialGrant {
    CredentialGrant {
        member,
        reference: reference.to_owned(),
        toward: toward.iter().map(|h| (*h).to_owned()).collect(),
        placeholder: placeholder(reference),
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

/// AC-T7.7.2, Shards' D242 (C2): do grant one member a credential twice. Expect it refused: a member's credential is
/// one grant's.
#[test]
fn w33_a_credential_granted_twice_is_refused() {
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
}

/// AC-T7.7.2, Shards' D242's step 3 (C2): Shards' injector fills a value in only where a grant's own placeholder stands,
/// so a host is no longer one credential's alone. Do grant one member two credentials toward one host, and two members
/// each their own credential toward one host, each credential by its own placeholder. Expect both taken.
#[test]
fn w33_two_credentials_toward_one_host_are_taken_each_by_its_own_placeholder() {
    let (mut grants, _) = harness();
    let two = [
        credential(claude(), "anthropic/ada", &["api.anthropic.com"]),
        credential(claude(), "anthropic/team", &["api.anthropic.com"]),
    ];
    assert_eq!(check_credentials(&two, &grants), Ok(()));
    let agent = member(MemberKind::Agent, "helper");
    grants.push(egress(agent.clone(), "api.anthropic.com", Proto::Tcp));
    let each = [
        credential(claude(), "anthropic/ada", &["api.anthropic.com"]),
        credential(agent, "anthropic/team", &["api.anthropic.com"]),
    ];
    assert_eq!(check_credentials(&each, &grants), Ok(()));
}

/// AC-T7.7.2, Shards' D242's step 3 (C2; RFC 6750 §2.1): a member sends its placeholder where the credential goes, as
/// a bearer token is sent. Do grant a credential with a placeholder that is no `b64token` in turn: none, one with a
/// space, a colon, an `=` before its end, a byte past ASCII. Expect each refused naming its placeholder's field, never
/// its bytes. Do grant it with tokens: kiln's, and one of every character a `b64token` takes, its `=` at the end.
/// Expect each taken.
#[test]
fn w33_a_placeholder_no_request_could_carry_is_refused_by_name() {
    let (grants, _) = harness();
    let with = |placeholder: &str| {
        let mut grant = credential(claude(), "anthropic/ada", &["api.anthropic.com"]);
        grant.placeholder = placeholder.to_owned();
        [grant]
    };
    for placeholder in [
        "",
        "kiln placeholder",
        "kiln:placeholder",
        "kiln=placeholder",
        "kiln-placeholder-\u{e9}",
    ] {
        let why = refused(&with(placeholder), &grants);
        assert!(
            why.contains("credentials[0].placeholder") && why.contains("RFC 6750"),
            "{placeholder:?}: {why}"
        );
        assert!(
            placeholder.is_empty() || !why.contains(placeholder),
            "the refusal printed its placeholder: {why}"
        );
    }
    for placeholder in [
        "kiln-placeholder-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "Az09-._~+/==",
    ] {
        assert_eq!(
            check_credentials(&with(placeholder), &grants),
            Ok(()),
            "{placeholder:?}"
        );
    }
}

/// AC-T7.7.2, Shards' D242's step 3 (C2): one placeholder could stand for neither of two credentials alone, as the
/// injector could not tell which a request carrying it asks. Do give two credentials one placeholder, one member's and
/// two members'. Expect each refused naming both grants and neither placeholder. Do give two members' grants of one
/// credential one placeholder. Expect it taken: it stands for that credential alone.
#[test]
fn w33_one_placeholder_for_two_credentials_is_refused() {
    let agent = member(MemberKind::Agent, "helper");
    let (mut grants, _) = harness();
    grants.push(egress(agent.clone(), "api.anthropic.com", Proto::Tcp));
    let shared = |grant: CredentialGrant| CredentialGrant {
        placeholder: placeholder("anthropic/ada"),
        ..grant
    };
    for two in [
        [
            credential(claude(), "anthropic/ada", &["api.anthropic.com"]),
            shared(credential(
                claude(),
                "anthropic/team",
                &["platform.claude.com"],
            )),
            credential(agent.clone(), "anthropic/ada", &["api.anthropic.com"]),
        ],
        [
            credential(claude(), "anthropic/ada", &["api.anthropic.com"]),
            credential(agent.clone(), "anthropic/ada", &["api.anthropic.com"]),
            shared(credential(
                agent.clone(),
                "anthropic/team",
                &["api.anthropic.com"],
            )),
        ],
    ] {
        let why = refused(&two, &grants);
        assert!(
            why.contains("credentials[1].placeholder")
                || why.contains("credentials[2].placeholder"),
            "{why}"
        );
        assert!(
            why.contains("anthropic/ada") && why.contains("anthropic/team"),
            "{why}"
        );
        assert!(!why.contains(&placeholder("anthropic/ada")), "{why}");
    }
    let one = [
        credential(claude(), "anthropic/ada", &["api.anthropic.com"]),
        credential(agent, "anthropic/ada", &["api.anthropic.com"]),
    ];
    assert_eq!(check_credentials(&one, &grants), Ok(()));
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
/// JSON and read it back, and read one a record written before placeholders holds. Expect it whole, each field by its
/// name, its placeholder last; the older one read with none.
#[test]
fn w33_a_credential_grant_round_trips_through_the_records_json() {
    let (_, credentials) = harness();
    let json = serde_json::to_string(&credentials).unwrap();
    assert_eq!(
        json,
        r#"[{"member":{"kind":"harness","name":"claude"},"reference":"anthropic/ada","toward":["api.anthropic.com","platform.claude.com"],"placeholder":"kiln-placeholder-616e7468726f7069632f616461"}]"#
    );
    let back: Vec<CredentialGrant> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, credentials);
    let older: CredentialGrant = serde_json::from_str(
        r#"{"member":{"kind":"harness","name":"claude"},"reference":"anthropic/ada","toward":["api.anthropic.com"]}"#,
    )
    .unwrap();
    assert_eq!(older.placeholder, "");
}

/// Shards' D242 (holding a placeholder is the capability): do format a credential grant with `{:?}`. Expect its
/// member, reference and hosts, and of its placeholder its length alone.
#[test]
fn w33_a_credential_grants_debug_shows_its_placeholders_length_alone() {
    let (_, credentials) = harness();
    let shown = format!("{credentials:?}");
    assert!(!shown.contains(&credentials[0].placeholder), "{shown}");
    assert!(shown.contains("placeholder: 43 bytes"), "{shown}");
    assert!(
        shown.contains("reference: \"anthropic/ada\"") && shown.contains("platform.claude.com"),
        "{shown}"
    );
}

/// Claude Code's own sign-in as kiln would grant it (docs/design/credential-grants.md C8): toward its provider, with an
/// earlier capture's placeholders.
fn sign_in(name: &str, cli: SignInCli) -> SignInGrant {
    SignInGrant {
        member: claude(),
        sign_in: name.to_owned(),
        cli,
        toward: vec!["api.anthropic.com".to_owned()],
        captured: Some(Captured {
            access: format!("{name}/access"),
            access_placeholder: placeholder(&format!("{name}/access")),
            refresh_placeholder: Some(placeholder(&format!("{name}/refresh"))),
        }),
    }
}

fn sign_in_refused(sign_ins: &[SignInGrant]) -> String {
    let (grants, credentials) = harness();
    match check_sign_ins(sign_ins, &credentials, &grants) {
        Err(Refusal::Invalid {
            what: "sign_ins",
            why,
        }) => why,
        other => panic!("{sign_ins:?} gave {other:?}"),
    }
}

/// C8: do check a harness's sign-in beside its credential, then each field broken in turn. Expect the sign-in taken, and
/// each broken one refused by its field: a name no store holds, a second sign-in of its name or its CLI, no host, a host
/// named twice or its member's egress doesn't reach, a captured reference no store holds, a placeholder no request
/// carries, and a placeholder that stands for two tokens (its own two, or a credential's).
#[test]
fn s33_a_sign_in_is_checked_by_its_fields_beside_the_runs_credentials() {
    let (grants, credentials) = harness();
    let claude_code = sign_in("claude/ada", SignInCli::ClaudeCode);
    check_sign_ins(std::slice::from_ref(&claude_code), &credentials, &grants).unwrap();
    let broken = |change: &dyn Fn(&mut SignInGrant)| {
        let mut grant = claude_code.clone();
        change(&mut grant);
        sign_in_refused(&[grant])
    };
    assert!(broken(&|g| g.sign_in = "claude ada".to_owned()).contains("sign_ins[0].sign_in"));
    assert!(broken(&|g| g.toward.clear()).contains("toward no host"));
    assert!(broken(&|g| g.toward.push("api.anthropic.com".to_owned())).contains("twice"));
    assert!(
        broken(&|g| g.toward = vec!["evil.example".to_owned()]).contains("reaches by name, by TCP")
    );
    assert!(
        broken(&|g| g.captured.as_mut().unwrap().access = String::new())
            .contains("captured.access")
    );
    assert!(
        broken(&|g| g.captured.as_mut().unwrap().access_placeholder = "not a token".to_owned())
            .contains("captured.access_placeholder")
    );
    assert!(
        broken(&|g| {
            let captured = g.captured.as_mut().unwrap();
            captured.refresh_placeholder = Some(captured.access_placeholder.clone());
        })
        .contains("stands for one token alone")
    );
    assert!(
        broken(&|g| g.captured.as_mut().unwrap().access_placeholder = placeholder("anthropic/ada"))
            .contains("credentials[0].placeholder")
    );
    let codex = SignInGrant {
        cli: SignInCli::Codex,
        ..sign_in("openai/ada", SignInCli::Codex)
    };
    check_sign_ins(&[claude_code.clone(), codex], &credentials, &grants).unwrap();
    assert!(
        sign_in_refused(&[
            claude_code.clone(),
            sign_in("claude/bo", SignInCli::ClaudeCode)
        ])
        .contains("signs ClaudeCode in again")
    );
    assert!(
        sign_in_refused(&[claude_code.clone(), claude_code])
            .contains("names sign-in claude/ada twice")
    );
    let printed = format!("{:?}", sign_in("claude/ada", SignInCli::ClaudeCode));
    assert!(
        !printed.contains(&placeholder("claude/ada/access")),
        "a placeholder printed: {printed}"
    );
}

/// The review's B-11 (= E-7; AC-T7.7.2, C5): the checks of a box's credential and sign-in grants sort and merge, O(n log
/// n) in the hosts named, never a look at every earlier host and every egress grant for each host. Do check a
/// credential, then a sign-in, each toward 1,024 hosts that egress grants of its member's own reach, counting the looks
/// each check makes at an earlier host or an egress grant. Expect both taken, each with at most the looks a merge of the
/// two sorted lists makes: a step past each host or egress grant, and a look at the host before each, 2 x hosts +
/// grants (the sorts are std's `sort_unstable`, O(n log n) by its contract, and not counted).
#[test]
fn b11_the_credential_and_sign_in_checks_merge_their_hosts_against_the_egress_grants() {
    let n = 1024;
    let hosts: Vec<String> = (0..n).map(|i| format!("h{i}.example.com")).collect();
    let grants: Vec<Grant> = hosts
        .iter()
        .map(|host| egress(claude(), host, Proto::Tcp))
        .collect();
    let toward: Vec<&str> = hosts.iter().map(String::as_str).collect();
    let credentials = [credential(claude(), "anthropic/ada", &toward)];
    let signed_in = SignInGrant {
        toward: hosts.clone(),
        ..sign_in("claude/ada", SignInCli::ClaudeCode)
    };
    let counted = |check: &dyn Fn() -> Result<(), Refusal>| {
        LOOKS.with(|looks| looks.set(0));
        check().unwrap();
        LOOKS.with(std::cell::Cell::get)
    };
    let credential_looks = counted(&|| check_credentials(&credentials, &grants));
    let sign_in_looks =
        counted(&|| check_sign_ins(std::slice::from_ref(&signed_in), &credentials, &grants));
    let bound = u64::try_from(2 * n + grants.len()).unwrap();
    assert!(
        credential_looks <= bound && sign_in_looks <= bound,
        "looks: the credential's {credential_looks}, the sign-in's {sign_in_looks}; a merge's at most {bound}"
    );
}

/// The review's B-11 (AC-T7.7.2, C2): the merge tells a grant's first unfit host in its list's order, as a look at each
/// host in turn would. Do check credentials toward [a, b, a] with b unreached, toward [a, a] with a unreached, and
/// toward [a, b, b, c] with c unreached, then a sign-in toward [a, b, a] with b unreached. Expect b at 1 told unreached,
/// a at 0 told unreached, b told named twice, then the sign-in's b at 1 told unreached.
#[test]
fn b11_a_grants_first_unfit_host_is_told_in_its_lists_order() {
    let grants = [
        egress(claude(), "a.example", Proto::Tcp),
        egress(claude(), "c.example", Proto::Udp),
    ];
    let reaches = [
        egress(claude(), "a.example", Proto::Tcp),
        egress(claude(), "b.example", Proto::Tcp),
    ];
    for (toward, over, says) in [
        (
            &["a.example", "b.example", "a.example"][..],
            &grants[..],
            "credentials[0].toward[1] \"b.example\" is no host",
        ),
        (
            &["a.example", "a.example"][..],
            &[][..],
            "credentials[0].toward[0] \"a.example\" is no host",
        ),
        (
            &["a.example", "b.example", "b.example", "c.example"][..],
            &reaches[..],
            "credentials[0].toward names \"b.example\" twice",
        ),
    ] {
        let why = refused(&[credential(claude(), "anthropic/ada", toward)], over);
        assert!(why.contains(says), "{toward:?}: {why}");
    }
    let signed_in = SignInGrant {
        toward: ["a.example", "b.example", "a.example"]
            .map(str::to_owned)
            .to_vec(),
        ..sign_in("claude/ada", SignInCli::ClaudeCode)
    };
    match check_sign_ins(&[signed_in], &[], &grants) {
        Err(Refusal::Invalid { why, .. }) => assert!(
            why.contains("sign_ins[0].toward[1] \"b.example\" is no host"),
            "{why}"
        ),
        other => panic!("{other:?}"),
    }
}
