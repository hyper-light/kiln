# Plan step 20 [kiln], step 1: Codex pinned (evidence)

Plan 01a1ce705d7076648c1d9f9ee26214d3 (step 20 [kiln], the Codex Agentfile), step 1 of 6; research
01a1ce705d71770686ed0e7e45e6bcdc. This records what step 1 built, what it ran and what changed on the
way. Step 6 joins it into the step's evidence record.

## Identity

- Role: implementer, Shards ROOT's step 20 agent (01a1ce6eb22f76988397e3264f4b3864).
- Policy readback: POLICY_READ.json. Rechecked on 2026-11-12, before this commit:
  - Shards origin/main 8f3cd5b74: development-rules.md, AGENTS.md and development-evidence.md have the
    same SHA-256 as read.
  - kiln origin/main 94639db7b: AGENTS.md the same; CLAUDE.md is still `@AGENTS.md`.
  - kiln's docs/ci.md changed as a whole, but its §0 (lines 59-219, SHA-256 prefix 1e464890a193c0d1)
    and §0.3 (f301e1d408763791) are byte-identical from 1a5a5f00d to 94639db7b.

## What it is

`.kiln/images/harness-codex/pin.sh` writes `pin.json` beside it. Its usage is in the script's header.

`verify` does the following, in order:

1. The release is `rust-vX.Y.Z`, neither a draft nor a prerelease.
2. cosign is the version OpenAI's linux-code-sign action installs at the tag, checked against
   sigstore's `cosign_checksums.txt`.
3. `codex-package_SHA256SUMS` matches GitHub's digest.
4. For each platform, before anything inside an archive is read:
   - the archive's SHA-256 and size match GitHub's, and its SHA256SUMS line;
   - its bundle's SHA-256 matches GitHub's;
   - `cosign verify-blob` passes with the identity `rust-release.yml@refs/tags/<tag>` and GitHub Actions'
     issuer, the TUF root in the directory of its own.
5. Then the archive checks (`pin.sh inspect`):
   - every member is a regular file or a directory, inside, with no set-ID bit;
   - codex-package.json says layoutVersion 1, the tag's version, the platform's target and entrypoint
     bin/codex;
   - bin/codex, bin/codex-code-mode-host, bin/codex-exec-server and codex-resources/bwrap ask no ELF
     interpreter.
6. Where the package's codex-path/rg does ask one (aarch64: `/lib/ld-linux-aarch64.so.1`), ripgrep's
   static aarch64-unknown-linux-musl archive is pinned:
   - its version is the one OpenAI's `scripts/codex_package/rg` names at the tag (15.2.0);
   - its SHA-256 and size match GitHub's, and its SHA-256 matches ripgrep's `.sha256`;
   - its one top directory holds an rg that asks no interpreter, whose own SHA-256 and size are pinned.
7. pin.json is written to a temporary file and renamed into place.

## The pin (rust-v0.167.2, read 2026-11-12T13:53:10Z)

| platform | file | sha256 | bytes | path, unpack |
|---|---|---|---|---|
| linux/amd64 | codex-package-x86_64-unknown-linux-musl.tar.gz | 3d77be17…7b3d | 200235673 | `.`, tree, strip 0 |
| linux/arm64 | codex-package-aarch64-unknown-linux-musl.tar.gz | 56b08c12…0695 | 187980188 | `.`, tree, strip 0, skip `codex-path/rg` |
| linux/arm64 | ripgrep-15.2.0-aarch64-unknown-linux-musl.tar.gz | 800b1e72…0915 | 1982561 | `codex-path/rg` 0755, member `ripgrep-15.2.0-aarch64-unknown-linux-musl/rg` (sha256 c14cdb38…386a, 4466000 bytes) |

The verification record names cosign v3.1.4 (cosign-linux-amd64, sha256 424e2c97…bbfa), the identity,
the issuer, SHA256SUMS' URL and digest (4f19652c…1986), and OpenAI's ripgrep pin
(`scripts/codex_package/rg` at the tag, sha256 41a10ca6…e1f1). The base is step 19's
buildpack-deps:trixie-scm by digest.

