//! Deploying by API's records (docs/deploy-api.md §14.1), on kiln's Records seam
//! (docs/seams.md, "Records"): kiln's own durable Records until Amphora's (CP-T1.5) takes its
//! place, `SimRecords` in tests. Everything else reaches them through here.
//!
//! Each record is a family's row, its value the record's JSON. A release is numbered by a counter
//! row taken under compare-and-set in the release's own commit, and found by the indexes written
//! in that commit too (by registration, app, environment, state, in flight): rows like any other,
//! so a copy through the seam carries them. A release is changed under compare-and-set on its
//! row, which is what makes two servers following one safe; the work queue is a family of leases
//! taken the same way, so a server that dies lets its leases lapse and another takes them (§16).
//! Revisions are held only between a read and its conditional commit, never kept.
//!
//! Composite keys put `\0` between their parts: it sorts below every character a name has, so a
//! family's key order is its parts' order (an app's registrations together, by environment).

use anyhow::Context as _;
use kiln_seams::records::{Cond, Consistency, Records, Refusal, Row, Scan, Tx, Write};
use serde::{Deserialize, Serialize};

use super::model::{App, Environment, Registration, Release, State};

/// The records, as each function here takes them.
pub type Recs<'a> = &'a dyn Records;

const ENV: &str = "deploy.env";
const APP: &str = "deploy.app";
const TEMPLATE: &str = "deploy.template";
const REGISTRATION: &str = "deploy.registration";
const PROPOSAL: &str = "deploy.proposal";
const RELEASE: &str = "deploy.release";
const BY_REG: &str = "deploy.release-by-reg";
const BY_APP: &str = "deploy.release-by-app";
const BY_ENV: &str = "deploy.release-by-env";
const BY_STATE: &str = "deploy.release-by-state";
const IN_FLIGHT: &str = "deploy.release-in-flight";
const KEYS: &str = "deploy.release-key";
const COUNTER: &str = "deploy.counter";
const WORK: &str = "deploy.work";
const STATE: &str = "deploy.state";

/// The counter row's key: the last release number given.
const RELEASES: &[u8] = b"release";

/// How often a write raced by another server's is tried again before it's given up.
const TRIES: usize = 64;

/// The most rows a list of a family reads: far more than deploy keeps (its environments, apps,
/// registrations, a fleet's proposals, a registration's releases in flight).
const LISTED: usize = 1_000_000;

/// The most releases being followed at once that a claim reads: one lease each.
const FOLLOWED: usize = 100_000;

fn refused(r: Refusal) -> anyhow::Error {
    anyhow::anyhow!("deploy's records: {r}")
}

fn to_json<T: Serialize>(v: &T) -> anyhow::Result<Vec<u8>> {
    serde_json::to_vec(v).context("encode a deploy record")
}

fn from_json<T: serde::de::DeserializeOwned>(b: &[u8]) -> anyhow::Result<T> {
    serde_json::from_slice(b).context("read a deploy record")
}

fn put(family: &str, key: Vec<u8>, value: Vec<u8>) -> Write {
    Write::Put { family: family.into(), key, value }
}

fn del(family: &str, key: Vec<u8>) -> Write {
    Write::Delete { family: family.into(), key }
}

/// `parts` joined by `\0`.
fn joined(parts: &[&str]) -> Vec<u8> {
    parts.join("\0").into_bytes()
}

/// `parts`, each followed by `\0`: a prefix of everything under them.
fn under(parts: &[&str]) -> Vec<u8> {
    let mut k = Vec::new();
    for p in parts {
        k.extend_from_slice(p.as_bytes());
        k.push(0);
    }
    k
}

