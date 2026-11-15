//! The node's descriptors within what its platform gives, counted and fitted on every OS (ROOT's one accounting of
//! 2026-11-13): each bound that holds descriptors at its most, named, beside what the process holds at its start, and
//! where they don't fit, the program places, which the connections and every hosted VM's lanes follow, cut to the most
//! that fit. Linux's node fits these terms with its pools' (`crate::netns::fit`); macOS and Windows fit them alone
//! ([`fit`]). Before 2026-11-13 macOS and Windows bounded the socket's connections by their raw descriptor limit,
//! counting nothing else, so a busy node could pass it.
//!
//! A hosted VM is a node VM under shards-init that its node reaches by Shards' socket lanes (D251), each a local
//! socket pair on the node's side (`hex_client::guest::lanes`), the lanes its run admits ([`lanes`]) counted among
//! the node's own.

use super::{KEPT, Limits, Platform};

/// A program place's streams at their most ([`crate::serve`]): for each of the [`hex_core::wire::MAX_FDS`] an exec
/// carries, the one passed, or the pipe of two the node makes for one asked for (`hex_api::Stdio::make_asked`),
/// until its program starts; then each asked for's given end, kept until its client takes it, or, for a program in a
/// guest, each stream's connection to the guest and the passed pipe copied through it.
pub const FDS_A_PLACE_STREAM: u64 = 2;

/// A hosted VM's controller's own (Shards' `control::Controller`): its QUIC driver's socket and its waker's.
pub const FDS_A_CONTROLLER: u64 = 2;

/// A hosted VM's guest client's control connections: a call's and the evented one (`hex_runtime::guest`'s
/// `Booted`).
const CONTROL_LANES: usize = 2;

/// The lanes a hosted VM's guest client may hold open at once at `places` program places, what its run's grant admits
/// (D251's `lanes`): its control connections, and for each place a program's streams ([`hex_core::wire::MAX_FDS`])
/// and the staging or removal connection of the box it is made or removed for.
pub fn lanes(places: usize) -> usize {
    places
        .saturating_mul(hex_core::wire::MAX_FDS.saturating_add(1))
        .saturating_add(CONTROL_LANES)
}

/// The descriptors `hosted` VMs hold at `places` program places: each one's lanes ([`lanes`]),
/// [`hex_client::guest::lanes::FDS_A_LANE`] each, and its controller.
pub fn hosted_descriptors(hosted: usize, places: usize) -> u64 {
    use hex_client::guest::lanes::FDS_A_LANE;
    let of = |n: usize, each: u64| u64::try_from(n).unwrap_or(u64::MAX).saturating_mul(each);
    of(
        hosted,
        of(lanes(places), FDS_A_LANE).saturating_add(FDS_A_CONTROLLER),
    )
}

/// The descriptors the program places hold: each place's streams ([`FDS_A_PLACE_STREAM`] for each of the
/// [`hex_core::wire::MAX_FDS`]), and the message read past the places, refused with its streams closed.
pub fn place_streams(places: usize) -> u64 {
    let streams = u64::try_from(hex_core::wire::MAX_FDS).unwrap_or(u64::MAX);
    u64::try_from(places)
        .unwrap_or(u64::MAX)
        .saturating_mul(FDS_A_PLACE_STREAM.saturating_mul(streams))
        .saturating_add(streams)
}

/// The connections a socket admits at `places` program places and `in_flight` calls, a descriptor each, within
/// `descriptors`: no more than the places' and calls' budgets admit, as it keeps [`KEPT`] of each for every connection
/// (`crate::netns`'s `connections` on Linux).
pub fn connections(descriptors: usize, places: usize, in_flight: usize) -> usize {
    let admitted = |budget: usize| budget.checked_div(KEPT).unwrap_or(budget);
    descriptors.min(admitted(places)).min(admitted(in_flight))
}

/// What each OS's socket holds of its own, and of each program it watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Own {
    /// The socket's own: macOS's listener and kqueue; Windows' listener, completion port and the socket its one
    /// `AcceptEx` at a time accepts into.
    pub socket: u64,
    /// Each program watched: none on macOS (a kqueue's `EVFILT_PROC` on its pid), two on Windows (its process
    /// handle and its wait packet, `hex_afunix::ProgramWatch`).
    pub a_watch: u64,
}

