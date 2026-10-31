# socket2, vendored

- **Upstream:** https://github.com/rust-lang/socket2, commit
  `8b8b96fdaa1301698fd2b6f4de188635df4a8bd1`, version 0.6.5, MIT OR Apache-2.0
  (`LICENSE-MIT`, `LICENSE-APACHE`). The complete upstream tree is taken.
- **Why this pin:** Amphora requires this revision's safe Windows `set_exclusive_address`
  setter (upstream PR #678). Kiln's registry and Amphora's git dependencies both resolve to
  this one copy through the root Cargo patches (CP-M1, AC-M1.4).

## Changes from upstream

Patches in `vendor/patches/socket2/` apply in name order.

| File | Change | Why | Recorded as |
|---|---|---|---|
| `src/lib.rs`, `src/sockaddr.rs`, `src/sys/unix.rs` | add `target_os = "linux"` to all six IUCV cfg predicates, retaining feature, architecture and GNU conditions | CP-M1: Windows GNU otherwise selects Linux-only APIs without their implementation or libc; preserve Linux GNU x86_64/s390x IUCV and the Windows exclusive-address setter | `0001-cp-m1-restrict-iucv-apis-to-linux-gnu.patch` |

## Taking a newer upstream

`python3 scripts/vendor.py socket2 <full-commit>` replays the recorded correction. Keep the
exclusive-address setter and the Linux-only IUCV guards when selecting a successor.
Upstream publication requires the person's go-ahead; no upstream branch has been published.
