# Step 3: Cursor's Agentfile, harness.json and its row in kiln's built-ins

Plan 01a1ce705e5c7504b88732f443d99288, step 3 (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 27).
Policy read back before any edit: Shards docs/development-rules.md sha256
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3, kiln AGENTS.md as of origin/main
(`../POLICY_READ.json`). Role: step 27's coordinator, doing this step itself (ROOT's word, 2026-11-12).

## What changed

- `crates/kiln/src/agentfiles/builtin.rs`: `harness-cursor` in `ALL`, after `harness-claude`, embedding
  `.kiln/images/harness-cursor/{Agentfile,pin.json,harness.json}` by `include_str!`, and
  `mod cursor_tests`. `Builtin` keeps its fields: step 23 (Goose) added `pin` and `files`, and step 30's
  table is on main, so neither needed taking in.
- `crates/kiln/src/agentfiles/builtin/cursor_tests.rs`: step 3's five tests (below).
- `.kiln/images/harness-cursor/Agentfile`: comments only, its instructions as e47d2e1d4 has them. They now
  say each thing step 3 asks them to: the base needs glibc and bash, with no RUN and no variant; the grants
  are the bring-up's until step 9; the project's volume moves to its host path with step 8; the whole home
  is the person's volume (step 7's shape), Cursor's layout inside it; HOME in run.env needs a shards at or
  after 997f4e47e. They name the sign-in's directory, `$HOME/.config/cursor`, and no longer its file, so
  the plan's scan for `auth.json` in what kiln embeds finds nothing.
- Merged: kiln main ce63fc099 (step 19's fetcher, `kiln agentfiles context`, and Claude Code's row, with
  the table's HOME test that now holds Cursor's row too), and `s20-surface-fix` a690a02c5: step 20's fix
  of `tests/agentfiles.rs`'s surface test, which held the listing of built-ins to `[]` and has failed on
  main since Goose's row landed. Its blob, d4deb53a7, is step 25's (`s25-devin-row`) byte for byte, so
  the two merge as one; merged as that commit, no third copy.

## How it was tested

The tree was tested before it was committed. A git bundle of this branch, of kiln main and of
`s20-surface-fix`, with the three files, went to the runner (`run-step3.sh`, this folder). There the
script made scratch commits of the same merges and files, and ran everything below on that tree. The
signed commits here then gave the same trees: 46efabb51 (main ce63fc099 merged), d9fa48de6
(s20-surface-fix merged) and c20f50b55 (step 3's files). A first merge, 4a78747f1, took main at
12aa3af55, two commits past the tree tested (a runner reaper, W24-R31); it was undone before anything
else, and main as it is then gets merged and tested again where this branch lands (step 6).

| Tree | Hash |
|---|---|
| branch at 7c0b73b8d merged with main ce63fc099 | 5562bb3629534d1e6c57420899f7b1049a001b04 |
| then merged with s20-surface-fix a690a02c5 | 73b02ad9a2a785a53f7937bfcbe9348d43024d74 |
| RED: main's merge and step 3's files | 0acc57c5d7a7f4743563552dbd310ae714ffd487 |
| GREEN: both merges and step 3's files, the tree committed | 4f72567ff6ea12b39cceab70e44bd6b0d5e019a2 |

Where: kiln-a094a12d (runner 8fadc1ec213332e16cb0d915a3fb913f), Fedora, Linux 6.19.10-300.fc44.x86_64,
x86_64, rustc and cargo 1.99.1, 2026-11-12 16:20-16:47 UTC, as the transient systemd user unit
s27s3-1794500423 at nice 10 and -j 4, its target seeded by reflink from step 20's finished one. Its log
is `run.log` (this folder, its SHA-256 checked as it came back), every step's command and exit in it.

A first run, capped at 17G (the 16.7 GiB `.kiln/ci.yaml` records for kiln's libraries' tests at -j 4),
was killed by the kernel: its rustc of kiln's library as its tests held 17.7 GB resident and 3.3 GB of
swap (unit s27s3-1794499716, 16:17Z). CI's rust job asks 24Gi, above both. This run ran uncapped; its
unit's memory.peak (page cache included) is below.

## Results

| Step | Command | Result |
|---|---|---|
| UNIT | `cargo test --locked -p kiln --lib agentfiles:: -j 4` | 45 passed, 0 failed: Cursor's five, and every test that reads each built-in (the table's, Goose's and Claude's beside it, step 19's fetcher's) |
| SHARDS | the same with `KILN_TEST_SHARDS` = shards v0.7.0's release binary (build 94838b8, sha256 a9c9092ac5f6a7c6b306a47ef5c9d27f48f6541885abe823c0063abc2d549388, held to v0.7.0's SHA256SUMS), filter `real_shards` | 2 passed: Shards' own check of every built-in's text, Cursor's among them, `checked` and no diagnostic |
| INTEG | `cargo test --locked -p kiln --test agentfiles -j 4` | 1 passed: the server lists the built-ins as `builtin::views()` has them |
| CLIPPY | `cargo clippy --locked -p kiln --all-targets -j 4 -- -D warnings` | exit 0, no warning in kiln (the five shown are a vendored dependency's, focal-memory's `fetch_update`) |
| INTEG_RED | the surface test at RED | failed as on main: the server's listing is harness-goose, harness-claude and harness-cursor (its text 2998 bytes, this Agentfile's), where main's test says none |

