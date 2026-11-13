//! Identity by CSR enrollment (docs/design/hex-quic.md Q6): a peer's key is made where the peer runs, and kiln's CA,
//! reached through the [`Ca`] seam (CLAUDE.md §9), issues its certificate from its request (PKCS #10, RFC 2986),
//! signed with that key, so the CA learns the key's public half alone; the private key never leaves the peer. The
//! CA names the certificate for the name it admitted, whatever the request asked (kiln `pki.rs` `sign_csr`: "The
//! certificate's name is forced"), and that name is the peer's identity on every link ([`crate::driver::peer_id`]).
//!
//! The seam has a simulator ([`sim::SimCa`]) and a conformance suite ([`conformance`]) that every implementation
//! passes: kiln's, over its own CA, is kiln's to make (the plan's step 17's launch seam takes it).

use hyper_transport::tls::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::Refused;
use crate::tls::Credentials;

/// What a peer sends kiln's CA: the name it enrolls as, its signing request, and the grant kiln gave it to
/// enroll with (kiln's enrollment token), which the CA checks. No key.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    pub name: String,
    /// The request, DER.
    pub csr: Vec<u8>,
    pub grant: Vec<u8>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("name", &self.name)
            .field("csr", &self.csr.len())
            .field("grant", &"<redacted>")
            .finish()
    }
}

/// What the CA answers: the peer's chain, its own certificate first, and the roots every peer's certificate
/// chains to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issued {
    pub chain: Vec<CertificateDer<'static>>,
    pub roots: Vec<CertificateDer<'static>>,
}

/// Why the CA refused, typed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CaRefused {
    /// The name may not enroll, or not with that grant.
    #[error("{name} may not enroll: {why}")]
    NotAdmitted { name: String, why: String },
    /// The request isn't a signing request the CA takes (its signature doesn't verify, or it can't be read).
    #[error("the signing request can't be taken: {why}")]
    Malformed { why: String },
    /// The CA couldn't be reached or answered nothing usable.
    #[error("kiln's CA can't be reached: {why}")]
    Unreachable { why: String },
}

/// kiln's CA, as Hex reaches it.
pub trait Ca: Send + Sync {
    /// Issues a certificate for `request`'s admitted name, carrying the key its CSR was signed with.
    fn enroll(&self, request: &Request) -> Result<Issued, CaRefused>;
}

/// A peer's own private key: made here, kept here, never in a request.
pub struct Key(rcgen::KeyPair);

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(<redacted>)")
    }
}

fn rcgen_refused(what: &'static str) -> impl Fn(rcgen::Error) -> Refused {
    move |e| Refused::Configuration {
        what,
        why: e.to_string(),
    }
}

/// A new key for `name` and the request kiln's CA issues its certificate from: ECDSA on P-384 with SHA-384
/// (CNSA 2.0's curve, as kiln's reach identities and the hybrid groups' P-384 half), the name its only SAN.
pub fn request(name: &str, grant: Vec<u8>) -> Result<(Key, Request), Refused> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384)
        .map_err(rcgen_refused("a new key"))?;
    let mut params = rcgen::CertificateParams::new(vec![name.to_owned()])
        .map_err(rcgen_refused("the request's name"))?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, name);
    let csr = params
        .serialize_request(&key)
        .map_err(rcgen_refused("the signing request"))?;
    Ok((
        Key(key),
        Request {
            name: name.to_owned(),
            csr: csr.der().to_vec(),
            grant,
        },
    ))
}

/// The credentials `issued` makes with `key`, once the certificate proves to be for `request`: its own the key's
/// public half, named the name asked, and under the roots it came with.
pub fn credentials(key: Key, request: &Request, issued: Issued) -> Result<Credentials, Refused> {
    let mismatch = |why: &str| Refused::Issued {
        why: why.to_owned(),
    };
    let leaf = issued
        .chain
        .first()
        .ok_or_else(|| mismatch("the CA answered no certificate"))?;
    if issued.roots.is_empty() {
        return Err(mismatch("the CA answered no roots"));
    }
    let cert = webpki::EndEntityCert::try_from(leaf)
        .map_err(|e| mismatch(&format!("its certificate can't be read: {e}")))?;
    let spki = rcgen::PublicKeyData::subject_public_key_info(&key.0);
    if cert.subject_public_key_info().as_ref() != spki.as_slice() {
        return Err(mismatch(
            "its certificate carries another key than this peer's",
        ));
    }
    if crate::driver::certificate_name(leaf).as_deref() != Some(request.name.as_str()) {
        return Err(mismatch("its certificate names another peer"));
    }
    Ok(Credentials {
        chain: issued.chain,
        key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.0.serialize_der())),
        roots: issued.roots,
    })
}

