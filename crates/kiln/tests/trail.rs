//! W27-R09 (docs/ci.md; step 2 of the plan "One trail of facts, and kiln verify", kiln plan
//! `01a17856b1b670deb829ec1600f2c2fd`): every signed statement kiln takes goes into its trail,
//! once, in the order kiln takes them: a push's commits as kiln checked them, a leg's records with
//! the server's delegation and its receipt, a custody hand-off, a revocation. Each entry names the
//! hash of the one before, and the server's checkpoints over them verify against kiln's root, each
//! extending the one before. The trail survives a restart, and appends from many requests at once
//! never interleave its chain.
//!
//! `cargo test -p kiln --test trail`

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, Server, http, start};
use kiln_ci::identity::chain::{Actor, Identity};
use kiln_ci::identity::custody::{Body, Kind, to_value};
use kiln_ci::identity::statement::{self, PAYLOAD_CUSTODY, Statement, Subject};
use kiln_ci::identity::trail::{self, Checkpoint, Entry, Hash};
use rcgen::SigningKey as _;
use rusqlite::OptionalExtension as _;
use serde_json::{Value, json};

const TALK: Talk = Talk { lines: 1, every_ms: 10 };
const ORG: &str = "hyper-light";

/// The trail as `GET /api/trail` gives it from its start, a page at a time: each entry, with its
/// hash as kept.
fn trail_of(s: &Server) -> anyhow::Result<Vec<(Entry, String)>> {
    let mut out: Vec<(Entry, String)> = Vec::new();
    loop {
        let (st, body) = http(s.port, "GET", &format!("/api/trail?from={}&limit=1000", out.len()), &s.admin, None)?;
        anyhow::ensure!(st == 200, "GET /api/trail: {st} {body}");
        let page: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
        if page.is_empty() {
            return Ok(out);
        }
        for v in &page {
            let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
            let prev: Hash = hex::decode(text("prev"))?.try_into().map_err(|_| anyhow::anyhow!("an entry's prev isn't a SHA-256: {v}"))?;
            let subjects: Vec<String> = serde_json::from_value(v.get("subjects").cloned().unwrap_or_else(|| json!([])))?;
            let index = v.get("index").and_then(Value::as_u64).ok_or_else(|| anyhow::anyhow!("an entry without its index: {v}"))?;
            out.push((Entry { index, prev, statement: text("statement"), kind: text("kind"), subjects, taken_at: v.get("taken_at").and_then(Value::as_i64).unwrap_or_default() }, text("hash")));
        }
    }
}

/// Whether the trail holds whole: each entry hashes as kept, its chain holds from its start, and
/// its first entries make the root `checkpoint` names.
fn holds(t: &[(Entry, String)], checkpoint: &Checkpoint) -> anyhow::Result<()> {
    if let Some((e, _)) = t.iter().find(|(e, kept)| hex::encode(e.hash()) != *kept) {
        anyhow::bail!("entry {} doesn't hash as kept", e.index);
    }
    let entries: Vec<Entry> = t.iter().map(|(e, _)| e.clone()).collect();
    Ok(trail::check_root(&entries, checkpoint)?)
}

/// kiln's root (DER).
fn root_of(dir: &Path) -> anyhow::Result<Vec<u8>> {
    kiln::pki::der_of(&std::fs::read_to_string(dir.join("data/pki/ca.pem"))?)
}

/// kiln's checkpoints, the newest first, each read as its server's signed note, verified against
/// kiln's root as of when it was signed.
fn checkpoints(s: &Server, root: &[u8]) -> anyhow::Result<Vec<Checkpoint>> {
    use base64::Engine as _;
    let (st, body) = http(s.port, "GET", "/api/trail/checkpoints?limit=1000", &s.admin, None)?;
    anyhow::ensure!(st == 200, "GET /api/trail/checkpoints: {st} {body}");
    let list: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("checkpoints").and_then(Value::as_array).cloned().unwrap_or_default();
    list.iter()
        .map(|c| {
            let note = c.get("note").and_then(Value::as_str).unwrap_or_default();
            let cert = base64::engine::general_purpose::STANDARD.decode(c.get("certificate").and_then(Value::as_str).unwrap_or_default())?;
            let (cp, by) = trail::verify_note(note, ORG, &[&cert], root, &[], c.get("signed_at").and_then(Value::as_i64).unwrap_or_default())?;
            anyhow::ensure!(by.identity == Identity::new(ORG, Actor::Server), "a checkpoint signed by {}", by.identity);
            anyhow::ensure!(Some(cp.size) == c.get("size").and_then(Value::as_u64), "a checkpoint of {} kept as {c}", cp.size);
            Ok(cp)
        })
        .collect()
}

