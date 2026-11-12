//! The seam a VM class's driver implements: one guest at a time, launched, read through its VMM's OS from
//! that launch through its readiness and an idle hold, reported on by its own agent, and torn down. The
//! census, the sampling, the evidence and the output are the producer's ([`super::run`]).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hex_api::CapacityResources;

/// What a driver says of its class for the evidence: one `name value` pair a line, each naming what it
/// is and where it came from (a path with its SHA-256, a variable and its value).
pub type Lines = Vec<(String, String)>;

/// One path held on disk: the file it names (its device and inode, which a rename keeps) and its allocated
/// bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Held {
    pub path: PathBuf,
    pub file: (u64, u64),
    pub bytes: u64,
}

/// What is held on disk under a guest's folders, sorted by path.
pub type Files = Vec<Held>;

/// The class's configured envelope, and where its configuration says so.
pub struct Envelope {
    pub resources: CapacityResources,
    /// Its vCPUs: the VMM threads its guest runs on, where the OS names them.
    pub vcpus: u32,
    pub source: String,
}

/// The patience a guest is booted within, before any run-out doubles it, and its record.
pub struct Patience {
    pub bound: Duration,
    pub source: String,
}

/// One reading of a guest's VMM's CPU, as its OS keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Use {
    /// The VMM's CPU so far, user and system, its guest's time included where the OS counts it as the
    /// VMM's (HVF's and KVM's both do).
    pub cpu: Duration,
    /// The part of `cpu` its OS counts as its guest's (Linux: guest_time; macOS: its vCPU threads').
    pub guest_cpu: Duration,
    /// The VMM threads its guest's vCPUs run on, where the OS reading names them.
    pub guest_threads: Option<u32>,
}

/// One reading of what a guest's VMM holds, as its OS keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holding {
    /// The VMM's footprint: the memory its OS charges it for its own, the guest RAM it has touched included
    /// (Linux: its cgroup's non-file memory, `memory.stat`'s `anon` and `kernel`, KVM's page tables among the
    /// kernel's; macOS: its `ri_phys_footprint`).
    pub footprint: u64,
    /// The page cache its OS charges it apart (Linux: its cgroup's `memory.stat` `file`): the images it read,
    /// shared by every VM that reads them and reclaimable, so recorded and not charged.
    pub file_pages: Option<u64>,
    /// The highest footprint it has reached, for the record (macOS: `ri_lifetime_max_phys_footprint`).
    pub peak_footprint: Option<u64>,
    /// The part of `footprint` that is the guest's RAM: `None` while the VMM maps none it can tell apart.
    pub guest_memory: Option<u64>,
    /// What is held on disk under its folders.
    pub files: Files,
}

/// Reads one launched guest's VMM, from a sampling thread of its own: its CPU, cheaply, at each window's
/// boundary; what it holds, which may take longer than a window to read, apart, so that never delays the
/// CPU's readings.
pub trait Probe: Send + 'static {
    type Reading: Send + 'static;
    fn read(&mut self) -> Result<Self::Reading, String>;
    /// After the last reading: what the readings took as found still holds (the guest's RAM where they
    /// found it).
    fn verify(&mut self) -> Result<(), String> {
        Ok(())
    }
}

/// A launched guest of `D` and the probes that read its VMM.
pub type Launched<D> = (
    <D as Driver>::Guest,
    <D as Driver>::Cpu,
    <D as Driver>::Holds,
);

pub trait Driver {
    /// A launched guest.
    type Guest;
    type Cpu: Probe<Reading = Use>;
    type Holds: Probe<Reading = Holding>;

    /// The class, as its records name it (`hex_runtime::guest::CLASS` for Shards).
    fn class(&self) -> String;
    /// What this run measures: the class's images and configuration.
    fn describe(&self) -> Result<Lines, String>;
    /// The envelope floor and ceiling hold.
    fn envelope(&self) -> Result<Envelope, String>;
    /// The first boot's patience.
    fn patience(&self) -> Result<Patience, String>;
    /// The folder the VMM's own files go on: its filesystem's block size is the disk quantum.
    fn runtime(&self) -> &Path;
    /// How its probes read the VMM's CPU, memory and disk (its OS's interfaces), for the evidence.
    fn sources(&self) -> Lines;

    /// What is held on disk under the class's runtime folders now: before a guest, and after its end.
    fn held(&self) -> Result<Files, String>;
    /// Starts one guest's VMM, told it has until `deadline` to be ready as the node tells its guests, and
    /// returns it with the probes that read it.
    fn launch(&mut self, deadline: Instant) -> Result<Launched<Self>, String>;
    /// Waits, until `deadline`, for the launched guest to be ready (its agent answering): `false` when
    /// the deadline passed first.
    fn ready(&mut self, guest: &mut Self::Guest, deadline: Instant) -> Result<bool, String>;
    /// Holds the ready guest idle until `until`: its end in that time is a failure.
    fn idle(&mut self, guest: &Self::Guest, until: Instant) -> Result<(), String>;
    /// The guest agent's own report of what it has free inside the envelope, until `deadline`.
    fn report(
        &mut self,
        guest: &Self::Guest,
        deadline: Instant,
    ) -> Result<CapacityResources, String>;
    /// Stops the guest and waits, until `deadline`, for its end.
    fn tear_down(&mut self, guest: Self::Guest, deadline: Instant) -> Result<(), String>;
}
