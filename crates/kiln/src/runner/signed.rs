//! What a leg receives and hands on, kept by its runner process as the leg goes for the leg to
//! sign (docs/ci.md W27-R05, R06), by digest:
//! - what it received, each from whom: its assignment ([`assigned`]), its checkout and its inputs
//!   (artifacts, caches, actions, images: [`received`]);
//! - what it hands on: each step's log, as the chain of the chunks sent ([`kiln_ci::logs`]), and
//!   each step's end; the test reports it uploaded; its artifacts and caches ([`handed_on`]);
//! - its attest steps' subjects, so the provenance kiln writes is signed as the leg only when it's
//!   of what the leg reported.
//!
//! The uplink's loop folds in what it sends, as it sends it (`agent.rs`), and the leg's steps say
//! what they took and gave. At the leg's end it signs, with its key while this process still holds
//! it ([`crate::runner::identity`]) and under the server's delegation of it ([`sign_records`]): its
//! results (one statement, [`kiln_ci::attest::leg`]), then its custody records, a receipt of each
//! thing it received and its hand-on of what it made, its results among them. Only digests are
//! kept here, never the key, and a secret only by its name.

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use kiln_ci::attest::{Subject, leg};
use kiln_ci::identity::chain::{Actor, Identity};
use kiln_ci::identity::statement;
use kiln_ci::logs::Chain;
use kiln_ci::wire::JobEventV2;
use parking_lot::Mutex;

use crate::proto::JobResult;
use crate::store::Status;

/// Whom a leg received a thing from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Giver {
    /// kiln's server: its git facade, results service, cache and mirror.
    Server,
    /// The leg's runner process, which kept it from an earlier leg.
    Runner,
    /// Where it came from, which has no identity of kiln's (an image's registry:
    /// `docker://<image>`).
    Elsewhere(String),
}

#[derive(Default)]
struct Leg {
    logs: BTreeMap<u32, Chain>,
    steps: BTreeMap<u32, (Status, Option<i32>)>,
    reports: Vec<leg::Report>,
    /// Each attest step's subjects, by the report's nonce.
    attests: HashMap<String, Vec<Subject>>,
    /// What it received, in the order it did: each thing, from whom, why.
    received: Vec<(statement::Subject, Giver, String)>,
    /// What it handed on as it went (artifacts, caches), besides its logs and reports.
    handed: Vec<statement::Subject>,
    /// kiln's server's hand-ons of the leg's inputs to this runner process (docs/ci.md W27-R08):
    /// its assignment and commit as it gave the leg, the tarball as it served it. Bundles.
    from_kiln: Vec<serde_json::Value>,
}

static LEGS: LazyLock<Mutex<HashMap<String, Leg>>> = LazyLock::new(Default::default);

/// A thing named `name` by `digests` (algorithm, lowercase hex), as a custody record names it.
pub fn thing(name: impl Into<String>, digests: impl IntoIterator<Item = (&'static str, String)>) -> statement::Subject {
    statement::Subject { name: name.into(), digest: digests.into_iter().map(|(a, h)| (a.to_owned(), h.to_ascii_lowercase())).collect(), ..statement::Subject::default() }
}

/// Leg `job`'s assignment as it came (`AssignmentV2`'s JSON), received from the server: by what it
/// says the leg is to do ([`leg::assignment`]), its secrets by name.
pub fn assigned(job: &str, json: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return };
    let (digest, secrets) = leg::assignment(&v);
    let mut assignment = thing("assignment", [("sha256", digest)]);
    if !secrets.is_empty() {
        assignment.annotations.insert("secrets".into(), secrets.into());
    }
    received(job, assignment, Giver::Server, "its assignment");
}

/// A thing leg `job` received, by its digests, from whom and why ("its checkout"): once, however
/// often the leg takes it.
pub fn received(job: &str, what: statement::Subject, from: Giver, why: &str) {
    if !well_formed(&what) {
        return tracing::debug!(job, thing = %what.name, "a thing received with a digest no record takes isn't kept");
    }
    let mut legs = LEGS.lock();
    let l = legs.entry(job.to_owned()).or_default();
    if !l.received.iter().any(|(s, f, _)| *s == what && *f == from) {
        l.received.push((what, from, why.to_owned()));
    }
}

/// kiln's server handed this runner process an input of leg `job` (`bundle`, its signed hand-on,
/// the Transfers plan's step 11): kept, once, for this process's receipt of it at the leg's end.
pub fn kiln_handed(job: &str, bundle: serde_json::Value) {
    let mut legs = LEGS.lock();
    let l = legs.entry(job.to_owned()).or_default();
    if !l.from_kiln.contains(&bundle) {
        l.from_kiln.push(bundle);
    }
}

