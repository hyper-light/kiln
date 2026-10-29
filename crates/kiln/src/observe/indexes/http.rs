//! Indexes' API (`docs/observe.md` §8.10): Datadog's (`/api/v1/logs/config/indexes`,
//! `…/index-order`) at kiln's paths, its bodies and answers; an index as Datadog gives one
//! (`is_rate_limited` its quota's word) and, beside, kiln's: its part, and its day against its
//! quota.

use std::sync::{Arc, OnceLock, PoisonError};

use ::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::super::api::{Fail, fail};
use super::{ExclusionDef, Filter, IndexDef, Indexes, Kept, Reset};
use crate::http::server::{self as srv, Response};

/// The server's indexes, once it started.
static KEPT: OnceLock<Arc<Indexes>> = OnceLock::new();

pub fn set(i: Arc<Indexes>) {
    let _ = KEPT.set(i);
}

/// The server's indexes; none on a runner.
pub fn kept() -> Option<&'static Arc<Indexes>> {
    KEPT.get()
}

fn indexes() -> Result<&'static Arc<Indexes>, Fail> {
    KEPT.get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "log indexes are kept by the server"))
}

/// The indexes now, routed by: here from the next record, every runner told.
fn apply(store: &Indexes) {
    let defs = store.defs();
    super::activate(&defs);
    if let Some(o) = super::super::api::get() {
        o.set_indexes(defs, store.removed_parts());
    }
}

/// An index's day as kiln says it.
#[derive(Serialize)]
struct Today {
    /// Records it kept since its day began.
    count: u64,
    limit: u64,
    /// When its day began, and the next does (ms).
    start: i64,
    next_reset: i64,
    warned: bool,
}

#[derive(Serialize)]
struct KilnOut {
    part: u16,
    created: i64,
    modified: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    today: Option<Today>,
}

/// An index as the API gives one: Datadog's, and kiln's beside.
#[derive(Serialize)]
pub struct IndexOut<'a> {
    #[serde(flatten)]
    def: &'a IndexDef,
    is_rate_limited: bool,
    kiln: KilnOut,
}

impl<'a> IndexOut<'a> {
    pub fn of(k: &'a Kept) -> Self {
        let day = super::quota::day_of(&k.def.name).filter(|d| d.part == k.part);
        Self {
            def: &k.def,
            is_rate_limited: day.as_ref().is_some_and(|d| d.over),
            kiln: KilnOut {
                part: k.part,
                created: k.created,
                modified: k.modified,
                today: day.map(|d| Today { count: d.count, limit: d.limit, start: d.start, next_reset: d.next_reset(), warned: d.warned }),
            },
        }
    }
}

/// Datadog's `LogsIndexUpdateRequest`: an index whole but its name; `disable_daily_limit` its quota
/// gone.
#[derive(Debug, Deserialize)]
pub struct Update {
    pub filter: Filter,
    #[serde(default)]
    pub exclusion_filters: Vec<ExclusionDef>,
    #[serde(default)]
    pub daily_limit: Option<u64>,
    #[serde(default)]
    pub daily_limit_reset: Option<Reset>,
    #[serde(default)]
    pub daily_limit_warning_threshold_percentage: Option<f64>,
    #[serde(default)]
    pub disable_daily_limit: bool,
    #[serde(default)]
    pub num_retention_days: Option<u32>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Datadog's `LogsIndexesOrder`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Order {
    pub index_names: Vec<String>,
}

/// A store's change, on the blocking pool (it writes a file).
async fn change<R: Send + 'static>(f: impl FnOnce(&Indexes) -> Result<R, Fail> + Send + 'static) -> Result<R, Fail> {
    let store = Arc::clone(indexes()?);
    crate::rt::blocking(move || {
        let r = f(&store)?;
        apply(&store);
        Ok(r)
    })
    .await
    .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
}

