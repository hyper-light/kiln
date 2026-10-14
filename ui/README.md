# kiln UI

The web console for kiln. It is built with Vite, React 19, TypeScript, Tailwind v4, Radix, xterm.js and uPlot, and styled after hyperlight-site. `npm run build` writes `dist/`, which the Rust server embeds with rust-embed. The server serves it at `/` and falls back to `index.html` for unknown paths.

```sh
npm install
npm run mock        # mock API on :7070 (prints admin token + one-time login URL)
npm run dev         # Vite on :5173, proxies /api (incl. WebSockets), /metrics, /healthz → :7070
npm run build       # tsc -b && vite build → dist/
npm run typecheck
npm run lint
```

Set `KILN_URL=http://host:port` to point the dev proxy at a real server.

## Mock server

`mock/server.ts` implements the whole `docs/api.md` contract. It simulates the hyper-light repos, about three days of run history, live runs that move through a job DAG, streamed ANSI step logs, agents (bare-metal plus ephemeral k8s pods), 5 s host samples and the `/api/events` stream. The terminal is a real PTY through `node-pty`; if that module is unavailable, it falls back to an echo shell.

- Admin token: `kiln-dev-token` (override with `KILN_TOKEN`).
- To turn auth off, set `MOCK_NO_AUTH=1`.
- If `dist/` exists, the mock serves it with SPA fallback, the same way the Rust binary does.

## Layout

```
src/api/         types.ts (contract types), client.ts (fetch + 401 → login), events.ts (shared reconnecting WS)
src/components/  ui/ (button, tabs, dialog, toast, status, misc), shell/ (header, palette, motion), charts/ (uPlot, lazy), terminal/ (xterm setup, lazy)
src/views/       overview, runs, run-detail/ (DAG, log viewer), repos, agents, terminal/, metrics (route /telemetry), system, settings, login
mock/            mock API server + fake data
```

## Notes

- The UI's metrics page lives at **`/telemetry`**, because `/metrics` is the server's Prometheus endpoint.
- PTY sessions use the WebGL renderer and fall back to the DOM renderer. Log views always use the DOM renderer, because browsers cap the number of live WebGL contexts. Setting localStorage `kiln.term.renderer` to `"dom"` forces the DOM renderer everywhere.
- While a terminal has focus, every key goes to the PTY. The Ctrl/Cmd+K palette only opens when no terminal is focused.
