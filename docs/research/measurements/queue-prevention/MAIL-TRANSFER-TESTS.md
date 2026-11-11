# Transfer handler regression source (uncompiled/unrun)

POLICY_READ implementer: full Shards origin/main
`7d7e0855a5073ed5163b4fe7ed3355c9ed7c2506` AGENTS/rules/evidence/CLAUDE and
current Kiln AGENTS/ci section 0/ROOT reread. Rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Kiln base `f555736bd887a4196c8841564cb9b4d6c1ede95f`, macOS arm64, isolated
`/private/tmp/kiln-queue-prevention-20261111`. Root fixed production behavior;
this lane owns only the cfg(test) declaration in mail_transfer.rs, new
mail_transfer_tests.rs and this evidence. Prior Told/wait/mail/handoff SQL/storage
fixtures remain frozen.

The narrow read-only source trace found two concrete handler errors, reported to
root before these tests: terminal handling of an old committed record could
reset routing after a newer handoff, and certificate-only gates omitted the
stored process instance. Root fixed these in production. This is source-level
evidence; it is not represented as a live production reproduction. Root approved
real existing Hub/Store/Instances fixture authoring without production helper
extraction.

Two enabled Rust fixtures now use the existing scratch_hub, actual in-memory
Store migrations and handoffs offer/prepare/adopt, and actual Instances
connected/announce/join metadata:

- `replaying_an_older_committed_handoff_cannot_reassign_the_latest_owner`
  commits A→B then B→C, with one actual immutable dispatched mail original.
  It installs the latest terminal owner through the private real terminal
  handler, then replays A→B Offer, Ready, Adopt and Abort through actual apply.
  Every step checks routing owner C, stable holder label, latest durable C record
  and dispatch readiness. Destination reconciliation proves the exact original
  Token and complete publication are unchanged. Removing the latest-record
  ownership guard should fail the owner assertion; mutation execution is owed.
- `same_certificate_with_a_new_instance_cannot_pass_stored_transfer_principal_gates`
  commits one real handoff, proves exact destination identity is accepted,
  invalidates current Hello through Instances.connected and proves denial,
  then provides actual same-instance Hello as a positive reconnect control.
  A new InstanceHello under the identical fixed certificate PeerId/machine
  passes current_connection but fails claim/Open, inventory, generic adoption,
  dispatch and phase-send checks. Durable binding remains the original process.
  This distinguishes missing current metadata from a valid current connection
  with the wrong stored process identity. Removing stored-instance comparisons
  should fail the corresponding assertion; mutation execution is owed.

Fixed hexadecimal PeerIds are explicit fixture certificate identities. Process
identity comes from real InstanceHello fields and the current principal helper;
there is no invented authentication cache or synthetic empty receipt authority.
The tests model authenticated metadata at the existing internal seam, not TLS
handshake/certificate issuance. No runner/provider process, private production
body or external service is used. The existing scratch_hub opens its isolated
loopback endpoint only when root executes; each authored test signals shutdown
on success or error. Actual endpoint/resource cleanup remains unverified until
execution, and the fixture's existing persistent scratch-directory behavior was
not broadened or claimed clean here.

Actual lane checks: `rustfmt --edition 2024 --emit stdout` parsed both files with
exit 0; named-file whitespace check passed. No own compile, test, mutation,
actor/service, audit, profiling, benchmark, heavy build, full suite, child or
commit occurred. Root owns fresh compilation, bounded runtime/mutation execution,
cleanup witnesses, supported-platform coverage and all applicable final gates.
These are source fixtures, not passing readiness or performance evidence.

Frozen SHA-256:

- mail_transfer.rs: `588311e7dafc38d8568d27c7e80f8833e1cf265732bfd180ae6b7b799fe2e40f`
- mail_transfer_tests.rs: `2d3f56635a7ba886155763593efa4e83cbc65f5fb811b36fbf4d0fffaf85f44c`
- production prefix before the test declaration:
  `d384872924ff491d5c3bab025168bb22d80cfb706575c8042b27047c869ad6b3`
