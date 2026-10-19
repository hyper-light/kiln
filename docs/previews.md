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

- **A real browser on the agent's machine.** Each stream is a tab of a browser of its profile's
  (see "Profiles" below): by default the conversation's, whose cookies and storage its streams
  share, and nobody else's. kiln's capture extension (`crates/kiln/src/browser/extension`) captures the tab as its
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
- **Its fields have the viewer's own.** The page says which of its fields has the focus and where
  its fields are. A field that takes text gets one of the viewer's in the address's place, made
  like it: a password's hides what's typed, the keyboard is the one it wants (email, a number
  pad), and its autocomplete is the page field's, so the viewer's password manager fills a
  username, a password or a one-time code. A tap on a field brings the keyboard up at once, in the
  tap itself (a phone only allows it then). Enter, Tab and the arrows go to the page; nothing typed
  stays in the viewer's field. While a phone's keyboard is up, the page's window is what's left
  above it, and the page keeps its field in sight.
- **Copy:** what's selected in the page comes to the viewer's clipboard (Copy, or the shortcut),
  never a password field's text.
- **Shared with the agent.** `use_live` clicks, taps, types, presses keys, scrolls and goes places
  in the same page, and gets a picture back. The person sees it happen, and where the agent
  touched.
- **Where it is:** the frame shows the page's address. Type another, or go back, forward, reload.
- **Out of view** the frame lets go of the stream (it pauses) and keeps its last picture. A stream
  that ended can be started again from the frame.

### Opening a page

A person opens one too, from the agent's Tools tab (Live pages): pick what to open and a profile,
and it streams there, live, with Close to end it.

- **What its machine serves.** The ports its machine listens on, read from the system
  (`/proc/net/tcp` on Linux, `lsof` on a Mac, `netstat` on Windows), with what serves each
  (`node vite`, `python3 http.server`). The agent's own servers come first, and so does anything
  its processes started. The system's services (ports under 1024) aren't listed, nor (on Linux and
  Macs) other users' servers.
- **The project's app.** A project says how its app starts in `.kiln/preview.yaml`:

  ```yaml
  start: npm run dev     # how it starts, in the session's folder
  port: 5173             # where it listens
  path: /                # the page to open (/ by default)
  env: { BROWSER: none } # what it's started with besides
  ready_s: 60            # how long it may take to listen (60 s by default, at most 600)
  ```

  "Start the app" starts it on the agent's machine and opens it once it listens (on `127.0.0.1`
  or `::1`). Its output goes to `browser/apps/<session>.log` in the runner's state folder. It runs
  until it's stopped (Stop the app) or its conversation closes.
- **Another address**, typed: `localhost:3000` is `http://localhost:3000`.

#### Profiles

| Profile | What it keeps | Who uses it |
| --- | --- | --- |
| This conversation's (the default) | Cookies and storage, between the conversation's streams (the agent's too). Removed when it closes. | Everyone who may use agents here (`agents.run`). |
| Clean | Nothing. Each stream is a browser of its own, stopped and removed as the stream closes. | Everyone who may use agents here. |
| Your own | Cookies and storage, across conversations: a site you sign in to stays signed in. | You alone: it isn't listed to anyone else, and they can't watch or close it. |

An agent uses (`use_live`) its own conversation's streams, never one in a person's own profile.
Opening and closing a stream, and starting and stopping an app, are in the audit log
(`agent.stream.open`, `agent.stream.close`, `agent.app.start`, `agent.app.stop`).
Your own profile is kept on the agent's machine, as files of the user the runner runs as: agents
running on that machine can read them. It's for a person signed in as themselves (the owner or a
user), never a job, an agent or a token with a narrower grant.

### On your device

A port of the agent's machine also opens on the person's own device, in that device's own browser:
"Open on this device" (or "Copy a link", for another device) in the Tools tab's Live pages.

