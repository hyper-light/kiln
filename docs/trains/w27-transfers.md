# Train notes: W27 transfers, every hand-over signed by both sides

The Transfers plan (kiln plan `01a17856b1b477eda14a574ae312a123`, the Review plan's step 8; docs/ci.md
W27-R08 and W27-AC05). Every hand-over in kiln is now a transfer, and each is signed by both sides:
- an agent session moved to another machine, taken over by a newer process of its machine, carried
  through a blue/green upgrade, or resumed from kiln's copy;
- work handed between agents and people;
- a change or a review request changing hands;
- a promotion between environments, and the replant's runner role.
What a copy left behind does through kiln is refused and recorded, and so is a hand-over's word
reaching a process it isn't for.

**Where it stands (2026-10-30, when written):**
- **Merged:** no. It's the local branch `w27-custody`, built on the signing plan's `w27-sign`
  (merged in at aa163ed7f3), which lands first. Landing, pushing and deploying wait for Ada's word.
- **Deployed:** no.
- **Checked:**
  - clippy for kiln with all targets, clean;
  - the kiln library's W27, custody, deploy, updates, blue/green, agent and harness-host tests;
  - in a release build with the `chaos` faults, every test of `tests/transfers.rs` and
    `tests/custody.rs`: the 14:18 drop and the twin played again among them;
  - the transfers simulation's 200 seeds.
  The gate is in docs/ci.md W27's tests line once it has run.

## What it delivers

- Custody records (`crate::custody`, migration `w27_custody`): each signed record kept by its
  statement's digest, its hop checked as kiln takes it, and breaks and refusals recorded. Readable
  over `GET /api/custody`, `kiln custody` and `kiln transfers refused | unsigned`.
- Sessions' hand-overs. The hand-on is signed at the epoch the session leaves; the receipt is signed
  with the key kiln certifies at the next epoch. Both are kept before the epoch rises.
- Acts fenced to the session's holder. Each act an agent makes through kiln is signed; a copy left
  behind's is refused, answered with why, and recorded.
- Words addressed to one process. The server signs the process each word is for, and any other
  process refuses it.
- Work, changes, review requests and promotions handed on, and the replant's runner role.
- The simulation (`sim::transfers`): no stale act is ever applied, and each of five planted bugs is
  found.

## Deploying it

- **The wire, before any runner runs it.** Its new message variants collide with production's as
  this branch stands: d50f1fd322, checked 2026-10-30.
  - `AgentMsg` 56–58 here are `LegResults`, `LegSignature`, `Custody`. Production has `Unread`
    (56) and `LinkUnread` (57).
  - `ServerMsg` 62–63 here are `SignAsLeg`, `Custody`. Production has `Unread` (62) and
    `ReachRefused` (63).
  The landing merge must put main's variants first, in the order agreed with Kiln PRs:
  - `AgentMsg`: `HeadroomStatus`, `Unread`, `LinkUnread`, then `LegResults`, `LegSignature`,
    then `Custody`;
  - `ServerMsg`: `Headroom`, `Unread`, `ReachRefused`, then `SignAsLeg`, then `Custody`.
  Then update `proto::w27_r08_custody_is_appended_after_headroom` to name the new indices. The
  capability bits don't collide: 41 is main's `CAP_UNREAD`; 42, 43 and 44 are `CAP_AGENT_GIT`,
  `CAP_LEG_SIGNS` and `CAP_CUSTODY`. Diff the tails against production's commit again before the
  runners take it.
- **Order.** The server first, then the runners. A runner from before signs nothing, and its
  hand-overs are recorded as unsigned, naming which side is from before. A server from before
  announces no `CAP_CUSTODY`, so runners send it nothing signed.
- **Config:** nothing new in kiln.toml.
- **First start:** the migration only adds tables.

## Watching it

- `kiln transfers refused`: what kiln refused, and why. A copy left behind acting shows here.
- `kiln transfers unsigned`: hand-overs that went unsigned, and which side is from before.
- `GET /api/custody/breaks`: chains that don't walk. It should stay empty.

## Rollback

- Back to the release before: its migration runner ignores the tables it doesn't know, and no
  setting needs to come out of kiln.toml.
- Hand-overs go unsigned again, as before.

## Open

- An approval carried across a rebase-only revision (step 7's last part). It waits on the signing
  plan's signed verdicts, 532d8a031a on `w27-verdicts`.
- Steps 11–16: artifacts, machines and intermediaries. They start once Ada has seen plan v7.
- A session's `Open` at a later epoch isn't signed itself (docs/reach.md, "Not yet").
