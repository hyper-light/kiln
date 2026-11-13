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
//! - **Its link** ([`Quic`], the holder's items 2 and 3): `kiln_seams::capacity::Capacity` over
//!   Hex's QUIC wire to the keeper's `CAPACITY_PORT`, the real one of the seam's simulator.
//! - **Its node's links** ([`hold`], [`links`]): where `[capacity]` names the keeper
//!   ([`KeeperAt`]), the runner's reach node holds through it (its placement drives the holding,
//!   `reach::place`): one link after another, each said to the node as it opens, its words both
//!   ways, and its end.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context as _;
use base64::Engine as _;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use hex_core::lanes::Why as LaneWhy;
use hex_quic::capacity::CAPACITY_PORT;
use hex_quic::enroll::{Ca, CaRefused, Issued, Request, store};
use hex_quic::tls::Credentials;
use hex_quic::{CipherSuite, Config, Conn, Endpoint, PeerId, Shape};
use hyper_quic::rustls::pki_types::CertificateDer;
use kiln_seams::capacity::{Link, Refusal as LinkRefusal, ToHolder, ToKeeper};
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

/// The holder's enrollment in `dir` as it stands, for `name` (its machine's: the name kiln's CA
/// forces): its credentials where they're kept, for that name and not due for renewal, else its
/// request for kiln's CA (begun here where there was none: a key made here, never sent). A
/// certificate due for renewal is let go, its key kept, so its request is asked again; an
/// enrollment begun for another name (the machine since known by its machine id) is let go whole
/// and begun again, since kiln's CA answers for the machine's name alone and the store keeps an
/// answer for its request's name alone.
pub fn prepared(dir: &Path, name: &str) -> anyhow::Result<Result<Credentials, Request>> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    if store::request(dir, Vec::new()).is_ok_and(|request| request.name != name) {
        for file in [store::KEY, store::REQUEST, store::CHAIN, store::ROOTS] {
            let path = dir.join(file);
            if path.exists() {
                std::fs::remove_file(&path).with_context(|| format!("let go of {}", path.display()))?;
            }
        }
    }
    let chain = dir.join(store::CHAIN);
    if let Ok(pem) = std::fs::read_to_string(&chain) {
        if !crate::pki::needs_renewal(&pem)
            && let Ok(credentials) = store::load(dir)
        {
            return Ok(Ok(credentials));
        }
        std::fs::remove_file(&chain).with_context(|| format!("let go of {}", chain.display()))?;
        let roots = dir.join(store::ROOTS);
        if roots.exists() {
            std::fs::remove_file(&roots).with_context(|| format!("let go of {}", roots.display()))?;
        }
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

// ------------------------------------------------------------------------------------ its link

/// Hex's keeper on its node, reached over QUIC on its `CAPACITY_PORT` (Hex's capacity-view.md CV9;
/// the holder's items 2 and 3): `kiln_seams::capacity::Capacity`'s real one. Its endpoint holds the
/// holder's credentials ([`credentials`]) and keeps one peer, the keeper, and as many links to it as
/// a keeper keeps for a holder (one, and the one replacing it: hex-quic.md Q8). A link dials the
/// keeper by the name its certificate carries (its scheduler's, as the runner's configuration names
/// it) where no connection to it is up, refuses one that negotiated another suite than
/// `TLS13_AES_256_GCM_SHA384`, and is a connection of its own to the port, its words framed as Hex
/// frames them (`hex_core::capacity`), read and written by threads of its own (hex_quic's waits
/// block), and reset as it's let go. A dial waits on the transport's own liveness: one nothing
/// answers ends once QUIC's idle timeout passes (RFC 9000 §10.1), never on a time of kiln's.
pub struct Quic {
    endpoint: Arc<Endpoint>,
    keeper: String,
    address: SocketAddr,
    peer: Arc<Mutex<Option<PeerId>>>,
}

fn unreachable(e: impl std::fmt::Display) -> LinkRefusal {
    LinkRefusal::Unreachable(e.to_string())
}

fn ended(reset: bool, why: impl std::fmt::Display) -> LinkRefusal {
    LinkRefusal::Ended { reset, why: why.to_string() }
}

impl Quic {
    /// An endpoint of `credentials` for the keeper named `keeper` at `address`, bound on any
    /// address of its family.
    pub fn new(credentials: Credentials, keeper: &str, address: SocketAddr) -> Result<Self, LinkRefusal> {
        let any: IpAddr = if address.is_ipv4() { Ipv4Addr::UNSPECIFIED.into() } else { Ipv6Addr::UNSPECIFIED.into() };
        let config = Config { credentials, bind: SocketAddr::new(any, 0), listen: false, serves: Vec::new(), shape: Shape { connections: 1, lanes: 2 } };
        let endpoint = Endpoint::start(config).map_err(unreachable)?;
        Ok(Self { endpoint: Arc::new(endpoint), keeper: keeper.to_owned(), address, peer: Arc::default() })
    }
}

impl kiln_seams::capacity::Capacity for Quic {
    fn link(&self) -> BoxFuture<'_, Result<Box<dyn Link>, LinkRefusal>> {
        let (endpoint, keeper, address, peer) = (Arc::clone(&self.endpoint), self.keeper.clone(), self.address, Arc::clone(&self.peer));
        Box::pin(async move {
            let (tx, rx) = futures::channel::oneshot::channel();
            std::thread::Builder::new()
                .name("capacity-link".into())
                .spawn(move || {
                    let _ = tx.send(open(&endpoint, &keeper, address, &peer));
                })
                .map_err(|e| unreachable(format!("a thread to open its link: {e}")))?;
            rx.await.map_err(|_| unreachable("its link's thread ended unanswered"))?
        })
    }
}

