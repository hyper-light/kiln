//! A runtime of the Shards class on Agentfile images alone (joint plan step 12's item B; kiln's step 17, part 7: launch
//! through Hex): no guest of Hex's, no PID 1 and no guest configuration, which stay for every other host. Its boxes, each
//! a Shards image VM booted under shards-init (D231), are the image class's ([`ImageClass`]); their run hosts the
//! `shards` binary given, with the Shards home and cache given; their records a node Store of its own under the
//! directory given, changed one at a time; and each run host's diagnostics a file beside them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hex_api::{
    BoxId, BoxInfo, BoxSpec, Class, HostUse, Member, Proc, Program, Refusal, Runtime, Signal,
    Stdio, Usage,
};
use hex_core::record::{BoxRecord, PoolRecord};
use hex_runtime::devices::{Books, PoolPreparationCheck, PoolRecords, Records, Transaction};
use hex_runtime::guest::{ImageClass, ImageWaits, host_images};
use shards_api::image::Images;

use crate::store::Store;

/// The Shards class on Agentfile images as a runtime of its own: its record authority this runtime's own Store.
pub struct ImageRuntime {
    store: Store,
    /// A box's record is read and changed by one transaction at a time.
    turn: Mutex<()>,
    class: ImageClass,
}

impl ImageRuntime {
    /// The class whose run hosts are the `shards` binary `shards` (built at Hex's vendored Shards revision, so it speaks
    /// the image channel Hex's `shards_api` does), their Shards home and cache `paths`, whose store holds the images,
    /// its boxes' records under `records` (`records/state` and `records/run`, made where missing), and each run host's
    /// diagnostics in `records/image-diagnostics`. Shards' image boot as every host runs it ([`host_images`]), its waits
    /// learned from their own answers (decision 9).
    pub fn new(
        shards: PathBuf,
        paths: shards_api::RuntimePaths,
        records: &Path,
    ) -> Result<Arc<Self>, Refusal> {
        Self::with_images(shards, paths, records, host_images(), Ok(ImageWaits::new()))
    }

    /// [`ImageRuntime::new`] over the image boot `images` and the waits `waits` given.
    pub fn with_images(
        shards: PathBuf,
        paths: shards_api::RuntimePaths,
        records: &Path,
        images: Result<Arc<dyn Images>, Refusal>,
        waits: Result<ImageWaits, Refusal>,
    ) -> Result<Arc<Self>, Refusal> {
        let store = Store::open(&records.join("state"), &records.join("run"))?;
        let class = ImageClass::new(shards)
            .with_boot(images, waits, records.join("image-diagnostics"))
            .with_paths(Arc::new(paths));
        let runtime = Arc::new(Self {
            store,
            turn: Mutex::new(()),
            class,
        });
        let books: Arc<dyn Books> = runtime.clone();
        runtime.class.bind_books(Arc::downgrade(&books))?;
        // The session the Store stamps its records with, so a run of an earlier boot is known to have ended with it
        // (T-T7.1.2): none where the Store reads none.
        runtime
            .class
            .bind_boot_session(runtime.store.boot().cloned())?;
        Ok(runtime)
    }
}

/// What the class never asks of this runtime's records: a pool, a device, a native launch. Refused as broken, since only
/// a fault of Hex's own could ask it.
fn unasked(what: &str) -> Refusal {
    Refusal::Broken(format!("an image runtime's records hold no {what}"))
}

