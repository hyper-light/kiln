//! One capacity view per machine (joint plan step 13, `docs/design/capacity-view.md`; AC-T7.4.1): Hex's
//! scheduler and kiln's load balancer each decide where work goes at the node that asks, and each admits
//! on the machine the work lands on. Both admit against one ledger of that machine's room, so neither
//! oversubscribes the other.
//!
//! - [`Keeper`]: the machine's one ledger (CV1), hosted by the agent that measures the room (Hex's node
//!   agent). Its host's own admissions are `local`, passed into every decision; every other admitter is a
//!   holder whose reservations the keeper writes before it grants them (CV2).
//! - [`Holder`]: the side that reserves through the keeper (kiln's runner): its work starts only on a
//!   grant, and after every (re)join it says its whole live set (CV5).
//! - The seam (CLAUDE.md §9): [`CapacityView`], the keeper as a transport serves it; its simulator
//!   [`sim::SimView`]; and [`conformance`], the contract both it and Hex's node pass.
//!
//! Pure (CLAUDE.md §9): no clock, disk, network or randomness. The keeper's records go through
//! [`Records`]; its shell carries the messages and times nothing; a holder's shell times its waits
//! (Patience, CV6) and says when one ran out ([`Holder::ran_out`]).

pub mod conformance;
mod holder;
mod keeper;
pub mod sim;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};

use crate::Refusal;

pub use holder::{Event, Holder, NotNow, Out};
pub use keeper::{Budget, Forgotten, Keeper, NotForgotten, Records, Stop};

/// A room, or what a reservation holds: CPU in thousandths, memory and disk in bytes, and GPUs, the
/// dimensions both placement kernels count (`hex_sim::lb::Amounts`, kiln's `lb` amounts), in their order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Amounts {
    pub cpu_milli: u64,
    pub mem_bytes: u64,
    pub disk_bytes: u64,
    pub gpus: u64,
}

impl Amounts {
    pub const ZERO: Amounts = Amounts {
        cpu_milli: 0,
        mem_bytes: 0,
        disk_bytes: 0,
        gpus: 0,
    };

    /// From the kernels' `[cpu, mem, disk, gpus]`.
    pub const fn from_array(a: [u64; 4]) -> Self {
        let [cpu_milli, mem_bytes, disk_bytes, gpus] = a;
        Self {
            cpu_milli,
            mem_bytes,
            disk_bytes,
            gpus,
        }
    }

    /// As the kernels' `[cpu, mem, disk, gpus]`.
    pub const fn to_array(self) -> [u64; 4] {
        [self.cpu_milli, self.mem_bytes, self.disk_bytes, self.gpus]
    }

    fn zip(self, other: Self, f: impl Fn(u64, u64) -> Option<u64>) -> Option<Self> {
        Some(Self {
            cpu_milli: f(self.cpu_milli, other.cpu_milli)?,
            mem_bytes: f(self.mem_bytes, other.mem_bytes)?,
            disk_bytes: f(self.disk_bytes, other.disk_bytes)?,
            gpus: f(self.gpus, other.gpus)?,
        })
    }

    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.zip(other, u64::checked_add)
    }

    pub fn checked_sub(self, other: Self) -> Option<Self> {
        self.zip(other, u64::checked_sub)
    }

    /// What is left of this once `other` is taken: none in a dimension `other` exceeds (no room past none).
    pub fn less(self, other: Self) -> Self {
        Self {
            cpu_milli: self.cpu_milli.saturating_sub(other.cpu_milli),
            mem_bytes: self.mem_bytes.saturating_sub(other.mem_bytes),
            disk_bytes: self.disk_bytes.saturating_sub(other.disk_bytes),
            gpus: self.gpus.saturating_sub(other.gpus),
        }
    }

    /// The larger in each dimension.
    pub fn max(self, other: Self) -> Self {
        Self {
            cpu_milli: self.cpu_milli.max(other.cpu_milli),
            mem_bytes: self.mem_bytes.max(other.mem_bytes),
            disk_bytes: self.disk_bytes.max(other.disk_bytes),
            gpus: self.gpus.max(other.gpus),
        }
    }

    /// The smaller in each dimension.
    pub fn min(self, other: Self) -> Self {
        Self {
            cpu_milli: self.cpu_milli.min(other.cpu_milli),
            mem_bytes: self.mem_bytes.min(other.mem_bytes),
            disk_bytes: self.disk_bytes.min(other.disk_bytes),
            gpus: self.gpus.min(other.gpus),
        }
    }

    /// Whether this fits in `room`, in every dimension.
    pub fn fits(self, room: Self) -> bool {
        self.cpu_milli <= room.cpu_milli
            && self.mem_bytes <= room.mem_bytes
            && self.disk_bytes <= room.disk_bytes
            && self.gpus <= room.gpus
    }

    pub fn is_zero(self) -> bool {
        self == Self::ZERO
    }
}

