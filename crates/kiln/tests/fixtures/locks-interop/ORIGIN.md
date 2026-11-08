# W29-AC20/AC22: kiln beside the pinned `gh actions-lock`

Plan 01a19a37's step 12. Each case under `cases/` holds a tree, its source facts, verify scenarios
and what the pinned upstream wrote and found there (`expected/`). kiln's tests
(`crates/kiln/src/ci/locks/tests/interop.rs`) replay the same facts through kiln's
dependency-source simulator and compare its `actions.lock` bytes, its reading of each lock and
its `ci.locks.verify` verdicts.

## Upstream, frozen

- `github/gh-actions-lock` at `113f91d77e0af03838b6ae3bef0e038cbab4c696`, the CLI.
- `github/actions-lockfile` `go/v0.0.6` (`ac158834765237ee94c36606cfc7d87316aa49ab`), the official
  parser, as the CLI's `go.mod` requires it.

Both are MIT-licensed, by GitHub, Inc.: `LICENSE-gh-actions-lock` and `LICENSE-actions-lockfile` are
their licenses at those commits. From them come `cases/upstream-golden-json/tree/`, copied verbatim
from the CLI's `cmd/gh-actions-lock/testdata/golden-json`, and every file under `cases/*/expected/`,
which the pinned CLI and parser wrote. No upstream code is vendored or built into kiln.

## The cases

| Case | Tree | Scenarios |
| --- | --- | --- |
| `upstream-golden-json` | upstream's golden fixture, with its v0.0.2 lock and its facts (`check_json_golden_test.go`) | its own lock (`given`), and the lock written fresh |
| `nested-composite` | a composite calling a composite, which uses an action in a repository's subdirectory | fresh |
| `f02-mantle`, `f02-hyper-raft`, `f02-slates`, `f02-kiln` | F-02's repositories' `.github/workflows`, verbatim, at their F-02 commits (`crates/kiln-ci/tests/fixtures/v1/SOURCES.md`): tags, branches (`stable`, `release/v1`), a local reusable workflow, and kiln's own full-commit pins | fresh; and for hyper-raft, each case of upstream's `docs/troubleshooting.md` that kiln gives a code |
| `f02-focal` | the same, for focal | fresh, recorded but not compared: its release workflow takes its matrix from a job's outputs, so the images it runs aren't known before it runs, and kiln writes neither lock (W29-R24). Upstream, which locks no image, writes its file, and kiln reads that file alike |

The source facts are synthetic and recorded here: `harness/facts.py` writes `facts.json` (each
repository's owner and repository ids, refs, commit graph and files) and `scenarios.json`. A
commit id is the SHA-1 of a label. The golden fixture keeps upstream's own facts. Node actions
carry `main: index.js`, which GitHub and kiln require and upstream's test action leaves out.

Documented findings and kiln's codes:

| `gh actions-lock` | kiln |
| --- | --- |
| `not-pinned`, `ref-changed` | `KCI4050` |
| `unreachable-pin` | `KCI4051` |
| `repo-replaced` | `KCI4052` |
| `tag-commit-mismatch` | `KCI4053` |
| `stale` | `KCI4055` |
| `ref-moved` (warning) | `KCI4057` (warning) |

## Regenerating `expected/`

`harness/run.sh` copies the pinned CLI's source into its own directory under `~/.cache`, puts
`harness/interop_test.go` beside the CLI's tests (whose helpers it uses), and runs it with no token
and its own Go caches. Its fake host answers every request from the facts, and a request no fact
answers fails the case, so nothing leaves the process; only the Go module proxy is reached, for the
CLI's dependencies. `harness/run.sh --write` copies the results here. It runs:

- `--no-interactive --no-narrow` on the tree without a lock, to write `actions.lock`. By default
  the CLI narrows a partial ref it pins for the first time (`v4` to `v4.6.2`) and rewrites the
  workflow; kiln never rewrites a `uses:` line, so the trees are compared unnarrowed.
- `--verify --json=valid,findings` per scenario, the frozen check `ci.locks.verify` answers as.
- `--relock --no-fix --json=valid,findings` per scenario, as upstream's own golden test runs it,
  which re-resolves each ref: only then does a moved partial ref give its `ref-moved` warning.

Recorded with Go 1.27.2 on 2026-11-03, through kiln's MCP `run` (`needs: tool:go,cpus=4,mem=4g`),
exit 0.
