// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

use std::collections::BTreeMap;

use crate::backend::bpf::*;
use crate::backend::rule::SeccompRule;
use crate::backend::{Error, Result, SeccompAction, TargetArch};

/// Filter containing rules assigned to syscall numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeccompFilter {
    /// Map of syscall numbers to their rule chain and the actions applied to it.
    rules: BTreeMap<i64, SyscallRules>,
    /// Default action to apply to syscalls that are not present in `rules`.
    mismatch_action: SeccompAction,
    /// Target architecture of the generated BPF filter.
    target_arch: TargetArch,
}

/// Hex addition (see vendor/seccompiler/UPSTREAM.md): one syscall's argument rules, the action when
/// one of them matches, and the action when the syscall is made but none match. The stock
/// [`SeccompFilter::new`] gives every matching syscall one filter-wide `match_action` and falls
/// through to the filter-wide default; an OCI seccomp profile instead names an action per syscall
/// group and can allow a syscall for most arguments while refusing one value (Docker's AF_NETLINK
/// `socket` rule), or refuse one argument combination while allowing the rest — argument rules of
/// differing actions. Carrying a per-rule on-match action, and the syscall's own fallthrough, lets a
/// single installed filter model such a profile, which [`SeccompFilter::new`] cannot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyscallRules {
    /// The action a rule with no action of its own takes when it matches — and, when `rules` is
    /// empty, the syscall's action unconditionally. [`SeccompFilter::new`] gives every rule this one.
    pub on_match: SeccompAction,
    /// The syscall's argument rules, each with the action it takes when its conditions all match
    /// (`None` uses `on_match`); the first matching rule's action wins. An empty list means the
    /// syscall is unconditional — `on_match` is its action.
    pub rules: Vec<(SeccompRule, Option<SeccompAction>)>,
    /// The action when the syscall is made but no rule matches. Usually the filter-wide default, but
    /// an OCI profile may make it differ (allow in general, refuse one value).
    pub otherwise: SeccompAction,
}

impl SeccompFilter {
    /// Creates a new filter with a set of rules, an on-match and default action.
    ///
    /// # Arguments
    ///
    /// * `rules` - Map containing syscall numbers and their respective [`SeccompRule`]s.
    /// * `mismatch_action` - [`SeccompAction`] taken for all syscalls that do not match the filter.
    /// * `match_action` - [`SeccompAction`] taken for system calls that match the filter.
    /// * `target_arch` - Target architecture of the generated BPF filter.
    ///
    /// # Example
    ///
    /// ```
    /// use seccompiler::{
    ///     SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule,
    /// };
    /// use std::convert::TryInto;
    ///
    /// let filter = SeccompFilter::new(
    ///     vec![
    ///         (libc::SYS_accept4, vec![]),
    ///         (
    ///             libc::SYS_fcntl,
    ///             vec![
    ///                 SeccompRule::new(vec![
    ///                     SeccompCondition::new(
    ///                         1,
    ///                         SeccompCmpArgLen::Dword,
    ///                         SeccompCmpOp::Eq,
    ///                         libc::F_SETFD as u64,
    ///                     )
    ///                     .unwrap(),
    ///                     SeccompCondition::new(
    ///                         2,
    ///                         SeccompCmpArgLen::Dword,
    ///                         SeccompCmpOp::Eq,
    ///                         libc::FD_CLOEXEC as u64,
    ///                     )
    ///                     .unwrap(),
    ///                 ])
    ///                 .unwrap(),
    ///                 SeccompRule::new(vec![SeccompCondition::new(
    ///                     1,
    ///                     SeccompCmpArgLen::Dword,
    ///                     SeccompCmpOp::Eq,
    ///                     libc::F_GETFD as u64,
    ///                 )
    ///                 .unwrap()])
    ///                 .unwrap(),
    ///             ],
    ///         ),
    ///     ]
    ///     .into_iter()
    ///     .collect(),
    ///     SeccompAction::Trap,
    ///     SeccompAction::Allow,
    ///     std::env::consts::ARCH.try_into().unwrap(),
    /// );
    /// ```
    ///
    /// [`SeccompRule`]: struct.SeccompRule.html
    /// [`SeccompAction`]: enum.SeccompAction.html
    pub fn new(
        rules: BTreeMap<i64, Vec<SeccompRule>>,
        mismatch_action: SeccompAction,
        match_action: SeccompAction,
        target_arch: TargetArch,
    ) -> Result<Self> {
        // Doesn't make sense to have equal default and on-match actions.
        if mismatch_action == match_action {
            return Err(Error::IdenticalActions);
        }
        // Hex change (UPSTREAM.md): every syscall shares the one `match_action` and falls through to
        // the filter-wide default, which reproduces the stock behaviour exactly.
        let rules = rules
            .into_iter()
            .map(|(nr, chain)| {
                (
                    nr,
                    SyscallRules {
                        on_match: match_action.clone(),
                        // Every rule takes the one filter-wide `match_action` (`None`), which
                        // reproduces the stock BPF byte for byte.
                        rules: chain.into_iter().map(|r| (r, None)).collect(),
                        otherwise: mismatch_action.clone(),
                    },
                )
            })
            .collect();

        Ok(Self {
            rules,
            mismatch_action,
            target_arch,
        })
    }

