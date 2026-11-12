# Step 1: Cursor's Linux builds pinned (kiln plan 01a1ce705e5c7504b88732f443d99288)

Parent: kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 27. Policy readback: [POLICY_READ.json](POLICY_READ.json)
(Shards docs/development-rules.md df2acf47…, kiln AGENTS.md 16b05ef8…). Role: implementer, under step 27's
agent (coordinator of this plan); no subagents; nothing merged or pushed (landing is step 6's).

## What this step adds

- [.kiln/images/harness-cursor/pin.sh](../../../../.kiln/images/harness-cursor/pin.sh): reads the version from
  `https://cursor.com/install`'s one `DOWNLOAD_URL` line (or takes it as its argument, one URL path segment of
  `[0-9A-Za-z._-]`, never `.` or `..`), fetches each Linux build and its `.sha256` over HTTPS alone, accepts a
  build only as its header says, and writes pin.json in step 19's shape: `version`, `version_read` (the
  installer's URL, its SHA-256, when), `verified` (`checksum: same-origin`, `signed: false`, why), `base`, and
  per build `{platform, files: [{url, sha256, size, path ".", unpack: {format tar.gz, strip 1,
  unpacked_bytes, entries}}]}`, the unpack in the Tree form agreed with step 26.
- [pin_test.sh](pin_test.sh): pin.sh's tests, releases served over HTTPS from 127.0.0.1 (and the same files
  over plain HTTP, for the refusals of HTTP).
- [mutate.sh](mutate.sh): guard-removal mutants of pin.sh, each of which pin_test.sh must fail.
- [.kiln/images/harness-cursor/pin.json](../../../../.kiln/images/harness-cursor/pin.json), with
  [pin.log](pin.log), pin.sh's run on a Linux runner as its record.

## Results

Both runs were on a Linux runner through `kiln run --needs linux/amd64` (CachyOS, Linux 7.3.3 x86_64,
curl 8.23.0 with OpenSSL 3.6.5, Python 3.14.9), from files whose SHA-256 as run equals commit b75ff50fc's
(pin.sh 210673ec…, pin_test.sh 8d7b7dea…, mutate.sh e18990dd…).

**Live** ([pin.log](pin.log), 2026-11-12T14:02:52Z). `https://cursor.com/install`, SHA-256 e8c7a37d…, the
planning copy's (so unchanged since 07:32Z), downloads 2026.11.11-b912975, the version harness.json names.

| Build | URL | SHA-256 | Size | Entries | Unpacked bytes |
|---|---|---|---|---|---|
| linux/amd64 | `…/lab/2026.11.11-b912975/linux/x64/agent-cli-package.tar.gz` | be01f42ad334f81d05a9992fcf060a52cc746f52cc451e849eb652146a6e3c91 | 176703341 | 435 (353 files, 82 directories) | 562236246 |
| linux/arm64 | `…/lab/2026.11.11-b912975/linux/arm64/agent-cli-package.tar.gz` | f068af64ed91b5304605976586f690579679f740a41f388091a6b3a2e776987c | 175384019 | 435 (353 files, 82 directories) | 552211752 |

Each URL and its `.sha256` downloaded again, apart from pin.sh: sha256sum, stat, the `.sha256`'s line and GNU
tar's entry and byte counts equal pin.json's for 2 of 2 builds, and `dist-package/` itself is one entry of
each. pin.json (SHA-256 4d5f12ae0b03dd7c9117ce92897d4fc5589777c1ab49e65985730e6eafa19a1b) and pin.log came
back byte-exact, base64 with their SHA-256 checked.

**Tests** ([tests.log](tests.log)): 52 of 52 cases, in 5 s.
- Three pin: from the installer, over an earlier pin.json, and a version asked for. Each pin.json is checked
  field by field against sha256sum, stat and GNU tar.
- 49 refuse, each with its exit status and the check named in its message, and each leaving no pin.json, no
  temporary file, and an earlier pin.json untouched. The acceptance criterion's cases:
  - a sidecar with one hex digit changed: 65;
  - a 404 sidecar: 69;
  - a sidecar naming another file: 65;
  - an installer without DOWNLOAD_URL: 65;
  - version arguments outside `[0-9A-Za-z._-]`: 64;
  - an archive holding `../x`: 65;
  - a symlink, a hard link, a FIFO and a character device: 65 each;
  - no `dist-package/cursor-agent`: 65.
