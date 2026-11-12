# Step 4: Antigravity's image in microVMs, both platforms

Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 26, its plan's step 4. These are the records of
`scripts/step4/step4.sh`, run with a `shards` that `scripts/step4/build-shards.sh` built from Shards
main 8f3cd5b7411dfeea47956a5143eec2c3466b058f (`shards version`: 0.7.0, commit 8f3cd5b). That
commit contains 997f4e47e (run.env HOME) and 1da9132b7 (harness.json startup). Both builds used
Shards' test-release profile, since these are smokes, not measurements of shards.

| Directory | Where | shards SHA-256 | Phases |
|---|---|---|---|
| `linux-amd64/` | kiln-a094a12d (Fedora 44, Linux 6.19.10, x86_64, KVM) | 16ba8a13c570bfd253533a4127bc4ffa9d5a2d82524cddd2fb31e0831a14d412 | build, smoke, harness, probe, control, index, bases, then probe and control again with the egress-by-address checks |
| `mac-arm64/` | adas-mac-studio.local-2 (M3 Ultra, macOS 26.4, Hypervisor.framework) | c4a2b97aab9eb72141335705c1004d1ab58fcefd63a5c766e4bc01f9b43bafdb | build, index, smoke, harness, probe, control |

In each directory:

- `record.txt` is what the script said, in order.
- `*.log` is each command's whole output.
- `smoke.out` holds the smoke's stdout bytes, and `smoke.err` its stderr.
- `proj-*/probe/` holds the probe's files, as the run's project volume holds them on the host.
- `homevol*/` holds agy's logs and config from the run's home volume, also on the host.

In the copies of the home volumes, `.gemini` is renamed `dot-gemini` and `.gitconfig`
`dot-gitconfig`, so that no tool run in this repository reads them as its own configuration. They
are byte for byte what the runs wrote. The archives these were copied from had SHA-256
b202229b0dc09f933f198d4f35becf29ee3386e22cfb35eceeed6dfbd443099c (Linux) and
1a22eb339e2beacbf71e4e5274de80dcf8568e160c84f9238445d8a4299c331f (Mac).

The two runs' contexts hold the same files: each `bin/agy` is the pin's member and the embedded
harness.json is unchanged. Their images are separate builds, however. Each is one OCI index holding
both platforms (Linux 1042e2ef…, Mac dfcb1095…), and the two hosts' manifests differ by a few bytes
per platform.
