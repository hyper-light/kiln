//! Groups that split, merge and move themselves (CP-T1.9; AC-T1.9.1 to AC-T1.9.3;
//! docs/design/reshard.md). A pure core, with no clock, disk or network (CLAUDE.md §9):
//!
//! - [`directory`]: which group holds each position of the key space, where each group is placed,
//!   and the operations under way. Every position is in exactly one group's span, always: the one
//!   commit point of an operation is its flip here.
//! - [`holding`]: what one group holds, kept in its own state machine: the span it answers for, the
//!   part of it fenced for a cutover, and the span it is taking in. A group answers a key only from
//!   its own holding, so a router with a stale directory can never write to the wrong group.
//! - [`plan`]: when to split, merge or move, from each group's measured size and load and each
//!   node's measured cores: no threshold picked by anyone.
//! - [`driver`]: each operation's steps, copy, then catch-up in rounds, then a fenced cutover,
//!   as actions a shell carries out and answers.
//!
//! A key's position is its FNV-1a hash, finished so every byte reaches its high bits
//! ([`position`]), so a monotonic key (kiln's time-ordered ids) spreads over every group instead of
//! piling onto the last.

pub mod directory;
pub mod driver;
pub mod holding;
pub mod plan;
pub mod replicated;

/// A key's place in the key space.
pub type Pos = u64;
/// A place in a group's log of changes: the order its writes were made in.
pub type Seq = u64;

/// A group: the unit with one writer, one log and one place. Ids are never used again.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct GroupId(pub u64);

/// A machine groups are placed on.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct NodeId(pub u64);

/// An operation: a split, a merge or a move. Ids grow from 1, and are never used again (0: none).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct OpId(pub u64);

/// `key`'s position: its FNV-1a hash, finished by MurmurHash3's 64-bit finalizer (`fmix64`,
/// Austin Appleby, SMHasher's `MurmurHash3.cpp`; its shifts and multipliers are that function's,
/// not tuned here). Spans are runs of positions, so a position's high bits must depend on every
/// byte of the key: FNV-1a's last multiply carries a change in the key's last byte only into its
/// low bits and bits 40 to 56, so keys alike but for their ends (kiln's time-ordered ids, `k0` to
/// `k19`) would all fall in one span. The finalizer is a bijection, so no two hashes meet that
/// didn't already.
#[must_use]
pub fn position(key: &str) -> Pos {
    position_of(key.as_bytes())
}

/// [`position`] of a key of bytes: a Records row's routing key ([`row_key`]) as well as a string.
#[must_use]
pub fn position_of(key: &[u8]) -> Pos {
    let mut h = crate::groups::fnv_bytes(key);
    h ^= h.wrapping_shr(33);
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h.wrapping_shr(33);
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ h.wrapping_shr(33)
}

/// A Records row's routing key: its namespace, family and key, as Records addresses a row
/// (docs/design/reshard.md, decision "A row routes by its namespace, family and key"). Each of the
/// first two is preceded by its length as a little-endian u32 (a format value: it makes the
/// three parts one unambiguous string of bytes), and the key ends it. None for a namespace or
/// family longer than a u32 counts, which Records' own bounds refuse long before.
#[must_use]
pub fn row_key(namespace: &str, family: &str, key: &[u8]) -> Option<Vec<u8>> {
    let ns = u32::try_from(namespace.len()).ok()?;
    let fam = u32::try_from(family.len()).ok()?;
    let mut out = Vec::with_capacity(
        namespace
            .len()
            .checked_add(family.len())?
            .checked_add(key.len())?
            .checked_add(8)?,
    );
    out.extend_from_slice(&ns.to_le_bytes());
    out.extend_from_slice(namespace.as_bytes());
    out.extend_from_slice(&fam.to_le_bytes());
    out.extend_from_slice(family.as_bytes());
    out.extend_from_slice(key);
    Some(out)
}

/// The namespace, family and key of a [`row_key`] (None: bytes no `row_key` made).
#[must_use]
pub fn row_parts(row_key: &[u8]) -> Option<(String, String, Vec<u8>)> {
    fn part(at: &[u8]) -> Option<(String, &[u8])> {
        let (len, rest) = at.split_first_chunk::<4>()?;
        let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
        let (text, rest) = rest.split_at_checked(len)?;
        Some((String::from_utf8(text.to_vec()).ok()?, rest))
    }
    let (namespace, rest) = part(row_key)?;
    let (family, key) = part(rest)?;
    Some((namespace, family, key.to_vec()))
}

