//! Compiles an OCI `linux.seccomp` profile ([`Seccomp`]) to a BPF program and loads it in the
//! container init before `execve` (`init::become_process`). The compile is sans-IO (CLAUDE.md §9):
//! it turns the parsed profile into a [`seccompiler::BpfProgram`], and the init makes the one
//! `seccomp(2)` call: through [`seccompiler::apply_filter`] when the spec asks for no_new_privs, and
//! through `sys::load_seccomp_filter`, before the user and capabilities change, when it doesn't.
//!
//! A profile Hex can't honour is refused by a typed [`OciError`], never run with the control dropped
//! (CLAUDE.md §3): an unknown or `SCMP_ACT_NOTIFY` action, an unknown comparison, an argument index
//! out of range, a profile that names architectures but not the host's, or a flag that needs a
//! user-notification listener Hex doesn't implement.
//!
//! The mapping follows runc/crun semantics, where those are a choice the OCI spec leaves open:
//! - Arguments are compared at their full 64-bit width ([`SeccompCmpArgLen::Qword`]); libseccomp's
//!   datum is a `u64`, so this is what a profile author writes against.
//! - `SCMP_CMP_MASKED_EQ` is `(arg & value) == valueTwo`; the other operators compare `arg` to
//!   `value` and ignore `valueTwo`.
//! - A rule that names no `errnoRet` returns `EPERM`; the default action's errno is `defaultErrnoRet`
//!   or `EPERM`.
//! - A syscall name the host architecture doesn't have is skipped: it can't be invoked here, so the
//!   rule is a no-op and skipping drops no control. runc and crun skip it too; a cross-architecture
//!   profile lists names for other arches (e.g. `*_time64`, `socketcall`) that aren't numbers here.
//! - The filter is built for the host architecture alone. A profile that also lists compat or
//!   foreign architectures is still accepted when it lists the host's: the native filter's
//!   architecture gate refuses (kills) any syscall made under a non-host architecture, which is at
//!   least as strict as the profile's rules for them, so nothing is weakened.

use std::collections::BTreeMap;

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, SyscallRules, SyscallTable, TargetArch,
};

use crate::linux::oci::OciError;
use crate::linux::oci::spec::{Seccomp, SeccompArg};

/// Linux `EPERM` (uapi/asm-generic/errno-base.h, stable across architectures): the errno a
/// `SCMP_ACT_ERRNO` rule returns when the profile names none, as runc and crun default it.
const EPERM: u32 = 1;

/// The `SCMP_ARCH_*` tokens libseccomp defines. A token not here is a typo or an arch Hex has never
/// heard of: refused (CLAUDE.md §3), never read as "some other arch" and silently skipped.
const KNOWN_SCMP_ARCHES: &[&str] = &[
    "SCMP_ARCH_X86",
    "SCMP_ARCH_X86_64",
    "SCMP_ARCH_X32",
    "SCMP_ARCH_ARM",
    "SCMP_ARCH_AARCH64",
    "SCMP_ARCH_MIPS",
    "SCMP_ARCH_MIPS64",
    "SCMP_ARCH_MIPS64N32",
    "SCMP_ARCH_MIPSEL",
    "SCMP_ARCH_MIPSEL64",
    "SCMP_ARCH_MIPSEL64N32",
    "SCMP_ARCH_PPC",
    "SCMP_ARCH_PPC64",
    "SCMP_ARCH_PPC64LE",
    "SCMP_ARCH_S390",
    "SCMP_ARCH_S390X",
    "SCMP_ARCH_PARISC",
    "SCMP_ARCH_PARISC64",
    "SCMP_ARCH_RISCV64",
];

/// Compiles an OCI seccomp profile to a BPF program for the host architecture, or refuses it by name.
pub fn compile(profile: &Seccomp) -> Result<BpfProgram, OciError> {
    let arch = host_target_arch()?;
    check_architectures(&profile.architectures, arch)?;
    check_flags(&profile.flags)?;

    let default_action = action(&profile.default_action, profile.default_errno_ret)?;
    let table = SyscallTable::new(arch);
    let chains = syscall_chains(profile, &table, &default_action)?;

    let filter = SeccompFilter::with_syscall_rules(chains, default_action, arch)
        .map_err(|e| OciError::Config(format!("seccomp: assembling the filter: {e}")))?;
    filter
        .try_into()
        .map_err(|e| OciError::Config(format!("seccomp: compiling the filter to BPF: {e}")))
}

