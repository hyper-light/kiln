//! The DB query audit (2026-11-06; docs/resilience.md C5): a server builds its index migrations by
//! itself once it serves, on a new database and on one from before a migration of indexes alone (it
//! serves at once): each index made, the migration recorded once, and `/metrics` saying none is left.
//! And it takes the audit log's rows into its search by itself (`audit::keep_search`), `q` finding them.
//!
//! `cargo test -p kiln --test store_indexes`

#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use common::{Options, http, start};

/// The agents area's migration of the audit: its index, on a table a new server has.
const MIGRATION: &str = "20261106-2105_dbq_agents";
const INDEXES: [&str; 1] = ["goal_items_by_status"];

/// Until the server has built its index migrations: each of `INDEXES` there, the migration recorded,
/// and `/metrics` saying none is left. What `/metrics` said then.
fn until_built(s: &common::Server, db: &std::path::Path) -> anyhow::Result<String> {
    let c = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let made: i64 = c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1", INDEXES, |r| r.get(0))?;
        let recorded: i64 = c.query_row("SELECT COUNT(*) FROM migrations WHERE name = ?1", [MIGRATION], |r| r.get(0))?;
        let (_, metrics) = http(s.port, "GET", "/metrics", &s.admin, None)?;
        let left = metrics.lines().find_map(|l| l.strip_prefix("kiln_store_indexes_left ")).map(str::to_owned);
        if made == 1 && recorded == 1 && left.as_deref() == Some("0") {
            return Ok(metrics);
        }
        anyhow::ensure!(Instant::now() < deadline, "built {made} of 1, recorded {recorded}, left {left:?}:\n{}", s.log());
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
fn dbq_r4_a_server_builds_its_index_migrations_once_it_serves() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let options = Options { agent: false, ci: "enabled = false", extra: "" };
    let db = dir.path().join("data").join("kiln.db");
    // A new server's: built by it as it serves, its tables empty.
    let first = start(dir.path(), &options)?;
    until_built(&first, &db)?;
    drop(first);
    // Then as a build from before the migration left it: its indexes not made, it not recorded.
    {
        let c = rusqlite::Connection::open(&db)?;
        for index in INDEXES {
            c.execute_batch(&format!("DROP INDEX IF EXISTS {index}"))?;
        }
        anyhow::ensure!(c.execute("DELETE FROM migrations WHERE name = ?1", [MIGRATION])? == 1, "{MIGRATION} was recorded as the first server built it");
        // Rows of the audit log its search doesn't hold yet.
        for n in ["one", "two", "three"] {
            c.execute("INSERT INTO audit2 (ts, principal, action, result, detail) VALUES (CAST(strftime('%s', 'now') AS INTEGER), 'admin', 'POST /api/x', '200', ?1)", [format!("dbq-r4 search {n}")])?;
        }
    }
    let s = start(dir.path(), &options)?;
    let (code, body) = http(s.port, "GET", "/readyz", "", None)?;
    anyhow::ensure!(code == 200, "ready as it serves: {code} {body}");
    let metrics = until_built(&s, &db)?;
    anyhow::ensure!(metrics.lines().any(|l| l == "kiln_store_indexes_built_total 1"), "built since it started:\n{metrics}");
    let c = rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (held, newest): (i64, i64) = c.query_row("SELECT (SELECT newest FROM audit2_indexed), (SELECT max(id) FROM audit2)", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if held == newest {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the search holds up to {held}, the log {newest}:\n{}", s.log());
        std::thread::sleep(Duration::from_millis(250));
    }
    let (code, body) = http(s.port, "GET", "/api/audit?q=search%20two", &s.admin, None)?;
    let rows: serde_json::Value = serde_json::from_str(&body)?;
    let found: Vec<&str> = rows.as_array().map(|r| r.iter().filter_map(|v| v.get("detail").and_then(serde_json::Value::as_str)).collect()).unwrap_or_default();
    anyhow::ensure!(code == 200 && found == ["dbq-r4 search two"], "q finds the row it holds: {code} {body}");
    Ok(())
}

/// The harness's servers are their own (2026-11-06: a test's server, its `KILN_DATA_DIR` a runner's
/// session's, opened production's kiln.db): `common::kiln()` passes none of the test's `KILN_*` on.
/// Shown in a child of this test whose `KILN_DATA_DIR` is a decoy: the server it starts keeps its
/// kiln.db in its own data dir, and nothing is written to the decoy.
#[test]
fn harness_servers_never_see_the_parents_kiln_data_dir() -> anyhow::Result<()> {
    let removed = common::kiln().get_envs().any(|(k, v)| k == "KILN_DATA_DIR" && v.is_none());
    anyhow::ensure!(removed, "the harness's kiln is given no KILN_DATA_DIR");
    let decoy = tempfile::tempdir()?;
    let out = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "a_harness_server_beside_a_decoy_data_dir", "--ignored", "--test-threads", "1"])
        .env("KILN_DATA_DIR", decoy.path())
        .env("KILN_TEST_DECOY_DATA_DIR", decoy.path())
        .output()?;
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    anyhow::ensure!(out.status.success() && said.contains("1 passed"), "the child: {said}");
    let written: Vec<_> = std::fs::read_dir(decoy.path())?.flatten().map(|e| e.file_name()).collect();
    anyhow::ensure!(written.is_empty(), "the decoy data dir was written: {written:?}");
    Ok(())
}

/// [`harness_servers_never_see_the_parents_kiln_data_dir`]'s child: starts nothing unless its
/// `KILN_DATA_DIR` is the decoy its parent made.
#[test]
#[ignore = "run by harness_servers_never_see_the_parents_kiln_data_dir, in the environment it gives it"]
fn a_harness_server_beside_a_decoy_data_dir() -> anyhow::Result<()> {
    let (Some(data), Some(decoy)) = (std::env::var_os("KILN_DATA_DIR"), std::env::var_os("KILN_TEST_DECOY_DATA_DIR")) else {
        anyhow::bail!("run by its parent test alone: it starts nothing without the decoy");
    };
    anyhow::ensure!(data == decoy, "KILN_DATA_DIR isn't the parent's decoy: nothing started");
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: false, ci: "enabled = false", extra: "" })?;
    let (code, body) = http(s.port, "GET", "/readyz", "", None)?;
    anyhow::ensure!(code == 200, "serving: {code} {body}");
    anyhow::ensure!(dir.path().join("data").join("kiln.db").is_file(), "its kiln.db in its own data dir:\n{}", s.log());
    Ok(())
}
