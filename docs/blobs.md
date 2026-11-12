# Where kiln keeps its bytes

kiln keeps its records (runs, sessions, settings) in its stores of records, and its bytes in one
store of bytes: kiln-seams' `Blobs` (docs/seams.md, "Blobs"), S3's semantics with a content's
SHA-256 as its etag. Three stores implement it, and every byte kiln keeps on its server goes through
the one `[blobs]` names:

- **local** (the default): the server's data directory. What kiln kept before it had this store
  is read where it lies, unchanged.
- **s3**: any S3-compatible store (AWS S3, MinIO and the rest), so a lost server disk loses
  nothing kiln keeps there.

- **mantle** (CP-T10.2): the vendored `mantle-client` over native QUIC to a Mantle cell, using a
  client certificate issued by that cell. docs/mantle.md has the integration and its boundaries.

## Configuring it

```toml
[blobs]
# "local" (the server's data directory), "s3" or "mantle".
store = "local"
# The largest object kept, in bytes (64 GiB).
object_max = 68719476736

[blobs.s3]
endpoint = "https://s3.us-east-1.amazonaws.com"   # or http://127.0.0.1:9000 for a MinIO
region = "us-east-1"
bucket = "kiln"
# Where kiln's objects go in the bucket: at most 60 bytes.
prefix = "kiln"
# Path-style addressing (<endpoint>/<bucket>/<key>), as MinIO wants it; false: virtual-hosted.
path_style = true
access_key_id = "AKIA..."
# A file holding the secret key: never the secret in kiln.toml.
secret_access_key_file = "/etc/kiln/s3-secret"
# A session token, for temporary credentials.
# session_token_file = "/etc/kiln/s3-token"
# PEM certificates for an HTTPS endpoint the public roots don't vouch for.
# ca_file = "/etc/kiln/s3-ca.pem"
# Make the bucket at start when it isn't there.
create_bucket = false

[blobs.mantle]
# The directory `mantle init client <dir> --cell <the cell's directory>` made: its config, the
# client's certificate and key, and the cell authority's certificate.
client = "/etc/kiln/mantle-client"
# Where the first node of that config listens now (host:port), if it moved; the others stay.
endpoint = ""
bucket = "kiln"
# kiln's keys go under it: at most 60 bytes.
prefix = "kiln"
# Make the bucket at start when it isn't there.
create_bucket = false
# How long a request has, every exchange it needs included: 1 to 3600 seconds.
timeout_s = 120
```

Without `access_key_id` and `secret_access_key_file`, the S3 store takes `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN` from the server's environment. A bad
`[blobs.s3]` (no endpoint, no credentials, a bucket that isn't a bucket's name, a prefix past 60
bytes) stops the server at start, saying which.

## The keys kiln keeps

| Keys | What | Written by | Let go |
|---|---|---|---|
| `cas/<sha256>` | the content-addressed store's objects: CI caches, artifacts, registry blobs, release assets, test reports; what CD deploys is read here | `ci::services::cas` | an hour unused once nothing refers to it (`cas_index`, the CI tables) |
| `sync/<sha256>` | the server's copy of the contents synced repos' steps name | `sync::contents` | a week after no repo's tree names it |
| `binaries/kiln-<target>`, `binaries/kiln-<target>.json` | runner upgrade binaries for other platforms, and what kiln knows of each | `updates` | when it's removed (Settings → Updates) or replaced |
| `recordings/<session>/<recording>/<file>` | live pages' recordings (video, log, test), copied from their machine when they end | `harness::streams` | when the conversation closes |
| `transcripts/<session>/<source>/<first>-<last>.jsonl.br` | a piece of a closed conversation's own record (Claude Code's transcript, Codex's rollouts): lines `first` to `last`, brotli, written once and never replaced | `harness::archive` | `[conversations] keep_archived_days` (365) after its last close; the conversation store lets no record go before it's archived, or given up 30 days past its own keeping |

Keys are 1 to 960 bytes of UTF-8, in `/`-separated segments of 1 to 254 bytes, with no `.` or
`..` segment, no segment ending in a space, no control characters and no backslash: the same keys
on every store.

## The local store

`kiln::blobs::local::LocalBlobs`, in `<data>/cas` and `kiln.db`.

- **Objects** are the content-addressed store's: content cut into chunks by FastCDC
  (`kiln_ci::store::cdc`), each kept once under its SHA-256 at
  `<data>/cas/chunks/<2 hex>/<2 hex>/<digest>`, an object the list of its chunks (`cas_objects`).
  Two keys with the same content share one object, and a new version of a large object shares
  every chunk the change didn't touch. A key is a row (`blob_keys`) naming its object by its
  content's SHA-256; a condition (`If-None-Match`, `If-Match`) is judged and the key written in
  one transaction.
- **Durability:** a new chunk is flushed to the disk before it's named, and its folder after; a key
  is written in a transaction once its chunks are. A read of a whole object is checked against its
  SHA-256: a get of one that doesn't hash to its name is refused, and a read streamed in pieces
  ends before its last piece, so its reader gets fewer bytes than it was promised, never all of
  them.
- **Uploads in parts** are cut into chunks as each part arrives (`blob_uploads`, `blob_parts`); the
  object's SHA-256 is taken as the parts come in order, or read back once at completion.
- **Letting go:** an object no key holds stays an hour, so a read under way finishes; then it goes,
  and its chunks once no object or part uses them. Uploads left two days go. The server does this
  every hour.
- **What kiln kept before** stays where it was. The content-addressed store's objects are the local
  store's at `cas/<digest>` (the migration that made the tables keyed them: rows, no bytes moved).
  Sync's contents (`<data>/sync/blobs/<2 hex>/<hash>`) are taken in where they lie the first time
  the server starts (a `.taken` mark says when), and runner binaries (`<data>/binaries`) each time
  it starts: their keys name those files, read as they are, until they're replaced or let go. New
  writes go to the chunks.

### Presigned URLs

The local and Mantle stores' presigned URLs lead to the server's
`/api/blobs/<key>?expires=…&sig=…` (and `&length=…` for a put). The URL is the only credential, so
it's held to what it says:

- **What it signs:** its method, its exact key, its expiry and, for a put, the length of the body
  it takes. HMAC-SHA256 under the server's own key (`<data>/blobs.key`, made once, readable by the
  server alone).
- **How long:** at most an hour (every store's bound), however its expiry was written.
- **The check:** in constant time, before anything is read or written.
- **What it reaches:** the one object it names. No listing, no prefixes, and a key with a `.` or
  `..` segment is refused as any bad key is. A put's body must be exactly the length signed: one
  longer or shorter is refused and nothing is kept.
- **Refusals look alike:** a tampered, expired, other-method, other-key or other-length URL, and a
  URL for an object that isn't there, all get one answer: `403` "this URL grants nothing here".

An S3 store's presigned URLs lead to the S3 itself, which checks them (the length signed as
`content-length`).

## The S3 store

`kiln::blobs::s3::S3Blobs`: S3's REST API, signed with Signature Version 4, over kiln's HTTP
client.

- **Where:** an object at key `k` is `<prefix>/o/<k> `, a space after the key. A store that keeps
  keys as paths can't list an object and another under it: MinIO keeps `p/x` but leaves it out of
  every listing once `p` is there (checked 2026-10-21, RELEASE.2026-09-22, with boto3 as with
  kiln). With a space after each key, `p ` and `p/x ` are apart, and as a space is the lowest
  character a key may hold, S3's order of the keys is kiln's.
- **Etags:** S3's ETag is an MD5 (or a digest of parts' MD5s), not the content's SHA-256. kiln keeps
  the SHA-256 with each object (`x-amz-meta-kiln-sha256`) and sends it as `x-amz-checksum-sha256`,
  so S3 checks what it got. `If-Match` is judged against head's SHA-256 and sent on as S3's
  `If-Match` of the ETag head saw, so a write between the two is refused, not lost.
- **Uploads in parts** go to an upload at `<prefix>/u/<id>`. Completing joins them there, takes
  the SHA-256 (as the parts came, or by reading the joined object back once) and copies it into
  place with it within S3 (CopyObject, or UploadPartCopy past 5 GiB), so the key appears once,
  whole. An abort asks S3 for the upload's parts first: MinIO answers an abort of an upload that
  isn't there `204`, as if it were.
- **Objects from elsewhere** (written by a presigned put or another S3 client) carry no SHA-256:
  the first head or get reads one whole, once, and keeps its SHA-256 with it.
- **Time:** head, get and list say S3's Last-Modified, to the second.
- **Retries:** `503 SlowDown` is sent again, three times at most, a backoff apart, as is a
  conditional write S3 answers `409`; anything else is `Unavailable`, never guessed.
- **Upkeep:** uploads begun two days ago, and what a crash left joined, go every hour.

## The Mantle store

`kiln::blobs::mantle::MantleBlobs` (CP-T10.2): Mantle's native object protocol over QUIC, through
the vendored `mantle-client`, on hyper-rt. kiln presents the client certificate the cell's
authority issued it (`mantle init client`) and takes only nodes that authority certified.

- **Where:** an object at key `k` is a pointer at `<prefix>/o/<k>`: a small JSON document naming
  its content's SHA-256, its size and the native ETag of its bytes, which are kept once, at
  `<prefix>/d/<sha256>`, written only where nothing is. Mantle keeps no metadata kiln could set
  and has no CopyObject or UploadPartCopy (docs/mantle.md), so the SHA-256 lives in the pointer.
- **Etags:** the seam's etag is the SHA-256 the pointer names. `If-None-Match` writes the pointer
  only where there's none; `If-Match` is judged against the pointer's SHA-256, and the pointer is
  then written on Mantle's `If-Match` of the native ETag read, so a write between the two is
  refused, not lost.
- **Writes:** up to 5 MiB in memory goes in one put, its SHA-256 stated for Mantle to check before
  anything commits. More, and a completion's parts (read from the cell as they go), go as Mantle's
  native upload, held to a CRC-64/NVME: each run (8 MiB at the most) read whole before it's sent,
  so no exchange waits on its source. Bytes kept under their SHA-256 already are read back and
  hashed before a pointer is made to them.
- **Reads:** a whole get is hashed against the pointer's SHA-256; a range is given as it is while
  the bytes' native ETag is the pointer's, else only once the whole object hashes to it. Bytes
  that don't are refused `BadChecksum`.
- **Asked again:** mantle-client asks again only what no node can have taken, and what's slowed
  a few times; the rest is the caller's. Mantle judges a put asked again afresh (at 87ab09e1, as at 17eb8032, it
  knows a write again by the file its gateway wrote, not by the client's identity), so a put whose
  answer was lost on the way is checked against its key before it's sent again: a document (a
  pointer, an upload's record) carries its write's own id, bytes their SHA-256, and a write the
  cell applied isn't sent again, so it leaves no second version. One it didn't apply is sent again,
  at once the first time; a write refused for its condition was still done where the key holds
  it. A read, a delete (`NotFound` counts as done) or a request the client's transport had no room
  for, or the cell slowed, is asked again after a pause that doubles from a quarter of a second to
  eight, until its deadline. A native upload given up is resumed by its identity from where the
  cell holds it, and a Finish asked again once the upload is gone is answered with the version it
  made; where the cell has no such upload, the key's bytes say whether it finished, else it begins
  again as a new one. Bytes kept under their SHA-256 already aren't sent again, and an upload the
  cell refused (its condition, say) is aborted, so none is left open. A test build's `mantle_lost`
  (chaos) loses the cell's first answer to each mutation after the cell applied it.
- **Uploads in parts** are kept in Mantle at `<prefix>/u/<id>/` (the upload, and each part's bytes
  and SHA-256), so they outlive kiln's restart. Completing writes the parts it names into the
  upload first, checks each part's bytes against its SHA-256 as it takes the whole's, then streams
  the parts, 8 MiB at a time, into the content's place before the pointer is written: nothing is
  spooled on kiln's disk. Completing or aborting removes the upload's keys.
- **At once:** requests go on one shard, through one client a node of the cell (two at least, four
  at the most), each first asking a node of its own. Those that come together are asked at once,
  one through each client (a caller waits for each answer before it asks again, so they're
  concurrent ones); a completion, one writing as the next reads its parts, and a listing, its keys'
  pointers read through all of them (Mantle's listing says each pointer's own size, not its
  object's), go alone. A node at 87ab09e1, as at 17eb8032, admits two native clients at once and runs two
  operations at once, so one a node takes half of each.
  64 requests more wait at most, and one past them is refused `Full`. Each is answered within
  `timeout_s`, every exchange it needs included. A cell that doesn't answer is `Unavailable`, with
  Mantle's status, and never keeps the server from listening.
- **Time:** head, get and list say the pointer's Last-Modified, to the second.
- **Not yet:** bytes no pointer names any more (deleted or replaced), and uploads never completed
  or aborted, stay in the cell: nothing collects them yet (T10's).
- **Custody (W27-R08):** the server's store is wrapped as every store is
  (`custody::blobs::Attested`), its records naming `kiln://<organization>/blobs/mantle`. Bytes
  changed in the cell are refused by the adapter's own hash before the wrapper sees them, and the
  wrapper still records the break (or audits the refusal, for a key that names its content).

