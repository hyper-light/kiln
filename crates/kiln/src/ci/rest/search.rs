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
    /// `created`, `updated` or `comments`, and whether descending.
    pub sort: Option<(String, bool)>,
    /// What a match is: the terms, with `AND`, `OR`, `NOT` (or `-`) and parentheses, as
    /// GitHub's advanced issue search has them.
    pub filter: Expr,
}

/// A search's condition.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum Expr {
    #[default]
    All,
    Term(Term),
    Not(Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
}

/// One condition on a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    State(String),
    Merged(bool),
    Draft(bool),
    Head(String),
    Base(String),
    Author(String),
    Label(String),
    Unlabelled,
    /// A word (or a quoted phrase) a title or a body holds.
    Word(String),
}

/// Its words, qualifiers, parentheses and operators, a double-quoted value kept whole.
fn tokens(q: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    for c in q.chars() {
        match c {
            '"' => quoted = !quoted,
            c if (c.is_whitespace() || c == '(' || c == ')') && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                if c == '(' || c == ')' {
                    out.push(c.to_string());
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
    // What isn't a condition (the repository, the kind, the order) comes out first.
    let mut rest = Vec::new();
    for t in tokens(q) {
        let lower = t.to_ascii_lowercase();
        match lower.split_once(':') {
            Some(("repo", _)) => out.repo = t.split_once(':').map(|(_, v)| v.to_owned()),
            Some(("is" | "type", "pr" | "pull-request")) => prs = true,
            Some(("is" | "type", "issue")) => issues = true,
            Some(("sort", s)) => {
                let (field, desc) = match s.rsplit_once('-') {
                    Some((f, dir)) => (f.to_owned(), dir != "asc"),
                    None => (s.to_owned(), true),
                };
                out.sort = Some((field, desc));
            }
            _ => rest.push(t),
        }
    }
    out.issues_only = issues && !prs;
    let mut at = 0;
    out.filter = or(&rest, &mut at);
    out
}

fn or(t: &[String], at: &mut usize) -> Expr {
    let mut any = vec![and(t, at)];
    while t.get(*at).is_some_and(|w| w == "OR") {
        *at += 1;
        any.push(and(t, at));
    }
    if any.len() == 1 { any.pop().unwrap_or_default() } else { Expr::Or(any) }
}

fn and(t: &[String], at: &mut usize) -> Expr {
    let mut all = Vec::new();
    while let Some(w) = t.get(*at) {
        match w.as_str() {
            "OR" | ")" => break,
            "AND" => *at += 1,
            _ => all.push(not(t, at)),
        }
    }
    match all.len() {
        0 => Expr::All,
        1 => all.pop().unwrap_or_default(),
        _ => Expr::And(all),
    }
}

fn not(t: &[String], at: &mut usize) -> Expr {
    let Some(w) = t.get(*at).cloned() else { return Expr::All };
    *at += 1;
    match w.as_str() {
        "NOT" => Expr::Not(Box::new(not(t, at))),
        "(" => {
            let inner = or(t, at);
            if t.get(*at).is_some_and(|w| w == ")") {
                *at += 1;
            }
            inner
        }
        _ => match w.strip_prefix('-').filter(|q| q.contains(':')) {
            Some(q) => Expr::Not(Box::new(term(q))),
            None => term(&w),
        },
    }
}

/// One term: a qualifier kiln has the data for, or a word. A qualifier it hasn't (`org:`,
/// `archived:`, …) narrows nothing.
fn term(w: &str) -> Expr {
    let Some((key, value)) = w.split_once(':') else { return Expr::Term(Term::Word(w.to_lowercase())) };
    let lower = value.to_ascii_lowercase();
    let t = match (key.to_ascii_lowercase().as_str(), lower.as_str()) {
        ("is" | "state", s @ ("open" | "closed")) => Term::State(s.to_owned()),
        ("is", "merged") => Term::Merged(true),
        ("is", "unmerged") => Term::Merged(false),
        ("is", "draft") => Term::Draft(true),
        ("draft", d) => Term::Draft(d == "true"),
        ("head", _) => Term::Head(value.to_owned()),
        ("base", _) => Term::Base(value.to_owned()),
        ("author", _) => Term::Author(value.to_owned()),
        ("label", _) => {
            // `label:a,b` is either of them.
            let any: Vec<Expr> = value.split(',').map(|l| Expr::Term(Term::Label(l.to_lowercase()))).collect();
            return if any.len() == 1 { any.into_iter().next().unwrap_or_default() } else { Expr::Or(any) };
        }
        ("no", "label") => Term::Unlabelled,
        _ => return Expr::All,
    };
    Expr::Term(t)
}

fn text<'a>(v: &'a Value, at: &str) -> &'a str {
    v.pointer(at).and_then(Value::as_str).unwrap_or_default()
}

impl Expr {
    fn holds(&self, pr: &Value) -> bool {
        match self {
            Expr::All => true,
            Expr::Not(e) => !e.holds(pr),
            Expr::And(all) => all.iter().all(|e| e.holds(pr)),
            Expr::Or(any) => any.iter().any(|e| e.holds(pr)),
            Expr::Term(t) => {
                let names = || pr.get("labels").and_then(Value::as_array).into_iter().flatten().map(|l| text(l, "/name").to_lowercase()).collect::<Vec<_>>();
                match t {
                    Term::State(s) => text(pr, "/state") == s,
                    Term::Merged(m) => !pr.get("merged_at").is_none_or(Value::is_null) == *m,
                    Term::Draft(d) => pr.get("draft").and_then(Value::as_bool).unwrap_or(false) == *d,
                    Term::Head(h) => text(pr, "/head/ref") == h.rsplit(':').next().unwrap_or(h),
                    Term::Base(b) => text(pr, "/base/ref") == b,
                    Term::Author(a) => text(pr, "/user/login").eq_ignore_ascii_case(a),
                    Term::Label(l) => names().contains(l),
                    Term::Unlabelled => names().is_empty(),
                    Term::Word(w) => format!("{} {}", text(pr, "/title"), text(pr, "/body")).to_lowercase().contains(w.as_str()),
                }
            }
        }
    }
}

impl Query {
    /// Whether pull request `pr` (the facade's JSON of one) is a match.
    pub fn matches(&self, pr: &Value) -> bool {
        !self.issues_only && self.filter.holds(pr)
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
        // The advanced syntax, as `gh pr list --search` sends it: parentheses, AND, OR, NOT.
        assert_eq!(numbers("( author:dev OR author:ada ) repo:acme/app state:open type:pr"), [1]);
        assert_eq!(numbers("(author:lin OR label:bug) AND is:pr"), [1, 2]);
        assert_eq!(numbers("NOT author:ada"), [2]);
        assert_eq!(numbers("label:perf,bug"), [1, 2], "a list of labels is either");
    }
}
