//! Observe's store after a restart (docs/observe.md §4.5, §5): the server replays its log's entries
//! since the last cut as it opens, and until the replica has applied them all a read sees only the
//! first of them. Found by the comparison against ClickHouse (bench/clickhouse, 2026-10-29): a day's
//! points read 23% at 330 ms after a start and whole at 1.7 s, `/readyz` answering 200 throughout.
//! Now a query is refused (503) until the store has replayed, never answered short, and readiness
//! waits for it too.

mod common;

use std::time::{Duration, Instant};

use serde_json::json;

use common::{Fleet, at, http, now_ms};

const SERIES: i64 = 200;
const SECONDS: i64 = 3 * 3600;

/// The points the store holds of `replay.v` over the test's span, or the answer's status if refused.
fn held(f: &Fleet, from_s: i64) -> anyhow::Result<Result<i64, (u16, String)>> {
    let (from, to) = (from_s * 1000, (from_s + SECONDS) * 1000);
    let body = json!({ "query": format!("sum:replay.v{{*}}.rollup(count, {SECONDS})"), "from": from, "to": to, "step": SECONDS * 1000 });
    let (code, text) = http(f.http, "POST", "/api/observe/v1/query", &f.admin, Some(&body))?;
    if code != 200 {
        return Ok(Err((code, text)));
    }
    let v: serde_json::Value = serde_json::from_str(&text)?;
    let n: f64 = at(&v, &["series"]).as_array().map(|s| s.iter().flat_map(|x| at(x, &["values"]).as_array().cloned().unwrap_or_default()).filter_map(|x| x.as_f64()).sum()).unwrap_or(0.0);
    Ok(Ok(n as i64))
}

#[test]
fn after_a_restart_a_query_is_whole_or_refused_never_short() -> anyhow::Result<()> {
    let mut f = Fleet::new()?;
    // 200 series, a point a second each for three hours before now (2,160,000 points), in requests
    // of five minutes of every series; the span's start on a multiple of its own length, as the
    // query's one bucket is.
    let from_s = (now_ms() / 1000 - SECONDS - 600).div_euclid(SECONDS) * SECONDS;
    for chunk in (0..SECONDS).step_by(300) {
        let series: Vec<serde_json::Value> = (0..SERIES)
            .map(|s| json!({ "metric": "replay.v", "type": "gauge", "tags": [format!("s:{s}")], "points": (chunk..(chunk + 300).min(SECONDS)).map(|i| json!([from_s + i, 1.0])).collect::<Vec<_>>() }))
            .collect();
        let (code, text) = http(f.http, "POST", "/api/observe/v1/series", &f.admin, Some(&json!({ "series": series })))?;
        anyhow::ensure!((200..300).contains(&code), "writing points: {code} {text}");
    }
    let whole = SERIES * SECONDS;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if held(&f, from_s)? == Ok(whole) {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the points weren't all readable in 120 s: {:?}", held(&f, from_s)?);
        std::thread::sleep(Duration::from_millis(100));
    }

    // Killed (as a crash or a host's restart would) and started again on its data: it answers
    // /healthz as soon as it listens, and replays its log from then.
    f.kill_server();
    f.start_server()?;

    // From the moment it answers: every query whole or refused, and once readiness says ready, the
    // next query whole.
    let (mut refused, mut not_ready) = (0u32, 0u32);
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let ready = http(f.http, "GET", "/readyz", "", None)?.0 == 200;
        if !ready {
            not_ready += 1;
        }
        match held(&f, from_s)? {
            Ok(n) => {
                anyhow::ensure!(n == whole, "a query after the restart answered {n} of {whole} points, not refused");
                break;
            }
            Err((503, _)) => {
                anyhow::ensure!(!ready, "ready, yet a query was refused as replaying");
                refused += 1;
            }
            Err((code, text)) => anyhow::bail!("a query after the restart: {code} {text}"),
        }
        anyhow::ensure!(Instant::now() < deadline, "still replaying 120 s after the restart");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The window this test is for: the store was still replaying when the first query came (were it
    // not, nothing above was checked).
    anyhow::ensure!(refused > 0 && not_ready > 0, "no query came while the store replayed (refused {refused}, not ready {not_ready}): write more points");
    Ok(())
}
