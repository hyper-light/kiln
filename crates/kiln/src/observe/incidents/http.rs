//! Incidents' API (`docs/observe.md` §9.5): Datadog's (`/api/v2/incidents…`, JSON:API: an
//! incident's `data` with its `attributes` and `relationships`, times RFC 3339, durations seconds),
//! at kiln's paths with a session (`/api/observe/v1/incidents…`): declared, listed, searched (with
//! its facets), read, changed and removed; its todos (`relationships/todos`) and attachments; and
//! kiln's own, its timeline (read, a note put on it). At Datadog's own paths the ingest token, a
//! writer's of data, reads and changes no incident.

use std::collections::HashMap;

use ::http::StatusCode;
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::super::api::{Fail, fail, read_json};
use super::{Cell, Change, Field, Handle, Incident, Incidents, Refused, search};
use crate::http::server::{self as srv, Request, Response};

/// A page's incidents when it does not say, and at the most (Datadog's).
pub const PAGE: usize = 10;
pub const PAGE_MAX: usize = 100;
/// A request's body at the most.
const BODY: usize = 1 << 20;

fn kept() -> Result<&'static std::sync::Arc<Incidents>, Fail> {
    super::get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "incidents are kept by the server"))
}

fn refused(e: Refused) -> Fail {
    let code = match &e {
        Refused::Invalid(_) => StatusCode::BAD_REQUEST,
        Refused::NoIncident | Refused::NoItem(_) => StatusCode::NOT_FOUND,
        Refused::Full(_) => StatusCode::CONFLICT,
    };
    fail(code, e)
}

/// A change to the incidents, on the blocking pool (it writes and syncs a file, under the store's
/// lock): what `f` returns, or why it was refused.
async fn change<T: Send + 'static>(f: impl FnOnce(&Incidents, i64) -> Result<T, Refused> + Send + 'static) -> Result<T, Fail> {
    let k = kept()?.clone();
    crate::rt::blocking(move || f(&k, super::super::clock_ms())).await.map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.map_err(refused)
}

/// Who asks: as the audit names the caller (`user:<name>`, `admin`).
fn caller(req: &Request) -> String {
    req.extensions().get::<crate::authz::Caller>().map_or_else(|| "kiln".to_owned(), |c| c.name.clone())
}

/// A time as Datadog's API gives one: RFC 3339 (null when there is none), written as it is
/// serialized: nothing allocated.
#[derive(Debug, Clone, Copy)]
pub struct Time(pub Option<i64>);

impl Serialize for Time {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let Some(t) = self.0.and_then(|ms| time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()) else { return s.serialize_none() };
        let mut buf = [0u8; 64];
        let mut w = buf.as_mut_slice();
        // Its length is how far the writer went: `format_into`'s count is short of a fraction of a
        // second (`time` 0.3.55 counts `.123Z` as `.1`; a test holds this to `format`).
        if t.format_into(&mut w, &time::format_description::well_known::Rfc3339).is_err() {
            return s.serialize_none();
        }
        let n = 64 - w.len();
        s.serialize_str(buf.get(..n).and_then(|b| std::str::from_utf8(b).ok()).unwrap_or(""))
    }
}

const fn at(ms: i64) -> Time {
    Time(Some(ms))
}

/// A time as the API takes one: RFC 3339, or ms since the epoch.
fn time_of(v: &Value, what: &str) -> Result<Option<i64>, Fail> {
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => n.as_i64().map(Some).ok_or_else(|| fail(StatusCode::BAD_REQUEST, format!("{what}: a time"))),
        Value::String(s) => time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
            .map(|t| Some(i64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX)))
            .map_err(|_| fail(StatusCode::BAD_REQUEST, format!("{what}: RFC 3339 (2026-10-22T12:00:00Z)"))),
        _ => Err(fail(StatusCode::BAD_REQUEST, format!("{what}: a time"))),
    }
}

/// A user, as a relationship names one.
#[derive(Serialize)]
struct UserRef<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    id: &'a str,
}

fn user(id: &str) -> UserRef<'_> {
    UserRef { kind: "users", id }
}

#[derive(Serialize)]
struct Data<T> {
    data: T,
}

/// A field as Datadog's API gives it: its type and value.
#[derive(Serialize)]
struct FieldOut<'a, V: Serialize> {
    #[serde(rename = "type")]
    kind: &'a str,
    value: V,
}

/// An incident's fields, its severity and state among them, as Datadog's API gives them.
struct Fields<'a>(&'a Incident);

