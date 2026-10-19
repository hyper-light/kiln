# Previews

What an agent makes shows in its conversation, live: files it mentions (docs/agents.md, "Files in
messages"), and pages it shows. Nothing is a picture standing in for the real thing. A page runs
in the viewer's own browser, whichever browser and system that is.

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
