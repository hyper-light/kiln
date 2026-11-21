//! A run's credential leases as Hex asks them for the Shards image VMs it boots (Shards' D242; kiln
//! plan 01a1d2b88be57304b972761e2a5c6781 step 6), answered from the run-lease [`Broker`]: kiln's
//! [`Lessors`] are Hex's lease seam (`hex_api::lease`), through which its image class leases a run's
//! credentials (`ImageClass::with_lessors`). They make a lessor for each run as it boots, which
//! leases a credential's value to that run alone, just in time and by reference, gives each lease
//! back as the run lets go of it, and ends the run's leases with the run.
//!
//! - **The run's identity** is the SHA-256 of the run's own key where it binds an agent-control
//!   endpoint (D252: told with each ask, the same for all of them), else a name made for the run as
//!   its lessor is: 256 random bits, the form the broker keeps a key's SHA-256 in, which names no
//!   other run. It is the run's channel, as D242 names a run without a key: it proves nothing a
//!   channel doesn't, and nothing else holds it. An ask under a second key is refused.
//! - **What it answers** is D242's, in Hex's types (`hex_api::lease::Lease`, `LeaseRefused`): the
//!   value, under the store's name for its lease, and how long it lives (none: until its run ends),
//!   or a refusal of D242's kinds, in the broker's words, which never hold a value.
//! - **A run's sign-ins** (D242, step 8's third slice): a capture's tokens kept by the broker, its
//!   CLI given placeholders; a refresh made at the sign-in's provider by the broker alone
//!   ([`super::signin`]), its CLI answered placeholders; a revocation in the microVM said, the
//!   sign-in kept.
//! - **The run's end** is its lessor let go of. Hex's image class makes it as the run boots and the
//!   run's host holds it for as long as the run can ask (Shards' `RunLessor`), so once no one holds
//!   it, nothing asks through it again: the run is kept ended in the broker's ledger, so nothing more
//!   is leased under its identity, and every lease it holds still is revoked. Its host gives each
//!   lease back as the run ends; the end revokes what a give-back couldn't.
//! - Hex asks on threads of its own, off kiln's runtime, so each call blocks on the broker there
//!   ([`crate::rt::block_on`]); a lessor let go of on one of the runtime's shards, where nothing may
//!   block, ends its run on a task of its own.
//! - **Every runner leases from the one broker** its server opens over kiln's credential store
//!   ([`open`], kept in `Hub::lessors`), so a sign-in is refreshed one at a time across every run
//!   (the broker's `Turn`). The server's own runner asks it in its process ([`Lessors`]); every
//!   other runner puts each of its runs' asks ([`Ask`]) to its server over its link
//!   ([`Remote`]: `crate::reach::Question::Lessor`), which answers them from the broker as the
//!   server's own runner's are answered ([`answer`]), values hex on the link's JSON. A runner's
//!   runs lease their own people's sign-ins alone: the server answers a lease, a capture or a
//!   refresh only of a sign-in of a person one of that runner's isolated agents of its harness
//!   acts for, and no other credential.
//! - **What crosses a link is wiped once used**, as Shards' injector and its HTTP/2 wipe what they
//!   hold of a value (zeroize; Hex's C7 for its own wire): each ask's and answer's JSON is made the
//!   size it comes to ([`Words`]: a buffer grown by copying leaves what it held behind unwiped), and
//!   every copy of it kiln holds on its way, its frame in the net thread's outbox
//!   (`crate::net::Out::Wiped`), the words a frame is read into and their tree on the other side
//!   (`crate::wire::payload`, `crate::reach::node`), is wiped as it's let go of. The transport's
//!   own copies (hyper-transport's streams and packets) are its own.
//! - **A runner hears whether its server answers** on each connection ([`Heard`]): its caps, its
//!   Welcome, then its named features. A launch from before that word, a session restored as its
//!   runner started among them, waits for it ([`answering`]) and is then granted its sign-in, or
//!   not, as any launch is: Hex grants a sign-in only as a box is made (its C8), so one launched
//!   without it would keep none until it started again.
//!
//! Each runner's sandbox hands its lessors to Hex's image runtime and carries each launch's
//! credential grants and sign-in into its image's, by reference (`sandbox::hexvm`,
//! `hex_node::images::ImageRuntime::leasing`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use hex_api::BoxId;
use hex_api::lease::{Lease, LeaseRefused, Lessor, SignInRefused, SignInTokens, Value};
use kiln_seams::Clock;
use kiln_seams::secrets::{Refusal, SecretValue};
use parking_lot::Mutex;

use super::broker::{Broker, Refused, Tokens, Unrefreshed};
use super::signin::{self, Provider};

/// The broker's refusal as D242 kinds it: a run ended or moved as such, a credential the store
/// holds none of (or can't name) as denied, and every other, a value past its own end among them
/// (its sign-in refreshes it), as unavailable for now.
fn refused(why: Refused) -> LeaseRefused {
    match why {
        Refused::RunEnded(_) => LeaseRefused::Ended,
        Refused::KeyReplaced { asked, current } => LeaseRefused::KeyReplaced { asked, current: Some(current) },
        Refused::Store(e @ (Refusal::NotFound(_) | Refusal::Invalid { .. })) => LeaseRefused::Denied(e.to_string()),
        other => LeaseRefused::Unavailable(other.to_string()),
    }
}

/// A sign-in refusal of the broker's, as D242 kinds it: a run that ended as such; a sign-in nothing
/// was captured for, and a refresh its provider refused, as denied, so its CLI signs in anew; every
/// other as unavailable for now.
fn sign_in_refused(why: Refused) -> SignInRefused {
    match why {
        Refused::RunEnded(_) => SignInRefused::Ended,
        Refused::NoSignIn(_) | Refused::Upstream { why: Unrefreshed::Refused(_), .. } => SignInRefused::Denied(why.to_string()),
        other => SignInRefused::Unavailable(other.to_string()),
    }
}

/// A sign-in whose name names no provider the broker refreshes at ([`Provider::of`]): denied.
fn unprovided(sign_in: &str) -> SignInRefused {
    SignInRefused::Denied(format!("sign-in {sign_in:?} names no provider kiln refreshes at (claude/… or codex/…)"))
}

/// The lessors of the server's own runner's microVMs, over the run-lease broker [`Broker::open`] opens on kiln's
/// credential store (`KilnSecrets`) with its ledger in the server's store, on the process's clock: what its runner's
/// sandbox leases its VMs' credentials through (`sandbox::Leasing`), and what answers every other runner's runs
/// ([`answer`]). Opened once for the server (`Hub::lessors`): opening it revokes what a restart left leased.
pub async fn open(hub: &Arc<crate::hub::Hub>) -> anyhow::Result<Lessors> {
    if let Some(opened) = hub.lessors.get() {
        return Ok(opened.shared());
    }
    let secrets: Arc<dyn kiln_seams::secrets::Secrets> = Arc::new(super::seam::KilnSecrets::new(Arc::clone(hub), crate::rt::clock()));
    let broker = Broker::open(secrets, hub.store.clone(), crate::rt::clock()).await?;
    OPENED_HERE.store(true, Ordering::Release);
    MOVED.notify();
    Ok(hub.lessors.get_or_init(|| Lessors::new(Arc::new(broker), crate::rt::clock())).shared())
}

/// Whether this process opened its server's broker ([`open`]): its own runner's runs ask it here.
static OPENED_HERE: AtomicBool = AtomicBool::new(false);

/// What this runner has heard of whether its server answers its runs' asks over its link ([`Heard`]), moved by its
/// server's word ([`heard`]). A [`Remote`]'s runs ask it only while it answers.
static HEARD: LazyLock<Mutex<Heard>> = LazyLock::new(|| Mutex::new(Heard::Nothing));

/// Told as [`HEARD`] moves, or this process opens its server's broker: what waits for either looks again
/// ([`answering`]).
static MOVED: LazyLock<crate::rt::Changed> = LazyLock::new(crate::rt::Changed::new);

/// What a runner has heard of whether its server answers its runs' leases and sign-ins over its link
/// ([`crate::proto::feature::LEASES`]), as a server says what it speaks on each connection (docs/wire.md, "Named
/// features"): its caps before its Welcome, and its named features after them where its caps say it names any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heard {
    /// Nothing yet: no connection of this process's has said.
    Nothing,
    /// This connection's caps say its server names features (`CAP_FEATURES`): their names say whether it answers.
    Naming,
    /// Its server names `secrets.leases`: it answers them.
    Answers,
    /// It doesn't: its caps name no features, its features don't name it, or its first connection's Welcome came with
    /// no caps before it (a kiln from before them).
    Refuses,
}

/// A server's word on a connection, as [`Heard`] takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Word {
    /// Its caps (`ServerMsg::Caps`): whether they say it names features.
    Caps { names: bool },
    /// Its Welcome.
    Welcome,
    /// Its named features (`ServerMsg::Features`): whether they name `secrets.leases`.
    Features { leases: bool },
    /// Its connection went.
    Gone,
}

impl Heard {
    /// What it is once `word` is heard. Answered or refused is kept across connections until a later one's word moves it,
    /// so a run's asks go on as its link comes back; caps that name features, with their names not yet come, are the
    /// connection's, and go with it.
    pub fn then(self, word: Word) -> Heard {
        match (self, word) {
            (_, Word::Caps { names: false }) => Heard::Refuses,
            (_, Word::Features { leases: true }) => Heard::Answers,
            (_, Word::Features { leases: false }) | (Heard::Nothing, Word::Welcome) => Heard::Refuses,
            (Heard::Nothing, Word::Caps { names: true }) => Heard::Naming,
            (Heard::Naming, Word::Gone) => Heard::Nothing,
            (heard, Word::Caps { names: true } | Word::Welcome | Word::Gone) => heard,
        }
    }

    /// Whether its server has said: answered or refused.
    fn said(self) -> bool {
        matches!(self, Heard::Answers | Heard::Refuses)
    }
}

/// This runner's server said `word` on its connection (`crate::agent`'s loop): what it has heard moves as
/// [`Heard::then`] says, and what waits for it looks again.
pub fn heard(word: Word) {
    let moved = {
        let mut heard = HEARD.lock();
        let next = heard.then(word);
        std::mem::replace(&mut *heard, next) != next
    };
    if moved {
        MOVED.notify();
    }
}