/// A link to `keeper` at `address`: a connection to its port on the connection up, else on one
/// dialed now, its suite checked.
fn open(endpoint: &Endpoint, keeper: &str, address: SocketAddr, peer: &Mutex<Option<PeerId>>) -> Result<Box<dyn Link>, LinkRefusal> {
    if let Some(up) = *peer.lock()
        && let Ok(conn) = endpoint.open(up, CAPACITY_PORT)
    {
        return QuicLink::start(conn);
    }
    *peer.lock() = None;
    let dialed = endpoint.dial(keeper, address, None).map_err(unreachable)?;
    match endpoint.negotiated(dialed).map_err(unreachable)? {
        Some((CipherSuite::TLS13_AES_256_GCM_SHA384, _)) => {}
        other => return Err(unreachable(format!("its connection to {keeper} negotiated {other:?}, not TLS13_AES_256_GCM_SHA384"))),
    }
    let conn = endpoint.open(dialed, CAPACITY_PORT).map_err(unreachable)?;
    *peer.lock() = Some(dialed);
    QuicLink::start(conn)
}

/// One link over QUIC (module docs): its connection, the keeper's words as its reader heard them,
/// and its writer's queue.
struct QuicLink {
    conn: Arc<Conn>,
    heard: futures::lock::Mutex<futures::channel::mpsc::UnboundedReceiver<Result<ToHolder, LinkRefusal>>>,
    say: std::sync::mpsc::Sender<ToKeeper>,
}