/// kiln's hand-on (`hand_on`, as it came with what kiln served leg `job`: a tarball, an action, an
/// artifact; docs/ci.md W27-R08) against what this runner process got (`got`, by its digests).
/// Signed by kiln (checked against its root, `ca`, when this process has it) and of what came, it's
/// kept for this process's receipt of it at the leg's end. Else what's wrong, said after the
/// thing's name ("… isn't what kiln handed on (…)"), and, for other bytes, this process's receipt
/// of what it got is signed and sent at once ([`got_other`]): the leg doesn't run on it.
pub fn check_handed(job: &str, hand_on: serde_json::Value, ca: Option<&[u8]>, got: statement::Subject) -> Result<(), String> {
    if let Some(ca) = ca
        && let Err(e) = statement::verify(&hand_on, ca, &[], crate::util::now_unix())
    {
        return Err(format!("came with a hand-on of kiln's that doesn't verify ({e})"));
    }
    let Some(said) = crate::runner::custody::statement_of(&hand_on).and_then(|s| s.subject.into_iter().next()) else { return Ok(()) };
    let other = said.digest.iter().find_map(|(alg, hex)| got.digest.get(alg).filter(|g| !g.eq_ignore_ascii_case(hex)).map(|g| (alg.clone(), hex.clone(), g.clone())));
    match other {
        None => {
            kiln_handed(job, hand_on);
            Ok(())
        }
        Some((alg, said, got_hex)) => {
            got_other(&hand_on, got);
            Err(format!("isn't what kiln handed on (kiln said {alg} {said}, this runner got {got_hex})"))
        }
    }
}

/// This runner process got something other than what kiln's server handed it for a leg
/// (`hand_on`): its receipt of what it got (`got`, by its digests), following that hand-on, signed
/// as this process and sent at once, so kiln records the break at that hop (docs/ci.md W27-R08;
/// the Transfers plan's step 11). The leg doesn't run on it.
pub fn got_other(hand_on: &serde_json::Value, got: statement::Subject) {
    use crate::runner::custody::{digest_of, giver_of};
    let Some(by) = crate::runner::identity::uri("runner") else {
        return tracing::warn!(target: "kiln.custody", thing = %got.name, "this runner got other bytes than kiln handed on, and isn't certified to sign its receipt of them");
    };
    let body = serde_json::json!({ "v": 1, "kind": "received", "by": by, "from": giver_of(hand_on).unwrap_or_default(), "why": format!("the leg's {}, other bytes than kiln handed on: the leg doesn't run on them", got.name) });
    let mut st = statement::Statement::new(body);
    st.subject = vec![got];
    st.follows = vec![digest_of(hand_on)];
    match statement::payload(&st).map_err(anyhow::Error::from).and_then(|p| sign("runner", statement::PAYLOAD_CUSTODY, &p)) {
        Ok(b) => crate::runner::custody::send(&b),
        Err(e) => tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "this runner's receipt of other bytes than kiln handed on couldn't be signed"),
    }
}

/// A thing leg `job` handed on as it went (an artifact, a cache), by its digests.
pub fn handed_on(job: &str, what: statement::Subject) {
    if !well_formed(&what) {
        return tracing::debug!(job, thing = %what.name, "a thing handed on with a digest no record takes isn't kept");
    }
    LEGS.lock().entry(job.to_owned()).or_default().handed.push(what);
}

/// Whether a record can name `what`: its digests well formed (a malformed one would spoil the
/// leg's records whole).
fn well_formed(what: &statement::Subject) -> bool {
    let mut st = statement::Statement::new(serde_json::Value::Null);
    st.subject = vec![what.clone()];
    statement::payload(&st).is_ok()
}

/// A chunk of step `idx`'s log, as it's sent.
pub fn log(job: &str, idx: u32, data: &[u8]) {
    LEGS.lock().entry(job.to_owned()).or_default().logs.entry(idx).or_default().push(data);
}

/// Step `idx`'s end, as it's sent.
pub fn step(job: &str, idx: u32, status: Status, exit_code: Option<i32>) {
    LEGS.lock().entry(job.to_owned()).or_default().steps.insert(idx, (status, exit_code));
}

/// A test report step `idx` uploaded, its SHA-256 (hex).
pub fn report(job: &str, idx: u32, path: &str, sha256: &str) {
    LEGS.lock().entry(job.to_owned()).or_default().reports.push(leg::Report { idx, path: path.to_owned(), sha256: sha256.to_ascii_lowercase() });
}

/// A leg's event as it's sent (`AgentMsg::JobEvent`'s JSON): a step's end, its status as the
/// server records it; an attest step's report, its subjects under its nonce.
pub fn event(job: &str, json: &str) {
    match serde_json::from_str::<JobEventV2>(json) {
        Ok(JobEventV2::StepFinished { idx, outcome, conclusion, exit_code, .. }) => step(job, idx, crate::ci::runs::step_status(outcome, conclusion), exit_code),
        Ok(JobEventV2::Attest { nonce, subjects, .. }) => {
            LEGS.lock().entry(job.to_owned()).or_default().attests.insert(nonce, subjects);
        }
        _ => {}
    }
}

/// Lets go of what was kept for a leg (one whose server doesn't take signed results).
pub fn forget(job: &str) {
    LEGS.lock().remove(job);
}

/// The leg's records at its end `end`, signed as the leg under the server's delegation of it: a
/// JSON array of Sigstore bundles, its results, then its custody records (a receipt of each thing
/// it received, then its hand-on of what it made, its results among them). What was kept for it
/// goes.
pub fn sign_records(job: &str, end: &JobResult) -> anyhow::Result<String> {
    sign_records_with(job, end, &leg_slot(job))
}

