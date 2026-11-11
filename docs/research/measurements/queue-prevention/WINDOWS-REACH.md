# Windows Reach failure: retained evidence and proposed narrow repair

2026-11-11. Implementer: kiln_status_trace; coordinator: root. POLICY_READ:
Shards origin/main `3830327b6c8d7ac22452a5e32ffa32799263132c`, rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Full AGENTS/rules/evidence/CLAUDE, Kiln AGENTS/ci§0 and ROOT reread. Production
Rust is frozen after the expressly approved narrow portability change below.
This record does not establish a live Windows repair or deployment.

## Observed failure

Windows `f766f09294f2e17e3b8a631ffd4e6a4b` is connected on normal server control.
The 08:38:54 UTC update inventory identified current main PID17420, f555 build
`14ad7179f912`, jobs0/agents4/terminals0. These are a dated observation, not current
zero-work permission. Its reported compiled facts at 08:45:53 name source
`f555736bd887a4196c8841564cb9b4d6c1ede95f`, Windows amd64; the underlying facts
sample is older. Reach directory at 08:42:32 advertises it with version2 and a
fingerprint but no feature list. The same response advertises all15 f555 features
for the Mac and primary Linux. This discrepancy does not prove a mixed binary.

Mac-initiated Windows requests had failed in15s. One separately authorized exact
metadata request from the primary Linux f555 initiator also exited1 in15.104s.
The server journal at 08:46:08–09 records that Windows refused that link
`01a1c9c8acc970c38ae28022f865ee58`: it observed zero of a signed relay record's one
frame, with an all-zero received frame digest. The same failure occurred from
the Mac earlier. This is evidence of missing/unreadable frames, not proof of
malicious mutation. The frame/custody checks must remain strict.

One normal control DirList GET at 08:47:47 returned HTTP200, exists=true and
truncated=false for the known source bundle:
`C:\Users\corph\.kiln\reach\e30189ea29182644fe5a4b2e06c7dac2\kiln-offline-fix-20261110-8a5ae447ef`.
Thus control is alive while the Reach handshake fails. The source/cache directory
exists; the external Cargo target/cache location is still unknown. No further
Windows Reach retries or probes were made.

Exact safe receipts: [Windows control/facts/refusal](windows-reach-20261111T084747Z.json)
and [earlier fleet/B24/Windows journal](operations-diag-20261111T084007Z.json).
The Linux request's unparsed stdout was not retained; the journal is the exact
refusal evidence. No token, private key, process environment or transcript was read/output.

## Source explanation to verify on the actual Windows host

Pinned f555 source has these connected seams:

- `reach/node.rs:725` synchronously persists the new placement epoch before
  creating the node handle. Failure returns an error.
- `reach/place.rs:548–549` implements that new-file sync using
  `File::open(new_path)?.sync_all()`. `File::open` is read-only. Rust's Windows
  `File::fsync` directly invokes `FlushFileBuffers` (local Rust1.94.1/nightly
  standard-library source agrees).
- `agent.rs:1794` logs `reach is off on this runner` on startup error and retains
  no Reach handle. `agent.rs:4751` forwards Reach frames only when that handle exists.
- `reach/node.rs:47,859–862,3166` ties the exact15s failure to incomplete TLS
  handshake, before a remote command or resource admission is reached.

