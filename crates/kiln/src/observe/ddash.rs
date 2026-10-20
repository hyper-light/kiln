//! Datadog's dashboard JSON (`docs/observe.md` §11): a dashboard as Datadog's API gives one
//! (`GET /api/v1/dashboard/{id}`; its integrations' own are the same) taken as kiln's, and kiln's
//! given so.
//!
//! A widget's requests become kiln's queries: a metric query as written (kiln's language is
//! Datadog's); a log, span or event query (`data_source` `logs`, `spans`, `events`) as kiln's
//! records operand, `logs("…").rollup("count").by("service")`, its groups' limit a `top()`; each
//! formula with its queries in the places of their names. A widget kiln draws otherwise is drawn so
//! and said (a change as a top list of the changes; a bar chart, a geomap and a host map as top
//! lists; a treemap as a pie); one kiln cannot draw is kept as a note that names it, in its place,
//! so nothing goes unsaid; Datadog's own images (its logos) are left out.
//!
//! The layout: an ordered dashboard's 12 columns are kiln's, its rows 4/3 of kiln's (the median of
//! Datadog's timeseries in its 678 integration dashboards, 3 rows, is kiln's default of 4); a free
//! one's columns scaled to 12 by its own width, a unit of it as tall as wide on a dashboard 1,440 px
//! across.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::{Value, json};

use super::dashboards::{Dashboard, Layout, TemplateVariable, Widget};

/// kiln's rows a row of an ordered Datadog dashboard: 4/3 (see the module's notes).
const ROWS: f64 = 4.0 / 3.0;
/// A free dashboard drawn 1,440 px across: kiln's 12 columns are 120 px, its rows 76 px.
const COLUMN_PX: f64 = 120.0;
const ROW_PX: f64 = 76.0;
/// A group limit this high (kiln's own export's) is none: kiln's groups' bound is far above.
const NO_LIMIT: u64 = 1_000;

/// What an import made of a dashboard's widgets: how many Datadog's held (its groups' among them),
/// how many kiln draws as Datadog does, and those it draws otherwise or not at all, each with why.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Report {
    pub widgets: usize,
    pub drawn: usize,
    pub otherwise: Vec<Said>,
    pub not_drawn: Vec<Said>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Said {
    pub title: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub why: String,
}

/// A Datadog dashboard as kiln's, and what became of its widgets. Each query kiln made is checked
/// as kiln reads it: one it cannot read keeps its widget a note that says why.
pub fn import(v: &Value) -> Result<(Dashboard, Report), String> {
    let title = v.get("title").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).ok_or("a Datadog dashboard's title")?;
    let widgets = v.get("widgets").and_then(Value::as_array).ok_or("a Datadog dashboard's widgets")?;
    let free = v.get("layout_type").and_then(Value::as_str) == Some("free");
    let scale = Scale::of(widgets, free);
    let template_variables = vars(v);
    let mut report = Report::default();
    let mut out: Vec<Widget> = widgets.iter().filter_map(|w| widget(w, &scale, false, &template_variables, &mut report)).collect();
    out.sort_by_key(|w| (w.layout.y, w.layout.x));
    let description = v.get("description").and_then(Value::as_str).unwrap_or("").to_owned();
    Ok((Dashboard { id: String::new(), title: title.to_owned(), description, template_variables, widgets: out, builtin: false, created: 0, modified: 0 }, report))
}

/// Where a widget goes on kiln's grid (see the module's notes).
#[derive(Debug, Clone, Copy)]
struct Scale {
    /// A free dashboard's columns a unit of it; none, an ordered one's.
    free: Option<f64>,
}

impl Scale {
    const ORDERED: Self = Self { free: None };

    fn of(widgets: &[Value], free: bool) -> Self {
        if !free {
            return Self::ORDERED;
        }
        let widest = widgets.iter().filter_map(|w| w.get("layout")).map(|l| num(l, "x") + num(l, "width")).fold(0.0, f64::max);
        Self { free: Some(if widest > 0.0 { 12.0 / widest } else { 1.0 }) }
    }

    /// A widget's place: its edges scaled, so neighbours still meet; on the 12 columns, 1×1 at
    /// the least.
    fn place(&self, l: Option<&Value>) -> Layout {
        let (x, y, w, h) = l.map_or((0.0, 0.0, 6.0, 3.0), |l| (num(l, "x"), num(l, "y"), num(l, "width"), num(l, "height")));
        let (col, row) = match self.free {
            None => (1.0, ROWS),
            Some(c) => (c, c * COLUMN_PX / ROW_PX),
        };
        let (left, right) = ((x * col).round().clamp(0.0, 11.0), ((x + w) * col).round().clamp(1.0, 12.0));
        let (top, bottom) = ((y * row).round().max(0.0), ((y + h) * row).round().max(0.0));
        Layout { x: left as u32, y: top as u32, w: (right - left).max(1.0) as u32, h: (bottom - top).max(1.0) as u32 }
    }
}

fn num(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_f64).filter(|x| x.is_finite()).unwrap_or(0.0)
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// Its template variables: each its name, its tag (its `prefix`; its name where it has none) and its
/// default (its first of `defaults`, else `default`, else `*`).
fn vars(v: &Value) -> Vec<TemplateVariable> {
    let mut out: Vec<TemplateVariable> = Vec::new();
    for t in v.get("template_variables").and_then(Value::as_array).into_iter().flatten() {
        let name = text(t, "name").trim();
        if name.is_empty() || out.iter().any(|o| o.name == name) {
            continue;
        }
        let tag = Some(text(t, "prefix").trim()).filter(|p| !p.is_empty()).unwrap_or(name);
        let default = t
            .get("defaults")
            .and_then(Value::as_array)
            .and_then(|d| d.first())
            .and_then(Value::as_str)
            .or_else(|| t.get("default").and_then(Value::as_str))
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .unwrap_or("*");
        out.push(TemplateVariable { name: name.to_owned(), tag: tag.to_owned(), default: default.to_owned() });
    }
    out
}

