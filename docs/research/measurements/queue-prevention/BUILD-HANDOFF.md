# Next signed prevention build: cache handoff

Prepared read-only on 2026-11-11 07:38–07:42 UTC by implementer
`/root/kiln_status_trace`. Shards policy `7d7e0855a5073ed5163b4fe7ed3355c9ed7c2506`,
rules SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`;
full policy/evidence/CLAUDE and current Kiln ROOT read. Rust remains frozen.
No builds, checkout/fetch, install, restart, kill or service changes were performed.
Native CLI Reach metadata requests used `cpus=1,mem=1g,disk=1g`, `--no-sync`.
Local socket access required approved execution outside the filesystem sandbox.

Safe raw receipt: [build-cache-20261111T074141Z.json](build-cache-20261111T074141Z.json),
SHA-256 `0c09323c996aa020f242f58d0a56bb86098cdcaf4d95a63443d5c768fdfffa7d`.
This record prepares commands; it is not new-commit build, smoke or deployment proof.

## Source identity and reuse

Read-only `git ls-remote https://github.com/hyper-light/kiln.git refs/heads/main`
on d132 returned **`f555736bd887a4196c8841564cb9b4d6c1ede95f`** at 07:39 UTC.
The same local command failed DNS. No ref was fetched or merged.

Root authorizes reuse of the known **remote** f555 source directory for the next
signed SHA, after preserving its completed artifact/proof and verifying content-clean
HEAD=f555. Its absolute source path matters to Cargo path-package cache identity.
Use normal, non-force checkout of the signed commit. Do not reset, clean, stash,
discard local changes or replace that directory. Any dirty/untracked source blocks
the checkout until root preserves it explicitly. The completed **local** checkout
`/private/tmp/kiln-offline-fix-20261110` stays untouched.

## Mac ARM64 host producing Linux amd64 musl

Verified runner: `d1323898b255a3ed435eb83fa0f7b0cf` (Darwin arm64).

| Item | Verified value |
| --- | --- |
| Source | `/Users/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/kiln-offline-fix-20261110-8a5ae447ef` |
| Current source HEAD | `f555736bd887a4196c8841564cb9b4d6c1ede95f` |
| Cargo target directory | `/Users/adalundhe/.cache/kiln-offline-release-20261111/mac-linux-target` |
| Pinned tools | `/Users/adalundhe/.cache/kiln-offline-release-20261111/mac-zig` |
| Rust/cargo | rustc 1.99.1 (58cf8f9c3), cargo 1.99.1 |
| Zig / cargo-zigbuild | 0.17.0 / 0.23.4, verified through the private venv |
| Node / npm | `/Users/adalundhe/.nvm/versions/node/v26.13.0/bin/{node,npm}` |
| Free space at 07:38 | 2,330,319,577,088 bytes |
| Existing Linux artifact | target `x86_64-unknown-linux-musl/release/kiln`, 353,177,136 bytes; source `dist/kiln-x86_64-unknown-linux-musl` also present |
| Existing UI cache | `ui/node_modules`, `ui/dist/client/_shell.html`, `ui/dist/server` present, ordinary paths |

Keep the required Mac floor **30 GiB plus the admitted build scratch**. ROOT's
successful r73 path used normal 2 CPU / 32 GiB / 8 GiB admission. The current 1 CPU
metadata request is not a build allocation. Recheck actual space/admission at start.

The cached compiler wrapper is exactly:

```
/Users/adalundhe/Library/Caches/cargo-zigbuild/0.23.4/wrappers/6a87/zigcc-x86_64-unknown-linux-musl-e859.sh
```

It exports `CARGO_ZIGBUILD_ZIG_VERSION=0.17.0` and
`CARGO_ZIGBUILD_ZIG_COMMAND=/Users/adalundhe/.cache/kiln-offline-release-20261111/mac-zig/bin/python-zig`.
That `python-zig` console script exists. Preserve this exact command; switching to
the older `mac-zig/zigbin/zig` changes compiler identity. That older wrapper exists
and requires `KILN_TASK_ZIGROOT`, but is **not** the command named by the active
compiler wrapper. Global `cargo-zigbuild` and `bun` were absent from PATH; call
the venv tool by its absolute path. No global PATH/environment dump was taken.

The actual jemalloc fingerprint records the above compiler as
`CC_x86_64_unknown_linux_musl`; `CC_x86_64-unknown-linux-musl`, `CFLAGS`, target
CFLAGS, `JEMALLOC_OVERRIDE`, malloc/page/hugepage/quantum/vaddr overrides are unset.
The kiln fingerprint has `rustflags=[]` and features `["jemalloc"]`. Do not invent
new allocator overrides or disable the allocator to get a link through.

The repaired cache is intact at
`mac-linux-target/x86_64-unknown-linux-musl/release/build/tikv-jemalloc-sys-e6cba4955ddf98aa/out/lib/`:

| Archive | Bytes | SHA-256 | Zig ar members |
| --- | ---: | --- | ---: |
| `libjemalloc.a` | 7,345,622 | `a46a50443f66cca793241ef2ce94f55e2569e82c0dcc8ecc647a010cb244c6ef` | 62 |
| `libjemalloc_pic.a` | 7,345,812 | `88ecb7419a5d08f7d184abcbc0553a852480674301384fff8aa7bb2ae3288396` | 62 |

`zig ar t` exits 0 and lists actual jemalloc/arena/etc. objects. The generated
Makefile still says `AR=ar`: ROOT's earlier Mac archive issue produced 96-byte
archives, then r73 repaired/rebundled them with Zig ar. Preserving the current
fingerprint reuses the repaired outputs. If a dependency rebuild regenerates empty
archives, stop and retain the evidence; do not repeatedly relink or delete whole
caches. That repair is a separate root-owned mutation, not part of this handoff.
If the next commit changes jemalloc/dependency configuration, these old artifacts
are no longer proof of its generated library.

