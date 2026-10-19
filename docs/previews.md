# Previews

What an agent makes shows in its conversation, live: files it mentions (docs/agents.md, "Files in
messages"), pages it shows, and pages it streams from its machine. Nothing is a picture standing
in for the real thing. A page it wrote runs in the viewer's own browser, whichever browser and
system that is; one that needs the agent's machine runs there and streams.

## Pages

An agent shows a page with kiln's `show_page` tool: a chart, a table, a diagram, a mock-up, a
small tool to try. It writes a whole HTML document (or names an HTML file). The page shows inline
in the conversation at the conversation's width, as tall as it is, and works as written: scripts,
animation, sound, forms.

- **Kept, with what it uses.** The server keeps the page and every file it uses: pictures,
  styles, scripts and modules, fonts, data and models. That's what the page names (its
  attributes, its styles' `url(...)` and `@import`s, its modules' imports, full paths and web
  addresses quoted in its scripts), what those files name in turn, and whatever kiln's browser saw
  it load, dynamic imports and fetches included. Files of the agent's machine are read like files
  in messages (`CAP_FILES`). Files from the web are fetched once by the server and kept, so a
  viewer's browser never goes there; pin their versions. The server fetches only from the public
  internet, never its own machine, its networks or a cloud's metadata service. Up to 2 MB for the
  page, 32 MB a file, 128 MB in all, 200 files.
- **Served where its files were.** A page is served at `/v/<id>` followed by its own path on its
  machine (`/v/<id>/home/ada/report/index.html`), each of its files beside it at theirs, and what
  it loaded from the web under `/v/<id>/_web/<scheme>/<host>/<path>`. So whatever the page and its
  files say relative to themselves (`./lib.js`, `url(../fonts/a.woff2)`, a module's own imports)
  leads where it led on the machine. Only full paths and web addresses are rewritten, to where
  their copies are served. It shows the same for good, after the files change or go. Each file is
  served as what its name says it is (`text/css`, `text/javascript`, `font/woff2`, ...).
- **Measured where it was made.** Before the tool answers, kiln's browser on the agent's machine
  loads the page as the conversation will show it, from an origin only that browser knows
  (`https://kiln.invalid` followed by the page's path), answered from the machine's own files
  (never what looks like a secret). There, modules, `fetch` and relative paths work as they will
  where it's shown. It measures the page's height at each width a viewer's conversation might
  have (320 to 1144 px, laid out as a frame lays it out, whatever the page tells a phone), so the
  frame takes its height before the page loads and nothing moves. The agent gets a picture of it,
  what its console printed, its layout shift, long tasks and frame rate.
- **In kiln's look.** The page gets kiln's theme first in its head: dark, a transparent
  background, Geist and Geist Mono, and kiln's colors as CSS variables (`--foreground`,
  `--muted-foreground`, `--border`, `--card`, `--accent`, `--success`, `--warning`,
  `--destructive`, `--chart-1` to `--chart-5`, `--font-sans`, `--font-mono`, `--radius`). Its own
  styles come after and win.
- **Sandboxed.** The id in a page's address is 128 random bits, and the address is the page's only
  key. The answer's Content-Security-Policy sandboxes it: scripts and forms run in an
  opaque origin, with no kiln cookies, no storage and no access to the page that frames it. It loads
  only its own files, kiln's fonts, and what it makes itself (`data:`, `blob:`). Only kiln may
  frame it, and its address is never sent on as a referrer. The conversation's frame is sandboxed
  the same way.
- **Its own address (optional).** With `[server] pages_url` set (`https://kiln-pages.example`), pages
  are served only there, and kiln's own address sends `/v/` there. Use a site of its own (another
  registrable domain, not a subdomain of kiln's) so browsers keep pages in processes apart from
  kiln's.
- **Its life in the conversation.** While a page is out of view, what moves in it pauses:
  animations, `requestAnimationFrame`, video, and Web Audio. Far out of view the frame is let go;
  what was done in it comes back when it's shown again.

### The bridge

kiln puts a small script first in every page. It speaks MCP Apps' messages
(`ui/initialize`, `ui/notifications/initialized`, `ui/notifications/size-changed`,
`ui/open-link`, `ui/message`), so a page written for an MCP Apps host works in kiln. It also gives
the page `window.kiln`:

- `kiln.reply(value, label)` says the person's choice back to the agent: it goes into the
  composer, for the person to send (the label, and the value when it's more than the label).
- `kiln.save(state)` keeps what the person did, up to 64 KB, in their browser. When the page is
  shown again, `kiln.onrestore(state)` is called with it.

Links to the web open in a new tab.

### The tools

- `preview_page`: look at a page in kiln's browser on the agent's machine, as a person would: HTML,
  an HTML file, or an address (a dev server there, or the web). The answer has a picture and the
  page's console, height, layout shift, long tasks and frame rate. A phone by default (390×844, 2×,
  dark). `measure: true` also gives the height at each width.
- `show_page`: show a page in the conversation. The answer has the page's id and heights, and what
  kiln's browser saw of it.

The browser is Chrome for Testing at a version kiln pins (its download checked against a SHA-256
kiln knows). It's downloaded the first time a machine needs it, about 200 MB, into kiln's state
folder there. It runs headless, shared by the machine's agents, each page in a context of its own,
and stops after ten minutes unused. It never uses a browser the person installed, nor their
profile. On Linux and macOS kiln speaks to it over a pipe only kiln holds (`--remote-debugging-pipe`):
no port on the machine reaches it, and it ends when the runner does, however the runner ends.
Where the system won't let it sandbox itself (some containers), set `KILN_BROWSER_SANDBOX=0` in
the runner's environment.

### API

- `GET /v/<id>/`: leads to the page (open: its address is its key).
- `GET /v/<id>/<path>`: the page, or a file it uses, at its path (takes `Range`).
- `GET /v/_/fonts/geist.woff2`, `/v/_/fonts/geist-mono.woff2`: kiln's fonts, for pages.

An agent's MCP server publishes through its runner, for its own session only (`crate::harness::visuals::ask`):
`{"op": "page", "do": "publish", "title", "entry", "html", "loaded": {"local", "web"}}` (`entry`:
where the page stands on its machine; `loaded`: what kiln's browser there saw it load), then
`{"op": "page", "do": "measured", "id", "heights"}`. Pages are kept in `<data>/harness/visuals/`,
their content beside the files conversations keep, and they go when their conversation closes.

## Streams

When a page needs the agent's machine (its dev server at `http://localhost:5173`, its files, its
network, its GPU), kiln runs it there and streams it. An agent shows one with `show_live`, and the
page appears in the conversation, live.

- **A real browser on the agent's machine.** Each stream is a tab of a browser of its own profile.
  A conversation's streams share its cookies and storage, kept between them, and nobody else's.
  kiln's capture extension (`crates/kiln/src/browser/extension`) captures the tab as its
  compositor draws it, only when something changed, so a still page costs nothing. It encodes it
  right there with WebCodecs: H.264 (VP8, VP9 or AV1 when asked) at the viewer's own window size and
  pixel density, and the page's sound as Opus. Once the page is still, its last picture is sent
  again, sharp.
- **Relayed by the server.** Chunks go up the runner's stream lane (`LANE_STREAM`, runners with
  `CAP_STREAM`) and out to each viewer's socket. A viewer that joins gets the decoders'
  configuration and a fresh key frame at once. One too slow for a chunk loses it, and the next key
  frame mends it. With nobody looking, the stream pauses: nothing is captured, encoded or sent.
  After ten minutes unwatched it closes.
