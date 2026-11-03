//! What a leg signs of what it did and made (docs/ci.md W27-R05, R06), as the custodian of what it
//! hands on: one statement at its end of its results, signed as the leg on its runner, and the
//! provenance kiln writes of what it built, signed as the leg once it's of what the leg made.
//!
//! - **Its results** ([`statement`], payload type [`PAYLOAD_RESULTS`]): a kiln statement
//!   ([`crate::identity::statement`]) numbered 1 on the leg's [`STREAM`]. Its subjects are each
//!   step's log by its chunks' chain (`log/<step>`, digest [`crate::logs::DIGEST`], its bytes its
//!   size and its chunks in its annotations) and each test report the leg uploaded by its SHA-256
//!   (`report/<step>/<path>`). Its body is each step's end (status, exit code) and the leg's
//!   conclusion.
//! - **Read back** ([`read`]) from a statement kiln took, and **judged** ([`judge`]) against what
//!   kiln keeps of the leg. Each break is said: a step's end that isn't the leg's, a log kiln
//!   changed or lost, or keeps as the leg's though the leg signed none of it, a report that isn't
//!   the one the leg uploaded or that either side lacks.
//! - **Provenance** ([`provenance_is_the_legs`], [`materials_are_received`]): the statement kiln
//!   writes of what a leg built is the leg's to sign only when its subjects are what the leg's
//!   attest step reported (its hand-ons) and its materials are among what it received.
//! - **Its custody** ([`custody`], on its [`CUSTODY`] stream): the Transfers plan's custody
//!   records, a receipt of each thing it received (its assignment by what it says the leg is to do,
//!   [`assignment`]; its checkout; its inputs) and its hand-on of what it made, which follows them.
//! - **Under kiln's delegation** ([`delegation`], [`under_kilns_delegation`]): as kiln certifies a
//!   leg, its server delegates the leg to it, signed as itself: the leg's identity its `delegate`,
//!   what it may do ([`MAY`]), until its certificate ends, about the commit it checks out. Each of
//!   the leg's records names that delegation in `under`, and is taken only when it acts under it.
//!
//! No clock, disk or network.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Subject;
use crate::identity::statement::{self, Statement};
use crate::logs::{self, Chain};

/// A leg's results' payload type.
pub const PAYLOAD_RESULTS: &str = "application/vnd.kiln.ci.results+json";
/// The stream a leg numbers its results on: it signs them once, at its end.
pub const STREAM: &str = "results";
/// What kiln's delegation of a leg entitles it to (the Transfers plan's `may`): to take its
/// assignment, its checkout and its inputs, and to run.
pub const MAY: [&str; 4] = ["receive:assignment", "receive:checkout", "receive:input", "act:run"];

/// A step's end, as the leg signs it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepEnd {
    pub idx: u32,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// The leg's end.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conclusion {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A results statement's body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Body {
    #[serde(default)]
    pub steps: Vec<StepEnd>,
    pub conclusion: Conclusion,
}

/// A test report a leg uploaded: its step, its path, its SHA-256 (hex).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub idx: u32,
    pub path: String,
    pub sha256: String,
}

/// What a leg hands on at its end, as it signs it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Results {
    /// Each step's end, by its index.
    pub steps: BTreeMap<u32, StepEnd>,
    /// Each step's log, as its chain.
    pub logs: BTreeMap<u32, Chain>,
    pub reports: Vec<Report>,
    pub conclusion: Conclusion,
}

/// The statement of a leg's results: unsigned, its signing the caller's (the runner's, with the
/// leg's key).
pub fn statement(r: &Results) -> Statement {
    let body = Body { steps: r.steps.values().cloned().collect(), conclusion: r.conclusion.clone() };
    let mut st = Statement::new(serde_json::to_value(&body).unwrap_or(Value::Null));
    st.subject = r
        .logs
        .iter()
        .map(|(idx, c)| statement::Subject {
            name: format!("log/{idx}"),
            digest: BTreeMap::from([(logs::DIGEST.to_owned(), c.head_hex())]),
            size: Some(c.bytes),
            annotations: BTreeMap::from([("chunks".to_owned(), Value::from(c.chunks))]),
        })
        .chain(r.reports.iter().map(|rep| statement::Subject { name: format!("report/{}/{}", rep.idx, rep.path), digest: BTreeMap::from([("sha256".to_owned(), rep.sha256.to_ascii_lowercase())]), ..statement::Subject::default() }))
        .collect();
    st.stream = Some(STREAM.to_owned());
    st.seq = Some(1);
    st
}

/// A leg's results, read back from its statement, or why it isn't one.
pub fn read(st: &Statement) -> Result<Results, String> {
    let body: Body = serde_json::from_value(st.body.clone()).map_err(|e| format!("its body isn't a leg's results: {e}"))?;
    let mut r = Results { steps: body.steps.into_iter().map(|s| (s.idx, s)).collect(), conclusion: body.conclusion, ..Results::default() };
    for s in &st.subject {
        if let Some(idx) = s.name.strip_prefix("log/") {
            let idx: u32 = idx.parse().map_err(|_| format!("its subject {:?} names no step", s.name))?;
            let head = s.digest.get(logs::DIGEST).ok_or_else(|| format!("its subject {} has no {} digest", s.name, logs::DIGEST))?;
            let (Some(bytes), Some(chunks)) = (s.size, s.annotations.get("chunks").and_then(Value::as_u64)) else {
                return Err(format!("its subject {} doesn't say its size and chunks", s.name));
            };
            let chain = Chain::from_hex(head, chunks, bytes).ok_or_else(|| format!("its subject {}'s digest isn't a chain's head", s.name))?;
            r.logs.insert(idx, chain);
        } else if let Some(rest) = s.name.strip_prefix("report/") {
            let (idx, path) = rest.split_once('/').ok_or_else(|| format!("its subject {:?} names no step and path", s.name))?;
            let idx: u32 = idx.parse().map_err(|_| format!("its subject {:?} names no step", s.name))?;
            let sha256 = s.digest.get("sha256").ok_or_else(|| format!("its subject {} has no sha256 digest", s.name))?;
            r.reports.push(Report { idx, path: path.to_owned(), sha256: sha256.clone() });
        } else {
            return Err(format!("its subject {:?} is neither a log nor a report", s.name));
        }
    }
    Ok(r)
}

