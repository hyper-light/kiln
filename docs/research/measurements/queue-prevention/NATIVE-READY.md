# Native takeover feature readiness

Implementer POLICY_READ: Shards origin/main
06ae067c0beda71c98a8c6f9633df9feb71fc11b; rules SHA256
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Root owns compilation, the isolated SCM campaign and integration proof. This batch
changes only `crates/kiln/src/agent.rs` plus this evidence record. Base is signed
private checkpoint 0ff33887757bc0584e7a69abfa0d2759ec7d850d.

## Actual red and mechanism

The coordinator reported the shortened-path real native campaign against exact
0ff338 release artifact (SHA prefix `33d5689`): `native-live-short-0ff338/result.json`
exit 1 after 23.309 s. Initial publication was revision 1/generation 1/epoch 0,
Admitted; native provider PID 10804. Old source reported at 10:04:35 that the new
runner/server had not negotiated journal handover and retained the source. The
earlier long Unix-socket-path problem was absent from this rerun. This implementer
did not execute the campaign or independently verify its cleanup witnesses.

The production path read in 0ff338 confirms the ordering dependency:

- `Hub` sends Caps before Welcome. It sends named features separately after taking
  the runner's wide capability frame; Welcome is not proof that reply arrived.
- Welcome consumed `take_over` and spawned the existing takeover worker immediately.
- The worker's Ready frame captured `handing::takes_mail()`, initially false.
- Only the later Features handler set that global from the actual `mail-transfer`
  feature. `Host::move_out_signing` correctly refused a pending journal when either
  destination support or the source's negotiated server support was false.

This is an initialization race in the actual native caller path. The refusal is
the required custody guard; weakening it would hide the defect.

## Narrow correction

One explicit per-connection negotiation state is owned by the existing agent loop:
AwaitingCaps, AwaitingNames, Legacy or Named. Actual Caps lacking `CAP_FEATURES`
is a known legacy answer. An actual named reply settles negotiation even when
`mail-transfer` is absent. Welcome alone leaves unknown state unknown.

Caps, Welcome and Features may each release the existing pending takeover, but
only once, after Welcome and a known answer. The existing worker is factored into
one helper; no new manager, timer, polling, actor queue or per-message task exists.
The Ready capability calculation and source pending-journal refusal are unchanged.
Connected/Closed reset negotiation; Connected also clears the shared support bit,
so a new connection cannot reuse an old negotiated positive. Missing Inbox remains
an independent negative capability.

Features before Welcome waits for Welcome. Welcome before Caps waits for Caps;
named Caps then waits for Features, whereas actual legacy Caps is sufficient to
send Ready with unsupported mail capability. Duplicate handshake events cannot
consume a second pending operation. Disconnect before negotiation leaves the
operation pending for actual reconnection events. Shutdown still uses the existing
agent/upgrade cancellation owner; no new native descriptors exist before Ready.

A server that never sends Caps or named features is **not** guessed legacy. Its
upgrade can reach the existing `READY_WITHIN` deadline and retain the old source.
This deliberately leaves truly pre-Caps legacy compatibility unverified/held.

## Checks and remaining work

`native_ready_waits_for_actual_features_on_the_current_connection` is a pure state
fixture covering the delayed and reversed event orders, duplicate events, known
legacy Caps and reconnect. Its source is written, not executed. It does not prove
actual SCM/FD transfer or cancellation cleanup. Named-negative replies still flow
through the unchanged capability calculation and existing source refusal guard.

Actual light checks: production-tree
`rustfmt --edition 2024 --emit stdout crates/kiln/src/agent.rs` and scoped
`git diff --check` both exited 0. No Cargo, native process, service, live campaign,
benchmark or deployment was run by this implementer. The first private-copy parser
attempt could not resolve sibling modules; the actual-tree parser above is the
successful check. Root owes a fresh compiled fixture run and the actual shortened
native campaign, including source retention/FD/process cleanup on failure.

Frozen `agent.rs` SHA256:
`a330dcea93219a2e4e0265e372d2f9c2fb05d8cc47a70c9283194702f645c9e7`.
