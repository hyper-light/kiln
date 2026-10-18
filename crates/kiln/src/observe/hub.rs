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
    let out = o.outgoing();
    while let Ok(m) = out.recv().await {
        let peer = hub.instances.lock().newest_with(&m.machine, proto::CAP_OBSERVE);
        let Some(peer) = peer else { continue };
        let msg = ServerMsg::Observe { json: wire::encode(&m.msg), bytes: m.bytes };
        if let Ok(b) = proto::encode(&msg) {
            hub.net.send(peer, proto::LANE_OBSERVE, Lane::Control, b).await;
        }
    }
    Ok(())
}

/// What a machine's runner said on observe's lane.
pub fn heard(machine: &str, json: String, bytes: Vec<u8>) {
    if let Some(o) = super::api::get() {
        o.heard(machine, json, bytes);
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
}

/// A runner process of `machine` went: the group follows the newest that is left, if any.
pub fn lost(hub: &Hub, machine: &str) {
    if let Some(o) = super::api::get() {
        o.up(machine, hub.instances.lock().newest_with(machine, proto::CAP_OBSERVE).is_some());
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

/// This process's own figures (`kiln.process.*`, tagged with its machine and role): its memory,
/// CPU time, open files and threads, written each host sample.
pub fn process_figures(o: &super::Observe, host: &str, role: &str) {
    let p = crate::host::process_self();
    let ts = crate::util::now_ms();
    let tags = [("host", host), ("role", role)];
    #[allow(clippy::cast_precision_loss)]
    let points: [(&str, Kind, f64); 4] = [
        ("kiln.process.rss_bytes", Kind::Gauge, p.rss_bytes as f64),
        ("kiln.process.cpu_seconds", Kind::Counter, p.cpu_seconds),
        ("kiln.process.open_fds", Kind::Gauge, p.open_fds as f64),
        ("kiln.process.threads", Kind::Gauge, p.threads as f64),
    ];
    o.write(points.iter().filter_map(|&(metric, kind, v)| SeriesKey::new(metric, tags, kind).ok().map(|k| (k, vec![(ts, v)]))).collect());
}