/// Whether this process's runs are answered their leases and sign-ins: its server's broker is open here (the server's
/// own runner), or this runner's server said it answers them over its link ([`Heard::Answers`]). Known at once on the
/// server's own runner and once its server has said, else when it says ([`heard`]): an isolated launch's sign-in waits
/// for it (`sandbox::isolated`), so one launched before its server's word (a session restored as its runner started)
/// is granted its sign-in as any launch is, rather than signing in itself in its microVM. Waited for as it's told,
/// never polled, for as long as its server takes to say: with no server answering, none of its run's leases could be.
pub async fn answering() -> bool {
    loop {
        let moved = MOVED.listen();
        if OPENED_HERE.load(Ordering::Acquire) {
            return true;
        }
        let heard = *HEARD.lock();
        if heard.said() {
            return heard == Heard::Answers;
        }
        moved.await;
    }
}

/// The lessors of a machine's runs: one for each run, over the broker.
pub struct Lessors {
    broker: Arc<Broker>,
    clock: Arc<dyn Clock>,
}

impl Lessors {
    /// Over `broker`, with the clock it reads its values' ends on.
    pub fn new(broker: Arc<Broker>, clock: Arc<dyn Clock>) -> Lessors {
        Lessors { broker, clock }
    }

    /// Lessors over the same broker, on the same clock.
    fn shared(&self) -> Lessors {
        Lessors { broker: Arc::clone(&self.broker), clock: Arc::clone(&self.clock) }
    }
}

impl hex_api::lease::Lessors for Lessors {
    /// The lessor of box `id`'s run, as the run boots (its id for words alone), with a channel of its
    /// own made now, asking the broker in this process.
    fn lessor(&self, id: &BoxId) -> Result<Arc<dyn Lessor>, hex_api::Refusal> {
        RunLessor::made(To::Broker { broker: Arc::clone(&self.broker), clock: Arc::clone(&self.clock) }, id)
    }
}

/// The lessors of a runner other than the server's own: each of its runs' asks put to the broker its server opened,
/// over this runner's link to it (`crate::reach::node::Handle::lessor`), and answered there ([`answer`]).
pub struct Remote(crate::reach::node::Handle);

impl Remote {
    /// Over this runner's reach node, its link to its server.
    pub fn new(node: crate::reach::node::Handle) -> Remote {
        Remote(node)
    }
}

impl hex_api::lease::Lessors for Remote {
    /// The lessor of box `id`'s run, as the run boots, with a channel of its own made now, asking the server.
    fn lessor(&self, id: &BoxId) -> Result<Arc<dyn Lessor>, hex_api::Refusal> {
        RunLessor::made(To::Server(self.0.clone()), id)
    }
}

/// A value or a token as an ask or an answer carries it: in Hex's holder, wiped as it drops and never printed; hex on
/// a link's JSON.
pub struct Carried(Value);

impl Carried {
    pub fn new(value: Value) -> Carried {
        Carried(value)
    }
}

impl Clone for Carried {
    fn clone(&self) -> Carried {
        Carried(Value::new(self.0.expose().to_vec()))
    }
}

impl PartialEq for Carried {
    fn eq(&self, other: &Carried) -> bool {
        self.0 == other.0
    }
}

impl std::fmt::Debug for Carried {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Carried({} bytes)", self.0.expose().len())
    }
}

/// Written as hex, through a buffer made its length and wiped once written: none of it left behind.
impl serde::Serialize for Carried {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error as _;
        let value = self.0.expose();
        let Some(len) = value.len().checked_mul(2) else { return Err(S::Error::custom("a value too long to carry as hex")) };
        let mut text = vec![0u8; len];
        let said = match hex::encode_to_slice(value, &mut text) {
            Ok(()) => std::str::from_utf8(&text).map_err(S::Error::custom).and_then(|t| s.serialize_str(t)),
            Err(e) => Err(S::Error::custom(e)),
        };
        hex_api::lease::wipe(&mut text);
        said
    }
}

/// Read from hex into a buffer made its length (a decode into a growing one would leave copies behind), the hex wiped.
impl<'de> serde::Deserialize<'de> for Carried {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Carried, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(d)?;
        let mut value = vec![0u8; text.len() / 2];
        let decoded = hex::decode_to_slice(&text, &mut value).map_err(<D::Error as serde::de::Error>::custom);
        let mut text = text.into_bytes();
        hex_api::lease::wipe(&mut text);
        match decoded {
            Ok(()) => Ok(Carried(Value::new(value))),
            Err(e) => {
                hex_api::lease::wipe(&mut value);
                Err(e)
            }
        }
    }
}

/// JSON that may carry a credential's value or a sign-in's tokens, as a run's ask or its answer crosses a runner's link
/// ([`RunLessor`], [`answer`]): written into a buffer made the size it comes to, as serde_json's own grows by copying
/// itself and leaves what it held behind unwiped (zeroize can't reach a buffer already let go of); wiped as it's let go
/// of; never printed (its `Debug` says its length alone).
pub struct Words(String);

impl Words {
    /// `value` as JSON.
    pub fn of<T: serde::Serialize>(value: &T) -> anyhow::Result<Words> {
        let mut counted = Counted(0);
        serde_json::to_writer(&mut counted, value)?;
        let mut out = Vec::with_capacity(counted.0);
        if let Err(e) = serde_json::to_writer(&mut out, value) {
            hex_api::lease::wipe(&mut out);
            return Err(e.into());
        }
        match String::from_utf8(out) {
            Ok(text) => Ok(Words(text)),
            Err(e) => {
                let mut bytes = e.into_bytes();
                hex_api::lease::wipe(&mut bytes);
                anyhow::bail!("its JSON wasn't UTF-8")
            }
        }
    }

    /// Words made elsewhere, the text of a link's answer (`crate::reach::node`), taken to be wiped as they're let go of.
    pub fn taken(text: String) -> Words {
        Words(text)
    }

    pub fn text(&self) -> &str {
        &self.0
    }

    /// Its text handed on whole, nothing left here to wipe: into a message whose holder wipes it once its frame is made
    /// (the runner's uplink, `crate::hub::Hub::send_wiped`).
    pub fn into_text(mut self) -> String {
        std::mem::take(&mut self.0)
    }
}

impl Drop for Words {
    fn drop(&mut self) {
        wipe_text(&mut self.0);
    }
}

impl std::fmt::Debug for Words {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Words({} bytes)", self.0.len())
    }
}

/// What a run's ask of the broker begins with as kiln writes it ([`Words::of`] of a `crate::reach::Question::Lessor`,
/// whose tag serde writes first): how its server knows one before reading it (`crate::wire::payload`), so none of its
/// words is copied to be looked at, or kept beside it as it came.
const ASKED: &str = r#"{"op":"lessor","#;

/// Whether `json` is a run's ask of the broker as kiln writes one ([`ASKED`]).
pub fn asked(json: &str) -> bool {
    json.starts_with(ASKED)
}

/// What a writer is given, counted: the length [`Words::of`] makes its buffer.
struct Counted(usize);