impl Serialize for Fields<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        let i = self.0;
        let mut m = s.serialize_map(Some(i.fields.len() + 2))?;
        for (k, f) in i.fields.iter().filter(|(k, _)| *k != "severity" && *k != "state") {
            m.serialize_entry(k, &FieldOut { kind: &f.kind, value: &f.value })?;
        }
        m.serialize_entry("severity", &FieldOut { kind: "dropdown", value: i.severity.as_str() })?;
        m.serialize_entry("state", &FieldOut { kind: "dropdown", value: i.state.as_str() })?;
        m.end()
    }
}

#[derive(Serialize)]
struct Name<'a> {
    name: &'a str,
}

#[derive(Serialize)]
struct Postmortem<'a> {
    url: &'a str,
    title: &'a str,
}

/// kiln's own beside Datadog's: its monitor, its todos counted, its postmortem.
#[derive(Serialize)]
struct KilnOut<'a> {
    monitor: Option<&'a super::FromMonitor>,
    todos: usize,
    todos_open: usize,
    postmortem: Option<Postmortem<'a>>,
}

#[derive(Serialize)]
struct Attributes<'a> {
    public_id: u64,
    title: &'a str,
    customer_impacted: bool,
    customer_impact_scope: Option<&'a str>,
    customer_impact_start: Time,
    customer_impact_end: Time,
    customer_impact_duration: Option<i64>,
    time_to_detect: Option<i64>,
    time_to_internal_response: Option<i64>,
    time_to_repair: Option<i64>,
    time_to_resolve: Option<i64>,
    created: Time,
    modified: Time,
    detected: Time,
    declared: Time,
    resolved: Time,
    severity: &'a str,
    state: &'a str,
    fields: Fields<'a>,
    notification_handles: &'a [Handle],
    is_test: bool,
    visibility: &'static str,
    archived: Option<()>,
    declared_by: Name<'a>,
    kiln: KilnOut<'a>,
}

#[derive(Serialize)]
struct Kept<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    id: &'a str,
}

#[derive(Serialize)]
struct Relationships<'a> {
    commander_user: Data<Option<UserRef<'a>>>,
    created_by_user: Data<UserRef<'a>>,
    declared_by_user: Data<UserRef<'a>>,
    last_modified_by_user: Data<UserRef<'a>>,
    attachments: Data<Vec<Kept<'a>>>,
    responders: Data<[(); 0]>,
    impacts: Data<[(); 0]>,
    integrations: Data<[(); 0]>,
}

/// An incident as Datadog's API gives one: `id`, `type`, `attributes` (its fields with its
/// severity and state among them, its figures), `relationships`; kiln's own beside under
/// `attributes.kiln`. Borrowed, written as it is serialized.
#[derive(Serialize)]
pub struct IncidentOut<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    attributes: Attributes<'a>,
    relationships: Relationships<'a>,
}

impl<'a> IncidentOut<'a> {
    pub fn of(i: &'a Incident) -> Self {
        let d = i.durations();
        IncidentOut {
            id: &i.id,
            kind: "incidents",
            attributes: Attributes {
                public_id: i.public_id,
                title: &i.title,
                customer_impacted: i.customer_impacted,
                customer_impact_scope: i.customer_impact_scope.as_deref(),
                customer_impact_start: Time(i.customer_impact_start),
                customer_impact_end: Time(i.customer_impact_end),
                customer_impact_duration: d.customer_impact_duration,
                time_to_detect: d.time_to_detect,
                time_to_internal_response: d.time_to_internal_response,
                time_to_repair: d.time_to_repair,
                time_to_resolve: d.time_to_resolve,
                created: at(i.created),
                modified: at(i.modified),
                detected: at(i.detected),
                declared: at(i.declared),
                resolved: Time(i.resolved),
                severity: &i.severity,
                state: &i.state,
                fields: Fields(i),
                notification_handles: &i.notification_handles,
                is_test: i.is_test,
                visibility: "organization",
                archived: None,
                declared_by: Name { name: &i.created_by },
                kiln: KilnOut {
                    monitor: i.monitor.as_ref(),
                    todos: i.todos.len(),
                    todos_open: i.todos.iter().filter(|t| t.completed.is_none()).count(),
                    postmortem: i.attachments.iter().find(|a| a.attachment_type == "postmortem").map(|a| Postmortem { url: &a.url, title: &a.title }),
                },
            },
            relationships: Relationships {
                commander_user: Data { data: i.commander.as_deref().map(user) },
                created_by_user: Data { data: user(&i.created_by) },
                declared_by_user: Data { data: user(&i.created_by) },
                last_modified_by_user: Data { data: user(&i.last_modified_by) },
                attachments: Data { data: i.attachments.iter().map(|a| Kept { kind: "incident_attachments", id: &a.id }).collect() },
                responders: Data { data: [] },
                impacts: Data { data: [] },
                integrations: Data { data: [] },
            },
        }
    }
}

