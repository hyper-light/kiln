# Source journal transfer preparation

Implementer policy749c3ca0602c43dac74976e8db713102f5b15874 /
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3; current ROOT
and TRANSPORT.md give role/source context. This is a proposal while R67 holds Rust writes.
No transfer implementation, test, live red, or successful handover is claimed here.

## Partial implementation checkpoint after R67

Resumed POLICY_READ implementer7d7e0855a5073ed5163b4fe7ed3355c9ed7c2506, rules SHA256
unchanged. The proposal above/below records the decision history; this section states what is
actually written. Root selected same-token transfer across verified epochs and owns server
`mail_transfer.rs`, durable Store handoff transaction and owner/claim/generic-adoption gates.
The sender/destination handshake is Offer → destination Offered-policy persistence → Ready →
source Offered → bounded package import/debt → Adopt → Committed. New Open/capacity is never proof.

Written source primitives:

- NEW `inbox/transfer.rs`: one-row fenced export with domain/length-framed digest of exact
  originals/current edits/native handoff/terminal metadata; conditional durable role/phase;
  destination rows staged under the immutable attempt; full persisted hash/count verification;
  only exact Committed can install unchanged tokens at the destination epoch. No staged body
  is made active on partial import. Opaque legacy flight is typed and never invented as Send.
- NEW `moving/stream.rs`: explicit Unknown/Off/Reserved disk policy; private streamed Spool
  authorized from actual free volume bytes and the effective explicit floor before each piece;
  exact offsets and existing protocol byte bounds; sync before retention. Unretained spool Drop
  removes its own newly created file only. v4 header decoder then reads one Pending value at a
  time. Package gains optional manifest; Saved gains metadata plus serde-skipped journal File.
- Existing Reach owner receives adoption into Spool rather than a whole package Vec; actual
  authenticated policy event precedes destination Ready. Existing handing waiter now has typed
  Session/Mail answers in the same owner, Control phases and RAII removal. Actual Welcome/feature
  events replay durable metadata, one row at a time, without a polling task.
- NEW `host/transfer.rs`: authenticated phases persist before waiter notification; no-attachment
  v4 receiver stages/imports/restores debt, waits Committed, then activates and saves. File-bearing
  or opaque native-flight packages remain explicitly held until complete file/native evidence is
  implemented; they do not degrade to words-only or fabricated replay.
- `loops::file` and its real callers propagate failed/missing Host storage as typed refusal.
  The command no longer says Taken after failed IO. Untracked legacy failure is logged without
  claiming success; sender custody remains upstream.
- Host handover APIs propagate the source agent's new Result. `move_out_signing` preflights
  journal/IDs and settles before detaching; unreadable IDs are no longer replaced with a guessed
  in-memory list. Agent prepares this before terminal detachment. The finite checkpoint safely
  refuses a direct handover with existing Pending until the complete SCM manifest path lands.
  This is an explicit retained failure, not a claim that full handover is implemented.

Verification actually run by this lane: fourteen named rustfmt emit-stdout parser checks0 and
named diff-check0. Exact whole-file hashes/check commands in `mail-transfer-source.json`; shared
files include other agents' disjoint changes. No Rust compile/test executed by this implementer.
Root's R67 test compilation0/source-unchanged predates this batch and is not this batch's proof.
New simulated fixture sources are manifest original/edit/token binding, partial import/commit
activation, and exact spool offset/bound/drop cleanup; all remain unrun at this checkpoint.

Required remaining implementation: source v4 package assembly/stream send; feature-gated direct
bluegreen Ready plus extra journal SCM descriptor; all sender FD/custody retained until Commit;
complete attachment evidence/materialization per row; recovery/activation from durable retained
package after destination/server restart; actual physical authority for full existing header
State/events decoding and any surviving encoded copies. The new source HandOver API can also
wait on a bounded sink; moving that operation off the draining control turn remains necessary
before claiming its old quiet/full-sink deadlock fixed. No new timer may substitute for that owner.

## Exact current seams

- `host::Saved` and `moving::Package` carry recent deduplication IDs/digests, not the separate
  `inbox-pending` journal. Pending also contains immutable original command, explicit current
  edit revision/body, exact body byte charge, lease/token/stage, disposition, original accepted
  time/sequence and digest. Copying only queue text or recent IDs loses these semantics.
