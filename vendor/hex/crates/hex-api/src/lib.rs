//! Hex's seam (CLAUDE.md §9): the [`Runtime`] trait every runtime class implements, the types it
//! speaks, a simulator implementation ([`sim::SimRuntime`]) and the conformance suite
//! ([`conformance`]) that both it and every real runtime pass, and the [`differential`] suite that
//! holds each real class to the simulator's trace. Other products reach Hex only through
//! this crate.
//!
//! A box is the unit Hex runs: a resource bound ([`Bounds`]) holding processes. The `process` class
//! holds host processes in it; the `oci` class holds an OCI container (CP-T7.1, docs/design/).

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

mod bounds;
pub mod capacity;
pub mod conformance;
mod credentials;
mod devices;
pub mod differential;
#[cfg(windows)]
mod end;
pub mod event;
mod grants;
mod id;
pub mod lease;
pub mod patience;
mod runtime;
pub mod serve;
pub mod sim;
mod spec;

pub use bounds::{Bounds, IoRates, MIN_CPU_MILLI, MIN_MEMORY};
pub use credentials::{
    Captured, CredentialGrant, REFERENCE_MAX, SignInCli, SignInGrant, check_credentials,
    check_sign_ins,
};
pub use devices::{
    BackendUnavailable, DeviceHolder, DeviceId, DeviceInfo, DeviceKind, DeviceUnavailable,
    MAX_DEVICES, PhysicalDevice, PhysicalReport,
};
pub use grants::{
    Destination, Grant, Member, MemberKind, Ports, Proto, Published, Receiver, check_grants,
    check_published, unheld,
};
pub use id::BoxId;
pub use runtime::{
    Allocation, Asked, BackingGrant, BackingKey, BackingOffer, BackingPhase, BoxInfo, BoxState,
    Capacity, CapacityResources, Charged, ComponentAt, ConnectFailure, ConnectPhase, ConnectReply,
    ControlLink, ControlWake, Execution, Exit, GpuEligibility, GuestEnd, GuestFailure, GuestTime,
    HomeVolume, HostUse, ImageArch, ImageOs, ImagePlatform, IoKind, Killer, Learned, LearnedAwait,
    LearnedCeiling, LearnedEnd, LearnedHolder, LearnedHolding, LearnedOverhead, LearnedPrior,
    LearnedRank, LearnedRate, LearnedReservations, LearnedUnacted, LearnedUnread,
    LearnedUnrecorded, LearnedUse, LearnedWindow, LifecycleCause, MAX_LEARNED_HOLDINGS,
    MAX_LEARNED_PRIORS, MAX_RUNTIME_ROUTES, MachineTime, Membership, Negotiated, NodeRun,
    NodeVmPhase, NodeVmReceipt, PauseCause, Proc, RateHeld, Refusal, Reserved, RouteKey,
    RouteOffer, RouteReceipt, Runtime, RuntimeCost, Signal, StartedFrom, SwapCause, SwapWhen,
    Swapped, Upstream, Usage, VmSize, VolumeCause, WARM_ROOM, WarmHeld, WarmPool,
};
pub use spec::{
    Ask, BlockCount, BoxOwner, BoxSpec, Class, ClassSpec, End, ExecLimit, Given, ImageMember,
    ImageShare, ImageVolume, Mount, Network, OciSpec, Program, Rootfs, ServedMount, ShardsImage,
    Stdio, StringUnit, User, check_pinned, drive_absolute,
};

/// A caller's owned stream: Unix's descriptor becomes a file; Windows keeps the handle's resource kind.
#[cfg(unix)]
pub type OwnedStream = std::fs::File;
#[cfg(windows)]
pub use end::OwnedStream;
