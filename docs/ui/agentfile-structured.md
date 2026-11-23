# An Agentfile's builds, install sources, and its structured view

Serves kiln plan 01a1cd8e862e7169a074db3d5fa33f3d's build plan P4.5 (prebuild on save), P4.6 (install sources),
P5.3 (per-harness settings) and P5.4 (agents and the Agentfile in step), from the kiln × Hex × Shards design,
sections B, C and D. Decision 2 holds: until Reliquary shares images, each runner builds its own. Rules:
[principles.md](principles.md).

## Built, in the app (`/agents/agentfiles/:id`)

- **Builds, first**: a bar per runner in kiln's status colours (building drifts in the prism), "n/m built · k
  refused", Build vN. A row per runner: state dot, runner, platform, step meter while building, "serving vN" while a
  newer one isn't built, when; a refused row opens Shards' words in place with Build again; a runner that doesn't
  build says why in a word or two ("no microVMs", "kiln too old"). Rows go queued → building → built while shown.
- **Sources**: a row per `HARNESS` or `AGENT` source: kind glyph and word (https, ssh, s3, git, path), the directive
  and name, the source cut short, its pin (sha256, object version, ref → the commit it named, kiln's pin), the
  credential its fetch is handed by name and scope (never a value), and the origin that fetch alone is granted. A row
  jumps the editor to its line; its pencil opens the source picker in place, which writes the line in Shards' syntax
  and refuses a bad pin in a few words.
- **Versions** carry their image: built or not, a digest per platform, size, how many runners start sessions from it.
- Code: `ui/src/views/agentfiles/agentfile.tsx`, `builds.tsx`, `sources.ts`, `agentfiles.css`; data
  `ui/src/api/agentfiles.ts`; mock `ui/mock/agentfiles.ts` (`GET|POST /api/agentfiles/:id/builds`). Tests:
  `ui/tests/agentfile-sources.test.ts`, `ui/e2e/agentfile-builds.spec.ts`.

## Designed, not built yet

The structured view below (per-harness cards, agents, grants, volumes, other lines) waits on `agentfiles.check`
returning Shards' structure; Save starting the builds waits on the server. The standalone mockup it was drawn in
([mockups/agentfile.html](mockups/agentfile.html)) is superseded in look by the app; its structure stands.

## Where it lives

One of your Agentfiles, `/agents/agentfiles/:id` (`views/agentfiles/agentfile.tsx`).

- Kept as they are: the name field, Save and Discard, Versions and Delete.
- The text editor gets a **structured view** beside it:
  - on a wide window (the page at 1040 px and more) the two stand side by side, the text sticky at the right;
  - narrower, a `Segmented` control `Structured | Text` picks one, Structured first.
- Side by side on wide windows stays (decision 8).
- One Save keeps whichever was changed: the view writes the text, and the text is what's kept, a version as
  today.
- **On your runners** (the builds) joins the page under Save, above Versions.

## The structured view

Its sections are separated by hairlines, each headed as the page's `af-h`.

### Harnesses

- One card per `HARNESS` in the text.
- Its head reads `claude · Claude Code · 2.1.311 · kiln's pin`, folds the card, and has **Remove**.
- A native select of the ten harnesses not yet in it, then **Add**, writes a new harness's lines with kiln's
  defaults: its newest pin, its first model, your sign-in, and its vendor's hosts.
- Each card has six rows. Each row has a key, the directive it writes in faint mono, and its control. Under the
  control, an `afs-writes` line says what it writes or means.

| Row | Control | Writes |
|---|---|---|
| Version | for kiln's pin: the newest, and each older pin an Agentfile version still uses (`2.1.298 (kept: rust-dev v7 uses it)`); for other sources, what the source names | the path or URL |
| Install source | `Segmented` Path · Git · S3 · HTTPS tarball, and that kind's fields (below) | `HARNESS <id> [flags] FROM <source>` |
| Model | the harness's models | `LABEL kiln.harness.<id>.model=<model>` |
| Sign-in | "Your own sign-in, kept in your HOME volume" or "API key: <secret name> · <scope>" | `LABEL kiln.harness.<id>.sign-in=home \| secret:<NAME>` |
| Volumes | its HOME, said as it is | `VOLUME --target-kind=harness /home/<id> FOR <id>` |
| Egress | `host:port` chips with ✕, and an add field with **Allow** | `NETWORK --egress=… <id>-out FOR <id>`, its `EXPOSE … AS egress` and `CONNECT` |

The egress row ends: "TCP out to these hosts alone, by name. Nothing comes in; no other host, port or protocol
goes out." Each of kiln's hosts carries its reason as its title ("Claude Code's API").

### The source picker (P4.6)

Each kind writes Shards' own flags for `AGENT` and `HARNESS`, as they are at Shards main
(`crates/agentfile/src/agentfile.rs`: `--checksum`, `--redirect`, `--endpoint`, `--region`, `--path-style`,
`--version-id`).

| Kind | Fields | Pinned by | Credential, by reference |
|---|---|---|---|
| Path | the path | kiln's pin, checked by SHA-256 as kiln fills it | none |
| Git | over https or ssh; repository; ref; folder in it | the commit the ref resolves to, shown from the last build ("v0.68.0 → 3f9c2e1d") | Token (as `GIT_AUTH_TOKEN` for its host) or SSH key; the runner's known_hosts is trusted, and the key never enters the VM |
| S3 | bucket; key; endpoint, with AWS, Cloudflare R2 and Mantle filling it; region; path-style; object version; SHA-256 | the object version, or its ETag plus SHA-256 | access key ID and secret access key, signing with SigV4 |
| HTTPS tarball | URL; SHA-256 (required: the field is refused until it's 64 hex digits); sign-in to the server; redirects allowed to | the digest | a bearer or basic header |

- Every credential is a native select of the person's **secret names**, each with its scope, and "None" first.
  The scope is the new **`user`** scope (the person, 2026-11-14): a person's own secrets, for install-source
  credentials.
- No value is ever shown, typed or written. The line under it says so: "kept by kiln with this version and
  handed to the fetch step alone; never in the text, a layer, the build record or a log".
- Every kind with an origin says the grant: "Fetches from releases.example.dev:443 alone: a grant for that step
  and no other (default deny)", plus each `--redirect` named.

### Agents

- One row per `AGENT`: name (a link to its definition), its harness, its skills and MCP counts, its definition
  version, and at the right "in step" (ok tone), "differs from its definition" (warn tone), or "Make a
  definition of it" for one written in the text alone.
- A row that differs opens the same conflict block as the definition's editor: Setting | This text |
  Definition vN. Its choices are **Take the definition's** and **Keep this text's: update the definition**.
  Neither side overwrote the other (P5.4).

### Grants

- A `data-table` of every flow the text grants: Who | To | Ports | Way | From. Its rows come from the harnesses'
  egress, each agent's MCP, and each source's fetch, which is "out, fetch only".
- Under it: "Nothing else goes in or out, and nothing reaches kiln's MCP over the network: it comes over the
  link kiln drives each harness by."

### Volumes

- Path | For | What it is | Kept on: each harness's HOME, and each agent's volumes ("the agent's, across its
  sessions"), all "each runner" (decision 1: per machine, Slates later).
- "The project is plugged in at its own path as a session starts; no volume holds it."

### Base and other instructions

- The `FROM` (its digest cut short), and every line the view doesn't show (`RUN`, `COPY`, `ARG` …) as written,
  read only.
- "Lines the view doesn't show stay as written: edit them in the text."

## On your runners (P4.5)

- A `data-table`: state dot | Runner | Platform | Build | Image | Took | Why.
- One row per runner of yours:
  - a microvm runner builds for its own platform (Macs `linux/arm64`, adacachy2 `linux/amd64`);
  - a runner that doesn't build says why, faint: "not built: can't run microVMs", "not built: runs sessions on
    the host".
- The build cell reads `built 4 min ago`, `building step 6 of 9` (with a `Meter`), `queued`, or `failed 4 min
  ago`.
- A failed row's **Why** opens, under the row:
  - Shards' words, as Shards said them, in a `words` block with the fail edge, e.g. "Shards refused the build at
    line 13 (HARNESS codex): the download from releases.example.dev for linux/amd64 has SHA-256 4b07e1a9c3d5…,
    and --checksum=sha256:9f2c55d0b7a1… names another.";
  - **Build again**;
  - "Sessions on adacachy2 start from v7's image until this one builds."
- Save starts them all (the page shows them go queued → building → built). The summary at the section's head
  reads `v9 · 4 of 5 built`.

## What it needs from the server

- `agentfiles.check` returns, beside its diagnostics, the structure:
  - harnesses, with source, flags, model and sign-in labels, egress and volumes;
  - agents;
  - grants;
  - the other lines, each with its line range, so the view never parses on its own. The mockup's small parser
    stands in for Shards'.
- A `user` secret scope (`/api/ci/secrets?scope=user:<id>`, names only, its owner alone), beside `server`,
  `org`, `repo`, `workflow` and `env`.
- An Agentfile's credential references, kept with each version: a map from source to secret name and scope.
  They aren't in the text, and are refused if they name a secret the person can't use.
- kiln keeps a harness pin while any Agentfile version references it.
- Builds:
  - `GET /api/agentfiles/:id/builds`: per runner, platform, state, step, digest, took, and Shards' words on
    failure;
  - `POST /api/agentfiles/:id/builds/:runner`: Build again;
  - an `agentfile_build` event.

## Tests (UI, Chromium and WebKit, desktop and phone)

- P5.3 AC1: for a harness card, set version, source, model, sign-in and egress. The text changes to match.
- P5.3 AC2: edit the text's `LABEL … model=` and `NETWORK` lines; the card follows. Shards' check passes both.
- P4.6 AC1 and AC2:
  - set a git source over ssh with the key `GH_DEPLOY_KEY`; the text reads `HARNESS x FROM
    git+ssh://…#<ref>`;
  - the build uses the key;
  - the text, the page's HTML and the build record hold no secret value.
- P4.5:
  - save, and each microvm runner's row goes to built;
  - a session started there records no build;
  - a broken source shows Shards' words in its row.
- P5.4: change pr-triage's definition, then this text; the Agents row shows the conflict until a choice.

## Decided (the person and decision 8, 2026-11-14)

- A harness's model and sign-in are written as labels: `LABEL kiln.harness.<id>.model=…` and
  `LABEL kiln.harness.<id>.sign-in=home|secret:<NAME>`. A secret's name, never its value.
- Install-source credentials come from the new `user` secret scope.
- The view and the text stand side by side on wide windows.
- An older pin stays while any Agentfile version references it.