/// An incident as the API gives it, as a value (tests, and what is not answered at once).
pub fn incident_json(i: &Incident) -> Value {
    serde_json::to_value(IncidentOut::of(i)).unwrap_or(Value::Null)
}

#[derive(Serialize)]
struct AttachmentBody<'a> {
    #[serde(rename = "documentUrl")]
    url: &'a str,
    title: &'a str,
}

#[derive(Serialize)]
struct AttachmentAttributes<'a> {
    attachment_type: &'a str,
    attachment: AttachmentBody<'a>,
    modified: Time,
}

#[derive(Serialize)]
struct AttachmentRelationships<'a> {
    incident: Data<Kept<'a>>,
}

#[derive(Serialize)]
struct AttachmentOut<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    id: &'a str,
    attributes: AttachmentAttributes<'a>,
    relationships: AttachmentRelationships<'a>,
}

fn attachment_out<'a>(incident: &'a str, a: &'a super::Attachment) -> AttachmentOut<'a> {
    AttachmentOut {
        kind: "incident_attachments",
        id: &a.id,
        attributes: AttachmentAttributes { attachment_type: &a.attachment_type, attachment: AttachmentBody { url: &a.url, title: &a.title }, modified: at(a.modified) },
        relationships: AttachmentRelationships { incident: Data { data: Kept { kind: "incidents", id: incident } } },
    }
}

#[derive(Serialize)]
struct TodoAttributes<'a> {
    content: &'a str,
    assignees: &'a [String],
    due_date: Option<&'a str>,
    completed: Time,
    created: Time,
    modified: Time,
    incident_id: &'a str,
}

#[derive(Serialize)]
struct TodoOut<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    id: &'a str,
    attributes: TodoAttributes<'a>,
}

fn todo_out<'a>(incident: &'a str, t: &'a super::Todo) -> TodoOut<'a> {
    TodoOut {
        kind: "incident_todos",
        id: &t.id,
        attributes: TodoAttributes {
            content: &t.content,
            assignees: &t.assignees,
            due_date: t.due_date.as_deref(),
            completed: Time(t.completed),
            created: at(t.created),
            modified: at(t.modified),
            incident_id: incident,
        },
    }
}

/// A timeline's cell: what it says, its time RFC 3339.
#[derive(Serialize)]
struct CellOut<'a> {
    id: &'a str,
    created: Time,
    author: &'a str,
    #[serde(flatten)]
    cell: &'a Cell,
}

fn cell_out(c: &super::TimelineCell) -> CellOut<'_> {
    CellOut { id: &c.id, created: at(c.created), author: &c.author, cell: &c.cell }
}

#[derive(Serialize)]
struct UserAttributes<'a> {
    name: &'a str,
    handle: &'a str,
    email: Option<()>,
    icon: Option<()>,
    uuid: &'a str,
}

#[derive(Serialize)]
struct UserOut<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    id: &'a str,
    attributes: UserAttributes<'a>,
}

/// What `include` asks of what is given: the users named, the attachments.
#[derive(Serialize)]
#[serde(untagged)]
enum Included<'a> {
    User(UserOut<'a>),
    Attachment(AttachmentOut<'a>),
}

fn included<'a>(incidents: &[&'a Incident], include: Option<&str>) -> Vec<Included<'a>> {
    let asks = |w: &str| include.is_some_and(|i| i.split(',').any(|x| x.trim() == w));
    let mut out = Vec::new();
    if asks("users") {
        let mut seen: Vec<&str> = Vec::new();
        for i in incidents {
            for u in i.commander.iter().map(String::as_str).chain([i.created_by.as_str(), i.last_modified_by.as_str()]) {
                if !seen.contains(&u) {
                    seen.push(u);
                    out.push(Included::User(UserOut { kind: "users", id: u, attributes: UserAttributes { name: u, handle: u, email: None, icon: None, uuid: u } }));
                }
            }
        }
    }
    if asks("attachments") {
        for i in incidents {
            out.extend(i.attachments.iter().map(|a| Included::Attachment(attachment_out(&i.id, a))));
        }
    }
    out
}

