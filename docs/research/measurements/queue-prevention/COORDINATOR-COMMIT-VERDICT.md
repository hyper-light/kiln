# Coordinator commit verification before the manual Windows build

Current procedure; supersedes the unexecuted `git verify-commit` lines in older
WINDOWS-REACH/BUILD-HANDOFF preparation. Policy06ae067c0beda71c98a8c6f9633df9feb71fc11b,
rules SHA256df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3,
implementer kiln_status_trace. Full current policy/evidence/CLAUDE read; Root and
Kiln instructions retained. Root owns execution, signing, push and deployment.
No new Rust, crypto implementation, signing override, CI setting or actor is introduced.

`gitsign.rs:53–57` refuses Git's `--verify` by design. Kiln's actual CMS check is
`ci/pushsig.rs::checked`→`kiln_ci::identity::gitsig::verify`, against its root,
complete revocations and current check time. The shipped `kiln __git-hook update`
uses that code without starting a daemon/runtime. `ci.verdict` is CI test coverage,
not a signature verdict. `GET /api/verify?commit=repo@SHA` requires a native-hosted
repository; this actual canonical repository is upstream and does not qualify.

## Root-run verification

[verify-coordinator-commit.py](verify-coordinator-commit.py) is an unexecuted
coordinator preflight for the primary Linux host. It uses the known credential()
helper and exactly `/home/adalundhe/.config/kiln/server.env`; no credentials or
private keys are printed/copied. Existing loopback APIs `/ca.pem` and
`/api/identities/revocations` supply public root and complete revocations. A
**previously trusted DER-root SHA256 pin is mandatory**: do not derive the expected
pin from the same fresh response. This is not a Reach leaf fingerprint or TOFU.
The existing shipped Kiln executable also requires its previously verified hash.

The existing enrolled Mac runner's public trust anchor was read from exactly
`/Users/adalundhe/.local/share/kiln/agent/ca.pem` (one607B public certificate,
mtime2026-10-21T05:57:47.236291Z). Its DER-SHA256 is
`c815d046a45d7f59ec224962255ab6b5872524acf915482dc392fd5f99fe4749`;
OpenSSL's public-subject view confirms O=hyper-light,CN=kiln CA. This is the prior
enrollment file used by agent.rs's state-dir CA loading seam, not a hash accepted
from the current server response. No private key was read or certificate bytes
printed. The verifier still must reject if the primary's current public root
differs; no fresh-response substitution is authorized. Expected org is hyper-light.

The local source repository must contain the exact signed commit object. Root can
use its normal authenticated Git bundle import; after canonical push, separately
retain the normal upstream ref check. The helper checks Git's object hash before
the signature and never changes that source's index, objects or refs. Its new
private output directory contains a scratch bare Git view using existing objects
as alternates, with **only the target's parents referenced**. Before calling the
hook it requires `rev-list target --not --all` to name exactly that one target.
This is essential: running the hook in a clone whose refs already contain target
would inspect zero commits and could return success without checking it.

The hook uses a required agent-signature policy and the complete root/revocation
snapshot. No SSH keys are admitted in this CMS-only preflight; missing/foreign/
revoked/changed signatures fail. The helper requires a single target verdict,
`seen=by`, signature digest present, no refusal/warnings, and the actual signer URI
to name the authorized native agent and expected organization. Its exact returned
principal/epoch is recorded rather than guessed. Do not backdate the check or
ignore revocations if it refuses. This is a coordinator verification using the
shipped hook, not a claim that upstream GitHub applied Kiln's native push policy.

Example for Root to fill and execute under normal admitted metadata resources:

```sh
python3 verify-coordinator-commit.py \
  --source-repo '<verified local Git bundle import or checkout>' \
  --sha '<exact signed target SHA>' \
  --agent 01a1c9f5886e72e48d3a2554f9406e3e \
  --org hyper-light \
  --root-sha256 c815d046a45d7f59ec224962255ab6b5872524acf915482dc392fd5f99fe4749 \
  --kiln /home/adalundhe/.local/share/kiln-server/bin/kiln \
  --kiln-sha256 87107f09976eb189264a7d1e5ee7fc7435797507a819be51ff89b4234044c8ff \
  --output '<new private evidence directory that does not exist>'
```

It keeps context/revocations and hook output private (0600), and emits only a
bounded successful receipt or a refusal code/evidence path. The receipt names
repository, exact SHA, actual signer/org/method, trusted binary/root hashes,
revocation snapshot hash/count, check interval and the exact positive hook verdict.
No receipt is produced on an empty verdict or any failure. Protocol timeouts are
bounded one-shot operation deadlines; no retries, probes, service or signing action.

Root's signing receipt identifies checkpoint
`0ff33887757bc0584e7a69abfa0d2759ec7d850d`, parentf555, native agent above, with a
gpgsig present. Root subsequently executed this helper on the primary through
normal Reach and reported a **positive exact-target CMS verdict**, actual agent
correct, methodkiln.__git-hook.update, with no zero-target pass. Receipt SHA256:
`6d28df55e3484d474e06d7daa7529f5623088600bb986233d4772ed3f27d42b3`;
private output under primary
`~/.cache/kiln-queue-prevention-cms-20261111/verdict-0ff338`. This is coordinator
execution, not an implementer-run check.

Root then released creation of the actual manual-only
[.kiln/windows-recovery.yaml](../../../../.kiln/windows-recovery.yaml).
It now exists with the reviewed receipt gate. For the same-SHA workflow/compiled-source
design, Root must sign/push the resulting final checkpoint and verify **that exact
resulting SHA** before its manual trigger. The earlier0ff receipt cannot authorize
a later SHA. The workflow reads its assigned KILN_SHA and a receipt with that SHA;
it contains no0ff build-target literal. Do not silently
substitute a recipe commit for the compiled-source commit.

