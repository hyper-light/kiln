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
- **And in other engines.** Where the agent's machine has Firefox, the page is measured in it too,
  the same way. In WebKit, viewers' own browsers measure it: a frame in Safari tells kiln the
  height its page said at its width, once it's settled, and kiln keeps it with the page, so the
  next viewer in Safari has the frame at the page's height before it loads. A frame takes its own
  engine's height where one was measured near its width (within 24 px), or two around it (within
  160 px each way), and Chromium's otherwise.
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
  kiln's browser saw of it. The conversation draws the page whichever harness called it (and
  `show_live`'s stream the same way): it knows the call by the harness's name for it (Claude
  Code's and Codex's `mcp__kiln__show_page`, Devin's "Called show_page from kiln") or, from an ACP
  agent that words it otherwise, by kiln's answer in its output (`ui/src/views/harness/page-of.ts`).

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
- `GET /api/harness/sessions/:id/pages/:page/heights`: a page the session showed, its heights:
  `heights` (Chromium's, at each width) and `engines` (`firefox`, `webkit`: its machine's
  Firefox, viewers' own browsers at their widths).
- `POST /api/harness/sessions/:id/pages/:page/heights` with `{engine, width, height}`: the height a
  viewer's own browser measured the page at, at its width (Firefox's and WebKit's; Chromium's are
  its machine's). At most 24 widths are kept an engine.

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
  again, sharp. Its bitrate follows its size (about a tenth of a bit a pixel at its rate, 2 to
  25 Mb/s; a page's flat colours and text take far less than video), unless the stream asked for
  one. Each size is captured exactly, even (a capture left to choose its own within a shape comes
  odd, and each picture cut to even was a key frame of its own); a new size is a new capture, the
  encoders kept. A picture the encoder is too far behind to take is let go before it's encoded, so
  no key frame has to follow it.
- **Drawn on the machine's GPU** where it has one: on Linux, a render node the runner may open
  (`/dev/dri/renderD*`) has the stream's browser draw and composite on it, through ANGLE's EGL, not
  on the processor. A heavy page, or a big window, comes several times as fast (on a busy machine,
  GitHub's diff scrolled at 57 pictures a second against 33; 1440p at 48 against 13).
  `KILN_BROWSER_GPU=0` in the runner's environment keeps it on the processor, and
  `KILN_BROWSER_FLAGS` gives the browser more flags of the machine's own.
- **Relayed by the server.** Chunks go up the runner's stream lane (`LANE_STREAM`, runners with
  `CAP_STREAM`) and out to each viewer's socket. A viewer that joins gets the decoders'
  configuration and a fresh key frame at once. One too slow for a chunk loses it, and the next key
  frame mends it. With nobody looking, the stream pauses: nothing is captured, encoded or sent.
  After ten minutes unwatched it closes.
- **Over WebRTC when it can.** With `[server] webrtc_addr` set, the server is each viewer's WebRTC
  peer and sends the stream's chunks to it as RTP, as they were encoded. Until the peer plays, and
  again if it goes, the pictures come over the viewer's socket; its events and what's done in it
  always do. See "WebRTC" below.
- **Shown in the viewer's browser** with WebCodecs (Chrome, Edge, Firefox and Safari have it). The
  frame is the page's window: the conversation's width, as tall as the conversation shows. The
  page lays out at that size on the agent's machine, as a phone lays it out for a phone. Its
  sound plays once the person turns it on.
- **Its resolution, as the person picks it:** Fit (the frame's own size, at the screen's density,
  by default), or a screen's: 720p (1280×720), 1080p (1920×1080), 1440p (2560×1440) or 4K
  (3840×2160, laid out at 1920×1080 at 2×). A screen's window is drawn whole in the frame, and what's
  done in the frame lands where the page has it. Measured on one machine (a Ryzen 9950X3D, its
  GPU drawing), 1080p streamed at 60 pictures a second and 1440p at about 50; 4K's capture took
  about 45 a second and Chrome's encoder about 30 (H.264 in software: a GPU's encoder, where
  Chrome uses one, as on a Mac, does more).
- **What the person does goes to it:** taps and the mouse (as touch from a phone), the wheel (the
  frame's own: the conversation around it doesn't scroll, and a frame's turns go as one), keys
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
- **Where it is:** the frame shows the page's address. Type another (a host without its scheme
  goes over https, or over http when it's this machine's, its LAN's or has a port; words are
  searched for, on DuckDuckGo), or go back, forward, reload.
- **Tabs, as a browser's** (Chromium). A link to a new tab, or a popup (a sign-in's, say), opens a
  tab of the stream's own, shown as Chrome shows it (one opened in the background isn't). With
  more than one, a select says which is shown and Close tab closes it (the tab that opened it is
  shown in its place); New tab opens a blank one, its address field ready to type in. The capture
  moves to the tab shown (the same encoders, from a key frame), and what's done in the frame goes
  to it. The agent's `use_live` has them too (`{"tab": {"show" | "new" | "close": ...}}`), and its
  answer lists them. Closing the last closes the stream.
