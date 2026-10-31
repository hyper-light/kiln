//! The server's side of observe's lane (`docs/observe.md` §5): what the hub hands the group and
//! sends for it, and the host samples runners report, written as metrics. The hub calls these at
//! the points sync's own hooks sit.

use std::sync::Arc;
use std::time::Instant;

use futures::future::{BoxFuture, Fuse, FusedFuture as _};
use futures::FutureExt as _;

use kiln_obs::batch::{Kind, SeriesKey};

use super::wire;
use crate::hub::Hub;
use crate::metrics::HostSample;
use crate::net::Kind as Lane;
use crate::proto::{self, ServerMsg};

/// The existing transport owner also reconciles canonical listener desired state. Its Quiver
/// hint is lossy; the existing status cadence independently probes the small canonical revision.
/// Reads and at most one outgoing send stay concurrently polled: SQL never blocks bulk/heartbeats.
pub async fn send_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    let Some(o) = super::api::get() else { return Ok(()) };
    let kept = super::api::statsd_metadata().ok_or_else(|| anyhow::anyhow!("canonical Observe listeners not initialized before transport owner"))?;
    let out = o.outgoing();
    let mut wakes = kept.subscribe();
    let mut known = None;
    let mut requested = true;
    let mut next = Instant::now() + super::owner::STATUS_EVERY;
    let mut reading: Fuse<BoxFuture<'static, Result<Option<super::statsd::Configuration>, super::statsd::Error>>> = Fuse::terminated();
    let mut sending: Fuse<BoxFuture<'static, ()>> = Fuse::terminated();
    enum Step {
        Read(Result<Option<super::statsd::Configuration>, super::statsd::Error>),
        Wake(Result<(), crate::bus::RecvError>),
        Periodic,
        Out,
        Sent,
    }
    loop {
        if requested && reading.is_terminated() {
            requested = false;
            let kept = kept.clone();
            reading = async move { kept.refresh(known).await }.boxed().fuse();
        }
        let mut frame = None;
        let step = {
            let receiving = sending.is_terminated();
            let output = async { if receiving { out.recv().await } else { futures::future::pending().await } }.fuse();
            let wake = async { match &mut wakes { Some(wake) => wake.recv().await, None => futures::future::pending().await } }.fuse();
            let periodic = crate::rt::sleep(next.saturating_duration_since(Instant::now())).fuse();
            futures::pin_mut!(output, wake, periodic);
            futures::select! {
                result = reading => Step::Read(result),
                result = wake => Step::Wake(result),
                () = periodic => Step::Periodic,
                result = output => { frame = Some(result); Step::Out },
                () = sending => Step::Sent,
            }
        };
        match step {
            Step::Read(Ok(configuration)) => {
                if let Some(configuration) = configuration {
                    known = Some(configuration.revision);
                    super::api::apply_statsd(configuration);
                } else {
                    o.retry_statsd(); // Same revision still owed after a lost config frame.
                }
            }
            Step::Read(Err(error)) => {
                tracing::warn!(%error, "canonical Observe listener reconciliation unavailable");
                o.statsd_unavailable(error.to_string());
            }
            Step::Wake(Ok(())) => requested = true,
            Step::Wake(Err(error)) => {
                tracing::warn!(%error, "Observe metadata wake missed; reconcile canonical state");
                requested = true;
                wakes = None; // No busy-loop on a broken topic; retry subscription next cadence.
            }
            Step::Periodic => {
                next = Instant::now() + super::owner::STATUS_EVERY;
                requested = true;
                if wakes.is_none() { wakes = kept.subscribe(); }
            }
            Step::Out => {
                let Some(Ok(m)) = frame else { return Ok(()) };
                let hub = hub.clone();
                sending = async move {
                    // Retain the output item's queue lease through actual send selection/wait.
                    // Dropping this future on cancellation/restart also returns same-owner room.
                    let mut queued = m.queued;
                    let peer = hub.instances.lock().newest_with(&m.machine, proto::CAP_OBSERVE);
                    if let Some(peer) = peer {
                        let msg = ServerMsg::Observe { json: wire::encode(&m.msg), bytes: m.bytes };
                        if let Ok(b) = proto::encode(&msg) {
                            if let Some(queued) = queued.take() {
                                // Transfer before await; same net owner retains the final hold
                                // through LaneFull/late/twice, endpoint admission or definitive drop.
                                hub.net.send_held(peer, proto::LANE_OBSERVE, Lane::Control, b, crate::net::FrameHold::new(queued)).await;
                            } else {
                                hub.net.send(peer, proto::LANE_OBSERVE, Lane::Control, b).await;
                            }
                        }
                    }
                    drop(queued);
                }.boxed().fuse();
            }
            Step::Sent => {}
        }
    }
}

