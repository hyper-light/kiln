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
//! - **Discovery** (K2; [`admit`], [`Directory`], [`discover`], `POST /api/agents/slates-fleet/members`): a runner
//!   that joins says where its node is reached and which certificate it holds, with its machine's proof; the server
//!   takes the word only for that machine's own node (a certificate kiln's CA issued naming the fleet and that
//!   machine's domain, at an address Slates can dial: an IPv4 literal or a DNS name, and a base port whose block of
//!   two fits the port range), keeps it in memory by machine, and answers with every node whose runner is connected.
//!   A server that restarts hears each again at its runner's next word; a node that joins later is also admitted by
//!   Slates' own enrollment through any listed node, under the fleet's root.
//! - **The manifest** ([`write_manifest`]): Slates' fleet manifest in the node's folder ([`MANIFEST`]), naming every
//!   listed node with its address, its failure domain and its certificate (each peer's kept under [`PEERS`]), this
//!   node with its key, and kiln's CA as the fleet's enrollment root; the path a `slates_api::role::FleetSelection`
//!   names with this machine's name.
//!
//! - **The operator's choice** (K3; [`place_of`], `[slates_fleet]` in the server's configuration,
//!   `POST /api/agents/slates-fleet/place`): which runners join, each by its machine's name with where its node is
//!   reached, and the fleet's `f`; off while none is listed (kiln's AGENTS.md: opt-in automation). Only a listed
//!   runner's word is taken, at the address the operator gave it ([`admit_chosen`]).
//! - **A chosen runner's start** ([`join`], called by `workspace::slates::ensure_up` before it starts the machine's
//!   instance): its place asked of the server, its identity made or renewed, its word said and the fleet's nodes
//!   heard, the manifest written, and the manifest read by Slates' own reader (`slates_api::role::check_fleet`) as the
//!   instance's start will read it; the instance then starts with `StartSpec.fleet` naming the manifest and the
//!   machine. A runner the operator didn't list starts its instance alone, as before.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
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
/// Slates' fleet manifest in the node's folder (its `role::fleet` format).
pub const MANIFEST: &str = "fleet.json";
/// Where the node's folder keeps the other nodes' certificates (DER), one per machine.
pub const PEERS: &str = "peers";
/// The ports a Slates node serves its peers on from its base: probes on the base, records on the next (Slates'
/// `deploy::PORTS_PER_NODE`), so a base port leaves room for one more.
const PORTS_PER_NODE: u16 = 2;
/// The longest DNS name (RFC 1035 §2.3.4: 255 octets on the wire, 253 written).
const DNS_NAME_BYTES: usize = 253;
/// The longest DNS label (RFC 1035 §2.3.4).
const DNS_LABEL_BYTES: usize = 63;

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

// ------------------------------------------------------------------------------------ discovery (K2)

/// Where a node is reached, as Slates' manifest states it (its `deploy::NodeAddress`): an IPv4 literal or a DNS name,
/// and the base of the node's two ports, which must both fit the port range. Slates dials no IPv6 address.
pub fn check_address(address: &str) -> anyhow::Result<()> {
    let (host, port) = address.rsplit_once(':').ok_or_else(|| anyhow::anyhow!("{address:?} names no port: a node's address is host:port"))?;
    let port: u16 = port.parse().map_err(|_| anyhow::anyhow!("{address:?}'s port isn't a port number"))?;
    anyhow::ensure!(port != 0, "{address:?}: a node's base port is never 0");
    anyhow::ensure!(port.checked_add(PORTS_PER_NODE - 1).is_some(), "{address:?}: a node serves on its base port and the next, past the port range here");
    if host.parse::<Ipv4Addr>().is_ok() {
        return Ok(());
    }
    let label_ok = |label: &str| {
        !label.is_empty() && label.len() <= DNS_LABEL_BYTES && !label.starts_with('-') && !label.ends_with('-') && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    anyhow::ensure!(
        !host.is_empty() && host.len() <= DNS_NAME_BYTES && host.split('.').all(label_ok) && host.chars().any(|c| c.is_ascii_alphabetic()),
        "{address:?}: a node's host is an IPv4 address or a DNS name (Slates dials no IPv6 address)"
    );
    Ok(())
}

/// A runner's word that its node is in the fleet: where its peers reach it, the certificate it holds (DER, base64), and,
/// once its daemon serves, the fleet member id it serves as (K4: what a move of a volume to it names).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeAnnouncement {
    pub address: String,
    pub certificate: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<u64>,
}

