# Legacy input ownership during a native upgrade

This is an implementation checkpoint, not a deployment or passing-test claim.
Parent: `5b7814c09bf9eb538b371fc312fa18ec4de8f34f`.

Coordinator POLICY_READ: Shards origin/main
`0d261aa931ae2e33e1f4a528b33f7e26b5ee7c90`, rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Root owns integration, live validation, builds, signing and rollout. Existing scoped
implementers own source, receipt and private fixture lanes; their current policy
readbacks are retained in ROOT.md. Full gate is skipped at the user's request.

## Actual failure

Normal Reach run69186 upgraded an isolated, actual f555 supervisor and runner into
ef473cf while retaining the same gated native provider. The server issued the
destination epoch1 certificate. The real source journal still contained the
original tokenless LegacyOpaque ProviderFlight lease at epoch0. Its native UUID
and byte charge remained, but the public epoch1 capacity could not represent that
old-epoch debt. The proof timed out after180.509seconds and cleaned up only its
owned processes. No production session participated.

The private result, logs and database remain under
`~/.cache/kiln-queue-prevention-release-20261111/legacy-native-reach-1-ef473cf`.
The typed pending row was read after the owned writer exited. Its original ID,
epoch0, provider_flight stage, token=None and disposition=None are the red witness;
an inaccessible active database was never counted as an empty journal.

The prior modern journal upgrade proof passed18 checks. It does not cover this
older source, which has neither modern transfer tokens nor a transfer manifest.
The production rollout is held rather than treating those different protocols as
equivalent.

## Correction at the existing owners

The existing Inbox Head retains the exact compact source proof and explicit
AwaitingPromotion, AwaitingCertificate, Rebinding and Active phases. A small native
startup marker fences ordinary restore; it never authorizes ownership. The actual
promotion event and an installed certificate for the actual held destination key,
session, epoch, organization, person and current runner are separate boundaries.
Server validation remains the sole chain, trust-root and complete-revocation
authority. A raw Identity frame or public certificate filename cannot release debt.

One existing Records transaction rebinds one exact legacy pending row and its Head
cursor together, conditioned on both revisions. This is a bounded transaction
shape, not a chosen queue depth. Token-bearing or nonlegacy rows require their
existing verified transfer protocol. IDs, command bodies, revisions, digests,
accepted times, stages, byte charges and dispositions are preserved. A legacy
absent accepted_seq is materialized as its original row revision, preserving the
existing effective receipt order when the row itself gains a migration revision.
An existing explicit sequence is unchanged. A resource or
storage failure retains the fence, original rows and debt for an existing-event
retry. It does not delete originals or create retry credit.

The existing Session actor serializes provider completion with migration. It
flushes recorded input effects, crosses the existing Host lane barrier, migrates
rows, advances Controller and State together, saves the owner and activates the
Head before removing the startup fence. It retires terminal promotion metadata
before opening the existing Controller gate, with no await between the retirement
acknowledgment and activation. Provider output arriving meanwhile waits
on that same actor. Ordinary receipt replay cannot independently restore or reopen
a Rebinding owner. Metadata work runs through the existing harness operations;
the runner's network loop continues delivering events. There is no polling timer,
probe, separate spool or second admission controller.

The existing Records change wait carries only its family cursor and last observed
own-Head revision while this controller is Promoting. It reads the own Head before
consuming that notification cursor, so cancelling the read cannot lose the wake.
Known commits advance the cursor; unrelated Head events and failed reads cannot
self-authorize retry credit. An unknown retirement outcome reconciles the exact
stored epoch and live controller on a later real owner event.

An interruption can resume from the exact durable phase and cursor. A crash before
an authentic promotion event became durable remains held; missing TakeOver state
is not a substitute for that event. Descriptor-loss recovery must visibly retain
uncertainty and require manual resume instead of silently replaying native work.
Expired source-proof recovery and unavailable platform continuity remain explicit
limits until actually demonstrated.

The narrow restart interval after the startup marker is removed but before
terminal metadata retirement is not separately fault-tested here. Active already
means certified epoch1, migrated rows and durable Saved1; a subsequent ordinary
restart is same-epoch recovery, not another0→1 authorization. Native process
continuity through a crash and a stronger requirement to re-certify before every
same-epoch provider restart are not claimed by the controlled upgrade proof.

## Windows and validation status

Native Windows run2377 failed compilation at Session::input_changed because its
OwnedChanged variant was declared only for Unix/tests. Mail state publication
needs the same actor wake on every platform; Unix-only owned-Reach implementation
remains scoped to Unix. The manual recovery recipe's preserved source guard now
names the actual ef473cf checkout left by that failed build. Its original14ad
artifact hash requirement is unchanged.

The native read-only check observed exact ef HEAD, the original14ad binary
(414221824bytes), and unchanged00b2 preservation stash, backup and patch hashes.
It also found exactly five missing task-owned proof PNGs, making that first
preflight fail. A separate native preview observed unset core.longpaths, actual
path lengths277/263/273/269/265, missing normal and extended paths, and pinned HEAD
blob identities and sizes. The [official Git for Windows FAQ](https://github.com/git-for-windows/git-for-windows.github.io/blob/main/content/faq.md)
documents its long-path setting. These observations support a scoped repair;
they do not establish what originally deleted the files.

Root reviewed and executed the exact-five repair request
`7c0de1fcd4ddc105037ff79e215eb55d2ae3bd39b7f0727f2650ed3d13d4d6aa`.
Native exit0: only this checkout's local core.longpaths was enabled, the five
named files were restored from HEAD, each Git blob matched, source was clean,
ef HEAD and the original stash were unchanged. No reset, stash, global config,
service or agent restart occurred. All owned temporary terminals closed, with
the final inventory empty. The corrected recovery recipe separately passed the
native PowerShell parser and four-argument binding check (workflow SHA256
`d34aa8f26f6a7b4bbf23179acfbbe172db58714ff7ac6fa83668039edf69c9fa`).
The repair has actual execution evidence; a separate repair-parser-only pass is
not claimed. Private control traces remain in the coordinator's
`kiln-windows-control-20261111` Reach scope.

At this checkpoint the new focused, restart, authority-negative, clippy, release
and actual native upgrade checks are pending. They must bind an unchanged source
capture or exact signed release SHA. No new performance ratio, whole-process
allocation bound, seven-target result or all-machines deployment is claimed.
The previous R01–R16 evidence accounting remains linked from SOURCE-CHECKPOINT-3.md;
this document adds the compatibility failure and its required correction.
