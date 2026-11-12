# Source-owned wait response proof (implementation in progress)

POLICY_READ implementer: full Shards origin/main
`749c3ca0602c43dac74976e8db713102f5b15874` AGENTS/rules/evidence;
rules SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Kiln AGENTS/ci section 0 and current ROOT reread. Source base
`f555736bd887a4196c8841564cb9b4d6c1ede95f`, macOS arm64, isolated
`/private/tmp/kiln-queue-prevention-20261111`. This lane owns only ops hand_over/
wait integration, transient wait claims, the bounded state_reads registration
helper, focused fixtures and this record. Source journal/provider ownership is
the separate queue_index_consumers lane; shared Command/classification is root.
No new response/body archive, manager, periodic retry, polling or native-call-ID
guess is authorized.

## Existing seam and approved replacement

The prior `ops::hand_over` selected peer queue IDs, read each source body, called
`state_reads::remove`/AtState QueueRemove, then returned sender/text/time without
message identity. Thus removal preceded any proof that a native tool result
reached its model. The current `Cx` contains only hub and authenticated caller;
it contains no native tool-call identity. An HTTP/MCP operation return is not a
native provider consumption receipt. This is a scoped source observation, not a
new live crash reproduction.

The approved smaller design retains existing durable source Pending items and
input leases. The server assembles one bounded response with exact message IDs,
then registers metadata through one owner-only Command::MailHandoff request on
the existing bounded live Reply seam. The source journal owns one outstanding
handoff for an original/turn. Registration success is not delivery or consumed
credit. The queue and leases remain until the source correlates the configured
kiln native tool result, exact marker and digest, and actual main-turn completion
(or stronger pinned-provider acceptance proof). Unknown/missing proof retains
the original for normal delivery. There is no QueueRemove or UserCancelled
transition in the completed handoff path.

Successful response fields are:

```json
{
  "messages": [{"id":"original-id","from":"peer","name":null,"text":"exact peer body","at":1}],
  "mail_handoff": {"id":"handoff-id","turn":1}
}
```

The hash covers only canonical `{handoff,turn,messages}`. The shared
`wait::handoff_digest` serializes fields in that order; each message uses fixed
`id,from,name,text,at` order with borrowed string data and a bounded SHA-256 writer.
This normalizes object insertion order, including serde_json preserve_order and
native key reordering. Unknown/malformed message fields fail closed. The marker
contains no digest, so there is no circular hash. Other wait outcome, agent brief
and elapsed-time fields are excluded from mail proof. The whole candidate wait
response must fit the unchanged 512-KiB bound before registration.

Registration fields are exact `client,revision,handoff,turn,ids,response_digest`.
The reply is `{result:{type:"registered",revision:<same queue revision>}}` only
after source journal metadata is durable. A stale queue, unknown owner, missing
reply, malformed reply or lost response leaves queue/body/lease custody intact.
The server returns no handed messages on registration failure. Caller context
provides no native call ID; the source binds the actual observed native result
later. Queue items with attachments are not handed as text-only mail and remain
available for normal file-complete delivery.

Server claims last only through actual response assembly/registration, with
request-local Drop cleanup on all returns/cancellation. Their key is recipient
plus original ID. Durable source metadata owns overlap and terminal proof; the
server's former 120-second/4096-entry remembered-delivery cache is removed. This
is one existing seam, not another source of accepted-body authority.

## Partial development freeze

Root requested a combined compiler checkpoint while this stage was being
implemented. At that checkpoint only the shared digest, transient claim change
and compatibility call-site were written. **ops::hand_over still contained the
old remove-based implementation**; state_reads registration integration and
focused proof fixtures were pending. This partial stage must not be represented
as a completed safety fix or deployment-ready implementation.

Parser and scoped diff checks exit 0. Frozen wait.rs SHA-256:
`991eb1f438a609ea815d2d61de01e9b3ad41f5f92865a49f3fd66d8ef3347797`;
ops.rs SHA-256:
`dbd0f8cba5217a29192b9df59e99fb1bfbd5841a3721dff85f9cd10699442e89`.
No Rust compilation, test execution, provider actor, service action, heavy check,
profiling, benchmark, audit, child delegation or commit was performed by this
lane. The coordinator owns actual combined compilation, native proof/fault
validation, resource/cleanup measurement and all applicable final policy gates.