/// One of Datadog's widgets as kiln's (none: one left out).
fn widget(w: &Value, scale: &Scale, in_group: bool, vars: &[TemplateVariable], report: &mut Report) -> Option<Widget> {
    let def = w.get("definition")?;
    let kind = text(def, "type");
    let title = text(def, "title").trim().to_owned();
    report.widgets += 1;
    let mut k = Widget {
        id: String::new(),
        title: title.clone(),
        kind: String::new(),
        queries: Vec::new(),
        search: String::new(),
        text: String::new(),
        layout: if in_group { Scale::ORDERED.place(w.get("layout")) } else { scale.place(w.get("layout")) },
        options: Value::Null,
        widgets: Vec::new(),
    };
    let said = |why: String| Said { title: title.clone(), kind: kind.to_owned(), why };
    // As kiln draws it: the kind, and how it differs from Datadog's (none: as Datadog draws it).
    let drawn: Result<Option<String>, String> = match kind {
        "group" if !in_group => {
            k.kind = "group".into();
            k.widgets = def.get("widgets").and_then(Value::as_array).into_iter().flatten().filter_map(|c| widget(c, &Scale::ORDERED, true, vars, report)).collect();
            k.widgets.sort_by_key(|c| (c.layout.y, c.layout.x));
            Ok(None)
        }
        "note" | "free_text" => {
            k.kind = "note".into();
            k.text = text(def, if kind == "note" { "content" } else { "text" }).to_owned();
            Ok(None)
        }
        "timeseries" | "query_value" | "toplist" | "query_table" | "heatmap" | "distribution" | "sunburst" | "change" | "bar_chart" | "treemap" | "geomap" | "hostmap" => {
            queried(def, kind, vars, &mut k)
        }
        "log_stream" => {
            k.kind = "logs".into();
            k.search = text(def, "query").to_owned();
            Ok(None)
        }
        "list_stream" => {
            let q = def.get("requests").and_then(Value::as_array).and_then(|r| r.first()).and_then(|r| r.get("query"));
            match q.map_or("", |q| text(q, "data_source")) {
                "logs_stream" | "logs_issue_stream" => {
                    k.kind = "logs".into();
                    k.search = q.map_or("", |q| text(q, "query_string")).to_owned();
                    Ok(None)
                }
                "event_stream" => {
                    k.kind = "events".into();
                    k.search = q.map_or("", |q| text(q, "query_string")).to_owned();
                    Ok(None)
                }
                other => Err(format!("a list of {other}: kiln lists logs and events")),
            }
        }
        "event_stream" | "event_timeline" => {
            k.kind = "events".into();
            k.search = text(def, "query").to_owned();
            Ok(None)
        }
        "check_status" => {
            // A service check's status (`super::api`'s `checks/status`): every group's (Datadog's
            // cluster), or one group's.
            let tags = |key: &str| def.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>()).unwrap_or_default();
            let check = text(def, "check").trim();
            if check.is_empty() {
                Err("a check status without its check".into())
            } else {
                k.kind = "check".into();
                let grouping = if text(def, "grouping") == "check" { "check" } else { "cluster" };
                k.options = json!({ "check": check, "grouping": grouping, "group": text(def, "group"), "group_by": tags("group_by"), "tags": tags("tags") });
                Ok(None)
            }
        }
        "manage_status" => {
            k.kind = "monitors".into();
            k.search = monitor_filter(text(def, "query"));
            Ok(None)
        }
        "image" => {
            let url = text(def, "url");
            if !url.starts_with("https://") {
                report.not_drawn.push(said("Datadog's own image (a logo): left out".into()));
                return None;
            }
            k.kind = "note".into();
            k.text = format!("[{}]({url})", if title.is_empty() { "Image" } else { title.as_str() });
            Ok(Some("an image, as a link to it".into()))
        }
        "group" => Err("a group in a group".into()),
        other => Err(format!("Datadog's {other} widget: kiln does not draw it")),
    };
    match drawn {
        Ok(None) => report.drawn += 1,
        Ok(Some(why)) => report.otherwise.push(said(why)),
        Err(why) => {
            // Kept in its place, as a note that says what it was.
            k.kind = "note".into();
            k.queries.clear();
            k.search.clear();
            k.options = Value::Null;
            k.widgets.clear();
            k.text = format!("{}*Datadog's `{kind}` widget, not drawn: {why}.*", if title.is_empty() { String::new() } else { format!("**{title}**\n\n") });
            report.not_drawn.push(said(why));
        }
    }
    Some(k)
}

/// A monitor summary's search as kiln's monitors widget takes it (a name's or a tag's words):
/// Datadog's `tag:(env:prod)` its `env:prod`, its quotes left off; `status:` and `type:` dropped.
fn monitor_filter(q: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for w in q.split_whitespace() {
        let w = w.strip_prefix("tag:").unwrap_or(w).trim_matches(['(', ')', '"', '\'']);
        if !w.is_empty() && !w.starts_with("status:") && !w.starts_with("type:") && !matches!(w, "AND" | "OR" | "NOT") {
            words.push(w.to_owned());
        }
    }
    words.into_iter().next().unwrap_or_default()
}

/// A widget of queries: its kind, its queries made kiln's, how it is drawn.
fn queried(def: &Value, kind: &str, vars: &[TemplateVariable], k: &mut Widget) -> Result<Option<String>, String> {
    let (kiln_kind, otherwise) = match kind {
        "timeseries" => ("timeseries", None),
        "query_value" => ("query_value", None),
        "toplist" => ("toplist", None),
        "query_table" => ("table", None),
        "heatmap" => ("heatmap", None),
        "distribution" => ("distribution", None),
        "sunburst" => ("pie", None),
        "change" => ("toplist", Some("a change, as a top list of the changes")),
        "bar_chart" => ("toplist", Some("a bar chart, as a top list")),
        "geomap" => ("toplist", Some("a geomap, as a top list of its places")),
        "hostmap" => ("toplist", Some("a host map, as a top list of its hosts")),
        _ => ("pie", Some("a treemap, as a pie")),
    };
    k.kind = kiln_kind.into();
    let requests: Vec<&Value> = match def.get("requests") {
        Some(Value::Array(a)) => a.iter().collect(),
        // A host map's: what fills each host.
        Some(Value::Object(o)) => o.get("fill").into_iter().collect(),
        _ => Vec::new(),
    };
    let mut options = serde_json::Map::new();
    for r in &requests {
        let mut made = request(r)?;
        if kind == "change" {
            made = made.iter().map(|q| change(q, text(r, "compare_to"), text(r, "change_type"))).collect();
        }
        k.queries.extend(made);
        if !options.contains_key("display")
            && kind == "timeseries"
            && let Some(d) = r.get("display_type").and_then(Value::as_str).filter(|d| matches!(*d, "line" | "bars" | "area"))
        {
            options.insert("display".into(), json!(d));
        }
        if !options.contains_key("aggregator")
            && let Some(a) = aggregator_of(r)
        {
            options.insert("aggregator".into(), json!(a));
        }
        if !options.contains_key("limit")
            && let Some(l) = r.get("formulas").and_then(Value::as_array).and_then(|f| f.first()).and_then(|f| f.get("limit"))
        {
            if let Some(n) = l.get("count").and_then(Value::as_u64) {
                options.insert("limit".into(), json!(n));
            }
            if let Some(o) = l.get("order").and_then(Value::as_str).filter(|o| matches!(*o, "asc" | "desc")) {
                options.insert("order".into(), json!(o));
            }
        }
    }
    if k.queries.is_empty() {
        return Err("no query kiln reads".into());
    }
    if let Some(u) = def.get("custom_unit").and_then(Value::as_str).filter(|u| !u.trim().is_empty()) {
        options.insert("unit".into(), json!(u.trim()));
    }
    k.options = if options.is_empty() { Value::Null } else { Value::Object(options) };
    // Each as kiln reads it, its template variables filled.
    for q in &k.queries {
        kiln_obs::query::parse::parse(&super::dashboards::filled_with_any(q, vars)).map_err(|e| format!("its query {q:?}: {e}"))?;
    }
    Ok(otherwise.map(str::to_owned))
}