- **Out of view** the frame lets go of the stream (it pauses) and keeps its last picture. A stream
  that ended can be started again from the frame.

### WebRTC

A stream's viewers take it over WebRTC when the server has it and it gets through, and over their
socket (WebCodecs) otherwise. Nothing changes for the page or its runner: the server is the peer
(`crates/kiln/src/harness/webrtc.rs`, on str0m, its DTLS and SRTP from aws-lc-rs).

- **Turned on** with `[server] webrtc_addr`, a UDP address such as `0.0.0.0:7072`
  (`KILN_WEBRTC_ADDR`). Viewers are told to reach it at `webrtc_advertise`, a list of `host:port`s
  (`KILN_WEBRTC_ADVERTISE`, comma-separated): a forwarded or NATed port, or a Tailscale address.
  Without it they're told the bound address, or, bound on every address, each LAN address of the
  machine at the bound port. Unset, there's no WebRTC and viewers stay on their sockets.
- **One port, every viewer.** A viewer's socket offers a peer connection (receive-only video and
  audio). The server answers as an ICE-lite peer, with a host candidate at each address viewers
  are told of, and drives every peer from one loop on one UDP socket. The offer and answer go over
  the viewer's socket, which only its person (or one they share the conversation with) has.
- **As it was encoded.** H.264 goes out on the payload type the viewer offered for it in fragments
  (packetization mode 1), constrained baseline first; VP8, VP9 and AV1 go on theirs, and the sound
  as Opus. Nothing is decoded or encoded again. A picture's media time is when it's sent, so a
  still frame sent again sharp shows at once, and each picture says it's to be shown as it comes
  (playout delay 0): a browser otherwise holds one back now and then to smooth a video's pace,
  which a page that changes when it does hasn't. What a viewer loses is sent again (NACK). Past that
  (PLI), a key frame is asked of the runner, and nothing more of the picture goes to that viewer
  until it comes.
