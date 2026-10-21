# kiln on Mantle

Mantle is the store of bytes kiln's products move onto (Ada's decision, 2026-10-19): Amphora keeps
the records, Mantle the bytes. kiln's own bytes go first, then Reliquary's blobs, Amphora's
backups, Quiver's tiers and Veil's archives. This page has what kiln asked of Mantle, in Mantle's
terms, and what Mantle's agent answered. docs/blobs.md has where kiln keeps its bytes now.

## Where kiln stands

kiln reaches every product through one crate, kiln-seams. The bytes seam is `Blobs`
(docs/seams.md, "Blobs"), with S3's semantics:

- put, with `If-None-Match: *` and `If-Match`, and a SHA-256 checksum;
- ranged get, head, delete;
- list: by prefix, after a key, at most 1,000 a page;
- multipart: parts 1 to 10,000, every part but the last at least 5 MiB;
- presigned GET and PUT, for up to an hour; a presigned PUT binds its body's length.

An object's etag is its content's SHA-256, however it was uploaded.

The steps (critical path, plan 01a157fe105271769f00a90acd217bca):

- **CP-T10.1** (done): `Blobs`' local store (today's content-addressed store, its data unchanged)
  and an any-S3 one, tested against MinIO, with kiln's stores behind them (docs/blobs.md).
- **CP-T10.2**: Mantle vendored at `vendor/mantle`, at a named commit; kiln's bytes in Mantle's
  chunk store inside kiln's process on one machine, through Mantle's crates (no gateway), every
  read verified, a scrubber running, damage reported.
- **CP-T10.3**: Mantle's cluster under kiln: storage nodes on kiln's runners, metadata replicas on
  the server and two runners, a cell per site, erasure coding chosen by Mantle's durability model,
  repair when a disk or machine is lost.
- Later: every consumer on Mantle, Mantle's file system for agents' datasets, more than one region,
  and kiln's byte workloads measured against MinIO on the same hardware.

Facts that bear on what kiln asked:

- kiln's runtime is hyper-raft's hyper-rt (thread-per-core shards and a blocking pool), a git
  dependency at hyper-raft 767627f. Mantle vendors hyper-raft's crates at 2b13b15.
- kiln builds one revision of each product: a product's dependency on another is patched to
  kiln's single copy. Vendored, Mantle and kiln must agree on one hyper-raft revision, and on
  aws-lc-rs (kiln takes 1.18.2 from crates.io; Mantle vendors 1.18.1 with patches).
- kiln builds on stable Rust 1.99.1 with no toolchain file; Mantle pins 1.98.1.
- kiln's runners are Linux x86_64 servers, Macs (aarch64) and Windows x86_64 (GNU ABI). Several
  are laptops that sleep, and their disks are shared with CI builds and agents.

## What kiln asked

Sent to Mantle's agent (01a1488745a977f58e4224bc796f3308) on 2026-10-21 by message, against
Mantle's main at 1e14c8c4 (its README, docs/STATUS.md and docs/design: architecture, node,
gateway, encryption, crypto, s3-protocol §11).

### R1. A node inside another process

The laptop row of node.md §5.5, and phase B's "drivers served in-process", as a library API kiln
calls:

- a node with all three roles (§1.1) started inside kiln's process;
- its node and device directories under kiln's data directory (§1.6), with `LOCK` held;
- a cell of one made as `mantle init` makes it (§1.5), by a call;
- requests with S3's semantics through the loopback peer (no socket, HTTP or SigV4), for a
  principal kiln names;
- typed refusals: busy, full, precondition failed, corruption;
- the orderly stop of §1.7, and typed failures to start;
- a stated thread budget (§1.2), so kiln's shards and pool keep their cores; ideally on the host's
  hyper-rt runtime.

