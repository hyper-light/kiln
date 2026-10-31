//! U19: one local transfer/controller lease shared by Sync and task recovery.
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct Controller { dir: PathBuf, state: Mutex<State> }
struct State { lease: Option<File>, fenced: bool, sync_claimed: bool }
pub(super) struct SyncOwner(Arc<Controller>);
impl Drop for SyncOwner { fn drop(&mut self) { super::lock(&self.0.state).sync_claimed = false; } }

impl Controller {
    pub fn open(dir: &Path) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(dir)?;
        let this = Arc::new(Self { dir: dir.canonicalize()?, state: Mutex::new(State { lease: None, fenced: false, sync_claimed: false }) });
        this.try_activate()?;
        Ok(this)
    }
    pub fn dir(&self) -> &Path { &self.dir }
    pub fn active(&self) -> bool { let state = super::lock(&self.state); state.lease.is_some() && !state.fenced }
    pub fn try_activate(&self) -> anyhow::Result<bool> {
        let mut state = super::lock(&self.state);
        anyhow::ensure!(!state.fenced, "sync controller is fenced until restart");
        if state.lease.is_some() { return Ok(true); }
        let lease = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(self.dir.join("controller.lock"))?;
        match lease.try_lock() {
            Ok(()) => { state.lease = Some(lease); Ok(true) }
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(e) => Err(anyhow::anyhow!("sync controller lease failed: {e}")),
        }
    }
    pub(super) fn claim_sync(self: &Arc<Self>) -> anyhow::Result<SyncOwner> {
        let mut state = super::lock(&self.state);
        anyhow::ensure!(state.lease.is_some() && !state.fenced && !state.sync_claimed, "sync controller is unavailable or already has a local owner");
        state.sync_claimed = true;
        Ok(SyncOwner(self.clone()))
    }
    pub(super) fn fence(&self) { super::lock(&self.state).fenced = true; }
}
