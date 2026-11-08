//! Finished runs' observations and the learned run-time model's statistics (docs/needs.md; the
//! `w05_run_observations` migration).

use std::collections::BTreeMap;

use kiln_ci::runtime::{Mask, Model, Observation, ROWS_KEPT, Stats};
use rusqlite::{OptionalExtension as _, params};

use super::Store;

impl Store {
    /// Keeps runner `runner`'s finished runs: each once (by its id; one let go of, by the newest
    /// id let go of from that runner for that key: a runner's UUIDv7 ids rise as it makes them),
    /// folded into its key's statistics, and only its key's newest [`ROWS_KEPT`] rows kept as they
    /// came; and each that did its kind's whole work folded into its kind's speed cell on this
    /// runner (W05-R26, `run_speed_cells`). Returns how many were new, and the speed cells' runs
    /// among them (their kind and y) for the fleet's fit to take in.
    pub async fn record_runs(&self, runner: String, runs: Vec<Observation>) -> anyhow::Result<(usize, Vec<(String, f64)>)> {
        self.call(move |c| {
            let tx = c.savepoint()?;
            let now = crate::util::now_ms();
            let mut new = 0usize;
            let mut speeds = Vec::new();
            for o in &runs {
                if o.id.is_empty() || o.key.is_empty() {
                    continue;
                }
                // Folded in already and let go since: refused, so it counts once.
                let through: Option<String> = tx.query_row("SELECT through FROM run_pruned WHERE runner = ?1 AND key = ?2", params![runner, o.key], |r| r.get(0)).optional()?;
                if through.is_some_and(|t| o.id <= t) {
                    continue;
                }
                let json = serde_json::to_string(o)?;
                let inserted = tx.execute(
                    "INSERT OR IGNORE INTO run_observations (id, runner, key, kind, received_ms, wall_s, p, json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![o.id, runner, o.key, o.kind, now, o.wall_s, o.p, json],
                )?;
                if inserted == 0 {
                    continue;
                }
                new += 1;
                if let (Some(key), Some(y)) = (kiln_ci::pace::speed::key_of(o), kiln_ci::pace::speed::y_of(o)) {
                    tx.execute(
                        "INSERT INTO run_speed_cells (key, runner, n, sum, sq) VALUES (?1, ?2, 1, ?3, ?4) ON CONFLICT(key, runner) DO UPDATE SET n = n + 1, sum = sum + excluded.sum, sq = sq + excluded.sq",
                        params![key, runner, y, y * y],
                    )?;
                    speeds.push((key.to_owned(), y));
                }
                let had: Option<String> = tx.query_row("SELECT stats FROM run_model WHERE key = ?1", params![o.key], |r| r.get(0)).optional()?;
                let stats: BTreeMap<Mask, Stats> = had.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
                let mut m = Model { stats: [(o.key.clone(), stats)].into() };
                m.observe(o);
                let stats = m.stats.remove(&o.key).unwrap_or_default();
                let runs: u64 = stats.values().map(|s| s.n).sum();
                tx.execute(
                    "INSERT INTO run_model (key, stats, runs, updated_ms) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(key) DO UPDATE SET stats = excluded.stats, runs = excluded.runs, updated_ms = excluded.updated_ms",
                    params![o.key, serde_json::to_string(&stats)?, i64::try_from(runs).unwrap_or(i64::MAX), now],
                )?;
                // The key's rows past its newest ROWS_KEPT go; each runner's newest id among them is
                // remembered as seen.
                let keep = i64::try_from(ROWS_KEPT).unwrap_or(i64::MAX);
                let gone: Vec<(String, String)> = {
                    let mut q = tx.prepare("SELECT runner, MAX(id) FROM run_observations WHERE key = ?1 AND id NOT IN (SELECT id FROM run_observations WHERE key = ?1 ORDER BY received_ms DESC, id DESC LIMIT ?2) GROUP BY runner")?;
                    let rows = q.query_map(params![o.key, keep], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
                    rows.collect::<Result<_, _>>()?
                };
                for (who, through) in gone {
                    tx.execute(
                        "INSERT INTO run_pruned (runner, key, through) VALUES (?1, ?2, ?3) ON CONFLICT(runner, key) DO UPDATE SET through = MAX(through, excluded.through)",
                        params![who, o.key, through],
                    )?;
                }
                tx.execute(
                    "DELETE FROM run_observations WHERE key = ?1 AND id NOT IN (SELECT id FROM run_observations WHERE key = ?1 ORDER BY received_ms DESC, id DESC LIMIT ?2)",
                    params![o.key, keep],
                )?;
            }
            tx.commit()?;
            Ok((new, speeds))
        })
        .await
    }

    /// Every kind's speed cell on every runner (W05-R26): what the fleet's fit starts from.
    pub async fn speed_cells(&self) -> anyhow::Result<Vec<(String, String, kiln_ci::pace::speed::Cell)>> {
        self.call(|c| {
            let mut q = c.prepare("SELECT key, runner, n, sum, sq FROM run_speed_cells")?;
            let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, kiln_ci::pace::speed::Cell { n: u64::try_from(r.get::<_, i64>(2)?).unwrap_or(0), sum: r.get(3)?, sq: r.get(4)? })))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// The learned run-time model of `keys` (every key's when none are named): what the place
    /// core predicts from ([`Model::predict`]).
    pub async fn run_model(&self, keys: Option<Vec<String>>) -> anyhow::Result<Model> {
        self.call(move |c| {
            let mut m = Model::default();
            let mut put = |key: String, stats: String| {
                if let Ok(s) = serde_json::from_str::<BTreeMap<Mask, Stats>>(&stats) {
                    m.stats.insert(key, s);
                }
            };
            match keys {
                Some(keys) => {
                    let mut q = c.prepare("SELECT key, stats FROM run_model WHERE key = ?1")?;
                    for k in keys {
                        if let Some((key, stats)) = q.query_row(params![k], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).optional()? {
                            put(key, stats);
                        }
                    }
                }
                None => {
                    let mut q = c.prepare("SELECT key, stats FROM run_model")?;
                    let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
                    for row in rows {
                        let (key, stats) = row?;
                        put(key, stats);
                    }
                }
            }
            Ok(m)
        })
        .await
    }

    /// Key `key`'s rows as kept, newest first.
    pub async fn run_observations(&self, key: String) -> anyhow::Result<Vec<Observation>> {
        self.call(move |c| {
            let mut q = c.prepare("SELECT json FROM run_observations WHERE key = ?1 ORDER BY received_ms DESC, id DESC")?;
            let rows = q.query_map(params![key], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                if let Ok(o) = serde_json::from_str(&row?) {
                    out.push(o);
                }
            }
            Ok(out)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str, cpu_s: f64, idle: f64, wall_s: f64) -> Observation {
        Observation { id: id.into(), key: "cmd:k".into(), kind: "command".into(), wall_s, cpu_s: Some(cpu_s), idle_cpus: Some(idle), ..Observation::default() }
    }

    /// docs/needs.md: every run counts in the key's statistics once, however often it's sent;
    /// only the newest ROWS_KEPT rows stay; the model fits from the statistics, older rows and all.
    #[test]
    fn w05_runs_fold_into_the_model_and_rows_are_bounded() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = Store::open(&dir.path().join("kiln.db"))?;
            // wall = 1 + 2·cpu/idle, exactly.
            let runs: Vec<Observation> = (0..12).map(|i| {
                let (cpu, idle) = (f64::from(i) + 1.0, f64::from(i % 4) + 1.0);
                run(&format!("r{i:02}"), cpu, idle, 1.0 + 2.0 * cpu / idle)
            }).collect();
            assert_eq!(store.record_runs("mac".into(), runs.clone()).await?.0, 12);
            assert_eq!(store.record_runs("mac".into(), runs).await?.0, 0, "sent again: kept once");
            let kept = store.run_observations("cmd:k".into()).await?;
            assert_eq!(kept.len(), ROWS_KEPT);
            assert_eq!(kept.first().map(|o| o.id.as_str()), Some("r11"));
            let m = store.run_model(None).await?;
            assert_eq!(m.stats.get("cmd:k").and_then(|s| s.get(&0)).map(|s| s.n), Some(12));
            let p = m.predict("cmd:k", &kiln_ci::facts::RunnerFacts::default(), &kiln_ci::runtime::Now { idle_cpus: Some(2.0), ..Default::default() });
            let mean_cpu = 6.5;
            assert!(p.is_some_and(|p| (p.mean_s - (1.0 + 2.0 * mean_cpu / 2.0)).abs() < 1e-6), "{p:?}");
            assert_eq!(store.run_model(Some(vec!["other".into()])).await?.stats.len(), 0);
            Ok(())
        })
    }