- HTTP is refused where it would succeed: the same files are served over plain HTTP, and curl says
  `Protocol "http" is disabled`, and `(in redirect)` for an HTTPS answer redirecting there.

**Mutants** ([tests.log](tests.log)): 29 of 29 killed, in 153 s, each failing the case its check is for.
One finding behind a check: Python's tarfile ends an archive silently at a header it can't read, not only at
the end-of-archive blocks. So pin.sh reads what follows the last entry and refuses anything but zeros (the
`garbage-header` case, the `past-the-end` mutant).

## Definitions the next steps rely on

- `entries`: how many entries the archive holds, every one a regular file or a directory, `dist-package/`
  itself among them (it is an entry of Cursor's archives; the strip turns it into the per-arch directory
  itself). It is what `tar -tzf ARCHIVE | wc -l` counts. Step 2's unpacker counts every entry it reads,
  this one included, and must pass over the entry the strip leaves empty rather than refuse it, as GNU
  tar's `--strip-components` does.
- `unpacked_bytes`: the sum of the regular files' sizes, what GNU tar's verbose listing gives for `-`
  entries. Directories count nothing. Step 4's check must sum file sizes (`find -type f`), not `du -b`,
  which adds each directory's own size, and must leave out the harness.json materialize writes beside the
  tree.

## Rules (R01-R16) for this step

| Rule | Proof or gap |
|---|---|
| R01 | Every failure exits with its sysexits status (64 usage, 65 a failed check, 69 no 200 answer or a missing program) and a message naming the check; archive errors from Python's tarfile, gzip and zlib are caught. No Rust here. |
| R02 | The design is the plan's, on research 01a1ce705e5c7504bdcac9721fa699e3 (f5-f10: the installer, the unsigned sidecars, the archive's layout); the shape follows steps 19 and 26. |
| R03 | Every literal's source: the URLs and the `x64`/`arm64` names are Cursor's installer's; `BASE` is step 19's shared default; strip 1 is the installer's `tar --strip-components=1`; the sidecar's one line is sha256sum's format (64 hex, two spaces, the name, a newline); TLS 1.2 is RFC 8996's floor; 64/65/69 are sysexits.h's; buffers are Python's `io.DEFAULT_BUFFER_SIZE`; `entries` and `unpacked_bytes` are measured from the pinned archive. No fetch retries or timeouts, which would be chosen numbers: a failed fetch fails the pin, which is rerun by hand. |
| R04 | One pass with typed outcomes: usage, version read (argument or installer), per build sidecar then archive then measure, then one atomic write. Each path an archive makes is tracked as `file`, `dir` or `parent`, so collisions are refused by state, not by order. |
| R05 | No cache. The test server's readiness is the port it prints on its pipe, not a poll. |
| R06, R07, R13 | One script with a Python helper per check; comments say why (sources, refusals), not what. `--proto-redir` was dropped as redundant: curl(1) says `--proto` holds redirects too, and the `https-only-redirect` mutant shows `--proto` alone keeps HTTP redirects out. |
| R08, R14 | No performance claim. Run times are single observations. |
| R09, R10 | Live: pin.sh against Cursor's origins on a Linux runner, then every pinned URL downloaded again and measured apart from pin.sh ([pin.log](pin.log)). Mutation: [tests.log](tests.log), each check taken out and pin_test.sh failing at its case. |
| R11 | Run on Linux x86_64 alone (the plan's runner). pin.sh needs only bash, curl and python3; macOS and Windows runs of it are unverified. |
| R12 | The shared layout (`.kiln/images/harness-<id>/pin.sh`) and the HTTPS-fixture approach steps 25 and 26 use. |
| R15 | Implementer's light tests only; no subagents; commits on the branch, no merge or push. |
| R16 | Temporary directories under the runner's `~/.cache`, removed on exit; nothing reclaimed. |

## Unverified

- pin.sh on macOS or Windows hosts.
- Whether a version argument other than the installer's is still served: the test serves one; the live run
  pins the installer's.
- Everything after step 1 (unpacking, builds, smokes, CI): steps 2 to 6.
