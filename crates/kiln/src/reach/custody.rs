//! Custody of what one runner hands another over a reach link (docs/ci.md W27-R08; the Transfers
//! plan's step 12): a sync's workspace and a run, from the runner that asks to the runner that does
//! it, and the run's output back.
//!
//! - **Who signs.** The asking side signs as the agent session whose run it is, when this process
//!   holds the session's key at its epoch, and gives the doing runner's process a delegation to
//!   receive the workspace and the run for it ([`custody::reach_delegation`]); a run no session
//!   asked for (a person's, at a terminal) is this runner process's own. The doing side signs as
//!   its runner process, taking custody for the session under that delegation.
//! - **What.** The workspace by its tree's digest (what `kiln run --reuse` reads,
//!   [`super::reuse::digest_of`]), the run by what it runs ([`run_digest`]), its output by the
//!   SHA-256 of its bytes in the order the run said them.
//! - **Where.** Each hand-on rides the link ahead of what it hands on ([`Ask::Custody`],
//!   [`Say::Custody`]) and goes to kiln too. Each side checks what the other handed on before using
//!   it, and signs its receipt of what it got: a digest that changed on the way is the break kiln
//!   records at that hop, and the copy, the run or the output is refused where it would be used.
//! - **Links from before.** Nothing rides a link at version 1. The asking side signs its hand-on
//!   anyway, saying the other runner speaks 1 and no receipt comes; the doing side takes what a
//!   runner from before hands it as from outside kiln (`reach-v1:<runner>`). Either way the hop is
//!   named, never missing.
//!
//! [`Ask::Custody`]: super::wire::Ask::Custody
//! [`Say::Custody`]: super::wire::Say::Custody

use kiln_ci::identity::custody::{self, Kind};
use kiln_ci::identity::statement::{self, Checked, PAYLOAD_CUSTODY, PAYLOAD_DELEGATION, Statement};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::wire::RunSpec;
use crate::runner::identity;

/// Who a side of a link signs as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signer {
    slot: String,
    /// Its identity's URI.
    pub uri: String,
    /// The agent session it is, at its epoch, without the process holding its key: whom the doing
    /// side holds what it takes for.
    pub session: Option<String>,
}

impl Signer {
    /// The asking side for agent session `session` (None: a run no session asked for): the
    /// session, when this process holds its key at its epoch, else this runner process. None when
    /// this process sends kiln no custody records (no identity kiln certified, a server from
    /// before).
    pub fn asker(session: Option<&str>) -> Option<Self> {
        if !crate::runner::custody::sends() {
            return None;
        }
        if let Some(s) = session
            && let Some(epoch) = identity::epoch_held(s)
        {
            let slot = identity::slot("agent", Some(s), Some(epoch));
            if let Some(uri) = identity::uri(&slot) {
                return Some(Self { slot, session: Some(unheld(&uri)), uri });
            }
        }
        Self::doer()
    }

    /// The doing side: this runner process, when it sends kiln its custody records.
    pub fn doer() -> Option<Self> {
        if !crate::runner::custody::sends() {
            return None;
        }
        Some(Self { slot: "runner".to_owned(), uri: identity::uri("runner")?, session: None })
    }

    /// `s` signed as `payload_type`, its body naming this signer: its bundle.
    fn sign_as(&self, payload_type: &str, s: &Statement) -> Option<Value> {
        match crate::runner::custody::sign_record(&self.slot, &self.uri, payload_type, s) {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::warn!(target: "kiln.custody", signer = %self.uri, error = %format!("{e:#}"), "a custody record of a reach link couldn't be signed");
                None
            }
        }
    }

    fn sign(&self, s: &Statement) -> Option<Value> {
        self.sign_as(PAYLOAD_CUSTODY, s)
    }
}

/// An agent session's identity at its epoch without the process holding its key (`…/agent/<s>/
/// epoch/<n>`), from one with it; any other identity as it is.
fn unheld(uri: &str) -> String {
    match uri.split_once("/machine/") {
        Some((head, _)) if head.contains("/agent/") => head.to_owned(),
        _ => uri.to_owned(),
    }
}

fn push(h: &mut Sha256, field: &[u8]) {
    h.update((field.len() as u64).to_be_bytes());
    h.update(field);
}

