//! A runner's identity as a node of kiln's Slates fleet (kiln goal 95101ef32ffb8b099942fe076619c310: "the slates
//! server federates and replicates changes across kiln runners", its piece K1): the key the runner's Slates node
//! proves itself with, made on the runner and never sent, and its certificate from kiln's CA, which every node of the
//! fleet trusts as the fleet's one enrollment root.
//!
//! Slates' fleet (its `role::fleet` manifest, `slates_api::role::FleetSelection`) names one TLS name every node's
//! certificate carries, and admits a node whose certificate chains to an enrollment root and carries
//! `r<region>.d<domain>.<fleet>` for the failure domain it announces (slates' `discovery::scope_name`, checked as the
//! node enrolls). kiln is that root: one fleet per kiln server, named [`FLEET`]; each runner its own failure domain
//! ([`domain_of`] its machine's name), so slates spreads a volume's copies across runners; region 0.
//!
//! - **The server's side** ([`issue`], `POST /api/agents/slates-fleet`): a runner proves its machine as a second
//!   runner process does (its current certificate and a CSR for that certificate's key), and its node's request is
//!   signed for the fleet's names alone, whatever it asked (`Pki::sign_slates_node`), as `crate::capacity`'s holders
//!   are signed for their machine's name.
//! - **The runner's side** ([`prepared`], [`finished`], [`identity`]): its node's files in a folder of its own, DER as
//!   slates' manifest reads them: the key ([`KEY`], `0600`, made where there is none and kept across renewals), the
//!   certificate ([`CERTIFICATE`]) and kiln's CA ([`ROOT`]). The server is asked where the certificate is missing, is
//!   for another key, names another fleet or domain (the machine since renamed), or is due for renewal; an answer is
//!   kept only once it proves to be for the node's key, under the CA the runner already trusts.
//!
//! Who joins the fleet, and when, is the operator's to say (K3, opt-in, kiln's AGENTS.md); this module only makes each
//! runner's identity.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use base64::Engine as _;
use rcgen::{CertificateParams, KeyPair, PublicKeyData as _};
use serde::{Deserialize, Serialize};
use x509_parser::extensions::GeneralName;

use crate::pki::{Leaf, Pki};

/// The fleet's TLS name: the name every node's certificate carries and every peer verifies its sessions against
/// (slates' manifest `name`). One fleet per kiln server, so one name; kiln's CA, not the name, says whose fleet it is.
pub const FLEET: &str = "slates.kiln";
/// The node's private key (PKCS #8, DER), made on the runner and never sent.
pub const KEY: &str = "node.key.der";
/// The node's certificate (DER), as kiln's CA issued it.
pub const CERTIFICATE: &str = "node.crt.der";
/// kiln's CA certificate (DER): the fleet's enrollment root (slates' manifest `enrollment_roots`).
pub const ROOT: &str = "kiln-ca.der";
/// The mode of the node's key file: its owner's alone, as every key kiln keeps.
const KEY_MODE: u32 = 0o600;
/// The mode of the node's certificate and root: anyone may read a certificate.
const CERTIFICATE_MODE: u32 = 0o644;
/// The most bytes the server's answer may hold: a certificate and a root, each a few kilobytes, base64.
const ANSWER_BYTES: usize = 1 << 20;
/// The most bytes of a refusal's body kept for its message.
const REFUSAL_BYTES: usize = 1 << 16;

/// The failure domain of the runner whose machine is named `machine`: the first eight bytes of the SHA-256 of the
/// name, so each runner is its own domain (slates spreads a volume's copies across domains) and keeps it while its
/// name stands.
pub fn domain_of(machine: &str) -> u64 {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(machine.as_bytes());
    u64::from_be_bytes(<[u8; 8]>::try_from(digest.get(..8).unwrap_or_default()).unwrap_or_default())
}

