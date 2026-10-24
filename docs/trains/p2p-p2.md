# Train notes: P2, runners as learners of the cluster group

docs/p2p.md's P2: each runner holds a replica of kiln's records and its front door answers the
owner's reads from it. What it delivers, what deploying it does and how to watch it, its risks,
its rollback, and what's still open. docs/p2p.md's "P2, as built" has the design.

**Where it stands (2026-10-24, when written):**
- **Merged:** with this file, as one fast-forward of main on 407aecde: the KilnCD fix-up, this
  train, and a compact page head's fix. The landing commit's message has the gate's evidence.
  Rebased onto the wire's channel (01080ecb to fd101874): the group's messages are `Cluster`,
  after the channel's `Caps` and `Wire` (`AgentMsg` 47, `ServerMsg` 55), carried (the cluster
  group) in docs/wire.md.
- **Deployed:** not yet. Deploys go through Ada, and so do runner upgrades: a runner holds a
  replica only once it runs this build.
- **Gated** on fedora (2026-10-24, 05:38 to 05:59Z) on the tree this push carries (407aecde and these
  three commits): clippy across the workspace with all targets, clean, for Linux, Windows
  (`x86_64-pc-windows-gnu`) and macOS (`aarch64-apple-darwin`, both through cargo-zigbuild); every
  workspace test passed (2,161, with 4 test threads) but `sync::tests::a_commit_on_one_copy_moves_its_branch_…`,
  which fedora's global tag signing breaks (its `git tag` asks for a message), on main too, and
  which passed in a clean git config. The head fix's UI checks: tests (126), lint and build pass;
  typecheck fails on fedora only for `node-pty`, an optional dependency npm skipped there (`ui/` is
  main's but `index.css`, and CI's `ui` and `ui-checks` passed on 8adeadba's same `ui/`); its lines
  are as prettier writes them (the file as a whole isn't, on main too).
- **Built on P1,** live since 2026-10-24 02:02Z (2493f75): kiln.db's writes through the cluster
  group.
- **Checked:**
  - clippy, clean;
  - the store, protocol, front door and KilnCD unit tests, among them:
    - a learner starting from a copy, following the log, and taking a new copy once the log left
      it behind (`store::group::tests::a_learner_starts_from_a_copy_…`);
    - a copy holding every table that replicates, row for row with its rowids, and none of the
      server's own (`store::tests::a_learner_s_copy_is_the_database_but_this_node_s_own_tables`);
    - the owner's reads answered from a runner's replica, a write the server just made in the
      next read there, and everything else forwarded to the server, anyone's but the owner's
      included (`frontdoor::replica::tests::the_owner_s_reads_are_answered_…`);
  - every integration test, through real servers whose runners now hold replicas (`hosting`
    with 4 test threads: its 37 run all at once fail on fedora, on main's code too);
  - a copy of a 1,162 MB store, 930 MB of it the server's log search: 231 MB in 1.8 s, under a
    2 GB memory limit;
  - a lab on adacachy (2026-10-24, the build before the rebase onto the channel): a throwaway server and a runner joined to it, the
    runner's front door on `p2lab.test:18443`. The runner became a learner from a 1.47 MB copy
    (4.8 s), and through its front door, 6 of 6:
    - the owner's runs, read from its replica (`kiln-read: p2lab-runner 3163`);
    - a repository the server had just added, in the next read there (5 of 5 tries more by hand,
      each read at or past the server's last write);
    - anyone but the owner sent to the server (401);
    - a read the replica doesn't answer (`/api/agents`) sent to the server;
    - the UI's page;
    - the UI's events WebSocket (101).

## What it delivers

- **Learners.** Every runner on this build becomes a learner of the cluster group, added by the
  server one change at a time. It starts from a copy of kiln.db (`GET /api/cluster/copy`) and
  follows the log over its link. A runner away while the log compacted gets a fresh copy.
- **Reads at the front door.** A runner with a replica serves its front door request by request.
  The owner's `GET` of the runs, a run, a job and the repositories, and the CI pages' own
  (`/api/ci/runs`, a run with its legs, its graph), is answered there, marked
  `kiln-read: <runner> <seq>`, once the replica holds every write the server had made when asked.
  Everything else goes to the server as it came, WebSockets included.
- **Seen:** `/healthz/server`'s `store` counts the learners and gives kiln.db's size.

## Deploying it

1. The server first. Nothing changes until a runner on this build connects: the server only adds
   runners that say they can hold a replica.
2. Then the runners, as Ada upgrades them. Each one, once connected:
   - is told its member, and fetches a copy (the server's log says "a copy of kiln.db for a
     runner's learner", with its size and how long it took);
   - says "this machine holds a replica of kiln's records" in its own log;
   - serves its front door request by request from then on.

Watch:
- `/healthz/server`: `store.learners` grows as runners join; `store.bytes` is kiln.db's size.
- Through a runner's front door, as the owner:
  `curl -sI -H "Authorization: Bearer <admin token>" https://<front door>/api/runs | grep -i kiln-read`
  names the runner.

## Risks

- **Copies read the database.** Each one is a pass over what replicates (not the log search),
  made one at a time. Five runners joining after an upgrade make five, each of a few seconds at
  production's size.
- **Disk on each runner:** a copy of what replicates, plus its log (compacted).
- **The front door's traffic changes shape** on a runner with a replica: requests one by one
  instead of a byte pipe, so the server sees the runner's own requests. A WebSocket is upgraded
  through it, then piped.
- **Owner checks** ask the server once per credentials every 30 s. With the server away, nobody
  counts as the owner, and every request is forwarded, as before P2.

## Rolling back

- **Learners off:** `[cluster] learners = false` on the server (or `KILN_CLUSTER_LEARNERS=0`), then
  its restart. As each runner reconnects, the server tells it to let its replica go, and its front
  door pipes connections to the server whole again. The replica's files stay on its disk.
- **A replica that falls behind** (its 2 s to catch up passed) leaves every read to the server for
  30 s, then tries again.
- **A server from before P2** never tells a runner to hold a replica: its front door pipes
  connections whole, as before.
- **A runner from before P2** never says it can hold one.
- **The release before starts on this one's database.** The group's configuration (its voters and
  learners) is kept in `cluster_conf`, not on `cluster_state`. Every release since P1 fills
  `cluster_state`'s four columns by position as it starts, so a fifth column stopped 8adeadba from
  starting at all ("table cluster_state has 5 columns but 4 values were supplied"). The rollback
  drill of 2026-10-24 found this before production took it. A database an earlier P2 build widened
  is narrowed when this one opens it, its configuration kept.

## Open

- Learners are added, never removed: a runner deleted stays a learner, lagging.
- Only the reads above are a replica's; the rest of the API is still the server's.
- Reads for anyone but the owner go to the server (its route table judges them).
- While the log has no room (stalled under a slow disk), the store's writes wait for it in order
  (`Held`), up to 256 MiB. Past that, a write kiln.db holds goes without its entry, and the
  server's log says so; a learner then lacks it until it takes a new copy.
