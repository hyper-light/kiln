# Hex's changes to seccompiler

seccompiler is vendored (vendor/MANIFEST.toml) at `c3cf77d65815037931ae5bc2fca010713defdc8c`.
Each recorded change (`vendor/patches/seccompiler/*.patch`, applied in name order) is a row here.
`scripts/vendor.py check seccompiler` proves the tree is that upstream plus these, byte for byte.

| Files | Change | Why | Patch |
| --- | --- | --- | --- |
| `src/lib.rs`, `src/syscall_table/mod.rs`, `src/backend/mod.rs`, `src/backend/filter.rs` | Expose the syscall-name table without the `json` feature (`mod syscall_table` unconditional, `SyscallTable` public), give `SeccompFilter` per-rule actions (`SyscallRules`, `with_syscall_rules`), and size each syscall's BPF block for what it holds. | Hex's `oci` class compiles an OCI `linux.seccomp` profile to a BPF program with this crate and loads it in the container init. It resolves syscall names per target arch but parses the OCI format itself, so it needs the tables without pulling in `serde_json`. An OCI profile names an action per syscall group and can give one syscall argument rules of *differing* actions — the containers/common default refuses `socket(AF_NETLINK, _, NETLINK_AUDIT)` with EINVAL but allows every other `socket` — which `SeccompFilter::new`'s one filter-wide match action can't express; each rule in `SyscallRules` carries its own on-match action, beside the syscall's own fallthrough. Sizing a syscall's block for the unconditional case allocates it once instead of growing it (the default profile: 1272 → 574 allocations, 43.7 → 32.4 µs). `SeccompFilter::new` keeps its signature and byte-for-byte BPF output (upstream tests pass). | `0001-oci-seccomp-expose-the-syscall-table-and-compile-per-rule.patch` |
