# Step 4: Goose's image, built by a real Shards for both platforms and run in microVMs

Plan 01a1ce705e3d72af83ef15ae83b62f45, step 4 (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 23),
2026-11-12. The image is the one kiln's table embeds at kiln main 172176b23: `.kiln/images/harness-goose/`'s
Agentfile, harness.json and pin.json.

## How it ran

- **Shards.** [build-shards.sh](build-shards.sh) (sha256 `646b3034…` on both runners) builds the whole `shards` from Shards main
  `8f3cd5b7411dfeea47956a5143eec2c3466b058f`, which holds 1da9132b7 (a harness starts from its
  harness.json; v0.7.0 lacks it) and 997f4e47e (run.env sets a harness's HOME and PATH). It uses Shards'
  pinned Rust 1.98.1, zig 0.16.0 checked by digest, `cargo build --locked --profile test-release -p shards`,
  and no GPU renderer. Both builds say `shards version 0.7.0, build 8f3cd5b`:
  - Linux: kiln-a094a12d (Linux 6.19.10 x86_64, KVM), 206 s, sha256 `36583dde…` ([e_build_linux.txt](evidence/e_build_linux.txt)).
  - macOS: the M3 Ultra Mac Studio, adas-mac-studio.local-2 (Darwin 25.4.0 arm64), 110 s, sha256 `66a45b4b…`
    ([e_build_mac.txt](evidence/e_build_mac.txt)).
- **The runs.** [smoke-run.sh](smoke-run.sh) carries this tree's [smoke.sh](smoke.sh), pin.json (sha256
  `abcf53e7…`), Agentfile (`509626e2…`) and harness.json (`50df9c4a…`) to a runner, and runs smoke.sh there.
  `all` ran on Linux under `systemd-run --user` ([e_smoke_linux.txt](evidence/e_smoke_linux.txt)); `arch` ran on
  the Mac through `kiln run --detach` ([e_smoke_mac.txt](evidence/e_smoke_mac.txt)). Each run had a fresh
  SHARDS_HOME, an empty DOCKER_CONFIG and SHARDS_LOCAL_STORE=none, as Shards' harness bring-up has them.
- **The context.** smoke.sh assembles it the way `kiln agentfiles context goose` will: each pinned archive,
  its size and SHA-256 checked before anything reads it, then its one regular member streamed to
  `goose-<arch>/bin/goose`, mode 0755, checked against the pin's member digest and size. That fetcher is step
  19's and isn't written yet; its unpacker is step 25's and hasn't landed. Step 3 holds the fetcher's context to
  the same pin.

## Results, by the step's acceptance criteria

1. **One OCI index with linux/amd64 and linux/arm64 manifests.** `shards images --tree` lists `harness-goose:smoke`
   with `linux/amd64` and `linux/arm64` under it, on both hosts (index `936cdfe0a043` on Linux, `d0d4bfd16cdb` on
   the Mac).
2. **The smoke prints the pin's line on each arch.** `shards run vm --rm --platform linux/<arch> harness-goose:smoke
   /harness/goose/bin/goose --version` wrote exactly `20 31 2e 35 39 2e 30 0a`: pin.json's smoke line, " 1.59.0",
   and its newline. That held on linux/amd64 (Linux, KVM) and on linux/arm64 (the M3 Ultra, Hypervisor.framework).
   D115 let the run's own command run, so the probe's version file wasn't needed in its place.
3. **The harness starts as harness.json declares.** Run with the image's own harness.json (`shards run vm -d … sleep
   120`), `shards read processes` shows the member `harness goose` exec `/harness/goose/bin/goose acp` on both hosts.
   With no ACP client attached, its stdin ends and it exits 0, as the planning run measured; in use, kiln is that
   client. The process record holds each exec's command line, not its environment. GOOSE_PATH_ROOT shows in what
   `goose acp` wrote, `config/`, `data/sessions/sessions.db` and `state/logs/cli`, all under the host folder bound
   at `/home/goose/.local/share/goose`, and in the probe's environment (below).
4. **The probe** (Linux; harness.json's command replaced by `sh -c PROBE`, its env kept):
   - its uid is 200000, and its environment holds `GOOSE_PATH_ROOT=/home/goose/.local/share/goose`, `HOME=/tmp` and
     `PATH=/harness/goose:/harness/goose/bin:…`;
   - it wrote the version file, " 1.59.0", into PROJECT, and `goose info -v` puts Goose's config, sessions database
     and logs under the root;
   - the host's ROOT gained `config/`, `data/` (`apps/`, `sessions/`) and `state/logs/`;
   - `goose run --no-session -t hi` with a placeholder key got "Authentication failed for
     https://api.openai.com/v1/responses. Status: 401 Unauthorized": DNS and TLS reached the provider;
   - a TCP connect to 1.1.1.1:80 got nothing: it timed out (exit 124).
5. **The control** (the probe without the three network lines) reached nothing: no name lookup, Goose's "Network
   error: Could not connect to api.openai.com", and port 80's connect refused (Permission denied).
6. **Other bases**, the probe on each, by digest:
   - Alpine (`alpine:3.24@sha256:f59abe65…`): Goose runs with busybox sh, and the provider answers 401. The probe
     used `nc`, as there is no bash.
   - debian:trixie-slim (`sha256:a72b4433…`): the version prints, then HTTPS fails with "Error builder error.". That
     is the missing CA bundle (research f18), whatever the keychain advice Goose prints after it says.
7. **Secrets.** The image's env is `PATH` alone, nothing it adds holds a credential, and its build reports no
   SecretsUsedInArgOrEnv. A copy of the Agentfile given `ENV OPENAI_API_KEY=placeholder-not-a-key` draws `SecretsUsedInArgOrEnv:
   Do not use ARG or ENV instructions for sensitive data (ENV "OPENAI_API_KEY") (line 29)`, and the scan finds it.

## Single observations, not claims

- Builds: both platforms with the base pulled, 28.4 s on Linux and 273 s on the Mac. One platform each on Linux:
  the probe 5.4 s, the control 1.6 s, Alpine 3.1 s, trixie-slim 4.6 s, the secrets copy 1.1 s.
- Smoke runs: 4.45 s on Linux and 16.1 s on the Mac, each the host's first run, which downloaded its guest
  kernel (`vmlinux-6.18.48-x86_64` and `Image-6.18.48-aarch64`, both of kernel-6.18.48-17c3ba205400).
- Sizes: the context is 289 MB (`goose-amd64` 147 MB, `goose-arm64` 142 MB); the image's content is 194 MB for
  linux/amd64 and 193 MB for linux/arm64 (`shards images --tree`).
- The two hosts' builds of one context differ: index `936cdfe0` against `d0d4bfd1`, each platform's size 34 bytes
  apart (194010006 and 194009972; 192909315 and 192909281). Not looked into here: step 5's CI builds one index,
  which image() serves by digest.
- One download on the Mac failed to resolve release-assets.githubusercontent.com once (curl exit 6); curl's retry
  fetched it, and its digest checked.

## Not run

- Linux arm64 with KVM (no such host here), and Windows hosts.
- A real provider turn: it needs a credential (step 33, ROOT's step 35).
- The probe, the control and the other bases on arm64: the plan has them on Linux.
- The fetcher's own context: step 3's.
