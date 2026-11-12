# Step 4: Antigravity's image in microVMs, both platforms

Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 26, its plan's step 4. These are the records of
`scripts/step4/step4.sh` for agy 1.5.7, the version the pin holds since c0a853f8. The runs used a
`shards` that `scripts/step4/build-shards.sh` built from Shards main
8f3cd5b7411dfeea47956a5143eec2c3466b058f (`shards version`: 0.7.0, commit 8f3cd5b). That commit
contains 997f4e47e (run.env HOME) and 1da9132b7 (harness.json startup). Both builds used Shards'
test-release profile, since these are smokes, not measurements of shards.

agy 1.5.6's runs, with agy's own logs, are this directory at commit 3423c8df. They gave the same
results, but with `--version` writing 31 2e 35 2e 36 0a.

| Directory | Where | shards SHA-256 | Phases | Archive SHA-256 |
|---|---|---|---|---|
| `linux-amd64/` | kiln-a094a12d (Fedora 44, Linux 6.19.10, x86_64, KVM) | 16ba8a13c570bfd253533a4127bc4ffa9d5a2d82524cddd2fb31e0831a14d412 | build, index, smoke, harness, probe, control, bases | aafb818dd9a2b3b115b03c9277580b6f926a55f924ea33d3e43a991788db0723 |
| `mac-arm64/` | adas-mac-studio.local-2 (M3 Ultra, macOS 26.4, Hypervisor.framework) | c4a2b97aab9eb72141335705c1004d1ab58fcefd63a5c766e4bc01f9b43bafdb | build, index, smoke, harness, probe, control | d99d56a3e45c6affe2f1bbbfe1626ba789f6bcec2b1eee9e226814ffe68a7b19 |

The last column is the SHA-256 of the archive each directory was copied from.

In each directory:

- `record.txt` is what the script said, in order.
- `*.log` is each command's whole output.
- `smoke.out` holds the smoke's stdout bytes, and `smoke.err` its stderr.
- `proj-*/probe/` holds the probe's files, as the run's project volume holds them on the host.
- `homevol*/` holds agy's config from the run's home volume, also on the host.

In those copies, `.gemini` is renamed `dot-gemini` and `.gitconfig` `dot-gitconfig`, so that no
tool run in this repository reads them as its own configuration. They are byte for byte what the
runs wrote.

The two runs' contexts hold the same files: each `bin/agy` is the pin's member, and harness.json
is the embedded one. Their images are separate builds, however. Each is one OCI index holding both
platforms (Linux cbab89ce…, Mac a6f44701…), and the two hosts' manifests differ by a few bytes per
platform.