/// How a request's values are summed over the range: its `aggregator`, or its first query's, as
/// kiln's (Datadog's `area` a sum, `l2norm` and `percentile` a mean).
fn aggregator_of(r: &Value) -> Option<&'static str> {
    let a = r.get("aggregator").and_then(Value::as_str).or_else(|| r.get("queries").and_then(Value::as_array).and_then(|q| q.first()).and_then(|q| q.get("aggregator")).and_then(Value::as_str))?;
    Some(match a {
        "sum" | "area" => "sum",
        "min" => "min",
        "max" => "max",
        "last" => "last",
        _ => "avg",
    })
}

/// A request's queries as kiln's: its `q` (each of a list), its formulas of its named queries, or
/// (each its own) its queries where it has no formula; Datadog's older log, span and event
/// queries (`log_query`, `apm_query`, `event_query`) too.
fn request(r: &Value) -> Result<Vec<String>, String> {
    if let Some(q) = r.get("q").and_then(Value::as_str) {
        return Ok(split_top(q).into_iter().map(str::to_owned).filter(|q| !q.is_empty()).collect());
    }
    for (key, src) in [("log_query", "logs"), ("apm_query", "spans"), ("event_query", "events")] {
        if let Some(lq) = r.get(key) {
            let search = lq.get("search").map_or("", |s| text(s, "query"));
            let compute = lq.get("compute").map(|c| {
                // The older form says the measure's facet as `facet`.
                let mut c = c.clone();
                if let (Some(f), Some(o)) = (c.get("facet").cloned(), c.as_object_mut()) {
                    o.entry("metric").or_insert(f);
                }
                c
            });
            return Ok(vec![records(src, search, compute.as_ref(), lq.get("group_by"))?]);
        }
    }
    // A distribution's newer request names one query, not a list (`request_type: histogram`).
    let queries: &[Value] = match (r.get("queries").and_then(Value::as_array), r.get("query")) {
        (Some(q), _) => q,
        (None, Some(q)) if q.is_object() => std::slice::from_ref(q),
        _ => return Err("a request kiln does not read".into()),
    };
    let mut named: HashMap<&str, String> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for q in queries {
        let name = text(q, "name");
        named.insert(name, operand(q)?);
        order.push(name);
    }
    let formulas: Vec<&str> = r.get("formulas").and_then(Value::as_array).into_iter().flatten().map(|f| text(f, "formula")).filter(|f| !f.trim().is_empty()).collect();
    if formulas.is_empty() {
        return Ok(order.iter().filter_map(|n| named.get(n).cloned()).collect());
    }
    Ok(formulas.iter().map(|f| formula(f, &named)).collect())
}

/// A named query's operand: a metric query as written; a log, span or event query as kiln's.
fn operand(q: &Value) -> Result<String, String> {
    match q.get("data_source").and_then(Value::as_str).unwrap_or("metrics") {
        "metrics" => q.get("query").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| "a metrics query without its query".into()),
        src @ ("logs" | "spans" | "events") => records(src, q.get("search").map_or("", |s| text(s, "query")), q.get("compute"), q.get("group_by")),
        other => Err(format!("{other} data: kiln has metrics, logs, spans and events")),
    }
}

/// `src("search").rollup(…).by(…)`, kiln's records operand of Datadog's log, span or event query:
/// its measure (`compute`), its groups (`group_by`), the most of them it keeps a `top()` by the
/// measure its groups are sorted by (one facet's; several facets' limits multiplied).
fn records(src: &str, search: &str, compute: Option<&Value>, group_by: Option<&Value>) -> Result<String, String> {
    let agg = compute.map_or("count", |c| text(c, "aggregation"));
    let metric = compute.map(|c| text(c, "metric")).filter(|m| !m.is_empty());
    let rollup = match (if agg.is_empty() { "count" } else { agg }, metric) {
        ("count", _) => String::new(),
        (a @ ("cardinality" | "sum" | "avg" | "min" | "max"), Some(m)) => format!(".rollup(\"{a}\", \"{}\")", quoted(m)),
        (a, _) => return Err(format!("a {a} of {src}: kiln's are a count, and a cardinality, sum, avg, min and max of a facet")),
    };
    let groups: Vec<&Value> = group_by.and_then(Value::as_array).into_iter().flatten().collect();
    let facets: Vec<&str> = groups.iter().map(|g| text(g, "facet")).filter(|f| !f.is_empty()).collect();
    let by = if facets.is_empty() { String::new() } else { format!(".by(\"{}\")", quoted(&facets.join(","))) };
    let op = format!("{src}(\"{}\"){rollup}{by}", quoted(search));
    let limit = groups.iter().filter_map(|g| g.get("limit").and_then(Value::as_u64)).fold(None, |a: Option<u64>, l| Some(a.unwrap_or(1).saturating_mul(l.max(1))));
    match limit {
        Some(n) if n < NO_LIMIT && !facets.is_empty() => {
            let sort = groups.first().and_then(|g| g.get("sort"));
            let rank = match sort.map_or("count", |s| text(s, "aggregation")) {
                "avg" => "mean",
                "min" => "min",
                "max" => "max",
                "last" => "last",
                _ => "sum",
            };
            let dir = if sort.map(|s| text(s, "order")) == Some("asc") { "asc" } else { "desc" };
            Ok(format!("top({op}, {n}, '{rank}', '{dir}')"))
        }
        _ => Ok(op),
    }
}