/// What a syscall's groups add up to while merging the profile: its argument rules, each with the
/// action it takes when it matches, and the action for the syscall named with no arguments (which
/// stands as the fallthrough when argument rules don't match — allow in general, refuse one value).
struct Chain {
    rules: Vec<(SeccompRule, SeccompAction)>,
    unconditional_action: Option<SeccompAction>,
}

/// Merges the profile's syscall groups into one rule chain per syscall number for the host arch.
fn syscall_chains(
    profile: &Seccomp,
    table: &SyscallTable,
    default_action: &SeccompAction,
) -> Result<BTreeMap<i64, SyscallRules>, OciError> {
    let mut acc: BTreeMap<i64, Chain> = BTreeMap::new();

    for group in &profile.syscalls {
        let group_action = action(&group.action, group.errno_ret)?;
        // The AND-bound conditions, built once and shared by every name in the group.
        let conditions = group
            .args
            .iter()
            .map(condition)
            .collect::<Result<Vec<SeccompCondition>, OciError>>()?;

        for name in &group.names {
            let Some(nr) = table.get_syscall_nr(name) else {
                // Not a syscall on this architecture: it can't be invoked here, so the rule is a
                // no-op. Skipping drops no control (CLAUDE.md §3); runc and crun skip it too.
                continue;
            };
            let chain = acc.entry(nr).or_insert_with(|| Chain {
                rules: Vec::new(),
                unconditional_action: None,
            });
            if conditions.is_empty() {
                set_once(&mut chain.unconditional_action, &group_action).map_err(|()| {
                    OciError::Config(format!(
                        "seccomp: syscall {name:?} is given two unconditional actions; \
                         Hex refuses an ambiguous profile rather than pick one"
                    ))
                })?;
            } else {
                let rule = SeccompRule::new(conditions.clone()).map_err(|e| {
                    OciError::Config(format!("seccomp: invalid rule for {name:?}: {e}"))
                })?;
                // Each argument rule keeps its own action, so a group that refuses one value and a
                // group that allows the rest — argument rules of differing actions, as the default
                // profile's `socket` has — live in one syscall's chain (first match wins).
                chain.rules.push((rule, group_action.clone()));
            }
        }
    }

    let mut chains = BTreeMap::new();
    for (nr, chain) in acc {
        // A syscall with no argument rules takes its one unconditional action (else the default),
        // regardless of arguments. One with argument rules takes the first matching rule's own
        // action; a call matching none takes the syscall's unconditional action if it named one
        // (the general allow beside a refused value), else the filter-wide default. `on_match` is
        // the action a rule without its own would take; every rule here names one, so it stands only
        // for the unconditional case.
        let (on_match, rules, otherwise) = if chain.rules.is_empty() {
            (
                chain
                    .unconditional_action
                    .unwrap_or_else(|| default_action.clone()),
                Vec::new(),
                default_action.clone(),
            )
        } else {
            (
                default_action.clone(),
                chain.rules.into_iter().map(|(r, a)| (r, Some(a))).collect(),
                chain
                    .unconditional_action
                    .unwrap_or_else(|| default_action.clone()),
            )
        };
        chains.insert(
            nr,
            SyscallRules {
                on_match,
                rules,
                otherwise,
            },
        );
    }
    Ok(chains)
}

/// Records an action for a syscall slot, or signals a clash: a second, different action where only
/// one can stand.
fn set_once(slot: &mut Option<SeccompAction>, action: &SeccompAction) -> Result<(), ()> {
    match slot {
        Some(existing) if existing != action => Err(()),
        _ => {
            *slot = Some(action.clone());
            Ok(())
        }
    }
}

