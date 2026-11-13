//! One device program per device policy, shared by every container with that policy (decision 2 of
//! docs/design/runtime-architecture.md). A device program is stateless: it decides an access from the
//! device's type and numbers and the access asked, so one loaded copy attached to many cgroups decides
//! for each exactly as a copy of its own would. A copy per container held a vmalloc'd page, 1.2 KiB of
//! per-cpu stats and slab beside them: 6.3-7.4 KiB of kernel memory per idle container, which a shared
//! program pays once per node (bench/competitive/RESULT.md, "What closed it").
//!
//! Two holders keep the programs (goal 5 extended, step 1). The node agent outlives its containers, so it
//! keeps each policy's program open once loaded ([`Programs`]) and a create only attaches it; on a root
//! node the default policy's program sits once on the oci boxes' root, so a box with that policy attaches
//! nothing ([`give`]). The command line runs as a process per container, so it remembers each policy's
//! program by id in the hints file ([`Hints`]) and looks there before it loads: the program a hint names
//! is attached only once the kernel shows its translated instructions are the policy's byte for byte
//! ([`is_policy`]), else a copy is loaded and remembered. A hint is only a hint: a stale, missing or
//! planted one costs a load, never the policy.
//!
//! This is the pure core: Hex's digest of a policy, the hints' format and bound, the match, the held
//! programs and which program decides for a box. `run.rs` and `programs.rs` drive it.

use serde::{Deserialize, Serialize};

use super::OciError;
use super::resources::{Insn, allow_all};
use super::sys::{BPF_PROG_TYPE_CGROUP_DEVICE, ProgramIdentity};

/// The hints file, in the runtime's state root.
pub const HINTS_FILE: &str = ".device-programs.json";

/// The most programs the command line's hints file names (CLAUDE.md §2): one per distinct device policy it
/// loaded, past which the one remembered longest ago goes. The file is read whole by every `hex oci create`, so its
/// bound is a read's size ([`MAX_HINT_BYTES`]), not a node's; a node agent's held programs are bounded by its host
/// (`hex_node::sizes::Sizes::programs`).
pub const MAX_HINTS: usize = 64;

/// The most bytes of the hints file read: 64 programs take under 3 KiB, so a larger file isn't one
/// this module wrote, and is read as no hints.
pub const MAX_HINT_BYTES: u64 = 64 * 1024;

/// Hex's own digest of a policy's program: 64-bit FNV-1a (its offset basis and prime, IETF
/// draft-eastlake-fnv §2.1) over the instructions' bytes as the kernel dumps them. It keys the hints and
/// the node's held programs, so a program is found before anything is loaded, which the kernel's tag
/// can't do (a program's tag is known only once it's loaded). A key, not a proof: a hint is checked
/// against the kernel's translation, and a held program against its instructions, before either is
/// attached, so two policies whose digests collide only miss each other's program.
pub fn digest(insns: &[Insn]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    insns
        .iter()
        .flat_map(Insn::bytes)
        .fold(OFFSET_BASIS, |h, b| (h ^ u64::from(b)).wrapping_mul(PRIME))
}

/// A program remembered: the digest of its policy, as 16 hex digits, and its id when it was remembered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hint {
    pub digest: String,
    pub id: u32,
}

/// The programs remembered, the most recently remembered first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hints {
    pub programs: Vec<Hint>,
}

impl Hints {
    /// The hints `bytes` hold. Bytes that don't parse are no hints, a file an older Hex keyed by the
    /// kernel's tag among them: a hint is never used unchecked, and the next program remembered rewrites
    /// the file whole.
    pub fn parse(bytes: &[u8]) -> Self {
        let mut hints = serde_json::from_slice::<Self>(bytes).unwrap_or_default();
        hints.programs.truncate(MAX_HINTS);
        hints
    }

    /// The id remembered for the policy with `digest`, if any.
    pub fn candidate(&self, digest: u64) -> Option<u32> {
        let digest = hex(digest);
        self.programs
            .iter()
            .find(|h| h.digest == digest)
            .map(|h| h.id)
    }