/// The name a node of failure domain `domain` carries beside the fleet's (slates' `discovery::scope_name`, region 0).
pub fn scope_name(fleet: &str, domain: u64) -> String {
    format!("r0.d{domain}.{fleet}")
}

/// What a runner sends `POST /api/agents/slates-fleet`: the proof of its machine (its current certificate, and a CSR
/// for that certificate's key) and its node's request (PKCS #10, DER, base64), whose key is the node's own.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollNode {
    pub cert_pem: String,
    pub proof_csr: String,
    pub csr: String,
}

/// What the server answers: the machine it proved, the fleet's name, the node's failure domain, and the node's
/// certificate and kiln's roots (DER, base64).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeIssued {
    pub machine: String,
    pub fleet: String,
    pub domain: u64,
    pub certificate: String,
    pub roots: Vec<String>,
}

fn b64(der: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(der)
}

fn unb64(what: &str, text: &str) -> anyhow::Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(text).with_context(|| format!("{what} isn't base64"))
}

/// The server's side (module docs): the machine `ask` proves, and its node's certificate for the fleet's names alone.
pub fn issue(pki: &Pki, ask: &EnrollNode) -> anyhow::Result<NodeIssued> {
    let machine = pki.prove_machine(&ask.cert_pem, &ask.proof_csr)?;
    let domain = domain_of(&machine);
    let der = pki.sign_slates_node(&unb64("the node's request", &ask.csr)?, FLEET, domain)?;
    Ok(NodeIssued { machine, fleet: FLEET.to_owned(), domain, certificate: b64(&der), roots: vec![b64(&pki.ca_der)] })
}

/// A runner's node identity as its files hold it: what a fleet manifest names for this node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIdentity {
    /// The fleet's TLS name.
    pub fleet: String,
    /// The node's failure domain.
    pub domain: u64,
    /// Its key (DER).
    pub key: PathBuf,
    /// Its certificate (DER).
    pub certificate: PathBuf,
    /// kiln's CA, the fleet's enrollment root (DER).
    pub root: PathBuf,
}

/// Where a runner's node identity stands ([`prepared`]).
#[derive(Debug)]
pub enum Prepared {
    /// Its certificate is held: for its key, under the root it keeps, naming the fleet and its domain, and not due for
    /// renewal.
    Ready(NodeIdentity),
    /// Its request for kiln's CA (PKCS #10, DER), signed with its key.
    Ask(Vec<u8>),
}

