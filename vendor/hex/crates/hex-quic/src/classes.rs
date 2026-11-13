//! What hyper-transport is told of Hex's frames (docs/design/hex-quic.md Q3): their kinds and classes are the
//! lanes' own ([`hex_core::lanes`]), each class's frames at most one data frame's bound, and every class
//! lossless, as a frame here is its sender's only copy: one the budget can't take now waits, its lane unread,
//! so QUIC's flow control holds its sender, rather than being skipped (CLAUDE.md §3).

use hex_core::lanes::{self, Class, Kind};
use hyper_transport::Classes;

/// Hex's frames, as hyper-transport's `Classes`.
pub struct Hex;

/// A sender's role: an enrolled member of the fleet, a node or kiln's balancer alike, peers of each other
/// (the plan's directive: "Hex and kiln's load balancer are peer to peer"). Every member may send every kind;
/// what a connection may do is its port's to say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Member,
}

/// The most bytes a frame of any class takes: a data frame's bound, which every connection's own frames
/// (at most 12 bytes) are within.
pub fn frame_bound() -> u64 {
    u64::try_from(lanes::DATA_BOUND).unwrap_or(u64::MAX)
}

impl Classes for Hex {
    type Class = Class;
    type Kind = Kind;
    type Role = Role;
    const RANKS: u8 = Class::RANKS;

    fn rank(class: Class) -> u8 {
        class.rank()
    }

    fn class_of(kind: Kind, role: Role) -> Option<Class> {
        match role {
            Role::Member => Some(kind.class()),
        }
    }

    fn frame_bound(_class: Class) -> u64 {
        frame_bound()
    }

    fn kind_code(kind: Kind) -> u16 {
        kind.code()
    }

    fn kind_of(code: u16) -> Option<Kind> {
        Kind::of(code)
    }

    fn lossless(_class: Class) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HEX-QUIC-9 (CP-T7.3): each kind's code names it again through hyper-transport's mapping, every kind a
    /// member sends has its class, and every class is lossless.
    #[test]
    fn hex_quic_9_every_kind_maps_to_its_class_and_code_and_nothing_is_skipped() {
        for kind in Kind::ALL {
            assert_eq!(<Hex as Classes>::kind_of(Hex::kind_code(kind)), Some(kind));
            let class = Hex::class_of(kind, Role::Member);
            assert_eq!(class, Some(kind.class()));
            assert!(Hex::lossless(kind.class()));
            assert!(Hex::rank(kind.class()) < Hex::RANKS);
        }
        assert!(frame_bound() >= 12, "a connection's own frames fit");
    }
}
