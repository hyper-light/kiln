//! W27-R01 (docs/ci.md): revocations are recorded and checked. A runner forgotten has its
//! certificate revoked, with when, why and by whom; after the server restarts, the old
//! certificate is still refused; the machine enrolling again under the same name gets a new key
//! that works, and the old certificate stays refused.
//!
//! `cargo test -p kiln --test identities`

#![cfg(unix)]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, http, start};
use serde_json::Value;

const TALK: Talk = Talk { lines: 1, every_ms: 10 };

/// How many times the runner's log says the server took it: a QUIC connection alone ("connected
/// to server") comes before the TLS directory decides.
fn connections(log: &Path) -> usize {
    std::fs::read_to_string(log).unwrap_or_default().matches("server welcomed agent").count()
}

/// Whether the server takes the runner again (its log says so once more) within `within`.
fn connects_again(log: &Path, within: Duration) -> bool {
    let (seen, deadline) = (connections(log), Instant::now() + within);
    while Instant::now() < deadline {
        if connections(log) > seen {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

fn revocations(port: u16, admin: &str) -> anyhow::Result<Vec<Value>> {
    let (st, body) = http(port, "GET", "/api/identities/revocations", admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    Ok(serde_json::from_str::<Value>(&body)?.get("revocations").and_then(Value::as_array).cloned().unwrap_or_default())
}

#[test]
fn w27_r01_revocation_survives_restart_and_reenrolling_keeps_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut s = start(dir.path(), &Options::default())?;
    let state = dir.path().join("fedora-2-state");

    // Enrolled and connected; then forgotten.
    let (runner, log) = agents::runner(dir.path(), s.port, "fedora-2", None, TALK)?;
    agents::connected(s.port, &s.admin, "fedora-2", &log, Duration::from_secs(60))?;
    // Its own log has said so too (it may trail the server's list), so what follows counts anew.
    let deadline = Instant::now() + Duration::from_secs(30);
    while connections(&log) == 0 {
        anyhow::ensure!(Instant::now() < deadline, "the runner never logged its welcome");
        std::thread::sleep(Duration::from_millis(100));
    }
    let old_cert = std::fs::read_to_string(state.join("agent.pem"))?;
    let (st, body) = http(s.port, "DELETE", "/api/agents/fedora-2", &s.admin, None)?;
    anyhow::ensure!(st == 204, "forget: {st} {body}");
    let revs = revocations(s.port, &s.admin)?;
    anyhow::ensure!(revs.len() == 1, "one certificate revoked: {revs:?}");
    let rev = revs.first().cloned().unwrap_or_default();
    assert_eq!(rev.get("reason").and_then(Value::as_str), Some("runner forgotten"));
    assert_eq!(rev.get("by").and_then(Value::as_str), Some("admin"));
    assert!(rev.get("at").and_then(Value::as_i64).is_some_and(|at| at > 1_700_000_000), "{rev}");
    assert!(!connects_again(&log, Duration::from_secs(5)), "refused at once:\n{}", std::fs::read_to_string(&log)?);
    drop(runner);

    // The server restarts: the old certificate is still refused (before W27 a restart forgot it).
    s.restart()?;
    let (runner, log) = agents::runner(dir.path(), s.port, "fedora-2", None, TALK)?;
    assert!(!connects_again(&log, Duration::from_secs(8)), "the revoked certificate came back after a restart:\n{}\n{}", std::fs::read_to_string(&log)?, s.log());
    drop(runner);
    assert_eq!(revocations(s.port, &s.admin)?.len(), 1, "the revocation is kept");

    // Enrolled again under the same name: a new key, which works.
    let old_state = dir.path().join("fedora-2-old-state");
    std::fs::rename(&state, &old_state)?;
    let (_runner, log) = agents::runner(dir.path(), s.port, "fedora-2", None, TALK)?;
    agents::connected(s.port, &s.admin, "fedora-2", &log, Duration::from_secs(60))?;
    assert_ne!(std::fs::read_to_string(state.join("agent.pem"))?, old_cert, "enrolling again brings a new certificate");

    // The old certificate is still refused: a copy of the old runner, beside the new one.
    let copy = dir.path().join("fedora-2-copy-state");
    std::fs::rename(&old_state, &copy)?;
    let (_copy, copy_log) = agents::runner(dir.path(), s.port, "fedora-2-copy", None, TALK)?;
    assert!(!connects_again(&copy_log, Duration::from_secs(8)), "the old certificate connected after the name enrolled again:\n{}", std::fs::read_to_string(&copy_log)?);
    assert_eq!(revocations(s.port, &s.admin)?.len(), 1, "re-enrolling undid nothing");
    Ok(())
}
