---
name: kiln-agents
description: Hand parts of your work to other coding agents through kiln (any harness, on any machine), wait for them and read what they concluded. Use when work splits into independent pieces, needs another platform or machine, or a second opinion; and to brief children well, keep their permissions low, merge what they did and leave what they ask to the person.
---

# Agents that run agents

kiln runs coding agents (Claude Code, Codex, Gemini CLI, opencode, Goose, Amp, Antigravity) on
every machine it has. You can start some yourself: each is your **child**, works on its own,
and reports back. The tools are `agents_start`, `agents_wait`, `agents_result`, `agents_send`,
`agents_list`, `agents_get`, `agents_children`, `agents_interrupt`, `agents_stop`,
`agents_change` and `agents_changes` (in code mode `kiln.agents.*`; in a shell
`kiln spawn --on …`, `kiln agents wait`, `kiln agents result`).

## When to fan out

Start children when the work splits into pieces that don't touch the same files and can be done
and checked on their own, or when a piece needs another machine (another platform, a GPU, more
cores). Don't for a few minutes of work, for steps that depend on each other's results as they
go, or for anything you'd have to explain at more length than doing it. One or two children,
then more if it works; a child that starts children of its own is rarely what you want.

Children in one folder share it: **only one should write at a time**. Until kiln gives agents
workspaces of their own, give writers separate folders (or repositories), or let one write and
the others read, review or test.

## Briefing a child

A child starts with nothing but your prompt. Say:

- **What to do**, and why, in a few sentences. Name the files and functions it's about.
- **What done looks like**: the tests that must pass, the behaviour to see, what not to change.
- **What to report**: end its reply with what it did, what it found, and anything left. Its last
  reply is what you get back.
- **Where**: `cwd` (by default yours, on your machine), `on` for another machine (a name, or
  what it needs: `linux/amd64`, `macos`, `+gpu`, `cpus>=16`).

```js
// code_run: two children on another machine, in two harnesses, then what they concluded
const [a, b] = await Promise.all([
  kiln.agents.start({ kind: "claude", on: "linux/amd64", cwd: "~/src/app", mode: "acceptEdits",
    prompt: "Make `cargo test -p parser` pass on Linux. Done when it does; change only crates/parser. End with what you changed and why." }),
  kiln.agents.start({ kind: "codex", on: "linux/amd64", cwd: "~/src/app-docs", mode: "untrusted",
    prompt: "Review docs/parser.md against crates/parser/src/lib.rs and list what's out of date. Change nothing." }),
]);
const ids = [a.agents[0].id, b.agents[0].id];
const done = await kiln.agents.wait({ agents: ids, timeout_s: 600 });
return { waited: done.agents.map(x => ({ id: x.id, stand: x.stand })),
         results: await Promise.all(ids.map(agent => kiln.agents.result({ agent }))) };
```

## Waiting and reading results

`agents_wait` returns when each agent has **settled** (idle, needs a person, paused for a usage
limit, stopped, or ended), or when its time is up (`timeout_s`, at most 600: call it again to
wait longer). `until: "change"` returns on any change; `any: true` when the first is done. A
message you just sent counts: an agent isn't settled until it has had its turn.

Each agent comes back with its `stand`:

| stand | what it means | what to do |
|---|---|---|
| `idle` | its turn is over | `agents_result`, then send more or stop it |
| `needs_you` | an approval, a question or a plan waits on **a person** | tell the person; wait again |
| `paused` | its account hit a usage limit | wait, or work on without it |
| `stopped` | it crashed or couldn't start (`exit` says why) | read why; start another if it's worth it |
| `ended` | it's done for good, or gone | `agents_result` if it's still listed |
| `working`, `offline` | still at it, or its machine is away | wait again |

`agents_result` gives its last reply, how its last turn ended (`last_turn.how`: done, cut,
stopped, limit, refused, blocked), what it changed (files, lines added and removed), its folder,
machine and cost. Check what it changed before you build on it: read the files, run the tests.

## Keeping children's permissions low

A child gets **no more than you have**: its mode is at most yours (plan < ask < accept edits <
auto < bypass), bypass needs your own, and if your commands run only on your machine, so do
your children's. Ask for less than that: `plan` or `read-only` to look and report, `acceptEdits`
(Claude Code) or `untrusted` (Codex) to change files with a person approving commands. With no
`mode`, a child starts in its harness's default when that's within yours, else in the most that is.

## Merging their work

Today children work in real folders. When they're done: read what each changed
(`agents_result` lists the files), run the tests yourself, and fix conflicts between them before
you go on. Never assume a child's claim; check it. (Workspaces of their own, merged by kiln, are
coming: see `workspace` in `agents_start`.)

## Putting work up for review

`agents_change` opens a change request from an agent's work: yours (`agent` is your own id) or
a child's. It takes the files the agent changed, commits them on the folder's last commit without
touching the folder's branch, index or working tree, and pushes them as the branch
`agents/<what it's about>-<id>`: into kiln's review for a repository kiln hosts, as a pull request
for one on GitHub, GitLab or Bitbucket. Give it a `title` and a `body` that say what changed and
why. Calling it again pushes a new version.

Then comments on it, requests for changes, failing checks (with the failure's excerpt) and the
land queue taking it out come to the agent as messages. Answer them by changing the code and
calling `agents_change` again; landing it is for people and the land queue. `agents_result` and
`agents_changes` say where each change stands.

## Never answering for the person

What a child asks a person (an approval to run a command, a question, a plan to approve) is the
**person's** to answer. No tool lets you answer it, and you must not ask the child to work around
it. When a child `needs_you`, tell the person what it asks (`needs` in its stand) and where (its
page in kiln, or Mission Control), then wait for it again or go on with something else.

## Steering and ending

`agents_send` (`queue` after its turn, `steer` into it, `now` stopping it) and `agents_interrupt`
work on your own children (and theirs) only. `agents_stop` ends one; `children: true` ends what
it started too. End children you no longer need: each holds a harness process on its machine.
