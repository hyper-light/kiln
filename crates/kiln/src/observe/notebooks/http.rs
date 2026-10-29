//! Notebooks' API (`docs/observe.md` §11.1): Datadog's v1 (`/api/v1/notebooks…`: a notebook's
//! `data` with its `id`, `type` and `attributes`, times RFC 3339) at kiln's paths with a session
//! (`/api/observe/v1/notebooks…`): made, listed (Datadog's filters, order and pages), read, changed
//! whole and removed; each graph cell with how kiln draws it beside (`kiln.widget`). And kiln's own:
//! an incident's postmortem made of a template (`POST /api/observe/v1/incidents/{id}/postmortem`).
//! At Datadog's own paths the ingest token, a writer's of data, reads and changes no notebook.

use std::collections::HashMap;
use std::sync::Arc;

use ::http::StatusCode;
use serde::Serialize;
use serde_json::{Value, json};

use super::super::api::{Fail, fail, read_json};
use super::super::dashboards::Widget;
use super::super::ddash::{self, Said};
use super::super::incidents::http::Time;
use super::{Metadata, Notebook, Notebooks, Pick, Refused, Summary, draft_of, postmortem, sort};
use crate::http::server::{self as srv, Request, Response};

/// A request's body at the most: a notebook's bound, and room for its envelope.
const BODY: usize = super::NOTEBOOK + (64 << 10);
/// A list's page when it does not say (Datadog's), and at the most.
pub const COUNT: usize = 100;
pub const COUNT_MAX: usize = 1_000;

fn kept() -> Result<&'static Arc<Notebooks>, Fail> {
    super::get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "notebooks are kept by the server"))
}

fn refused(e: Refused) -> Fail {
    let code = match &e {
        Refused::Invalid(_) => StatusCode::BAD_REQUEST,
        Refused::NoNotebook => StatusCode::NOT_FOUND,
        Refused::Full(_) => StatusCode::CONFLICT,
    };
    fail(code, e)
}

/// Who asks: as the audit names the caller (`user:<name>`, `admin`).
fn caller(req: &Request) -> String {
    req.extensions().get::<crate::authz::Caller>().map_or_else(|| "kiln".to_owned(), |c| c.name.clone())
}

/// Something done to the notebooks, on the blocking pool (it reads or writes and syncs a file).
async fn change<T: Send + 'static>(f: impl FnOnce(&Notebooks, i64) -> Result<T, Fail> + Send + 'static) -> Result<T, Fail> {
    let k = kept()?.clone();
    crate::rt::blocking(move || f(&k, super::super::clock_ms())).await.map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
}

fn id_of(id: &str) -> Result<u64, Fail> {
    id.parse().map_err(|_| fail(StatusCode::NOT_FOUND, "no such notebook"))
}

#[derive(Serialize)]
struct Author<'a> {
    handle: &'a str,
    name: &'a str,
}

/// How kiln draws a graph cell, and what it said where it draws it otherwise or not at all.
#[derive(Serialize)]
struct KilnCell {
    widget: Option<Widget>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    said: Vec<Said>,
}

#[derive(Serialize)]
struct CellOut<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    attributes: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    kiln: Option<KilnCell>,
}

fn cell_out(c: &super::Cell) -> CellOut<'_> {
    let kiln = c.attributes.get("definition").filter(|d| d.get("type").and_then(Value::as_str) != Some("markdown")).map(|d| {
        let (widget, report) = ddash::cell_widget(d);
        KilnCell { widget, said: report.otherwise.into_iter().chain(report.not_drawn).collect() }
    });
    CellOut { id: &c.id, kind: "notebook_cells", attributes: &c.attributes, kiln }
}

#[derive(Serialize)]
struct KilnOut<'a> {
    /// The incident it is the postmortem of.
    incident: Option<&'a str>,
}

#[derive(Serialize)]
struct Attributes<'a> {
    author: Author<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cells: Option<Vec<CellOut<'a>>>,
    created: Time,
    modified: Time,
    name: &'a str,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    time: Option<&'a Value>,
    metadata: &'a Metadata,
    template_variables: Option<&'a [Value]>,
    kiln: KilnOut<'a>,
}

/// A notebook as Datadog's API gives one (its cells and time; a list's without them when it asks).
#[derive(Serialize)]
pub struct NotebookOut<'a> {
    id: u64,
    #[serde(rename = "type")]
    kind: &'static str,
    attributes: Attributes<'a>,
}

