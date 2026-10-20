---
name: kiln-ci-review
description: Submit, review and land changes on kiln-hosted repositories - stacked diffs, change requests, review threads and suggestions, the trunk's review policy, landing strategies and the land queue. Use when proposing a change, answering review comments, reviewing someone's diff, or finding out why a diff can't land or left the queue.
---

# Review and landing on kiln

kiln hosts repositories and reviews changes as **diffs**: one commit each, stacked when one
builds on another, identified across versions by its `Change-Id:` trailer. A **change request**
is a branch reviewed whole. Nothing reaches a trunk except through the land queue (unless the
trunk's policy allows a direct push for someone).

## Submitting

With git, push to the trunk's review ref; each commit becomes a diff, or a new version of its
diff when its `Change-Id` is one kiln has seen:

```sh
git push origin HEAD:refs/for/main                      # diffs, one per commit
git push -o reviewer=ada -o topic=parser origin HEAD:refs/for/main
git push origin HEAD:refs/for/main/branch/my-feature    # a change request of the branch
```

Push options: `draft`, `ready`, `reviewer=<name>`, `message=<what changed>`, `topic=`,
`title=` (a change request's). Without git, submit patches:

```js
// code_run: commits as diffs, from `git format-patch --stdout` text.
return await kiln.ci.diffs.submit_patch({ repo: input.repo, mbox: input.mbox });
```

Keep each diff one coherent change that builds and passes on its own: a reviewer reads them
one at a time, and the queue lands them in order.

## What a diff needs to land

```js
const d = await kiln.ci.diffs.get({ diff: input.diff });
const r = await kiln.ci.diffs.readiness({ diff: input.diff });
return { state: d.state, versions: d.versions.length, runs: d.runs, blockers: r.blockers, strategies: r.strategies };
```

`blockers` are sentences: accepts missing (from whom, by the trunk's `.kiln/review.yaml` and
`.kiln/owners.yaml`), changes asked for, open threads, required checks failing or not run.
Fix the cause; never ask a person to lower the policy so your diff passes.

## Answering review

Read the threads, fix the code, push a new version (same `Change-Id`), then reply where you
changed something and resolve what's settled:

```js
const t = await kiln.ci.diffs.threads({ diff: input.diff });
// Each thread: where it is (`version`, `file`, `side`, lines, and `now` on the latest version or
// `outdated`), its `state` and its `comments`.
const open = t.threads.filter(x => x.state !== "resolved");
// ... after pushing the fix:
for (const x of open.filter(x => input.fixed.includes(x.id))) {
  await kiln.ci.diffs.reply({ diff: input.diff, thread: x.id, body: `Done in v${input.version}: ${input.how}`, publish: true });
  await kiln.ci.diffs.resolve({ comment: x.comments[0].id, state: "resolved" });
}
return open.length;
```

Comments are drafts until published, alone (`publish: true`) or with a review.

## Reviewing

Comment on lines (`file`, `line` or `start_line`/`end_line`, `side: "new"`), with a `suggestion`
(the replacement text) when the fix is mechanical, then decide once:

```js
await kiln.ci.diffs.comment({ diff: input.diff, file: "src/parse.rs", line: 42, body: "This panics on an empty input.", suggestion: "    let Some(first) = input.first() else { return Ok(()) };" });
await kiln.ci.diffs.review({ diff: input.diff, action: "request_changes", body: "One edge case; the rest reads well." });
```

`accept`, `request_changes`, `comment` or `resign` (step off). An agent's accept counts only where
the trunk's policy says `agents-can-approve`; never accept your own work, and review what the
change does, not only whether its checks pass.

## Landing and the queue

```js
return await kiln.ci.diffs.land({ diff: input.diff, strategy: "stack" }); // or the trunk's default when left out
```

- Diffs: `stack` (each diff a commit, with its trailers), `stack-squash` (the stack as one
  commit), `ff-only` (the commits as they are; refused once the trunk moved on).
- Change requests: `squash`, `merge`, `rebase`, `ff-only`.

The queue tests a candidate (the trunk with the entry, and those queued before it, applied) with
every workflow that takes `land` or `merge_group`, then moves the trunk. A failing batch is
bisected; the culprit leaves the queue with a reason ("`ci` failure on the candidate", "needs
rebase: conflicts in …", a benchmark gate's verdict) and its diffs go back to review. Fix and
land again.

Landing is opt-in for agents (`land` capability); without it, say the diff is ready and leave
landing to a person.