/// What kiln keeps of a leg, held against what the leg signed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Kept {
    /// Each step's end as kiln recorded it: its status and exit code.
    pub steps: BTreeMap<u32, (String, Option<i32>)>,
    /// Each signed log, as what kiln keeps of it stands to the leg's chain (`Logs::check`).
    pub logs: BTreeMap<u32, logs::Verdict>,
    /// The reports kiln took for the leg.
    pub reports: Vec<Report>,
    /// The leg's end as kiln recorded it.
    pub conclusion: Option<(String, Option<i32>)>,
    /// Each step whose log kiln keeps chunks of, as taken from the leg (its latest attempt's, how
    /// many), that the leg signed no log of.
    pub unsigned_logs: BTreeMap<u32, u64>,
    /// The steps whose results the leg received reused ([`reused`]): their reports are their
    /// source leg's, which kiln copied over, not the leg's to sign.
    pub reused: BTreeSet<u32>,
}

/// Each break between what a leg signed and what kiln keeps of it, said; none when they agree.
/// Steps kiln ended itself and the leg didn't (a leg cancelled before them) aren't the leg's to
/// sign, so they aren't held against it, nor are a reused step's reports (its source leg's). A log
/// kiln keeps as taken from the leg is, signed or not: one the leg signed none of isn't the leg's.
pub fn judge(signed: &Results, kept: &Kept) -> Vec<String> {
    let mut out = Vec::new();
    for s in signed.steps.values() {
        match kept.steps.get(&s.idx) {
            None => out.push(format!("step {}: kiln has no end for it; the leg signed {}", s.idx, said(&s.status, s.exit_code))),
            Some((status, exit)) if *status != s.status || *exit != s.exit_code => {
                out.push(format!("step {}: kiln has it {}; the leg signed {}", s.idx, said(status, *exit), said(&s.status, s.exit_code)));
            }
            Some(_) => {}
        }
    }
    for (idx, chain) in &signed.logs {
        match kept.logs.get(idx) {
            Some(logs::Verdict::Verified { .. }) => {}
            Some(logs::Verdict::Altered { piece, at }) => out.push(format!("step {idx}'s log: its piece {piece} (at byte {at}) isn't what the leg sent")),
            Some(logs::Verdict::Incomplete { have, want }) => out.push(format!("step {idx}'s log: kiln took {have} of the {want} chunks the leg sent")),
            Some(logs::Verdict::Differs) => out.push(format!("step {idx}'s log: kiln took another log than the one the leg sent")),
            None if chain.chunks > 0 => out.push(format!("step {idx}'s log: kiln keeps none of the {} chunks the leg sent", chain.chunks)),
            None => {}
        }
    }
    for (idx, chunks) in kept.unsigned_logs.iter().filter(|(idx, n)| **n > 0 && !signed.logs.contains_key(idx)) {
        out.push(format!("step {idx}'s log: kiln keeps {chunks} chunk{} of it as the leg's, and the leg signed none", if *chunks == 1 { "" } else { "s" }));
    }
    for r in &signed.reports {
        match kept.reports.iter().find(|k| k.idx == r.idx && k.path == r.path) {
            None => out.push(format!("report {} of step {}: kiln has none", r.path, r.idx)),
            Some(k) if !k.sha256.eq_ignore_ascii_case(&r.sha256) => out.push(format!("report {} of step {}: kiln keeps another than the one the leg uploaded", r.path, r.idx)),
            Some(_) => {}
        }
    }
    for k in kept.reports.iter().filter(|k| !kept.reused.contains(&k.idx)) {
        if !signed.reports.iter().any(|r| r.idx == k.idx && r.path == k.path) {
            out.push(format!("report {} of step {}: kiln has it, and the leg didn't sign it", k.path, k.idx));
        }
    }
    let c = &signed.conclusion;
    match &kept.conclusion {
        Some((status, exit)) if *status != c.status || *exit != c.exit_code => out.push(format!("the leg's end: kiln has it {}; the leg signed {}", said(status, *exit), said(&c.status, c.exit_code))),
        _ => {}
    }
    out
}

fn said(status: &str, exit: Option<i32>) -> String {
    match exit {
        Some(code) => format!("{status} (exit code {code})"),
        None => status.to_owned(),
    }
}

/// Whether `statement` (the in-toto statement kiln wrote of what a leg built) is the leg's to sign:
/// SLSA provenance whose subjects are exactly those the leg's attest step `reported`. Why not,
/// otherwise.
pub fn provenance_is_the_legs(statement: &Value, reported: &[Subject]) -> Result<(), String> {
    if statement.get("_type").and_then(Value::as_str) != Some(super::STATEMENT) {
        return Err("it isn't an in-toto statement".into());
    }
    if statement.get("predicateType").and_then(Value::as_str) != Some(super::PROVENANCE) {
        return Err("it isn't SLSA provenance".into());
    }
    let mut named: Vec<Subject> = serde_json::from_value(statement.get("subject").cloned().unwrap_or(Value::Null)).map_err(|e| format!("its subjects: {e}"))?;
    let mut ours = reported.to_vec();
    let order = |a: &Subject, b: &Subject| a.name.cmp(&b.name).then_with(|| a.digest.cmp(&b.digest));
    named.sort_by(order);
    ours.sort_by(order);
    if named != ours {
        return Err(format!("its subjects ({}) aren't the {} this leg's attest step reported", named.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", "), ours.len()));
    }
    Ok(())
}