#[derive(Serialize)]
struct Pagination {
    offset: usize,
    size: usize,
    next_offset: Option<usize>,
}

#[derive(Serialize)]
struct Meta {
    pagination: Pagination,
}

fn pagination(offset: usize, size: usize, total: usize) -> Meta {
    Meta { pagination: Pagination { offset, size, next_offset: (offset + size < total).then_some(offset + size) } }
}

/// One incident answered: `data`, and what `include` asks.
#[derive(Serialize)]
struct One<'a> {
    data: IncidentOut<'a>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    included: Vec<Included<'a>>,
}

#[derive(Serialize)]
struct Listed<T> {
    data: Vec<T>,
    meta: Meta,
}

#[derive(Serialize)]
struct Many<'a> {
    data: Vec<IncidentOut<'a>>,
    included: Vec<Included<'a>>,
    meta: Meta,
}

#[derive(Serialize)]
struct SearchAttributes<'a> {
    facets: search::Facets<'a>,
    incidents: Vec<Data<IncidentOut<'a>>>,
    total: usize,
}

#[derive(Serialize)]
struct SearchData<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    attributes: SearchAttributes<'a>,
}

/// A search's answer, Datadog's.
#[derive(Serialize)]
pub struct SearchOut<'a> {
    data: SearchData<'a>,
    included: Vec<Included<'a>>,
    meta: Meta,
}

/// A page: `page[size]` (10, 100 at the most) from `page[offset]`.
fn page(q: &HashMap<String, String>) -> Result<(usize, usize), Fail> {
    let num = |k: &str, d: usize| q.get(k).map_or(Ok(d), |v| v.trim().parse::<usize>().map_err(|_| fail(StatusCode::BAD_REQUEST, format!("{k}: a number"))));
    Ok((num("page[size]", PAGE)?.clamp(1, PAGE_MAX), num("page[offset]", 0)?))
}

/// A string-or-null attribute: absent none, null `Some(None)`.
fn opt_text(a: &Map<String, Value>, k: &str) -> Result<Option<Option<String>>, Fail> {
    match a.get(k) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) => Ok(Some(Some(s.clone()))),
        Some(_) => Err(fail(StatusCode::BAD_REQUEST, format!("{k}: a text"))),
    }
}

/// A declaring's or a change's body (Datadog's `data.attributes` and `data.relationships`).
fn change_of(body: &Value) -> Result<Change, Fail> {
    let data = body.get("data").ok_or_else(|| fail(StatusCode::BAD_REQUEST, "the body is Datadog's: {\"data\": {\"type\": \"incidents\", \"attributes\": …}}"))?;
    if let Some(t) = data.get("type").and_then(Value::as_str)
        && t != "incidents"
    {
        return Err(fail(StatusCode::BAD_REQUEST, "data.type: incidents"));
    }
    let empty = Map::new();
    let a = data.get("attributes").and_then(Value::as_object).unwrap_or(&empty);
    let mut c = Change { title: opt_text(a, "title")?.flatten(), ..Change::default() };
    if let Some(v) = a.get("customer_impacted") {
        c.customer_impacted = Some(v.as_bool().ok_or_else(|| fail(StatusCode::BAD_REQUEST, "customer_impacted: true or false"))?);
    }
    c.customer_impact_scope = opt_text(a, "customer_impact_scope")?;
    for (k, slot) in [("customer_impact_start", &mut c.customer_impact_start), ("customer_impact_end", &mut c.customer_impact_end)] {
        if let Some(v) = a.get(k) {
            *slot = Some(time_of(v, k)?);
        }
    }
    if let Some(v) = a.get("detected") {
        c.detected = time_of(v, "detected")?;
    }
    c.severity = opt_text(a, "severity")?.flatten();
    c.state = opt_text(a, "state")?.flatten();
    if let Some(f) = a.get("fields") {
        let f = f.as_object().ok_or_else(|| fail(StatusCode::BAD_REQUEST, "fields: an object of fields by name"))?;
        for (name, v) in f {
            let field: Field = serde_json::from_value(v.clone()).map_err(|_| fail(StatusCode::BAD_REQUEST, format!("fields.{name}: {{\"type\": …, \"value\": …}}")))?;
            c.fields.insert(name.clone(), field);
        }
    }
    if let Some(h) = a.get("notification_handles") {
        c.notification_handles =
            Some(serde_json::from_value::<Vec<Handle>>(h.clone()).map_err(|_| fail(StatusCode::BAD_REQUEST, "notification_handles: [{\"display_name\": …, \"handle\": \"@slack-ops\"}]"))?);
    }
    if let Some(v) = a.get("is_test") {
        c.is_test = Some(v.as_bool().ok_or_else(|| fail(StatusCode::BAD_REQUEST, "is_test: true or false"))?);
    }
    if let Some(cells) = a.get("initial_cells").and_then(Value::as_array) {
        for cell in cells.iter().take(super::CELLS) {
            let content = cell
                .pointer("/content/content")
                .and_then(Value::as_str)
                .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "initial_cells: [{\"cell_type\": \"markdown\", \"content\": {\"content\": …}}]"))?;
            c.initial_cells.push((content.to_owned(), cell.get("important").and_then(Value::as_bool).unwrap_or(false)));
        }
    }
    if let Some(r) = data.pointer("/relationships/commander_user") {
        c.commander = Some(match r.get("data") {
            Some(Value::Null) | None => None,
            Some(d) => Some(d.get("id").and_then(Value::as_str).ok_or_else(|| fail(StatusCode::BAD_REQUEST, "commander_user: {\"data\": {\"type\": \"users\", \"id\": …}}"))?.to_owned()),
        });
    }
    Ok(c)
}