/// What run `spec` of the asking runner's reach session `session` is, by its SHA-256 (hex): its
/// number, command, folder, copy, shell and whether it reads input, the names of the environment
/// it's given (never their values), and its workspace's tree digest (`tree`, "" without a sync),
/// each field behind its length. Both sides work it out of what the run says.
pub fn run_digest(session: &str, spec: &RunSpec, tree: &str) -> String {
    let mut h = Sha256::new();
    push(&mut h, b"kiln/reach-run/v1");
    push(&mut h, session.as_bytes());
    push(&mut h, spec.id.to_string().as_bytes());
    push(&mut h, spec.command.as_bytes());
    push(&mut h, spec.cwd.as_deref().unwrap_or_default().as_bytes());
    push(&mut h, spec.mirror.as_deref().unwrap_or_default().as_bytes());
    push(&mut h, spec.shell.as_deref().unwrap_or_default().as_bytes());
    push(&mut h, if spec.stdin { b"stdin" } else { b"" });
    let mut names: Vec<&str> = spec.env.iter().map(|(k, _)| k.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    for n in names {
        push(&mut h, n.as_bytes());
    }
    push(&mut h, tree.as_bytes());
    hex::encode(h.finalize())
}

/// Why a record another runner sent over the link isn't one this side takes.
fn checked(root: &[u8], bundle: &Value, peer: &str) -> Result<Checked, String> {
    let c = statement::verify(bundle, root, &[], crate::util::now_unix()).map_err(|e| format!("it doesn't verify: {e}"))?;
    if !c.custodian.contains(&format!("/machine/{peer}/")) {
        return Err(format!("it's signed by {}, not by {peer}", c.custodian));
    }
    Ok(c)
}

/// The first thing a record is about, by its SHA-256.
fn sha256_of(c: &Checked) -> Option<(&str, &str)> {
    let t = c.statement.subject.first()?;
    Some((t.name.as_str(), t.digest.get("sha256")?.as_str()))
}

/// Sends `bundles` to kiln, in order, as this process's custody records.
fn to_kiln(bundles: &[&Value]) {
    for b in bundles {
        crate::runner::custody::send(b);
    }
}

/// Tells kiln, once, that this side refused `what` from `from` over a reach link, and why: kept with
/// kiln's refusals (docs/ci.md W27-R08).
pub fn refused(what: &str, from: &str, why: &str) {
    tracing::warn!(target: "kiln.custody", %what, %from, %why, "refused over a reach link");
    if crate::runner::custody::sends() {
        crate::runner::custody::send_once(&json!({ "refused_hop": { "what": what, "from": from, "why": why } }));
    }
}

// ----------------------------------------------------------------------------- the asking side

/// What the asking side hands on ahead of a sync's commit or a run: the frame that rides the link
/// ([`super::wire::Ask::Custody`]'s JSON), and its hand-on's digest when it signed one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Handed {
    pub frame: String,
    pub hand_on: Option<String>,
}

/// The asking side's hand-on of `thing` (`key` names it in the frame: `workspace` or `run`) to the
/// doing runner's process `doer`, with the session's delegation to it, each also sent to kiln. On a
/// link at version 1 (`v1`) the hand-on says the doer speaks 1, and nothing rides the link.
fn hand_on(me: Option<&Signer>, doer: Option<&str>, key: (&str, Value), thing: statement::Subject, why: &str, v1: Option<&str>) -> Handed {
    let unsigned = |why: &str| Handed { frame: json!({ key.0: key.1, "unsigned": why }).to_string(), hand_on: None };
    let Some(me) = me else { return unsigned("this runner signs no custody records (kiln certified it no identity, or its server is from before)") };
    let Some(doer) = doer else { return unsigned("kiln didn't say which process of the other runner takes it") };
    let why = match v1 {
        Some(peer) => format!("{why}; {peer} speaks reach version 1, so no receipt of it comes"),
        None => why.to_owned(),
    };
    let Some(h) = me.sign(&custody::reach_hand_on(&me.uri, doer, None, thing, Vec::new(), &why)) else { return unsigned("its hand-on couldn't be signed") };
    let g = me.session.is_some().then(|| me.sign_as(PAYLOAD_DELEGATION, &custody::reach_delegation(&me.uri, doer))).flatten();
    to_kiln(&g.iter().chain(std::iter::once(&h)).collect::<Vec<_>>());
    let digest = crate::runner::custody::digest_of(&h);
    let mut frame = json!({ key.0: key.1, "hand_on": h });
    if let (Some(g), Some(o)) = (g, frame.as_object_mut()) {
        o.insert("delegation".into(), g);
    }
    Handed { frame: frame.to_string(), hand_on: Some(digest) }
}