/// Whether the provenance `statement` kiln wrote of what a leg built names as its materials
/// (SLSA's resolved dependencies) only things the leg `received`: each by a digest one of its
/// receipts agrees on, as the Transfers plan matches a hop, a checkout by the server's tarball by
/// the commit it was of. Why not, otherwise, naming each it didn't receive: a leg never vouches
/// for what it didn't receive, such as an action a later step fetches.
pub fn materials_are_received(statement: &Value, received: &[statement::Subject]) -> Result<(), String> {
    let mut missing = Vec::new();
    for dep in statement.pointer("/predicate/buildDefinition/resolvedDependencies").and_then(Value::as_array).into_iter().flatten() {
        let named = dep.get("uri").or_else(|| dep.get("name")).and_then(Value::as_str).unwrap_or("a material");
        // One named by no digest is nothing the leg can say it received.
        let Some(digest) = dep.get("digest").and_then(Value::as_object).filter(|d| !d.is_empty()) else {
            missing.push(format!("{named} (no digest)"));
            continue;
        };
        if !received.iter().any(|got| among(digest, got)) {
            let digests: Vec<String> = digest.iter().map(|(alg, hex)| format!("{alg} {}", short(hex.as_str().unwrap_or_default()))).collect();
            missing.push(format!("{named} ({})", digests.join(", ")));
        }
    }
    match missing.as_slice() {
        [] => Ok(()),
        [one] => Err(format!("its material {one} isn't among what the leg received")),
        all => Err(format!("its materials {} aren't among what the leg received", all.join("; "))),
    }
}

/// Whether a material named by `digest` is what a leg received as `got`, as a hop's two records
/// are matched ([`agree`]).
fn among(digest: &serde_json::Map<String, Value>, got: &statement::Subject) -> bool {
    let material = statement::Subject { name: got.name.clone(), digest: digest.iter().filter_map(|(alg, h)| Some((alg.clone(), h.as_str()?.to_ascii_lowercase()))).collect(), ..statement::Subject::default() };
    agree(&material, got).is_ok()
}

/// The delegation kiln's server (`by`, its identity's URI) gives a leg as it certifies it
/// (payload type [`statement::PAYLOAD_DELEGATION`], the Transfers plan's body): its `delegate` the
/// leg's identity `uri`, [`MAY`], `until` its certificate ends (unix seconds), about what kiln
/// handed it when it knows that: the `commit` it checks out, and its `assignment` by its receipt's
/// digest ([`assignment`]). Unsigned: the server signs it as itself.
pub fn delegation(by: &str, uri: &str, commit: Option<&str>, assignment: Option<&str>, until: i64) -> Statement {
    let mut st = Statement::new(serde_json::json!({ "v": 1, "by": by, "delegate": uri, "may": MAY, "until": until }));
    let named = |name: &str, alg: &str, hex: &str| statement::Subject { name: name.into(), digest: BTreeMap::from([(alg.to_owned(), hex.to_ascii_lowercase())]), ..statement::Subject::default() };
    st.subject = commit.map(|c| named("checkout", "gitCommit", c)).into_iter().chain(assignment.map(|a| named("assignment", "sha256", a))).collect();
    st
}

/// The stream a leg numbers its custody records on: what it received, then what it handed on.
pub const CUSTODY: &str = "custody";

/// What a leg's receipt of its assignment (an `AssignmentV2`, [`crate::wire`]) is over: the
/// fields that say what the leg is to do, as kiln sent them. Never its secrets' values (their
/// names stand for them), the values it masks, the environment its needed jobs' secret outputs
/// bring, its tokens or its integrations': a secret is named, never named by a digest of its
/// value. A field kiln adds later counts once it's here.
const ASSIGNED: [&str; 40] = [
    "job_id", "run_id", "attempt", "repo", "sha", "git_ref", "dialect", "leg", "executor", "container", "services", "job_container", "job_services", "steps", "workflow_env",
    "job_env", "workflow_defaults", "job_defaults", "outputs", "job_workflow", "environment_url", "environment", "contexts", "items", "timeout_minutes", "continue_on_error", "checkout",
    "actions", "exports_env", "secret_outputs", "run_env", "quarantine", "debug", "limits", "network", "trusted", "warm", "cold", "exclusive", "holds",
];

/// A leg's receipt of its assignment `v` (an `AssignmentV2`, as JSON): its SHA-256 (hex) over
/// [`ASSIGNED`]'s fields as canonical JSON ([`canonical`]), its secrets by name and its `github`
/// context without its token; and its secrets' names. kiln computes it of what it sent and the leg
/// of what it got, so the two meet whatever order either built its JSON in.
pub fn assignment(v: &Value) -> (String, Vec<String>) {
    let mut names: Vec<String> = v.get("secrets").and_then(Value::as_array).into_iter().flatten().filter_map(|s| s.get("name").and_then(Value::as_str)).map(str::to_owned).collect();
    names.sort();
    let mut kept: serde_json::Map<String, Value> = ASSIGNED.iter().filter_map(|k| v.get(*k).map(|x| ((*k).to_owned(), x.clone()))).collect();
    if let Some(github) = kept.get_mut("contexts").and_then(|c| c.get_mut("github")).and_then(Value::as_object_mut) {
        github.remove("token");
    }
    kept.insert("secrets".into(), Value::from(names.clone()));
    (crate::identity::chain::hex(&<sha2::Sha256 as sha2::Digest>::digest(canonical(&Value::Object(kept)))), names)
}

/// `v` as canonical JSON: each object's keys in order at every depth, no whitespace.
pub fn canonical(v: &Value) -> Vec<u8> {
    fn put(v: &Value, out: &mut Vec<u8>) {
        match v {
            Value::Object(o) => {
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort();
                out.push(b'{');
                for (i, k) in keys.into_iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    out.extend_from_slice(Value::from(k.as_str()).to_string().as_bytes());
                    out.push(b':');
                    if let Some(x) = o.get(k) {
                        put(x, out);
                    }
                }
                out.push(b'}');
            }
            Value::Array(a) => {
                out.push(b'[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    put(x, out);
                }
                out.push(b']');
            }
            scalar => out.extend_from_slice(scalar.to_string().as_bytes()),
        }
    }
    let mut out = Vec::new();
    put(v, &mut out);
    out
}

