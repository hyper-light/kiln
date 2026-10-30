//! A commit's signature in git's x509 form (docs/ci.md W27-R03), checked purely: what a signer
//! signs for a commit, the CMS signed data git keeps in the commit's `gpgsig` header (gpgsm's, so
//! `git verify-commit` with `gpg.format = x509` reads it, given kiln's root), and whether kiln
//! takes it.
//!
//! - **What's signed** ([`payload`]) is the commit object without its `gpgsig` header, as git has it.
//! - **The signature** ([`armor`]) is CMS signed data (RFC 5652), detached: the signer's certificate
//!   and one signer info, whose signed attributes say the content type (data) and the payload's
//!   SHA-256. Its signature is over those attributes' DER ([`to_sign`]), so a key that signs the bytes
//!   it's given (a runner's held key, the server's) signs a commit without seeing it. It's armored as
//!   gpgsm armors it: `-----BEGIN SIGNED MESSAGE-----`.
//! - **Checking** ([`verify`]) takes the signer's chain from the signature and checks it with
//!   [`chain::verify_signed`] at the time kiln vouches for (when it took the commit), never the
//!   commit's own dates, which its maker writes.
//!
//! ECDSA P-256 with SHA-256, the keys kiln makes; no clock, disk or network.

use base64::Engine as _;
use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::{CmsVersion, ContentInfo};
use cms::signed_data::{CertificateSet, EncapsulatedContentInfo, SignedAttributes, SignedData, SignerIdentifier, SignerInfo, SignerInfos};
use const_oid::db::rfc5911::{ID_CONTENT_TYPE, ID_DATA, ID_MESSAGE_DIGEST, ID_SIGNED_DATA};
use const_oid::db::rfc5912::{ECDSA_WITH_SHA_256, ID_SHA_256};
use der::asn1::{Any, ObjectIdentifier, OctetString, SetOfVec};
use der::{Decode as _, Encode as _};
use sha2::{Digest as _, Sha256};
use x509_cert::Certificate;
use x509_cert::attr::Attribute;
use x509_cert::spki::AlgorithmIdentifierOwned;

use super::chain::{self, Revocation, Verified};

/// The armor's label, as gpgsm writes a detached signature.
pub const ARMOR: &str = "SIGNED MESSAGE";

/// Why a commit's signature isn't taken.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("the commit isn't signed")]
    Unsigned,
    #[error("its signature isn't one kiln reads: {0}")]
    Malformed(String),
    #[error("its signature is over another commit")]
    NotThisCommit,
    #[error(transparent)]
    Signer(#[from] chain::Refusal),
}

fn bad(e: impl std::fmt::Display) -> Refusal {
    Refusal::Malformed(e.to_string())
}

/// Where a commit's headers end: the index of the empty line between them and its message.
fn headers_end(commit: &[u8]) -> usize {
    commit.windows(2).position(|w| w == b"\n\n").map_or(commit.len(), |i| i + 1)
}

fn is_signature(line: &[u8]) -> bool {
    line.starts_with(b"gpgsig ") || line.starts_with(b"gpgsig-sha256 ")
}

/// A commit object without its `gpgsig` header (and that header's continuation lines): what its
/// signature covers.
pub fn payload(commit: &[u8]) -> Vec<u8> {
    let end = headers_end(commit);
    let (headers, message) = commit.split_at(end);
    let mut out = Vec::with_capacity(commit.len());
    let mut skipping = false;
    for line in headers.split_inclusive(|b| *b == b'\n') {
        if skipping && line.first() == Some(&b' ') {
            continue;
        }
        skipping = is_signature(line);
        if !skipping {
            out.extend_from_slice(line);
        }
    }
    out.extend_from_slice(message);
    out
}

/// The signature a commit carries in its `gpgsig` header, as armored text, if it has one.
pub fn signature_of(commit: &[u8]) -> Option<String> {
    let (headers, _) = commit.split_at(headers_end(commit));
    let mut sig: Option<String> = None;
    for line in headers.split_inclusive(|b| *b == b'\n') {
        let line = String::from_utf8_lossy(line);
        match (&mut sig, line.strip_prefix("gpgsig ")) {
            (Some(s), _) => match line.strip_prefix(' ') {
                Some(more) => s.push_str(more),
                None => break,
            },
            (None, Some(first)) => sig = Some(first.to_owned()),
            (None, None) => {}
        }
    }
    sig
}