/// The asking side's hand-on of copy `key`'s workspace, its tree's digest `tree`, to `doer`.
pub fn workspace_hand_on(me: Option<&Signer>, doer: Option<&str>, key: &str, tree: &str, v1: Option<&str>) -> Handed {
    hand_on(me, doer, ("workspace", json!(key)), custody::reach_workspace(key, tree), "the workspace of a run on another machine", v1)
}

/// The asking side's hand-on of run `spec` (its reach session `session`, its workspace's tree
/// `tree`) to `doer`.
pub fn run_hand_on(me: Option<&Signer>, doer: Option<&str>, session: &str, spec: &RunSpec, tree: &str, v1: Option<&str>) -> Handed {
    hand_on(me, doer, ("run", json!(spec.id)), custody::reach_run(spec.id, &run_digest(session, spec, tree)), "a run on another machine", v1)
}

/// The doing side's receipt in `frame` (its JSON, as it rode the link), of hand-on `handed` of
/// `name` with SHA-256 `want`: why it isn't one, if it isn't.
pub fn check_receipt(root: &[u8], peer: &str, frame: Option<&str>, handed: &Handed, want: &str) -> Result<(), String> {
    let Some(hand_on) = handed.hand_on.as_deref() else { return Ok(()) };
    let v: Value = frame.and_then(|f| serde_json::from_str(f).ok()).ok_or_else(|| format!("{peer} sent no receipt of it"))?;
    if let Some(why) = v.get("unsigned").and_then(Value::as_str) {
        return Err(format!("{peer} didn't sign its receipt of it: {why}"));
    }
    let c = checked(root, v.get("receipt").unwrap_or(&Value::Null), peer).map_err(|why| format!("{peer}'s receipt of it: {why}"))?;
    let is_receipt = custody::body(&c).is_ok_and(|b| b.kind == Kind::Received);
    if !is_receipt || !c.statement.follows.iter().any(|f| f == hand_on) {
        return Err(format!("{peer}'s receipt isn't of its hand-on {hand_on}"));
    }
    match sha256_of(&c) {
        Some((_, got)) if got == want => Ok(()),
        Some((name, got)) => Err(format!("{peer} received {name} as sha256:{got}, not the sha256:{want} handed on")),
        None => Err(format!("{peer}'s receipt names nothing by its SHA-256")),
    }
}

/// The asking side's receipt of run `id`'s output from `peer`: its hand-on in `frame` (as it rode
/// the link), what this side got of it (`got`, its SHA-256), signed and sent to kiln. Why it's
/// refused, if it is: no hand-on came, it doesn't verify, or what came isn't what was handed on
/// (the receipt of what came then is the break kiln records).
pub fn take_output(me: Option<&Signer>, root: &[u8], peer: &str, id: u64, frame: Option<&str>, got: &str) -> Result<(), String> {
    let v: Value = frame.and_then(|f| serde_json::from_str(f).ok()).ok_or_else(|| format!("{peer} handed on no output of run {id}"))?;
    if let Some(why) = v.get("unsigned").and_then(Value::as_str) {
        return Err(format!("{peer} didn't sign its hand-on of run {id}'s output: {why}"));
    }
    let bundle = v.get("hand_on").cloned().unwrap_or(Value::Null);
    let c = checked(root, &bundle, peer).map_err(|why| format!("{peer}'s hand-on of run {id}'s output: {why}"))?;
    let to_me = custody::body(&c).ok().filter(|b| b.kind == Kind::HandedOn).and_then(|b| b.to);
    let Some(me) = me else { return Ok(()) };
    if !to_me.as_deref().is_some_and(|t| custody::names(t, &me.uri) || t == me.uri) {
        return Err(format!("{peer} handed run {id}'s output on to {}, not to {}", to_me.unwrap_or_default(), me.uri));
    }
    if let Some(r) = me.sign(&custody::reach_receipt(&me.uri, &c.custodian, None, custody::reach_output(id, got), &c.digest, None, "its run's output")) {
        to_kiln(&[&bundle, &r]);
    }
    match sha256_of(&c) {
        Some((_, want)) if want == got => Ok(()),
        Some((_, want)) => Err(format!("its output isn't what {peer} handed on: it came as sha256:{got}, handed on as sha256:{want}")),
        None => Err(format!("{peer}'s hand-on of run {id}'s output names it by no SHA-256")),
    }
}

