# kiln UI

The web console for kiln. It is built with Vite, React 19, TypeScript, Tailwind v4, Radix, xterm.js and uPlot, and styled after hyperlight-site. `npm run build` writes `dist/`, which the Rust server embeds with rust-embed. The server serves it at `/` and answers a page that renders only in the browser with the app's shell (`_shell.html`).

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
- `PORT` (and `HOST`) move it off :7070, e.g. when a real kiln runs there; point the dev server at it with `KILN_URL=http://127.0.0.1:<port> npm run dev`.
- If `dist/` exists, the mock serves it with SPA fallback, the same way the Rust binary does.
- Notifications (`mock/push.ts`): `/api/push` keeps devices and mutes as the server does. It can't deliver a real push: what it would send is logged and kept at `GET /api/mock/push/outbox`. `MOCK_PUSH_FLEET=1` adds agents standing every way Mission control shows (two need you, one stopped, one paused, some working), for a list worth sorting on a phone.
- Runners → Add machines: discovery hears runners, machines not set up yet, another server and its runner; machines ask to join (with codes), scans and probes find more, and an invited machine asks a few seconds later. `MOCK_SERVER_OS=linux|windows` changes the server's own DNS-SD (default `macos`, Bonjour), `MOCK_NETWORK=empty` leaves nothing else on the network, and a machine runs the one-liner `MOCK_AUTOJOIN_SECONDS` (30) after the page first asks for it (`MOCK_AUTOJOIN=0`: never).

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

## Bytes on a slow link

What each page costs on the wire is held to a budget on every build (goal 8, plan `01a1a832ebfd`;
the arithmetic and the lab's numbers are in `docs/browser-lab/ui-loads.md`):

- `node ci/ui/page_bytes.mjs --dist ui/dist` prints each of the lab's pages (sign-in, overview,
  Mission control, a conversation, `/runs` (Ship), a run) as the shell plus its route, in Brotli
  as kiln sends it, with requests and critical waves. `--check` holds them to
  `ci/ui/page_budgets.json` and names any page over; `--ratchet` moves the budgets down to the
  build (never up); `--since <dist>` says what a visit after a deploy from that build downloads
  again. `ci/ui/two_builds.sh` builds twice, the second with a one-line change in the
  conversation, and fails when more than 2 hashed files take new names. Both run in the ui job.
- `KILN_UI_SOURCEMAP=1 npx vite build`, then `node ci/ui/attribute.mjs --page conversation`, says
  which modules a page's code is made of.
- The build names chunks by their own content through an import map (`scripts/stable-names.mjs`)
  and lists the pages the server renders (`scripts/server-pages.mjs`); kiln's own build packs the
  files with Brotli (`crates/kiln/build.rs`), and docs/http.md "The UI's files" says how kiln
  serves them.
- After a page is ready, not before: the editor, xterm (Ship's log boxes load it as a log shows),
  uPlot and tree-sitter come only where a page shows them, and the conversation's Plan and Tools
  tabs load when one is pointed at or opened. Code shows plain until its grammar is here, and the
  grammars start once the page's load event has fired. The update checks revalidate the shell
  (a 304 while the build is the same) and take its import map as the build.
- The splash is in every page's HTML (`components/shell/splash.tsx`, rendered by the document),
  so a slow link shows it before the code arrives.
