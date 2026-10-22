//! The research operations (`research.*`, docs/research.md), in kiln's one operation table
//! (`crate::ops::own`): the MCP tools agents record what they find out with, code mode's
//! `kiln.research.*`, and the console's `/api/research` routes. And `conversations.search`: every
//! agent's conversations, for what an agent looks into inside kiln.

use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Add, Error, Research};
use crate::ops::{Cx, Fail, Op, Out, path};

fn fail(e: Error) -> Fail {
    match e {
        Error::Bad(m) => (StatusCode::BAD_REQUEST, m),
        Error::Missing => (StatusCode::NOT_FOUND, "no such research".into()),
        Error::Other(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

fn json_of<T: serde::Serialize>(v: &T) -> Out {
    serde_json::to_value(v).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn id_of(v: &Value) -> Result<String, Fail> {
    v.get("id").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).ok_or_else(|| (StatusCode::BAD_REQUEST, "`id` is the research's id".into()))
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

fn text(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Whether `cx`'s caller adds to research `r`: a person to any; an agent to its own and the
/// research of the agents it started.
fn may_add(cx: &Cx, r: &Research) -> Result<(), Fail> {
    match (cx.caller.agent.as_deref(), r.agent.as_deref()) {
        (None, _) => Ok(()),
        (Some(_), Some(a)) => crate::harness::lineage::may_steer(&cx.hub, &cx.caller, a),
        (Some(_), None) => Err((StatusCode::FORBIDDEN, "this research is a person's: an agent adds to its own (and to that of the agents it started)".into())),
    }
}

async fn start(cx: Cx, v: Value) -> Out {
    let question = text(&v, "question").ok_or_else(|| (StatusCode::BAD_REQUEST, "`question` is what's to be found out".to_owned()))?;
    let plan = text(&v, "plan");
    if let Some(p) = &plan {
        crate::plans::get(&cx.hub, p).await.map_err(|_| (StatusCode::NOT_FOUND, format!("no plan {p}")))?;
    }
    let r = super::start(&cx.hub, &question, cx.caller.agent.as_deref(), &cx.caller.name, plan.as_deref()).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "research.start".into(), r.id.clone()).await;
    json_of(&super::view(&r, false))
}

async fn add(cx: Cx, v: Value) -> Out {
    let id = id_of(&v)?;
    let r = super::get(&cx.hub, &id).await.map_err(fail)?;
    may_add(&cx, &r)?;
    let a: Add = serde_json::from_value(v).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let view = super::add(&cx.hub, &id, a).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "research.add".into(), id).await;
    json_of(&view)
}

async fn get(cx: Cx, v: Value) -> Out {
    let r = super::get(&cx.hub, &id_of(&v)?).await.map_err(fail)?;
    let markdown = v.get("markdown").is_none_or(|m| m.as_bool().unwrap_or_else(|| m.as_str() != Some("false")));
    json_of(&super::view(&r, markdown))
}

async fn list(cx: Cx, v: Value) -> Out {
    json_of(&json!({"research": super::list(&cx.hub, text(&v, "plan"), text(&v, "agent")).await.map_err(fail)?}))
}

/// The words of a conversation's event, shortened: what an agent reads of a hit.
fn event_words(body: &str, n: usize) -> String {
    let mut words = String::new();
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        collect(&v, &mut words);
    }
    let t = words.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > n { format!("{}…", t.chars().take(n).collect::<String>()) } else { t }
}

fn collect(v: &Value, out: &mut String) {
    match v {
        Value::String(s) if s.len() > 2 => {
            out.push_str(s);
            out.push(' ');
        }
        Value::Array(a) => a.iter().for_each(|x| collect(x, out)),
        Value::Object(o) => o.iter().filter(|(k, _)| !matches!(k.as_str(), "id" | "parent" | "client_id" | "tool_use_id" | "call_id")).for_each(|(_, x)| collect(x, out)),
        _ => {}
    }
}

async fn search(cx: Cx, v: Value) -> Out {
    let Some(store) = cx.hub.harness.convo.clone() else { return Err((StatusCode::NOT_FOUND, "this server keeps no conversations (KILN_CONVERSATIONS is off)".into())) };
    let num = |k: &str| v.get(k).and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok())));
    let query = crate::convo::store::Query {
        q: text(&v, "q").unwrap_or_default(),
        session: text(&v, "agent"),
        kind: text(&v, "kind"),
        tool: text(&v, "tool"),
        status: None,
        since: num("since"),
        until: num("until"),
        limit: num("limit").and_then(|l| usize::try_from(l).ok()).map(|l| l.clamp(1, 100)).or(Some(30)),
    };
    let rows = crate::rt::blocking(move || store.search(query)).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let rows = rows.map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")))?;
    let infos: std::collections::HashMap<String, crate::harness::session::Info> = cx.hub.harness.list().into_iter().map(|i| (i.id.clone(), i)).collect();
    let hits: Vec<Value> = rows
        .iter()
        .map(|r| {
            let info = infos.get(&r.session);
            let at = serde_json::from_str::<Value>(&r.body).ok().and_then(|e| e.get("at").and_then(Value::as_i64));
            json!({"agent": r.session, "name": info.map(|i| i.name.clone()), "machine": info.map(|i| crate::harness::wait::machine(&i.target).to_owned()), "at": at, "words": event_words(&r.body, 600)})
        })
        .collect();
    Ok(json!({"hits": hits}))
}

const SOURCE: &str = "A source: {type, title, locator, machine?, author?, published?, origin?, dated?}. type: inside (the machines and systems you work in) measurement (a command you ran: locator the command, quote what it printed), code, data, log, doc, media, record (kiln's: locator plan:<id> or agent:<id>); outside: standard, paper (peer-reviewed), docs (a product's own), book, preprint, news, blog, wiki, forum, vendor, social, web. locator: where it is (a file and its lines, src/a.rs:40-52; a URL with #section; doi:…).";

