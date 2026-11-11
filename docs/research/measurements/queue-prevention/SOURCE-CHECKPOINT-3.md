# Private checkpoint 3: preserve owner capacity and the handover epoch

This checkpoint is build input, not deployment/readiness approval. Parent is
`1d5c3821ad1f2e7b6b5cccff0543d89d953dedb7`. Full gate remains skipped at the user's
explicit request; relevant focused checks, actual native authorization and CMS
verification remain required before promotion.

Coordinator POLICY_READ: Shards origin/main
`0d261aa931ae2e33e1f4a528b33f7e26b5ee7c90`, rules SHA256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Root owns integration/builds/live/audits; scoped implementers read the same policy.
R01–R16 baseline accounting and unverified portability/performance scope remain in
SOURCE-CHECKPOINT-2.md and the linked admission/coalescing/producer records.

## Reproduced defects and correction

The grouped coordinator run on frozen source capture
`check-1794394450452539000` passed 47/48. The exact next-goal original remained in
SQLite as revision 6, generation 0, Waiting while its receiver was Idle with no
admitted debt or input bytes. The receiver controller had generation 32 and
eligible sender `server`; the registry still projected generation 30 with no
eligible sender. The private test snapshot is in checkpoint3-core.log.

The deterministic real Host/Registry/SQLite/native regression
`harness::ops::end_to_end::mail_peers_capacity_survives_an_ordinary_owner_state`
was then compiled at unchanged capture `check-1794394901327881000` before the
production correction. It failed with `MailPeers did not update the authoritative
State` (mail-peers-ordering-red.log/json). A real sender grant followed by ordinary
rename State reproduces the split owner. No consumption/credit is fabricated.

Host had directly published capacity after changing its controller, leaving Core
State stale; an ordinary State could revoke the projection and leave waiting work
stuck with no new event. The finite correction routes Host's capacity-bearing
mail events through existing Session/Core output and the existing actor wake.
State snapshots use the current controller capacity; the typed receipt event
retains its exact token and boundary. No extra queue, timer, polling manager,
Info-generation merge or authority increase is introduced. The test subsequently
requires the original receipt revision to reach native input once and become
Consumed. The temporary large diagnostic wrapper is preserved privately and
removed from source; the original busy fixture's assertions/deadline are retained.

The actual isolated old0ff→1d5 native upgrade retained the same provider process,
original body and mail token but requested an extra identity at epoch 2 although
custody committed 0→1. The stronger live observer reproduced this failure in
26.92 seconds, with owned cleanup successful:
`~/.cache/kiln-queue-prevention-release-20261111/native-actor-red-1d5c382`.
The corrected public aggregate native-actor-red-observer-correction.json binds the
private raw log hash and records requested/refused epoch 2. References to the
valid epoch-1 hand-on inside that error are not counted as an epoch-1 refusal.

The native journal branch now structurally binds the untrusted hand-on to its
retained source timeline and exact destination epoch, registers the existing
custody receipt before Open, and avoids the legacy second increment. The existing
server remains the sole signature/root/complete-revocation authority. Missing
signed native state stays held; no empty history is invented. Receipt construction
failure propagates before activation. The legacy no-journal branch is unchanged.
Its focused epoch regression passed in the first checkpoint-3 run; a wire guard
then caught a local decode, now routed through the existing typed wire reader.

## Validation and operational limits

At source capture 1794394450452539000, compile and library clippy passed; focused
545 tests passed and one wire-reader guard failed. That guard was corrected before
the next compile, without waiving the guard. The subsequent source changes require
fresh positive checks; none of the earlier results is labeled their success.

The live harness now also requires an exact verified destination epoch-1 public
certificate and an authorized agents.result command executed as the surviving
provider's child, with both terminal and agent environment identity marks removed.
It rejects any unexpected requested epoch and retains all prior mail/PID witnesses.
Actual positive execution against the new release binary is still pending at
this private source checkpoint. It is fake-provider protocol/authentication proof,
not a model-understanding, every-provider or performance claim.

Root verified direct primary→artifact-source Reach routing with the small exact
1d5 Linux build receipt. The current primary service binary supports admitted run
flags; the older ~/.local/bin CLI does not. All artifact transfers/builds still
use normal admission, exact hashes and measured disk floors.

Canonical main remains f555 and prevention is not deployed at checkpoint creation.
Windows can build/stage through its validated manual recipe after source push and
CMS receipt delivery. Its old updater restarts four active agents, so promotion is
held until real quiescence/continuity is established. B24 still has the prior
verified NoRoute condition; preserve its old live Claude. Offline runners remain
unverified. No queue communications are cleared or silently dropped here.