Each mutant was made at GREEN by the script and run with `cargo test --locked -p kiln --lib step27_`;
each had to fail it, and each did (exit 101):

| Mutant | What changed | Failed |
|---|---|---|
| M1 | kinds.rs: Cursor's kind launches `Acp(&["acp", "--mutant"])`, nothing else | the kind test alone |
| M2 | kinds.rs: its launch sets `NO_OPEN_BROWSER=0`, nothing else | the kind test alone |
| M3 | the plan's `ENV CURSOR_API_KEY=x` at the end of the embedded Agentfile | the shape test and the credential test |
| M4 | pin.json: amd64's digest one character short (63) | the pin test and the mislead test |
| M5 | pin.json: no linux/arm64 build | the pin test and the mislead test |
| M6 | pin.json: amd64's tree unpacked with strip 0 | the pin test and the mislead test |

After them the tree was GREEN again with no change (`git status` empty). The unit's memory.peak was
40355115008 bytes and its swap.peak 252588032 bytes; memory.peak counts the page cache the 58G target
is read and written through, so it is no build's need. The first run's kill is the measure: kiln's
library as its tests needs more than 17.7 GB resident and 3.3 GB swapped at -j 4.

## What each acceptance criterion rests on

| Step 3's acceptance | Held by |
|---|---|
| kiln embeds the Agentfile, harness.json and pin.json through `ALL`'s harness-cursor entry, one table | every step27 test finds exactly one entry running cursor, named harness-cursor, with harness.json its one file; `every_builtin_is_a_valid_agentfile_once`; `step23_kilns_dockerfile_copies_what_builtins_embed` (kiln's image build copies them); the surface test (the server lists the row) |
| The Agentfile: ARG BASE by digest and ARG TARGETARCH, HARNESS cursor FROM ./cursor-${TARGETARCH}, exactly the three grants, exactly the volumes /workspace and /home/cursor, no ENV, RUN or other ARG | `step27_cursor_agentfile_is_the_plans_shape`: the instructions, exactly |
| harness.json is an OSI config: schemaVersion 1, name cursor, the pin's version | `step27_cursor_pin_and_harness_file_hold`, through `osi_problems`: Shards' reader's rules (`Config::parse`, crates/image/src/osi.rs at Shards main 7ad28fab1), unknown fields refused as it refuses them |
| Its command: cursor-agent, --disable-auto-update, then exactly the arguments kiln's cursor kind launches with | `step27_cursor_runs_as_kilns_cursor_kind_launches_it`, against `Kind::launch`'s own arguments for a default spec; M1 |
| Its env: exactly HOME=/home/cursor, XDG_CACHE_HOME=/tmp/.cache, NO_OPEN_BROWSER=1, the last the kind's launch's | `step27_cursor_pin_and_harness_file_hold` (exactly, and HOME the second volume); `step27_cursor_runs_as_kilns_cursor_kind_launches_it` (`Kind::launch`'s env); main's `step19_a_harness_home_is_its_volume_and_none_is_under_tmp`; M2 |
| No embedded file holds a credential, or an ENV or ARG named like one | `step27_no_cursor_file_holds_a_credential`: the plan's eight words anywhere in the Agentfile, pin.json and harness.json, comments included; ENV and ARG names saying key, token, secret, password or credential; such a name given a value; M3 |
| The pin: version one URL path segment, each SHA-256 64 lowercase hex, builds for exactly linux/amd64 and linux/arm64, each one tar.gz tree stripped of 1 with both counts | `step27_cursor_pin_and_harness_file_hold`; `step27_a_cursor_pin_or_harness_file_that_would_mislead_a_build_is_caught` (fifteen edits of the pin and seven of the harness file, each refused for its own reason); M4-M6 |

## Not shown here

- Shards' own reader on harness.json: kiln links no Shards, so the test holds harness.json to that
  reader's rules as written; Shards itself reads it in step 4's build.
- Cursor's pin has no `smoke` yet. Every harness pin on main carries one (Goose's, Claude's); step 5
  adds Cursor's through pin.sh, `cursor-agent --disable-auto-update --version` printing the pin's
  version, when image() smokes builds.
- Clippy on macOS and Windows: kiln's gate runs them, and the full gate waits for the person's word
  (AGENTS.md, "Checks before handing over"). This change has no platform code: a table row, its tests,
  and comments.
- Shards v0.7.0 is the release the static check ran with. Running the image needs v0.8.0 or a main
  shards at or after 997f4e47e (HOME in run.env), which steps 4 and 5 use.
