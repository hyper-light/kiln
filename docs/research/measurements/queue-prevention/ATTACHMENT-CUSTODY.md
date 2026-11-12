# Durable attachment references under the existing frame cap

POLICY_READ implementer: Shards origin/main 749c3ca0602c43dac74976e8db713102f5b15874;
rules SHA256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Full AGENTS/rules/evidence and current ROOT reread. Kiln AGENTS/ci section 0 loaded.
Owned only released Registry upload/cache/route spans, HTTP harness_upload, broadcast attachment
normalization, one approved canonical-preview call in mail_delivery and attachment fixtures.
No child agents, source Host/provider/materialization changes, builds, services or live actions.

## Evidence and decision

Existing primary implementation seams read: kiln-seams/src/blobs.rs (conditional immutable put,
SHA-256 checked before storage, typed refusal); kiln/src/blobs/local.rs (chunk/object/key commit,
non-cas namespace key pins the object); blobs/chunks.rs (chunk sync and directory sync before
naming); blobs/io.rs (bounded metadata get and ranged reads); store/storage.rs (actual SQLite
layout reserve); docs/blobs.md and the ROOT storage design.

The old Registry Upload Rcu map used a 30-minute TTL and 512-MiB pool, evicted sent files,
forgot all uploads at restart, and route_attachments silently omitted missing bytes or failed
local writes. It also generated a fresh receiving file ID on each retry. The old broadcast path
expanded uploads into inline base64 per recipient. These are source observations, not a new live
failure reproduction or performance measurement. Coordinator owns remaining R09/R10 live proof.

Use the existing Blobs/CAS implementation instead of a new file cache, manager or worker.
Every new HTTP upload writes an immutable bytes key `attachments/uploads/<UUIDv7>` and then its
immutable metadata `.json` key (name, MIME, size, SHA-256). Both puts use IfNoneMatch plus expected
SHA. HTTP ACK follows both successful receipts and exact size/hash checks. A partial failure can
leave pinned bytes without metadata; no ACK transfers caller ownership, no original is deleted,
and later lookup cannot fabricate a completed upload from those bytes. No cleanup daemon added.

The non-CAS key namespace is deliberate: LocalBlobs pins keys outside `cas/`; CI CAS collection
otherwise finds its unreferenced objects after a grace period. Existing local/S3/Mantle Blobs
adapters remain authoritative. No parallel cache or OS-specific storage path was introduced.

Before any put: enforce existing Host ATTACHMENT_MAX (25 MiB), encode metadata within the existing
state_view FRAME (512 KiB), count actual CDC cuts for data+metadata and object/key rows for both
puts, call storage_reserve_tables for blob_keys/cas_objects/cas_chunks, then compare real data-volume
free bytes against those bytes, SQLite scratch and the configured existing disk_free floor.
Unknown authority/refusal transfers no custody. This preflight cannot reserve against unrelated
concurrent filesystem writers; actual commit errors still prevent ACK. The reservation is
conservative, especially for a remote Blobs adapter; no optimality or throughput claim is made.

## Identity and transitions

An exact immutable accepted mail retry is answered BEFORE owner, capacity, disk or attachment
lookup. Changed input under that identity remains Reused. New custody obtains an exact canonical
wire preview from attachment_command: it validates durable metadata/body head, or complete inline
bytes; rejects missing/ambiguous/path-only originals; derives the receiving file ID and measures
the bounded shape without changing the original stored command. mail_delivery uses that preview
in longest-token envelope preflight. Oversized inline input remains typed preaccept TooLarge.

The original server file_id remains unchanged in ledger and every broadcast recipient. Receiving
file ID uses existing util::derive_id with namespace, recipient, original message ID, attachment
index and content SHA. Different content/recipient cannot silently inherit another file ID. Route
repeats the same canonical command, reading no more than existing FILE_CHUNK per range, checking
metadata version/size and full content SHA before final done and command dispatch. A missing,
corrupt, partial, local-write or transport failure returns typed held/unreachable; no name-only
message is sent. No wire/frame cap is raised and no giant broadcast inline copy is created.

Source materialization remains a separate owner lane. Its existing skip-on-failure path was
reported to the coordinator/source agent, who is changing it to fail closed before acceptance.
The server attachment changes alone do not prove source/provider file consumption or retention.

## Finite checks and remaining proof

Parser only, actual exit 0: rustfmt --edition 2024 --emit stdout for registry.rs, mail_delivery.rs,
inbox/faults.rs, broadcast.rs and api.rs (no source formatting). git diff --check exit 0.
Source hashes are in attachment-source.json. No Rust build/test execution by this implementer.

Replaced old cache-eviction/512-MiB attachment fixtures with bounded disposable fixtures:
- w20_a_durable_upload_routes_to_the_same_wire_command_after_registry_reopen;
- w20_missing_upload_refuses_new_custody_but_an_exact_committed_original_keeps_its_receipt;
- w20_attachment_content_and_recipient_fence_stable_receiver_ids;
- w20_an_oversized_inline_original_is_refused_before_custody_and_a_partial_upload_has_no_metadata;
- w20_upload_bytes_and_metadata_survive_the_actual_store_reopening (existing helper is Unix-only).

These source fixtures are UNRUN. They cover immutable conditional-put refusal/no deletion,
preview/route parity, retry and registry/store reopen, missing preaccept refs/no ledger write,
accepted legacy receipt priority, changed input Reused, bounded inline refusal, partial metadata
failure and content/recipient identity. Coordinator owns compile, native supported-target tests,
actual crash/fault/transfer/move/cleanup evidence, source materialization integration, resource
profiling and R01–R16 review. The existing Blobs Vec put can copy a whole bounded upload; no
allocation-free, memory-optimal or comparative performance claim is made.
