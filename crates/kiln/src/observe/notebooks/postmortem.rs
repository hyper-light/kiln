//! Postmortems (`docs/observe.md` §9.5, §11.1), Datadog's: a notebook made of a template (a
//! notebook that is a template; kiln's own when none is named) and an incident. Each Markdown
//! cell's variables, Datadog's (`{{incident.title}}`, `{{incident.timeline}}`, its fields by name:
//! the Incident Variables Reference, docs.datadoghq.com), are filled with what the incident says;
//! a graph keeps its definition and takes the notebook's time, the incident's (from 30 minutes
//! before it was detected or its impact began to 30 minutes after it was resolved or its impact
//! ended, or now); the notebook is of type postmortem, the incident's postmortem.
//!
//! kiln's own template follows the Google SRE book's example postmortem (Beyer et al., *Site
//! Reliability Engineering*, O'Reilly 2016, appendix D): summary, impact, root causes, trigger,
//! resolution, detection, action items, lessons learned (what went well, what went wrong, where
//! we got lucky), timeline, supporting information.

use serde_json::{Value, json};

use super::super::incidents::{Cell as Said, Incident};
use super::{Draft, Metadata, NAME, Notebook};

/// Before and after the incident's own times, its notebook's time reaches (ms).
pub const MARGIN: i64 = 30 * 60_000;
/// Responders a summary names before it says how many more (Datadog's `responders_summary`).
pub const RESPONDERS: usize = 5;

/// The fields kiln's incidents have whether or not one says them: a variable of one an incident
/// does not have is empty, not left as written.
const KNOWN_FIELDS: [&str; 5] = ["summary", "root_cause", "services", "teams", "detection_method"];

fn utc(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .ok()
        .and_then(|t| t.format(&time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second] UTC")).ok())
        .unwrap_or_default()
}

fn clock(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok().and_then(|t| t.format(&time::macros::format_description!("[hour]:[minute]:[second]")).ok()).unwrap_or_default()
}

/// A duration as a person reads it: `2h 05m`, `14m 00s`, `40s`.
pub fn took(seconds: i64) -> String {
    let s = seconds.max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3_600 => format!("{}m {:02}s", s / 60, s % 60),
        3_600..86_400 => format!("{}h {:02}m", s / 3_600, (s % 3_600) / 60),
        _ => format!("{}d {}h", s / 86_400, (s % 86_400) / 3_600),
    }
}

/// Who worked it: its commander, then each person who wrote on its timeline (its monitor not one).
fn responders(i: &Incident) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for who in i.commander.iter().map(String::as_str).chain(i.timeline.iter().map(|c| c.author.as_str())) {
        if !who.starts_with("monitor:") && !out.contains(&who) {
            out.push(who);
        }
    }
    out
}

/// A Markdown list item's text on as many lines as it has, each after the first indented under it.
fn item(text: &str) -> String {
    text.trim().lines().collect::<Vec<_>>().join("\n  ")
}

