# Bounded live proof for direct native journal handover

POLICY_READ implementer: Shards origin/main
3830327b6c8d7ac22452a5e32ffa32799263132c; full AGENTS/rules/evidence/CLAUDE read.
Rules SHA256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Kiln AGENTS, ci section 0 and current ROOT read. This lane owns this document and
`direct-native-live.py` only. Root executes every live/heavy step. Nothing below was run here.

The coordinator reports 552 focused tests passed from
`check-1794384968309236000`, plus QueueEdit growth/shrink tests. Those include bounded
header/fence/abort/pool and direct same-process handover fixtures. They do not establish
external-process SCM descriptor transfer. Public ops caller failures are a different lane.
The built artifact available when this plan was written is still f555 for a full CLI; the new
prevention source currently has a library test executable only. Do not use the f555 binary
and label its behavior as proof of the new protocol.

## Existing seams and what they prove

- `crates/kiln/tests/transfers.rs:394`,
  `w27_r08_session_hand_overs_blue_green_sends_nothing_again`, starts an isolated server,
  private HOME, real `kiln supervise`, blue and green, and the existing fake Claude helper.
  It exercises native SCM and signed custody. It does not assert a nonempty journal or a
  signed durable publication before upgrade, so it can pass without the new fourth FD.
- `crates/kiln/src/bluegreen.rs`, `frames_carry_descriptors_both_ways`, sends a real pipe FD
  through SCM_RIGHTS, but uses generic Forward/Ack frames in one test process. It is not a
  source/destination journal transfer proof.
- `agent::take_session` checks exactly three native FDs plus one journal FD for a manifest,
  before import/Adopt. `Host::prepare_native_import` stages and verifies the journal/debt;
  the same import preparation owns its mutex through Committed and durable activation.
  Source Saved/Moving retains the source mutex/native handles through ACK.
- `tests/common/mod.rs::{start_once,kiln}` supplies the isolated CLI/environment pattern;
  `tests/common/agents.rs` supplies HTTP opening, private fake harness and headroom policy.
  `tests/store_handover.rs` already implements a local stand-in handover socket, useful for
  a future bounded socket interposer rather than adding production fault/barrier APIs.

## Runnable success proof

Run only after the first full binary from the signed prevention source is built and the
coordinator binds its artifact SHA to its exact source/build receipt. The driver never builds,
installs, writes the input binary, contacts production, runs a model, or restarts an installed
runner. It writes a byte-suffixed copy, as the existing transfers test does, so its one private
supervised runner has an upgrade to the server's exact binary.

Root dispatches a normal Reach job with the existing finite validation envelope: 2 CPU,
32 GiB memory and 8 GiB scratch, on a Unix runner. No placement by hostname, floor bypass or
production service lifecycle is involved. On macOS select arm64. The live peak is unmeasured;
these are an operational ceiling reused from the admitted focused validation, not a claimed
optimal requirement. Use actual free disk above the operator floor plus the two binary copies;
the driver checks that before spawning. Linux data belongs under ~/.cache, not RAM-backed /tmp.

The exact command inside that already admitted job is:

```sh
python3 docs/research/measurements/queue-prevention/direct-native-live.py \
  --repo /absolute/path/to/the/signed/prevention/checkout \
  --binary /absolute/path/to/its/full/native/kiln \
  --sha256 ACTUAL_BINARY_SHA256_FROM_BUILD_RECEIPT \
  --source-receipt /absolute/path/to/retained/build-source-receipt.json \
  --out /absolute/path/under/cache/to/a/NEW/native-scm-proof-directory \
  --timeout-seconds 570 \
  --keep-free-bytes 32212254720
```

The shown 30-GiB floor is the Mac owner's operational requirement, not a new product constant.
For another runner use its actual effective policy. The fixture deadline combines existing
Ready 180 s, custody wait 300 s, readiness 60 s and cancellation allowance 30 s. Allow a bounded
50 s beyond it for cleanup; root job timeout 620 s. These are fixture/protocol deadlines, not
product probes. Observation loops are bounded test observers like the existing common::until.
The driver is Unix-only; Windows SCM claims would be false.

This uses the new prevention code at both ends (the byte-suffixed old fixture image has the
same source). It does not prove the first upgrade from the deployed f555 protocol; root needs a
separate old-source/new-destination compatibility run for that migration.

The driver creates one private loopback server, one supervised runner and one fake native
Claude process. It reuses the existing fake-claude source, in a private copy, with one explicit
completion seam adapted to include original ID, completed terminal reason and positive API
work duration. Started does not count as consumption. The existing GO gate holds that first
turn until after handover. No account or external model is read. The child environment is an
allowlist, with a fresh HOME, explicit fake program path, new local PKI and no inherited Kiln
endpoint/supervisor/credentials. Only the fixture's virtual runner has its test headroom setting
changed, as existing common::agents does; host/production headroom is untouched.