`[blobs.mantle] client` names the directory `mantle init client` made: it holds the client's key,
so it's the server user's alone. A missing or malformed one refuses kiln's configuration at start,
its keys never printed. `endpoint` replaces where the first node is, not who the nodes are.

`crates/kiln/tests/mantle_blobs.rs` makes a cell of three founders on loopback, each its own
failure domain with its own volume, from the `mantle` executable `KILN_TEST_MANTLE_BIN` names
(Mantle's release build: `cargo build --release -p mantle` in `vendor/mantle`), and runs
kiln-seams' Blobs suite against this store there, then W27-R08's put, read and changed object:
`cargo test -p kiln --test mantle_blobs -- --ignored` (its cell under `TMPDIR`). With
`--features chaos` it also runs the suite with each mutation's first answer lost.

## Moving a server to S3

With the server stopped:

```console
$ kiln blobs copy        # every object of the local store into [blobs.s3], each checked
$ kiln config set blobs.store s3
```

then start it. `kiln blobs copy` takes in what an older kiln kept as files first, copies each
object (large ones in parts), checks each copy's SHA-256, and skips what the S3 store holds
already, so a copy cut short is finished by running it again. The local store's files stay where
they are until you remove them.

## Rolling back

A kiln from before these stores can run on the same data directory again (a rollback), and this
release after it, with nothing a key holds lost:

- **Before going back,** take `[blobs]` out of `kiln.toml`, and `keep_archived_days` out of its
  `[conversations]`, if they're there: an older kiln refuses a section or a setting it doesn't know
  (`kiln install` writes both into a `kiln.toml` it makes). A server moved to the S3 store doesn't
  go back: an older kiln can't read S3, and nothing copies its objects back.
- **What the older kiln finds:** the content-addressed store as it always was (`cas_objects` and
  the chunks), so caches, artifacts, registry blobs and release assets kept since are there, read
  and let go as before. An object a key other than `cas/<digest>` holds (sync's contents, runner
  binaries, recordings, transcripts) is pinned there (its `last_used_at` past any time), so the
  older kiln's upkeep never lets it go, nor its chunks. It doesn't know keys: contents and binaries
  kept since come back from their machines (sync's runners send again what's wanted; a binary is
  fetched or uploaded again), and recordings and transcripts are where it always read them.
- **Coming back:** before it serves, this release takes in what the older kiln did
  (`LocalBlobs::reconcile`). Objects it kept, or used after this release let them go, become keys
  at `cas/<digest>`, with kiln's CAS's records of them. Keys whose object it let go, or whose file
  it removed or replaced, go, with a warning. Sync's contents it kept as files since the `.taken`
  mark are taken in where they lie, and a file whose content the store holds already is removed; a
  binary it kept newer than the store's is the platform's.
- **Transcripts:** the older kiln doesn't archive them, nor hold its conversation store's pruning
  back for them. A conversation that closes while it runs, or closed just before the rollback, is
  archived when this release starts again (as it starts, it notes every conversation that ended
  unnoted), unless the older kiln has let its record go by then (`keep_closed_days` after its
  close).
- **One case loses a key:** the older kiln unpins an object when it reads it or keeps the same
  content again as CI's (a release pipeline's artifact is also the runner binary kept of it). If
  nothing of CI refers to it an hour later, it lets it go; coming back, this release drops the keys
  that held it, and their content comes back as above (sync's runners send it again; the binary is
  fetched or uploaded again).

