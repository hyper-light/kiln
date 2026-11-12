# Plan step 27, step 2: Cursor's tar.gz trees in kiln's one unpacker (evidence)

Plan 01a1ce705e5c7504b88732f443d99288 (step 27 [kiln], the Cursor CLI Agentfile), v5, step 2 of 6:
"unpack in kiln's fetcher: tar.gz archives, stripped, nothing but files and directories"; research
01a1ce705e5c7504bdcac9721fa699e3 (f10: the packages hold only files and directories under
dist-package/; f25: kiln's own tar and flate2). The module it changes is step 25's (plan
01a1ce705e4d7008993f5c7291ef4476, step 2, branch `s25-unpack`), converted here as step 25's coordinator
decided (below).

## Identity

- Role: implementer, kiln agent 01a1cf3fbf0c72f38531c455cc4647f2. Coordinator: step 27's agent
  01a1ce6fc21570b5b3882c8c52f0c4e4, which carries the plan itself since kiln lost the plan run that
  started this agent. ROOT: 01a148884a4770fdaf0111f3f792a248.
- POLICY_READ, 2026-11-12 (read 02:1x PST, re-read 05:45 PST after kiln's restart):
  - Shards origin/main 8f3cd5b7411dfeea47956a5143eec2c3466b058f: docs/development-rules.md sha256
    df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3 (R01-R16, read in full), AGENTS.md
    f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41, docs/development-evidence.md
    61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63; unchanged from ac87190d3 (02:1x).
  - kiln origin/main 94639db7b83076143ff48fe5dac6edb9d3a33743: AGENTS.md sha256
    16b05ef80db024a7b54b0e703d42c8cf5c1f330a626d6bd85ae4525609447a4c (CLAUDE.md is `@AGENTS.md`, sha256
    336cc4fbf19beaada7ccf9986414fa91851a8d7a07dfb3ccbe800a69eed0ab49), docs/ci.md
    be867453ea3527c50e5a030c94f931ef9bffb80f8f58a868e98b0cb7b47a4baa (§0, §0.3 binding).
- Owned paths: `crates/kiln/src/agentfiles/unpack.rs` (step 25's, converted), its tests
  `crates/kiln/src/agentfiles/unpack/tests.rs`, the `pub mod unpack;` line in `agentfiles/mod.rs`, and
  this folder.
- Worktree `~/Projects/kiln/.claude/worktrees/s27-unpack`, branch `s27-unpack` from kiln origin/main
  94639db7b.

## Where things stood

- Step 19's fetcher (`materialize` and `kiln agentfiles context`, its plan 01a1cdf3cc7a70e4b3ca2ec610352383,
  step 3) is on no branch: `s19-claude-agentfile` is main's 1a5a5f00d, and step 19's own step 1 had not
  started, at 02:1x and again at 06:05 PST. So the materialize hook and the HTTP light-integration test
  wait for it (Owed, below); no second fetcher is written.
- Step 25's step-2 agent (01a1ceb7914e702aa9059e6dfdd3c79a) had written kiln's unpacker: committed
  a04cffd0c on `s25-unpack`, extended (uncommitted) toward pi's and then Cursor's and agy's pins. Steps
  25 and 28 share it. R12 allows one module, so this step converts it rather than write a second.
- The contract: steps 26 and 27 agreed one typed enum, `Member {format, member, sha256, size}` or
  `Tree {format, strip, unpacked_bytes, entries}`, both or neither a typed error (step 27's planning
  evidence, step26-messages.txt). Step 25's coordinator accepted it, with the whole-tree rename and the
  normalized modes, and decided that step 27 converts the module in its own commit on top of step 25's
  (`evidence/s25-step2-brief.md`, a verbatim copy of step 25's brief, sha256
  bae70dd8a8f0b881bea47cdd18b85d08a70fd4992d34bd2d65366017b0a14301, section "Update: step 27 converts
  the schema after your commit"). Step 26's agent was told the enum lives in this module (kiln mail,
  2026-11-12 05:4x).
- Cursor's x64 package, measured at planning (`evidence/s27-archive-entries-x64.txt`, a copy of step
  27's planning evidence, sha256 ea6644751e1a03edaa3011fa6124d9901398a0395b02b2776a0b2ea086609136): 435
  entries, 353 regular files and 82 directories, none outside dist-package/, no `..` or absolute path,
  no set-ID bit, modes 0644 (329), 0755 (24 files) and 0755 (82 directories). `tar -t` lists
  dist-package/ itself, so the leading directory the strip drops is an entry, counted, and taken as the
  tree's root rather than refused as "an entry the strip leaves empty" (a file the strip leaves no name
  is refused).

## The decision (R02, R03, R05, R12)

Sources, beside step 25's (its unpack.md):

- tar 0.4.46 (kiln's pin), `src/archive.rs` `next_entry`: GNU `L`/`K` and pax `x` headers of a GNU or
  ustar header are consumed and applied to the next entry; a pax global header (`g`) is passed through
  as an entry; pax `size` overrides the header's ("Disagreement among parsers allows construction of
  malicious archives that appear different when parsed"). `src/entry.rs` `unpack`: files are opened
  `create_new`, short copies fail, modes are `mode & 0o777` unless `preserve_permissions`, directories'
  times aren't set. So entries are written by this module, never by tar's `unpack`/`unpack_in`, and
  every check is kiln's.
- Linux rename(2): a directory moved to another parent needs write permission on itself (its `..`);
  renamed within one parent, as staging is renamed to its sibling, it doesn't.
- The pins: Cursor's (above), Devin's (step 25's raw scan: 250 headers, 234 files, 16 directories, only
  0755/0644 modes listed, no set-ID or sticky bit), pi's, ripgrep's and fd's (step 28's measured needs,
  quoted in the brief: files and directories only; pi 26 directories 0755, 204 files 0644, 4 files 0755).

Weighed and chosen:

- **Placement.** Step 25's merged a tree into a per-arch directory already holding harness.json, moving
  each top-level entry in. This step's acceptance needs the per-arch directory to appear only once the
  whole archive is in, so a tree is staged beside the first directory on the way to its target that
  isn't there (the per-arch directory itself, for `.`), and that one directory is renamed into place
  once the entries are in, the totals checked and the modes set. A target there already is refused
  (`Exists`); the fetcher writes a build's trees first, then members, then harness.json. Every pinned
  build that has a tree lists it first (Devin, pi, Cursor). A tree at a deeper path stages beside its
  first missing directory the same way.
- **Schema.** Step 25's object took every field as optional. The agreed enum makes each form's fields
  required: `Spec` deserializes through `Fields` (no unknown field), and `TryFrom<Fields>` gives the
  typed `Format`, `Both`, `Neither` or `Lacks(field)`, the same reasons a pin's parse reports. Devin's
  pin (`{format}`) then needs `strip: 0` and its totals, and pi's its tree's totals and its members'
  SHA-256 and size; step 25's coordinator took that on for Devin, and on step 28's re-pin for pi.
- **Totals.** `entries` counts each directory and regular file, the strip's leading directories
  included, tar's metadata headers not: what `tar -t` lists. `unpacked_bytes` sums the regular files'
  sizes, as `tar -tv` lists them. Both are held to the pin as entries come (the entry that takes a total
  past the pin's is refused before it's written, so nothing past the pinned bound is written, R03) and
  at the end.
- **Modes.** 0755 when the owner's execute bit (S_IXUSR) is set, else 0644, for every file and
  directory a tree writes (step 27's rule), instead of step 25's `mode & 0o777`. Directories the
  archive only implies and the directories between staging and a deeper target get 0755; the root gets
  its leading entry's, else 0755. Set explicitly, so neither the archive's other bits nor the umask
  reach the context. A member keeps the pin's `mode`, else its own as a tree's entry would get it. The
  pinned archives above hold only 0755/0644 entries, so no real pin unpacks differently.
- **Refusals added:** the sticky bit (`Sticky`, entries' and pins' modes), with set-ID (`SetId`)
  refused rather than cleared as before; a leading directory the strip drops that isn't the one before
  it, a directory's too (`Prefix`); a file the strip leaves no name (`Nameless`); the same path twice
  (`Duplicate`, its own variant now, not `Exists`); `harness.json` at the root of a tree at `.`, ASCII
  case aside as case-insensitive file systems take names (`HarnessFile`).
- **Errors** stay step 25's shape, `Unpack { url, entry, why }`: `entry` names the entry at fault (none
  when the fault is the pin's, the target's or a total's at the end), `why` is the typed reason (R04).
- **Failure leaves nothing:** staging is removed whole, its directories made the owner's to search and
  change first (a directory kept at 0644 would otherwise keep `remove_dir_all` from what it holds), and
  the archive is removed either way, as before.

Literals and their sources: `0o6000` (S_ISUID | S_ISGID) and `0o1000` (S_ISVTX), POSIX `<sys/stat.h>`;
`0o100` (S_IXUSR) and 0755/0644, step 27's rule; `0o700`, the owner's search and change, only for
removing a failed staging directory; `tar.gz`, the one format; `.<name>.unpack-<UUIDv7>`, staging's
name; `harness.json`, OSI's harness file. No limit is invented: the bounds are the pin's totals.

## Tests

Where: `kiln run` couldn't sync this 648 MB worktree (17,162 files) to either runner: "did not answer
the sync in 60s" three times, runs r2 and r4 on adacachy2 and `evidence/run-macstudio-1.log` on
adas-mac-studio.local-2 (a fourth, r7, was stopped while it synced). Each attempt left the runner a copy of the tree
at 94639db7b with 416 tracked files missing and this step's files whole (their SHA-256 checked equal on
both). The missing files were restored there from that copy's own git objects (`git ls-files -d |
xargs git checkout HEAD --`, after which `git status` shows this step's files alone), and every run
below ran there with `--no-sync`, files sent later through stdin and their SHA-256 compared
(`push.sh`, kept outside the repository). Each log names its host, toolchain, tree and files' SHA-256:
unpack.rs c839318f8d148e5b23106aaf1af5b8050606df9a8987ca15b8a77da5ddfe2838, unpack/tests.rs
c0f93809d48f83aeb047afb15e1e457ed43e4a0e7b3611c665bf75b7513cf1cb, mod.rs
c882e23c7355c0167d74e36f818dc5dc77e2f4526b57d50fa4366e6142bab1f2, on 94639db7b.

`run-tests.sh` (`cargo test --locked -p kiln --lib agentfiles::unpack`, nice 10, -j 4, cargo set as
.kiln/ci.yaml's rust job sets it):

- Linux x86_64, adacachy2 (runner 00aad8daff2fa3c9daa65d53c425d450, Linux 7.3.3-1-cachyos, rustc
  1.98.1), 2026-11-12T14:53:58Z: 13 passed, 0 failed (`evidence/run-linux-1.log`; build 8 m 49 s).
- macOS arm64, adas-mac-studio.local-2 (runner d1323898b255a3ed435eb83fa0f7b0cf, Darwin 25.4.0, rustc
  1.99.1), 2026-11-12T14:52:49Z: 13 passed, 0 failed (`evidence/run-macstudio-2.log`; build 9 m 14 s;
  its one warning is the linker's on the test binary's unwind table, none of this change's).

`run-clippy.sh` (kiln's gate, `ci/gate.sh -p kiln --cpus 4 --mem 12g clippy`: `cargo clippy --locked -p
kiln --all-targets -j 4 -- -D warnings` in a systemd unit capped at 12G), Linux x86_64, adacachy2,
rustc and clippy 1.98.1, 2026-11-12T15:04:01Z: `GATE: PASS`, passed in 290 s, no warning
(`evidence/clippy-linux-1.log`). Its anonymous memory peaked at 11.7 GiB against the 12G cap, swapping
11.4G, so a next gate of this step asks mem=16g. Cargo.lock is unchanged (no dependency added: tar
0.4.46, flate2 1.1.10, sha2, hex, uuid and serde were kiln's already; tempfile is its dev-dependency).

The 13 tests (`crates/kiln/src/agentfiles/unpack/tests.rs`), against the step's list:

| Test | What it holds |
|---|---|
| `a_strip_1_tree_appears_whole_with_its_files_directories_and_modes` | the good archive (Cursor's shape: dist-package/, nested directories, executables): exactly its files and directories under the per-arch directory, 0755/0644 whatever the archive's other bits (0700, 0444, 0600, 0664, 0640, 0750, 0555 among them), implied directories 0755, the root dist-package/'s; no staging directory, no archive; harness.json goes in after |
| `the_per_arch_directory_appears_only_once_the_whole_archive_is_in` | the archive through a FIFO, half written: staging is there, the per-arch directory isn't; all written: the per-arch directory is, staging isn't |
| `nothing_of_a_tree_shows_until_all_of_it_is_in` | a link as the last entry; entries one above and one below the pin's, bytes one above and one below: each refused (`Entries`, `Bytes`), at the entry that passes the pin's total or at the end; nothing left |
| `each_entry_a_tree_may_not_hold_is_refused_naming_it_and_leaves_nothing` | `/x`, `../x`, `dist-package/../../x`, a symlink, a hard link, a character device, a block device, a FIFO, a setuid file, a setgid directory, a sticky directory and file, a duplicate path, entries outside the strip prefix (a file's and a directory's), a file the strip leaves no name, harness.json at the root (and `Harness.JSON`), a contiguous file and an unknown typeflag: each its typed `Why`, naming the entry; nothing left |
| `a_target_already_there_is_refused_and_left_as_it_was` | `Exists(".")`, the per-arch directory untouched |
| `a_tree_at_a_path_inside_appears_whole_beside_what_is_there` | a tree at `opt/cursor`, staged beside `opt` |
| `devins_shape_unpacks_at_strip_0_with_nothing_beside_it` | strip 0 with `./`, a pax global header, a GNU long name and a pax path, none counted as an entry; no marker |
| `a_pi_shaped_build_unpacks_its_tree_then_picks_its_members` | pi's tree, then bin/rg and bin/fd as members held to their SHA-256 and size, past a link and a set-ID file; a colliding member refused as `Exists` |
| `a_member_is_one_regular_file_held_to_its_pin` | missing, a link, a directory, set-ID and sticky entries and pinned modes, another size, another digest: each typed, nothing written |
| `the_unpack_object_is_one_form_or_refused` | both member and strip (two ways), neither, a field lacking (three ways), another format: each typed through `TryFrom<Fields>`, and the same reason from a pin's parse; an unknown field; a mode for a tree, a path or member outside, a member at `.` |
| `what_is_not_a_gzip_tar_is_refused_whole` | `Io`, no entry named |
| `an_archive_cut_short_leaves_nothing_behind` | a tree and a member from a truncated archive: `Io` naming the entry, nothing left |
| `a_directory_its_owner_may_not_search_is_kept_and_still_discarded_whole` | a 0600 lead and 0640 directory kept 0644 with their file; a staging tree holding 0644 directories discarded whole |

## Mutations

`mutate.py` (this folder) removes each guard this step adds, one at a time, rebuilds and runs the test
named beside it, which must fail; the module is rewritten with its own text after each, and the whole
suite runs once more at the end. macOS arm64, adas-mac-studio.local-2, 2026-11-12T15:03:13Z, 19 min:
14 of 14 killed, the unmutated suite 13 passed, unpack.rs back at c839318f…
(`evidence/mutate-macstudio.log`).

| Mutant | Guard removed | Failing test |
|---|---|---|
| staging-is-the-target | the staging directory (unpack straight into the target) | the_per_arch_directory_appears_only_once_the_whole_archive_is_in |
| entries-bound-removed | entries held to the pin as they come | nothing_of_a_tree_shows_until_all_of_it_is_in |
| bytes-bound-removed | bytes held to the pin as they come | nothing_of_a_tree_shows_until_all_of_it_is_in |
| entries-total-removed | entries held to the pin at the end | nothing_of_a_tree_shows_until_all_of_it_is_in |
| bytes-total-removed | bytes held to the pin at the end | nothing_of_a_tree_shows_until_all_of_it_is_in |
| sticky-allowed | the sticky bit refused | each_entry_a_tree_may_not_hold_is_refused_naming_it_and_leaves_nothing |
| prefix-unchecked | one leading directory | each_entry_a_tree_may_not_hold_is_refused_naming_it_and_leaves_nothing |
| duplicates-allowed | the same path twice refused | each_entry_a_tree_may_not_hold_is_refused_naming_it_and_leaves_nothing |
| harness-file-allowed | harness.json at the root refused | each_entry_a_tree_may_not_hold_is_refused_naming_it_and_leaves_nothing |
| nameless-file-dropped | a file the strip leaves no name refused | each_entry_a_tree_may_not_hold_is_refused_naming_it_and_leaves_nothing |
| modes-as-archived | 0755/0644 (modes kept as `mode & 0o777`) | a_strip_1_tree_appears_whole_with_its_files_directories_and_modes |
| target-there-unchecked | a target there already refused | a_target_already_there_is_refused_and_left_as_it_was |
| neither-taken | neither form refused | the_unpack_object_is_one_form_or_refused |
| discard-unsearched | staging's directories made searchable before removal | a_directory_its_owner_may_not_search_is_kept_and_still_discarded_whole |

The scripts as committed: `run-tests.sh` prints each file's SHA-256 with whichever of `sha256sum` and
`shasum` a host has, a change made after the two test runs above, which ran it at
30bb92947c160ff0bdd7a1d48fa250558307cd0c45bbd30b1c9744788f802308 (the same cargo command; on macOS its
`sha256sum` lines printed nothing). `run-clippy.sh` and `mutate.py` ran as committed.

## R01-R16

- R01: no `unwrap`, `expect`, `panic!`, `unreachable!` or indexing in the module (`get(..n)` and
  iterators instead); totals add with `saturating_add` and compare, a timestamp with `checked_add`;
  kiln's workspace lints deny the rest, and clippy `-D warnings` on `-p kiln --all-targets` is the
  check (Tests, below). A malformed archive or pin is an `Unpack` error value, never a crash.
- R02: the sources above (tar 0.4.46's own code, rename(2), the measured pins); step 25's design and
  its alternatives weighed against this step's acceptance; the decision recorded with each's reason.
- R03: no limit is invented. The bound is the pin's own totals, measured from the archive at pin time
  (step 1), checked as entries come. Each literal's source is listed above.
- R04: typed refusals throughout (`Why`, callers match its variants; `Unpack` names the entry). One
  call runs one order (the pin's form, the target, the entries into staging with their checks, the
  totals, the modes, the rename, the archive's removal), with one failure path that undoes what it
  made; `Spec` is one of two forms, never a mix of optional fields.
- R05: one function at the seam the fetcher calls (`tar_gz`) and the pin object it reads; no trait, no
  layer, no cache, no wait or poll.
- R06: one focused function per step (`first_missing`, `fill`, `taken`, `Seen::admit`, `modes`,
  `discard`, `one`, `copy`); no flag combinations.
- R07: comments give sources, invariants and decisions; none repeats the code.
- R08, R14: no performance claim; profiling is the coordinator's. (The member's digest is computed only
  where a pin asks for it; a tree's files are copied without hashing.)
- R09: no defect is fixed here; each refusal is shown on a crafted archive, each guard by a mutant.
- R10: this record, the scripts that ran the tests and mutants, and their logs (`evidence/`).
- R11: the code is std, tar and flate2 alone, the same on every platform but for setting modes, which
  are Unix's. Tests ran where Tests says; Windows is unverified (no modes are set there, so a context
  made on Windows has none).
- R12: one unpacker, step 25's, converted rather than a second written; kiln's own tar and flate2;
  Cargo.lock unchanged.
- R13: the step's contract and no more: no fetcher, no download, no pin file.
- R15: implementer, light tests only; commits on this branch, no merge, no push (landing is step 6's).
- R16: build output under the runners' `~/.cache/s27-unpack/`; no evidence there.

## Owed

- The materialize hook, the HTTP light-integration test (a pinned tar.gz served by kiln's own HTTP
  server; a wrong digest and a short body refused as Digest and Size before any entry is read, nothing
  left) and the live `kiln agentfiles context cursor DIR --platform linux/amd64`: they wait for step
  19's fetcher on main or its branch.
- Step 25's extension: not committed on `s25-unpack` when this was (its working copy unchanged since
  02:35 PST). When it is, merge it into this branch keeping this unpack.rs (the agreed conversion; its
  02:35 copy, sha256 097897d59db246b83d6f37cc170de4e1cb77e1dbfc3e81a9cdcb4e8d823033fa, is what this
  started from), take its docs as they are, and re-run the module's tests.
- The pins the enum now asks of others: Devin's `strip: 0` and its totals (step 25's coordinator took it
  on), pi's tree totals and its members' SHA-256 and size (step 28's re-pin), and Cursor's own totals
  as Tree defines them (step 1: entries as `tar -t` lists them, 435 for x64; unpacked_bytes the files'
  sizes summed).
- clippy natively on macOS and Windows, and Windows's tests (the gate; full runs on the person's word).
