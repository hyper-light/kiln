# Stacked diffs, review and landing

> **Superseded by [ci.md](ci.md), W14** (2026-10-17), which adapts this draft: change requests beside
> stacked diffs, merge strategies chosen at land time, the GitHub facade and App. Kept for history.

> **Draft spec.** None of this is built yet. It describes how kiln hosts code, reviews it as
> stacks of small diffs, tests every version, and lands it, with all policy kept in git.

kiln is where code is pushed, reviewed, tested and landed. The unit of review is a **diff**: one
commit, reviewed on its own. A series of dependent commits is a **stack** of diffs, each reviewed
and landed in order. People and agents use the same workflow and the same API.

## Repositories

| Mode | What kiln does |
| --- | --- |
| `native` (new repositories) | kiln hosts the git repository. Clone and push over HTTPS with a kiln token. Only the land queue writes trunk. |
| `upstream` (today's repositories) | kiln polls an upstream such as GitHub, anonymously for public repositories, as it does today. `push` and `pull_request` triggers keep working. |
| `native` + `mirror` | a native repository whose trunk kiln also pushes to an upstream after each land (needs a push key for that upstream only). |

`kiln repo import <url>` copies an upstream repository into native mode once.

```sh
kiln clone hyper-light/kiln     # remote + commit-msg hook + `kiln` helpers
```

## Diffs, versions and stacks

- **Diff**: a commit under review, numbered per kiln (`D123`). Its identity survives amends and
  rebases through a `Change-Id: I<40 hex>` trailer (the Gerrit format, so existing hooks and
  tools work). The commit-msg hook adds it; `kiln submit` adds it to commits that lack it.
- **Version**: each push of a changed commit adds a version (`D123 v3`). A version records the
  commit, its parent and the trunk commit it was based on. A version whose patch is unchanged and
  only sits on a new base is marked **rebase-only**, so reviewers know nothing new is in it.
- **Stack**: a chain of diffs where each diff's parent commit is the diff below it; the bottom
  diff's parent is on trunk. Pushing a fork of a stack makes a tree that shares the lower diffs.
- **Interdiff**: compare any two versions of a diff. Rebase noise is removed, as in
  `git range-diff`.

Every version is kept as `refs/kiln/diffs/D123/v3`, so any version can be fetched and nothing
reviewed is ever garbage-collected.

## Submitting

```sh
git commit -m "pipeline: job outputs"        # D1
git commit -m "pipeline: dynamic matrices"   # D2, on top of D1
kiln submit                                  # or: git push kiln HEAD:refs/for/main
```

kiln takes the commits between trunk and `HEAD`, bottom to top. For each one it creates a diff,
or adds a version to the diff with the same `Change-Id`. It links the stack, starts `diff`
pipelines for every new version, and prints each diff's number and link. Push options:
`-o draft`, `-o reviewer=ada,agent:review`, `-o message="addressed comments"` (a note on the new
versions), `-o topic=<name>`.

To change something in the middle of a stack, amend that commit (`git rebase -i`, or
`kiln absorb` to route working-copy edits to the commits that last touched those lines) and
submit again. Only the diffs that changed get new versions.

`kiln sync` fetches trunk, drops landed diffs from the local stack and rebases the rest.

## Review

| State | Meaning |
| --- | --- |
| `draft` | visible, not asking for review |
| `needs-review` | waiting for reviewers |
| `changes-requested` | a reviewer asked for changes; the next version returns it to `needs-review` |
| `accepted` | enough approvals for the policy (see below) |
| `landing` | in the land queue |
| `landed` | on trunk |
| `abandoned` | closed by its author; can be reclaimed |

- **Comments** anchor to a version, file, side and line range. They follow the code to newer
  versions, or are marked outdated when that code is gone. Threads can be resolved and reopened.
  General comments sit on the diff.
- **Suggestions**: a comment can propose replacement lines; the author (or their agent) applies
  them as an amend.
- **Actions**: reviewers accept, request changes, comment or resign. Authors mark ready,
  abandon or reclaim. Anyone allowed can **commandeer** a diff and take over authorship.
- Reviews and comments are drafted privately and published together, like a review in other tools.

## Landing

"Land D3" lands D3 and every diff below it, in order. Each must satisfy the policy on its latest
version.

Each trunk has one **land queue**:

1. The candidate is rebased onto trunk's tip, diff by diff. On a conflict the candidate leaves
   the queue as "needs rebase" and its author (or agent) is told.
2. Pipelines with `on: land` run on the rebased candidate.
3. When they pass, trunk moves forward to the rebased commits in one atomic update. Each landed
   commit gets `Differential-Revision: D123` and `Reviewed-by:` trailers.
4. `push` pipelines then run on trunk as usual (deploys).

**Batching**: the queue may test up to `batch` candidates together. On failure it bisects the
batch to find the culprit, ejects it, and lands the rest.

After a land, kiln rebases the remaining diffs of each affected stack onto the new trunk as
rebase-only versions, so stacks never go stale on the server. Authors pick them up with `kiln sync`.

## Policy, kept in git

Policy files are read from **trunk at the moment they apply**, never from the diff under review,
so a diff can't loosen the rules it is judged by. Changing them is itself a reviewed diff.

`.kiln/review.yaml`:

```yaml
trunks: [main]
land:
  approvals: 1               # accepts from people other than the author
  owners: true               # every touched path needs one of its owners' accepts
  checks: [ci]               # pipelines that must pass on the version (diff) and the candidate (land)
  resolve-threads: true      # no open threads
  agents-can-approve: false  # agents' accepts are advisory
  sticky-approvals: rebase   # keep accepts across: none · rebase (rebase-only versions) · trivial
queue:
  batch: 4
direct-push: []              # who may bypass the queue and push trunk directly (nobody by default)
```

`.kiln/owners.yaml`:

```yaml
- paths: ["crates/kiln/src/harness/**"]
  owners: [ada, team:agents]
- paths: ["docs/**"]
  owners: [ada]
  optional: true             # suggested reviewers, not required
```

`.kiln/environments.yaml`:

```yaml
staging:
  branches: [main]
production:
  branches: [main]
  approvals: { reviewers: [ada], count: 1 }
  concurrency: 1
  secrets: [DEPLOY_TOKEN]
```

## Testing stacks

- A `diff` run tests a version's commit, which includes the diffs below it. `changed()` and
  `paths` filters look only at that diff's own files.
- When a lower diff gets a new version, the diffs above are restacked and re-tested. A job whose
  `paths` and inputs are unchanged for that diff reuses its previous result instead of running again.
- The diff page shows each version's checks. The stack page shows one line of checks per diff,
  so a red diff in the middle stands out.

## Agents

- An agent session in kiln acts as `agent:<session>`, on behalf of the person who started it.
  It can submit stacks, reply to comments and amend its diffs. Its accepts count only if
  `agents-can-approve` is on.
- **Comments go to the author's agent.** A comment on a diff an agent wrote is queued into that
  session, with the thread, the file and lines, and the version. The agent amends the right commit,
  resubmits, and replies with a link to the new version. If the session has ended, kiln offers to
  resume it.
- **Agents as reviewers.** Requesting review from `agent:review` starts a read-only review
  session that leaves comments through the API like anyone else.
- Agents without git can submit a patch series over the API (`POST /api/repos/:repo/submit`).

## API (draft)

| Method | Path | |
| --- | --- | --- |
| GET/POST | `/git/<owner>/<repo>.git/…` | git smart HTTP (clone, fetch, push to `refs/for/<trunk>`) |
| GET | `/api/repos/:repo/diffs?state=&author=&reviewer=` | diffs |
| GET | `/api/diffs/:id` | diff, versions, stack, review state, checks |
| GET | `/api/diffs/:id/versions/:v/files` | the version's file diffs |
| GET | `/api/diffs/:id/interdiff?from=&to=` | compare two versions |
| GET/POST | `/api/diffs/:id/comments` | threads; `PATCH /api/comments/:id` resolves or edits |
| POST | `/api/diffs/:id/review` | `{ action: accept \| request_changes \| comment \| resign, body, comments }` |
| POST | `/api/diffs/:id/{ready,abandon,reclaim,commandeer}` | |
| POST | `/api/diffs/:id/land` | land it and everything below |
| GET | `/api/repos/:repo/queue` | the land queue; `DELETE /api/queue/:entry` takes one out |
| GET | `/api/stacks/:id` | a stack as a tree |
| POST | `/api/repos/:repo/submit` | a patch series as diffs (for clients without git) |

`/api/events` adds `diff`, `diff_version`, `review`, `comment` and `queue` messages. kiln's MCP
server exposes the same operations as tools, so agents use the API people use.

## CLI

`kiln clone`, `kiln submit [--draft] [--reviewers …] [--message …]`, `kiln sync`, `kiln land [D…]`,
`kiln diffs` (your stacks), `kiln checkout D123[@v2]`, `kiln absorb`, `kiln review D123`
(opens it).

## In the UI

- **Inbox**: needs your review · your stacks · your agents' stacks · landing.
- **Stack**: diffs bottom to top above trunk. Each row has its number, title, author (a person or
  an agent's mark), state, a beam per required check, open threads and version. "Land to here".
- **Diff**: title and summary, a version picker with interdiff, files, the diff (unified on
  phones), threads inline, checks with logs, the stack in miniature, and the review bar
  (Accept · Request changes · Comment).
- **Land queue**: candidates, the batch under test, and where each one is.

## Storage

Bare repositories live in kiln's data directory (a PVC on Kubernetes). Diffs, versions, comments,
reviews and the queue live in kiln's database. Every write is audited with its actor, person or agent.
