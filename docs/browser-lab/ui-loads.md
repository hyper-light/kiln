# UI page loads

How long kiln's web UI takes to load at 50 and 100 kbit/s, and what holds it to that: goal 8 of the
browser protocol program (goal `6c34876e530e8ffead64f802bc486621`, plan
`01a1a832ebfd742cabb4861d737c3832`, research `01a1a832ebfe72a3b69e7a0bc151bf03`). It belongs with
the browser lab (docs/browser-lab.md, goal 1's): the page loads run on that lab's links and browsers.
How kiln serves the files is docs/http.md, "The UI's files"; the UI's side is ui/README.md, "Bytes
on a slow link".

## Pages, visits, links

- **Pages** (`ci/ui/page_bytes.mjs`'s `PAGES`, `ci/ui/pageload.mjs`'s): sign-in (`/` signed out),
  the overview (`/`), Mission control (`/agents`), a long conversation (`/agents/:id`), `/runs`
  (which lands on Ship, `/ship`) and a run with its logs (`/runs/:id`).
- **Visits**: first (an empty cache), repeat (the same build, the cache kept, a new tab), after a
  deploy (the cache from build A, the server on build A plus a one-line change in the
  conversation's own view, `ci/ui/two_builds.sh`'s), and an in-app move to a second page.
- **Links**: goal 1's 50k and 100k profiles (50 and 100 kbit/s each way, 800 ms base RTT, normal
  jitter of 50 ms each way, a queue of 4 s at the rate), never DevTools throttling; Chromium and
  WebKit, desktop and phone; HTTPS with a test root the browsers trust (a repeat visit over an
  untrusted certificate is refused, not reported as warm: `pageload.mjs`).

## The budgets' arithmetic

Below 100 kbit/s a page's time is its bytes over the link's rate plus a round trip for each wave of
fetches its critical path waits on, plus the handshake (at 800 ms the link holds 5 KB in flight, far
under TCP's first window, so slow start isn't what limits it):

    bytes = (target - waves x RTT - handshake) x rate

with RTT 0.8 s, the handshake 2 RTT and a 4 KiB server flight (goal 7's to cut), and waves as
`page_bytes.mjs` counts them (the document, what it names, the route's code, the fonts: 4 for a
first visit; 3 after a deploy; 2 for a repeat). The time targets are proposed, and Ada's to
confirm:

| visit | target (proposed) | bytes it allows at 50 kbit/s | at 100 kbit/s |
| --- | --- | --- | --- |
| first, ready | 30 s at 50k, 15 s at 100k | 153,404 | 123,404 (at 15 s) |
| first, readable | 10 s at 50k | 28,403 | |
| after a deploy, ready | 10 s at 50k | 33,404 | |
| repeat, ready | 5 s at 50k | 7,153 | |

`ci/ui/page_budgets.json` holds these and each page's budget (today's bytes plus 2 KiB, moved only
down), and the ui job fails a page over its budget, naming the page, the measure and both numbers
(`node ci/ui/page_bytes.mjs --check`; a seeded +10 KiB in a route's chunk fails it:
`ci/ui/page_bytes.test.mjs`).

## Before and after, from the build

`node ci/ui/page_bytes.mjs --dist ui/dist` on fedora, main at `01d3ff1c99b` against this goal's
branch at step 7 (Brotli as kiln sends it; KiB):

| page | main: total (JS, CSS, fonts, HTML), requests | now: total (JS, CSS, fonts, HTML), requests |
| --- | --- | --- |
| sign-in | 228.4 (152.8, 22.7, 51.3, 1.6), 47 | 230.9 (150.4, 22.7, 51.3, 6.5), 49 |
| overview | 224.3 (148.6, 22.7, 51.3, 1.6), 44 | 226.9 (146.3, 22.7, 51.3, 6.5), 46 |
| Mission control | 466.8 (356.0, 58.0, 51.3, 1.6), 98 | 467.4 (351.7, 58.0, 51.3, 6.5), 100 |
| conversation | 519.4 (405.9, 60.6, 51.3, 1.6), 103 | 508.9 (390.5, 60.6, 51.3, 6.5), 104 |
| `/runs` (Ship) | 473.3 (366.3, 54.1, 51.3, 1.6), 87 | 381.2 (270.1, 53.4, 51.3, 6.5), 83 |
| run | 243.9 (167.5, 23.5, 51.3, 1.6), 51 | 246.3 (165.0, 23.5, 51.3, 6.5), 53 |

