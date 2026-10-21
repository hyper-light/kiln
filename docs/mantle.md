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

Not yet: on 2026-10-21 Mantle's agent was in a long turn of its own work with kiln's message
queued. Its answers go here, by request, as they come. Nothing in CP-T10.1 waits on them: it runs
on kiln's local store and on stock S3.