/// Each of `checkpoints` (the newest first) extends the one before it, by the consistency proof
/// over the trail's leaves.
fn each_extends(t: &[(Entry, String)], checkpoints: &[Checkpoint]) -> anyhow::Result<()> {
    let leaves: Vec<Hash> = t.iter().map(|(e, _)| e.hash()).collect();
    for pair in checkpoints.windows(2) {
        let [new, old] = pair else { continue };
        let upto = leaves.get(..usize::try_from(new.size)?).unwrap_or_default();
        let proof = trail::consistency(upto, usize::try_from(old.size)?).unwrap_or_default();
        trail::extends(old, new, &proof)?;
    }
    Ok(())
}

/// Waits up to `within` for `f` to say something.
fn until<T>(within: Duration, what: &str, mut f: impl FnMut() -> anyhow::Result<Option<T>>) -> anyhow::Result<T> {
    let deadline = Instant::now() + within;
    loop {
        if let Some(v) = f()? {
            return Ok(v);
        }
        anyhow::ensure!(Instant::now() < deadline, "{what}: not within {within:?}");
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Where statement `digest` is in the trail: its one entry's index.
fn at(t: &[(Entry, String)], digest: &str) -> anyhow::Result<u64> {
    let found: Vec<u64> = t.iter().filter(|(e, _)| e.statement == digest).map(|(e, _)| e.index).collect();
    match found.as_slice() {
        [one] => Ok(*one),
        [] => anyhow::bail!("{digest} isn't in the trail"),
        many => anyhow::bail!("{digest} is in the trail {} times", many.len()),
    }
}

/// The digest of kiln's own statement `s` of `payload_type`.
fn own(payload_type: &str, s: &Statement) -> anyhow::Result<String> {
    Ok(statement::digest(payload_type, &statement::payload(s)?))
}

fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = std::process::Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    anyhow::ensure!(o.status.success(), "git {args:?}: {out}");
    Ok(out)
}

/// A server (`[ci]` with `ci` more) whose one runner is `fedora-2`, and its repository `acme/app`
/// pushed to twice, the second push a workflow whose one leg runs there and attests what it built
/// (W18): returned once that run has ended, successful, with each push's commit.
fn pushed_and_ran(dir: &Path, ci: &str) -> anyhow::Result<(Server, agents::Runner, [String; 2])> {
    let s = start(dir, &Options { agent: false, ci, extra: "" })?;
    let (runner, log) = agents::runner_with(dir, s.port, "fedora-2", None, TALK, &[])?;
    agents::connected(s.port, &s.admin, "fedora-2", &log, Duration::from_secs(60))?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.join("githome"), dir.join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "an app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    let first = git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    // Once the server has polled its trunk, the workflow comes as a push.
    until(Duration::from_secs(60), "the trunk polled", || Ok((!http(s.port, "GET", "/api/repos", &s.admin, None)?.1.contains("\"last_polled_at\":null")).then_some(())))?;
    let steps = "      - run: printf 'the artifact\\n' > artifact.txt\n      - uses: kiln/attest@v1\n        with: { subject-path: artifact.txt }\n";
    std::fs::write(work.join(".kiln/e2e.yaml"), format!("on: push\njobs:\n  e2e:\n    runs-on: self-hosted\n    steps:\n{steps}"))?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "the workflow"])?;
    let second = git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let run = until(Duration::from_secs(300), "a finished run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=10", &s.admin, None)?;
        let runs: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(runs.into_iter().find(|r| r.get("finished_at").is_some_and(|f| !f.is_null())))
    })?;
    anyhow::ensure!(run.get("conclusion").and_then(Value::as_str) == Some("success"), "the run failed: {run}\n{}\n{}", s.log(), std::fs::read_to_string(&log).unwrap_or_default());
    Ok((s, runner, [first, second]))
}

