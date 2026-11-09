//! Kiln's own records, made for the adapters' tests and measurements.

use serde_json::{Value, json};

pub fn run(id: &str, repo: &str, status: crate::store::Status) -> crate::store::Run {
    crate::store::Run {
        id: id.into(),
        repo_id: format!("id-{repo}"),
        repo_name: repo.into(),
        number: 7,
        pipeline: "ci".into(),
        event: "push".into(),
        branch: "main".into(),
        sha: "0123456789abcdef0123456789abcdef01234567".into(),
        message: "bp-g2: the protocol".into(),
        author: "ada".into(),
        status,
        created_at: 1_793_825_000_000,
        started_at: None,
        finished_at: None,
        error: None,
        concurrency_group: None,
    }
}

pub fn job(id: &str, run: &str, steps: &[(&str, &str, Option<i64>)]) -> Option<crate::store::Job> {
    let steps: Vec<Value> = steps.iter().enumerate().map(|(i, (name, status, fin))| json!({"idx": i, "name": name, "status": status, "started_at": 1_793_825_000_000i64, "finished_at": fin, "exit_code": fin.map(|_| 0)})).collect();
    serde_json::from_value(json!({"id": id, "run_id": run, "key": "test", "display": "test (linux)", "status": "running", "labels": ["linux"], "needs": [], "agent": "fedora", "executor": "host", "attempt": 1, "queued_at": 1_793_825_000_000i64, "started_at": 1_793_825_000_100i64, "finished_at": null, "exit_code": null, "error": null, "peak_rss": null, "cpu_seconds": null, "steps": steps})).ok()
}

pub fn agent(name: &str, busy: u32, cpu: f32, at: i64) -> crate::hub::AgentLive {
    crate::hub::AgentLive {
        label: None,
        name: name.into(),
        peer: None,
        sampled_at: None,
        labels: vec!["linux".into()],
        slots: 8,
        hostname: name.into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        version: "0.2.0".into(),
        ephemeral: false,
        connected: true,
        last_seen: at,
        enrolled_at: 0,
        running: Vec::new(),
        busy,
        draining: false,
        sample: Some(crate::metrics::HostSample { ts_ms: at, cpu_pct: cpu, cpus: 32, load1: cpu / 10.0, mem_total: 128 << 30, mem_used: (40 << 30) + at as u64 % 1000, disk_total: 8 << 40, disk_used: 7 << 40, net_rx: at as u64 * 3, net_tx: at as u64 * 2, procs: 900 }),
        battery: None,
        dropped: Vec::new(),
        dropped_toward: Vec::new(),
        refused: None,
        terminal: true,
        outpost: false,
        dedicated_job: None,
        exclusive: false,
        held: Vec::new(),
        lost_at: None,
        facts: None,
        instances: Vec::new(),
        transport: None,
        skew: crate::skew::Skew::default(),
        stale: false,
    }
}

