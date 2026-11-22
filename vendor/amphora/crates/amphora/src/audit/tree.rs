//! RFC 6962's Merkle tree (Certificate Transparency, section 2.1) as a pure core: its hashes, the
//! inclusion and consistency proofs of sections 2.1.1 and 2.1.2, and their verification as RFC 9162
//! (section 2.1.3.2 and 2.1.4.2) states it. No storage: proofs are built from perfect subtrees'
//! hashes a caller's lookup gives, so the audit log's shell ([`super::AuditLog`]) reads them from
//! its rows and a benchmark from memory (AL.1, docs/design/audit.md).
//!
//! The RFC defines the tree's hash and its proofs recursively; here each is a loop that walks down
//! from the root, at most [`LEVELS`] steps, keeping what the recursion would return on the way back
//! (CLAUDE.md section 11).

use sha2::{Digest, Sha256};

/// A SHA-256 digest: RFC 6962 section 2.1 fixes the tree's hash as SHA-256, 32 bytes.
pub type Hash = [u8; 32];

/// The most levels under a root: a format bound, as tree sizes are u64, so a tree holds fewer than
/// 2^64 leaves, an inclusion proof at most 64 hashes and a compact range at most 64 subtrees.
pub const LEVELS: usize = 64;

/// RFC 6962 section 2.1's domain separation: 0x00 before a leaf, 0x01 before two children.
const LEAF: u8 = 0x00;
const NODE: u8 = 0x01;

/// The perfect subtree whose leaves are `index << level` up to `(index + 1) << level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Node {
    pub level: u8,
    pub index: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    #[error("leaf {index} is outside a tree of {size}")]
    OutOfRange { index: u64, size: u64 },
    #[error("tree size {old} isn't an earlier version of a tree of {size}")]
    NotEarlier { old: u64, size: u64 },
    /// A caller asked for a range that isn't a run of aligned perfect subtrees, or a frontier that
    /// isn't its size's: a fault in this module's callers, never in the data.
    #[error("leaves {start} to {end} aren't a tree's subtrees")]
    Shape { start: u64, end: u64 },
}

/// RFC 6962 section 2.1: MTH({d(0)}) = SHA-256(0x00 || d(0)).
#[must_use]
pub fn leaf_hash(data: &[u8]) -> Hash {
    let mut h = Sha256::new();
    h.update([LEAF]);
    h.update(data);
    h.finalize().into()
}