/// A custodian: a key made here, and the certificate kiln's CA issued for it.
struct Signer {
    uri: String,
    key: rcgen::KeyPair,
    der: Vec<u8>,
}

impl Signer {
    /// A key of this test's, certified by the server for its owner (the admin token).
    fn certified(s: &Server) -> anyhow::Result<Self> {
        let key = kiln::pki::new_key_pem()?;
        let csr = kiln::pki::make_csr_with_key("kiln-identity", &key)?;
        let (st, body) = http(s.port, "POST", "/api/me/identity", &s.admin, Some(&json!({ "csr": csr })))?;
        anyhow::ensure!(st == 200, "certifying the owner's key: {st} {body}");
        let v: Value = serde_json::from_str(&body)?;
        let field = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(Self { uri: field("uri"), key: rcgen::KeyPair::from_pem(&key)?, der: kiln::pki::der_of(&field("cert_pem"))? })
    }

    /// `s`'s payload as this custodian signs it: its body naming it.
    fn payload(&self, s: &Statement) -> anyhow::Result<Vec<u8>> {
        let mut s = s.clone();
        if let Some(o) = s.body.as_object_mut() {
            o.insert("by".into(), Value::String(self.uri.clone()));
        }
        Ok(statement::payload(&s)?)
    }

    fn sign(&self, s: &Statement) -> anyhow::Result<(String, Value)> {
        let p = self.payload(s)?;
        Ok((statement::digest(PAYLOAD_CUSTODY, &p), statement::bundle(PAYLOAD_CUSTODY, &p, &self.key.sign(&statement::to_sign(PAYLOAD_CUSTODY, &p))?, &self.der)))
    }
}

