//! Incidents searched as Datadog's API searches them (`GET /api/v2/incidents/search`): facets
//! joined by `AND` (or a space), a facet's values by `OR` within parentheses
//! (`state:(active OR stable) AND severity:SEV-1`), `*` any part of a value; a word alone, a part
//! of the title. Facets: `state`, `severity`, `commander`, `created_by`, `last_modified_by`,
//! `postmortem` and `impact` (`true`, `false`), `public_id`, `is_test`, and an incident's own fields
//! by name (`services:checkout`, `detection_method:monitor`, or `fields.services:…`). And what
//! matched counted by facet, as Datadog's search answers them.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::Incident;

/// One term: a facet's values (any matches), or a part of the title; each lowercased as it is parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Term {
    Facet { key: String, values: Vec<Want> },
    Text(String),
}

/// A value a facet asks for: its text, lowercased, and whether `*` stands for any part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Want {
    text: String,
    glob: bool,
}

impl Want {
    fn new(v: &str) -> Self {
        let text = v.to_lowercase();
        Self { glob: text.contains('*'), text }
    }

    /// Whether `have` is what it asks, case aside: nothing allocated but for a glob of a value of
    /// capitals or of more than ASCII.
    fn matches(&self, have: &str) -> bool {
        match (self.glob, have.is_ascii()) {
            (false, true) => have.eq_ignore_ascii_case(&self.text),
            (true, true) if !have.bytes().any(|b| b.is_ascii_uppercase()) => kiln_obs::index::glob(&self.text, have),
            (false, false) => have.to_lowercase() == self.text,
            _ => kiln_obs::index::glob(&self.text, &have.to_lowercase()),
        }
    }
}

/// A search, parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    terms: Vec<Term>,
}

/// The words of a query: a parenthesized group or a quoted text one word.
fn words(q: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut depth, mut quoted) = (0usize, false);
    for c in q.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            '(' if !quoted => {
                depth += 1;
                cur.push(c);
            }
            ')' if !quoted => {
                depth = depth.checked_sub(1).ok_or("a `)` that closes nothing")?;
                cur.push(c);
            }
            c if c.is_whitespace() && depth == 0 && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if depth > 0 || quoted {
        return Err("a `(` or a quote that does not close".into());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

fn unquote(s: &str) -> &str {
    s.trim().trim_matches('"')
}

/// A query parsed; an empty one matches every incident.
pub fn parse(q: &str) -> Result<Query, String> {
    if q.len() > 4096 {
        return Err("a query of 4,096 bytes at the most".into());
    }
    let mut terms = Vec::new();
    for w in words(q)? {
        match w.as_str() {
            "AND" => continue,
            "OR" => return Err("facets are joined by AND: OR joins a facet's values, in its parentheses (state:(active OR stable))".into()),
            _ => {}
        }
        match w.split_once(':') {
            Some((k, v)) if !k.is_empty() && !k.starts_with('"') => {
                let v = v.trim();
                let values: Vec<Want> = match v.strip_prefix('(').and_then(|v| v.strip_suffix(')')) {
                    Some(inner) => inner.split(" OR ").map(unquote).filter(|s| !s.is_empty()).map(Want::new).collect(),
                    None => vec![Want::new(unquote(v))],
                };
                if values.is_empty() || values.iter().any(|w| w.text.is_empty()) {
                    return Err(format!("{k}: a value, or values in parentheses"));
                }
                terms.push(Term::Facet { key: k.trim_start_matches("fields.").to_ascii_lowercase(), values });
            }
            _ => terms.push(Term::Text(unquote(&w).to_lowercase())),
        }
    }
    Ok(Query { terms })
}

fn yes_no(b: bool) -> &'static str {
    if b { "true" } else { "false" }
}

/// Whether any of an incident's values of facet `key` holds of `f` (nothing allocated).
fn any_value(i: &Incident, key: &str, f: impl Fn(&str) -> bool) -> bool {
    match key {
        "state" => f(&i.state),
        "severity" => f(&i.severity),
        "commander" => i.commander.as_deref().is_some_and(&f),
        "created_by" => f(&i.created_by),
        "last_modified_by" => f(&i.last_modified_by),
        "postmortem" => f(yes_no(i.has_postmortem())),
        "impact" | "customer_impacted" => f(yes_no(i.customer_impacted)),
        "is_test" => f(yes_no(i.is_test)),
        "title" => f(&i.title),
        "public_id" => {
            let mut buf = [0u8; 20];
            let mut w = buf.as_mut_slice();
            let n = if std::io::Write::write_fmt(&mut w, format_args!("{}", i.public_id)).is_ok() { 20 - w.len() } else { 0 };
            f(buf.get(..n).and_then(|b| std::str::from_utf8(b).ok()).unwrap_or(""))
        }
        field => match i.fields.get(field).map(|x| &x.value) {
            Some(Value::String(s)) => f(s),
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).any(f),
            _ => false,
        },
    }
}

