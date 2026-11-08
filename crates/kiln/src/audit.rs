//! The audit log, v2 (docs/ci.md W19-R05): one table, `audit2`, a row per change.
//!
//! - **A request that changes something** (any method but GET and HEAD, past authentication)
//!   writes exactly one row, refused or not: who (`principal`, and `on_behalf_of` for a token or
//!   an agent acting for a user), with which `token`, in what `scope`, the route (`action`), the
//!   path (`target`), the status (`result`) and the address (`ip`). What its handler records
//!   on the way (`Store::audit`) becomes that row's `detail`, not rows of its own: the request
//!   runs inside a slot, a task-local of sorts, that collects them.
//! - **What isn't a request** (a leg reading secrets, a scheduled exchange, a terminal session
//!   opened by a GET) writes a row of its own through `Store::audit`.
//! - Rows are kept `[retention] audit_days` (400 by default); today's v1 rows moved in with
//!   migration `20261018-2200_w19_audit2.sql`.
//! - **`q`**, text anywhere in a row, is read off a trigram index of its own (`audit2_search`, the DB
//!   query audit of 2026-11-06), which each node keeps up with audit2 for itself ([`keep_search`]); the
//!   rows it doesn't hold yet, and text too short for a trigram, are read as they are ([`list`]).

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::authz::Caller;
use crate::hub::Hub;
use crate::store::Store;

/// What a request's handlers recorded, for its row.
#[derive(Debug, Default)]
pub struct Slot {
    notes: Vec<String>,
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<Mutex<Slot>>>> = const { RefCell::new(None) };
}

/// Puts the slot back as it was, even when polling unwinds.
struct Restore(Option<Arc<Mutex<Slot>>>);

impl Drop for Restore {
    fn drop(&mut self) {
        let prev = self.0.take();
        CURRENT.with(|c| *c.borrow_mut() = prev);
    }
}

/// A future that runs with `slot` as the current one: what it records goes there.
pub struct Scoped<F: Future> {
    inner: Pin<Box<F>>,
    slot: Arc<Mutex<Slot>>,
}

impl<F: Future> Future for Scoped<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let slot = Arc::clone(&self.slot);
        let _restore = Restore(CURRENT.with(|c| c.replace(Some(slot))));
        self.inner.as_mut().poll(cx)
    }
}

/// When a request is being audited here: `detail` goes into its row, and the call is done.
pub fn note(actor: &str, action: &str, detail: &str) -> bool {
    CURRENT.with(|c| match c.borrow().as_ref() {
        Some(slot) => {
            let text = if detail.is_empty() { action.to_owned() } else { format!("{action}: {detail}") };
            let mut s = slot.lock();
            // Who the handler named, when not the request's principal (`admin` for the owner).
            s.notes.push(if actor.is_empty() || actor == "admin" { text } else { format!("{text} (as {actor})") });
            true
        }
        None => false,
    })
}

/// One row.
#[derive(Debug, Clone, Default)]
pub struct Entry {
    pub principal: String,
    pub on_behalf_of: Option<String>,
    pub token: Option<String>,
    pub scope: Option<String>,
    pub action: String,
    pub target: Option<String>,
    pub result: String,
    pub ip: Option<String>,
    pub detail: String,
}

pub async fn write(store: &Store, e: Entry) {
    let now = crate::util::now_unix();
    let r = store
        .call(move |c| {
            c.execute(
                "INSERT INTO audit2 (ts, principal, on_behalf_of, token, scope, action, target, result, ip, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![now, e.principal, e.on_behalf_of, e.token, e.scope, e.action, e.target, e.result, e.ip, e.detail],
            )?;
            Ok(())
        })
        .await;
    if let Err(e) = r {
        tracing::warn!(error = %e, "audit write failed");
    }
}

/// `principal` and `on_behalf_of` from a caller's name: `agent:x for user:ada`, `pat:01ab of
/// user:ada`, `user:ada`, `admin`, `job:<leg>`.
pub fn who(caller: &Caller) -> (String, Option<String>) {
    match caller.name.split_once(" for ").or_else(|| caller.name.split_once(" of ")) {
        Some((p, b)) => (p.to_owned(), Some(b.to_owned())),
        None => (caller.name.clone(), None),
    }
}

/// A request, as its row names it.
pub struct Req<'a> {
    pub caller: &'a Caller,
    pub method: &'a http::Method,
    pub path: &'a str,
    pub q: &'a HashMap<String, String>,
    pub ip: Option<std::net::IpAddr>,
}