impl<'a> NotebookOut<'a> {
    pub fn of(n: &'a Notebook) -> Self {
        NotebookOut {
            id: n.id,
            kind: "notebooks",
            attributes: Attributes {
                author: Author { handle: &n.author, name: &n.author },
                cells: Some(n.cells.iter().map(cell_out).collect()),
                created: Time(Some(n.created)),
                modified: Time(Some(n.modified)),
                name: &n.name,
                status: "published",
                time: Some(&n.time),
                metadata: &n.metadata,
                template_variables: n.template_variables.as_deref(),
                kiln: KilnOut { incident: n.incident.as_deref() },
            },
        }
    }

    fn summary(s: &'a Summary) -> Self {
        NotebookOut {
            id: s.id,
            kind: "notebooks",
            attributes: Attributes {
                author: Author { handle: &s.author, name: &s.author },
                cells: None,
                created: Time(Some(s.created)),
                modified: Time(Some(s.modified)),
                name: &s.name,
                status: "published",
                time: None,
                metadata: &s.metadata,
                template_variables: None,
                kiln: KilnOut { incident: s.incident.as_deref() },
            },
        }
    }
}

#[derive(Serialize)]
struct Data<T> {
    data: T,
}

fn flag(q: &HashMap<String, String>, k: &str, d: bool) -> Result<bool, Fail> {
    match q.get(k).map(String::as_str) {
        None => Ok(d),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(_) => Err(fail(StatusCode::BAD_REQUEST, format!("{k}: true or false"))),
    }
}

/// `GET notebooks`: Datadog's list: `author_handle`, `exclude_author_handle`, `query` (a part of
/// the name or the author's handle), `is_template`, `type`; `sort_field` and `sort_dir`; `start`
/// and `count` (100, 1,000 at the most); `include_cells` (its cells and time, read from its file).
pub async fn list(q: &HashMap<String, String>) -> Result<Response, Fail> {
    let text = |k: &str| q.get(k).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let pick = Pick { author: text("author_handle"), not_author: text("exclude_author_handle"), is_template: flag(q, "is_template", false)?, kind: text("type"), ..Pick::default() }
        .query(text("query").as_deref());
    let num = |k: &str, d: usize| q.get(k).map_or(Ok(d), |v| v.trim().parse::<usize>().map_err(|_| fail(StatusCode::BAD_REQUEST, format!("{k}: a number"))));
    let (start, count) = (num("start", 0)?, num("count", COUNT)?.clamp(1, COUNT_MAX));
    let field = text("sort_field").unwrap_or_else(|| "modified".to_owned());
    let desc = match text("sort_dir").as_deref() {
        None | Some("desc") => true,
        Some("asc") => false,
        Some(_) => return Err(fail(StatusCode::BAD_REQUEST, "sort_dir: asc or desc")),
    };
    let cells = flag(q, "include_cells", true)?;
    let (page, total, filtered) = kept()?.read_summaries(|all| -> Result<_, Fail> {
        let mut picked: Vec<&Summary> = all.values().filter(|s| pick.keeps(s)).collect();
        sort(&mut picked, &field, desc).map_err(refused)?;
        Ok((picked.iter().skip(start).take(count).map(|s| (*s).clone()).collect::<Vec<_>>(), all.len(), picked.len()))
    })?;
    let meta = json!({ "page": { "total_count": total, "total_filtered_count": filtered } });
    if !cells {
        return Ok(srv::json(StatusCode::OK, &json!({ "data": page.iter().map(NotebookOut::summary).collect::<Vec<_>>(), "meta": meta })));
    }
    let read = change(move |k, _| Ok(page.iter().filter_map(|s| k.get(s.id)).collect::<Vec<_>>())).await?;
    Ok(srv::json(StatusCode::OK, &json!({ "data": read.iter().map(NotebookOut::of).collect::<Vec<_>>(), "meta": meta })))
}

/// `POST notebooks`: one made (200, as Datadog's).
pub async fn create(req: Request) -> Result<Response, Fail> {
    let by = caller(&req);
    let body: Value = read_json(req.into_body(), BODY).await?;
    let d = draft_of(&body).map_err(refused)?;
    let n = change(move |k, now| k.create(d, &by, now, None).map_err(refused)).await?;
    Ok(srv::json(StatusCode::OK, &Data { data: NotebookOut::of(&n) }))
}

/// `GET notebooks/{id}`.
pub async fn get_one(id: &str) -> Result<Response, Fail> {
    let id = id_of(id)?;
    let n = change(move |k, _| k.get(id).ok_or_else(|| refused(Refused::NoNotebook))).await?;
    Ok(srv::json(StatusCode::OK, &Data { data: NotebookOut::of(&n) }))
}

/// `PUT notebooks/{id}`: changed whole, as Datadog's body says (its cells' ids kept where given).
pub async fn update(id: &str, req: Request) -> Result<Response, Fail> {
    let id = id_of(id)?;
    let body: Value = read_json(req.into_body(), BODY).await?;
    let d = draft_of(&body).map_err(refused)?;
    let n = change(move |k, now| k.update(id, d, now).map_err(refused)).await?;
    Ok(srv::json(StatusCode::OK, &Data { data: NotebookOut::of(&n) }))
}

