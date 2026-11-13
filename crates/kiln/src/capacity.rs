//! kiln's runner as its machine's capacity holder: its shell (docs/lb.md "A machine's keeper"; Hex's
//! `docs/design/capacity-view.md`, "What kiln's runner implements as a holder on `CAPACITY_PORT`";
//! kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 13). Its identity, the holder's item 1: enrolled
//! with kiln's CA by CSR, its key made on the runner and never sent, its certificate named the
//! runner's machine, which the keeper's configuration names.
//!
//! - **kiln's CA as Hex reaches it** ([`PkiCa`], Hex's `hex_quic::enroll::Ca`): each name admitted
//!   once, with a grant of its own; a request's signature checked as it's parsed; its certificate
//!   issued for the admitted name alone, under kiln's root (`Pki::sign_holder`). It passes Hex's
//!   own suite for that seam (HEX-ENROLL-1 to 5).
//! - **The server's side** ([`issue`], `POST /api/agents/capacity`): a runner proves its machine as
//!   for a second runner process (its current certificate and a CSR for that certificate's key), and
//!   its holder's request is admitted for that machine's name alone, once, and issued.
//! - **The runner's side** ([`credentials`]): its enrollment kept in a folder of its own
//!   (`hex_quic::enroll::store`), begun where there's none, asked of the server where it's
//!   unanswered or its certificate is due for renewal, and loaded.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context as _;
use base64::Engine as _;
use hex_quic::enroll::{Ca, CaRefused, Issued, Request, store};
use hex_quic::tls::Credentials;
use hyper_quic::rustls::pki_types::CertificateDer;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::pki::{Leaf, Pki};

/// kiln's CA as Hex reaches it (module docs).
pub struct PkiCa<'a> {
    pki: &'a Pki,
    grants: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl<'a> PkiCa<'a> {
    pub fn new(pki: &'a Pki) -> Self {
        Self { pki, grants: Mutex::new(BTreeMap::new()) }
    }

    /// `name` may enroll once, with `grant`.
    pub fn admit(&self, name: &str, grant: &[u8]) {
        self.grants.lock().insert(name.to_owned(), grant.to_vec());
    }

    /// kiln's root, every peer's anchor.
    pub fn root(&self) -> CertificateDer<'static> {
        CertificateDer::from(self.pki.ca_der.clone())
    }
}

impl Ca for PkiCa<'_> {
    fn enroll(&self, request: &Request) -> Result<Issued, CaRefused> {
        let not_admitted = |why: &str| CaRefused::NotAdmitted { name: request.name.clone(), why: why.to_owned() };
        {
            let mut grants = self.grants.lock();
            match grants.get(&request.name) {
                Some(grant) if *grant == request.grant => {
                    grants.remove(&request.name);
                }
                Some(_) => return Err(not_admitted("its grant isn't the one admitted")),
                None => return Err(not_admitted("it was never admitted, or enrolled already")),
            }
        }
        let leaf = self.pki.sign_holder(&request.csr, &request.name).map_err(|e| CaRefused::Malformed { why: format!("{e:#}") })?;
        Ok(Issued { chain: vec![CertificateDer::from(leaf)], roots: vec![self.root()] })
    }
}

/// What a runner sends `POST /api/agents/capacity`: the proof of its machine (its current
/// certificate, and a CSR for that certificate's key) and its holder's request (PKCS #10, DER,
/// base64), whose key is the holder's own.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollHolder {
    pub cert_pem: String,
    pub proof_csr: String,
    pub csr: String,
}

/// What the server answers: the holder's chain, its own certificate first, and kiln's roots, each
/// DER in base64.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Enrolled {
    pub name: String,
    pub chain: Vec<String>,
    pub roots: Vec<String>,
}

fn b64(der: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(der)
}

fn unb64(what: &str, text: &str) -> anyhow::Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(text).with_context(|| format!("{what} isn't base64"))
}