Microsoft requires a handle opened with GENERIC_WRITE for
[FlushFileBuffers](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers).
Rust documents [File::open](https://doc.rust-lang.org/std/fs/struct.File.html#method.open)
as read-only. This is a concrete source/API mismatch. Whether it caused this
runner's startup error is still **unverified** until its own log or native fixture
is observed. The expressly approved contract correction opens the existing new
epoch file with `OpenOptions::new().write(true)` before `sync_all()`. Preserve the
flush, rename and final sync; skipping durability would break epoch fencing.

A second source seam can explain false Reach availability after node startup fails:
`reach/relay.rs:357–366` closes links on connection loss but retains machine-keyed
announcements; only `forget` removes them. `hub.rs:2679` presents Reach availability
as current control connected plus the retained announcement's accept bit. Old
32f Announce lacked the f555 feature list; f555 node startup announces the complete
list without a Windows-specific branch. Retaining an older announcement is
consistent with the live discrepancy, but the precise historical announcement
owner is not captured. Any correction must bind the advertisement to the existing
authenticated connection owner, invalidating only that owner's advertisement;
an old disconnect must not clear a newer owner's valid advertisement. No new cache
or authentication bypass is proposed. A server-side availability correction alone
cannot start a node whose local epoch sync failed.

## Reviewable diagnostic; root executes and cleans up

[windows-reach-diagnostic.py](windows-reach-diagnostic.py) emits the exact existing
`POST /api/terminal/sessions` request, targeting this runner. It never sends it.
[windows-reach-diagnostic.ps1](windows-reach-diagnostic.ps1) is the readable payload.
Root owns creation, bounded output readback and deletion of the newly owned terminal;
no existing terminal/service/task/process is touched. This is metadata diagnosis,
not a route for bypassing normal build headroom.

Default: inspect only ScheduledTask `kiln-agent` XML in memory. Resolve its explicit
state-dir, or use the documented default only after matching its current principal.
Read only that state's `kiln.log`: at most32KiB from its tail, at most12 matching
Reach-start/epoch lines of800 characters, rejecting credential-like lines. Task
arguments and XML are never printed. No matching line means **not observed in
this bounded tail**, not a passing startup. The script has a cooperative30s deadline;
root must enforce the terminal's30s protocol deadline if an OS call blocks.

```sh
python3 docs/research/measurements/queue-prevention/windows-reach-diagnostic.py
# Optional, separately explicit fixture request for root review/execution:
python3 docs/research/measurements/queue-prevention/windows-reach-diagnostic.py --fixture
```

The opt-in fixture creates one new GUID-private TEMP directory and exactly20 ASCII
bytes `0123456789abcdefghij` in `epoch.tmp`, never the actual placement epoch.
It records read-only and writable FileStream `Flush(true)` results **and direct
Win32 FlushFileBuffers results on those handles**. The small P/Invoke declaration
is loaded only for this explicit fixture. This distinction matters because the
[primary .NET implementation](https://github.com/dotnet/runtime/blob/main/src/libraries/System.Private.CoreLib/src/System/IO/Strategies/OSFileStreamStrategy.cs#L124-L130)
can skip native flushing when the stream cannot write. Managed read-only success
alone is not a Rust sync_all pass. Both handles close before cleanup. The script
deletes only the exact newly created file if all20 bytes still match, then its
empty private directory; changed content is preserved with a typed cleanup failure.
An early `fixture-owned-path` JSON line records the exact path and expected20B
before any flush, so root can inspect/delete only that exact unchanged file and
empty private directory if the terminal deadline prevents `finally` cleanup.
No actual Windows execution or PowerShell parse result is claimed here.

## Minimum validation and operational route

Coordinator validation still owed: retain an actual startup error/native error
and cleanup result if obtained, and run the new real-filesystem `EpochFs` regression
which advances twice and reads the final durable epoch on native Windows. Run
the old read-only implementation as the red mutation. Existing crash-order tests
remain necessary but do not exercise Windows handle rights. Then verify a new
node startup/complete feature advertisement and one normally admitted metadata run
with all TLS/custody checks intact. Advertisement ownership needs its own exact
old-owner/new-owner/disconnect regression before a lifecycle change.

No local initiator repair has been demonstrated; both initiators failed. A corrected
Windows binary can be built through existing admitted native CI/control if Reach
cannot be bootstrapped, with the usual resource/headroom and active-session gates.
No heavy terminal build, service kill, custom credential, installer bypass or
proof relaxation is authorized by this diagnostic. Existing Windows agents and
their accepted communication remain untouched.

R01–R16 scope: one expressly approved source portability correction, with no
performance, tested portability or readiness claim; source/API research and
the bounded Reach-timeout red are retained. Diagnostic Python parsing/request
round-trip plus Rust parse/diff checks are the only local light checks. Actual Windows
log/fixture, Rust regression/green/mutation, builds, audits and deployment remain
coordinator work. Mac free-space floor and current instance/job ownership still apply.

## Root terminal outcome and finite portability change

Root's execution93708 used the earlier script SHA
`ab93bb56d314ab4a141ea9cabd103ff71087935dc92a8c3e35614cc7fd685bd5`,
owned terminal `01a1c9d497f170dbb952d33dc03c0ded`. Its collector timed out after50s:
**no Windows output, startup error, managed flush or native flush result was received**.
Root reports owned_terminal_closed=true and an empty after-inventory. No native red
or successful fixture cleanup is claimed. If the fixture ran without returning a
receipt, its potential private20B file is preserved; no unknown-file search/deletion
was performed. The later pre-flush-receipt script is not the executed version.

Root then explicitly authorized the narrow Win32 contract portability correction,
without claiming the live cause: `EpochFs::sync_new` now opens the existing new
epoch file with write access. Write→sync→rename→final sync ordering is unchanged.
One portable real-tempfile test calls next_epoch twice, checks1 then2, persisted
`place-epoch`2 and absent `.new`. Existing SimDisk crash/mutation coverage remains.
Owned Rust source SHA256:
`68be8d515b068c022d0d23e187e7157fecae57ae70c8a61e7f6b7785b780a311`.
Rustfmt parse-only0 and scoped diff-check0; no tests/builds were run by this lane.
Rustfmt format-check1 reflected the existing file's long-line/import style; nothing
was reformatted. Native Windows red/green and actual startup causation remain open.

## Existing native CI/control recovery route (proposal; never enqueued)

There is a separate existing source path: `ci/dispatch.rs::assign` commits the
live grant and signed handed record, then calls Hub.command_start with AssignV2.
The authenticated server control connection delivers it to `agent.rs`'s AssignV2
branch, which verifies epoch/lease, checks normal slots/resource holds/draining/
upgrade state and starts `runner::runtime::Leg`. It does not establish a Reach
link. This is source evidence only: no Windows CI leg has been attempted here.

Required limits before root uses it: CI enabled; a current Windows capacity/sample;
CI scheduler resource mode and headroom enabled; ordinary worker slot/lease and
physical free-space checks retained; explicit CPU/memory/disk holds and Cargo jobs
matching admitted CPUs. Do not use a pooled executor to skip host admission.
`agent::leg_room(reach=None)` currently returnsOk, so the unified Reach-local
admission ledger is **not available** in that state. The CI scheduler's normal
sample-based headroom and agent slot/resource checks remain, but this route must
not be represented as equivalent to the missing local ledger. If root cannot
establish normal headroom authority from current CI state, hold the build.

Use one reviewed native Windows workflow at the authorized **signed prevention
SHA**, preserving source/recipe identity. The existing release.yaml `windows` job
is a Linux cross-build and is not this route. Resources below are required inputs
with no guessed defaults: this lane retained R38's63m05s/artifact proof but not its
resource envelope. Root must use its actual R38/Windows budget evidence. The
90-minute deadline follows the existing release job; it grants no extra resources.

Reviewable workflow body for `.kiln/windows-recovery.yaml` at that signed SHA:

```yaml
name: native-windows-recovery
on:
  manual:
    inputs:
      build_cpus: { type: string, required: true }
      build_memory: { type: string, required: true }
      build_disk: { type: string, required: true }
      cargo_target_dir: { type: string, required: true }
jobs:
  native:
    # The exact machine owns the previously completed source/cache being preserved.
    runs-on: [f766f09294f2e17e3b8a631ffd4e6a4b]
    requires: ["os == windows", "arch == x86_64", "tools has cargo", "tools has node", "tools has git"]
    resources:
      cpu: "${{ inputs.build_cpus }}"
      memory: "${{ inputs.build_memory }}"
      disk: "${{ inputs.build_disk }}"
    timeout-minutes: 90
    env:
      CARGO_BUILD_JOBS: "${{ inputs.build_cpus }}"
      CARGO_TARGET_DIR: "${{ inputs.cargo_target_dir }}"
    steps:
      - name: preserve completed source, verify signed source, build native artifact
        shell: powershell
        run: |
          $ErrorActionPreference = 'Stop'
          function Checked-Native([string]$exe, [string[]]$arguments) {
            & $exe @arguments
            if ($LASTEXITCODE -ne 0) { throw "$exe failed: $LASTEXITCODE" }
          }
          $sha = $env:KILN_SHA
          if ($sha -notmatch '^[0-9a-f]{40}$') { throw 'missing exact source SHA' }
          $src = 'C:\Users\corph\.kiln\reach\e30189ea29182644fe5a4b2e06c7dac2\kiln-offline-fix-20261110-8a5ae447ef'
          if (-not [IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) { throw 'unverified absolute target path required' }
          $artifacts = Join-Path $env:GITHUB_WORKSPACE 'recovery-artifacts'
          [void][IO.Directory]::CreateDirectory($artifacts)
          Set-Location $src
          $old = & git rev-parse HEAD
          if ($LASTEXITCODE -ne 0 -or $old -ne 'f555736bd887a4196c8841564cb9b4d6c1ede95f') { throw 'completed source HEAD changed' }
          $dirty = & git --no-optional-locks status --porcelain --untracked-files=all
          if ($LASTEXITCODE -ne 0 -or $dirty) { throw 'preserve changed source before checkout' }
          $oldArtifact = Join-Path $src 'dist\kiln-x86_64-pc-windows-msvc.exe'
          if (Test-Path -LiteralPath $oldArtifact) {
            $oldHash = (Get-FileHash -LiteralPath $oldArtifact -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($oldHash -ne '14ad7179f912bf132fefd738d159ace4b3ae8bea1283bc9c2b80a613723f9c2d') { throw 'old artifact differs; preserve separately' }
          }
          Checked-Native git @('fetch', 'https://github.com/hyper-light/kiln.git', $sha)
          Checked-Native git @('verify-commit', $sha)
          Checked-Native git @('checkout', '--detach', $sha)
          $actual = & git rev-parse HEAD
          if ($LASTEXITCODE -ne 0 -or $actual -ne $sha) { throw 'source SHA mismatch' }
          $env:KILN_COMMIT = $sha
          Checked-Native npm @('ci', '--prefix', 'ui', '--no-audit', '--no-fund')
          $env:KILN_UI_SERVER = 'off'
          Checked-Native npm @('run', '--prefix', 'ui', 'build')
          Remove-Item Env:KILN_UI_SERVER
          Checked-Native node @('ui/scripts/pack-ui-server.mjs', '--require', '--target', 'x86_64-pc-windows-msvc')
          Checked-Native cargo @('build', '--release', '--locked', '-p', 'kiln', '--target', 'x86_64-pc-windows-msvc', '-j', $env:CARGO_BUILD_JOBS)
          $binary = Join-Path $env:CARGO_TARGET_DIR 'x86_64-pc-windows-msvc\release\kiln.exe'
          Checked-Native $binary @('--version')
          $hash = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash.ToLowerInvariant()
          Copy-Item -LiteralPath $binary -Destination (Join-Path $artifacts 'kiln-x86_64-pc-windows-msvc.exe')
          [ordered]@{ source_sha=$sha; old_source_sha=$old; target='x86_64-pc-windows-msvc'; sha256=$hash; bytes=(Get-Item -LiteralPath $binary).Length } |
            ConvertTo-Json -Compress | Set-Content -LiteralPath (Join-Path $artifacts 'build.json') -Encoding UTF8
    artifacts: ["recovery-artifacts/*"]
```

This is an unparsed/unrun workflow proposal, not an installed file. The exact old
dist filename still needs the normally admitted preflight to confirm it. Preserve
old artifacts/receipts before checkout; this recipe writes new output into the CI
workspace, leaving old dist output in place. Do not run a second time blindly if
the first attempt already checked out the new SHA: reconcile its exact job/source
state. Native tools, MSVC readiness, actual target directory and scratch remain
unverified. The metadata preflight must inspect only known old-source/Cargo config
and the prior target's metadata, not walk arbitrary caches. If the prior target
cannot be recovered, an explicitly owned fresh target is possible only with a
new normally admitted scratch budget; it is not a claimed cache hit.

Existing API sequence (all mutations remain root-owned and **not executed**):

1. Validate/preview the reviewed manifest with existing CI validate/plan-preview;
   verify current Windows headroom/slot/capability state. Upload the signed commit
   if absent using `GET /api/ci/repos/hyper-light%2Fkiln/heads` then
   `POST .../packs?sha=<signed-SHA>` with a normal git pack. The existing
   `kiln workflow run ... --server URL --repo hyper-light/kiln --report FILE`
   packages/follows through these routes; run it only in a content-clean checkout
   of the signed SHA so it does not create a different working-tree commit.
2. Explicit existing start request, after filling measured resource inputs:

   ```json
   {"sha":"<signed-SHA>","event":"workflow_dispatch","workflows":[".kiln/windows-recovery.yaml"],"job":"native","inputs":{"build_cpus":"2","build_memory":"32Gi","build_disk":"16Gi","cargo_target_dir":"C:\\Users\\corph\\.cache\\kiln-offline-release-20261111\\windows-native-target"}}
   ```

   Send once to `POST /api/ci/repos/hyper-light%2Fkiln/local-runs`; retain returned
   run/leg IDs. Queued/held is not running. A rejection or missing current authority
   means hold, not route around it. Follow actual run events and artifact receipts.
3. Download through the existing run artifacts API; verify exact bytes/hash,
   native smoke and signed source/recipe/runner/attempt/grant receipts. Stage with
   the existing accurate binary/provenance publisher, not a fabricated source stamp.
4. Only once root has promoted the server/fleet target to that signed SHA and
   the matching Windows artifact is available, the existing targeted update is
   `POST /api/updates/runners/f766f09294f2e17e3b8a631ffd4e6a4b`
   with `{"way":"download"}`. While the server target remains f555, the endpoint
   correctly refuses an already-current f555 runner; do not bypass that fence.
   Root first lists current sessions/terminals and controls upgrade timing.
   Verify new source/build plus fresh Reach readiness/features and one normally
   admitted metadata command afterward; do not weaken custody to make it pass.

No workflow, pack, run, build, artifact stage or upgrade was created by this lane.

## Exact prebuild metadata at 09:12–09:14 UTC; build route held

[Safe read-only receipt](windows-prebuild-20261111T091401Z.json). Three normal
1CPU/1GiB/1GiB primary Reach metadata requests performed loopback API reads; the
only remote Windows operations were bounded DirList on the two named cache
parents and three exact resulting target paths. No filesystem walk, environment,
PKI/credential search, terminal, probe, config update or build occurred.

The actual target root is now verified by layout:
`C:\Users\corph\.cache\kiln-offline-release-20261111\windows-native-target`.
It contains `release` and `x86_64-pc-windows-msvc`; both release directories contain
`deps`, `build`, `examples`, `incremental`. Native-target deps modification time is
1794375047. The other exact parent `kiln-offline-state-fix-20261110` does not exist.
These are warm Cargo target directories, not a verified current artifact hash or
compiler fingerprint; no cache contents were removed or changed. This updates
BUILD-HANDOFF's earlier unknown Windows target path.

Current backend/config facts are decisive:

| Field | Actual observed value |
| --- | --- |
| CI2 enabled / kiln_workflows | false / false |
| CI scheduler mode | slots |
| Scheduler headroom | true |
| Overcommit CPU / memory / disk | 1.5 / 1.0 / 1.0 |
| Load admission / target_use override | null / null |
| CI routing explain | HTTP404 because CI2 is off |
| Windows headroom support / applied / enabled | true / true / true |
| Windows pacing / status timestamp | threads / 1794388325 (09:12:05UTC) |
| Windows slots / busy / running CI IDs | 8 / 0 / [] at 09:12:05UTC |
| Resource facts timestamp | 1794388333 (09:12:13UTC) |
| Physical threads / free memory / free disk | 32 / 113,576,214,528B / 161,837,694,976B |
| Native tool facts | cargo1.99.0, node24.21.0, git2.56.0; host executor |
| Host CPU/load sample timestamp | 1794380406476 (07:00:06.476UTC), stale |

The fresh facts establish reported memory/disk, not current idle CPUs. Fresh
headroom status is not proof that the CI scheduler received a new HostSample.
No current idle-CPU authority is inferred from an old sample. The exact recipe's
2CPU/32GiB/16GiB request is root's expressly approved **operational conservative
ceiling**, informed by the same crate's two rustc processes/Mac footprint; it is
not a measured Windows peak or a learned production constant. Root can choose8GiB
scratch only with corresponding evidence. Actual normal admission may refuse or
hold it; this record grants no capacity.

The exact CI2 local-runs body above is therefore **conditional and cannot start
on the current disabled backend**. It needs the reviewed workflow in the signed
SHA, root's separately authorized CI configuration/admission state, and normal
current resource authority. No setting was enabled or changed by this lane.

There is also a currently implemented legacy backend, independent of CI2 and Reach:
`POST /api/repos/hyper-light%2Fkiln/trigger`
`{"branch":"<signed-SHA>","pipeline":"native-windows-recovery"}`. `gitops::manual`
accepts an exact40-hex commit; the legacy Hub scheduler sends ServerMsg::Assign
over authenticated control to the normal worker. It checks the runner's labels,
requirements, build capabilities, slots, declared CPU/memory shares and the normal
headroom rule. It does **not** solve the remaining authority gap: legacy
`pipeline::Resources` permits CPU/memory only and rejects a disk field, while
`Hub::known_headroom` explicitly uses the last sample however old. A disk.free
requirement is a facts check, not a disk reservation. Do not silently remove the
disk hold or treat the stale sample as current merely to use this route. It is
documented as an existing seam, not authorized as an equivalent recovery build.

Hold the heavy Windows build until root can establish the ordinary required
authority/configuration, using actual connection/sample/worker events rather
than synthetic probes or repeated attempts. The exact source/cache/build/API
recipe remains reviewable for that point. No ready-to-run or successful Windows
admission/build claim is made.

## Root-selected existing legacy manual route

Root expressly selected the existing legacy CI route for this manual recovery,
keeping all its currently implemented CPU/memory/headroom checks unchanged. It
does not need CI2, kiln_workflows, resource mode or any repository setting enabled.
The route's documented limitations remain; no Reach-local parity or disk
reservation is claimed. This supersedes the earlier suggestion to require CI2.

Before preparing the recipe, the live read-only API at 09:23:16UTC verified:
repository ID `01a13865558f700ea2b5bc931a4637f3`, name `hyper-light/kiln`,
enabled=true, upstream `github.com/hyper-light/kiln.git`, branches=[], last_error=null.
Its pinned f555 workflow listing includes actual `.kiln/ci.yaml`, `release.yaml`,
`bench.yaml`, `e2e.yaml`, `image.yaml`, `sim.yaml`, `stress.yaml`, `custody-lab.yaml`,
`h3-fuzz.yaml`, `wire-lab.yaml`. The existing Windows release job is Linux-cross;
the new recovery job must be its own explicit manual-only pipeline.

Review artifacts, **not installed or enqueued**:

- [windows-recovery.legacy.yaml](windows-recovery.legacy.yaml): complete copy for
  root to review and place at `.kiln/windows-recovery.yaml` in the eventual signed
  prevention commit. `on.manual=true`, no automatic trigger, one job only, exact
  owning Windows runner, CPU2/memory32GiB, Cargo-j2 and the existing90-minute release
  deadline. No retry, cancellation, pool, new executor or source/runner setting.
- [windows-recovery-trigger.json](windows-recovery-trigger.json): exact existing
  request shape. Root replaces the deliberately invalid SHA placeholder only after
  final tests0 and canonical signed push, then sends once to:
  `POST /api/repos/01a13865558f700ea2b5bc931a4637f3/trigger`.
  `branch` accepts a full40-hex commit; `pipeline` matches the exact file path.
  The response is the created run array; record run/job IDs and verify its assigned
  source SHA. An empty array is not a running build. No jobs selector or recipe
  upload is accepted by this legacy endpoint: the single job is in the signed file.

This follows the actual `api::TriggerReq`/`gitops::manual` contract. The legacy
parser's `Triggers.manual` is a boolean, and `JobResources` permits only CPU/memory;
the review copy uses exactly that schema. `KILN_WORKSPACE`, `KILN_SHA`, runner,
run/job/attempt come from legacy `exec.rs`, not GitHub-only environment names.
Native worker checkout must be Git-based, with HEAD exactly the assigned signed
SHA. Archive fallback without `.git` is rejected before the owned cache changes.
The script fetches objects **locally from that CI workspace**, verifies the commit,
and non-force checks it out in the content-clean exact old Reach clone. It accepts
only its expected f555 HEAD or the same target SHA; no reset/clean/force/discard,
GitHub credential assumption, key read or custom trust bypass.

The source `dist/kiln-x86_64-pc-windows-msvc.exe` is checked against old414,221,824B
artifact hash14ad7179f912… and retained before Cargo can overwrite the cached
binary. If absent, only an exactly matching known cached binary can be copied to
that dist path; otherwise stop for review. Existing dist output is never replaced
with the new build. Old artifact+before receipt are additionally copied into this
job's artifact directory. Failed/missing source/signature/old artifact refuses
the build instead of guessing or discarding data.

One direct DriveInfo observation checks at least16GiB available on each distinct
source/target/workspace drive before mutation/build. It is an operational preflight,
not a hold or a promise that other writers cannot consume space afterward. Legacy
headroom/slots/resource accounting remains the normal authority and can hold or
refuse the job. The old CPU sample remains dated; it is not re-labelled fresh.
The normal build then makes the UI afresh, requires the native Windows Bun pack,
builds target `x86_64-pc-windows-msvc` with exact KILN_COMMIT and cargo-j2, runs the
binary's native --version, hashes it and writes source/runner/run/job/attempt proof.

Actual legacy artifacts contract: `exec.rs::upload_artifacts` resolves the job's
`recovery-artifacts/*` relative to KILN_WORKSPACE, builds a tar and uploads with
the job's existing authenticated artifact path. Piece uploads name offsets and
finish with size/SHA256; the server promotes the complete verified tar. Root reads
`GET /api/jobs/<exact-job-ID>/artifacts`, an application/x-tar response, extracts
the new named binary/proof and verifies bytes/hash before the existing accurate
binary publisher. No temporary listener or alternate transfer protocol is needed.
The old artifact is included under `previous-...`; never publish that as the new one.

On 09:27:56UTC the **actual existing** `POST /api/ci/validate` returned HTTP200,
kind=workflow, dialect=kiln, diagnostics=[] for this review YAML. This is a pure
front-end validation with no run/configuration mutation. Direct legacy parser
execution and native PowerShell parsing/execution remain unrun; root owns final
validation and execution. The recipe itself has never been put under `.kiln/`,
packed, pushed or triggered by this lane.

Root subsequently reported the separate native Win32 comparison succeeded:
read-only FlushFileBuffers returned false/error5, writable returned true/error0,
exact20B cleanup=true, owned terminal closed/after=[]. Root retains its receipt.
This reproduces the handle-rights contract defect on Windows; the runner's actual
Reach startup cause still awaits its own log. No native Rust EpochFs test result
is inferred from the P/Invoke fixture.