    /// Remembers program `id` for the policy with `digest` first, in place of any earlier one for it,
    /// keeping at most [`MAX_HINTS`].
    pub fn remember(&mut self, digest: u64, id: u32) {
        let digest = hex(digest);
        self.programs.retain(|h| h.digest != digest);
        self.programs.insert(0, Hint { digest, id });
        self.programs.truncate(MAX_HINTS);
    }

    /// The hints as the file holds them.
    pub fn to_bytes(&self) -> Result<Vec<u8>, OciError> {
        serde_json::to_vec(self)
            .map_err(|e| OciError::Broken(format!("encoding the device program hints: {e}")))
    }
}

/// Whether a loaded program, as the kernel describes it (`identity`, and its translated instructions
/// when it dumps them), is the program for `insns`: a cgroup device program whose translation is `insns`
/// byte for byte. A device program decides from its instructions alone, so that proves it decides as a
/// copy of `insns` would. The kernel's translation of Hex's device program is the program itself on the
/// kernels it runs on (research f55: 800 of 800 bytes equal on fedora); a kernel that rewrites it, or
/// withholds its dump, matches nothing, and the caller loads a copy of its own.
pub fn is_policy(identity: &ProgramIdentity, translation: Option<&[u8]>, insns: &[Insn]) -> bool {
    let len = insns.len().checked_mul(8);
    identity.prog_type == BPF_PROG_TYPE_CGROUP_DEVICE
        && len.is_some_and(|n| u32::try_from(n).is_ok_and(|n| n == identity.xlated_len))
        && !insns.is_empty()
        && translation.is_some_and(|t| t.iter().copied().eq(insns.iter().flat_map(Insn::bytes)))
}

/// The device programs a node agent holds, one per policy, the most recently used first: each kept
/// open once loaded, so a create with its policy only attaches it (research f60). At most `bound`; past
/// it the program used longest ago is handed back to be closed. A program attached to a cgroup lives on
/// there once its holder closes it (f53), so every box keeps its policy.
#[derive(Debug)]
pub struct Programs<P> {
    bound: usize,
    held: Vec<Held<P>>,
}

#[derive(Debug)]
struct Held<P> {
    digest: u64,
    insns: Vec<Insn>,
    program: P,
}

impl<P> Programs<P> {
    pub fn new(bound: usize) -> Self {
        Self {
            bound,
            held: Vec::new(),
        }
    }

    /// The program held for `insns`, made the most recently used. Its digest finds it and its
    /// instructions prove it, so a policy whose digest collides with another's finds nothing.
    pub fn get(&mut self, insns: &[Insn]) -> Option<&P> {
        let d = digest(insns);
        let at = self
            .held
            .iter()
            .position(|h| h.digest == d && h.insns == insns)?;
        let held = self.held.remove(at);
        self.held.insert(0, held);
        self.held.first().map(|h| &h.program)
    }

    /// Holds `program` for `insns` as the most recently used, in place of any held under the same
    /// digest, and hands back every program that puts past the bound, the one used longest ago first,
    /// for the caller to close.
    pub fn put(&mut self, insns: Vec<Insn>, program: P) -> Vec<P> {
        let d = digest(&insns);
        let mut out: Vec<P> = Vec::new();
        if let Some(at) = self.held.iter().position(|h| h.digest == d) {
            out.push(self.held.remove(at).program);
        }
        self.held.insert(
            0,
            Held {
                digest: d,
                insns,
                program,
            },
        );
        while self.held.len() > self.bound {
            match self.held.pop() {
                Some(h) => out.push(h.program),
                None => break,
            }
        }
        out
    }

    /// How many programs are held.
    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

/// What a box's cgroup is given for its device policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Give {
    /// Nothing: the root's program decides for the box, or no program anywhere does, which allows every
    /// device, as the policy asks.
    Nothing,
    /// This program, attached to the box's cgroup as its own, which decides for it in the root's place.
    Program(Vec<Insn>),
}