/// RFC 6962 section 2.1: SHA-256(0x01 || MTH(left) || MTH(right)).
#[must_use]
pub fn node_hash(left: &Hash, right: &Hash) -> Hash {
    let mut h = Sha256::new();
    h.update([NODE]);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// RFC 6962 section 2.1: the hash of an empty list is the hash of an empty string.
#[must_use]
pub fn empty_root() -> Hash {
    Sha256::digest([]).into()
}

/// The perfect subtrees, largest first, whose leaves are `start` up to `end`: RFC 6962's MTH of
/// that range is their hashes folded from the right. `start` must be a multiple of the first's
/// width, as every range a tree's split makes is.
pub fn subtrees(start: u64, end: u64) -> Result<Vec<Node>, TreeError> {
    let shape = TreeError::Shape { start, end };
    let mut left = end.checked_sub(start).ok_or(shape)?;
    let mut at = start;
    let mut out = Vec::with_capacity(LEVELS);
    // Each turn takes the highest bit of what is left: at most 64 turns.
    while let Some(level) = left.checked_ilog2() {
        let width = 1u64.checked_shl(level).ok_or(shape)?;
        if at.checked_rem(width) != Some(0) {
            return Err(shape);
        }
        out.push(Node {
            level: u8::try_from(level).map_err(|_| shape)?,
            index: at.checked_div(width).ok_or(shape)?,
        });
        at = at.checked_add(width).ok_or(shape)?;
        left = left.checked_sub(width).ok_or(shape)?;
    }
    Ok(out)
}

/// RFC 6962's MTH of leaves `start` up to `end` (not empty), from its perfect subtrees' hashes.
fn range_hash<E: From<TreeError>>(
    start: u64,
    end: u64,
    node: &mut impl FnMut(Node) -> Result<Hash, E>,
) -> Result<Hash, E> {
    let parts = subtrees(start, end)?;
    let mut hashes = Vec::with_capacity(parts.len());
    for part in parts {
        hashes.push(node(part)?);
    }
    fold(&hashes).ok_or_else(|| E::from(TreeError::Shape { start, end }))
}

/// Subtrees' hashes, largest first, folded from the right: MTH of their leaves together (RFC 6962
/// section 2.1, the split at the largest power of two below the size). `None` for no subtrees.
fn fold(hashes: &[Hash]) -> Option<Hash> {
    let (last, rest) = hashes.split_last()?;
    Some(
        rest.iter()
            .rev()
            .fold(*last, |acc, left| node_hash(left, &acc)),
    )
}

/// The tree's root over `size` leaves (RFC 6962's MTH), from its perfect subtrees' hashes.
pub fn root<E: From<TreeError>>(
    size: u64,
    mut node: impl FnMut(Node) -> Result<Hash, E>,
) -> Result<Hash, E> {
    if size == 0 {
        return Ok(empty_root());
    }
    range_hash(0, size, &mut node)
}

/// The largest power of two below `n`, for `n` of at least 2: where RFC 6962 splits a tree.
fn split(n: u64) -> Option<u64> {
    1u64.checked_shl(n.checked_sub(1)?.checked_ilog2()?)
}

/// RFC 6962 section 2.1.1's PATH(index, D[size]): the hashes beside the leaf's path, leaf first.
pub fn inclusion_proof<E: From<TreeError>>(
    index: u64,
    size: u64,
    mut node: impl FnMut(Node) -> Result<Hash, E>,
) -> Result<Vec<Hash>, E> {
    if index >= size {
        return Err(TreeError::OutOfRange { index, size }.into());
    }
    let (mut lo, mut hi) = (0u64, size);
    let mut proof = Vec::with_capacity(LEVELS);
    // Each turn at least halves hi - lo: at most 64 turns.
    while let Some(k) = hi.checked_sub(lo).and_then(split) {
        let mid = lo
            .checked_add(k)
            .ok_or(TreeError::Shape { start: lo, end: hi })?;
        if index < mid {
            proof.push(range_hash(mid, hi, &mut node)?);
            hi = mid;
        } else {
            proof.push(range_hash(lo, mid, &mut node)?);
            lo = mid;
        }
    }
    proof.reverse();
    Ok(proof)
}

/// RFC 6962 section 2.1.2's PROOF(old, D[size]) for 0 < old < size; empty for old == size.
pub fn consistency_proof<E: From<TreeError>>(
    old: u64,
    size: u64,
    mut node: impl FnMut(Node) -> Result<Hash, E>,
) -> Result<Vec<Hash>, E> {
    if old == 0 || old > size {
        return Err(TreeError::NotEarlier { old, size }.into());
    }
    let (mut lo, mut hi) = (0u64, size);
    // SUBPROOF's flag: whether the old tree's root is still this subtree's, so its hash is known.
    let mut whole = true;
    let mut proof = Vec::with_capacity(LEVELS);
    let shape = |lo, hi| TreeError::Shape { start: lo, end: hi };
    // Each turn at least halves hi - lo: at most 64 turns.
    loop {
        let width = hi.checked_sub(lo).ok_or(shape(lo, hi))?;
        let m = old.checked_sub(lo).ok_or(shape(lo, hi))?;
        if m == width {
            if !whole {
                proof.push(range_hash(lo, hi, &mut node)?);
            }
            break;
        }
        let k = split(width).ok_or(shape(lo, hi))?;
        let mid = lo.checked_add(k).ok_or(shape(lo, hi))?;
        if m <= k {
            proof.push(range_hash(mid, hi, &mut node)?);
            hi = mid;
        } else {
            proof.push(range_hash(lo, mid, &mut node)?);
            lo = mid;
            whole = false;
        }
    }
    proof.reverse();
    Ok(proof)
}

fn half(n: u64) -> u64 {
    n.checked_shr(1).unwrap_or(0)
}

/// RFC 9162 section 2.1.3.2: whether `proof` shows the leaf hashed `leaf` at `index` in the tree of
/// `size` leaves whose root is `root`.
#[must_use]
pub fn verify_inclusion(index: u64, size: u64, leaf: &Hash, proof: &[Hash], root: &Hash) -> bool {
    let Some(mut sn) = size.checked_sub(1) else {
        return false;
    };
    if index >= size || proof.len() > LEVELS {
        return false;
    }
    let mut fn_ = index;
    let mut r = *leaf;
    for p in proof {
        if sn == 0 {
            return false;
        }
        if fn_ & 1 == 1 || fn_ == sn {
            r = node_hash(p, &r);
            while fn_ & 1 == 0 && fn_ != 0 {
                fn_ = half(fn_);
                sn = half(sn);
            }
        } else {
            r = node_hash(&r, p);
        }
        fn_ = half(fn_);
        sn = half(sn);
    }
    sn == 0 && r == *root
}

/// RFC 9162 section 2.1.4.2: whether `proof` shows the tree of `size` leaves rooted at `root`
/// extends the tree of `old` leaves rooted at `old_root`, for 0 < old <= size.
#[must_use]
pub fn verify_consistency(
    old: u64,
    size: u64,
    old_root: &Hash,
    root: &Hash,
    proof: &[Hash],
) -> bool {
    if old == 0 || old > size {
        return false;
    }
    if old == size {
        return proof.is_empty() && old_root == root;
    }
    // At most a path's 64 hashes and the old root's subtree.
    if proof.len() > LEVELS.saturating_add(1) {
        return false;
    }
    let (first, rest) = if old.is_power_of_two() {
        (old_root, proof)
    } else {
        match proof.split_first() {
            Some(split) => split,
            None => return false,
        }
    };
    let (Some(mut fn_), Some(mut sn)) = (old.checked_sub(1), size.checked_sub(1)) else {
        return false;
    };
    while fn_ & 1 == 1 {
        fn_ = half(fn_);
        sn = half(sn);
    }
    let (mut fr, mut sr) = (*first, *first);
    for c in rest {
        if sn == 0 {
            return false;
        }
        if fn_ & 1 == 1 || fn_ == sn {
            fr = node_hash(c, &fr);
            sr = node_hash(c, &sr);
            while fn_ & 1 == 0 && fn_ != 0 {
                fn_ = half(fn_);
                sn = half(sn);
            }
        } else {
            sr = node_hash(&sr, c);
        }
        fn_ = half(fn_);
        sn = half(sn);
    }
    fr == *old_root && sr == *root && sn == 0
}

/// A tree's right edge: the hashes of its compact range, the perfect subtrees [`subtrees`] gives for
/// `0..size`. Appending a leaf completes the subtrees it closes, which an append writes once and
/// proofs read ever after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontier {
    size: u64,
    nodes: Vec<(Node, Hash)>,
}

