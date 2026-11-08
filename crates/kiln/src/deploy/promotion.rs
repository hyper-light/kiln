//! A promotion as a transfer (docs/ci.md W27-R08, with W27-R07; the Transfers plan's step 8): a
//! version released from the environment it runs in to the next (an `after` its gates passed
//! there, or a release promoted from it) is handed on by CD for the environment it leaves, naming
//! the images that ran there by their digests, and received by CD for the one it reaches as it
//! lands there: the same digests, both records signed by CD and kept as custody records
//! (`crate::custody`). A release whose version or images aren't what runs in an environment it must
//! run in first is refused as a promotion, and that's recorded (`transfer_refusals`). An `after` on
//! a linked server (`staging@eu`) is that server's environment: its signed statement is the gate's
//! evidence, and this server's CD signs no hand-on for it.

use std::collections::BTreeMap;

use kiln_ci::identity::custody::{Body, Kind as Record, to_value};
use kiln_ci::identity::statement::{Statement, Subject};
use serde_json::{Value, json};

use super::Deploy;
use super::model::{Artifact, GateResult, Kind, Release};
use super::store;

/// The gate result a release keeps its promotion's hand-ons under.
pub const GATE: &str = "promotion";

/// The things a promotion of `r` hands on: its images, each by its digest (`sha256:<hex>`). An
/// image named without a digest is no thing of it: a promotion is of the same bytes.
pub fn things(r: &Release) -> Vec<Subject> {
    things_of(&r.artifacts)
}

fn things_of(artifacts: &BTreeMap<String, Artifact>) -> Vec<Subject> {
    artifacts
        .iter()
        .filter_map(|(key, a)| {
            let (alg, hex) = a.digest.split_once(':')?;
            let name = if a.repository.is_empty() { key.clone() } else { a.repository.clone() };
            Some(Subject { name, digest: BTreeMap::from([(alg.to_ascii_lowercase(), hex.to_ascii_lowercase())]), ..Subject::default() })
        })
        .collect()
}

/// "registry.example/acme/web sha256:aaaa…", each image, or "no image digests".
fn named(things: &[Subject]) -> String {
    let each: Vec<String> = things.iter().map(|t| format!("{} {}", t.name, t.digest.iter().map(|(alg, hex)| format!("{alg}:{hex}")).collect::<Vec<_>>().join(" "))).collect();
    if each.is_empty() { "no image digests".to_owned() } else { each.join(", ") }
}

/// Where a promotion's records came from, as custody records say it.
fn came_from(r: &Release) -> String {
    let release = if r.id == 0 { "a new deploy release".to_owned() } else { format!("deploy release {}", r.id) };
    format!("{release} ({} to {}, asked by {})", r.app, r.environment, r.requested_by)
}

/// The environments release `r` is promoted from, each with the release that runs there and since
/// when: every `after` of this server's its gates passed, and the one it was promoted from by hand.
async fn sources(d: &Deploy, r: &Release) -> anyhow::Result<Vec<(String, i64, i64)>> {
    let mut from: Vec<(String, i64, i64)> = r
        .gates
        .iter()
        .filter(|g| g.passed)
        .filter_map(|g| {
            let up = g.gate.strip_prefix("after:").filter(|up| !up.contains('@'))?;
            let release = g.evidence.get("release").and_then(Value::as_i64)?;
            Some((up.to_owned(), release, g.evidence.get("since").and_then(Value::as_i64).unwrap_or_default()))
        })
        .collect();
    if let Some(id) = r.promoted_from
        && !from.iter().any(|(_, release, _)| *release == id)
        && let Some(p) = store::release(&*d.records, id).await?
    {
        from.push((p.environment.clone(), p.id, p.ended_at.unwrap_or(p.created_at)));
    }
    Ok(from)
}

/// Gate result `g` kept on `r`, in place of one before it.
fn set(r: &mut Release, g: GateResult) {
    r.gates.retain(|x| x.gate != g.gate);
    r.gates.push(g);
}

/// CD's receipts of `things` for the deploys of `app` to environment `up` through KilnCD, which a
/// promotion from `up` follows (docs/ci.md W27-R08; the Transfers plan's step 14): its chain then
/// walks back through what was deployed there, and from there to what made it. None where KilnCD
/// recorded none (another provider, or no sync since custody to production was built).
async fn deployed_there(d: &Deploy, app: &super::model::App, up: &str, things: &[Subject]) -> Vec<String> {
    let id = super::kilncd::app_id(app, up);
    let mut out = Vec::new();
    for hex in things.iter().filter_map(|t| t.digest.get("sha256")) {
        match crate::cd::custody::cd_holding(&d.hub, &id, hex).await {
            Ok(Some(r)) if !out.contains(&r) => out.push(r),
            Ok(_) => {}
            Err(e) => tracing::warn!(target: "kiln.custody", app = %id, error = %format!("{e:#}"), "what a promotion follows wasn't read"),
        }
    }
    out
}