- **Shown in the viewer's browser** with WebCodecs (Chrome, Edge, Firefox and Safari have it). The
  frame is the page's window: the conversation's width, as tall as the conversation shows. The
  page lays out at that size on the agent's machine, as a phone lays it out for a phone. Its
  sound plays once the person turns it on.
- **What the person does goes to it:** taps and the mouse (as touch from a phone), the wheel, keys
  by meaning (a Mac's Command is Control on a page elsewhere, for its shortcuts), text from a
  phone's keyboard (Type), compositions, a paste. Measured on one machine, a tap shows in the
  picture 12 to 30 ms later (17 ms at the median).
- **Shared with the agent.** `use_live` clicks, taps, types, presses keys, scrolls and goes places
  in the same page, and gets a picture back. The person sees it happen, and where the agent
  touched.
- **Where it is:** the frame shows the page's address. Type another, or go back, forward, reload.
- **Out of view** the frame lets go of the stream (it pauses) and keeps its last picture. A stream
  that ended can be started again from the frame.

### API

- `POST /api/harness/sessions/:id/streams` with `{url, width, height, scale, mobile, codec,
  bitrate}`: a stream of a page, on the session's machine.
- `GET /api/harness/sessions/:id/streams`: the session's streams.
- `GET /api/harness/sessions/:id/streams/:stream/ws`: a viewer's socket. It's sent binary chunks
  (`crate::browser::stream::frame`: kind `v`, `a` or `c`, flags, timestamp in µs, part, parts, the
  bytes) and JSON events (`opened`, `navigated`, `loaded`, `focus`, `dialog`, `agent`, `closed`,
  `error`). It sends `{"input": [...]}` (what the person does) and `{"control": {...}}`
  (`resize`, `pause`, `keyframe`, `go`, `back`, `forward`, `reload`).
- `DELETE /api/harness/sessions/:id/streams/:stream`: close it (its tab goes).

An agent's MCP server opens one through its runner, for its own session only (`{"op": "stream",
"do": "open", "url", "title"}`, `crate::harness::streams::ask`). `use_live` acts in the stream
where it runs (`crate::browser::stream::act`).
