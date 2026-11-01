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
  (merged in at aa163ed7f3) and its signed verdicts (532d8a031a, from `w27-verdicts`), which land
  first. Landing, pushing and deploying wait for Ada's word.
- **Deployed:** no.
- **Checked:**
  - clippy for kiln with all targets, clean;
  - the kiln library's W27, custody, deploy, updates, blue/green, agent and harness-host tests;
  - in a release build with the `chaos` faults, every test of `tests/transfers.rs` and
    `tests/custody.rs`: the 14:18 drop and the twin played again among them;
  - the transfers simulation's 200 seeds.
  - `ci/gate.sh` at f9adba9e62 (2026-10-31, on fedora-2: adacachy's disk was too short of room for
    it): GATE: PASS. That's clippy for Linux, Windows and macOS, and the workspace's 4,109 tests, 0
    failed. The gate before it, at 75e3c7d7f3, failed only `plans::stack::end_to_end::p_ac1`: a
    library's test binary written as git's update hook. 1868928967 fixed it on `w27-trail`, and it's
    cherry-picked here as f9adba9e62.

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
- A person's side signed in the browser with its own key: a plan step taken over, and Ship's Hand
  off. kiln signs for them only where the browser can't (a page that isn't secure), saying why.
- An approval carried to a rebase-only version, signed by kiln as a transfer and shown in Ship.
- The simulation (`sim::transfers`): no stale act is ever applied, and each of five planted bugs is
  found.
- A leg's custody, from the land to what it made (step 11): kiln hands a leg's inputs (its
  assignment, its commit naming its land, the tarball it serves, the caches it finds) to the runner
  process running it; that process receives each and hands each to the leg's executor; the leg's
  hand-on of what it made names the images its token pushed to kiln's registry. A walk from an
  artifact or an image goes back through every hop. A tarball changed on its way is refused before
  the leg runs, and the break recorded.
- The custody simulation (`sim::custody`, step 15): things handed on through relays that drop,
  change, swap and replay what they carry, under kiln-sim's nemesis, walked by kiln-ci's custody
  core: no chain with a break walks as unbroken, and each of three planted bugs is found.
- Between machines (step 12, so far): docs/ci.md W27 lists every path where one machine takes
  something from another. A runner's object from a peer's cache is handed on by the peer and
  received by the runner that fetched it, each signing; a copy changed on its way is refused, and
  the break recorded. No runner in production asks a peer yet: nothing mints its cache's tokens.
  kiln's store of bytes: kiln signs for the store at each put and whole read of an object whose
  key doesn't name its content (binaries, transcripts); a read whose bytes aren't what the store
  says is refused, and its break recorded. The conversation feed: each batch goes with its giver's
  hand-on, and the taker refuses one that isn't what it says before taking any of it.
  kiln run's sync and a reach run: the agent session that asked hands its workspace (by its tree's
  digest) and its run on to the other runner's process, with a delegation to receive both for it;
  that runner receives each for the session under it, and hands the output back, which the session
  receives. A copy changed on its way is refused before anything runs in it, and the break
  recorded; so is a run or an output that isn't what was handed on.

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
- **Merging main into it (Ship, from CI/CD Integration, 2026-10-31).** main's `ship.rs`
  `hand_off_in` (88de8655b6, b1ef50d5ce, not in this branch) takes a commit's author from
  `made_by(&rec).agent`, not `authors_agent`. This branch's `handing()` (the browser-signed Hand
  off) must take that, keeping its live-revision and no-one-has-it filters;
  `ship::tests::w26_r18_made_by_says_who_made_each_commit` holds it. Against `ship-actions`
  (b978aaecd5), `ui/src/api/ship.ts`, `ui/mock/ship.ts`, `docs/api.md` and both conflicts in
  `authz/mod.rs` keep both sides.
- **Order.** The server first, then the runners. A runner from before signs nothing, and its
  hand-overs are recorded as unsigned, naming which side is from before. A server from before
  announces no `CAP_CUSTODY`, so runners send it nothing signed.
- **Step 11 on the wire:** no new variants or bits. kiln's hand-ons of a leg's inputs go as
  `ServerMsg::Custody` with a new JSON shape (`{leg, hand_on}`), which a runner from before ignores;
  the tarball's comes in a response header (`Kiln-Custody-Hand-On`) and a cache's in a new field of
  the lookup's answer, both ignored by a runner from before. A runner asks `/_kiln/pushed` at a
  leg's end; a server from before answers 404, and the leg's hand-on names no images, as before.
- **Step 12 on the wire:** a runner asks the server which process of a peer takes reach
  (`ReachAsk` op `custody`); a server from before doesn't know the op, the runner hears no URI,
  and nothing is signed for that peer, as before. The feed's `Msg::Batch` has an optional
  `hand_on` (JSON): a taker from before ignores it, and a giver from before sends none. Reach's
  links move to version 2 (`wire::VERSION`, with `MIN_VERSION` 1): a link runs at the lower of
  the two ends' versions, a runner from before refusing a Hello at 2 is asked again at 1, and only
  a link at 2 carries the new `Ask::Custody` and `Say::Custody`. Runners announce their version
  (`Announce.version`, already there); the server's directory says it (`RunnerInfo.reach_version`,
  a new optional field). `AgentMsg::Custody` carries a new JSON shape, `{"refused_hop": …}`, which
  a server from before refuses as an unreadable record, as it does any it can't read.
- **kiln's store of bytes, after it:** a whole get of an object whose bytes don't hash to its etag
  is refused (`BadChecksum`), where it used to be served as the store gave it.
- **Merging main: reach's variants.** main has `Ask::Unread` and `Say::Unread` (index 11 on each),
  which this branch lacks. The landing merge keeps main's first, `Custody` after them on both
  enums (index 12), and `ASK_VARIANTS` and `SAY_VARIANTS` become 13; then
  `reach::wire`'s `w27_r08_custody_frames_are_appended_after_every_older_variant` names `Unread`
  as the variant `Custody` comes after.
- **Config:** nothing new in kiln.toml.
- **First start:** the migrations only add tables (`ci_leg_pushed` with step 11).

## Watching it

- `kiln transfers refused`: what kiln refused, and why. A copy left behind acting shows here.
- `kiln transfers unsigned`: hand-overs that went unsigned, and which side is from before.
- `GET /api/custody/breaks`: chains that don't walk. It should stay empty.

## Rollback

- Back to the release before: its migration runner ignores the tables it doesn't know, and no
  setting needs to come out of kiln.toml.
- Hand-overs go unsigned again, as before.

## Open

- Steps 12–16: machines, intermediaries, production, the walk and its simulation, and the end to
  end. Ada approved plan v7 on 2026-10-31; step 11 is built, and step 12's peers' objects.
- A session's `Open` at a later epoch isn't signed itself (docs/reach.md, "Not yet").
