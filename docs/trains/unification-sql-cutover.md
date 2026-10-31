# SQLite-to-Amphora format cutover

This is an offline format boundary. Source review is not a cutover or a passing rollback drill.
The person controls deployment and service changes; no agent restarts a runner as part of this
implementation.

Before starting the new serving owner, disable **all old SQL, group/log, replica backup producers
and their automatic restart**. Keep them disabled through any interruption until the directory
fence is durable. Neither `--offline`, an absent process, a SQL/file lock, nor a missing old name
proves that those producers cannot resume. Side-by-side old runner handover is not this boundary.

Provision the original 32-byte bootstrap master and its independently obtained SHA256 at
`<data>/secrets/master.sha256` (64 lowercase hexadecimal digits, optional LF/CRLF). This sidecar
needs no new TOML table in a configuration shared with an older parser. If a configured
`[ci.secrets] trusted_master_sha256` also exists, they must agree. This preserves configuration
parse compatibility; it does not authorize the older data writer after cutover.

Run the unified local CLI with the exact directory, without contacting the running API:

```sh
kiln store cutover --data-dir /the/actual/data-directory --offline
```

The command preserves SQL/WAL/SHM and any retired cluster log by verified same-filesystem links,
removing write permissions while preserving existing read privacy. Every attempt first flushes
the full canonical data-directory ancestor chain bottom-up, including newly created ancestors;
existence after an earlier sync failure is never considered durability proof. It validates the pinned
read-only snapshot/log, syncs the prepared inventory and source directories, removes only the
verified original links, then publishes the prepared directory at `kiln.db` and syncs its parent.
There is a recoverable non-atomic gap during that portable file-to-directory transition: the
external prohibition on old processes/restart must persist across crashes. Ordinary startup
refuses pending/unfenced state; resume this same explicit command rather than creating a fresh
store. No writable SQLite fallback, extra database, source-copy fallback or source deletion is
used. Any original or archived/pending rollback journal refuses unchanged before preparation
or SQLite read: use the existing old OFFLINE recovery/checkpoint owner first, without guessing
journal hotness. Unsupported durability/IO/source divergence refuses the transition. SQLite retains normal committed-WAL handling,
not URI immutable mode (which can omit WAL). Before any SQLite read, the inputs must deny an
actual nontruncating write-open by this process; elevated/ACL write authority refuses. An
uncheckpointed WAL must include its read-only SHM source; otherwise use the existing old offline
recovery owner to checkpoint a stopped snapshot. No migration source write is attempted.

The canonical Amphora restore is the one paged import owner. A completed receipt must match the
preserved source identity. Any writes an older build accepted after an earlier import are
explicit divergence requiring reconciliation; they are never silently overwritten or discarded.
Fresh and Amphora-only hosts also fence the old CREATE-on-open pathname. The current Windows
implementation refuses all disk-backed Store activation (fresh and Amphora-only included) before
mutation because it lacks the required directory flush contract. In-memory operation is unchanged;
durable Windows activation remains unsupported until that contract is implemented and validated.

Start only the canonical serving owner after successful cutover. Replicated source/baseline
selection remains explicit and startup validates the same fence and restore identity. The CLI
does not install, deploy, start services, generate replacement masters or choose replica policy.

A SQLite-era binary cannot be an image-only rollback target after this transition. Keep the
fence and preserved sources; roll forward to a compatible Amphora build. Removing the fence and
restoring an old snapshot loses later accepted state and is not a supported rollback.

The self-deploy guard runs the **previous** image. A fix in the new image cannot repair the
already-old first-upgrade guard. Perform this first format transition under the supervised
offline/no-image-only-rollback boundary (or a separately validated bridge rollout). Subsequent
new guards hold on unknown schema requirements, changed non-image pod-template fields, or
unproven external/volume/key-reference, image-pull-secret or declared service-account configuration. They patch only the exact container image;
complete configuration/revision rollback needs its own proven owner.
