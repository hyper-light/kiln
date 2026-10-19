# Agents that run the work

> **Spec, 2026-10-18.** What this document asks for is being built in the order below. The status
> table is the source of truth for what is done.

kiln already runs any harness as a structured agent session, on any machine, and is a better
console for one agent than any harness's own (docs/agents.md). This spec is about the loop around
**many** agents:

- kiln gets your attention when an agent needs it;
- agents start, steer and wait on other agents;
- a plan runs as one agent per step;
- agents work in isolated workspaces (Slates volumes now, Shards microVMs next) and their work
  merges;
- what an agent changed goes from its Changes tab through review and CI to landed;
- agents run CI/CD, whatever its provider, and look after the runners: diagnosing, upgrading and
  repairing them.

Two rules hold throughout. **An agent never answers what waits on a person:** approvals,
questions, plans to approve, landing grants and confirmations of disruptive repairs go to people.
**An agent never has more authority than whoever it acts for:** its rights are the intersection of
its grant and that person's (W10-R01, W20-R06), and an agent it starts gets no more than it has.

## Status

| | Workstream | Depends on | Status |
|---|---|---|---|
| N | Notifications and a phone-first Mission Control | — | done: N1–N3 (docs/notifications.md); an iPhone check by a person is left |
| O | Agents that run agents | — | built: O1–O5 (docs/agents.md, "Agents that run agents"), tested on a hub in process; O-AC1 with real harnesses across two machines not run yet |
| P | A plan run as one agent per step | O (W for parallel steps) | built: P1–P3 for steps in order (parallel steps run one at a time until workspaces exist, and say so); P4 waits on C |
| R | Runners: diagnostics, upgrades and repair, by agents and people | O | built: R1–R3 and R5 (docs/releases.md, "Diagnosing and repairing a runner"); R4, a runner doctor that wakes by itself, waits on attachments (ci.md W20-R08) |
| W | Workspaces: Slates volumes, then Shards microVMs | — (Shards: its networking and file device) | designing |
| C | From an agent's changes to landed: change requests, CI, review | W14 (ci.md), W | designing |
| K | Agentic control of CI/CD, whatever the provider | W20 (ci.md), the provider model | coordinating |

## N: Notifications

How it works, its API and its settings: docs/notifications.md.

**N1 Push to every device you sign in on.** The browser's own notifications (Web Push: the
service worker, RFC 8291 encryption and RFC 8292 VAPID), on phones and desktops:

- The server makes its VAPID key pair once (P-256) and keeps it with its data. `GET /api/push`
  says whether push is available and gives the public key.
- A device subscribes from Settings → Notifications (`POST /api/push/subscriptions` with the
  browser's subscription, a device name and what it wants to hear; `GET` lists your devices,
  `DELETE /api/push/subscriptions/:id` removes one, `POST /api/push/test` sends a test).
  Subscriptions belong to the person signed in.
- What it hears about, per device: **needs you** (an approval, a question or a plan waits on you),
  **finished** (a turn ended and you haven't seen it), **stopped** (it crashed or couldn't start),
  **paused** (a usage limit). Each agent can be muted (its ⋯ menu). Low battery on a machine with
  agents (`machine.battery_low`) is one too.
- An agent's notifications go to the person who started it (the owner, on a kiln nobody else signs
  in to).
- One notification per agent at a time: it carries the agent's id as its tag, so a newer state
  replaces the older one. Its title is the agent's name; its text is the one line Mission Control
  shows ("Asks: which database?", "Bash: cargo test -p kiln", "Finished: all 160 tests pass").
  Tapping it opens the agent.
- No notification while you're looking: the service worker shows none when a focused kiln window
  is already on that agent.
- Needs you is sent urgent; the rest normal. A push service's 404 or 410 drops that subscription;
  429 and 5xx are retried with backoff. The last delivery and its error are kept per device, so
  Settings can say why a device hears nothing.
- The phone needs a secure page: HTTPS through a front door (docs/front-door.md) or localhost.
  Over plain `http://` Settings says so and links to the front door. On an iPhone, Web Push needs
  kiln added to the Home Screen: kiln has a web app manifest and icons, and Settings says how.