    /// Hex addition (see vendor/seccompiler/UPSTREAM.md): a filter whose syscalls each carry their
    /// own actions, which [`SeccompFilter::new`]'s single filter-wide `match_action` cannot express.
    /// Used to compile an OCI seccomp profile, where each syscall group names its own action and the
    /// action when no rule matches can differ from the filter-wide default.
    pub fn with_syscall_rules(
        rules: BTreeMap<i64, SyscallRules>,
        mismatch_action: SeccompAction,
        target_arch: TargetArch,
    ) -> Result<Self> {
        Ok(Self {
            rules,
            mismatch_action,
            target_arch,
        })
    }

    /// Appends a chain of rules to an accumulator, updating the length of the filter.
    ///
    /// # Arguments
    ///
    /// * `syscall_number` - The syscall to which the rules apply.
    /// * `chain` - The chain of rules for the specified syscall.
    /// * `mismatch_action` - The action to be taken in none of the rules apply.
    /// * `accumulator` - The expanding BPF program.
    fn append_syscall_chain(
        syscall_number: i64,
        chain: Vec<(SeccompRule, Option<SeccompAction>)>,
        mismatch_action: SeccompAction,
        match_action: SeccompAction,
        accumulator: &mut Vec<Vec<sock_filter>>,
    ) -> Result<()> {
        // The rules of the chain are translated into BPF statements.
        let chain: Vec<_> = chain
            .into_iter()
            .map(|(rule, action)| {
                let mut bpf: BpfProgram = rule.into();
                // Last statement is this rule's on-match action: its own, or the syscall's shared
                // `match_action` when it names none (as every rule of `SeccompFilter::new` does).
                let action = action.unwrap_or_else(|| match_action.clone());
                bpf.push(bpf_stmt(BPF_RET | BPF_K, u32::from(action)));
                bpf
            })
            .collect();
        let chain_len: usize = chain.iter().map(Vec::len).sum();

        // The chain starts with a comparison checking the loaded syscall number against the
        // syscall number of the chain. Sizing the block for what it will hold — the chain and the
        // fallthrough, or, for an unconditional syscall (empty chain), two jumps plus the on-match
        // and fallthrough actions, five statements — allocates it once instead of growing it, on the
        // container-start path. (Hex change, UPSTREAM.md: the empty-chain case was sized `+ 2`.)
        let mut built_syscall = Vec::with_capacity(if chain.is_empty() { 5 } else { chain_len + 2 });
        built_syscall.push(bpf_jump(
            BPF_JMP | BPF_JEQ | BPF_K,
            // Safe because linux system call numbers are nowhere near the u32::MAX value.
            syscall_number.try_into().unwrap(),
            0,
            1,
        ));

        if chain.is_empty() {
            built_syscall.push(bpf_stmt(BPF_JMP | BPF_JA, 1));
            built_syscall.push(bpf_stmt(BPF_JMP | BPF_JA, 2));
            // If the chain is empty, we only need to append the on-match action.
            built_syscall.push(bpf_stmt(BPF_RET | BPF_K, u32::from(match_action)));
        } else {
            // The rules of the chain are appended.
            chain
                .into_iter()
                .for_each(|mut rule| built_syscall.append(&mut rule));
        }

        // The default action is appended, if the syscall number comparison matched and then all
        // rules fail to match, the default action is reached.
        built_syscall.push(bpf_stmt(BPF_RET | BPF_K, mismatch_action.into()));

        accumulator.push(built_syscall);

        Ok(())
    }
}