/// [`sign_records`] with the key held under `slot`.
fn sign_records_with(job: &str, end: &JobResult, slot: &str) -> anyhow::Result<String> {
    let kept = LEGS.lock().remove(job).unwrap_or_default();
    let uri = crate::runner::identity::uri(slot).ok_or_else(|| anyhow::anyhow!("no identity is certified for {slot}"))?;
    let (server, runner) = peers(&uri)?;
    let under = crate::runner::identity::under(slot);
    let results = leg::Results {
        steps: kept.steps.iter().map(|(idx, (status, exit))| (*idx, leg::StepEnd { idx: *idx, status: status.as_str().to_owned(), exit_code: *exit })).collect(),
        logs: kept.logs,
        reports: kept.reports,
        conclusion: leg::Conclusion { status: end.status.as_str().to_owned(), exit_code: end.exit_code, error: end.error.clone() },
    };
    let mut st = leg::statement(&results);
    st.under.clone_from(&under);
    let payload = statement::payload(&st)?;
    let mut bundles = vec![sign(slot, leg::PAYLOAD_RESULTS, &payload)?];
    // What it hands on: its logs and reports as its results name them, what it handed on as it
    // went, and its results by their digest.
    let mut outputs = st.subject;
    outputs.extend(kept.handed);
    let digest = statement::digest(leg::PAYLOAD_RESULTS, &payload);
    outputs.push(thing("results", [("sha256", digest.trim_start_matches("sha256:").to_owned())]));
    // What this process gave the leg's executor, from kiln or from what it held: its receipt of
    // each thing kiln handed it for the leg and its hand-on of each to the leg, signed as this
    // process (docs/ci.md W27-R08), which the leg's receipts follow.
    let to_leg: Vec<statement::Subject> = kept.received.iter().filter(|(_, f, _)| matches!(f, Giver::Server | Giver::Runner)).map(|(s, _, _)| s.clone()).collect();
    let mut runners = runner_records(&uri, &kept.from_kiln, &to_leg).into_iter();
    let delegation: Vec<String> = under.clone().into_iter().collect();
    let received: Vec<leg::Receipt> = kept
        .received
        .into_iter()
        .map(|(subject, from, why)| {
            let (from, follows) = match from {
                // Where kiln has no identity: its chain begins at the leg's receipt.
                Giver::Elsewhere(at) => (at, Vec::new()),
                given => match (runners.next(), given) {
                    // From this process, which handed it to the leg's executor (W27-R08).
                    (Some(hand_on), _) => (runner.clone(), hand_on),
                    // Without its hand-on (kiln handed it nothing signed): under kiln's delegation.
                    (None, Giver::Server) => (server.clone(), delegation.clone()),
                    (None, _) => (runner.clone(), delegation.clone()),
                },
            };
            leg::Receipt { subject, from, why, follows }
        })
        .collect();
    for (record, _) in leg::custody(&uri, &received, outputs, &server, under.as_deref())? {
        bundles.push(sign(slot, statement::PAYLOAD_CUSTODY, &statement::payload(&record)?)?);
    }
    Ok(serde_json::to_string(&bundles)?)
}

/// This runner process's custody records of the inputs of the leg it runs (`leg`, its identity's
/// URI), the Transfers plan's step 11: its receipt of each hand-on kiln's server gave it for the
/// leg (`from_kiln`), and its hand-on of each thing it gives the leg's executor (`to_leg`), signed
/// as this process and sent on their own, ahead of the leg's (they aren't the leg's records). For
/// each of `to_leg` in turn, what the leg's receipt of it follows: this process's hand-on of it.
/// None when kiln handed it nothing (a server from before) or this process isn't certified, or one
/// didn't sign: the leg's receipts then follow kiln's delegation, as before.
fn runner_records(leg: &str, from_kiln: &[serde_json::Value], to_leg: &[statement::Subject]) -> Vec<Vec<String>> {
    use crate::runner::custody::{digest_of, giver_of, statement_of};
    // Records this process wouldn't send kiln (a pod's runner, a server from before) aren't signed.
    if !crate::runner::custody::sends() {
        return Vec::new();
    }
    let Some(by) = crate::runner::identity::uri("runner") else { return Vec::new() };
    let handed: Vec<leg::Handed> = from_kiln
        .iter()
        .filter_map(|b| Some(leg::Handed { subject: statement_of(b)?.subject.into_iter().next()?, hand_on: digest_of(b), from: giver_of(b)? }))
        .collect();
    if handed.is_empty() {
        return Vec::new();
    }
    let (records, hand_ons) = match leg::runner_custody(&by, leg, &handed, to_leg, &format!("for {leg}")) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "kiln.custody", %leg, error = %e, "this runner's receipts of the leg's inputs couldn't be written: the leg's follow kiln's delegation");
            return Vec::new();
        }
    };
    let mut signed = Vec::new();
    for (st, _) in &records {
        match statement::payload(st).map_err(anyhow::Error::from).and_then(|p| sign("runner", statement::PAYLOAD_CUSTODY, &p)) {
            Ok(b) => signed.push(b),
            Err(e) => {
                tracing::warn!(target: "kiln.custody", %leg, error = %format!("{e:#}"), "this runner's receipts of the leg's inputs couldn't be signed: the leg's follow kiln's delegation");
                return Vec::new();
            }
        }
    }
    for b in &signed {
        crate::runner::custody::send(b);
    }
    hand_ons.into_iter().map(|d| vec![d]).collect()
}

/// The leg's server and runner process, by their identities' URIs, from the leg's own (`uri`).
fn peers(uri: &str) -> anyhow::Result<(String, String)> {
    let me = Identity::parse(uri).map_err(|e| anyhow::anyhow!("{e}"))?;
    let Actor::Leg { enrolled_by, machine, instance, .. } = me.actor else { anyhow::bail!("{uri} isn't a leg's identity") };
    let server = Identity::new(me.org.clone(), Actor::Server).uri().map_err(|e| anyhow::anyhow!("{e}"))?;
    let runner = Identity::new(me.org, Actor::Runner { enrolled_by, machine, instance }).uri().map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((server, runner))
}

