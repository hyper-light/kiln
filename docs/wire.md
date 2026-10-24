# The wire: what every message kiln sends is held to

Kiln's processes talk over three kinds of wire:
- runners and the server, over QUIC lanes (`crate::proto`'s `AgentMsg` and `ServerMsg`);
- the UI and the server, over WebSockets;
- a session and wherever it runs, inside `HarnessCmd` and `Harness` (`Command`, `Update`).

Each message is in one **class**, and the class says what its sender may count on. This page lists
every message with its class, why, and what it does today. `crate::wire` holds the same tables
(`wire::class_of` gives any message's class), matched without a catch-all arm, so a new message
can't be added to a wire without a class, and a test (`cargo test -p kiln wire::contract`) fails
when this page and the tables disagree.

Why it matters: QUIC is reliable only while its connection lives. "A CONNECTION_CLOSE frame causes
all streams to immediately become closed; open streams can be assumed to be implicitly reset"
(RFC 9000, §10.2). So whatever is in flight when a link drops is gone, unless the application
keeps it and sends it again. Only the process that applied a message can say it was applied, so
acknowledgements come from that process, never from the transport under it.

## The classes

| Class | What its sender may count on |
|---|---|
| handshake | Part of connecting. The connection being made or refused answers it. |
| command, parked | It carries an id, and the process that applied it answers: an ACK, or a NACK saying why not (refused, unknown, stale epoch, unsupported, malformed). It is sent again under the same id until answered, and taken once however often it comes. While its receiver is away it is kept, then delivered in order once the receiver is back. |
| command, refused while away | As a parked command, except that while its receiver is away it is refused at once as unreachable. Done later it would surprise (a keystroke, an interrupt), so it is never replayed. |
| command, as its `Command` says | `HarnessCmd` carries one `Command`, and that command's own row decides. |
| ask | A question asked under a `req`, answered by an answer. Its asker waits with a deadline, and a lost link answers it at once with a typed error. |
| answer | The answer to an ask or a command. |
| ack | An acknowledgement, or a NACK, itself. |
| stream | An item of an ordered stream: a session's timeline, a terminal's output, a job's events. Items are numbered and acknowledged cumulatively. After a reconnect they are sent again from where the receiver says it has them, and a snapshot is sent only when the sender no longer has what's missing. |
| state | The newest of something that changes: an inventory, a session's state, a terminal's size. The newest wins, and it is sent again on every connect, so one lost to a cut comes once the link is back. |
| carried (…) | A protocol of its own, carried on the link, that acknowledges and resends by itself: the conversation feed (`convo/feed.rs`), repo sync's Raft, the cluster group's learners (docs/p2p.md), reach's links, ports' windows, the tunnel, and the channel itself (`crate::wire::channel`, whose items carry other messages, each with its own class). |
| telemetry | Measurements sent every few seconds. The next one stands in for a lost one, so there is no ACK. |
| media | Live pictures and sound. A lost frame is overtaken by the next and never sent again. |

## Who builds what

Ada made this kiln's top priority on 2026-10-23: nothing kiln sends is fire-and-forget, and no
crash or reconnect replays a whole session. This split was proposed to the T3 Sends agent by
message on 2026-10-23; its answer is awaited. Messages *to agents* are critical-path track T3's,
on `cp/t3-sends` (its `docs/sends.md`):
- `Send`, `SkillRun` and the queue's commands: their ids made once (CP-T3.2), the inbox that takes
  each once (CP-T3.2), their states end to end and parking for a runner that's away (CP-T3.3);
- the composer's durable outbox (CP-T3.4);
- the Uncertain state after a crash mid-handoff (CP-T3.5).

Everything else here is kiln plan `01a16ba4988b71358e7edb6f309a5986` ("Nothing kiln sends between
its processes is fire-and-forget"):
- the channel each runner and the server keep, numbering, acknowledging and resending;
- every other command answered;
- runners' updates acknowledged and sent again after a reconnect;
- the UI's commands answered;
- loss below the channel counted and repaired;
- a lab that injects faults.

Messages to agents ride the same channel, and their answers are T3's inbox answers.

## Commands

The server's commands to a runner process ride that process's command streams on the channel
(`crate::wire::commands`), one stream for each way a command waits:

- **Parked** (`p:<born>.<gen>`): a leg to cancel, a session to close, a state's newest. While its
  runner is away it waits. After a reconnect it goes again from where the runner has the stream.
- **Live** (`l:<born>.<gen>`): a leg to run, a terminal or a page to open, what a viewer does. It's
  refused at once while its runner is away. A link that drops before it's answered ends it
  `unanswered`: sent again later, it would surprise.

The runner process takes each command once however often it comes, applies it as if it had come
alone, and answers it: an ACK, or a NACK with its kind (`unknown`, `refused`, `stale_epoch`,
`unsupported`, `malformed`) and why. Within a live link, a command whose answer is late goes
again (`Want` and patience, as for any item: "Loss below the channel").

Its caller gets one of these answers:

| Answer | What it means |
|---|---|
| `taken` | The runner applied it. |
| `refused` | The runner refused it, saying how and why. |
| `unreachable` | It never reached the runner: away for a live command, gone for good, or too many waiting. |
| `unanswered` | The runner may or may not have applied it: its link dropped before it answered a live command, or no answer came in time. |
| `parked` | A person's parked command to a runner that's away: it goes once the runner is back. |
| `sent` | Sent to a runner from before the channel, which can't say. |

Callers take it in one of two ways:
- **An API call waits for it** (`Hub::command`). Its HTTP error says what the runner said: 404 for
  `unknown`, 409 for another refusal, 503 for `unreachable`, 504 for `unanswered`.
- **A caller that mustn't wait gets it to come** (`Hub::command_start`): the scheduler, and the
  hub as it takes runners' messages, which is where the answers come from. It acts on what it
  knows at once (a leg whose runner is away goes back in the queue) and logs the rest.

A runner process that started again has neither stream. Its parked commands not yet answered go
again on a new generation, and its live ones end `unanswered`. A parked command waits for its
runner a day at the most. A forgotten runner's waiting commands end `unreachable`. Commands live
in the server's memory: one that restarts lets go of them, which their callers see as `unanswered`.

## Loss below the channel

kiln's QUIC layer (`crate::net`) holds frames a lane can't take yet in a queue of 8,192 frames per
peer and lane. It counts every frame it lets go before the wire, by lane and reason, in
`kiln_net_frames_dropped_total{lane,reason}`:

| Reason | When |
|---|---|
| `shed` | A lane that may lose frames (every lane but control and sync: the channel's own items among them, which it sends again) had its queue full: its oldest frame went. |
| `stuck` | The control or sync lane had its queue full. Its peer is stuck, so the connection is closed rather than a control frame lost, and what it held goes with it. |
| `refused` | The transport wouldn't take the frame. |
| `closed` | Its connection closed, or its peer wasn't connected, with the frame queued. |
| `ended` | The net loop started over (a rebuild), or stopped, with the frame queued. |
| `chaos` | A test build's fault (`shed=`, `crate::runner::chaos`). |

The channel makes up for what of these it carried, without waiting for a reconnect:
- A receiver that sees an item past a gap drops it and asks for the stream again from the gap
  (`Want`).
- An item whose answer doesn't come within the stream's patience (15 s for session updates and
  terminal output) is sent again, and the receiver takes it once.
- A closed connection resumes as every reconnect does (`Have`).
- What each side has (`Have`) is said again every few seconds, the runner's with its samples and
  the server's every 5 s. A lost one doesn't hold a link's sends up: a sender's sends wait for the
  first, and it takes the ones after as answers only.

What a runner's QUIC layer drops it tells the server with its samples (`NetDrops`), once that
changes. The runner's page shows both sides' counts, and the server exports what each runner last
said as `kiln_runner_net_frames_dropped{runner,lane,reason}`. A frame QUIC took and lost with its
connection isn't counted here: the channel resends it after the reconnect, as it does what the
queue let go.

## Revisions

Each of the server's copies of a session counts the updates it takes: its revision (`rev`). The
numbers belong to one server process, so they're said with that process's incarnation (`born`).
Every frame a viewer gets carries the copy's revision, and an attach starts with it (`open` or
`resumed`). A viewer that reconnects says what it has (`?after=<born>.<rev>`). It then gets:
- `resumed`;
- the newest meta, state, changes and files;
- only the events changed since that revision.

It gets the whole session (`open`) only when the copy can't resume it: another incarnation, an
`Open` or `Reset` since, or a revision past what the copy has. A viewer that falls behind is caught
up the same way, from the last revision it was sent, and the live frames that catch-up already
holds are skipped.

## Runner to server: `AgentMsg`

| Message | Class | Why | Today |
|---|---|---|---|
| `Hello` | handshake | The runner says who it is and what it runs. | Answered by `Welcome` or `Refused`. |
| `Heartbeat` | telemetry | Free slots and running jobs, every few seconds. | As its class says. |
| `Sample` | telemetry | The machine's load, every `sample_seconds`. | As its class says. |
| `JobStarted` | answer | Answers an `Assign`: the leg runs here. | Not acknowledged as a frame. |
| `StepStarted` | stream | A v1 leg's steps, in order. | Not kept across a reconnect. |
| `StepFinished` | stream | A v1 leg's steps, in order. | Not kept across a reconnect. |
| `JobFinished` | stream | A leg's result: its stream's last item. | Kept until `Received` and sent again after a reconnect (`CAP_RECEIVED`). |
| `JobSample` | telemetry | A running leg's memory and CPU. | As its class says. |
| `Reject` | answer | The NACK of an `Assign`: the runner couldn't take the leg, and why. | Not acknowledged as a frame. |
| `TermOutput` | stream | A terminal's output, in order. | On the channel: a stream per terminal (`t:<terminal>#<boot>.<gen>`), kept until the server has it, sent again after a cut, its viewer kept attached through it. A stream that starts over starts with the terminal's replay, and its viewer is repainted. Without the channel: not kept across a reconnect; a viewer attaches again (`TermAttach`). |
| `TermExit` | stream | A terminal's last item. | On the channel: its stream's last item. Without it: not kept across a reconnect. |
| `TermList` | state | The terminals still running here. | Sent on every connect, not acknowledged. |
| `TermReplay` | answer | Answers `TermAttach`. | As its class says. |
| `AiSessions` | state | The AI sessions found running on this machine. | Sent when they change, not acknowledged. |
| `AiTranscript` | answer | Answers `AiTranscript`. | As its class says. |
| `Harness` | stream | A session's updates, in order, a stream per session. | On the channel: a stream per session (`s:<session>#<boot>.<gen>`), each update kept until the server has applied it, sent again after a cut from where the server has it. The session goes whole only when its stream starts: at its start, or once a server that restarted lost it. Without the channel (an older runner or server): fire-and-forget, and every connect sends every session whole. |
| `HarnessInventory` | state | The sessions this machine runs and the harnesses it offers. | Sent on every connect, not acknowledged. |
| `AiView` | answer | Answers `AiView`. | As its class says. |
| `Instance` | handshake | Which runner process of its machine this connection is. | As its class says. |
| `Build` | handshake | The build and what it can do. | As its class says. |
| `UpgradeLog` | stream | An upgrade's output. | Not kept across a reconnect. |
| `UpgradeDone` | answer | Answers `Upgrade`. | As its class says. |
| `HarnessTaskOutput` | answer | Answers `HarnessTaskOutput`. | As its class says. |
| `DirList` | answer | Answers `DirList`. | As its class says. |
| `HarnessHistory` | answer | Answers `HarnessHistory`. | As its class says. |
| `Sync` | carried (repo sync's Raft) | Repo sync: hyper-raft acknowledges and resends its own messages. | As its class says. |
| `FrontDoorStatus` | state | The machine's front door. | Sent on every connect and change, not acknowledged. |
| `JobEvent` | stream | A v2 leg's events, numbered per leg (`seq`). | Kept until `Received` and sent again after a reconnect (`CAP_RECEIVED`). |
| `Facts` | state | What the runner is: OS, CPUs, tools and so on. | Sent on connect, every 10 minutes and when asked. |
| `Reach` | carried (reach link) | A frame of a link between two runners, relayed. The link's ends frame it. | As its class says. |
| `ReachHello` | state | This runner's reach identity. | Sent on every connect. |
| `ReachAsk` | ask | A question for the server, answered by `ReachAnswer`. | As its class says. |
| `Convo` | carried (conversation feed) | The runner's conversation store, fed to the server's. | Acknowledged and resent by number (`convo/feed.rs`). |
| `UpdatePrefs` | state | Whether this machine takes upgrades. | Sent on every connect and change. |
| `Power` | state | The machine's battery. | Sent on change and every 10 minutes. |
| `Answer` | answer | Answers `Diagnose`, `Repair`, `Preview` or `Workspace`. | As its class says. |
| `HarnessFileData` | answer | Answers `HarnessFileRead`. | As its class says. |
| `StreamFrame` | media | A streamed page's picture and sound. | As its class says. |
| `StreamEvent` | stream | What a stream says: opened, navigated, closed. | Not kept across a reconnect. |
| `PortOpened` | answer | Answers `PortOpen`. | As its class says. |
| `PortData` | carried (port window) | A port's bytes, at most a window ahead of `PortAck`. | As its class says. |
| `PortAck` | ack | Opens the server's window on a port. | As its class says. |
| `PortClose` | carried (port window) | A port's end. | As its class says. |
| `Executor` | answer | Answers `Executor`. | As its class says. |
| `Undeployed` | answer | Answers `Undeploy`. | As its class says. |
| `Caps` | handshake | What the runner can do, in 64 bits (`Build`'s 32 ran out): `CAP_WIRE` says it keeps the channel. | New: sent from plan step 3. |
| `Wire` | carried (the channel) | A frame of the channel: numbered items carrying other messages, their ACKs and NACKs, wants, and what each side has after a reconnect. | New: `crate::wire::channel`. |
| `Cluster` | carried (the cluster group) | The runner's learner of the cluster group (docs/p2p.md, P2): hyper-raft's messages, which it acknowledges and resends, and asks for where the server's records are before a read. | As its class says: a read whose ask goes unanswered in 2 s goes to the server. |
| `NetDrops` | telemetry | What the runner's QUIC layer let go before it reached the wire, by lane and reason, counted from its start: each supersedes the last ("Loss below the channel"). | Sent with its samples once it changed, to a server announcing `CAP_NET_DROPS`. |

## Server to runner: `ServerMsg`

| Message | Class | Why | Today |
|---|---|---|---|
| `Welcome` | handshake | The server takes the connection. | As its class says. |
| `Refused` | handshake | The server refuses it, saying why. | As its class says. |
| `Assign` | command, refused while away | A v1 leg to run. A runner that's away gets none, and the scheduler gives it to another. | On the channel ("Commands"): answered once taken, then by `JobStarted` or `Reject`. |
| `Cancel` | command, parked | A leg to stop. A leg still running when its runner is back must stop then. | On the channel ("Commands"): answered once applied. |
| `Drain` | command, parked | Take no more jobs. | On the channel ("Commands"): answered once applied. |
| `TermOpen` | command, refused while away | A terminal to open. | On the channel ("Commands"): answered once applied; refused at once while away. |
| `TermInput` | command, refused while away | Keystrokes, never replayed later. | On the channel ("Commands"): answered once typed; refused at once while away, never typed later. |
| `TermResize` | state | A terminal's size: the newest wins. | On the channel ("Commands"), parked: the newest goes, answered once taken. |
| `TermClose` | command, parked | A terminal to close. | On the channel ("Commands"): answered once applied. |
| `TermAttach` | ask | Answered by `TermReplay`. | As its class says. |
| `AiTranscript` | ask | Answered by `AiTranscript`. | As its class says. |
| `HarnessOpen` | command, refused while away | An agent to start. One started later than asked would surprise. | Fire-and-forget: reported opened once queued. |
| `HarnessCmd` | command, as its `Command` says | Carries one `Command`. | Fire-and-forget: `Sent::Ok` once queued in the server's own process (`harness/registry.rs`). |
| `HarnessClose` | command, parked | A session closed while its machine was away must still close once it's back. | Fire-and-forget. |
| `AiView` | ask | Answered by `AiView`. | As its class says. |
| `HarnessFile` | command, parked | A piece of a file a message carries, kept with the message. | Fire-and-forget. |
| `Retire` | command, parked | An older runner process may stop. | On the channel ("Commands"): answered once applied. |
| `Upgrade` | command, refused while away | Upgrade the runner. The updater tries again on its own schedule. | On the channel ("Commands"): answered once taken, then by `UpgradeDone`. |
| `HarnessTaskOutput` | ask | Answered by `HarnessTaskOutput`. | As its class says. |
| `DirList` | ask | Answered by `DirList`. | As its class says. |
| `HarnessHistory` | ask | Answered by `HarnessHistory`. | As its class says. |
| `Sync` | carried (repo sync's Raft) | Repo sync. | As its class says. |
| `FrontDoor` | state | The front door's settings and certificate. | On the channel ("Commands"), parked: the newest goes, answered once taken. Sent on connect and change. |
| `AssignV2` | command, refused while away | A v2 leg to run, as `Assign`. | On the channel ("Commands"): answered once taken, then by `JobStarted` or `Reject`. |
| `SecretsFor` | command, parked | A running leg's secrets. A leg still running when its runner is back needs them. | Fire-and-forget. |
| `JobTerminalOpen` | command, refused while away | A terminal into a leg. | On the channel ("Commands"): answered once applied; refused at once while away. |
| `KeepRelease` | command, parked | A kept leg may be cleaned up. | On the channel ("Commands"): answered once applied. |
| `ProbeFacts` | ask | Answered by `Facts`. | As its class says. |
| `Reach` | carried (reach link) | A frame of a link from another runner. | As its class says. |
| `ReachAnswer` | answer | Answers `ReachAsk`. | As its class says. |
| `KeepExtend` | command, parked | Keep a kept leg longer. | On the channel ("Commands"): answered once applied. |
| `JobRerunStep` | command, refused while away | Run a kept leg's step again. | On the channel ("Commands"): answered once applied; refused at once while away. |
| `JobExec` | command, refused while away | Run a command in a leg's shell. | On the channel ("Commands"): answered once applied; refused at once while away. |
| `Convo` | carried (conversation feed) | The feed's answers. | As its class says. |
| `HarnessPackage` | command, refused while away | A moving session's package. A move goes only to a machine that's up. | Fire-and-forget. |
| `HarnessRelease` | command, parked | A machine that comes back must let go of a session that went on elsewhere. | Fire-and-forget. |
| `UpdateAvailable` | state | The newest kiln for this machine. | On the channel ("Commands"), parked: answered once taken. Sent once per commit. |
| `Diagnose` | ask | Answered by `Answer`. | As its class says. |
| `Repair` | ask | Answered by `Answer`. | As its class says. |
| `HarnessFileRead` | ask | Answered by `HarnessFileData`. | As its class says. |
| `StreamOpen` | command, refused while away | Stream a page. | On the channel ("Commands"): answered once applied; refused at once while away. |
| `StreamControl` | command, refused while away | What a viewer asks of a stream. | On the channel ("Commands"): answered once applied; refused at once while away. |
| `StreamInput` | command, refused while away | What a viewer did, never replayed later. | On the channel ("Commands"): answered once applied; refused at once while away. |
| `StreamClose` | command, parked | Close a stream. | On the channel ("Commands"): answered once applied. |
| `Preview` | ask | Answered by `Answer`. | As its class says. |
| `PortOpen` | ask | Answered by `PortOpened`. | As its class says. |
| `PortData` | carried (port window) | Bytes for a port. | As its class says. |
| `PortAck` | ack | Opens the runner's window on a port. | As its class says. |
| `PortClose` | carried (port window) | A port's end. | As its class says. |
| `Workspace` | ask | Answered by `Answer`. | As its class says. |
| `Executor` | ask | Answered by `Executor`. | As its class says. |
| `Received` | ack | The server has a leg's result. | As its class says. |
| `Undeploy` | command, parked | Take down a deployment. Answered by `Undeployed`. | On the channel ("Commands"): answered once taken, then by `Undeployed`. |
| `Caps` | handshake | What the server can do, in 64 bits, to a runner that said `CAP_WIRE`. | New: sent from plan step 3. |
| `Wire` | carried (the channel) | A frame of the channel, as the runner's. | New: `crate::wire::channel`. |
| `Cluster` | carried (the cluster group) | To the runner's learner: join or leave the group (said again on every connect), hyper-raft's messages, and where the server's records are. | As its class says. |

## Commands to a session: `Command`

From the UI's session socket, the API and kiln's MCP operations. A command for a session on a runner
travels in `HarnessCmd`.

| Command | Class | Why | Today |
|---|---|---|---|
| `Send` | command, parked | A message. T3's: its inbox takes each id once and parks it for a runner that's away. | T3 (`cp/t3-sends`): ids and inbox answers built, not on main. |
| `QueueEdit` | command, parked | Goes with the messages it edits (T3). | Fire-and-forget. |
| `QueueRemove` | command, parked | Goes with the messages it edits (T3). | Fire-and-forget. |
| `QueueMove` | command, parked | Goes with the messages it edits (T3). | Fire-and-forget. |
| `QueueSendNow` | command, refused while away | It interrupts the turn under way. | Fire-and-forget. |
| `QueueSteer` | command, refused while away | It steers the turn under way. | Fire-and-forget. |
| `Approve` | command, parked | Answers one approval by its id. One that no longer waits is NACKed unknown. | Fire-and-forget. |
| `Interrupt` | command, refused while away | An interrupt later would stop the wrong turn. | Fire-and-forget. |
| `ForceStop` | command, refused while away | As `Interrupt`. | Fire-and-forget. |
| `SetMode` | command, refused while away | The person sees it didn't apply, and sets it again. | Fire-and-forget. |
| `SetModel` | command, refused while away | As `SetMode`. | Fire-and-forget. |
| `SetEffort` | command, refused while away | As `SetMode`. | Fire-and-forget. |
| `MoveTo` | command, refused while away | A move starts from a machine that's up. | Fire-and-forget. |
| `Moving` | command, refused while away | A move's progress note. | Fire-and-forget. |
| `Control` | command, refused while away | A control's value, or an action. | Fire-and-forget. |
| `StopTask` | command, refused while away | Stops one task now. | Fire-and-forget. |
| `Restart` | command, refused while away | Starts the harness again now. | Fire-and-forget. |
| `Suggest` | ask | Answered by a `Reply` to its client. | As its class says. |
| `Snapshot` | ask | The whole session, for a server that lost track of it. A resume by revision replaces it. | Asked for by a server that restarted. |
| `McpEnable` | command, refused while away | Switches an MCP server now. | Fire-and-forget. |
| `McpReconnect` | command, refused while away | As `McpEnable`. | Fire-and-forget. |
| `McpRefresh` | command, refused while away | As `McpEnable`. | Fire-and-forget. |
| `McpAuth` | ask | Answered to its client with the address to open. | As its class says. |
| `McpAdd` | command, refused while away | Runs the harness's own CLI now. | Fire-and-forget. |
| `McpRemove` | command, refused while away | As `McpAdd`. | Fire-and-forget. |
| `SkillView` | ask | Answered to its client. | As its class says. |
| `SkillRun` | command, parked | Delivered like a message (T3). | T3 (`cp/t3-sends`). |
| `SkillsReload` | command, refused while away | Reads the skills again now. | Fire-and-forget. |
| `StopTasks` | command, refused while away | Stops what runs now. | Fire-and-forget. |
| `Resume` | command, refused while away | Ends a pause now. | Fire-and-forget. |
| `AutoResume` | command, refused while away | A setting the person sets again if it didn't apply. | Fire-and-forget. |
| `PauseAt` | command, refused while away | As `AutoResume`. | Fire-and-forget. |
| `MaxSubagents` | command, refused while away | As `AutoResume`. | Fire-and-forget. |
| `MoveOnLimit` | command, refused while away | As `AutoResume`. | Fire-and-forget. |
| `LimitHold` | command, refused while away | The server works the hold out again once the machine is back. | Fire-and-forget. |

## Updates of a session: `Update`

From wherever a session runs to the server, inside `Harness`, and on to its viewers.

| Update | Class | Why | Today |
|---|---|---|---|
| `Open` | stream | A session's first item, and its first again after a reset. | As `Harness` (above). |
| `Closed` | stream | Its last item. | As `Harness`. |
| `State` | state | Its state: the newest wins. | As `Harness`. |
| `Meta` | state | Its harness's settings: the newest wins. | As `Harness`. |
| `Events` | stream | Timeline events, upserted by id. | As `Harness`. |
| `Changes` | state | Its whole changeset: each replaces the last. | As `Harness`. |
| `Stream` | stream | Text appended to a streaming event. | As `Harness`. |
| `Reply` | answer | Answers one client's ask. | As its class says. |
| `Reset` | stream | The timeline starts over (a snapshot follows). | As `Harness`. |

## The UI's sockets

The session socket's commands up are `Command`s, each classed in its own table.

| Socket | Way | Message | Class | Why | Today |
|---|---|---|---|---|---|
| `/api/harness/sessions/:id/ws` | down | `open` | stream | Starts what a viewer is sent: the whole session, its revision and the server's incarnation (`rev`, `born`). | Sent on an attach without `after=`, or when the copy can't resume it. |
| `/api/harness/sessions/:id/ws` | down | `resumed` | stream | Starts a resume (`?after=<born>.<rev>`): the newest meta, state, changes and files, then only the events changed since. | Sent when the copy can resume the viewer. |
| `/api/harness/sessions/:id/ws` | down | `meta` | state | The newest wins. | Sent whole on every attach. |
| `/api/harness/sessions/:id/ws` | down | `state` | state | The newest wins. | Sent whole on every attach. |
| `/api/harness/sessions/:id/ws` | down | `state_patch` | state | What changed since the last state. | As its class says. |
| `/api/harness/sessions/:id/ws` | down | `changes` | state | The whole changeset. | Sent whole on every attach. |
| `/api/harness/sessions/:id/ws` | down | `files` | state | Files its messages mention. | Sent on attach. |
| `/api/harness/sessions/:id/ws` | down | `events` | stream | Timeline events. | Every attach, and a viewer that falls behind, gets them all again. |
| `/api/harness/sessions/:id/ws` | down | `synced` | stream | Follows a snapshot. | As its class says. |
| `/api/harness/sessions/:id/ws` | down | `closed` | stream | The session is gone. | As its class says. |
| `/api/harness/sessions/:id/ws` | down | `error` | answer | A command's refusal. | Says some refusals. Others go unanswered. |
| `/api/events` | down | `event` | stream | Kiln's events for the UI. | Not resumed after a reconnect. |
| `/api/terminal/ws` | up | `input` | command, refused while away | Keystrokes, never replayed later. | Not answered. |
| `/api/terminal/ws` | up | `resize` | state | The newest wins. | Not answered. |
| `/api/terminal/ws` | down | `output` | stream | By byte offset. | A reconnect attaches again from a replay. |
| `/api/ci/legs/:id/terminal` | up | `input` | command, refused while away | As the terminal's. | Not answered. |
| `/api/ci/legs/:id/terminal` | up | `resize` | state | As the terminal's. | Not answered. |
| `/api/ci/legs/:id/terminal` | down | `output` | stream | As the terminal's. | As the terminal's. |
| `/api/jobs/:id/logs/ws` | down | `log` | stream | A step's log, by offset. | As its class says. |
| `/api/harness/sessions/:id/streams/:sid/ws` | up | `input` | command, refused while away | What the viewer did. | Not answered. |
| `/api/harness/sessions/:id/streams/:sid/ws` | down | `frame` | media | Picture and sound. | As its class says. |
| `/api/harness/sessions/:id/streams/:sid/ws` | down | `event` | stream | What the stream says. | Not resumed. |
| `/api/deploy/apps/:app/logs/follow` | down | `log` | stream | A deployment's logs, followed. | As its class says. |
| `/api/tunnel` | up | `datagram` | carried (tunnel) | The tunnel's own protocol. | As its class says. |
| `/api/tunnel` | down | `datagram` | carried (tunnel) | The tunnel's own protocol. | As its class says. |