/// Enrolls `name` with `ca` under `grant`: a key made here, its request sent, and the credentials the answer makes.
pub fn enroll(ca: &dyn Ca, name: &str, grant: Vec<u8>) -> Result<Credentials, Refused> {
    let (key, request) = request(name, grant)?;
    let issued = ca.enroll(&request).map_err(Refused::Ca)?;
    credentials(key, &request, issued)
}

pub mod store;

pub mod sim {
    //! A CA in this process, as kiln's CA behaves at the seam: it admits a name with a grant, takes each grant
    //! once, checks the request's signature, and issues for the admitted name alone, under one root.

    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::{Ca, CaRefused, Issued, Request};
    use crate::Refused;

    pub struct SimCa {
        issuer: rcgen::CertifiedIssuer<'static, rcgen::KeyPair>,
        root: hyper_transport::tls::CertificateDer<'static>,
        grants: Mutex<BTreeMap<String, Vec<u8>>>,
    }

    impl SimCa {
        /// A new CA, its key on P-384, its root a self-signed certificate of a CA.
        pub fn new() -> Result<Self, Refused> {
            let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384)
                .map_err(super::rcgen_refused("the CA's key"))?;
            let mut params = rcgen::CertificateParams::new(Vec::<String>::new())
                .map_err(super::rcgen_refused("the CA's root"))?;
            params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            params
                .distinguished_name
                .push(rcgen::DnType::CommonName, "hex simulated kiln CA");
            params.key_usages = vec![
                rcgen::KeyUsagePurpose::KeyCertSign,
                rcgen::KeyUsagePurpose::CrlSign,
                rcgen::KeyUsagePurpose::DigitalSignature,
            ];
            let issuer = rcgen::CertifiedIssuer::self_signed(params, key)
                .map_err(super::rcgen_refused("the CA's root"))?;
            let root = issuer.der().clone();
            Ok(Self {
                issuer,
                root,
                grants: Mutex::new(BTreeMap::new()),
            })
        }

        /// `name` may enroll once, with `grant`.
        pub fn admit(&self, name: &str, grant: &[u8]) {
            if let Ok(mut grants) = self.grants.lock() {
                grants.insert(name.to_owned(), grant.to_vec());
            }
        }

        /// The CA's root.
        pub fn root(&self) -> hyper_transport::tls::CertificateDer<'static> {
            self.root.clone()
        }
    }

    impl Ca for SimCa {
        fn enroll(&self, request: &Request) -> Result<Issued, CaRefused> {
            let not_admitted = |why: &str| CaRefused::NotAdmitted {
                name: request.name.clone(),
                why: why.to_owned(),
            };
            {
                let mut grants = self.grants.lock().map_err(|_| CaRefused::Unreachable {
                    why: "the CA's grants are poisoned".to_owned(),
                })?;
                match grants.get(&request.name) {
                    Some(grant) if *grant == request.grant => {
                        grants.remove(&request.name);
                    }
                    Some(_) => return Err(not_admitted("its grant isn't the one admitted")),
                    None => return Err(not_admitted("it was never admitted, or enrolled already")),
                }
            }
            let csr = rcgen::CertificateSigningRequestParams::from_der(&request.csr.clone().into())
                .map_err(|e| CaRefused::Malformed { why: e.to_string() })?;
            let mut params = rcgen::CertificateParams::new(vec![request.name.clone()])
                .map_err(|e| CaRefused::Malformed { why: e.to_string() })?;
            params
                .distinguished_name
                .push(rcgen::DnType::CommonName, request.name.clone());
            params.is_ca = rcgen::IsCa::NoCa;
            params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
            // A peer both listens and dials: its certificate serves either side of a link.
            params.extended_key_usages = vec![
                rcgen::ExtendedKeyUsagePurpose::ServerAuth,
                rcgen::ExtendedKeyUsagePurpose::ClientAuth,
            ];
            let cert = rcgen::CertificateSigningRequestParams {
                params,
                public_key: csr.public_key,
            }
            .signed_by(&*self.issuer)
            .map_err(|e| CaRefused::Malformed { why: e.to_string() })?;
            Ok(Issued {
                chain: vec![cert.der().clone()],
                roots: vec![self.root.clone()],
            })
        }
    }
}