Required witnesses, checked by the script:

1. Its private server has the new `agent_mail_handoffs` schema. The exact message is admitted,
   not merely retained, and the actual fake provider records that original ID/PID before GO.
2. The server commits a handoff with manifest.count > 0, source epoch equal to the saved attempt,
   and destination epoch exactly +1. A plain idle transfer does not satisfy the check.
3. Upgrade reaches its real Done phase, including session ACK. Revision, attempt generation
   and immutable original body hash remain identical; its binding is now the destination epoch.
4. The same verified fake-process PID/start identity is alive afterward. Both source and
   destination private fsynced header files match the exact manifest and retain that PID.
   Success reaches take_session's four-descriptor gate. The driver does not separately trace
   the kernel ancillary buffer or claim mutation coverage of every descriptor-lifetime race.
5. Opening GO lets that same native turn complete. Only then is the ledger Consumed at the same
   immutable attempt/destination epoch. Exact caller retry remains terminal and reaches the
   stand-in once. This is a real-process fake-provider test, not a real-model test.

The script retains private `result.json`, raw server/supervisor logs, PID records, state files,
spools and its source/helper/artifact hashes under the requested output directory. It prints
only a sanitized result summary. Generated token files and raw logs are private evidence; do
not commit them or paste them into conversation. The build receipt is named and hashed; the
coordinator still owns establishing that it corresponds to the executed binary's source.

Cleanup first closes only its exact fixture session by API, then TERM/KILLs only its recorded
server/supervisor process groups. Any surviving provider is stopped only if its PID/start/command
still identifies the copied helper under the fixture directory. No process-name matching,
process-environ read, installed binary replacement, user HOME mutation or source journal purge
occurs. Preserve output even on failure. SIGKILL of the driver itself cannot run finally; root
then uses the recorded exact PIDs/identities, never pkill or a fleet restart. Root should also
record the known active-user PIDs/start identities before and after, verifying unchanged
identity/liveness, and retain a before/after fixture process list; script isolation is not
itself an observed forbidden-effect witness.

## Exact boundary follow-ups, still missing live coverage

Do not race an arbitrary sleep against Commit and call that a deterministic crash proof.
The current script covers successful external-process SCM only. The compiled unit/Host guards
cover cancellation and snapshot logic, but their pass is not live crash/restart evidence.

The smallest further live fixture should reuse the existing local socket stand-in/Conn framing:
place a fixture-only Unix socket interposer between blue's listener and green, retaining every
received SCM FD until its matching frame ACK. It needs no production feature or barrier. Record
frame name, byte hash, FD count and explicit parent pipe events; forward bytes/FDs unchanged.
Root may implement it in a disposable fixture after reviewing its FD ownership. Do not add a
product manager, timer or production phase hook just for this test. Required scenarios are:

- **Pre-import refusal/cancel:** stop forwarding the Session frame after receiving its exact
  header + four FDs but before green receives it. Terminate only the owned green and let the
  existing blue failure/cancel path run. Verify server Aborted before blue resumes; original
  remains unchanged, same native process receives the next intended input once, no extra writer.
- **Lost Abort answer:** after the preceding failure, interrupt only the fixture runner/server
  connection while Aborting is durable. Reconnect the same instance/certificate. Verify replay
  sends Abort (not Offer), remains fenced while unknown, resumes only on exact Aborted. If the
  authentic server says Committed, verify blue never kills green or resumes its old writer.
- **Committed but Session ACK lost:** interposer receives green's successful Session ACK but
  withholds it from blue. The exact committed server metadata and both headers must already be
  durable. Kill only the owned green instance. Restart it with the same --instance and agent.pem,
  without replaying stale FD integers. Verify marker prevents startup survivor killing/new actor
  until exact phase recovery; the restored journal/body/attempt survives and unknown native
  flight stays explicitly manual/uncertain, not synthetically consumed or automatically resent.
- **Source preheader crash:** hold destination Ready (before source Offered), so only the fence
  exists. Restart only the owned source with the same identity. Verify no automatic actor/native
  kill; full originals stay held/manual. This intentionally does not promise full Frozen recovery
  before the full header could be written under actual Offered policy.

For each scenario capture exact source/binary IDs, owned child PIDs/start identities, message
revision/generation/epochs/body hash, server phase row, marker and header hash, provider prompt
count, and cleanup/forbidden-effect witnesses. A metadata assertion without the actual native
process/socket sequence is insufficient. Attachment transfer/materialization, code-signing
portability of the byte-suffixed fixture binary, fresh-instance/certificate replacement, Linux
libc/kernel variants, Windows native move, and total decoded/process memory remain separate
unverified coverage. No new signature bypass, cap increase or receipt inference is permitted.
