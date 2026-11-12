# Mail transport: finite source checkpoint

POLICY_READ implementer, Shards origin/main749c3ca0602c43dac74976e8db713102f5b15874,
rules SHA256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Kiln base f555736bd887a4196c8841564cb9b4d6c1ede95f; task ROOT governs roles.
This is source inspection and a parser-checked implementation checkpoint, not live reproduction,
transport fault validation, readiness, or deployment. Root owns those obligations.

## Decision and inspected evidence

The existing owner is `agent.rs`'s event loop and its `wire::uplink::Updates` session streams.
`Updates` keeps sequence-numbered items until answered, responds to wants, and has an existing
protocol retransmission deadline. Ordinary TERM shedding is recovered by this channel; promoting
all history to Control would increase connection failure pressure without removing a missing
source-journal replay. That broader change is rejected.

`net.rs::Outbox::queue` instead sheds the oldest entry on a full non-Control lane. A full Control
lane returns Stuck and its driver closes the connection. A raw Admit/Peers/Ack has no Updates
sequence, so its lane must be Control, and files required by that Admit must precede it on the
same lane. Root owns those downlink changes. This does not enlarge OUTBOX_FRAMES or any byte cap.
After uncertain failure the durable server attempt must replay its original token, not a fresh
generation. Transport acceptance is never source or provider consumption.

`Updates::push_seq` can mark a stream stale and discard its unanswered backlog when no picture
waits. A session snapshot excludes its source Accepted/terminal mail journal. Previously stale
reset processing ran only after another incoming server Wire frame, which need not arrive if
the discarded stream was the sole unanswered stream. Both an actual local push and an incoming
Wire now call the same reset helper. It retains the existing `take_stale` picture-ACK gate and
terminal replay behavior. Local resets require connected+welcomed, so an outage does not build
new snapshots. Only an actual session reset requests mail-journal replay.

`Host::replay_mail` was present but had no caller in agent.rs. Actual Welcome, first MAIL_FLOW
feature negotiation/state-page refresh, and session stream reset now request that existing read.
One select-owned future is polled beside the bounded harness sink it writes to; awaiting it inline
would block the loop draining that sink. Further real events coalesce into one additional pass.
Disconnect/feature loss cancels the read; source journal custody is unchanged and the next actual
connection/feature event reconciles it. No timer, periodic probe, per-message waiter, body cache,
or synthetic capacity event is added.

For a server without the Updates capability, raw Open and MailFlow use Control in that order;
raw history stays Term. Feature refresh first sends current owner Open headers, then journal
replay. This preserves ownership ordering instead of assuming cross-lane ordering.

`Instances` identifies peers by certificates, stable across a same-certificate reconnect.
Registry's former born:generation:peer replay identity could therefore stay unchanged. The existing
tunnel registry already receives the actual authenticated TLS exporter on Connected and removes
it on Closed. A getter hashes that existing fixed-size value; mail_connection includes the hash
and requires Hello on the new connection (`!rejoining`). No new connection counter/map is added,
and neither exporter bytes nor private payloads enter evidence. Missing exporter/owner/Hello holds
delivery explicitly. `Owned::HarnessFile` also now selects the actual session holder during a
blue/green overlap, not the newest unrelated process.

Primary specifications consulted2026-11-11UTC:

- [RFC9000 sections2 and2.2](https://www.rfc-editor.org/rfc/rfc9000#section-2): ordered bytes
  within a stream, with no ordering across different streams. This supports placing required
  attachment chunks and their Admit on the same existing Control lane.
- [RFC8446 section7.5](https://www.rfc-editor.org/rfc/rfc8446#section-7.5): exporter derivation
  from the handshake exporter secret. This is the construction the existing net owner uses.
- [RFC9266 section2](https://www.rfc-editor.org/rfc/rfc9266#section-2): TLS1.3 unique handshake
  secrets support exporter-derived connection binding. Kiln reuses its existing application
  exporter label; it does not claim to implement the RFC's specifically named SASL binding.

## Frozen source and checks

Named `rustfmt --edition2024 --emit stdout` parser checks all exited0; named `git diff --check`
exited0. No file was reformatted and no Rust compilation/unit test was executed by this lane.

| Path | SHA256 at transport freeze |
| --- | --- |
| crates/kiln/src/agent.rs | 2cab301865cd9ad6341bace2b16f2d739ea7301c62e140c8f1fa23879f17475a |
| crates/kiln/src/tunnel.rs | 87bfd221ec6b6afb8ed9cfb4d5b8f097f70645fd04d1388b94f9a53ce548491c |
| crates/kiln/src/instances.rs | b369cba51c3a45a576e11e69f18c1cc88075c217afce61eb0ea274a317e00e8e |
| crates/kiln/src/harness/registry.rs | 44b078a80ca59cebe4e01797c2f027f642a991fda91a66cc0ed7e5111881b0b1 |

Registry is shared; this lane changed only mail_connection. Parent owns source hashes/combined
compile snapshots and downlink lane changes. No fixture source was added before this urgent freeze.

## Owed checks and limits

Root compile/type/lint, supported-platform checks, actual constrained-outbox/stream-overflow
red/green/mutation, same-certificate disconnect/reconnect with old-token terminal consume,
Control overflow closure, blue/green attachment-holder routing, and negotiated raw compatibility
remain owed. Suggested focused fixture names: `mail_connection_changes_only_on_real_reconnect`,
`a_local_overflow_replays_mail_without_another_server_frame`,
`mail_replay_yields_to_its_bounded_sink`, `raw_owner_open_precedes_mail_on_control`,
`attachment_chunks_stay_with_the_session_holder`.

This source checkpoint makes no new performance or4–5x claim (R08/R14), no allocation/cleanup
proof, and no full cross-platform transport claim. Existing physical channels, protocol bounds
and journal authority remain binding. R01/R04/R06/R07/R13 need coordinator review of the combined
source; R02/R03/R05/R12 are addressed by the inspected existing mechanisms and primary sources
above, without treating them as live R09/R10 evidence. R11/R15/R16 integration/platform/resource
and final evidence review remain coordinator-owned. Transfer of source journals/debt during
move/bluegreen is a separate critical batch; this checkpoint alone does not establish no-loss moves.
