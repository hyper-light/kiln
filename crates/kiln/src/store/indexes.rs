//! Index migrations, built by the store once kiln serves (the DB query audit, 2026-11-06). A migration
//! of indexes alone, each `CREATE INDEX IF NOT EXISTS`, changes no row: the queries an index serves
//! answer the same without it, only slower. Applied with the others as the store opened, before kiln
//! served, the audit's held kiln down while it read the custody tables whole three times: 6.7 s at
//! production's size on a quiet host (the trail's 0.9 s more), ten times that at ten times the rows,
//! past what Kubernetes waits for a start before it kills the server, each start then doing it all
//! again. So the migrations leave them ([`super::migrations::later_of`]) to this builder, on every
//! database: [`super::Store::build_indexes`] builds them once kiln serves, an index at a time, its table
//! read first on a read connection, off the writer, into the OS's cache; then the index made in one turn
//! of the writer with the pages there (0.6 s for the largest at production's size, where the build alone
//! held the writer 5.1 s), its migration recorded with its last; then a pause as long as that turn. An
//! index built stays (`IF NOT EXISTS`); a stop loses the one under way, and the next start builds the
//! rest. A store made in memory, its tables empty and nothing served from it, builds them as it's made
//! ([`super::Store::build_now`]). Said in the log as each is built, and on `/metrics` ([`render`]).

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rusqlite::{Connection, params};

/// Indexes kiln's queries name (`INDEXED BY`), which SQLite refuses to run a query without: their
/// migrations are applied in their turn, as the store opens. Every `INDEXED BY` in kiln's code of an
/// index kiln.db's migrations make is here (`tests::dbq_r4_the_indexes_queries_name_are_listed`).
pub(crate) const NAMED: &[&str] = &["runs_repo_sha"];

/// How long the builder pauses after an index at least, and after one it built, as long as the writer
/// was its: it takes at most half the writer's time.
const GAP: Duration = if cfg!(test) { Duration::from_millis(1) } else { Duration::from_secs(1) };

/// How long it waits before trying an index that failed again (doubling from the first, up to the last).
const RETRY: (Duration, Duration) = if cfg!(test) { (Duration::from_millis(5), Duration::from_millis(20)) } else { (Duration::from_secs(30), Duration::from_secs(30 * 60)) };

/// One index a migration makes: its name, its table, the statement that makes it unless it's there,
/// and the read of what building it reads ([`index_of`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub name: String,
    pub table: String,
    pub sql: String,
    pub ahead: String,
}

/// An index migration not applied yet ([`super::migrations::later_of`]): its name, and its indexes in
/// their order.
#[derive(Debug, Clone)]
pub struct Later {
    pub migration: String,
    pub indexes: Vec<Index>,
}

/// A store's index migrations not applied yet, their indexes in their migrations' order, each with its
/// migration's name; and whether the builder is under way.
#[derive(Default)]
pub(crate) struct Builds {
    left: parking_lot::Mutex<VecDeque<(String, Index)>>,
    of: usize,
    running: AtomicBool,
}

impl Builds {
    pub(crate) fn new(later: Vec<Later>) -> Self {
        let left: VecDeque<(String, Index)> = later.into_iter().flat_map(|l| l.indexes.into_iter().map(move |index| (l.migration.clone(), index))).collect();
        let of = left.len();
        LEFT.fetch_add(u64::try_from(of).unwrap_or(0), Ordering::Relaxed);
        Self { left: parking_lot::Mutex::new(left), of, running: AtomicBool::new(false) }
    }

    /// The next to build, and whether it's the last of its migration's left.
    fn next(&self) -> Option<(String, Index, bool)> {
        let left = self.left.lock();
        let (migration, index) = left.front()?.clone();
        let last = left.iter().filter(|(m, _)| *m == migration).count() == 1;
        Some((migration, index, last))
    }

    /// The next was built.
    fn built(&self) {
        if self.left.lock().pop_front().is_some() {
            sub(&LEFT, 1);
        }
    }

    /// How many are left.
    fn left(&self) -> usize {
        self.left.lock().len()
    }

}

impl Drop for Builds {
    fn drop(&mut self) {
        sub(&LEFT, u64::try_from(self.left.get_mut().len()).unwrap_or(0));
    }
}

/// Clears the builder's flag as it ends, however it ends.
struct Running<'a>(&'a AtomicBool);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Builds `index` on the store's writer `conn`, and with it records `migration` as applied once it's that
/// migration's last, the two kept together or neither (a savepoint: a transaction of its own, or part of
/// the one its call runs in): the one step of an index migration, however it's driven. Its time on the
/// writer.
fn build_one(conn: &mut Connection, index: &Index, migration: Option<&str>) -> anyhow::Result<Duration> {
    let began = Instant::now();
    let t = super::tx(conn)?;
    t.execute_batch(&index.sql)?;
    if let Some(name) = migration {
        t.execute("INSERT OR IGNORE INTO migrations (name, applied_at) VALUES (?1, ?2)", params![name, crate::util::now_unix()])?;
    }
    t.commit()?;
    Ok(began.elapsed())
}