/// A reservation's name: its holder's epoch when it made it and its count in that epoch, so a holder
/// never reuses one (CV2) and the keeper keeps no tombstones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ReservationId {
    pub epoch: u64,
    pub seq: u64,
}

/// A reservation as a holder lists it in its sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Held {
    pub id: ReservationId,
    pub amount: Amounts,
}

/// What every message after a hello carries (CV4, Chubby's sequencer): the keeper's epoch, the session
/// the keeper opened, and the holder's epoch. The keeper acts on no message whose fence isn't the
/// holder's current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fence {
    pub keeper: u64,
    pub session: u64,
    pub holder: u64,
}

/// The machine's view as the keeper answers it (CV7): its room, and everything held there (the host's
/// own admissions and every holder's reservations).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct View {
    pub room: Amounts,
    pub held: Amounts,
}

impl View {
    /// The room left for a new charge: the room less everything held, none in a dimension that is over.
    pub fn free(&self) -> Amounts {
        self.room.less(self.held)
    }

    /// How far what is held exceeds the room (the room shrank): nothing new is admitted until it is none.
    pub fn over(&self) -> Amounts {
        self.held.less(self.room)
    }
}

/// Where a holder's reserved work runs, as its host's OS accounts that work's memory (CV12): the host reads it in
/// the same sample as its free memory, which already lacks what the work uses, and credits the room that use, up
/// to the holder's reservations, while the holder is synced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    /// A cgroup v2 directory, relative to the unified hierarchy's root (Linux).
    Cgroup(String),
    /// A process group (macOS).
    Group(i32),
    /// A job object, by its name (Windows).
    Job(String),
}

/// Why a holder's scope earns no credit (CV12): the session that named it is closed, typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopeRefused {
    /// The host's configuration declares no user for this holder, so nothing can prove the scope its own.
    NoUser,
    /// The scope isn't the holder's user's: another user owns it, or runs in it.
    NotOwned,
    /// The host couldn't read the scope: it doesn't exist, or the OS refused.
    Unreadable,
    /// The scope names nothing a host could read: a path leaving the hierarchy, a name the OS can't take.
    Invalid,
    /// This host's OS accounts no scope of that kind.
    NotThisPlatform,
}

/// What a host's sample read of one holder's scope (CV12): what its work used, or why it earns no credit.
pub type Read = Result<Amounts, ScopeRefused>;

/// A holder's word to the keeper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToKeeper {
    /// Opens a session: who it is, its process's epoch, and where its reserved work runs, if it says (CV12).
    Hello {
        holder: String,
        epoch: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<Scope>,
    },
    /// Its whole live set, once per session: every reservation whose work may still run (CV5).
    Sync { fence: Fence, held: Vec<Held> },
    /// Hold `amount` as `id` if it fits.
    Reserve {
        fence: Fence,
        id: ReservationId,
        amount: Amounts,
    },
    /// `id`'s work can no longer run.
    Release { fence: Fence, id: ReservationId },
}

impl ToKeeper {
    /// The fence it carries: every message but a hello's.
    pub fn fence(&self) -> Option<Fence> {
        match self {
            ToKeeper::Hello { .. } => None,
            ToKeeper::Sync { fence, .. }
            | ToKeeper::Reserve { fence, .. }
            | ToKeeper::Release { fence, .. } => Some(*fence),
        }
    }
}

/// Why the keeper didn't grant a reservation. The answer carries the view, so the next decision is made
/// on the room there is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Why {
    /// It doesn't fit the room left.
    Room,
    /// The holder holds as many reservations as the keeper keeps for one.
    Full { bound: u64 },
    /// It asks nothing.
    Empty,
    /// It asks GPUs: a GPU is held by its identity through Hex's device holder, not by a count (CV3).
    Devices,
    /// The id is held with another amount.
    Disagrees,
}