impl QuicLink {
    fn start(conn: Conn) -> Result<Box<dyn Link>, LinkRefusal> {
        let conn = Arc::new(conn);
        let (heard_tx, heard_rx) = futures::channel::mpsc::unbounded();
        let (say_tx, say_rx) = std::sync::mpsc::channel::<ToKeeper>();
        let reader = Arc::clone(&conn);
        std::thread::Builder::new().name("capacity-hear".into()).spawn(move || hear(&reader, &heard_tx)).map_err(|e| unreachable(format!("a thread to hear its keeper: {e}")))?;
        let writer = Arc::clone(&conn);
        std::thread::Builder::new()
            .name("capacity-say".into())
            .spawn(move || {
                for word in say_rx {
                    let framed = match hex_core::capacity::encode(&word) {
                        Ok(framed) => framed,
                        Err(_) => break,
                    };
                    if writer.write_all(&framed, None).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| unreachable(format!("a thread to speak to its keeper: {e}")))?;
        Ok(Box::new(Self { conn, heard: futures::lock::Mutex::new(heard_rx), say: say_tx }))
    }
}

/// The keeper's words off `conn` as they come, each one message of Hex's framing, until the link
/// ends: why, last. Bytes that aren't a message reset the connection as breaking the protocol.
fn hear(conn: &Conn, out: &futures::channel::mpsc::UnboundedSender<Result<ToHolder, LinkRefusal>>) {
    let mut deframer = hex_core::capacity::deframer();
    let mut buf = vec![0u8; hex_core::lanes::DATA_BOUND];
    loop {
        let n = match conn.read(&mut buf, None) {
            Ok(0) => {
                let _ = out.unbounded_send(Err(ended(true, "the keeper ended the link")));
                return;
            }
            Ok(n) => n,
            Err(e) => {
                let reset = e.kind() == std::io::ErrorKind::ConnectionReset;
                let _ = out.unbounded_send(Err(ended(reset, e)));
                return;
            }
        };
        let mut rest = buf.get(..n).unwrap_or_default();
        while !rest.is_empty() {
            let (taken, message) = match deframer.take(rest) {
                Ok(took) => took,
                Err(fault) => {
                    conn.reset(LaneWhy::Protocol);
                    let _ = out.unbounded_send(Err(ended(true, format!("the keeper said bytes that aren't a message: {fault}"))));
                    return;
                }
            };
            rest = rest.get(taken..).unwrap_or_default();
            if let Some(bytes) = message {
                let word = match hex_core::capacity::decode::<ToHolder>(&bytes) {
                    Ok(word) => word,
                    Err(fault) => {
                        conn.reset(LaneWhy::Protocol);
                        let _ = out.unbounded_send(Err(ended(true, format!("the keeper said something that isn't a word: {fault}"))));
                        return;
                    }
                };
                if out.unbounded_send(Ok(word)).is_err() {
                    return;
                }
            } else if taken == 0 {
                break;
            }
        }
    }
}

impl Link for QuicLink {
    fn say(&self, msg: ToKeeper) -> BoxFuture<'_, Result<(), LinkRefusal>> {
        let out = match self.conn.gone() {
            Some(why) => Err(ended(false, format!("its connection ended ({why:?})"))),
            None => self.say.send(msg).map_err(|_| ended(false, "its writer ended")),
        };
        Box::pin(async move { out })
    }

    fn hear(&self) -> BoxFuture<'_, Result<ToHolder, LinkRefusal>> {
        Box::pin(async move {
            let mut heard = self.heard.lock().await;
            heard.next().await.unwrap_or_else(|| Err(ended(false, "the link ended")))
        })
    }
}

impl Drop for QuicLink {
    fn drop(&mut self) {
        // Its reader wakes and ends; the keeper hears its holder away (CV5).
        self.conn.reset(LaneWhy::Gone);
    }
}

// ------------------------------------------------------------------------------ its node's links

/// The machine's keeper as this runner reaches it (`[capacity]`, docs/lb.md "A machine's keeper"):
/// where its capacity port is (`host:port`) and the name its certificate carries; and what the
/// runner's holder enrolls with ([`credentials`]): its folder (`<state>/capacity`), the server,
/// kiln's root (PEM), and the machine's own name and certificate, whose name the holder's carries.
#[derive(Debug, Clone)]
pub struct KeeperAt {
    pub address: String,
    pub name: String,
    pub dir: PathBuf,
    pub server_url: String,
    pub ca_pem: String,
    pub machine_name: String,
    pub machine: Leaf,
}

/// The runner's node's link to its machine's keeper ([`KeeperAt`]): its holder's credentials
/// (enrolled with kiln's CA where they're missing or due for renewal), its endpoint for the
/// keeper's address, then [`links`] for as long as the node is there. Meanwhile the runner, its
/// keeper known and not synced, takes nothing new (CV11). What it can't do yet it does again, on
/// no clock of its own: an enrollment the server didn't answer as the server comes back (`wake`,
/// its node's word of that); an address that didn't resolve, or an endpoint not made, at a dial's
/// pace ([`links`]).
pub async fn hold(at: KeeperAt, node: crate::memory::queue::Sender<crate::reach::node::In>, wake: crate::memory::queue::Receiver<()>) {
    let quic = loop {
        let began = Instant::now();
        match endpoint(&at).await {
            Ok(quic) => break quic,
            Err(NotYet::Enrolled(e)) => {
                tracing::warn!(error = %format!("{e:#}"), keeper = %at.address, "this runner's capacity holder isn't enrolled with kiln's CA: it takes nothing new until it is, and asks again as the server comes back");
                if wake.recv().await.is_err() {
                    return;
                }
            }
            Err(NotYet::Reached(e)) => {
                tracing::warn!(error = %format!("{e:#}"), keeper = %at.address, "this runner can't reach its machine's capacity keeper: it takes nothing new until it does, and tries again at a dial's pace");
                paced(began).await;
                if node.is_closed() {
                    return;
                }
            }
        }
    };
    links(&quic, &node).await;
}