impl super::Store {
    /// Builds the store's index migrations not applied yet ([`Builds`]), an index at a time: what building
    /// it reads, read first on a read connection (unless it's there already); then made in one turn of the
    /// writer past the cluster group (a change of the schema, which the group's log doesn't carry: a
    /// learner's copy waits for them, [`super::Store::indexes_built`]), its migration recorded in the same
    /// transaction once it's its last; then a pause as long as that turn, a second at least. An index that
    /// fails to build is tried again later, doubling the wait from 30 s to half an hour, and said each time.
    /// Returns once each is built, at once with none left or with the builder under way. Started by the
    /// server once it serves (`crate::server`), or by a learner's copy asked for before: a tool opening
    /// kiln.db beside a server builds none.
    pub async fn build_indexes(&self) -> anyhow::Result<()> {
        let builds = &self.inner.builds;
        if builds.left() == 0 || builds.running.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let _running = Running(&builds.running);
        tracing::info!(indexes = builds.left(), "kiln.db: building its index migrations");
        let mut failed = 0u32;
        while let Some((migration, index, last)) = builds.next() {
            let began = Instant::now();
            let name = index.name.clone();
            let there = self.read(move |c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1)", [name], |r| r.get::<_, bool>(0))?)).await.unwrap_or(false);
            if !there {
                let ahead = index.ahead.clone();
                if let Err(e) = self.read(move |c| Ok(c.query_row(&ahead, [], |_| Ok(()))?)).await {
                    tracing::warn!(index = %index.name, error = %format!("{e:#}"), "kiln.db: an index's table wasn't read ahead of its build: the writer reads it");
                }
            }
            let ahead = began.elapsed();
            AHEAD_MS.fetch_add(millis(ahead), Ordering::Relaxed);
            BUILDING.store(1, Ordering::Relaxed);
            let (step, record) = (index.clone(), last.then(|| migration.clone()));
            let held = self.call_local(move |c| build_one(c, &step, record.as_deref())).await;
            BUILDING.store(0, Ordering::Relaxed);
            match held {
                Ok(held) => {
                    failed = 0;
                    builds.built();
                    BUILT.fetch_add(1, Ordering::Relaxed);
                    HELD_MS.fetch_add(millis(held), Ordering::Relaxed);
                    tracing::info!(index = %index.name, %migration, recorded = last, done = builds.of.saturating_sub(builds.left()), of = builds.of, read_ahead_ms = millis(ahead), writer_ms = millis(held), "kiln.db: an index built");
                    crate::rt::sleep(held.max(GAP)).await;
                }
                Err(e) => {
                    failed = failed.saturating_add(1);
                    FAILED.fetch_add(1, Ordering::Relaxed);
                    let wait = RETRY.0.saturating_mul(1u32.checked_shl(failed.saturating_sub(1)).unwrap_or(u32::MAX)).min(RETRY.1);
                    tracing::warn!(index = %index.name, %migration, error = %format!("{e:#}"), retry_in_s = wait.as_secs(), "kiln.db: an index of its migrations didn't build; its queries go on without it");
                    crate::rt::sleep(wait).await;
                }
            }
        }
        tracing::info!(indexes = builds.of, "kiln.db: every index of its migrations is built");
        Ok(())
    }