- The server needs outbound HTTPS to the push services (Apple's, Google's, Mozilla's). Payloads are
  encrypted end to end; the push service can't read them.

**N2 The same events through kiln's other channels.** Agent events (`agent.needs_you`,
`agent.finished`, `agent.stopped`, `agent.paused`) join the events W19's notification rules and
outgoing webhooks take, so Slack, Telegram, Discord, ntfy-style webhooks and the rest can carry
them too, with W19's `where` and `throttle`.

**N3 Mission Control opens on what needs you.**

- Agents that need you are first, whatever the sort; the sort orders the rest.
- On a phone the page's head is one line (its title and how the agents stand), the filter and the
  sort share a row, and the message box for many agents stays folded until you select agents or
  open it. It folds in the page's flow: no sheets or overlays.
- When this device has notifications off, one quiet line offers to turn them on.

**Acceptance.** N-AC1: an approval on an agent reaches a subscribed phone as one notification
within 2 s of the harness asking, and tapping it opens the agent, verified with a real Chromium
push subscription in a test (and by the user on an iPhone). N-AC2: a second state of the same agent
replaces the first, and none shows while a focused window is on that agent. N-AC3: a dead
subscription (410) is dropped. N-AC4: on a 390×844 screen the first agent row is in the top third
of Mission Control.

## O: Agents that run agents

Today an agent can start agents only with `kiln spawn` in a kiln shell, on its own machine, and
can't wait on one or read what it concluded. kiln's MCP server has no agent operations.

**O1 Operations.** `agents.*` join kiln's one operation registry (W20-R01), so each is an HTTP
route, an MCP tool (`agents_start`, …), a code-mode function (`kiln.agents.start`) and a CLI command:

| Operation | |
|---|---|
| `agents.list` | agents, filtered (machine, harness, state, parent, plan run) |
| `agents.get` | one agent: its info and state |
| `agents.start` | `{kind, on?, cwd?, prompt?, model?, effort?, mode?, name?, count?, workspace?}`; `on` is a machine's name or what it needs (`linux/amd64`, `+gpu`), picked as reach picks |
| `agents.send` | a message: `queue`, `steer` or `now` |
| `agents.wait` | until the agent is idle, needs a person, or has ended (or any change), up to a timeout; returns where it stands |
| `agents.result` | what it concluded: its last reply, how its last turn ended, what it changed (files, lines, its workspace or branch, its change request), cost |
| `agents.interrupt`, `agents.stop` | |

There is no operation for answering another agent's approvals or questions.