The finite next source fixtures are: fixed canonical bytes and SHA-256 independent
of message-object key order; changed message ID/sender/name/text/time, marker and
turn changing the digest; malformed/unknown fields and oversized escaped UTF-8
content refusing before registration; exact full response envelope budget;
recipient-scoped transient claim cleanup after failure/cancellation; and Reply
matcher refusal of stale/missing/wrong-type registration acknowledgment. These
are test requirements, not executed results. Provider fixtures must separately
prove the exact native result plus correlated main-turn completion; the old
fake-cat operations fixture that expected queue removal immediately after an
HTTP wait result cannot establish that proof and requires an explicit retained
queue assertion before a genuine source event.

## Completed server-side source checkpoint (uncompiled)

After coordinator r67 test-binary compilation passed with unchanged captured
source, root released the Rust hold. The server wait path now stages exact
messages and marker, validates the full returned wait Value under FRAME, and
sends one owner-only MailHandoff registration through the existing Reply seam.
It no longer calls QueueRemove/AtState removal. A source item must belong to the
same queue field revision; a text-only result skips attached originals. A failed
registration removes only the staged response fields, returns a typed
`mail_handoff_error`, and drops request-local claims. Source queue/body/lease
custody remains unchanged. No native tool-call ID is invented from Cx.

`state_reads::request` attaches the live reply listener before dispatch. Only the
constructed internal MailHandoff command uses `mail_delivery::send`'s current
owner/Control route, because public Registry dispatch correctly refuses it.
Registration acceptance requires exact `type:registered` and the requested queue
revision. A receipt for another client, missing/stale revision, lost feed, timeout
or an ordinary queue-acceptance outcome cannot stand in for this acknowledgment.

The shared digest now validates nonempty bounded message count and fixed message
schema before reserving borrowed references. A streaming SHA-256 writer refuses
encoded bytes beyond FRAME. Its canonical vector independently derived with
Python json UTF-8 compact encoding has 138 bytes and SHA-256
`4f321f0d9414582e25d29dc364893ae2ea6caaf760b4335853894ba7706329d8`;
the Rust fixture pins that vector and object-order independence. Other new test
source covers mutation of each proof field, malformed/unknown fields, escaped
over-budget data, recipient-scoped claim release, whole wait Value budget,
rollback of staged output and exact registration acknowledgment. These six new
Rust fixtures are unrun. Existing mail selection fixture now holds an RAII claim
explicitly, then proves dropping it does not permanently mark delivery.

Actual parser and scoped diff checks exit 0. Source hashes:

- wait.rs: `cf960b4299bc67842bb34201ff50908d2e9447f175f380fe88c3aee6c9f6dab0`
- state_reads.rs: `66dad967b451a0cd0c3ee3bd106ba559a116696ecc29cf77e5d6092077fefe96`
- ops.rs: `67c41c0861bb6dfc6c28ae02434fb63c3b25b5f6676279f54aba90ba7c3f0ed1`

No Rust compile or execution has validated these hashes. r67 covers the earlier
partial freeze, not this completed server-side chunk. Source journal metadata,
physical lease growth accounting, native provider proof and its fault fixtures
remain the source lane/coordinator's integration responsibility. Existing wait
fallback RECHECK and SENT observation behavior were not broadened or changed by
this handoff chunk; this is not a claim that all legacy polling has been removed.

The returned wait Value and owner control command have exact byte checks. FRAME
is the owner/control/state cap, not automatically the MCP transport cap. Existing
`mcp::tool_answer` wraps the Value twice (escaped text plus structuredContent), so
the full native MCP/RPC envelope can be larger and its existing encoder/transport
enforces its own cap. No guessed ID, arbitrary reserve, cap increase or MCP
interface change was introduced. Coordinator review will measure the real wrapper
at that actual transport seam; source proof remains absent on response failure,
which cannot consume or erase the retained source original.