impl std::io::Write for Counted {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `text` wiped, its whole allocation (hex_api's `wipe`: zeroize's writes, which the compiler keeps), and left empty:
/// words that may have carried a credential's value or a sign-in's tokens across a runner's link, as they're let go of.
pub fn wipe_text(text: &mut String) {
    let mut bytes = std::mem::take(text).into_bytes();
    hex_api::lease::wipe(&mut bytes);
}

/// Each string of `value`, its keys' too, wiped as it's let go of ([`wipe_text`]): a tree read from words that may have
/// carried a credential's value or a sign-in's tokens. As deep as the JSON it was read from, which serde_json reads 128
/// levels deep at the most.
pub fn wipe_value(value: serde_json::Value) {
    match value {
        serde_json::Value::String(mut text) => wipe_text(&mut text),
        serde_json::Value::Array(items) => items.into_iter().for_each(wipe_value),
        serde_json::Value::Object(fields) => {
            for (mut key, value) in fields {
                wipe_text(&mut key);
                wipe_value(value);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

/// A run's ask of the broker, as its lessor asks it (`hex_api::lease::Lessor`): by the identity its leases are issued
/// to, the run's key's SHA-256 or its channel's ([`RunLessor`]). What a runner other than the server's own puts to its
/// server over its link (`crate::reach::Question::Lessor`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "ask", rename_all = "snake_case")]
pub enum Ask {
    /// Credential `reference`'s value, leased to the run.
    Lease { identity: String, reference: String },
    /// Lease `lease` given back.
    Given { identity: String, lease: String },
    /// The run has ended, under each of the identities it had.
    Ended { identities: Vec<String> },
    /// Sign-in `sign_in`'s tokens, as its run's injector took them, kept.
    Capture {
        sign_in: String,
        access: Carried,
        #[serde(default)]
        refresh: Option<Carried>,
        #[serde(default)]
        expires_in: Option<u64>,
        #[serde(default)]
        scope: Option<String>,
    },
    /// Sign-in `sign_in` refreshed at its provider.
    Refresh { sign_in: String },
}

/// A refusal on a link, by D242's kinds (`LeaseRefused`, `SignInRefused`), each in its words.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Said {
    Denied { why: String },
    Unavailable { why: String },
    Ended,
    KeyReplaced { asked: String, current: Option<String> },
    Lapsed,
    Unanswered,
}

impl From<LeaseRefused> for Said {
    fn from(why: LeaseRefused) -> Said {
        match why {
            LeaseRefused::Denied(why) => Said::Denied { why },
            LeaseRefused::Unavailable(why) => Said::Unavailable { why },
            LeaseRefused::Ended => Said::Ended,
            LeaseRefused::KeyReplaced { asked, current } => Said::KeyReplaced { asked, current },
            LeaseRefused::Lapsed => Said::Lapsed,
        }
    }
}

impl From<SignInRefused> for Said {
    fn from(why: SignInRefused) -> Said {
        match why {
            SignInRefused::Denied(why) => Said::Denied { why },
            SignInRefused::Unavailable(why) => Said::Unavailable { why },
            SignInRefused::Ended => Said::Ended,
            SignInRefused::Unanswered => Said::Unanswered,
        }
    }
}

impl Said {
    /// As a lease's refusal: kind for kind, an orchestrator answering no sign-in's unavailable.
    fn lease(self) -> LeaseRefused {
        match self {
            Said::Denied { why } => LeaseRefused::Denied(why),
            Said::Unavailable { why } => LeaseRefused::Unavailable(why),
            Said::Ended => LeaseRefused::Ended,
            Said::KeyReplaced { asked, current } => LeaseRefused::KeyReplaced { asked, current },
            Said::Lapsed => LeaseRefused::Lapsed,
            Said::Unanswered => LeaseRefused::Unavailable("its server answers no lease".into()),
        }
    }

    /// As a sign-in's refusal: kind for kind, a lease's own kinds (a replaced key, a lapse) unavailable.
    fn sign_in(self) -> SignInRefused {
        match self {
            Said::Denied { why } => SignInRefused::Denied(why),
            Said::Unavailable { why } => SignInRefused::Unavailable(why),
            Said::Ended => SignInRefused::Ended,
            Said::Unanswered => SignInRefused::Unanswered,
            other @ (Said::KeyReplaced { .. } | Said::Lapsed) => SignInRefused::Unavailable(other.lease().to_string()),
        }
    }
}

/// The broker's answer to an [`Ask`].
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "as", rename_all = "snake_case")]
pub enum Answer {
    /// Its value and its lease, living `life_ms` from now (none: until its run ends).
    Leased {
        lease: String,
        value: Carried,
        life_ms: Option<u64>,
    },
    Given,
    /// How many leases the run's end revoked that its host hadn't given back.
    Ended {
        revoked: usize,
    },
    Captured {
        captured: hex_api::Captured,
    },
    /// The token response its CLI is to have, its tokens placeholders ([`signin::answer`]).
    Refreshed {
        response: Carried,
    },
    Refused {
        said: Said,
    },
}

impl Answer {
    /// Its kind, for words: never what it carries.
    fn kind(&self) -> &'static str {
        match self {
            Answer::Leased { .. } => "leased",
            Answer::Given => "given",
            Answer::Ended { .. } => "ended",
            Answer::Captured { .. } => "captured",
            Answer::Refreshed { .. } => "refreshed",
            Answer::Refused { .. } => "refused",
        }
    }
}

/// `ask` answered by `broker`, on `clock`: a value leased (its life from now, none of it left where its end has
/// passed, which its run then has lapsed), given back, a run's end kept and its leases revoked, a sign-in captured
/// (one whose name names no provider refused, nothing kept) or refreshed by the broker alone.
async fn answered(broker: Arc<Broker>, clock: Arc<dyn Clock>, ask: Ask) -> Answer {
    let no = |said: Said| Answer::Refused { said };
    match ask {
        Ask::Lease { identity, reference } => match broker.lease(&identity, &reference).await {
            Ok(leased) => {
                let life_ms = leased.until_ms.map(|end| u64::try_from(end.saturating_sub(clock.unix_ms())).unwrap_or(0));
                // Handed on in Hex's holder, which wipes it as it drops, as the broker's own does.
                Answer::Leased { lease: leased.lease, value: Carried(Value::new(leased.value.expose().to_vec())), life_ms }
            }
            Err(why) => no(refused(why).into()),
        },
        Ask::Given { identity, lease } => match broker.release_lease(&identity, &lease).await {
            Ok(_) => Answer::Given,
            Err(e) => no(Said::Unavailable { why: e.to_string() }),
        },
        Ask::Ended { identities } => {
            let mut revoked = 0usize;
            for name in identities {
                match broker.release(&name).await {
                    Ok(n) => revoked = revoked.saturating_add(n),
                    Err(e) => return no(Said::Unavailable { why: e.to_string() }),
                }
            }
            Answer::Ended { revoked }
        }
        Ask::Capture { sign_in, access, refresh, expires_in, scope } => {
            if Provider::of(&sign_in).is_none() {
                return no(unprovided(&sign_in).into());
            }
            let tokens = Tokens { access: SecretValue::new(access.0.into_bytes()), refresh: refresh.map(|r| SecretValue::new(r.0.into_bytes())), expires_in_s: expires_in, scope };
            match broker.capture(&sign_in, tokens).await {
                Ok(c) => Answer::Captured { captured: hex_api::Captured { access: c.access.reference, access_placeholder: c.access.placeholder, refresh_placeholder: c.refresh.map(|r| r.placeholder) } },
                Err(why) => no(sign_in_refused(why).into()),
            }
        }
        Ask::Refresh { sign_in } => {
            let Some(provider) = Provider::of(&sign_in) else { return no(unprovided(&sign_in).into()) };
            match broker.refresh(&sign_in, provider.upstream()).await {
                Ok(c) => match signin::answer(&c, clock.unix_ms()) {
                    Ok(response) => Answer::Refreshed { response: Carried(Value::new(response)) },
                    Err(e) => no(Said::Unavailable { why: format!("its answer: {e}") }),
                },
                Err(why) => no(sign_in_refused(why).into()),
            }
        }
    }
}

/// The server's answer to `ask`, from a run of runner `runner` other than its own (`crate::reach::Question::Lessor`;
/// [`Remote`]), from the broker it opened ([`open`]), as its own runner's runs are answered: JSON of an [`Answer`], in
/// words wiped as they're let go of ([`Words`]; sent by `crate::hub::Hub::send_wiped`). Denied, typed, where the runner
/// may not ask it ([`unheld`]); unavailable where the broker isn't open.
pub async fn answer(hub: &crate::hub::Hub, runner: &str, ask: Ask) -> Words {
    let said = match (unheld(hub, runner, &ask), hub.lessors.get()) {
        (Some(why), _) => Answer::Refused { said: Said::Denied { why } },
        (None, None) => Answer::Refused { said: Said::Unavailable { why: "its server's run-lease broker isn't open".into() } },
        (None, Some(lessors)) => answered(Arc::clone(&lessors.broker), Arc::clone(&lessors.clock), ask).await,
    };
    Words::of(&said).unwrap_or_else(|e| Words(serde_json::json!({ "error": format!("its answer: {e:#}") }).to_string()))
}

/// Why runner `runner`'s run may not ask `ask`, where it may not: a runner kiln doesn't know, or a job's, which holds no
/// isolated agent; a lease of anything but a sign-in's access token (the one credential kiln grants a run); and a
/// lease, a capture or a refresh of a sign-in no isolated agent on that runner is granted ([`holds_sign_in`]). A
/// give-back and a run's end are its identity's, which only its run knows.
fn unheld(hub: &crate::hub::Hub, runner: &str, ask: &Ask) -> Option<String> {
    if hub.agents.load().get(runner).is_none_or(|a| a.ephemeral) {
        return Some("a job's runner holds no isolated agent to lease for".into());
    }
    let name = match ask {
        Ask::Lease { reference, .. } => match reference.strip_prefix("sign-ins/").and_then(|r| r.strip_suffix("/access")) {
            Some(name) => name,
            None => return Some(format!("{reference:?} is no sign-in's access token, the one credential a runner's run is leased")),
        },
        Ask::Capture { sign_in, .. } | Ask::Refresh { sign_in } => sign_in.as_str(),
        Ask::Given { .. } | Ask::Ended { .. } => return None,
    };
    (!holds_sign_in(hub, runner, name)).then(|| format!("{} holds no isolated agent granted sign-in {name}", hub.shown(runner)))
}

/// Whether runner `runner` holds an isolated agent granted sign-in `name`: one of its person's, of its provider's
/// harness, as its launch names it (`Provider::sign_in`).
fn holds_sign_in(hub: &crate::hub::Hub, runner: &str, name: &str) -> bool {
    hub.harness.list_shared().iter().any(|s| {
        let granted = s.isolation.as_ref().and_then(|i| i.person.as_deref()).zip(Provider::of_harness(&s.kind)).and_then(|(person, p)| p.sign_in(person));
        granted.as_deref() == Some(name) && hub.harness.located_on(&s.id, runner)
    })
}

/// Where a run's asks are answered: the broker in this process (the server's own runner's runs), or the server that
/// opened it, over this runner's link to it (every other runner's).
enum To {
    Broker { broker: Arc<Broker>, clock: Arc<dyn Clock> },
    Server(crate::reach::node::Handle),
}

/// One run's lessor: its leases issued to the run's own key where it has one, else to its channel;
/// its run ended as it is let go of.
struct RunLessor {
    to: To,
    run: String,
    channel: String,
    /// The SHA-256 of the run's key, once an ask has told it (D252), in the broker's form.
    key: Mutex<Option<String>>,
}

impl RunLessor {
    /// Box `id`'s run's lessor (its id for words alone), its asks answered `to`, with a channel of its own made now.
    fn made(to: To, id: &BoxId) -> Result<Arc<dyn Lessor>, hex_api::Refusal> {
        let channel = super::kiln::random::<32>().map_err(|e| hex_api::Refusal::Os { op: "making a run's lease channel", why: format!("{e:#}") })?;
        Ok(Arc::new(RunLessor { to, run: id.as_str().to_owned(), channel: hex::encode(channel), key: Mutex::new(None) }))
    }

    /// The identity the run's leases are issued to: its key where `key` tells one (the first ask's,
    /// kept, and every later ask's the same), else its channel.
    fn identity(&self, key: Option<[u8; 32]>) -> Result<String, LeaseRefused> {
        let Some(key) = key else {
            return Ok(self.key.lock().clone().unwrap_or_else(|| self.channel.clone()));
        };
        let told = hex::encode(key);
        let mut kept = self.key.lock();
        match kept.as_deref() {
            None => {
                *kept = Some(told.clone());
                Ok(told)
            }
            Some(k) if k == told => Ok(told),
            Some(k) => Err(LeaseRefused::Unavailable(format!("run {}: asked under key {told}, where it was told key {k}: a run binds one key (D252)", self.run))),
        }
    }

