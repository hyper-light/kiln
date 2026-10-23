# Train notes: deploying by API, P5 to P8, with P6

The deploying-by-API phases that land together (docs/deploy-api.md, §19's plan):
- P5: apps end to end, its one page among them, and its chat;
- P7: platforms under releases (AWS, Google Cloud, Azure, the hosted platforms, and the rest of
  cd-targets.md §3);
- P8: every call with its page and its MCP operation;
- P6: deploy's records on kiln's Records seam, and several servers.

These notes cover what each delivers, what deploying it does and how to watch it, its risks, its
rollback, and what's still open.

**Where it stands (2026-10-23, when written):**
- **Merged:** with this file, as one fast-forward of main. The landing commit's message has the
  gate's evidence.
- **Deployed:** not yet. Deploys go through Ada.
- **Gated** on fedora before the push, on the commit that carries this file.
- **Checked before the gate,** on adacachy at idle priority:
  - clippy, clean;
  - the deploy and records unit tests;
  - every deploy integration test, through real servers on the new store, and the CI job that
    releases through deploy. Two CI compose tests failed once under adacachy's load, then passed
    on a re-run.
- **On the test servers, on 94924e5's build** (this train before main's last four commits and the
  copy's retry): test server A ran on its own data, which P6 migrated, copying 238 rows onto the
  seam. The UI's files came through kiln's kept-assets folder, because the debug build was
  compiled before the UI was built. It made `kiosk` from the new-app page:
  - its source repository `acme/kiosk`, from the template's starter;
  - its registries, `shop/kiosk` on kiln's own registry;
  - its registrations in staging and production;
  - its first releases of 6.10.2. Staging's (35) succeeded. Production's (36) waited for Grace's
    approval, given in the fake Slack, then succeeded.

  Its pods ran on kilncd-e2e and on deploy-argo.
- **Rolling back past P6, drilled:** on a scratch copy of test server A's database, 94924e5
  migrated it and copied its rows onto the seam. Then a build from before P6 started on it: the
  ci-actions worktree's, holding 7656471, the release production ran that day. Its health answered
  in 2 s, its repositories and runs read back, and it left P6's migration alone.

## What it delivers

Each item's row in docs/deploy-api.md's Status table has the detail and how it was tested.

- **P5, apps end to end:**
  - templates;
  - apps made from a template with their source repository (one of kiln's own, or made on GitHub,
    GitLab or Bitbucket), their registries (kiln's own registry, with tag policies) and kiln's base
    chart;
  - registrations and first releases;
  - all of it from one page, `/deployments/releases/new`;
  - maintenance: owners, template, archive, delete;
  - fleet changes as releases, or as pull requests in each app's source repository;
  - secrets per app: sealed, versioned and rotated, delivered on KilnCD, on Argo CD and on
    platforms;
  - owners as groups, kiln's own and identity providers' from each sign-in.
- **P5's chat:**
  - a release's one message in each chat (a Slack bot's, a Discord webhook's, a Telegram bot's),
    edited as the release moves;
  - Approve and Reject on it while the release waits;
  - chat commands;
  - links between chat accounts and kiln's users.
- **P7, platforms under releases through KilnCD:**
  - Lambda, Workers, Vercel and ECS;
  - AWS: CloudFormation, Beanstalk, EC2 and static sites;
  - Google Cloud: Cloud Run, Functions, App Engine and Compute;
  - Azure: Container Apps, App Service, Functions, scale sets and static sites;
  - the hosted platforms: Cloudflare Pages, Fly, Netlify, Render, Heroku and DigitalOcean;
  - the rest: Nomad, and through kiln's runners host services, compose, microVMs,
    Terraform/OpenTofu, Pulumi and jobs.
- **P8:**
  - every `/api/deploy` call has its page and its MCP operation (§12.4 lists all 56);
  - a platform's definition is edited on its app's page;
  - the `kiln-deploy` skill names every operation.
- **P6:**
  - deploy's records, its work queue, its timers and its key-value state are rows of kiln's
    Records seam. They're kept in kiln's store until Amphora's Records (CP-T1.5) take their place;
  - each release's lease is taken under compare-and-set and passes to another server once it
    lapses, so a release a dead server left is finished by another;
  - what kiln's deploy tables held is copied onto the seam once, and deploying by API doesn't
    start until it is.

## Deploying it

- **No config:** kiln.toml gains no setting. `[cd]` is on by default, so production already runs
  deploying by API.
- **Five migrations, each additive:**
  - `20261022-1700_p5_groups`: `user_groups`, `user_group_members`;
  - `20261022-1800_p5_registry_policies`: `registry_policies`;
  - `20261022-1900_p5_fleet_proposals`: `deploy_fleet_proposals`;
  - `20261022-2000_chat_releases`: `release_chat_posts`, `chat_links`, `chat_link_codes`, and
    `ci_integrations.signing_ref`, a new column that may be empty;
  - `20261023-1300_p6_records`: `records_versions`, `records_families`, `records_meta`.
- **At the first start:**
  - **P6's copy.** Production's deploy tables were empty on 2026-10-23 (0 environments, apps and
    releases, read through its own operations), so it copies nothing. It writes the counter row,
    and after that it never copies again. When it does copy, it logs
    `deploy: its records copied from kiln's tables onto the Records seam, the tables left as they
    were` with `rows=<n>`.
  - **The kiln-base chart.** Each server publishes the kiln-base chart it ships to its own
    registry, as `kiln/charts/kiln-base:<version>`.
- **Nothing acts until it's asked:**
  - the platforms act only for an app whose package names them, with that platform's account;
  - chat posts only where a chat is set up (Settings, Chat).

## Watching it

- **The Releases board** (`/deployments/releases`, or `deploy.board` over MCP): what runs in each
  environment and what's on its way.
- **A copy that can't be made:**
  - the supervisor's error line, `supervised task failed; restarting`, with `child=deploy` and
    `its records couldn't be copied from kiln's tables onto the Records seam`;
  - `kiln_supervisor_restarts_total` climbing;
  - deploy's calls answering 503 with the same reason.
- **`records_versions`' size** in kiln's store (under "Risks").
- **No new metric series.** The delivery metrics (DORA's four keys) are read from the releases
  over the API, as before.

## Risks

- **Nothing compacts kiln's Records yet.**
  - `records_versions` keeps every version of every row.
  - While a release is in flight, each poll writes its lease and when it's due next, and the
    worker wakes every second.
  - `StoreRecords::compact` exists and is tested (RECORDS-14), but nothing calls it.
  - Production writes none of these rows until it deploys by API.
  - It wants a compaction on a timer before production deploys by API in earnest (under "Ada's
    open decisions"). Until then, nobody should make apps in production's deploy.
- **A copy that can't be made keeps deploying by API down.**
  - Deploying by API doesn't start until the copy is made, so no release is made before it and
    nothing is forgotten.
  - The flip side: one row the copy can't read (a release or registration whose JSON doesn't
    parse) keeps it down until that row is mended or removed. Kiln's other services run on.
  - Production has nothing to copy.
- **Several servers are tested in one process.** The two-server test runs two hubs in one process
  over one Records. Two processes need Amphora as its own process (CP-T1.2) or replicated groups.
- **P7's platforms are tested against fakes of their APIs**, not against real accounts. Each
  provider's tests run through releases as deploy defines them. The first real use of each is its
  first contact with the real API.
- **Records with P5's new fields stop a build from before P5 reading them.** The build from before
  P6 couldn't read test server A's environments, which carry `targets.kilncd`. Production has no
  such records yet.

## Rolling back

- **To the release before** (kiln:7656471, or later from main): a KilnCD rollback
  (`kiln cd rollback default/kiln-production <id>`), or revert the landing on main, then build and
  sync.
- **No config to take out:** the train adds no setting.
- **The release before runs on this train's data** (drilled, above):
  - its migration runner applies only the migrations it lists that aren't recorded yet, so it
    ignores the five new ones;
  - it keeps no schema version that it could refuse (`crates/kiln/src/store/migrations.rs`, the
    same in 7656471 and in this train).
- **What the release before sees:**
  - **Deploy's records as they were when P6 first started.** The copy leaves kiln's deploy tables
    whole. Whatever deploy recorded on the seam after that, the release before doesn't see.
  - **Nothing copied a second time when coming forward again,** since the counter row is there.
    Whatever the release before recorded in its tables in between stays in its tables only.
  - **Environments with P5's fields** (`targets.kilncd`, from `kiln deploy env add --kilncd`),
    which stop it reading environments. Take those fields out, or the environments, before going
    back.
  - **Chat messages already posted,** as they were. It doesn't edit them.
  - **The new tables, read by nothing.** So is the kiln-base chart in the registry.
- **Production today:** it holds no deploy records, so going back loses and hides nothing.

## Ada's open decisions

1. **P6 in this train,** or on its own after it. Kiln Distro recommended it in, since the rollback
   drill passed.
2. **Compaction for kiln's Records** (under "Risks"): a follow-up goal, finished before production
   deploys by API in earnest.

## Open with others

- **Amphora:**
  - its Records in place of kiln's (CP-T1.5);
  - Amphora as its own process (CP-T1.2), for two servers in two processes.
- **Quiver:**
  - the persisted class (CP-T2.2), for deploy's streams;
  - the UI's feed on the bus (CP-T2.1).
- **Kiln Distro:** the gate, the push, the deploy with Ada, and §6's 24 hours after it.