/// W27-R09: a push, a leg, a custody hand-off and a revocation, each in the trail once and in the
/// order kiln took them; a checkpoint signed at its interval, and one asked for, each verifying as
/// the server's against kiln's root and holding over the trail, the later extending the earlier.
#[test]
fn w27_r09_every_signed_statement_is_in_the_trail() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (s, _runner, [first, second]) = pushed_and_ran(dir.path(), "kiln_workflows = true\ntrail = { checkpoint_every = 4, checkpoint_minutes = 600 }")?;
    let root = root_of(dir.path())?;
    let server = format!("kiln://{ORG}/server");
    // The leg's records come with its end, and kiln's receipt of what it handed on after them.
    let db = rusqlite::Connection::open(dir.path().join("data/kiln.db"))?;
    until(Duration::from_secs(60), "kiln's receipt of the leg's hand-on", || Ok(db.query_row("SELECT 1 FROM ci_leg_records WHERE uri = ?1", [&server], |_| Ok(())).optional()?))?;
    // The provenance kiln wrote of what the leg built, and the leg's signature of it.
    let origins = || -> anyhow::Result<Vec<String>> { Ok(db.prepare("SELECT origin FROM ci_attestations ORDER BY id")?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?) };
    until(Duration::from_secs(30), "the provenance, signed by kiln and by the leg", || origins().map(|o| (o.iter().any(|o| o == "kiln") && o.iter().any(|o| o == "leg")).then_some(())))?;
    // More than 4 entries by now: a checkpoint signed at its interval.
    until(Duration::from_secs(30), "a checkpoint at its interval", || Ok(checkpoints(&s, &root)?.into_iter().next()))?;

    // The owner hands a thing to kiln's server; then their identity is revoked.
    let owner = Signer::certified(&s)?;
    let thing = Subject { name: "release notes".into(), digest: BTreeMap::from([("sha256".to_owned(), "e".repeat(64))]), ..Subject::default() };
    let body = Body { to: Some(server.clone()), why: "a test".into(), ..Body::new(Kind::HandedOn, "") };
    let (handed, signed) = owner.sign(&Statement { subject: vec![thing], stream: Some("sync".into()), seq: Some(1), ..Statement::new(to_value(&body)) })?;
    let (st, answer) = http(s.port, "POST", "/api/custody", &s.admin, Some(&signed))?;
    anyhow::ensure!(st == 201, "the hand-off: {st} {answer}");
    let (st, answer) = http(s.port, "POST", "/api/identities/revoke", &s.admin, Some(&json!({ "subject": owner.uri, "reason": "key lost" })))?;
    anyhow::ensure!(st == 200, "the revocation: {st} {answer}");
    let (st, answer) = http(s.port, "POST", "/api/trail/checkpoints", &s.admin, None)?;
    anyhow::ensure!(st == 200, "a checkpoint asked for: {st} {answer}");

    let t = trail_of(&s)?;
    // Each push's commit as kiln checked it: unsigned, which the repository only warns of.
    let pushes = [&first, &second].map(|sha| own(trail::PAYLOAD_CHECKED, &trail::checked_commit("acme/app", "refs/heads/main", sha, None, "unsigned", None)));
    let [p1, p2] = pushes;
    let (p1, p2) = (at(&t, &p1?)?, at(&t, &p2?)?);
    // The leg's records as kiln took them, the server's delegation of the leg, and kiln's receipt
    // of what it made: each once, after the push that ran it.
    let mut leg: Vec<String> = db.prepare("SELECT digest FROM ci_leg_records WHERE refused IS NULL")?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    leg.extend(db.prepare("SELECT digest FROM ci_leg_delegations")?.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?);
    anyhow::ensure!(leg.len() >= 3, "a leg's delegation, records and receipt: {leg:?}");
    // What it attested (W18): each statement once, beside each signature of it, kiln's and the
    // leg's of the provenance alike.
    let attested: Vec<String> = db.prepare("SELECT bundle FROM ci_attestations")?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    let mut signatures: BTreeMap<String, usize> = BTreeMap::new();
    for b in &attested {
        let v: Value = serde_json::from_str(b)?;
        let (payload_type, payload) = (v.pointer("/dsseEnvelope/payloadType").and_then(Value::as_str).unwrap_or_default(), v.pointer("/dsseEnvelope/payload").and_then(Value::as_str).unwrap_or_default());
        let digest = statement::digest(payload_type, &base64::Engine::decode(&base64::engine::general_purpose::STANDARD, payload)?);
        *signatures.entry(digest).or_default() += 1;
    }
    for (digest, signed) in &signatures {
        let kept: i64 = db.query_row("SELECT COUNT(*) FROM trail_signatures WHERE statement = ?1", [digest], |r| r.get(0))?;
        assert_eq!(kept, i64::try_from(*signed)?, "{digest}: each signature of it beside it");
    }
    anyhow::ensure!(signatures.values().any(|n| *n == 2), "the provenance, signed twice: {signatures:?}");
    leg.extend(signatures.into_keys());
    let leg: Vec<u64> = leg.iter().map(|d| at(&t, d)).collect::<anyhow::Result<_>>()?;
    let (leg_first, leg_last) = (leg.iter().min().copied().unwrap_or_default(), leg.iter().max().copied().unwrap_or_default());
    // The hand-off, then every revocation kiln recorded, the owner's among them.
    let hand_off = at(&t, &handed)?;
    let mut revoked = Vec::new();
    let rows: Vec<(Option<String>, Option<String>, String, i64, String, String)> =
        db.prepare("SELECT serial, key_sha256, subject, at, reason, by FROM revocations")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<Result<_, _>>()?;
    for (serial, key, subject, when, reason, by) in &rows {
        let index = at(&t, &own(trail::PAYLOAD_REVOCATION, &trail::revocation(serial.as_deref(), key.as_deref(), subject, *when, reason, by))?)?;
        if subject == &owner.uri {
            revoked.push(index);
        }
    }
    anyhow::ensure!(!revoked.is_empty(), "the owner's revocation: {rows:?}");
    assert!(p1 < p2 && p2 < leg_first && leg_last < hand_off && revoked.iter().all(|r| *r > hand_off), "in the order taken: pushes {p1}, {p2}; the leg {leg_first}..={leg_last}; the hand-off {hand_off}; the revocation {revoked:?}");

    // A checkpoint at its interval (every 4), and the one asked for at the trail's size, each the
    // server's, each holding over the trail; the later extends the earlier.
    let cps = checkpoints(&s, &root)?;
    let newest = cps.first().ok_or_else(|| anyhow::anyhow!("no checkpoint"))?;
    assert_eq!(newest.size, t.len() as u64, "the one asked for covers the whole trail");
    assert!(cps.len() >= 2 && cps.iter().skip(1).all(|c| c.size >= 4), "one signed at its interval before it: {:?}", cps.iter().map(|c| c.size).collect::<Vec<_>>());
    for c in &cps {
        holds(&t, c)?;
    }
    each_extends(&t, &cps)?;
    Ok(())
}

