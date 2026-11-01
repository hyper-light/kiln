# SQLite-to-Amphora format cutover

The first start of a unified build that finds a SQLite-era `kiln.db` cuts it over itself. There is
no command to run first, and no step for a person beyond the deploy.

## What a start does

1. **Takes the file once nothing else has it.** SQLite can't leave WAL mode or begin an exclusive
   transaction while another connection holds the database, so the start asks for both with no
   wait. A process that still has it open (an older build not yet stopped) keeps it: this start
   fails saying so, and the next one takes it. Leaving WAL folds the WAL into the file, and opening
   it to write rolls back a hot journal, so the file alone is then the source.
2. **Fences it.** The SQLite file and a retired cluster log (`cluster/cluster.log`) are kept by
   same-filesystem links inside a directory at `kiln.db`, read-only, with `amphora-format.json`
   naming them. The directory blocks old SQLite opens and old replica rename-over replacement.
   Each step is flushed before the next (on Windows, NTFS journals directories and a read-only
   link can't be flushed through, so only the writable files are). A start that stops part way
   goes on from where it stopped.
3. **Checks the retired cluster log (U15).** The old build wrote each change into `kiln.db` first,
   numbered there (`cluster_state.seq`), then into its log, and made `kiln.db` durable at a log
   index (`cluster_state.applied`) only on a compaction or a stop that got that far. So the log's
   committed entries past `applied` are writes `kiln.db` holds already. Only a write the disk lost
   from `kiln.db` and the log kept (numbered past `seq`) is refused, for the old build's offline
   recovery; nothing is replayed into the import.
4. **Imports it** into Amphora a page at a time (the restore's cursor commits with each page), and
   then the conversation sources (`harness/conversations.db` and the local agent's), each page with
   its cursor. A conversation source is hashed whole when its import starts, so a change under it
   is refused; once its import has finished, a start that finds its files as they were (size, last
   write and, on unix, which file) doesn't read it again.

Fresh and Amphora-only hosts fence the old CREATE-on-open pathname too. Linux, macOS and Windows
cut over alike.

## Rehearsed (2026-11-01)

On copies, with the unified tree's test build (debug, lowest IO priority, adacachy at load ~120):

| Data | Source | Took | Result |
|---|---|---|---|
| Production (`kiln-data`) | `kiln.db` 122 MB (187,381 rows), log 65 MB, 13 committed writes past `applied` | 117 s | 230 tables, 187,415 rows (the new migrations' tables with them); all 13 writes held (`seq` 6,096,746 past the last, 6,091,779) |
| adacachy's retired server (`~/.local/share/kiln`, Oct 22–26) | 4 KB file and 3.6 MB WAL; log durable at 0, writes 1 to 14 | 6.7 s | all 14 held (`seq` 14) |
| jupiter's retired server (Windows) | `kiln.db` with WAL, log | 5.7 s | fenced, read-only source, imported |

Production's `harness/conversations.db` is 11 GB (443 sessions, 828,350 events, 4,430,759
records). Its first import reads all of it twice (hash, then pages) before the hub serves. The
rehearsal read it at 1 to 6 MB/s on a saturated disk and was stopped; at production's IO priority
in a quiet window it's faster, but plan on tens of minutes.

## Deploying the first unified build

- **No guard for this one.** The self-deploy guard runs the *previous* image, whose guard puts the
  previous image back when the new pods aren't available within 10 minutes; a SQLite-era image
  can't open the fence, so that rollback can't start. Turn auto-rollback off for the app first
  (`cd.apps.set_auto_rollback {app: "default/kiln-production", on: false}`), watch the start by
  hand, and clear it (`on: null`) once kiln serves. Later unified guards hold on unknown schema
  requirements, changed non-image pod-template fields, and unproven external configuration.
- **Room for the first start.** Production's values give the start up to two hours
  (`startupProbe` 3,600 × 2 s in `deploy/argocd/values/kiln.yaml`) before the kubelet restarts it.
  Later starts take seconds; the liveness and readiness probes are unchanged.
- **A quiet window.** The imports share adacachy's disk with production: deploy with no gate
  running and the host's load down.
- **The master key.** A host whose legacy data holds protected state (secret keys, CI secrets,
  OIDC keys, the GitHub App, sealed tokens, hooks, chat integrations) needs the original master's
  SHA256 at `<data>/secrets/master.sha256` (64 lowercase hexadecimal digits, optional line
  ending), or `[ci.secrets] trusted_master_sha256`, before its first start; they must agree if both
  are set. Production's data holds none, so its first start makes and binds a new master.
- **Runners.** A runner's host database is `[server] data_dir/kiln.db` of the configuration it
  reads, which on adacachy and jupiter is a retired server's data (rehearsed above).

## Rollback

A SQLite-era binary can't be a rollback target after this: it can't open the fence. Keep the
fence and its preserved sources, and roll forward to a compatible Amphora build. Removing the
fence and restoring an old snapshot loses later accepted state and isn't a supported rollback.

## Open

- Four U15 tests fail on Windows where they create a fixture log with the retired settings
  ("a segment is a multiple of the alignment, four blocks at least", and "an acknowledged frame
  does not verify" reading one): the fixtures, not the reader, which read jupiter's real log. The
  gate runs tests on Linux.
- The conversation store's upkeep (pruning, compaction) runs only while nothing is asked of it
  for a couple of seconds, as on main: a store asked without pause never prunes.