// ------------------------------------------------------------------------------ the doing side

/// What the doing side took a run as, for its hand-on of the run's output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Took {
    /// Whom the output goes to: the session whose run it is, or the asking runner's process.
    pub to: String,
    /// The receipt of the run, which the output is made from.
    pub receipt: String,
}

/// What the doing side made of a frame the asking side sent ahead of something: its hand-on and
/// delegation, checked; or why the asker didn't sign; or why it isn't taken.
enum Asked {
    Signed(Box<Offered>),
    Unsigned(String),
}

/// A signed hand-on that came ahead of something, checked, as it came, and the delegation that
/// came with it.
struct Offered {
    hand_on: Checked,
    bundle: Value,
    delegation: Option<(Checked, Value)>,
}

/// What `peer` sent ahead of something, checked against kiln's root `root` (`me`, when this process
/// has an identity: it must be to it): why it isn't taken, if it isn't.
fn asked(root: &[u8], peer: &str, me: Option<&Signer>, frame: Option<&Value>) -> Result<Asked, String> {
    let Some(v) = frame else { return Err(format!("no hand-on of it came from {peer}")) };
    if let Some(why) = v.get("unsigned").and_then(Value::as_str) {
        return Ok(Asked::Unsigned(why.to_owned()));
    }
    let bundle = v.get("hand_on").cloned().unwrap_or(Value::Null);
    let h = checked(root, &bundle, peer).map_err(|why| format!("{peer}'s hand-on of it: {why}"))?;
    let b = custody::body(&h).map_err(|e| format!("{peer}'s hand-on of it: {e}"))?;
    let to_me = |to: &str| me.is_none_or(|m| m.uri == to);
    if b.kind != Kind::HandedOn || !b.to.as_deref().is_some_and(to_me) {
        return Err(format!("{peer}'s hand-on of it is to {}, not to this runner", b.to.unwrap_or_default()));
    }
    // A session's: taken for it only under its delegation to this process.
    let delegation = match v.get("delegation") {
        None if h.custodian.contains("/agent/") => return Err(format!("it's agent session {}'s, which gave this runner no delegation to take it", unheld(&h.custodian))),
        None => None,
        Some(g) => {
            let c = checked(root, g, peer).map_err(|why| format!("its delegation: {why}"))?;
            let grant = custody::grant(&c).map_err(|why| format!("its delegation: {why}"))?;
            if !to_me(&grant.delegate) || c.custodian != h.custodian {
                return Err(format!("its delegation is {}'s to {}, not {}'s to this runner", c.custodian, grant.delegate, h.custodian));
            }
            Some((c, g.clone()))
        }
    };
    Ok(Asked::Signed(Box::new(Offered { hand_on: h, bundle, delegation })))
}

