# Steps 2 and 4, checked before what they wait for landed

`run-premerge.sh` (sha256 d3818eac445bd8ea5c01560ab822eff153497bfb0db393510ade53f0dcc10b78) on kiln-a094a12d (runner
8fadc1ec213332e16cb0d915a3fb913f, Fedora, Linux 6.19.10-300.fc44.x86_64, rustc 1.99.1), 2026-11-12 17:52-18:41 UTC, as
the transient systemd user unit s27pre-1794505932; its log is `premerge.log` (sha256
a0e76633cd57f658ad6bfbeab3075930eb06665310f69b40ef9f647b9af25646, checked as it came back).

What it tested: s27-cursor-agentfile at 63dd9bafd (Cursor's row) merged with step 28's `s28-unpack-hook` at
8e571d7d8 (the one unpacker, 4d1f96a5d, and the fetcher's hook, 73fc55df9), neither on kiln main then. The merge
was clean: tree 02d477040e13f43a37b39154921aabd1e1a14ffb.

- `cargo test --locked -p kiln --lib agentfiles:: -j 4`: 60 passed, 0 failed. Among them the unpacker's own
  tests (Cursor's shape, `a_strip_1_tree_appears_whole_with_its_files_directories_and_modes`; wrong counts,
  `nothing_of_a_tree_shows_until_all_of_it_is_in`; a nameless file, a sticky bit and a root harness.json,
  `each_entry_a_tree_may_not_hold_is_refused_naming_it_and_leaves_nothing`), step 28's hook's, step 19's
  fetcher's over every built-in, and Cursor's five.
- kiln's own binary, built from that tree, made Cursor's live context from Cursor's own archives:
  `kiln agentfiles context cursor DIR --platform linux/amd64,linux/arm64`, exit 0 in 18 s. DIR held the Agentfile
  (the embedded one) and cursor-amd64/ and cursor-arm64/, each with the embedded harness.json at 0644 and
  cursor-agent a 0755 file, and each exactly the pin's tree:

  | Directory | Entries (directories and files, harness.json aside) | Bytes (files, harness.json aside) | Pinned | Modes |
  |---|---|---|---|---|
  | cursor-amd64 | 435 | 562236246 | 435, 562236246 | 82 directories 0755, 329 files 0644, 24 files 0755 |
  | cursor-arm64 | 435 | 552211752 | 435, 552211752 | 82 directories 0755, 329 files 0644, 24 files 0755 |

  Nothing else was there (no link, device or FIFO), and the modes are the archive's own
  (`../../evidence/archive-entries-x64.txt`: 329 `-rw-r--r--`, 24 `-rwxr-xr-x`, 82 `drwxr-xr-x`).
- The package's own `cursor-amd64/cursor-agent --disable-auto-update --version`, on this x86_64 host with HOME
  an empty folder: `2026.11.11-b912975`, the pin's version. It wrote 13 entries into that HOME (not listed).

This is not step 2's condition, which needs the unpacker on kiln main and its test at main's revision
(`../landed/`); it shows ahead of that that Cursor's row and archives fit the unpacker as it was about to land.