/// Audits a request that changes something: runs `fut` with a slot for what its handlers
/// record, then writes the one row, with the status `status_of` reads from the result.
pub async fn request<F, T>(hub: &Hub, r: Req<'_>, fut: F, status_of: impl Fn(&T) -> u16) -> T
where
    F: Future<Output = T>,
{
    let Req { caller, method, path, q, ip } = r;
    let slot = Arc::new(Mutex::new(Slot::default()));
    let out = Scoped { inner: Box::pin(fut), slot: Arc::clone(&slot) }.await;
    let segs: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let rule = crate::authz::rule(method, &segs);
    let scope = crate::authz::gate::scope_of(hub, rule, &segs, q).await.text();
    let (principal, on_behalf_of) = who(caller);
    let notes = std::mem::take(&mut slot.lock().notes);
    let action = if rule.pattern == crate::authz::UNNAMED.pattern { format!("{method} {path}") } else { format!("{method} {}", rule.pattern) };
    write(
        &hub.store,
        Entry { principal, on_behalf_of, token: caller.token.clone(), scope: Some(scope), action, target: Some(path.to_owned()), result: status_of(&out).to_string(), ip: ip.map(|i| i.to_string()), detail: notes.join("; ") },
    )
    .await;
    out
}

/// `GET /api/audit`: newest first, filtered by any field (`principal`, `on_behalf_of`, `token`,
/// `scope`, `action`, `target`, `result`, `ip`: each exactly one of its comma-separated values;
/// `since`, `until`: Unix seconds; `q`: text in any of `principal`, `on_behalf_of`, `action`, `target`
/// and `detail`, as it's written but for ASCII letters' case), a page at a time (`before`: the last
/// row's `id`).
pub async fn list(store: &Store, q: &HashMap<String, String>) -> anyhow::Result<Value> {
    const FIELDS: &[&str] = &["principal", "on_behalf_of", "token", "scope", "action", "target", "result", "ip"];
    let mut conds = Vec::new();
    for f in FIELDS {
        let Some(v) = q.get(*f).map(|v| v.trim()).filter(|v| !v.is_empty()) else { continue };
        let any: Vec<String> = v.split(',').map(str::trim).filter(|o| !o.is_empty()).map(str::to_owned).collect();
        if !any.is_empty() {
            conds.push(Cond::In(f, any));
        }
    }
    for (k, op) in [("since", ">="), ("until", "<")] {
        if let Some(v) = q.get(k).and_then(|v| v.parse::<i64>().ok()) {
            conds.push(Cond::Ts(op, v));
        }
    }
    let limit: i64 = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(200).clamp(1, 1000);
    let ask = Ask {
        conds,
        text: q.get("q").map(|v| v.trim()).filter(|v| !v.is_empty()).map(str::to_owned),
        before: q.get("before").and_then(|v| v.parse::<i64>().ok()),
        limit: usize::try_from(limit).unwrap_or(200),
    };
    // A read: on a read connection of its own, beside the writer rather than in its turn behind
    // every write (the DB query audit, 2026-11-06).
    store.read(move |c| Ok(Value::Array(ask.rows(c, Turns::Timed)?))).await
}

/// The columns of a row the list says, in [`entry`]'s order.
const COLUMNS: &str = "id, ts, principal, on_behalf_of, token, scope, action, target, result, ip, detail";

/// A condition of the list's on a row: one of a field's values, or a time (`ts`, by its operator).
#[derive(Debug, Clone)]
enum Cond {
    In(&'static str, Vec<String>),
    Ts(&'static str, i64),
}

/// What the list is asked for.
#[derive(Debug, Clone)]
struct Ask {
    conds: Vec<Cond>,
    /// `q`, trimmed.
    text: Option<String>,
    before: Option<i64>,
    limit: usize,
}

/// A statement's values, numbered in the order they're added.
#[derive(Default)]
struct Args(Vec<rusqlite::types::Value>);

impl Args {
    /// `v` as the statement's next value: its place, `?n`.
    fn push(&mut self, v: impl Into<rusqlite::types::Value>) -> String {
        self.0.push(v.into());
        format!("?{}", self.0.len())
    }

    /// Whether `like` (a `LIKE` pattern, `\` its escape) matches any of the searched fields of the row
    /// `t` names (`""` or `"a."`). The pattern `+`: as a bare value, SQLite would compile the statement
    /// again for each value bound to it (for an index it might seek by its prefix, which `%` never has).
    fn like(&mut self, like: &str, t: &str) -> String {
        let n = self.push(like.to_owned());
        let any: Vec<String> = ["principal", "on_behalf_of", "action", "target", "detail"].iter().map(|f| format!("{t}{f} LIKE +{n} ESCAPE '\\'")).collect();
        format!("({})", any.join(" OR "))
    }

    /// The conditions on the row `t` names, each after an `AND`. `chunked`: for a read of a range of ids,
    /// a time read off each row (`+ts`), not off `audit2_ts`, which keeps rows by time, not by id: SQLite
    /// would read every row of the time's range for each range of ids.
    fn conds(&mut self, conds: &[Cond], t: &str, chunked: bool) -> String {
        let mut out = String::new();
        for c in conds {
            match c {
                Cond::In(field, values) => {
                    let any: Vec<String> = values.iter().map(|v| self.push(v.clone())).collect();
                    out.push_str(&format!(" AND {t}{field} IN ({})", any.join(", ")));
                }
                Cond::Ts(op, v) => {
                    let (plus, at) = (if chunked { "+" } else { "" }, self.push(*v));
                    out.push_str(&format!(" AND {plus}{t}ts {op} {at}"));
                }
            }
        }
        out
    }

    /// Its values, the first three (a range's top and bottom, `?1` and `?2`, and how many rows of it to
    /// read at most, `?3`) as `hi`, `lo` and `want`.
    fn range(&self, hi: i64, lo: i64, want: usize) -> Vec<rusqlite::types::Value> {
        [hi.into(), lo.into(), i64::try_from(want).unwrap_or(i64::MAX).into()].into_iter().chain(self.0.iter().skip(3).cloned()).collect()
    }
}

/// A row of [`COLUMNS`], as the list says it.
fn entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let principal: String = r.get(2)?;
    let behalf: Option<String> = r.get(3)?;
    Ok(json!({
        "id": r.get::<_, i64>(0)?, "ts": r.get::<_, i64>(1)?, "principal": principal, "on_behalf_of": behalf,
        "token": r.get::<_, Option<String>>(4)?, "scope": r.get::<_, Option<String>>(5)?, "action": r.get::<_, String>(6)?,
        "target": r.get::<_, Option<String>>(7)?, "result": r.get::<_, String>(8)?, "ip": r.get::<_, Option<String>>(9)?,
        "detail": r.get::<_, String>(10)?,
        // v1's field, for its readers.
        "actor": match &behalf { Some(b) => format!("{principal} for {b}"), None => principal.clone() },
    }))
}

/// The trigrams of `q` the search reads for at most, spread over it, and of those the ones it ANDs.
const TILES: usize = 6;
const TERMS: usize = 3;
/// How many of the newest rows holding a trigram say how sparsely they hold it ([`search_query`]).
const PROBE: i64 = 32;
/// The ids [`Ask::race`]'s read A takes in its first range, and in its largest: each after it twice as
/// many, or eight times after one read in under [`CHUNK_QUICK`].
const CHUNK: (i64, i64) = (if cfg!(test) { 8 } else { 1_024 }, 1 << 20);
const CHUNK_QUICK: Duration = Duration::from_micros(250);
/// How many rows a time's conditions may take for the list to read only the ids between theirs
/// ([`window`]).
const WINDOW: i64 = 4_096;
/// How long [`Ask::race`]'s read A goes on alone while it finds rows, before the search goes beside it.
const HEAD: Duration = Duration::from_millis(2);

/// How the two reads of a search for `q` take their turns ([`Ask::race`]).
#[derive(Debug, Clone, Copy)]
enum Turns {
    /// The conditions' rows alone while they likely fill the list ([`Race::a_fills`]), then each read as
    /// long as the other has run.
    Timed,
    /// This many of the conditions' ranges for each of the search's (tests: none, the search first).
    #[cfg(test)]
    Rows(usize),
}

impl Ask {
    /// The list's rows, newest first.
    fn rows(&self, c: &Connection, turns: Turns) -> anyhow::Result<Vec<Value>> {
        let (Some(first), Some(newest)) = ends(c)? else { return Ok(Vec::new()) };
        let top = self.before.map_or(newest, |b| newest.min(b.saturating_sub(1)));
        let Some(text) = &self.text else { return self.one(c, None, top) };
        let like = format!("%{}%", text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let (indexed, _) = searched(c)?;
        let tiles = tiles(text);
        // Text of fewer than three characters has no trigram, and rows the search doesn't hold yet only a read.
        if tiles.is_empty() || indexed < first {
            return self.one(c, Some(&like), top);
        }
        let (lo, top) = match window(c, &self.conds)? {
            Window::Wide => (first, top),
            Window::Between(lo, hi) => (lo.max(first), top.min(hi)),
            Window::Empty => return Ok(Vec::new()),
        };
        if top < lo {
            return Ok(Vec::new());
        }
        self.race(c, &like, &tiles, (lo, top), indexed.min(top), turns)
    }

    /// The list in one read of the rows its conditions take, newest first, each read for `like` when
    /// there's text: its first `limit`.
    fn one(&self, c: &Connection, like: Option<&str>, top: i64) -> anyhow::Result<Vec<Value>> {
        let (sql, a) = self.read_one(like, top);
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(a.0.iter()), entry)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// [`Ask::one`]'s read, and its values.
    fn read_one(&self, like: Option<&str>, top: i64) -> (String, Args) {
        let mut a = Args::default();
        let mut sql = format!("SELECT {COLUMNS} FROM audit2 WHERE id <= {}", a.push(top));
        if let Some(l) = like {
            let l = a.like(l, "");
            sql.push_str(&format!(" AND {l}"));
        }
        sql.push_str(&a.conds(&self.conds, "", false));
        let limit = a.push(i64::try_from(self.limit).unwrap_or(1000));
        sql.push_str(&format!(" ORDER BY id DESC LIMIT +{limit}"));
        (sql, a)
    }

    /// The list of rows holding `q` (as `like` has it) among ids `lo..=top`, read two ways side by side,
    /// newest first, their turns `turns`: (A) the rows the conditions take that hold the text, a range of
    /// ids at a time (the first [`CHUNK`]s, growing), as many of each as the list still lacks; (B) the rows the
    /// search holds with some of the text's trigrams ([`search_query`]), from `bound` down (the newest it
    /// holds), a row at a time, each read for the text and the conditions (one read: the search reads
    /// every row's entry for a trigram again for each). Every row the list takes from the deeper of the
    /// two up is found, once A has read past `bound`: the list is its first `limit` rows there, as soon as
    /// there are as many, or either has read all its rows. As quick as the quicker of the two, twice it at
    /// most: A where the text is in most of the rows the conditions take, or the conditions take few rows
    /// of an index's; B where the text is rare.
    fn race(&self, c: &Connection, like: &str, tiles: &[String], (lo, top): (i64, i64), bound: i64, turns: Turns) -> anyhow::Result<Vec<Value>> {
        let (a_sql, a) = self.read_a(like);
        let mut sa = c.prepare(&a_sql)?;
        let mut race = Race { a_found: Vec::new(), b_found: Vec::new(), a: Some(top.saturating_add(1)), b: (bound >= lo).then(|| bound.saturating_add(1)), lo, bound, limit: self.limit };
        let mut size = CHUNK.0;
        let began = Instant::now();
        // A alone while the search holds none of the rows; and, its turns timed, for its first range, then
        // on while it finds rows, for [`HEAD`], or while its next range would likely fill the list: text in
        // many rows is found as quickly as without the search, and rare text that range didn't find goes
        // to the search at once.
        let timed = matches!(turns, Turns::Timed);
        if timed && !race.done() {
            race.range_a(&mut sa, &a, &mut size)?;
        }
        while !race.done() && (race.b.is_none() || (timed && ((!race.a_found.is_empty() && began.elapsed() < HEAD) || race.a_fills(top, size)))) {
            race.range_a(&mut sa, &a, &mut size)?;
        }
        if race.done() {
            return Ok(race.rows());
        }
        let (mut a_ran, mut b_ran) = (began.elapsed(), Duration::ZERO);
        let opened = Instant::now();
        let (b_sql, b) = self.read_b(like, search_query(c, tiles, bound)?, (lo, bound));
        let mut sb = c.prepare(&b_sql)?;
        let mut rb = sb.query(rusqlite::params_from_iter(b.0.iter()))?;
        b_ran += opened.elapsed();
        let mut a_turns = 0usize;
        while !race.done() {
            let b_turn = race.b.is_some()
                && match turns {
                    Turns::Timed => b_ran <= a_ran,
                    #[cfg(test)]
                    Turns::Rows(n) => a_turns >= n,
                };
            let at = Instant::now();
            if b_turn {
                race.row_b(&mut rb)?;
                b_ran += at.elapsed();
                a_turns = 0;
            } else {
                race.range_a(&mut sa, &a, &mut size)?;
                a_ran += at.elapsed();
                a_turns = a_turns.saturating_add(1);
            }
        }
        Ok(race.rows())
    }

    /// [`Ask::race`]'s read A, and its values: the rows the conditions take among a range of ids (`?1`
    /// its top, `?2` its bottom) that hold the text (`like`), newest first, `?3` at most.
    fn read_a(&self, like: &str) -> (String, Args) {
        let mut a = Args(vec![rusqlite::types::Value::Null, rusqlite::types::Value::Null, rusqlite::types::Value::Null]);
        let l = a.like(like, "");
        let cs = a.conds(&self.conds, "", true);
        (format!("SELECT {COLUMNS} FROM audit2 WHERE id <= ?1 AND id >= ?2 AND {l}{cs} ORDER BY id DESC LIMIT +?3"), a)
    }

    /// [`Ask::race`]'s read B, and its values: the rows the search holds with `query`'s trigrams among ids
    /// `lo..=bound`, newest first, each with whether it holds the text and the conditions take it (its
    /// last column).
    fn read_b(&self, like: &str, query: String, (lo, bound): (i64, i64)) -> (String, Args) {
        let mut b = Args::default();
        let (m, hi, lo) = (b.push(query), b.push(bound), b.push(lo));
        let l = b.like(like, "a.");
        let cs = b.conds(&self.conds, "a.", false);
        let sql = format!(
            "SELECT s.rowid, a.ts, a.principal, a.on_behalf_of, a.token, a.scope, a.action, a.target, a.result, a.ip, a.detail, ({l}{cs}) FROM audit2_search s CROSS JOIN audit2 a ON a.id = s.rowid
             WHERE audit2_search MATCH {m} AND s.rowid <= {hi} AND s.rowid >= {lo} ORDER BY s.rowid DESC"
        );
        (sql, b)
    }
}

/// Where the two reads of [`Ask::race`] stand.
struct Race {
    /// The rows each has found to hold the text, newest first: none in both (a row one finds where the
    /// other has read is left to it).
    a_found: Vec<(i64, Value)>,
    b_found: Vec<(i64, Value)>,
    /// How far down each has read: every id from it up (A's to the list's top, B's to `bound`). None:
    /// every one, down to `lo`.
    a: Option<i64>,
    b: Option<i64>,
    lo: i64,
    bound: i64,
    limit: usize,
}

impl Race {
    /// How far down every row the list takes is found: A's, until A has read past the newest row the
    /// search holds; then the deeper of the two. None: every one.
    fn floor(&self) -> Option<i64> {
        match (self.a, self.b) {
            (None, _) => None,
            (Some(a), _) if a > self.bound.saturating_add(1) => Some(a),
            (Some(_), None) => None,
            (Some(a), Some(b)) => Some(a.min(b)),
        }
    }

    /// How many rows found from `floor` up.
    fn found_from(&self, floor: i64) -> usize {
        self.a_found.partition_point(|(id, _)| *id >= floor) + self.b_found.partition_point(|(id, _)| *id >= floor)
    }

    fn done(&self) -> bool {
        self.floor().is_none_or(|f| self.found_from(f) >= self.limit)
    }

    /// Whether A, alone so far from `top` down, would likely fill the list with its next range of `next`
    /// ids: as many rows found again for as many ids more.
    fn a_fills(&self, top: i64, next: i64) -> bool {
        let Some(at) = self.a else { return false };
        let (read, found, limit) = (top.saturating_add(1).saturating_sub(at), i64::try_from(self.a_found.len()).unwrap_or(i64::MAX), i64::try_from(self.limit).unwrap_or(i64::MAX));
        read > 0 && found.saturating_mul(read.saturating_add(next)) >= limit.saturating_mul(read)
    }

    /// Reads A's next range of ids, `size` of them, with `st` (`?1` its top, `?2` its bottom, `?3` the rows
    /// it reads at most: those the list lacks past the ones found above it), and grows `size` ([`CHUNK`]):
    /// where A stands then (down to the range's bottom, or to its last row when it holds as many as it
    /// reads), and the rows found, but those B has read past.
    fn range_a(&mut self, st: &mut rusqlite::Statement<'_>, args: &Args, size: &mut i64) -> anyhow::Result<()> {
        let Some(at) = self.a else { return Ok(()) };
        let hi = at.saturating_sub(1);
        let lo = hi.saturating_sub(size.saturating_sub(1)).max(self.lo);
        let want = self.limit.saturating_sub(self.found_from(at)).max(1);
        let began = Instant::now();
        let mut rows = st.query(rusqlite::params_from_iter(args.range(hi, lo, want)))?;
        let (mut n, mut last) = (0usize, None);
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            if !(id <= self.bound && self.b.is_none_or(|b| id >= b)) {
                self.a_found.push((id, entry(r)?));
            }
            (n, last) = (n + 1, Some(id));
        }
        let reached = if n >= want { last.unwrap_or(lo) } else { lo };
        self.a = (reached > self.lo).then_some(reached);
        // Eight times the ids after a range read in a moment (the conditions' rows off an index), else twice.
        let grow = if began.elapsed() < CHUNK_QUICK { 8 } else { 2 };
        *size = size.saturating_mul(grow).min(CHUNK.1);
        Ok(())
    }

    /// Reads B's next row: where B stands then, and the row when it holds the text and the conditions
    /// take it, unless A has read past it.
    fn row_b(&mut self, rows: &mut rusqlite::Rows<'_>) -> anyhow::Result<()> {
        self.b = match rows.next()? {
            Some(r) => {
                let id: i64 = r.get(0)?;
                if self.a.is_some_and(|a| id < a) && r.get::<_, Option<bool>>(11)?.unwrap_or(false) {
                    self.b_found.push((id, entry(r)?));
                }
                Some(id)
            }
            None => None,
        };
        Ok(())
    }

    /// The list: its first `limit` rows found from the floor up, newest first.
    fn rows(self) -> Vec<Value> {
        let floor = self.floor().unwrap_or(i64::MIN);
        let (mut a, mut b) = (self.a_found.into_iter().peekable(), self.b_found.into_iter().peekable());
        let mut out = Vec::new();
        while out.len() < self.limit {
            let next = match (a.peek(), b.peek()) {
                (Some((x, _)), Some((y, _))) => if x > y { a.next() } else { b.next() },
                (Some(_), None) => a.next(),
                (None, _) => b.next(),
            };
            match next {
                Some((id, v)) if id >= floor => out.push(v),
                _ => break,
            }
        }
        out
    }
}

/// The ids a time's conditions take ([`window`]).
enum Window {
    /// More rows than [`WINDOW`], or no time's condition: each row read for its time as it comes.
    Wide,
    Between(i64, i64),
    Empty,
}

/// The ids of the rows the list's time conditions (`since`, `until`) take, off `audit2_ts`, when they
/// take [`WINDOW`] rows at most: the list's rows are among them (a row's time, taken as its request
/// ended, is a moment out of its id's order, so the ids aren't those of the times' first and last).
fn window(c: &Connection, conds: &[Cond]) -> rusqlite::Result<Window> {
    let mut a = Args::default();
    let times: Vec<String> = conds.iter().filter_map(|cond| if let Cond::Ts(op, v) = cond { Some(format!("ts {op} {}", a.push(*v))) } else { None }).collect();
    if times.is_empty() {
        return Ok(Window::Wide);
    }
    let sql = format!("SELECT count(*), min(id), max(id) FROM (SELECT id FROM audit2 WHERE {} LIMIT +{})", times.join(" AND "), a.push(WINDOW + 1));
    let (n, lo, hi): (i64, Option<i64>, Option<i64>) = c.query_row(&sql, rusqlite::params_from_iter(a.0.iter()), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(match (lo, hi) {
        _ if n > WINDOW => Window::Wide,
        (Some(lo), Some(hi)) => Window::Between(lo, hi),
        _ => Window::Empty,
    })
}

/// The trigrams that tile `text` (from its first character, every third, and its last three), [`TILES`]
/// at most, spread over it from its first to its last: each a row holding the text holds. None for text
/// of fewer than three characters.
fn tiles(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let Some(last) = chars.len().checked_sub(3) else { return Vec::new() };
    let mut starts: Vec<usize> = (0..=last).step_by(3).collect();
    if starts.last() != Some(&last) {
        starts.push(last);
    }
    if starts.len() > TILES {
        let n = starts.len() - 1;
        starts = (0..TILES).filter_map(|i| starts.get(i * n / (TILES - 1)).copied()).collect();
    }
    let mut out: Vec<String> = Vec::new();
    for s in starts {
        let tile: String = chars.get(s..s + 3).unwrap_or_default().iter().collect();
        if !out.contains(&tile) {
            out.push(tile);
        }
    }
    out
}

/// How many of the newest rows the search holds at `?2` and below hold trigram `?1`, [`PROBE`] at most
/// (`?3`), and the oldest of them.
const PROBE_READ: &str = "SELECT count(*), min(rowid) FROM (SELECT rowid FROM audit2_search WHERE audit2_search MATCH ?1 AND rowid <= ?2 ORDER BY rowid DESC LIMIT +?3)";

/// The search's query for `tiles`: [`TERMS`] of them ANDed, those the newest rows the search holds at
/// `bound` and below hold most sparsely (the newest [`PROBE`] rows holding each, the further back they
/// go the sparser; fewer than that, sparser still). A trigram in every row (`/ap`) rules out next to
/// nothing, and costs the search a read of every row's entry for it: a target, its path and a
/// 32-character id, 12 ms at ten times production's size with six of its trigrams, 1.6 ms with its sparsest three.
fn search_query(c: &Connection, tiles: &[String], bound: i64) -> rusqlite::Result<String> {
    let literal = |t: &str| format!("\"{}\"", t.replace('"', "\"\""));
    let mut held: Vec<((u8, i64), &String)> = Vec::new();
    if tiles.len() > TERMS {
        let mut st = c.prepare_cached(PROBE_READ)?;
        for t in tiles {
            let (n, oldest): (i64, Option<i64>) = st.query_row(params![literal(t), bound, PROBE], |r| Ok((r.get(0)?, r.get(1)?)))?;
            held.push((if n < PROBE { (0, n) } else { (1, oldest.unwrap_or(bound) - bound) }, t));
        }
        held.sort();
    } else {
        held = tiles.iter().map(|t| ((0, 0), t)).collect();
    }
    Ok(held.iter().take(TERMS).map(|(_, t)| literal(t)).collect::<Vec<_>>().join(" AND "))
}

/// A workload's credentials from an integration, for kiln's ID token (W11's federation): who it
/// was for, and who set it going.
pub async fn exchange(hub: &Hub, i: &crate::ci::integrations::Integration, w: &kiln_ci::oidc::Workload<'_>, what: &str) {
    let workload = format!("{}:{}", w.source, w.app.unwrap_or(w.repository));
    let env = w.environment.map(|e| format!(", environment {e}")).unwrap_or_default();
    hub.store.audit(format!("{workload} for {}", w.actor), "integration.exchange".into(), format!("{} ({}) for {}{env}: {what}", i.name, i.kind, w.repository)).await;
}

/// Drops rows older than `days` (and [`keep_search`], on every node, lets them go from its search).
pub async fn prune(store: &Store, days: u32) {
    let before = crate::util::now_unix() - i64::from(days.max(1)) * 86_400;
    let _ = store.call(move |c| Ok(c.execute("DELETE FROM audit2 WHERE ts < ?1", [before]).map(|_| ())?)).await;
}

/// Rows the search takes in, or lets go, in a turn of the writer: 2,000 is 5-10 ms of it at production's
/// size (the DB query audit, 2026-11-06).
const BATCH: i64 = 2_000;
/// How often [`keep_search`] looks for rows written since: until they're in, the list reads them as they
/// are, a few at production's pace (7,000 a day).
const EVERY: Duration = Duration::from_secs(30);
/// The pause after a turn of the writer while it's behind, as long as the turn at least: the writer is the
/// search's half the time at most.
const PAUSE: Duration = Duration::from_millis(100);

/// Keeps this node's search of audit2 (`audit2_search`, a table of its own: `crate::store::LOCAL`) up with
/// its rows for as long as the server runs (`crate::server`), whichever way they came: the rows written
/// since in, the rows pruned since out, a [`BATCH`] of each in a turn of the writer ([`index_some`]), until
/// it holds them all, then every [`EVERY`]. Started on a database it doesn't hold yet, it takes them all in
/// that way; stopped, it goes on where its last turn left it.
#[allow(clippy::disallowed_methods)] // (e) the search's upkeep every EVERY; the audit log's writes could tell
pub async fn keep_search(store: &Store) -> anyhow::Result<()> {
    let mut behind: Option<(Instant, i64)> = None;
    loop {
        let wait = match search_pass(store).await {
            Ok(None) => EVERY,
            Ok(Some((more, left, held))) => {
                if more && behind.is_none() && left > BATCH {
                    tracing::info!(rows = left, "the audit log's search is taking in its rows");
                    behind = Some((Instant::now(), left));
                }
                if !more && let Some((since, rows)) = behind.take() {
                    tracing::info!(rows, took_s = since.elapsed().as_secs(), "the audit log's search holds every row");
                }
                if more { held.max(PAUSE) } else { EVERY }
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "the audit log's search couldn't take in its rows: they're read as they are meanwhile");
                EVERY
            }
        };
        crate::rt::sleep(wait).await;
    }
}

/// One pass of [`keep_search`]'s: none when the search holds what audit2 does, read without the writer;
/// else a turn of the writer, and whether there's more, how many rows it had to take in, and the turn's time.
async fn search_pass(store: &Store) -> anyhow::Result<Option<(bool, i64, Duration)>> {
    let behind = store
        .read(|c| {
            let (newest, oldest) = searched(c)?;
            let (first, last) = ends(c)?;
            Ok((last.unwrap_or(0).saturating_sub(newest), gone(first, newest) > oldest))
        })
        .await?;
    if behind.0 <= 0 && !behind.1 {
        return Ok(None);
    }
    let began = Instant::now();
    let more = store.call_local(|c| index_some(c, BATCH)).await?;
    Ok(Some((more, behind.0, began.elapsed())))
}

/// How far the search has come: the newest row of audit2 it holds, and the oldest it may.
fn searched(c: &Connection) -> rusqlite::Result<(i64, i64)> {
    Ok(c.prepare_cached("SELECT newest, oldest FROM audit2_indexed WHERE one = 1")?.query_row([], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.unwrap_or((0, 0)))
}

/// audit2's oldest row and its newest (each by itself: the two in one read scan the table).
fn ends(c: &Connection) -> rusqlite::Result<(Option<i64>, Option<i64>)> {
    c.prepare_cached("SELECT (SELECT min(id) FROM audit2), (SELECT max(id) FROM audit2)")?.query_row([], |r| Ok((r.get(0)?, r.get(1)?)))
}

/// Below which the rows the search holds are gone from audit2: its oldest row (every one below it
/// pruned), past the newest the search holds none.
fn gone(first: Option<i64>, newest: i64) -> i64 {
    first.unwrap_or(i64::MAX).min(newest.saturating_add(1))
}

/// A turn of the writer for the search, kept whole or not at all (a savepoint: a transaction of its own, or
/// part of the one its call runs in): the rows audit2 let go since out of it, then the rows written since
/// into it, `batch` of each at most; where it has come with them. Whether there's more to do.
pub(crate) fn index_some(c: &mut Connection, batch: i64) -> anyhow::Result<bool> {
    let tx = crate::store::tx(c)?;
    let (mut newest, mut oldest) = searched(&tx)?;
    let (first, last) = ends(&tx)?;
    let mut more = false;
    let gone = gone(first, newest);
    if gone > oldest {
        let to = gone.min(oldest.saturating_add(batch));
        tx.execute("DELETE FROM audit2_search WHERE rowid >= ?1 AND rowid < ?2", params![oldest, to])?;
        more |= to < gone;
        oldest = to;
    }
    if last.is_some_and(|l| l > newest) {
        let upto: Option<i64> = tx.query_row("SELECT max(id) FROM (SELECT id FROM audit2 WHERE id > ?1 ORDER BY id LIMIT +?2)", params![newest, batch], |r| r.get(0))?;
        if let Some(upto) = upto {
            tx.execute("INSERT INTO audit2_search (rowid, principal, on_behalf_of, action, target, detail) SELECT id, principal, on_behalf_of, action, target, detail FROM audit2 WHERE id > ?1 AND id <= ?2", params![newest, upto])?;
            more |= last.is_some_and(|l| l > upto);
            newest = upto;
        }
    }
    tx.execute("INSERT INTO audit2_indexed (one, newest, oldest) VALUES (1, ?1, ?2) ON CONFLICT (one) DO UPDATE SET newest = excluded.newest, oldest = excluded.oldest", params![newest, oldest])?;
    tx.commit()?;
    Ok(more)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w19_r05_who() {
        let agent = Caller::with(crate::authz::gate::Kind::Agent, "agent:fixer for user:ada", None, None);
        assert_eq!(who(&agent), ("agent:fixer".into(), Some("user:ada".into())));
        let pat = Caller::with(crate::authz::gate::Kind::Pat, "pat:01ab of user:ada", None, None);
        assert_eq!(who(&pat), ("pat:01ab".into(), Some("user:ada".into())));
        assert_eq!(who(&Caller::owner()), ("admin".into(), None));
    }

    /// The DB query audit (2026-11-06): `GET /api/audit`'s list is a read, answered while a write holds the
    /// writer (a search no row matches reads the whole table: 20 ms at production's size, that long the
    /// writer's, before), and finds the rows written before it.
    #[test]
    fn the_list_is_read_beside_the_writer() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        crate::rt::block_on(async move {
            let s = Store::open(&dir.path().join("kiln.db"))?;
            write(&s, Entry { principal: "admin".into(), action: "POST /api/x".into(), result: "200".into(), ..Default::default() }).await;
            let (held_tx, held) = futures::channel::oneshot::channel::<()>();
            let (go_tx, go) = std::sync::mpsc::channel::<()>();
            let writer = s.call(move |_| {
                let _ = held_tx.send(());
                go.recv_timeout(std::time::Duration::from_secs(10)).map_err(|_| anyhow::anyhow!("the list waited behind the write"))
            });
            let reader = async {
                let _ = held.await;
                let listed = list(&s, &HashMap::new()).await;
                let _ = go_tx.send(());
                listed
            };
            let (wrote, listed) = futures::join!(writer, reader);
            wrote?;
            let rows = listed?;
            anyhow::ensure!(rows.as_array().is_some_and(|r| r.len() == 1), "the row written before it: {rows}");
            Ok(())
        })
    }

    /// A row as the search tests write it: its id, time and searched fields.
    #[derive(Debug, Clone)]
    struct Row {
        id: i64,
        ts: i64,
        principal: String,
        on_behalf_of: Option<String>,
        action: String,
        target: Option<String>,
        detail: String,
    }

    impl Row {
        /// W19-R05's `q`: the text in any of the searched fields, as written but for ASCII letters' case.
        fn holds(&self, text: &str) -> bool {
            let text = text.to_ascii_lowercase();
            [Some(&self.principal), self.on_behalf_of.as_ref(), Some(&self.action), self.target.as_ref(), Some(&self.detail)].into_iter().flatten().any(|f| f.to_ascii_lowercase().contains(&text))
        }
    }

    /// `n` rows from `from` on, of fields that hold what the search must tell apart: `\`, `%`, `_` and `"`,
    /// text in either case, letters past ASCII in either case, ids, fields left out.
    fn rows(from: i64, n: i64) -> Vec<Row> {
        const DETAILS: &[&str] = &["", "a\\b done", "ab done", "50% done", "50 done", "x_y", "xzy", "say \"hi\"", "Δelta", "δelta", "harness: started", "HARNESS restarted", "café", "CAFÉ"];
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d ^ u64::try_from(from).unwrap_or(0);
        (from..from + n)
            .map(|id| {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                let pick = |k: u64, of: usize| usize::try_from((seed >> k) % u64::try_from(of).unwrap_or(1)).unwrap_or(0);
                Row {
                    id,
                    ts: 1_000_000 + id * 60,
                    principal: ["admin", "user:ada", "agent:fixer", "pat:01ab"].get(pick(7, 4)).copied().unwrap_or("admin").to_owned(),
                    on_behalf_of: [None, Some("user:ada"), Some("user:Bob")].get(pick(13, 3)).copied().flatten().map(str::to_owned),
                    action: ["POST /api/x", "DELETE /api/goals/{id}", "integration.exchange", "PATCH /api/settings"].get(pick(19, 4)).copied().unwrap_or("POST /api/x").to_owned(),
                    target: (id % 5 != 0).then(|| format!("/api/runs/{:016x}", seed >> 3)),
                    detail: DETAILS.get(pick(29, DETAILS.len())).copied().unwrap_or("").to_owned(),
                }
            })
            .collect()
    }

    async fn insert(s: &Store, rows: Vec<Row>) -> anyhow::Result<()> {
        s.call(move |c| {
            let mut st = c.prepare("INSERT INTO audit2 (id, ts, principal, on_behalf_of, action, target, result, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, '200', ?7)")?;
            for r in &rows {
                st.execute(params![r.id, r.ts, r.principal, r.on_behalf_of, r.action, r.target, r.detail])?;
            }
            Ok(())
        })
        .await
    }

    /// Takes rows into the search until it holds them all; how many turns that took.
    async fn index_all(s: &Store, batch: i64) -> anyhow::Result<usize> {
        let mut turns = 0;
        loop {
            turns += 1;
            if !s.call_local(move |c| index_some(c, batch)).await? {
                return Ok(turns);
            }
        }
    }

    /// The DB query audit (2026-11-06; docs/ci.md W19-R05): `q` finds the rows holding its text in any of
    /// the searched fields, as it's written but for ASCII letters' case (`\`, `%`, `_` and `"` as
    /// themselves: the search escaped `%` and `_` but not `\`, its escape, so `a\b` found `ab` and `\`
    /// found `%`), with every other condition, newest first, a page at a time; whether the search holds
    /// none of the rows, some or all, and however its two reads take their turns.
    #[test]
    fn dbq_r4_q_finds_its_text_as_written_however_it_is_read() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let s = Store::in_memory()?;
            let all = rows(1, 400);
            let rare = all.iter().find_map(|r| r.target.as_ref().and_then(|t| t.get(12..22)).map(str::to_owned)).unwrap_or_default();
            let texts = ["a\\b", "\\", "50%", "%", "x_y", "_", "ab", "HARNESS", "harness", "Δel", "δel", "say \"hi\"", "\"", "café", "CAFÉ", "api/runs/0", rare.as_str(), "zzzz", "a", "ad", "user:ada", "/api/goals/{id}", "user:bob"];
            let asks: Vec<(Vec<Cond>, Option<i64>, usize)> = vec![
                (vec![], None, 200),
                (vec![Cond::In("principal", vec!["admin".into()])], None, 200),
                (vec![Cond::In("principal", vec!["admin".into(), "pat:01ab".into()]), Cond::Ts(">=", 1_000_000 + 120 * 60)], None, 200),
                (vec![Cond::In("on_behalf_of", vec!["user:ada".into()]), Cond::Ts("<", 1_000_000 + 330 * 60)], None, 200),
                (vec![], Some(250), 7),
                (vec![], None, 1),
            ];
            let turns = [Turns::Timed, Turns::Rows(0), Turns::Rows(1), Turns::Rows(usize::MAX)];
            let check = |s: Store, known: Vec<Row>, held: &'static str| {
                let (texts, asks) = (texts.map(str::to_owned), asks.clone());
                async move {
                    for text in &texts {
                        for (conds, before, limit) in &asks {
                            let ask = Ask { conds: conds.clone(), text: Some(text.clone()), before: *before, limit: *limit };
                            let want: Vec<i64> = known
                                .iter()
                                .rev()
                                .filter(|r| before.is_none_or(|b| r.id < b) && r.holds(text))
                                .filter(|r| {
                                    conds.iter().all(|c| match c {
                                        Cond::In("principal", v) => v.contains(&r.principal),
                                        Cond::In("on_behalf_of", v) => r.on_behalf_of.as_ref().is_some_and(|b| v.contains(b)),
                                        Cond::Ts(">=", t) => r.ts >= *t,
                                        Cond::Ts("<", t) => r.ts < *t,
                                        _ => false,
                                    })
                                })
                                .take(*limit)
                                .map(|r| r.id)
                                .collect();
                            for t in turns {
                                let ask = ask.clone();
                                let got: Vec<i64> = s.read(move |c| Ok(ask.rows(c, t)?.iter().filter_map(|v| v.get("id").and_then(Value::as_i64)).collect())).await?;
                                anyhow::ensure!(got == want, "{held}: q={text:?} {conds:?} before {before:?} limit {limit} ({t:?}): {got:?}, not {want:?}");
                            }
                        }
                    }
                    anyhow::Ok(())
                }
            };
            insert(&s, all.iter().take(300).cloned().collect()).await?;
            check(s.clone(), all.iter().take(300).cloned().collect(), "none held").await?;
            index_all(&s, 64).await?;
            insert(&s, all.iter().skip(300).cloned().collect()).await?;
            check(s.clone(), all.clone(), "300 held, 100 not").await?;
            index_all(&s, 64).await?;
            check(s.clone(), all.clone(), "every one held").await?;
            Ok(())
        })
    }

    /// The DB query audit (2026-11-06): the search takes audit2's rows in a batch at a time, and lets go of
    /// those audit2 prunes: below its oldest row at once (one pruned out of order meanwhile, held still, is
    /// never listed), every one when it's empty; a pass with nothing to do doesn't take the writer; what
    /// it's done stays, across a restart.
    #[test]
    fn dbq_r4_the_search_keeps_up_with_audit2() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("kiln.db");
        crate::rt::block_on(async move {
            let s = Store::open(&db)?;
            let held = |s: &Store| {
                let s = s.clone();
                async move { s.read(|c| Ok((searched(c)?, c.query_row("SELECT count(*) FROM audit2_search", [], |r| r.get::<_, i64>(0))?))).await }
            };
            insert(&s, rows(1, 250)).await?;
            anyhow::ensure!(index_all(&s, 100).await? == 3, "250 rows, 100 a turn");
            anyhow::ensure!(held(&s).await? == ((250, 1), 250), "{:?}", held(&s).await?);
            let asked = s.asked();
            anyhow::ensure!(search_pass(&s).await?.is_none() && s.asked() == asked + 1, "holding every row, a pass only reads");
            s.call(|c| Ok(c.execute_batch("DELETE FROM audit2 WHERE id <= 120 OR id = 130")?)).await?;
            index_all(&s, 100).await?;
            anyhow::ensure!(held(&s).await? == ((250, 121), 130), "below audit2's oldest let go, 130 held still: {:?}", held(&s).await?);
            let ask = Ask { conds: vec![], text: Some("/api/runs/".into()), before: Some(131), limit: 1000 };
            let listed: Vec<i64> = s.read(move |c| Ok(ask.rows(c, Turns::Rows(0))?.iter().filter_map(|v| v.get("id").and_then(Value::as_i64)).collect())).await?;
            anyhow::ensure!(listed.iter().all(|id| (121..131).contains(id) && *id != 130) && !listed.is_empty(), "{listed:?}");
            insert(&s, rows(251, 10)).await?;
            drop(s);
            let s = Store::open(&db)?;
            anyhow::ensure!(held(&s).await? == ((250, 121), 130), "kept across a restart");
            index_all(&s, 100).await?;
            anyhow::ensure!(held(&s).await? == ((260, 121), 140), "and on from there: {:?}", held(&s).await?);
            s.call(|c| Ok(c.execute_batch("DELETE FROM audit2")?)).await?;
            index_all(&s, 100).await?;
            anyhow::ensure!(held(&s).await? == ((260, 261), 0), "audit2 empty, the search too: {:?}", held(&s).await?);
            Ok(())
        })
    }

    /// The DB query audit (2026-11-06): `GET /api/audit`'s query as the list reads it: `q` trimmed, a
    /// field's values split at commas, `limit` between 1 and 1,000, `before` a row's id.
    #[test]
    fn dbq_r4_the_list_reads_its_query() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let s = Store::in_memory()?;
            let all = rows(1, 1200);
            insert(&s, all.clone()).await?;
            let ids = |q: &[(&str, &str)]| {
                let (s, q) = (s.clone(), q.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect::<HashMap<_, _>>());
                async move { anyhow::Ok(list(&s, &q).await?.as_array().map(|a| a.iter().filter_map(|v| v.get("id").and_then(Value::as_i64)).collect::<Vec<_>>()).unwrap_or_default()) }
            };
            let newest = |n: usize, f: &dyn Fn(&Row) -> bool| -> Vec<i64> { all.iter().rev().filter(|r| f(r)).take(n).map(|r| r.id).collect() };
            anyhow::ensure!(ids(&[]).await? == newest(200, &|_| true), "200 newest first");
            anyhow::ensure!(ids(&[("q", "  user:ada ")]).await? == newest(200, &|r| r.holds("user:ada")), "q trimmed");
            anyhow::ensure!(ids(&[("principal", " admin, pat:01ab ,"), ("limit", "5")]).await? == newest(5, &|r| r.principal == "admin" || r.principal == "pat:01ab"), "a field's values");
            for (limit, n) in [("0", 1), ("-3", 1), ("5000", 1000), ("x", 200)] {
                anyhow::ensure!(ids(&[("limit", limit)]).await?.len() == n, "limit {limit}: {n}");
            }
            anyhow::ensure!(ids(&[("before", "101"), ("q", "x_y")]).await? == newest(200, &|r| r.id < 101 && r.holds("x_y")), "before a row");
            Ok(())
        })
    }

    /// The DB query audit (2026-11-06): the list's reads are compiled once each, however often their values
    /// are bound: the race's two, run once a range of ids, the probe of a trigram's, once a trigram, and
    /// the one of a list in one read. SQLite compiles a statement again as a value it might plan by is
    /// bound (a `LIKE` pattern as a bare value, each range).
    #[test]
    fn dbq_r4_the_lists_reads_are_compiled_once() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let s = Store::in_memory()?;
            insert(&s, rows(1, 60)).await?;
            index_all(&s, 64).await?;
            s.read(|c| {
                let ask = Ask { conds: vec![Cond::In("principal", vec!["admin".into(), "pat:01ab".into()]), Cond::Ts(">=", 0)], text: Some("api/runs".into()), before: None, limit: 5 };
                let like = "%api/runs%";
                let (a_sql, a) = ask.read_a(like);
                let (b_sql, b) = ask.read_b(like, search_query(c, &tiles("api/runs"), 60)?, (1, 60));
                // Each with its values bound again as the list binds them: A's for each range, the probe's
                // for each trigram; B's and the one read's once.
                let mut runs: Vec<(String, Vec<Vec<rusqlite::types::Value>>)> = vec![
                    (a_sql, [(60, 41, 5), (40, 21, 3), (20, 1, 1)].into_iter().map(|(hi, lo, want)| a.range(hi, lo, want)).collect()),
                    (PROBE_READ.to_owned(), ["\"api\"", "\"/ru\"", "\"ns/\""].into_iter().map(|t| vec![t.to_owned().into(), 60i64.into(), PROBE.into()]).collect()),
                    (b_sql, vec![b.0]),
                ];
                for like in [None, Some(like)] {
                    let (sql, args) = ask.read_one(like, 60);
                    runs.push((sql, vec![args.0]));
                }
                for (sql, values) in &runs {
                    let mut st = c.prepare(sql)?;
                    for v in values {
                        let mut rows = st.query(rusqlite::params_from_iter(v.iter()))?;
                        while rows.next()?.is_some() {}
                    }
                    anyhow::ensure!(st.get_status(rusqlite::StatementStatus::RePrepare) == 0, "compiled again as its values were bound: {sql}");
                }
                Ok(())
            })
            .await
        })
    }

    /// Inside a slot, what's recorded joins it; outside, it doesn't.
    #[test]
    fn w19_r05_slots() {
        assert!(!note("admin", "x", ""), "no request: a row of its own");
        let slot = Arc::new(Mutex::new(Slot::default()));
        let fut = Scoped {
            inner: Box::pin(async {
                assert!(note("admin", "integration.create", "audit (webhook)"));
                assert!(note("user:bob", "secret.set", "TOKEN"));
            }),
            slot: Arc::clone(&slot),
        };
        let mut fut = std::pin::pin!(fut);
        assert!(fut.as_mut().poll(&mut Context::from_waker(std::task::Waker::noop())).is_ready());
        assert!(!note("admin", "after", ""), "the slot ends with its request");
        assert_eq!(slot.lock().notes, vec!["integration.create: audit (webhook)".to_owned(), "secret.set: TOKEN (as user:bob)".to_owned()]);
    }
}