/// What [`hold`] couldn't do yet, and so what it waits for to try again.
enum NotYet {
    /// Its holder's enrollment: the server's to answer.
    Enrolled(anyhow::Error),
    /// The keeper's address resolved, or an endpoint made for it.
    Reached(anyhow::Error),
}

/// The holder's endpoint for the keeper `at` names.
async fn endpoint(at: &KeeperAt) -> Result<Quic, NotYet> {
    let creds = credentials(&at.dir, &at.server_url, &at.ca_pem, &at.machine_name, &at.machine).await.map_err(NotYet::Enrolled)?;
    let (address, name) = (at.address.clone(), at.name.clone());
    let made = crate::rt::blocking(move || -> anyhow::Result<Quic> {
        use std::net::ToSocketAddrs as _;
        let to = address.to_socket_addrs().with_context(|| format!("resolve the keeper's address {address}"))?.next().ok_or_else(|| anyhow::anyhow!("the keeper's address {address} resolves to nothing"))?;
        Quic::new(creds, &name, to).map_err(|e| anyhow::anyhow!("the holder's endpoint for {name} at {to}: {e}"))
    })
    .await;
    made.and_then(|quic| quic).map_err(NotYet::Reached)
}

/// QUIC's idle timeout before a path has a sample: three of RFC 9002's initial probe timeouts
/// (§6.2.2, `hex_quic::limits::initial_pto`), the least RFC 9000 §10.1 lets it be, which a dial
/// nothing answers waits out.
fn dial_wait() -> std::time::Duration {
    hex_quic::limits::initial_pto().saturating_mul(3)
}

/// Waits until a dial's wait ([`dial_wait`]) after `began`, if that's still to come.
async fn paced(began: Instant) {
    if let Some(next) = began.checked_add(dial_wait())
        && Instant::now() < next
    {
        crate::rt::sleep_until(next).await;
    }
}

/// Links to the keeper `capacity` reaches, one after another, for as long as `node` is there (Hex's
/// capacity-view.md CV9): each said to the node as it opens (`In::KeeperLinked`, with where its
/// words go, said on it in the order sent), each of the keeper's words passed on as it's heard
/// (`In::KeeperHeard`), and its end said (`In::KeeperEnded`: `Reset` where the keeper started again
/// or reset it, else `Died`), then the next. A dial waits on QUIC's own liveness (a handshake
/// nothing answers ends at its idle timeout, RFC 9000 §10.1); one refused sooner, or a link that
/// ended sooner, is followed by the next no sooner than that timeout after it began ([`dial_wait`]),
/// so a keeper that refuses at once is asked at the pace of one that answers nothing. No time is
/// kiln's own.
pub async fn links<C: kiln_seams::capacity::Capacity + ?Sized>(capacity: &C, node: &crate::memory::queue::Sender<crate::reach::node::In>) {
    use crate::reach::node::In;
    loop {
        let began = Instant::now();
        match capacity.link().await {
            Ok(link) => {
                let (say, said) = crate::memory::queue::unbounded::<ToKeeper>(&crate::memory::owners::CHANNELS);
                if node.send(In::KeeperLinked(say)).await.is_err() {
                    return;
                }
                let ended = carry(&*link, &said, node).await;
                // Let go of first, so the keeper hears its holder away (CV5) before the next opens.
                drop(link);
                let Some(how) = ended else { return };
                if node.send(In::KeeperEnded(how)).await.is_err() {
                    return;
                }
            }
            Err(why) => tracing::debug!(error = %why, "no link to this machine's capacity keeper now"),
        }
        paced(began).await;
    }
}