The tests of `kiln::blobs::local` run the older kiln's upkeep word for word (main at 64b69db: its
collection's SQL, then its sweep) on a store this release wrote, and then this release's
reconciliation on what it left; `kiln::sync::contents` and `kiln::updates` test taking in what it
kept as files.

## Watching it

- `kiln_blobs_requests_total{store, op, outcome}`: requests by operation and outcome (`ok`,
  `refused` for a typed refusal, `unavailable` when the store couldn't answer).
- `kiln_blobs_request_seconds{store, op}`: how long each took.
- `kiln_blobs_bytes_total{store, direction}`: bytes `written` and `read`.
- `kiln_blobs_collected_total{kind}`: objects let go that nothing refers to any more, by what kept
  them: `cas`, `sync`, `recordings`, `transcripts`.
- `kiln_blobs_archives_given_up_total`: closed conversations whose record couldn't be archived in
  the 30 days held past the conversation store's keeping, given up and let go by the store; each is
  an event `given_up` naming the session and the sources of its record.
- Events on target `kiln.blobs` (`event` = `put`, `get`, `list`, `adopt`, `collect`, `refused`,
  `given_up`): refusals at debug, an unavailable store at warn.

The "Blobs" row of the Kiln overview dashboard (deploy/grafana/dashboards/kiln-overview.json)
shows request rates by operation and outcome, p50 and p99 latency, bytes a second, and the share
refused or unavailable.

## The suite against MinIO

Every store passes the same suite of the contract's 163 clauses (`kiln_seams::blobs::cases`): the
simulator and the local store in every `cargo test`, the S3 store against a MinIO when one is
named. The lab's MinIO runs in a container on fedora (`cgr.dev/chainguard/minio`, MinIO
RELEASE.2026-09-22):

