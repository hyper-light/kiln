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
| G | Goal stacks as a graph, run side by side on subagents | O, P | in progress (docs/goals.md, "What a goal waits for"). The person's decisions, 2026-10-24: order is inferred as far as it can be, then goals fan out; while subagents are in play the stack's agent coordinates, and otherwise it does the work itself; a plan is a subgraph drawing on the same pool, filled greedily; with no Subagents setting there's no maximum; a goal's subagent ends once its goal is met; every subagent at work draws on the session's pool. G1 (built): what a goal waits for (`needs` on `goals_add` and `goals_update`, `goals_order` for several at once), with loops refused, a goal taken out passing on what it waited for, and the next goal the first one ready (in the stack's order where nothing says otherwise). G2: the ready goals out to subagents, with the agent coordinating, and goals that say nothing ordered by the agent first. Built of G2 (2026-11-05; docs/goals.md, "Goals side by side" and "Several goals under way at once"): several goals of a stack under way at once, each ending alone; the agent sets goals under way itself for its own harness's subagents (`goals_update` `active`), beside its goal under way, which stays the one kiln sends it and holds it to; and on a stack that runs side by side (`side_by_side`), its goals ready together that say what they wait for go each to a subagent of the agent's (P2's subagents, under way there and here), as many as its Subagents setting leaves room for, the rest waiting for room while the agent coordinates; a goal's parts that say what they wait for (a plan inside it) fan out the same way; met there is met here, not met comes back to the agent, and pauses, removals, clears and restarts are followed as P2's are. Not built: kiln asking the agent to order the goals that say nothing (today they go in turn, on the agent). G3: a plan's steps as a subgraph of the same pool (with P2's subagents). G4: one pool per session. G5: docs, metrics and G-AC1, a live check |
| R | Runners: diagnostics, upgrades and repair, by agents and people | O | built: R1–R5 (docs/releases.md, "Diagnosing and repairing a runner"; docs/agents.md, "Attached agents"): R4 is an agent attached to the runners, woken by a failed upgrade, a failing check or a machine offline (the CI's `runner.offline`), held to `diagnose` and `manage-runners`, its incident on the runner's row in System → Updates. R-AC1 passed 2026-10-19 on a throwaway server and runner (a self-build without npm failed; Claude Code as the runner doctor diagnosed it as a missing dependency, retried by download with a person approving its calls, the runner upgraded, and the doctor resolved its incident once told the upgrade went through: in the lab by hand, the message kiln now sends itself) |
| W | Workspaces: Slates volumes, then Shards microVMs | Slates (merging: its mountable work volumes), Shards (networking, file device) | built: W1–W4, W6 and W7 as far as Slates allows (an overlay volume of its own per agent, its change requests from it, writing it to disk by the write mode kiln gives its volume, approved with kiln's own launcher secret: everywhere kiln starts Slates itself). Since 2026-11-18 kiln vendors Slates (`vendor/slates`) and drives it through its library alone, never its command line: kiln's own executable is the instance's anchor and daemon, so no machine installs Slates. Since 2026-11-18 (the person's decision, kiln goal 01a1ec34…) a volume is what every agent on a machine's host gets unless a person chooses its folder (W2), an agent started in another agent's folder works in that agent's volume, which goes with the last of them, an agent lands as its granted mode lets it and never raises its own permission (W6), and a Windows machine with WinFsp mounts each workspace at a drive letter of its own (W1; its run on Windows owed: kiln's `workspace::bare_metal_tests`, and since 2026-11-19 `slates_e2e`'s bare-metal drive, which ran on Unix alone). **Held** (2026-10-19: Slates is far from complete): W5 merging between agents. Moving an agent with its volume to another machine is built inside kiln's Slates fleet (W7, kiln goal 95101ef3 K4, 2026-11-18): by Slates' own move, refused by kind where it can't be; a listed runner's workspace is a clone of its folder's captured base, so it moves (K5, 2026-11-18), and one laid over its folder elsewhere is refused `not_plain`. Since 2026-11-18 an agent started in a person's shell whose folder lies in another agent's mount works in that agent's volume, as one started in another agent's folder does: its machine, reading the shell's folder after the server chose a volume of its own for it, takes the volume it is in (`workspace::from_shell`), where it was laid over that mount and Slates refused it (A-227). Since kiln took Slates at ce9b6905e (contract 42, Slates' A-507) its library's unmount runs no program on macOS (`unmount(2)`, where it ran `umount` by name, so a runner whose `PATH` lacked `/sbin` couldn't take a workspace's mount down) and is believed only when the kernel's mount table agrees; kiln still keeps a volume whose mount stays rather than destroy it beneath its mount. W8 built as far as Shards' D286 on its branch allows: an isolated session's volume served to its microVM with no mount, its change request from the volume's own changes (its real-VM run owed). On Linux KVM and Windows WHP (W8's plan, UI goal 88, 2026-11-18) the VMM and the device are the same, a Windows stream a pipe whose DAX asks Slates judges by a live holder; built of it: an isolated start's volume needs no host mount, so a WHP host without WinFsp or a KVM host without FUSE gives one (`lineage::WorkspaceUse`), and a Windows folder's place is judged as Windows judges it, so its harness is told its folders as its microVM sees them however Windows spells them (`sandbox::windows_path`, 2026-11-19). Since kiln took Slates at 38ed3dfd8 (contract 50, Slates' A-587) a microVM's run writes the volume Slates serves it again: Slates read a Shards guest's owner word (its A-281) in the short SETXATTR form after its A-378 made a guest's kernel send the extended one, so the run was shown its share as the host's and its create refused `EACCES` (main's macOS microVM drive, r134). W9, an agent in a container of its own on its volume, bound from the host's mount (UI goal 88, 2026-11-18): designed on Linux, macOS and Windows; built of it: Slates' bind as a library verb (A-520), `workspace::container::{start, end}`, and (2026-11-19) a runner's facts saying whether its agents can run in containers and what it lacks, `sandbox::container`, and (2026-11-20) the `container` engine, set by a runner's owner, its sessions launched in containers on their volumes; a harness's sign-in there by a secret its environment grants by name. W10, the Kubernetes engine, designed 2026-11-19 (kiln goal 28e73c34's part 01a1f124…): an agent's pod works in its home runner's volume through kubelet's mount of that runner's Slates export over RPC-with-TLS (no copy on a node, no CSI driver, no privilege); built of it: a listed runner's node certificate names the host it's listed at, so a node's RPC-with-TLS client can verify the export, and (2026-11-19) a client of kiln's server takes a certificate kiln's CA issued only in the server's role, the server's own or a front door's, so neither a node's nor a capacity holder's stands in for the server at a name they share; kiln's link takes a runner only in a runner's role, reissuing over it, once, the certificate of a runner enrolled before that role held, and a linked server is trusted only in a server's |
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
  sort share a row, and the composer is at the top, whole, as on a desktop and as the agent page's
  composer: in the page's flow, no sheets or overlays.
- When this device has notifications off, one quiet line offers to turn them on.

**Acceptance.** N-AC1: an approval on an agent reaches a subscribed phone as one notification
within 2 s of the harness asking, and tapping it opens the agent, verified with a real Chromium
push subscription in a test (and by the user on an iPhone). N-AC2: a second state of the same agent
replaces the first, and none shows while a focused window is on that agent. N-AC3: a dead
subscription (410) is dropped. N-AC4: on a 390×844 screen the composer and the first agent row are both
in view without scrolling (the composer sits above the list).

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
their agent's chat, and so do the subagents kiln starts for an agent's goals and plan steps side by
side, and for the steps of a plan run an agent runs (docs/goals.md, "A plan's steps side by side"):
those are its subagents (`subagent`), on its machine with its settings, in its chat and on its row.
Ending a parent lists its live children and offers to end them too.

**O3 No more than its parent.** A child's permission mode is at most its parent's (plan < ask <
accept edits < auto < bypass); bypass needs the parent's own `allow_bypass`; its `reach` is within
the parent's. Depth is limited (default 3, `[agents] max_depth`), and so are an agent's subagents
at work, to its own Subagents setting: one pool whatever each does, with no limit when it has none
(a server may still set `[agents] max_children` for every agent). Every call is audited as
`agent:<id>` for its person.

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

