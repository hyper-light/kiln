# Seams

kiln reaches every product through `kiln-seams` (`crates/kiln-seams`; the critical path's master
plan, §3, CP-M2): a trait for each, its simulator, and its conformance suite. A track that needs
another's component builds and tests against the simulator, and switches to the real one when it
lands; the real one passes the same suite.

Each section below says what a seam is, what implements it, what uses it, its methods, and its
contract, clause by clause. A clause's id is its conformance case's (`kiln_seams::<seam>::cases`),
and its words are the case's: `crates/kiln-seams/tests/doc_truth.rs` fails when this page and the
code part (a method or a clause added, removed or reworded on one side only).

How a suite runs: `kiln_seams::conformance::run(seam, &cases, make)` runs each case against a
fresh fixture from `make`, an implementation and what the case needs of it (a simulated clock to
move, a real one to wait out). Every simulator is deterministic, in memory and bounded: past its
bound it refuses, typed, and says so, never dropping silently.

## Clock

Time: a monotonic now (what durations and deadlines are measured by), the wall clock (what records
are stamped with), and sleeps.

- **Implementations:** `kiln::rt::RtClock` (kiln's runtime: `Instant`, the system's clock, the
  runtime's timers); `kiln_seams::clock::SimClock`, whose time moves only when a test moves it
  (`advance`, `advance_to_next`), its sleeps waking in deadline order, at most 1,048,576 of them
  waiting (one more completes at once, and `refusals()` counts it). A test waits for the code
  under test to come round to its next sleep (`sleeping_since`, `advance_and_settle`) without
  sleeping itself.
- **Used by:** the goal driver (`goals/mod.rs`, through the hub's clock) and the harness host
  (`harness/host.rs`: its watch every 10 s, the half second before its record is written).
- **Passed by:** both, every clause (`rt`'s tests run the suite on real time).

Methods: `now`, `unix_ms`, `sleep_until`, `sleep`, `unix_s`.

- **CLOCK-1** now() never goes back, and moves by the time that passes
- **CLOCK-2** unix_ms() is the wall clock, moving forward as time passes; unix_s() is it in seconds
- **CLOCK-3** a sleep until now or a deadline already past, or of zero, completes at once
- **CLOCK-4** a sleep never completes before its deadline
- **CLOCK-5** a sleep completes once its deadline has passed
- **CLOCK-6** sleeps complete in deadline order, whatever order they started in
- **CLOCK-7** a sleep of Duration::MAX waits as long as the platform's time can say, without overflowing
- **CLOCK-8** a thousand sleeps at once all complete once their deadlines pass

## Spawn

Tasks, run on their own.

- **Implementations:** `kiln::rt::RtSpawn` (kiln's runtime; a task it can't take is refused
  `Full`); `kiln_seams::spawn::SimSpawn`, which runs its tasks only in `run_until_stalled`, one
  poll each in the order they were woken, at most a million polls a run (a task that wakes itself
  forever ends the run at its budget), at most 10,000 tasks held.
- **Used by:** kiln-sim (CP-T6.1), to run kiln's loops on a seeded scheduler.
- **Passed by:** both, every clause.

Methods: `spawn_boxed`.

- **SPAWN-1** a spawned task runs to its end, and its handle gives its output
- **SPAWN-2** a task runs to its end whether or not its handle is kept
- **SPAWN-3** tasks run side by side: two that wait on each other both end
- **SPAWN-4** abort ends a task at its next wait: its handle says Aborted, and what came after never runs
- **SPAWN-5** a task that panics ends alone: its handle says Lost, and other tasks run on
- **SPAWN-6** a thousand tasks all run to their ends

## Records

Every record kiln keeps, by family and key, each at the revision of the commit that last wrote it.

- **Implementations:** Amphora's typed records (CP-T1.5, which takes this suite past 200 cases);
  `kiln_seams::records::SimRecords`: linearizable, snapshot reads from each key's last 32
  versions and the last 100,000 deleted keys' histories (older: refused `Compacted`), a change
  feed of the last 100,000 changes (a read of changes or a wait from before it: refused
  `Compacted`), a million rows. Its `compact` forgets the history before a revision at once,
  as a compaction does; a suite's fixture makes its store do that for RECORDS-14.
- **Used by:** goal stacks, goal items, plans, plan runs and keep-going, once CP-T1.5 moves them
  off rusqlite; the session journal (CP-T3.1).
- **Passed by:** the simulator, every clause; Amphora's, when CP-T1.5 lands it.
- **Bounds:** family names 1 to 64 of `a-z 0-9 _ - .`; keys 1 to 1024 bytes; values to 1 MiB;
  1000 conditions and 1000 writes a transaction; 1000 rows a page or a change read.

Methods: `get`, `scan`, `commit`, `changes`, `wait`, `head`.

- **RECORDS-1** a row written is read back with its value and its commit's revision; a key never written reads as none
- **RECORDS-2** every commit's revision is higher than every one before it, and head() is the newest
- **RECORDS-3** a write conditioned on a row's absence succeeds once; the next is refused Conflict and writes nothing
- **RECORDS-4** a write conditioned on a row's revision (compare-and-set) succeeds at the current one and is refused Conflict at a stale one, naming the current
- **RECORDS-5** a transaction is all or nothing: one failed condition and no write applies, the head unmoved; together, its writes share one revision
- **RECORDS-6** a deleted row reads as none, leaves scans, and satisfies an absence condition
- **RECORDS-7** a scan returns a family's rows in key order, within its prefix, after its cursor, at most its limit, saying when more follow; a limit of 0 is refused
- **RECORDS-8** families are apart: one key in two families is two rows
- **RECORDS-9** a family's change feed gives every change committed after a revision, in commit order, a delete as none, at most its limit
- **RECORDS-10** a wait completes once a change to the family is committed after its revision, at once when one already is
- **RECORDS-11** a snapshot read sees the store as it was just after its revision; one ahead of the head is refused Ahead
- **RECORDS-12** bad family names, empty or long keys, large values and large transactions are refused, typed, and write nothing
- **RECORDS-13** a transaction with no writes commits nothing and returns the head; a key written twice in one commit keeps the last
- **RECORDS-14** after a compaction, snapshots and the feed from its revision on read as before; from before it, a snapshot, a read of changes and a wait are each refused Compacted or answered right, at once

## Bus

Topics of messages, in order, read from any offset kept, each topic of a durability class
(Focal's doc 33: ephemeral, replicated, persisted, windowed).

- **Implementations:** Quiver (CP-T2.1, on Focal's bus); `kiln_seams::bus::SimBus`: in memory
  (ephemeral, whatever class is asked), messages stamped by its clock, 10,000 topics.
- **Used by:** the UI's live feed of session events (CP-T2.1), then CI and CD events, webhooks and
  sends.
- **Passed by:** the simulator, every clause; Quiver, when CP-T2.1 lands it.
- **Bounds:** topic names 1 to 128 of `A-Z a-z 0-9 _ - . /`; payloads to 1 MiB, keys to 1024
  bytes; 1000 messages a read; a topic retains 1 to a million messages.

Methods: `create`, `publish`, `read`, `wait`, `bounds`.

- **BUS-1** a topic's messages get offsets 0, 1, 2… in the order they were published
- **BUS-2** a read from an offset gives the messages from it in publish order, keys and payloads intact, and where to go on
- **BUS-3** every reader reads every message kept: readers don't take them from each other
- **BUS-4** a topic keeps its newest messages up to its retention; a read from before them is refused Behind, naming where they start
- **BUS-5** a wait completes once a message at its offset or later is there, at once when one is
- **BUS-6** a publish to or read of a topic that doesn't exist is refused NoTopic
- **BUS-7** creating a topic again with the same settings changes nothing; with others it's refused Exists; bad names and a retention of 0 are refused
- **BUS-8** a payload over the limit is refused TooLarge and publishes nothing
- **BUS-9** a read gives at most its limit and says where to go on; at the end it's empty; a limit of 0 is refused
- **BUS-10** topics are apart: each has its own messages and offsets

## Blobs

Bytes, with S3's semantics. An object's etag is its content's SHA-256, however it was uploaded.

- **Implementations:** kiln's CAS, any S3, then Mantle (CP-T10.1, which takes this suite past 150
  cases); `kiln_seams::blobs::SimBlobs`: in memory, its presigned URLs signed and timed by its
  clock, objects to 64 MiB, a million of them, a GiB in all (uploads' parts included), 1000
  uploads under way.
- **Used by:** the CAS, sync's blobs, CD's artifacts, recordings, transcripts and upgrade binaries,
  once CP-T10.1 moves them behind it; Reliquary's bytes (CP-T9.1).
- **Passed by:** the simulator, every clause; the local and S3 ones, when CP-T10.1 lands them.
- **Bounds:** keys 1 to 1024 bytes (S3's); 1000 keys a listing (S3's); part numbers 1 to 10,000,
  every part but the last at least 5 MiB (S3's); presigned URLs for up to 7 days (S3's).

Methods: `put`, `get`, `head`, `delete`, `list`, `start_upload`, `upload_part`, `complete_upload`, `abort_upload`, `presign`.

- **BLOBS-1** an object put is got back whole, with its size and its content's SHA-256 as its etag
- **BLOBS-2** a get with a byte range gives those bytes (an end past the object, to its end); a range outside it is refused InvalidRange
- **BLOBS-3** head gives an object's metadata; head or get of a key with no object is refused NotFound
- **BLOBS-4** a put over an object replaces it, and new content has a new etag
- **BLOBS-5** a put If-None-Match creates only where there's no object; else it's refused PreconditionFailed and stores nothing
- **BLOBS-6** a put If-Match replaces only the object with that etag; else it's refused PreconditionFailed
- **BLOBS-7** a deleted object is gone; deleting a key with no object succeeds
- **BLOBS-8** a listing gives a prefix's objects in key order, after its cursor, at most its limit, saying when it's truncated; a limit of 0 is refused
- **BLOBS-9** a put with a SHA-256 its content doesn't have is refused BadChecksum and stores nothing
- **BLOBS-10** a multipart upload's parts, uploaded in any order, are joined in part order into one object
- **BLOBS-11** a multipart upload is refused when a part but the last is under the minimum or parts are named out of order; part numbers run 1 to 10,000
- **BLOBS-12** an aborted upload is gone: completing it is refused NoSuchUpload
- **BLOBS-13** a presigned get works until it expires by the store's clock, then is refused Expired; none lasts more than 7 days
- **BLOBS-14** a presigned put stores what its holder sends; a URL changed after signing is refused
- **BLOBS-15** keys are 1 to 1024 bytes: an empty or longer one is refused Invalid

## Secrets

Secrets by path, after AWS Secrets Manager and Vault: versions, the stages `AWSCURRENT`,
`AWSPREVIOUS` and `AWSPENDING`, scheduled deletes, and leases. A value never shows: its `Debug`
and `Display` say only how long it is, and dropped, its buffer is zeroed (`zeroize`): a copy made
of it is the copy's to clear.

- **Implementations:** Veil (CP-T8.1); `kiln_seams::secrets::SimSecrets`: in memory, timed by its
  clock, 100,000 secrets and leases.
- **Used by:** kiln's deploy and CI secrets (behind the secrets service's `Backend`), once CP-T8.1
  lands Veil.
- **Passed by:** the simulator, every clause; Veil, when CP-T8.1 lands it.
- **Bounds:** names 1 to 512 of `A-Z a-z 0-9 / _ + = . @ -` and values to 64 KiB (Secrets
  Manager's); 100 names a listing; 100 versions kept a secret (the oldest without a stage go
  first); deletes 7 to 30 days out (Secrets Manager's); leases up to a day.

Methods: `create`, `put`, `get`, `promote`, `describe`, `list`, `delete`, `restore`, `lease`, `read_leased`, `renew`, `revoke`.

- **SECRETS-1** a secret created has one version, current, read back as created
- **SECRETS-2** a version put current becomes AWSCURRENT, and the one it replaced AWSPREVIOUS
- **SECRETS-3** a version put pending is read as AWSPENDING and isn't current until promoted; promoted, the old current is previous and the pending stage goes
- **SECRETS-4** a version is read by its id with the value it was made with, however many came after; an unknown one is refused NoSuchVersion
- **SECRETS-5** a value never shows: not in Debug or Display, a describe or a listing
- **SECRETS-6** creating a name that exists is refused Exists, reading one that doesn't NotFound; bad names and values over 64 KiB are refused
- **SECRETS-7** a delete is scheduled 7 to 30 days out: reads are refused Deleted until it's restored, and once the window passes it's gone
- **SECRETS-8** a lease reads the current value until its time passes by the store's clock, then is refused Expired; renewing extends it, revoking ends it
- **SECRETS-9** a listing gives names with a prefix in name order, after its cursor, at most its limit

## Artifacts

OCI repositories, as the Distribution Specification v1.1 has them: blobs by digest, manifests by
tag or digest, kept byte for byte, tags that move, and referrers.

- **Implementations:** Reliquary (CP-T9.1, from kiln's `ci/registry`, passing the OCI conformance
  suite); `kiln_seams::artifacts::SimArtifacts`: in memory, 100,000 repositories, and 100,000
  blobs, manifests and tags in each, a GiB in all.
- **Used by:** kiln's registry, with agents, harnesses, skills and MCP servers checked on push,
  once CP-T9.1 lands Reliquary.
- **Passed by:** the simulator, every clause; Reliquary, when CP-T9.1 lands it.
- **Bounds:** manifests to 4 MiB; names to 255 bytes; 1000 tags a listing.

Methods: `push_blob`, `blob`, `push_manifest`, `manifest`, `tags`, `delete_manifest`, `referrers`.

- **ARTIFACTS-1** a blob pushed is named by the SHA-256 of its bytes and read back by it; one never pushed is refused BLOB_UNKNOWN
- **ARTIFACTS-2** a blob pushed under a digest that isn't its own is refused DIGEST_INVALID and stored nowhere
- **ARTIFACTS-3** a manifest is read back by tag and by digest, byte for byte, with its media type; its digest is its bytes' SHA-256
- **ARTIFACTS-4** a manifest that references a blob or manifest not in the repository is refused MANIFEST_BLOB_UNKNOWN, and its tag isn't set
- **ARTIFACTS-5** a tag moves to the manifest last pushed under it; the one before stays, by its digest
- **ARTIFACTS-6** a repository's tags are listed in order, after a cursor, at most a limit
- **ARTIFACTS-7** a manifest deleted by digest is gone, and every tag on it with it; deleting it again is refused MANIFEST_UNKNOWN
- **ARTIFACTS-8** the referrers of a manifest are the manifests whose subject it is, filtered by artifact type when asked
- **ARTIFACTS-9** names, tags and digests outside the Distribution Specification's grammar are refused NAME_INVALID, TAG_INVALID and DIGEST_INVALID
- **ARTIFACTS-10** a repository sees only what was pushed to it
- **ARTIFACTS-11** a manifest pushed under another digest, not JSON, of another media type than it says, or over 4 MiB is refused

## Sandbox

A harness or a CI leg launched in a boundary (the host, a rootless OCI container, a microVM): its
program, its folder, its limits and the hosts it may reach, its stdio streamed back and its end said
once, last. Its limits are memory and processes, and its shares of the CPU and the disk, on cgroup
v2's weights (1 to 10,000, 100 an even share: a build held below an agent). The suite asks its
fixture for programs by what they do (echo, exit with a code, use memory, start processes,
connect), so each implementation supplies its own.

How a limit ends a run, as Linux's cgroup v2 has it (agreed with Hex's track, CP-T7.1): memory when
the boundary killed it for going over (`memory.events` counts an `oom_kill`); processes when it was
refused one past its limit (`pids.max` fails the fork, the run isn't stopped) and then failed. A run
refused a process that copes and ends well ends with its code.

- **Implementations:** `host` (today's launch), Shards, then Hex's classes: `process`, `oci`,
  `firecracker`, `shards` (CP-T4.1, CP-T7.1, CP-T7.2); `kiln_seams::sandbox::SimSandbox`: built-in
  programs that do those things on the host boundary (a container or a microVM is refused
  `Unsupported`), 1000 runs at once, 10,000 outputs unread a run (past it, the run is killed).
- **Used by:** every harness kiln launches and its CI legs, once CP-T4.1 and CP-T7.1 land them.
- **Passed by:** the simulator, every clause; `host`, Shards and Hex's, when their tracks land them.

Methods: `launch`, `boundaries`.

A launch gives a `Run`: stdin given to it in order, what it says (its output, then its end, once),
and a kill (its end, unless it had one, is `Killed`).

Methods of Run: `stdin`, `close_stdin`, `next`, `kill`.

- **SANDBOX-1** what a run writes reaches its stdout in order, then its end, then nothing
- **SANDBOX-2** stdin reaches the program in order, and closing it is the end of its input
- **SANDBOX-3** a run's stderr and exit code are said as they are, and its end comes once, last
- **SANDBOX-4** a run killed ends Killed, and takes no more input
- **SANDBOX-5** a run over its memory limit is ended OverLimit(Memory); under it, it runs to its end
- **SANDBOX-6** a run refused processes past its process limit, that then fails, ends OverLimit(Processes); under it, it runs to its end
- **SANDBOX-7** a run reaches only the hosts it's granted
- **SANDBOX-8** a launch of a program that doesn't exist is refused NoSuchProgram
- **SANDBOX-9** a run works in the folder it's given
- **SANDBOX-10** cpu and io weights outside cgroup v2's 1 to 10,000 are refused Invalid; at its ends a run goes ahead

## Placement

Where work runs, by the resources it needs and the labels it asks for. A placement never
over-commits a node; one that can't be made is refused with why, in words; releasing frees exactly
what was placed.

- **Implementations:** Hex (CP-T7.1, with priorities, quotas and fair share);
  `kiln_seams::placement::SimPlacement`: on the node with the most memory free, then by name, a
  million placements.
- **Used by:** builds that can't starve a machine, and microVM CI legs (CP-T7.1).
- **Passed by:** the simulator, every clause; Hex, when CP-T7.1 lands it. Every case runs on the
  same two nodes (`kiln_seams::placement::conformance_nodes`).

Methods: `place`, `release`, `nodes`.

- **PLACEMENT-1** work placed is on a node whose use grows by what it needs
- **PLACEMENT-2** no node is ever over-committed: work that doesn't fit is refused Unplaceable, saying why
- **PLACEMENT-3** releasing frees exactly what was placed; releasing what isn't placed is fine
- **PLACEMENT-4** the same id placed again is where it is, and uses nothing more
- **PLACEMENT-5** work goes only on nodes with the labels it asks for; none with them, it's refused
- **PLACEMENT-6** a refusal's why names what's short
- **PLACEMENT-7** small work fills the cluster exactly to its capacity, and is placed again once some is released

## HarnessPort

kiln's agents as the code that drives them sees them: who they are, what their harness takes (its
command catalog), and commands to them. Generic over kiln's types: an agent is kiln's
`harness::session::Info`, a command its `harness::model::Command`.

- **Implementations:** kiln's harness registry (`harness/port.rs`: the hub's own sessions and every
  runner's); `kiln_seams::harness::SimHarness`, which keeps agents in memory and records each
  command it's handed, stamped by its clock: 100,000 agents, what the 100,000 closed last were
  sent, and 100,000 commands an agent (past it, refused `Full`). It keeps nothing for an agent
  it doesn't know (a catalog, a machine away).
- **Used by:** the goal driver and keep-going (`goals/mod.rs`, `goals/keep.rs`); the inbox and the
  dispatcher of sends (CP-T3.2, CP-T3.3).
- **Passed by:** the simulator, every clause. kiln's registry passes HARNESS-1, HARNESS-2 and
  HARNESS-4 (`harness/port.rs`'s test). Its "delivered" means the command is on its way to the
  session's runner; HARNESS-3 and HARNESS-5 (each command reaches its agent once, in order, and
  only it) are CP-T3.2's and CP-T3.3's: sends deduplicated, then acknowledged end to end.

Methods: `agents`, `agent`, `catalog`, `send`.

- **HARNESS-1** an open agent is listed once by agents() and found by agent(id); one never opened is neither
- **HARNESS-2** a command to an agent the port doesn't know is refused NoSession
- **HARNESS-3** commands delivered to an agent reach it in the order they were sent, each once
- **HARNESS-4** a closed agent is no longer known, and a command to it is refused NoSession
- **HARNESS-5** what's sent to one agent never reaches another