/// An `SCMP_ACT_*` name as a seccompiler action. `errno_ret` is the rule's `errnoRet` (or the
/// profile's `defaultErrnoRet` for the default action), already resolved from the profile.
fn action(name: &str, errno_ret: Option<u32>) -> Result<SeccompAction, OciError> {
    let act = match name {
        "SCMP_ACT_ALLOW" => SeccompAction::Allow,
        "SCMP_ACT_ERRNO" => SeccompAction::Errno(errno_ret.unwrap_or(EPERM)),
        // SCMP_ACT_KILL is libseccomp's historical name for killing the offending thread.
        "SCMP_ACT_KILL" | "SCMP_ACT_KILL_THREAD" => SeccompAction::KillThread,
        "SCMP_ACT_KILL_PROCESS" => SeccompAction::KillProcess,
        "SCMP_ACT_TRAP" => SeccompAction::Trap,
        "SCMP_ACT_TRACE" => SeccompAction::Trace(errno_ret.unwrap_or(0)),
        "SCMP_ACT_LOG" => SeccompAction::Log,
        "SCMP_ACT_NOTIFY" => {
            return Err(OciError::Config(
                "seccomp: SCMP_ACT_NOTIFY needs a user-notification listener, which Hex does not \
                 implement; refused rather than run with the rule dropped"
                    .to_owned(),
            ));
        }
        other => {
            return Err(OciError::Config(format!(
                "seccomp: unknown or unsupported action {other:?}"
            )));
        }
    };
    Ok(act)
}

/// One `args` entry as a seccompiler condition, or why it can't be one.
fn condition(arg: &SeccompArg) -> Result<SeccompCondition, OciError> {
    let index = u8::try_from(arg.index)
        .ok()
        .filter(|i| *i <= 5)
        .ok_or_else(|| {
            OciError::Config(format!(
                "seccomp: argument index {} is out of range (a syscall has arguments 0..=5)",
                arg.index
            ))
        })?;
    // libseccomp compares the full 64-bit argument, so a profile's values are written for that width.
    let (op, value) = match arg.op.as_str() {
        "SCMP_CMP_EQ" => (SeccompCmpOp::Eq, arg.value),
        "SCMP_CMP_NE" => (SeccompCmpOp::Ne, arg.value),
        "SCMP_CMP_LT" => (SeccompCmpOp::Lt, arg.value),
        "SCMP_CMP_LE" => (SeccompCmpOp::Le, arg.value),
        "SCMP_CMP_GT" => (SeccompCmpOp::Gt, arg.value),
        "SCMP_CMP_GE" => (SeccompCmpOp::Ge, arg.value),
        // libseccomp masked-eq is `(arg & value) == valueTwo`. seccompiler's MaskedEq(mask) compares
        // `(arg & mask)` to the condition value, so the mask is `value` and the value is `valueTwo`.
        "SCMP_CMP_MASKED_EQ" => (
            SeccompCmpOp::MaskedEq(arg.value),
            arg.value_two.unwrap_or(0),
        ),
        other => {
            return Err(OciError::Config(format!(
                "seccomp: unknown or unsupported arg comparison {other:?}"
            )));
        }
    };
    SeccompCondition::new(index, SeccompCmpArgLen::Qword, op, value)
        .map_err(|e| OciError::Config(format!("seccomp: invalid argument condition: {e}")))
}

/// The host as a seccompiler target architecture, or a refusal naming it.
fn host_target_arch() -> Result<TargetArch, OciError> {
    TargetArch::try_from(std::env::consts::ARCH).map_err(|_| {
        OciError::Config(format!(
            "seccomp: no filter target for host architecture {} (Hex targets x86_64 and aarch64)",
            std::env::consts::ARCH
        ))
    })
}

/// The `SCMP_ARCH_*` name of a target architecture.
fn host_scmp_arch(arch: TargetArch) -> &'static str {
    match arch {
        TargetArch::x86_64 => "SCMP_ARCH_X86_64",
        TargetArch::aarch64 => "SCMP_ARCH_AARCH64",
        TargetArch::riscv64 => "SCMP_ARCH_RISCV64",
    }
}