/// The doing side's receipt of what `peer` handed it (`thing`, as this side got it), from frame
/// `frame`: signed, sent to kiln after the hand-on and delegation it follows, and the frame that
/// says so to the asker (`key` names it). Why it's refused, if it is: no hand-on came, one that
/// doesn't verify, isn't to this process, comes without a session's delegation to it, or is of
/// other bytes than came (signed for all the same: it's the break kiln records at this hop).
fn receive(root: &[u8], peer: &str, key: (&str, Value), frame: Option<&Value>, thing: statement::Subject, why: &str) -> Result<(String, Option<Took>), String> {
    let receipt_frame = |r: Value| json!({ key.0: key.1.clone(), "receipt": r }).to_string();
    let unsigned = |why: &str| json!({ key.0: key.1.clone(), "unsigned": why }).to_string();
    let me = Signer::doer();
    let (hand_on, bundle, delegation) = match asked(root, peer, me.as_ref(), frame) {
        Ok(Asked::Signed(o)) => {
            let Offered { hand_on, bundle, delegation } = *o;
            (hand_on, bundle, delegation)
        }
        // The asker signs nothing: what it hands on is taken as from outside kiln, a named hop.
        Ok(Asked::Unsigned(said)) => {
            let r = me.as_ref().and_then(|me| outside(me, &format!("reach:{peer}"), thing, &format!("{why}; {peer} signed no hand-on: {said}")));
            if let Some(r) = &r {
                to_kiln(&[r]);
            }
            return Ok((r.map(receipt_frame).unwrap_or_else(|| unsigned("this runner signs no custody records")), None));
        }
        Err(why) => return Err(why),
    };
    let session = delegation.as_ref().map(|_| unheld(&hand_on.custodian));
    let got = thing.digest.get("sha256").cloned().unwrap_or_default();
    let want = sha256_of(&hand_on).map(|(_, d)| d.to_owned()).unwrap_or_default();
    let under = delegation.as_ref().map(|(c, _)| c.digest.clone());
    let changed = || format!("it isn't what {peer} handed on: it came as sha256:{got}, handed on as sha256:{want}");
    // Checked whether this side signs or not; signed, its receipt of what came is the record.
    let Some(r) = me.as_ref().and_then(|me| me.sign(&custody::reach_receipt(&me.uri, &hand_on.custodian, session.as_deref(), thing, &hand_on.digest, under.as_deref(), why))) else {
        return if got == want { Ok((unsigned("this runner signs no custody records"), None)) } else { Err(changed()) };
    };
    to_kiln(&delegation.iter().map(|(_, g)| g).chain([&bundle, &r]).collect::<Vec<_>>());
    if got != want {
        return Err(changed());
    }
    let to = session.unwrap_or_else(|| hand_on.custodian.clone());
    Ok((receipt_frame(r.clone()), Some(Took { to, receipt: crate::runner::custody::digest_of(&r) })))
}

/// The doing side takes copy `key` of `peer`'s workspace, which hashes to `tree` once committed,
/// as the frame (`frame`) `peer` sent ahead of the commit hands it on: the frame that says so to the
/// asker, or why the copy is refused.
pub fn take_workspace(root: &[u8], peer: &str, key: &str, frame: Option<&Value>, tree: &str) -> Result<String, String> {
    receive(root, peer, ("workspace", json!(key)), frame, custody::reach_workspace(key, tree), "a workspace to run in").map(|(f, _)| f)
}

/// The doing side takes run `spec` of `peer`'s reach session `session` (its workspace's tree
/// `tree`), as frame `frame` hands it on: the frame that says so, and what the run's output goes
/// back under; or why the run is refused.
pub fn take_run(root: &[u8], peer: &str, session: &str, spec: &RunSpec, tree: &str, frame: Option<&Value>) -> Result<(String, Option<Took>), String> {
    receive(root, peer, ("run", json!(spec.id)), frame, custody::reach_run(spec.id, &run_digest(session, spec, tree)), "a run to do for another machine")
}

/// What a runner from before (reach version 1) hands this one: taken as from outside kiln, its
/// receipt of `thing` signed and sent to kiln, a named hop that begins there. Nothing rides the link.
pub fn from_before(peer: &str, thing: statement::Subject, why: &str) -> Option<Took> {
    let me = Signer::doer()?;
    let from = format!("reach-v1:{peer}");
    let r = outside(&me, &from, thing, &format!("{why}; {peer} speaks reach version 1, so it signed no hand-on"))?;
    to_kiln(&[&r]);
    Some(Took { to: from, receipt: crate::runner::custody::digest_of(&r) })
}

/// `me`'s receipt of `thing` from outside kiln (`from`, none of kiln's identities), following
/// nothing: where its chain begins, a named hop.
fn outside(me: &Signer, from: &str, thing: statement::Subject, why: &str) -> Option<Value> {
    let mut s = custody::reach_receipt(&me.uri, from, None, thing, "", None, why);
    s.follows.clear();
    me.sign(&s)
}

/// The doing side's workspace of a runner from before: [`from_before`].
pub fn workspace_from_before(peer: &str, key: &str, tree: &str) {
    from_before(peer, custody::reach_workspace(key, tree), "a workspace to run in");
}

/// The doing side's run of a runner from before: [`from_before`].
pub fn run_from_before(peer: &str, session: &str, spec: &RunSpec, tree: &str) -> Option<Took> {
    from_before(peer, custody::reach_run(spec.id, &run_digest(session, spec, tree)), "a run to do for another machine")
}

