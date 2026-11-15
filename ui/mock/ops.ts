/**
 * kiln's operations in the mock (`GET /api/ops`, docs/ci.md W20-R01), as the server lists them: the
 * ones of the agents, runners, sandboxes, secrets and confirmations the UI's surfaces do, each with its
 * name, summary and flags as the server's registry has them (crates/kiln/src: agentdefs/ops.rs,
 * doctor.rs, secrets/mine.rs, harness/overrides.rs, confirms.rs). The real list has every operation;
 * the command palette makes entries only of those it knows where to do.
 */
import type http from "node:http";

const OPS = [
  {
    name: "agentdefs.list",
    summary:
      "Agent definitions (an agent as data: one of kiln's ten harnesses, its model, instructions, tools and MCP servers, skills, limits and Agentfile): yours, newest change first, and beside them, read only, kiln's own (`builtin`: one per CI skill, one per harness, and the agents kiln starts from its UI, such as the about card; agentdefs.copy makes one yours). `all`: everyone's, for the server's owner and those with agentdefs.manage.",
    read_only: true,
    destructive: false,
    input: { type: "object" },
  },
  { name: "agentdefs.get", summary: "One of your agent definitions, with its settings and version.", read_only: true, destructive: false, input: { type: "object" } },
  {
    name: "agentdefs.create",
    summary:
      "Keep an agent definition of yours: its `name` (lowercase letters and digits joined by `.`, `_` or `-`; once among yours), `harness` (claude, codex, devin, gemini, opencode, goose, amp, antigravity, cursor or pi), `model`, `instructions`, `tools`, `mcp` servers, `skills`, `limits` and `agentfile` (one of yours, `agentfile:<id>`, or kiln's for its harness, `harness-<harness>`; left out: its harness's own). Checked first as agentdefs.check checks it: one kiln can't run isn't kept, and its words say why. Answers it, version 1.",
    read_only: false,
    destructive: false,
    input: { type: "object" },
  },
  {
    name: "agentdefs.update",
    summary:
      "Change one of your agent definitions: what's given changes (null `model` or `agentfile`: back to its harness's). New settings, checked as agentdefs.create checks them, are a new version; a new `name` alone is none.",
    read_only: false,
    destructive: false,
    input: { type: "object" },
  },
  { name: "agentdefs.versions", summary: "Every version of one of your agent definitions' settings, newest first: when, and by whom.", read_only: true, destructive: false, input: { type: "object" } },
  {
    name: "agentdefs.restore",
    summary: "Make an earlier version of one of your agent definitions current again, as a new version (nothing is lost), checked again.",
    read_only: false,
    destructive: false,
    input: { type: "object" },
  },
  { name: "agentdefs.delete", summary: "Delete one of your agent definitions and its versions.", read_only: false, destructive: true, input: { type: "object" } },
  {
    name: "agentdefs.copy",
    summary:
      "Make one of kiln's own agent definitions (`kiln:<name>`, as agentdefs.list's `builtin` names them: one per CI skill, one per harness, and kiln's own agents such as the about card), or one of yours, one of yours: version 1, named `name` (by default as it is), checked as agentdefs.create checks it. Kiln's own are read only: copy one to change it.",
    read_only: false,
    destructive: false,
    input: { type: "object" },
  },
  {
    name: "agentdefs.use",
    summary:
      "Have one of your own agent definitions (`id`) stand in for one of kiln's UI agents (`builtin`: one agentdefs.list marks `replaceable`, such as kiln:kiln-about, the card that answers \"ask about this code\") where kiln starts it for you: its harness, model and Agentfile, first told its instructions and then kiln's brief. Only your own use changes. Null `id`: kiln's own again. Answers what stands in for what (`uses`), as agentdefs.list does.",
    read_only: false,
    destructive: false,
    input: { type: "object" },
  },
  {
    name: "agentdefs.check",
    summary:
      "Check an agent definition's settings as agentdefs.create would, keeping nothing: `ok`, and each of the `problems` that would refuse it (a harness that isn't one of kiln's ten, an Agentfile you don't reach or that's for another harness, a model kiln's machines don't list for its harness, an MCP server without a name or a way in, a limit of 0).",
    read_only: false,
    destructive: false,
    input: { type: "object" },
  },
  {
    name: "agentdefs.run",
    summary:
      "Run one of your agent definitions: an agent started as agents.start starts one, with its harness and model, in a microVM of its own made from its Agentfile (its harness's own where it names none), first told its instructions and then your `prompt`; `on` and `cwd` as agents.start takes them, `name` its name (by default the definition's). Answers as agents.start does, with the definition's id and version (`agentdef`).",
    read_only: false,
    destructive: false,
    input: { type: "object" },
  },
  {
    name: "runners.engine",
    summary:
      "How a runner runs new agent sessions, its engine: `host` (each a process on the machine, no microVM) or `microvm` (each in a Shards microVM of its own, through Hex); where that comes from (its owner set it, or what the runner can do: `microvm` where it can hold microVMs, else `host`) and why; whether it can hold microVMs now and why not; its isolation facts; and how many of its sessions run on its engine and how many on the other, which finish there (`draining`).",
    read_only: true,
    destructive: false,
    input: { type: "object" },
  },
  {
    name: "runners.engine.set",
    summary:
      "Set how a runner runs new agent sessions: `host` or `microvm`, or null for what it can do (`microvm` where it can hold microVMs, else `host`). `microvm` is refused, saying why, where the runner can't hold microVMs now. Nothing running stops: its sessions on the other engine finish there, and new sessions take the new one.",
    read_only: false,
    destructive: false,
    input: { type: "object" },
  },
  {
    name: "secrets.mine.list",
    summary:
      "Your own secrets (the `user` scope): each one's name, size, and when it was set and last used, never its value. A build of one of your Agentfiles is given those it names as `LABEL kiln.secret.<ID>=user:<NAME>`; no one else sees them, the server's owner included.",
    read_only: true,
    destructive: false,
    input: { type: "object" },
  },
  {
    name: "secrets.mine.set",
    summary: "Set one of your own secrets: its `name` (letters, digits and `_`, not starting with a digit) and `value`, kept sealed and never shown again. A person sets their own; an agent can't.",
    read_only: false,
    destructive: true,
    input: { type: "object" },
  },
  {
    name: "secrets.mine.delete",
    summary: "Delete one of your own secrets by its `name`. A build of an Agentfile naming it is refused from then on.",
    read_only: false,
    destructive: true,
    input: { type: "object" },
  },
  {
    name: "sessions.ports.publish",
    summary:
      "Publish a port into a running microVM session, in place (no rebuild, no new microVM): the host's address (`bind`: who may reach it, 127.0.0.1 this machine alone) and `host_port` to its `member`'s `guest_port` (`harness`, the default, `agent:<name>` or `mcp:<name>`), `protocol` tcp or udp. A microVM session's alone: a session on its machine's host refuses it, naming its engine. Kept on the session and given again whenever its harness starts again; agents_get shows it (state.overrides).",
    read_only: false,
    destructive: false,
    input: { type: "object" },
  },
  {
    name: "sessions.ports.unpublish",
    summary: "Unpublish a port a running microVM session publishes: its `bind`, `host_port` and `protocol`.",
    read_only: false,
    destructive: true,
    input: { type: "object" },
  },
  {
    name: "sessions.egress.grant",
    summary:
      "Grant a running microVM session's `member` (`harness`, the default, `agent:<name>` or `mcp:<name>`) egress to `host` (a DNS name or an address, an MCP server's among them) on `port`, in place: nothing else is reached (default deny). A microVM session's alone: a session on its machine's host refuses it, naming its engine.",
    read_only: false,
    destructive: false,
    input: { type: "object" },
  },
  {
    name: "sessions.egress.revoke",
    summary: "Revoke egress a running microVM session's `member` was granted: its `host` and `port`; its next connection there is refused.",
    read_only: false,
    destructive: true,
    input: { type: "object" },
  },
  {
    name: "sessions.networks.connect",
    summary:
      "Connect a running microVM session's `member` (`harness`, the default, `agent:<name>` or `mcp:<name>`) to the named `network` it shares with the runs connected to it, in place. A microVM session's alone: a session on its machine's host refuses it, naming its engine.",
    read_only: false,
    destructive: false,
    input: { type: "object" },
  },
  { name: "sessions.networks.disconnect", summary: "Disconnect a running microVM session's `member` from the named `network`.", read_only: false, destructive: true, input: { type: "object" } },
  {
    name: "sessions.volumes.add",
    summary:
      "Give a running session a folder of its machine (`path`), `access` ro or rw, in place: it joins its harness's own allowed folders (each harness's way) and the session's scope (kiln holds its file calls there to `access`), and in a microVM session it's plugged into the microVM too, at `at` (else where kiln plugs that folder in) for `members` (else its harness). Every engine. Kept on the session and given again whenever its harness starts again; agents_get shows how each took it (state.overrides).",
    read_only: false,
    destructive: false,
    input: { type: "object" },
  },
  {
    name: "sessions.volumes.remove",
    summary: "Take a folder (`path`) back from a running session: out of its harness's allowed folders, its scope and, in a microVM, unplugged.",
    read_only: false,
    destructive: true,
    input: { type: "object" },
  },
  {
    name: "confirms.list",
    summary:
      "What your agents asked to do that waits on your click (deleting a definition, ending or changing one of your agents they didn't start, anything kiln marks destructive), newest first; with `agent`, that agent's. A person's: an agent can't read them.",
    read_only: true,
    destructive: false,
    input: { type: "object" },
  },
  {
    name: "confirms.answer",
    summary:
      "Your click on what an agent asked (`id`, as confirms.list gives it): `yes` confirms it, and the agent's same call (same arguments, its target as it was) then runs once, as yours; else it's declined. Its agent is told either way. A person's: an agent's call is refused.",
    read_only: false,
    destructive: false,
    input: { type: "object" },
  },
];

export function handleOps(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: { json: (res: http.ServerResponse, status: number, body: unknown) => void }): boolean {
  if (url.pathname !== "/api/ops" || (req.method ?? "GET") !== "GET") return false;
  ctx.json(res, 200, { operations: OPS });
  return true;
}