    /// Builds every index migration left at once, this thread waiting: for a store made in memory
    /// (`Store::in_memory`), whose tables are empty and whose reads take the writer's turn, before it's
    /// handed out. Each step is [`super::Store::build_indexes`]'s, past the cluster group as it is.
    pub(crate) fn build_now(&self) -> anyhow::Result<()> {
        let builds = &self.inner.builds;
        while let Some((migration, index, last)) = builds.next() {
            let record = last.then_some(migration);
            self.now(move |c| build_one(c, &index, record.as_deref()))?;
            builds.built();
            BUILT.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Once every index migration is built: by the builder's steps here ([`super::Store::build_indexes`]),
    /// or, with the builder under way, by it. What a learner's copy of kiln.db waits for
    /// (`Store::copy_for_learner`), so that its indexes are kiln.db's: the group's log, which alone writes
    /// the learner's replica, carries no index.
    pub async fn indexes_built(&self) -> anyhow::Result<()> {
        self.build_indexes().await?;
        while self.inner.builds.left() > 0 {
            crate::rt::sleep(Duration::from_millis(250)).await;
        }
        Ok(())
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// `n` less `by`, never below none.
fn sub(n: &AtomicU64, by: u64) {
    let mut now = n.load(Ordering::Relaxed);
    while let Err(seen) = n.compare_exchange_weak(now, now.saturating_sub(by), Ordering::Relaxed, Ordering::Relaxed) {
        now = seen;
    }
}

/// What's on `/metrics` of the index migrations' builds, every store's of the process together.
static LEFT: AtomicU64 = AtomicU64::new(0);
static BUILT: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);
static HELD_MS: AtomicU64 = AtomicU64::new(0);
static AHEAD_MS: AtomicU64 = AtomicU64::new(0);
static BUILDING: AtomicU64 = AtomicU64::new(0);

/// The builds' figures, in Prometheus's text: how many indexes are left, built and failed, whether the
/// writer builds one now, and the seconds the writer was the builds' and their reads ahead took.
pub fn render(o: &mut String) {
    let mut one = |name: &str, kind: &str, help: &str, value: String| {
        let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}");
    };
    let secs = |ms: &AtomicU64| format!("{:.3}", ms.load(Ordering::Relaxed) as f64 / 1000.0);
    one("kiln_store_indexes_left", "gauge", "Indexes of kiln.db's index migrations left to build (store::indexes).", LEFT.load(Ordering::Relaxed).to_string());
    one("kiln_store_index_building", "gauge", "Whether the store's writer is building one of them now (1) or not (0).", BUILDING.load(Ordering::Relaxed).to_string());
    one("kiln_store_indexes_built_total", "counter", "Indexes of index migrations built, since kiln started.", BUILT.load(Ordering::Relaxed).to_string());
    one("kiln_store_index_failures_total", "counter", "Builds of those indexes that failed (each tried again later), since kiln started.", FAILED.load(Ordering::Relaxed).to_string());
    one("kiln_store_index_writer_seconds_total", "counter", "Seconds the store's writer spent building them: what writes waited behind.", secs(&HELD_MS));
    one("kiln_store_index_read_ahead_seconds_total", "counter", "Seconds their tables took to read ahead of their builds, on read connections, off the writer.", secs(&AHEAD_MS));
}

/// A migration's statements, its comments left out: split at each `;` outside quotes and parentheses. A
/// trigger's body splits where it shouldn't, which only makes its migration not one of indexes alone.
pub(crate) fn statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut now = String::new();
    let mut depth = 0usize;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' | '`' => {
                // To its closing quote; a doubled one is the quote itself.
                now.push(c);
                while let Some(d) = chars.next() {
                    now.push(d);
                    if d == c {
                        if chars.peek() == Some(&c) {
                            chars.next();
                            now.push(c);
                        } else {
                            break;
                        }
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                for d in chars.by_ref() {
                    if d == '\n' {
                        now.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut star = false;
                for d in chars.by_ref() {
                    if star && d == '/' {
                        break;
                    }
                    star = d == '*';
                }
                now.push(' ');
            }
            '(' => {
                depth += 1;
                now.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                now.push(c);
            }
            ';' if depth == 0 => {
                let s = now.trim();
                if !s.is_empty() {
                    out.push(s.to_owned());
                }
                now.clear();
            }
            _ => now.push(c),
        }
    }
    let s = now.trim();
    if !s.is_empty() {
        out.push(s.to_owned());
    }
    out
}

/// The index `statement` makes when it's `CREATE INDEX IF NOT EXISTS <name> ON <table> (<columns>)`, with
/// a `WHERE` or without, and nothing else: not a unique one, which can fail on what its table holds (made
/// as the store opens, its failure said there, as before). Its read ahead takes what building it reads:
/// every row's condition, and the columns of each it holds (`SELECT count(*), max(<column>), ... FROM
/// <table> NOT INDEXED WHERE <condition>`).
pub(crate) fn index_of(statement: &str) -> Option<Index> {
    let open = statement.find('(')?;
    let head: Vec<&str> = statement.get(..open)?.split_whitespace().collect();
    let [create, index, if_, not, exists, name, on, table] = head.as_slice() else { return None };
    let said = [(create, "CREATE"), (index, "INDEX"), (if_, "IF"), (not, "NOT"), (exists, "EXISTS"), (on, "ON")].iter().all(|(w, k)| w.eq_ignore_ascii_case(k));
    if !said || !plain(name) || !plain(table) {
        return None;
    }
    let close = close_of(statement, open)?;
    let columns: Vec<&str> = split_top(statement.get(open + 1..close)?).into_iter().map(|c| without_order(c.trim())).collect();
    if columns.iter().any(|c| c.is_empty()) {
        return None;
    }
    let rest = statement.get(close + 1..)?.trim();
    let condition = if rest.is_empty() {
        None
    } else {
        let (word, cond) = rest.split_once(char::is_whitespace)?;
        if !word.eq_ignore_ascii_case("WHERE") || cond.trim().is_empty() {
            return None;
        }
        Some(cond.trim())
    };
    let read: Vec<String> = columns.iter().map(|c| format!("max({c})")).collect();
    let ahead = format!("SELECT count(*), {} FROM {table} NOT INDEXED{}", read.join(", "), condition.map(|c| format!(" WHERE {c}")).unwrap_or_default());
    Some(Index { name: (*name).to_owned(), table: (*table).to_owned(), sql: statement.to_owned(), ahead })
}

/// Whether `sql` names `name` as a word (in any case): a later migration that drops, makes again or
/// names an index keeps the one that makes it from being left for after kiln serves.
pub(crate) fn mentions(sql: &str, name: &str) -> bool {
    let (hay, needle) = (sql.to_ascii_lowercase(), name.to_ascii_lowercase());
    hay.match_indices(&needle).any(|(at, _)| {
        let before = hay.get(..at).and_then(|h| h.chars().next_back());
        let after = hay.get(at + needle.len()..).and_then(|h| h.chars().next());
        !before.is_some_and(word_char) && !after.is_some_and(word_char)
    })
}

fn word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// A name kiln's migrations write as it is: letters, digits and `_`, not starting with a digit.
fn plain(w: &str) -> bool {
    w.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && w.chars().all(word_char)
}

/// A column without its `ASC` or `DESC`: the order an index keeps, which a read of it doesn't take.
fn without_order(c: &str) -> &str {
    match c.rsplit_once(char::is_whitespace) {
        Some((front, last)) if last.eq_ignore_ascii_case("ASC") || last.eq_ignore_ascii_case("DESC") => front.trim_end(),
        _ => c,
    }
}

/// Where the parenthesis that `s` opens at `open` closes, outside quotes.
fn close_of(s: &str, open: usize) -> Option<usize> {
    let (mut depth, mut quote) = (0usize, None::<char>);
    for (i, c) in s.get(open..)?.char_indices() {
        match (quote, c) {
            // A doubled quote closes and opens again: inside it still.
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

/// `s` split at each comma outside quotes and parentheses.
fn split_top(s: &str) -> Vec<&str> {
    let (mut out, mut depth, mut quote, mut start) = (Vec::new(), 0usize, None::<char>, 0usize);
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth = depth.saturating_sub(1),
            (None, ',') if depth == 0 => {
                out.push(s.get(start..i).unwrap_or_default());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s.get(start..).unwrap_or_default());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::migrations::MIGRATIONS;
    use rusqlite::Connection;

    /// The audit's migrations: the three of indexes alone, which the builder makes, and the CI area's, whose
    /// triggers make it a migration the store applies as it opens.
    const AUDIT: &[&str] = &["20261106-2101_dbq_trail", "20261106-2105_dbq_agents", "20261106-2108_dbq_custody_indexes"];
    const CI: &str = "20261106-2104_dbq_ci";

    /// A database as a build from before the audit's migrations left it, with rows the audit's indexes
    /// serve: every other migration applied and recorded.
    fn before_the_audit(path: &std::path::Path) -> anyhow::Result<()> {
        let mut c = Connection::open(path)?;
        c.execute_batch(crate::store::SCHEMA)?;
        let older: Vec<(&str, &str)> = MIGRATIONS.iter().copied().filter(|(n, _)| !AUDIT.contains(n) && *n != CI).collect();
        crate::store::migrations::apply_list(&mut c, &older)?;
        for n in 0..300_i64 {
            let kind = ["handed-on", "received", "taken", "passed-on"].get(usize::try_from(n % 4)?).copied().unwrap_or("received");
            c.execute(
                "INSERT INTO custody (digest, payload_type, kind, custodian, counterpart, under, envelope, taken_at) VALUES (?1, 'application/vnd.kiln.custody+json', ?2, ?3, ?4, ?5, '{}', ?6)",
                params![format!("sha256:{n:04}"), kind, format!("kiln://hyper-light/server/{}", n % 3), format!("kiln://hyper-light/person/ada/machine/m/agent/s{}/epoch/1", n % 7), (n % 10 == 0).then(|| format!("sha256:{:04}", n / 2)), n],
            )?;
            c.execute("INSERT INTO custody_things (record, name, algorithm, hex) VALUES (?1, ?2, ?3, ?4)", params![format!("sha256:{n:04}"), format!("Work-{}", n % 11), if n % 2 == 0 { "kiln-work-v1" } else { "sha256" }, format!("{n:064x}")])?;
        }
        Ok(())
    }

    fn has(c: &Connection, index: &str) -> anyhow::Result<bool> {
        Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1)", [index], |r| r.get(0))?)
    }

    fn recorded(c: &Connection, migration: &str) -> anyhow::Result<i64> {
        Ok(c.query_row("SELECT COUNT(*) FROM migrations WHERE name = ?1", [migration], |r| r.get(0))?)
    }

    /// Reads the audit's indexes serve, each as its rows: the same before an index is there and after.
    fn reads(c: &Connection) -> anyhow::Result<Vec<String>> {
        let mut out = Vec::new();
        for sql in [
            "SELECT digest FROM custody WHERE under = 'sha256:0010' ORDER BY digest",
            "SELECT digest FROM custody WHERE custodian = 'kiln://hyper-light/server/1' AND kind = 'received' ORDER BY taken_at DESC, rowid DESC LIMIT 3",
            "SELECT c.digest FROM custody c WHERE c.kind IN ('handed-on', 'taken', 'passed-on') AND c.counterpart LIKE '%/agent/%' AND c.counterpart LIKE '%/agent/s3/epoch/%' ORDER BY c.taken_at, c.rowid",
            "SELECT record FROM custody_things WHERE name LIKE 'work-3' ORDER BY record",
            "SELECT record FROM custody_things WHERE algorithm = 'kiln-work-v1' AND record > 'sha256:0280' ORDER BY record",
        ] {
            let rows: Vec<String> = c.prepare(sql)?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
            out.push(rows.join(","));
        }
        Ok(out)
    }

    /// The audit's indexes, each by its migration.
    fn audits_indexes() -> anyhow::Result<Vec<(&'static str, Index)>> {
        let mut out = Vec::new();
        for (name, sql) in MIGRATIONS.iter().filter(|(n, _)| AUDIT.contains(n)) {
            for st in statements(sql) {
                out.push((*name, index_of(&st).ok_or_else(|| anyhow::anyhow!("{name}: {st}"))?));
            }
        }
        Ok(out)
    }

    /// The DB query audit (2026-11-06): a database from before the audit opens with its index migrations
    /// left to the builder; reads and writes go on without them, the same rows read; built, each index is
    /// there, each migration recorded once, the reads the same. The CI area's, with triggers among its
    /// indexes, applies as the store opens. Built again, nothing changes.
    #[test]
    fn dbq_r4_indexes_are_built_after_kiln_serves_and_reads_are_the_same() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("kiln.db");
        before_the_audit(&db)?;
        let before = reads(&Connection::open(&db)?)?;
        let indexes = audits_indexes()?;
        crate::rt::block_on(async move {
            let s = Store::open(&db)?;
            let c = Connection::open(&db)?;
            anyhow::ensure!(s.inner.builds.of == indexes.len(), "the audit's {} left, not {}", indexes.len(), s.inner.builds.of);
            for (_, i) in &indexes {
                anyhow::ensure!(!has(&c, &i.name)?, "{} is left for after kiln serves", i.name);
            }
            anyhow::ensure!(has(&c, "land_queue_live")? && recorded(&c, CI)? == 1, "a migration with triggers among its indexes applies as the store opens");
            anyhow::ensure!(AUDIT.iter().all(|m| recorded(&c, m).is_ok_and(|n| n == 0)), "none recorded before its indexes are built");
            anyhow::ensure!(reads(&c)? == before, "the same rows without the indexes");
            s.kv_set("k".into(), "v".into()).await?;
            s.build_indexes().await?;
            anyhow::ensure!(!s.inner.builds.running.load(Ordering::Acquire), "built, the builder done");
            for (migration, i) in &indexes {
                anyhow::ensure!(has(&c, &i.name)?, "{} built", i.name);
                anyhow::ensure!(recorded(&c, migration)? == 1, "{migration} recorded once");
            }
            anyhow::ensure!(reads(&c)? == before, "the same rows with them");
            anyhow::ensure!(s.kv_get("k".into()).await?.as_deref() == Some("v"), "the write before them is there");
            s.build_indexes().await?;
            drop(s);
            let again = Store::open(&db)?;
            anyhow::ensure!(again.inner.builds.of == 0, "opened again, nothing is left");
            let (applied, left) = crate::store::migrations::apply(&mut Connection::open(&db)?)?;
            anyhow::ensure!(applied.is_empty() && left.is_empty(), "every migration recorded: {applied:?} {}", left.len());
            Ok(())
        })
    }

    /// The DB query audit (2026-11-06): a stop midway (some of a migration's indexes built, each in its
    /// own transaction, the migration not yet recorded) leaves the next start the rest: the migration left
    /// again, its built indexes there still and not read ahead again, the rest built, it recorded once.
    #[test]
    fn dbq_r4_a_build_stopped_midway_goes_on_at_the_next_start() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("kiln.db");
        before_the_audit(&db)?;
        let indexes = audits_indexes()?;
        crate::rt::block_on(async move {
            let s = Store::open(&db)?;
            let custody: Vec<Index> = indexes.iter().filter(|(m, _)| *m == "20261106-2108_dbq_custody_indexes").map(|(_, i)| i.clone()).collect();
            for i in custody.iter().take(2) {
                let sql = i.sql.clone();
                s.call_local(move |c| Ok(c.execute_batch(&sql)?)).await?;
            }
            drop(s);
            let c = Connection::open(&db)?;
            anyhow::ensure!(custody.iter().take(2).all(|i| has(&c, &i.name).unwrap_or(false)), "two of custody's built");
            anyhow::ensure!(recorded(&c, "20261106-2108_dbq_custody_indexes")? == 0, "custody's isn't recorded");
            let s = Store::open(&db)?;
            anyhow::ensure!(s.inner.builds.of == indexes.len(), "the audit's migrations left again, whole: {}", s.inner.builds.of);
            s.build_indexes().await?;
            for (migration, i) in &indexes {
                anyhow::ensure!(has(&c, &i.name)? && recorded(&c, migration)? == 1, "{} built, {migration} recorded once", i.name);
            }
            Ok(())
        })
    }

    /// The DB query audit (2026-11-06): a learner's copy of kiln.db asked for while the store's index
    /// migrations are left waits for them: with the builder under way, for it; with none, built by its own
    /// call. Each copy has kiln.db's indexes, every migration recorded on both.
    #[test]
    fn dbq_r4_a_learners_copy_waits_for_the_index_migrations() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let indexes = audits_indexes()?;
        crate::rt::block_on(async move {
            for under_way in [true, false] {
                let db = dir.path().join(format!("kiln-{under_way}.db"));
                before_the_audit(&db)?;
                let s = Store::open_grouped(&db, &dir.path().join(format!("cluster-{under_way}")))?;
                anyhow::ensure!(s.inner.builds.left() == indexes.len(), "left to the builder");
                let copy = dir.path().join(format!("copy-{under_way}.db"));
                if under_way {
                    let (built, copied) = futures::join!(s.build_indexes(), s.copy_for_learner(copy.clone()));
                    built?;
                    copied?;
                } else {
                    s.copy_for_learner(copy.clone()).await?;
                }
                let (c, theirs) = (Connection::open(&db)?, Connection::open(&copy)?);
                for (migration, i) in &indexes {
                    anyhow::ensure!(has(&c, &i.name)? && has(&theirs, &i.name)?, "{}: on kiln.db and its copy ({under_way})", i.name);
                    anyhow::ensure!(recorded(&c, migration)? == 1 && recorded(&theirs, migration)? == 1, "{migration} recorded on both ({under_way})");
                }
            }
            anyhow::Ok(())
        })
    }

    /// The DB query audit (2026-11-06): the builder makes every index migration, on a new database too
    /// (its tables empty, each built at once); a store made in memory has them all as it's handed out; and
    /// which migrations are index migrations is the list's alone: indexes alone, unnamed by a later
    /// migration, with no epoch, and none of their indexes a query names; they're built in their
    /// migrations' order.
    #[test]
    fn dbq_r4_the_builder_makes_every_index_migration() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let indexes = audits_indexes()?;
        crate::rt::block_on(async move {
            let s = Store::open(&dir.path().join("new.db"))?;
            anyhow::ensure!(s.inner.builds.of == indexes.len(), "a new database's are the builder's too: {}", s.inner.builds.of);
            s.build_indexes().await?;
            let c = Connection::open(dir.path().join("new.db"))?;
            anyhow::ensure!(indexes.iter().all(|(m, i)| has(&c, &i.name).unwrap_or(false) && recorded(&c, m).is_ok_and(|n| n == 1)), "built and recorded");
            let m = Store::in_memory()?;
            let named: Vec<String> = indexes.iter().map(|(_, i)| i.name.clone()).collect();
            let built: i64 = m.read(move |c| Ok(c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name IN (SELECT value FROM json_each(?1))", [serde_json::to_string(&named)?], |r| r.get(0))?)).await?;
            anyhow::ensure!(usize::try_from(built)? == indexes.len() && m.inner.builds.left() == 0, "in memory, every one at once: {built}");
            anyhow::Ok(())
        })?;
        let applied: std::collections::HashSet<String> = ["20260101-0000_t_base".to_owned()].into();
        let list = [
            ("20260101-0000_t_base", "CREATE TABLE t (a, b);"),
            ("20260101-0001_t_alone", "-- an index\nCREATE INDEX IF NOT EXISTS t_a ON t (a);\nCREATE INDEX IF NOT EXISTS t_b ON t (b DESC) WHERE b IS NOT NULL;"),
            ("20260101-0002_t_unique", "CREATE UNIQUE INDEX IF NOT EXISTS t_u ON t (a);"),
            ("20260101-0003_t_with_a_table", "CREATE TABLE u (x); CREATE INDEX IF NOT EXISTS u_x ON u (x);"),
            ("20260101-0004_t_dropped_later", "CREATE INDEX IF NOT EXISTS t_d ON t (a, b);"),
            ("20260101-0005_t_drops", "DROP INDEX IF EXISTS t_d;"),
            ("20260101-0006_t_epoch", "-- kiln:epoch 1\nCREATE INDEX IF NOT EXISTS t_e ON t (b);"),
            ("20260101-0007_t_named", "CREATE INDEX IF NOT EXISTS runs_repo_sha ON t (a);"),
            ("20260101-0008_t_semicolon", "CREATE INDEX IF NOT EXISTS t_s ON t (a) WHERE a LIKE '%;%';"),
        ];
        let later = crate::store::migrations::later_of(&list, &applied);
        let names: Vec<&str> = later.iter().map(|l| l.migration.as_str()).collect();
        anyhow::ensure!(names == ["20260101-0001_t_alone", "20260101-0008_t_semicolon"], "{names:?}");
        let fresh: Vec<String> = crate::store::migrations::later_of(&list, &Default::default()).into_iter().map(|l| l.migration).collect();
        anyhow::ensure!(fresh == names, "the same on a new database: {fresh:?}");
        let alone = later.first().map(|l| l.indexes.clone()).unwrap_or_default();
        anyhow::ensure!(alone.iter().map(|i| i.ahead.as_str()).eq(["SELECT count(*), max(a) FROM t NOT INDEXED", "SELECT count(*), max(b) FROM t NOT INDEXED WHERE b IS NOT NULL"]), "{alone:?}");
        let builds = Builds::new(later);
        let order: Vec<String> = builds.left.lock().iter().map(|(_, i)| i.name.clone()).collect();
        anyhow::ensure!(order == ["t_a", "t_b", "t_s"], "in their migrations' order: {order:?}");
        anyhow::ensure!(builds.next().is_some_and(|(_, _, last)| !last), "t_a isn't its migration's last");
        builds.built();
        anyhow::ensure!(builds.next().is_some_and(|(_, _, last)| last), "t_b is its migration's last");
        Ok(())
    }

    /// The DB query audit (2026-11-06): a migration's statements and the index each makes, read the way
    /// kiln's migrations write them (comments, quotes holding `;` and `(`, expressions of several
    /// parentheses, collations and orders); and every index of the shipped migrations of indexes alone
    /// reads, its read ahead a statement SQLite takes on kiln's schema.
    #[test]
    fn dbq_r4_indexes_read_from_their_statements() -> anyhow::Result<()> {
        let sql = "-- one; two\nCREATE INDEX IF NOT EXISTS a ON t (x) WHERE y = 'a;b(' AND z LIKE '%''%';\n/* three; */ create index if not exists b on t (lower(x), substr(y, 1, 2) COLLATE NOCASE DESC);";
        let st = statements(sql);
        anyhow::ensure!(st.len() == 2, "{st:?}");
        let a = st.first().and_then(|s| index_of(s)).ok_or_else(|| anyhow::anyhow!("a: {st:?}"))?;
        anyhow::ensure!(a.ahead == "SELECT count(*), max(x) FROM t NOT INDEXED WHERE y = 'a;b(' AND z LIKE '%''%'", "{}", a.ahead);
        let b = st.get(1).and_then(|s| index_of(s)).ok_or_else(|| anyhow::anyhow!("b: {st:?}"))?;
        anyhow::ensure!(b.ahead == "SELECT count(*), max(lower(x)), max(substr(y, 1, 2) COLLATE NOCASE) FROM t NOT INDEXED", "{}", b.ahead);
        for not in ["CREATE UNIQUE INDEX IF NOT EXISTS u ON t (x)", "CREATE INDEX u ON t (x)", "CREATE INDEX IF NOT EXISTS main.u ON t (x)", "CREATE INDEX IF NOT EXISTS u ON t (x) ORDER BY x", "CREATE TABLE IF NOT EXISTS u (x)", "CREATE INDEX IF NOT EXISTS u ON t ()"] {
            anyhow::ensure!(index_of(not).is_none(), "{not}");
        }
        anyhow::ensure!(mentions("DROP INDEX IF EXISTS Custody_Under;", "custody_under") && !mentions("CREATE INDEX custody_under_by ON x (y)", "custody_under"), "as a word, in any case");
        let mut ahead = Vec::new();
        for (name, sql) in MIGRATIONS {
            let made: Option<Vec<Index>> = statements(sql).iter().map(|s| index_of(s)).collect();
            ahead.extend(made.unwrap_or_default().into_iter().map(|i| (*name, i)));
        }
        anyhow::ensure!(AUDIT.iter().all(|m| ahead.iter().any(|(n, _)| n == m)), "each of the audit's index migrations is read as one: {:?}", ahead.iter().map(|(n, i)| (*n, i.name.as_str())).collect::<Vec<_>>());
        crate::rt::block_on(async move {
            let s = Store::in_memory()?;
            for (name, i) in ahead {
                let sql = i.ahead.clone();
                s.read(move |c| Ok(c.query_row(&sql, [], |_| Ok(()))?)).await.map_err(|e| anyhow::anyhow!("{name}'s {}: {e:#}", i.name))?;
            }
            Ok(())
        })
    }

    /// The DB query audit (2026-11-06), measured by hand on a copy of a database at production's size
    /// (`DBQ_DB`, as a build from before the audit left it; a fresh copy each run, so its pages aren't in
    /// the OS's cache; not a `KILN_*` name, which a run kept from a live server's environment drops): how
    /// long the store takes to open, then how long writes waited while its index migrations were built
    /// (one every 20 ms, as a busy server's writes come), against the same writes before the build began.
    #[test]
    #[ignore = "a measurement on a database at production's size, run by hand"]
    fn dbq_r4_measured_on_a_copy() -> anyhow::Result<()> {
        let db = std::path::PathBuf::from(std::env::var("DBQ_DB")?);
        // Each index's build said as it's built (its read ahead and its turn of the writer).
        let _ = tracing_subscriber::fmt().with_env_filter("info").with_test_writer().try_init();
        crate::rt::block_on(async move {
            let began = Instant::now();
            let s = Store::open(&db)?;
            eprintln!("the store opened in {:?}, {} indexes left to its builder", began.elapsed(), s.inner.builds.of);
            let writes = |s: Store, n: usize| async move {
                let mut took = Vec::with_capacity(n);
                for i in 0..n {
                    let at = Instant::now();
                    s.call(move |c| Ok(c.execute("INSERT OR REPLACE INTO kv (k, v) VALUES ('dbq/r4', ?1)", [i.to_string()])?)).await?;
                    took.push(at.elapsed());
                    crate::rt::sleep(Duration::from_millis(20)).await;
                }
                anyhow::Ok(took)
            };
            let said = |what: &str, mut took: Vec<Duration>| {
                took.sort_unstable();
                let at = |q: f64| took.get(((took.len() as f64 - 1.0) * q).round() as usize).copied().unwrap_or_default();
                eprintln!("{what}: {} writes, median {:?}, p99 {:?}, max {:?}, over 100 ms {}", took.len(), at(0.5), at(0.99), at(1.0), took.iter().filter(|d| **d > Duration::from_millis(100)).count());
            };
            said("writes before the build", writes(s.clone(), 250).await?);
            let (stop_tx, stop) = futures::channel::oneshot::channel::<()>();
            let load = {
                let s = s.clone();
                async move {
                    let mut took = Vec::new();
                    let mut stop = stop;
                    loop {
                        let at = Instant::now();
                        s.call(|c| Ok(c.execute("INSERT OR REPLACE INTO kv (k, v) VALUES ('dbq/r4', 'x')", [])?)).await?;
                        took.push(at.elapsed());
                        if stop.try_recv().ok().flatten().is_some() {
                            return anyhow::Ok(took);
                        }
                        crate::rt::sleep(Duration::from_millis(20)).await;
                    }
                }
            };
            let build = {
                let s = s.clone();
                async move {
                    let began = Instant::now();
                    let built = s.build_indexes().await;
                    let _ = stop_tx.send(());
                    built.map(|()| began.elapsed())
                }
            };
            let (built, took) = futures::join!(build, load);
            eprintln!("the build took {:?}", built?);
            said("writes during the build", took?);
            let mut o = String::new();
            render(&mut o);
            eprintln!("{}", o.lines().filter(|l| !l.starts_with('#')).collect::<Vec<_>>().join("\n"));
            Ok(())
        })
    }

    /// The DB query audit (2026-11-06): every index kiln's code names (`INDEXED BY`) that kiln.db's
    /// migrations make is in [`NAMED`], and each there is named.
    #[test]
    fn dbq_r4_the_indexes_queries_name_are_listed() -> anyhow::Result<()> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(std::path::PathBuf, String)>) -> anyhow::Result<()> {
            for e in std::fs::read_dir(dir)? {
                let path = e?.path();
                if path.is_dir() {
                    walk(&path, out)?;
                } else if path.extension().is_some_and(|x| x == "rs") {
                    let text = std::fs::read_to_string(&path)?;
                    out.push((path, text));
                }
            }
            Ok(())
        }
        let mut files = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files)?;
        let made: Vec<String> = MIGRATIONS
            .iter()
            .flat_map(|(_, sql)| statements(sql))
            .chain(statements(crate::store::SCHEMA))
            .filter_map(|s| {
                let words: Vec<&str> = s.split_whitespace().collect();
                let at = words.iter().position(|w| w.eq_ignore_ascii_case("INDEX"))?;
                let name = words.get(at + 1..)?.iter().find(|w| !["IF", "NOT", "EXISTS"].iter().any(|k| w.eq_ignore_ascii_case(k)))?;
                words.first().is_some_and(|w| w.eq_ignore_ascii_case("CREATE")).then(|| (*name).to_owned())
            })
            .collect();
        // Spelt apart, so this test's own text isn't one of them.
        let marker = ["INDEXED", " BY "].concat();
        let mut named = std::collections::BTreeMap::<String, Vec<std::path::PathBuf>>::new();
        for (path, text) in &files {
            for (at, _) in text.match_indices(&marker) {
                let name: String = text.get(at + marker.len()..).unwrap_or_default().chars().take_while(|c| word_char(*c)).collect();
                if made.contains(&name) {
                    named.entry(name).or_default().push(path.clone());
                }
            }
        }
        let names: Vec<&str> = named.keys().map(String::as_str).collect();
        let mut listed: Vec<&str> = NAMED.to_vec();
        listed.sort_unstable();
        anyhow::ensure!(names == listed, "kiln's queries name {names:?}, NAMED says {listed:?}: an index a query names is made as the store opens, unless the query's callers wait for it (WAITED)");
        Ok(())
    }
}
