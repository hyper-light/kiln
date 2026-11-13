//! The fleet's per-core speeds as kiln's server fits them from its run history (W05-R26;
//! docs/needs.md "Pace"; `kiln_ci::pace::speed`).
//!
//! - **The durations** are the runs runners report as they end (`RunnerFacts.runs`), which the
//!   store keeps in `run_observations` and folds into each kind's speed cell on its runner
//!   (`run_speed_cells`, a numbered migration: `Store::record_runs`).
//! - **Refit on each run kept**, incrementally: the run's kind's terms in the normal equations are
//!   taken out, its cell changed and put back, and the machines' system solved (exact, O(M³) for M
//!   machines), off the hub's loops' paths only in that it's a short lock.
//! - **Said** in the directory (`RunnerInfo.speed`): each runner reads its own and carries it, as
//!   speed now, in its word to owners.

use std::sync::Arc;

use kiln_ci::pace::speed::{Fit, Fleet};
use parking_lot::Mutex;

/// The fleet's cells and their latest fit.
#[derive(Default)]
pub struct Speeds {
    fleet: Mutex<Option<Fleet>>,
    fit: arc_swap::ArcSwap<Fit>,
}

impl Speeds {
    /// The latest fit (nothing measured before the first run is folded in, or the store read).
    pub fn fit(&self) -> Arc<Fit> {
        self.fit.load_full()
    }

    /// Every cell the store keeps, once: the fit from them.
    async fn loaded(&self, store: &crate::store::Store) {
        if self.fleet.lock().is_some() {
            return;
        }
        let cells = match store.speed_cells().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "couldn't read the fleet's speed cells: its speeds start from the runs kept from now on");
                Vec::new()
            }
        };
        let mut g = self.fleet.lock();
        if g.is_none() {
            let mut f = Fleet::default();
            for (key, runner, cell) in &cells {
                f.add(key, runner, *cell);
            }
            self.fit.store(Arc::new(f.fit()));
            *g = Some(f);
        }
    }

    /// Runner `runner`'s runs just kept (`speeds`: each one's kind and y) folded in, and the fit
    /// made again when one changed a kind that ran on two or more machines (the rest say nothing
    /// of speed: most kinds, 4,679 of production's 4,701 on 2026-11-12).
    pub async fn observe(&self, store: &crate::store::Store, runner: &str, speeds: &[(String, f64)]) {
        self.loaded(store).await;
        let mut g = self.fleet.lock();
        let Some(f) = g.as_mut() else { return };
        let mut shared = false;
        for (key, y) in speeds {
            shared |= f.observe(key, runner, *y);
        }
        if shared {
            self.fit.store(Arc::new(f.fit()));
        }
    }

    /// The store read as the server starts, so the directory says speeds before a run is kept.
    pub async fn start(&self, store: &crate::store::Store) {
        self.loaded(store).await;
    }

    /// Kind `key`'s work, ln of its CPU-seconds on the reference core, as the fit stands
    /// (`kiln_ci::pace::speed::Fleet::work`): `None` before the store is read, or for a kind none
    /// of whose runs is kept.
    pub fn work(&self, key: &str) -> Option<f64> {
        let fit = self.fit();
        self.fleet.lock().as_ref().and_then(|f| f.work(key, &fit))
    }
}