/// A thing a leg received, as it signs its receipt: by its digests (`subject`), from whom (an
/// identity's URI, or where it came from when that has none), why, and the records it follows: the
/// hand-on its giver signed of it (its runner's, [`runner_custody`]), else kiln's delegation of the
/// leg, else none (what came from where kiln has no identity begins its chain there).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub subject: statement::Subject,
    pub from: String,
    pub why: String,
    pub follows: Vec<String>,
}

/// A thing kiln's server handed a leg's runner process for the leg (the Transfers plan's step 11):
/// its assignment, its commit, the tarball it served it, by digest (`subject`), with the hand-on's
/// digest (`hand_on`) and its signer (`from`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handed {
    pub subject: statement::Subject,
    pub hand_on: String,
    pub from: String,
}

/// The runner process a leg's identity (`leg`, its URI) runs on, as its own identity's URI: the
/// same machine and instance, enrolled by the same person.
pub fn runner_of(leg: &str) -> Result<String, String> {
    use crate::identity::chain::{Actor, Identity};
    let me = Identity::parse(leg).map_err(|e| e.to_string())?;
    let Actor::Leg { enrolled_by, machine, instance, .. } = me.actor else { return Err(format!("{leg} isn't a leg's identity")) };
    Identity::new(me.org, Actor::Runner { enrolled_by, machine, instance }).uri().map_err(|e| e.to_string())
}

/// The custody records of runner process `by` for the leg it runs (its identity's URI `leg`), the
/// Transfers plan's step 11, unsigned, numbered on its `stream` for that leg, each with its digest:
/// a receipt of each thing kiln's server handed it for the leg (`handed`, each following its
/// hand-on); then a hand-on of each thing it hands the leg's executor (`to_leg`), following the
/// receipts of what it was made from: those sharing a digest with it and, for a checkout, the
/// commit's. What it held from before (a cache kept from an earlier leg) follows nothing. And, for
/// each of `to_leg` in turn, the hand-on the leg's receipt of it follows.
pub fn runner_custody(by: &str, leg: &str, handed: &[Handed], to_leg: &[statement::Subject], stream: &str) -> Result<(Vec<(Statement, String)>, Vec<String>), statement::Refusal> {
    let mut out: Vec<(Statement, String)> = Vec::new();
    let mut seq = 0;
    let mut record = |body: Value, subject: Vec<statement::Subject>, follows: Vec<String>| -> Result<String, statement::Refusal> {
        seq += 1;
        let mut st = Statement::new(body);
        st.subject = subject;
        st.follows = follows;
        st.stream = Some(stream.to_owned());
        st.seq = Some(seq);
        let digest = statement::digest(statement::PAYLOAD_CUSTODY, &statement::payload(&st)?);
        out.push((st, digest.clone()));
        Ok(digest)
    };
    let mut receipts: Vec<(&statement::Subject, String)> = Vec::new();
    for h in handed {
        let body = serde_json::json!({ "v": 1, "kind": "received", "by": by, "from": h.from, "why": format!("kiln handed it the leg's {}", h.subject.name) });
        receipts.push((&h.subject, record(body, vec![h.subject.clone()], vec![h.hand_on.clone()])?));
    }
    let shares = |a: &statement::Subject, b: &statement::Subject| a.digest.iter().any(|(alg, hex)| b.digest.get(alg) == Some(hex));
    let mut hand_ons = Vec::new();
    for t in to_leg {
        let follows: Vec<String> = receipts.iter().filter(|(s, _)| shares(s, t) || (t.name.starts_with("checkout") && s.name == "commit")).map(|(_, d)| d.clone()).collect();
        let body = serde_json::json!({ "v": 1, "kind": "handed-on", "by": by, "to": leg, "why": format!("the leg's {}, to its executor", t.name) });
        hand_ons.push(record(body, vec![t.clone()], follows)?);
    }
    Ok((out, hand_ons))
}

/// A leg's custody records, unsigned, in their order on its [`CUSTODY`] stream, each with its
/// digest ([`statement::digest`]), each naming the leg (`by`, its identity's URI) as its signer,
/// since a statement's digest is the same whoever signs it, all acting `under` the server's
/// delegation of the leg:
/// - a receipt (`received`) of each thing it `received`, following what its [`Receipt`] says: its
///   runner's hand-on of it, else that delegation, the server's words that gave it the leg;
/// - then its hand-on (`handed-on`) to kiln's `server` of what it made (`outputs`, its logs and
///   reports, its artifacts and caches, its results), following each receipt: the inputs it made
///   them from, which are its provenance's materials.
pub fn custody(by: &str, received: &[Receipt], outputs: Vec<statement::Subject>, server: &str, under: Option<&str>) -> Result<Vec<(Statement, String)>, statement::Refusal> {
    let mut out: Vec<(Statement, String)> = Vec::new();
    let mut seq = 0;
    let mut record = |body: Value, subject: Vec<statement::Subject>, follows: Vec<String>| -> Result<String, statement::Refusal> {
        seq += 1;
        let mut st = Statement::new(body);
        st.subject = subject;
        st.follows = follows;
        st.stream = Some(CUSTODY.to_owned());
        st.seq = Some(seq);
        st.under = under.map(str::to_owned);
        let digest = statement::digest(statement::PAYLOAD_CUSTODY, &statement::payload(&st)?);
        out.push((st, digest.clone()));
        Ok(digest)
    };
    let mut receipts = Vec::new();
    for r in received {
        let body = serde_json::json!({ "v": 1, "kind": "received", "by": by, "from": r.from, "why": r.why });
        receipts.push(record(body, vec![r.subject.clone()], r.follows.clone())?);
    }
    record(serde_json::json!({ "v": 1, "kind": "handed-on", "by": by, "to": server, "why": "what it made" }), outputs, receipts)?;
    Ok(out)
}