/// The provenance kiln wrote of what leg `job` built (`payload`, an in-toto statement), signed as
/// the leg once it's of the subjects its attest step `nonce` reported (its hand-ons) and its
/// materials are among what the leg received; why not, otherwise.
pub fn sign_provenance(job: &str, nonce: &str, payload_type: &str, payload: &[u8]) -> Result<String, String> {
    sign_provenance_with(job, nonce, payload_type, payload, &leg_slot(job))
}

/// [`sign_provenance`] with the key held under `slot`.
fn sign_provenance_with(job: &str, nonce: &str, payload_type: &str, payload: &[u8], slot: &str) -> Result<String, String> {
    let (reported, received) = {
        let legs = LEGS.lock();
        let l = legs.get(job);
        let reported = l.and_then(|l| l.attests.get(nonce).cloned()).ok_or_else(|| format!("this runner holds no attest report {nonce} of leg {job}"))?;
        (reported, l.map(|l| l.received.iter().map(|(s, _, _)| s.clone()).collect::<Vec<_>>()).unwrap_or_default())
    };
    if payload_type != kiln_ci::attest::PAYLOAD_TYPE {
        return Err(format!("{payload_type} isn't an in-toto statement's payload type"));
    }
    let st: serde_json::Value = serde_json::from_slice(payload).map_err(|e| format!("it isn't JSON: {e}"))?;
    leg::provenance_is_the_legs(&st, &reported)?;
    leg::materials_are_received(&st, &received)?;
    sign(slot, payload_type, payload).map(|b| b.to_string()).map_err(|e| format!("{e:#}"))
}

/// Where leg `job`'s key is held ([`crate::runner::identity`]).
fn leg_slot(job: &str) -> String {
    crate::runner::identity::slot("leg", Some(job), None)
}

/// How long a leg's first step waits for kiln to answer its identity (docs/ci.md W27-R05). kiln
/// answers in milliseconds; past this (a busy server, the network) the leg runs, and its records
/// are signed only if the identity comes before its end.
pub const IDENTITY_WAIT: Duration = Duration::from_secs(2);

/// Holds leg `job`'s first step until kiln has answered its identity, certified or refused,
/// [`IDENTITY_WAIT`] at most, so all the leg signs is signed as it: the provenance kiln asks of it
/// as it goes, its records at its end. A leg of one quick step ended before kiln's answer otherwise,
/// its records unsigned (docs/ci.md W27-R05). For a server announcing `CAP_LEG_SIGNS`. Logged when
/// it waits.
pub async fn identity_first(job: &str) {
    first_waits(job, &leg_slot(job), IDENTITY_WAIT).await;
}

/// [`identity_first`] for the key held under `slot`, `bound` at most: whether kiln answered.
async fn first_waits(job: &str, slot: &str, bound: Duration) -> bool {
    if !crate::runner::identity::awaited(slot) {
        return true;
    }
    let started = Instant::now();
    let waited = || u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    while crate::runner::identity::awaited(slot) {
        if started.elapsed() >= bound {
            tracing::warn!(job, waited_ms = waited(), "kiln hadn't answered the leg's identity in time: the leg runs, its records signed only if the identity comes before its end");
            return false;
        }
        crate::rt::sleep(Duration::from_millis(5)).await;
    }
    tracing::info!(job, waited_ms = waited(), "the leg's first step waited for kiln to answer its identity");
    true
}