- **The socket stays.** The first picture comes over it, while the peer connects. Once the peer
  plays, the viewer says so and its pictures go to the peer alone (its canvas, which takes taps
  and keys, is clear over WebRTC's video). If the peer doesn't play within ten seconds (the
  server's port is out of reach), fails, or the server's WebRTC stops, the pictures come over the
  socket again, from a key frame. A page that saw WebRTC fail twice doesn't try it again.
- **With no WebCodecs, Media Source.** A browser hides WebCodecs on an http:// address other than
  localhost (kiln at its LAN or Tailscale address). There the socket's pictures play through the
  browser's Media Source Extensions, which need no secure context (`ui/src/views/harness/live-source.ts`):
  each H.264 picture becomes a fragment of MP4 of its own, as it comes, on a timeline with no gaps
  (each lasts a sixtieth of a second and playing waits at the last for the next), the stream marked
  live so Chrome shows each picture as soon as it's decoded. No sound comes that way. It needs no
  WebRTC and no UDP, so it works where the server has no WebRTC or the network carries only TCP (a
  company's proxy). Where WebRTC gets through it takes over, as it does from WebCodecs. Only a
  browser with neither (an iPhone before iOS 17.1) has WebRTC alone, and says so if that can't get
  through. Measured on one machine: a tap's picture shown 45 to 53 ms after it, at a real http://
  address as with WebRTC.
- **Measured** on one machine (server, runner and viewer together): the first picture came over
  the socket 0.3 s after Open and the pictures over WebRTC 0.4 s after. A tap's picture came 22 ms
  after the tap and was shown a millisecond later, on a desktop and a phone alike (the lab notices
  it at 28 to 35 ms, as it does over the socket), and the socket carried no pictures meanwhile.
  Over a real network, a lost packet holds up nothing behind it over WebRTC, as one does over the
  socket's TCP.
- **In a cluster** the server's UDP port has to reach viewers, as QUIC's does for runners: the
  chart's `ports.webrtc` and `webrtcAdvertise` (and the external Service's NodePort), and on a
  host that forwards LAN ports to it, `KILN_LAN_WEBRTC=1 deploy/cluster/expose-lan.sh up`.

### Engines

A live page runs in Chromium unless asked otherwise. To test it in another engine, pick one (Engine
in the Tools tab's Live pages; `engine` in `show_live`):

- **Firefox**: the machine's own (on its `PATH`, in Applications on a Mac, in Program Files on
  Windows; `KILN_FIREFOX` says where otherwise), headless, driven over its WebDriver BiDi. One
  Firefox a profile (the conversation's, a person's own, or a clean one each time), each stream a
  tab of its.
- **WebKit**: Safari on a Mac, through `safaridriver` (turned on once with `safaridriver --enable`,
  which asks for an administrator's password, or with Safari's Settings › Developer › Allow remote
  automation). Its automated window opens on that Mac's screen, keeps nothing, and is one at a
  time. It has to be seen there: Safari throttles a window it doesn't show (the Mac's screen locked
  or asleep, the window covered or on another desktop), and then its pictures take about a second
  each instead of 16 ms, and a click waits half a minute or is lost. On Linux, WebKitGTK's `WebKitWebDriver` when it's installed
  (`apt install webkit2gtk-driver`). Its browser draws on a display of kiln's own, so nothing comes
  up on the machine's screen and a machine without one runs it: Xvfb when it's installed (no port,
  and only kiln holds its key; its pictures come in about 20 ms), else GTK's broadway backend in
  memory (its sockets in a folder only the runner's user reaches; its pictures come a second
  apart).

Neither has a capture a page can be streamed from. So their page's picture is taken as it changes
(Firefox's in about 5 ms: up to 30 a second while it changes or was just used, a few a second while
it's still, none while nobody looks; WebKit's up to about 6 a second, Safari taking one in about
16 ms), and one that hasn't changed goes nowhere: still costs
nothing on the wire. Each picture is encoded by kiln's capture extension as a tab's frames are
(H.264, a sharp key frame when still), so viewers, recordings and the agent's `use_live` work the
same. What viewers do goes to them as WebDriver actions (the pointer, touch, keys, text, the wheel).
They stream without sound. The Tools tab offers the engines the agent's machine has (`engines` in
`GET .../preview`); runners announce `CAP_ENGINES`.

A Firefox a runner started stops with it, and one a runner left running (a crash) is stopped when
the next one starts (its profile says whose it is).

### Devices

By default a live page lays out as its viewer's frame (its size, the screen's density, touch from
a phone). A device holds it to the device's instead (Device in Live pages; `device` in
`show_live`): iPhone 15, iPhone SE, Pixel 8, iPad Air, Laptop, Desktop. Its window, density and
touch are the device's, and so is its user agent (Chromium and Firefox say it; Safari's window is
sized to it). A viewer's own size then changes nothing: the page is drawn whole in the frame,
letterboxed, and taps and clicks land where the page has them. The devices are in
`crate::browser::devices` (and `devices` in `GET .../preview`).

### Simulators

