//! The facade's search (docs/ci.md W14-R11): `GET /search/issues` and GraphQL's `search`, over the
//! job's repository's pull requests (kiln's diffs and change requests). The qualifiers are GitHub's
//! issue search's, as far as kiln keeps their data: `repo:`, `is:` and `type:` (`pr`, `issue`,
//! `open`, `closed`, `merged`, `unmerged`, `draft`), `state:`, `head:`, `base:`, `author:`,
//! `label:` and `-label:`, `no:label`, `sort:`, and words, each of which a title or a body must
//! hold. A native repository has no issues besides its pull requests, so `is:issue` finds none; a
//! qualifier kiln has nothing for (`org:`, `archived:`, …) narrows nothing.

use std::collections::HashMap;

use http::StatusCode;
use serde_json::{Value, json};

use super::repos::Site;
use crate::http::server::{self as srv, Response};

/// A search query, read.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Query {
    /// `owner/name`.
    pub repo: Option<String>,
    /// `is:issue` (and no `is:pr`): issues only.
    pub issues_only: bool,
    pub state: Option<String>,
    pub merged: Option<bool>,
    pub draft: Option<bool>,
    pub head: Option<String>,
    pub base: Option<String>,
    pub author: Option<String>,
    pub labels: Vec<String>,
    pub without: Vec<String>,
    pub unlabelled: bool,
    pub words: Vec<String>,
    /// `created`, `updated` or `comments`, and whether descending.
    pub sort: Option<(String, bool)>,
}

/// Its words and qualifiers, a double-quoted value kept whole.
fn tokens(q: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    for c in q.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `q` as GitHub's issue search reads it.
pub fn parse(q: &str) -> Query {
    let mut out = Query::default();
    let (mut prs, mut issues) = (false, false);
    for t in tokens(q) {
        let Some((key, value)) = t.split_once(':') else {
            out.words.push(t.to_lowercase());
            continue;
        };
        let value = value.to_owned();
        match (key.to_ascii_lowercase().as_str(), value.to_ascii_lowercase().as_str()) {
            ("repo", _) => out.repo = Some(value),
            ("is" | "type", "pr" | "pull-request") => prs = true,
            ("is" | "type", "issue") => issues = true,
            ("is" | "state", s @ ("open" | "closed")) => out.state = Some(s.to_owned()),
            ("is", "merged") => out.merged = Some(true),
            ("is", "unmerged") => out.merged = Some(false),
            ("is", "draft") => out.draft = Some(true),
            ("draft", d) => out.draft = Some(d == "true"),
            ("head", _) => out.head = Some(value),
            ("base", _) => out.base = Some(value),
            ("author", _) => out.author = Some(value),
            ("label", _) => out.labels.extend(value.split(',').map(str::to_owned)),
            ("-label", _) => out.without.extend(value.split(',').map(str::to_owned)),
            ("no", "label") => out.unlabelled = true,
            ("sort", s) => {
                let (field, desc) = match s.rsplit_once('-') {
                    Some((f, dir)) => (f.to_owned(), dir != "asc"),
                    None => (s.to_owned(), true),
                };
                out.sort = Some((field, desc));
            }
            _ => {}
        }
    }
    out.issues_only = issues && !prs;
    out
}

fn text<'a>(v: &'a Value, at: &str) -> &'a str {
    v.pointer(at).and_then(Value::as_str).unwrap_or_default()
}

impl Query {
    /// Whether pull request `pr` (the facade's JSON of one) is a match.
    pub fn matches(&self, pr: &Value) -> bool {
        if self.issues_only {
            return false;
        }
        let names: Vec<String> = pr.get("labels").and_then(Value::as_array).into_iter().flatten().map(|l| text(l, "/name").to_lowercase()).collect();
        let merged = !pr.get("merged_at").is_none_or(Value::is_null);
        let words = format!("{} {}", text(pr, "/title"), text(pr, "/body")).to_lowercase();
        self.state.as_deref().is_none_or(|s| text(pr, "/state") == s)
            && self.merged.is_none_or(|m| merged == m)
            && self.draft.is_none_or(|d| pr.get("draft").and_then(Value::as_bool).unwrap_or(false) == d)
            && self.head.as_deref().is_none_or(|h| text(pr, "/head/ref") == h.rsplit(':').next().unwrap_or(h))
            && self.base.as_deref().is_none_or(|b| text(pr, "/base/ref") == b)
            && self.author.as_deref().is_none_or(|a| text(pr, "/user/login").eq_ignore_ascii_case(a))
            && self.labels.iter().all(|l| names.contains(&l.to_lowercase()))
            && !self.without.iter().any(|l| names.contains(&l.to_lowercase()))
            && (!self.unlabelled || names.is_empty())
            && self.words.iter().all(|w| words.contains(w.as_str()))
    }

    /// Whether it asks about the repository named `name` (or none at all).
    pub fn of(&self, name: &str) -> bool {
        self.repo.as_deref().is_none_or(|r| r.eq_ignore_ascii_case(name))
    }

