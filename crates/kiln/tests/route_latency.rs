//! Goal 1, plan 01a1a59c2a68 step 2: each route class's histograms on a real server's /metrics
//! (`kiln::route_latency`): a request moves its own class's, how long it took and how long it
//! waited before its handler started.

mod common;

use std::collections::BTreeMap;

use common::Options;

/// Each class's `kiln_route_seconds` and `kiln_route_wait_seconds` counts, from /metrics.
fn counts(port: u16, admin: &str) -> anyhow::Result<BTreeMap<(String, String), u64>> {
    let (code, text) = common::http(port, "GET", "/metrics", admin, None)?;
    anyhow::ensure!(code == 200, "/metrics: {code}");
    let mut out = BTreeMap::new();
    for line in text.lines() {
        for family in ["kiln_route_seconds", "kiln_route_wait_seconds"] {
            let Some(rest) = line.strip_prefix(family).and_then(|r| r.strip_prefix("_count{class=\"")) else { continue };
            let Some((class, n)) = rest.split_once("\"} ") else { continue };
            out.insert((family.to_owned(), class.to_owned()), n.trim().parse()?);
        }
    }
    anyhow::ensure!(out.len() == 10, "five classes, two families each: {out:?}");
    Ok(out)
}

#[test]
fn g1_s2_a_request_moves_its_own_class() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = common::start(dir.path(), &Options::default())?;
    let before = counts(s.port, &s.admin)?;
    // A page, an API change, a runner's route, and an API read; each answered, whatever it says.
    for (method, path) in [("GET", "/agents"), ("POST", "/api/harness/sessions/none/command"), ("GET", "/api/sync/blobs/00"), ("GET", "/api/harness/sessions")] {
        let (code, _) = common::http(s.port, method, path, &s.admin, Some(&serde_json::json!({})))?;
        anyhow::ensure!(code > 0, "{method} {path}: no answer");
    }
    let after = counts(s.port, &s.admin)?;
    let moved = |family: &str, class: &str| {
        let key = (family.to_owned(), class.to_owned());
        after.get(&key).copied().unwrap_or(0).saturating_sub(before.get(&key).copied().unwrap_or(0))
    };
    for family in ["kiln_route_seconds", "kiln_route_wait_seconds"] {
        anyhow::ensure!(moved(family, "page") == 1, "{family} page moved {}", moved(family, "page"));
        anyhow::ensure!(moved(family, "api_write") == 1, "{family} api_write moved {}", moved(family, "api_write"));
        anyhow::ensure!(moved(family, "runner") == 1, "{family} runner moved {}", moved(family, "runner"));
        // The read, and the first /metrics (timed once it answered); the server's own looks may add.
        anyhow::ensure!(moved(family, "api_read") >= 2, "{family} api_read moved {}", moved(family, "api_read"));
        anyhow::ensure!(moved(family, "websocket") == 0, "{family} websocket moved {}", moved(family, "websocket"));
    }
    Ok(())
}
