# UI request fixture migration checkpoint

POLICY_READ implementer: full Shards origin/main
`749c3ca0602c43dac74976e8db713102f5b15874` AGENTS/rules/evidence and Kiln
ROOT reread; rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Kiln base `f555736bd887a4196c8841564cb9b4d6c1ede95f`, isolated clone
`/private/tmp/kiln-queue-prevention-20261111`, macOS arm64, Node v24.14.1.
Owned changes: `ui/tests/asked-commands.test.ts` and this evidence. Production
Held startup guard belongs to the coordinator; prior Told/mail remain frozen.

The coordinator removed the obsolete send/inbox-ACK retry subcase from the
request-only fixture. Its remaining commands exercise actual request/response
semantics: pending, parked, refused, reconnect under the same ID and unanswered
expiry. This lane preserved those assertions and corrected their final stale
mail-related assertion description. A search of `ui/tests` found no remaining
ordinary `store.send({type:"send"})` fixture calls; mail fixtures already use
awaited `sendMail` and `resend` with fake IndexedDB and authenticated scope.
Those other test files were not edited by this lane.

The new regression runs with a browser-shaped window and no IndexedDB. A
BroadcastChannel constructor spy proves module import does not create a channel
when durable mail storage is unsupported. With an explicit authenticated test
scope and an open, synced socket, both ordinary-channel bypass and `sendMail`
refuse the mail. Assertions preserve the exact UTF-8 command, inline attachment,
reply identity and original Blob, verify no wire command/asked/outgoing/Held
entry, and require the visible save failure. No retry timer, storage fallback,
communication deletion or production text/tag heuristic was introduced.

Executed scoped command from `ui/`:

```sh
TSX_TSCONFIG_PATH=tsconfig.app.json node --import tsx --test tests/asked-commands.test.ts
```

Final formatted source: exit 0; 2 tests, 2 passed, 0 failed/cancelled/skipped/todo,
duration 142.979667 ms. The initial scoped run also passed both cases.
The two cases were the nonmail request ACK/NACK/reconnect fixture and unsupported
durable-storage/channel regression. The initial `tsx` CLI invocation exited
before any tests with sandbox EPERM creating its own IPC pipe; Node's registered
tsx loader avoided that CLI listener and ran the scoped tests without escalation.
Scoped Prettier ran successfully with `--print-width 200`. These are test-only
changes; they do not reproduce browser crash durability or establish complete app
delivery, performance, seven-platform coverage or readiness.

The coordinator separately reports native Chromium and WebKit Held fixtures;
this lane did not run those actors or claim their result. Full-suite run r54 was
interrupted by the coordinator at its obsolete legacy fixture; its remaining
tests are unrun, not failed. Typecheck/build/full suite, runtime guard mutation,
cross-lane integration and all applicable final policy gates remain coordinator
work. No production edits, builds, services, heavy tests, children or commits.