/// What a runner sends `POST /api/agents/slates-fleet/members`: its machine's proof (its current certificate, and a
/// CSR for that certificate's key) and its node's word.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Announce {
    pub cert_pem: String,
    pub proof_csr: String,
    pub node: NodeAnnouncement,
}

/// A node of the fleet as the server lists it: its machine, where it's reached, its failure domain, its certificate
/// (DER, base64), and the member id its daemon serves as, once its runner said it (K4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub machine: String,
    pub address: String,
    pub domain: u64,
    pub certificate: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<u64>,
}

/// What the server answers: the fleet's name and its nodes whose runners are connected, by machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Members {
    pub fleet: String,
    pub members: Vec<Member>,
}

/// The server's side of a runner's word (module docs): the node of the machine `ask` proves, taken only with that
/// machine's own certificate (issued by this CA, still valid, naming the fleet and the machine's domain) and an
/// address Slates can dial.
pub fn admit(pki: &Pki, ask: &Announce) -> anyhow::Result<Member> {
    let machine = pki.prove_machine(&ask.cert_pem, &ask.proof_csr)?;
    check_address(&ask.node.address)?;
    let der = unb64("the node's certificate", &ask.node.certificate)?;
    anyhow::ensure!(pki.issued(&der), "the node's certificate wasn't issued by this server's CA");
    anyhow::ensure!(crate::pki::still_valid(&crate::pki::cert_pem(&der)), "the node's certificate has expired");
    let domain = domain_of(&machine);
    anyhow::ensure!(
        dns_names(&der).is_some_and(|names| names == [FLEET.to_owned(), scope_name(FLEET, domain)]),
        "that certificate isn't {machine}'s node's: it doesn't name the fleet {FLEET} for {machine}'s domain"
    );
    Ok(Member { machine, address: ask.node.address.clone(), domain, certificate: ask.node.certificate.clone(), member: ask.node.member })
}

/// The fleet's nodes as their runners last said, by machine (one each: a runner's newer word replaces its older), held
/// in the server's memory: bounded by the machines the server knows, since only a known machine's word is taken.
#[derive(Default)]
pub struct Directory {
    members: parking_lot::Mutex<BTreeMap<String, Member>>,
}

impl Directory {
    /// Takes a node's word ([`admit`]'s), in place of its machine's older one.
    pub fn admit(&self, member: Member) {
        self.members.lock().insert(member.machine.clone(), member);
    }

    /// The nodes whose runners `live` says are connected, by machine.
    pub fn members(&self, live: impl Fn(&str) -> bool) -> Members {
        let members = self.members.lock().values().filter(|member| live(&member.machine)).cloned().collect();
        Members { fleet: FLEET.to_owned(), members }
    }
}

/// The runner's word for the node `identity` names, reached at `address` (module docs).
pub fn announcement(identity: &NodeIdentity, address: &str) -> anyhow::Result<NodeAnnouncement> {
    check_address(address)?;
    let der = std::fs::read(&identity.certificate).with_context(|| format!("read {}", identity.certificate.display()))?;
    Ok(NodeAnnouncement { address: address.to_owned(), certificate: b64(&der), member: None })
}

/// The runner's side of discovery (module docs): its node's word, with `machine`'s proof, to the server at
/// `server_url` (whose CA is `ca_pem`), and the fleet's live nodes it answers with.
pub async fn discover(server_url: &str, ca_pem: &str, machine_name: &str, machine: &Leaf, node: &NodeAnnouncement) -> anyhow::Result<Members> {
    use crate::http::client::{Client, Roots};
    let ask = Announce { cert_pem: machine.cert_pem.clone(), proof_csr: crate::pki::make_csr_with_key(machine_name, &machine.key_pem)?, node: node.clone() };
    let http = Client::new(Roots::WebPkiPlusDer(crate::join::pem_der(ca_pem)?), None)?;
    let url = format!("{}/api/agents/slates-fleet/members", server_url.trim_end_matches('/'));
    let resp = http.post(&url).json(&ask).send().await.with_context(|| format!("reach {url}"))?;
    if !resp.is_success() {
        let status = resp.status();
        let body = resp.bytes(REFUSAL_BYTES).await.unwrap_or_default();
        anyhow::bail!("the server refused the fleet node's word ({status}): {}", String::from_utf8_lossy(&body).trim());
    }
    resp.json(ANSWER_BYTES).await.context("the fleet's nodes")
}