/// `commit` signed: its `gpgsig` header set to `armored`, after its other headers, where git puts it.
pub fn with_signature(commit: &[u8], armored: &str) -> Vec<u8> {
    let unsigned = payload(commit);
    let (headers, message) = unsigned.split_at(headers_end(&unsigned));
    let mut out = headers.to_vec();
    out.extend_from_slice(b"gpgsig ");
    for (i, line) in armored.trim_end_matches('\n').split('\n').enumerate() {
        if i > 0 {
            out.push(b' ');
        }
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    out.extend_from_slice(message);
    out
}

/// The signed attributes for `payload`: its content type (data) and its SHA-256.
fn attributes(payload: &[u8]) -> Result<SignedAttributes, Refusal> {
    let content_type = Attribute { oid: ID_CONTENT_TYPE, values: SetOfVec::try_from(vec![Any::encode_from(&ID_DATA).map_err(bad)?]).map_err(bad)? };
    let digest = OctetString::new(Sha256::digest(payload).to_vec()).map_err(bad)?;
    let message_digest = Attribute { oid: ID_MESSAGE_DIGEST, values: SetOfVec::try_from(vec![Any::encode_from(&digest).map_err(bad)?]).map_err(bad)? };
    SetOfVec::try_from(vec![content_type, message_digest]).map_err(bad)
}

/// What a signer signs for a commit's `payload`: the DER of the signature's signed attributes.
/// ECDSA P-256 over SHA-256, the signature ASN.1 (as kiln's held keys sign).
pub fn to_sign(payload: &[u8]) -> Result<Vec<u8>, Refusal> {
    attributes(payload)?.to_der().map_err(bad)
}

/// The signature git keeps for `payload`, armored: `signature` (over [`to_sign`]'s bytes) by the
/// key `chain`'s first certificate certifies, the rest of `chain` its issuers (DER, each).
pub fn armor(payload: &[u8], signature: &[u8], chain: &[&[u8]]) -> Result<String, Refusal> {
    let certs = chain.iter().map(|d| Certificate::from_der(d)).collect::<Result<Vec<_>, _>>().map_err(bad)?;
    let leaf = certs.first().ok_or_else(|| bad("no certificate to sign under"))?;
    let sid = SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber { issuer: leaf.tbs_certificate.issuer.clone(), serial_number: leaf.tbs_certificate.serial_number.clone() });
    let sha256 = AlgorithmIdentifierOwned { oid: ID_SHA_256, parameters: None };
    let info = SignerInfo {
        version: CmsVersion::V1,
        sid,
        digest_alg: sha256.clone(),
        signed_attrs: Some(attributes(payload)?),
        signature_algorithm: AlgorithmIdentifierOwned { oid: ECDSA_WITH_SHA_256, parameters: None },
        signature: OctetString::new(signature.to_vec()).map_err(bad)?,
        unsigned_attrs: None,
    };
    let certificates = CertificateSet(SetOfVec::try_from(certs.into_iter().map(CertificateChoices::Certificate).collect::<Vec<_>>()).map_err(bad)?);
    let signed = SignedData {
        version: CmsVersion::V1,
        digest_algorithms: SetOfVec::try_from(vec![sha256]).map_err(bad)?,
        encap_content_info: EncapsulatedContentInfo { econtent_type: ID_DATA, econtent: None },
        certificates: Some(certificates),
        crls: None,
        signer_infos: SignerInfos(SetOfVec::try_from(vec![info]).map_err(bad)?),
    };
    let der = ContentInfo { content_type: ID_SIGNED_DATA, content: Any::encode_from(&signed).map_err(bad)? }.to_der().map_err(bad)?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {ARMOR}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(&String::from_utf8_lossy(chunk));
        out.push('\n');
    }
    out.push_str(&format!("-----END {ARMOR}-----\n"));
    Ok(out)
}

/// The DER an armored signature holds.
fn unarmor(armored: &str) -> Result<Vec<u8>, Refusal> {
    let body: String = armored
        .lines()
        .map(str::trim)
        .skip_while(|l| *l != format!("-----BEGIN {ARMOR}-----"))
        .skip(1)
        .take_while(|l| *l != format!("-----END {ARMOR}-----"))
        .collect();
    if body.is_empty() {
        return Err(bad(format!("it isn't armored as a {ARMOR}")));
    }
    base64::engine::general_purpose::STANDARD.decode(body).map_err(bad)
}

