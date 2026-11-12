# Step 27's merge with kiln main, tested at its tree

The person's rule (ROOT's goal for step 27, 2026-11-12): test only the change, with unit tests and light
integration tests of the code it touches; run kiln's clippy gate on Linux, macOS and Windows where a runner has
room now; merge to kiln main and end.

The tree: s27-cursor-agentfile at b85b17c16 merged with kiln main at 507069d4b, tree
005013f624a9b2c2672f962cf100e1f19ece3d64. `run-merge.sh` (sha256 dff9f9e2…) made the same merge from a git bundle
on each runner, as scratch commits, and refused to run on any other tree; every log below says the tree it
tested. The signed merge here, 5af829a8a, has that tree; this record's commit adds only docs/research files,
which no kiln test reads.

| Where | What | Result | Log (sha256) |
|---|---|---|---|
| kiln-a094a12d, Linux 6.19.10 x86_64, rustc 1.99.1 | `cargo test --locked -p kiln --lib agentfiles:: -j 4` | 45 passed, 0 failed | `linux-tests-and-16g-gate.log` (0c8cf0a4…) |
| the same | `cargo test --locked -p kiln --test agentfiles -j 4` | 1 passed, 0 failed | the same |
| the same | `ci/gate.sh -p kiln --cpus 4 --mem 16g clippy` | killed at its ask: SIGTERM with 15.8 GiB anonymous and 2.9 GB swapped (Fedora's systemd-oomd ends a unit held at its limit) | the same |
| the same | `ci/gate.sh -p kiln --cpus 4 --mem 24g clippy` (CI's rust job's 24Gi) | `GATE: PASS` in 712 s; anonymous memory peak 22.6 GiB, 2.2 GB swapped | `linux-gate-24g.log` (e394e1d5…) |
| adas-mac-studio.local-2, Darwin 25.4.0 arm64, rustc 1.99.1 | `ci/gate.sh -p kiln --cpus 8 --mem 16g clippy` (uncapped on macOS) | `GATE: PASS` | `macos-gate.log` (461af470…) |
| jupiter, Windows (MINGW64_NT-10.0-26200) x86_64, rustc 1.99.0 | `ci/gate.sh -p kiln --cpus 8 --mem 16g clippy` (uncapped on Windows) | `GATE: PASS` | `windows-gate.log` (8d285e47…) |

Measured for whoever asks next: kiln's clippy gate at this revision peaks at 22.6 GiB of anonymous memory at
-j 4 on Linux, with 2.2 GB swapped under a 24G cap, so it needs an ask of 25g. Step 2's 11.7 GiB of an earlier
tree no longer holds.

## Again, as main moved: kiln main at 378327f49, with step 26's Antigravity row

Main took step 26's harness-antigravity (378327f49) before this landed. `builtin.rs` conflicted, both rows following
harness-claude's: `ALL` keeps goose, claude, antigravity and cursor in that order, and both test modules. The
surface test merged as one, step 26's fix being step 20's blob (d4deb53a7). `run-merge.sh` (sha256 a1038b09…) took
the resolution from `resolved/` and the main the bundle pinned (`refs/s27/test-main`); each run below made the merge,
placed it and held the tree to f03a163784ae793978c5bd825fdd7c0db5750efa, the tree of the signed merge 67f02926e.

| Where | What | Result | Log (sha256) |
|---|---|---|---|
| kiln-a094a12d, Linux x86_64, rustc 1.99.1 | `cargo test --locked -p kiln --lib agentfiles:: -j 4` | 52 passed (Antigravity's seven with them), 0 failed | `linux-2.log` (eacb3f95…) |
| the same | `cargo test --locked -p kiln --test agentfiles -j 4` | 1 passed, 0 failed | the same |
| the same | `ci/gate.sh -p kiln --cpus 4 --mem 24g clippy` | `GATE: PASS` in 58 s (the target warm), anonymous peak 13.3 GiB | the same |
| adas-mac-studio.local-2, macOS arm64, rustc 1.99.1 | `ci/gate.sh -p kiln --cpus 8 --mem 16g clippy` | `GATE: PASS` | `macos-2.log` (aeed5790…) |
| jupiter, Windows x86_64, rustc 1.99.0 | `ci/gate.sh -p kiln --cpus 8 --mem 16g clippy` | `GATE: PASS` | `windows-2.log` (2137c95a…) |

Not run here, the person's rule leaving them to ROOT: image builds, smokes and microVM runs (`../step4/`), and
the whole workspace's tests.