pub mod conformance {
    //! The [`Ca`] seam's contract as cases every implementation passes: the simulator here, kiln's CA in kiln.
    //! Each reads "do X, expect Y"; [`run`] returns the cases that failed and why.

    use super::{Ca, CaRefused, Request, request};

    /// How a fixture admits a name: the grant the CA takes for it once.
    pub trait Fixture {
        fn admit(&self, name: &str) -> Vec<u8>;
        /// The CA's roots, as its peers hold them.
        fn roots(&self) -> Vec<hyper_transport::tls::CertificateDer<'static>>;
    }

    /// A case that failed.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Failure {
        pub case: &'static str,
        pub why: String,
    }

    type Case = (
        &'static str,
        fn(&dyn Ca, &dyn Fixture) -> Result<(), String>,
    );

    pub const CASES: &[Case] = &[
        ("HEX-ENROLL-1", issued_for_the_request),
        ("HEX-ENROLL-2", named_as_admitted),
        ("HEX-ENROLL-3", not_admitted_is_refused),
        ("HEX-ENROLL-4", a_forged_request_is_refused),
        ("HEX-ENROLL-5", a_grant_is_taken_once),
    ];

    /// Runs every case; returns the failures.
    pub fn run(ca: &dyn Ca, fixture: &dyn Fixture) -> Vec<Failure> {
        CASES
            .iter()
            .filter_map(|(case, run)| run(ca, fixture).err().map(|why| Failure { case, why }))
            .collect()
    }

    fn named(case: &str) -> String {
        format!("hex-enroll-{case}.test")
    }

    /// HEX-ENROLL-1: an admitted name's request is issued a certificate that carries the request's key, chains to
    /// the CA's roots, and serves both sides of a link (server and client authentication).
    fn issued_for_the_request(ca: &dyn Ca, fx: &dyn Fixture) -> Result<(), String> {
        let name = named("1");
        let (key, request) = request(&name, fx.admit(&name)).map_err(|e| e.to_string())?;
        let issued = ca.enroll(&request).map_err(|e| e.to_string())?;
        let roots = fx.roots();
        if issued.roots != roots {
            return Err("its roots aren't the CA's".to_owned());
        }
        let leaf = issued.chain.first().ok_or("no certificate")?.clone();
        let credentials = super::credentials(key, &request, issued).map_err(|e| e.to_string())?;
        let anchors: Vec<hyper_quic::rustls::pki_types::TrustAnchor<'_>> = roots
            .iter()
            .map(|root| webpki::anchor_from_trusted_cert(root).map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
        let cert = webpki::EndEntityCert::try_from(&leaf).map_err(|e| e.to_string())?;
        let now = hyper_quic::rustls::pki_types::UnixTime::now();
        let intermediates = credentials.chain.get(1..).unwrap_or_default();
        for usage in [
            webpki::KeyUsage::server_auth(),
            webpki::KeyUsage::client_auth(),
        ] {
            cert.verify_for_usage(
                webpki::ALL_VERIFICATION_ALGS,
                &anchors,
                intermediates,
                now,
                usage,
                None,
                None,
            )
            .map_err(|e| format!("it doesn't verify for both sides of a link: {e}"))?;
        }
        Ok(())
    }

    /// HEX-ENROLL-2: a request that asks another name is issued for the admitted one.
    fn named_as_admitted(ca: &dyn Ca, fx: &dyn Fixture) -> Result<(), String> {
        let name = named("2");
        let grant = fx.admit(&name);
        let (_, asked) = request("someone-else.test", Vec::new()).map_err(|e| e.to_string())?;
        let request = Request {
            name: name.clone(),
            csr: asked.csr,
            grant,
        };
        let issued = ca.enroll(&request).map_err(|e| e.to_string())?;
        let leaf = issued.chain.first().ok_or("no certificate")?;
        match crate::driver::certificate_name(leaf) {
            Some(got) if got == name => Ok(()),
            other => Err(format!("it was named {other:?}, not {name}")),
        }
    }

    /// HEX-ENROLL-3: a name never admitted is refused NotAdmitted.
    fn not_admitted_is_refused(ca: &dyn Ca, _fx: &dyn Fixture) -> Result<(), String> {
        let (_, request) = request(&named("3"), b"no grant".to_vec()).map_err(|e| e.to_string())?;
        match ca.enroll(&request) {
            Err(CaRefused::NotAdmitted { .. }) => Ok(()),
            other => Err(format!("{other:?}")),
        }
    }

    /// HEX-ENROLL-4: a request whose signature doesn't verify (its last byte changed) is refused Malformed.
    fn a_forged_request_is_refused(ca: &dyn Ca, fx: &dyn Fixture) -> Result<(), String> {
        let name = named("4");
        let (_, mut request) = request(&name, fx.admit(&name)).map_err(|e| e.to_string())?;
        if let Some(last) = request.csr.last_mut() {
            *last ^= 0x01;
        }
        match ca.enroll(&request) {
            Err(CaRefused::Malformed { .. }) => Ok(()),
            other => Err(format!("{other:?}")),
        }
    }

    /// HEX-ENROLL-5: a grant enrolls once: the same request again is refused NotAdmitted.
    fn a_grant_is_taken_once(ca: &dyn Ca, fx: &dyn Fixture) -> Result<(), String> {
        let name = named("5");
        let (_, request) = request(&name, fx.admit(&name)).map_err(|e| e.to_string())?;
        ca.enroll(&request).map_err(|e| e.to_string())?;
        match ca.enroll(&request) {
            Err(CaRefused::NotAdmitted { .. }) => Ok(()),
            other => Err(format!("{other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sim::SimCa;
    use super::*;

    struct Sim<'a>(&'a SimCa);

    impl conformance::Fixture for Sim<'_> {
        fn admit(&self, name: &str) -> Vec<u8> {
            let grant = format!("grant for {name}").into_bytes();
            self.0.admit(name, &grant);
            grant
        }
        fn roots(&self) -> Vec<CertificateDer<'static>> {
            vec![self.0.root()]
        }
    }

    /// HEX-ENROLL (Q6): the simulator CA passes the seam's conformance suite, every case.
    #[test]
    fn hex_enroll_the_simulator_passes_the_ca_conformance_suite() {
        let ca = SimCa::new().unwrap();
        let failures = conformance::run(&ca, &Sim(&ca));
        assert!(failures.is_empty(), "{failures:#?}");
        assert_eq!(conformance::CASES.len(), 5);
    }

    /// HEX-ENROLL-6 (Q6): the key never travels: no run of its private scalar is in the request a peer sends, and the
    /// credentials enrollment makes hold the key made with the request.
    #[test]
    fn hex_enroll_6_the_private_key_never_leaves_the_peer() {
        let ca = SimCa::new().unwrap();
        ca.admit("node-a.test", b"g");
        let (key, request) = request("node-a.test", b"g".to_vec()).unwrap();
        let secret = key.0.serialize_der();
        // PKCS #8 (RFC 5958) around RFC 5915's ECPrivateKey: version 1, then the private scalar as an OCTET STRING
        // of 48 bytes for P-384 (`02 01 01 04 30`).
        let at = secret
            .windows(5)
            .position(|w| w == [0x02, 0x01, 0x01, 0x04, 0x30])
            .unwrap();
        let scalar = &secret[at + 5..at + 5 + 48];
        let sent: Vec<u8> = [
            request.csr.as_slice(),
            request.name.as_bytes(),
            &request.grant,
        ]
        .concat();
        for window in scalar.windows(16) {
            assert!(
                !sent.windows(16).any(|w| w == window),
                "a run of the private key is in the request"
            );
        }
        let issued = ca.enroll(&request).unwrap();
        let credentials = credentials(key, &request, issued).unwrap();
        assert_eq!(credentials.key.secret_der(), secret.as_slice());
    }

    /// HEX-ENROLL-7 (Q6): an answer for another key or another name is refused, typed, never taken.
    #[test]
    fn hex_enroll_7_an_answer_for_another_key_or_name_is_refused() {
        let ca = SimCa::new().unwrap();
        ca.admit("node-b.test", b"g");
        let (_, theirs) = request("node-b.test", b"g".to_vec()).unwrap();
        let issued = ca.enroll(&theirs).unwrap();
        let (mine, my_request) = request("node-b.test", b"g".to_vec()).unwrap();
        assert!(matches!(
            credentials(mine, &my_request, issued.clone()),
            Err(Refused::Issued { .. })
        ));
        let (key, _) = request("node-b.test", Vec::new()).unwrap();
        let renamed = Request {
            name: "node-c.test".to_owned(),
            ..theirs.clone()
        };
        assert!(matches!(
            credentials(key, &renamed, issued),
            Err(Refused::Issued { .. })
        ));
    }
}