/// Why a message is malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Invalid {
    /// The holder's name is empty, too long, or not visible ASCII.
    Name,
    /// An id of an epoch after the holder's own.
    FutureId,
    /// A new reservation not of the holder's current epoch.
    IdEpoch,
    /// One id listed twice.
    DuplicateId,
    /// A sync listing more than the keeper keeps for one holder.
    SyncTooLong { bound: u64 },
    /// Amounts whose sum can't be counted.
    Overflow,
}

/// Why the keeper closed a session, or refused to open one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Close {
    /// A newer process of this holder said hello: this one's epoch is behind `current`.
    OlderEpoch {
        current: u64,
    },
    /// The fence names no current session: a link that has since dropped, or a keeper since restarted.
    Session,
    /// A reserve or release before the session's sync.
    NotSynced,
    /// The keeper serves as many holders as its host authorizes.
    HoldersFull {
        bound: u64,
    },
    Invalid(Invalid),
    /// The scope the hello named earns no credit, and why (CV12): the holder may say hello again without it.
    Scope(ScopeRefused),
    /// The peer isn't a holder the host's configuration authorizes, or said hello as another holder than the one its
    /// link authenticates (CV8, CV9).
    NotAuthorized,
}

/// The keeper's word to a holder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToHolder {
    /// The session is open: its fence, and the view.
    Welcome { fence: Fence, view: View },
    /// The sync is done: the keeper's count and sum of this holder's reservations now, and how many it
    /// released (not listed), adopted (listed, not known) and took the larger amount of (both differ).
    Synced {
        fence: Fence,
        view: View,
        count: u64,
        sum: Amounts,
        released: u64,
        adopted: u64,
        disagreed: u64,
    },
    /// `id` holds `amount`: its work may start.
    Granted {
        fence: Fence,
        id: ReservationId,
        amount: Amounts,
        view: View,
    },
    /// `id` holds nothing, and why.
    Refused {
        fence: Fence,
        id: ReservationId,
        why: Why,
        view: View,
    },
    /// `id` holds nothing (said for an id never held too).
    Released {
        fence: Fence,
        id: ReservationId,
        view: View,
    },
    /// The view changed for a reason this holder didn't cause.
    Hint { fence: Fence, view: View },
    /// The session is closed, or was never opened: `rejected` is the fence of the message refused
    /// (none for a hello).
    Closed { rejected: Option<Fence>, why: Close },
}

/// The seam (CLAUDE.md §9): a machine's capacity view as a transport serves it to its holders, each
/// holder's connection a link the transport numbers. What it answers goes back on the link the message
/// came on. Implemented by the simulator ([`sim::SimView`]) and by Hex's node; [`conformance::view`]
/// holds both to one contract.
pub trait CapacityView {
    /// `msg`, said on link `link`: the keeper's answer to say back on it.
    fn hear(&mut self, link: u64, msg: ToKeeper) -> Result<ToHolder, Refusal>;
    /// Link `link` dropped: its holder is away, and keeps what it holds (CV5).
    fn link_down(&mut self, link: u64) -> Result<(), Refusal>;
    /// The hints owed, each with the link to say it on (CV7).
    fn hints(&mut self) -> Result<Vec<(u64, ToHolder)>, Refusal>;
}

/// The host's side of a machine, which the conformance suite drives besides its links.
pub trait Host: CapacityView {
    /// The room measured anew, with what each scoped holder's work uses read in the same sample, by the link its
    /// holder is on (CV12). A synced holder whose scope isn't here earns no credit; one refused is closed, typed.
    fn sample(&mut self, room: Amounts, used: &[(u64, Read)]) -> Result<(), Refusal>;
    /// The room measured anew, no holder's use with it.
    fn set_room(&mut self, room: Amounts) -> Result<(), Refusal> {
        self.sample(room, &[])
    }
    /// The links whose synced holders named a scope, and each scope: what the host reads in its next sample.
    fn scopes(&mut self) -> Result<Vec<(u64, Scope)>, Refusal>;
    /// The host's own admissions are now `local` (a booking rose, a box ended).
    fn set_local(&mut self, local: Amounts) -> Result<(), Refusal>;
    /// The keeper's process ends and starts again from what it kept: every link is gone.
    fn restart(&mut self) -> Result<(), Refusal>;
}

/// A count as the wire carries it.
fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}