/// Writes Slates' fleet manifest for this node into `dir` (module docs) from the fleet's nodes as the server listed
/// them, with fault tolerance `f` (the operator's, K3), and returns its path. This node must be listed, with the
/// certificate kept here; every node's address is checked again and no two may hold one certificate (Slates refuses
/// both). Each other node's certificate is kept under [`PEERS`], and a peer no node lists any more is let go. Only
/// this node's key is named: Slates reads no other's.
pub fn write_manifest(dir: &Path, identity: &NodeIdentity, machine: &str, members: &Members, f: u32) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(members.fleet == identity.fleet, "the server lists the fleet {}, and this node is {}'s", members.fleet, identity.fleet);
    let own = std::fs::read(&identity.certificate).with_context(|| format!("read {}", identity.certificate.display()))?;
    let me = members.members.iter().find(|m| m.machine == machine).ok_or_else(|| anyhow::anyhow!("the server doesn't list {machine}'s node"))?;
    anyhow::ensure!(unb64("its certificate", &me.certificate)? == own, "the server lists {machine}'s node with another certificate than the one kept in {}", dir.display());
    let mut certificates = BTreeSet::new();
    let peers = dir.join(PEERS);
    std::fs::create_dir_all(&peers).with_context(|| format!("create {}", peers.display()))?;
    let mut nodes = Vec::with_capacity(members.members.len());
    let mut kept = BTreeSet::new();
    for member in &members.members {
        check_address(&member.address).with_context(|| format!("{}'s node", member.machine))?;
        anyhow::ensure!(certificates.insert(member.certificate.as_str()), "two nodes hold one certificate ({})", member.machine);
        let (certificate, key) = if member.machine == machine {
            (CERTIFICATE.to_owned(), KEY.to_owned())
        } else {
            anyhow::ensure!(crate::pki::sanitize_name(&member.machine) == member.machine, "{:?} isn't a machine's name", member.machine);
            let file = format!("{}.crt.der", member.machine);
            crate::util::write_secret(&peers.join(&file), &unb64("a node's certificate", &member.certificate)?, CERTIFICATE_MODE)
                .with_context(|| format!("keep {}'s certificate in {}", member.machine, peers.display()))?;
            kept.insert(file.clone());
            (format!("{PEERS}/{file}"), String::new())
        };
        nodes.push(serde_json::json!({ "node": member.machine, "address": member.address, "certificate": certificate, "key": key, "domain": member.domain }));
    }
    for entry in std::fs::read_dir(&peers).with_context(|| format!("read {}", peers.display()))? {
        let entry = entry?;
        if !entry.file_name().to_str().is_some_and(|name| kept.contains(name)) {
            std::fs::remove_file(entry.path()).with_context(|| format!("let go of {}", entry.path().display()))?;
        }
    }
    let manifest = serde_json::json!({ "name": members.fleet, "f": f, "enrollment_roots": [ROOT], "nodes": nodes });
    let path = dir.join(MANIFEST);
    crate::util::write_secret(&path, &serde_json::to_vec_pretty(&manifest)?, CERTIFICATE_MODE).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

// -------------------------------------------------------------------------- the operator's choice (K3)

/// Where the operator placed a runner in the fleet (`[slates_fleet]`): the fleet's name, its fault tolerance, and where
/// the runner's node is reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Place {
    pub fleet: String,
    pub f: u32,
    pub address: String,
}

/// The place `[slates_fleet]` gives the machine named `machine`: none when it lists no runner (off) or not that one;
/// refused, naming what's wrong, when the setting can't make a fleet: a name kiln doesn't give a machine, an address
/// Slates can't dial, or fewer runners than `2f + 1` (a write commits at `f + 1` of `2f + 1` copies).
pub fn place_of(setting: &crate::config::SlatesFleet, machine: &str) -> Result<Option<Place>, String> {
    if setting.nodes.is_empty() {
        return Ok(None);
    }
    for (name, address) in &setting.nodes {
        if crate::pki::sanitize_name(name) != *name {
            return Err(format!("[slates_fleet.nodes] {name:?} isn't a machine's name as kiln writes it (lowercase letters, digits, '-' and '.')"));
        }
        check_address(address).map_err(|e| format!("[slates_fleet.nodes] {name}: {e:#}"))?;
    }
    let candidates = u64::from(setting.f).saturating_mul(2).saturating_add(1);
    if u64::try_from(setting.nodes.len()).unwrap_or(u64::MAX) < candidates {
        return Err(format!("[slates_fleet] f = {} needs at least {candidates} runners (a write commits at f + 1 of 2f + 1 copies), and {} are listed", setting.f, setting.nodes.len()));
    }
    Ok(setting.nodes.get(machine).map(|address| Place { fleet: FLEET.to_owned(), f: setting.f, address: address.clone() }))
}

