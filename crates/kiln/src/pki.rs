//! kiln's private CA. Created on first start under `<data>/pki`, never needs a password, and every
//! file holding key material is written `0600`.
//!
//! * The server's QUIC leaf carries the SAN [`SERVER_NAME`] (the name agents verify) plus the
//!   host's names.
//! * Agents generate their key locally and send only a CSR; the server signs it after checking
//!   the enrollment token. Private keys never cross the network.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, PublicKeyData as _, SerialNumber,
};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

/// The DNS name agents expect in the server certificate (sent as SNI).
pub const SERVER_NAME: &str = "kiln-server";
const CA_YEARS: i64 = 10;
const LEAF_DAYS: i64 = 365;
/// Ephemeral pod agents get short-lived certificates.
pub const EPHEMERAL_DAYS: i64 = 2;
/// Renew a leaf when it has less than this left.
const RENEW_DAYS: i64 = 30;
/// How long a front door's certificate (HTTPS on a runner) lasts: a year, renewed when it has
/// less than [`RENEW_DAYS`] left.
pub const FRONT_DOOR_DAYS: i64 = 365;
/// The longest a TLS server certificate may last for browsers and Apple's platforms.
pub const MAX_TLS_DAYS: i64 = 398;

pub struct Pki {
    dir: PathBuf,
    issuer: Issuer<'static, KeyPair>,
    pub ca_pem: String,
    pub ca_der: Vec<u8>,
}

/// A certificate + private key, both PEM.
impl std::fmt::Debug for Leaf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Leaf").field("cert_pem", &self.cert_pem.len()).field("key_pem", &"<redacted>").finish()
    }
}

#[derive(Clone)]
pub struct Leaf {
    pub cert_pem: String,
    pub key_pem: String,
}

fn pem_to_der(pem: &str) -> anyhow::Result<Vec<u8>> {
    use hyper_quic::rustls::pki_types::CertificateDer;
    use hyper_quic::rustls::pki_types::pem::PemObject as _;
    Ok(CertificateDer::from_pem_slice(pem.as_bytes())
        .map_err(|e| anyhow::anyhow!("bad certificate pem: {e:?}"))?
        .to_vec())
}

fn not_after_of(pem: &str) -> Option<OffsetDateTime> {
    let der = pem_to_der(pem).ok()?;
    let (_, cert) = x509_parser::parse_x509_certificate(&der).ok()?;
    Some(cert.validity().not_after.to_datetime())
}

/// The first DNS SAN of a DER certificate: the identity an agent is bound to.
pub fn cert_dns_name(der: &[u8]) -> Option<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    let san = cert.subject_alternative_name().ok()??;
    san.value.general_names.iter().find_map(|n| match n {
        x509_parser::extensions::GeneralName::DNSName(d) => Some((*d).to_owned()),
        _ => None,
    })
}

/// The name a leaf certificate carries (its first DNS SAN).
pub fn leaf_name(leaf: &Leaf) -> Option<String> {
    cert_dns_name(&pem_to_der(&leaf.cert_pem).ok()?)
}

fn leaf_params(names: Vec<String>, days: i64) -> anyhow::Result<CertificateParams> {
    let mut p = CertificateParams::new(names.clone()).context("leaf names")?;
    let now = OffsetDateTime::now_utc();
    p.not_before = now - Duration::hours(1);
    p.not_after = now + Duration::days(days);
    p.is_ca = IsCa::NoCa;
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    p.extended_key_usages =
        vec![ExtendedKeyUsagePurpose::ServerAuth, ExtendedKeyUsagePurpose::ClientAuth];
    if let Some(n) = names.first() {
        p.distinguished_name.push(DnType::CommonName, n.clone());
    }
    Ok(p)
}

/// What a certificate says about itself, for status lines and checks. Never holds key material.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CertInfo {
    /// Its subject's common name (empty when it has none).
    pub subject: String,
    /// Its issuer's common name, else the whole issuer name.
    pub issuer: String,
    /// Its DNS names and IP addresses (subject alternative names), in order.
    pub names: Vec<String>,
    pub not_before: i64,
    pub not_after: i64,
    /// SHA-256 of the certificate (DER), hex.
    pub sha256: String,
    /// Its extended key usages allow TLS servers (or it lists none, which allows any).
    pub server_auth: bool,
}

