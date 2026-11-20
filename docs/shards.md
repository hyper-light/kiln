# Shards and Hex in kiln

kiln runs coding agents' harnesses in Shards microVMs, with Hex as their runtime (kiln plan
01a1cd8e862e7169a074db3d5fa33f3d). This page is step 16's: how kiln holds both, carries Shards, and checks
Agentfiles with Shards' own parser. Shards' side of the same decision is its D237 (its
`docs/design/architecture.md`).

## One Shards revision

kiln vendors Shards (`vendor/shards`) and Hex (`vendor/hex`) as it vendors every product (docs/vendoring.md):
each at one revision, with kiln's recorded changes, and one copy of each project in kiln's tree.

- **Shards** is pinned at a revision of its `main` (`vendor/MANIFEST.toml`; `vendor/shards/UPSTREAM.md`).
- **Hex** is pinned at a commit of its own, with kiln's records of hyper-raft's changes and kiln's layout as
  recorded changes (`vendor/hex/UPSTREAM.md`, which says what Hex holds at that commit). Hex vendors no Shards: it
  names Shards' crates by git at one revision (its root manifest's `[workspace.dependencies]`), which kiln's root
  manifest points at kiln's own copy (`[patch."https://github.com/hyper-light/shards"]`), so one Shards revision
  serves Hex and kiln, at one version: Hex asks for exactly the commit kiln vendors (`7158020b61`, Shards' main),
  and `crates/kiln/tests/vendored.rs` fails the gate where they differ. kiln's copy holds Hex's `vendor/MANIFEST.toml` and `vendor/patches`,
  so `scripts/vendor.py check` holds Hex's pins of what it does vendor to kiln's. The two move together:
  `scripts/vendor.py shards <commit>` at Hex's new Shards revision, then `scripts/vendor.py hex <commit>`; and
  hyper-raft with them where Hex's pin of it moves (`e0613f52` since Shards' agent-control link reads the negotiated
  idle timeout of hyper-quic's patch Q34 and a handshake's progress, Q35), each product that holds a pin of it
  (Quiver) moved to kiln's by a recorded change, and Hex's own pin of it kiln's. Shards names hyper-quic by its git
  revision, which kiln's root manifest points at its copy (`[patch."https://github.com/hyper-light/hyper-raft"]`).
- **hyper-raft** gains Hex's one change Hex's code calls (`Endpoint::negotiated`, the suite and group a QUIC
  connection negotiated), and Quiver, which records kiln's changes to hyper-raft, records it too
  (`vendor/hyper-raft/UPSTREAM.md`, `vendor/quiver/UPSTREAM.md`).

kiln links `shards-agentfile`, `shards-api`, `shards-image` and `hex-node` by name (each pinned exactly in
`crates/kiln/Cargo.toml`, patched to its copy in the root manifest). `shards-image` is Shards' image store, where
kiln finds the images the carried `shards` built for its microVMs (`sandbox::image`, docs/isolation.md): one
revision of Shards writes the store and reads it.

## kiln carries Shards

kiln is the one file there is to install, and Hex's node boots harness microVMs through `shards-api`, a client
of the `shards` binary. So kiln carries a `shards` and the two processes it starts, written out by kiln as
Shards writes out its own (Shards' D36, D182):

| Carried | What it is |
|---|---|
| `shards` | Shards' binary, built to be carried (`SHARDS_HELPERS=beside`, Shards' D237): its guests (shards-init, the in-VM server) inside it; its VM and network processes beside it, which it starts |
| `shards-vm` | the VM process: one per microVM, on macOS in App Sandbox with the hypervisor entitlement |
| `shards-net` | the network process: one per networked microVM, on macOS in App Sandbox with the network's client and server |

