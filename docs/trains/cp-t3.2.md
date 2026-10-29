# Train notes: CP-T3.2, each message once

CP-T3.2's part of the critical path's merge trains (CP-M4; its master plan's §6): what it delivers,
what deploying it does and how to watch it, its risks and its rollback, and what's still open. How
it works is docs/sends.md.

**Where it stands (2026-10-29):**
- **Landed** on kiln's main in two parts. The change itself is 074ec6163d (2026-10-25), and production
  runs it: kiln:750aa4bbf6 contains it, synced 2026-10-28 05:58Z. Its follow-up, after the
  twenty-second adversarial check, landed with this note on Ada's "Land it" (2026-10-28). It isn't
  deployed yet.
- **Checked** by 22 fresh adversarial checks over its life, each by an agent that didn't build it.
  The last found every criterion met at 05870d1677 (the follow-up on main 750aa4bbf6) and no new
  defects.
- **Gated** at 05870d1677 as a differential pass: every step of ci/gate.sh ran on the gate's own
  build, env and caps. Clippy for Linux, Windows and macOS passed, and every CP-T3.2 test passed.
  Every failure is main's or the gate environment's, each checked alone or A/B'd against main. The
  landing merge's commit message names them. No commit of main could pass the gate literally then:
  - kiln's lib binary overflows its stack in `updates::tests::the_terminal_script_runs`;
  - the gate's MemoryMax, GIT_CONFIG_GLOBAL and TMPDIR fail facts, hosted_actions and builds' w24_r24;
  - on adacachy, its guard stops tests/pools.rs's Docker burst on the burst's own containers.

**What it delivers:**
- **Message ids.** Every sender makes a message's id once, and every hop and retry carries it. The
  senders are the UI's composer, agents_send's key, plan runs' keyed ids, the goal driver's
  `send_id` and broadcasts.
- **The inbox.** Each session has one on the runner that holds it, and it takes each id once,
  fenced by the session's epoch. A message sent again is answered with the first time's answer,
  marked a repeat, and is never delivered again. Its refusals are typed: `stale_epoch` (naming both
  epochs), `behind`, `moved`, `reused`, `invalid`, `full`, `no_session` and `unavailable`.
- **Ids travel with their session** through moves, hand-overs between a machine's runner processes,
  and blue/green upgrades.
- **Idempotency keys.** agents_send and the API take one: a call again under it is the same message,
  answered as the first.
- **The follow-up:**
  - a session whose host is gone stays rather than moving without its ids;
  - a message with an id for a runner that's away is refused at once (`Unreachable`), never parked;
  - the UI's Retry keeps a message's id when it may have got there;
  - a message sent again after its upload left the server is its repeat;
  - docs/sends.md's open items are updated.

**Deploying it:**
- **Either side may upgrade first.** A runner says it has inboxes (`CAP_INBOX`), and the server sends
  it messages as `HarnessSend`, a variant appended last. A runner from before gets `HarnessCmd` as
  before, its messages unchecked (`kiln_sends_unchecked_total`).
- **A send waits for a synced commit on its runner.** The inbox's check adds at most 2 ms at p99
  (bench send_path). End to end, a send's p99 was 5.1 ms on fedora's NVMe, against main's 0.03 ms
  before: the sync main never made. docs/sends.md "On disk" and "Evidence" have the figures.
- **Configuration.** No new setting. `KILN_INBOX=off` runs a runner without inboxes, as before.
- **Migrations.** Two add nullable columns: `goal_items.send_id` and `ended_turn`, and
  `goal_stacks.asked`.

**Watching it:** `kiln_sends_*` (docs/sends.md "Metrics"; Grafana's Sends row). Look for:
- `refused_total{reason}`: `unavailable` means a runner's records can't be read, and `moved` or
  `stale_epoch` come in bursts around moves and upgrades;
- `unanswered_total`: answers lost on the way;
- `unchecked_total`: hosts without inboxes;
- `take_seconds`: it should sit near the drive's sync floor.

**Risks:** docs/sends.md "What's open". Every known case loses a message rather than doubling it,
until CP-T3.1:
- a session's queue is in its host's JSON record, written half a second after a change, so a crash
  in that window can hand a message again;
- CP-T3.3 to CP-T3.5 are still to come;
- a message changed between its tries is refused `reused`.

**Rollback:**
- **The wire only grew.** An older server never sends `HarnessSend`, and an older runner never says
  `CAP_INBOX`.
- **An older runner leaves its inbox records alone, and a newer one reads them again.** A message an
  older runner took isn't in them, so a retry of it after going forward again is delivered.
- **The migrations' columns are nullable**, and the migration runner skips what it doesn't know.

**Open with others:**
- **The goal-stack agent's typed wire.** Its step 4, which answers a send a runner can't read from
  its inbox, was reviewed by T3 (two findings, both fixed). It lands after its steps 1 to 3; steps 1
  and 2 are on main.
- **CP-T3.3 owes a nonce** on the inbox's answers before `Update::Inbox` may be sent again (the
  wire's acks, split with the goal-stack agent).
- **Kiln Metrics** agreed the `kiln_sends_*` names on 2026-10-24. None are pending.

**Ada's:**
- **Taken:** landing on a differential pass ("Land it").
- **Reported, unchanged:** ci/gate.sh's guard on adacachy measures its own Docker burst as
  production's strain.