/// The node's key in `dir`: the one kept there, or one made now and kept (`0600`), never sent.
fn key_in(dir: &Path) -> anyhow::Result<KeyPair> {
    let path = dir.join(KEY);
    match std::fs::read(&path) {
        Ok(der) => KeyPair::try_from(der.as_slice()).with_context(|| format!("read the fleet node's key {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = KeyPair::generate().context("make the fleet node's key")?;
            crate::util::write_secret(&path, &key.serialize_der(), KEY_MODE).with_context(|| format!("keep the fleet node's key in {}", path.display()))?;
            Ok(key)
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// The DNS names a DER certificate carries, in order.
fn dns_names(der: &[u8]) -> Option<Vec<String>> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    let san = cert.subject_alternative_name().ok()??;
    Some(
        san.value
            .general_names
            .iter()
            .filter_map(|name| match name {
                GeneralName::DNSName(dns) => Some((*dns).to_owned()),
                _ => None,
            })
            .collect(),
    )
}

/// Whether DER certificate `der` is for `key`.
fn for_key(der: &[u8], key: &KeyPair) -> bool {
    x509_parser::parse_x509_certificate(der).is_ok_and(|(_, cert)| key.der_bytes() == cert.public_key().subject_public_key.data.as_ref())
}

/// Whether DER certificate `der`, issued by `root`, is the node's for `key`, carrying exactly the fleet's names for
/// `domain`, and not yet due for renewal.
fn stands(der: &[u8], root: &[u8], key: &KeyPair, fleet: &str, domain: u64) -> bool {
    crate::pki::verify_issued(root, der)
        && for_key(der, key)
        && dns_names(der).is_some_and(|names| names == [fleet.to_owned(), scope_name(fleet, domain)])
        && !crate::pki::needs_renewal(&crate::pki::cert_pem(der))
}

/// The node's identity in `dir` for the machine named `machine` (module docs): its certificate where it stands, else
/// its request for kiln's CA, its key made here where there is none.
pub fn prepared(dir: &Path, machine: &str) -> anyhow::Result<Prepared> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let key = key_in(dir)?;
    let domain = domain_of(machine);
    if let (Ok(der), Ok(root)) = (std::fs::read(dir.join(CERTIFICATE)), std::fs::read(dir.join(ROOT)))
        && stands(&der, &root, &key, FLEET, domain)
    {
        return Ok(Prepared::Ready(identity_in(dir, FLEET, domain)));
    }
    // The server names the certificate whatever the request asks; the request names the fleet for a reader of it.
    let request = CertificateParams::new(vec![FLEET.to_owned()])
        .context("the fleet node's request")?
        .serialize_request(&key)
        .context("sign the fleet node's request")?;
    Ok(Prepared::Ask(request.der().to_vec()))
}

fn identity_in(dir: &Path, fleet: &str, domain: u64) -> NodeIdentity {
    NodeIdentity { fleet: fleet.to_owned(), domain, key: dir.join(KEY), certificate: dir.join(CERTIFICATE), root: dir.join(ROOT) }
}

/// The identity kiln's CA's answer makes with the key in `dir`, kept there once it proves to be the node's: issued by
/// `trusted` (the CA the runner already trusts, which the answer's roots must hold), for the node's key, carrying
/// exactly the fleet's names for the domain it gives.
pub fn finished(dir: &Path, trusted: &[u8], issued: &NodeIssued) -> anyhow::Result<NodeIdentity> {
    let der = unb64("the node's certificate", &issued.certificate)?;
    let roots = issued.roots.iter().map(|root| unb64("a root", root)).collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(roots.iter().any(|root| root.as_slice() == trusted), "the answer's roots don't hold the CA this runner trusts");
    let key = key_in(dir)?;
    anyhow::ensure!(crate::pki::verify_issued(trusted, &der), "the node's certificate wasn't issued by the CA this runner trusts");
    anyhow::ensure!(for_key(&der, &key), "the node's certificate is for another key than the one kept in {}", dir.display());
    anyhow::ensure!(
        dns_names(&der).is_some_and(|names| names == [issued.fleet.clone(), scope_name(&issued.fleet, issued.domain)]),
        "the node's certificate doesn't name the fleet {} for domain {}",
        issued.fleet,
        issued.domain
    );
    crate::util::write_secret(&dir.join(ROOT), trusted, CERTIFICATE_MODE).with_context(|| format!("keep kiln's CA in {}", dir.display()))?;
    crate::util::write_secret(&dir.join(CERTIFICATE), &der, CERTIFICATE_MODE).with_context(|| format!("keep the node's certificate in {}", dir.display()))?;
    Ok(identity_in(dir, &issued.fleet, issued.domain))
}

/// The runner's side (module docs): the node's identity in `dir`, enrolled with the server at `server_url` (whose CA
/// is `ca_pem`) by `machine`'s proof where its certificate is missing or no longer stands.
pub async fn identity(dir: &Path, server_url: &str, ca_pem: &str, machine_name: &str, machine: &Leaf) -> anyhow::Result<NodeIdentity> {
    use crate::http::client::{Client, Roots};
    let csr = match prepared(dir, machine_name)? {
        Prepared::Ready(identity) => return Ok(identity),
        Prepared::Ask(csr) => csr,
    };
    let ask = EnrollNode { cert_pem: machine.cert_pem.clone(), proof_csr: crate::pki::make_csr_with_key(machine_name, &machine.key_pem)?, csr: b64(&csr) };
    let trusted = crate::join::pem_der(ca_pem)?;
    let http = Client::new(Roots::WebPkiPlusDer(trusted.clone()), None)?;
    let url = format!("{}/api/agents/slates-fleet", server_url.trim_end_matches('/'));
    let resp = http.post(&url).json(&ask).send().await.with_context(|| format!("reach {url}"))?;
    if !resp.is_success() {
        let status = resp.status();
        let body = resp.bytes(REFUSAL_BYTES).await.unwrap_or_default();
        anyhow::bail!("the server refused the fleet node's certificate ({status}): {}", String::from_utf8_lossy(&body).trim());
    }
    let issued: NodeIssued = resp.json(ANSWER_BYTES).await.context("the fleet node's certificate")?;
    finished(dir, &trusted, &issued)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine enrolled with `pki`, as a runner holds it: its certificate and key.
    fn machine(pki: &Pki, name: &str) -> anyhow::Result<Leaf> {
        pki.issue_local(name)
    }

    /// The request a runner sends for the node in `dir`, proving `leaf`'s machine named `name`.
    fn ask(dir: &Path, name: &str, leaf: &Leaf) -> anyhow::Result<EnrollNode> {
        let Prepared::Ask(csr) = prepared(dir, name)? else {
            anyhow::bail!("a node with no certificate asked for none");
        };
        Ok(EnrollNode { cert_pem: leaf.cert_pem.clone(), proof_csr: crate::pki::make_csr_with_key(name, &leaf.key_pem)?, csr: b64(&csr) })
    }

    fn eku(der: &[u8]) -> anyhow::Result<(bool, bool)> {
        let (_, cert) = x509_parser::parse_x509_certificate(der).map_err(|e| anyhow::anyhow!("{e}"))?;
        let e = cert.extended_key_usage().map_err(|e| anyhow::anyhow!("{e}"))?.ok_or_else(|| anyhow::anyhow!("no extended key usage"))?.value;
        Ok((e.client_auth, e.server_auth))
    }

    /// K1: a runner's node key is made on the runner and kept (`0600`); its request, sent with its machine's proof, is
    /// signed by kiln's CA for the fleet's two names alone (the fleet's TLS name and `r0.d<domain>.<fleet>`, its domain
    /// its machine's), for both sides of a session; the answer is kept, and the node then stands without asking again,
    /// on the same key.
    #[test]
    fn k1_a_runners_node_is_signed_for_the_fleets_names_and_kept_on_its_own_key() -> anyhow::Result<()> {
        let scratch = tempfile::tempdir()?;
        let pki = Pki::open_scratch(&scratch.path().join("pki"))?;
        let dir = scratch.path().join("fleet");
        let leaf = machine(&pki, "bench-a")?;
        let request = ask(&dir, "bench-a", &leaf)?;
        let key = std::fs::read(dir.join(KEY))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(dir.join(KEY))?.permissions().mode() & 0o777, KEY_MODE);
        }
        let issued = issue(&pki, &request)?;
        assert_eq!((issued.machine.as_str(), issued.fleet.as_str(), issued.domain), ("bench-a", FLEET, domain_of("bench-a")));
        let der = unb64("certificate", &issued.certificate)?;
        assert_eq!(dns_names(&der), Some(vec![FLEET.to_owned(), scope_name(FLEET, domain_of("bench-a"))]));
        assert_eq!(eku(&der)?, (true, true), "a node is both sides of the fleet's sessions");
        assert!(pki.issued(&der));
        assert!(for_key(&der, &KeyPair::try_from(key.as_slice())?), "signed for the node's own key");
        let identity = finished(&dir, &pki.ca_der, &issued)?;
        assert_eq!(identity, identity_in(&dir, FLEET, domain_of("bench-a")));
        assert_eq!(std::fs::read(dir.join(ROOT))?, pki.ca_der);
        let Prepared::Ready(again) = prepared(&dir, "bench-a")? else {
            anyhow::bail!("a node whose certificate stands asked again");
        };
        assert_eq!(again, identity);
        assert_eq!(std::fs::read(dir.join(KEY))?, key, "the key is kept, never made again");
        Ok(())
    }

    /// K1: the fleet's names are the server's, never the request's; a machine's proof made with another key is refused;
    /// each runner is its own failure domain, the same one every time.
    #[test]
    fn k1_the_server_names_the_node_and_refuses_another_machines_proof() -> anyhow::Result<()> {
        let scratch = tempfile::tempdir()?;
        let pki = Pki::open_scratch(&scratch.path().join("pki"))?;
        let leaf = machine(&pki, "bench-a")?;
        let other = machine(&pki, "bench-b")?;
        let mut request = ask(&scratch.path().join("a"), "bench-a", &leaf)?;
        // A request asking for other names gets the fleet's.
        let key = KeyPair::generate()?;
        let asked = CertificateParams::new(vec!["evil.example".to_owned(), "kiln-server".to_owned()])?.serialize_request(&key)?;
        request.csr = b64(asked.der());
        let issued = issue(&pki, &request)?;
        assert_eq!(dns_names(&unb64("certificate", &issued.certificate)?), Some(vec![FLEET.to_owned(), scope_name(FLEET, domain_of("bench-a"))]));
        // The proof's CSR must be for the machine certificate's own key.
        request.proof_csr = crate::pki::make_csr_with_key("bench-a", &other.key_pem)?;
        assert!(issue(&pki, &request).is_err());
        assert_eq!(domain_of("bench-a"), domain_of("bench-a"));
        assert_ne!(domain_of("bench-a"), domain_of("bench-b"));
        assert_eq!(scope_name("slates.kiln", 7), "r0.d7.slates.kiln");
        assert!(pki.sign_slates_node(asked.der(), "kiln-server", 1).is_err(), "the server's own name names no fleet");
        assert!(pki.sign_slates_node(asked.der(), "Not A Name", 1).is_err());
        Ok(())
    }

    /// K1: an answer is kept only once it proves to be the node's: issued by the CA the runner trusts, for its key; and
    /// a certificate for another machine's domain (the machine since renamed) no longer stands, so the node asks again
    /// with the key it kept.
    #[test]
    fn k1_an_answer_for_another_key_or_ca_is_refused_and_a_rename_asks_again() -> anyhow::Result<()> {
        let scratch = tempfile::tempdir()?;
        let pki = Pki::open_scratch(&scratch.path().join("pki"))?;
        let stranger = Pki::open_scratch(&scratch.path().join("stranger"))?;
        let dir = scratch.path().join("fleet");
        let leaf = machine(&pki, "bench-a")?;
        let request = ask(&dir, "bench-a", &leaf)?;
        let issued = issue(&pki, &request)?;
        assert!(finished(&dir, &stranger.ca_der, &issued).is_err(), "roots without the trusted CA");
        let mut forged = issued.clone();
        forged.roots.push(b64(&stranger.ca_der));
        assert!(finished(&dir, &stranger.ca_der, &forged).is_err(), "issued by a CA the runner doesn't trust");
        let elsewhere = ask(&scratch.path().join("elsewhere"), "bench-a", &leaf)?;
        assert!(finished(&dir, &pki.ca_der, &issue(&pki, &elsewhere)?).is_err(), "a certificate for another key");
        assert!(!dir.join(CERTIFICATE).exists(), "nothing refused was kept");
        finished(&dir, &pki.ca_der, &issued)?;
        let key = std::fs::read(dir.join(KEY))?;
        let Prepared::Ask(_) = prepared(&dir, "bench-a-renamed")? else {
            anyhow::bail!("a certificate for the machine's old name still stood");
        };
        assert_eq!(std::fs::read(dir.join(KEY))?, key, "asked again with the key it kept");
        Ok(())
    }
}