- **Built** by kiln's own build: `crates/kiln/build.rs` runs `ci/shards/pack.sh` from `vendor/shards` for the
  platform kiln is built for (its output in `OUT_DIR`, its cargo target directory `target/shards` beside kiln's, so a
  build where `vendor/shards` hasn't changed takes its last), with Shards' own toolchain file (its Rust, its musl
  targets) and the zig it pins for its guests' C (`scripts/install-zig` installs it where none is on `PATH`); on
  Windows, Git for Windows' bash runs it where none is on `PATH`. On Linux the VM and network processes are static
  (musl), as Shards builds its own. A target whose Shards is built only on its own platform (Windows' MSVC build,
  which the release's Linux job cross-builds kiln beside) takes it from the folder `KILN_SHARDS_DIR` names, made
  there by the same script (`.kiln/release.yaml`'s `shards-windows`).
- **Embedded**, always: kiln is one binary with its microVM engine in it. A build that can't carry Shards fails,
  saying why; no kiln is built without it. The one exception is a lint, asked by name (`KILN_SHARDS=lint`, kiln's
  gate's clippy through zig for macOS and Windows from Linux): it checks kiln's code against empty stand-ins, and
  the binary it makes refuses to start.
- **Written out** by `crate::shards::write_out` into `<base>/shards/<id>/`: named by what's carried and, on a
  Mac, by who signs it; written whole (beside its place, each file synced and signed, its ready mark last,
  then renamed into place), so no process runs half a folder or another build's. A process holds the folder
  it uses while it keeps the answer (`Held`, a shared lock on the folder's `lock`); the process that writes a
  folder out removes every other one no process holds, as Shards' D182 does.
- **Signed on a Mac** as kiln signs its own builds (`os::macsign`): the VM process with Shards' entitlements
  (`vendor/shards/resources/vm.entitlements`: App Sandbox, the hypervisor), the network process with its own
  (`net.entitlements`), both under the Hardened Runtime, as Shards' build signs them; `shards` itself without
  entitlements. kiln's identity signs them where `kiln signing setup` made one, so macOS knows each as the
  program it was across kiln's upgrades (its local network privacy keys a program by its signature); else ad
  hoc, as Shards' own builds are. Each signer gets its own folder.

### Why not the others

- *`shards` writing its own helpers out (Shards' D36):* its helpers would be the bytes its build signed, ad hoc,
  and only where kiln is built on a Mac (kiln's releases are cross-built on Linux); no carrier can sign them
  again, as a file signed again is another size, which `shards` takes for a copy cut short.
- *Linking Shards' command as a library and serving `api-run` from kiln:* `crates/shards` is one binary's crate;
  as a library it would bring all of Shards' daemon, builder and registry into kiln, and its command line into
  kiln's.
- *`SHARDS_VM_BINARY`:* it names a VM process alone, for tests and VMM work, and Shards' configured client
  clears every variable it does not name.

## Agentfiles checked in kiln's process

Every Agentfile kiln keeps (`agentfiles.create`, `update`, `check`) is checked by Shards' own parser and build
checks, `shards-agentfile`, in kiln's process (`crates/kiln/src/agentfiles/check.rs`): the lint subrequest
`shards build --call=check` answers, planned to a target no stage is named so it stops before any base is
resolved. A text Shards refuses is refused in Shards' words, at its line; its warnings are diagnostics that
refuse nothing. No server needs a `shards` on its `PATH` for it, and none reports `checked: false` but where
the check itself never ended.

## Tests and what is owed

- `cargo test -p kiln --lib agentfiles::check shards::` and `agentfiles::builtin`: the in-process check
  against Shards' recorded words, every built-in Agentfile passing it; the carried folder written whole,
  held, collected.
- `cargo test -p kiln --test agentfiles`: Agentfiles on every surface, refused in Shards' words.
- `docs/research/s16-vendor/smoke.sh`, on a host with a hypervisor: packs Shards, fetches Shards' pinned kernel,
  builds a PID 1 that powers off, and runs `tests/shards_carried.rs`, which writes the carried Shards out,
  checks each signature on a Mac, and boots the guest through `shards-api` with the carried `shards`.
- `docs/research/s16-vendor/measure.sh <base> <head> [runs]`: clean release builds of kiln before step 16, with
  Hex and Shards' crates, and carrying Shards, and `ci/shards/pack.sh`'s own; each one's time and binary size,
  with n, p50, p90, p99 and max.

Owed: the smoke and the measurements on macOS (HVF), Linux (KVM) and Windows (WHP), run by the plan's
coordinator, their numbers recorded here and in Shards' D237; Hex's own tests at kiln's revision.