/// [`admit`] under the operator's choice: the word of a runner `setting` lists, for its node at the address listed for
/// it, and no other.
pub fn admit_chosen(pki: &Pki, ask: &Announce, setting: &crate::config::SlatesFleet) -> anyhow::Result<Member> {
    let member = admit(pki, ask)?;
    let place = place_of(setting, &member.machine).map_err(|e| anyhow::anyhow!("{e}"))?;
    let place = place.ok_or_else(|| anyhow::anyhow!("{} isn't in kiln's Slates fleet: [slates_fleet.nodes] doesn't list it", member.machine))?;
    anyhow::ensure!(place.address == member.address, "{}'s node is listed at {}, not {}", member.machine, place.address, member.address);
    Ok(member)
}

/// What a runner's start needs to join (module docs): its node's folder (`<state>/slates-fleet`), the server, kiln's
/// root (PEM), and its machine's own name and certificate, whose proof every ask carries.
#[derive(Debug, Clone)]
pub struct Source {
    pub dir: PathBuf,
    pub server_url: String,
    pub ca_pem: String,
    pub machine_name: String,
    pub machine: Leaf,
}

/// The runner's source, set once as its process starts ([`set_source`]); none in a process that is no runner's.
static SOURCE: std::sync::OnceLock<Source> = std::sync::OnceLock::new();

/// Sets the runner's source (the runner's start, once): from then on its machine's instance asks to join as it starts.
pub fn set_source(source: Source) {
    let _ = SOURCE.set(source);
}

/// The runner's source, when this process is a runner's.
pub fn source() -> Option<&'static Source> {
    SOURCE.get()
}

/// What a runner sends `POST /api/agents/slates-fleet/place`: its machine's proof.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskPlace {
    pub cert_pem: String,
    pub proof_csr: String,
}

/// The place the server's `[slates_fleet]` gives the runner `source` is (none when it lists it not).
pub async fn ask_place(source: &Source) -> anyhow::Result<Option<Place>> {
    use crate::http::client::{Client, Roots};
    let ask = AskPlace { cert_pem: source.machine.cert_pem.clone(), proof_csr: crate::pki::make_csr_with_key(&source.machine_name, &source.machine.key_pem)? };
    let http = Client::new(Roots::WebPkiPlusDer(crate::join::pem_der(&source.ca_pem)?), None)?;
    let url = format!("{}/api/agents/slates-fleet/place", source.server_url.trim_end_matches('/'));
    let resp = http.post(&url).json(&ask).send().await.with_context(|| format!("reach {url}"))?;
    if !resp.is_success() {
        let status = resp.status();
        let body = resp.bytes(REFUSAL_BYTES).await.unwrap_or_default();
        anyhow::bail!("the server refused this runner's place in kiln's Slates fleet ({status}): {}", String::from_utf8_lossy(&body).trim());
    }
    resp.json(ANSWER_BYTES).await.context("this runner's place in kiln's Slates fleet")
}

/// The fleet selection the manifest written in `dir` makes for `machine`, read by Slates' own reader exactly as the
/// instance's start reads it: refused in Slates' words, so an instance is never started on a manifest it would refuse.
pub fn checked(manifest: &Path, machine: &str) -> anyhow::Result<slates_api::role::FleetSelection> {
    let selection = slates_api::role::FleetSelection { manifest: manifest.to_string_lossy().into_owned(), node: machine.to_owned() };
    slates_api::role::check_fleet(&selection).map_err(|e| anyhow::anyhow!("Slates refuses the fleet manifest {}: {e}", manifest.display()))?;
    Ok(selection)
}

/// What a runner's start joined with ([`join`]): the fleet selection its instance starts with, and its node's word, said
/// again with its member id once its daemon serves ([`report_member`]).
#[derive(Debug, Clone)]
pub struct Joined {
    pub selection: slates_api::role::FleetSelection,
    pub node: NodeAnnouncement,
}