/// The server's side (module docs): the machine `ask` proves, and its holder's certificate, named
/// that machine alone and issued once, through [`PkiCa`].
pub fn issue(pki: &Pki, ask: &EnrollHolder) -> anyhow::Result<Enrolled> {
    let name = pki.prove_machine(&ask.cert_pem, &ask.proof_csr)?;
    let ca = PkiCa::new(pki);
    let grant = crate::util::random_token().into_bytes();
    ca.admit(&name, &grant);
    let issued = ca.enroll(&Request { name: name.clone(), csr: unb64("its holder's request", &ask.csr)?, grant })?;
    Ok(Enrolled { name, chain: issued.chain.iter().map(|c| b64(c)).collect(), roots: issued.roots.iter().map(|c| b64(c)).collect() })
}

impl Enrolled {
    fn issued(&self) -> anyhow::Result<Issued> {
        let ders = |what: &str, list: &[String]| -> anyhow::Result<Vec<CertificateDer<'static>>> { list.iter().map(|t| unb64(what, t).map(CertificateDer::from)).collect() };
        Ok(Issued { chain: ders("a certificate of its chain", &self.chain)?, roots: ders("a root", &self.roots)? })
    }
}

/// The holder's enrollment in `dir` as it stands: its credentials where they're kept and not due
/// for renewal, else its request for kiln's CA (begun here where there was none: a key made here,
/// never sent). A certificate due for renewal is let go, its key kept, so its request is asked
/// again.
pub fn prepared(dir: &Path, name: &str) -> anyhow::Result<Result<Credentials, Request>> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let chain = dir.join(store::CHAIN);
    if let Ok(pem) = std::fs::read_to_string(&chain) {
        if !crate::pki::needs_renewal(&pem)
            && let Ok(credentials) = store::load(dir)
        {
            return Ok(Ok(credentials));
        }
        std::fs::remove_file(&chain).with_context(|| format!("let go of {}", chain.display()))?;
        let _ = std::fs::remove_file(dir.join(store::ROOTS));
    }
    if !dir.join(store::REQUEST).exists() {
        store::begin(dir, name).map_err(|e| anyhow::anyhow!("begin the holder's enrollment in {}: {e}", dir.display()))?;
    }
    Ok(Err(store::request(dir, Vec::new()).map_err(|e| anyhow::anyhow!("read the holder's request in {}: {e}", dir.display()))?))
}

/// The credentials kiln's CA's answer makes with the enrollment in `dir`, once it proves to be for
/// its key and request; kept there.
pub fn finished(dir: &Path, issued: &Issued) -> anyhow::Result<Credentials> {
    store::finish(dir, issued).map_err(|e| anyhow::anyhow!("keep kiln's CA's answer in {}: {e}", dir.display()))?;
    store::load(dir).map_err(|e| anyhow::anyhow!("load the holder's credentials in {}: {e}", dir.display()))
}