On a Mac with Xcode, a page in WebKit laid out as an iPhone or iPad (iPhone 15, iPhone SE, iPad
Air) runs in an iOS Simulator, in the real Mobile Safari (`crate::browser::simulators`). kiln keeps
a simulator of its own for each model (`kiln iPhone 15`, the model Xcode has, else the newest of
its kind) on the newest iOS runtime there, boots it with no Simulator window, turns on its Safari's
remote automation, and drives it through `safaridriver` as it does Safari (its picture as it
changes, touch for every pointer, the device's own window). It stays booted for the next stream.

What a Mac needs once: Xcode and an iOS simulator runtime (`xcodebuild -downloadPlatform iOS`,
several GB). `safaridriver --enable` is for Safari on the Mac itself; a simulator's Safari is let be
automated inside the simulator, by kiln. Until then the Tools tab says what's missing (`ios` in
the preview's `engines`), and a stream asking for one says the same. A simulator's screen is an
odd number of pixels wide (an iPhone 15's 1179): H.264 takes only even sizes, so its picture loses
its last column.

### Recordings

Record (in a live page's own controls) keeps what it shows and what's done in it, on the agent's
machine, until its conversation closes. The agent records with `use_live` too: `{"record": true}`,
then `{"record": false}` answers with the files.

- **Never encoded again.** Its video is what the stream already sent, put into a fragmented MP4 as
  it comes: the H.264 pictures (as `avc3`, so a new window size mid-way is just its new key frame)
  and the Opus sound. A fragment ends at a key frame, so a recording cut short plays to there.
  While it records, the stream goes on whether anyone looks or not; a page that doesn't change
  costs nothing, and its last picture lasts until it changes.
- **What was done, beside it** (`.inputs.jsonl`): the page and window it began with, then each of
  the person's inputs and the agent's actions, where the page went and when its window changed,
  each at its time.
- **As a test** (`.spec.ts`): that log as a Playwright test, its window and touch as they were,
  presses as clicks, taps or drags, keys and text, the wheel, a wait where someone paused, and
  where it ended as an assertion, to add what the page should show.
- **Played again**: `use_live`'s `{"replay": "<.inputs.jsonl>"}` does a recording's log again in
  a live page, in its recorded window, each step after its pause (at most two seconds).

Each recording shows below its live page: Watch plays it there; Test and Log are its test and
log to keep. A recording of a stream in a person's own profile is theirs alone, as the stream is.

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
- **Another address**, typed: `localhost:3000` is `http://localhost:3000`, `github.com/x` is
  `https://github.com/x`, and words are searched for.

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
  bitrate, profile, engine, device}`: a stream of a page, on the session's machine. `profile` is `conversation`
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
- `GET /api/harness/sessions/:id/recordings`: the recordings of its streams, and
  `GET /api/harness/sessions/:id/recordings/:recording/:what` (`video`, `inputs`, `test`) a
  recording's file, from its machine (the video in ranges).
- `GET /api/harness/sessions/:id/streams/:stream/ws`: a viewer's socket. It's sent binary chunks
  (`crate::browser::stream::frame`: kind `v`, `a` or `c`, flags, timestamp in µs, part, parts, the
  bytes) and JSON events (`opened`, `navigated`, `loaded`, `focus`, `fields`, `selected`,
  `copied`, `dialog`, `agent`, `closed`, `error`). It sends `{"input": [...]}` (what the person
  does) and `{"control": {...}}` (`resize`, `pause`, `keyframe`, `go`, `back`, `forward`,
  `reload`, `copy`, `record`). Its events include `recording` and `recorded` (a recording's
  files, length and size, and its `id` here). With the server's WebRTC on, it's told
  `{"webrtc": {"can": true}}` first and may send `{"webrtc": {"offer": sdp}}`, answered with
  `{"webrtc": {"answer": sdp}}` (or `{"error"}`). It sends `{"webrtc": {"playing": true}}` once its
  peer shows the stream (its chunks then go to the peer alone), and `{"playing": false}` or
  `{"close": true}` to have them back. A peer that went is said as `{"webrtc": {"gone": why}}`.
- `DELETE /api/harness/sessions/:id/streams/:stream`: close it (its tab goes).

An agent's MCP server opens one through its runner, for its own session only (`{"op": "stream",
"do": "open", "url", "title"}`, `crate::harness::streams::ask`). `use_live` acts in its own
session's streams where they run (`crate::browser::stream::act`), never one in a person's own
profile.