    /// `ask`'s answer, blocking on it off kiln's runtime: from the broker in this process, or from the server over this
    /// runner's link. Why not, in words, where it couldn't be asked or answered.
    fn ask(&self, ask: Ask) -> Result<Answer, String> {
        match &self.to {
            To::Broker { broker, clock } => {
                let (broker, clock) = (Arc::clone(broker), Arc::clone(clock));
                crate::rt::block_on(async move { Ok(answered(broker, clock, ask).await) }).map_err(|e| format!("run {}: the broker: {e:#}", self.run))
            }
            To::Server(_) if *HEARD.lock() != Heard::Answers => Err(format!("run {}: its server doesn't answer runs' leases (it names no `{}`): a kiln from before", self.run, crate::proto::feature::LEASES)),
            // Its ask and the answer in words wiped as they're let go of, here and on their way ([`Words`]).
            To::Server(node) => {
                let json = Words::of(&crate::reach::Question::Lessor { ask }).map_err(|e| format!("run {}: its ask: {e:#}", self.run))?;
                let node = node.clone();
                let said = crate::rt::block_on(async move { node.lessor(json).await }).map_err(|e| format!("run {}: its server: {e:#}", self.run))?;
                serde_json::from_str::<Answer>(said.text()).map_err(|e| format!("run {}: its server gave no lessor's answer (line {}, column {})", self.run, e.line(), e.column()))
            }
        }
    }
}

impl Lessor for RunLessor {
    /// Credential `reference`'s value, leased to the run now, or why not, typed.
    fn lease(&self, reference: &str, key: Option<[u8; 32]>) -> Result<Lease, LeaseRefused> {
        let identity = self.identity(key)?;
        match self.ask(Ask::Lease { identity, reference: reference.to_owned() }).map_err(LeaseRefused::Unavailable)? {
            Answer::Leased { lease, value, life_ms } => Ok(Lease { lease, value: value.0, life: life_ms.map(Duration::from_millis) }),
            Answer::Refused { said } => Err(said.lease()),
            other => Err(LeaseRefused::Unavailable(format!("run {}: a lease answered {}", self.run, other.kind()))),
        }
    }

    /// Lease `lease` given back: the run let go of it, or ended. A give-back the broker couldn't
    /// make is said, never dropped unsaid: the run's end revokes what it holds still.
    fn release(&self, lease: &str) {
        let identity = self.key.lock().clone().unwrap_or_else(|| self.channel.clone());
        match self.ask(Ask::Given { identity, lease: lease.to_owned() }) {
            Ok(Answer::Given) => {}
            Ok(Answer::Refused { said }) => tracing::error!(run = %self.run, lease, "a run's credential lease wasn't given back: {}", said.lease()),
            Ok(other) => tracing::error!(run = %self.run, lease, "a run's credential lease wasn't given back: its give-back answered {}", other.kind()),
            Err(e) => tracing::error!(run = %self.run, lease, "a run's credential lease wasn't given back: {e}"),
        }
    }

    /// Sign-in `sign_in`'s tokens, as the run's injector took them from its CLI's token response,
    /// kept by the broker (D242, step 8's third slice): the reference its access token is leased by
    /// from now, and the placeholders its CLI holds in their places. Its ID token, inert, is not
    /// kept. A sign-in whose name names no provider is refused, nothing kept.
    fn capture(&self, sign_in: &str, tokens: SignInTokens, key: Option<[u8; 32]>) -> Result<hex_api::Captured, SignInRefused> {
        self.identity(key).map_err(|why| SignInRefused::Unavailable(why.to_string()))?;
        if Provider::of(sign_in).is_none() {
            return Err(unprovided(sign_in));
        }
        let SignInTokens { access, refresh, expires_in, scope, .. } = tokens;
        let ask = Ask::Capture { sign_in: sign_in.to_owned(), access: Carried(access), refresh: refresh.map(Carried), expires_in, scope };
        match self.ask(ask).map_err(SignInRefused::Unavailable)? {
            Answer::Captured { captured } => Ok(captured),
            Answer::Refused { said } => Err(said.sign_in()),
            other => Err(SignInRefused::Unavailable(format!("run {}: a capture answered {}", self.run, other.kind()))),
        }
    }

    /// Sign-in `sign_in` refreshed at its provider by the broker, which alone calls it, one refresh
    /// of a sign-in at a time across every run (D242, step 8's third slice): the token response its
    /// CLI is to have, its tokens placeholders ([`signin::answer`]).
    fn refresh(&self, sign_in: &str, key: Option<[u8; 32]>) -> Result<Value, SignInRefused> {
        self.identity(key).map_err(|why| SignInRefused::Unavailable(why.to_string()))?;
        if Provider::of(sign_in).is_none() {
            return Err(unprovided(sign_in));
        }
        match self.ask(Ask::Refresh { sign_in: sign_in.to_owned() }).map_err(SignInRefused::Unavailable)? {
            Answer::Refreshed { response } => Ok(response.0),
            Answer::Refused { said } => Err(said.sign_in()),
            other => Err(SignInRefused::Unavailable(format!("run {}: a refresh answered {}", self.run, other.kind()))),
        }
    }