/// `DELETE notebooks/{id}`.
pub async fn remove(id: &str) -> Result<Response, Fail> {
    let id = id_of(id)?;
    if change(move |k, _| k.remove(id).map_err(refused)).await? { Ok(srv::status(StatusCode::NO_CONTENT)) } else { Err(refused(Refused::NoNotebook)) }
}

/// `POST incidents/{id}/postmortem` (kiln's): the incident's postmortem made, a notebook of
/// `template_id` (a template's) or of kiln's own template, and attached to it as its postmortem.
/// One an incident: an incident with one is refused (409), and a notebook made as another was
/// attached is removed.
pub async fn postmortem_of(incident: &str, req: Request) -> Result<Response, Fail> {
    let by = caller(&req);
    let body: Value = read_json(req.into_body(), 64 << 10).await?;
    let template = match body.get("template_id") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).ok_or_else(|| fail(StatusCode::BAD_REQUEST, "template_id: a notebook's id"))?),
    };
    let incidents = super::super::incidents::get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "incidents are kept by the server"))?.clone();
    let incident = incident.to_owned();
    let n = change(move |k, now| {
        let i = incidents.get(&incident).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such incident"))?;
        if i.has_postmortem() {
            return Err(fail(StatusCode::CONFLICT, "the incident has a postmortem: remove it to make another"));
        }
        let t = template.map(|id| k.get(id).filter(|t| t.metadata.is_template).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such template: a notebook that is a template"))).transpose()?;
        let n = k.create(postmortem::draft(&i, t.as_ref(), &k.link_base, now), &by, now, Some(i.id.clone())).map_err(refused)?;
        let url = format!("{}/observe/notebooks/{}", k.link_base, n.id);
        match incidents.attach(&i.id, "postmortem", &url, &n.name, &by, now) {
            Ok(_) => Ok(n),
            Err(e) => {
                if let Err(r) = k.remove(n.id) {
                    tracing::warn!(notebook = n.id, error = %r, "a postmortem not attached was not removed");
                }
                Err(fail(StatusCode::CONFLICT, e))
            }
        }
    })
    .await?;
    Ok(srv::json(StatusCode::CREATED, &Data { data: NotebookOut::of(&n) }))
}

/// Datadog's notebooks API at its paths: a writer's token reads and changes no notebook.
pub fn datadog() -> Response {
    srv::json(StatusCode::FORBIDDEN, &json!({ "errors": ["notebooks are read and changed through kiln's API (/api/observe/v1/notebooks), with a session"] }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A notebook as Datadog's API gives it: its id a number, its cells `notebook_cells` with their
    /// attributes as given and kiln's drawing beside a graph's (none beside Markdown), times RFC 3339.
    #[test]
    fn a_notebook_reads_as_datadog_s() {
        let n = Notebook {
            id: 7,
            name: "Checkout".into(),
            cells: vec![
                super::super::Cell { id: "c1".into(), attributes: json!({ "definition": { "type": "markdown", "text": "# Why" } }) },
                super::super::Cell {
                    id: "c2".into(),
                    attributes: json!({ "definition": { "type": "timeseries", "requests": [{ "q": "avg:app.latency.p95{service:checkout}", "display_type": "line" }] }, "graph_size": "m" }),
                },
            ],
            time: json!({ "live_span": "1h" }),
            metadata: Metadata { is_template: false, take_snapshots: false, kind: Some("investigation".into()) },
            template_variables: None,
            author: "user:ada".into(),
            created: 1_792_670_400_000,
            modified: 1_792_670_460_000,
            incident: None,
            next: 2,
        };
        let v = serde_json::to_value(NotebookOut::of(&n)).unwrap();
        assert_eq!((v["id"].as_u64(), v["type"].as_str()), (Some(7), Some("notebooks")));
        let a = &v["attributes"];
        assert_eq!((a["created"].as_str(), a["status"].as_str(), a["author"]["handle"].as_str()), (Some("2026-10-22T12:00:00Z"), Some("published"), Some("user:ada")));
        assert_eq!(a["cells"][0]["type"], "notebook_cells");
        assert!(a["cells"][0].get("kiln").is_none(), "Markdown is drawn as it is");
        assert_eq!(a["cells"][1]["kiln"]["widget"]["kind"], "timeseries");
        assert_eq!(a["cells"][1]["kiln"]["widget"]["queries"][0], "avg:app.latency.p95{service:checkout}");
        assert_eq!(a["metadata"], json!({ "is_template": false, "take_snapshots": false, "type": "investigation" }));
    }
}