/// Positions `lo` to `hi`, both included: never empty.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Span {
    lo: Pos,
    hi: Pos,
}

impl Span {
    /// Every position.
    pub const ALL: Self = Self {
        lo: 0,
        hi: Pos::MAX,
    };

    /// `lo` to `hi`, if `lo` is at most `hi`.
    #[must_use]
    pub fn new(lo: Pos, hi: Pos) -> Option<Self> {
        if lo <= hi {
            Some(Self { lo, hi })
        } else {
            None
        }
    }

    #[must_use]
    pub fn lo(&self) -> Pos {
        self.lo
    }

    #[must_use]
    pub fn hi(&self) -> Pos {
        self.hi
    }

    #[must_use]
    pub fn contains(&self, pos: Pos) -> bool {
        self.lo <= pos && pos <= self.hi
    }

    /// Whether every position of `other` is in this one.
    #[must_use]
    pub fn covers(&self, other: &Self) -> bool {
        self.lo <= other.lo && other.hi <= self.hi
    }

    /// Whether the two share a position.
    #[must_use]
    pub fn meets(&self, other: &Self) -> bool {
        self.lo <= other.hi && other.lo <= self.hi
    }

    /// Cut before `at`: `lo` to `at - 1`, and `at` to `hi`; none unless `at` is past `lo` and in
    /// the span, so both sides hold a position.
    #[must_use]
    pub fn cut(&self, at: Pos) -> Option<(Self, Self)> {
        if at <= self.lo || at > self.hi {
            return None;
        }
        let left_hi = at.checked_sub(1)?;
        Some((
            Self {
                lo: self.lo,
                hi: left_hi,
            },
            Self {
                lo: at,
                hi: self.hi,
            },
        ))
    }

    /// The two as one, if one ends just before the other begins.
    #[must_use]
    pub fn join(&self, other: &Self) -> Option<Self> {
        let (first, second) = if self.lo <= other.lo {
            (self, other)
        } else {
            (other, self)
        };
        if first.hi.checked_add(1) == Some(second.lo) {
            Some(Self {
                lo: first.lo,
                hi: second.hi,
            })
        } else {
            None
        }
    }

    /// What is left of this span without `part`, which must be all of it, or a part of it that
    /// begins or ends with it (`Ok(None)`: nothing left). Anything else is refused.
    pub fn without(&self, part: &Self) -> Result<Option<Self>, Refused> {
        if !self.covers(part) {
            return Err(Refused::Invalid(
                "the part taken out is not inside the span",
            ));
        }
        match (part.lo == self.lo, part.hi == self.hi) {
            (true, true) => Ok(None),
            (true, false) => Ok(part.hi.checked_add(1).map(|lo| Self { lo, hi: self.hi })),
            (false, true) => Ok(part.lo.checked_sub(1).map(|hi| Self { lo: self.lo, hi })),
            (false, false) => Err(Refused::Invalid(
                "the part taken out would leave the span in two",
            )),
        }
    }
}

/// What the core refuses, and why. Nothing is changed by a refused request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// The group is already in an operation: one at a time each.
    #[error("group {0:?} is already in an operation")]
    Busy(GroupId),
    /// The directory has no such group.
    #[error("no group {0:?}")]
    NoGroup(GroupId),
    /// The directory has no such operation.
    #[error("no operation {0:?}")]
    NoOp(OpId),
    /// The operation isn't at the stage the request needs.
    #[error("operation {op:?} is not {needs}")]
    Stage { op: OpId, needs: &'static str },
    /// A request from an operation that has ended here (a stale or repeated request).
    #[error("operation {0:?} has ended here")]
    Ended(OpId),
    /// A request that doesn't fit what is held.
    #[error("{0}")]
    Invalid(&'static str),
    /// The spans would stop covering every position exactly once: the change is refused whole.
    #[error("the directory would break: {0}")]
    Broken(&'static str),
}
