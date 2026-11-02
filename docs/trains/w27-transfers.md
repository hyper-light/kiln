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

**Where it stands (2026-11-01, when written):**
- **Merged:** no. It's the local branch `w27-custody`, built on the signing plan's `w27-sign`
  (merged in at aa163ed7f3) and its signed verdicts (532d8a031a, from `w27-verdicts`), which land
  first. Steps 11 to 16 are on it: step 12's sync and reach run from `w27-s12-reach`, step 13 from
  `w27-relays` (ee47b78d3e), step 14 from `w27-custody-production` (e474474b02), step 15 from
  `w27-custody-sim`. Landing, pushing and deploying wait for Ada's word.
- **Deployed:** no.
- **Checked at f20c3107dd** (2026-11-01, through kiln's MCP `run` on fedora-2): clippy plain and
  with `chaos`; kiln-ci's identity tests, 90; the kiln library's custody, relays, reach, runner,
  proto, migrations, blue/green, agent, harness, CD, deploy, simulation and authz tests, 282;
  `tests/custody.rs`; with `chaos`, every test of `tests/transfers.rs` (16) and identities' walk
  and custody build; in a release build with `chaos`, the 14:18 drop, the session hand-overs, the
  replant and every relays test (14). Both simulations' 200 seeds, each planted bug found.
- **Checked before:**
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
- Between machines (step 12, all but the cluster group): docs/ci.md W27 lists every path where one machine takes
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
- Relays (step 13): every intermediary records what it passes on, unchanged, numbered on a stream
  of its own to its receiver: a runner relaying its agents' calls to kiln, kiln relaying an agent's
  words to the runner holding the agent they're for, kiln relaying reach links' frames, and
  blue/green's old build forwarding to the new one. A drop, swap or alteration is refused where
  it's used and recorded as a break naming the relay. `GET /api/custody/relays` lists each relay's
  streams.
- To production (step 14): a KilnCD sync hands its cluster the manifests it applied and the images
  they name, by digest, and kiln follows each image through its node's resolution of the index to
  what each pod runs. A pod running anything else is a break, and an environment requiring a
  verified chain (`gates.require_chain`) doesn't count that deploy healthy.
  `GET /api/cd/apps/:app/custody` shows it.
- The walk (step 15): `kiln custody walk <digest>` (`GET /api/custody/walk`) walks a thing's chain
  back from where it's used to where it was made, each hop in words, or names its first break;
  `--export` writes every record it read as signed, with kiln's CA certificate and its
  revocations, for an auditor to check without kiln.
- End to end (step 16): on a test cluster, a running pod's chain walks back to its commit, and each
  break that path can play is refused where it would be used and recorded.

## Deploying it

- **The wire, as it landed.** W27 landed on main on 2026-11-01 (481fedb9a3, on Ada's word) with
  main's variants first: `AgentMsg` 56 and 57 are main's `Unread` and `LinkUnread`, then 58 to 60
  W27's `LegResults`, `LegSignature` and `Custody`; `ServerMsg` 62 to 64 are main's `Unread`,
  `ReachRefused` and `Freeze` (W26-R22), then 65 and 66 W27's `SignAsLeg` and `Custody`. Bits 41
  to 43 are main's (`CAP_UNREAD`, `CAP_WORKSPACE_COMMITS`, `CAP_WORKSPACE_COMPARE`), 44 to 46
  W27's (`CAP_AGENT_GIT`, `CAP_LEG_SIGNS`, `CAP_CUSTODY`) and 53 main's `CAP_FREEZE`.
- **What this plan adds to it.** No message variant: `CAP_RELAYS` (47), and reach's
  `Ask::Custody` and `Say::Custody` (after main's reach `Unread`, only on links at reach version
  2). unify/takeover was told bits from 47 too, so whichever lands second renumbers. Diff the
  tails against production's commit again before the runners take it (`wire.py`).
- **Merging main into it (2026-11-01, after W27 landed).** main reads messages typed
  (`wire/payload.rs`): the relays' check of an agent's words runs over the bytes kiln sent
  (`HarnessCmd`'s and `HarnessSend`'s, kept raw); `mcp::runner_ask` takes the typed question, the
  ask as it came (`raw`, which `relays::call` and `Relays::placed` read) and its placing; path a's
  `custody` question is `reach::Question::Custody`; blue/green's `Ready` says `relays` and main's
  `stores`; `cd.apps.custody` takes a typed input (`CdAppsCustody`). main's `ship.rs` takes a
  commit's author from `made_by(&rec).agent`, in this plan's `handing()`.
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
- **Step 13 on the wire:** one new bit, `CAP_RELAYS` (47). With it, a runner sends its record of
  each act it passes on (`passed_on` beside the ask) and checks what kiln relays to it (`relayed`
  in an agent's words; kiln's records of a reach link's frames, in `ServerMsg::Custody`'s new
  `{"reach": …}` shape); without it on either side, things go as before. Blue/green's old and new
  builds of one runner speak `Frame::Forward`'s `passed_on` between themselves.
- **Step 14 on the wire:** nothing new. CD's records stay on the server.
- **kiln's store of bytes, after it:** a whole get of an object whose bytes don't hash to its etag
  is refused (`BadChecksum`), where it used to be served as the store gave it.
- **Merging main: reach's variants.** main has `Ask::Unread` and `Say::Unread` (index 11 on each),
  which this branch lacks. The landing merge keeps main's first, `Custody` after them on both
  enums (index 12), and `ASK_VARIANTS` and `SAY_VARIANTS` become 13; then
  `reach::wire`'s `w27_r08_custody_frames_are_appended_after_every_older_variant` names `Unread`
  as the variant `Custody` comes after.
- **Merging main: `runner_ask`'s split.** main's `mcp::runner_ask` takes the ask both typed and as
  it came (`ask: &Question, raw: &Value`). Step 13's `relays::Relays::placed` reads `passed_on` from
  the ask's JSON, so it takes `raw`.
- **Config:** nothing new in kiln.toml.
- **First start:** the migrations add tables and indexes (`ci_leg_pushed` with step 11;
  `cd_custody_syncs`, `cd_custody_pods` and an index on `registry_manifests` with step 14's
  `w27_cd_custody`) and one column with a default (`custody_breaks.relay`, with step 13's
  `w27_relays`). Both of the last two are stamped 20261101-0200: kiln applies them by name, and
  they touch different tables.

## Watching it

- `kiln transfers refused`: what kiln refused, and why. A copy left behind acting shows here.
- `kiln transfers unsigned`: hand-overs that went unsigned, and which side is from before.
- `GET /api/custody/breaks`: chains that don't walk. It should stay empty.
- `kiln custody walk <digest>`: a thing's chain, in words, or its first break.
- `GET /api/custody/relays`: each relay's streams, numbers missing and records unanswered.
- `GET /api/cd/apps/:app/custody`: whether each pod runs what its deploy handed on.

## Rollback

- Back to the release before: its migration runner ignores the tables it doesn't know, and an
  older build never names `custody_breaks.relay`; no setting needs to come out of kiln.toml.
- Hand-overs go unsigned again, as before.

## Open

- Step 12's cluster group (its learners' copies and log): waits on Ada's word on T2's files.
- The blob stores stand in for Mantle's puts and gets until CP-T10.2 vendors it (put to Ada).
- A runner keeps its reach directory 30 s and refuses a runner that announced after its last read
  (put to Ada: re-read once before refusing).
- Step 16's end to end runs on the test cluster, then the gate on this branch.
- A session's `Open` at a later epoch isn't signed itself (docs/reach.md, "Not yet").