fn one<'a>(i: &'a Incident, q: &HashMap<String, String>) -> One<'a> {
    One { data: IncidentOut::of(i), included: included(&[i], q.get("include").map(String::as_str)) }
}

/// `POST incidents`: one declared.
pub async fn create(q: &HashMap<String, String>, req: Request) -> Result<Response, Fail> {
    let by = caller(&req);
    let body: Value = read_json(req.into_body(), BODY).await?;
    let c = change_of(&body)?;
    let i = change(move |k, now| k.declare(c, &by, now)).await?;
    Ok(srv::json(StatusCode::CREATED, &one(&i, q)))
}

/// `GET incidents`: a page of them, the newest first.
pub fn list(q: &HashMap<String, String>) -> Result<Response, Fail> {
    let (size, offset) = page(q)?;
    let include = q.get("include").map(String::as_str);
    Ok(kept()?.read_all(|all| {
        let shown: Vec<&Incident> = all.iter().skip(offset).take(size).copied().collect();
        srv::json(StatusCode::OK, &Many { data: shown.iter().map(|i| IncidentOut::of(i)).collect(), included: included(&shown, include), meta: pagination(offset, size, all.len()) })
    }))
}

/// `GET incidents/search`: those `query` matches, their facets counted, sorted by when declared.
pub fn search_of(q: &HashMap<String, String>) -> Result<Response, Fail> {
    let query = search::parse(q.get("query").map_or("", String::as_str)).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let (size, offset) = page(q)?;
    let newest_first = match q.get("sort").map(String::as_str) {
        None | Some("-created") => true,
        Some("created") => false,
        Some(_) => return Err(fail(StatusCode::BAD_REQUEST, "sort: created or -created")),
    };
    let include = q.get("include").map(String::as_str);
    Ok(kept()?.read_all(|all| srv::json(StatusCode::OK, &search_page(all, &query, newest_first, offset, size, include))))
}

/// A search's answer, Datadog's: what `query` matched of `all` (the newest first; the oldest first
/// unless `newest_first`), counted by facet, and a page of it, read in place.
pub fn search_page<'a>(all: &[&'a Incident], query: &search::Query, newest_first: bool, offset: usize, size: usize, include: Option<&str>) -> SearchOut<'a> {
    let mut matched: Vec<&Incident> = all.iter().filter(|i| query.matches(i)).copied().collect();
    if !newest_first {
        matched.reverse();
    }
    let shown: Vec<&Incident> = matched.iter().skip(offset).take(size).copied().collect();
    SearchOut {
        data: SearchData {
            kind: "incidents_search_results",
            attributes: SearchAttributes { facets: search::facets(&matched), incidents: shown.iter().map(|i| Data { data: IncidentOut::of(i) }).collect(), total: matched.len() },
        },
        included: included(&shown, include),
        meta: pagination(offset, size, matched.len()),
    }
}

/// `GET incidents/{id}` (its id, or its public id).
pub fn get_one(id: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    kept()?.read(id, |i| srv::json(StatusCode::OK, &one(i, q))).ok_or_else(no_incident)
}