/// kiln's receipt of a leg's hand-on `h` of what it made, unsigned (the server, `by`, signs it as
/// itself): `received` from the leg, of the same things by the same digests, following it, the one
/// record on kiln's `stream` for that leg. kiln took custody of them so; whether what it keeps
/// still is them is judged whenever it's read.
pub fn receipt(by: &str, h: &statement::Checked, stream: &str) -> Statement {
    let mut st = Statement::new(serde_json::json!({ "v": 1, "kind": "received", "by": by, "from": h.custodian, "why": "the leg's hand-on of what it made" }));
    st.subject.clone_from(&h.statement.subject);
    st.follows = vec![h.digest.clone()];
    st.stream = Some(stream.to_owned());
    st.seq = Some(1);
    st
}

/// What a leg's custody is judged by ([`judge_custody`]), each record as kiln checked it: the
/// leg's custody records, kiln's receipts of its hand-on, its results, the delegation that gave it
/// the leg, whose subjects are what kiln handed it (its assignment, its commit), and the tarballs
/// kiln served it (`served`, each `checkout` by the SHA-256 of what kiln sent and the commit it
/// was of).
#[derive(Clone, Debug, Default)]
pub struct Taken<'a> {
    pub legs: Vec<&'a statement::Checked>,
    pub kilns: Vec<&'a statement::Checked>,
    pub results: Option<&'a statement::Checked>,
    pub delegation: Option<&'a statement::Checked>,
    pub served: Vec<statement::Subject>,
}

/// A custody record's kind (`received`, `handed-on`, …).
pub fn kind(c: &statement::Checked) -> &str {
    c.statement.body.get("kind").and_then(Value::as_str).unwrap_or_default()
}

/// The steps whose results a leg's custody records say it received reused (`reused/<step>`, W24's
/// replays): what kiln copied into them is their source leg's.
pub fn reused(records: &[&statement::Checked]) -> BTreeSet<u32> {
    records.iter().filter(|c| kind(c) == "received").flat_map(|c| c.statement.subject.iter()).filter_map(|s| s.name.strip_prefix("reused/")?.parse().ok()).collect()
}

/// Each break in a leg's custody, said: a record missing from its stream (or numbered twice); a
/// hand-on of what it made that isn't there, isn't one, or doesn't name its results; one kiln
/// signed no receipt of, or a receipt of kiln's that follows none; and a receipt of the leg's that
/// isn't of what kiln handed it (its assignment, which every leg receives, and its commit, when it
/// checks that out).
pub fn judge_custody(t: &Taken<'_>) -> Vec<String> {
    let mut breaks: Vec<String> = statement::gaps(t.legs.iter().copied()).iter().map(ToString::to_string).collect();
    let hand_ons: Vec<&statement::Checked> = t.legs.iter().copied().filter(|c| kind(c) == "handed-on").collect();
    match hand_ons.as_slice() {
        [] => breaks.push("the leg signed no hand-on of what it made".into()),
        [h] => {
            let named = h.statement.subject.iter().find(|s| s.name == "results").and_then(|s| s.digest.get("sha256"));
            if named.map(String::as_str) != t.results.and_then(|r| r.digest.strip_prefix("sha256:")) {
                breaks.push("its hand-on doesn't name the results kiln took".into());
            }
        }
        more => breaks.push(format!("the leg signed {} hand-ons of what it made; a leg signs one", more.len())),
    }
    for h in &hand_ons {
        match t.kilns.iter().find(|k| k.statement.follows.contains(&h.digest)) {
            None => breaks.push(format!("kiln signed no receipt of the leg's hand-on {}", short(&h.digest))),
            Some(k) if k.statement.subject != h.statement.subject => breaks.push(format!("kiln's receipt of the leg's hand-on {} names other things", short(&h.digest))),
            Some(_) => {}
        }
    }
    for k in &t.kilns {
        if !hand_ons.iter().any(|h| k.statement.follows.contains(&h.digest)) {
            breaks.push(format!("kiln's receipt {} follows no hand-on of the leg's", short(&k.digest)));
        }
    }
    let receipts: Vec<&statement::Subject> = t.legs.iter().filter(|c| kind(c) == "received").flat_map(|c| &c.statement.subject).collect();
    for handed in t.delegation.map(|d| d.statement.subject.as_slice()).unwrap_or_default() {
        match receipts.iter().find(|r| r.name == handed.name) {
            Some(got) => {
                if let Err(why) = agree(handed, got) {
                    breaks.push(format!("{}: {why}", handed.name));
                }
            }
            None if handed.name == "assignment" => breaks.push("assignment: kiln handed it, the leg signed no receipt of it".into()),
            None => {}
        }
    }
    breaks.extend(tarball_breaks(&t.served, &receipts));
    breaks
}

/// A checkout by the server's tarball (its receipt names it by its SHA-256) held to the tarballs
/// kiln served the leg (`served`): one of them, by the bytes kiln sent. A receipt of one kiln never
/// served, or of other bytes, is a break.
pub fn tarball_breaks(served: &[statement::Subject], receipts: &[&statement::Subject]) -> Vec<String> {
    let mut breaks = Vec::new();
    for got in receipts.iter().filter(|r| r.name == "checkout").filter_map(|r| Some((*r, r.digest.get("sha256")?))) {
        let (receipt, sha256) = got;
        let of_it: Vec<&statement::Subject> = served.iter().filter(|s| s.name == "checkout").collect();
        if of_it.iter().any(|s| agree(s, receipt).is_ok()) {
            continue;
        }
        match of_it.first().and_then(|s| s.digest.get("sha256")) {
            Some(sent) => breaks.push(format!("checkout: kiln served the tarball sha256 {}, the leg received {}", short(sent), short(sha256))),
            None => breaks.push(format!("checkout: the leg received a tarball (sha256 {}) kiln has no record of serving it", short(sha256))),
        }
    }
    breaks
}