/// Revokes each of `serials` (certificates kiln never issued), `threads` requests at a time.
fn revoke_all(s: &Server, serials: &[String], threads: usize) -> anyhow::Result<()> {
    let (port, admin) = (s.port, s.admin.as_str());
    std::thread::scope(|scope| -> anyhow::Result<()> {
        let running: Vec<_> = serials
            .chunks(serials.len().div_ceil(threads.max(1)).max(1))
            .map(|chunk| {
                scope.spawn(move || -> anyhow::Result<()> {
                    for serial in chunk {
                        let (st, body) = http(port, "POST", "/api/identities/revoke", admin, Some(&json!({ "serial": serial, "reason": "lost in a test" })))?;
                        anyhow::ensure!(st == 200, "{serial}: {st} {body}");
                    }
                    Ok(())
                })
            })
            .collect();
        for t in running {
            t.join().map_err(|_| anyhow::anyhow!("a thread of requests stopped short"))??;
        }
        Ok(())
    })
}

/// `GET /api/trail/log/<path>` as the owner: its bytes, none for a 404.
fn fetch(s: &Server, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let (st, body) = common::raw(s.port, "GET", &format!("/api/trail/log/{path}"), &format!("Authorization: Bearer {}\r\n", s.admin), &[])?;
    match st {
        200 => Ok(Some(body)),
        404 => Ok(None),
        _ => anyhow::bail!("GET /api/trail/log/{path}: {st} {}", String::from_utf8_lossy(&body)),
    }
}

/// W27-R09: the trail reads as a tiled log (C2SP tlog-tiles). From its checkpoint and its tiles
/// alone, a client proves an entry in it and a later checkpoint consistent with an earlier one.
#[test]
fn w27_r09_the_trail_reads_as_a_tiled_log() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: false, ci: "trail = { checkpoint_every = 100000, checkpoint_minutes = 600 }", extra: "" })?;
    let root = root_of(dir.path())?;
    let serials = |from: u64, to: u64| -> Vec<String> { (from..to).map(|i| format!("{:032x}", 0xbeef_0000 + i)).collect() };
    let signed = |s: &Server| -> anyhow::Result<Checkpoint> {
        let (st, body) = http(s.port, "POST", "/api/trail/checkpoints", &s.admin, None)?;
        anyhow::ensure!(st == 200, "a checkpoint asked for: {st} {body}");
        checkpoints(s, &root)?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no checkpoint"))
    };
    revoke_all(&s, &serials(0, 300), 6)?;
    let first = signed(&s)?;
    anyhow::ensure!(first.size >= 300, "{}", first.size);
    // The log's checkpoint is the one the server signed last.
    let note = String::from_utf8(fetch(&s, "checkpoint")?.ok_or_else(|| anyhow::anyhow!("no checkpoint served"))?)?;
    assert_eq!(Checkpoint::parse(trail::open(&note)?.0)?, first);
    let mut tile = |t: trail::Tile| -> Option<Vec<Hash>> { fetch(&s, &t.path(false)).ok().flatten().and_then(|b| trail::hashes_of(&t, &b)) };
    // An entry read from its bundle, proven in the tree from tiles alone.
    for m in [7u64, 299] {
        let bundle = trail::Tile { level: 0, index: m / 256, width: trail::width_at(first.size, 0, m / 256).unwrap_or(0) };
        let data = trail::unbundle(&fetch(&s, &bundle.path(true))?.ok_or_else(|| anyhow::anyhow!("entries of {m}"))?).ok_or_else(|| anyhow::anyhow!("a bundle"))?;
        let e = Entry::parse(data.get(usize::try_from(m % 256)?).ok_or_else(|| anyhow::anyhow!("entry {m} in its bundle"))?)?;
        assert_eq!(e.index, m);
        let proof = trail::inclusion_by(first.size, m, &mut |lo, hi| trail::mth_from(first.size, lo, hi, &mut tile)).ok_or_else(|| anyhow::anyhow!("a proof of {m}"))?;
        assert!(trail::verify_inclusion(&e.hash(), m, first.size, &proof, &first.root), "entry {m}");
    }
    assert!(fetch(&s, "tile/0/005")?.is_none(), "a tile the trail hasn't");
    // Grown, the next checkpoint proven consistent with the first, from tiles alone.
    revoke_all(&s, &serials(300, 330), 3)?;
    let next = signed(&s)?;
    let proof = trail::consistency_by(next.size, first.size, &mut |lo, hi| trail::mth_from(next.size, lo, hi, &mut tile)).ok_or_else(|| anyhow::anyhow!("a consistency proof"))?;
    assert!(trail::verify_consistency(first.size, next.size, &first.root, &next.root, &proof));
    Ok(())
}