/// A string inside kiln's double quotes: its `\` and `"` escaped.
fn quoted(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// A formula with each query in the place of its name (bracketed), its strings left as they are: a
/// name is a word that is a query's and not a function's (no `(` after it).
fn formula(f: &str, named: &HashMap<&str, String>) -> String {
    let mut out = String::with_capacity(f.len() + named.values().map(String::len).sum::<usize>());
    let mut rest = f;
    while let Some(c) = rest.chars().next() {
        if c == '\'' || c == '"' {
            // A string, whole.
            let end = rest.get(1..).and_then(|r| r.find(c)).map_or(rest.len(), |e| e + 2);
            out.push_str(rest.get(..end).unwrap_or(rest));
            rest = rest.get(end..).unwrap_or("");
        } else if c.is_ascii_alphabetic() || c == '_' {
            let end = rest.find(|x: char| !(x.is_ascii_alphanumeric() || x == '_')).unwrap_or(rest.len());
            let word = rest.get(..end).unwrap_or("");
            rest = rest.get(end..).unwrap_or("");
            match named.get(word) {
                Some(q) if !rest.trim_start().starts_with('(') => {
                    out.push('(');
                    out.push_str(q);
                    out.push(')');
                }
                _ => out.push_str(word),
            }
        } else {
            out.push(c);
            rest = rest.get(c.len_utf8()..).unwrap_or("");
        }
    }
    out
}

/// A change widget's request's query as the change it shows: its value less its value the period
/// before (`compare_to`), or (`relative`) that as a percent of the value before.
fn change(q: &str, compare_to: &str, change_type: &str) -> String {
    let before = match compare_to {
        "hour_before" => "hour_before",
        "week_before" => "week_before",
        "month_before" => "month_before",
        _ => "day_before",
    };
    if change_type == "relative" { format!("100 * (({q}) - {before}({q})) / {before}({q})") } else { format!("({q}) - {before}({q})") }
}

/// A list of queries split at its commas outside brackets and strings (Datadog's `q` may hold
/// several).
fn split_top(q: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut quote, mut start) = (0i32, None::<char>, 0usize);
    for (i, c) in q.char_indices() {
        match (quote, c) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '(' | '{' | '[') => depth += 1,
            (None, ')' | '}' | ']') => depth -= 1,
            (None, ',') if depth == 0 => {
                out.push(q.get(start..i).unwrap_or("").trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(q.get(start..).unwrap_or("").trim());
    out
}

// ---------------------------------------------------------------------------------- export

/// A kiln dashboard as Datadog's JSON: an ordered dashboard, its widgets' layout in Datadog's rows
/// (3/4 of kiln's), each query a request: one of metrics alone its `q`, as written; one with
/// records operands its named queries and a formula of them.
pub fn export(d: &Dashboard) -> Value {
    json!({
        "title": d.title,
        "description": d.description,
        "layout_type": "ordered",
        "template_variables": d.template_variables.iter().map(|v| json!({ "name": v.name, "prefix": v.tag, "default": v.default, "available_values": [] })).collect::<Vec<_>>(),
        "widgets": d.widgets.iter().map(export_widget).collect::<Vec<_>>(),
    })
}

fn export_widget(w: &Widget) -> Value {
    let rows = |at: u32| (f64::from(at) / ROWS).round() as u64;
    let (top, bottom) = (rows(w.layout.y), rows(w.layout.y + w.layout.h));
    let layout = json!({ "x": w.layout.x, "y": top, "width": w.layout.w, "height": bottom.saturating_sub(top).max(1) });
    let opt = |k: &str| w.options.get(k).and_then(Value::as_str);
    let aggregator = opt("aggregator").unwrap_or("avg");
    let scalar = |queries: &[String]| queries.iter().map(|q| export_request(q, "scalar", None, Some(aggregator))).collect::<Vec<_>>();
    let mut def = match w.kind.as_str() {
        "timeseries" => json!({ "type": "timeseries", "requests": w.queries.iter().map(|q| export_request(q, "timeseries", Some(opt("display").unwrap_or("line")), None)).collect::<Vec<_>>() }),
        "query_value" => json!({ "type": "query_value", "requests": scalar(&w.queries), "precision": 2 }),
        "toplist" => json!({ "type": "toplist", "requests": scalar(&w.queries) }),
        "table" => json!({ "type": "query_table", "requests": scalar(&w.queries) }),
        "pie" => json!({ "type": "sunburst", "requests": scalar(&w.queries) }),
        "heatmap" | "distribution" => json!({ "type": w.kind, "requests": w.queries.iter().map(|q| export_request(q, "timeseries", None, None)).collect::<Vec<_>>() }),
        "logs" => json!({
            "type": "list_stream",
            "requests": [{ "query": { "data_source": "logs_stream", "query_string": w.search }, "columns": [{ "field": "status_line", "width": "auto" }, { "field": "timestamp", "width": "auto" }, { "field": "service", "width": "auto" }, { "field": "content", "width": "compact" }], "response_format": "event_list" }],
        }),
        "events" => json!({ "type": "event_stream", "query": w.search, "event_size": "s" }),
        "monitors" => json!({ "type": "manage_status", "query": w.search, "summary_type": "monitors" }),
        "check" => {
            let list = |key: &str| w.options.get(key).cloned().filter(Value::is_array).unwrap_or_else(|| json!([]));
            let mut d = json!({ "type": "check_status", "check": opt("check").unwrap_or(""), "grouping": opt("grouping").unwrap_or("cluster"), "group_by": list("group_by"), "tags": list("tags") });
            if let (Some(g), Some(o)) = (opt("group").filter(|g| !g.is_empty()), d.as_object_mut()) {
                o.insert("group".into(), json!(g));
            }
            d
        }
        "group" => json!({ "type": "group", "layout_type": "ordered", "widgets": w.widgets.iter().map(export_widget).collect::<Vec<_>>() }),
        _ => json!({ "type": "note", "content": w.text }),
    };
    if let Some(o) = def.as_object_mut() {
        o.insert("title".into(), json!(w.title));
        if let Some(u) = opt("unit").filter(|_| w.kind == "query_value") {
            o.insert("custom_unit".into(), json!(u));
        }
    }
    json!({ "definition": def, "layout": layout })
}

/// One of a widget's queries as a Datadog request (`response_format` `timeseries` or `scalar`).
fn export_request(q: &str, response_format: &str, display: Option<&str>, aggregator: Option<&str>) -> Value {
    let ast = kiln_obs::query::parse::parse(q).ok().filter(|a| a.nodes.iter().any(|n| matches!(n, kiln_obs::query::parse::Node::Records(_))));
    let mut r = match ast {
        None => json!({ "q": q }),
        Some(ast) => {
            // Each leaf named in the order it is written, the formula the query with each leaf's
            // text its name.
            let mut leaves = ast.leaves.clone();
            leaves.sort_by_key(|(_, at)| at.start);
            let mut queries = Vec::with_capacity(leaves.len());
            let mut formula = String::with_capacity(q.len());
            let mut from = 0usize;
            for (n, (id, at)) in leaves.iter().enumerate() {
                let name = format!("query{}", n + 1);
                formula.push_str(q.get(from..at.start).unwrap_or(""));
                formula.push_str(&name);
                from = at.end;
                let mut dq = match ast.nodes.get(*id) {
                    Some(kiln_obs::query::parse::Node::Records(r)) => export_records(r, &name),
                    _ => json!({ "data_source": "metrics", "name": name, "query": q.get(at.clone()).unwrap_or("") }),
                };
                if let (Some(a), Some(o)) = (aggregator, dq.as_object_mut())
                    && o.get("data_source").and_then(Value::as_str) == Some("metrics")
                {
                    o.insert("aggregator".into(), json!(a));
                }
                queries.push(dq);
            }
            formula.push_str(q.get(from..).unwrap_or(""));
            json!({ "queries": queries, "formulas": [{ "formula": formula.trim() }], "response_format": response_format })
        }
    };
    if let Some(o) = r.as_object_mut() {
        if let Some(d) = display {
            o.insert("display_type".into(), json!(d));
        }
        if let (Some(a), true) = (aggregator, o.contains_key("q")) {
            o.insert("aggregator".into(), json!(a));
        }
    }
    r
}

/// A records operand as Datadog's log, span or event query: its search as written, its measure,
/// its facets (each to [`NO_LIMIT`]: kiln keeps them all).
fn export_records(r: &kiln_obs::query::parse::RecordQuery, name: &str) -> Value {
    use kiln_obs::logs::store::Measure;
    let src = match r.of {
        kiln_obs::query::parse::RecordSource::Logs => "logs",
        kiln_obs::query::parse::RecordSource::Spans => "spans",
        kiln_obs::query::parse::RecordSource::Events => "events",
    };
    let (aggregation, metric) = match &r.measure {
        Measure::Count => ("count", None),
        Measure::Cardinality(f) => ("cardinality", Some(f.name())),
        Measure::Sum(m) => ("sum", Some(format!("@{m}"))),
        Measure::Avg(m) => ("avg", Some(format!("@{m}"))),
        Measure::Min(m) => ("min", Some(format!("@{m}"))),
        Measure::Max(m) => ("max", Some(format!("@{m}"))),
    };
    let mut compute = json!({ "aggregation": aggregation });
    if let (Some(m), Some(o)) = (metric, compute.as_object_mut()) {
        o.insert("metric".into(), json!(m));
    }
    let group_by: Vec<Value> = r.by.iter().map(|f| json!({ "facet": f.name(), "limit": NO_LIMIT, "sort": { "aggregation": aggregation, "order": "desc" } })).collect();
    json!({ "data_source": src, "name": name, "search": { "query": r.text }, "indexes": ["*"], "compute": compute, "group_by": group_by })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imp(v: &Value) -> anyhow::Result<(Dashboard, Report)> {
        import(v).map_err(|e| anyhow::anyhow!(e))
    }

    fn widgets(d: &Dashboard) -> Vec<&Widget> {
        d.widgets.iter().flat_map(|w| std::iter::once(w).chain(&w.widgets)).collect()
    }

    /// Datadog's widgets of each kind kiln draws, in its newer requests (formulas of named
    /// queries, of metrics and of logs) and its older (`q`, `log_query`), taken as kiln's.
    #[test]
    fn a_datadog_dashboard_is_taken_as_kiln_s() -> anyhow::Result<()> {
        let v = json!({
            "title": "Shop",
            "description": "Its traffic",
            "layout_type": "ordered",
            "template_variables": [{ "name": "env", "prefix": "env", "available_values": [], "default": "prod" }, { "name": "svc", "prefix": null, "defaults": ["*"] }],
            "widgets": [
                { "definition": { "type": "note", "content": "**Read me**" }, "layout": { "x": 0, "y": 0, "width": 12, "height": 1 } },
                { "definition": { "type": "timeseries", "title": "Errors a request", "requests": [{
                    "queries": [
                        { "data_source": "logs", "name": "query1", "search": { "query": "service:shop status:error" }, "indexes": ["*"], "compute": { "aggregation": "count" }, "group_by": [{ "facet": "@http.route", "limit": 10, "sort": { "aggregation": "count", "order": "desc" } }] },
                        { "data_source": "metrics", "name": "query2", "query": "sum:shop.requests{$env} by {http.route}.as_count()" }
                    ],
                    "formulas": [{ "formula": "default_zero(query1) / query2 * 100", "alias": "errors %" }],
                    "response_format": "timeseries", "display_type": "bars" }] }, "layout": { "x": 0, "y": 1, "width": 6, "height": 3 } },
                { "definition": { "type": "query_value", "title": "p95", "requests": [{ "queries": [{ "data_source": "metrics", "name": "a", "query": "p95:shop.latency{$env}", "aggregator": "max" }], "formulas": [{ "formula": "a" }], "response_format": "scalar" }], "custom_unit": "ms" }, "layout": { "x": 6, "y": 1, "width": 3, "height": 2 } },
                { "definition": { "type": "toplist", "title": "Slowest", "requests": [{ "q": "top(avg:shop.latency{*} by {route}, 10, 'mean', 'desc'), avg:shop.cpu{*}" }] }, "layout": { "x": 9, "y": 1, "width": 3, "height": 4 } },
                { "definition": { "type": "group", "title": "Logs", "layout_type": "ordered", "widgets": [
                    { "definition": { "type": "sunburst", "title": "By status", "requests": [{ "queries": [{ "data_source": "logs", "name": "q", "search": { "query": "service:shop" }, "compute": { "aggregation": "count" }, "group_by": [{ "facet": "status", "limit": 5 }] }], "formulas": [{ "formula": "q" }], "response_format": "scalar" }] }, "layout": { "x": 0, "y": 0, "width": 4, "height": 4 } },
                    { "definition": { "type": "list_stream", "title": "Errors", "requests": [{ "query": { "data_source": "logs_stream", "query_string": "service:shop status:error" }, "columns": [], "response_format": "event_list" }] }, "layout": { "x": 4, "y": 0, "width": 8, "height": 4 } },
                    { "definition": { "type": "timeseries", "title": "Old", "requests": [{ "log_query": { "index": "*", "search": { "query": "source:nginx" }, "compute": { "aggregation": "avg", "facet": "@duration" }, "group_by": [{ "facet": "host", "limit": 1000 }] } }] }, "layout": { "x": 0, "y": 4, "width": 12, "height": 3 } }
                ] }, "layout": { "x": 0, "y": 5, "width": 12, "height": 9 } },
                { "definition": { "type": "change", "title": "Day over day", "requests": [{ "q": "sum:shop.orders{*} by {region}.as_count()", "compare_to": "day_before", "change_type": "relative" }] }, "layout": { "x": 0, "y": 14, "width": 6, "height": 3 } },
                { "definition": { "type": "slo", "title": "Checkout SLO", "slo_id": "abc", "view_type": "detail" }, "layout": { "x": 6, "y": 14, "width": 6, "height": 3 } },
                { "definition": { "type": "image", "url": "/static/images/logos/shop_large.svg" }, "layout": { "x": 0, "y": 17, "width": 2, "height": 2 } },
                { "definition": { "type": "manage_status", "title": "Monitors", "query": "tag:(service:shop) status:alert", "summary_type": "monitors" }, "layout": { "x": 2, "y": 17, "width": 10, "height": 3 } },
                { "definition": { "type": "check_status", "title": "Agent up", "check": "datadog.agent.up", "grouping": "cluster", "group_by": [], "tags": ["$env"] }, "layout": { "x": 0, "y": 20, "width": 2, "height": 2 } }
            ]
        });
        let (d, report) = imp(&v)?;
        anyhow::ensure!(d.title == "Shop" && d.description == "Its traffic");
        anyhow::ensure!(d.template_variables.len() == 2 && d.template_variables.first().is_some_and(|t| t.tag == "env" && t.default == "prod"));
        anyhow::ensure!(d.template_variables.get(1).is_some_and(|t| t.tag == "svc" && t.default == "*"), "a variable of no prefix: its name its tag");
        let all = widgets(&d);
        let by_title = |t: &str| all.iter().find(|w| w.title == t).copied().ok_or_else(|| anyhow::anyhow!("no widget {t}"));
        let ts = by_title("Errors a request")?;
        anyhow::ensure!(ts.kind == "timeseries" && ts.options.get("display") == Some(&json!("bars")), "{ts:?}");
        anyhow::ensure!(
            ts.queries == [r#"default_zero((top(logs("service:shop status:error").by("@http.route"), 10, 'sum', 'desc'))) / (sum:shop.requests{$env} by {http.route}.as_count()) * 100"#],
            "{:?}",
            ts.queries
        );
        // Its layout: the columns as they are, the rows 4/3 of kiln's, edges meeting.
        anyhow::ensure!(ts.layout == Layout { x: 0, y: 1, w: 6, h: 4 }, "{:?}", ts.layout);
        let qv = by_title("p95")?;
        anyhow::ensure!(qv.queries == ["(p95:shop.latency{$env})"] && qv.options.get("aggregator") == Some(&json!("max")) && qv.options.get("unit") == Some(&json!("ms")), "{qv:?}");
        anyhow::ensure!(by_title("Slowest")?.queries == ["top(avg:shop.latency{*} by {route}, 10, 'mean', 'desc')", "avg:shop.cpu{*}"], "a q of two");
        let group = by_title("Logs")?;
        anyhow::ensure!(group.kind == "group" && group.widgets.len() == 3, "{group:?}");
        anyhow::ensure!(by_title("By status")?.kind == "pie" && by_title("By status")?.queries == [r#"(top(logs("service:shop").by("status"), 5, 'sum', 'desc'))"#]);
        anyhow::ensure!(by_title("Errors")?.kind == "logs" && by_title("Errors")?.search == "service:shop status:error");
        anyhow::ensure!(by_title("Old")?.queries == [r#"logs("source:nginx").rollup("avg", "@duration").by("host")"#], "a limit of 1,000 is none: {:?}", by_title("Old")?.queries);
        let ch = by_title("Day over day")?;
        anyhow::ensure!(
            ch.kind == "toplist"
                && ch.queries
                    == ["100 * ((sum:shop.orders{*} by {region}.as_count()) - day_before(sum:shop.orders{*} by {region}.as_count())) / day_before(sum:shop.orders{*} by {region}.as_count())"]
        );
        let slo = by_title("Checkout SLO")?;
        anyhow::ensure!(slo.kind == "note" && slo.text.contains("Datadog's `slo` widget, not drawn"), "kept, in its place: {slo:?}");
        anyhow::ensure!(by_title("Monitors")?.kind == "monitors" && by_title("Monitors")?.search == "service:shop");
        anyhow::ensure!(!all.iter().any(|w| w.text.contains("logos")), "Datadog's own image left out");
        let up = by_title("Agent up")?;
        anyhow::ensure!(up.kind == "check" && up.options.get("check") == Some(&json!("datadog.agent.up")) && up.options.get("tags") == Some(&json!(["$env"])), "{up:?}");
        anyhow::ensure!(report.widgets == 13 && report.drawn == 10, "{report:?}");
        anyhow::ensure!(report.otherwise.len() == 1 && report.not_drawn.len() == 2, "{report:?}");
        // Kept as kiln keeps dashboards.
        let dir = tempfile::tempdir()?;
        super::super::dashboards::Dashboards::open(dir.path()).put(d).map_err(|e| anyhow::anyhow!(e))?;
        Ok(())
    }

    /// A distribution's newer request, one query rather than a list, is read as one.
    #[test]
    fn a_request_of_one_query_is_read_as_its_query() -> anyhow::Result<()> {
        let v = json!({ "title": "x", "layout_type": "ordered", "widgets": [
            { "definition": { "type": "distribution", "title": "Sizes", "requests": [{ "request_type": "histogram", "query": { "data_source": "metrics", "name": "query1", "aggregator": "avg", "query": "avg:payload.size{$env}" } }] } }
        ] });
        let (d, report) = imp(&v)?;
        anyhow::ensure!(report.drawn == 1 && d.widgets.first().is_some_and(|w| w.kind == "distribution" && w.queries == ["avg:payload.size{$env}"]), "{report:?} {:?}", d.widgets);
        Ok(())
    }

    /// A query kiln cannot read, or data kiln has none of, keeps its widget a note that says so;
    /// the dashboard is taken all the same.
    #[test]
    fn what_kiln_cannot_read_is_said_not_lost() -> anyhow::Result<()> {
        let v = json!({ "title": "x", "layout_type": "ordered", "widgets": [
            { "definition": { "type": "timeseries", "title": "Signals", "requests": [{ "queries": [{ "data_source": "security_signals", "name": "q", "search": { "query": "*" }, "compute": { "aggregation": "count" } }], "formulas": [{ "formula": "q" }] }] } },
            { "definition": { "type": "timeseries", "title": "Percentile", "requests": [{ "queries": [{ "data_source": "logs", "name": "q", "search": { "query": "*" }, "compute": { "aggregation": "pc99", "metric": "@duration" } }], "formulas": [{ "formula": "q" }] }] } },
            { "definition": { "type": "query_value", "title": "Broken", "requests": [{ "q": "avg:x{" }] } }
        ] });
        let (d, report) = imp(&v)?;
        anyhow::ensure!(d.widgets.iter().all(|w| w.kind == "note") && report.not_drawn.len() == 3 && report.drawn == 0, "{report:?}");
        anyhow::ensure!(report.not_drawn.iter().any(|s| s.why.contains("security_signals")) && report.not_drawn.iter().any(|s| s.why.contains("pc99")), "{report:?}");
        anyhow::ensure!(import(&json!({ "widgets": [] })).is_err(), "no title");
        Ok(())
    }

    /// An ordered dashboard's layout comes back from kiln exactly: each widget's top and bottom, of
    /// any of Datadog's whole rows (`round(¾·round(4n/3)) = n`), and its columns.
    #[test]
    fn datadog_s_rows_come_back_from_kiln_exactly() -> anyhow::Result<()> {
        let widgets: Vec<Value> =
            (0..300u64).map(|n| json!({ "definition": { "type": "note", "content": n.to_string() }, "layout": { "x": n % 12, "y": n, "width": 12 - n % 12, "height": 1 + n % 7 } })).collect();
        let (d, _) = imp(&json!({ "title": "rows", "layout_type": "ordered", "widgets": widgets }))?;
        let back = export(&d);
        let laid: Vec<(String, Value)> = back
            .get("widgets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|w| (w.get("definition").map_or("", |d| text(d, "content")).to_owned(), w.get("layout").cloned().unwrap_or(Value::Null)))
            .collect();
        anyhow::ensure!(laid.len() == 300);
        for (content, l) in laid {
            let n: u64 = content.parse()?;
            anyhow::ensure!(l == json!({ "x": n % 12, "y": n, "width": 12 - n % 12, "height": 1 + n % 7 }), "{n}: {l}");
        }
        Ok(())
    }

    /// A free dashboard's columns scaled to 12 by its widest; each widget on the grid.
    #[test]
    fn a_free_layout_is_scaled_to_kiln_s_grid() -> anyhow::Result<()> {
        let v = json!({ "title": "x", "layout_type": "free", "widgets": [
            { "definition": { "type": "note", "content": "a" }, "layout": { "x": 0, "y": 0, "width": 48, "height": 5 } },
            { "definition": { "type": "note", "content": "b" }, "layout": { "x": 48, "y": 0, "width": 48, "height": 15 } }
        ] });
        let (d, _) = imp(&v)?;
        let l: Vec<Layout> = d.widgets.iter().map(|w| w.layout).collect();
        anyhow::ensure!(l.first().is_some_and(|a| a.x == 0 && a.w == 6) && l.get(1).is_some_and(|b| b.x == 6 && b.w == 6 && b.h >= 2), "{l:?}");
        Ok(())
    }

    /// Given as Datadog's and taken back, a dashboard is the same: its kinds, its queries (a
    /// records operand's too), its layout.
    #[test]
    fn a_dashboard_given_as_datadog_s_comes_back_the_same() -> anyhow::Result<()> {
        let w = |title: &str, kind: &str, queries: &[&str], x: u32, y: u32| Widget {
            id: String::new(),
            title: title.into(),
            kind: kind.into(),
            queries: queries.iter().map(|q| (*q).to_owned()).collect(),
            search: String::new(),
            text: String::new(),
            layout: Layout { x, y, w: 6, h: 4 },
            options: Value::Null,
            widgets: Vec::new(),
        };
        let d = Dashboard {
            id: String::new(),
            title: "Round".into(),
            description: String::new(),
            template_variables: vec![TemplateVariable { name: "env".into(), tag: "env".into(), default: "*".into() }],
            widgets: vec![
                w("a", "timeseries", &["avg:cpu{$env} by {host}"], 0, 0),
                w("b", "timeseries", &[r#"100 * logs("status:error").by("service") / logs("*").by("service")"#], 6, 0),
                w("c", "query_value", &[r#"sum:requests{*}.as_count() + spans("service:web").rollup("count")"#], 0, 4),
                w("d", "pie", &[r#"logs("*").rollup("cardinality", "@user").by("service")"#], 6, 4),
                Widget { options: json!({ "check": "db.can_connect", "grouping": "check", "group": "host:db-1", "group_by": ["host"], "tags": ["env:prod"] }), ..w("e", "check", &[], 0, 8) },
            ],
            builtin: false,
            created: 0,
            modified: 0,
        };
        let (back, report) = imp(&export(&d))?;
        anyhow::ensure!(report.drawn == 5 && report.not_drawn.is_empty(), "{report:?}");
        for (a, b) in d.widgets.iter().zip(&back.widgets) {
            anyhow::ensure!(a.kind == b.kind && a.layout == b.layout && (a.kind != "check" || a.options == b.options), "{a:?} {b:?}");
            // The same query, read alike (Datadog's formula brackets each named query, and a count
            // says no rollup).
            for (qa, qb) in a.queries.iter().zip(&b.queries) {
                let read = |q: &str| kiln_obs::query::parse::parse(q).map(|a| (a.nodes, a.root)).map_err(|e| anyhow::anyhow!("{q}: {e}"));
                anyhow::ensure!(read(qa)? == read(qb)?, "{qa} came back {qb}");
            }
        }
        Ok(())
    }

    /// Datadog's own: every dashboard of its integrations (`KILN_DD_DASHBOARDS`, a folder of
    /// `integrations-core/*/assets/dashboards/*.json`, `docs/observe.md` §12.25) taken, kept as
    /// kiln keeps dashboards, given as Datadog's and taken again the same (its widgets' kinds,
    /// queries and layout); what kiln draws of Datadog's widgets, by type, and the time an import
    /// takes, said.
    #[test]
    #[ignore = "needs Datadog's integration dashboards: KILN_DD_DASHBOARDS=<folder>"]
    fn datadog_s_integration_dashboards_are_taken_and_come_back_the_same() -> anyhow::Result<()> {
        let Ok(dir) = std::env::var("KILN_DD_DASHBOARDS") else { return Ok(()) };
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        let mut todo = vec![std::path::PathBuf::from(dir)];
        while let Some(d) = todo.pop() {
            for e in std::fs::read_dir(&d)? {
                let p = e?.path();
                if p.is_dir() {
                    todo.push(p);
                } else if p.extension().is_some_and(|x| x == "json") {
                    files.push(p);
                }
            }
        }
        files.sort();
        let store = tempfile::tempdir()?;
        let kept = super::super::dashboards::Dashboards::open(store.path());
        let (mut total, mut taken, mut unreadable, mut took) = (Report::default(), 0usize, 0usize, std::time::Duration::ZERO);
        let mut by_type: std::collections::BTreeMap<String, (usize, usize, usize)> = std::collections::BTreeMap::new();
        let read = |q: &str, vars: &[TemplateVariable]| kiln_obs::query::parse::parse(&super::super::dashboards::filled_with_any(q, vars)).map(|a| (a.nodes, a.root));
        for f in &files {
            let Ok(v) = serde_json::from_slice::<Value>(&std::fs::read(f)?) else {
                unreadable += 1;
                continue;
            };
            let t = std::time::Instant::now();
            let (d, r) = imp(&v).map_err(|e| anyhow::anyhow!("{}: {e}", f.display()))?;
            took += t.elapsed();
            taken += 1;
            total.widgets += r.widgets;
            total.drawn += r.drawn;
            for (i, list) in [&r.otherwise, &r.not_drawn].into_iter().enumerate() {
                for s in list {
                    let e = by_type.entry(s.kind.clone()).or_default();
                    if i == 0 {
                        e.1 += 1;
                    } else {
                        e.2 += 1;
                    }
                }
            }
            total.otherwise.extend(r.otherwise);
            total.not_drawn.extend(r.not_drawn);
            let d = kept.put(d).map_err(|e| anyhow::anyhow!("{}: kept: {e}", f.display()))?;
            let (again, r2) = imp(&export(&d)).map_err(|e| anyhow::anyhow!("{}: again: {e}", f.display()))?;
            anyhow::ensure!(r2.not_drawn.iter().all(|s| s.kind == "note") || r2.not_drawn.is_empty(), "{}: given back, a widget not drawn: {:?}", f.display(), r2.not_drawn);
            let flat = |d: &Dashboard| d.widgets.iter().flat_map(|w| std::iter::once(w.clone()).chain(w.widgets.clone())).collect::<Vec<_>>();
            let (a, b) = (flat(&d), flat(&again));
            anyhow::ensure!(a.len() == b.len(), "{}: {} widgets came back {}", f.display(), a.len(), b.len());
            // Each widget found again by what it is (a top a row apart can change the order two
            // widgets are read in): its kind, title, search, text and queries, read alike (and a
            // check status's check, groups and tags).
            let same = |x: &Widget, y: &Widget| {
                x.kind == y.kind
                    && x.title == y.title
                    && x.search == y.search
                    && x.text == y.text
                    && (x.kind != "check" || x.options == y.options)
                    && x.queries.len() == y.queries.len()
                    && x.queries.iter().zip(&y.queries).all(|(qa, qb)| read(qa, &d.template_variables).ok() == read(qb, &again.template_variables).ok())
            };
            let mut taken = vec![false; b.len()];
            for x in &a {
                let found = b.iter().enumerate().find(|(i, y)| !taken.get(*i).copied().unwrap_or(true) && same(x, y));
                let Some((i, y)) = found else { anyhow::bail!("{}: {x:?} did not come back", f.display()) };
                if let Some(t) = taken.get_mut(i) {
                    *t = true;
                }
                // Datadog's rows are coarser than kiln's: its top and bottom come back within a row
                // (exactly, where they came of Datadog's whole rows), its columns exactly.
                let near = |p: u32, q: u32| p.abs_diff(q) <= 1;
                let (lx, ly) = (x.layout, y.layout);
                anyhow::ensure!(lx.x == ly.x && lx.w == ly.w && near(lx.y, ly.y) && near(lx.y + lx.h, ly.y + ly.h), "{}: {lx:?} came back {ly:?}", f.display());
            }
            kept.delete(&d.id).map_err(|e| anyhow::anyhow!(e))?;
        }
        println!(
            "{taken} dashboards taken ({unreadable} not JSON), {} widgets: {} drawn as Datadog draws them ({:.1}%), {} otherwise, {} not drawn; {:.0} µs an import",
            total.widgets,
            total.drawn,
            100.0 * total.drawn as f64 / total.widgets.max(1) as f64,
            total.otherwise.len(),
            total.not_drawn.len(),
            took.as_secs_f64() * 1e6 / taken.max(1) as f64
        );
        let mut rows: Vec<(String, (usize, usize, usize))> = by_type.into_iter().collect();
        rows.sort_by_key(|(_, (_, o, n))| std::cmp::Reverse(o + n));
        for (t, (_, o, n)) in rows {
            println!("  {t:<22} {o:>5} otherwise {n:>5} not drawn");
        }
        let mut whys: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for s in &total.not_drawn {
            // The reason, its particulars (a query's text) left off.
            *whys.entry(s.why.split(':').next().unwrap_or("").to_owned()).or_default() += 1;
        }
        let mut whys: Vec<(String, usize)> = whys.into_iter().collect();
        whys.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        for (w, n) in whys.iter().take(25) {
            println!("  {n:>5}  {w}");
        }
        // The queries kiln could not read, each reason once (the first 60).
        let mut unread: Vec<&str> = total.not_drawn.iter().map(|s| s.why.as_str()).filter(|w| w.starts_with("its query")).collect();
        unread.sort_unstable();
        unread.dedup();
        for w in unread.iter().take(60) {
            println!("  unread: {}", w.chars().take(300).collect::<String>());
        }
        anyhow::ensure!(taken > 0, "no dashboard in the folder");
        Ok(())
    }
}