/// Whether `hay` holds `needle` (lowercased), case aside: nothing allocated for ASCII.
pub(crate) fn contains_folded(hay: &str, needle: &str) -> bool {
    if hay.is_ascii() && needle.is_ascii() {
        let (h, n) = (hay.as_bytes(), needle.as_bytes());
        n.is_empty() || h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
    } else {
        hay.to_lowercase().contains(needle)
    }
}

impl Query {
    pub fn matches(&self, i: &Incident) -> bool {
        self.terms.iter().all(|t| match t {
            Term::Facet { key, values } => any_value(i, key, |have| values.iter().any(|w| w.matches(have))),
            Term::Text(t) => contains_folded(&i.title, t),
        })
    }
}

/// A facet's values at the most, the most common first (Datadog's facets list what most have).
pub const FACET_VALUES: usize = 100;

/// The field types a facet counts: those of values from a list (a text of one's own, a summary or
/// a root cause, is never the same twice).
const COUNTED: [&str; 4] = ["dropdown", "multiselect", "metrictag", "autocomplete"];

/// A facet's value and how many of what matched have it.
#[derive(Debug, Serialize)]
pub struct Count<'a> {
    name: &'a str,
    count: u64,
}

/// A user's facet: Datadog's shape (`handle`, `name`, …), of what kiln knows of whom (its name).
#[derive(Debug, Serialize)]
pub struct UserCount<'a> {
    name: &'a str,
    handle: &'a str,
    email: Option<()>,
    uuid: &'a str,
    count: u64,
}

#[derive(Debug, Serialize)]
pub struct Aggregates {
    min: Option<i64>,
    max: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct Figure {
    name: &'static str,
    aggregates: Aggregates,
}

#[derive(Debug, Serialize)]
pub struct FieldFacet<'a> {
    name: &'a str,
    facets: Vec<Count<'a>>,
    aggregates: Aggregates,
}

/// What matched, counted by facet, as Datadog's search answers it; borrowed from what matched.
#[derive(Debug, Serialize)]
pub struct Facets<'a> {
    state: Vec<Count<'a>>,
    severity: Vec<Count<'a>>,
    commander: Vec<UserCount<'a>>,
    created_by: Vec<UserCount<'a>>,
    last_modified_by: Vec<UserCount<'a>>,
    responder: [(); 0],
    postmortem: Vec<Count<'a>>,
    impact: Vec<Count<'a>>,
    fields: Vec<FieldFacet<'a>>,
    time_to_repair: [Figure; 1],
    time_to_resolve: [Figure; 1],
}

/// Counts of values, the most first (then by name), [`FACET_VALUES`] of them at the most.
fn sorted(by: BTreeMap<&str, u64>) -> Vec<Count<'_>> {
    let mut out: Vec<Count<'_>> = by.into_iter().map(|(name, count)| Count { name, count }).collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(b.name)));
    out.truncate(FACET_VALUES);
    out
}

fn users(by: BTreeMap<&str, u64>) -> Vec<UserCount<'_>> {
    sorted(by).into_iter().map(|c| UserCount { name: c.name, handle: c.name, email: None, uuid: c.name, count: c.count }).collect()
}

/// The least and the most of a figure, over what has it.
#[derive(Default)]
struct Range(Option<(i64, i64)>);

impl Range {
    fn add(&mut self, x: Option<i64>) {
        if let Some(x) = x {
            self.0 = Some(self.0.map_or((x, x), |(lo, hi)| (lo.min(x), hi.max(x))));
        }
    }

    fn figure(&self, name: &'static str) -> [Figure; 1] {
        [Figure { name, aggregates: Aggregates { min: self.0.map(|r| r.0), max: self.0.map(|r| r.1) } }]
    }
}