## Deliver the positive receipt and trigger exactly once

A receipt cannot be inside the very commit it verifies without circularity.
Legacy TriggerReq accepts only branch/pipeline, not a receipt/env input. Root's
approved existing-control route is a small task-private artifact, copied after
verification and before trigger to this exact owned-cache filename on Windows:

```
C:\Users\corph\.cache\kiln-offline-release-20261111\commit-verdict-<assigned-SHA>.json
```

Root performs the copy through normal authenticated control and verifies its exact
bytes/SHA256 against the helper's receipt; no terminal build or resource bypass.
Only that receipt is delivered, not context, certificate material or credentials.
Keep the coordinator receipt/evidence as authority. The review workflow requires
this artifact, bounds it by the existing512KiB control-frame metadata cap, reads it
once, checks exact source/repository/org/method and the positive signer verdict,
then hashes and exports those same bytes. Missing, wrong or incomplete metadata
refuses the build before cache checkout. It does not claim to independently verify
CMS or turn a plaintext artifact into a signed server statement.

After canonical push and receipt delivery, Root fills the exact SHA in
[windows-recovery-trigger.json](windows-recovery-trigger.json) and sends once to
the existing repository-ID trigger endpoint. All original legacy CPU/memory/
headroom guards remain. The16GiB one-time free-space preflight is not a physical
reservation. Final artifact acceptance must match the job's coordinator-verdict
SHA256 and source SHA against Root's original receipt, plus the native artifact
hash/size/smoke and ordinary job/runner/attempt record.

## Current validator driver and parser checkpoint

[validate-windows-recovery.py](validate-windows-recovery.py) prepares the actual
current-file API check for Root. On the primary, with the exact final file copied
or checked out there:

```sh
python3 validate-windows-recovery.py --file '<exact final checkout>/.kiln/windows-recovery.yaml'
```

It calls only existing `POST /api/ci/validate`, path`.kiln/windows-recovery.yaml`,
content from that file, dialectkiln; outputs its byte SHA256, kind/dialect,
diagnostics/outline and no-run/no-config-change witnesses. It fails unless HTTP200,
workflow/kiln and diagnostics[]; no trigger endpoint is called. The actual trigger
remains separately Root-owned:
`POST /api/repos/01a13865558f700ea2b5bc931a4637f3/trigger`, body from the trigger
template with final SHA and exact pipeline path. Legacy `gitops::manual` accepts
that40-hex SHA, and its pipeline filter matches this exact file. Job artifacts are
still relative to KILN_WORKSPACE and downloaded at `/api/jobs/<job-ID>/artifacts`.

Local YAML2.9.1 parsing passed for the actual file, including manual-only trigger,
one job, CPU2/memory32Gi, exact runner, assigned-SHA receipt path, no hardcoded0ff
or gitverify invocation, and exact review-body equivalence except the first comment.
Both Python helpers pass AST parsing; scoped diff-check passes. The previous live
API validation predates the receipt-gate correction. Current API validation,
native PowerShell, native Rust test/build and deployment remain **unrun by this
lane**. Root's positive0ff CMS result does not cover the final recipe checkpoint.
No build readiness or Windows startup-cause claim follows from this preparation.

Root subsequently ran the current actual-file pure API validator: HTTP200,
workflow SHA25646610bbd138a560d15977934a1cf3bdeb26a399e835bc5c826110d7273fba3a4,
one workflow_dispatch job/one leg, diagnostics[], no enqueue/configuration change.
This supersedes the preceding current-API-unrun status for those exact bytes;
native PowerShell/Rust/build execution is still separate.

## Root-run native parser and read-only Git preflight request

[windows-recovery-native-parser.mjs](windows-recovery-native-parser.mjs) prepares
the exact owned-terminal request, never sends it. It parses the actual YAML with
the existing YAML2.9.1 dependency, embeds only its one step as gzip/base64 and
checks the resulting process-command length against32767 characters. Root's
current request is
[windows-recovery-native-parser-request.json](windows-recovery-native-parser-request.json);
the readable payload and generation metadata have matching `.ps1`/`.meta.json`
names. Current command length20,478 characters; step6,893B, gzip2,728B;
step SHA256340dbc0cff489c55a52a9c3298358255ad599bedc89cfa6972a994467b674d76.

The native payload decompresses and checks exact length/SHA, then calls only
PowerShell Parser.ParseInput on the actual workflow text. It never dot-sources,
invokes a ScriptBlock or executes that step. A tiny separate mock with the same
Checked-Native parameter signature checks literal-array argument binding and
returns data only. Its `passed` field describes parser/binding results, not CI
admission. No build/checkout/configuration or file mutation is performed.

Root also explicitly requested one read-only preflight in that same payload:
exact known old Windows source HEAD; streamed `git status --porcelain
--untracked-files=all`, retaining at most20 lines with total count/exit code;
only core.filemode and core.autocrlf values/exit codes; exact warm target directory
existence. `--no-optional-locks` avoids optional index writes; a per-command
core.fsmonitor=false prevents starting a monitor, without changing stored config.
No configall/envall/cache walk, fetch, checkout or source mutation. A nonzero Git
exit is not a clean source. Root assesses this metadata separately before the
expensive manual job. The request has not been executed by this lane.

Root's latest independent read-only task/log diagnostic used owned terminal
01a1ca07c89870c49f6dbb0c341b45a8, closed/after=[]. It verified the default task's
exact log path `C:\Users\corph\AppData\Local\kiln\agent\kiln.log`, size12,179,142B.
The bounded last32,768B contained no matching startup-Reach line. This is bounded
absence, not a confirmed Reach startup cause. The earlier native Win32 access-rights
comparison remains the distinct positive contract reproduction.