/// One link's words both ways until it ends: how it ended; none once its node is gone.
async fn carry(link: &dyn Link, said: &crate::memory::queue::Receiver<ToKeeper>, node: &crate::memory::queue::Sender<crate::reach::node::In>) -> Option<kiln_ci::place::keeper::WaitEnded> {
    use futures::FutureExt as _;
    use kiln_ci::place::keeper::WaitEnded;
    enum Next {
        Say(Option<ToKeeper>),
        Heard(Result<ToHolder, LinkRefusal>),
    }
    loop {
        // Each is cancel-safe: a word waits in its queue, and the keeper's in the link's.
        let next = futures::select_biased! {
            word = said.recv().fuse() => Next::Say(word.ok()),
            heard = link.hear().fuse() => Next::Heard(heard),
        };
        match next {
            // Refused only once the link has ended, which its hearing says next.
            Next::Say(Some(word)) => {
                let _ = link.say(word).await;
            }
            Next::Say(None) => return None,
            Next::Heard(Ok(word)) => {
                if node.send(crate::reach::node::In::KeeperHeard(word)).await.is_err() {
                    return None;
                }
            }
            Next::Heard(Err(LinkRefusal::Ended { reset: true, .. })) => return Some(WaitEnded::Reset),
            Next::Heard(Err(_)) => return Some(WaitEnded::Died),
        }
    }
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
    /// machine's name alone (whatever a request asks, HEX-ENROLL-2 above); the runner's folder
    /// keeps the answer, and its credentials verify under kiln's root. A folder begun for another
    /// name (the machine since known by its id) begins again. A proof of another key, or of a
    /// certificate this CA didn't issue, is refused, and nothing is issued.
    #[test]
    fn a_runner_proves_its_machine_and_its_holder_is_named_after_it() -> anyhow::Result<()> {
        let (dir, pki) = scratch()?;
        let machine_key = rcgen::KeyPair::generate()?.serialize_pem();
        let machine_cert = pki.sign_csr(&crate::pki::make_csr_with_key("runner-7", &machine_key)?, "runner-7", 30)?;
        let folder = dir.path().join("capacity");
        let before = match prepared(&folder, "runner-7-before-its-id")? {
            Err(request) => request,
            Ok(_) => anyhow::bail!("a new folder holds no credentials"),
        };
        let request = match prepared(&folder, "runner-7")? {
            Err(request) => request,
            Ok(_) => anyhow::bail!("a folder begun for another name holds no credentials"),
        };
        anyhow::ensure!(request.name == "runner-7" && request.csr != before.csr, "begun again for its machine's name, with a key of its own");
        let ask = EnrollHolder { cert_pem: machine_cert.clone(), proof_csr: crate::pki::make_csr_with_key("runner-7", &machine_key)?, csr: b64(&request.csr) };
        let enrolled = issue(&pki, &ask)?;
        anyhow::ensure!(enrolled.name == "runner-7", "named after the machine it proved: {}", enrolled.name);
        let credentials = finished(&folder, &enrolled.issued()?)?;
        anyhow::ensure!(credentials.chain.len() == 1, "its own certificate");
        anyhow::ensure!(prepared(&folder, "runner-7")?.is_ok(), "kept: the next start loads it");
        let other_key = rcgen::KeyPair::generate()?.serialize_pem();
        let forged = EnrollHolder { proof_csr: crate::pki::make_csr_with_key("runner-7", &other_key)?, ..ask.clone() };
        anyhow::ensure!(issue(&pki, &forged).is_err(), "a proof for another key is refused");
        let (_other_dir, other_pki) = scratch()?;
        let stranger = other_pki.sign_csr(&crate::pki::make_csr_with_key("runner-7", &machine_key)?, "runner-7", 30)?;
        anyhow::ensure!(issue(&pki, &EnrollHolder { cert_pem: stranger, ..ask }).is_err(), "a certificate another CA issued is refused");
        Ok(())
    }

    /// The holder's items 2 and 3, and the seam's contract on the real wire: kiln's link over QUIC
    /// to Hex's own keeper (`hex_quic::capacity::Server` serving a `SimView` on loopback), every
    /// certificate kiln's CA's (`PkiCa`), passes the Capacity suite, CAPACITY-1 to 7, as the
    /// simulator does: each case on a machine of its own, its two holders each with an endpoint of
    /// their own, each link asserting TLS13_AES_256_GCM_SHA384.
    #[test]
    fn kiln_s_link_to_hex_s_keeper_over_quic_passes_the_capacity_suite() -> anyhow::Result<()> {
        let (_dir, pki) = scratch()?;
        let cases = kiln_seams::capacity::cases::<Loopback>();
        let report = futures::executor::block_on(kiln_seams::conformance::run("Capacity over QUIC", &cases, || {
            let fixture = Loopback(loopback(&pki).map_err(|e| format!("{e:#}")));
            async move { fixture }
        }));
        report.into_result().map(|_| ()).map_err(|e| anyhow::anyhow!(e))
    }

    /// The node's links to its keeper ([`links`]) over the seam's simulator (Hex's own keeper): one
    /// opened and said to the node, its words said in the order sent and the keeper's answers passed
    /// on in the order said (CV9), and its end, the keeper started again, said as a reset.
    #[test]
    fn the_nodes_links_carry_words_both_ways_and_say_how_each_ended() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            use crate::reach::node::In;
            use kiln_ci::place::keeper::WaitEnded;
            let budget = hex_api::capacity::Budget { holders: 1, reservations: 4, name_bytes: 255 };
            let sim = kiln_seams::capacity::SimCapacity::new(budget, hex_api::capacity::conformance::ROOM).map_err(|e| anyhow::anyhow!("{e}"))?;
            let (node, heard) = crate::memory::queue::unbounded::<In>(&crate::memory::owners::CHANNELS);
            let keeper = sim.clone();
            let task = crate::rt::spawn(async move { links(&keeper, &node).await });
            let next = || async { heard.recv().await.map_err(|e| anyhow::anyhow!("the node heard nothing more: {e}")) };
            let In::KeeperLinked(say) = next().await? else { anyhow::bail!("its link said first") };
            let said = |word: ToKeeper| say.try_send(word).map_err(|_| anyhow::anyhow!("its link took no word"));
            said(ToKeeper::Hello { holder: "runner-1".into(), epoch: 1, scope: None })?;
            let In::KeeperHeard(ToHolder::Welcome { fence, .. }) = next().await? else { anyhow::bail!("its hello welcomed") };
            said(ToKeeper::Sync { fence, held: Vec::new() })?;
            said(ToKeeper::Reserve { fence, id: hex_api::capacity::ReservationId { epoch: 1, seq: 0 }, amount: hex_api::capacity::Amounts { cpu_milli: 1000, ..hex_api::capacity::Amounts::ZERO } })?;
            anyhow::ensure!(matches!(next().await?, In::KeeperHeard(ToHolder::Synced { .. })), "its sync answered first");
            anyhow::ensure!(matches!(next().await?, In::KeeperHeard(ToHolder::Granted { .. })), "then its reserve");
            sim.restart().map_err(|e| anyhow::anyhow!("{e}"))?;
            anyhow::ensure!(matches!(next().await?, In::KeeperEnded(WaitEnded::Reset)), "the keeper started again: its link ended, reset");
            task.abort();
            Ok(())
        })
    }

    struct Machine {
        _keeper: Endpoint,
        a: Quic,
        b: Quic,
    }

    struct Loopback(Result<Machine, String>);

    impl kiln_seams::capacity::CapacityFixture for Loopback {
        fn capacity(&self, holder: &str) -> Result<&dyn kiln_seams::capacity::Capacity, String> {
            let machine = self.0.as_ref().map_err(|e| format!("the machine didn't start: {e}"))?;
            match holder {
                "conformance-a" => Ok(&machine.a),
                "conformance-b" => Ok(&machine.b),
                other => Err(format!("no holder {other} here")),
            }
        }
    }

    fn enrolled(ca: &PkiCa<'_>, name: &str) -> anyhow::Result<Credentials> {
        ca.admit(name, name.as_bytes());
        hex_quic::enroll::enroll(ca, name, name.as_bytes().to_vec()).map_err(|e| anyhow::anyhow!("enroll {name}: {e}"))
    }

    /// A machine whose keeper serves `conformance-a` and `conformance-b` on loopback.
    fn loopback(pki: &Pki) -> anyhow::Result<Machine> {
        let ca = PkiCa::new(pki);
        let keeper = Endpoint::start(Config {
            credentials: enrolled(&ca, "keeper.capacity.test")?,
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            listen: true,
            serves: vec![CAPACITY_PORT],
            shape: Shape { connections: 4, lanes: 8 },
        })
        .map_err(|e| anyhow::anyhow!("the keeper's endpoint: {e}"))?;
        let budget = hex_api::capacity::Budget { holders: 2, reservations: 8, name_bytes: 255 };
        let view = hex_api::capacity::sim::SimView::new(budget, hex_api::capacity::conformance::ROOM).map_err(|e| anyhow::anyhow!("{e}"))?;
        let server = hex_quic::capacity::Server::new(view, &["conformance-a".to_owned(), "conformance-b".to_owned()], Arc::new(|_: &str| {}));
        let acceptor = keeper.acceptor(CAPACITY_PORT);
        std::thread::Builder::new().name("keeper".into()).spawn(move || server.serve(acceptor))?;
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, keeper.local_addr().port()));
        let a = Quic::new(enrolled(&ca, "conformance-a")?, "keeper.capacity.test", address).map_err(|e| anyhow::anyhow!("{e}"))?;
        let b = Quic::new(enrolled(&ca, "conformance-b")?, "keeper.capacity.test", address).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Machine { _keeper: keeper, a, b })
    }
}