fn id_of(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

/// A release number in key order, oldest first.
fn ascending(id: i64) -> [u8; 8] {
    id_of(id).to_be_bytes()
}

/// A release number in key order, newest first.
fn newest_first(id: i64) -> [u8; 8] {
    (!id_of(id)).to_be_bytes()
}

/// A release number from either encoding's last eight bytes.
fn number(key: &[u8], newest: bool) -> Option<i64> {
    let tail: [u8; 8] = key.get(key.len().checked_sub(8)?..)?.try_into().ok()?;
    let n = u64::from_be_bytes(tail);
    i64::try_from(if newest { !n } else { n }).ok()
}

fn with_id(prefix: Vec<u8>, id: [u8; 8]) -> Vec<u8> {
    let mut k = prefix;
    k.extend_from_slice(&id);
    k
}

fn release_key(id: i64) -> Vec<u8> {
    newest_first(id).to_vec()
}

fn text(v: &[u8]) -> anyhow::Result<i64> {
    std::str::from_utf8(v).ok().and_then(|s| s.parse().ok()).ok_or_else(|| anyhow::anyhow!("a deploy index row isn't a release number"))
}

async fn get_json<T: serde::de::DeserializeOwned>(store: Recs<'_>, family: &str, key: &[u8]) -> anyhow::Result<Option<T>> {
    match store.get(family, key, Consistency::Linearizable).await.map_err(refused)? {
        Some(row) => Ok(Some(from_json(&row.value)?)),
        None => Ok(None),
    }
}

async fn put_json<T: Serialize>(store: Recs<'_>, family: &str, key: Vec<u8>, v: &T) -> anyhow::Result<()> {
    store.commit(Tx { conds: Vec::new(), writes: vec![put(family, key, to_json(v)?)] }).await.map_err(refused)?;
    Ok(())
}

/// Deletes a row: whether there was one.
async fn delete_row(store: Recs<'_>, family: &str, key: Vec<u8>) -> anyhow::Result<bool> {
    if store.get(family, &key, Consistency::Linearizable).await.map_err(refused)?.is_none() {
        return Ok(false);
    }
    store.commit(Tx { conds: Vec::new(), writes: vec![del(family, key)] }).await.map_err(refused)?;
    Ok(true)
}

/// Rows of `family` under `prefix`, in key order: page by page, every page read at one revision,
/// so the list is one moment's. More than `max` is refused, not cut short without a word.
async fn rows(store: Recs<'_>, family: &str, prefix: &[u8], max: usize) -> anyhow::Result<Vec<Row>> {
    let at = Consistency::Snapshot(store.head().await.map_err(refused)?);
    let mut out: Vec<Row> = Vec::new();
    let mut after = None;
    let mut more = false;
    while out.len() < max {
        let page = store.scan(family, Scan { prefix: prefix.to_vec(), after: after.clone(), limit: max.saturating_sub(out.len()).clamp(1, 1000), at }).await.map_err(refused)?;
        after = page.rows.last().map(|r| r.key.clone());
        out.extend(page.rows);
        more = page.more;
        if !more {
            break;
        }
    }
    if more {
        anyhow::bail!("{family} holds more than {max} rows under its prefix, more than deploy reads of it");
    }
    Ok(out)
}

/// Every record of a family under `prefix`, in key order.
async fn all<T: serde::de::DeserializeOwned>(store: Recs<'_>, family: &str, prefix: &[u8]) -> anyhow::Result<Vec<T>> {
    rows(store, family, prefix, LISTED).await?.iter().map(|r| from_json(&r.value)).collect()
}

// ------------------------------------------------------------------ environments

pub async fn put_environment(store: Recs<'_>, e: &Environment) -> anyhow::Result<()> {
    put_json(store, ENV, e.name.clone().into_bytes(), e).await
}

pub async fn environment(store: Recs<'_>, name: &str) -> anyhow::Result<Option<Environment>> {
    get_json(store, ENV, name.as_bytes()).await
}

pub async fn environments(store: Recs<'_>) -> anyhow::Result<Vec<Environment>> {
    all(store, ENV, b"").await
}

pub async fn delete_environment(store: Recs<'_>, name: &str) -> anyhow::Result<bool> {
    delete_row(store, ENV, name.as_bytes().to_vec()).await
}

// ------------------------------------------------------------------ apps

pub async fn put_app(store: Recs<'_>, a: &App) -> anyhow::Result<()> {
    put_json(store, APP, a.name.clone().into_bytes(), a).await
}

pub async fn app(store: Recs<'_>, name: &str) -> anyhow::Result<Option<App>> {
    get_json(store, APP, name.as_bytes()).await
}

pub async fn apps(store: Recs<'_>) -> anyhow::Result<Vec<App>> {
    all(store, APP, b"").await
}

pub async fn delete_app(store: Recs<'_>, name: &str) -> anyhow::Result<bool> {
    delete_row(store, APP, name.as_bytes().to_vec()).await
}

// ------------------------------------------------------------------ templates

pub async fn put_template(store: Recs<'_>, t: &super::model::Template) -> anyhow::Result<()> {
    put_json(store, TEMPLATE, t.name.clone().into_bytes(), t).await
}

pub async fn template(store: Recs<'_>, name: &str) -> anyhow::Result<Option<super::model::Template>> {
    get_json(store, TEMPLATE, name.as_bytes()).await
}

pub async fn templates(store: Recs<'_>) -> anyhow::Result<Vec<super::model::Template>> {
    all(store, TEMPLATE, b"").await
}

pub async fn delete_template(store: Recs<'_>, name: &str) -> anyhow::Result<bool> {
    delete_row(store, TEMPLATE, name.as_bytes().to_vec()).await
}

// ------------------------------------------------------------------ registrations

pub async fn put_registration(store: Recs<'_>, r: &Registration) -> anyhow::Result<()> {
    put_json(store, REGISTRATION, joined(&[&r.app, &r.environment]), r).await
}

pub async fn registration(store: Recs<'_>, app: &str, env: &str) -> anyhow::Result<Option<Registration>> {
    get_json(store, REGISTRATION, &joined(&[app, env])).await
}

/// Registrations of an app, of an environment, or every one: by app, then environment.
pub async fn registrations(store: Recs<'_>, app: Option<&str>, env: Option<&str>) -> anyhow::Result<Vec<Registration>> {
    match (app, env) {
        (Some(a), Some(e)) => Ok(registration(store, a, e).await?.into_iter().collect()),
        (Some(a), None) => all(store, REGISTRATION, &under(&[a])).await,
        (None, _) => Ok(all::<Registration>(store, REGISTRATION, b"").await?.into_iter().filter(|r| env.is_none_or(|e| r.environment == e)).collect()),
    }
}

pub async fn delete_registration(store: Recs<'_>, app: &str, env: &str) -> anyhow::Result<bool> {
    delete_row(store, REGISTRATION, joined(&[app, env])).await
}

// ------------------------------------------------------------------ releases

/// The index rows a release has, in its state: by registration, app, environment and state
/// (newest first), and while it's in flight, by registration oldest first.
fn indexes(r: &Release) -> Vec<(&'static str, Vec<u8>)> {
    let mut out = vec![
        (BY_REG, with_id(under(&[&r.app, &r.environment]), newest_first(r.id))),
        (BY_APP, with_id(under(&[&r.app]), newest_first(r.id))),
        (BY_ENV, with_id(under(&[&r.environment]), newest_first(r.id))),
        (BY_STATE, state_key(r.state, r.id)),
    ];
    if r.state.in_flight() {
        out.push((IN_FLIGHT, flight_key(r)));
    }
    out
}

fn state_key(s: State, id: i64) -> Vec<u8> {
    with_id(under(&[s.as_str()]), newest_first(id))
}

fn flight_key(r: &Release) -> Vec<u8> {
    with_id(under(&[&r.app, &r.environment]), ascending(r.id))
}

fn idempotency_row(app: &str, env: &str, key: &str) -> Vec<u8> {
    joined(&[key, app, env])
}

/// Records a new release, numbering it; returns it with its id, and `true`. One asked for with an
/// idempotency key its registration has used already isn't recorded again: that release is
/// returned, with `false`.
pub async fn insert_release(store: Recs<'_>, r: &Release) -> anyhow::Result<(Release, bool)> {
    let mut r = r.clone();
    let key = r.idempotency_key.as_deref().map(|k| idempotency_row(&r.app, &r.environment, k));
    for _ in 0..TRIES {
        if let Some(k) = &key
            && let Some(row) = store.get(KEYS, k, Consistency::Linearizable).await.map_err(refused)?
        {
            let used = text(&row.value)?;
            let found = release(store, used).await?.ok_or_else(|| anyhow::anyhow!("release {used} vanished"))?;
            return Ok((found, false));
        }
        let counter = store.get(COUNTER, RELEASES, Consistency::Linearizable).await.map_err(refused)?;
        let (last, cond) = match &counter {
            Some(row) => (text(&row.value)?, Cond::At { family: COUNTER.into(), key: RELEASES.to_vec(), rev: row.rev }),
            None => (0, Cond::Absent { family: COUNTER.into(), key: RELEASES.to_vec() }),
        };
        r.id = last.saturating_add(1);
        let mut conds = vec![cond];
        let mut writes = vec![put(COUNTER, RELEASES.to_vec(), r.id.to_string().into_bytes()), put(RELEASE, release_key(r.id), to_json(&r)?)];
        if let Some(k) = &key {
            conds.push(Cond::Absent { family: KEYS.into(), key: k.clone() });
            writes.push(put(KEYS, k.clone(), r.id.to_string().into_bytes()));
        }
        writes.extend(indexes(&r).into_iter().map(|(f, k)| put(f, k, r.id.to_string().into_bytes())));
        match store.commit(Tx { conds, writes }).await {
            Ok(_) => return Ok((r, true)),
            // Another server numbered one meanwhile, or recorded one under its key: look again.
            Err(Refusal::Conflict { .. }) => {}
            Err(e) => return Err(refused(e)),
        }
    }
    anyhow::bail!("release numbers kept moving under a release of {} in {}", r.app, r.environment)
}

/// Writes a release back if its state is still `expected` (another server, or a cancel, may have
/// moved it meanwhile). Whether it did.
pub async fn update_release(store: Recs<'_>, r: &Release, expected: State) -> anyhow::Result<bool> {
    for _ in 0..TRIES {
        let Some(row) = store.get(RELEASE, &release_key(r.id), Consistency::Linearizable).await.map_err(refused)? else { return Ok(false) };
        let was: Release = from_json(&row.value)?;
        if was.state != expected {
            return Ok(false);
        }
        let id = r.id.to_string().into_bytes();
        let mut writes = vec![put(RELEASE, release_key(r.id), to_json(r)?)];
        if was.state != r.state {
            writes.push(del(BY_STATE, state_key(was.state, r.id)));
            writes.push(put(BY_STATE, state_key(r.state, r.id), id.clone()));
        }
        match (was.state.in_flight(), r.state.in_flight()) {
            (true, false) => writes.push(del(IN_FLIGHT, flight_key(r))),
            (false, true) => writes.push(put(IN_FLIGHT, flight_key(r), id)),
            _ => {}
        }
        match store.commit(Tx { conds: vec![Cond::At { family: RELEASE.into(), key: release_key(r.id), rev: row.rev }], writes }).await {
            Ok(_) => return Ok(true),
            // It moved between the read and the write: look at it again.
            Err(Refusal::Conflict { .. }) => {}
            Err(e) => return Err(refused(e)),
        }
    }
    anyhow::bail!("release {} kept moving under its update", r.id)
}

/// The release a registration was asked for with `key`, if it was.
pub async fn release_by_key(store: Recs<'_>, app: &str, env: &str, key: &str) -> anyhow::Result<Option<Release>> {
    match store.get(KEYS, &idempotency_row(app, env, key), Consistency::Linearizable).await.map_err(refused)? {
        Some(row) => release(store, text(&row.value)?).await,
        None => Ok(None),
    }
}

/// The releases asked for under one idempotency key's prefix (a fleet's: `fleet:<id>:`), oldest
/// first.
pub async fn releases_keyed(store: Recs<'_>, prefix: &str) -> anyhow::Result<Vec<Release>> {
    let mut out = Vec::new();
    for row in rows(store, KEYS, prefix.as_bytes(), LISTED).await? {
        if let Some(r) = release(store, text(&row.value)?).await? {
            out.push(r);
        }
    }
    out.sort_by_key(|r| r.id);
    Ok(out)
}

pub async fn release(store: Recs<'_>, id: i64) -> anyhow::Result<Option<Release>> {
    get_json(store, RELEASE, &release_key(id)).await
}

/// Releases, newest first, filtered.
#[derive(Debug, Default, Clone)]
pub struct ReleaseQuery {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub state: Option<State>,
    /// Those of one version.
    pub version: Option<String>,
    pub limit: usize,
}

pub async fn releases(store: Recs<'_>, q: ReleaseQuery) -> anyhow::Result<Vec<Release>> {
    let limit = if q.limit == 0 { 100 } else { q.limit.min(1000) };
    let keep = |r: &Release| q.app.as_ref().is_none_or(|a| &r.app == a) && q.environment.as_ref().is_none_or(|e| &r.environment == e) && q.state.is_none_or(|s| r.state == s) && q.version.as_ref().is_none_or(|v| r.version.as_ref() == Some(v));
    // The narrowest index the filters have; the releases themselves when there are none.
    let (family, prefix) = match (&q.app, &q.environment, q.state) {
        (Some(a), Some(e), _) => (BY_REG, under(&[a, e])),
        (Some(a), None, _) => (BY_APP, under(&[a])),
        (None, Some(e), _) => (BY_ENV, under(&[e])),
        (None, None, Some(s)) => (BY_STATE, under(&[s.as_str()])),
        (None, None, None) => (RELEASE, Vec::new()),
    };
    let at = Consistency::Snapshot(store.head().await.map_err(refused)?);
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = store.scan(family, Scan { prefix: prefix.clone(), after: after.clone(), limit: 1000, at }).await.map_err(refused)?;
        after = page.rows.last().map(|r| r.key.clone());
        for row in &page.rows {
            let r = if family == RELEASE {
                from_json::<Release>(&row.value)?
            } else {
                let Some(r) = release(store, text(&row.value)?).await? else { continue };
                r
            };
            if keep(&r) {
                out.push(r);
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
        if !page.more {
            return Ok(out);
        }
    }
}

/// The releases in flight for a registration, oldest first.
pub async fn flying(store: Recs<'_>, app: &str, env: &str) -> anyhow::Result<Vec<Release>> {
    let mut out = Vec::new();
    for row in rows(store, IN_FLIGHT, &under(&[app, env]), LISTED).await? {
        let Some(id) = number(&row.key, false) else { continue };
        if let Some(r) = release(store, id).await?.filter(|r| r.state.in_flight()) {
            out.push(r);
        }
    }
    Ok(out)
}

/// The newest release in flight for a registration: the one a secret's change joins while it
/// waits (§3.5).
pub async fn newest_in_flight(store: Recs<'_>, app: &str, env: &str) -> anyhow::Result<Option<Release>> {
    Ok(flying(store, app, env).await?.pop())
}

/// A registration's release in flight, if any: the oldest, when several wait their turn.
pub async fn in_flight(store: Recs<'_>, app: &str, env: &str) -> anyhow::Result<Option<Release>> {
    Ok(flying(store, app, env).await?.into_iter().next())
}

/// Every release in flight, of every registration, oldest first: the deploy queue (docs/ci.md
/// W28-R38).
pub async fn in_flight_all(store: Recs<'_>) -> anyhow::Result<Vec<Release>> {
    let mut out = Vec::new();
    for row in rows(store, IN_FLIGHT, &[], LISTED).await? {
        let Some(id) = number(&row.key, false) else { continue };
        if let Some(r) = release(store, id).await?.filter(|r| r.state.in_flight()) {
            out.push(r);
        }
    }
    out.sort_by_key(|r| r.id);
    Ok(out)
}

// ------------------------------------------------------------------ the work queue

/// One release being followed: when it's next due, and the lease of the server following it.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Work {
    due_at: i64,
    #[serde(default)]
    owner: String,
    #[serde(default)]
    lease_until: i64,
}

/// Follow a release from `due_at` (unix seconds) on.
pub async fn schedule(store: Recs<'_>, release: i64, due_at: i64) -> anyhow::Result<()> {
    let key = ascending(release).to_vec();
    for _ in 0..TRIES {
        let (cond, w) = match store.get(WORK, &key, Consistency::Linearizable).await.map_err(refused)? {
            Some(row) => (Cond::At { family: WORK.into(), key: key.clone(), rev: row.rev }, Work { due_at, ..from_json(&row.value)? }),
            None => (Cond::Absent { family: WORK.into(), key: key.clone() }, Work { due_at, ..Work::default() }),
        };
        match store.commit(Tx { conds: vec![cond], writes: vec![put(WORK, key.clone(), to_json(&w)?)] }).await {
            Ok(_) => return Ok(()),
            Err(Refusal::Conflict { .. }) => {}
            Err(e) => return Err(refused(e)),
        }
    }
    anyhow::bail!("release {release}'s work kept moving under its schedule")
}

/// Takes the releases due now that no other server holds, for `lease_secs`: each under
/// compare-and-set on its row, so two servers never both take one, and a lease that lapses (its
/// server gone) is another's to take.
pub async fn claim(store: Recs<'_>, owner: &str, now: i64, lease_secs: i64, max: usize) -> anyhow::Result<Vec<i64>> {
    let mut due: Vec<(i64, i64, kiln_seams::records::Rev, Work)> = Vec::new();
    for row in rows(store, WORK, b"", FOLLOWED).await? {
        let (Some(id), Ok(w)) = (number(&row.key, false), from_json::<Work>(&row.value)) else { continue };
        if w.due_at <= now && (w.lease_until <= now || w.owner == owner) {
            due.push((w.due_at, id, row.rev, w));
        }
    }
    due.sort_by_key(|(at, id, ..)| (*at, *id));
    let mut taken = Vec::new();
    for (_, id, rev, w) in due {
        if taken.len() >= max {
            break;
        }
        let leased = Work { owner: owner.to_owned(), lease_until: now.saturating_add(lease_secs), ..w };
        let key = ascending(id).to_vec();
        match store.commit(Tx { conds: vec![Cond::At { family: WORK.into(), key: key.clone(), rev }], writes: vec![put(WORK, key, to_json(&leased)?)] }).await {
            Ok(_) => taken.push(id),
            // Another server took it, or it moved: not this one's now.
            Err(Refusal::Conflict { .. }) => {}
            Err(e) => return Err(refused(e)),
        }
    }
    Ok(taken)
}

/// Done following a release.
pub async fn done(store: Recs<'_>, release: i64) -> anyhow::Result<()> {
    store.commit(Tx { conds: Vec::new(), writes: vec![del(WORK, ascending(release).to_vec())] }).await.map_err(refused)?;
    Ok(())
}

// ------------------------------------------------------------------ fleet changes in sources

/// Keeps a fleet change's proposal in one source repository (§3.3).
pub async fn put_proposal(store: Recs<'_>, p: &super::sources::Proposal) -> anyhow::Result<()> {
    put_json(store, PROPOSAL, joined(&[&p.fleet, &p.repo]), p).await
}

/// A fleet change's proposals, by repository.
pub async fn proposals(store: Recs<'_>, fleet: &str) -> anyhow::Result<Vec<super::sources::Proposal>> {
    all(store, PROPOSAL, &under(&[fleet])).await
}

// ------------------------------------------------------------------ the rest

/// Deploy's other state, text by key (`deploy/move/<env>`, how far each environment has read its
/// release repository, what a mirror reported): what kiln's key-value table kept for it.
pub async fn kv_get(store: Recs<'_>, key: &str) -> anyhow::Result<Option<String>> {
    Ok(store.get(STATE, key.as_bytes(), Consistency::Linearizable).await.map_err(refused)?.map(|r| String::from_utf8_lossy(&r.value).into_owned()))
}

pub async fn kv_set(store: Recs<'_>, key: &str, value: &str) -> anyhow::Result<()> {
    store.commit(Tx { conds: Vec::new(), writes: vec![put(STATE, key.as_bytes().to_vec(), value.as_bytes().to_vec())] }).await.map_err(refused)?;
    Ok(())
}

/// Deploy's state under a key's prefix, in key order.
pub async fn kv_prefix(store: Recs<'_>, prefix: &str) -> anyhow::Result<Vec<(String, String)>> {
    Ok(rows(store, STATE, prefix.as_bytes(), LISTED).await?.into_iter().map(|r| (String::from_utf8_lossy(&r.key).into_owned(), String::from_utf8_lossy(&r.value).into_owned())).collect())
}

// ------------------------------------------------------------------ from before the seam

/// Copies what kiln's store held for deploy before its records moved onto the seam: every
/// environment, app, template, registration, release (with its number, so links still work),
/// lease and proposal, and deploy's key-value state. Once, at the first start that finds the
/// counter row missing; it's written last, so a copy cut short is done again whole. The tables
/// are left as they were, for the release before this one. How many rows it copied: 0 when
/// another server's copy was first.
pub async fn adopt(store: Recs<'_>, old: &crate::store::Store) -> anyhow::Result<usize> {
    if store.get(COUNTER, RELEASES, Consistency::Linearizable).await.map_err(refused)?.is_some() {
        return Ok(0);
    }
    copy(store, old).await
}

/// [`adopt`]'s copy. Each of its commits holds only while there's no counter row: two servers
/// starting together both copy until one writes the counter, the other's commits are refused from
/// then on, and what it copied before that was the same rows. So nothing it copies overwrites a
/// release made, or changed, after the first copy, and no release number comes back.
async fn copy(store: Recs<'_>, old: &crate::store::Store) -> anyhow::Result<usize> {
    let read = |sql: &'static str| {
        let old = old.clone();
        async move {
            old.call(move |c| {
                let mut s = c.prepare(sql)?;
                let rows = s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<Result<Vec<(String, String)>, _>>()?;
                Ok(rows)
            })
            .await
        }
    };
    let mut writes: Vec<Write> = Vec::new();
    for (name, spec) in read("SELECT name, spec FROM deploy_environments").await? {
        writes.push(put(ENV, name.into_bytes(), spec.into_bytes()));
    }
    for (name, spec) in read("SELECT name, spec FROM deploy_apps").await? {
        writes.push(put(APP, name.into_bytes(), spec.into_bytes()));
    }
    for (name, spec) in read("SELECT name, spec FROM deploy_templates").await? {
        writes.push(put(TEMPLATE, name.into_bytes(), spec.into_bytes()));
    }
    for (_, spec) in read("SELECT app, spec FROM deploy_registrations").await? {
        let r: Registration = from_json(spec.as_bytes())?;
        writes.push(put(REGISTRATION, joined(&[&r.app, &r.environment]), spec.into_bytes()));
    }
    for (_, spec) in read("SELECT fleet, spec FROM deploy_fleet_proposals").await? {
        let p: super::sources::Proposal = from_json(spec.as_bytes())?;
        writes.push(put(PROPOSAL, joined(&[&p.fleet, &p.repo]), spec.into_bytes()));
    }
    let mut last = 0;
    for (_, spec) in read("SELECT CAST(id AS TEXT), spec FROM deploy_releases ORDER BY id").await? {
        let r: Release = from_json(spec.as_bytes())?;
        last = last.max(r.id);
        let id = r.id.to_string().into_bytes();
        writes.push(put(RELEASE, release_key(r.id), spec.into_bytes()));
        if let Some(k) = &r.idempotency_key {
            writes.push(put(KEYS, idempotency_row(&r.app, &r.environment, k), id.clone()));
        }
        writes.extend(indexes(&r).into_iter().map(|(f, k)| put(f, k, id.clone())));
    }
    for (release, row) in read("SELECT CAST(release AS TEXT), json_object('due_at', due_at, 'owner', COALESCE(lease_owner, ''), 'lease_until', COALESCE(lease_until, 0)) FROM deploy_work").await? {
        writes.push(put(WORK, ascending(text(release.as_bytes())?).to_vec(), row.into_bytes()));
    }
    for (k, v) in old.kv_prefix("deploy/".into()).await? {
        writes.push(put(STATE, k.into_bytes(), v.into_bytes()));
    }
    let n = writes.len();
    let first = || vec![Cond::Absent { family: COUNTER.into(), key: RELEASES.to_vec() }];
    // In commits the seam takes (1000 writes each), the counter in the last.
    let counter = vec![put(COUNTER, RELEASES.to_vec(), last.to_string().into_bytes())];
    for chunk in writes.chunks(1000).map(<[Write]>::to_vec).chain(std::iter::once(counter)) {
        match store.commit(Tx { conds: first(), writes: chunk }).await {
            Ok(_) => {}
            // Another server's copy was first, and stands.
            Err(Refusal::Conflict { .. }) => return Ok(0),
            Err(e) => return Err(refused(e)),
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What kiln's tables held for deploy before its records moved onto the seam is copied once:
    /// each record, each release with its number, key, indexes and lease, and deploy's key-value
    /// state; the next release is numbered after the last, and a second start copies nothing.
    #[test]
    fn what_kiln_s_tables_held_is_copied_onto_the_seam_once() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let old = crate::store::Store::in_memory()?;
            // As the release before this one kept them.
            old.call(|c| {
                c.execute_batch(
                    r#"INSERT INTO deploy_environments (name, spec, updated_at) VALUES ('staging', '{"name":"staging"}', 0);
                    INSERT INTO deploy_apps (name, spec, updated_at) VALUES ('web', '{"name":"web","package":{"kind":"helm","repo":"oci://ghcr.io/acme/charts","chart":"web"}}', 0);
                    INSERT INTO deploy_registrations (app, environment, spec) VALUES ('web', 'staging', '{"app":"web","environment":"staging","enabled":true,"created_at":0}');
                    INSERT INTO deploy_releases (id, app, environment, state, spec, created_at, idempotency_key) VALUES
                      (3, 'web', 'staging', 'succeeded', '{"id":3,"app":"web","environment":"staging","kind":"deploy","state":"succeeded","requested_by":"ada","created_at":0,"idempotency_key":"k1"}', 0, 'k1'),
                      (7, 'web', 'staging', 'delivering', '{"id":7,"app":"web","environment":"staging","kind":"deploy","state":"delivering","requested_by":"ada","created_at":1}', 1, NULL);
                    INSERT INTO deploy_work (release, due_at, lease_owner, lease_until) VALUES (7, 100, 'server-old', 50);"#,
                )?;
                Ok(())
            })
            .await?;
            old.kv_set("deploy/move/staging".into(), r#"{"phase":"compared"}"#.into()).await?;
            let recs = kiln_seams::records::SimRecords::new();
            let st: Recs<'_> = &recs;
            anyhow::ensure!(adopt(st, &old).await? > 0, "rows copied");
            anyhow::ensure!(st.get(ENV, b"staging", Consistency::Linearizable).await.map_err(refused)?.is_some(), "the environment");
            anyhow::ensure!(apps(st).await?.iter().map(|a| a.name.as_str()).collect::<Vec<_>>() == ["web"], "the app");
            anyhow::ensure!(registration(st, "web", "staging").await?.is_some(), "the registration");
            let listed: Vec<i64> = releases(st, ReleaseQuery { app: Some("web".into()), environment: Some("staging".into()), limit: 10, ..ReleaseQuery::default() }).await?.iter().map(|r| r.id).collect();
            anyhow::ensure!(listed == [7, 3], "each release with its number, newest first: {listed:?}");
            anyhow::ensure!(release_by_key(st, "web", "staging", "k1").await?.map(|r| r.id) == Some(3), "its idempotency key");
            anyhow::ensure!(in_flight(st, "web", "staging").await?.map(|r| r.id) == Some(7), "the release in flight");
            anyhow::ensure!(claim(st, "server-new", 200, 60, 16).await? == [7], "its lease, lapsed, another's to take");
            anyhow::ensure!(kv_get(st, "deploy/move/staging").await?.is_some(), "deploy's key-value state");
            // The next release is numbered after the last copied.
            let next = Release { id: 0, idempotency_key: None, state: State::Waiting, ..release(st, 3).await?.ok_or_else(|| anyhow::anyhow!("release 3"))? };
            anyhow::ensure!(insert_release(st, &next).await?.0.id == 8, "numbered after the last");
            anyhow::ensure!(adopt(st, &old).await? == 0, "copied once");
            Ok(())
        })
    }

    /// Two servers starting together both find the counter missing. Whichever copies first stands;
    /// the other's copy, refused once the counter is written, overwrites nothing: a release made
    /// after the first copy keeps its number, and one changed keeps its change.
    #[test]
    fn two_servers_starting_together_copy_once() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let old = crate::store::Store::in_memory()?;
            old.call(|c| {
                c.execute_batch(
                    r#"INSERT INTO deploy_releases (id, app, environment, state, spec, created_at, idempotency_key) VALUES
                      (7, 'web', 'staging', 'delivering', '{"id":7,"app":"web","environment":"staging","kind":"deploy","state":"delivering","requested_by":"ada","created_at":1}', 1, NULL);"#,
                )?;
                Ok(())
            })
            .await?;
            let recs = kiln_seams::records::SimRecords::new();
            let st: Recs<'_> = &recs;
            // Server A copies, then a release is made and another changed.
            anyhow::ensure!(adopt(st, &old).await? > 0, "server A copies");
            let seven = release(st, 7).await?.ok_or_else(|| anyhow::anyhow!("release 7"))?;
            let made = insert_release(st, &Release { id: 0, state: State::Waiting, ..seven.clone() }).await?.0;
            anyhow::ensure!(made.id == 8, "numbered after the copy: {}", made.id);
            anyhow::ensure!(update_release(st, &Release { state: State::Succeeded, ..seven }, State::Delivering).await?, "release 7 lands");
            // Server B found the counter missing too, before A wrote it: its copy is refused now.
            anyhow::ensure!(copy(st, &old).await? == 0, "server B's copy stands down");
            anyhow::ensure!(release(st, 7).await?.map(|r| r.state) == Some(State::Succeeded), "release 7 keeps its change");
            anyhow::ensure!(release(st, 8).await?.is_some(), "release 8 is still there");
            let next = insert_release(st, &Release { id: 0, ..made }).await?.0;
            anyhow::ensure!(next.id == 9, "no number comes back: {}", next.id);
            Ok(())
        })
    }

    /// A family holding more than its bound is refused, not read short: a list isn't cut off
    /// without a word, nor a claim blind to the leases past it.
    #[test]
    fn a_family_past_its_bound_is_refused_not_read_short() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let recs = kiln_seams::records::SimRecords::new();
            let st: Recs<'_> = &recs;
            for i in 0..5 {
                schedule(st, i, 10).await?;
            }
            anyhow::ensure!(rows(st, WORK, b"", 5).await?.len() == 5, "at its bound, every row");
            let past = rows(st, WORK, b"", 3).await;
            anyhow::ensure!(past.as_ref().is_err_and(|e| e.to_string().contains("deploy.work holds more than 3 rows")), "past it, refused: {past:?}");
            Ok(())
        })
    }

    /// Two servers racing for one release's work: one takes it, the other doesn't, and once the
    /// lease lapses the other may.
    #[test]
    fn a_lease_is_one_server_s_until_it_lapses() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let recs = kiln_seams::records::SimRecords::new();
            let st: Recs<'_> = &recs;
            schedule(st, 5, 10).await?;
            let (a, b) = futures::join!(claim(st, "a", 10, 60, 16), claim(st, "b", 10, 60, 16));
            let (a, b) = (a?, b?);
            anyhow::ensure!(a.len() + b.len() == 1, "one of them takes it: {a:?}, {b:?}");
            let (holder, other) = if a.is_empty() { ("b", "a") } else { ("a", "b") };
            anyhow::ensure!(claim(st, other, 30, 60, 16).await?.is_empty(), "not the other's while the lease holds");
            anyhow::ensure!(claim(st, holder, 30, 60, 16).await? == [5], "the holder renews it");
            anyhow::ensure!(claim(st, other, 91, 60, 16).await? == [5], "the other's once it lapses");
            done(st, 5).await?;
            anyhow::ensure!(claim(st, holder, 1000, 60, 16).await?.is_empty(), "done: nothing left");
            Ok(())
        })
    }
}