The following is a **future root-run recipe**, reconstructed from the verified
active cache and source release interface; no complete r73 shell file was found.
Run only after the signed prevention SHA is authorized and available, under its
normal admitted build allocation. Before checkout, record old artifact hashes and
preserve the old proof outside files the checkout can replace.

```sh
set -eu
PREVENTION_SHA='<authorized signed prevention SHA>'
PREVENTION_SRC='/Users/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/kiln-offline-fix-20261110-8a5ae447ef'
PREVENTION_CACHE='/Users/adalundhe/.cache/kiln-offline-release-20261111'
cd "$PREVENTION_SRC"
test "$(git rev-parse HEAD)" = f555736bd887a4196c8841564cb9b4d6c1ede95f
PREVENTION_STATUS=$(git --no-optional-locks status --porcelain --untracked-files=all)
test -z "$PREVENTION_STATUS"
git fetch https://github.com/hyper-light/kiln.git "$PREVENTION_SHA"
git verify-commit "$PREVENTION_SHA"
git checkout --detach "$PREVENTION_SHA"
test "$(git rev-parse HEAD)" = "$PREVENTION_SHA"
export KILN_COMMIT="$PREVENTION_SHA"
export CARGO_TARGET_DIR="$PREVENTION_CACHE/mac-linux-target"
export CARGO_ZIGBUILD_ZIG_COMMAND="$PREVENTION_CACHE/mac-zig/bin/python-zig"
export PATH='/Users/adalundhe/.cargo/bin:/Users/adalundhe/.nvm/versions/node/v26.13.0/bin:'"$PATH"
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS JEMALLOC_OVERRIDE
unset X86_64_UNKNOWN_LINUX_MUSL_JEMALLOC_OVERRIDE
unset KILN_UI_SERVER
# Rebuild from this signed UI, or import root's independently verified same-source UI artifact.
npm ci --prefix ui --no-audit --no-fund
KILN_UI_SERVER=off npm run --prefix ui build
node ui/scripts/pack-ui-server.mjs --require --target x86_64-unknown-linux-musl
nice -n 10 "$PREVENTION_CACHE/mac-zig/bin/cargo-zigbuild" zigbuild \
  --release --locked -p kiln --target x86_64-unknown-linux-musl --features jemalloc -j 2
mkdir -p dist
cp "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/kiln" dist/kiln-x86_64-unknown-linux-musl
shasum -a 256 dist/kiln-x86_64-unknown-linux-musl
file dist/kiln-x86_64-unknown-linux-musl
```

`ci/release/build.sh` explicitly rejects Linux targets when running on a Mac, so
this approved cross path invokes the pinned cargo-zigbuild directly. Keep real
Linux Bun/UI packing enabled. The Linux executable cannot be smoked on this Mac:
root must check ELF/static architecture, transfer with a matching hash and run
native Linux `--version`/appropriate focused smoke before promotion. This recipe
has **not** built the prevention SHA. It does not assert cache hits or build timing.

## Native Windows: current access unverified

Runner `f766f09294f2e17e3b8a631ffd4e6a4b`. Known prior Reach source suffix:
`e30189ea29182644fe5a4b2e06c7dac2/kiln-offline-fix-20261110-8a5ae447ef`, under
that runner's home `.kiln/reach`. Its absolute home and previous target/cache path
were **not reverified**. Native CLI and one MCP fallback both returned
`did not answer in 15s`, with no command output. No further retries/probes were run.
Do not invent a Windows cache directory or infer that zero output means no cache.

Historical ROOT evidence: native f555 r38 completed after 63m05s, artifact
414,221,824 bytes, SHA-256
`14ad7179f912bf132fefd738d159ace4b3ae8bea1283bc9c2b80a613723f9c2d`, native
`--version` exited 0. This is old-release proof, not current path/tool readiness.

When root's next native build leg can reach it, inspect only that known source
directory's HEAD, clean status, Cargo target/default `target` and dependency build
metadata, plus tool versions/free space. Reuse the verified old source path with
the same clean/signed/non-force checkout procedure above. Record the actual
`CARGO_TARGET_DIR` from the old build receipt/fingerprint if recoverable; otherwise
say cache reuse is unknown rather than assign an invented path.

Conditional native PowerShell build body, **not runnable until those variables,
MSVC toolchain and normal resource allocation are verified**:

```powershell
Set-Location $VerifiedOldReachSource
$env:KILN_COMMIT = $AuthorizedSignedPreventionSha
$env:CARGO_TARGET_DIR = $VerifiedPriorCargoTargetDir
npm ci --prefix ui --no-audit --no-fund
if ($LASTEXITCODE -ne 0) { throw 'npm ci failed' }
$env:KILN_UI_SERVER = 'off'
npm run --prefix ui build
if ($LASTEXITCODE -ne 0) { throw 'UI build failed' }
Remove-Item Env:KILN_UI_SERVER
node ui/scripts/pack-ui-server.mjs --require --target x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { throw 'Windows UI pack failed' }
cargo build --release --locked -p kiln --target x86_64-pc-windows-msvc -j $VerifiedAdmittedCpuCount
if ($LASTEXITCODE -ne 0) { throw 'native Windows build failed' }
```

Then root records the exact resulting path/hash, PE architecture and native smoke.
No Windows native tools, current free space, cache hit, new artifact or prevention
deployment is claimed by this preparation.