    /// W05-R26: a run that did its kind's whole work is folded into its kind's speed cell on its
    /// runner once, however often it's sent; a command without a work key, or a failed run, isn't.
    #[test]
    fn w05_r26_runs_fold_into_speed_cells_once() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = Store::open(&dir.path().join("kiln.db"))?;
            let leg = |id: &str, cpu_s: f64| Observation { id: id.into(), key: "leg:k".into(), kind: "leg".into(), wall_s: 10.0, cpu_s: Some(cpu_s), ..Observation::default() };
            let runs = vec![leg("a1", 20.0), leg("a2", 40.0), Observation { exit_code: Some(1), ..leg("a3", 5.0) }, run("c1", 1.0, 1.0, 2.0)];
            let (new, speeds) = store.record_runs("mac".into(), runs.clone()).await?;
            assert_eq!((new, speeds.len()), (4, 2));
            assert_eq!(store.record_runs("mac".into(), runs).await?.1.len(), 0, "sent again: folded once");
            store.record_runs("linux".into(), vec![leg("b1", 10.0)]).await?;
            let cells = store.speed_cells().await?;
            let mac = cells.iter().find(|(k, r, _)| k == "leg:k" && r == "mac").map(|c| c.2).ok_or_else(|| anyhow::anyhow!("mac's cell"))?;
            assert_eq!(mac.n, 2);
            assert!((mac.sum - (20f64.ln() + 40f64.ln())).abs() < 1e-12);
            assert_eq!(cells.len(), 2);
            Ok(())
        })
    }
}