/// `GET logs/config/indexes`.
pub fn list() -> Result<Response, Fail> {
    let all = indexes()?.list();
    Ok(srv::json(StatusCode::OK, &serde_json::json!({ "indexes": all.iter().map(IndexOut::of).collect::<Vec<_>>() })))
}

/// `GET logs/config/indexes/{name}`.
pub fn get(name: &str) -> Result<Response, Fail> {
    let k = indexes()?.get(name).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such index"))?;
    Ok(srv::json(StatusCode::OK, &IndexOut::of(&k)))
}

/// `POST logs/config/indexes`: made, last in the order (Datadog's 200), of a part the store holds
/// nothing of.
pub async fn create(def: IndexDef) -> Result<Response, Fail> {
    let k = change(move |s| {
        let taken: Vec<u16> = super::super::api::shared()
            .map(|store| {
                let store = store.read().unwrap_or_else(PoisonError::into_inner);
                store.log_parts().into_iter().chain(store.part_cutoffs().keys().copied()).collect()
            })
            .unwrap_or_default();
        s.create(def, &|p| taken.contains(&p)).map_err(|(over, e)| fail(if over { StatusCode::UNPROCESSABLE_ENTITY } else { StatusCode::BAD_REQUEST }, e))
    })
    .await?;
    Ok(srv::json(StatusCode::OK, &IndexOut::of(&k)))
}

/// `PUT logs/config/indexes/{name}`.
pub async fn update(name: &str, u: Update) -> Result<Response, Fail> {
    let name = name.to_owned();
    let def = IndexDef {
        name: name.clone(),
        filter: u.filter,
        exclusion_filters: u.exclusion_filters,
        daily_limit: u.daily_limit,
        daily_limit_reset: u.daily_limit_reset,
        daily_limit_warning_threshold_percentage: u.daily_limit_warning_threshold_percentage,
        num_retention_days: u.num_retention_days,
        tags: u.tags,
    };
    let k = change(move |s| s.put(&name, def, u.disable_daily_limit).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?.ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such index"))).await?;
    Ok(srv::json(StatusCode::OK, &IndexOut::of(&k)))
}

/// `DELETE logs/config/indexes/{name}`: its records no longer searched, its files let go; its name
/// and part never used again (`main`'s name aside).
pub async fn delete(name: &str) -> Result<Response, Fail> {
    let name = name.to_owned();
    match change(move |s| s.delete(&name).map_err(|e| fail(StatusCode::BAD_REQUEST, e))).await? {
        true => Ok(srv::json(StatusCode::OK, &serde_json::json!({}))),
        false => Err(fail(StatusCode::NOT_FOUND, "no such index")),
    }
}

/// `GET logs/config/index-order`.
pub fn order() -> Result<Response, Fail> {
    Ok(srv::json(StatusCode::OK, &Order { index_names: indexes()?.order() }))
}

/// `PUT logs/config/index-order`: every index, each once.
pub async fn set_order(o: Order) -> Result<Response, Fail> {
    let names = o.index_names;
    let order = change(move |s| {
        s.set_order(&names).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
        Ok(s.order())
    })
    .await?;
    Ok(srv::json(StatusCode::OK, &Order { index_names: order }))
}

/// The parts a read of the live logs reads: those of the indexes `names` (each must be one), or
/// of every index and kiln's events; none said where no indexes are kept here (a runner's API).
pub fn searchable(names: Option<&[String]>) -> Result<Option<Vec<u16>>, Fail> {
    let Some(store) = kept() else { return Ok(None) };
    let defs = store.defs();
    match names.filter(|n| !n.is_empty()) {
        None => Ok(Some(defs.iter().map(|k| k.part).chain([kiln_obs::logs::store::EVENTS]).collect())),
        Some(names) => names
            .iter()
            .map(|n| defs.iter().find(|k| k.def.name == *n).map(|k| k.part).ok_or_else(|| fail(StatusCode::BAD_REQUEST, format!("no index named {n:?}"))))
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
    }
}