- **At pages' own site only.** It needs `[server] pages_url` (see "Its own address" above). The
  app is served at that site's root, so a dev server's absolute paths work as they do on its own
  address. kiln never serves an app from kiln's own address, where it would be one of kiln's pages
  with the person's kiln session.
- **A link, then a cookie.** kiln makes a link with a code good once, for ten minutes. Followed,
  the code becomes that site's cookie for the opening (`kiln_open`, HttpOnly, never sent on to the
  app), and the device goes on to the app's page. One opening at a time per device: the latest
  link followed is the one it shows.
- **As the app's own.** Each request goes over a connection through the agent's runner to the app's
  port, on loopback (`127.0.0.1`, else `::1`), and only to a port the session's machine lists. Its
  `Host`, `Origin` and `Referer` are the app's own `localhost` address, so a dev server's host and
  origin checks hold; `X-Forwarded-Host` and `X-Forwarded-Proto` say where it was asked. Bodies
  stream both ways under flow control (at most 4 MiB a connection's runner sends ahead), and an
  upgrade (a dev server's hot reload) goes through: asked of the app first, then the two
  connections are piped. Its redirects to its own address stay at pages' site, and its cookies
  are that site's alone. A service worker isn't let in: it would outlive the opening, on a site
  every page and opening shares.
- **Ending.** After twelve hours, with End in the Tools tab, or when its conversation closes. A
  device then sees that it ended. Opening and ending are in the audit log (`agent.opened.create`,
  `agent.opened.end`). An opening is its person's: nobody else lists or ends it.

Connections run on the runner link's own lane (`LANE_PORTS`, runners with `CAP_PORTS`):
`ServerMsg::PortOpen`, `PortData`, `PortAck` and `PortClose` down, `AgentMsg::PortOpened`,
`PortData`, `PortAck` and `PortClose` up (`crate::browser::ports`, `crate::harness::opened`).

### API

- `POST /api/harness/sessions/:id/streams` with `{url, width, height, scale, mobile, codec,
  bitrate, profile}`: a stream of a page, on the session's machine. `profile` is `conversation`
  (the default), `clean` or `personal`; width and height are a phone's (390 by 640) until a
  viewer says its own. With `{"start": true}` instead of a url, the project's app starts first
  (its `.kiln/preview.yaml`) and its page opens.
- `GET /api/harness/sessions/:id/preview`: what a person may open from the session: `{ports:
  [{port, address, pid, command, agents}], app: {configured, start, port, path, running, error}}`
  (`running`: the app's address while it runs). `POST /api/harness/sessions/:id/preview/stop`
  stops the app. Runners answer these with `CAP_PREVIEW`.
- `POST /api/harness/sessions/:id/opened` with `{port, path}`: the app on that port opened on the
  person's own device: `{id, port, path, at, until, url}`, `url` its link (good once, for ten
  minutes). `GET` lists the person's openings of the session (without their links), and
  `DELETE /api/harness/sessions/:id/opened/:opened` ends one.
- `GET /api/harness/sessions/:id/streams`: the session's streams (not another person's own).
- `GET /api/harness/sessions/:id/streams/:stream/ws`: a viewer's socket. It's sent binary chunks
  (`crate::browser::stream::frame`: kind `v`, `a` or `c`, flags, timestamp in µs, part, parts, the
  bytes) and JSON events (`opened`, `navigated`, `loaded`, `focus`, `fields`, `selected`,
  `copied`, `dialog`, `agent`, `closed`, `error`). It sends `{"input": [...]}` (what the person
  does) and `{"control": {...}}` (`resize`, `pause`, `keyframe`, `go`, `back`, `forward`,
  `reload`, `copy`).
- `DELETE /api/harness/sessions/:id/streams/:stream`: close it (its tab goes).

An agent's MCP server opens one through its runner, for its own session only (`{"op": "stream",
"do": "open", "url", "title"}`, `crate::harness::streams::ask`). `use_live` acts in its own
session's streams where they run (`crate::browser::stream::act`), never one in a person's own
profile.
