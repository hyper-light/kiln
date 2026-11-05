//! The server's side of observe's lane (`docs/observe.md` §5): what the hub hands the group and
//! sends for it, and the host samples runners report, written as metrics. The hub calls these at
//! the points sync's own hooks sit.

use std::sync::Arc;

use kiln_obs::batch::{Kind, SeriesKey};

use super::wire;
use crate::hub::Hub;
use crate::metrics::HostSample;
use crate::net::Kind as Lane;
use crate::proto::{self, ServerMsg};

/// Sends what the group says to each machine's replicating runner process, on observe's lane.
pub async fn send_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    let Some(o) = super::api::get() else { return Ok(()) };
    send_each(&hub, &o.outgoing()).await;
    Ok(())
}

/// What `out` holds, each sent in its turn, the shard given up between them once this loop's turn would pass its
/// quantum (goal 9, plan 01a1a87b2cec step 3): its queue and the net thread's take a message without waiting, so a
/// replica catching up (up to thousands waiting, each up to half a mebibyte copied as it's encoded) was sent in one
/// poll.
async fn send_each(hub: &Hub, out: &async_channel::Receiver<Box<super::owner::Out>>) {
    let mut turns = crate::rt::Turns::new();
    while let Ok(m) = out.recv().await {
        let peer = hub.instances.load().newest_with(&m.machine, proto::CAP_OBSERVE);
        if let Some(peer) = peer {
            let msg = ServerMsg::Observe { json: wire::encode(&m.msg), bytes: m.bytes };
            if let Ok(b) = proto::encode(&msg) {
                hub.net.send(peer, proto::LANE_OBSERVE, Lane::Control, b).await;
            }
        }
        turns.after().await;
    }
}

/// What a machine's runner said on observe's lane.
pub fn heard(machine: &str, msg: wire::Msg, bytes: Vec<u8>) {
    if let Some(o) = super::api::get() {
        o.heard(machine, msg, bytes);
    }
}

/// A runner process of `machine` that replicates said so: the newest such process replicates, and
/// every other one lets go of the machine's replica.
pub async fn announced(hub: &Hub, machine: &str) {
    let Some(o) = super::api::get() else { return };
    let (newest, others) = {
        let inst = hub.instances.load();
        let newest = inst.newest_with(machine, proto::CAP_OBSERVE);
        let others: Vec<_> = inst.of(machine).into_iter().filter(|p| Some(*p) != newest && inst.get(*p).is_some_and(|i| i.caps & proto::CAP_OBSERVE != 0)).collect();
        (newest, others)
    };
    let release = ServerMsg::Observe { json: wire::encode(&wire::Msg::Release), bytes: Vec::new() };
    if let Ok(b) = proto::encode(&release) {
        for p in others {
            hub.net.send(p, proto::LANE_OBSERVE, Lane::Control, b.clone()).await;
        }
    }
    o.up(machine, newest.is_some());
}

/// A runner process of `machine` went: the group follows the newest that is left, if any.
pub fn lost(hub: &Hub, machine: &str) {
    if let Some(o) = super::api::get() {
        o.up(machine, hub.instances.load().newest_with(machine, proto::CAP_OBSERVE).is_some());
    }
}

/// A host sample, as metrics of its machine (`host:<machine>`), in Datadog's names for them
/// (docs.datadoghq.com "System Check": `system.cpu.*`, `system.mem.*`, `system.disk.*`,
/// `system.net.*`, `system.load.*`), so dashboards and monitors written for them read here.
pub fn host_sample(machine: &str, s: &HostSample) {
    let Some(o) = super::api::get() else { return };
    let ts = s.ts_ms;
    let host = [("host", machine)];
    #[allow(clippy::cast_precision_loss)]
    let points: [(&str, Kind, f64); 10] = [
        ("system.cpu.pct_usage", Kind::Gauge, f64::from(s.cpu_pct)),
        ("system.cpu.num_cores", Kind::Gauge, f64::from(s.cpus)),
        ("system.load.1", Kind::Gauge, f64::from(s.load1)),
        ("system.mem.total", Kind::Gauge, s.mem_total as f64),
        ("system.mem.used", Kind::Gauge, s.mem_used as f64),
        ("system.disk.total", Kind::Gauge, s.disk_total as f64),
        ("system.disk.used", Kind::Gauge, s.disk_used as f64),
        ("system.net.bytes_rcvd", Kind::Counter, s.net_rx as f64),
        ("system.net.bytes_sent", Kind::Counter, s.net_tx as f64),
        ("system.processes.number", Kind::Gauge, f64::from(s.procs)),
    ];
    let series: Vec<(SeriesKey, Vec<(i64, f64)>)> = points.iter().filter_map(|&(metric, kind, v)| SeriesKey::new(metric, host, kind).ok().map(|k| (k, vec![(ts, v)]))).collect();
    o.write(series);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use super::*;

    /// Goal 9, plan 01a1a87b2cec step 3: observe's sender, with thousands of messages waiting (a replica catching up),
    /// gives its shard up between them once its turn would pass the quantum, and takes every one: another task on its
    /// shard that wants it runs while it sends. Its queue and the net thread's take a message without waiting, so it
    /// took them all in one poll.
    #[test]
    fn g9_s3_a_backlog_is_sent_in_turns() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let (tx, rx) = async_channel::unbounded();
            for n in 0..20_000u32 {
                tx.try_send(Box::new(super::super::owner::Out { machine: "away".into(), msg: wire::Msg::Release, bytes: n.to_be_bytes().to_vec() })).map_err(|_| anyhow::anyhow!("the queue refused one"))?;
            }
            drop(tx);
            let (ran, stop) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
            let other = {
                let (ran, stop) = (Arc::clone(&ran), Arc::clone(&stop));
                crate::rt::spawn(async move {
                    while !stop.load(Ordering::SeqCst) {
                        ran.fetch_add(1, Ordering::SeqCst);
                        hyper_rt::futures::yield_now().await;
                    }
                })
            };
            let before = ran.load(Ordering::SeqCst);
            send_each(&hub, &rx).await;
            let during = ran.load(Ordering::SeqCst) - before;
            stop.store(true, Ordering::SeqCst);
            let _ = other.await;
            anyhow::ensure!(during > 0, "the other task never ran while the backlog was sent");
            anyhow::ensure!(rx.is_empty(), "every one taken");
            Ok(())
        })
    }
}
