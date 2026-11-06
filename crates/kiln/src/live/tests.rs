//! The protocol's endpoint, the pure parts (goal 3 of the browser protocol program, plan
//! `01a1a832eb2f711f9c6f51d88effe89d` step 2): the version a handshake takes, and the topics' owner.
//! The endpoint end to end is `tests/live.rs`.

use kiln_bp::frames::{Cursor, Delta, Topic};
use kiln_bp::topic::Start;

use super::owner::Topics;
use super::{chosen, token, versions};
use crate::wire::events::{Queue, ServerEvent};

/// bp-g3 step 2: the newest version both speak, from the page's list; none this build speaks is none.
#[test]
fn bp_g3_s2_the_handshake_takes_the_newest_version_both_speak() {
    assert_eq!(chosen(&token(kiln_bp::VERSION)), Some(kiln_bp::VERSION));
    assert_eq!(chosen(&format!("chat, {} ,x", token(kiln_bp::VERSION))), Some(kiln_bp::VERSION));
    assert_eq!(chosen(&token(kiln_bp::VERSION + 1)), None, "a newer one this build doesn't speak");
    assert_eq!(chosen("kiln-bp.x, kiln-bp., chat"), None);
    assert_eq!(chosen(""), None);
    assert_eq!(versions().first(), Some(&token(kiln_bp::VERSION)));
}

/// bp-g3 step 2: the owner makes the queue's counts a change once; a subscription from nothing gets
/// the record as a snapshot, one from the cursor before the change only the change, and one from a
/// cursor of another incarnation a snapshot again.
#[test]
fn bp_g3_s2_the_owner_starts_subscriptions_from_their_cursors() -> anyhow::Result<()> {
    let mut t = Topics::new(77);
    let queue = Topic { kind: crate::bp::kind::QUEUE, key: String::new() };
    assert!(matches!(t.start(&queue, None), Start::Snapshot { ref items, .. } if items.is_empty()), "an empty topic");
    let changes = t.event(&ServerEvent::Queue(Queue::Counts { queued: 3, running: 1 }));
    assert_eq!(changes.len(), 1, "one change: {changes:?}");
    let c = changes.first().ok_or_else(|| anyhow::anyhow!("no change"))?;
    assert_eq!(c.topic, queue);
    assert!(matches!(*c.published.delta, Delta::Set { .. }));
    // The same counts again change nothing.
    assert!(t.event(&ServerEvent::Queue(Queue::Counts { queued: 3, running: 1 })).is_empty());
    match t.start(&queue, None) {
        Start::Snapshot { cursor, items } => {
            assert_eq!(cursor, Cursor { born: 77, rev: 1 });
            assert_eq!(items.len(), 1);
        }
        other => anyhow::bail!("a snapshot: {other:?}"),
    }
    match t.start(&queue, Some(&Cursor { born: 77, rev: 0 })) {
        Start::Resume { changes, .. } => assert_eq!(changes.len(), 1),
        other => anyhow::bail!("resumed: {other:?}"),
    }
    assert!(matches!(t.start(&queue, Some(&Cursor { born: 5, rev: 0 })), Start::Snapshot { .. }), "another incarnation's cursor");
    let unknown = Topic { kind: 9_999, key: String::new() };
    assert!(matches!(t.start(&unknown, None), Start::Refuse { .. }));
    Ok(())
}

/// bp-g3 step 3: a socket's numbers read as JSON without a lock.
#[test]
fn bp_g3_s3_a_sockets_numbers_read_whole() {
    let s = super::Stats { id: 9, who: "admin".into(), ..super::Stats::default() };
    s.unsent.store(1_200, std::sync::atomic::Ordering::Relaxed);
    let v = s.json();
    assert_eq!(v.get("id").and_then(serde_json::Value::as_u64), Some(9));
    assert_eq!(v.get("unsent").and_then(serde_json::Value::as_u64), Some(1_200));
    assert_eq!(v.get("queued").and_then(serde_json::Value::as_array).map(Vec::len), Some(5));
    let mut out = String::new();
    super::render(&mut out);
    assert!(out.contains("kiln_proto_sockets_opened_total"));
}

/// bp-g3 step 2: a request's read runs through the API's own routes as the caller and comes back whole;
/// the protocol's own endpoint and paths outside the API are refused.
#[test]
fn bp_g3_s2_a_read_runs_through_the_apis_routes() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let read = crate::rt::timeout(std::time::Duration::from_secs(20), crate::api::live_read(std::sync::Arc::clone(&hub), crate::authz::Caller::owner(), "/api/system", 1 << 20)).await;
        let (status, body) = read.map_err(|_| anyhow::anyhow!("the read took past 20 s"))?;
        anyhow::ensure!(status == 200, "{status}: {}", String::from_utf8_lossy(&body));
        let v: serde_json::Value = serde_json::from_slice(&body)?;
        anyhow::ensure!(v.get("runtime").is_some(), "{v}");
        let (status, _) = crate::api::live_read(std::sync::Arc::clone(&hub), crate::authz::Caller::owner(), "/api/live/sockets", 1 << 20).await;
        anyhow::ensure!(status == 404, "the protocol's own endpoint: {status}");
        let (status, _) = crate::api::live_read(hub, crate::authz::Caller::owner(), "/healthz", 1 << 20).await;
        anyhow::ensure!(status == 404, "outside the API: {status}");
        Ok(())
    })
}