```console
$ docker run -d --name kiln-t10-minio --env-file minio.env -p 127.0.0.1:19300:9000 \
    -v kiln-t10-minio:/data cgr.dev/chainguard/minio:latest server /data --address :9000
$ KILN_TEST_S3="http://127.0.0.1:19300 kiln-t10 <access key id> <file holding the secret>" \
    cargo test -p kiln --lib blobs::s3 -- --ignored
```

Recorded 2026-10-21 on fedora: 163 of 163 clauses on the simulator, the local store and the S3
store against MinIO.

## The labs

`crates/kiln/tests/blobs_labs.rs`: each lab is a real kiln server, its bytes where `[blobs]`
says, the lab the same either way. Each runs on the local store in every `cargo test`, and again
on S3 with `KILN_TEST_S3` set and `--include-ignored`.

- **CI:** a CI run's leg hands the lab its own token. Through it, as GitHub's toolkit does, the
  lab saves a 40 MiB cache in two parts, restores it whole and across the parts' join, uploads a
  9 MiB artifact in blocks, finalizes it and downloads it. Both are then in the store at
  `cas/<sha256>`, and the run ends green.
- **Sync:** two copies of a synced folder on the server's runner: a file written in one comes out
  in the other, and again when it changes. Its contents are in the store at `sync/<sha256>` and
  served from there; contents that don't hash to their name are refused and nothing is kept.
- **Upgrade:** a runner binary uploaded is listed, served whole by its platform and by its build,
  is still there after the server starts again, and goes when it's removed. On the local store, a
  binary an older kiln left in `<data>/binaries` is served from where it lies.
- **Presigned URLs:** a get, a ranged get and a put of the declared length work through
  `/api/blobs`. A tampered, expired, other-method, other-key, other-length or `..` URL, or one for
  an object that isn't there, gets one answer, and the refused puts keep nothing.
- **Moving to S3** (S3 only): a server on its local store keeps a binary and sync's contents; it's
  stopped, `kiln blobs copy` runs (twice, the second copying nothing), and the server starts on S3
  and serves both from there.

Recorded 2026-10-21 on fedora: all eight pass, on the local store and on MinIO.

## Not behind it yet

What kiln still keeps on the server's disk outside this store (critical path CP-T10.4 moves the
rest):

- the v1 engine's job artifacts (`<data>/artifacts/<job>.tar`);
- the files conversations mention and published pages' content (`<data>/harness/files/blobs`);
- CI logs (`<data>/logs`);
- the git repositories kiln hosts (`<data>/git`);
- the conversation store's own file, live transcripts included: records, Amphora's (CP-T1).