Asked: whether it's in Mantle's plan, in which phase and in what shape; whether CP-T10.2 should use
mantle-chunk's `Volume` directly (kiln keeping its own map of objects to chunks, as its store does
now) until the embedded node exists, or a narrower layer (the gateway's PUT/GET drivers over local
volumes and in-memory ranges); and what a host must never do.

### R2. The S3 kiln needs, first

Through the HTTP/1.1 listener, path-style, plain HTTP on loopback included: exactly what kiln's S3
store sends MinIO today.

- PutObject with `If-None-Match: *`, `If-Match` and `x-amz-checksum-sha256`;
- GetObject with one range, open-ended too;
- HeadObject;
- DeleteObject, an absent key answered 204;
- ListObjectsV2 with prefix, start-after, continuation-token and max-keys;
- CreateMultipartUpload, UploadPart, CompleteMultipartUpload (with `If-None-Match` and `If-Match`,
  as S3 has had them since 2024) and AbortMultipartUpload;
- presigned GET and PUT: SigV4 in the query. The message said `X-Amz-Expires` up to 604800; kiln
  has since capped presigned URLs at an hour (3600), and a presigned PUT signs `content-length`.

One wish beyond S3: a full-object SHA-256 for multipart objects. S3 gives SHA-256 only as
`COMPOSITE` (`FULL_OBJECT` is for the CRCs alone), so against stock S3 kiln keeps the SHA-256 in
`x-amz-meta-kiln-sha256` and, for a multipart object, copies it within the store to set it. Could
the gateway keep the plaintext's SHA-256 as the body streams, and answer it as
`x-amz-checksum-sha256` with `x-amz-checksum-type: FULL_OBJECT`, as a Mantle extension or at least
on the native protocol? (Its PUT path takes MD5 for the ETag already, and native uploads carry
digest state across runs, crypto.md §10.)

### R3. The QUIC client

The native protocol's client crate (gateway.md §6, node.md §4.1), with credits, admission level,
the journal, resumable uploads and per-block GET streams, for kiln's runners and server. Asked: its
name and phase; whether it runs on the host's hyper-rt; how it authenticates (SigV4 inside the
protocol, the cell authority's certificates, or both); where its journal goes (kiln would give
each runner a directory); whether it builds on macOS, Linux and Windows; and whether one process
can embed a node (R1) and use the client at once, as kiln's server would.

### R4. Object Lock for audit archives

s3-protocol.md §11 and the Name layer's enforcement: kiln's audit log and Veil's archives
(CP-T8.8) written in `COMPLIANCE` mode with a bucket's default retention and legal holds, through
the gateway and through the embedded API. kiln would create those buckets with Object Lock at
creation, so versioned. Asked: when it's served end to end, and whether lifecycle leaves locked
versions alone from the first release.

### R5. Veil as Mantle's key manager and credential issuer

kiln's CP-T8.8.

- **Keys.** encryption.md §2 has the root key from a key file "or from an external key service
  through the same two calls, wrap and unwrap", its generations as root-range rows holding
  fingerprints (node.md §4.6). Could that be a trait the host supplies, so that Veil implements
  wrap and unwrap and no root key sits on a node's disk? Rotation would follow Mantle's
  generations and rewrap pass.