impl TryFrom<SeccompFilter> for BpfProgram {
    type Error = Error;
    fn try_from(filter: SeccompFilter) -> Result<Self> {
        // Initialize the result with the precursory architecture check.
        let mut result = build_arch_validation_sequence(filter.target_arch);

        // If no rules are set up, the filter will always return the default action,
        // so let's short-circuit the function.
        if filter.rules.is_empty() {
            result.extend(vec![bpf_stmt(
                BPF_RET | BPF_K,
                u32::from(filter.mismatch_action),
            )]);

            return Ok(result);
        }

        // The called syscall number is loaded.
        let mut accumulator = vec![vec![bpf_stmt(
            BPF_LD | BPF_W | BPF_ABS,
            u32::from(SECCOMP_DATA_NR_OFFSET),
        )]];

        let mut iter = filter.rules.into_iter();

        // For each syscall adds its rule chain to the filter, with that syscall's own actions
        // (Hex change, UPSTREAM.md): the action when a rule matches, and the action when the syscall
        // is made but no rule matches, which may differ from the filter-wide default.
        let mismatch_action = filter.mismatch_action;

        iter.try_for_each(|(syscall_number, chain)| {
            SeccompFilter::append_syscall_chain(
                syscall_number,
                chain.rules,
                chain.otherwise,
                chain.on_match,
                &mut accumulator,
            )
        })?;

        // The default action is once again appended, it is reached if all syscall number
        // comparisons fail.
        accumulator.push(vec![bpf_stmt(BPF_RET | BPF_K, mismatch_action.into())]);

        // Finally, builds the translated filter by consuming the accumulator.
        accumulator
            .into_iter()
            .for_each(|mut instructions| result.append(&mut instructions));

        if result.len() >= BPF_MAX_LEN {
            return Err(Error::FilterTooLarge(result.len()));
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::SeccompFilter;
    use crate::backend::bpf::*;
    use crate::backend::condition::SeccompCondition as Cond;
    use crate::backend::SeccompCmpArgLen as ArgLen;
    use crate::backend::SeccompCmpOp::*;
    use crate::backend::{Error, SeccompAction, SeccompRule};
    use std::collections::BTreeMap;
    use std::convert::TryInto;
    use std::env::consts::ARCH;

    fn create_test_bpf_filter(arg_len: ArgLen) -> SeccompFilter {
        SeccompFilter::new(
            vec![
                (
                    1,
                    vec![
                        SeccompRule::new(vec![
                            Cond::new(2, arg_len.clone(), Le, 14).unwrap(),
                            Cond::new(2, arg_len.clone(), Ne, 10).unwrap(),
                        ])
                        .unwrap(),
                        SeccompRule::new(vec![
                            Cond::new(2, arg_len.clone(), Gt, 20).unwrap(),
                            Cond::new(2, arg_len.clone(), Lt, 30).unwrap(),
                        ])
                        .unwrap(),
                        SeccompRule::new(vec![Cond::new(2, arg_len.clone(), Ge, 42).unwrap()])
                            .unwrap(),
                    ],
                ),
                (
                    9,
                    vec![SeccompRule::new(vec![
                        Cond::new(1, arg_len, MaskedEq(0b100), 36).unwrap()
                    ])
                    .unwrap()],
                ),
                (10, vec![]),
            ]
            .into_iter()
            .collect(),
            SeccompAction::Trap,
            SeccompAction::Allow,
            ARCH.try_into().unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn test_seccomp_filter_validate() {
        // Filter has identical on-match and default actions.
        assert_eq!(
            SeccompFilter::new(
                BTreeMap::new(),
                SeccompAction::Allow,
                SeccompAction::Allow,
                ARCH.try_into().unwrap()
            )
            .unwrap_err(),
            Error::IdenticalActions
        );
    }

    #[test]
    fn test_seccomp_filter_too_large() {
        let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
        for _ in 1..1000 {
            rules
                .entry(1)
                .or_default()
                .append(&mut vec![SeccompRule::new(vec![Cond::new(
                    2,
                    ArgLen::Dword,
                    Le,
                    14,
                )
                .unwrap()])
                .unwrap()]);
        }

        let filter = SeccompFilter::new(
            rules.into_iter().collect(),
            SeccompAction::Allow,
            SeccompAction::Trap,
            ARCH.try_into().unwrap(),
        )
        .unwrap();

        assert_eq!(
            TryInto::<BpfProgram>::try_into(filter).unwrap_err(),
            Error::FilterTooLarge(5002)
        );
    }

    #[test]
    fn test_empty_filter_output() {
        // An empty filter should just validate the architecture and return the mismatch_action.
        let mut expected_program = Vec::new();
        expected_program.extend(build_arch_validation_sequence(ARCH.try_into().unwrap()));
        expected_program.extend(vec![bpf_stmt(BPF_RET, 0x7fff_0000)]);

        let filter = SeccompFilter::new(
            BTreeMap::new(),
            SeccompAction::Allow,
            SeccompAction::Trap,
            ARCH.try_into().unwrap(),
        )
        .unwrap();
        let prog: BpfProgram = filter.try_into().unwrap();

        assert_eq!(expected_program, prog);
    }

    #[test]
    fn test_filter_bpf_output_dword() {
        // Compares translated filter with hardcoded BPF program.
        let filter = create_test_bpf_filter(ArgLen::Dword);

        let mut instructions = Vec::new();
        instructions.extend(build_arch_validation_sequence(ARCH.try_into().unwrap()));
        instructions.extend(vec![
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 0),
            bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, 1, 0, 1),
            bpf_stmt(BPF_JMP | BPF_JA, 1),
            bpf_stmt(BPF_JMP | BPF_JA, 6),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 32),
            bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, 10, 3, 0),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 32),
            bpf_jump(BPF_JMP | BPF_JGT | BPF_K, 14, 1, 0),
            bpf_stmt(BPF_RET, 0x7fff_0000),
            bpf_stmt(BPF_JMP | BPF_JA, 1),
            bpf_stmt(BPF_JMP | BPF_JA, 6),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 32),
            bpf_jump(BPF_JMP | BPF_JGE | BPF_K, 30, 3, 0),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 32),
            bpf_jump(BPF_JMP | BPF_JGT | BPF_K, 20, 0, 1),
            bpf_stmt(BPF_RET, 0x7fff_0000),
            bpf_stmt(BPF_JMP | BPF_JA, 1),
            bpf_stmt(BPF_JMP | BPF_JA, 4),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 32),
            bpf_jump(BPF_JMP | BPF_JGE | BPF_K, 42, 0, 1),
            bpf_stmt(BPF_RET, 0x7fff_0000),
            bpf_stmt(BPF_RET, 0x0003_0000),
            bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, 9, 0, 1),
            bpf_stmt(BPF_JMP | BPF_JA, 1),
            bpf_stmt(BPF_JMP | BPF_JA, 5),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 24),
            bpf_stmt(BPF_ALU | BPF_AND | BPF_K, 0b100),
            bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, 36 & 0b100, 0, 1),
            bpf_stmt(BPF_RET, 0x7fff_0000),
            bpf_stmt(BPF_RET, 0x0003_0000),
            bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, 10, 0, 1),
            bpf_stmt(BPF_JMP | BPF_JA, 1),
            bpf_stmt(BPF_JMP | BPF_JA, 2),
            bpf_stmt(BPF_RET | BPF_K, 0x7fff_0000),
            bpf_stmt(BPF_RET, 0x0003_0000),
            bpf_stmt(BPF_RET, 0x0003_0000),
        ]);

        let bpfprog: BpfProgram = filter.try_into().unwrap();

        assert_eq!(bpfprog, instructions);
    }

    #[test]
    fn test_filter_bpf_output_qword() {
        let filter = create_test_bpf_filter(ArgLen::Qword);

        let mut instructions = Vec::new();
        instructions.extend(build_arch_validation_sequence(ARCH.try_into().unwrap()));
        instructions.extend(vec![
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 0),
            bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, 1, 0, 1),
            bpf_stmt(BPF_JMP | BPF_JA, 1),
            bpf_stmt(BPF_JMP | BPF_JA, 11),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 36),
            bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, 0, 0, 2),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 32),
            bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, 10, 6, 0),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 36),
            bpf_jump(BPF_JMP | BPF_JGT | BPF_K, 0, 4, 0),
            bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, 0, 0, 2),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 32),
            bpf_jump(BPF_JMP | BPF_JGT | BPF_K, 14, 1, 0),
            bpf_stmt(BPF_RET, 0x7fff_0000),
            bpf_stmt(BPF_JMP | BPF_JA, 1),
            bpf_stmt(BPF_JMP | BPF_JA, 12),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 36),
            bpf_jump(BPF_JMP | BPF_JGT | BPF_K, 0, 9, 0),
            bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, 0, 0, 2),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 32),
            bpf_jump(BPF_JMP | BPF_JGE | BPF_K, 30, 6, 0),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 36),
            bpf_jump(BPF_JMP | BPF_JGT | BPF_K, 0, 3, 0),
            bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, 0, 0, 3),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 32),
            bpf_jump(BPF_JMP | BPF_JGT | BPF_K, 20, 0, 1),
            bpf_stmt(BPF_RET, 0x7fff_0000),
            bpf_stmt(BPF_JMP | BPF_JA, 1),
            bpf_stmt(BPF_JMP | BPF_JA, 7),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 36),
            bpf_jump(BPF_JMP | BPF_JGT | BPF_K, 0, 3, 0),
            bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, 0, 0, 3),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 32),
            bpf_jump(BPF_JMP | BPF_JGE | BPF_K, 42, 0, 1),
            bpf_stmt(BPF_RET, 0x7fff_0000),
            bpf_stmt(BPF_RET, 0x0003_0000),
            bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, 9, 0, 1),
            bpf_stmt(BPF_JMP | BPF_JA, 1),
            bpf_stmt(BPF_JMP | BPF_JA, 8),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 28),
            bpf_stmt(BPF_ALU | BPF_AND | BPF_K, 0),
            bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, 0, 0, 4),
            bpf_stmt(BPF_LD | BPF_W | BPF_ABS, 24),
            bpf_stmt(BPF_ALU | BPF_AND | BPF_K, 0b100),
            bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, 36 & 0b100, 0, 1),
            bpf_stmt(BPF_RET, 0x7fff_0000),
            bpf_stmt(BPF_RET, 0x0003_0000),
            bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, 10, 0, 1),
            bpf_stmt(BPF_JMP | BPF_JA, 1),
            bpf_stmt(BPF_JMP | BPF_JA, 2),
            bpf_stmt(BPF_RET | BPF_K, 0x7fff_0000),
            bpf_stmt(BPF_RET, 0x0003_0000),
            bpf_stmt(BPF_RET, 0x0003_0000),
        ]);

        let bpfprog: BpfProgram = filter.try_into().unwrap();
        assert_eq!(bpfprog, instructions);
    }
}