    /// The run's microVM revoked sign-in `sign_in` (its CLI's logout, or a sign-in anew), answered
    /// there: said, and its tokens kept, as the sign-in outlives the run (D242, step 8's question 6).
    fn revoked(&self, sign_in: &str, _key: Option<[u8; 32]>) {
        tracing::info!(run = %self.run, sign_in, "a run's microVM revoked its sign-in, answered there: its tokens kept");
    }
}

impl Drop for RunLessor {
    /// The run's end, as nothing asks through its lessor after: kept so in the broker's ledger, then
    /// every lease held for it revoked, and nothing more leased to it (the broker's
    /// [`Broker::release`]), under its channel and its key alike. Off the runtime it ends here; on
    /// one of its shards, where nothing may block, on a task of its own.
    fn drop(&mut self) {
        let mut identities = vec![std::mem::take(&mut self.channel)];
        identities.extend(self.key.get_mut().take());
        let run = std::mem::take(&mut self.run);
        let ask = Ask::Ended { identities };
        let ending = match &self.to {
            To::Broker { broker, clock } => {
                let (broker, clock) = (Arc::clone(broker), Arc::clone(clock));
                futures::future::FutureExt::boxed(async move { anyhow::Ok(answered(broker, clock, ask).await) })
            }
            To::Server(node) => {
                let node = node.clone();
                futures::future::FutureExt::boxed(async move {
                    let json = Words::of(&crate::reach::Question::Lessor { ask })?;
                    let said = node.lessor(json).await?;
                    anyhow::Ok(serde_json::from_str::<Answer>(said.text())?)
                })
            }
        };
        if crate::rt::on_shard() {
            drop(crate::rt::spawn(async move { ended(&run, ending.await) }));
            return;
        }
        ended(&run, crate::rt::block_on(ending));
    }
}

/// Says what a run's end revoked that its host hadn't given back, or why it couldn't: a lease the
/// store couldn't revoke is revoked as the broker next opens, where the run's end was kept.
fn ended(run: &str, said: anyhow::Result<Answer>) {
    match said {
        Ok(Answer::Ended { revoked: 0 }) => {}
        Ok(Answer::Ended { revoked }) => tracing::info!(run, revoked, "a run's credential leases its host hadn't given back, revoked at its end"),
        Ok(Answer::Refused { said }) => tracing::error!(run, "a run's credential leases weren't revoked at its end: {}", said.lease()),
        Ok(other) => tracing::error!(run, "a run's credential leases weren't revoked at its end: it answered {}", other.kind()),
        Err(e) => tracing::error!(run, "a run's credential leases weren't revoked at its end: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use hex_api::BoxId;
    use hex_api::lease::conformance::{self, Fixture};
    use hex_api::lease::{LeaseRefused, Lessor};
    use kiln_seams::secrets::SecretValue;

    use super::super::broker::tests::{Under, unders};
    use super::*;

    fn v(s: &str) -> SecretValue {
        SecretValue::new(s.as_bytes().to_vec())
    }

    /// The stores under test, each with its broker and its lessors, made on the runtime; the test
    /// asks them from its own thread, as Hex does from its.
    fn stores() -> anyhow::Result<Vec<(Under, Arc<Broker>, Lessors)>> {
        crate::rt::block_on(async {
            let mut made = Vec::new();
            for u in unders().await? {
                let broker = Arc::new(u.broker().await?);
                let lessors = Lessors::new(Arc::clone(&broker), Arc::new(u.clock.clone()));
                made.push((u, broker, lessors));
            }
            Ok(made)
        })
    }

    /// Box `name`'s run's lessor, as Hex's image class asks kiln's lessors for one as the run boots
    /// (`ImageClass::run_lessor`): through Hex's seam alone.
    fn lessor(lessors: &Lessors, name: &str) -> anyhow::Result<Arc<dyn Lessor>> {
        let id = BoxId::new(name).map_err(anyhow::Error::msg)?;
        hex_api::lease::Lessors::lessor(lessors, &id).map_err(anyhow::Error::msg)
    }

    /// Credential `name` holds `value` in the store, with no end of its own.
    fn hold(u: &Under, name: &str, value: &[u8]) -> anyhow::Result<()> {
        let (secrets, name, value) = (Arc::clone(&u.secrets), name.to_owned(), SecretValue::new(value.to_vec()));
        crate::rt::block_on(async move { secrets.create(&name, value).await.map(|_| ()).map_err(anyhow::Error::msg) })
    }

    /// Whether the store reads `lease` still.
    fn reads(u: &Under, lease: &str) -> anyhow::Result<bool> {
        let (secrets, lease) = (Arc::clone(&u.secrets), lease.to_owned());
        crate::rt::block_on(async move { Ok(secrets.read_leased(&lease).await.is_ok()) })
    }

    /// How many leases the broker's ledger holds for runs and hasn't revoked, every run's together.
    fn unrevoked(u: &Under) -> anyhow::Result<i64> {
        let ledger = u.ledger.clone();
        crate::rt::block_on(async move { ledger.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM broker_leases WHERE revoked_ms IS NULL", [], |r| r.get::<_, i64>(0))?)).await })
    }

    /// Hex's lease contract's fixture: kiln's lessors, and the store they lease from.
    struct Contract<'a> {
        under: &'a Under,
        lessors: &'a Lessors,
    }

    impl Fixture for Contract<'_> {
        fn lessors(&self) -> &dyn hex_api::lease::Lessors {
            self.lessors
        }
        fn hold(&self, reference: &str, value: &[u8]) -> Result<(), String> {
            hold(self.under, reference, value).map_err(|e| format!("{e:#}"))
        }
        fn holds(&self, lease: &str) -> Result<bool, String> {
            reads(self.under, lease).map_err(|e| format!("{e:#}"))
        }
    }

    /// CP-T8.3 (D242; Hex's docs/design/credential-grants.md C3): kiln's lessors over its run-lease
    /// broker pass Hex's lease contract (HEX-LEASE-1 to 4) through its seam alone, over the
    /// simulator's store and kiln's own alike, and leave none of its cases' leases held in the
    /// broker's ledger.
    #[test]
    fn cp_t8_3_kilns_lessors_pass_hexs_lease_contract() -> anyhow::Result<()> {
        for (u, _, lessors) in stores()? {
            let failures = conformance::run(&Contract { under: &u, lessors: &lessors });
            anyhow::ensure!(failures.is_empty(), "{}: {failures:?}", u.name);
            anyhow::ensure!(unrevoked(&u)? == 0, "{}: a case left a lease held", u.name);
        }
        Ok(())
    }

    /// CP-T8.3 (D242): through Hex's seam, a run's lessor leases its credential's value from the
    /// broker by reference, living until the run ends where its provider gives it no end, its value
    /// never shown; each lease it gives back is revoked alone, the run's others and another run's
    /// held on; and one another run's lessor gives back is held on.
    #[test]
    fn cp_t8_3_a_runs_lessor_leases_by_reference_through_hexs_seam_and_gives_each_lease_back() -> anyhow::Result<()> {
        for (u, _, lessors) in stores()? {
            let store = u.name;
            hold(&u, "providers/anthropic", b"sk-ant-api03-real")?;
            let (run, other) = (lessor(&lessors, "box-a")?, lessor(&lessors, "box-b")?);
            let one = run.lease("providers/anthropic", None).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(one.value.expose() == b"sk-ant-api03-real", "{store}: the value");
            anyhow::ensure!(one.life.is_none(), "{store}: a value with no end of its own lives until its run ends: {:?}", one.life);
            anyhow::ensure!(!format!("{one:?}").contains("real"), "{store}: a lease shows its value");
            let two = run.lease("providers/anthropic", None).map_err(anyhow::Error::msg)?;
            let theirs = other.lease("providers/anthropic", None).map_err(anyhow::Error::msg)?;
            run.release(&one.lease);
            anyhow::ensure!(!reads(&u, &one.lease)?, "{store}: a lease given back still reads");
            anyhow::ensure!(reads(&u, &two.lease)? && reads(&u, &theirs.lease)?, "{store}: giving one lease back ended another");
            other.release(&two.lease);
            anyhow::ensure!(reads(&u, &two.lease)?, "{store}: another run gave this run's lease back");
        }
        Ok(())
    }

    /// CP-T8.3 (D242): a run's end, as Hex and the run's host let go of its lessor, revokes every
    /// lease it holds still, issued to its key or to its channel, and nothing more is leased under
    /// its key; another run's leases are held on, and it leases on.
    #[test]
    fn cp_t8_3_a_runs_end_revokes_every_lease_it_holds() -> anyhow::Result<()> {
        const A: [u8; 32] = [0xa1; 32];
        for (u, _, lessors) in stores()? {
            let store = u.name;
            hold(&u, "providers/anthropic", b"sk-ant-api03-real")?;
            hold(&u, "providers/openai", b"sk-proj-real")?;
            let (keyed, bare, other) = (lessor(&lessors, "box-k")?, lessor(&lessors, "box-b")?, lessor(&lessors, "box-o")?);
            let mut held = Vec::new();
            for (run, key) in [(&keyed, Some(A)), (&bare, None)] {
                for reference in ["providers/anthropic", "providers/openai"] {
                    held.push(run.lease(reference, key).map_err(anyhow::Error::msg)?.lease);
                }
            }
            let theirs = other.lease("providers/anthropic", None).map_err(anyhow::Error::msg)?;
            drop((keyed, bare));
            for lease in &held {
                anyhow::ensure!(!reads(&u, lease)?, "{store}: a run's lease outlived its end");
            }
            anyhow::ensure!(reads(&u, &theirs.lease)?, "{store}: one run's end revoked another's lease");
            let after = lessor(&lessors, "box-k")?.lease("providers/anthropic", Some(A)).err();
            anyhow::ensure!(after == Some(LeaseRefused::Ended), "{store}: an ask under the key of a run that ended: {after:?}");
            anyhow::ensure!(other.lease("providers/openai", None).is_ok(), "{store}: one run's end ended another's leases");
        }
        Ok(())
    }

    /// CP-T8.3 (D242): a run let go of on one of the runtime's shards, which can't block, ends on a
    /// task of its own, its leases revoked all the same: waited for by what its end leaves in the
    /// store, bounded only against a hang.
    #[test]
    fn cp_t8_3_a_run_let_go_of_on_the_runtime_ends_all_the_same() -> anyhow::Result<()> {
        for (u, _, lessors) in stores()? {
            let store = u.name;
            hold(&u, "providers/anthropic", b"sk-ant-api03-real")?;
            let run = lessor(&lessors, "box-a")?;
            let one = run.lease("providers/anthropic", None).map_err(anyhow::Error::msg)?;
            crate::rt::block_on(async move {
                drop(run);
                Ok(())
            })?;
            let until = Instant::now().checked_add(Duration::from_secs(60)).ok_or_else(|| anyhow::anyhow!("{store}: no bound past now"))?;
            while reads(&u, &one.lease)? {
                anyhow::ensure!(Instant::now() < until, "{store}: a run let go of on a shard kept its lease");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        Ok(())
    }

    /// CP-T8.3 (D242): through Hex's seam, each refusal is D242's kind, in the broker's words: a
    /// credential the store holds none of, or can't name, denied; a run's second key unavailable;
    /// an ask under a key a move replaced, that key and the run's key now; a value past its own end
    /// unavailable until its sign-in refreshes it, and one with an end lives until then.
    #[test]
    fn cp_t8_3_a_lessor_refuses_by_d242s_kinds() -> anyhow::Result<()> {
        const A: [u8; 32] = [0xa1; 32];
        const B: &str = "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
        for (u, broker, lessors) in stores()? {
            let store = u.name;
            hold(&u, "providers/openai", b"sk-proj-real")?;
            let run = lessor(&lessors, "box-a")?;
            anyhow::ensure!(matches!(run.lease("providers/none", None), Err(LeaseRefused::Denied(_))), "{store}: a credential the store holds none of");
            anyhow::ensure!(matches!(run.lease("not a name", None), Err(LeaseRefused::Denied(_))), "{store}: a reference no store names");
            let keyed = lessor(&lessors, "box-k")?;
            keyed.lease("providers/openai", Some(A)).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(matches!(keyed.lease("providers/openai", Some([0xc3; 32])), Err(LeaseRefused::Unavailable(_))), "{store}: a second key");
            let moved = Arc::clone(&broker);
            crate::rt::block_on(async move { moved.moved(&hex::encode(A), B).await.map_err(anyhow::Error::msg) })?;
            let after = keyed.lease("providers/openai", Some(A)).err();
            anyhow::ensure!(after == Some(LeaseRefused::KeyReplaced { asked: hex::encode(A), current: Some(B.to_owned()) }), "{store}: an ask under a replaced key: {after:?}");
            let b = Arc::clone(&broker);
            let captured = crate::rt::block_on(async move { b.capture("claude/ada", super::super::broker::Tokens { access: v("oat-real"), refresh: None, expires_in_s: Some(600), scope: None }).await.map_err(anyhow::Error::msg) })?;
            let short = run.lease(&captured.access.reference, None).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(short.life == Some(Duration::from_secs(600)), "{store}: its life {:?}", short.life);
            u.clock.advance(Duration::from_secs(601));
            anyhow::ensure!(matches!(run.lease(&captured.access.reference, None), Err(LeaseRefused::Unavailable(_))), "{store}: a value past its own end");
        }
        Ok(())
    }

    /// Claude Code's and Codex's token endpoints stood in for, once for every test here (each
    /// test's sign-ins its own refresh tokens): each refresh asked of it kept, by its path and
    /// body, and answered as its refresh token says: `rt-refused` refused (RFC 6749 §5.2's 400),
    /// `rt-broken` unavailable (503), any other given its next tokens, Codex's with an ID token.
    struct StandIn {
        asked: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    }

    impl StandIn {
        /// The refreshes asked of it with refresh token `rt`, by their paths and bodies.
        fn asked(&self, rt: &str) -> Vec<(String, serde_json::Value)> {
            self.asked.lock().iter().filter(|(_, body)| body.get("refresh_token").and_then(serde_json::Value::as_str) == Some(rt)).cloned().collect()
        }
    }

    fn stand_in() -> anyhow::Result<&'static StandIn> {
        use crate::http::server::{self as srv, Request, Response};

        struct Fake {
            asked: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
        }
        impl srv::Handler for Fake {
            async fn handle(self: Arc<Self>, req: Request) -> Response {
                let path = req.uri().path().to_owned();
                let body = srv::body_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
                let asked: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                let rt = asked.get("refresh_token").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
                self.asked.lock().push((path.clone(), asked));
                match rt.as_str() {
                    "rt-refused" => srv::json(http::StatusCode::BAD_REQUEST, &serde_json::json!({ "error": "invalid_grant", "error_description": "Refresh token revoked" })),
                    "rt-broken" => srv::json(http::StatusCode::SERVICE_UNAVAILABLE, &serde_json::json!({ "error": "temporarily_unavailable" })),
                    _ if path == "/oauth/token" => srv::json(http::StatusCode::OK, &serde_json::json!({ "id_token": format!("eyJh.eyJp.sig-{rt}"), "access_token": format!("at-{rt}"), "refresh_token": format!("{rt}-2") })),
                    _ => srv::json(http::StatusCode::OK, &serde_json::json!({ "access_token": format!("at-{rt}"), "refresh_token": format!("{rt}-2"), "expires_in": 3600, "token_type": "Bearer" })),
                }
            }
        }

        /// Bound, served and made both providers' endpoint before any test asks it.
        fn start() -> anyhow::Result<StandIn> {
            let asked = Arc::new(Mutex::new(Vec::new()));
            let fake = Arc::new(Fake { asked: Arc::clone(&asked) });
            let addr = crate::rt::block_on(async move {
                let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
                let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
                crate::rt::spawn(async move {
                    while let Ok(s) = tcp.accept().await {
                        let (h, r) = (Arc::clone(&fake), s.peer_addr().ok());
                        crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
                    }
                });
                anyhow::Ok(addr)
            })?;
            crate::ci::integrations::override_endpoint("claude:token", &format!("http://{addr}"));
            crate::ci::integrations::override_endpoint("codex:token", &format!("http://{addr}"));
            Ok(StandIn { asked })
        }
        static S: std::sync::LazyLock<Result<StandIn, String>> = std::sync::LazyLock::new(|| start().map_err(|e| format!("{e:#}")));
        S.as_ref().map_err(|e| anyhow::anyhow!("the stand-in token endpoint: {e}"))
    }

    fn sign_in_tokens(access: &str, refresh: &str, expires_in: Option<u64>, id_token: Option<&str>, scope: Option<&str>) -> SignInTokens {
        let value = |s: &str| Value::new(s.as_bytes().to_vec());
        SignInTokens { access: value(access), refresh: Some(value(refresh)), expires_in, id_token: id_token.map(value), scope: scope.map(str::to_owned) }
    }

    /// A refresh's answer as its CLI reads it: its fields, and its words whole.
    fn read(answer: &Value) -> anyhow::Result<(serde_json::Map<String, serde_json::Value>, String)> {
        let words = String::from_utf8(answer.expose().to_vec())?;
        match serde_json::from_str(&words)? {
            serde_json::Value::Object(fields) => Ok((fields, words)),
            other => anyhow::bail!("an answer that is no object: {other}"),
        }
    }

    fn placeholder(fields: &serde_json::Map<String, serde_json::Value>, name: &str) -> bool {
        fields.get(name).and_then(serde_json::Value::as_str).is_some_and(|p| p.starts_with("kiln-placeholder-"))
    }

    /// Plan step 33's step 8 (D242, its third slice; CP-T8.3): through Hex's seam, a run's sign-in
    /// is captured into the broker, its CLI given placeholders and its access token leased by
    /// reference; its refresh is made at its provider by the broker alone, as its CLI's own would
    /// be (Claude Code's scope asked again, Codex's none), the rotated tokens kept under the same
    /// references and its CLI answered placeholders, never a token, Codex's ID token left out; and a
    /// revocation in its microVM keeps the sign-in.
    #[test]
    fn cp_t8_3_a_runs_sign_ins_are_captured_refreshed_and_revoked_outside_through_hexs_seam() -> anyhow::Result<()> {
        let provider = stand_in()?;
        for (u, _, lessors) in stores()? {
            let store = u.name;
            let run = lessor(&lessors, &format!("box-s-{store}"))?;
            let claude = format!("claude/ada-{store}");
            let scope = "user:inference user:profile";
            let c = run.capture(&claude, sign_in_tokens(&format!("at-claude-{store}"), &format!("rt-claude-{store}"), Some(28800), None, Some(scope)), None).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(c.access == format!("sign-ins/{claude}/access"), "{store}: its reference {}", c.access);
            let refresh = c.refresh_placeholder.clone().unwrap_or_default();
            anyhow::ensure!(c.access_placeholder.starts_with("kiln-placeholder-") && refresh.starts_with("kiln-placeholder-") && c.access_placeholder != refresh, "{store}: {c:?}");
            let held = run.lease(&c.access, None).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(held.value.expose() == format!("at-claude-{store}").as_bytes(), "{store}: the captured access token isn't the one leased");

            let (fields, words) = read(&run.refresh(&claude, None).map_err(anyhow::Error::msg)?)?;
            let asked = provider.asked(&format!("rt-claude-{store}"));
            let expect = serde_json::json!({ "grant_type": "refresh_token", "refresh_token": format!("rt-claude-{store}"), "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e", "scope": scope });
            anyhow::ensure!(asked == [("/v1/oauth/token".to_owned(), expect)], "{store}: Claude Code's provider was asked {asked:?}");
            anyhow::ensure!(!words.contains(&format!("claude-{store}")), "{store}: a token in the answer");
            anyhow::ensure!(placeholder(&fields, "access_token") && placeholder(&fields, "refresh_token"), "{store}: {fields:?}");
            let said = |name: &str| fields.get(name).cloned();
            anyhow::ensure!((said("token_type"), said("expires_in"), said("scope")) == (Some("Bearer".into()), Some(3600u64.into()), Some(scope.into())), "{store}: {fields:?}");
            anyhow::ensure!(fields.len() == 5, "{store}: its provider's other fields in the answer: {fields:?}");
            let rotated = run.lease(&c.access, None).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(rotated.value.expose() == format!("at-rt-claude-{store}").as_bytes(), "{store}: the rotated access token isn't kept under its reference");
            run.revoked(&claude, None);
            let after = run.lease(&c.access, None).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(after.value.expose() == format!("at-rt-claude-{store}").as_bytes(), "{store}: a revocation in the microVM ended the sign-in");

            let codex = format!("codex/ada-{store}");
            run.capture(&codex, sign_in_tokens(&format!("at-codex-{store}"), &format!("rt-codex-{store}"), None, Some("eyJh.eyJp.shards-placeholder"), None), None).map_err(anyhow::Error::msg)?;
            let (fields, words) = read(&run.refresh(&codex, None).map_err(anyhow::Error::msg)?)?;
            let asked = provider.asked(&format!("rt-codex-{store}"));
            let expect = serde_json::json!({ "client_id": "app_EMoamEEZ73f0CkXaXp7hrann", "grant_type": "refresh_token", "refresh_token": format!("rt-codex-{store}") });
            anyhow::ensure!(asked == [("/oauth/token".to_owned(), expect)], "{store}: Codex's provider was asked {asked:?}");
            anyhow::ensure!(!words.contains(&format!("codex-{store}")) && !words.contains("eyJ"), "{store}: a token in the answer: {fields:?}");
            anyhow::ensure!(placeholder(&fields, "access_token") && placeholder(&fields, "refresh_token") && fields.len() == 3, "{store}: {fields:?}");
        }
        Ok(())
    }

    /// Plan step 33's step 8 (D242, its third slice): through Hex's seam, each sign-in refusal is
    /// D242's kind: a sign-in whose name names no provider, captured by no one and nothing kept; a
    /// refresh of one never captured, and one its provider refuses, denied, so its CLI signs in
    /// anew; one its provider can't answer unavailable, its words holding no token.
    #[test]
    fn cp_t8_3_a_sign_ins_refusals_are_d242s_kinds() -> anyhow::Result<()> {
        stand_in()?;
        for (u, _, lessors) in stores()? {
            let store = u.name;
            let run = lessor(&lessors, &format!("box-r-{store}"))?;
            let devin = format!("devin/ada-{store}");
            let refused = run.capture(&devin, sign_in_tokens("at-devin", "rt-devin", None, None, None), None).err();
            anyhow::ensure!(matches!(refused, Some(SignInRefused::Denied(_))), "{store}: {refused:?}");
            let kept = run.lease(&format!("sign-ins/{devin}/access"), None).err();
            anyhow::ensure!(matches!(kept, Some(LeaseRefused::Denied(_))), "{store}: a sign-in naming no provider was kept: {kept:?}");
            anyhow::ensure!(matches!(run.refresh(&devin, None), Err(SignInRefused::Denied(_))), "{store}: a refresh naming no provider");
            anyhow::ensure!(matches!(run.refresh(&format!("claude/none-{store}"), None), Err(SignInRefused::Denied(_))), "{store}: a refresh of a sign-in never captured");
            for (name, rt) in [(format!("claude/refused-{store}"), "rt-refused"), (format!("codex/broken-{store}"), "rt-broken")] {
                run.capture(&name, sign_in_tokens("at-x", rt, None, None, None), None).map_err(anyhow::Error::msg)?;
                match (rt, run.refresh(&name, None)) {
                    ("rt-refused", Err(SignInRefused::Denied(_))) => {}
                    ("rt-broken", Err(SignInRefused::Unavailable(words))) => anyhow::ensure!(!words.contains(rt), "{store}: {words}"),
                    (_, got) => anyhow::bail!("{store}: {name}: {:?}", got.map(|_| "answered")),
                }
            }
        }
        Ok(())
    }

    /// Plan step 33's step 8 (D242, its third slice): two refreshes of one sign-in at once, as two
    /// runs' lessors ask them of the one broker, make one call to its provider, the second taking
    /// the first's tokens, as a rotated refresh token retires every other holder's (RFC 6749 §6).
    #[test]
    fn cp_t8_3_two_refreshes_of_one_sign_in_at_once_make_one_call_to_its_provider() -> anyhow::Result<()> {
        let provider = stand_in()?;
        for (u, broker, _) in stores()? {
            let store = u.name;
            let name = format!("claude/once-{store}");
            let rt = format!("rt-once-{store}");
            let b = Arc::clone(&broker);
            let (n, r) = (name.clone(), rt.clone());
            let (one, two) = crate::rt::block_on(async move {
                b.capture(&n, Tokens { access: v("at-once"), refresh: Some(v(&r)), expires_in_s: None, scope: None }).await.map_err(anyhow::Error::msg)?;
                anyhow::Ok(futures::join!(b.refresh(&n, Provider::ClaudeCode.upstream()), b.refresh(&n, Provider::ClaudeCode.upstream())))
            })?;
            let (one, two) = (one.map_err(anyhow::Error::msg)?, two.map_err(anyhow::Error::msg)?);
            anyhow::ensure!(one == two, "{store}: the second refresh didn't take the first's tokens");
            let calls = provider.asked(&rt).len();
            anyhow::ensure!(calls == 1, "{store}: {calls} calls to the provider");
        }
        Ok(())
    }

    /// Plan step 33 (D242): a run's ask and the broker's answer cross a runner's link as JSON
    /// (`crate::reach::Question::Lessor`) and come back as they went: a value and a sign-in's
    /// tokens hex there, never in a `Debug`, and each refusal kind for kind.
    #[test]
    fn s33_a_runs_asks_and_answers_cross_a_link_as_they_went() -> anyhow::Result<()> {
        let carried = |s: &str| Carried::new(Value::new(s.as_bytes().to_vec()));
        let ask = Ask::Capture { sign_in: "claude/owner".into(), access: carried("at-real"), refresh: Some(carried("rt-real")), expires_in: Some(60), scope: Some("user:inference".into()) };
        let question = crate::reach::Question::Lessor { ask };
        let json = serde_json::to_string(&question)?;
        anyhow::ensure!(!json.contains("at-real") && json.contains(&hex::encode("at-real")), "{json}");
        anyhow::ensure!(serde_json::from_str::<crate::reach::Question>(&json)? == question, "the ask came back otherwise");
        let shown = format!("{question:?}");
        anyhow::ensure!(!shown.contains("at-real") && !shown.contains(&hex::encode("at-real")), "an ask shows a token: {shown}");
        let leased = Answer::Leased { lease: "l1".into(), value: carried("sk-real"), life_ms: Some(5) };
        anyhow::ensure!(!format!("{leased:?}").contains("sk-real"), "an answer shows its value");
        match serde_json::from_str::<Answer>(&serde_json::to_string(&leased)?)? {
            Answer::Leased { lease, value, life_ms } => anyhow::ensure!((lease.as_str(), value.0.expose(), life_ms) == ("l1", &b"sk-real"[..], Some(5)), "came back otherwise"),
            other => anyhow::bail!("{other:?}"),
        }
        let leases = [LeaseRefused::Denied("d".into()), LeaseRefused::Unavailable("u".into()), LeaseRefused::Ended, LeaseRefused::KeyReplaced { asked: "a".into(), current: Some("c".into()) }, LeaseRefused::Lapsed];
        for why in leases {
            let said: Said = serde_json::from_str(&serde_json::to_string(&Said::from(why.clone()))?)?;
            anyhow::ensure!(said.lease() == why, "{why:?}");
        }
        for why in [SignInRefused::Denied("d".into()), SignInRefused::Unavailable("u".into()), SignInRefused::Ended, SignInRefused::Unanswered] {
            anyhow::ensure!(Said::from(why.clone()).sign_in() == why, "{why:?}");
        }
        Ok(())
    }

    /// Plan step 33 (D242): the broker answers a run's asks as a runner's link carries them, the
    /// answer a runner other than the server's own is given: a capture's placeholders and its
    /// access token leased by their reference, a give-back, and the run's end revoking what it
    /// still held.
    #[test]
    fn s33_the_broker_answers_a_links_asks_as_its_own_runners() -> anyhow::Result<()> {
        for (u, broker, _) in stores()? {
            let store = u.name;
            let clock: Arc<dyn Clock> = Arc::new(u.clock.clone());
            let ask = |a: Ask| {
                let (broker, clock) = (Arc::clone(&broker), Arc::clone(&clock));
                crate::rt::block_on(async move { anyhow::Ok(answered(broker, clock, a).await) })
            };
            let identity = "a1".repeat(32);
            let capture = Ask::Capture { sign_in: format!("claude/link-{store}"), access: Carried::new(Value::new(b"at-link".to_vec())), refresh: None, expires_in: None, scope: None };
            let Answer::Captured { captured } = ask(capture)? else { anyhow::bail!("{store}: no capture") };
            anyhow::ensure!(captured.access == format!("sign-ins/claude/link-{store}/access") && captured.refresh_placeholder.is_none(), "{store}: {captured:?}");
            let Answer::Leased { lease, value, life_ms } = ask(Ask::Lease { identity: identity.clone(), reference: captured.access.clone() })? else { anyhow::bail!("{store}: no lease") };
            anyhow::ensure!(value.0.expose() == b"at-link" && life_ms.is_none(), "{store}: its value or its life");
            anyhow::ensure!(matches!(ask(Ask::Given { identity: identity.clone(), lease: lease.clone() })?, Answer::Given), "{store}: its give-back");
            anyhow::ensure!(!reads(&u, &lease)?, "{store}: a lease given back still reads");
            let Answer::Leased { lease: held, .. } = ask(Ask::Lease { identity: identity.clone(), reference: captured.access.clone() })? else { anyhow::bail!("{store}: no second lease") };
            anyhow::ensure!(matches!(ask(Ask::Ended { identities: vec![identity.clone()] })?, Answer::Ended { revoked: 1 }), "{store}: its end");
            anyhow::ensure!(!reads(&u, &held)?, "{store}: a run's lease outlived its end");
            let after = ask(Ask::Lease { identity, reference: captured.access })?;
            anyhow::ensure!(matches!(after, Answer::Refused { said: Said::Ended }), "{store}: an ask after its run's end: {after:?}");
            let unprovided = ask(Ask::Refresh { sign_in: "devin/link".into() })?;
            anyhow::ensure!(matches!(unprovided, Answer::Refused { said: Said::Denied { .. } }), "{store}: {unprovided:?}");
        }
        Ok(())
    }

    /// Plan step 33 (D242): what a runner hears of whether its server answers its runs' leases, word by word, as each
    /// connection says it: caps naming no features refuse at once; caps naming them wait for their names, which say; a
    /// first connection's Welcome with no caps before it refuses (a kiln from before them); what was said is kept across
    /// a connection's end, and a later one's caps and Welcome, until its names or its caps say otherwise; caps whose
    /// names never came go with their connection. Written, not run (the person's rule of 2026-11-16).
    #[test]
    fn s33_a_runner_hears_whether_its_server_answers_on_each_connection() {
        use super::Heard::{Answers, Naming, Nothing, Refuses};
        let names = Word::Caps { names: true };
        let (yes, no) = (Word::Features { leases: true }, Word::Features { leases: false });
        let cases: [(&[Word], Heard); 13] = [
            (&[], Nothing),
            (&[names], Naming),
            (&[names, Word::Welcome], Naming),
            (&[names, Word::Welcome, yes], Answers),
            (&[names, Word::Welcome, no], Refuses),
            (&[Word::Caps { names: false }], Refuses),
            (&[Word::Welcome], Refuses),
            (&[names, Word::Gone], Nothing),
            (&[names, Word::Gone, Word::Welcome], Refuses),
            (&[names, yes, Word::Gone, names, Word::Welcome], Answers),
            (&[names, yes, Word::Gone, Word::Caps { names: false }], Refuses),
            (&[names, yes, names, Word::Welcome, no], Refuses),
            (&[Word::Caps { names: false }, Word::Gone, names, Word::Welcome, yes], Answers),
        ];
        for (words, expect) in cases {
            assert_eq!(words.iter().fold(Nothing, |heard, word| heard.then(*word)), expect, "{words:?}");
        }
        assert!(Answers.said() && Refuses.said() && !Nothing.said() && !Naming.said(), "only an answer or a refusal is said");
    }

    /// Plan step 33 (D242): a run's ask crosses its runner's link in words made the size they come to (none left behind
    /// as a buffer grows), its tokens hex there and in no `Debug`, beginning as its server knows a run's ask by
    /// ([`asked`]); the server reads one straight, keeping none of its words as they came, where an agent's question is
    /// kept so (its act is checked against them); and words let go of are emptied. Written, not run (the person's rule
    /// of 2026-11-16).
    #[test]
    fn s33_a_runs_ask_crosses_its_link_in_words_made_its_size_and_is_read_straight() -> anyhow::Result<()> {
        use crate::wire::payload::{Carried as Read, FromRunner};
        let carried = |s: &str| Carried::new(Value::new(s.as_bytes().to_vec()));
        let ask = Ask::Capture { sign_in: "claude/owner".into(), access: carried("at-real"), refresh: Some(carried("rt-real")), expires_in: Some(60), scope: None };
        let words = Words::of(&crate::reach::Question::Lessor { ask })?;
        anyhow::ensure!(words.0.len() == words.0.capacity(), "made {} bytes for {}", words.0.capacity(), words.0.len());
        let text = words.text();
        anyhow::ensure!(asked(text) && !text.contains("at-real") && text.contains(&hex::encode("rt-real")), "{text}");
        anyhow::ensure!(!format!("{words:?}").contains(&hex::encode("at-real")), "its words show what they say");
        match crate::wire::payload_of(crate::proto::AgentMsg::ReachAsk { req: 7, json: text.to_owned() }).map_err(|e| anyhow::anyhow!("{e}"))? {
            Read::Read(FromRunner::ReachAsk { req: 7, ask, raw }) => anyhow::ensure!(matches!(*ask, crate::reach::Question::Lessor { ask: Ask::Capture { .. } }) && raw.is_null(), "read as {ask:?}, kept as {raw}"),
            _ => anyhow::bail!("a run's ask wasn't read as one"),
        }
        let directory = serde_json::json!({ "op": "directory" });
        match crate::wire::payload_of(crate::proto::AgentMsg::ReachAsk { req: 8, json: directory.to_string() }).map_err(|e| anyhow::anyhow!("{e}"))? {
            Read::Read(FromRunner::ReachAsk { req: 8, ask, raw }) => anyhow::ensure!(*ask == crate::reach::Question::Directory && raw == directory, "read as {ask:?}, kept as {raw}"),
            _ => anyhow::bail!("an agent's question wasn't read as one"),
        }
        let mut said = String::from("at-real");
        wipe_text(&mut said);
        anyhow::ensure!(said.is_empty() && words.into_text().starts_with(ASKED), "words let go of aren't emptied");
        Ok(())
    }

    /// `ask` of a run of runner `runner`'s, as `hub` answers it over the runner's link ([`answer`]): read back.
    async fn asked_of(hub: &crate::hub::Hub, runner: &str, ask: Ask) -> anyhow::Result<Answer> {
        let words = answer(hub, runner, ask).await;
        Ok(serde_json::from_str::<Answer>(words.text())?)
    }

    /// Plan step 33 (D242): the server answers a runner's runs over its link ([`answer`]) only for sign-ins of people
    /// one of that runner's isolated agents of the sign-in's harness acts for, and for no other credential: its own
    /// isolated agent's sign-in captured and leased, the value its capture kept; a provider's key, its person's sign-in
    /// of a harness its agent there isn't isolated for, another runner's agent's sign-in, a person no agent there acts
    /// for, and any ask from a job's runner or from one kiln doesn't know, denied, saying no value; a give-back and a
    /// run's end, its identity's own, answered; and its own agent's sign-in unavailable where the server's broker isn't
    /// open. Written, not run (the person's rule of 2026-11-16).
    #[test]
    fn s33_a_runner_is_answered_only_for_its_own_isolated_agents_sign_ins() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::registry::Location;
        use crate::harness::session::{Info, Isolation};
        for (u, broker, _) in stores()? {
            let store = u.name;
            let clock: Arc<dyn Clock> = Arc::new(u.clock.clone());
            crate::rt::block_on(async move {
                let (hub, closed) = (crate::hub::tests::scratch_hub().await?, crate::hub::tests::scratch_hub().await?);
                hub.lessors.set(Lessors::new(broker, clock)).map_err(|_| anyhow::anyhow!("{store}: its lessors were open already"))?;
                for h in [&hub, &closed] {
                    h.put_agent(crate::hub::tests::live_runner("r1", "linux", "x86_64"));
                    h.put_agent(crate::hub::tests::live_runner("r2", "linux", "x86_64"));
                    h.put_agent(crate::hub::AgentLive { ephemeral: true, ..crate::hub::tests::live_runner("job", "linux", "x86_64") });
                }
                let isolated = |person: &str| Some(Isolation { kind: Isolation::MICROVM.into(), image: None, person: Some(person.into()), cpus: None, memory: None, disk: None });
                let sessions = [
                    ("r1", Info { id: format!("claude-{store}"), kind: "claude".into(), isolation: isolated("owner"), ..Info::default() }),
                    ("r1", Info { id: format!("codex-{store}"), kind: "codex".into(), ..Info::default() }),
                    ("r2", Info { id: format!("codex2-{store}"), kind: "codex".into(), isolation: isolated("user:7"), ..Info::default() }),
                ];
                for (runner, info) in sessions {
                    let id = info.id.clone();
                    for h in [&hub, &closed] {
                        h.harness.ingest(&Location::Machine(runner.into()), &id, Update::Open { info: Box::new(info.clone()), since: None }).await;
                    }
                }
                let token = format!("at-{store}");
                let identity = "a1".repeat(32);
                let capture = |name: &str| Ask::Capture { sign_in: name.into(), access: Carried::new(Value::new(token.as_bytes().to_vec())), refresh: None, expires_in: None, scope: None };
                let lease = |name: &str| Ask::Lease { identity: identity.clone(), reference: format!("sign-ins/{name}/access") };

                let Answer::Captured { captured } = asked_of(&hub, "r1", capture("claude/owner")).await? else { anyhow::bail!("{store}: r1's own agent's sign-in wasn't captured") };
                anyhow::ensure!(captured.access == "sign-ins/claude/owner/access", "{store}: {captured:?}");
                let Answer::Leased { lease: held, value, .. } = asked_of(&hub, "r1", lease("claude/owner")).await? else { anyhow::bail!("{store}: r1's own agent's sign-in wasn't leased") };
                anyhow::ensure!(value.0.expose() == token.as_bytes(), "{store}: another value was leased");

                let others = [
                    ("r1", Ask::Lease { identity: identity.clone(), reference: "providers/anthropic".into() }),
                    ("r1", lease("codex/owner")),
                    ("r1", capture("codex/user-7")),
                    ("r1", lease("claude/user-7")),
                    ("r2", lease("claude/owner")),
                    ("job", lease("claude/owner")),
                    ("ghost", lease("claude/owner")),
                ];
                for (runner, ask) in others {
                    let shown = format!("{ask:?}");
                    let said = asked_of(&hub, runner, ask).await?;
                    anyhow::ensure!(matches!(said, Answer::Refused { said: Said::Denied { .. } }), "{store}: {runner} asking {shown}: {said:?}");
                    anyhow::ensure!(!format!("{said:?}").contains(&token), "{store}: a refusal said a value");
                }

                anyhow::ensure!(matches!(asked_of(&hub, "r1", Ask::Given { identity: identity.clone(), lease: held }).await?, Answer::Given), "{store}: its give-back");
                anyhow::ensure!(matches!(asked_of(&hub, "r1", Ask::Ended { identities: vec![identity.clone()] }).await?, Answer::Ended { .. }), "{store}: its run's end");
                let shut = asked_of(&closed, "r1", lease("claude/owner")).await?;
                anyhow::ensure!(matches!(shut, Answer::Refused { said: Said::Unavailable { .. } }), "{store}: with its broker not open: {shut:?}");
                anyhow::Ok(())
            })?;
        }
        Ok(())
    }

    /// Plan step 33 (D242), as production runs it (2026-11-20, `[local_agent]` off): the server names `secrets.leases` to
    /// its runners only while its broker is open ([`crate::hub::Hub::features`]), and once it is (as it starts, its own
    /// runner or none: `crate::server`), its runner's isolated agent's sign-in is captured and leased. Named with no
    /// broker open, every runner's microVMs were granted sign-ins it refused, "its server's run-lease broker isn't open",
    /// and Claude Code in each said "Not logged in".
    #[test]
    fn s33_a_server_names_leases_only_while_its_broker_is_open() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::registry::Location;
        use crate::harness::session::{Info, Isolation};
        let Some((u, broker, _)) = stores()?.into_iter().next() else { anyhow::bail!("no store to lease from") };
        let clock: Arc<dyn Clock> = Arc::new(u.clock.clone());
        crate::rt::block_on(async move {
            let hub = crate::hub::tests::scratch_hub().await?;
            hub.put_agent(crate::hub::tests::live_runner("r1", "macos", "aarch64"));
            let isolation = Isolation { kind: Isolation::MICROVM.into(), image: None, person: Some("owner".into()), cpus: None, memory: None, disk: None };
            let info = Info { id: "claude-vm".into(), kind: "claude".into(), isolation: Some(isolation), ..Info::default() };
            hub.harness.ingest(&Location::Machine("r1".into()), "claude-vm", Update::Open { info: Box::new(info), since: None }).await;
            let capture = || Ask::Capture { sign_in: "claude/owner".into(), access: Carried::new(Value::new(b"at-owner".to_vec())), refresh: None, expires_in: None, scope: None };
            let names = |hub: &crate::hub::Hub| hub.features().iter().any(|n| n == crate::proto::feature::LEASES);

            anyhow::ensure!(!names(&hub), "it named {} with no broker open", crate::proto::feature::LEASES);
            let shut = asked_of(&hub, "r1", capture()).await?;
            anyhow::ensure!(matches!(shut, Answer::Refused { said: Said::Unavailable { .. } }), "with no broker open: {shut:?}");

            hub.lessors.set(Lessors::new(broker, clock)).map_err(|_| anyhow::anyhow!("its lessors were open already"))?;
            anyhow::ensure!(names(&hub), "it didn't name {} with its broker open", crate::proto::feature::LEASES);
            let others = hub.features().into_iter().filter(|n| n != crate::proto::feature::LEASES).count();
            anyhow::ensure!(others + 1 == crate::proto::feature::ALL.len(), "it named {others} other features of {}", crate::proto::feature::ALL.len() - 1);
            let Answer::Captured { captured } = asked_of(&hub, "r1", capture()).await? else { anyhow::bail!("r1's isolated agent's sign-in wasn't captured") };
            anyhow::ensure!(captured.access == "sign-ins/claude/owner/access", "{captured:?}");
            let leased = asked_of(&hub, "r1", Ask::Lease { identity: "a1".repeat(32), reference: captured.access }).await?;
            let Answer::Leased { value, .. } = leased else { anyhow::bail!("r1's isolated agent's sign-in wasn't leased: {leased:?}") };
            anyhow::ensure!(value.0.expose() == b"at-owner", "another value was leased");
            anyhow::Ok(())
        })
    }

    /// Plan step 33 (D242): a runner other than the server's own leases through its link. Its run's lessor ([`Remote`])
    /// puts each ask to its server through its reach node (`Handle::lessor`) as a `lessor` question, its tokens hex
    /// there alone, and the server's answer comes back as the broker gave it: a capture's placeholders, its access token
    /// leased by their reference, a give-back, and the run's end revoking what it still held. The server stands in here
    /// as `answer` answers, from the broker it opened. Written, not run (the person's rule of 2026-11-16).
    #[test]
    fn s33_a_remote_runners_runs_lease_through_its_link() -> anyhow::Result<()> {
        use crate::proto::AgentMsg;
        use crate::reach::node::{self, Config, In};
        let Some((u, broker, _)) = stores()?.into_iter().next() else { anyhow::bail!("no store to lease from") };
        let clock: Arc<dyn Clock> = Arc::new(u.clock.clone());
        let dir = tempfile::tempdir()?;
        let cfg = Config { mirrors: dir.path().join("mirrors"), sense: false, ..Config::for_runner("remote", dir.path(), false) };
        let asked: Arc<Mutex<Vec<String>>> = Arc::default();
        let said = Arc::clone(&asked);
        let handle = crate::rt::block_on(async move {
            let (handle, out) = node::start(cfg)?;
            handle.feed(In::Server(true)).await;
            let server = handle.clone();
            crate::rt::spawn(async move {
                while let Ok(msg) = out.recv().await {
                    let AgentMsg::ReachAsk { req, json } = msg else { continue };
                    let Ok(crate::reach::Question::Lessor { ask }) = serde_json::from_str::<crate::reach::Question>(&json) else { continue };
                    said.lock().push(json);
                    let answer = answered(Arc::clone(&broker), Arc::clone(&clock), ask).await;
                    let Ok(answer) = serde_json::to_value(&answer) else { continue };
                    server.feed(In::Answer { req, answer }).await;
                }
            });
            anyhow::Ok(handle)
        })?;
        heard(Word::Features { leases: true });
        let id = BoxId::new("box-remote").map_err(anyhow::Error::msg)?;
        let run = hex_api::lease::Lessors::lessor(&Remote::new(handle), &id).map_err(anyhow::Error::msg)?;
        let c = run.capture("claude/remote", sign_in_tokens("at-remote", "rt-remote", Some(600), None, None), None).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(c.access == "sign-ins/claude/remote/access" && c.access_placeholder.starts_with("kiln-placeholder-"), "{c:?}");
        let one = run.lease(&c.access, None).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(one.value.expose() == b"at-remote", "another value was leased");
        run.release(&one.lease);
        anyhow::ensure!(!reads(&u, &one.lease)?, "a lease given back over the link still reads");
        let two = run.lease(&c.access, None).map_err(anyhow::Error::msg)?;
        drop(run);
        anyhow::ensure!(!reads(&u, &two.lease)?, "a run's lease outlived its end over the link");
        let asked = asked.lock().clone();
        anyhow::ensure!(asked.len() == 5 && asked.iter().all(|j| super::asked(j)), "its server was asked {asked:?}");
        anyhow::ensure!(asked.iter().all(|j| !j.contains("at-remote") && !j.contains("rt-remote")) && asked.iter().any(|j| j.contains(&hex::encode("rt-remote"))), "its tokens crossed other than as hex: {asked:?}");
        Ok(())
    }
}