/// Whether what kiln handed (`handed`) and what the leg received (`got`) are one thing, as the
/// Transfers plan matches a hop: they share an algorithm and agree on each they share. A tarball's
/// checkout names the commit it was of beside its own digest.
fn agree(handed: &statement::Subject, got: &statement::Subject) -> Result<(), String> {
    let shared: Vec<(&String, &String, &String)> = handed.digest.iter().filter_map(|(alg, h)| got.digest.get(alg).map(|g| (alg, h, g))).collect();
    if shared.is_empty() {
        return match (handed.digest.get("gitCommit"), got.annotations.get("commit").and_then(Value::as_str)) {
            (Some(h), Some(g)) if h.eq_ignore_ascii_case(g) => Ok(()),
            (Some(h), Some(g)) => Err(format!("kiln handed gitCommit {}, the leg received {}", short(h), short(g))),
            _ => Err("no common digest: what kiln handed and what the leg received share no algorithm".into()),
        };
    }
    match shared.into_iter().find(|(_, h, g)| h != g) {
        Some((alg, h, g)) => Err(format!("kiln handed {alg} {}, the leg received {}", short(h), short(g))),
        None => Ok(()),
    }
}

/// A digest's first 12 hex digits.
fn short(d: &str) -> &str {
    let hex = d.strip_prefix("sha256:").unwrap_or(d);
    hex.get(..12).unwrap_or(hex)
}

