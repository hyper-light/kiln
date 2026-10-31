//! The conversation stores' board on the server (`crate::convo::board`; docs/conversations.md,
//! "Telemetry"): what the server sees of every store, sampled every few seconds into each store's
//! series, its notes and `/metrics`, and answered to the System page.

use std::sync::Arc;

use futures::FutureExt as _;

use crate::convo::board::{Machine, SAMPLE_EVERY, Seen, Status};
use crate::hub::Hub;
use crate::rt::Interval;

/// What the server sees of every store now.
pub fn seen(hub: &Hub) -> Seen {
    let r = &hub.harness;
    let machines = hub.agents.lock().values().filter(|a| !a.ephemeral && !a.outpost).map(|a| Machine { name: a.name.clone(), connected: a.connected, version: a.version.clone() }).collect();
    Seen {
        now: crate::util::now_ms(),
        server: r.convo.as_ref().map(crate::convo::store::Store::report),
        version: crate::convo::telemetry::version(),
        machines,
        restore: serde_json::to_value(r.restore_view()).unwrap_or_default(),
    }
}

/// The board as it stands (`GET /api/conversations/status`).
pub fn status(hub: &Hub) -> Status {
    hub.harness.board.status(&seen(hub))
}

/// Every `SAMPLE_EVERY`: each store's series takes a point, what changed is noted, and `/metrics`
/// says where each store stands.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    let mut tick = Interval::new(SAMPLE_EVERY);
    loop {
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            () = tick.tick().fuse() => {}
        }
        // U15: observe operation counters only. Replication belongs to Amphora; there is
        // no application peer cursor polling or custom feed/mirror sampling here.
        let status = hub.harness.board.sample(&seen(&hub));
        crate::metrics::set_convo(&status);
    }
}