/// Checks `armored`, a commit's signature, over `payload` (the commit without its signature,
/// [`payload`]), as of `at` (unix seconds: when kiln took the commit) against kiln's root `root`
/// and `revocations`: who signed it, or why it isn't taken.
pub fn verify(payload: &[u8], armored: &str, root: &[u8], revocations: &[Revocation], at: i64) -> Result<Verified, Refusal> {
    let info = ContentInfo::from_der(&unarmor(armored)?).map_err(bad)?;
    if info.content_type != ID_SIGNED_DATA {
        return Err(bad("it isn't CMS signed data"));
    }
    let signed: SignedData = info.content.decode_as().map_err(bad)?;
    let [si] = signed.signer_infos.0.as_slice() else { return Err(bad(format!("it has {} signers; a commit's signature has one", signed.signer_infos.0.len()))) };
    if si.digest_alg.oid != ID_SHA_256 || si.signature_algorithm.oid != ECDSA_WITH_SHA_256 {
        return Err(bad("it isn't ECDSA over SHA-256"));
    }
    let SignerIdentifier::IssuerAndSerialNumber(sid) = &si.sid else { return Err(bad("it names its signer by key, not by issuer and serial")) };
    let certs: Vec<&Certificate> = signed.certificates.iter().flat_map(|s| s.0.iter()).filter_map(|c| if let CertificateChoices::Certificate(c) = c { Some(c) } else { None }).collect();
    let is_leaf = |c: &Certificate| c.tbs_certificate.issuer == sid.issuer && c.tbs_certificate.serial_number == sid.serial_number;
    let leaf = certs.iter().find(|c| is_leaf(c)).ok_or_else(|| bad("it names a signer whose certificate it doesn't carry"))?;
    let mut ders = vec![leaf.to_der().map_err(bad)?];
    for c in certs.iter().filter(|c| !is_leaf(c)) {
        ders.push(c.to_der().map_err(bad)?);
    }
    let attrs = si.signed_attrs.as_ref().ok_or_else(|| bad("it has no signed attributes"))?;
    let value = |oid: ObjectIdentifier| attrs.iter().find(|a| a.oid == oid).and_then(|a| a.values.iter().next());
    if value(ID_CONTENT_TYPE).and_then(|v| v.decode_as::<ObjectIdentifier>().ok()) != Some(ID_DATA) {
        return Err(bad("its content type isn't data"));
    }
    let digest = value(ID_MESSAGE_DIGEST).and_then(|v| v.decode_as::<OctetString>().ok()).ok_or_else(|| bad("it has no message digest"))?;
    if digest.as_bytes() != Sha256::digest(payload).as_slice() {
        return Err(Refusal::NotThisCommit);
    }
    let chain: Vec<&[u8]> = ders.iter().map(Vec::as_slice).collect();
    Ok(chain::verify_signed(&chain, root, revocations, at, &attrs.to_der().map_err(bad)?, si.signature.as_bytes())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::chain::{Actor, Identity};
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType, SigningKey as _, date_time_ymd};

    /// 2026-09-21T14:13:20Z: inside every test leaf's validity.
    const AT: i64 = 1_790_000_000;

    const COMMIT: &[u8] = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nparent 4c1e2a90d1f3b5a7c9e1f3a5b7c9d1e3f5a7b9c1\nauthor sam <sam@kiln> 1790000000 +0000\ncommitter kiln <kiln@localhost> 1790000000 +0000\n\nfresh: replayed onto main@5d0e7b\n\nKiln-Change: kc-7f3a9e\n";

    fn ca(name: &str) -> anyhow::Result<(Vec<u8>, Issuer<'static, KeyPair>)> {
        let key = KeyPair::generate()?;
        let mut p = CertificateParams::new(Vec::<String>::new())?;
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.distinguished_name.push(rcgen::DnType::CommonName, name);
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
        p.not_before = date_time_ymd(2025, 1, 1);
        p.not_after = date_time_ymd(2036, 1, 1);
        let der = p.self_signed(&key)?.der().to_vec();
        Ok((der, Issuer::new(p, key)))
    }

    fn signer(issuer: &Issuer<'static, KeyPair>, uri: &str) -> anyhow::Result<(Vec<u8>, KeyPair)> {
        let mut p = CertificateParams::new(Vec::<String>::new())?;
        p.subject_alt_names = vec![SanType::URI(uri.try_into()?)];
        p.is_ca = IsCa::ExplicitNoCa;
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.not_before = date_time_ymd(2026, 1, 1);
        p.not_after = date_time_ymd(2027, 1, 1);
        let key = KeyPair::generate()?;
        Ok((p.signed_by(&key, issuer)?.der().to_vec(), key))
    }

    /// `commit` signed as `cert`'s holder: the signed commit.
    fn signed(commit: &[u8], cert: &[u8], key: &KeyPair) -> anyhow::Result<Vec<u8>> {
        let p = payload(commit);
        Ok(with_signature(commit, &armor(&p, &key.sign(&to_sign(&p)?)?, &[cert])?))
    }

    #[test]
    fn w27_r03_a_commit_verifies_as_its_maker() -> anyhow::Result<()> {
        let (root, issuer) = ca("kiln CA")?;
        for (uri, who) in [
            ("kiln://hyper-light/server", "kiln server of hyper-light"),
            ("kiln://hyper-light/server/cd", "CD, for the kiln server of hyper-light"),
            ("kiln://hyper-light/person/sam", "sam of hyper-light"),
            ("kiln://hyper-light/person/sam/agent/matrix/epoch/3", "agent matrix, epoch 3, for sam, of hyper-light"),
        ] {
            let (cert, key) = signer(&issuer, uri)?;
            let c = signed(COMMIT, &cert, &key)?;
            let sig = signature_of(&c).ok_or_else(|| anyhow::anyhow!("{uri}: no gpgsig"))?;
            assert!(sig.starts_with("-----BEGIN SIGNED MESSAGE-----\n") && sig.ends_with("-----END SIGNED MESSAGE-----\n"), "{sig}");
            assert_eq!(payload(&c), COMMIT, "{uri}: the signature is all signing added");
            let v = verify(&payload(&c), &sig, &root, &[], AT)?;
            assert_eq!(v.to_string(), who);
        }
        // An agent's commit verifies as the agent, never as its person.
        let (cert, key) = signer(&issuer, "kiln://hyper-light/person/sam/agent/matrix/epoch/3")?;
        let c = signed(COMMIT, &cert, &key)?;
        let v = verify(&payload(&c), &signature_of(&c).unwrap_or_default(), &root, &[], AT)?;
        assert!(!v.identity.is_person("sam"));
        assert_eq!(v.acts_for(), vec![Identity::new("hyper-light", Actor::Person { name: "sam".into() })]);
        Ok(())
    }

    #[test]
    fn w27_r03_the_gpgsig_header_is_where_git_keeps_it() -> anyhow::Result<()> {
        let (_, issuer) = ca("kiln CA")?;
        let (cert, key) = signer(&issuer, "kiln://hyper-light/server")?;
        let c = String::from_utf8(signed(COMMIT, &cert, &key)?)?;
        let (headers, message) = c.split_once("\n\n").ok_or_else(|| anyhow::anyhow!("no message"))?;
        let lines: Vec<&str> = headers.lines().collect();
        assert!(lines.get(4).is_some_and(|l| *l == "gpgsig -----BEGIN SIGNED MESSAGE-----"), "after the committer: {headers}");
        assert!(lines.iter().skip(5).all(|l| l.starts_with(' ')), "each line after its first a continuation: {headers}");
        assert_eq!(message, "fresh: replayed onto main@5d0e7b\n\nKiln-Change: kc-7f3a9e\n");
        // Signed again, it keeps one signature.
        let again = with_signature(c.as_bytes(), &signature_of(c.as_bytes()).unwrap_or_default());
        assert_eq!(again, c.as_bytes());
        assert_eq!(signature_of(COMMIT), None);
        Ok(())
    }

    #[test]
    fn w27_r03_a_changed_or_foreign_commit_signature_is_refused() -> anyhow::Result<()> {
        let (root, issuer) = ca("kiln CA")?;
        let (cert, key) = signer(&issuer, "kiln://hyper-light/person/sam")?;
        let c = signed(COMMIT, &cert, &key)?;
        let sig = signature_of(&c).unwrap_or_default();
        // Its message changed after it was signed.
        let changed = String::from_utf8(payload(&c))?.replace("main@5d0e7b", "main@000000");
        assert_eq!(verify(changed.as_bytes(), &sig, &root, &[], AT), Err(Refusal::NotThisCommit));
        // Certified by another CA.
        let (_, other) = ca("someone else's CA")?;
        let (fcert, fkey) = signer(&other, "kiln://hyper-light/person/sam")?;
        let f = signed(COMMIT, &fcert, &fkey)?;
        assert!(matches!(verify(&payload(&f), &signature_of(&f).unwrap_or_default(), &root, &[], AT), Err(Refusal::Signer(chain::Refusal::NotIssued { .. }))));
        // Signed by a key whose identity kiln revoked before it took the commit, whatever the
        // commit's own dates say.
        let key_id = chain::verify(&[&cert], &root, &[], AT)?.key;
        let revoked = [Revocation { serial: None, key: Some(key_id), at: AT - 60, reason: "device lost".into(), by: "sam".into() }];
        assert!(matches!(verify(&payload(&c), &sig, &root, &revoked, AT), Err(Refusal::Signer(chain::Refusal::Revoked { .. }))));
        assert!(verify(&payload(&c), &sig, &root, &revoked, AT - 120).is_ok(), "taken before the revocation");
        // Not a signature at all.
        assert!(matches!(verify(COMMIT, "-----BEGIN PGP SIGNATURE-----\nabc\n-----END PGP SIGNATURE-----\n", &root, &[], AT), Err(Refusal::Malformed(_))));
        Ok(())
    }
}
