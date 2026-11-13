use std::sync::Mutex;

use hex_api::lease::Value;
use hex_api::{Destination, Grant, Member, MemberKind, Ports, Proto};
use shards_api::image::Lessor as _;

use super::*;

fn member(kind: MemberKind, name: &str) -> Member {
    Member {
        kind,
        name: name.to_owned(),
    }
}

fn egress(member: &Member, name: &str) -> Grant {
    Grant::Egress {
        member: member.clone(),
        to: Destination::Name(name.to_owned()),
        ports: Ports {
            proto: Proto::Tcp,
            first: 443,
            last: 443,
        },
    }
}

fn credential(member: &Member, reference: &str, toward: &[&str]) -> hex_api::CredentialGrant {
    hex_api::CredentialGrant {
        member: member.clone(),
        reference: reference.to_owned(),
        toward: toward.iter().map(|h| (*h).to_owned()).collect(),
    }
}

/// AC-T7.7.2, joint plan step 33 (C4): do lower a harness's and an agent's grants of one credential toward hosts in
/// another order, and a second credential. Expect one grant of each credential toward every host its members' grants
/// name, each once and sorted; every host lowered one a grant of the box names for that credential (Hex adds none);
/// and a box granting none lowered to none.
#[test]
fn w33_a_boxs_credentials_lower_to_one_grant_of_each_credential_toward_its_hosts() {
    let (claude, helper) = (
        member(MemberKind::Harness, "claude"),
        member(MemberKind::Agent, "helper"),
    );
    let mut image = ShardsImage::new(hex_api::sim::SIM_IMAGE);
    image.grants = vec![
        egress(&claude, "platform.claude.com"),
        egress(&claude, "api.anthropic.com"),
        egress(&helper, "api.anthropic.com"),
        egress(&helper, "api.github.com"),
    ];
    image.credentials = vec![
        credential(
            &claude,
            "anthropic/ada",
            &["platform.claude.com", "api.anthropic.com"],
        ),
        credential(&helper, "github/ada", &["api.github.com"]),
        credential(&helper, "anthropic/ada", &["api.anthropic.com"]),
    ];
    let made = lowered(&image).unwrap();
    assert_eq!(
        made,
        vec![
            CredentialGrant {
                reference: "anthropic/ada".to_owned(),
                toward: vec![
                    "api.anthropic.com".to_owned(),
                    "platform.claude.com".to_owned()
                ],
            },
            CredentialGrant {
                reference: "github/ada".to_owned(),
                toward: vec!["api.github.com".to_owned()],
            },
        ]
    );
    for grant in &made {
        for host in &grant.toward {
            assert!(
                image
                    .credentials
                    .iter()
                    .any(|c| c.reference == grant.reference && c.toward.contains(host)),
                "{} toward {host}: a grant the box doesn't name",
                grant.reference
            );
        }
    }
    image.credentials.clear();
    assert_eq!(lowered(&image).unwrap(), Vec::new());
}

/// C2: do lower a box whose credential its own egress doesn't reach. Expect it refused as invalid credentials, so no
/// boot asks for it.
#[test]
fn w33_a_credential_the_check_refuses_is_never_lowered() {
    let claude = member(MemberKind::Harness, "claude");
    let mut image = ShardsImage::new(hex_api::sim::SIM_IMAGE);
    image.credentials = vec![credential(&claude, "anthropic/ada", &["api.anthropic.com"])];
    assert!(matches!(
        lowered(&image),
        Err(Refusal::Invalid {
            what: "credentials",
            ..
        })
    ));
}

/// A run's lessor that answers as the test sets it and keeps what it was asked and given back.
#[derive(Default)]
struct Told {
    answer: Mutex<Option<Result<Lease, LeaseRefused>>>,
    asked: Mutex<Vec<(String, Option<[u8; 32]>)>>,
    released: Mutex<Vec<String>>,
}

impl Lessor for Told {
    fn lease(&self, reference: &str, key: Option<[u8; 32]>) -> Result<Lease, LeaseRefused> {
        self.asked.lock().unwrap().push((reference.to_owned(), key));
        self.answer.lock().unwrap().take().unwrap()
    }
    fn release(&self, lease: &str) {
        self.released.lock().unwrap().push(lease.to_owned());
    }
}

/// Shards' D242 (C3): do ask the run's lessor through Shards' interface under a key, as the run host asks. Expect the
/// ask passed on with its key; the value handed on whole; a lease with no end living past what the run channel carries
/// (until the run ends) and one with an end living that long from now; each refusal of Shards' kind and words; and a
/// lease Shards gives back given back to the run's lessor.
#[test]
fn w33_a_runs_leases_answer_shards_run_host_from_its_own_lessor() {
    let told = Arc::new(Told::default());
    let leases = RunLeases(told.clone());
    let lease = |life| Lease {
        lease: "lease-1".to_owned(),
        value: Value::new(b"sk-ant-api03-real".to_vec()),
        life,
    };
    *told.answer.lock().unwrap() = Some(Ok(lease(None)));
    let asked = Instant::now();
    let got = leases.lease("anthropic/ada", Some([7; 32])).unwrap();
    assert_eq!(
        told.asked.lock().unwrap().clone(),
        vec![("anthropic/ada".to_owned(), Some([7; 32]))]
    );
    assert_eq!(got.lease, "lease-1");
    assert_eq!(got.value.0, b"sk-ant-api03-real");
    assert!(got.until.duration_since(asked) >= RUN_LIFE);
    *told.answer.lock().unwrap() = Some(Ok(lease(Some(Duration::from_secs(600)))));
    let asked = Instant::now();
    let short = leases.lease("anthropic/ada", None).unwrap();
    let lives = short.until.duration_since(asked);
    assert!(
        lives >= Duration::from_secs(600) && lives < RUN_LIFE,
        "{lives:?}"
    );
    for (theirs, shards) in [
        (
            LeaseRefused::Denied("no such".to_owned()),
            LeaseRefusal::Denied("no such".to_owned()),
        ),
        (
            LeaseRefused::KeyReplaced {
                asked: "a1".repeat(32),
                current: Some("b2".repeat(32)),
            },
            LeaseRefusal::KeyReplaced {
                asked: "a1".repeat(32),
                current: Some("b2".repeat(32)),
            },
        ),
        (
            LeaseRefused::Unavailable("down".to_owned()),
            LeaseRefusal::Unavailable("down".to_owned()),
        ),
        (LeaseRefused::Ended, LeaseRefusal::Ended),
        (LeaseRefused::Lapsed, LeaseRefusal::Lapsed),
    ] {
        *told.answer.lock().unwrap() = Some(Err(theirs));
        assert_eq!(leases.lease("anthropic/ada", None).unwrap_err(), shards);
    }
    leases.release("lease-1");
    assert_eq!(
        told.released.lock().unwrap().clone(),
        vec!["lease-1".to_owned()]
    );
}