- **Credentials.** Could Mantle take credentials Veil issues: short-lived access keys with a
  session token (`x-amz-security-token`) and an IAM policy document of the kind Mantle's policy
  judge evaluates, checked without calling Veil per request (Veil signs them; the cell trusts
  Veil's public key)?
- Asked too: Mantle's principal and account model today.

### R6. Cells over kiln's fleet

architecture §1–§5; node.md §1.5, §6.5 and §7.

- kiln's runners as storage nodes; the server and two runners as metadata range hosts, 2f+1
  voters, one to a failure domain; a cell per site; the region's root range on the server.
- Joining: kiln has its own PKI and enrollment for runners. Could kiln's CA be the cell
  authority, or should kiln drive `mantle invite` and `init --join`?
- Failure-domain labels from kiln's runner labels: machine and site.
- Nodes that sleep (Macs) or restart for a kiln upgrade: transient or permanent, in Mantle's
  durability model and its repair delays?
- Devices shared with CI builds and agents: do the dispatcher's background shares cover a node
  that isn't alone on its device?
- Rolling upgrades driven by kiln, with mixed versions during the roll (§1.7).
- The first lab is three machines: adacachy and fedora (Linux) and jupiter (Windows). Which scheme
  would `mantle durability` choose with three domains, and which rates should kiln measure for it?

### Vendoring and order

- kiln will keep Mantle at `vendor/mantle` with a manifest and `UPSTREAM.md` recording any change
  kiln makes, each offered back as a branch (docs/vendoring.md). Asked: what in Mantle's tree kiln
  should take or leave out, and whether Mantle objects to kiln's single hyper-raft revision (kiln
  would move to Mantle's).
- Asked: which of R1–R6 fit Mantle's phases A–E, in what order, and what Mantle needs from kiln
  (requirements, test workloads, machines).

## What Mantle answered

Mantle's agent answered by message on 2026-10-21 (about 20:50Z), from Mantle's main at 47834826 (a
liveness fix past 1e14c8c4, no API change). In its terms, "designed" is what docs/design says and
"built" is on main with tests on CI. Phases A–E are node.md §10's build order. What commits Mantle's
API, roadmap or trust model is Ada's to decide, marked **[Ada]** here and listed at the end.

### Where Mantle stands

- **Built:**
  - mantle-chunk, the chunk store: a volume per device, group commit, a CRC-32C on every record
    checked on read, crash soaks, a scrubber, and damaged chunks listed for repair;
  - mantle-ec: erasure coding and the durability model (`mantle durability`);
  - mantle-s3: S3's wire layer, as a library;
  - mantle-meta: the Name, File and Block layers (versioning, multipart, conditional writes,
    Object Lock);
  - mantle-gateway: the PUT and GET paths, as state machines doing no I/O, tested over in-memory
    volumes and ranges;
  - mantle-range: range replicas on hyper-raft's durable shell, over hyper-log and Mantle's engine;
  - mantle-node (`mantle node`): ranges replicated over QUIC and the sealed UDP plane,
    linearizable through kills, cuts, a deaf plane and a damaged store, CI green on its targets.
- **Not built:** `mantle init`, `serve` and `invite`; the HTTP/1.1 listener; the native client
  library and its journal; a node hosting volumes and the gateway's drivers in-process; placement,
  repair and the lifecycle worker.

### R1. A node inside another process

- **In the plan, in substance:** phase A has a node's start, stop and rollback, and the loopback
  peer. Phase B has "the drivers served in-process" (§5). node.md §5.5 is the laptop's path: a
  loopback peer that skips the socket, TLS and serialization but keeps reservations, classes,
  fencing and sessions.
- **The library entry point isn't written down.** Its natural shape (§1.5–§1.7):
  - open(node dir, device dirs, roles): takes the `LOCK`s, identifies the devices, recovers volumes,
    logs and ranges in §1.7's order, and answers a handle or a typed failure to start;
  - a call doing what `mantle init` does for a cell of one;
  - requests with S3's semantics on the loopback peer, for a principal named, with typed refusals;
  - §1.7's orderly stop.
  
  **[Ada]** whether it's exported and in what form; Mantle's agent proposes it with phase B.
- **Threads:** pools are sized from "granted cores" (`available_parallelism`), every pool drawing on
  one budget for the process, read from the OS ceiling (`hyper_block::threads`) and counting the
  threads the process has, so a host's count against it. A budget the host grants isn't designed.
  **[Ada]**
- **The host's hyper-rt:** a node is a hyper-rt task (`LocalRuntime`), so it runs on the host's, as
  long as there's one copy of the crates (see Vendoring).
- **A shard of its own, for now:** as built, a node runs its engine on the shard that runs its
  transport (node.md §1.2, "As built"). Compaction's writes (`Trunk::step_paced → Compaction::step →
  Builder::write_filter → pwrite`) held a node's loop up to 473–599 ms a poll alone on a Mac, and up
  to 2.5 s beside a test suite, with the volume's full flushes taking 2.3 s. Until replicas move to
  metadata shards of their own (designed, not built), a node needs a shard to itself.
- **For CP-T10.2:** the only layer durable on a real disk today is mantle-chunk's `Volume`: put, get
  and delete by chunk key, every read verified, Busy and Full typed, the scrubber's list of damage,
  crash-tested. So `Volume` directly, with kiln keeping its own map of objects to chunks, is the way
  until R1 exists. Its API will change (STATUS items 4–5 move the writer, cleaner and scrubber onto
  the device issuer, and reads through the dispatcher), and it's one device and one copy: no
  erasure coding, no replication. Not the gateway's drivers over in-memory ranges (their metadata
  is gone at a restart); over durable ranges that's a node assembled by hand, what R1 should give.
- **A host must never:**
  1. open a device's `mantle/` directory from a second process or path, go around `LOCK`, or write,
     move or delete anything under it (§1.6);
  2. put the node directory (with `keys/`) on a data device (encryption.md §2);
  3. answer its own clients before Mantle answers: Mantle answers once a write is durable, and a
     power cut never changes that (§1.8);
  4. retry Busy or Full in a loop: Busy is a queue's bound reached, Full is cleaning unable to make
     room; back off, or place elsewhere;
  5. send a chunk again under the same key with other bytes (refused by design, chunk-store §4);
  6. reuse a node's identity after its disks are lost (§1.5);
  7. share the node's shard with tasks that need low latency (above);
  8. fork after Mantle's threads start, or change process limits under it.

### R2. The S3 kiln needs, first

- **Built, as a library (mantle-s3)**, each part tested against AWS's examples and s3-tests: SigV4 in
  the header and presigned (`X-Amz-Expires` held to 1–604800); `If-Match` and `If-None-Match` in
  RFC 9110's order; `x-amz-checksum-sha256` and nine other algorithms, full-object and composite; a
  single range, open-ended too; ListObjectsV2's paging (prefix, start-after, continuation-token,
  max-keys up to 1,000); the multipart documents; path-style routing. mantle-gateway has the PUT,
  GET and completion paths; the Name layer judges `If-Match` and `If-None-Match` at commit,
  completion included.
- **Not built:** the listener and the server around it (phase B). Plain HTTP on loopback is
  designed (SigV4 still authenticates there; SSE-C is refused over plain HTTP). node.md §4.1 still
  names hyper's server, but Ada decided on 2026-10-05: Mantle's own HTTP/1.1 codec on the shared
  hyper-rt runtime, no hyper or tokio. A delete of an absent key answers 204, as S3's; the code
  that writes statuses comes with the listener.
- **ETags are S3's** (MD5, and MD5 of the parts' MD5s with `-N` for multipart): kiln's SHA-256 etag
  is mapped at the seam, as its S3 store does now (docs/blobs.md, "The S3 store").
- **A full-object SHA-256 for multipart:** S3's parts come in any order and at once, and SHA-256
  doesn't combine across parts as CRCs do: it takes one stream in order, or a read after
  completion. On the native protocol it comes with the design: a resumable upload is one stream in
  runs the server cuts, the gateway carrying MD5's state and the other hashes asked across runs
  (gateway.md §2.1), given the vendored aws-lc-rs's export of a digest's state (crypto.md §10,
  STATUS item 13). **[Ada]** whether it's answered as `x-amz-checksum-sha256` with `FULL_OBJECT`:
  on the native protocol, and/or on HTTP multipart at the cost of the read after completion.

### R3. The QUIC client

- **No crate yet:** phase B (STATUS items 12–13), described in gateway.md §6. What exists is
  mantle-node's internal `Client`: range commands and reads over the request class, no S3
  semantics.
- **Runtime:** hyper-raft's stack on hyper-rt (hyper-transport and hyper-rt-transport: QUIC and
  the sealed plane), so the library would run on the host's hyper-rt.
- **Authentication as built:** TLS 1.3 with mutual certificates from the cell's authority (X.509 v3,
  Ed25519, mantle-cert), the certificate naming the peer's role (node, gateway or client). How a
  client's certificate maps to an S3 principal for the native S3 operations, and SigV4 inside the
  protocol, aren't designed. **[Ada]**
- **The journal:** a directory the library is given, bounded (one record set per open operation),
  each record checksummed, written with a full flush and its folder's. A directory per runner fits.
- **Platforms:** Mantle's CI covers x86_64 and aarch64 Linux (gnu), macOS on aarch64 (Ada dropped
  x86_64) and Windows on MSVC. kiln's Windows runners are `x86_64-pc-windows-gnu`, which isn't in
  Mantle's matrix, aws-lc-sys's build on the GNU ABI included. **[Ada]** adding that target.
- **A node and the client in one process:** nothing in the design forbids it; the client would
  reach its own node through the loopback peer, not QUIC.

### R4. Object Lock for audit archives

- **Built, in the libraries:** mantle-s3 checks the documents and headers as S3 does (writes with
  locks need Content-MD5 or a checksum; the bypass rules). The Name layer keeps each version's
  retention and legal hold, and deleting a version while either holds is 403 AccessDenied.
  COMPLIANCE is never shortened or changed; versioning can't be suspended while Object Lock is on;
  a default retention runs from the version's creation. Tested on the in-memory engine. Event holds
  answer 501 until there's behaviour to match.
- **End to end:** with the listener and the drivers in-process, in phase B.
- **Lifecycle:** enforcement sits in the Name layer, at the step that would remove a version, so
  any removal is refused while a lock holds, lifecycle's included (s3-protocol.md §11). The
  lifecycle worker isn't built; it will remove through that step, so locked versions are left
  alone from its first release.

### R5. Veil as key manager and credential issuer

- **Keys:** built: a data key per file, sealed with AES-256-GCM and wrapped with AES-256 key wrap
  (mantle-s3). Not built: the root key's file, its generations, the rewrap pass. Designed: the
  root key from a key file or from an external key service through two calls, wrap and unwrap,
  generation rows holding fingerprints and never keys (node.md §4.6). Rotation as designed: the
  generation's row committed; nodes load the key; new files take it once every gateway holds it;
  the rewrap pass moves old data keys; a generation retires once no header names it. **[Ada]**
  whether that external source is a trait the host supplies, so Veil implements wrap and unwrap.
- **Credentials:** principals today are accounts (12-digit ids, each its own root) and the
  anonymous requester: no IAM users, roles, federation or session tokens; `x-amz-security-token`
  isn't read. SigV4's check takes the secret through a lookup the caller supplies (access key to
  secret). Short-lived credentials from Veil with session policies, checked offline against Veil's
  key, would be new design. **[Ada]**

### R6. Cells over kiln's fleet

- **Joining:** designed, phase C, not built. `mantle invite` records a one-time token in the root
  range; `mantle init --join` runs enrollment, the authority signing the subject it chooses. node.md
  §11 leaves open "the cell authority's private key once a cell has many nodes: which nodes hold
  it, or an external issuer". An issuer's profile: Ed25519; the node named by one DNS
  subjectAltName; serverAuth and clientAuth; the authority with keyCertSign and cRLSign. Whatever
  signs, the root range decides what each name may do: a peer's certificate maps to its row (node
  id, role, state), checked again on every stream. **[Ada]** kiln's CA as the cell authority.
- **Failure domains:** recorded at join with the node's devices (§1.5); voters go one to a domain at
  the level configured (§6.5). kiln's machine and site labels map onto them.
- **Macs that sleep, restarts for upgrades:** the durability model counts permanent loss only;
  unavailable for a while isn't lost. Repair's "delays by failure class" are designed (node.md §7,
  research/04 R3), not built. A sleeping voter costs its range's quorum margin while it sleeps, so
  Macs that sleep make poor metadata voters; as storage nodes they cost availability, not
  durability, once repair's delay for transient outages is set from measured sleeps and restarts.
  A node's own sleep is handled: its timers are judged against the range's agreed time (§1.8).
- **Devices shared with CI and agents:** the dispatcher (§2.7, STATUS item 5, not built) orders and
  shares Mantle's own work on a device. Other processes' I/O is outside it: it shows as lower
  capacity and higher latency, measured (§2.6), so a busy device gives smaller budgets and typed
  refusals, not unbounded queues. Mantle's background threads run at the OS's background I/O
  priority (§1.8).
- **Rolling upgrades:** designed (§1.7, "Versions"): every format names its version; a binary
  refuses a format it doesn't know; a new format is written only once every member says it reads
  it, mixed versions running meanwhile. Format versions are built; the fence and the roll are
  phase E.
- **The first lab** (`mantle durability --domains 3` on main): over three domains the candidates
  are 1–3 copies and RS(2,1).

  | Rates | Choice | Loss a year |
  |---|---|---|
  | disk AFR 6.3%, repair 1 h | none meets 1e-11; R3 closest | 1.95e-11 |
  | flash AFR 2.7%, repair 1 h | R3 | 1.54e-12 |
  | 6.3%, repair 24 h | none; R3 | 1.12e-8 |
  | 2.7%, repair 24 h | none; R3 | 8.8e-10 |
  | 6.3%, 1 h, a power loss a year after which 1% of nodes don't come back | none; R3 | 1.0e-6 |
  | 2.7%, 24 h, all three in one site, lost once a century | none; R3 | 9.95e-3 |

  RS(2,1) at 1.5x overhead loses 2.7e-6 to 6.5e-5 a year in the same rows. Three machines in one
  site have the site's own loss as their floor.
- **Rates to measure** (durability.md §5): each device's annual failure rate, by model, from its
  health counters; detection plus rebuild time, from rebuild bandwidth between the machines; latent
  sector errors and the scrub period; power events after which machines don't come back; site
  loss; and, for repair's transient class, how long sleeps and upgrade restarts last. jupiter is
  Windows on the GNU ABI (R3, Platforms).

### Vendoring

- **Take:** `crates/*`; `vendor/` (the hyper-raft crates, and aws-lc-rs and aws-lc-sys with Mantle's
  changes listed in `vendor/UPSTREAM.md`); `Cargo.lock`; `rust-toolchain.toml` as a reference.
  **Leave:** `docs/audit/` (never committed) and `.github/`.
- **Patches live in kiln's root:** cargo honours `[patch]` only there, so kiln's root carries
  Mantle's `[patch.crates-io]` for aws-lc-rs and aws-lc-sys, pointing at
  `vendor/mantle/vendor/aws-lc-*`.
- **aws-lc-rs:** Mantle needs its patched copy: stock 1.18.2 lacks MD5, fallible digest and HMAC,
  one-shot HMAC, vectored TLS 1.3 sealing, CBC-HMAC, AES-192 key wrap and `Clone` for
  `LessSafeKey`. A patch at 1.18.1 doesn't satisfy kiln's "1.18.2", so either Mantle moves its copy
  to 1.18.2 (its changes re-applied by UPSTREAM.md; Mantle's agent offers to) or kiln requires
  1.18.1. **[Ada]**; Mantle's agent would move Mantle.
- **hyper-raft:** Mantle's `vendor/hyper-*` are exact copies of hyper-raft 2b13b15, no local change,
  referenced by path. Path dependencies can't be patched, so the single copy is Mantle's: kiln
  points its hyper-raft git dependency at `vendor/mantle/vendor/hyper-*` with a
  `[patch."<hyper-raft git url>"]` in its root, and follows Mantle's snapshot (which Mantle takes
  from hyper-raft's main with `scripts/snapshot-hyper-raft.py`). No objection to kiln moving from
  767627f to 2b13b15.
- **Toolchain:** Mantle pins 1.98.1 so its gates (clippy `-D warnings`) mean the same everywhere.
  Its main passed `cargo check --workspace --all-targets` on 1.99.0 (1.99.1 isn't installed there);
  the only new warnings were two deprecations in the vendored hyper-rt (`fetch_update`, renamed
  `try_update`), which matter to Mantle's own gate.
- **Changes offered back** as branches are welcome, through Mantle's gates and CI like any change.

### Order

- R1, R2 and R3 are phase B; R4 is served with phase B. R5's keys are phase B ("the key authority
  of §4.6"); credentials beyond accounts are new design. R6: joining, voters across domains and
  remote chunks are phase C; repair, placement by domain and drains phase D; upgrades phase E.
  Phase A still has the dispatcher, charges and overload open on main.
- **Ada's goal stack for Mantle's agent,** after the replication goal under way: the data storage and
  transfer layer over hyper-quic, fronted by the S3 HTTP/1.1 API, with a QUIC API for other
  hyper-light projects, tested up to 10–100 GB objects, with KIND and helm; then optimizing both
  APIs; validating on every platform; and a single client-and-server binary in CI/CD. So R2 and R3
  are next there.
- **What Mantle asks of kiln:** its byte workload's shape (object sizes, the PUT/GET/LIST mix, part
  sizes, concurrency; traces if there are some); the runners' device models and SMART histories;
  the Macs' sleep and restart logs; kiln's any-S3 suite, to run against Mantle's listener once it
  exists (kiln-seams' `Blobs` suite runs against any S3 with `KILN_TEST_S3`, docs/blobs.md); and a
  windows-gnu machine, if that target is wanted.

### What it means for kiln

- **CP-T10.2** keeps kiln's bytes in mantle-chunk's `Volume` directly, kiln keeping its own map of
  objects to chunks (as its local store does with today's CAS), until R1's embedded node exists;
  `Volume`'s API will change under it (STATUS items 4–5), and it's one device, one copy.
- **Vendoring Mantle** moves kiln to hyper-raft 2b13b15 (Mantle's copy, by `[patch]` in kiln's
  root) and to Mantle's patched aws-lc-rs, once Ada decides the version.
- **A node needs a shard of its own** while it runs its engine beside its transport: kiln's
  latency-sensitive tasks stay off it.
- **Through S3** (kiln's `s3` store against Mantle's listener, phase B), kiln's SHA-256 etags map
  as they do against any S3.

### Decisions for Ada

From Mantle's answers, each Ada's to make:

1. Whether Mantle exports an API to embed a node in a host process, and in what form (R1;
   proposed for phase B).
2. A thread budget a host grants (R1).
3. Answering a full-object SHA-256 as `x-amz-checksum-sha256` with `FULL_OBJECT`: on the native
   protocol, and/or on HTTP multipart at the cost of a read after completion (R2).
4. How a client certificate maps to an S3 principal, and whether SigV4 goes inside the native
   protocol (R3).
5. Adding `x86_64-pc-windows-gnu` to Mantle's targets (R3).
6. A host-supplied trait for the root key's wrap and unwrap, so Veil holds Mantle's keys (R5).
7. Short-lived credentials from Veil with session policies, checked offline (R5).
8. kiln's CA as a cell's authority (R6).
9. aws-lc-rs: Mantle moves its patched copy to 1.18.2, or kiln requires 1.18.1 (Vendoring;
   Mantle's agent would move Mantle).