- `Host::move_out_signing` currently detaches harnesses before reading IDs; failed carry falls
  back to its in-memory IDs and continues removing sessions. This must become fallible before
  any old-owner release, terminal detach, or handover stream emission.
- `take_over_now` currently writes Decided::Taken before open/import. Journal import and
  restored debt must precede the positive decision; otherwise a committed Taken may authorize
  old-owner release without a complete destination original.
- `Host::open_seeded` already attaches Input in Restoring before starting its actor.
  `replay_session_mail` reads one row at a time, restores live leases including excess as debt,
  and calls restore_complete only at successful end-of-scan. Input unavailable keeps target0.
  This is the gate to reuse, rather than another transfer queue/controller.
- `Host::adopt`, adopt_package, move_in and their agent callers are currently synchronous.
  Exact import through Records is async; these callers need Result propagation without
  nested block_on or early success ACK. Root granted their narrow transfer paths.

## Bounded representation and physical copies

Use a private transfer-only spool with streaming serde sequence for Pending records. Export and
import process one exact row at a time; custom serde must not quietly turn into Vec<Pending>.
The [Serde streaming-array example](https://serde.rs/stream-array.html) demonstrates SeqAccess
processing without collecting a Vec. Existing package byte bounds still limit wire/disk framing;
they are not permission to allocate a package-sized RAM buffer.

Trace every final encoder: bluegreen::Conn::send currently to_vec(frame); normal moves use
to_vec(Package), then Reach::adopt(&bytes); local hand-to-newer retries clone the complete Vec.
Custom journal serde alone does not remove those copies. A selected implementation must count
exact serialized bytes with a writer, reserve actual temporary storage from the same strict
host aggregate pool before allocation, retain the guard for every surviving encoded copy, and
release it on final ownership/drop. Source agent proposes Input::reserve_buffer and RAII growth
for this real consumer. Refuse a copy that cannot fit without freeing any accepted source lease.
Prefer borrowed retry sends to cloning. Package/Frame counted output must never first pass
through to_value. Spool cleanup is transfer abort/success cleanup only: never remove the source
journal or retained originals to make the transfer fit.

A package carrying journal data must version-gate old receivers; silently ignoring its new
optional field is unsafe. Empty/ordinary packages retain prior compatibility where feasible.
Partial imported originals persist but remain fenced/unavailable until the exact import and
ownership handshake completes. A typed refusal leaves the previous source responsible.

## Epoch/token decision required at the shared owner

An existing source lease uses old epoch E and immutable attempt token T. Destination runs E+1.
Controller.restore rejects E there. Merely rewriting its lease epoch to E+1 makes new-owner
Consumed(T,E+1) fail the server's exact attempt binding. The current ledger Transferred path
requeues Waiting and a subsequent claim increments attempt generation, while the source journal
still holds T. Retrying a new Admit then finds the old original and replays T: no successful
receipt can close that mismatch. For ProviderFlight, issuing another attempt also cannot mean
another provider request.

The coordinator must select and integrate an authenticated ownership transition, either:

1. Preserve the same uncertain/admitted T across a verified destination epoch transition,
   retaining old-epoch receipt authorization and never treating it as new admission; or
2. Keep destination Restoring until the exact original journal is atomically rebound to a
   server-issued post-transfer attempt, preserving old terminal history and provider uncertainty.

The implementer recommended the first for already accepted/ProviderFlight work. The ledger and
relay are other owned spans, so neither semantic is invented locally. Source old-owner release
must follow positive complete destination custody, not the start of streaming. Owner/Open/Flow
ordering also needs an explicit fence: a late old-owner Transferred event cannot simply pass
the current-owner check after new Open has replaced it.

## Coordinator-owned validation owed

Actual live journal export/import failure before detach; no partial Session/Terminal handover;
ordinary IDs plus originals/current edits/attachments/terminal history across restart; exact
token consumes after source disconnect and verified move; ProviderFlight remains uncertain;
partial import/retry/accounted debt before actor writes; old receiver refusal; allocation/peak
memory of every serialization path; source/destination disk failure and cleanup; supported
platforms. Parser or simulated pure rows will not establish these live properties.
