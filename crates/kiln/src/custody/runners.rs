//! Runners' custody records, taken off the hub's loop (docs/ci.md W27-R08; goal 9, plan 01a1a87b2cec step 3). Each record
//! a runner process sends (`crate::proto::AgentMsg::Custody`) is handed by the hub's loop, its JSON not yet read
//! (`crate::wire::payload::Later`), to the one task that reads, checks and keeps runners' records ([`intake_loop`]), which
//! takes each in turn, in the order they came: every record kept or refused as it was ([`super::on_runner`]), none let go.
//! The hub's loop read each into a JSON tree on its shard (train 16's lab, g9p16: its polls labeled `runner
//! AgentMsg::Custody` held the shard to 0.48 ms on the CPU) and started a task of its own for it, so records a runner sent
//! one after another were checked and kept in whatever order their tasks ended.

use std::sync::Weak;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::hub::Hub;
use crate::instances::PeerId;

/// The bytes of runners' custody records waiting for the intake, at the most: as many as the hub's queue of runner events
/// they came off holds (`crate::net::EVENTS_BYTES`, one connection's receive window). At the bound the hub's loop waits,
/// and behind it the net thread and QUIC's flow control: no record is let go.
pub const RECORDS_BYTES: usize = crate::net::EVENTS_BYTES;

/// A custody record runner process `peer` sent, as the hub's loop hands it to the intake: the machine it's of, the
/// process it came over as that was named when it came (none: one the server doesn't know), and the record, read where
/// it's taken.
pub struct Sent {
    pub peer: PeerId,
    pub machine: String,
    pub instance: Option<String>,
    pub record: crate::wire::payload::Later,
}

impl crate::memory::queue::Weigh for Sent {
    /// Its place, the names it carries, and its record's text.
    fn weight(&self) -> usize {
        size_of::<Sent>().saturating_add(self.machine.capacity()).saturating_add(self.instance.as_ref().map_or(0, String::capacity)).saturating_add(self.record.bytes())
    }
}

/// Runners' custody records on their way to the one task that takes them ([`intake_loop`]): what the hub's loop hands it,
/// in the order it came, and that task's end of it, for the first to ask ([`Intake::starting`]).
pub struct Intake {
    tx: crate::memory::queue::Sender<Sent>,
    rx: crate::memory::queue::Receiver<Sent>,
    started: AtomicBool,
}

impl Default for Intake {
    fn default() -> Self {
        let (tx, rx) = crate::memory::queue::bounded(&crate::memory::owners::CUSTODY_RECORDS, RECORDS_BYTES);
        Self { tx, rx, started: AtomicBool::new(false) }
    }
}

impl Intake {
    /// What its task takes, to the first caller only: its task starts then, and what was handed it before waits for it.
    pub fn starting(&self) -> Option<crate::memory::queue::Receiver<Sent>> {
        (!self.started.swap(true, Ordering::AcqRel)).then(|| self.rx.clone())
    }

    /// `sent`, taken in its turn: waits while the intake is [`RECORDS_BYTES`] behind, so none is let go. False once its task
    /// has stopped (the hub's going).
    pub async fn send(&self, sent: Sent) -> bool {
        self.tx.send(sent).await.is_ok()
    }
}

/// The intake's one task: each record read off the shards (on the compute pool), then checked and kept ([`super::on_runner`]),
/// one at a time, in the order they came; one that can't be read refused to its runner and noted, as it was where its
/// frame was read. It holds the hub weakly: once the hub goes, so does this.
pub async fn intake_loop(hub: Weak<Hub>, rx: crate::memory::queue::Receiver<Sent>) {
    while let Ok(Sent { peer, machine, instance, record }) = rx.recv().await {
        let Some(hub) = hub.upgrade() else { return };
        match crate::rt::compute(move || record.read()).await {
            Ok(Ok(bundle)) => super::on_runner(&hub, &machine, instance, bundle).await,
            Ok(Err(e)) => hub.unreadable_payload(peer, e, true).await,
            Err(e) => tracing::warn!(target: "kiln.custody", %machine, error = %format!("{e:#}"), "a runner's custody record couldn't be read off the shards"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::hub::tests::{hear, hub_with_runner};
    use crate::proto::{AgentMsg, CAP_UNREAD, CAP_WIRE, LANE_TERM, ServerMsg};
    use crate::wire::unread::{Code, Wire};

    /// Goal 9, plan 01a1a87b2cec step 3: a runner's custody records are read, checked and kept off the hub's loop by custody's
    /// intake, one at a time in the order the runner sent them, each taken as having come over its process's connection;
    /// one that isn't JSON is refused to its runner and noted, as where its frame was read; and the intake's task is one,
    /// started with the first record.
    #[test]
    fn g9_s3_a_runners_custody_records_are_taken_off_the_loop_in_the_order_sent() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = hub_with_runner(crate::rt::clock(), "fedora", CAP_UNREAD | CAP_WIRE).await?;
            anyhow::ensure!(!hub.custody_in.started.load(std::sync::atomic::Ordering::Acquire), "no task before the first record");
            // Records a runner process says it refused (W27-R08's hop between machines), each kept as a refusal, in turn.
            const SENT: usize = 12;
            for n in 0..SENT {
                let json = serde_json::json!({ "refused_hop": { "what": format!("run {n}"), "from": "box-b", "why": "its digest differs" } }).to_string();
                hear(&hub, 1, LANE_TERM, &AgentMsg::Custody { json }).await?;
            }
            anyhow::ensure!(hub.custody_intake().starting().is_none(), "one task takes them");
            hear(&hub, 1, LANE_TERM, &AgentMsg::Custody { json: "{\"refused_hop\":".into() }).await?;
            let deadline = Instant::now() + Duration::from_secs(30);
            let kept = loop {
                let rows = crate::custody::refusals(&hub.store, 100).await?;
                if rows.len() >= SENT {
                    break rows;
                }
                anyhow::ensure!(Instant::now() < deadline, "{} of {SENT} records kept in 30 s", rows.len());
                crate::rt::sleep(Duration::from_millis(5)).await;
            };
            // Newest first: in the order sent, oldest first.
            let why: Vec<&str> = kept.iter().rev().filter_map(|r| r.get("why").and_then(serde_json::Value::as_str)).collect();
            let sent: Vec<String> = (0..SENT).map(|n| format!("run {n} from box-b: its digest differs")).collect();
            assert_eq!(why, sent, "kept in the order sent");
            anyhow::ensure!(kept.iter().all(|r| r.get("came_from").and_then(serde_json::Value::as_str).is_some_and(|c| c.starts_with("runner fedora (instance "))), "taken as having come over its process's connection: {kept:?}");
            // The one that isn't JSON: its runner told (it takes word of it), as when its frame's read refused it.
            let told = || hub.sent.lock().iter().any(|(p, m)| matches!(m, ServerMsg::Unread { wire: Wire::RunnerPayload, what, code: Code::Malformed, .. } if *p == 1 && what == "Custody"));
            while !told() {
                anyhow::ensure!(Instant::now() < deadline, "the runner wasn't told its record couldn't be read: {:?}", hub.sent.lock());
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            Ok(())
        })
    }
}