/// Its timeline as Markdown, the oldest first: each cell's time, what it says, who said it.
fn timeline(i: &Incident) -> String {
    let mut out = String::new();
    for c in &i.timeline {
        let said = match &c.cell {
            Said::Markdown { content, important } => format!("{}{}", if *important { "**Important:** " } else { "" }, item(content)),
            Said::Changed { what, from, to } => match what.as_str() {
                "declared" => format!("Declared: {to}"),
                "postmortem" => format!("Postmortem attached: {to}"),
                _ => format!("{} {} → {}", capital(what), if from.is_empty() { "none" } else { from }, if to.is_empty() { "none" } else { to }),
            },
            Said::Monitor { monitor, state, value, .. } => format!("Monitor {monitor}: {state}{}", value.map(|v| format!(" at {v}")).unwrap_or_default()),
        };
        out.push_str(&format!("- **{}** · {} · {said}\n", clock(c.created), c.author));
    }
    out.trim_end().to_owned()
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// A field's value as text: a list's values joined.
fn field(i: &Incident, name: &str) -> Option<String> {
    i.fields.get(name).map(|f| f.values().join(", "))
}

/// What variable `name` (the part after `incident.`) says of `i` at `now`: Datadog's, then kiln's
/// own, then a field of its; none for one neither knows (left as written).
pub fn value(i: &Incident, name: &str, link_base: &str) -> Option<String> {
    let d = i.durations();
    let time = |ms: Option<i64>| ms.map(utc).unwrap_or_default();
    let figure = |s: Option<i64>| s.map(took).unwrap_or_default();
    let commander = i.commander.clone().unwrap_or_default();
    Some(match name {
        "id" => i.public_id.to_string(),
        "title" => i.title.clone(),
        "severity" => i.severity.clone(),
        "status" => capital(&i.state),
        "created" => utc(i.created),
        "detected" => utc(i.detected),
        "resolved" => time(i.resolved),
        "commander" | "commander_name" | "commander_handle" => commander,
        "created_by_name" | "created_by_handle" => i.created_by.clone(),
        "commander_email" | "created_by_email" => String::new(),
        "responders_full_list" => responders(i).join(", "),
        "responders_summary" => {
            let all = responders(i);
            let named = all.iter().take(RESPONDERS).copied().collect::<Vec<_>>().join(", ");
            if all.len() > RESPONDERS { format!("{named} and {} more", all.len() - RESPONDERS) } else { named }
        }
        "customer_impacted" => if i.customer_impacted { "True" } else { "False" }.to_owned(),
        // Its scope, and how long: the impact's duration, or ongoing while it and the incident are.
        "customer_impact" => match (i.customer_impacted, &i.customer_impact_scope) {
            (true, scope) => {
                let scope = scope.as_deref().unwrap_or("Customers impacted");
                match (d.customer_impact_duration, i.customer_impact_start, i.open()) {
                    (Some(s), _, _) => format!("{scope} ({})", took(s)),
                    (None, Some(_), true) => format!("{scope} (ongoing)"),
                    _ => scope.to_owned(),
                }
            }
            (false, _) => "None".to_owned(),
        },
        "scope_of_impact" => i.customer_impact_scope.clone().unwrap_or_default(),
        "customer_impact_start" => time(i.customer_impact_start),
        "customer_impact_end" => time(i.customer_impact_end),
        "customer_impact_duration" => figure(d.customer_impact_duration),
        "timeline" => timeline(i),
        // Notebooks' own, Datadog's live card: its facts as they were when written.
        "card" => format!(
            "| Incident #{} | {} |\n| --- | --- |\n| Severity | {} |\n| Status | {} |\n| Commander | {} |\n| Customers | {} |",
            i.public_id,
            i.title.replace('|', "\\|"),
            i.severity,
            capital(&i.state),
            if commander.is_empty() { "none" } else { commander.as_str() },
            if i.customer_impacted { "impacted" } else { "not impacted" }
        ),
        // Channels and meetings kiln does not open, and what an AI would write: empty.
        n if n.starts_with("slack_channel") || n.starts_with("ms_teams_") || n.starts_with("zoom_") || n.starts_with("ai_") => String::new(),
        // kiln's own.
        "url" => format!("{link_base}/observe/incidents/{}", i.public_id),
        "time_to_detect" => figure(d.time_to_detect),
        "time_to_internal_response" => figure(d.time_to_internal_response),
        "time_to_repair" => figure(d.time_to_repair),
        "time_to_resolve" => figure(d.time_to_resolve),
        "todos" => i
            .todos
            .iter()
            .map(|t| {
                let who = if t.assignees.is_empty() { String::new() } else { format!(" ({})", t.assignees.join(", ")) };
                let due = t.due_date.as_deref().map(|d| format!(", due {d}")).unwrap_or_default();
                format!("- [{}] {}{who}{due}", if t.completed.is_some() { "x" } else { " " }, item(&t.content))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        "links" => i.attachments.iter().filter(|a| a.attachment_type == "link").map(|a| format!("- [{}]({})", a.title.replace(']', "\\]"), a.url)).collect::<Vec<_>>().join("\n"),
        f => return field(i, f).or_else(|| KNOWN_FIELDS.contains(&f).then(String::new)),
    })
}

/// `text` with each `{{incident.<name>}}` (spaces inside the braces aside) filled; one neither
/// Datadog nor kiln knows left as written.
pub fn render(text: &str, i: &Incident, link_base: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("{{") {
        let (before, from) = rest.split_at(open);
        out.push_str(before);
        let Some(close) = from.find("}}") else {
            out.push_str(from);
            return out;
        };
        let (var, after) = from.split_at(close + 2);
        let name = var.get(2..var.len().saturating_sub(2)).map(str::trim).unwrap_or("");
        match name.strip_prefix("incident.").and_then(|n| value(i, n, link_base)) {
            Some(v) => out.push_str(&v),
            None => out.push_str(var),
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// kiln's own template's cells (see the module's notes).
pub fn builtin() -> Vec<Value> {
    let md = |t: &str| json!({ "definition": { "type": "markdown", "text": t } });
    let facts = [
        ("Incident", "[#{{incident.id}}]({{incident.url}})"),
        ("Severity", "{{incident.severity}}"),
        ("Status", "{{incident.status}}"),
        ("Commander", "{{incident.commander}}"),
        ("Responders", "{{incident.responders_full_list}}"),
        ("Services", "{{incident.services}}"),
        ("Teams", "{{incident.teams}}"),
        ("Found by", "{{incident.detection_method}}"),
        ("Detected", "{{incident.detected}}"),
        ("Declared", "{{incident.created}}"),
        ("Resolved", "{{incident.resolved}}"),
        ("Time to detect", "{{incident.time_to_detect}}"),
        ("Time to respond", "{{incident.time_to_internal_response}}"),
        ("Time to repair", "{{incident.time_to_repair}}"),
        ("Time to resolve", "{{incident.time_to_resolve}}"),
    ];
    let table: String = facts.iter().map(|(k, v)| format!("\n| {k} | {v} |")).collect();
    vec![
        md(&format!("# Postmortem: {{{{incident.title}}}}\n\n| | |\n| --- | --- |{table}")),
        md("## Summary\n\n{{incident.summary}}"),
        md("## Impact\n\n{{incident.customer_impact}}"),
        md("## Root causes\n\n{{incident.root_cause}}"),
        md("## Trigger\n\n_What set it off: a deploy, a change of configuration, a load._"),
        md("## Resolution\n\n_What ended it, and what was done to get there._"),
        md("## Detection\n\n_How it was found, and how it could have been found sooner._"),
        md("## Action items\n\n{{incident.todos}}"),
        md("## Lessons learned\n\n### What went well\n\n### What went wrong\n\n### Where we got lucky"),
        md("## Timeline\n\n{{incident.timeline}}"),
        md("## Supporting information\n\n{{incident.links}}"),
    ]
}

/// The incident's time, as a notebook's: from [`MARGIN`] before it was detected or its impact
/// began to [`MARGIN`] after it was resolved or its impact ended (`now` while it is not).
pub fn time_of(i: &Incident, now: i64) -> Value {
    let rfc =
        |ms: i64| time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default();
    let start = i.customer_impact_start.map_or(i.detected, |s| s.min(i.detected)) - MARGIN;
    let end = i.resolved.unwrap_or(now).max(i.customer_impact_end.unwrap_or(i.detected)) + MARGIN;
    json!({ "start": rfc(start), "end": rfc(end.max(start + 60_000)), "live": i.resolved.is_none() })
}

/// A postmortem's draft: `template`'s cells (kiln's own when none) with `i`'s variables filled,
/// its graphs on the notebook's time, which is the incident's; its name the incident's.
pub fn draft(i: &Incident, template: Option<&Notebook>, link_base: &str, now: i64) -> Draft {
    let cells: Vec<Value> = template.map_or_else(builtin, |t| t.cells.iter().map(|c| c.attributes.clone()).collect());
    let cells = cells
        .into_iter()
        .map(|mut a| {
            if let Some(text) = a.pointer("/definition/text").and_then(Value::as_str).map(|t| render(t, i, link_base))
                && let Some(def) = a.get_mut("definition").and_then(Value::as_object_mut)
            {
                def.insert("text".into(), Value::String(text));
            }
            if let Some(o) = a.as_object_mut() {
                o.remove("time");
            }
            (None, a)
        })
        .collect();
    let name: String = format!("Postmortem: #{} {}", i.public_id, i.title).chars().take(NAME).collect();
    Draft {
        name,
        cells,
        time: time_of(i, now),
        metadata: Metadata { is_template: false, take_snapshots: false, kind: Some("postmortem".into()) },
        template_variables: template.and_then(|t| t.template_variables.clone()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::super::super::incidents::{Change, Field, Incidents};
    use super::*;

    fn worked() -> (tempfile::TempDir, Incident) {
        let dir = tempfile::tempdir().unwrap();
        let kept = Incidents::open(dir.path());
        let t0 = 1_792_670_400_000; // 2026-10-22 12:00:00 UTC
        let c = Change {
            title: Some("Checkout | failing".into()),
            severity: Some("SEV-2".into()),
            customer_impacted: Some(true),
            customer_impact_scope: Some(Some("Every checkout fails".into())),
            customer_impact_start: Some(Some(t0)),
            detected: Some(t0 + 60_000),
            commander: Some(Some("ada".into())),
            fields: [("services".to_owned(), Field { kind: "multiselect".into(), value: serde_json::json!(["checkout", "payments"]) })].into(),
            initial_cells: vec![("Paged payments.\nThen the gateway team.".into(), true)],
            ..Change::default()
        };
        let i = kept.declare(c, "user:bob", t0 + 120_000).unwrap();
        kept.add_todo(&i.id, "Roll back", vec!["ada".into()], Some("2026-10-30".into()), "user:ada", t0 + 180_000).unwrap();
        let t = kept.add_todo(&i.id, "Write it up", vec![], None, "user:cy", t0 + 190_000).unwrap();
        kept.change_todo(&i.id, &t.id, None, None, None, Some(true), "user:cy", t0 + 200_000).unwrap();
        kept.attach(&i.id, "link", "https://grafana.example.com/d/1", "The dashboard", "user:cy", t0 + 210_000).unwrap();
        let c = Change { state: Some("resolved".into()), customer_impact_end: Some(Some(t0 + 900_000)), ..Change::default() };
        let i = kept.update(&i.id, c, "user:ada", t0 + 1_200_000).unwrap();
        (dir, i)
    }

    /// Datadog's variables filled as its reference says, kiln's own beside, a field by its name;
    /// one neither knows left as written; spaces inside the braces aside.
    #[test]
    fn a_template_s_variables_are_the_incident_s() {
        let (_dir, i) = worked();
        let r = |t: &str| render(t, &i, "https://kiln.example.com");
        assert_eq!(r("{{incident.id}}: {{ incident.title }} ({{incident.severity}}, {{incident.status}})"), "1: Checkout | failing (SEV-2, Resolved)");
        assert_eq!(r("{{incident.detected}} → {{incident.resolved}}"), "2026-10-22 12:01:00 UTC → 2026-10-22 12:20:00 UTC");
        assert_eq!(r("{{incident.commander}} / {{incident.created_by_handle}} / [{{incident.commander_email}}]"), "ada / user:bob / []");
        assert_eq!(r("{{incident.responders_full_list}}"), "ada, user:bob, user:ada", "its commander, then who wrote on its timeline");
        assert_eq!(r("{{incident.customer_impacted}}: {{incident.customer_impact}}; {{incident.customer_impact_duration}}"), "True: Every checkout fails (15m 00s); 15m 00s");
        assert_eq!(r("{{incident.services}} / [{{incident.teams}}] / {{incident.no_such_thing}}"), "checkout, payments / [] / {{incident.no_such_thing}}");
        assert_eq!(r("[{{incident.slack_channel_link}}{{incident.ai_summary}}{{incident.zoom_link}}]"), "[]");
        assert_eq!(r("{{incident.url}}"), "https://kiln.example.com/observe/incidents/1");
        assert_eq!(r("{{incident.todos}}"), "- [ ] Roll back (ada), due 2026-10-30\n- [x] Write it up");
        assert_eq!(r("{{incident.links}}"), "- [The dashboard](https://grafana.example.com/d/1)");
        assert_eq!(r("{{incident.time_to_detect}}, {{incident.time_to_repair}}"), "1m 00s, 14m 00s");
        let t = r("{{incident.timeline}}");
        assert!(t.starts_with("- **12:02:00** · user:bob · Declared: SEV-2 · active\n- **12:02:00** · user:bob · **Important:** Paged payments.\n  Then the gateway team."), "{t}");
        assert!(t.ends_with("· user:ada · State active → resolved"), "{t}");
        assert!(r("{{incident.card}}").contains("| Incident #1 | Checkout \\| failing |"), "a table's cell keeps its pipe");
        assert_eq!(r("{{incident.title"), "{{incident.title", "an unclosed variable left as written");
        assert_eq!(r("{{weather}}"), "{{weather}}");
    }

    /// kiln's template made a postmortem of an incident: every variable filled, its name the
    /// incident's, of type postmortem, on the incident's time; a template's graphs keep their
    /// definitions and take its time.
    #[test]
    fn a_postmortem_is_made_of_a_template_and_an_incident() {
        let (_dir, i) = worked();
        let d = draft(&i, None, "https://kiln.example.com", 1_792_672_000_000);
        assert_eq!((d.name.as_str(), d.metadata.kind.as_deref(), d.cells.len()), ("Postmortem: #1 Checkout | failing", Some("postmortem"), 11));
        for (_, c) in &d.cells {
            let text = c.pointer("/definition/text").and_then(Value::as_str).unwrap();
            assert!(!text.contains("{{incident."), "a variable left: {text}");
        }
        assert_eq!(d.time, json!({ "start": "2026-10-22T11:30:00Z", "end": "2026-10-22T12:50:00Z", "live": false }));
        super::super::check(&d).unwrap();
        let t = Notebook {
            id: 9,
            name: "Our template".into(),
            cells: vec![
                super::super::Cell { id: "c1".into(), attributes: json!({ "definition": { "type": "markdown", "text": "# {{incident.title}}" } }) },
                super::super::Cell { id: "c2".into(), attributes: json!({ "definition": { "type": "timeseries", "requests": [{ "q": "avg:errors{*}" }] }, "time": { "live_span": "1h" } }) },
            ],
            time: json!({ "live_span": "1h" }),
            metadata: Metadata { is_template: true, take_snapshots: false, kind: Some("postmortem".into()) },
            template_variables: None,
            author: "ada".into(),
            created: 0,
            modified: 0,
            incident: None,
            next: 2,
        };
        let d = draft(&i, Some(&t), "", 0);
        assert_eq!(d.cells[0].1, json!({ "definition": { "type": "markdown", "text": "# Checkout | failing" } }));
        assert_eq!(d.cells[1].1, json!({ "definition": { "type": "timeseries", "requests": [{ "q": "avg:errors{*}" }] } }), "on the notebook's time");
        assert_eq!(took(7_500), "2h 05m");
        assert_eq!(took(200_000), "2d 7h");
    }
}