impl Frontier {
    /// The frontier of a tree of `size` leaves, from its compact range's hashes in [`subtrees`]'s
    /// order.
    pub fn new(size: u64, hashes: &[Hash]) -> Result<Self, TreeError> {
        let parts = subtrees(0, size)?;
        if parts.len() != hashes.len() {
            return Err(TreeError::Shape {
                start: 0,
                end: size,
            });
        }
        Ok(Self {
            size,
            nodes: parts.into_iter().zip(hashes.iter().copied()).collect(),
        })
    }

    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub fn root(&self) -> Hash {
        let hashes: Vec<Hash> = self.nodes.iter().map(|(_, h)| *h).collect();
        fold(&hashes).unwrap_or_else(empty_root)
    }

    /// Appends a leaf whose hash is `leaf`; each perfect subtree of level 1 or more it completes is
    /// pushed onto `completed` with its hash, the leaf itself first as level 0.
    pub fn push(&mut self, leaf: Hash, completed: &mut Vec<(Node, Hash)>) -> Result<(), TreeError> {
        let shape = TreeError::Shape {
            start: 0,
            end: self.size,
        };
        let mut top = (
            Node {
                level: 0,
                index: self.size,
            },
            leaf,
        );
        completed.push(top);
        // The compact range's levels fall from left to right, so merges happen only at its end:
        // at most 64 of them.
        while let Some(&(left, left_hash)) = self.nodes.last() {
            if left.level != top.0.level {
                break;
            }
            self.nodes.pop();
            let level = left.level.checked_add(1).ok_or(shape)?;
            top = (
                Node {
                    level,
                    index: left.index.checked_div(2).ok_or(shape)?,
                },
                node_hash(&left_hash, &top.1),
            );
            completed.push(top);
        }
        self.nodes.push(top);
        self.size = self.size.checked_add(1).ok_or(shape)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn hex(s: &str) -> Hash {
        bytes(s).try_into().unwrap()
    }

    fn bytes(s: &str) -> Vec<u8> {
        s.as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    /// The leaves of Certificate Transparency's reference tests (transparency-dev/merkle at
    /// 75a6539, testonly/constants.go: LeafInputs).
    fn ct_leaves() -> Vec<Vec<u8>> {
        [
            "",
            "00",
            "10",
            "2021",
            "3031",
            "40414243",
            "5051525354555657",
            "606162636465666768696a6b6c6d6e6f",
        ]
        .iter()
        .map(|s| bytes(s))
        .collect()
    }

    /// Their roots for sizes 0 to 8 (the same file's RootHashes).
    const CT_ROOTS: [&str; 9] = [
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
        "fac54203e7cc696cf0dfcb42c92a1d9dbaf70ad9e621f4bd8d98662f00e3c125",
        "aeb6bcfe274b70a14fb067a5e5578264db0fa9b51af5e0ba159158f329e06e77",
        "d37ee418976dd95753c1c73862b9398fa2a2cf9b4ff0fdfe8b30cd95209614b7",
        "4e3bbb1f7b478dcfe71fb631631519a3bca12c9aefca1612bfce4c13a86264d4",
        "76e67dadbcdf1e10e1b74ddc608abd2f98dfb16fbce75277b5232a127f2087ef",
        "ddb89be403809e325750d3d263cd78929c2942b7942a34b77e122c9594a74c8c",
        "5dc9da79a70659a9ad559cb701ded9a2ab9d823aad2f4960cfe370eff4604328",
    ];

    /// Every perfect subtree's hash, built by a [`Frontier`] leaf by leaf, as a store keeps them.
    fn build(leaves: &[Vec<u8>]) -> (Frontier, HashMap<Node, Hash>) {
        let mut frontier = Frontier::new(0, &[]).unwrap();
        let mut nodes = HashMap::new();
        for leaf in leaves {
            let mut completed = Vec::new();
            frontier.push(leaf_hash(leaf), &mut completed).unwrap();
            nodes.extend(completed);
        }
        (frontier, nodes)
    }

    fn lookup(nodes: &HashMap<Node, Hash>) -> impl FnMut(Node) -> Result<Hash, TreeError> + '_ {
        |n| {
            nodes.get(&n).copied().ok_or(TreeError::Shape {
                start: n.index,
                end: n.index,
            })
        }
    }

    /// An oracle apart from the code under test: RFC 6962's tree built level by level, an unpaired
    /// last node carried up unchanged (the tree the RFC's split at the largest power of two makes).
    fn oracle_root(leaves: &[Vec<u8>]) -> Hash {
        if leaves.is_empty() {
            return empty_root();
        }
        let mut level: Vec<Hash> = leaves.iter().map(|l| leaf_hash(l)).collect();
        while level.len() > 1 {
            level = level
                .chunks(2)
                .map(|p| {
                    if p.len() == 2 {
                        node_hash(&p[0], &p[1])
                    } else {
                        p[0]
                    }
                })
                .collect();
        }
        level[0]
    }

    /// RFC 6962 section 2.1.1's PATH as sides: at each split from the root down, whether the leaf
    /// is in the left part. Two sizes whose paths share these sides give the same proof hashes in
    /// the same order.
    fn path_shape(index: u64, size: u64) -> Vec<bool> {
        let (mut lo, mut hi) = (0u64, size);
        let mut sides = Vec::new();
        while hi - lo > 1 {
            let k = 1u64 << (hi - lo - 1).ilog2();
            let left = index < lo + k;
            sides.push(left);
            if left {
                hi = lo + k;
            } else {
                lo += k;
            }
        }
        sides
    }

    fn leaves(n: usize) -> Vec<Vec<u8>> {
        (0..n).map(|i| format!("event {i}").into_bytes()).collect()
    }

    /// AL.1 (T-AL.1.2): do build Certificate Transparency's reference tree leaf by leaf; expect its
    /// published root at every size, each perfect subtree's published hash, and the published
    /// paths and proofs of its 8-leaf tree.
    #[test]
    fn al1_ct_reference_vectors() {
        let all = ct_leaves();
        for (size, want) in CT_ROOTS.iter().enumerate() {
            let (frontier, nodes) = build(&all[..size]);
            assert_eq!(frontier.root(), hex(want), "frontier root at {size}");
            assert_eq!(
                root(size as u64, lookup(&nodes)).unwrap(),
                hex(want),
                "root at {size}"
            );
        }
        let (_, nodes) = build(&all);
        let level1_1 = hex("5f083f0a1a33ca076a95279832580db3e0ef4584bdff1f54c8a360f50de3031e");
        let level2_1 = hex("6b47aaf29ee3c2af9af889bc1fb9254dabd31177f16232dd6aab035ca39bf6e4");
        assert_eq!(nodes[&Node { level: 1, index: 1 }], level1_1);
        assert_eq!(nodes[&Node { level: 2, index: 1 }], level2_1);
        let leaf1 = hex("96a296d224f285c67bee93c30f8a309157f0daa35dc5b87e410b78630a09cfc7");
        assert_eq!(
            inclusion_proof(0, 8, lookup(&nodes)).unwrap(),
            vec![leaf1, level1_1, level2_1]
        );
        assert_eq!(
            consistency_proof(1, 8, lookup(&nodes)).unwrap(),
            vec![leaf1, level1_1, level2_1]
        );
        assert_eq!(
            consistency_proof(4, 8, lookup(&nodes)).unwrap(),
            vec![level2_1]
        );
    }

    /// AL.1 (T-AL.1.3): do prove every leaf of every tree up to 70 leaves, and every earlier size of
    /// each; expect each proof to verify against the oracle's roots, and to fail with any one hash
    /// changed, with the wrong index or leaf, with a size of another path shape (RFC 9162 section
    /// 2.1.3.2 reads the size only for the shape), or against another root.
    #[test]
    fn al1_every_proof_verifies_and_every_altered_one_fails() {
        let all = leaves(70);
        let roots: Vec<Hash> = (0..=all.len()).map(|n| oracle_root(&all[..n])).collect();
        let (_, nodes) = build(&all);
        let mut checked = 0u64;
        for n in 1..=all.len() {
            let size = n as u64;
            let r = &roots[n];
            assert_eq!(root(size, lookup(&nodes)).unwrap(), *r);
            for (i, event) in all.iter().enumerate().take(n) {
                let index = i as u64;
                let leaf = leaf_hash(event);
                let proof = inclusion_proof(index, size, lookup(&nodes)).unwrap();
                assert!(verify_inclusion(index, size, &leaf, &proof, r));
                for i in 0..proof.len() {
                    let mut bad = proof.clone();
                    bad[i][0] ^= 1;
                    assert!(!verify_inclusion(index, size, &leaf, &bad, r));
                }
                assert!(!verify_inclusion(
                    index,
                    size,
                    &leaf_hash(b"other"),
                    &proof,
                    r
                ));
                if size > 1 {
                    assert!(!verify_inclusion(
                        (index + 1) % size,
                        size,
                        &leaf,
                        &proof,
                        r
                    ));
                }
                // RFC 9162 section 2.1.3.2 reads the size only for the path's shape, so a size whose
                // path has the same shape (leaf 0 of 3 and of 4) verifies against the same root: the
                // signed root binds the size, the proof doesn't. A size of another shape fails; the
                // larger tree's own root fails whatever the shape.
                assert_eq!(
                    verify_inclusion(index, size + 1, &leaf, &proof, r),
                    path_shape(index, size) == path_shape(index, size + 1),
                    "leaf {index} of {size} checked as one of {}",
                    size + 1
                );
                if let Some(larger) = roots.get(n + 1) {
                    assert!(!verify_inclusion(index, size + 1, &leaf, &proof, larger));
                }
                checked += 1;
            }
            for m in 1..=n {
                let old = m as u64;
                let o = &roots[m];
                let proof = consistency_proof(old, size, lookup(&nodes)).unwrap();
                assert!(
                    verify_consistency(old, size, o, r, &proof),
                    "{old} -> {size}"
                );
                for i in 0..proof.len() {
                    let mut bad = proof.clone();
                    bad[i][31] ^= 0x80;
                    assert!(!verify_consistency(old, size, o, r, &bad));
                }
                if old < size {
                    assert!(!verify_consistency(old, size, &roots[m - 1], r, &proof));
                    assert!(!verify_consistency(old, size, o, &roots[n - 1], &proof));
                }
                checked += 1;
            }
        }
        // Non-vacuity: 70 trees' leaves and earlier sizes, each pair checked once.
        assert_eq!(checked, 2 * (70 * 71 / 2));
    }

    /// AL.1: do ask for proofs outside the tree; expect typed refusals.
    #[test]
    fn al1_proofs_outside_the_tree_are_refused() {
        let (_, nodes) = build(&leaves(5));
        assert_eq!(
            inclusion_proof(5, 5, lookup(&nodes)),
            Err(TreeError::OutOfRange { index: 5, size: 5 })
        );
        assert_eq!(
            consistency_proof(0, 5, lookup(&nodes)),
            Err(TreeError::NotEarlier { old: 0, size: 5 })
        );
        assert_eq!(
            consistency_proof(6, 5, lookup(&nodes)),
            Err(TreeError::NotEarlier { old: 6, size: 5 })
        );
        assert!(!verify_inclusion(0, 0, &empty_root(), &[], &empty_root()));
    }

    /// AL.1: do append leaves; expect each append to complete exactly the subtrees its new size's
    /// trailing zero bits say (one leaf hash, then one node per closed level), so an append writes
    /// 1 + trailing_zeros(size) hashes and n appends about 2n.
    #[test]
    fn al1_an_append_completes_its_trailing_subtrees() {
        let mut frontier = Frontier::new(0, &[]).unwrap();
        let mut total = 0usize;
        for i in 0..1024u64 {
            let mut completed = Vec::new();
            frontier
                .push(leaf_hash(&i.to_be_bytes()), &mut completed)
                .unwrap();
            assert_eq!(completed.len(), 1 + (i + 1).trailing_zeros() as usize);
            total += completed.len();
        }
        assert_eq!(total, 2 * 1024 - 1);
        assert_eq!(
            subtrees(0, 1024).unwrap(),
            vec![Node {
                level: 10,
                index: 0
            }]
        );
        assert_eq!(
            subtrees(0, 6).unwrap(),
            vec![Node { level: 2, index: 0 }, Node { level: 1, index: 2 }]
        );
        assert!(subtrees(2, 6).is_err());
    }
}