/// `PATCH incidents/{id}`: what its body says changed.
pub async fn update(id: &str, q: &HashMap<String, String>, req: Request) -> Result<Response, Fail> {
    let by = caller(&req);
    let body: Value = read_json(req.into_body(), BODY).await?;
    let c = change_of(&body)?;
    let id = id.to_owned();
    let i = change(move |k, now| k.update(&id, c, &by, now)).await?;
    Ok(srv::json(StatusCode::OK, &one(&i, q)))
}

pub async fn remove(id: &str) -> Result<Response, Fail> {
    let id = id.to_owned();
    if change(move |k, _| k.remove(&id)).await? { Ok(srv::status(StatusCode::NO_CONTENT)) } else { Err(no_incident()) }
}

fn no_incident() -> Fail {
    fail(StatusCode::NOT_FOUND, "no such incident")
}

/// The id of the incident `id` names (its id, or its public id), for what is said of its parts.
fn id_of(id: &str) -> String {
    kept().ok().and_then(|k| k.read(id, |i| i.id.clone())).unwrap_or_else(|| id.to_owned())
}

/// `GET incidents/{id}/relationships/todos`.
pub fn todos(id: &str) -> Result<Response, Fail> {
    kept()?
        .read(id, |i| {
            let n = i.todos.len();
            srv::json(StatusCode::OK, &Listed { data: i.todos.iter().map(|t| todo_out(&i.id, t)).collect::<Vec<_>>(), meta: pagination(0, n.max(1), n) })
        })
        .ok_or_else(no_incident)
}

pub fn todo(id: &str, todo: &str) -> Result<Response, Fail> {
    kept()?
        .read(id, |i| i.todos.iter().find(|t| t.id == todo).map(|t| srv::json(StatusCode::OK, &Data { data: todo_out(&i.id, t) })))
        .ok_or_else(no_incident)?
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such todo"))
}

fn attributes(body: &Value) -> Result<&Map<String, Value>, Fail> {
    body.pointer("/data/attributes").and_then(Value::as_object).ok_or_else(|| fail(StatusCode::BAD_REQUEST, "the body is Datadog's: {\"data\": {\"attributes\": …}}"))
}

fn texts(v: Option<&Value>, what: &str) -> Result<Option<Vec<String>>, Fail> {
    v.map(|v| {
        v.as_array()
            .ok_or_else(|| fail(StatusCode::BAD_REQUEST, format!("{what}: a list")))?
            .iter()
            .map(|x| x.as_str().map(str::to_owned).or_else(|| x.get("handle").and_then(Value::as_str).map(str::to_owned)).ok_or_else(|| fail(StatusCode::BAD_REQUEST, format!("{what}: texts"))))
            .collect()
    })
    .transpose()
}

/// `POST incidents/{id}/relationships/todos`: one made.
pub async fn add_todo(id: &str, req: Request) -> Result<Response, Fail> {
    let by = caller(&req);
    let body: Value = read_json(req.into_body(), BODY).await?;
    let a = attributes(&body)?;
    let content = a.get("content").and_then(Value::as_str).unwrap_or_default();
    let assignees = texts(a.get("assignees"), "assignees")?.unwrap_or_default();
    let due = opt_text(a, "due_date")?.flatten();
    let (key, content) = (id.to_owned(), content.to_owned());
    let t = change(move |k, now| k.add_todo(&key, &content, assignees, due, &by, now)).await?;
    Ok(srv::json(StatusCode::CREATED, &Data { data: todo_out(&id_of(id), &t) }))
}

/// `PATCH incidents/{id}/relationships/todos/{todo}`: its content, assignees, due date; done
/// (`completed` a time) or not (`null`).
pub async fn change_todo(id: &str, todo: &str, req: Request) -> Result<Response, Fail> {
    let by = caller(&req);
    let body: Value = read_json(req.into_body(), BODY).await?;
    let a = attributes(&body)?;
    let content = opt_text(a, "content")?.flatten();
    let assignees = texts(a.get("assignees"), "assignees")?;
    let due = opt_text(a, "due_date")?;
    let completed = a.get("completed").map(|v| !v.is_null());
    let (key, todo) = (id.to_owned(), todo.to_owned());
    let t = change(move |k, now| k.change_todo(&key, &todo, content, assignees, due, completed, &by, now)).await?;
    Ok(srv::json(StatusCode::OK, &Data { data: todo_out(&id_of(id), &t) }))
}