/// macOS's socket's own (`crate::serve::macos::MacOs`).
pub const MACOS: Own = Own {
    socket: 2,
    a_watch: 0,
};

/// Windows' socket's own (`crate::serve::windows::Windows`).
pub const WINDOWS: Own = Own {
    socket: 3,
    a_watch: 2,
};

/// The node's bounds that hold descriptors on macOS and Windows: the socket's connections and program places, and its
/// calls in flight, which bound the connections; and the hosted VMs it can hold, each with its lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub descriptors: usize,
    pub places: usize,
    pub in_flight: usize,
    pub hosted: usize,
}

impl Bounds {
    /// The socket's connections at these bounds ([`connections`]).
    pub fn connections(&self) -> usize {
        connections(self.descriptors, self.places, self.in_flight)
    }
}

/// The descriptors a macOS or Windows node holds at `bounds`, each by its bound and named: the socket's connections
/// at their most, the one past them it accepts to refuse and its own; the program places' streams; the programs it
/// watches; the hosted VMs' lanes and controllers; and `beside`, what the process held at its start, where its OS's
/// reading of it is given.
pub fn terms(bounds: Bounds, own: Own, beside: Option<u64>) -> [(&'static str, u64); 5] {
    let of = |n: usize, each: u64| u64::try_from(n).unwrap_or(u64::MAX).saturating_mul(each);
    [
        (
            "the socket's connections",
            of(bounds.connections(), 1)
                .saturating_add(1)
                .saturating_add(own.socket),
        ),
        ("the program places' streams", place_streams(bounds.places)),
        ("programs watched", of(bounds.places, own.a_watch)),
        (
            "the hosted VMs' lanes",
            hosted_descriptors(bounds.hosted, bounds.places),
        ),
        ("beside those", beside.unwrap_or(0)),
    ]
}

/// The sum of [`terms`].
pub fn descriptors(bounds: Bounds, own: Own, beside: Option<u64>) -> u64 {
    terms(bounds, own, beside)
        .into_iter()
        .map(|(_, n)| n)
        .fold(0, u64::saturating_add)
}

/// A macOS or Windows node's bounds fitted to its descriptors: its socket's limits, and each hosted VM's lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fitted {
    pub limits: Limits,
    /// Each hosted VM's lanes ([`lanes`] at the fitted places): what its run's grant admits.
    pub lanes: usize,
}

/// `platform`'s bounds for a node that holds `hosted` VMs, fitted to its descriptors beside `beside` held, as Linux's
/// node fits its own (`crate::netns::fit`): where its terms at the platform's own bounds fit, they stay; where they
/// don't, the program places are the most whose terms fit, found by halving, and the connections and lanes follow
/// them. `Err` carries the least node's descriptors, one place and one connection, where even those don't fit.
pub fn fit(
    platform: &Platform,
    hosted: usize,
    own: Own,
    beside: Option<u64>,
) -> Result<Fitted, u64> {
    let hard = u64::try_from(platform.descriptors).unwrap_or(u64::MAX);
    let at = |places: usize| Bounds {
        descriptors: platform.descriptors,
        places,
        in_flight: platform.threads,
        hosted,
    };
    let fits = |places: usize| descriptors(at(places), own, beside) <= hard;
    let least = descriptors(at(1), own, beside);
    if least > hard {
        return Err(least);
    }
    let (mut low, mut high) = (1usize, platform.programs.max(1));
    if fits(high) {
        low = high;
    }
    while low < high {
        let mid = low.saturating_add(high.saturating_sub(low).div_ceil(2));
        if fits(mid) {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }
    let fitted = at(low);
    Ok(Fitted {
        limits: Limits {
            connections: fitted.connections(),
            message_bytes: hex_core::wire::MAX_MESSAGE,
            in_flight: fitted.in_flight,
            programs: fitted.places,
        },
        lanes: lanes(fitted.places),
    })
}

#[cfg(test)]
mod tests;