fn source_schema() -> Value {
    json!({"type": "object", "description": SOURCE, "properties": {
        "id": {"type": "string"},
        "type": {"type": "string", "enum": ["measurement", "code", "data", "log", "doc", "media", "record", "standard", "paper", "docs", "book", "preprint", "news", "blog", "wiki", "forum", "vendor", "social", "web"]},
        "title": {"type": "string"},
        "locator": {"type": "string"},
        "machine": {"type": "string"},
        "author": {"type": "string"},
        "published": {"type": "string"},
        "origin": {"type": "string", "description": "Who stands behind it, when several sources share one (they count once)."},
        "dated": {"type": "boolean", "description": "Out of date for this question."}
    }, "required": ["type"]})
}

fn cite_schema() -> Value {
    json!({"type": "object", "properties": {
        "source": {"description": "A source's id, or the source itself.", "anyOf": [{"type": "string"}, source_schema()]},
        "quote": {"type": "string", "description": "The exact words the claim rests on, as the source has them (… for words left out); for a picture, what it shows."},
        "at": {"type": "string", "description": "Where in the source: lines, a section, a page, a time."}
    }, "required": ["source"]})
}

/// The `research.*` operations, and `conversations.search`.
pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "research.start",
            summary: "Begin research on a question (yours; for a plan, its planning phase: `plan`): record what you find with research_add, each claim with its sources, and kiln grades each finding. Present research with research_get's markdown: always its sources and each finding's grade.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({"question": {"type": "string", "description": "What's to be found out."}, "plan": {"type": "string", "description": "The plan whose planning phase it is."}}),
                    &["question"],
                )
            },
            route: |_| Ok((Method::POST, path(&["api", "research"]))),
            run: |cx, v| Box::pin(start(cx, v)),
        },
        Op {
            name: "research.add",
            summary: "Record what you found out: sources (where each is and what kind of thing it is) and findings, each a claim with its citations (the source, the exact words quoted, where they are) and any counterclaims you found (with their own sources, or how strong they are). Look inside (the files on your machine and others: code, docs, data, pictures, video frames; kiln's records: conversations_search, plans, CI runs, deployments) and outside (the web, official docs and standards, papers: arXiv, Semantic Scholar, Crossref, PubMed; blogs, forums). kiln looks for each quote where you say it is and grades each finding (A to F, out of 100) by its sources' kinds, how far its citations hold up, how many independent sources agree, and what's against it: you describe the evidence, never the grade. By id, a finding or source is changed; `remove` takes them out; `skipped` says why there are no internal or no external sources.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "id": {"type": "string", "description": "The research's id."},
                        "sources": {"type": "array", "items": source_schema()},
                        "findings": {"type": "array", "items": {"type": "object", "properties": {
                            "id": {"type": "string", "description": "To change one there."},
                            "claim": {"type": "string"},
                            "cites": {"type": "array", "items": cite_schema()},
                            "against": {"type": "array", "description": "Counterclaims.", "items": {"type": "object", "properties": {"claim": {"type": "string"}, "cites": {"type": "array", "items": cite_schema()}, "strength": {"type": "string", "enum": ["strong", "moderate", "weak"]}}, "required": ["claim"]}},
                            "inferred": {"type": "boolean", "description": "Your inference from the sources, not something they say."},
                            "note": {"type": "string"}
                        }, "required": ["claim"]}},
                        "remove": {"type": "array", "items": {"type": "string"}, "description": "Findings or sources (by id) to take out."},
                        "skipped": {"type": "object", "properties": {"internal": {"type": "string"}, "external": {"type": "string"}}, "description": "Why there are no internal, or no external, sources."}
                    }),
                    &["id"],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "research", &id_of(v)?, "findings"]))),
            run: |cx, v| Box::pin(add(cx, v)),
        },
        Op {
            name: "research.get",
            summary: "Research as it stands: each finding with kiln's grade (A to F), score (out of 100) and why, its citations and how kiln's look for each quote came out (found, not found, gone, unchecked, being checked), its counterclaims; its sources; and `markdown`, the words to present it in: when you present research to a person, use it, so every claim carries its sources and grade.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"id": {"type": "string"}, "markdown": {"type": "boolean"}}), &["id"]),
            route: |v| Ok((Method::GET, path(&["api", "research", &id_of(v)?]))),
            run: |cx, v| Box::pin(get(cx, v)),
        },
        Op {
            name: "research.list",
            summary: "Research, newest change first: a plan's (`plan`), an agent's (`agent`), or all.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"plan": {"type": "string"}, "agent": {"type": "string"}}), &[]),
            route: |_| Ok((Method::GET, path(&["api", "research"]))),
            run: |cx, v| Box::pin(list(cx, v)),
        },
        Op {
            name: "conversations.search",
            summary: "Search every agent's conversation kiln keeps (what was said, what tools ran and printed), newest first: `q` words each hit holds (any case), `agent` one agent's, `kind` and `tool` one kind of event, `since`/`until` (unix ms). For what's been tried, decided or found before: cite a hit as a record (locator agent:<id>).",
            read_only: true,
            destructive: false,
            input: || {
                schema(
                    json!({"q": {"type": "string"}, "agent": {"type": "string"}, "kind": {"type": "string"}, "tool": {"type": "string"}, "since": {"type": "integer"}, "until": {"type": "integer"}, "limit": {"type": "integer", "minimum": 1, "maximum": 100}}),
                    &[],
                )
            },
            route: |_| Ok((Method::GET, path(&["api", "harness", "conversations", "search"]))),
            run: |cx, v| Box::pin(search(cx, v)),
        },
    ]
}