/// What matched, counted by facet, as Datadog's search answers: in one pass, its values borrowed
/// (a map's node allocated a few distinct values, nothing an incident).
pub fn facets<'a>(matched: &[&'a Incident]) -> Facets<'a> {
    let mut by: [BTreeMap<&str, u64>; 7] = Default::default();
    let mut fields: BTreeMap<&str, BTreeMap<&str, u64>> = BTreeMap::new();
    let (mut repair, mut resolve) = (Range::default(), Range::default());
    for i in matched {
        let [state, severity, commander, created_by, modified_by, postmortem, impact] = &mut by;
        *state.entry(i.state.as_str()).or_default() += 1;
        *severity.entry(i.severity.as_str()).or_default() += 1;
        if let Some(c) = &i.commander {
            *commander.entry(c.as_str()).or_default() += 1;
        }
        *created_by.entry(i.created_by.as_str()).or_default() += 1;
        *modified_by.entry(i.last_modified_by.as_str()).or_default() += 1;
        *postmortem.entry(yes_no(i.has_postmortem())).or_default() += 1;
        *impact.entry(yes_no(i.customer_impacted)).or_default() += 1;
        for (name, f) in i.fields.iter().filter(|(_, f)| COUNTED.contains(&f.kind.as_str())) {
            let values = fields.entry(name.as_str()).or_default();
            match &f.value {
                Value::String(s) => *values.entry(s.as_str()).or_default() += 1,
                Value::Array(a) => {
                    for v in a.iter().filter_map(Value::as_str) {
                        *values.entry(v).or_default() += 1;
                    }
                }
                _ => {}
            }
        }
        let d = i.durations();
        repair.add(d.time_to_repair);
        resolve.add(d.time_to_resolve);
    }
    let [state, severity, commander, created_by, modified_by, postmortem, impact] = by;
    Facets {
        state: sorted(state),
        severity: sorted(severity),
        commander: users(commander),
        created_by: users(created_by),
        last_modified_by: users(modified_by),
        responder: [],
        postmortem: sorted(postmortem),
        impact: sorted(impact),
        fields: fields.into_iter().map(|(name, values)| FieldFacet { name, facets: sorted(values), aggregates: Aggregates { min: None, max: None } }).collect(),
        time_to_repair: repair.figure("time_to_repair"),
        time_to_resolve: resolve.figure("time_to_resolve"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use serde_json::json;

    use super::super::{Change, Field, Incidents};
    use super::*;

    /// Datadog's examples and the rest of the syntax: facets joined by AND, values by OR, a field
    /// by its name, a word of the title; what is malformed refused; the facets of what matched.
    #[test]
    fn incidents_are_searched_as_datadog_searches_them() {
        let dir = tempfile::tempdir().unwrap();
        let kept = Incidents::open(dir.path());
        let mk = |t: &str, sev: &str, state: &str, services: &[&str]| {
            let c = Change {
                title: Some(t.into()),
                severity: Some(sev.into()),
                state: Some(state.into()),
                fields: [("services".to_owned(), Field { kind: "multiselect".into(), value: json!(services) })].into(),
                ..Change::default()
            };
            kept.declare(c, "user:ada", 1).unwrap()
        };
        mk("Checkout down", "SEV-1", "active", &["checkout", "payments"]);
        mk("Search slow", "SEV-2", "stable", &["search"]);
        mk("Old outage", "SEV-2", "resolved", &["checkout"]);
        mk("A drill", "SEV-4", "active", &[]);
        let all = kept.list();
        let titles = |q: &str| -> Vec<String> {
            let q = parse(q).unwrap();
            let mut t: Vec<String> = all.iter().filter(|i| q.matches(i)).map(|i| i.title.clone()).collect();
            t.sort();
            t
        };
        assert_eq!(titles("state:active AND severity:(SEV-2 OR SEV-1)"), ["Checkout down"]);
        assert_eq!(titles("state:(active OR stable OR resolved)").len(), 4);
        assert_eq!(titles("services:checkout"), ["Checkout down", "Old outage"]);
        assert_eq!(titles("fields.services:check* state:resolved"), ["Old outage"]);
        assert_eq!(titles("severity:sev-2 slow"), ["Search slow"], "case aside, a word of the title");
        assert_eq!(titles("\"a drill\""), ["A drill"]);
        assert_eq!(titles("").len(), 4);
        assert_eq!(titles("postmortem:false created_by:user:ada").len(), 4);
        assert_eq!(titles("STATE:Active Severity:SEV-1 CHECKOUT"), ["Checkout down"], "case aside, keys, values and words");
        assert_eq!(titles("public_id:2"), ["Search slow"]);
        assert_eq!(titles("services:(CHECK* OR nothing)"), ["Checkout down", "Old outage"], "a glob, case aside");
        for bad in ["state:active OR severity:SEV-1", "state:(active", "state:", "a)"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        let q = parse("services:checkout").unwrap();
        let matched: Vec<&Incident> = all.iter().filter(|i| q.matches(i)).collect();
        let f = serde_json::to_value(facets(&matched)).unwrap();
        assert_eq!(f["severity"], json!([{ "name": "SEV-1", "count": 1 }, { "name": "SEV-2", "count": 1 }]));
        assert_eq!(f["state"], json!([{ "name": "active", "count": 1 }, { "name": "resolved", "count": 1 }]));
        assert_eq!(f["fields"][0]["name"], "services");
        assert_eq!(f["fields"][0]["facets"][0], json!({ "name": "checkout", "count": 2 }));
        assert_eq!(f["created_by"][0]["count"], 2);
        // A text of one's own is not counted: a summary is never the same twice.
        let c = Change { title: Some("Noted".into()), fields: [("summary".to_owned(), Field { kind: "textbox".into(), value: json!("it broke") })].into(), ..Change::default() };
        kept.declare(c, "user:ada", 2).unwrap();
        let all = kept.list();
        let f = serde_json::to_value(facets(&all.iter().collect::<Vec<_>>())).unwrap();
        assert_eq!(f["fields"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect::<Vec<_>>(), ["services"]);
    }
}
