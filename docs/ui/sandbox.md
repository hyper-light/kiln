# An agent's Sandbox tab (Host on the host)

Kiln runs agents in Shards microVMs through Hex (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P2, P3). This tab is
where a session's sandbox is seen and changed, beside its conversation. Rules: [principles.md](principles.md).

## Where

- The agent page's inspector, after Changes, Plan and Tools; on a phone (and below 1100px) the tab strip, which
  scrolls sideways. `?tab=sandbox` opens it.
- Named **Sandbox** for a session in a microVM, **Host** for one on the host, so the tab says where it runs before it
  opens (`inspector.tsx` SandboxTabName).
- Code: `ui/src/views/harness/sandbox.tsx`, `sandbox.css`; marks shared with Mission and Runners in
  `ui/src/views/sandbox-marks.tsx`; data `ui/src/api/microvms.ts` (`sandboxRes`, `sandboxApi`).

## Top to bottom

1. **Now**: the engine glyph, runner, state dot and uptime; CPU, memory and disk as live bars (LimitMeter's `live`
   variant: kiln's prism breathing over the bar, its width eased as readings move every 2 s). On the host, one line:
   "Runs on the host, without a microVM".
2. **Controls**: Pause/Resume (a vCPU pause, microVM only, Undo in its toast), Restart, "Restart on vN" when its
   definition moved on, Stop asked in place.
3. **MicroVM** (microVM only): Hex box (id, node, state), image and its Agentfile version and digest, size, footprint
   (peak and steady), start (kind glyph and ms, its warm pool and that pool's warm hosts), secret names it was built
   with and their scope. **Host** (host only): runner, harness process (pid, user), why it's on the host.
4. **Components**: harness, agents and MCP servers at their paths, each with source glyph, pin and version. Swap
   (source and pin, inline; refused in Shards' terms) and Restore. The harness row's Switch presses the head's own
   model chip: kiln's one-click harness switch, unmoved. One line says what a swap does on this engine.
5. **Network** (microVM only): ingress (runner address:port → guest port/proto, to whom; Publish, unpublish ×),
   egress grants (host:ports, to whom, dns and mcp tags; Grant, revoke ×), then "nothing else", or "nothing granted";
   networks with each member (harness, agent, MCP) a connect/disconnect chip.
6. **Volumes** (both): path, ro/rw, members; Add volume inline (path, ro|rw, members); remove asked in place. One line
   says what adding does: on the host it adds to allowed directories and scope; in a microVM it mounts there first.
7. **Directories** (both): each member's allowed directories, one path a line; the scope (project, branch, paths).
8. **Last hour**: CPU, memory and network, full-width plots.

## What the server answers (the mock's routes, `ui/mock/microvms.ts`)

- `GET /api/microvm/sessions`: every session's sandbox in short (engine, runner, state, start, CPU, memory).
- `GET /api/microvm/sessions/:id`: the whole tab's data; host sessions carry `process` and no `network`, `hex`, `vm`,
  `pool` or `secrets`.
- `POST /api/microvm/sessions/:id/{pause|resume|restart|stop}`; `POST|DELETE …/ingress`, `…/egress`, `…/volumes`;
  `PUT …/networks/:name {member, connected}`; `POST|DELETE …/components/:name {source, pin}`. Network and pause on a
  host session are refused (409).

## Tests

`ui/e2e/agent-sandbox-tabs.spec.ts` (desktop and phone): a microVM session's tab top to bottom, a host session's Host
tab with nothing only a microVM has, and the Agents tab.