/// Refuses a profile whose `architectures` omit the host's, or name one Hex doesn't recognise.
/// An empty list means the native architecture only (libseccomp's default), which is fine.
fn check_architectures(arches: &[String], host: TargetArch) -> Result<(), OciError> {
    if arches.is_empty() {
        return Ok(());
    }
    let host_name = host_scmp_arch(host);
    let mut host_listed = false;
    for a in arches {
        if a == host_name {
            host_listed = true;
        } else if !KNOWN_SCMP_ARCHES.contains(&a.as_str()) {
            return Err(OciError::Config(format!(
                "seccomp: unknown architecture {a:?}"
            )));
        }
        // A recognised non-host architecture is covered by the native filter's architecture gate,
        // which refuses any syscall not made under the host arch — at least as strict as the
        // profile's own rules for it, so nothing is weakened (CLAUDE.md §3).
    }
    if !host_listed {
        return Err(OciError::Config(format!(
            "seccomp: architectures {arches:?} do not include the host {host_name}; Hex refuses \
             rather than apply a filter the profile did not mean for this host"
        )));
    }
    Ok(())
}

/// Refuses a flag Hex can't honour, and accepts those that don't make the filter less restrictive.
fn check_flags(flags: &[String]) -> Result<(), OciError> {
    for f in flags {
        match f.as_str() {
            // Hex applies the filter to the init's one thread (it re-exec'd). These flags don't
            // loosen it: TSYNC syncs threads (trivial with one), LOG only logs, SPEC_ALLOW opts out
            // of the speculation mitigation Hex leaves on. Honoured with the filter as built.
            "SECCOMP_FILTER_FLAG_TSYNC"
            | "SECCOMP_FILTER_FLAG_TSYNC_ESRCH"
            | "SECCOMP_FILTER_FLAG_LOG"
            | "SECCOMP_FILTER_FLAG_SPEC_ALLOW" => {}
            "SECCOMP_FILTER_FLAG_NEW_LISTENER" | "SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV" => {
                return Err(OciError::Config(format!(
                    "seccomp: flag {f} needs a user-notification listener, which Hex does not implement"
                )));
            }
            other => {
                return Err(OciError::Config(format!("seccomp: unknown flag {other:?}")));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::oci::spec::SeccompSyscall;

    // Builders for terse profiles.
    fn arg(index: u32, value: u64, value_two: Option<u64>, op: &str) -> SeccompArg {
        SeccompArg {
            index,
            value,
            value_two,
            op: op.to_owned(),
        }
    }
    fn group(
        names: &[&str],
        act: &str,
        errno: Option<u32>,
        args: Vec<SeccompArg>,
    ) -> SeccompSyscall {
        SeccompSyscall {
            names: names.iter().map(|s| (*s).to_owned()).collect(),
            action: act.to_owned(),
            errno_ret: errno,
            args,
        }
    }
    fn profile(
        default_action: &str,
        default_errno: Option<u32>,
        arches: &[&str],
        flags: &[&str],
        syscalls: Vec<SeccompSyscall>,
    ) -> Seccomp {
        Seccomp {
            default_action: default_action.to_owned(),
            default_errno_ret: default_errno,
            architectures: arches.iter().map(|s| (*s).to_owned()).collect(),
            flags: flags.iter().map(|s| (*s).to_owned()).collect(),
            syscalls,
        }
    }

    /// The host's `AUDIT_ARCH_*` value, which the filter's architecture gate checks `seccomp_data.arch`
    /// against (same definitions as the vendored seccompiler's `bpf.rs`).
    fn audit_arch() -> u32 {
        match std::env::consts::ARCH {
            "x86_64" => 62 | 0x8000_0000 | 0x4000_0000,
            "aarch64" => 183 | 0x8000_0000 | 0x4000_0000,
            "riscv64" => 243 | 0x8000_0000 | 0x4000_0000,
            other => panic!("the seccomp oracle test runs on x86_64/aarch64/riscv64, not {other}"),
        }
    }

    /// A `struct seccomp_data` as the kernel passes the BPF program: nr, arch, ip, args[6].
    fn seccomp_data(nr: i64, args: [u64; 6]) -> [u8; 64] {
        let mut d = [0u8; 64];
        d[0..4].copy_from_slice(&i32::try_from(nr).unwrap().to_le_bytes());
        d[4..8].copy_from_slice(&audit_arch().to_le_bytes());
        for (i, a) in args.iter().enumerate() {
            let off = 16 + i * 8;
            d[off..off + 8].copy_from_slice(&a.to_le_bytes());
        }
        d
    }

    fn load_word(d: &[u8; 64], k: u32) -> u32 {
        let off = usize::try_from(k).unwrap();
        u32::from_le_bytes([d[off], d[off + 1], d[off + 2], d[off + 3]])
    }

    /// Interprets the seccomp-BPF subset seccompiler emits and returns the action word for a
    /// synthetic syscall — an oracle for the compiled filter, needing no kernel and no privilege.
    fn run(prog: &BpfProgram, nr: i64, args: [u64; 6]) -> u32 {
        let d = seccomp_data(nr, args);
        let (mut pc, mut acc) = (0usize, 0u32);
        for _ in 0..1_000_000 {
            let ins = &prog[pc];
            match ins.code {
                0x20 => {
                    acc = load_word(&d, ins.k);
                    pc += 1;
                } // BPF_LD | BPF_W | BPF_ABS
                0x54 => {
                    acc &= ins.k;
                    pc += 1;
                } // BPF_ALU | BPF_AND | BPF_K
                0x15 => pc += 1 + usize::from(if acc == ins.k { ins.jt } else { ins.jf }), // JEQ
                0x25 => pc += 1 + usize::from(if acc > ins.k { ins.jt } else { ins.jf }),  // JGT
                0x35 => pc += 1 + usize::from(if acc >= ins.k { ins.jt } else { ins.jf }), // JGE
                0x05 => pc += 1 + usize::try_from(ins.k).unwrap(),                         // JA
                0x06 => return ins.k,                                                      // RET|K
                other => panic!("unexpected bpf opcode {other:#x} at pc {pc}"),
            }
        }
        panic!("the filter never returned");
    }

    const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
    fn errno(e: u32) -> u32 {
        libc::SECCOMP_RET_ERRNO | (e & libc::SECCOMP_RET_DATA)
    }

    // do X: compile a profile whose default is ERRNO(ENOSYS) and that allows read/write and refuses
    // getpid with EPERM; expect Y: the compiled filter returns each syscall's own action, and the
    // default for an unlisted one (CLAUDE.md §6; AC-T7.1.4 — the oci class honours linux.seccomp).
    #[test]
    fn compiled_filter_returns_each_syscalls_own_action() {
        let p = profile(
            "SCMP_ACT_ERRNO",
            Some(38),
            &[],
            &[],
            vec![
                group(&["read", "write"], "SCMP_ACT_ALLOW", None, vec![]),
                group(&["getpid"], "SCMP_ACT_ERRNO", Some(1), vec![]),
            ],
        );
        let prog = compile(&p).unwrap();
        assert_eq!(run(&prog, libc::SYS_read, [0; 6]), ALLOW);
        assert_eq!(run(&prog, libc::SYS_write, [0; 6]), ALLOW);
        assert_eq!(run(&prog, libc::SYS_getpid, [0; 6]), errno(1));
        // close is unlisted: the default action, not silently allowed.
        assert_eq!(run(&prog, libc::SYS_close, [0; 6]), errno(38));
    }

    // do X: a syscall allowed in general but refused for one argument value (Docker's AF_NETLINK
    // `socket` rule); expect Y: the matching argument gets the rule's errno, every other call the
    // syscall's own fallthrough action, not the filter-wide default.
    #[test]
    fn a_syscall_refused_for_one_argument_is_allowed_for_the_rest() {
        let p = profile(
            "SCMP_ACT_ERRNO",
            Some(38),
            &[],
            &[],
            vec![
                group(&["socket"], "SCMP_ACT_ALLOW", None, vec![]),
                group(
                    &["socket"],
                    "SCMP_ACT_ERRNO",
                    Some(22),
                    vec![arg(0, 16, None, "SCMP_CMP_EQ")], // AF_NETLINK
                ),
            ],
        );
        let prog = compile(&p).unwrap();
        assert_eq!(run(&prog, libc::SYS_socket, [16, 0, 0, 0, 0, 0]), errno(22));
        assert_eq!(run(&prog, libc::SYS_socket, [2, 0, 0, 0, 0, 0]), ALLOW); // AF_INET
        assert_eq!(run(&prog, libc::SYS_read, [0; 6]), errno(38)); // unlisted → default
    }

    // do X: the default profile's own `socket` shape — all argument rules, of differing actions: one
    // refuses AF_NETLINK audit sockets (arg0 == AF_NETLINK and arg2 == NETLINK_AUDIT) with EINVAL, two
    // more allow every other call. The one-match-per-syscall model used to refuse this; expect Y: the
    // compiled filter refuses exactly the audit-netlink socket and allows the rest (first match wins).
    #[test]
    fn argument_rules_of_differing_actions_compile_to_one_filter() {
        let p = profile(
            "SCMP_ACT_ERRNO",
            Some(38),
            &[],
            &[],
            vec![
                group(
                    &["socket"],
                    "SCMP_ACT_ERRNO",
                    Some(22),
                    vec![
                        arg(0, 16, None, "SCMP_CMP_EQ"), // AF_NETLINK
                        arg(2, 9, None, "SCMP_CMP_EQ"),  // NETLINK_AUDIT
                    ],
                ),
                group(
                    &["socket"],
                    "SCMP_ACT_ALLOW",
                    None,
                    vec![arg(2, 9, None, "SCMP_CMP_NE")],
                ),
                group(
                    &["socket"],
                    "SCMP_ACT_ALLOW",
                    None,
                    vec![arg(0, 16, None, "SCMP_CMP_NE")],
                ),
            ],
        );
        let prog = compile(&p).unwrap();
        // AF_NETLINK + NETLINK_AUDIT: the one combination refused, with the rule's errno.
        assert_eq!(run(&prog, libc::SYS_socket, [16, 0, 9, 0, 0, 0]), errno(22));
        // AF_NETLINK, a different protocol: allowed.
        assert_eq!(run(&prog, libc::SYS_socket, [16, 0, 3, 0, 0, 0]), ALLOW);
        // AF_INET with protocol 9: allowed (not a netlink socket).
        assert_eq!(run(&prog, libc::SYS_socket, [2, 0, 9, 0, 0, 0]), ALLOW);
        // An ordinary socket: allowed.
        assert_eq!(run(&prog, libc::SYS_socket, [2, 0, 0, 0, 0, 0]), ALLOW);
    }

    // do X: a masked-equal rule `(arg0 & 0xF) == 0x1`; expect Y: only arguments whose low nibble is
    // 1 match, confirming value is the mask and valueTwo the compared datum (runc semantics).
    #[test]
    fn masked_equal_maps_value_to_mask_and_value_two_to_datum() {
        let p = profile(
            "SCMP_ACT_ALLOW",
            None,
            &[],
            &[],
            vec![group(
                &["getpid"],
                "SCMP_ACT_ERRNO",
                Some(1),
                vec![arg(0, 0xF, Some(0x1), "SCMP_CMP_MASKED_EQ")],
            )],
        );
        let prog = compile(&p).unwrap();
        assert_eq!(run(&prog, libc::SYS_getpid, [0x1, 0, 0, 0, 0, 0]), errno(1));
        assert_eq!(
            run(&prog, libc::SYS_getpid, [0x11, 0, 0, 0, 0, 0]),
            errno(1)
        ); // 0x11 & 0xF == 1
        assert_eq!(run(&prog, libc::SYS_getpid, [0x2, 0, 0, 0, 0, 0]), ALLOW); // 2 & 0xF != 1
    }

    // do X: a 64-bit-wide equality; expect Y: a value differing only in the high 32 bits does not
    // match, so Hex compares the full argument (Qword), not just the low word.
    #[test]
    fn equality_compares_the_full_sixty_four_bit_argument() {
        let p = profile(
            "SCMP_ACT_ALLOW",
            None,
            &[],
            &[],
            vec![group(
                &["getpid"],
                "SCMP_ACT_ERRNO",
                Some(1),
                vec![arg(0, 5, None, "SCMP_CMP_EQ")],
            )],
        );
        let prog = compile(&p).unwrap();
        assert_eq!(run(&prog, libc::SYS_getpid, [5, 0, 0, 0, 0, 0]), errno(1));
        assert_eq!(
            run(&prog, libc::SYS_getpid, [(1 << 32) | 5, 0, 0, 0, 0, 0]),
            ALLOW
        );
    }

    // do X: a syscall named only for another architecture; expect Y: it is skipped (it has no number
    // here, so the rule can't apply), the profile still compiles, and nothing else changes.
    #[test]
    fn a_syscall_absent_on_this_arch_is_skipped_not_refused() {
        let p = profile(
            "SCMP_ACT_ALLOW",
            None,
            &[],
            &[],
            vec![group(
                &["this_is_not_a_syscall_42"],
                "SCMP_ACT_ERRNO",
                Some(1),
                vec![],
            )],
        );
        let prog = compile(&p).unwrap();
        assert_eq!(run(&prog, libc::SYS_read, [0; 6]), ALLOW);
    }

    // do X: a profile that lists the host arch plus a compat arch, with a TSYNC flag; expect Y: it
    // compiles (the host is covered; the flag doesn't loosen the filter).
    #[test]
    fn host_arch_plus_a_compat_arch_and_benign_flags_compile() {
        let host = host_scmp_arch(host_target_arch().unwrap());
        let p = profile(
            "SCMP_ACT_ALLOW",
            None,
            &[host, "SCMP_ARCH_X86"],
            &["SECCOMP_FILTER_FLAG_TSYNC"],
            vec![group(&["read"], "SCMP_ACT_ERRNO", Some(1), vec![])],
        );
        assert!(compile(&p).is_ok());
    }

    // do X: every profile a reviewer would expect refused; expect Y: each is a typed Config error
    // naming why, never a silent drop or a panic (CLAUDE.md §3, §6).
    #[test]
    fn unsupported_profiles_are_refused_by_name() {
        let host = host_scmp_arch(host_target_arch().unwrap()).to_owned();
        let cases: Vec<(&str, Seccomp)> = vec![
            (
                "NOTIFY",
                profile(
                    "SCMP_ACT_ALLOW",
                    None,
                    &[],
                    &[],
                    vec![group(&["read"], "SCMP_ACT_NOTIFY", None, vec![])],
                ),
            ),
            (
                "unknown action",
                profile("SCMP_ACT_FOO", None, &[], &[], vec![]),
            ),
            (
                "host arch absent",
                profile("SCMP_ACT_ALLOW", None, &["SCMP_ARCH_PPC64"], &[], vec![]),
            ),
            (
                "unknown arch",
                profile("SCMP_ACT_ALLOW", None, &["SCMP_ARCH_BOGUS"], &[], vec![]),
            ),
            (
                "unknown op",
                profile(
                    "SCMP_ACT_ALLOW",
                    None,
                    &[],
                    &[],
                    vec![group(
                        &["read"],
                        "SCMP_ACT_ERRNO",
                        Some(1),
                        vec![arg(0, 1, None, "SCMP_CMP_BOGUS")],
                    )],
                ),
            ),
            (
                "arg index out of range",
                profile(
                    "SCMP_ACT_ALLOW",
                    None,
                    &[],
                    &[],
                    vec![group(
                        &["read"],
                        "SCMP_ACT_ERRNO",
                        Some(1),
                        vec![arg(6, 1, None, "SCMP_CMP_EQ")],
                    )],
                ),
            ),
            (
                "listener flag",
                profile(
                    "SCMP_ACT_ALLOW",
                    None,
                    &[],
                    &["SECCOMP_FILTER_FLAG_NEW_LISTENER"],
                    vec![],
                ),
            ),
            (
                "unknown flag",
                profile(
                    "SCMP_ACT_ALLOW",
                    None,
                    &[],
                    &["SECCOMP_FILTER_FLAG_WHAT"],
                    vec![],
                ),
            ),
            (
                "two unconditional actions",
                profile(
                    "SCMP_ACT_ERRNO",
                    Some(38),
                    &[],
                    &[],
                    vec![
                        group(&["read"], "SCMP_ACT_ALLOW", None, vec![]),
                        group(&["read"], "SCMP_ACT_KILL", None, vec![]),
                    ],
                ),
            ),
        ];
        // The host arch alone must still be accepted (a control for the "host arch absent" case).
        assert!(compile(&profile("SCMP_ACT_ALLOW", None, &[&host], &[], vec![])).is_ok());
        for (why, p) in cases {
            assert!(
                matches!(compile(&p), Err(OciError::Config(_))),
                "{why} should be a Config refusal, got {:?}",
                compile(&p)
            );
        }
    }
}