/// CD's hand-on of release `r`, going now (its gates passed), from each environment it's promoted
/// from: kept, and kept on `r` as gate result [`GATE`] with the records' digests. Nothing for a
/// release that isn't a promotion; said so for one that names no image digests. Its body is what
/// the release and the gates say, no time of its own, so signing it again (its gates looked at
/// again) gives the same record, kept once.
pub async fn hand_on(d: &Deploy, r: &mut Release) -> anyhow::Result<()> {
    if r.kind != Kind::Deploy {
        return Ok(());
    }
    let from = sources(d, r).await?;
    if from.is_empty() {
        return Ok(());
    }
    let envs: Vec<String> = from.iter().map(|(e, ..)| e.clone()).collect();
    let things = things(r);
    if things.is_empty() {
        let detail = format!("a promotion from {}, not signed: the release names no image digests", envs.join(", "));
        set(r, GateResult { gate: GATE.into(), passed: true, detail, evidence: json!({ "from": envs }) });
        return Ok(());
    }
    let (org, version) = (d.hub.pki.organization(), r.version.clone().unwrap_or_default());
    let cd = crate::custody::cd_uri(&org);
    let app = store::app(&*d.records, &r.app).await?;
    let mut handed = Vec::new();
    for (up, release, since) in &from {
        let why = format!("promotion of {} {version} from {up} to {}: release {release} ran it there since {}", r.app, r.environment, super::repo::rfc3339(*since));
        let b = Body { to: Some(cd.clone()), component: Some(format!("environment {up}")), why, ..Body::new(Record::HandedOn, "") };
        let follows = match &app {
            Some(a) => deployed_there(d, a, up, &things).await,
            None => Vec::new(),
        };
        // Signed on the compute pool, CD's key and certificate read there (goal 9, plan 01a1a87b2cec step 3).
        let bundle = crate::custody::cd_signed_off(&d.hub.pki, Statement { subject: things.clone(), follows, ..Statement::new(to_value(&b)) }).await?;
        match crate::custody::take(&d.hub.store, &d.hub.pki.ca_der, &org, &bundle, &came_from(r), crate::util::now_unix()).await? {
            Ok(t) => handed.push(t.digest),
            Err(why) => {
                tracing::warn!(target: "kiln.custody", app = %r.app, environment = %r.environment, %why, "CD's hand-on of a promotion wasn't kept");
                let detail = format!("a promotion from {}: CD's hand-on wasn't kept: {why}", envs.join(", "));
                set(r, GateResult { gate: GATE.into(), passed: true, detail, evidence: json!({ "from": envs }) });
                return Ok(());
            }
        }
    }
    let detail = format!("promoted from {}: {} handed on by CD, signed", envs.join(", "), named(&things));
    set(r, GateResult { gate: GATE.into(), passed: true, detail, evidence: json!({ "from": envs, "hand_on": handed }) });
    Ok(())
}

/// CD's receipt of release `r`, landed now in its environment, for its promotion's hand-ons (gate
/// result [`GATE`]): the same images, received for the environment it reached, following them.
/// Nothing for a release that isn't a promotion, or whose hand-on wasn't signed.
pub async fn receipt(d: &Deploy, r: &Release) -> anyhow::Result<()> {
    let Some(g) = r.gates.iter().find(|g| g.gate == GATE) else { return Ok(()) };
    let handed: Vec<String> = g.evidence.get("hand_on").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default();
    if handed.is_empty() {
        return Ok(());
    }
    let org = d.hub.pki.organization();
    let why = format!("{} {} runs in {} (release {})", r.app, r.version.as_deref().unwrap_or_default(), r.environment, r.id);
    let b = Body { from: Some(crate::custody::cd_uri(&org)), component: Some(format!("environment {}", r.environment)), why, ..Body::new(Record::Received, "") };
    // Signed on the compute pool, CD's key and certificate read there (goal 9, plan 01a1a87b2cec step 3).
    let bundle = crate::custody::cd_signed_off(&d.hub.pki, Statement { subject: things(r), follows: handed, ..Statement::new(to_value(&b)) }).await?;
    match crate::custody::take(&d.hub.store, &d.hub.pki.ca_der, &org, &bundle, &came_from(r), crate::util::now_unix()).await? {
        Ok(t) => {
            if let Some(broke) = t.broke {
                tracing::warn!(target: "kiln.custody", app = %r.app, environment = %r.environment, record = %t.digest, %broke, "CD's receipt of a promotion breaks its chain");
            }
        }
        Err(why) => tracing::warn!(target: "kiln.custody", app = %r.app, environment = %r.environment, %why, "CD's receipt of a promotion wasn't kept"),
    }
    Ok(())
}

/// Release `r` refused by its gates (`results`, saying `why`): recorded as a promotion refused when
/// an environment it must run in first runs another version, or other images, or nothing of it.
pub async fn refused(d: &Deploy, r: &Release, results: &[GateResult], why: &str) -> anyhow::Result<()> {
    let Some((up, g)) = results.iter().filter(|g| !g.passed && g.evidence.get("runs").is_some()).find_map(|g| g.gate.strip_prefix("after:").map(|up| (up, g))) else { return Ok(()) };
    let there: BTreeMap<String, Artifact> = g.evidence.get("artifacts").cloned().and_then(|a| serde_json::from_value(a).ok()).unwrap_or_default();
    let runs = g.evidence.get("runs").and_then(Value::as_str).unwrap_or("nothing");
    let act = format!("a promotion of {} {} from {up} to {}", r.app, r.version.as_deref().unwrap_or_default(), r.environment);
    let why = format!("{why}: {up} runs {runs} ({}); this release names {}", named(&things_of(&there)), named(&things(r)));
    crate::custody::refused(&d.hub.store, "", &act, "", &why, &came_from(r), crate::util::now_unix()).await
}