/// A runner's start (module docs): what its machine's instance joins the fleet with, when the operator chose it; none
/// when not.
pub async fn join(source: &Source) -> anyhow::Result<Option<Joined>> {
    let Some(place) = ask_place(source).await? else {
        return Ok(None);
    };
    let identity = identity(&source.dir, &source.server_url, &source.ca_pem, &source.machine_name, &source.machine).await?;
    let node = announcement(&identity, &place.address)?;
    let members = discover(&source.server_url, &source.ca_pem, &source.machine_name, &source.machine, &node).await?;
    let manifest = write_manifest(&source.dir, &identity, &source.machine_name, &members, place.f)?;
    let selection = checked(&manifest, &source.machine_name)?;
    tracing::info!(manifest = %manifest.display(), nodes = members.members.len(), f = place.f, "this runner's Slates joins kiln's Slates fleet");
    Ok(Some(Joined { selection, node }))
}

/// The runner's word said again with the member id its daemon serves as since this start (K4): a daemon started again
/// is a new member, so its runner says it at every start.
pub async fn report_member(source: &Source, node: &NodeAnnouncement, member: u64) -> anyhow::Result<()> {
    let node = NodeAnnouncement { member: Some(member), ..node.clone() };
    discover(&source.server_url, &source.ca_pem, &source.machine_name, &source.machine, &node).await.map(|_| ())
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

    /// A runner's node in a folder of its own under `root`, its certificate from `pki` (K1's flow): its machine and its
    /// identity.
    fn node(pki: &Pki, root: &Path, name: &str) -> anyhow::Result<(Leaf, NodeIdentity)> {
        let leaf = machine(pki, name)?;
        let dir = root.join(name);
        let request = ask(&dir, name, &leaf)?;
        let identity = finished(&dir, &pki.ca_der, &issue(pki, &request)?)?;
        Ok((leaf, identity))
    }

    /// The word `name`'s runner sends for its node at `address`.
    fn word(name: &str, leaf: &Leaf, identity: &NodeIdentity, address: &str) -> anyhow::Result<Announce> {
        Ok(Announce { cert_pem: leaf.cert_pem.clone(), proof_csr: crate::pki::make_csr_with_key(name, &leaf.key_pem)?, node: announcement(identity, address)? })
    }

    /// K2: a runner's word is taken for its own node, and the server answers with every node whose runner is connected,
    /// one each, a runner's newer word in place of its older.
    #[test]
    fn k2_a_runner_is_heard_for_its_own_node_and_hears_of_the_connected_ones() -> anyhow::Result<()> {
        let scratch = tempfile::tempdir()?;
        let pki = Pki::open_scratch(&scratch.path().join("pki"))?;
        let directory = Directory::default();
        for (name, address) in [("bench-a", "10.0.0.1:17300"), ("bench-b", "runner-b.lan:17300"), ("bench-c", "10.0.0.3:17300")] {
            let (leaf, identity) = node(&pki, scratch.path(), name)?;
            directory.admit(admit(&pki, &word(name, &leaf, &identity, address)?)?);
        }
        let live = directory.members(|machine| machine != "bench-c");
        assert_eq!(live.fleet, FLEET);
        let listed: Vec<(&str, &str, u64)> = live.members.iter().map(|m| (m.machine.as_str(), m.address.as_str(), m.domain)).collect();
        assert_eq!(listed, [("bench-a", "10.0.0.1:17300", domain_of("bench-a")), ("bench-b", "runner-b.lan:17300", domain_of("bench-b"))]);
        let a = scratch.path().join("bench-a");
        assert_eq!(live.members.first().map(|m| unb64("certificate", &m.certificate)).transpose()?, Some(std::fs::read(a.join(CERTIFICATE))?));
        // A newer word moves the node; it stays one node.
        let leaf = machine(&pki, "bench-a")?;
        let identity = identity_in(&a, FLEET, domain_of("bench-a"));
        directory.admit(admit(&pki, &word("bench-a", &leaf, &identity, "10.0.0.9:17400")?)?);
        let all = directory.members(|_| true);
        assert_eq!(all.members.len(), 3);
        assert_eq!(all.members.first().map(|m| m.address.as_str()), Some("10.0.0.9:17400"));
        Ok(())
    }

    /// K2: a runner's word for another machine's node, or for a certificate another CA issued, is refused, and so is
    /// an address Slates can't dial: IPv6, port 0, a block of two past the port range, no host, no port, or a host that
    /// is no IPv4 address and no DNS name.
    #[test]
    fn k2_a_word_for_another_machines_node_or_an_address_slates_cant_dial_is_refused() -> anyhow::Result<()> {
        let scratch = tempfile::tempdir()?;
        let pki = Pki::open_scratch(&scratch.path().join("pki"))?;
        let (leaf_a, identity_a) = node(&pki, scratch.path(), "bench-a")?;
        let (_, identity_b) = node(&pki, scratch.path(), "bench-b")?;
        assert!(admit(&pki, &word("bench-a", &leaf_a, &identity_a, "10.0.0.1:17300")?).is_ok());
        assert!(admit(&pki, &word("bench-a", &leaf_a, &identity_b, "10.0.0.1:17300")?).is_err(), "bench-b's node in bench-a's word");
        let stranger = Pki::open_scratch(&scratch.path().join("stranger"))?;
        let Prepared::Ask(csr) = prepared(&scratch.path().join("bench-a"), "bench-a-elsewhere")? else {
            anyhow::bail!("a renamed machine's node asked for nothing");
        };
        let foreign = stranger.sign_slates_node(&csr, FLEET, domain_of("bench-a"))?;
        let mut forged = word("bench-a", &leaf_a, &identity_a, "10.0.0.1:17300")?;
        forged.node.certificate = b64(&foreign);
        assert!(admit(&pki, &forged).is_err(), "a certificate another CA issued");
        for address in ["[::1]:17300", "::1:17300", "10.0.0.1:0", "10.0.0.1:65535", ":17300", "10.0.0.1", "bad_name!:17300", "10.0.0.256:17300", "-lead.lan:17300"] {
            assert!(check_address(address).is_err(), "{address}");
        }
        for address in ["runner-a.lan:17300", "10.0.0.1:65534", "localhost:1"] {
            assert!(check_address(address).is_ok(), "{address}");
        }
        assert!(admit(&pki, &Announce { node: NodeAnnouncement { address: "[::1]:17300".to_owned(), ..forged.node.clone() }, ..word("bench-a", &leaf_a, &identity_a, "10.0.0.1:17300")? }).is_err());
        Ok(())
    }

    /// K2: the manifest Slates reads names the fleet, its fault tolerance and kiln's CA as its root, and every listed
    /// node by machine with its address, its domain and its certificate (each peer's kept beside it), this node alone
    /// with its key; every file it names is there, relative to it. A node no longer listed is let go of; a list
    /// without this node, with another certificate for it, or with one certificate twice is refused.
    #[test]
    fn k2_the_manifest_names_every_node_and_this_ones_key_as_slates_reads_it() -> anyhow::Result<()> {
        let scratch = tempfile::tempdir()?;
        let pki = Pki::open_scratch(&scratch.path().join("pki"))?;
        let directory = Directory::default();
        let mut identities = BTreeMap::new();
        for (name, address) in [("bench-a", "10.0.0.1:17300"), ("bench-b", "10.0.0.2:17300"), ("bench-c", "runner-c.lan:17302")] {
            let (leaf, identity) = node(&pki, scratch.path(), name)?;
            directory.admit(admit(&pki, &word(name, &leaf, &identity, address)?)?);
            identities.insert(name, identity);
        }
        let a = identities.get("bench-a").ok_or_else(|| anyhow::anyhow!("bench-a"))?.clone();
        let dir = scratch.path().join("bench-a");
        let members = directory.members(|_| true);
        let path = write_manifest(&dir, &a, "bench-a", &members, 1)?;
        assert_eq!(path, dir.join(MANIFEST));
        let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        assert_eq!(manifest.get("name"), Some(&serde_json::json!(FLEET)), "the manifest names its fleet: {manifest}");
        assert_eq!(manifest.get("f"), Some(&serde_json::json!(1)), "the manifest names its fault tolerance: {manifest}");
        assert_eq!(manifest.get("enrollment_roots"), Some(&serde_json::json!([ROOT])), "the manifest names kiln's CA as its root: {manifest}");
        let nodes = manifest.get("nodes").and_then(serde_json::Value::as_array).ok_or_else(|| anyhow::anyhow!("the manifest names no list of nodes: {manifest}"))?;
        let names: Vec<&str> = nodes.iter().filter_map(|n| n.get("node").and_then(serde_json::Value::as_str)).collect();
        assert_eq!(names, ["bench-a", "bench-b", "bench-c"]);
        assert_eq!(nodes.first(), Some(&serde_json::json!({ "node": "bench-a", "address": "10.0.0.1:17300", "certificate": CERTIFICATE, "key": KEY, "domain": domain_of("bench-a") })));
        assert_eq!(nodes.get(2), Some(&serde_json::json!({ "node": "bench-c", "address": "runner-c.lan:17302", "certificate": "peers/bench-c.crt.der", "key": "", "domain": domain_of("bench-c") })));
        for node in nodes {
            let certificate = node.get("certificate").and_then(serde_json::Value::as_str).ok_or_else(|| anyhow::anyhow!("a node names no certificate: {node}"))?;
            let name = node.get("node").and_then(serde_json::Value::as_str).ok_or_else(|| anyhow::anyhow!("a node names no machine: {node}"))?;
            let theirs = identities.get(name).ok_or_else(|| anyhow::anyhow!("{name}"))?;
            assert_eq!(std::fs::read(dir.join(certificate))?, std::fs::read(&theirs.certificate)?, "{name}'s certificate as Slates reads it");
        }
        assert_eq!(std::fs::read(dir.join(ROOT))?, pki.ca_der);
        assert!(dir.join(KEY).exists());
        // A node no longer listed is let go of.
        let fewer = directory.members(|machine| machine != "bench-c");
        write_manifest(&dir, &a, "bench-a", &fewer, 0)?;
        assert!(!dir.join(PEERS).join("bench-c.crt.der").exists());
        assert!(dir.join(PEERS).join("bench-b.crt.der").exists());
        // Refused: a list without this node, with another certificate for it, with one certificate twice, or of
        // another fleet.
        assert!(write_manifest(&dir, &a, "bench-a", &directory.members(|machine| machine != "bench-a"), 1).is_err());
        let b_certificate = members.members.get(1).map(|m| m.certificate.clone()).ok_or_else(|| anyhow::anyhow!("bench-b"))?;
        let with = |index: usize, certificate: &str| {
            let mut changed = members.clone();
            if let Some(member) = changed.members.get_mut(index) {
                member.certificate = certificate.to_owned();
            }
            changed
        };
        assert!(write_manifest(&dir, &a, "bench-a", &with(0, &b_certificate), 1).is_err(), "another certificate for this node");
        assert!(write_manifest(&dir, &a, "bench-a", &with(2, &b_certificate), 1).is_err(), "one certificate twice");
        let mut other = members.clone();
        other.fleet = "another.fleet".to_owned();
        assert!(write_manifest(&dir, &a, "bench-a", &other, 1).is_err(), "another fleet");
        Ok(())
    }

    /// The setting that lists bench-a, bench-b and bench-c at the addresses the K2 tests give them, `f` 1.
    fn chosen(f: u32) -> crate::config::SlatesFleet {
        crate::config::SlatesFleet {
            f,
            nodes: [("bench-a", "10.0.0.1:17300"), ("bench-b", "10.0.0.2:17300"), ("bench-c", "runner-c.lan:17302")].into_iter().map(|(m, a)| (m.to_owned(), a.to_owned())).collect(),
        }
    }

    /// K3: off while `[slates_fleet]` lists no runner; a listed runner's place is its listed address and the fleet's
    /// `f`; an unlisted one has none; a setting that can't make a fleet (too few runners for its `f`, an address Slates
    /// can't dial, a name kiln gives no machine) is refused, naming what's wrong.
    #[test]
    fn k3_the_operators_setting_places_the_runners_it_lists_and_no_other() -> anyhow::Result<()> {
        assert_eq!(place_of(&crate::config::SlatesFleet::default(), "bench-a"), Ok(None), "off until declared");
        assert_eq!(place_of(&chosen(1), "bench-a"), Ok(Some(Place { fleet: FLEET.to_owned(), f: 1, address: "10.0.0.1:17300".to_owned() })));
        assert_eq!(place_of(&chosen(1), "bench-d"), Ok(None));
        assert!(place_of(&chosen(2), "bench-a").is_err_and(|e| e.contains("at least 5")), "f = 2 over three runners");
        let mut ipv6 = chosen(1);
        ipv6.nodes.insert("bench-d".to_owned(), "[::1]:17300".to_owned());
        assert!(place_of(&ipv6, "bench-a").is_err_and(|e| e.contains("bench-d")));
        let mut named = chosen(1);
        named.nodes.insert("Bench D".to_owned(), "10.0.0.4:17300".to_owned());
        assert!(place_of(&named, "bench-a").is_err());
        Ok(())
    }

    /// K3: the server takes a runner's word only when the operator listed it, and only at the address it was listed at.
    #[test]
    fn k3_only_a_listed_runners_word_is_taken_at_its_listed_address() -> anyhow::Result<()> {
        let scratch = tempfile::tempdir()?;
        let pki = Pki::open_scratch(&scratch.path().join("pki"))?;
        let (leaf_a, identity_a) = node(&pki, scratch.path(), "bench-a")?;
        let (leaf_d, identity_d) = node(&pki, scratch.path(), "bench-d")?;
        assert!(admit_chosen(&pki, &word("bench-a", &leaf_a, &identity_a, "10.0.0.1:17300")?, &chosen(1)).is_ok());
        assert!(admit_chosen(&pki, &word("bench-a", &leaf_a, &identity_a, "10.0.0.9:17300")?, &chosen(1)).is_err(), "another address than its listed one");
        assert!(admit_chosen(&pki, &word("bench-d", &leaf_d, &identity_d, "10.0.0.4:17300")?, &chosen(1)).is_err(), "a runner not listed");
        assert!(admit_chosen(&pki, &word("bench-a", &leaf_a, &identity_a, "10.0.0.1:17300")?, &crate::config::SlatesFleet::default()).is_err(), "no fleet declared");
        Ok(())
    }

    /// K3: the manifest a chosen runner writes is read by Slates' own reader exactly as its instance's start reads it
    /// (`slates_api::role::check_fleet`): the fleet's name, its `f`, the runner's two peers and kiln's CA as the one root.
    /// A manifest its start would refuse (this node's key not its certificate's) is refused here first, in Slates' words.
    #[test]
    fn k3_slates_reads_the_manifest_a_chosen_runner_writes_as_its_start_will() -> anyhow::Result<()> {
        let scratch = tempfile::tempdir()?;
        let pki = Pki::open_scratch(&scratch.path().join("pki"))?;
        let directory = Directory::default();
        let mut identities = BTreeMap::new();
        for (name, address) in [("bench-a", "10.0.0.1:17300"), ("bench-b", "10.0.0.2:17300"), ("bench-c", "runner-c.lan:17302")] {
            let (leaf, identity) = node(&pki, scratch.path(), name)?;
            directory.admit(admit_chosen(&pki, &word(name, &leaf, &identity, address)?, &chosen(1))?);
            identities.insert(name, identity);
        }
        let a = identities.get("bench-a").ok_or_else(|| anyhow::anyhow!("bench-a"))?.clone();
        let dir = scratch.path().join("bench-a");
        let place = place_of(&chosen(1), "bench-a").map_err(|e| anyhow::anyhow!(e))?.ok_or_else(|| anyhow::anyhow!("bench-a has no place"))?;
        let manifest = write_manifest(&dir, &a, "bench-a", &directory.members(|_| true), place.f)?;
        let selection = checked(&manifest, "bench-a")?;
        assert_eq!(selection.node, "bench-a");
        let read = slates_api::role::check_fleet(&selection).map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(read, slates_api::role::FleetCheck { name: FLEET.to_owned(), f: 1, peers: 2, enrollment_roots: 1 });
        // This node's key replaced by another's: Slates' start would refuse it, and so does the check, first.
        std::fs::copy(scratch.path().join("bench-b").join(KEY), dir.join(KEY))?;
        assert!(checked(&manifest, "bench-a").is_err());
        Ok(())
    }

    /// K4: a runner's word carries the member id its daemon serves as, once it serves, and the server's list carries it
    /// for a move to name; a newer word replaces it (a daemon started again is a new member, said again once it serves).
    #[test]
    fn k4_a_nodes_member_id_rides_its_runners_word() -> anyhow::Result<()> {
        let scratch = tempfile::tempdir()?;
        let pki = Pki::open_scratch(&scratch.path().join("pki"))?;
        let directory = Directory::default();
        let (leaf, identity) = node(&pki, scratch.path(), "bench-a")?;
        let mut said = word("bench-a", &leaf, &identity, "10.0.0.1:17300")?;
        let member_of = |d: &Directory| d.members(|_| true).members.first().and_then(|m| m.member);
        directory.admit(admit(&pki, &said)?);
        assert_eq!(member_of(&directory), None, "not serving yet");
        said.node.member = Some(42);
        directory.admit(admit(&pki, &said)?);
        assert_eq!(member_of(&directory), Some(42));
        said.node.member = None;
        directory.admit(admit(&pki, &said)?);
        assert_eq!(member_of(&directory), None, "started again: said again once it serves");
        Ok(())
    }
}
