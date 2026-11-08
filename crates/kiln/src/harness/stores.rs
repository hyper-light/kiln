//! The conversation stores' board on the server (`crate::convo::board`; docs/conversations.md,
//! "Telemetry"): what the server sees of every store, sampled every few seconds into each store's
//! series, its notes and `/metrics`, and answered to the System page.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt as _;

use crate::convo::board::{Machine, PeerRow, SAMPLE_EVERY, Seen, Status};
use crate::hub::Hub;
use crate::rt::Interval;

/// How often the server's store's peers are read again: through where it has each runner's store,
/// those not heard from since it started too.
const PEERS_EVERY: Duration = Duration::from_secs(30);

/// What the server sees of every store now.
pub fn seen(hub: &Hub) -> Seen {
    let r = &hub.harness;
    let machines = hub.agents.load().values().filter(|a| !a.ephemeral && !a.outpost).map(|a| Machine { name: a.name.clone(), connected: a.connected, version: a.version.clone(), label: a.label.clone() }).collect();
    Seen {
        now: crate::util::now_ms(),
        server: r.convo.as_ref().map(crate::convo::store::Store::report),
        version: crate::convo::telemetry::version(),
        intake: r.feed.as_ref().map(crate::convo::feed::Intake::peers).unwrap_or_default(),
        mirrors: r.copies.as_ref().map(|m| m.feeds()).unwrap_or_default(),
        mirrors_wanted: r.copies.as_ref().map_or(0, |m| m.wanted()),
        machines,
        peers: r.board.peers(),
        restore: serde_json::to_value(r.restore_view()).unwrap_or_default(),
    }
}

/// The board as it stands (`GET /api/conversations/status`).
pub fn status(hub: &Hub) -> Status {
    hub.harness.board.status(&seen(hub))
}

/// Every `SAMPLE_EVERY`: each store's series takes a point, what changed is noted, and `/metrics`
/// says where each store stands.
#[allow(clippy::disallowed_methods)] // (e) conversation stores sampled every SAMPLE_EVERY, picked
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    let mut tick = Interval::new(SAMPLE_EVERY);
    let mut peers_at: Option<Instant> = None;
    loop {
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            () = tick.tick().fuse() => {}
        }
        if let Some(st) = hub.harness.convo.clone()
            && peers_at.is_none_or(|t| t.elapsed() >= PEERS_EVERY)
        {
            peers_at = Some(Instant::now());
            match crate::rt::blocking(move || st.peers()).await {
                Ok(Ok(rows)) => hub.harness.board.set_peers(rows.into_iter().map(|(name, rev, at)| PeerRow { name, rev, at }).collect()),
                Ok(Err(e)) | Err(e) => tracing::debug!(error = %format!("{e:#}"), "the conversation store's peers couldn't be read"),
            }
        }
        sample(&hub).await;
    }
}

/// A sample of every store ([`crate::convo::board::Board::sample`]), `/metrics` told of it, on the compute pool: what the
/// server sees of each store, each one's series and rates, its notes and the log, every few seconds on this task's shard
/// (goal 9, plan 01a1a87b2cec step 3). Waited for, so the samples keep their order.
pub(crate) async fn sample(hub: &Arc<Hub>) {
    let h = Arc::clone(hub);
    if let Err(e) = crate::rt::compute(move || crate::metrics::set_convo(&h.harness.board.sample(&seen(&h)))).await {
        tracing::debug!(error = %format!("{e:#}"), "the conversation stores weren't sampled");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 9, plan 01a1a87b2cec step 3: a sample made on the compute pool is the board's: its first says telemetry began,
    /// and the board answers that from then on.
    #[test]
    fn g9_s3_a_sample_is_made_off_the_shard() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let began = |hub: &Hub| status(hub).notes.iter().any(|n| n.text.starts_with("telemetry began"));
            anyhow::ensure!(!began(&hub), "nothing sampled yet");
            sample(&hub).await;
            anyhow::ensure!(began(&hub), "sampled: {:?}", status(&hub).notes.iter().map(|n| n.text.clone()).collect::<Vec<_>>());
            Ok(())
        })
    }
}