/// The doing side's hand-on of run `id`'s output (its SHA-256 `sha256`) to whom it took the run
/// from (`took`), made from its receipt of the run: the frame that rides the link ahead of the run's
/// exit ([`super::wire::Say::Custody`]), the hand-on also sent to kiln.
pub fn output_hand_on(took: Option<&Took>, id: u64, sha256: &str) -> String {
    let unsigned = |why: &str| json!({ "output": id, "unsigned": why }).to_string();
    let (Some(me), Some(took)) = (Signer::doer(), took) else { return unsigned("this runner took the run under no signed receipt") };
    let h = custody::reach_hand_on(&me.uri, &took.to, None, custody::reach_output(id, sha256), vec![took.receipt.clone()], "the run's output");
    match me.sign(&h) {
        Some(b) => {
            to_kiln(&[&b]);
            json!({ "output": id, "hand_on": b }).to_string()
        }
        None => unsigned("its hand-on couldn't be signed"),
    }
}

/// Which frame a custody JSON is for: `("workspace", key)`, `("run", id)` or `("output", id)`.
pub fn frame_of(json: &str) -> Option<(String, Value, String)> {
    let v: Value = serde_json::from_str(json).ok()?;
    let o = v.as_object()?;
    ["workspace", "run", "output"].iter().find_map(|k| o.get(*k).map(|id| ((*k).to_owned(), v.clone(), id.as_str().map_or_else(|| id.to_string(), str::to_owned))))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W27-R08 (the Transfers plan's step 12): a run's digest is what both sides work out of what
    /// it says, never its environment's values; every other field moves it.
    #[test]
    fn w27_r08_a_runs_digest_names_what_it_runs_not_its_secrets() {
        let spec = RunSpec { id: 3, command: "cargo test".into(), cwd: Some("crates/kiln".into()), mirror: Some("kiln-1b2c3d4e5f".into()), env: vec![("TOKEN".into(), "s3cr3t".into()), ("MODE".into(), "fast".into())], ..RunSpec::default() };
        let base = run_digest("s1", &spec, "t1");
        assert_eq!(base, run_digest("s1", &RunSpec { env: vec![("MODE".into(), "slow".into()), ("TOKEN".into(), "other".into())], ..spec.clone() }, "t1"), "values aside, in any order");
        for (i, other) in [
            run_digest("s2", &spec, "t1"),
            run_digest("s1", &RunSpec { id: 4, ..spec.clone() }, "t1"),
            run_digest("s1", &RunSpec { command: "cargo test -q".into(), ..spec.clone() }, "t1"),
            run_digest("s1", &RunSpec { cwd: None, ..spec.clone() }, "t1"),
            run_digest("s1", &RunSpec { mirror: None, ..spec.clone() }, "t1"),
            run_digest("s1", &RunSpec { env: Vec::new(), ..spec.clone() }, "t1"),
            run_digest("s1", &RunSpec { stdin: true, ..spec.clone() }, "t1"),
            run_digest("s1", &spec, "t2"),
        ]
        .iter()
        .enumerate()
        {
            assert_ne!(*other, base, "edit {i} moves it");
        }
        assert_eq!(unheld("kiln://o/person/ada/agent/s/epoch/1/machine/m/instance/i"), "kiln://o/person/ada/agent/s/epoch/1");
        assert_eq!(unheld("kiln://o/person/ada/machine/m/instance/i"), "kiln://o/person/ada/machine/m/instance/i");
    }

    #[test]
    fn frames_say_what_they_are_for() {
        assert_eq!(frame_of(r#"{"workspace":"kiln-1","unsigned":"x"}"#).map(|(k, _, id)| (k, id)), Some(("workspace".to_owned(), "kiln-1".to_owned())));
        assert_eq!(frame_of(r#"{"run":3,"receipt":{}}"#).map(|(k, _, id)| (k, id)), Some(("run".to_owned(), "3".to_owned())));
        assert_eq!(frame_of(r#"{"output":3,"hand_on":{}}"#).map(|(k, _, id)| (k, id)), Some(("output".to_owned(), "3".to_owned())));
        assert!(frame_of("{}").is_none() && frame_of("not json").is_none());
    }
}