## Runs

Both runs went through `scripts/run-step1.sh OUT latest` as a `systemd-run --user` unit on kiln-a094a12d
(Fedora, Linux 6.19.10-300.fc44.x86_64, x86_64). Their outputs came back as gzip and base64, and their
SHA-256 matched the runner's.

### Run 1 (red): `evidence/e_step1_run1.txt`, 2026-11-12T10:27:20Z

Its control pinned and its 16 mutations were refused (MUTATIONS passed=17). But "another tag's identity" (the release's tag set to
rust-v0.167.1) was refused by the package version check ("codex-package.json says version '0.167.2',
not '0.167.1'"), because verify read the archive before cosign. So no case showed cosign's identity
pin refusing anything.

The fix:

- cosign moved ahead of the archive checks, as the plan's step lists them. No package is parsed before
  its signature holds.
- Each mutation now must match a pattern naming the check it aims at.
- The plan's "layout" and "link" cases, which run 1 lacked, were added as crafted layoutVersion 2 and
  hard-link archives, along with a ripgrep archive with two top directories.

### Run 2 (green): `evidence/e_step1.txt`, 2026-11-12T13:52:51Z

- fetch, verify, mutations and fresh downloads all exited 0.
- MUTATIONS passed=20 failed=0: the control pinned with the expected shape, and 19 mutations were
  refused, each by its named check.
- "another tag's identity" is now cosign's: "none of the expected identities matched what was in the
  certificate".
- No crafted package carries OpenAI's signature, so verify's cosign refuses every one ("matching bundle
  to payload"). Each is also given to `pin.sh inspect`, which refuses it by name:
  - symlink and hard link: not a regular file or a directory;
  - absolute and `../` paths: leaves the archive;
  - set-ID: mode 4755;
  - another version, target or layoutVersion.
- In each refusal, pin.json was unchanged and no temporary file was left.
- FRESH archives=3 matching=3 members=1 matching=1: both packages and the ripgrep archive downloaded
  again with their pinned SHA-256 and size, and the rg member in the fresh ripgrep download is the
  pinned one.

## What changed from the plan's text, and why

- **The unpack shape is step 25's, not the one step 21 and this step agreed.** Step 21's agent wrote on
  2026-11-12 that step 25 had committed an unpack first (`crates/kiln/src/agentfiles/unpack.rs`,
  a04cffd0c on s25-unpack), that step 28 uses it, and that Gemini had moved to it, dropping
  {strip, to, only}. The shared layout's file is {url, sha256, size, path, mode, unpack} (step 19's plan,
  layout item 3), and step 25's working copy reads `unpack` as a tree {format, strip?} at `path` or a
  member {format, member, sha256?, size?} written at `path` with `mode` (unpack.rs in its worktree,
  uncommitted, sha256 097897d59db246b83d6f37cc170de4e1cb77e1dbfc3e81a9cdcb4e8d823033fa). pin.json
  follows that.
- **arm64's rg needs a `skip` (or a `replace`) that step 25's module doesn't have yet.** Codex runs
  codex-path/rg whenever it is a file. See `rg_command` in its install context at rust-v0.167.2, lines
  151-172 of `evidence/pages/codex-rust-v0.167.2--install_context_lib.rs`: a static rg anywhere else is
  never used. Step 25's module never overwrites (`create_new`, `Exists`). So the package tree carries
  `skip: ["codex-path/rg"]`, the rg member fills that path, and nothing is overwritten. On 2026-11-12 I
  asked step 25's coordinator (01a1ce6f55c3749ca87f3ef3393ec8b0) to choose between that `skip` (my
  recommendation) and a per-file `replace: true`, and who writes it. No answer yet. If it is `replace`,
  one jq expression in pin.sh changes and pin.json is written again.