Since 2026-10-24 a plan run on an agent goes as its graph says too. As goals (a run as **Goals**, or
a goal's plan, its steps its parts), a step's goal waits for the steps it waits for, and where more
than one step can be under way at once kiln hands each ready step to a subagent of the agent's own,
started for it and held to it on its own goal stack, as many at once as there's room for in the
agent's pool; the agent takes the steps there's no room for, and a step not met by its subagent
comes back to it (docs/goals.md, "A plan's steps side by side"). As one message or a step a turn,
the agent is told which steps go side by side and to give each a subagent of its own, at most its
setting at once. A run with an agent per step waits for room in its parent's pool too, rather than
stopping while others in it are at work. The pool: every subagent the agent has at work, whatever
each does (the agents it started that work, wait on a person or are held to a goal still to go,
and its harness's own), at most its Subagents setting, with no limit when it has none; one idle
with its work done (a met step's agent kept for review) holds no place.

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

**W1 Slates on every machine.** kiln vendors Slates (`vendor/slates` at one revision,
docs/vendoring.md) and drives it through its library (`slates-api`, Slates' A-278) alone, never its
command line (the person, 2026-11-18): every verb is a call of the library on a client of its own,
run on kiln's blocking pool (`crate::workspace::slates`). kiln carries no Slates binary, and no
machine installs one: kiln's own executable is Slates' anchor and daemon (`kiln slates-role
--instance <name> anchor`, and the daemon its anchor starts the same way), handed to Slates' own
main before kiln's runtime starts, so the daemon a runner talks to is the same code as the client it
links, built with every kiln on every platform kiln builds (CI builds it natively on each:
`.github/workflows/ci.yml`, Linux on x86-64 and arm64, Apple silicon, Windows on x86-64 and arm64). A role's process ends at any panic, as
Slates' own builds do, so its anchor starts a daemon again rather than leave one serving with a
thread gone. A runner keeps one instance, `kiln`, for all the machine's runner processes. The
instance's anchor starts apart from the runner: on Linux, where the runner runs under the user's
service manager (its user unit), in a systemd user scope of its own (`systemd-run --user --scope`,
which execs the anchor with its standard input kept, so kiln's launcher secret reaches it); in its
own process group elsewhere. So restarting or upgrading the runner never ends it (a volume lives
in the anchor's memory). kiln waits for a new instance's first answer while its anchor lives, with
no deadline of its own (the anchor judges its starting daemon by the daemon's own progress, never a
second): it asks the anchor to stop if none of its daemons ever serves (Slates' A-366,
`StartSpec::until_served`), so a start that can't come up ends saying so rather than waiting for
good, and a slow one under load is waited for. A new instance gets its one-node group. Runner facts say whether workspaces work
there and why not, in the library's words (`workspaces`: the Slates revision kiln links, a mount it
can make: FUSE through `fusermount3` on Linux, NFS on loopback on macOS, WinFsp on Windows,
installed once from winfsp.dev), and `GET /api/harness/workspaces` says it for every machine. On
Windows each workspace is mounted at a drive letter of its own, the highest free, since Slates mounts
a WinFsp volume at a drive letter alone (a directory mount point is an NTFS reparse point, a write to
the disk it refuses), so a Windows machine holds as many workspaces at once as it has letters free
(`workspace::place`); a letter Slates refuses as another volume's (its A-369 `DriveLetterTaken`: the Mount
Manager has given it to another volume, or a definition local to the daemon's session hides it) is passed
for the next free one, as is a letter another start took meanwhile. `KILN_SLATES_INSTANCE` names another instance (labs, tests), and
`KILN_SLATES_DIRECT` starts its anchor in its own process group on Linux too; every process of an
instance carries `--instance <name>`.

**W2 Every agent on a volume (the person's decision, 2026-11-18, kiln goal 01a1ec34…).** kiln's bare-metal
engine runs every harness and agent on a Slates volume: a start that names no `workspace` gets **its own Slates
volume**, whoever starts it (a person, an agent, a plan, a goal stack). Where its machine can't make one, the start is
refused, naming what the machine lacks (`lineage::NoWorkspace`: `fusermount3`, `/dev/fuse`, WinFsp, `mount_nfs`, a
kiln new enough, as the runner's facts type it, `workspaces.missing`); no agent runs in its plain folder instead, which
would be a second path. `workspace`: `volume` asks for one (refused the same way where the machine can't make one) and
`folder` (the folder itself) is a
person's explicit choice, kept as made, in Mission control's composer (More options, where Own volume is now the
choice made) and `agents.start`. Several agents on one folder each get their own. An agent started in another
agent's folder on that agent's machine (a child placed in its parent's folder, above all) works in that agent's
volume, as two agents in one folder did (`workspace.owner` names whose it is): a volume is never laid over a volume,
which Slates refuses (its A-227). An isolated agent's workspace is its microVM's boundary's to share (W8); a volume
asked for one is served to its microVM and never mounted, so a machine that lacks only a host mount (WinFsp,
`fusermount3`, `/dev/fuse`, `mount_nfs`) gives it one, and refuses one only for its agents on its host
(`lineage::WorkspaceUse`). A
conversation taken up (`resume`: a found session taken over or continued) works on a volume like any other, its
harness's record of it moved to where the harness looks from the mount's folder before it starts: Claude Code's
transcript (and its session's own files beside it) to the project folder Claude Code keeps for the mount's folder,
where `--resume` looks; Gemini CLI's chat to the project it keeps for that folder, where `session/load` looks; one
record of it, not two, as a move to another machine keeps it. A branch of one (`--fork-session`) reads a copy there,
the conversation itself staying where the person may go on with it. Codex goes on by its thread's id, given the
mount's folder (`thread/resume`), and an ACP agent is given that folder in `session/load`; a record that can't be
moved refuses the start, saying why. When any agent on a volume ends, its conversation goes back the other way once
its processes have ended (its harness writes the record until it ends: W3) and before its mount goes (the project its
harness kept for the mount's folder goes with the volume): Claude Code's transcript
and its session's own files, and Gemini CLI's chat and its subagents' folder, to the project its harness keeps for its
person's folder, where they resume it from where they work; one already there is written over in neither place, both
stay, and kiln logs why (`homes::NotMoved`), and a branch's copy of another conversation stays where it is. An agent
started in a person's shell
(`shell`) works on a volume like any other, laid over the shell's folder as its machine reads it at the start (or, where that folder
lies in another agent's mount on that machine, in that agent's volume, at the same path: the server chose a volume of its own
before the folder was read, and a volume is never laid over a volume, `workspace::from_shell`), and it
is handed the shell's environment moved onto the mount with it: every path in it that lies in that folder, a whole
value or each path of a list (`OLDPWD`, an activated virtual environment's `VIRTUAL_ENV` and `PATH` entry, a
`CARGO_TARGET_DIR`), names its place on the mount instead (spelled through a link or not), and its harness's `PWD` is
the folder it starts in; the harness's own homes (`HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GEMINI_CLI_HOME`,
`XDG_*`) stay where kiln reads its records, and nothing of its environment names the person's folder, so a command
that trusts it works in the volume, never past it (no second path; the composer offers Own volume for a shell's agent
as for any). A server sets the default with
`[agents] workspace`: `volume` (kiln's own) or `folder`, the folder itself as before; kiln's test servers and
scratch hubs say `folder`, so no test leaves a volume or a mount behind on the machine that runs it, and a test of
workspaces asks for its volumes.

**W3 How.** On the agent's machine, before its harness starts: an **overlay volume** laid over its
folder's repository (the library's overlay over `<top>`), mounted under the runner's state
directory (`workspaces/kiln-<id>`), its harness's folder the same place in the volume as the one
asked for, as git places it (`git rev-parse --show-prefix`: a path spelled through a symbolic link, or on
Windows with a short 8.3 name or a name in another case, isn't the one git names the repository's top by).
What the agent hasn't touched is read from the folder as it is now; what it changes
(edits, build output, git's own writes) is held in the volume alone, so the folder and other
agents never see it. Ended for good, the agent's volume goes with it, once no other agent works in it: the last
of the agents working in one ends it (on its machine's runner process), and an agent that couldn't start after all
takes only a volume made for it alone with it. Its mount goes first, once every process kiln started for the agent has
ended: its harness, which works in the mount, and its group (every process that started, followed as the agent's), each
end as the OS tells it (a pidfd on Linux, kqueue on macOS, a process handle on Windows), never a clock. A process whose
end the OS can't tell (no pidfd before Linux 5.3, descriptors exhausted) is refused by kind and not waited on, where it
held the release until the runner restarted (2026-11-19). Then the mount is asked to go once. Its volume is destroyed only once the kernel's mount table no longer holds its point (an unmount
that answers done is believed only when the table agrees); one whose mount stays is kept, never destroyed beneath a
live mount, which would leave the mount with nothing behind it, and never let go by force: what holds it once the
agent's processes have ended is another's (a person's shell or editor there), which a forced unmount would cut off.
The one mount kiln lets go of by force is one a gone Slates instance left (2026-11-19): kiln follows the machine's
instance from its daemon, as Slates' own status names it, and at that daemon's end, as the OS tells it, reads the
table's workspace mounts and asks Slates. A daemon its anchor restarted serves the same mounts again (the anchor holds
the NFS listener and the daemon's descriptors), so its end is followed next; no anchor holding the instance, or another
instance, means nothing will serve them again, and a process stuck on a hard NFS mount ends only once it's forced off,
so each is let go of (Slates' `mount::unmount_busy`: forced on macOS, detached on Linux; a WinFsp drive goes with its
daemon). Never on a clock: no end told, nothing let go.
kiln refuses it by kind (`MountStays`), as Slates' contract 42 types the unmount's answer (its A-507): busy, which the
OS says itself on macOS (`MountFault::Busy`, its `EBUSY`) and Linux's `fusermount3 -u` doesn't (an exit code alone, so
there a refusal while `lsof` names processes holding the mount), naming each process that holds it as `lsof` does (the
system's own, `/usr/sbin/lsof`, on macOS); refused for another reason, in the refusal's words and `lsof`'s; or
answered done while the table still lists it (`MountFault::StillMounted`). Before, kiln asked again on a clock for as
long as the agent's own end had taken, then let the mount go (detached on Linux, forced off on macOS: Slates'
`mount::unmount_busy`, contract 36);
kiln's clippy refuses a clock there (`rt::sleep`), and the let-go cut off whoever still worked in it. kiln reads that table
with the system's own `mount` (`/sbin/mount`) on macOS, never a program found on `PATH`. kiln names a mount by its real path, as
the kernel's mount table names it (macOS reaches `/var`, where temporary folders lie, through a link to
`/private/var`), and judges whether it's still there by that table, never by looking into the mount, which on a hard
NFS mount waits on its server and takes any refusal for no mount. A mount is its own attachment in Slates (its
file-system bridge's), lasting until it's unmounted, as a microVM's stream lasts until it closes (W8). On Windows a
drive letter is any workspace's to take, so a workspace's drive is its own only while its volume reports a mount
there: a Slates drive at its letter that it doesn't report is neither used as its mount at a restart nor taken down at
its end, which detaches its volume's own attachment at the letter. None of kiln's attachments
has the term Slates gives its command line's, which lapses once nothing renews it (Slates' A-384), so nothing kiln
keeps between an agent's turns needs renewing.

*Why not one base per folder, pinned and cloned, as first planned:* Slates (c8d6adb4) refuses a
clone of an overlay's snapshot (`ContentUnavailable`, pinned or not), and pinned files count as
diverged, so every pinned file shows up in a landing's plan as a replacement. So each agent gets
its own unpinned overlay: instant to make, memory only for what it changes, and a file it changed
that then changes on disk is reported as drift (`agents.workspace`).

**W3 on a runner in kiln's Slates fleet (K5, kiln goal 95101ef3, 2026-11-18).** An overlay's base
stays on its machine, so a move refuses it (`not_plain`). On a runner the server's `[slates_fleet]`
lists (W7), asked at each workspace's start, a workspace is instead a clone of its folder's captured
base (`workspace::slates::provision`): Slates captures the folder whole into a base of its own
(`Instance::capture_base`, its A-291: an overlay over the folder whose every file's bytes become the
volume's own, frozen into a snapshot, a file that changed while read captured again after Slates'
pause, at most three times), and the agent gets an O(1) clone of that snapshot, which reads no folder:
a plain volume a move takes. Agents started over one folder while it is as captured (Slates re-checks
every entry the capture witnessed and finds none changed) share one capture; a folder that changed
since gets a new one. Review and landing work against the capture: what the agent changed since it,
and nothing else, is its plan and its change request, each change judged by content against the
capture as it lands (Slates' A-424), so an outsider's edit made after the capture to a file the agent
didn't change is passed, and one to a file it did change is a conflict. Its drift is its base's: what
changed on disk since the capture. Its landing still waits on its grant (W6). A base goes with the
last workspace that's a clone of it, once Slates has finished that clone's destroy and gives its pin
back. A capture reads the folder whole, what git ignores too (build output, dependencies), into
memory, where an overlay reads only what the agent uses: so only a fleet runner, whose workspaces
move, pays for one, and a folder larger than a workspace's share of memory is refused there in
Slates' words. Owed: a capture of what git tracks alone; a second landing judged against what the
first landed (Slates' A-424 advance: an outsider's later edit of a file the agent landed is a
conflict until then). An agent whose volume moves with it works at the
mount its volume is placed at on the runner it goes to, its folder and its environment with it: every path of its
environment on the mount it left (moved there as it started, W2: an activated virtual environment, a
`CARGO_TARGET_DIR`) names its place on the new one. Only a runner in kiln's Slates fleet captures: a workspace
elsewhere can't move (a move needs the fleet), so a capture there would read the folder whole into memory for nothing.

**W4 Changes** are its harness's as today; its change request (C1) takes everything that changed in
its volume, its commands' changes too, since nothing else writes there.

**W5 Merging** agents' work into one volume (Slates' greens and work volumes, byte-exact conflicts,
rebase) waits on Slates: a work volume isn't a mountable volume yet (Slates' GAP-A9-14), so an agent
can't run its tools in one. Until then work merges where it always has: as change requests, through
review and the land queue (C).

**W6 Writing to disk.** **Write to disk…** in its Changes (`agents.land`) shows Slates' plan first:
what it would create, replace and delete, and each file whose mode alone it sets (Slates' contract 41) (what git ignores
and git's own directory left out), and any file changed in the folder since the agent changed it, in which case nothing is written until they agree. kiln
speaks Slates' typed consumer contract (slates-api's A-299, pinned at its `CONTRACT`): a plan is
Slates' present-only answer, which never writes whatever the volume's mode; a landing answers one
typed outcome, written, held for approval, conflicted or refused. A landing follows the write mode
the agent asking to write was given (the person's rule, 2026-11-18, kiln goals 01a1ec34… and
01a1ec34…569750…): kiln, the launcher, decides it from the lower of the asking agent's permission mode
and that of the agent whose work it is (an agent lands its own work, or the work of one it started;
`workspace::landing_policy`) and sets the mode its agent starts in as the workspace's policy when it
makes the volume (`workspace::slates::set_policy`), so a landing is written at once in a mode that edits unasked (accept edits, auto, bypass), held as an
approval request in one that asks (Claude Code's default, ask), and refused in one that only plans.
No agent raises its own permission (the person's rule, 2026-11-18): each agent's mode is judged as far
as it was granted (`lineage::granted`), the lower of its mode now and the level kiln recorded as its
launcher started it (`launched`, on kiln's one scale of modes, so it means the same in whichever
harness the agent goes on in; kiln's alone to record, never what a start asks, and carried through
restarts, moves and switches of harness). A harness that reports a higher mode, or an agent that
moved itself, writes nothing more, and a mode lowered since, by anyone, holds; so does what it hands
its children and its goals' and plan steps' subagents. Nor can an agent move itself: every way a
command reaches an agent (the REST command, the console's socket, `agents.broadcast`,
`agents.control`) refuses an agent's `set_mode` above its own grant, its own or a child's, and an
agent's change of any permission control (Codex's approvals and sandbox, an ACP agent's mode options:
`lineage::may_command`); a person sets those. A mode set by command is the agent's grant from then
(`State::launched`, which the session moves only on a `set_mode` it takes, and which carries it to the
server's copy; its spec carries it through restarts and moves): kiln sends one only as a person asks,
any mode, or as an agent may, so a person's raise of an agent's mode is a grant and its landings follow
it, and a mode lowered by anyone is the grant from then. So is a person's change of a permission control
(Codex's approvals and sandbox: kiln's own mode for the pair, as its adapter computes it on taking the
control, never Codex's word; an ACP agent's mode option: its mode word), which kiln sends only for a
person (`session::control_grant`). Its volume's write policy follows the grant
too (`workspace::follow_grant`, under kiln's launcher secret; a volume another agent owns keeps its
owner's). A session an older kiln began has no launch level recorded and is held to its harness's own
default.
`agents.land` follows the same rule: an agent in a mode that edits
unasked lands at once, its plan as it stands or the manifest it names; one in a mode that asks reads
its plan, and its writing waits on its person's click (`crate::confirms`: asked in their chat turn, 428
until they confirm, then the same call runs once as theirs; outside a turn there's no one to ask, and
it's refused); one that only plans lands nothing (403). Its answer names the outcome (`outcome`:
`written` with Slates' outcome, `approval_required` with the plan to approve by its manifest,
`conflicted` with each conflict typed, or `refused` with the policy's refusal), beside the fields
the UI reads. A landing whose plan meets conflicts (a file the agent changed that a person changed on
disk since) writes nothing whoever asks, a mode that edits unasked included: a conflict never writes
over the person's bytes, and there is no manifest to approve until a person settles it. A volume lands whole, so an agent working in another agent's volume
lands nothing of its own while that one works (409, naming whose it is). Setting a policy and
approving are the launcher's acts, proven by a secret of kiln's own: made once, kept 0600 in its
state (`slates-launcher.key`), and handed to the anchor at every start on its standard input, so
kiln's authority outlives its own restarts and the daemon's on every system. An approval is bound
to its request and manifest, not to the client that presented it, so kiln presents again at a
person's go-ahead and approves exactly the manifest the person saw; if anything changed, nothing is
written and the new plan is shown instead. kiln leaves out what git ignores in the workspace (the
agent's build output and dependencies, never written into a person's folder) and git's own
directory, each a prefix of the volume in the request's filter; and it writes only when the plan's
files are no more than git sees changed in the workspace, which holds whenever what git ignores is
left out (a build still running can add more). On Linux the anchor's systemd scope keeps the secret on
its standard input (Slates' `StartSpec::anchor_command_under`, contract 14), so kiln approves there as
everywhere. An instance an earlier kiln started as a systemd service was handed no secret, and refuses
kiln's policies and approvals until it starts again.

**W7 Moves and restarts.** A volume outlives the runner's restarts (its anchor runs apart). A
machine's restart loses it: Slates keeps volumes in memory. An agent with a volume doesn't move to
another machine yet (refused, saying why); that waits on a snapshot of an overlay that can be
cloned, or shipped, elsewhere.
Built toward it (kiln goal 95101ef3…, its first piece, 2026-11-18): each runner's identity as a node
of kiln's Slates fleet (`workspace::slates_fleet`): its key made on the runner and never sent, its
certificate from kiln's CA (`POST /api/agents/slates-fleet`, the machine's proof as a second runner
process gives it) for the fleet's name and the runner's own failure domain alone, kept as Slates'
fleet manifest reads it, with kiln's CA as the fleet's enrollment root. Its second piece: discovery
(`POST /api/agents/slates-fleet/members`: a runner says where its node is reached, taken only for
its own machine's node and an address Slates can dial, and hears of every node whose runner is
connected) and Slates' fleet manifest written beside the node's files (every node's address, failure
domain and certificate, this node's key, kiln's CA as the root). Its third: the operator's choice, in
the server's configuration (off until declared):

```toml
[slates_fleet]
f = 1                                   # a write commits at f + 1 of 2f + 1 copies: at least 2f + 1 runners
[slates_fleet.nodes]
"adas-mac-studio" = "192.168.1.20:17300"  # where each runner's node is reached: IPv4 or DNS name, and the
"adas-macbook-pro" = "192.168.1.21:17300" # base of its two UDP ports, which the other runners must reach
"adas-mac-mini" = "192.168.1.22:17300"
```

A listed runner asks its place as its machine's instance starts (`POST /api/agents/slates-fleet/place`),
makes or renews its node's certificate, says its word (taken only at its listed address) and hears the
others', writes the manifest, has Slates' own reader read it (`slates_api::role::check_fleet`, which
refuses what the instance's start would), and starts its instance as the fleet's node
(`StartSpec.fleet`). A runner not listed starts its instance alone, as before; one listed that can't
join starts none until it can. A listed runner starts its instance as it starts (a node holds the
fleet's copies only while its daemon runs), and says its node's member id once it serves. One listed
runner founds the fleet's consensus groups (its place says `founds`: the first the setting lists, the
same at every start), bootstrapping the root once its node serves; every other imports it. Until one
did, no fleet runner made a workspace ("this node has no consensus group yet": goal 91's live move,
2026-11-19, seen on three runners of one Mac).

Its fourth: an agent moves with its workspace volume (Move to…, `harness::relocate`;
`workspace::slates_move`). The server plans it: both runners in `[slates_fleet]`, the target's node
serving (the member id its runner said), no other agent working in the volume, both runners new
enough (`proto::feature::SLATES_MOVE`); else it's refused by kind (`[kind: not_in_fleet]`,
`target_not_serving`, `shared`, `older_runner`, `source_away`) and the agent stays, running. Once its
turn is over and its harness is down, its runner lets go of the workspace's mount and asks Slates to
move the volume to the target's node (`Instance::move_volume`, Slates' A-291: the head handed over and
served there, the old copy frozen and then let go), following Slates' answers until the configuration
names the target; Slates' refusals come back by kind too (`attached`, `already_moving`, `not_plain`,
`lineage`, `snapshots`, `not_a_member`, `outside_neighbourhood`, `unplaced`), the workspace mounted
again and the agent going on where it was. Then the session is handed over; the runner it goes to
places the volume's mount under its own place for workspaces and mounts it before the harness starts.
A workspace laid over its machine's folder is refused `not_plain`: its base stays on its machine.
A listed runner's workspaces are clones of a captured base (W3, K5), so they move. A workspace that
moved here from another runner lands as it would have there: its move carried what Slates judges it
by (Slates' A-503: the captured state of every path the agent changed, in the volume's catalog, and
the moved head, which the new owner keeps), so its plan holds the agent's changes since the capture
alone, before the move and after it, and an outsider's later edit of a file the agent didn't touch is
passed. Its landing waits on its grant (W6) as any's does, into the folder it was laid over, as
this runner's disk names it. Owed: a workspace taken over after its runner's loss (no move carried
its capture), which Slates still lands as a volume with no capture.

**The multi-runner test with injected faults (kiln goal 95101ef3, its part 60178ceb; Slates' A-580).** What the fleet
promises, proved by faults rather than said (`tests/slates_fleet_faults.rs`): three runners of a test's own server,
each its own machine (`KILN_MACHINE_ID`) with its own Slates instance (`KILN_SLATES_INSTANCE`), listed in the server's
`[slates_fleet]` with `f = 1` and nodes on loopback, form kiln's Slates fleet the way W7's runners do (each node's key
and certificate from kiln's CA, discovery, the manifest Slates' own reader takes, the instance started as the fleet's
node). An agent of the stand-in harness (no model) works in a captured clone on the first runner (W3, K5), and files are
written into its volume before each fault, each one Slates acknowledged. Each flow injects one fault, with no privilege
(as Slates' own `live_chaos.rs`: a stopped process is the unprivileged partition):
- **a runner killed**: the instance holding the agent's volume loses its anchor and daemon at once (SIGKILL, the
  machine's loss), and the fleet takes the volume over;
- **a runner partitioned**: its anchor and daemon stopped (SIGSTOP) past the fleet's failover delay, then let go on;
- **a runner restarted**: its runner and its instance stopped and started again, rejoining as the same machine;
- **a link lost mid-move**: an agent's move (`agents.move`) to the second runner, whose node is stopped once the move
  has begun and let go on after the failover delay.

Two more flows fault the links themselves (A-708), with Slates' own fault relay (`vendor/slates/crates/cli/tests/support/
relay.rs`, A-277) between the nodes: each runner is listed at a block of ports the relay holds and its node binds a block
of its own, which `[slates_fleet.binds]` names and kiln writes into the node's own manifest entry as Slates' `bind`, so
every datagram between two nodes crosses the relay and meets its path's rule, every draw from one printed seed
(`SLATES_CHAOS_SEED` replays it). The relay attributes a node's record links by its daemon's pid, told again whenever
that daemon is another process; each flow first checks its record links crossed the relay (A-330's check):
- **a slow, lossy link**: every path holds each datagram 40 ms ± 10 ms (so they reorder) and loses 1 % while files are
  written and the agent is moved to the second runner;
- **a runner cut off while it runs**: every path into and out of the volume's node cut past the failover delay, its
  processes running on (the partition a stopped process can't make), the files read through the other two, then healed.

Every flow ends with the same three verdicts, each from what an agent or a person can see, judged by pure code that
runs on every host (`workspace::slates_faults`): **no acknowledged change lost** (every file Slates acknowledged before
the fault is served after it byte for byte, by whichever node serves the volume then); **the move completes or refuses
by kind** (kiln's word on it, once it settles, is the agent working on its target with its workspace, or a refusal
naming a kind W7 lists, `workspace::slates_move::KINDS`, with the agent working where it was: never neither); and **the
fleet re-forms** (once the fault ends, every listed runner's node is a member again, serving). It is a test of what
holds, not of how fast: no latency is judged, and every wait is the fleet's own failover delay as its nodes report it.
Linux alone, where a workspace is a FUSE mount: on macOS it's a kernel NFS mount over loopback, and a daemon killed or
stopped under a live NFS mount has panicked the Mac (Slates' record of 2026-10-06), so the macOS flows wait until a
runner lets its workspace mounts go before a fault. Gated by `SLATES_FLEET_FAULTS=1` and skipped, saying so, without it,
since it starts three instances and their anchors:

```sh
SLATES_FLEET_FAULTS=1 cargo test -p kiln --test slates_fleet_faults -- --nocapture --test-threads 1
```

Not in it: a captured clone landed after its runner's loss and a moved live clone landed against its capture, which are
Slates' own fleet tests (its A-583); one link cut while every node runs, which needs a relay between the nodes that
kiln's manifest can't name yet (Slates' `bind`, its A-277); and runners on separate machines, the same flows driven
through `kiln run --on` once this one holds.

**W8 Shards.** An isolated session (`isolation: {"kind": "microvm"}`) runs its harness in a Shards
microVM built from an image (an Agentfile, W21-R01; docs/isolation.md), its network what kiln allows.
Its workspace of its own is a Slates volume made with no mount (kiln goal 01a1ec34599974448f6b7663ddb07001):
Slates serves the volume to the microVM itself, over Slates' own virtio-fs device and DAX window, on a
stream kiln asks of its instance and hands Shards through Hex (Shards' D286, Hex's `ShardsImage::served`,
`sandbox::served`), so nothing of it is mounted on the machine and the harness works at its project's own
path in its guest. Its change request is built from the volume's own changes (`workspace::review`: Slates'
landing entries, each file read from the volume and checked against its BLAKE3, and a file whose owner's execute bit
alone changed (all git keeps of a mode: git's `ce_permissions`) read there and checked against the folder's copy, whose
bytes its landing keeps; A-287; where the repository's `core.fileMode` is false git trusts no execute bit, so none
is committed: a written file keeps its index's mode, a new one is `100644`, and a mode change is one git can't see,
kiln goal 90), and its landing
is the same Slates landing as a mounted workspace's, leaving out what git ignores as a mounted one does: read from
the volume itself (its own `.gitignore` files, the repository's excludes and the person's, over what the landing
writes: each ignored file, or the shallowest directory git ignores above it), never from the runner's own folder,
which a working tree's `git -C` at an empty mount would read; a mode change git can't see holds it back for review.
Owed: its real-VM run (`tests/slates_e2e.rs`'s
`microvm::` drive); Shards' D286 on Shards' main (carried as a recorded change of vendor/shards until then).

**W8 on Linux KVM and Windows WHP (UI goal 88, kiln goal part 01a1f1249c5e75b5bc82c9eb011550f4; planned
2026-11-18).** What runs on macOS (Hypervisor.framework: the microVM drive's review step, run r1697) is the same kiln
code on Linux and Windows. Where the hosts differ:

- **The VMM: Shards' own, everywhere.** Every microVM is Shards' VM process (`shards-vm`), its hypervisor chosen as it
  is built (`vendor/shards/crates/vmm/build.rs`: `hvf` for macOS arm64, `kvm` for Linux x86_64 and aarch64, `whp` for
  Windows x86_64 and aarch64), each backend with the share slot's DAX window (`crates/vmm/src/hv/`,
  `devices/virtio/fs/window.rs`). kiln carries it built for its own platform (`ci/shards/pack.sh <target>`; Linux's
  VM and network processes static, musl), and a runner says whether its host runs it in Shards' words
  (`sandbox::facts_of`, `shards_api::host_check`: `/dev/kvm` missing or not this user's; the Windows Hypervisor
  Platform off). Owed (docs/shards.md): kiln's releases packing Shards for Linux and Windows, which carry none yet.
- **The device route: the same device, another stream.** Slates' own virtio-fs device in that VM answers on the
  stream kiln asks of its instance (`sandbox::served`, `Instance::serve_shards`) and Hex hands Shards' run host
  (D286). On Linux, as on macOS, the stream is a Unix socket passed as a descriptor, and a DAX map's copy rides the
  stream itself. On Windows it is a duplex pipe's end that Slates' daemon duplicated into kiln's process, Hex into
  the run host and the run host into its VM (`--slates-fs-handle`; Slates' A-179), and a DAX ask is answered with a
  section duplicated into the asking process only once its handle proves to be the same pipe as a live known
  holder's (Slates' A-180): the attaching client, kiln's runner process, or the last claimant that passed. kiln's
  copy goes as the boot answers (Hex lets it go), so a VM whose first DAX ask comes later is refused it (`EACCES`)
  and its read through the map fails. So on Windows kiln keeps the stream it attached until its run ends, handing Hex
  a duplicate (piece 3). Below kiln this route is proven: a Shards guest on GitHub's windows-2025 mapped a Slates file
  through its WHP window over the daemon's stream (2026-10-27, Slates §4.6).
- **Windows' volume: no WinFsp for a microVM.** A Windows host's own agents work in a WinFsp drive (W1), but an
  isolated agent's volume is made with no mount and served over the stream, so a host without WinFsp (jupiter, a WHP
  host, has none) holds isolated agents on volumes of their own, as a Linux host without FUSE does. Its review is the
  volume's own entries (A-287) and its landing Slates' Windows writer (A-519, A-527), neither through a mount.
- **Windows' paths.** A Windows project is seen in its guest at `/work/<its name>` (`sandbox::isolated::guest_path`).
  A served project's place is the volume's top as git names it, `C:/…` with forward slashes (Git for Windows'
  `normalize_ntpath`), while the session's folder is spelled as it was given (`C:\…`, `c:\…`, `\\?\C:\…`); matched
  as text, its harness was told its folder as the host spells it, which names nothing in its guest (piece 2).

In order, each its own piece:

1. **Built (this plan's first piece, 2026-11-18): an isolated start's volume needs no host mount.** Its workspace is
   judged by what a served volume needs (`lineage::WorkspaceUse::Served`: Slates and kiln's workspaces, no host
   mount) at both doors: `agents.start` by the boundary it places it in, `/api/harness/sessions` by the boundary the
   registry will give it (`Registry::isolation_for`, the rule `Registry::open_as` decides by). A runner's facts say
   that its kiln keeps no workspaces before what mount it lacks (`workspace::slates::facts`), so a host mount's lack
   alone means the rest is there. Test: `harness::lineage`'s
   `w8_an_isolated_agents_volume_needs_no_host_mount_so_a_runner_without_winfsp_or_fuse_serves_one`.
2. **Built (2026-11-19): Windows' paths.** A Windows path lies in a folder wherever Windows would open it there,
   judged from its text on any host (`sandbox::windows_path`): its prefix stripped as Git for Windows'
   `normalize_ntpath` strips it (`\??\`, `\\?\`, `\DosDevices\`, then `UNC\`) and `\\.\` too, `/` and `\` alike and a
   run of them one, `.` dropped and `..` taking back its parent, and a drive's letter, a share's names and every
   folder's name compared without case (Microsoft Learn, "File path formats on Windows systems"). `Seen::said` and
   `served_project` both judge by it, so git's `C:/…` top meets the session's `C:\…` folder and its harness works,
   and is told it works, at the same place in its guest. Still unjudged: a short 8.3 name or a link, which only the
   disk resolves (as git resolves its top, `GetFinalPathNameByHandleW`), and a share git names `//server/share`,
   which `guest_path` takes for a Unix path. Tests: `sandbox::windows_path`'s
   `w8_p2_a_windows_path_lies_in_a_folder_wherever_windows_would_open_it_there` and
   `w8_p2_a_windows_projects_folders_are_told_as_its_microvm_sees_them_however_windows_spells_them`.
3. **Windows' known holder** (kiln, Windows only, so built for main's Windows run): `served::SlatesVolumes` keeps the
   handle Slates' daemon gave kiln's process for each box until that box's run ends and hands Hex a duplicate, so
   every DAX ask of the VM's is judged against a live holder (A-180); its end closes the kept handle, after which the
   attachment lasts while the VM's copy does.
4. **Where a served volume is offered.** `/api/harness/workspaces` says per machine whether it gives a volume served
   to a microVM beside whether it gives a mounted one, so the composer offers Own volume for an isolated agent on a
   host without WinFsp, and the e2e microVM drive's readiness reads it.
5. **The drive on Windows.** `tests/slates_e2e.rs`'s `microvm::` drive is still `#[cfg(unix)]`, where its bare-metal
   sibling runs on Windows since 2026-11-19 (a drive letter judged Slates' by `slates_mounted_at`, the host's
   processes read through CIM's `Win32_Process` as `vm_e2e` reads them); its stand-in runs in the guest, so opening
   its gate is those looks over a session with no mount, which no drive letter may show.
6. **The runs, main's:** the `microvm::` drive on adacachy (KVM, now) and on jupiter (WHP, once 2, 3 and 5 are in),
   each through `kiln run` on a kiln carrying Shards packed for that host.

Owed on every host alike: Slates hands a successor daemon's fresh stream to a device over its resync channel (A-316,
A-339; Windows A-427), which `served::SlatesVolumes` drops (it hands Hex the stream alone, which is all D286 takes), so
a Slates daemon's restart ends a microVM's share.

**W9 Containers (UI goal 88, kiln goal 01a1f124…; designed 2026-11-18, Slates' A-520).** An agent whose runner runs
its sessions in containers works in an OCI container of its own on its Slates volume: the same volume W2 and W3 make (an
overlay over its folder; on a runner in kiln's Slates fleet a clone of the folder's captured base), mounted on its
machine through Slates' library and bound into the container, never copied into it. So its review, drift and landing
are W3, W4 and W6's, unchanged: kiln reads the mount on the host, and Slates plans and lands under the grant its
launcher set. Today no agent runs in a container (a runner's engine is `host` or `microvm`, `harness::engine`; the
seam's `Boundary::Container` is refused by every implementation); CI's container legs (`runner::docker`) are a
different thing, a leg's folders bound into a job container at GitHub's paths. The engine is the machine's own
container engine, reached through its command line as CI's legs reach it (`runner::docker::find`: `docker`, else
`podman`, by its full path), the engines Slates' container form was proven through; the seam's plan of Hex's rootless
`oci` class runs no container on a Mac or on Windows, and Slates' entry is in the OCI runtime specification's own words,
which a bundle Hex runs takes as it is, so a Linux runner can take that class later with none of this changed. In order:

1. **The engine's profile, first.** Before the workspace is made, kiln asks the engine for its profile and has Slates
   judge it (Slates' handshake: the endpoint its command line reaches, refused unless a local socket or pipe, since a
   bind's source resolves on the engine's host; the engine's kind, version and security options; judged by Slates'
   `slates_bridge_oci::runtime::judge`): a profile a container workload has run through, with how a container's ids
   reach the volume and how hard links behave through it, or a typed refusal (a remote engine, a profile no workload has
   run through), which refuses the start in Slates' words before anything is made. The handshake is Slates' library
   call `container_engine::engine_profile(runtime)` (A-520, contract 47; its command line's `slates oci-runtime` makes
   the same call), whose `EngineFault::Untested` names the engine and profile it refused.
2. **The mount, on the host.** `Instance::mount` at the workspace's place (W3: `workspaces/kiln-<id>` under the
   runner's state, by the real path the kernel's mount table names), shared with other users only where the profile
   needs it. A rootful Linux engine reaches the mount as users other than the person (the engine itself, as root, and
   its containers), so it's mounted `shared` (FUSE's `allow_other`, which the machine's operator grants once with
   `user_allow_other` in `/etc/fuse.conf`; Slates refuses `AllowOtherNotGranted` without it, and kiln names it as what
   the machine lacks, as W2 names `fusermount3`). A rootless engine maps the container's root to the person, and Docker
   Desktop reaches the Mac's folders as the person (below), so neither shares it (Slates refuses `shared` on a Mac).
3. **The bind, verified by Slates.** `Instance::bind_into_container(volume, mount point, destination, intent)` (Slates'
   A-520, contract 44): Slates checks in the kernel's mount table, never by looking into the mount, that the path is
   exactly this volume's mount (on Linux its own mount, by its record: A-227), records the binding, which lasts until
   it's detached or its mount goes, and answers the runtime's `mounts` entry: a non-recursive `bind`, `ro` or `rw` by the
   intent, `private`, `nosuid` and `nodev`. kiln never makes a bind of its own: it hands the engine that entry word for
   word, just after Slates' check that the source is still the mount it verified (`ContainerBind::check_source`, by
   the kernel's table and the mount id it never reuses, contract 47: a mount gone or replaced in between is never
   bound, where the engine would bind the bare folder left there). It binds with `--mount` and never `-v`, which makes a missing source on the host (Docker's
   bind-mount documentation), so a mount gone is the engine's refusal (`bind source path does not exist`), never an
   agent writing into the person's disk: Docker's `type=bind,bind-recursive=disabled,bind-propagation=private`, and
   `readonly` for a read; podman's `bind-nonrecursive` and `ro=true`. Neither engine's `--mount` has a word for `nosuid`
   and `nodev` (podman's `-v` takes both, `[no]suid` and `[no]dev`), so the container's own rules hold them:
   `--security-opt=no-new-privileges` (no process of the container gains a privilege by executing a setuid or setgid
   file) and `--cap-drop=MKNOD` (no device node made through it), the host's mount being `nosuid,nodev` already.
4. **Where the agent works.** The container sees its workspace at the same path the host mounts it, and its harness
   starts at the same place in it as it would on the host (W3's rule), so every path kiln reasons about on the host
   means the same inside (the agent's folder, its environment moved onto the mount (W2), a child started in its folder
   working in its volume): nothing is translated. Where the profile forwards a container's ids as the host's own (a
   rootful Linux engine), the container runs as the person's uid and gid, so what the agent makes is theirs and the
   permission bits hold; under rootless podman (`ContainerRootAsMountingUser`) it runs as the container's root, which
   the engine maps to the person, the one id an unshared mount admits.
5. **Its end.** The container goes first (its engine lets go of the files it held open), then the binding is detached,
   then the mount comes down as W3's does.

On each platform:

- **Linux.** The engine runs on the machine's own kernel and binds the FUSE mount itself. Slates proved it by use with a
  rootful Docker Engine over a shared mount (its T-4.13 Linux leg, its linux-oci CI lane) and with rootless podman (a
  program in the container mapping a Slates file through the bound mount: its G-mount step 3, 2026-10-25). Slates' judge
  holds the rootful Docker Engine's profile (`ContainerIdsAsHostIds`; every name of a hard-linked file served at once)
  and, since its A-520 third piece (contract 48), rootless podman's on its own Linux host (`engine_profile("podman")`:
  `ContainerRootAsMountingUser` over an unshared mount, which needs no `user_allow_other`), the case that needs no
  privilege anywhere; it still refuses a rootless Docker engine, podman run as root and a remote podman, each typed.
  Rootless podman's live check (the container's other ids refused, its hard links) is Slates' run, filed.
- **macOS.** Docker Desktop runs its engine in a Linux VM, and a bind's source resolves there, through Desktop's file
  sharing, which carries the Mac's folders into the VM over the Virtualization framework's virtio-fs device (Slates
  measured `virtiofs0 /run/host_virtiofs/Users` on 2026-10-27; `/private` is shared as `/run/host_mark/private`,
  2026-09-14). So the volume reaches the VM as its loopback NFS mount does, through the share, as the person: a runner's
  workspaces lie in its state under the person's home (`~/.local/share/kiln/agent/workspaces`), inside `/Users`, which
  Desktop shares unless its owner removed it; a runner whose state lies outside Desktop's shares is refused, naming the
  folder to share. Slates' judge holds this profile (`HostUserThroughShare`: every container id reaches the volume as
  the person; T-4.13 proved it by use). Two of Slates' measurements shape the engine here. The share resolves a hard
  link's other name by the first path it saw, so a file linked and then unlinked by its first name is missing by its
  other name for a moment (git finalizes its objects so): the container's git takes `core.createObject=rename`. And the
  share holds every file a container touched open past the container's life, so a delete inside leaves the NFS client's
  silly-renamed `.nfs.*` entry in the volume, which blocks an `rmdir` and the plain unmount (the end's forced unmount
  lets go of it, W3), and which must never land in the person's folder or go up in a change request: kiln leaves the NFS
  client's silly-rename names out of both (built 2026-11-20: Apple's NFS client names such a file `.nfs.%08x.%04x`, a
  counter and the low 16 bits of the process's id, `kext/nfs_vnops.c`; `workspace::git::silly_renamed` tells the name
  exactly, a commit's `git add -A` leaves it out by pathspec, `SILLY_RENAMED`, and a landing's excludes take each one git
  sees untracked).
- **Windows.** A Windows container can't bind a workspace: Slates ruled it out by a run (its G-mount step 4, 2026-10-26,
  CI's Windows runner, run 40257213499): Docker's engine is a service in a logon session of its own, and each logon
  session has its own drive letters (Microsoft Learn, "Services and Redirected Drives"), so it never sees the WinFsp
  drive Slates' daemon mounts as the person ("bind source path does not exist"); a drive every session sees needs the
  file system run as LocalSystem, which Slates' daemon never is; and past that the bind filter refuses a custom file
  system's volume (microsoft/Windows-Containers#335, closed by design). So a Windows runner's containers are Linux
  containers in Docker Desktop's WSL 2 VM, which reaches Windows' drives through WSL's 9P file server (`drvfs`, as Slates
  measured on CI's `windows-2025`, 2026-10-27). Whether that server, which serves the signed-in person's drives, reaches
  a WinFsp drive Slates' daemon mounted in that person's logon session is the Windows leg's first measurement. Slates'
  container form refuses Windows today (`Unsupported::Attachment`, `HostPlatform`); verifying a WinFsp drive as the
  volume's mount by the daemon's own record of it, as its Linux rule does (A-227), is Slates' to add once the measurement
  says the share reaches it (A-520). If it doesn't, the route is the volume's network export (Slates' NFS server runs on
  Windows: its A-184), mounted in the VM by the engine, which is its own measurement.

Built (2026-11-19, A-520's fourth piece): kiln takes Slates at contract 48 (de1031f1d), and the engine's first
runnable step, `workspace::container::{start, end}`: an agent's container started on its workspace, mounted already
(W3), by Slates' bind, its source checked just before the engine binds it, and the engine handed Slates' entry word for
word as its `--mount` says it (`engine_mount`), with the container's own rules for the two words `--mount` can't say
(`nosuid`: `--security-opt=no-new-privileges`; `nodev`: `--cap-drop=MKNOD`); a start refused after its bind lets the
bind go; and its end in order, the container, the bind, then W3's mount and volume. Its order and every refusal are
held by unit tests over a stand-in engine. Built (2026-11-19, A-520's fifth piece, 5a): a runner's facts say whether
its agents can run in containers (`RunnerFacts.containers`, `workspace::container::facts`, probed with its other facts),
asking in the order that asks least: its workspaces, its engine (`runner::docker::find`), Slates' judge of its profile
(`engine_profile`), then whether kiln runs a container under that profile's identity rule; what it lacks is typed
(`ContainerMissing`: `workspaces`, `engine`, `profile`, `shared_mount`, `platform`) and said in Slates' words where Slates
refused. Built (2026-11-19, 5b-i): the seam's `Boundary::Container` over the machine's engine (`sandbox::container`),
one sandbox per workspace: an idle keeper container started by `workspace::container::start` at the launch's own folder in
the workspace (refused outside it), the harness run beside it through the engine's `exec -i`, whose stdio are the
machine's pipes, its environment named on the command line and never valued; as the harness ends or is killed, the
container and its bind go (`workspace::container::release`) and the workspace stays for W3; it holds none of the seam's
limits and forwards no signal, each refused typed. Built (2026-11-20, 5b-ii): a runner's
`container` engine beside `host` and `microvm` (`harness::engine`), only ever its owner's choice, never a default: set
(`PUT /api/runners/:name/engine` `{"engine": "container"}`) only where its facts say its agents can run in containers, and
held as `host`, saying why, where they later can't; the image its agents run in is its administrator's (`[agent]
container_image`), and a runner that declares none says so in its container facts (`image`); a new conversation placed
on it is marked apart from a microVM's `isolation` (`Spec::container`, `InContainer`: its runner's image), drains by that
mark on a switch, and starts through `sandbox::container::launch` in its workspace volume, mounted on the machine, its
harness by name on the image's `PATH` with what a boundary's run is given (`Kind::elsewhere`), with no kiln MCP (its socket
is the machine's). A harness signs in in its container by a secret its person's environment grants it by name (built
2026-11-20, the person's decision: never an image's own): the person's own secret (`secrets.mine`), opened by the server
for that session alone, is given to its harness as it starts (`Kind::in_container`), named on the engine's command line
and valued only in that command line's own environment, gone with the container; a grant taken out of the environment, or
the secret deleted, gives the next start none. Owed, in order, each a piece: a workspace mounted shared for a rootful
Linux engine (until then refused, `NeedsSharedMount`); whether a bind keeps the mount's `nosuid` and `nodev` without
the container's rules (a setuid file and a device node made through one container's bind, used through another's),
measured; the `.nfs.*` names on a Mac; Windows' measurement and its route; and the goal's proof, a
test that starts an agent in a container through kiln on Linux, macOS and Windows (a heavy run, main's).

**W10 Kubernetes (kiln goal 28e73c34's part 01a1f1249c5e75b5bbf0c92c84385109, designed 2026-11-19).** An agent on
kiln's Kubernetes engine runs its harness in a pod of its own and works in the Slates volume the bare-metal engine would
give it (W2, W3), which kubelet mounts into the pod from the network export of the runner that holds the volume: never a
copy of the folder on a node. Slates decided the form (its SLATES_DESIGN.md §4.6 "Kubernetes publication without
privilege", AUD-29-75) and proves it with kubelet's own mount on KIND (`cargo xtask kind export`: a pod writes through
the mount, a second pod reads the bytes back, a pod's map follows a change made by another path). What follows is kiln's
part.

- **The volume stays at home.** Its *home* is the runner whose machine holds the agent's folder, which makes the volume
  there with no mount, as an isolated session's is (W8): an overlay laid over the folder (W3), on a fleet runner too,
  where W3 makes a captured clone only so that a volume can move with its agent, and a pod's never does (the pod, not a
  runner, is where its agent runs). The home's instance is a node of kiln's Slates fleet (W7: listed in
  `[slates_fleet]`, where a fleet of one at `f = 0` is Slates' own solo case, "the same code path"), because Slates
  serves the network export only on a fleet node whose manifest names an authority to check clients against (its
  `deploy::plan`: the probe plane's address and port, over TCP, on Unix), and kiln's CA is that authority. A Slates
  daemon on each node (a DaemonSet) was weighed and rejected: Slates lays an overlay only over a folder on its own
  machine (its base plane reads its own disk alone, R1), so a node's daemon could hold only a copy of the folder (a
  captured base moved there, K5), whose landing back onto the person's folder waits on Slates' A-424 stage 2 (W7); and
  every pod's volume would sit in its node's memory beside the pods, where kubelet's accounting doesn't see it. With the
  volume at home, review, drift and landing are the bare-metal engine's, unchanged. A daemon per node is worth its cost
  only once measurement shows NFS round trips across the network dominating an agent's tool time, and a moved clone can
  land.
- **kubelet mounts an `nfs` PersistentVolume, over RPC-with-TLS.** For each pod the server makes a PersistentVolume:
  `nfs.server` the home's host as `[slates_fleet.nodes]` lists it, `nfs.path` the pod's capability path
  (`/<name>@<attachment>.<token>`, `slates_api::mount::export_path`), and `mountOptions: [nfsvers=4.2, xprtsec=mtls,
  port=<the home's listed base port>, nosuid, nodev]`; a PersistentVolumeClaim in kiln's jobs namespace bound to it by
  name (`volumeName`, `storageClassName: ""`), the PersistentVolume reserved for that claim alone ("Specify the relevant
  PersistentVolumeClaim in the `claimRef` field of the PV so that other PVCs can not bind to it": Kubernetes,
  "Reserving a PersistentVolume"); and the pod's volume from the claim, mounted at the folder's own path, as an isolated
  session's guest has it (W8). A pod's own `nfs` volume can't be used: it takes no mount options, so it can't ask for
  `xprtsec=mtls`. Nor can a CSI driver: its node plugin mounts with `Bidirectional` propagation, which Kubernetes allows
  only in a privileged container, and Slates holds no privilege (its R10); kubelet, the node's own mount broker, mounts a
  PersistentVolume instead (both cited in Slates' §4.6).
- **One capability per pod.** As each pod starts, the home asks its daemon for an export attachment of the volume for
  that pod alone, bound to one Unix identity (Slates' A-115): that uid's calls are served, and every other caller, root
  included, is shown attributes and an `ACCESS` that grants nothing. The uid and gid are those that own the folder at
  home, and the pod's containers run as them (`runAsUser`, `runAsGroup`), so permission bits mean in the pod what they
  mean in the folder (Slates' §4.6 item 5). The path's token is half the capability, and RPC-with-TLS's mutual
  authentication the other half: a node that presents no certificate from kiln's CA gets no session (Slates' export
  takes TLS 1.3 alone, and only a client whose certificate chains to the fleet's root), so the token never crosses the
  network in the clear. Slates' export takes any client certificate under that root, and kiln's CA also signs its
  runners' own, so it's the token, never a TLS identity alone, that reaches a volume. A pod's end detaches its
  attachment, after which its path reaches nothing (Slates' detach rules), whatever kubelet still holds; the next pod
  gets a new token. The PersistentVolume carries the path, so reading it takes the cluster-wide right to read
  PersistentVolumes. Owed by Slates (A-521): its library has no export verb (slates-api serves a host mount and a VMM's
  descriptors; the command line's `slates export` attaches on the raw client), so Slates' first piece here adds one,
  answering the attachment and its path with typed refusals, and kiln calls that, never the command line or the raw
  client.
- **Two certificates, both from kiln's CA.** *The export's:* Slates serves the export with the node's fleet identity
  (`fleet::serve_export`), and a node's client checks it against the name its mount used: "Server certificate validation
  MUST include a check on whether the locally configured expected DNS-ID or iPAddress subjectAltName of the server that
  is contacted matches its presented certificate" (RFC 9289 §5.2.1; ktls-utils' `tlshd` hands the mount's server name to
  GnuTLS's `gnutls_certificate_verify_peers3`). So kiln's CA signs a listed runner's node certificate for the host its
  node is listed at too, beside the fleet's two names: an iPAddress for an IPv4 address, a dNSName for a name, as the
  operator wrote it, which the PersistentVolume names as written. Built 2026-11-19 (`Pki::sign_slates_node`,
  `workspace::slates_fleet::node_names`): the server takes the host from `[slates_fleet]`, never from the request, and
  refuses kiln's own reserved name there as it does for every certificate it issues; a node certificate that doesn't
  name the host its runner is listed at no longer stands, so a runner listed before asks again as its instance next
  starts, keeping its key; and the server takes a runner's word only with a certificate naming the host of the address
  it announces. Like a capacity holder's, a node's certificate serves either side of a session, so the host the operator
  lists for a runner should be that runner's alone. And since kiln's CA signs the server's certificate too, a node's, or
  a holder's (named as its machine), for a name the server is reached at would stand in for the server to a client that
  checks only the name it dialled; so every certificate's role is told by marks only its own issuer sets, and a client
  of kiln's server takes one kiln's CA issued only in the server's role: the server's own, the one that names
  `kiln-server` (which every other issuer refuses), or a front door's, the server's entrance on a runner (serverAuth its
  only extended key usage, its organization `kiln front door`). The QUIC link already checked the server under
  `kiln-server` alone; built 2026-11-19 for every HTTPS client of the server (`Roots::WebPkiPlusDer`: a runner's calls
  and its upgrade, a microVM's leg, a front door's upstream, a peer under its own CA; `pki::answers_as_server`,
  `http::tls`'s `KilnServer`, which asks the server's CA first, so the rule holds where a machine also trusts kiln's CA
  among its own roots). A client that isn't kiln's (a browser, `curl`, a node's `tlshd`) checks names alone, so for it
  the hosts the operator lists remain the guard. The link checks a runner's role the same way (built 2026-11-19): it
  takes a certificate in the runner's role (clientAuth alone) and refuses a holder's, a keeper's or a Slates fleet node's
  by the kind kiln recorded as it issued it (`identities::link_refusals`, read as the server starts and added to as each
  is issued). A runner enrolled before machines' certificates held their role (ba0ecd654, 2026-10-17) holds one serving
  both sides that no record holds (kiln records what it issues since 2026-10-26): it's taken until the runner says it
  takes the runner's role (the `runner-role` feature), which is then reissued it once over that link for the same key
  and name; the runner keeps it, presents it from its link's next connection on, and says so, and from that word on,
  recorded as a revocation of the old certificate's serial, the old one is refused: an event, never a date. A runner
  that never says it keeps its old certificate. A linked server (`deploy::link`) is trusted under its integration's CA
  by the same role rule, never by its name alone. *The node's:* Linux's NFS client hands the handshake to ktls-utils'
  `tlshd` (kernel `CONFIG_TLS` and `CONFIG_NET_HANDSHAKE`) with one certificate for the node: the pods on a node share
  its NFS client and its session, so the client is the node, never a pod ("The tuple (serial number of the presented
  certificate; Issuer) uniquely identifies the RPC client", RFC 9289 §5.2.1). kiln's CA issues it to the cluster's
  operator for each node's `tlshd` (`x509.truststore` kiln's CA, `x509.certificate` and `x509.private_key` the node's,
  its key `0600`, as Slates' `deploy/kind/tlshd.conf` has them), installed once per node by the operator, as
  `user_allow_other` is: never a privilege kiln or Slates holds. Owed: that issue (an operator's call, recorded as every
  certificate kiln issues is, W27-R01).
- **Review, drift and landing at home.** The server asks the volume's home, not the pod's runner (a workspace names its
  home runner where the two differ): its change request is built from the volume's own changes (`workspace::review`, as
  W8's), its drift is Slates' drift of the overlay there (W3), and its landing is W6's: the agent's granted mode, set by
  kiln, the launcher, as the volume's write policy, and written by the home's daemon into the person's folder under that
  grant. The pod never writes the folder, and nothing reaches its disk except through a landing.
- **In order.** (1) A start asks for the engine (planned: `isolation: {"kind": "pod"}`, beside W8's `microvm`), refused
  where the server has no `[kubernetes]`, or where the folder's machine isn't in kiln's Slates fleet or is Windows, where
  Slates serves no export. (2) The home makes the volume with no mount, sets its write policy (W6) and asks its daemon
  for the pod's export, answering the server with the export's host, port and path and the uid and gid. (3) The server
  makes the PersistentVolume, the claim and the pod in kiln's jobs namespace (`crate::k8s`, as it makes a job's pod),
  whose harness kiln's own runner in the pod starts in the claim's mount (`kiln agent --ephemeral`, connecting back over
  QUIC with mutual TLS, as a `k8s` pool's executor does). (4) At the agent's end, or for a pod that never starts, the
  server deletes the pod, the claim and the PersistentVolume, and the home detaches the export's attachment and ends the
  volume by W3's rules. A pod kubelet can't mount (its events' `FailedMount`: a node without `tlshd` or its
  certificate, or no route to the home) fails its start in kubelet's words, as a job's pod that can't start does
  (`k8s::reap_loop`).
- **What the cluster needs.** Each node: a kernel with `CONFIG_TLS` (`modprobe tls`) and `CONFIG_NET_HANDSHAKE`;
  nfs-utils' `mount.nfs`, which takes `xprtsec` (kind's node image carries nfs-common 2.8.3, as Slates records); ktls-utils'
  `tlshd`, running with the node's certificate and kiln's CA (wanted by `multi-user.target`, since a kind node never
  reaches `remote-fs.target`: Slates' `deploy/kind/node-tls.Dockerfile`); and TCP to each home's listed address and base
  port. kiln's service account: in its jobs namespace, PersistentVolumeClaims to create and delete beside its pods (it
  may only `get` them now), and PersistentVolumes to create, get and delete, which belong to no namespace (a
  ClusterRole). Nothing else: no CSI driver, no privileged container, no `hostPath`, no mount propagation, no
  StorageClass.
- **Proof, and the order it's built in.** The proof (owed, main's run): KIND on a Linux machine whose kernel has
  `CONFIG_TLS` (Docker Desktop's has none, as Slates measured on 2026-10-01), through kiln: an agent on the Kubernetes
  engine edits in its pod; its change request shows the edit; a file a person changed in the folder meanwhile shows as
  drift; its landing writes the folder only under its grant; and a pod deleted mid-session leaves no attachment, claim
  or PersistentVolume behind. Built in this order: the certificate naming the host (built); Slates' export verb (A-521,
  vendored since contract 50) and kiln's call of it; the PersistentVolume, claim and pod as pure builders, unit-tested
  as a job's pod is (built 2026-11-19: `k8s::volume::objects`, from the home's export, `PodExport`, whose `Debug` hides
  the token; an export a pod can't mount refused typed, `Unbuildable`), with the chart's RBAC (built 2026-11-20: `rbac.agentPods`,
  off until declared, grants the claims' create and delete in the jobs namespace and a ClusterRole for
  PersistentVolumes' create, get and delete alone; `k8s::volume::{make, unmake}` over kiln's client, the
  PersistentVolume before its claim, a refused claim's PersistentVolume taken back down, the claim before its
  PersistentVolume as they go); the pod's runner (built 2026-11-20, the person's decision: one code path, a runner's
  sessions, workspace, reporting and teardown reused): `kiln agent --ephemeral --session <id>` holds that one agent
  session's harness, which no other ephemeral runner does, and leaves once it has come and gone, its uplink drained, or
  gives up where it never came within the ephemeral wait (`agent::session_runner_ends`); and the pod itself (built 2026-11-20,
  `k8s::volume::agent_pod`): the harness's image running that runner, copied in by the init container a job's pod has,
  enrolling with a one-time token, the workspace's claim mounted at the folder's own path and worked in there, run as the
  folder's owner, no deadline; the home's part (built 2026-11-20:
  `workspace::held`, the `hold` and `release` asks of a runner that names `workspace.hold`: its volume made with no
  mount, its write policy from the agent's grant, exported to the folder's owner through `Instance::export`, the path,
  attachment, uid and gid answered, and every question about the pod's folder answered from the holding, as an isolated
  session's are, never of a folder the server names; let go, the export detached, then the volume ended); the engine and the workspace's home; the end and its reaper; the node certificate's issue; the proof.

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
the thread's id, the file, the lines, the version and commit they're on, who wrote it and their
words quoted, and a link; it answers in the thread, amends and pushes a new version that names the
threads it fixes (`threads`), and each thread then says which version fixed it (review.md,
Agents; ci.md W26-R18). Another agent's comment reaches it only where the repository lets agents fix
code from agents' feedback (off until someone who can land turns it on), saying why, at most three
times a thread until a person speaks there; Ship's Hand off sends it every open thread at once. A
draft stays a draft through its versions until the agent (`ready`) or its person (Ship's Ready)
marks it ready, on kiln and on GitHub, GitLab and Bitbucket alike.

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