pub async fn remove_todo(id: &str, todo: &str, req: &Request) -> Result<Response, Fail> {
    let (id, todo, by) = (id.to_owned(), todo.to_owned(), caller(req));
    change(move |k, now| k.remove_todo(&id, &todo, &by, now)).await?;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// `GET incidents/{id}/attachments`.
pub fn attachments(id: &str) -> Result<Response, Fail> {
    kept()?.read(id, |i| srv::json(StatusCode::OK, &Data { data: i.attachments.iter().map(|a| attachment_out(&i.id, a)).collect::<Vec<_>>() })).ok_or_else(no_incident)
}

/// `POST incidents/{id}/attachments`: one made (a postmortem, one an incident, or a link).
pub async fn attach(id: &str, req: Request) -> Result<Response, Fail> {
    let by = caller(&req);
    let body: Value = read_json(req.into_body(), BODY).await?;
    let a = attributes(&body)?;
    let kind = a.get("attachment_type").and_then(Value::as_str).unwrap_or("link");
    let url = a.get("attachment").and_then(|x| x.get("documentUrl")).and_then(Value::as_str).unwrap_or_default();
    let title = a.get("attachment").and_then(|x| x.get("title")).and_then(Value::as_str).unwrap_or_default();
    let (key, kind, url, title) = (id.to_owned(), kind.to_owned(), url.to_owned(), title.to_owned());
    let made = change(move |k, now| k.attach(&key, &kind, &url, &title, &by, now)).await?;
    Ok(srv::json(StatusCode::CREATED, &Data { data: attachment_out(&id_of(id), &made) }))
}

pub async fn change_attachment(id: &str, attachment: &str, req: Request) -> Result<Response, Fail> {
    let by = caller(&req);
    let body: Value = read_json(req.into_body(), BODY).await?;
    let a = attributes(&body)?;
    let url = a.get("attachment").and_then(|x| x.get("documentUrl")).and_then(Value::as_str).map(str::to_owned);
    let title = a.get("attachment").and_then(|x| x.get("title")).and_then(Value::as_str).map(str::to_owned);
    let (key, attachment) = (id.to_owned(), attachment.to_owned());
    let changed = change(move |k, now| k.change_attachment(&key, &attachment, url, title, &by, now)).await?;
    Ok(srv::json(StatusCode::OK, &Data { data: attachment_out(&id_of(id), &changed) }))
}

pub async fn remove_attachment(id: &str, attachment: &str, req: &Request) -> Result<Response, Fail> {
    let (id, attachment, by) = (id.to_owned(), attachment.to_owned(), caller(req));
    change(move |k, now| k.remove_attachment(&id, &attachment, &by, now)).await?;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// `GET incidents/{id}/timeline` (kiln's): its cells, the oldest first.
pub fn timeline(id: &str) -> Result<Response, Fail> {
    kept()?.read(id, |i| srv::json(StatusCode::OK, &Data { data: i.timeline.iter().map(cell_out).collect::<Vec<_>>() })).ok_or_else(no_incident)
}

/// `POST incidents/{id}/timeline` (kiln's): a note, `{"content": …, "important": …}`.
pub async fn note(id: &str, req: Request) -> Result<Response, Fail> {
    let by = caller(&req);
    let body: Value = read_json(req.into_body(), BODY).await?;
    let content = body.get("content").and_then(Value::as_str).unwrap_or_default();
    let important = body.get("important").and_then(Value::as_bool).unwrap_or(false);
    let (key, content) = (id.to_owned(), content.to_owned());
    let c = change(move |k, now| k.note(&key, &content, important, &by, now)).await?;
    Ok(srv::json(StatusCode::CREATED, &Data { data: cell_out(&c) }))
}

/// Datadog's incident API at its paths: a writer's token reads and changes no incident.
pub fn datadog() -> Response {
    srv::json(StatusCode::FORBIDDEN, &json!({ "errors": ["incidents are read and changed through kiln's API (/api/observe/v1/incidents), with a session"] }))
}

/// What a told change says, for the incident's handles.
pub fn told_text(i: &Incident, what: &str) -> String {
    let head = match what {
        "declared" => "Declared",
        "state" => match i.state.as_str() {
            "resolved" => "Resolved",
            "stable" => "Stable",
            _ => "Active again",
        },
        _ => "Severity changed",
    };
    let last_note = i.timeline.iter().rev().find_map(|c| match &c.cell {
        Cell::Markdown { content, .. } => Some(content.as_str()),
        _ => None,
    });
    format!("{head}: {} · {}{}", i.severity, i.state, last_note.map(|n| format!("\n{n}")).unwrap_or_default())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// Datadog's body read as a change: its attributes, its fields (severity and state among them),
    /// its handles, its first notes, its commander; a time RFC 3339; what is malformed refused.
    #[test]
    fn datadog_s_body_reads_as_a_change() {
        let body = json!({ "data": { "type": "incidents", "attributes": {
            "title": "Checkout latency spike", "customer_impacted": true, "customer_impact_scope": "Slow checkouts",
            "customer_impact_start": "2026-10-22T12:00:00Z", "fields": { "severity": { "type": "dropdown", "value": "SEV-3" }, "services": { "type": "multiselect", "value": ["checkout"] } },
            "initial_cells": [{ "cell_type": "markdown", "content": { "content": "Investigation started." }, "important": true }],
            "notification_handles": [{ "display_name": "Ops", "handle": "@slack-ops" }], "is_test": true
        }, "relationships": { "commander_user": { "data": { "type": "users", "id": "user:ada" } } } } });
        let c = change_of(&body).unwrap();
        assert_eq!((c.title.as_deref(), c.customer_impacted, c.is_test), (Some("Checkout latency spike"), Some(true), Some(true)));
        assert_eq!(c.customer_impact_start, Some(Some(1_792_670_400_000)));
        assert_eq!(c.fields.get("severity").map(|f| f.values()), Some(vec!["SEV-3"]));
        assert_eq!(c.initial_cells, vec![("Investigation started.".to_owned(), true)]);
        assert_eq!(c.commander, Some(Some("user:ada".to_owned())));
        assert_eq!(super::super::mentions(&c.notification_handles.unwrap()), vec!["slack-ops"]);
        // Null clears: the commander, the scope.
        let c = change_of(&json!({ "data": { "attributes": { "customer_impact_scope": null }, "relationships": { "commander_user": { "data": null } } } })).unwrap();
        assert_eq!((c.customer_impact_scope, c.commander), (Some(None), Some(None)));
        for bad in [json!({}), json!({ "data": { "type": "users" } }), json!({ "data": { "attributes": { "detected": "yesterday" } } }), json!({ "data": { "attributes": { "fields": { "x": 3 } } } })]
        {
            assert!(change_of(&bad).is_err(), "{bad}");
        }
    }

    /// A time written as `time` formats it, a fraction of a second of any length among them.
    #[test]
    fn times_are_written_as_rfc_3339() {
        for ms in [1_792_670_520_000i64, 1_792_670_520_123, 1_792_670_520_100, 1_792_670_520_120, 1_792_670_520_001, 0, -1, 253_402_300_799_999] {
            let want = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).unwrap().format(&time::format_description::well_known::Rfc3339).unwrap();
            assert_eq!(serde_json::to_value(Time(Some(ms))).unwrap(), Value::String(want), "{ms}");
        }
        assert_eq!(serde_json::to_value(Time(None)).unwrap(), Value::Null);
    }

    /// An incident as Datadog's API gives it: times RFC 3339, its severity and state its fields
    /// too, its commander a user, its figures seconds.
    #[test]
    fn an_incident_reads_as_datadog_s() {
        let dir = tempfile::tempdir().unwrap();
        let kept = Incidents::open(dir.path());
        let c = Change {
            title: Some("t".into()),
            severity: Some("SEV-1".into()),
            commander: Some(Some("user:ada".into())),
            customer_impact_start: Some(Some(1_792_670_400_000)),
            detected: Some(1_792_670_460_000),
            ..Change::default()
        };
        let i = kept.declare(c, "user:bob", 1_792_670_520_000).unwrap();
        let j = incident_json(&i);
        assert_eq!(j["attributes"]["fields"]["severity"], json!({ "type": "dropdown", "value": "SEV-1" }));
        assert_eq!(j["attributes"]["created"], "2026-10-22T12:02:00Z");
        assert_eq!((j["attributes"]["time_to_detect"].as_i64(), j["attributes"]["time_to_internal_response"].as_i64()), (Some(60), Some(60)));
        assert_eq!(j["relationships"]["commander_user"]["data"], json!({ "type": "users", "id": "user:ada" }));
        let inc = included(&[&i], Some("users"));
        assert_eq!(inc.len(), 2, "the commander and who declared it");
    }
}
