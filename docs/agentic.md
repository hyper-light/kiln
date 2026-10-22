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
| O | Agents that run agents | — | done: O1–O5 (docs/agents.md, "Agents that run agents"), tested on a hub in process. O-AC1 passed 2026-10-19 with real harnesses and models (no mock model exists): a Claude Code agent on a Linux runner started a Claude Code and a Codex agent on a Mac runner through kiln's MCP tools, waited for both (14 s) and read both results. Since: a parent is told when each child settles, so it needn't wait (docs/agents.md, "Told, not made to wait"); a Claude Code subagent in the background no longer holds its parent's turn open; each agent's processes are held together by the system (a cgroup on Linux, a job object on Windows, followed with kqueue on macOS: docs/agents.md, "What an agent left running") |
| P | A plan run as one agent per step | O (W for parallel steps' isolation) | built: P1–P4: each met step's work goes up as one diff on the diff of the step met before it, so the run ends as a stack (P-AC1, tested in process against kiln's own git hosting). Since 2026-10-21 a run follows its steps' `needs` (a graph): steps that don't wait for each other run side by side, as many at once as there's room for, sharing one folder unless given folders or machines of their own (their isolation, W5, held with Slates); a step can go on the main agent's own goal stack rather than to an agent of its own; and the agent holding a step splits its goal into its acceptance criteria and tests (`goals_split`) and says each met (`goals_done`). Tested in process: a five-step graph run side by side and blocked by one unmet step until retried, `max_parallel`, and a step given to the main agent. Checked live 2026-10-21 on a throwaway server with real Claude Code agents: a four-step graph (step 1 given to a Sonnet main agent, which split it into its two criteria and its test and said each met with `goals_done`; steps 2 and 3 side by side on Haiku agents of their own, met by Claude Code's `/goal`; step 4 after both) ran to done in 74 s, as four commits in the order met, each by its step's agent with only its own file; and since 2026-10-22 a goal turns into a plan (`goals_plan`): planned first (research inside and outside, each finding graded by kiln from its sources: docs/research.md), its steps then the goal's parts or a run, the goal met once every step is; plan runs and goal stacks draw where each step stands as the conversation draws work |
| R | Runners: diagnostics, upgrades and repair, by agents and people | O | built: R1–R5 (docs/releases.md, "Diagnosing and repairing a runner"; docs/agents.md, "Attached agents"): R4 is an agent attached to the runners, woken by a failed upgrade, a failing check or a machine offline (the CI's `runner.offline`), held to `diagnose` and `manage-runners`, its incident on the runner's row in System → Updates. R-AC1 passed 2026-10-19 on a throwaway server and runner (a self-build without npm failed; Claude Code as the runner doctor diagnosed it as a missing dependency, retried by download with a person approving its calls, the runner upgraded, and the doctor resolved its incident once told the upgrade went through: in the lab by hand, the message kiln now sends itself) |
| W | Workspaces: Slates volumes, then Shards microVMs | Slates (merging: its mountable work volumes; Linux landing: its issuer surface), Shards (networking, file device) | built: W1–W4, W6 and W7 as far as Slates allows (an overlay volume of its own per agent, its change requests from it, writing it to disk on a person's go-ahead where Slates takes one from kiln: macOS). **Held** (2026-10-19: Slates is far from complete): W5 merging between agents, moving an agent with a volume to another machine (refused, saying why), installing Slates from kiln's releases, workspaces on Windows (no mount there yet), writing to disk on Linux (Slates' issuer surface). W8 waits on Shards |
| C | From an agent's changes to landed: change requests, CI, review | W14 (ci.md), W | built: C1–C6 (docs/agents.md, "Change requests from agents"): `agents.change` and the Changes tab's **Open change request** (kiln's own review with an agent token; a pull request through GitHub, GitLab or Bitbucket when kiln has an integration there, else the branch and a link), comments, requested changes, failing checks and the land queue back to the agent, Mission Control's word on each agent's change, and the review UI; a plan run's steps go up as a stack, one diff per step (P4). A comment's message tells the agent how to answer in its thread once W14's `ci.diffs.reply` is there (the CI work's, on its way to main); until then agents answer with a new version |
| K | Agentic control of CI/CD, whatever the provider | W20 (ci.md), the provider model | K1: W20-R02's operations, the CI work's (in progress there, `ci.diffs.*` included); K2 built: attachments and incidents (W20-R08 to R10, `crate::ci::attach`; docs/agents.md, "Attached agents"), Agents → Attached, Mission Control grouping an attachment's agents under it; their questions come to Needs you and notifications as any agent's do |

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
the caller's identity, never from input). It's a top-level agent all the same: Mission Control
lists it as a row of its own, like an agent a person starts. A harness's own subagents stay in
their agent's chat.
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
steps need their own workspaces (W) to be kept apart.

Built (2026-10-21): a step names the steps it waits for (`needs`); a run starts each step once
they're met, and steps that don't wait for each other run side by side, as many at once as
`max_parallel` and the parent's `[agents] max_children` leave room for. With W5 held, steps side
by side in one folder share it and the run says so (each keeps to its own files, or the steps get
folders or machines of their own). A step not met holds up only the steps that wait for it; with
nothing at work the run stops, naming the steps that hold it up. A step can be given to an agent
that's there already (`{agent: "main"}`: the agent that ran the plan): its goal goes on that
agent's own goal stack and the run follows it there. The agent holding a step, its own or the
main agent, turns it into a goal stack with `goals_split` (its acceptance criteria and tests, a
goal each) and says each met with `goals_done` (docs/goals.md, "Agents work their own stack").

**P3 The run.** A plan run has its own page: each step with its agent, where it stands (waiting,
working, needs you, met, unmet), and what it changed; stop the run, retry a step, take a step over.
Mission Control groups the run's agents under it ("Plan: retry rework · 3 of 5 met").

**P4 What comes out.** Each step's agent merges into the run's workspace as its step is met (W5).
The run ends as one change request, or as a stack with one diff per step (C1): in the order the
steps were met, which keeps each after the steps it waits for.

Built, for steps in order (docs/agents.md, "Change requests from agents"): as each step is met,
the files its agent changed become one commit on the step before's, made beside the folder (C1's
scratch index), and go up for review at once. On a repository kiln hosts, each is pushed to
`refs/for/<trunk>` (topic `plan-<run>`) with its own agent's token, so each diff's author is the
agent that did its step and kiln links them as a stack; elsewhere, a branch per step with each
pull request onto the one before. A step that changed nothing has no diff; one whose commit
couldn't be made hands its files on to the next. What review and CI say to a step's agent waits
until the run ends (the next step works in the same folder); then `agents.change` from that
agent makes a new version of its own diff and makes the steps above again on it, or says why it
can't (its change touches what a step above changed too). Merging steps that ran side by side
into one workspace (W5) is held with Slates.

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
the runner's page and in System → Updates; what needs a person comes to Needs you and to
notifications.

Built: an attachment on `runners` (or a runner, or a pool) wakes on `upgrade` (an upgrade job
failed), `check` (kiln's checks of the machine, run every half hour for it, found something
failing) and `offline` (the CI's `runner.offline`, two minutes gone). Its agent gets the failed
upgrade's error and last lines, or the failing finding with all of them, may call `runners.*` and
`updates.*` within `diagnose` and `manage-runners`, and its open incident shows in the runner's row
in System → Updates.

**R5 People too.** System → Updates: **Diagnose** shows R2's findings in the runner's row;
**Repair** offers the fixes they name; **Ask an agent** starts the doctor with them.

**Acceptance.** R-AC1: an upgrade made to fail (a missing toolchain) is diagnosed with the right
finding, and an agent repairs it and retries the upgrade, which succeeds. R-AC2: a repair that
would restart a runner with live shells is refused until a person confirms.

## W: Workspaces

kiln is the harness Slates leaves room for: Slates owns the bytes (overlays, witnesses, drift,
merging, landing under a person's grant); kiln owns the agents, their identity, review, history and
the place a person says yes (Slates' SLATES_DESIGN.md §1.3, §4.13).

**W1 Slates on every machine.** A runner finds Slates (the binary kiln installed under its state
directory, else `slates` on `PATH`) and keeps one instance, `kiln`, for all the machine's runner
processes. The instance's anchor starts apart from the runner: its own systemd user unit on Linux,
its own process group elsewhere, so restarting or upgrading the runner never ends it (a volume
lives in the anchor's memory). A new instance gets its one-node group (`slates bootstrap root`).
Runner facts say whether workspaces work there and why not (`workspaces`: Slates installed, a
mount it can make: FUSE through `fusermount3` on Linux, NFS on loopback on macOS; Windows has none
yet), and `GET /api/harness/workspaces` says it for every machine. Building Slates for every
platform beside kiln (the release pipeline, fleet self-builds) is still to come: today a person
installs it (`cargo install --path crates/cli` in a Slates checkout puts `slates` on `PATH`).

**W2 New agents choose.** `workspace`: `folder` (the folder itself, still the default) or
`volume` (**its own Slates volume**), in New agents and `agents.start`, only where the machine can
make one; a machine that can't says why. Several agents on one folder each get their own.

**W3 How.** On the agent's machine, before its harness starts: an **overlay volume** laid over its
folder's repository (`slates volume create --base <top>`), mounted under the runner's state
directory (`workspaces/kiln-<id>`), its harness's folder the same place in the volume as the one
asked for. What the agent hasn't touched is read from the folder as it is now; what it changes
(edits, build output, git's own writes) is held in the volume alone, so the folder and other
agents never see it. Ended for good, the agent's volume goes with it.

*Why not one base per folder, pinned and cloned, as first planned:* Slates (c8d6adb4) refuses a
clone of an overlay's snapshot (`ContentUnavailable`, pinned or not), and pinned files count as
diverged, so every pinned file shows up in a landing's plan as a replacement. So each agent gets
its own unpinned overlay: instant to make, memory only for what it changes, and a file it changed
that then changes on disk is reported as drift (`agents.workspace`).

**W4 Changes** are its harness's as today; its change request (C1) takes everything that changed in
its volume, its commands' changes too, since nothing else writes there.

**W5 Merging** agents' work into one volume (Slates' greens and work volumes, byte-exact conflicts,
rebase) waits on Slates: a work volume isn't a mountable volume yet (Slates' GAP-A9-14), so an agent
can't run its tools in one. Until then work merges where it always has: as change requests, through
review and the land queue (C).

**W6 Writing to disk.** **Write to disk…** in its Changes (`agents.land`) shows Slates' plan first:
what it would create, replace and delete (git's own directory left out), and any file changed in
the folder since the agent changed it, in which case
nothing is written until they agree. On a person's go-ahead kiln gives Slates the grant for exactly
that plan and lands it; no agent can (refused by kind, whatever it asks). Slates holds a plan only
while the command that made it runs (a client's departure abandons it, AUD-29-07), so kiln plans
again at the go-ahead and grants only when the new plan's manifest is the one the person saw; if
anything changed, nothing is written and the new plan is shown instead. Slates' command line
leaves out one path per landing (each `--exclude` keeps its last value, though its wire format
takes several), so kiln leaves out `.git` and writes only when the plan's files are no more than
git sees changed in the workspace; when the agent's build output or dependencies (what git
ignores) would be written too, its work goes up for review instead, until Slates' command line
takes several. Slates takes a grant from
kiln where its anchor's segment has a name kiln can pass on (macOS, Windows). On Linux it doesn't
yet: the segment is a descriptor only the anchor's own children hold (Slates' GAP-A9-10), so the
plan says so, and the work goes up as a change request instead.

**W7 Moves and restarts.** A volume outlives the runner's restarts (its anchor runs apart). A
machine's restart loses it: Slates keeps volumes in memory. An agent with a volume doesn't move to
another machine yet (refused, saying why); that waits on a snapshot of an overlay that can be
cloned, or shipped, elsewhere.

**W8 Shards, next.** `isolation: "shards"` runs the harness in a microVM built from an image (an
Agentfile, W21-R01), its folder the agent's Slates volume, its network what kiln allows. Shards
can't host a harness yet: guest networking and a file device (virtio-fs or vhost-user to Slates)
are still to come in Shards. Until then New agents shows Shards with why it isn't available.

## C: From changes to landed

**C1 Into review.** **Open PR** in an agent's Changes (or the agent itself, through
`agents.change`): its work, from its volume or folder, committed on `agents/<name>` and pushed to
kiln's git as a change request (W14-R04). The author is `agent:<id>`, on behalf of its person.
For a repository whose provider is elsewhere (GitHub, GitLab, Bitbucket), a branch pushed there
and a pull request opened through that provider. Built: the commit is made beside the folder (a
scratch index from its last commit, with the agent's files), so a person's branch, index, working
tree and own edits are never touched; where kiln has no integration with the provider, the branch
is pushed with the machine's credentials and the change's link opens the pull request. A plan
run's steps go up as a stack, one diff per step (P4).

**C2 CI comes back to the agent.** The checks on its change show in its Changes; a failure goes to
the agent as a message with the failure excerpt (W20-R10's bundle) when its person allows it
(**Auto-Fix**).

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

Built (docs/agents.md, "Attached agents"): `crate::ci::attach` keeps attachments and incidents,
listens to kiln's events (`run.failed`, `deploy.failed`, `leg.flaky`, `land.ejected`,
`runner.offline`, failed upgrades, `@mentions`) and ticks for `stuck`, `schedule` and `check`; one
cause is one incident (its signature), within a day's limit; its agent acts for whoever attached
it, held to its capabilities on what it looks after, and reports with `incidents.report`. Agents →
Attached attaches, pauses and removes them and lists the incidents; Mission Control puts the agents
an attachment woke under it; a run's page carries its incident card; a repository can keep its
attachments in trunk's `.kiln/agents.yaml` (ci-agents.md's format). Not yet: a workspace of its own
per incident (Slates, held).

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