/// The runner's side (module docs): the holder's credentials in `dir`, enrolled with the server at
/// `server_url` (whose CA is `ca_pem`) by `machine`'s proof where they're missing or due for
/// renewal. Its name is the server's: the machine's.
pub async fn credentials(dir: &Path, server_url: &str, ca_pem: &str, machine_name: &str, machine: &Leaf) -> anyhow::Result<Credentials> {
    use crate::http::client::{Client, Roots};
    let request = match prepared(dir, machine_name)? {
        Ok(credentials) => return Ok(credentials),
        Err(request) => request,
    };
    let ask = EnrollHolder { cert_pem: machine.cert_pem.clone(), proof_csr: crate::pki::make_csr_with_key(machine_name, &machine.key_pem)?, csr: b64(&request.csr) };
    let http = Client::new(Roots::WebPkiPlusDer(crate::join::pem_der(ca_pem)?), None)?;
    let url = format!("{}/api/agents/capacity", server_url.trim_end_matches('/'));
    let resp = http.post(&url).json(&ask).send().await.with_context(|| format!("reach {url}"))?;
    if !resp.is_success() {
        let status = resp.status();
        let body = resp.bytes(1 << 16).await.unwrap_or_default();
        anyhow::bail!("the server refused the holder's certificate ({status}): {}", String::from_utf8_lossy(&body).trim());
    }
    let enrolled: Enrolled = resp.json(1 << 20).await.context("the holder's certificate")?;
    finished(dir, &enrolled.issued()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> anyhow::Result<(tempfile::TempDir, Pki)> {
        let dir = tempfile::tempdir()?;
        let pki = Pki::open_scratch(&dir.path().join("pki"))?;
        Ok((dir, pki))
    }

    struct Fixture<'a>(&'a PkiCa<'a>);

    impl hex_quic::enroll::conformance::Fixture for Fixture<'_> {
        fn admit(&self, name: &str) -> Vec<u8> {
            let grant = format!("grant for {name}").into_bytes();
            self.0.admit(name, &grant);
            grant
        }

        fn roots(&self) -> Vec<CertificateDer<'static>> {
            vec![self.0.root()]
        }
    }

    /// Step 13, the holder's item 1: kiln's CA, as Hex reaches it, passes Hex's own suite for the
    /// seam (HEX-ENROLL-1 to 5): issued for the request's key under kiln's root, named as admitted
    /// whatever it asked, a name never admitted refused, a forged request refused, a grant taken once.
    #[test]
    fn kiln_s_ca_passes_hex_s_enrollment_suite() -> anyhow::Result<()> {
        let (_dir, pki) = scratch()?;
        let ca = PkiCa::new(&pki);
        let failures = hex_quic::enroll::conformance::run(&ca, &Fixture(&ca));
        anyhow::ensure!(failures.is_empty(), "{failures:?}");
        Ok(())
    }

    /// The server's side and the runner's: a machine's proof admits its holder's request for that
    /// machine's name alone, whatever it asked; the runner's folder keeps the answer, and its
    /// credentials verify under kiln's root. A proof of another key, or of a certificate this CA
    /// didn't issue, is refused, and nothing is issued.
    #[test]
    fn a_runner_proves_its_machine_and_its_holder_is_named_after_it() -> anyhow::Result<()> {
        let (dir, pki) = scratch()?;
        let machine_key = rcgen::KeyPair::generate()?.serialize_pem();
        let machine_cert = pki.sign_csr(&crate::pki::make_csr_with_key("runner-7", &machine_key)?, "runner-7", 30)?;
        let folder = dir.path().join("capacity");
        let request = match prepared(&folder, "asked-another-name")? {
            Err(request) => request,
            Ok(_) => anyhow::bail!("a new folder holds no credentials"),
        };
        let ask = EnrollHolder { cert_pem: machine_cert.clone(), proof_csr: crate::pki::make_csr_with_key("runner-7", &machine_key)?, csr: b64(&request.csr) };
        let enrolled = issue(&pki, &ask)?;
        anyhow::ensure!(enrolled.name == "runner-7", "named after the machine it proved: {}", enrolled.name);
        let credentials = finished(&folder, &enrolled.issued()?)?;
        anyhow::ensure!(credentials.chain.len() == 1, "its own certificate");
        anyhow::ensure!(matches!(prepared(&folder, "runner-7")?, Ok(_)), "kept: the next start loads it");
        let other_key = rcgen::KeyPair::generate()?.serialize_pem();
        let forged = EnrollHolder { proof_csr: crate::pki::make_csr_with_key("runner-7", &other_key)?, ..ask.clone() };
        anyhow::ensure!(issue(&pki, &forged).is_err(), "a proof for another key is refused");
        let (_other_dir, other_pki) = scratch()?;
        let stranger = other_pki.sign_csr(&crate::pki::make_csr_with_key("runner-7", &machine_key)?, "runner-7", 30)?;
        anyhow::ensure!(issue(&pki, &EnrollHolder { cert_pem: stranger, ..ask }).is_err(), "a certificate another CA issued is refused");
        Ok(())
    }
}