/// The first certificate of a PEM chain, described.
pub fn cert_info(pem: &str) -> anyhow::Result<CertInfo> {
    cert_info_der(&pem_to_der(pem)?)
}

/// A DER certificate, described.
pub fn cert_info_der(der: &[u8]) -> anyhow::Result<CertInfo> {
    use x509_parser::extensions::GeneralName;
    let (_, cert) = x509_parser::parse_x509_certificate(der).map_err(|e| anyhow::anyhow!("not a certificate: {e}"))?;
    let common_name = |n: &x509_parser::x509::X509Name<'_>| n.iter_common_name().next().and_then(|cn| cn.as_str().ok()).map(str::to_owned);
    let mut names = Vec::new();
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for n in &san.value.general_names {
            match n {
                GeneralName::DNSName(d) => names.push((*d).to_owned()),
                GeneralName::IPAddress(b) => {
                    let ip = match b.len() {
                        4 => <[u8; 4]>::try_from(*b).ok().map(|a| std::net::IpAddr::from(a).to_string()),
                        16 => <[u8; 16]>::try_from(*b).ok().map(|a| std::net::IpAddr::from(a).to_string()),
                        _ => None,
                    };
                    names.extend(ip);
                }
                _ => {}
            }
        }
    }
    let server_auth = match cert.extended_key_usage() {
        Ok(Some(eku)) => eku.value.any || eku.value.server_auth,
        Ok(None) => true,
        Err(_) => false,
    };
    Ok(CertInfo {
        subject: common_name(cert.subject()).unwrap_or_default(),
        issuer: common_name(cert.issuer()).unwrap_or_else(|| cert.issuer().to_string()),
        names,
        not_before: cert.validity().not_before.timestamp(),
        not_after: cert.validity().not_after.timestamp(),
        sha256: crate::util::sha256_hex(der),
        server_auth,
    })
}

/// Whether a certificate naming `names` is valid for the host `host`: an exact name (ASCII
/// case-insensitive), or a wildcard (`*.kiln.home`) standing for exactly one leftmost label.
pub fn covers(names: &[String], host: &str) -> bool {
    let host = host.trim_end_matches('.');
    names.iter().any(|n| {
        let n = n.trim_end_matches('.');
        if n.eq_ignore_ascii_case(host) {
            return true;
        }
        match (n.strip_prefix("*."), host.split_once('.')) {
            (Some(suffix), Some((label, rest))) => !label.is_empty() && label != "*" && rest.eq_ignore_ascii_case(suffix),
            _ => false,
        }
    })
}

/// A CSR for a front door's certificate: `names` signed with `key_pem` (the server decides the
/// names it signs anyway, see [`Pki::sign_front_door`]).
pub fn front_door_csr(names: &[String], key_pem: &str) -> anyhow::Result<String> {
    let key = KeyPair::from_pem(key_pem).context("parse the front door's key")?;
    let mut params = CertificateParams::new(names.to_vec()).context("certificate names")?;
    params.distinguished_name = DistinguishedName::new();
    if let Some(n) = names.iter().find(|n| n.parse::<std::net::IpAddr>().is_err()) {
        params.distinguished_name.push(DnType::CommonName, n.clone());
    }
    Ok(params.serialize_request(&key)?.pem()?)
}

/// A fresh private key (PEM) for a front door's certificate.
pub fn new_key_pem() -> anyhow::Result<String> {
    Ok(KeyPair::generate().context("generate a key")?.serialize_pem())
}

/// Whether `cert_pem`'s first certificate is for the private key `key_pem`.
pub fn cert_matches_key(cert_pem: &str, key_pem: &str) -> bool {
    let (Ok(der), Ok(key)) = (pem_to_der(cert_pem), KeyPair::from_pem(key_pem)) else { return false };
    let Ok((_, cert)) = x509_parser::parse_x509_certificate(&der) else { return false };
    key.der_bytes() == cert.public_key().subject_public_key.data.as_ref()
}

/// Whether the CA certificate `ca_der` signed the certificate `der`.
pub fn verify_issued(ca_der: &[u8], der: &[u8]) -> bool {
    let (Ok((_, cert)), Ok((_, ca))) = (x509_parser::parse_x509_certificate(der), x509_parser::parse_x509_certificate(ca_der)) else {
        return false;
    };
    cert.verify_signature(Some(ca.public_key())).is_ok()
}