/// What a box with device policy `policy` (its program, or `None` for a policy that allows every device)
/// is given under a root whose program is `root` (the oci boxes' root on a root node carries the default
/// policy's, attached with `BPF_F_ALLOW_OVERRIDE`) or under none. A box with the root's policy attaches
/// nothing (research f40-f42); a box with an allow-all policy carries the allow-all program under a root
/// that has one, so it never takes the root's default (f43).
pub fn give(policy: Option<&[Insn]>, root: Option<&[Insn]>) -> Give {
    match (policy, root) {
        (Some(p), Some(r)) if p == r => Give::Nothing,
        (None, None) => Give::Nothing,
        (Some(p), _) => Give::Program(p.to_vec()),
        (None, Some(_)) => Give::Program(allow_all()),
    }
}

fn hex(digest: u64) -> String {
    format!("{digest:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::oci::resources::{default_program, devices};
    use crate::linux::oci::spec::DeviceRule;
    use proptest::prelude::*;

    fn identity(xlated_len: u32) -> ProgramIdentity {
        ProgramIdentity {
            prog_type: BPF_PROG_TYPE_CGROUP_DEVICE,
            id: 3,
            tag: [0xaa; 8],
            jited_len: 120,
            xlated_len,
        }
    }

    /// A policy of its own for each `major`: the default's devices and char `major`:* besides.
    fn policy(major: i64) -> Vec<Insn> {
        devices(&[DeviceRule {
            allow: true,
            kind: Some("c".to_owned()),
            major: Some(major),
            minor: None,
            access: Some("rw".to_owned()),
        }])
        .unwrap()
        .unwrap()
    }

    // AC-T7.1.5: do X: take FNV-1a of known inputs, and Hex's digest of two policies; expect Y: the published
    // vectors (draft-eastlake-fnv's test suite: "" and "a"), the same digest for the same instructions
    // whenever it's taken, and different ones for different policies (the hints and the held programs
    // are keyed by it).
    #[test]
    fn the_digest_is_fnv_1a_of_the_instructions() {
        assert_eq!(digest(&[]), 0xcbf2_9ce4_8422_2325, "FNV-1a 64 of nothing");
        let a = Insn {
            code: b'a',
            regs: 0,
            off: 0,
            imm: 0,
        };
        // FNV-1a 64 of "a" is af63dc4c8601ec8c; of "a" and seven zero bytes, step by step from it.
        let mut want: u64 = 0xaf63_dc4c_8601_ec8c;
        for _ in 0..7 {
            want = want.wrapping_mul(0x0000_0100_0000_01b3);
        }
        assert_eq!(digest(&[a]), want);
        let default = default_program().unwrap();
        assert_eq!(digest(&default), digest(&default_program().unwrap()));
        assert_ne!(digest(&default), digest(&policy(1)));
        assert_ne!(digest(&policy(1)), digest(&policy(2)));
    }

    // AC-T7.1.5: do X: remember programs for digests, then ask for each; expect Y: the latest id per digest,
    // newest first, one entry per digest (decision 2: one program per distinct device policy).
    #[test]
    fn a_digest_names_the_program_remembered_last_for_it() {
        let mut h = Hints::default();
        h.remember(1, 10);
        h.remember(2, 20);
        h.remember(1, 11);
        assert_eq!(h.candidate(1), Some(11));
        assert_eq!(h.candidate(2), Some(20));
        assert_eq!(h.candidate(3), None);
        assert_eq!(h.programs.len(), 2);
        assert_eq!(h.programs[0].id, 11, "the last remembered comes first");
    }

    // AC-T7.1.5: do X: remember one more program than the bound; expect Y: the bound holds and the one
    // remembered longest ago is the one dropped (CLAUDE.md §2).
    #[test]
    fn past_the_bound_the_oldest_hint_is_dropped() {
        let mut h = Hints::default();
        for n in 0..=MAX_HINTS {
            h.remember(u64::try_from(n).unwrap(), u32::try_from(n).unwrap());
        }
        assert_eq!(h.programs.len(), MAX_HINTS);
        assert_eq!(h.candidate(0), None, "the oldest is dropped");
        assert_eq!(h.candidate(1), Some(1));
    }

    // AC-T7.1.5: do X: parse what remember wrote, a file an older Hex wrote (keyed by the kernel's tag), and hostile
    // bytes; expect Y: the same hints back, and no hints (never a panic) for anything else (CLAUDE.md §6:
    // hostile input to every parser). A file past the bound is cut to it.
    #[test]
    fn hints_round_trip_and_other_bytes_are_no_hints() {
        let mut h = Hints::default();
        h.remember(0xab, 7);
        assert_eq!(Hints::parse(&h.to_bytes().unwrap()), h);
        for bytes in [
            &b""[..],
            b"\xff\xfe",
            b"{",
            b"[]",
            b"{\"programs\":5}",
            b"{\"programs\":[{\"digest\":1,\"id\":\"x\"}]}",
            b"{\"programs\":[{\"tag\":\"aaaaaaaaaaaaaaaa\",\"id\":3}]}",
        ] {
            assert_eq!(Hints::parse(bytes), Hints::default(), "{bytes:?}");
        }
        let deep = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
        assert_eq!(Hints::parse(deep.as_bytes()), Hints::default());
        let long = Hints {
            programs: (0..200)
                .map(|n| Hint {
                    digest: format!("{n:016x}"),
                    id: n,
                })
                .collect(),
        };
        assert_eq!(
            Hints::parse(&long.to_bytes().unwrap()).programs.len(),
            MAX_HINTS
        );
    }

    // AC-T7.1.4: do X: match loaded programs against a policy's instructions; expect Y: only a cgroup device
    // program whose translation is the instructions byte for byte is the policy's, never one of another
    // type, length or instruction, nor one whose dump the kernel withheld (goal 5 extended step 1's
    // planted hint: a candidate is refused by its translation).
    #[test]
    fn only_a_device_program_translated_to_the_instructions_is_the_policys() {
        let insns = default_program().unwrap();
        let bytes: Vec<u8> = insns.iter().flat_map(Insn::bytes).collect();
        let len = u32::try_from(bytes.len()).unwrap();
        assert!(is_policy(&identity(len), Some(&bytes), &insns));
        let mut other_type = identity(len);
        other_type.prog_type = 1;
        assert!(
            !is_policy(&other_type, Some(&bytes), &insns),
            "a socket filter"
        );
        assert!(
            !is_policy(&identity(len + 8), Some(&bytes), &insns),
            "another length"
        );
        let mut changed = bytes.clone();
        changed[12] ^= 1;
        assert!(
            !is_policy(&identity(len), Some(&changed), &insns),
            "one immediate differs"
        );
        assert!(
            !is_policy(&identity(len), None, &insns),
            "the dump withheld"
        );
        let other: Vec<u8> = policy(1).iter().flat_map(Insn::bytes).collect();
        assert!(
            !is_policy(
                &identity(u32::try_from(other.len()).unwrap()),
                Some(&other),
                &insns
            ),
            "another policy's program"
        );
        assert!(!is_policy(&identity(0), Some(&[]), &[]), "no instructions");
    }

    // AC-T7.1.5: do X: hold programs past the bound, using some in between; expect Y: the bound holds, and the
    // program handed back to be closed is the one used longest ago, not the one held longest (a get makes
    // a program the most recently used) (CLAUDE.md §2).
    #[test]
    fn past_the_bound_the_program_used_longest_ago_is_handed_back() {
        let mut p = Programs::new(2);
        assert!(p.put(policy(1), "one").is_empty());
        assert!(p.put(policy(2), "two").is_empty());
        assert_eq!(p.get(&policy(1)), Some(&"one"));
        assert_eq!(p.put(policy(3), "three"), vec!["two"]);
        assert_eq!(p.len(), 2);
        assert_eq!(p.get(&policy(2)), None);
        assert_eq!(p.get(&policy(1)), Some(&"one"));
        assert_eq!(p.get(&policy(3)), Some(&"three"));
    }

    // AC-T7.1.4: do X: hold a program, then look up instructions with the same digest but other instructions, and
    // hold those; expect Y: the lookup finds nothing (a held program is proven by its instructions, not
    // its digest), and holding the second hands the first back to be closed.
    #[test]
    fn a_digest_collision_finds_nothing_and_replaces_the_program() {
        let mut p = Programs::new(4);
        assert!(p.put(policy(1), "one").is_empty());
        // Forged: the same digest held for other instructions.
        let d = digest(&policy(1));
        p.held[0].insns = policy(2);
        assert_eq!(digest(&policy(1)), d);
        assert_eq!(p.get(&policy(1)), None, "the instructions don't match");
        assert_eq!(p.put(policy(1), "again"), vec!["one"]);
        assert_eq!(p.get(&policy(1)), Some(&"again"));
        assert_eq!(p.len(), 1);
    }

    // AC-T7.1.4: do X: give boxes their programs under the default at the root and under no root program; expect
    // Y: a box with the root's policy gets nothing; another policy gets its own program; an allow-all
    // policy gets the allow-all program under a root program and nothing under none (research f40-f43).
    #[test]
    fn a_box_gets_its_own_program_unless_the_roots_decides_for_it() {
        let default = default_program().unwrap();
        let other = policy(1);
        assert_eq!(give(Some(&default), Some(&default)), Give::Nothing);
        assert_eq!(
            give(Some(&other), Some(&default)),
            Give::Program(other.clone())
        );
        assert_eq!(give(None, Some(&default)), Give::Program(allow_all()));
        assert_eq!(give(None, None), Give::Nothing);
        assert_eq!(give(Some(&default), None), Give::Program(default.clone()));
        assert_eq!(give(Some(&other), None), Give::Program(other));
    }

    #[derive(Debug, Clone)]
    enum Op {
        Get(i64),
        Put(i64),
    }

    proptest! {
        // AC-T7.1.5, CLAUDE.md §6, model-based: under any sequence of lookups and holds, the held programs are an
        // oracle's least-recently-used list of at most `bound` policies: a lookup finds exactly what the
        // oracle holds, and a hold hands back exactly what the oracle drops. (The deterministic test above
        // is the one that must hit and drop.) The sizes are fixtures: bounds of 1-4 against 7 policies and up to 63
        // operations, so lookups hit and miss and holds drop.
        #[test]
        fn the_held_programs_are_a_least_recently_used_list(
            bound in 1usize..5,
            ops in proptest::collection::vec(
                prop_oneof![(1i64..8).prop_map(Op::Get), (1i64..8).prop_map(Op::Put)], 1..64),
        ) {
            let mut p = Programs::new(bound);
            let mut oracle: Vec<i64> = Vec::new();
            for op in ops {
                match op {
                    Op::Get(m) => {
                        let want = oracle.iter().position(|x| *x == m);
                        prop_assert_eq!(p.get(&policy(m)).copied(), want.map(|_| m));
                        if let Some(at) = want {
                            let x = oracle.remove(at);
                            oracle.insert(0, x);
                        }
                    }
                    Op::Put(m) => {
                        let mut want = Vec::new();
                        if let Some(at) = oracle.iter().position(|x| *x == m) {
                            want.push(oracle.remove(at));
                        }
                        oracle.insert(0, m);
                        while oracle.len() > bound {
                            want.push(oracle.pop().unwrap());
                        }
                        prop_assert_eq!(p.put(policy(m), m), want);
                    }
                }
                prop_assert!(p.len() <= bound);
                prop_assert_eq!(p.len(), oracle.len());
            }
        }
    }
}