    /// The matches among `prs`, ordered as it says (`sort`, or `sort`/`order` given apart).
    pub fn select(&self, prs: Vec<Value>, sort: Option<(String, bool)>) -> Vec<Value> {
        let mut hits: Vec<Value> = prs.into_iter().filter(|p| self.matches(p)).collect();
        if let Some((field, desc)) = sort.or_else(|| self.sort.clone()) {
            let at = match field.as_str() {
                "updated" => "/updated_at",
                "comments" => "/comments",
                _ => "/created_at",
            };
            hits.sort_by(|a, b| {
                let (x, y) = (a.pointer(at).cloned().unwrap_or_default(), b.pointer(at).cloned().unwrap_or_default());
                let o = x.as_i64().zip(y.as_i64()).map(|(x, y)| x.cmp(&y)).unwrap_or_else(|| x.as_str().unwrap_or_default().cmp(y.as_str().unwrap_or_default()));
                if desc { o.reverse() } else { o }
            });
        }
        hits
    }
}

/// A pull request as search answers it: the issue it is, its `pull_request` part, and a score.
pub fn issue_of(pr: &Value) -> Value {
    let num = pr.get("number").cloned().unwrap_or_default();
    json!({
        "id": pr.get("id"), "node_id": pr.get("node_id"), "number": num, "url": pr.get("issue_url"), "html_url": pr.get("html_url"),
        "repository_url": text(pr, "/base/repo/url"), "state": pr.get("state"), "title": pr.get("title"), "body": pr.get("body"), "user": pr.get("user"),
        "labels": pr.get("labels"), "assignees": pr.get("assignees"), "comments": pr.get("comments"), "draft": pr.get("draft"), "locked": false,
        "created_at": pr.get("created_at"), "updated_at": pr.get("updated_at"), "closed_at": pr.get("closed_at"), "author_association": pr.get("author_association"),
        "pull_request": { "url": pr.get("url"), "html_url": pr.get("html_url"), "diff_url": pr.get("diff_url"), "patch_url": pr.get("patch_url"), "merged_at": pr.get("merged_at") },
        "score": 1.0,
    })
}

/// `GET /search/issues?q=…[&sort=…&order=…&per_page=…&page=…]`.
pub async fn issues(s: Option<&Site<'_>>, params: &HashMap<String, String>) -> Result<Response, super::Fail> {
    let q = params.get("q").map(String::as_str).unwrap_or_default();
    if q.trim().is_empty() {
        return Ok(srv::json(StatusCode::UNPROCESSABLE_ENTITY, &json!({ "message": "Validation Failed", "errors": [{ "resource": "Search", "field": "q", "code": "missing" }] })));
    }
    let query = parse(q);
    let hits = match s {
        Some(s) if query.of(&s.repo.name) => {
            let sort = params.get("sort").map(|f| (f.clone(), params.get("order").is_none_or(|o| o != "asc")));
            query.select(super::pulls::all(s).await?, sort)
        }
        _ => Vec::new(),
    };
    let per_page = params.get("per_page").and_then(|p| p.parse::<usize>().ok()).unwrap_or(30).clamp(1, 100);
    let page = params.get("page").and_then(|p| p.parse::<usize>().ok()).unwrap_or(1).max(1);
    let items: Vec<Value> = hits.iter().skip((page - 1) * per_page).take(per_page).map(issue_of).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "total_count": hits.len(), "incomplete_results": false, "items": items })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: i64, title: &str, state: &str, merged: bool, head: &str, author: &str, labels: &[&str]) -> Value {
        json!({
            "number": number, "title": title, "body": "", "state": state, "merged_at": if merged { json!("2026-10-20T00:00:00Z") } else { Value::Null },
            "draft": false, "head": { "ref": head }, "base": { "ref": "main" }, "user": { "login": author },
            "labels": labels.iter().map(|l| json!({ "name": l })).collect::<Vec<_>>(), "created_at": format!("2026-10-{number:02}T00:00:00Z"),
        })
    }

    /// W14-R11: GitHub's issue-search qualifiers over pull requests.
    #[test]
    fn w14_r11_search_qualifiers() {
        let prs = vec![
            pr(1, "Fix the parser", "open", false, "fix-parser", "ada", &["bug"]),
            pr(2, "Faster builds", "closed", true, "speed", "lin", &["perf", "ci"]),
            pr(3, "Abandoned idea", "closed", false, "idea", "ada", &[]),
        ];
        let numbers = |q: &str| parse(q).select(prs.clone(), None).iter().filter_map(|p| p.get("number").and_then(Value::as_i64)).collect::<Vec<_>>();
        assert_eq!(numbers("repo:acme/app is:pr is:open"), [1]);
        assert_eq!(numbers("is:pr is:merged"), [2]);
        assert_eq!(numbers("is:closed is:unmerged"), [3]);
        assert_eq!(numbers("author:ada"), [1, 3]);
        assert_eq!(numbers("label:perf label:ci"), [2]);
        assert_eq!(numbers("-label:bug no:label"), [3]);
        assert_eq!(numbers("head:speed"), [2]);
        assert_eq!(numbers("head:someone:fix-parser"), [1], "head may name its owner");
        assert_eq!(numbers("\"the parser\""), [1], "a quoted phrase is one word");
        assert_eq!(numbers("parser builds"), Vec::<i64>::new(), "every word must be there");
        assert_eq!(numbers("is:issue"), Vec::<i64>::new(), "no issues but pull requests");
        assert_eq!(numbers("is:pr sort:created-asc"), [1, 2, 3]);
        assert_eq!(numbers("is:pr sort:created-desc"), [3, 2, 1]);
        assert!(parse("repo:acme/app").of("Acme/App") && !parse("repo:acme/other").of("acme/app") && parse("is:pr").of("acme/app"));
    }
}