/// Mutable config status can acknowledge only the same authenticated connection selected for
/// Observe. Called while Hub retains Instances through the synchronous owner enqueue.
pub(crate) fn current_status(instances: &crate::instances::Instances, machine: &str, peer: hyper_transport::PeerId, origin: Option<crate::net::Connection>) -> bool {
    instances.newest_with(machine, proto::CAP_OBSERVE) == Some(peer)
        && instances.accepts_connection(peer, origin)
        && instances.get(peer).is_some_and(|instance| instance.on_connection(origin))
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
        let inst = hub.instances.lock();
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
    if let Some(kept) = super::api::statsd_metadata() { kept.changed(); }
}

/// A runner process of `machine` went: the group follows the newest that is left, if any.
pub fn lost(hub: &Hub, machine: &str) {
    if let Some(o) = super::api::get() {
        o.up(machine, hub.instances.lock().newest_with(machine, proto::CAP_OBSERVE).is_some());
        if let Some(kept) = super::api::statsd_metadata() { kept.changed(); }
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
    use super::*;

    /// U06/U14: a certificate-stable peer on its old TLS connection, or an older process on a
    /// still-live connection, cannot ACK configuration for the newest selected Observe owner.
    #[test]
    fn u14_listener_status_ack_requires_selected_actual_connection() {
        let hello = || proto::Hello { protocol: proto::VERSION, name: "box".into(), labels: vec![], slots: 1, hostname: "box".into(), os: "linux".into(), arch: "x86_64".into(), version: "fixture".into(), ephemeral: false, dedicated_job: None, running: vec![], terminal: true };
        let mut instances = crate::instances::Instances::default();
        let old = crate::net::Connection::new(1, 1, [1; 32]);
        let reconnected = crate::net::Connection::new(1, 2, [2; 32]);
        let newest = crate::net::Connection::new(2, 1, [3; 32]);
        instances.connected(1, 1, Some([1; 32]));
        instances.announce(1, proto::InstanceHello { id: "old".into(), created_at: 1, pid: 1 });
        instances.join(1, "box", hello(), 1);
        instances.set_build(1, "fixture".into(), proto::CAP_OBSERVE);
        assert!(current_status(&instances, "box", 1, Some(old)));
        instances.connected(1, 2, Some([2; 32]));
        assert!(!current_status(&instances, "box", 1, Some(old)));
        assert!(!current_status(&instances, "box", 1, Some(reconnected)), "new TLS cannot borrow old Hello/caps");
        instances.announce(1, proto::InstanceHello { id: "old".into(), created_at: 1, pid: 1 });
        instances.join(1, "box", hello(), 2);
        instances.set_build(1, "fixture".into(), proto::CAP_OBSERVE);
        assert!(current_status(&instances, "box", 1, Some(reconnected)));
        instances.connected(2, 1, Some([3; 32]));
        instances.announce(2, proto::InstanceHello { id: "new".into(), created_at: 2, pid: 2 });
        instances.join(2, "box", hello(), 2);
        instances.set_build(2, "fixture".into(), proto::CAP_OBSERVE);
        assert!(!current_status(&instances, "box", 1, Some(reconnected)));
        assert!(current_status(&instances, "box", 2, Some(newest)));
        assert!(!current_status(&instances, "box", 2, None));
    }
}