/// Make an agent name safe to use as a DNS SAN.
pub fn sanitize_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let s = s.trim_matches(|c| c == '-' || c == '.').to_owned();
    if s.is_empty() { "agent".to_owned() } else { crate::util::truncate(&s, 63).to_owned() }
}

impl Pki {
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let ca_pem_path = dir.join("ca.pem");
        let ca_key_path = dir.join("ca.key");
        let (ca_pem, key) = match (
            std::fs::read_to_string(&ca_pem_path),
            std::fs::read_to_string(&ca_key_path),
        ) {
            (Ok(c), Ok(k)) => (c, KeyPair::from_pem(&k).context("parse ca.key")?),
            _ => {
                let key = KeyPair::generate().context("generate CA key")?;
                let mut p = CertificateParams::new(Vec::<String>::new())?;
                p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
                p.distinguished_name.push(DnType::CommonName, "kiln CA");
                p.distinguished_name.push(DnType::OrganizationName, "hyper-light");
                p.key_usages = vec![
                    KeyUsagePurpose::KeyCertSign,
                    KeyUsagePurpose::CrlSign,
                    KeyUsagePurpose::DigitalSignature,
                ];
                let now = OffsetDateTime::now_utc();
                p.not_before = now - Duration::hours(1);
                p.not_after = now + Duration::days(365 * CA_YEARS);
                let cert = p.self_signed(&key).context("self-sign CA")?;
                crate::util::write_secret(&ca_key_path, key.serialize_pem().as_bytes(), 0o600)?;
                crate::util::write_secret(&ca_pem_path, cert.pem().as_bytes(), 0o644)?;
                tracing::info!(dir = %dir.display(), "created kiln CA");
                (cert.pem(), key)
            }
        };
        let issuer = Issuer::from_ca_cert_pem(&ca_pem, key).context("load CA")?;
        let ca_der = pem_to_der(&ca_pem)?;
        Ok(Self { dir: dir.to_path_buf(), issuer, ca_pem, ca_der })
    }

    /// The server's leaf, (re)issued when missing, expiring, or when its names changed.
    pub fn server_leaf(&self, extra_names: &[String]) -> anyhow::Result<Leaf> {
        let mut names = vec![SERVER_NAME.to_owned(), "localhost".to_owned(), "127.0.0.1".to_owned()];
        let host = crate::config::Config::hostname();
        names.push(host);
        for n in extra_names {
            if !names.contains(n) {
                names.push(n.clone());
            }
        }
        let cert_path = self.dir.join("server.pem");
        let key_path = self.dir.join("server.key");
        let names_path = self.dir.join("server.names");
        let wanted = names.join(",");
        if let (Ok(c), Ok(k), Ok(n)) = (
            std::fs::read_to_string(&cert_path),
            std::fs::read_to_string(&key_path),
            std::fs::read_to_string(&names_path),
        ) {
            let fresh = not_after_of(&c)
                .is_some_and(|na| na - OffsetDateTime::now_utc() > Duration::days(RENEW_DAYS));
            if fresh && n == wanted {
                return Ok(Leaf { cert_pem: c, key_pem: k });
            }
        }
        let key = KeyPair::generate()?;
        let cert = leaf_params(names, LEAF_DAYS)?.signed_by(&key, &self.issuer)?;
        let leaf = Leaf { cert_pem: cert.pem(), key_pem: key.serialize_pem() };
        crate::util::write_secret(&key_path, leaf.key_pem.as_bytes(), 0o600)?;
        crate::util::write_secret(&cert_path, leaf.cert_pem.as_bytes(), 0o644)?;
        crate::util::write_secret(&names_path, wanted.as_bytes(), 0o644)?;
        tracing::info!("issued server certificate");
        Ok(leaf)
    }

    /// Sign an agent's CSR. The certificate's name is forced to `name`, whatever the CSR asked.
    pub fn sign_csr(&self, csr_pem: &str, name: &str, days: i64) -> anyhow::Result<String> {
        let csr = CertificateSigningRequestParams::from_pem(csr_pem).context("parse CSR")?;
        let signed = CertificateSigningRequestParams {
            params: leaf_params(vec![sanitize_name(name)], days)?,
            public_key: csr.public_key,
        }
        .signed_by(&self.issuer)
        .context("sign CSR")?;
        Ok(signed.pem())
    }

    /// One more certificate for a machine that is already enrolled: for a second runner process
    /// beside its first one (upgrades that keep the first one's terminals alive until they end).
    /// The proof is the machine's current certificate — issued by this CA and still valid — plus a
    /// CSR for that same key, whose signature shows the requester holds the private key. Returns
    /// the machine's name and the new certificate.
    pub fn sign_instance(&self, cert_pem: &str, csr_pem: &str) -> anyhow::Result<(String, String)> {
        let der = pem_to_der(cert_pem)?;
        let (_, cert) = x509_parser::parse_x509_certificate(&der).map_err(|e| anyhow::anyhow!("parse certificate: {e}"))?;
        let (_, ca) = x509_parser::parse_x509_certificate(&self.ca_der).map_err(|e| anyhow::anyhow!("parse CA: {e}"))?;
        cert.verify_signature(Some(ca.public_key())).map_err(|_| anyhow::anyhow!("that certificate was not issued by this server"))?;
        anyhow::ensure!(cert.validity().is_valid(), "that certificate has expired");
        let name = cert_dns_name(&der).ok_or_else(|| anyhow::anyhow!("that certificate names no machine"))?;
        anyhow::ensure!(name != SERVER_NAME, "that is the server's certificate");
        // Parsing checks the CSR's own signature (possession of its private key).
        let csr = CertificateSigningRequestParams::from_pem(csr_pem).context("parse CSR")?;
        anyhow::ensure!(
            csr.public_key.der_bytes() == cert.public_key().subject_public_key.data.as_ref(),
            "the CSR is for a different key than the certificate"
        );
        let signed = CertificateSigningRequestParams { params: leaf_params(vec![name.clone()], LEAF_DAYS)?, public_key: csr.public_key }
            .signed_by(&self.issuer)
            .context("sign CSR")?;
        Ok((name, signed.pem()))
    }

    /// Sign a runner's CSR for its front door: a TLS server certificate for exactly `names` (DNS
    /// names, wildcards such as `*.kiln.home`, and IP addresses), whatever the CSR asked, valid for
    /// `days` (fewer than [`MAX_TLS_DAYS`]). Its only extended key usage is serverAuth, so it never
    /// passes as a machine's client certificate, and it may not name [`SERVER_NAME`], the identity
    /// runners check the server by. Each one gets a random serial number.
    pub fn sign_front_door(&self, csr_pem: &str, names: &[String], days: i64) -> anyhow::Result<String> {
        anyhow::ensure!(!names.is_empty(), "a certificate needs at least one name");
        anyhow::ensure!(names.len() <= 64, "a certificate takes at most 64 names");
        for n in names {
            anyhow::ensure!(!n.trim_end_matches('.').eq_ignore_ascii_case(SERVER_NAME), "{SERVER_NAME} is the server's own identity");
        }
        // Parsing checks the CSR's own signature (possession of its private key).
        let csr = CertificateSigningRequestParams::from_pem(csr_pem).context("parse CSR")?;
        let mut p = CertificateParams::new(names.to_vec()).context("certificate names")?;
        let now = OffsetDateTime::now_utc();
        p.not_before = now - Duration::hours(1);
        p.not_after = now + Duration::days(days.clamp(1, MAX_TLS_DAYS - 1));
        p.is_ca = IsCa::ExplicitNoCa;
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.use_authority_key_identifier_extension = true;
        let mut serial: [u8; 16] = rand::random();
        if let Some(b) = serial.first_mut() {
            // A positive number, of the same length every time.
            *b = (*b & 0x7f) | 0x40;
        }
        p.serial_number = Some(SerialNumber::from_slice(&serial));
        p.distinguished_name = DistinguishedName::new();
        if let Some(n) = names.iter().find(|n| n.parse::<std::net::IpAddr>().is_err()) {
            p.distinguished_name.push(DnType::CommonName, n.clone());
        }
        p.distinguished_name.push(DnType::OrganizationName, "kiln front door");
        let signed = CertificateSigningRequestParams { params: p, public_key: csr.public_key }.signed_by(&self.issuer).context("sign CSR")?;
        Ok(signed.pem())
    }

    /// Whether this CA issued (signed) the DER certificate `der`.
    pub fn issued(&self, der: &[u8]) -> bool {
        verify_issued(&self.ca_der, der)
    }

    /// Issue a full leaf (key generated here). Used for the server's own in-process local agent.
    pub fn issue_local(&self, name: &str) -> anyhow::Result<Leaf> {
        let key = KeyPair::generate()?;
        let cert = leaf_params(vec![sanitize_name(name)], LEAF_DAYS)?.signed_by(&key, &self.issuer)?;
        Ok(Leaf { cert_pem: cert.pem(), key_pem: key.serialize_pem() })
    }
}