/// `payload` of `payload_type`, signed over its PAE with the key held under `slot`, in a bundle
/// with its certificate.
fn sign(slot: &str, payload_type: &str, payload: &[u8]) -> anyhow::Result<serde_json::Value> {
    let (sig, cert) = crate::runner::identity::sign(slot, &statement::to_sign(payload_type, payload))?;
    Ok(statement::bundle(payload_type, payload, &sig, &crate::pki::der_of(&cert)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_ci::identity::chain::{Actor, Identity};

    /// W27-R08 (the Transfers plan's step 11): kiln's hand-on of what it served a leg (an artifact,
    /// an action, a tarball) is kept for this runner's receipt when it's of what came, and said to
    /// be of other bytes when it isn't, the leg not taking them.
    #[test]
    fn w27_r08_kilns_hand_on_is_held_to_what_came() -> anyhow::Result<()> {
        use base64::Engine as _;
        let job = "w27-r08-held-to-what-came";
        let mut st = statement::Statement::new(serde_json::json!({ "v": 1, "kind": "handed-on", "by": "kiln://hyper-light/server", "to": "kiln://hyper-light/person/admin/machine/m/instance/i" }));
        st.subject = vec![thing("artifact/app", [("sha256", "ab".repeat(32))])];
        let payload = base64::engine::general_purpose::STANDARD.encode(statement::payload(&st)?);
        let hand_on = serde_json::json!({ "dsseEnvelope": { "payloadType": statement::PAYLOAD_CUSTODY, "payload": payload, "signatures": [] } });
        assert_eq!(check_handed(job, hand_on.clone(), None, thing("artifact/app", [("sha256", "ab".repeat(32))])), Ok(()));
        assert!(LEGS.lock().get(job).is_some_and(|l| l.from_kiln.contains(&hand_on)), "kept for this runner's receipt of it");
        let other = check_handed(job, hand_on, None, thing("artifact/app", [("sha256", "cd".repeat(32))]));
        assert!(other.as_ref().is_err_and(|w| w.starts_with("isn't what kiln handed on (kiln said sha256 abab")), "{other:?}");
        forget(job);
        Ok(())
    }

    /// The server's delegation of the leg [`held`] gives, by its digest.
    const DELEGATION: &str = "sha256:d1e9a7e0c0ffee00d1e9a7e0c0ffee00d1e9a7e0c0ffee00d1e9a7e0c0ffee00";

    /// A key held here and certified by `pki` as a leg, under the server's [`DELEGATION`], as a
    /// runner holds one while the leg runs: its identity and where it's held. It's held as an
    /// agent's slot, so no test holds a leg's key, which keeps this process undumpable while
    /// runner::identity's own test looks.
    fn held(pki: &crate::pki::Pki, dir: &std::path::Path, job: &str) -> anyhow::Result<(Identity, String)> {
        let asked: crate::proto::IdentityAsk = serde_json::from_str(&crate::runner::identity::ask("agent", Some(job), Some(1))?)?;
        let identity = Identity::new(pki.organization(), Actor::Leg { enrolled_by: "ada".into(), machine: "fedora-2".into(), instance: "01a174ed".into(), run: 812, attempt: 1, leg: "e2e".into() });
        let facts = [(kiln_ci::identity::chain::fulcio::BUILD_SIGNER_URI, "https://kiln.home/hyper-light/kiln/.kiln/ci.yml@refs/heads/main"), (kiln_ci::identity::chain::fulcio::SOURCE_REPOSITORY_DIGEST, "4c1e2a90d1f3b5a7c9e1f3a5b7c9d1e3f5a7b9c1")];
        let pem = pki.issue_identity(&asked.csr, &identity, &facts, time::Duration::hours(1))?;
        let given = crate::proto::IdentityGiven { kind: asked.kind, of: asked.of, epoch: asked.epoch, uri: identity.uri()?, cert_pem: Some(pem), error: None, delegation: Some(DELEGATION.into()) };
        crate::runner::identity::given(&serde_json::to_string(&given)?, dir, "main")?;
        Ok((identity, crate::runner::identity::slot("agent", Some(job), Some(1))))
    }

    /// Leg `job`'s records signed at its end `end` with the key under `slot`, each checked as kiln
    /// would take it now.
    fn signed(pki: &crate::pki::Pki, job: &str, end: &JobResult, slot: &str) -> anyhow::Result<Vec<statement::Checked>> {
        let bundles: Vec<serde_json::Value> = serde_json::from_str(&sign_records_with(job, end, slot)?)?;
        Ok(bundles.iter().map(|b| statement::verify(b, &pki.ca_der, &[], crate::util::now_unix())).collect::<Result<_, _>>()?)
    }

    /// The results among a leg's records.
    fn results_in(records: &[statement::Checked]) -> anyhow::Result<&statement::Checked> {
        records.iter().find(|c| c.payload_type == leg::PAYLOAD_RESULTS).ok_or_else(|| anyhow::anyhow!("no results among the leg's records"))
    }

    /// W27-R05: what a leg sends is kept as it goes and signed at its end as the leg, on its
    /// runner, under the server's delegation of it: its results verify to kiln's root as the leg,
    /// name the delegation given with its certificate, and read back as what was sent.
    #[test]
    fn w27_r05_a_legs_results_are_signed_as_the_leg() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
        let job = "w27-r05-signed";
        let (identity, slot) = held(&pki, dir.path(), job)?;
        log(job, 0, b"$ cargo test\n");
        log(job, 0, b"test result: ok\n");
        step(job, 0, Status::Succeeded, Some(0));
        report(job, 0, "target/junit.xml", &"AB".repeat(32));
        // A v2 leg's step ends as an event, its status as the server records it: a failure
        // `continue-on-error` let pass is failed.
        event(job, r#"{"t":"step_finished","idx":1,"outcome":"failure","conclusion":"success","exit_code":3,"outputs":{}}"#);
        let end = JobResult { job: job.into(), status: Status::Succeeded, exit_code: Some(0), error: None, peak_rss: None, cpu_seconds: None };
        let records = signed(&pki, job, &end, &slot)?;
        let checked = results_in(&records)?;
        assert_eq!(checked.signer.identity, identity);
        assert_eq!(checked.payload_type, leg::PAYLOAD_RESULTS);
        assert_eq!(checked.statement.under.as_deref(), Some(DELEGATION));
        let r = leg::read(&checked.statement).map_err(anyhow::Error::msg)?;
        assert_eq!(r.logs.get(&0), Some(&Chain::of([b"$ cargo test\n".as_slice(), b"test result: ok\n".as_slice()])));
        assert_eq!(r.steps.get(&0).map(|s| (s.status.as_str(), s.exit_code)), Some(("succeeded", Some(0))));
        assert_eq!(r.steps.get(&1).map(|s| (s.status.as_str(), s.exit_code)), Some(("failed", Some(3))));
        assert_eq!(r.reports.first().map(|x| x.sha256.as_str()), Some("ab".repeat(32).as_str()));
        assert_eq!(r.conclusion.status, "succeeded");
        // What was kept went with it: signed again, it's results of nothing.
        let again = signed(&pki, job, &end, &slot)?;
        assert!(leg::read(&results_in(&again)?.statement).map_err(anyhow::Error::msg)?.logs.is_empty());
        crate::runner::identity::forget(&slot);
        assert!(sign_records_with(job, &end, &slot).is_err(), "a key let go of signs nothing");
        Ok(())
    }

    /// W27-R05: what a leg received and handed on is signed at its end as the leg, under the
    /// server's delegation of it, as custody records numbered on its stream with no gap: a receipt
    /// of each thing, from whom (its assignment's naming its secrets, never their values or its
    /// tokens), then its hand-on of what it made to the server, following each receipt and naming
    /// its results by their digest. A record dropped on the way is a gap.
    #[test]
    fn w27_r05_a_legs_receipts_and_hand_ons_are_signed_as_the_leg() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
        let job = "w27-r05-custody";
        let (identity, slot) = held(&pki, dir.path(), job)?;
        let commit = "4c1e2a90d1f3b5a7c9e1f3a5b7c9d1e3f5a7b9c1";
        let assignment = serde_json::json!({ "job_id": job, "sha": commit, "secrets": [{ "name": "NPM_TOKEN", "value": "s3cr3t-value" }], "runtime": { "runtime_token": "t0k3n-value" } });
        assigned(job, &assignment.to_string());
        received(job, thing("checkout", [("gitCommit", commit.to_owned()), ("gitTree", "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_owned())]), Giver::Server, "its checkout");
        received(job, thing("action/actions/setup-node@v4", [("sha256", "cd".repeat(32))]), Giver::Runner, "its action actions/setup-node@v4");
        log(job, 0, b"$ cargo test\n");
        step(job, 0, Status::Succeeded, Some(0));
        report(job, 0, "target/junit.xml", &"ab".repeat(32));
        handed_on(job, thing("artifact/app", [("sha256", "ef".repeat(32))]));
        let end = JobResult { job: job.into(), status: Status::Succeeded, exit_code: Some(0), error: None, peak_rss: None, cpu_seconds: None };
        let records = signed(&pki, job, &end, &slot)?;
        assert!(records.iter().all(|c| c.signer.identity == identity && c.statement.under.as_deref() == Some(DELEGATION)), "each signed as the leg, under its delegation");
        let me = identity.uri()?;
        assert!(records.iter().filter(|c| c.payload_type == statement::PAYLOAD_CUSTODY).all(|c| c.statement.body.get("by").and_then(serde_json::Value::as_str) == Some(me.as_str())), "each custody record names its signer");
        let custody: Vec<&statement::Checked> = records.iter().filter(|c| c.payload_type == statement::PAYLOAD_CUSTODY).collect();
        let said = |c: &statement::Checked, k: &str| c.statement.body.get(k).and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
        let named: Vec<(u64, String, String, String)> = custody.iter().map(|c| (c.statement.seq.unwrap_or(0), said(c, "kind"), c.statement.subject.first().map(|s| s.name.clone()).unwrap_or_default(), said(c, "from"))).collect();
        let (server, runner) = peers(&identity.uri()?)?;
        assert_eq!(
            named,
            vec![
                (1, "received".into(), "assignment".into(), server.clone()),
                (2, "received".into(), "checkout".into(), server.clone()),
                (3, "received".into(), "action/actions/setup-node@v4".into(), runner),
                (4, "handed-on".into(), "log/0".into(), String::new()),
            ]
        );
        assert!(statement::gaps(custody.iter().copied()).is_empty(), "numbered 1 to 4 on its custody stream");
        // Its assignment by what it says the leg is to do: its secret by name, no value or token.
        let (receipt, secrets) = leg::assignment(&assignment);
        let a = custody.first().and_then(|c| c.statement.subject.first()).ok_or_else(|| anyhow::anyhow!("no assignment"))?;
        assert_eq!((a.digest.get("sha256"), a.annotations.get("secrets")), (Some(&receipt), Some(&serde_json::json!(secrets))));
        let payloads = serde_json::to_string(&records.iter().map(|c| &c.statement).collect::<Vec<_>>())?;
        assert!(!payloads.contains("s3cr3t-value") && !payloads.contains("t0k3n-value"), "no secret's value or token in what the leg signed");
        // Its hand-on: to the server, following every receipt, of its log, report, artifact and results.
        let hand_on = custody.last().ok_or_else(|| anyhow::anyhow!("no hand-on"))?;
        assert_eq!((said(hand_on, "to"), hand_on.statement.follows.clone()), (server, custody.iter().take(3).map(|c| c.digest.clone()).collect::<Vec<_>>()));
        let outputs: Vec<&str> = hand_on.statement.subject.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(outputs, ["log/0", "report/0/target/junit.xml", "artifact/app", "results"]);
        let results = results_in(&records)?;
        assert_eq!(hand_on.statement.subject.last().and_then(|s| s.digest.get("sha256")).map(String::as_str), results.digest.strip_prefix("sha256:"));
        // Each receipt follows the delegation, the server's words that gave it the leg.
        assert!(custody.iter().take(3).all(|c| c.statement.follows == [DELEGATION]));
        // One dropped on the way is a gap in its stream.
        let dropped: Vec<&statement::Checked> = custody.iter().copied().filter(|c| c.statement.seq != Some(2)).collect();
        assert_eq!(statement::gaps(dropped), vec![statement::Gap::Missing { custodian: identity.uri()?, stream: leg::CUSTODY.into(), from: 2, to: 2 }]);
        crate::runner::identity::forget(&slot);
        Ok(())
    }

    /// W27-R05: a leg's custody, judged as kiln takes it: whole, and received by kiln, it holds. A
    /// record dropped on the way, kiln's receipt of its hand-on missing, or a receipt of the leg's
    /// that isn't of what kiln handed it (another commit than its delegation names), each said.
    #[test]
    fn w27_r05_a_legs_custody_is_judged_whole() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = crate::store::Store::in_memory()?;
            let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
            crate::identities::own_identities(&store, &pki).await?;
            let job = "w27-r05-judged";
            let (identity, slot) = held(&pki, dir.path(), job)?;
            let commit = "4c1e2a90d1f3b5a7c9e1f3a5b7c9d1e3f5a7b9c1";
            let assignment = serde_json::json!({ "job_id": job, "sha": commit });
            assigned(job, &assignment.to_string());
            received(job, thing("checkout", [("gitCommit", commit.to_owned()), ("gitTree", "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_owned())]), Giver::Server, "its checkout");
            log(job, 0, b"ok\n");
            step(job, 0, Status::Succeeded, Some(0));
            let end = JobResult { job: job.into(), status: Status::Succeeded, exit_code: Some(0), error: None, peak_rss: None, cpu_seconds: None };
            let records = signed(&pki, job, &end, &slot)?;
            let now = crate::util::now_unix();
            let as_server = |payload_type: &str, st: &statement::Statement| -> anyhow::Result<statement::Checked> {
                let bundle = crate::identities::sign_as(&pki, "server", payload_type, &statement::payload(st)?)?;
                Ok(statement::verify(&bundle, &pki.ca_der, &[], now)?)
            };
            let server = peers(&identity.uri()?)?.0;
            let handed = |commit: &str| as_server(statement::PAYLOAD_DELEGATION, &leg::delegation(&server, &identity.uri()?, Some(commit), Some(&leg::assignment(&assignment).0), now + 3600));
            let legs: Vec<&statement::Checked> = records.iter().filter(|c| c.payload_type == statement::PAYLOAD_CUSTODY).collect();
            let hand_on = legs.iter().copied().find(|c| leg::kind(c) == "handed-on").ok_or_else(|| anyhow::anyhow!("no hand-on"))?;
            let ours = as_server(statement::PAYLOAD_CUSTODY, &leg::receipt(&server, hand_on, &format!("leg/{job}/1")))?;
            let (delegation, results) = (handed(commit)?, results_in(&records)?);
            let judged = |legs: Vec<&statement::Checked>, kilns: Vec<&statement::Checked>, delegation: &statement::Checked| leg::judge_custody(&leg::Taken { legs, kilns, results: Some(results), delegation: Some(delegation), served: Vec::new() });
            assert_eq!(judged(legs.clone(), vec![&ours], &delegation), Vec::<String>::new(), "whole, and received by kiln");
            let missing = judged(legs.clone(), Vec::new(), &delegation);
            assert!(missing.iter().any(|b| b.starts_with("kiln signed no receipt of the leg's hand-on")), "{missing:?}");
            let dropped = judged(legs.iter().copied().filter(|c| c.statement.seq != Some(2)).collect(), vec![&ours], &delegation);
            assert!(dropped.iter().any(|b| b.ends_with("'s record 2 on custody is missing")), "{dropped:?}");
            let other = judged(legs.clone(), vec![&ours], &handed("7d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f7a8b9c0")?);
            assert!(other.iter().any(|b| b == "checkout: kiln handed gitCommit 7d0e1f2a3b4c, the leg received 4c1e2a90d1f3"), "{other:?}");
            crate::runner::identity::forget(&slot);
            Ok(())
        })
    }

    /// W27-R06: the provenance kiln writes is signed as the leg only when it's of the subjects the
    /// leg's attest step reported and its materials are among what the leg received; another's is
    /// refused, saying so.
    #[test]
    fn w27_r06_provenance_is_signed_as_the_leg_only_when_its_subjects_are_the_legs() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
        let job = "w27-r06-signed";
        let (identity, slot) = held(&pki, dir.path(), job)?;
        let app = Subject { name: "dist/app".into(), digest: BTreeMap::from([("sha256".into(), "aa".repeat(32))]) };
        let attest = JobEventV2::Attest { idx: 1, nonce: "n1".into(), subjects: vec![app.clone()], provenance: true, sbom: false };
        event(job, &serde_json::to_string(&attest)?);
        let prov = |subjects: &[Subject]| serde_json::to_vec(&kiln_ci::attest::statement(subjects, kiln_ci::attest::PROVENANCE, serde_json::json!({})));
        let bundle: serde_json::Value = serde_json::from_str(&sign_provenance_with(job, "n1", kiln_ci::attest::PAYLOAD_TYPE, &prov(std::slice::from_ref(&app))?, &slot).map_err(anyhow::Error::msg)?)?;
        let leaf = bundle.pointer("/verificationMaterial/certificate/rawBytes").and_then(serde_json::Value::as_str).unwrap_or_default();
        use base64::Engine as _;
        let leaf = base64::engine::general_purpose::STANDARD.decode(leaf)?;
        let payload = base64::engine::general_purpose::STANDARD.decode(bundle.pointer("/dsseEnvelope/payload").and_then(serde_json::Value::as_str).unwrap_or_default())?;
        let sig = base64::engine::general_purpose::STANDARD.decode(bundle.pointer("/dsseEnvelope/signatures/0/sig").and_then(serde_json::Value::as_str).unwrap_or_default())?;
        let v = kiln_ci::identity::chain::verify_signed(&[&leaf], &pki.ca_der, &[], crate::util::now_unix(), &kiln_ci::attest::pae(kiln_ci::attest::PAYLOAD_TYPE, &payload), &sig)?;
        assert_eq!(v.identity, identity);
        let other = Subject { name: "dist/other".into(), digest: BTreeMap::from([("sha256".into(), "bb".repeat(32))]) };
        assert!(sign_provenance_with(job, "n1", kiln_ci::attest::PAYLOAD_TYPE, &prov(&[other])?, &slot).is_err_and(|e| e.contains("aren't the 1")));
        assert!(sign_provenance_with(job, "n2", kiln_ci::attest::PAYLOAD_TYPE, &prov(std::slice::from_ref(&app))?, &slot).is_err_and(|e| e.contains("no attest report n2")));
        // Its materials: signed when they're among what the leg received, refused when one isn't.
        let commit = "4c1e2a90d1f3b5a7c9e1f3a5b7c9d1e3f5a7b9c1";
        received(job, thing("checkout", [("gitCommit", commit.to_owned())]), Giver::Server, "its checkout");
        let built_from = |deps: serde_json::Value| serde_json::to_vec(&kiln_ci::attest::statement(std::slice::from_ref(&app), kiln_ci::attest::PROVENANCE, serde_json::json!({ "buildDefinition": { "resolvedDependencies": deps } })));
        let source = serde_json::json!({ "uri": "git+https://kiln.home/acme/app@refs/heads/main", "digest": { "gitCommit": commit } });
        assert!(sign_provenance_with(job, "n1", kiln_ci::attest::PAYLOAD_TYPE, &built_from(serde_json::json!([source.clone()]))?, &slot).is_ok());
        let later = serde_json::json!({ "uri": "git+https://github.com/actions/upload-artifact@v4", "digest": { "gitCommit": "ea165f8d65b6e75b540449e92b4886f43607fa02" } });
        assert!(sign_provenance_with(job, "n1", kiln_ci::attest::PAYLOAD_TYPE, &built_from(serde_json::json!([source, later]))?, &slot).is_err_and(|e| e.contains("upload-artifact@v4 (gitCommit ea165f8d65b6) isn't among what the leg received")));
        forget(job);
        crate::runner::identity::forget(&slot);
        Ok(())
    }

    /// W27-R05: a leg's first step waits for its identity: not at all with none asked for, until
    /// kiln answers, certifying it or refusing it, and no longer than its bound, after which it runs.
    /// Its keys are agents' slots, as [`held`]'s are.
    #[test]
    fn w27_r05_a_legs_first_step_waits_for_its_identity() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
        let (state, slot) = (dir.path().to_path_buf(), |job: &str| crate::runner::identity::slot("agent", Some(job), Some(1)));
        let ask = |job: &str| -> anyhow::Result<crate::proto::IdentityAsk> { Ok(serde_json::from_str(&crate::runner::identity::ask("agent", Some(job), Some(1))?)?) };
        let identity = Identity::new(pki.organization(), Actor::Leg { enrolled_by: "ada".into(), machine: "fedora-2".into(), instance: "01a174ed".into(), run: 812, attempt: 1, leg: "e2e".into() });
        let asked = ask("w27-r05-certified")?;
        let pem = pki.issue_identity(&asked.csr, &identity, &[], time::Duration::hours(1))?;
        let certified = serde_json::to_string(&crate::proto::IdentityGiven { kind: asked.kind, of: asked.of, epoch: asked.epoch, uri: identity.uri()?, cert_pem: Some(pem), ..crate::proto::IdentityGiven::default() })?;
        ask("w27-r05-refused")?;
        let refused = serde_json::to_string(&crate::proto::IdentityGiven { kind: "agent".into(), of: Some("w27-r05-refused".into()), epoch: Some(1), error: Some("not now".into()), ..crate::proto::IdentityGiven::default() })?;
        ask("w27-r05-unanswered")?;
        let (certified_at, refused_at, unanswered_at) = (slot("w27-r05-certified"), slot("w27-r05-refused"), slot("w27-r05-unanswered"));
        let (a, b, c) = (certified_at.clone(), refused_at.clone(), unanswered_at.clone());
        crate::rt::block_on(async move {
            let t = Instant::now();
            assert!(first_waits("none", &slot("w27-r05-none"), Duration::from_secs(5)).await, "none asked for: nothing to wait for");
            assert!(t.elapsed() < Duration::from_secs(1));
            // Answered 100 ms after the leg's start, by kiln's certificate and by its refusal.
            for (s, answer) in [(a, certified), (b, refused)] {
                let st = state.clone();
                crate::rt::spawn(async move {
                    crate::rt::sleep(Duration::from_millis(100)).await;
                    let _ = crate::runner::identity::given(&answer, &st, "main");
                });
                let t = Instant::now();
                assert!(first_waits("e2e", &s, Duration::from_secs(5)).await, "{s}: answered");
                assert!((Duration::from_millis(90)..Duration::from_secs(5)).contains(&t.elapsed()), "{s}: until the answer, {:?}", t.elapsed());
            }
            let t = Instant::now();
            assert!(!first_waits("e2e", &c, Duration::from_millis(200)).await, "never answered");
            assert!(t.elapsed() >= Duration::from_millis(200), "its bound: {:?}", t.elapsed());
            Ok(())
        })?;
        assert!(crate::runner::identity::uri(&certified_at).is_some() && crate::runner::identity::uri(&refused_at).is_none());
        for s in [certified_at, refused_at, unanswered_at] {
            crate::runner::identity::forget(&s);
        }
        Ok(())
    }
}
