//! A runtime of the Shards class on Agentfile images alone (joint plan step 12's item B; kiln's step 17, part 7: launch
//! through Hex): no guest of Hex's, no PID 1 and no guest configuration, which stay for every other host. Its boxes, each
//! a Shards image VM booted under shards-init (D231), are the image class's ([`ImageClass`]); their run hosts the
//! `shards` binary given, with the Shards home and cache given; their records a node Store of its own under the
//! directory given, changed one at a time; and each run host's diagnostics a file beside them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hex_api::lease::Lessors;
use hex_api::{
    BoxId, BoxInfo, BoxSpec, Class, HostUse, Member, Proc, Program, Refusal, Runtime, Signal,
    Stdio, Usage,
};
use hex_core::record::{BoxRecord, PoolRecord};
use hex_runtime::devices::{Books, PoolPreparationCheck, PoolRecords, Records, Transaction};
use hex_runtime::guest::{ImageClass, ImageWaits, host_images};
use shards_api::image::Images;

use crate::store::{Store, TURNS};
use crate::turns::Turns;

/// The Shards class on Agentfile images as a runtime of its own: its record authority this runtime's own Store.
pub struct ImageRuntime {
    store: Store,
    /// A box's record is read and changed by one transaction of it at a time; transactions of different boxes never
    /// wait on each other (the review's B-14), each box's turn its own, at most [`TURNS`] taken at once.
    turns: Turns,
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
        Self::built(shards, paths, records, images, waits, None)
    }

    /// [`ImageRuntime::new`], its VMs' credentials leased through `lessors`, its orchestrator's (Shards' D242; joint
    /// plan step 33, C3: [`ImageClass::with_lessors`]). A runtime made without them refuses a box granting a
    /// credential as it is created, `Unconfigured`.
    pub fn leasing(
        shards: PathBuf,
        paths: shards_api::RuntimePaths,
        records: &Path,
        lessors: Arc<dyn Lessors>,
    ) -> Result<Arc<Self>, Refusal> {
        Self::built(
            shards,
            paths,
            records,
            host_images(),
            Ok(ImageWaits::new()),
            Some(lessors),
        )
    }

    fn built(
        shards: PathBuf,
        paths: shards_api::RuntimePaths,
        records: &Path,
        images: Result<Arc<dyn Images>, Refusal>,
        waits: Result<ImageWaits, Refusal>,
        lessors: Option<Arc<dyn Lessors>>,
    ) -> Result<Arc<Self>, Refusal> {
        let store = Store::open(&records.join("state"), &records.join("run"))?;
        recover(&store)?;
        let class = ImageClass::new(shards)
            .with_boot(images, waits, records.join("image-diagnostics"))
            .with_paths(Arc::new(paths));
        let class = match lessors {
            Some(lessors) => class.with_lessors(lessors),
            None => class,
        };
        let runtime = Arc::new(Self {
            store,
            // A transaction waits for one of the same box under way, made by this process's own thread, bounded by the
            // slowest hold its transactions taught it and following the holder past it (decision 9), as the store's own
            // turns do; no record is read for it.
            turns: Turns::new(
                "changing an image box's record",
                "image box record changes under way",
                TURNS,
                hex_core::patience::Patience::measuring("images.turns"),
            ),
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

    /// The class its VMs boot through, a node VM's among them (joint plan step 39): what a node's guest runtime calls
    /// to boot, read and stop its node VM, its records this runtime's own Store.
    pub fn class(&self) -> &ImageClass {
        &self.class
    }

    /// Whether box `id`'s turn is free now, held by no transaction: what the tests of the runtime's turns read.
    #[cfg(test)]
    fn turn_free(&self, id: &BoxId) -> bool {
        self.turns.try_take(id).is_ok()
    }
}

/// The recovery a node runs over its records (`Recorded::recover`; T-T7.1.2), as an image runtime opens its own (the
/// review's B-13): the boot session marker taken, so a torn record is told from an earlier boot's; the temporary files a
/// crash left mid-write removed; a record the state directory can't read refused, naming its file, as the damage it is;
/// each record torn in the runtime place before this boot of the host removed, that boot's end having ended its run,
/// and each torn in this boot kept, so its name is never reused and its box stays held, as a node holds one, its list
/// naming it; and the node VM rows an earlier process left settled. A readable record's run of an earlier process is
/// the class's to hold or take as it reads the record. The last-resort ends a let-go row kept go with it: an image
/// runtime has no charge learner to carry them to.
fn recover(store: &Store) -> Result<(), Refusal> {
    let last_boot = store.take_boot_marker()?;
    store.remove_strays()?;
    let mut records = store.retained_records()?;
    let torn = store.take_torn(&mut records)?;
    store.refuse_unreadable(&records)?;
    if store.before_this_boot(last_boot.as_ref()) {
        for (id, _) in &torn {
            store.remove(id)?;
        }
    }
    store.settle_node_vms()?;
    Ok(())
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
    fn record(&self, id: &BoxId) -> Result<Option<Result<BoxRecord, String>>, Refusal> {
        self.store.get(id)
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
    /// One box's record read and changed by `callback` alone, each change through the Store's lifecycle writes, under the
    /// box's own turn.
    fn with_transaction(
        &self,
        id: &BoxId,
        devices: &[hex_api::DeviceId],
        callback: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        if !devices.is_empty() {
            return Err(unasked("device"));
        }
        let _turn = self.turns.take(id)?;
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
    fn node_vm(
        &self,
        run: &hex_api::NodeRun,
    ) -> Result<Option<hex_core::record::NodeVmRecord>, Refusal> {
        self.store.node_vm(run)
    }
    fn put_node_vm(&self, record: &hex_core::record::NodeVmRecord) -> Result<(), Refusal> {
        self.store.put_node_vm(record)
    }
    fn all_node_vms(&self) -> Result<Vec<hex_core::record::NodeVmRecord>, Refusal> {
        self.store.all_node_vms()
    }
    fn mint_node_run(&self) -> Result<hex_api::NodeRun, Refusal> {
        self.store.mint_node_run()
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
        _: Option<Duration>,
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
    fn mcp(&self, id: &BoxId, member: &Member) -> Result<Box<dyn Proc>, Refusal> {
        self.class.mcp(id, member)
    }
    fn signal_member(&self, id: &BoxId, member: &Member, signal: Signal) -> Result<(), Refusal> {
        self.class.signal_member(id, member, signal)
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
    fn started(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.class.started(id)
    }
}

#[cfg(test)]
#[path = "images/tests.rs"]
pub(crate) mod tests;