/// Whether a leg's record `u`, checked through kiln's records ([`statement::verify_under`]), acts
/// under kiln's delegation of the leg: under one delegation, signed by kiln's server (`server`,
/// its URI). Why not, otherwise.
pub fn under_kilns_delegation(u: &statement::Under, server: &str) -> Result<(), String> {
    match u.through.as_slice() {
        [] => Err("it acts under no delegation: a leg's records act under kiln's delegation of the leg".into()),
        [d] if d.custodian == server => Ok(()),
        [d] => Err(format!("its delegation is signed by {}, not by kiln's server", d.signer)),
        more => Err(format!("it acts under {} delegations, one under another; a leg's act under kiln's alone", more.len())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::statement;

    fn results() -> Results {
        let mut r = Results {
            conclusion: Conclusion { status: "success".into(), exit_code: Some(0), error: None },
            reports: vec![Report { idx: 1, path: "target/junit.xml".into(), sha256: "ab".repeat(32) }],
            ..Results::default()
        };
        for (idx, status) in [(0u32, "success"), (1, "success")] {
            r.steps.insert(idx, StepEnd { idx, status: status.into(), exit_code: Some(0) });
        }
        r.logs.insert(0, Chain::of([b"$ cargo build\n".as_slice()]));
        r.logs.insert(1, Chain::of([b"$ cargo test\n".as_slice(), b"test result: ok\n".as_slice()]));
        r
    }

    /// W27-R05: a leg's results become a statement that's well formed (its payload is what
    /// identity::statement signs) and read back as they were.
    #[test]
    fn w27_r05_a_legs_results_read_back_from_their_statement() -> Result<(), String> {
        let r = results();
        let st = statement(&r);
        let payload = statement::payload(&st).map_err(|e| e.to_string())?;
        let again: Statement = serde_json::from_slice(&payload).map_err(|e| e.to_string())?;
        assert_eq!(read(&again)?, r);
        assert_eq!((st.stream.as_deref(), st.seq), (Some(STREAM), Some(1)));
        let log = st.subject.iter().find(|s| s.name == "log/1").ok_or("no log/1 subject")?;
        assert!(log.digest.contains_key(logs::DIGEST) && !log.digest.contains_key("sha256"));
        assert_eq!((log.size, log.annotations.get("chunks")), (r.logs.get(&1).map(|c| c.bytes), Some(&serde_json::json!(2))), "a log's bytes are its size, its chunks an annotation");
        // Not a leg's results: another kind of body, a subject neither a log nor a report, or a
        // log that doesn't say its size and chunks.
        assert!(read(&Statement::new(serde_json::json!({"kind": "received"}))).is_err());
        let mut odd = st.clone();
        odd.subject.push(statement::Subject { name: "image/app".into(), digest: BTreeMap::from([("sha256".into(), "cd".repeat(32))]), ..statement::Subject::default() });
        assert!(read(&odd).is_err());
        let mut uncounted = st.clone();
        uncounted.subject.iter_mut().filter(|s| s.name == "log/1").for_each(|s| s.annotations.clear());
        assert!(read(&uncounted).is_err());
        Ok(())
    }

    /// What kiln keeps of a leg that is what it signed of `r`.
    fn kept_of(r: &Results) -> Kept {
        Kept {
            steps: r.steps.values().map(|s| (s.idx, (s.status.clone(), s.exit_code))).collect(),
            logs: r.logs.keys().map(|idx| (*idx, logs::Verdict::Verified { kept: 10, dropped: 0 })).collect(),
            reports: r.reports.clone(),
            conclusion: Some(("success".into(), Some(0))),
            ..Kept::default()
        }
    }

    /// W27-R05: what kiln keeps is held against what the leg signed, and each break is said: a
    /// step's end, a log changed or short, a report swapped or added, the leg's end.
    #[test]
    fn w27_r05_each_break_between_the_leg_and_kiln_is_said() {
        let r = results();
        let mut kept = kept_of(&r);
        assert_eq!(judge(&r, &kept), Vec::<String>::new());
        // A step kiln ended itself and the leg didn't sign isn't held against it.
        kept.steps.insert(2, ("skipped".into(), None));
        assert_eq!(judge(&r, &kept), Vec::<String>::new());
        kept.steps.insert(1, ("failure".into(), Some(101)));
        kept.logs.insert(0, logs::Verdict::Altered { piece: 0, at: 0 });
        kept.logs.insert(1, logs::Verdict::Incomplete { have: 1, want: 2 });
        kept.reports = vec![Report { idx: 1, path: "target/junit.xml".into(), sha256: "ef".repeat(32) }, Report { idx: 1, path: "extra.xml".into(), sha256: "12".repeat(32) }];
        kept.conclusion = Some(("cancelled".into(), None));
        let breaks = judge(&r, &kept);
        assert_eq!(breaks.len(), 6, "{breaks:#?}");
        assert!(breaks.iter().any(|b| b.starts_with("step 1: kiln has it failure (exit code 101); the leg signed success (exit code 0)")), "{breaks:#?}");
        assert!(breaks.iter().any(|b| b.contains("kiln took 1 of the 2 chunks")), "{breaks:#?}");
        assert!(breaks.iter().any(|b| b.contains("extra.xml") && b.contains("didn't sign")), "{breaks:#?}");
        assert!(breaks.iter().any(|b| b.starts_with("the leg's end: kiln has it cancelled")), "{breaks:#?}");
    }

    /// W27-R05: a log kiln keeps as taken from the leg, of a step the leg signed no log of, is a
    /// break, saying how much of it kiln keeps; a reused step's reports, its source leg's that kiln
    /// copied over, aren't held against the leg.
    #[test]
    fn w27_r05_a_log_the_leg_signed_none_of_is_a_break() {
        let r = results();
        let mut kept = kept_of(&r);
        // The logs it signed are checked as they are, not counted again.
        kept.unsigned_logs = r.logs.keys().map(|idx| (*idx, 3)).collect();
        assert_eq!(judge(&r, &kept), Vec::<String>::new());
        kept.unsigned_logs.insert(7, 2);
        kept.unsigned_logs.insert(8, 0);
        assert_eq!(judge(&r, &kept), ["step 7's log: kiln keeps 2 chunks of it as the leg's, and the leg signed none"]);
        kept.unsigned_logs.clear();
        kept.reports.push(Report { idx: 5, path: "junit.xml".into(), sha256: "cd".repeat(32) });
        assert!(judge(&r, &kept).iter().any(|b| b.starts_with("report junit.xml of step 5: kiln has it, and the leg didn't sign it")), "{:#?}", judge(&r, &kept));
        kept.reused.insert(5);
        assert_eq!(judge(&r, &kept), Vec::<String>::new(), "step 5's result was reused: its report is its source leg's");
    }

    /// W27-R05: a leg's receipt of its assignment is over what it says the leg is to do, whatever
    /// order its JSON was built in. A secret's value, a mask, a token or an integration's change
    /// nothing in it, and a secret is named, never named by its value's digest; a step changed
    /// changes it.
    #[test]
    fn w27_r05_an_assignments_receipt_is_over_what_the_leg_is_to_do() -> Result<(), String> {
        let a = serde_json::json!({
            "job_id": "j1", "sha": "4c1e2a", "steps": [{ "run": "make test" }],
            "secrets": [{ "name": "NPM_TOKEN", "value": "s3cr3t" }, { "name": "AWS_KEY", "value": "k3y" }],
            "masks": ["s3cr3t"], "env_from": [["KILN_OUT", "s3cr3t"]], "runtime": { "runtime_token": "t0k3n" }, "integrations": [{ "token": "z" }],
            "contexts": { "github": { "sha": "4c1e2a", "token": "ghs_x" } },
        });
        let (digest, names) = assignment(&a);
        assert_eq!(names, ["AWS_KEY", "NPM_TOKEN"]);
        let reordered: Value = serde_json::from_str(r#"{"contexts":{"github":{"token":"ghs_y","sha":"4c1e2a"}},"steps":[{"run":"make test"}],"sha":"4c1e2a","job_id":"j1","secrets":[{"value":"other","name":"AWS_KEY"},{"name":"NPM_TOKEN","value":"other"}]}"#).map_err(|e| e.to_string())?;
        assert_eq!(assignment(&reordered).0, digest, "the same work, its keys in another order, its secrets' values and tokens others");
        let mut changed = a.clone();
        if let Some(run) = changed.pointer_mut("/steps/0/run") {
            *run = Value::from("make deploy");
        }
        assert_ne!(assignment(&changed).0, digest);
        let said = String::from_utf8(canonical(&a)).map_err(|e| e.to_string())?;
        assert!(said.starts_with(r#"{"contexts":{"github":{"sha":"4c1e2a","token":"ghs_x"}},"env_from""#), "{said}");
        Ok(())
    }

    /// W27-R06: a leg vouches for provenance only when its materials are among what it received,
    /// each by a digest a receipt agrees on: the commit it checked out, an action at its pin.
    #[test]
    fn w27_r06_provenance_names_only_materials_the_leg_received() {
        let commit = "4c1e2a90d1f3b5a7c9e1f3a5b7c9d1e3f5a7b9c1";
        let checkout = statement::Subject { name: "checkout".into(), digest: BTreeMap::from([("gitCommit".into(), commit.into()), ("gitTree".into(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904".into())]), ..statement::Subject::default() };
        let of = |deps: Value| serde_json::json!({ "predicate": { "buildDefinition": { "resolvedDependencies": deps } } });
        let source = serde_json::json!({ "uri": "git+https://kiln.home/acme/app@refs/heads/main", "digest": { "gitCommit": commit.to_ascii_uppercase() } });
        assert_eq!(materials_are_received(&of(serde_json::json!([source.clone()])), std::slice::from_ref(&checkout)), Ok(()));
        // A checkout by the server's tarball is of the commit it names.
        let tarball = statement::Subject { name: "checkout".into(), digest: BTreeMap::from([("sha256".into(), "ab".repeat(32))]), annotations: BTreeMap::from([("commit".into(), Value::from(commit))]), ..statement::Subject::default() };
        assert_eq!(materials_are_received(&of(serde_json::json!([source.clone()])), std::slice::from_ref(&tarball)), Ok(()));
        let action = serde_json::json!({ "uri": "git+https://github.com/actions/upload-artifact@v4", "digest": { "gitCommit": "ea165f8d65b6e75b540449e92b4886f43607fa02" } });
        assert_eq!(materials_are_received(&of(serde_json::json!([source.clone(), action.clone()])), std::slice::from_ref(&checkout)), Err("its material git+https://github.com/actions/upload-artifact@v4 (gitCommit ea165f8d65b6) isn't among what the leg received".into()));
        let image = serde_json::json!({ "uri": "docker://postgres:16", "digest": { "sha256": "cd".repeat(32) } });
        assert_eq!(
            materials_are_received(&of(serde_json::json!([source, action, image])), std::slice::from_ref(&checkout)),
            Err("its materials git+https://github.com/actions/upload-artifact@v4 (gitCommit ea165f8d65b6); docker://postgres:16 (sha256 cdcdcdcdcdcd) aren't among what the leg received".into())
        );
        let other = serde_json::json!({ "uri": "git+https://kiln.home/acme/app@refs/heads/main", "digest": { "gitCommit": "7d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f7a8b9c0" } });
        assert!(materials_are_received(&of(serde_json::json!([other])), std::slice::from_ref(&checkout)).is_err());
        assert_eq!(materials_are_received(&of(serde_json::json!([])), &[]), Ok(()));
        let unnamed = serde_json::json!({ "uri": "git+https://kiln.home/acme/lib@v1" });
        assert_eq!(materials_are_received(&of(serde_json::json!([unnamed])), std::slice::from_ref(&checkout)), Err("its material git+https://kiln.home/acme/lib@v1 (no digest) isn't among what the leg received".into()));
    }


    /// W27-R05: a checkout by the server's tarball is held to the tarballs kiln served the leg, by
    /// the bytes kiln sent: the same bytes hold; other bytes, or a tarball kiln never served, are
    /// each a break; a checkout with git is no tarball's.
    #[test]
    fn w27_r05_a_tarball_checkout_is_judged_by_what_kiln_served() {
        let commit = "4c1e2a90d1f3b5a7c9e1f3a5b7c9d1e3f5a7b9c1";
        let named = |digests: &[(&str, String)], annotated: Option<&str>| statement::Subject {
            name: "checkout".into(),
            digest: digests.iter().map(|(a, h)| ((*a).to_owned(), h.clone())).collect(),
            annotations: annotated.map(|c| BTreeMap::from([("commit".to_owned(), Value::from(c))])).unwrap_or_default(),
            ..statement::Subject::default()
        };
        let served = vec![named(&[("sha256", "ab".repeat(32)), ("gitCommit", commit.to_owned())], None)];
        let same = named(&[("sha256", "ab".repeat(32))], Some(commit));
        assert_eq!(tarball_breaks(&served, &[&same]), Vec::<String>::new());
        let other = named(&[("sha256", "cd".repeat(32))], Some(commit));
        assert_eq!(tarball_breaks(&served, &[&other]), vec!["checkout: kiln served the tarball sha256 abababababab, the leg received cdcdcdcdcdcd".to_owned()]);
        assert_eq!(tarball_breaks(&[], &[&same]), vec!["checkout: the leg received a tarball (sha256 abababababab) kiln has no record of serving it".to_owned()]);
        let with_git = named(&[("gitCommit", commit.to_owned()), ("gitTree", "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_owned())], None);
        assert_eq!(tarball_breaks(&[], &[&with_git]), Vec::<String>::new(), "a checkout with git is no tarball's");
    }

    /// W27-R05, R06: kiln's delegation of a leg is a well-formed statement naming the leg as its
    /// delegate, what it may do and until when, about the commit it checks out (when kiln knows it).
    #[test]
    fn w27_r05_kilns_delegation_names_the_leg_and_its_commit() -> Result<(), String> {
        let leg = "kiln://hyper-light/person/ada/machine/fedora-2/instance/01a174ed/run/812/attempt/1/leg/e2e";
        let d = delegation("kiln://hyper-light/server", leg, Some("4C1E2A90D1F3B5A7C9E1F3A5B7C9D1E3F5A7B9C1"), Some(&"ab".repeat(32)), 1_793_340_000);
        statement::payload(&d).map_err(|e| e.to_string())?;
        assert_eq!((d.body.get("v"), d.body.get("delegate").and_then(Value::as_str)), (Some(&serde_json::json!(1)), Some(leg)));
        // Its signer named in it, and when it ends in unix seconds, as the Transfers plan's grants are.
        assert_eq!((d.body.get("by").and_then(Value::as_str), d.body.get("until").and_then(Value::as_i64)), (Some("kiln://hyper-light/server"), Some(1_793_340_000)));
        assert_eq!(d.body.get("may"), Some(&serde_json::json!(MAY)));
        let checkout = d.subject.first().ok_or("no subject")?;
        assert_eq!((checkout.name.as_str(), checkout.digest.get("gitCommit").map(String::as_str)), ("checkout", Some("4c1e2a90d1f3b5a7c9e1f3a5b7c9d1e3f5a7b9c1")));
        assert_eq!(d.subject.get(1).map(|s| (s.name.as_str(), s.digest.get("sha256").cloned())), Some(("assignment", Some("ab".repeat(32)))));
        assert!(delegation("kiln://hyper-light/server", leg, None, None, 1_793_340_000).subject.is_empty(), "nothing known, nothing named");
        Ok(())
    }

    /// W27-R06: the provenance kiln writes is the leg's to sign only when its subjects are the
    /// ones the leg's attest step reported, whatever their order.
    #[test]
    fn w27_r06_provenance_is_the_legs_only_when_its_subjects_are() {
        let a = Subject { name: "dist/app".into(), digest: BTreeMap::from([("sha256".into(), "aa".repeat(32))]) };
        let b = Subject { name: "dist/lib".into(), digest: BTreeMap::from([("sha256".into(), "bb".repeat(32))]) };
        let prov = super::super::statement(&[a.clone(), b.clone()], super::super::PROVENANCE, serde_json::json!({}));
        assert_eq!(provenance_is_the_legs(&prov, &[b.clone(), a.clone()]), Ok(()));
        assert!(provenance_is_the_legs(&prov, std::slice::from_ref(&a)).is_err());
        let swapped = Subject { digest: BTreeMap::from([("sha256".into(), "cc".repeat(32))]), ..b.clone() };
        assert!(provenance_is_the_legs(&prov, &[a.clone(), swapped]).is_err());
        let sbom = super::super::statement(&[a.clone(), b.clone()], super::super::SPDX, serde_json::json!({}));
        assert!(provenance_is_the_legs(&sbom, &[a, b]).is_err());
    }
}