**O2 Parents and children.** An agent started by an agent records its parent (`parent`, set from
the caller's identity, never from input). Mission Control shows children under their parent, and a
child's request for a person still comes first in the list and to notifications, naming the parent.
Ending a parent lists its live children and offers to end them too.

**O3 No more than its parent.** A child's permission mode is at most its parent's (plan < ask <
accept edits < auto < bypass); bypass needs the parent's own `allow_bypass`; its `reach` is within
the parent's. Depth (default 3) and live children per agent (default 24) are limited
(`[agents] max_depth`, `max_children`). Every call is audited as `agent:<id>` for its person.

**O4 Everywhere.** The operations work through MCP for every agent kiln starts on any machine, and
`kiln agents wait`, `kiln agents result` and `kiln spawn --on` from any kiln shell.

**O5 A skill.** `kiln-agents`: when to fan out, how to brief a child, waiting and reading results,
keeping children's permissions low, merging their work (W), and never answering for the person.

**Acceptance.** O-AC1: an agent on one machine starts two agents of different harnesses on
another machine, waits for both and reads their results, end to end with real harnesses against
the mock model. O-AC2: a child asked for bypass under an ask-mode parent is refused, through every
surface. O-AC3: a child's approval shows under Needs you and as a notification, and no operation
lets the parent answer it.

## P: A plan run as one agent per step

**P1** `plans.run` gains `mode: "agents"`: each step gets its own agent, held to the step as a goal
(its title, what to do, examples, "Done when:" its acceptance criteria, "Tested by:" its tests;
docs/goals.md), on the machine and harness chosen for the run or per step.

**P2 Order.** Steps run one after another by default: a step's agent starts when the step before
is met. `parallel` starts them together, and a step may name the steps it waits for. Parallel
steps need their own workspaces (W); in one plain folder kiln runs them one at a time and says why.

**P3 The run.** A plan run has its own page: each step with its agent, where it stands (waiting,
working, needs you, met, unmet), and what it changed; stop the run, retry a step, take a step over.
Mission Control groups the run's agents under it ("Plan: retry rework · 3 of 5 met").

**P4 What comes out.** Each step's agent merges into the run's workspace as its step is met (W5).
The run ends as one change request, or as a stack with one diff per step when the steps ran in
order (C1).

**Acceptance.** P-AC1: a three-step plan runs as three agents in order, each starting once the
step before is met, and ends as a stack of three diffs.

## R: Runners, by agents and people

**R1 Operations.** `runners.list`, `runners.get` (facts, health, version and build, upgrade
state, battery, load, connection), `updates.list`, `updates.retry {runner, way}`,
`updates.policy`; through every surface, as O1.

**R2 Diagnosis.** `runners.diagnose` runs kiln's checks on the machine and returns findings, each
with what it saw and what would fix it: kiln's build against the server's, the supervisor or
launcher, disk and memory against what the platform grants (cgroups, docs: runs anywhere), the
toolchains a self-build needs, the clock, the link to the server (QUIC and the TCP fallback),
certificates, macOS signing (`kiln signing`), the tail of the runner's log and of a failed
upgrade's output.

**R3 Repair.** `runners.repair {runner, action}` with named actions, each saying what it changes:
clearing the self-build cache, freeing kiln's caches, signing again (macOS), installing the
launcher again, upgrading the other way. An action that would stop what runs on the machine
(restarting the runner stops its shells and agents unless it can hand them over) lists what it
would stop and needs a person's confirmation; an agent can propose it, not confirm it.

**R4 A runner doctor.** An agent attached to runners (W20-R08's attachment, scope `runners`)
wakes on a failed upgrade, a failing check or a machine offline past its grace. It diagnoses
(R2, and reach for commands on the machine), repairs within its capabilities (R3), and reports on
the runner's page and in Settings → Updates; what needs a person comes to Needs you and to
notifications.

**R5 People too.** Settings → Updates: **Diagnose** shows R2's findings in the runner's row;
**Repair** offers the fixes they name; **Ask an agent** starts the doctor with them.

**Acceptance.** R-AC1: an upgrade made to fail (a missing toolchain) is diagnosed with the right
finding, and an agent repairs it and retries the upgrade, which succeeds. R-AC2: a repair that
would restart a runner with live shells is refused until a person confirms.

## W: Workspaces

**W1 Slates on every runner.** kiln's release pipeline builds Slates for every platform beside
kiln; runners install it beside kiln and supervise `slates anchor`. Runner facts say whether Slates
works there and how it mounts (macOS: NFS on loopback; Linux: FUSE with `fusermount3`; Windows:
WinFsp, limited).

**W2 New agents choose.** `workspace`: the folder itself, or **its own Slates volume** (the default
where Slates works and the folder is a git repository). Several agents on one folder each get a
volume.

**W3 How.** Per folder per machine, one **green** volume based on the folder (`--base`). Each
agent gets a **work** volume from the green's current version, mounted under kiln's data, as its
harness's folder. It sees a normal directory: git, cargo, npm and its editor work there.

**W4 Changes** are the work volume's own record, shown in the Changes tab as today.

**W5 Merging.** **Merge** (the Changes tab, `kiln.workspace.submit`, or a plan run's step being
met) submits the work volume: merged (the green's new version), already there, or conflicts as
files and byte ranges. Conflicts show in Changes and go to the agent to fix and merge again. A work
volume moves onto the green's newest version with **Catch up** (`rebase`).

**W6 Writing to disk.** **Write to disk** shows what the landing would write; the person's click is
the grant (kiln runs `slates grant` on the machine; nothing an agent calls can). A file changed on
disk since is refused, and said so. Most work never needs it: it goes to review from the volume
(C1).

**W7 Moves and restarts.** A volume outlives the runner's restarts (Slates keeps it). An agent
moving to another machine takes its volume (a snapshot cloned there; Slates' fleet replication once
it is beyond limited).

**W8 Shards, next.** `isolation: "shards"` runs the harness in a microVM built from an image (an
Agentfile, W21-R01), its folder the agent's Slates volume, its network what kiln allows. Shards
can't host a harness yet: guest networking and a file device (virtio-fs or vhost-user to Slates)
are still to come in Shards. Until then New agents shows Shards with why it isn't available.

## C: From changes to landed

**C1 Into review.** **Open change request** in an agent's Changes (or the agent itself, through
`kiln.ci.diffs.submit_patch` or the change-request operations): its work, from its volume or
folder, committed on `agents/<name>` and pushed to kiln's git as a change request (W14-R04), or as a
stack (W14-R03). The author is `agent:<id>`, on behalf of its person. For a repository whose
provider is elsewhere (GitHub, GitLab, Bitbucket), a branch pushed there and a pull request opened
through that provider.

**C2 CI comes back to the agent.** The checks on its change show in its Changes; a failure goes to
the agent as a message with the failure excerpt (W20-R10's bundle) when its person allows it
("Fix failing checks by itself").

**C3 Review comes back to the agent.** A comment on its change goes to it as a queued message with
the file, the lines, the version and a link; it answers, amends and pushes a new version
(review.md, Agents).

**C4 Mission Control.** A finished agent with an open change shows **Review** on its row; landed
shows as such.

**C5 The review UI** (ci.md W21-R09, built to W14-R06 and checked against W14-AC06 in Chromium and
WebKit): the inbox (needs your review, your stacks, your agents' stacks, your conversations), the
change request and diff page (versions and interdiffs, threads on any line or range, suggestions,
viewed files, next and previous unresolved), the stack, the land control with its strategy, and the
land queue. Diffs: syntax highlighting with the editor's grammars, word-level changes, moved
blocks, a whitespace toggle, split and unified (unified on phones), generated and `-diff` files
folded, renames, copies, binary and image diffs, and a 10,000-line diff scrolling at 60 fps.

**C6 Pushing as the agent.** An agent's work reaches kiln's git with an agent token (`kiln_agt_…`,
minted for `agent:<id>` acting for its person and limited to the repository), never with an MCP
token (those act as the owner). The change request records `author: agent:<id>` and `author_for`,
which is how comments, reviews, CI results (`diff_run`) and queue events (`queue`: ejected, needs
rebase) find their way back to the agent.

## K: Agentic control of CI/CD, whatever the provider

**K1** Every CI/CD operation a person has is an operation agents can call, for each provider kiln
supports (kiln CI, GitHub Actions; kiln CD, Argo CD): runs (list, get, trigger, cancel, rerun,
logs and failure excerpts), workflows (validate, preview, propose), deployments (list, diff, sync,
roll back, history), environments, runners and pools. Approvals stay with people unless policy
allows agents (W20-R09). This is W20-R02's coverage, made the same across providers.

**K2** Attached agents and incidents (W20-R08 to R10) show in Mission Control grouped by what they
look after, and their questions come to Needs you and to notifications (N).

**Ownership** (agreed 2026-10-18):

- **CI.** The CI work owns W14 (hosting, change requests, review, the land queue) and the CI
  provider model (ci.md W23: KilnCI, GitHub Actions, GitLab, Bitbucket; `crate::githost`). Its run
  API is the same for every provider: `GET /api/ci/runs` and `/runs/:id` (with `provider` and
  `external_url`), rerun, cancel, leg logs and dispatches route to the run's provider. This spec
  puts them in the operation registry as `kiln.ci.*`.
- **CD.** The deploy work owns the CD provider model (KilnCD or Argo CD per environment:
  docs/deploy-api.md, `crate::deploy`). It registers `deploy.*` operations (release, roll back,
  cancel, status, history, candidates, wait) that refuse agents an environment doesn't allow
  (`agents_may`).
- **This spec** owns the agents' side: orchestration, the permission ceiling, notifications, and
  routing review and CI results back to the authoring agent.

C uses W14's API: a change request opened by pushing `refs/for/<trunk>/branch/<name>`, by
`POST /api/repos/:repo/change-requests`, or as patches through `POST /api/repos/:repo/submit`;
read through `GET /api/change-requests/:id` (and `/comments`, `/land`); followed through the
`run`, `landed`, `review` and `comment` events.
