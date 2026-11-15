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

/// Placeholders as kiln's broker mints them (Shards' D242): a visible marker, then 256 random bits in hex.
const ADA: &str =
    "kiln-placeholder-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const GITHUB: &str =
    "kiln-placeholder-fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const HELPERS: &str =
    "kiln-placeholder-00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

fn credential(member: &Member, reference: &str, toward: &[&str]) -> hex_api::CredentialGrant {
    hex_api::CredentialGrant {
        member: member.clone(),
        reference: reference.to_owned(),
        toward: toward.iter().map(|h| (*h).to_owned()).collect(),
        placeholder: match reference {
            "github/ada" => GITHUB,
            _ => ADA,
        }
        .to_owned(),
    }
}

/// AC-T7.7.2, joint plan step 33 (C4; Shards' D242): do lower a harness's and an agent's grants of one credential with
/// one placeholder toward hosts in another order, and a second credential. Expect one grant of each credential toward
/// every host its members' grants name, each once and sorted, with its placeholder; every host lowered one a grant of
/// the box names for that credential (Hex adds none); and a box granting none lowered to none. Do give the agent its
/// own placeholder for the credential. Expect a grant of it for each placeholder, toward its own members' hosts alone,
/// as Shards' injector fills a value in only where its grant's placeholder stands.
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
                placeholder: ADA.to_owned(),
            },
            CredentialGrant {
                reference: "github/ada".to_owned(),
                toward: vec!["api.github.com".to_owned()],
                placeholder: GITHUB.to_owned(),
            },
        ]
    );
    for grant in &made {
        for host in &grant.toward {
            assert!(
                image
                    .credentials
                    .iter()
                    .any(|c| c.reference == grant.reference
                        && c.placeholder == grant.placeholder
                        && c.toward.contains(host)),
                "{} toward {host}: a grant the box doesn't name",
                grant.reference
            );
        }
    }
    // Sorted by credential, then by placeholder: the helper's before the harness's.
    image.credentials[2].placeholder = HELPERS.to_owned();
    assert_eq!(
        lowered(&image).unwrap(),
        vec![
            CredentialGrant {
                reference: "anthropic/ada".to_owned(),
                toward: vec!["api.anthropic.com".to_owned()],
                placeholder: HELPERS.to_owned(),
            },
            CredentialGrant {
                reference: "anthropic/ada".to_owned(),
                toward: vec![
                    "api.anthropic.com".to_owned(),
                    "platform.claude.com".to_owned()
                ],
                placeholder: ADA.to_owned(),
            },
            CredentialGrant {
                reference: "github/ada".to_owned(),
                toward: vec!["api.github.com".to_owned()],
                placeholder: GITHUB.to_owned(),
            },
        ]
    );
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

/// A run's lessor that serves its sign-ins as the test sets it, and keeps what it was asked and told.
/// A capture as the lessor heard it: its sign-in, its tokens and its key.
type Capture = (String, hex_api::lease::SignInTokens, Option<[u8; 32]>);

#[derive(Default)]
struct SignedIn {
    captured: Mutex<Vec<Capture>>,
    refreshed: Mutex<Vec<(String, Option<[u8; 32]>)>>,
    revoked: Mutex<Vec<(String, Option<[u8; 32]>)>>,
}

impl Lessor for SignedIn {
    fn lease(&self, _: &str, _: Option<[u8; 32]>) -> Result<Lease, LeaseRefused> {
        Err(LeaseRefused::Denied(
            "this lessor leases nothing".to_owned(),
        ))
    }
    fn release(&self, _: &str) {}
    fn capture(
        &self,
        sign_in: &str,
        tokens: hex_api::lease::SignInTokens,
        key: Option<[u8; 32]>,
    ) -> Result<hex_api::Captured, hex_api::lease::SignInRefused> {
        self.captured
            .lock()
            .unwrap()
            .push((sign_in.to_owned(), tokens, key));
        if sign_in == "refused" {
            return Err(hex_api::lease::SignInRefused::Denied(
                "not this one".to_owned(),
            ));
        }
        Ok(hex_api::Captured {
            access: "kiln/sign-ins/claude/access".to_owned(),
            access_placeholder: "kiln-placeholder-61636365737321".to_owned(),
            refresh_placeholder: Some("kiln-placeholder-7265667265736821".to_owned()),
        })
    }
    fn refresh(
        &self,
        sign_in: &str,
        key: Option<[u8; 32]>,
    ) -> Result<Value, hex_api::lease::SignInRefused> {
        self.refreshed
            .lock()
            .unwrap()
            .push((sign_in.to_owned(), key));
        Ok(Value::new(
            b"{\"access_token\":\"kiln-placeholder-61636365737321\"}".to_vec(),
        ))
    }
    fn revoked(&self, sign_in: &str, key: Option<[u8; 32]>) {
        self.revoked.lock().unwrap().push((sign_in.to_owned(), key));
    }
}

/// Shards' D242, step 8 (docs/design/credential-grants.md C8), red first: do capture, refresh and revoke a sign-in
/// through Shards' interface under a key, as the run host asks. Expect each passed on to the run's lessor with its
/// tokens whole and its key, its placeholders answered back, the refreshed response handed on whole, a refusal of
/// Shards' kind and words, and the revocation told.
#[test]
fn s33_a_runs_sign_ins_answer_shards_run_host_from_its_own_lessor() {
    use shards_api::image::{Secret, SignInRefusal, SignInTokens};
    let signed = Arc::new(SignedIn::default());
    let leases = RunLeases(signed.clone());
    let tokens = || SignInTokens {
        access: Secret(b"sk-ant-oat01-access".to_vec()),
        refresh: Some(Secret(b"sk-ant-ort01-refresh".to_vec())),
        expires_in: Some(28_800),
        id_token: None,
        scope: Some("user:inference".to_owned()),
    };
    let captured = leases
        .capture("claude/ada", tokens(), Some([9; 32]))
        .unwrap();
    assert_eq!(captured.access, "kiln/sign-ins/claude/access");
    assert_eq!(
        captured.access_placeholder,
        "kiln-placeholder-61636365737321"
    );
    assert_eq!(
        captured.refresh_placeholder.as_deref(),
        Some("kiln-placeholder-7265667265736821")
    );
    {
        let asked = signed.captured.lock().unwrap();
        let [(sign_in, given, key)] = &asked[..] else {
            panic!("{} captures asked", asked.len());
        };
        assert_eq!((sign_in.as_str(), *key), ("claude/ada", Some([9; 32])));
        assert_eq!(given.access.expose(), b"sk-ant-oat01-access");
        assert_eq!(
            given.refresh.as_ref().map(Value::expose),
            Some(&b"sk-ant-ort01-refresh"[..])
        );
        assert_eq!(
            (given.expires_in, given.scope.as_deref()),
            (Some(28_800), Some("user:inference"))
        );
    }
    assert_eq!(
        leases.capture("refused", tokens(), None).unwrap_err(),
        SignInRefusal::Denied("not this one".to_owned())
    );
    let refreshed = leases.refresh("claude/ada", None).unwrap();
    assert_eq!(
        refreshed.0,
        b"{\"access_token\":\"kiln-placeholder-61636365737321\"}"
    );
    leases.revoked("claude/ada", Some([9; 32]));
    assert_eq!(
        signed.refreshed.lock().unwrap().clone(),
        vec![("claude/ada".to_owned(), None)]
    );
    assert_eq!(
        signed.revoked.lock().unwrap().clone(),
        vec![("claude/ada".to_owned(), Some([9; 32]))]
    );
}