/// Rekor's v1 API in front of kiln-seams' simulated Sigstore, on a port of its own: an entry
/// logged, and read back by its index or its uuid.
fn rekor_of(sim: std::sync::Arc<kiln_seams::sigstore::SimSigstore>) -> anyhow::Result<common::Service> {
    use kiln_seams::Sigstore as _;
    use kiln_seams::sigstore::{Logged, Refusal};
    fn one(l: &Logged) -> String {
        Value::Object(serde_json::Map::from_iter([(l.uuid.clone(), l.answer.clone())])).to_string()
    }
    fn said(e: &Refusal) -> String {
        json!({ "message": e.to_string() }).to_string()
    }
    common::Service::start(std::sync::Arc::new(move |method: &str, got: &common::Got| -> (u16, String) {
        let (path, query) = got.path.split_once('?').unwrap_or((got.path.as_str(), ""));
        let read = |r: Result<Logged, Refusal>| match r {
            Ok(l) => (200, one(&l)),
            Err(e @ Refusal::NotFound { .. }) => (404, said(&e)),
            Err(e) => (500, said(&e)),
        };
        match (method, path) {
            ("POST", "/rekor/api/v1/log/entries") => match futures::executor::block_on(sim.log(serde_json::from_slice(&got.body).unwrap_or(Value::Null))) {
                Ok(l) => (201, one(&l)),
                Err(Refusal::Conflict { index }) => {
                    let uuid = futures::executor::block_on(sim.entry(index)).map(|l| l.uuid).unwrap_or_default();
                    (409, json!({ "message": format!("an equivalent entry already exists in the transparency log with UUID {uuid}") }).to_string())
                }
                Err(e @ Refusal::Invalid { .. }) => (400, said(&e)),
                Err(e) => (500, said(&e)),
            },
            ("GET", "/rekor/api/v1/log/entries") => match query.split('&').find_map(|kv| kv.strip_prefix("logIndex=")).and_then(|n| n.parse::<u64>().ok()) {
                Some(n) => read(futures::executor::block_on(sim.entry(n))),
                None => (400, "{}".to_owned()),
            },
            ("GET", p) => match p.strip_prefix("/rekor/api/v1/log/entries/").and_then(|uuid| sim.index_of(uuid)) {
                Some(n) => read(futures::executor::block_on(sim.entry(n))),
                None => (404, "{}".to_owned()),
            },
            _ => (404, "{}".to_owned()),
        }
    }))
}