The shell's HTML grew by the import map (4.4 KiB) and the splash (0.5 KiB); the chunks' JavaScript
shrank by about as much (shorter names for each other). Ship sends 26% less JavaScript (its log
boxes load xterm as a log shows); the conversation 4% less (its inspector's Plan and Tools tabs load
when opened). The step's 30% for the conversation and Mission control is missed: what's left is the
shell (react-dom 53 KiB, the router 20 KiB of its 146 KiB) and a hundred-odd modules their first
views render (markdown, the timeline, the composer, choices); `ci/ui/attribute.mjs` lists them.

After a deploy of a one-line change in the conversation's view (`ci/ui/two_builds.sh`):

| | main | now | now, Chromium, with a delta |
| --- | --- | --- | --- |
| hashed files renamed | 240 | 1 | 1 |
| the conversation downloads again | 396.3 KiB | 70.9 KiB (its route's chunk, the shell) | 6.6 KiB (a 62-byte delta for the chunk, the shell) |
| every other page | 147.0 to 345.8 KiB | 6.5 KiB (the shell) | 6.5 KiB |

## kiln's serving, against a real server

`ci/ui/serve_check.sh` on fedora (a debug kiln on loopback, build A then build B with A kept):

- compressible files sent compressed just after a start: 286; uncompressed: 0;
- the shell (any page), the manifest, `sw.js` and icons asked again with their ETags: each a 304,
  180 bytes with its head; 1,260 bytes for the seven;
- build B's changed chunk asked with A's version as the dictionary: `dcb`, 62 bytes, against 65,960
  for Brotli alone; its header names the dictionary's SHA-256; `Use-As-Dictionary:
  match="/assets/_app.agents._id-*.js", match-dest=("script")`, `Vary: Accept-Encoding,
  Available-Dictionary`;
- web-tree-sitter's runtime: 68,224 bytes Brotli (210,415 raw); a TypeScript grammar 93,067 (it was
  138,862 as gzip inflated in the page).

## What the arithmetic says the times are

Not measured: bytes over the rate, plus the waves and the handshake, at 50 kbit/s.

| | main | now |
| --- | --- | --- |
| conversation, first visit, ready | 90.6 s | 88.8 s |
| conversation, first paint | at ready (blank until the code runs) | 8.6 s (the HTML and the shell's stylesheets, then the splash) |
| conversation, after a deploy | 69.6 s | 16.3 s; 5.7 s in Chromium with a delta |
| a repeat visit's shell and unhashed files | their whole bytes again | 1.3 KB of 304s |

Server rendering the first views (step 8, Ada's call) would make readable the HTML, its
stylesheets and the rendered content: for the conversation about 70 KiB of HTML and CSS plus the
content (about 18 to 22 s at 50 kbit/s, for 20 to 40 KiB of it), for Mission control about 75 KiB (about 16 s), against
about 89 and 82 s now. The fonts (52,533 bytes, Geist and Geist Mono's Latin cuts: 8.4 s at 50
kbit/s) are 10% to 23% of a first visit; whether they may come after ready on slow links is Ada's.

## The lab's cells

Not yet run: the page-load cells (every page, visit, link, engine and width, 10 runs each, link
byte counters, times to first paint, readable, ready and settled at p50, p90 and max) run on goal
1's lab, which hadn't landed when this was written. `ci/ui/pageload.mjs` is the driver it runs:
one visit kind per run, its link bytes read from the lab's counters (`--link-bytes`), its server
moved from build A to B for the after-deploy visits (`--deploy`).
