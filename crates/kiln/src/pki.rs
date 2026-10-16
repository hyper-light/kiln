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
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DnType,
    ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
};
use time::{Duration, OffsetDateTime};

/// The DNS name agents expect in the server certificate (sent as SNI).
pub const SERVER_NAME: &str = "kiln-server";
const CA_YEARS: i64 = 10;
const LEAF_DAYS: i64 = 365;
/// Ephemeral pod agents get short-lived certificates.
pub const EPHEMERAL_DAYS: i64 = 2;
/// Renew a leaf when it has less than this left.
const RENEW_DAYS: i64 = 30;

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
}