/// W27-R09: each checkpoint is anchored in the Rekor kiln is configured with, and the trail held
/// to what Rekor saw. Rewritten from entry 2 on, every hash made again and its checkpoints signed
/// anew (as one holding kiln's key could), the trail's own checkpoints agree with it; the anchor
/// doesn't, and says so.
#[test]
fn w27_r09_a_rewritten_trail_is_caught_against_its_anchor() -> anyhow::Result<()> {
    use kiln_seams::Sigstore as _;
    let dir = tempfile::tempdir()?;
    let sim = std::sync::Arc::new(kiln_seams::sigstore::SimSigstore::new(1_790_000_000));
    let rekor = rekor_of(sim.clone())?;
    let rekor_url = format!("{}/rekor", rekor.url());
    let trust = dir.path().join("trusted_root.json");
    std::fs::write(&trust, sim.trusted_root("https://fulcio.test", &rekor_url).map_err(|e| anyhow::anyhow!("{e}"))?.to_string())?;
    let extra = format!("[ci.attest]\nrekor_url = \"{rekor_url}\"\ntrusted_root = {}\n", common::toml_path(&trust));
    let mut s = start(dir.path(), &Options { agent: false, ci: "trail = { checkpoint_every = 4, checkpoint_minutes = 600 }", extra: &extra })?;
    let root = root_of(dir.path())?;
    let anchors = |s: &Server| -> anyhow::Result<Vec<Value>> {
        let (st, body) = http(s.port, "GET", "/api/trail/anchors", &s.admin, None)?;
        anyhow::ensure!(st == 200, "GET /api/trail/anchors: {st} {body}");
        Ok(serde_json::from_str::<Value>(&body)?.get("anchors").and_then(Value::as_array).cloned().unwrap_or_default())
    };
    let holds = |a: &Value| a.get("holds") == Some(&json!(true));
    revoke_all(&s, &(0..8u64).map(|i| format!("{:032x}", 0xa11c_0000 + i)).collect::<Vec<_>>(), 4)?;
    // Anchored, and holding: Rekor's promise and proof verified against its trusted root.
    let first = until(Duration::from_secs(60), "an anchored checkpoint", || Ok(anchors(&s)?.into_iter().next()))?;
    assert!(holds(&first) && first.get("rekor") == Some(&json!("verified")), "{first}");
    let (size, index) = (first.get("size").and_then(Value::as_u64).unwrap_or_default(), first.get("index").and_then(Value::as_u64).unwrap_or(u64::MAX));
    // Rekor's entry, read from it directly: a hashedrekord of the checkpoint's note text.
    let (st, body) = http(s.port, "GET", "/api/trail/checkpoints?limit=1000", &s.admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let note = serde_json::from_str::<Value>(&body)?.get("checkpoints").and_then(Value::as_array).into_iter().flatten().find(|c| c.get("size").and_then(Value::as_u64) == Some(size)).and_then(|c| c.get("note").and_then(Value::as_str).map(str::to_owned)).ok_or_else(|| anyhow::anyhow!("checkpoint {size}"))?;
    let logged = futures::executor::block_on(sim.entry(index))?;
    let kept: Value = serde_json::from_slice(&base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &logged.body)?)?;
    assert_eq!(kept.pointer("/spec/data/hash/value").and_then(Value::as_str), Some(kiln::util::sha256_hex(trail::open(&note)?.0.as_bytes()).as_str()), "{kept}");
    assert_eq!(checkpoints(&s, &root)?.iter().find(|c| c.size == size).map(|c| Checkpoint::parse(&c.text()).ok()), Some(Some(Checkpoint::parse(trail::open(&note)?.0)?)));

    // Rewritten from entry 2 on, every hash after it made again; kiln's own checkpoints dropped.
    {
        let db = rusqlite::Connection::open(dir.path().join("data/kiln.db"))?;
        db.busy_timeout(Duration::from_secs(10))?;
        let tx = db.unchecked_transaction()?;
        let rows: Vec<(i64, String, String, String, i64)> = tx.prepare("SELECT idx, statement, kind, subjects, taken_at FROM trail ORDER BY idx")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<Result<_, _>>()?;
        let mut last: Option<Entry> = None;
        for (idx, statement, kind, subjects, taken_at) in rows {
            let kind = if idx == 2 { "application/vnd.kiln.forged+json".to_owned() } else { kind };
            let e = trail::next(last.as_ref(), &statement, &kind, serde_json::from_str(&subjects)?, taken_at)?;
            tx.execute("UPDATE trail SET prev = ?2, kind = ?3, hash = ?4 WHERE idx = ?1", rusqlite::params![idx, hex::encode(e.prev), e.kind, hex::encode(e.hash())])?;
            last = Some(e);
        }
        tx.execute("DELETE FROM trail_checkpoints", [])?;
        tx.commit()?;
    }
    s.restart()?;
    // kiln signs the rewritten trail anew, and anchors that too: its own checkpoints agree with
    // it. The anchor from before doesn't: Rekor saw another root.
    let list = until(Duration::from_secs(60), "the rewritten trail checkpointed and anchored", || {
        let list = anchors(&s)?;
        Ok((list.len() >= 2).then_some(list))
    })?;
    let before = list.iter().find(|a| a.get("index").and_then(Value::as_u64) == Some(index)).ok_or_else(|| anyhow::anyhow!("anchor {index}: {list:?}"))?;
    assert!(!holds(before), "{before}");
    let why = before.get("why").and_then(Value::as_str).unwrap_or_default();
    assert!(why.contains(&format!("the trail's first {size} entries don't make the root Rekor saw at entry {index}")), "{why}");
    assert!(list.iter().filter(|a| a.get("index").and_then(Value::as_u64) != Some(index)).any(holds), "the checkpoint signed anew holds against its own anchor: {list:?}");
    Ok(())
}