impl Books for ImageRuntime {
    fn records(&self) -> Result<Records, Refusal> {
        self.store.all()
    }
    fn pool_records(&self) -> Result<PoolRecords, Refusal> {
        Ok(Vec::new())
    }
    fn prepare_pool(
        &self,
        _: [u8; 32],
        _: hex_api::Execution,
        _: hex_api::RuntimeCost,
        _: &[hex_api::DeviceId],
        _: (&[u8; 32], &[hex_api::DeviceId], Option<&PoolRecord>),
        _: &mut PoolPreparationCheck<'_>,
    ) -> Result<PoolRecord, Refusal> {
        Err(unasked("pool"))
    }
    fn capacity(&self) -> Result<hex_api::Capacity, Refusal> {
        Err(unasked("capacity of a guest's"))
    }
    fn physical_devices(&self) -> Result<Vec<hex_api::DeviceInfo>, Refusal> {
        Ok(Vec::new())
    }
    /// One box's record read and changed by `callback` alone, each change through the Store's lifecycle writes.
    fn with_transaction(
        &self,
        _: &BoxId,
        devices: &[hex_api::DeviceId],
        callback: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        if !devices.is_empty() {
            return Err(unasked("device"));
        }
        let _turn = self.turn.lock().map_err(|_| {
            Refusal::Broken("an image runtime's record turn is poisoned".to_owned())
        })?;
        callback(&Kept(&self.store))
    }
    fn with_pool_transaction(
        &self,
        _: &hex_api::BackingKey,
        _: &[hex_api::DeviceId],
        _: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        Err(unasked("pool"))
    }
    fn with_handoff_transaction(
        &self,
        _: &BoxId,
        _: &hex_api::BackingKey,
        _: &[hex_api::DeviceId],
        _: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        Err(unasked("pool"))
    }
}

/// The runtime's records within one transaction: the Store's lifecycle reads and writes.
struct Kept<'a>(&'a Store);

impl Transaction for Kept<'_> {
    fn records(&self) -> Result<Records, Refusal> {
        self.0.all()
    }
    fn pool_records(&self) -> Result<PoolRecords, Refusal> {
        Ok(Vec::new())
    }
    fn read_pool(&self, _: &hex_api::BackingKey) -> Result<Option<PoolRecord>, Refusal> {
        Err(unasked("pool"))
    }
    fn put_pool(&self, _: &PoolRecord) -> Result<(), Refusal> {
        Err(unasked("pool"))
    }
    fn grant_pool(&self, _: &hex_api::BackingGrant) -> Result<PoolRecord, Refusal> {
        Err(unasked("pool"))
    }
    fn remove_pool(&self, _: &hex_api::BackingKey) -> Result<(), Refusal> {
        Err(unasked("pool"))
    }
    fn check_unsubmitted_pool(&self) -> Result<(), Refusal> {
        Err(unasked("pool"))
    }
    fn read(&self, id: &BoxId) -> Result<Option<BoxRecord>, Refusal> {
        self.0.read_lifecycle(id)
    }
    fn put(&self, record: &BoxRecord) -> Result<(), Refusal> {
        self.0.put_lifecycle(record)
    }
    fn remove(&self, id: &BoxId) -> Result<(), Refusal> {
        self.0.remove_lifecycle(id)
    }
    fn launch_managed(
        &self,
        _: &hex_runtime::devices::ManagedLaunch<'_>,
        _: shards_api::LaunchIo,
        _: Duration,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        Err(unasked("native launch"))
    }
    fn attach_managed(
        &self,
        _: Option<&shards_api::Identity>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        Err(unasked("native launch"))
    }
    fn finalize_managed(&self, _: Option<&shards_api::Identity>) -> Result<(), Refusal> {
        Err(unasked("native launch"))
    }
    fn managed_usage(&self) -> Result<Usage, Refusal> {
        Err(unasked("native launch"))
    }
}

/// Every call the image class's own: it serves the Shards class alone.
impl Runtime for ImageRuntime {
    fn classes(&self) -> Vec<Class> {
        self.class.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.class.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.class.start(id)
    }
    fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.class.exec(id, program, stdio)
    }
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.class.run(spec, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        self.class.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.class.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.class.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.class.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.class.delete(id)
    }
    fn member(&self, id: &BoxId, member: &Member) -> Result<Box<dyn Proc>, Refusal> {
        self.class.member(id, member)
    }
    fn room(&self, id: &BoxId, room: Option<i64>) -> Result<Option<u64>, Refusal> {
        self.class.room(id, room)
    }
    fn rate(
        &self,
        id: &BoxId,
        rate: Option<std::num::NonZeroU64>,
    ) -> Result<Option<std::num::NonZeroU64>, Refusal> {
        self.class.rate(id, rate)
    }
    fn host_use(&self, id: &BoxId) -> Result<HostUse, Refusal> {
        self.class.host_use(id)
    }
    fn delete_with_host_use(&self, id: &BoxId) -> Result<Option<HostUse>, Refusal> {
        self.class.delete_with_host_use(id)
    }
}

#[cfg(test)]
#[path = "images/tests.rs"]
mod tests;