/// Agent side: generate a key and CSR for `name`.
pub fn make_csr(name: &str) -> anyhow::Result<(String, String)> {
    let key = KeyPair::generate()?;
    let params = CertificateParams::new(vec![sanitize_name(name)])?;
    let csr = params.serialize_request(&key)?;
    Ok((csr.pem()?, key.serialize_pem()))
}

/// A CSR for `name` signed with an existing key: how a second runner process of an enrolled
/// machine asks for its own certificate (see [`Pki::sign_instance`]).
pub fn make_csr_with_key(name: &str, key_pem: &str) -> anyhow::Result<String> {
    let key = KeyPair::from_pem(key_pem).context("parse the machine's key")?;
    let params = CertificateParams::new(vec![sanitize_name(name)])?;
    Ok(params.serialize_request(&key)?.pem()?)
}

/// Does this leaf need renewing?
pub fn needs_renewal(cert_pem: &str) -> bool {
    not_after_of(cert_pem)
        .is_none_or(|na| na - OffsetDateTime::now_utc() < Duration::days(RENEW_DAYS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_signs_csr_and_persists() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = Pki::open(dir.path())?;
        let leaf = pki.server_leaf(&[])?;
        assert!(!needs_renewal(&leaf.cert_pem));
        let (csr, _key) = make_csr("Bench Box #1")?;
        let cert = pki.sign_csr(&csr, "Bench Box #1", 30)?;
        assert!(cert.contains("BEGIN CERTIFICATE"));
        // Reopen: same CA.
        let again = Pki::open(dir.path())?;
        assert_eq!(again.ca_pem, pki.ca_pem);
        assert_eq!(sanitize_name("Bench Box #1"), "bench-box--1");
        Ok(())
    }

    #[test]
    fn front_door_certificates_are_tls_server_certificates_for_exactly_their_names() -> anyhow::Result<()> {
        use x509_parser::extensions::GeneralName;
        let dir = tempfile::tempdir()?;
        let pki = Pki::open(dir.path())?;
        let key = new_key_pem()?;
        // The CSR asks for more than it gets: the server decides the names.
        let csr = front_door_csr(&["kiln.home".to_owned(), "evil.example".to_owned()], &key)?;
        let names: Vec<String> = ["kiln.home", "*.kiln.home", "ci.home", "kiln.local", "192.168.1.77", "fd00::77"].map(str::to_owned).to_vec();
        let pem = pki.sign_front_door(&csr, &names, FRONT_DOOR_DAYS)?;
        let der = pem_to_der(&pem)?;
        let (_, cert) = x509_parser::parse_x509_certificate(&der).map_err(|e| anyhow::anyhow!("{e}"))?;

        // SANs: the DNS names and the IP addresses asked for, nothing else.
        let san = cert.subject_alternative_name().map_err(|e| anyhow::anyhow!("{e}"))?.ok_or_else(|| anyhow::anyhow!("no SAN"))?;
        let dns: Vec<&str> = san.value.general_names.iter().filter_map(|n| if let GeneralName::DNSName(d) = n { Some(*d) } else { None }).collect();
        assert_eq!(dns, ["kiln.home", "*.kiln.home", "ci.home", "kiln.local"]);
        let ips = san.value.general_names.iter().filter(|n| matches!(n, GeneralName::IPAddress(_))).count();
        assert_eq!(ips, 2);
        let info = cert_info(&pem)?;
        assert_eq!(info.names, names);
        assert_eq!(info.subject, "kiln.home");
        assert_eq!(info.issuer, "kiln CA");
        assert!(info.server_auth);

        // serverAuth only: never a client certificate (which is how runners identify themselves).
        let eku = cert.extended_key_usage().map_err(|e| anyhow::anyhow!("{e}"))?.ok_or_else(|| anyhow::anyhow!("no EKU"))?;
        assert!(eku.value.server_auth && !eku.value.client_auth && !eku.value.any);
        let bc = cert.basic_constraints().map_err(|e| anyhow::anyhow!("{e}"))?.ok_or_else(|| anyhow::anyhow!("no basic constraints"))?;
        assert!(!bc.value.ca);

        // Validity: under 398 days in all, starting now, so browsers and iOS take it.
        let (nb, na) = (cert.validity().not_before.timestamp(), cert.validity().not_after.timestamp());
        let now = OffsetDateTime::now_utc().unix_timestamp();
        assert!(na - nb < MAX_TLS_DAYS * 86_400, "{} days", (na - nb) / 86_400);
        assert!(na - now > (FRONT_DOOR_DAYS - 1) * 86_400 && nb <= now);
        assert!(!needs_renewal(&pem));
        // Asking for longer gets the most browsers allow, no more.
        let long = cert_info(&pki.sign_front_door(&csr, &names, 5000)?)?;
        assert!(long.not_after - long.not_before < MAX_TLS_DAYS * 86_400);

        // Signed by this CA, for the CSR's key; each one gets its own serial.
        assert!(pki.issued(&der));
        assert!(!Pki::open(tempfile::tempdir()?.path())?.issued(&der));
        assert!(cert_matches_key(&pem, &key));
        assert!(!cert_matches_key(&pem, &new_key_pem()?));
        let again = x509_parser::parse_x509_certificate(&pem_to_der(&pki.sign_front_door(&csr, &names, 30)?)?).map_err(|e| anyhow::anyhow!("{e}"))?.1.raw_serial().to_vec();
        assert_ne!(again, cert.raw_serial().to_vec());

        // Never the server's identity, never nameless.
        assert!(pki.sign_front_door(&csr, &["kiln-server".to_owned()], 30).is_err());
        assert!(pki.sign_front_door(&csr, &[], 30).is_err());
        assert!(pki.sign_front_door("not a csr", &names, 30).is_err());
        Ok(())
    }

    #[test]
    fn wildcards_cover_one_label() {
        let names = vec!["kiln.home".to_owned(), "*.kiln.home".to_owned()];
        assert!(covers(&names, "kiln.home"));
        assert!(covers(&names, "KILN.home."));
        assert!(covers(&names, "ci.kiln.home"));
        assert!(!covers(&names, "a.b.kiln.home"));
        assert!(!covers(&names, "kiln.lan"));
        assert!(!covers(&names, "*.kiln.home.evil"));
        assert!(!covers(&[], "kiln.home"));
    }

    #[test]
    fn a_second_certificate_needs_the_machines_own_key_and_certificate() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = Pki::open(dir.path())?;
        let (csr, key) = make_csr("bench")?;
        let cert = pki.sign_csr(&csr, "bench", 30)?;

        // Same key, same machine: a distinct certificate for the same name.
        let again = make_csr_with_key("bench", &key)?;
        let (name, second) = pki.sign_instance(&cert, &again)?;
        assert_eq!(name, "bench");
        assert_ne!(pem_to_der(&second)?, pem_to_der(&cert)?, "a second runner needs its own certificate (peer id)");
        assert_eq!(leaf_name(&Leaf { cert_pem: second, key_pem: key.clone() }).as_deref(), Some("bench"));

        // Another key: refused, even with a valid certificate.
        let (other_csr, _other_key) = make_csr("bench")?;
        assert!(pki.sign_instance(&cert, &other_csr).is_err());

        // A certificate from another CA: refused.
        let foreign = Pki::open(tempfile::tempdir()?.path())?;
        let foreign_cert = foreign.sign_csr(&csr, "bench", 30)?;
        assert!(pki.sign_instance(&foreign_cert, &again).is_err());

        // The server's own certificate is never a machine's.
        let server = pki.server_leaf(&[])?;
        let server_csr = make_csr_with_key(SERVER_NAME, &server.key_pem)?;
        assert!(pki.sign_instance(&server.cert_pem, &server_csr).is_err());
        Ok(())
    }
}