/// W27-R09: forty revocations, eight requests at a time, each in the trail once and its chain
/// whole; after a restart the trail is as it was, appends go on from where it ended, and the next
/// checkpoint extends the last one signed before.
#[test]
fn w27_r09_the_trail_survives_restart_and_concurrent_appends() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut s = start(dir.path(), &Options { agent: false, ci: "trail = { checkpoint_every = 8, checkpoint_minutes = 600 }", extra: "" })?;
    let root = root_of(dir.path())?;
    let serials: Vec<String> = (0..40u64).map(|i| format!("{:032x}", 0xfeed_0000 + i)).collect();
    revoke_all(&s, &serials, 8)?;
    let before = trail_of(&s)?;
    for serial in &serials {
        let mine: Vec<&Entry> = before.iter().map(|(e, _)| e).filter(|e| e.kind == trail::PAYLOAD_REVOCATION && e.subjects.contains(&format!("x509Serial:{serial}"))).collect();
        assert_eq!(mine.len(), 1, "{serial}'s revocation, once");
    }
    let whole: Vec<Entry> = before.iter().map(|(e, _)| e.clone()).collect();
    trail::check(None, &whole)?;
    // Signed at its interval: every 8.
    let signed = until(Duration::from_secs(30), "a checkpoint at its interval", || Ok(checkpoints(&s, &root)?.into_iter().next()))?;
    let before = trail_of(&s)?;
    holds(&before, &signed)?;

    s.restart()?;
    let after = trail_of(&s)?;
    assert_eq!(after.get(..before.len()), Some(before.as_slice()), "the trail is as it was");
    let (st, body) = http(s.port, "POST", "/api/identities/revoke", &s.admin, Some(&json!({ "serial": "0badc0de", "reason": "after the restart" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (st, body) = http(s.port, "POST", "/api/trail/checkpoints", &s.admin, None)?;
    anyhow::ensure!(st == 200, "a checkpoint asked for: {st} {body}");
    let t = trail_of(&s)?;
    let late = t.iter().find(|(e, _)| e.subjects.contains(&"x509Serial:0badc0de".to_owned())).map(|(e, _)| e.index).ok_or_else(|| anyhow::anyhow!("the revocation after the restart isn't in the trail"))?;
    assert!(late >= before.len() as u64, "appended after what was there: {late}");
    let cps = checkpoints(&s, &root)?;
    assert_eq!(cps.first().map(|c| c.size), Some(t.len() as u64));
    for c in &cps {
        holds(&t, c)?;
    }
    each_extends(&t, &cps)?;
    Ok(())
}
