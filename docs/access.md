# Access: people, agents, tokens and the API

> **Superseded for CI by [ci.md](ci.md), W10** (2026-10-17), which adapts this draft and adds the OIDC
> issuer. Kept for history.

> **Draft spec.** None of this is built yet. Today kiln has one admin token (the root
> credential), password sign-in that yields UI sessions, one-time sign-in links, and per-job tokens.
> All of that keeps working; everything here is added alongside it.

One API serves the UI, the CLI, scripts and agents. Every caller is a **principal** with
**capabilities** in **scopes**. People and agents use the same routes, and the same MCP tools
mirror those routes for agents.

## Principals

| Principal | Who | Signs in with |
| --- | --- | --- |
| **Owner** | whoever installed kiln | the admin token (break-glass), the password, sign-in links |
| **User** | a person | password, sign-in link, later passkeys; personal access tokens for scripts |
| **Agent** | an agent session, an attached agent ([ci-agents.md](ci-agents.md)), an experiment's variant | a token kiln mints for it, always **on behalf of** a user |
| **Job** | a running job | its job token, scoped to its run plus what the pipeline grants |
| **Machine** | a runner | mutual TLS from kiln's CA (unchanged); not an API principal |

An agent can never do more than the person it acts for, and it never does more than its own
grant. Its effective rights are the intersection of the two.

## Capabilities and scopes

A **scope** is the whole server, a repository, a pipeline, an environment or a machine. A grant
on a repository covers its pipelines and environments.

| Area | Capabilities |
| --- | --- |
| Runs | `runs.read`, `runs.trigger`, `runs.cancel`, `runs.rerun`, `logs.read`, `artifacts.read`, `artifacts.write`, `caches.write` |
| Code | `diffs.read`, `diffs.submit`, `diffs.review`, `diffs.land`, `queue.manage` |
| Pipelines | `pipelines.edit` (always as a reviewed diff), `environments.deploy` |
| Machines | `machines.read`, `machines.manage`, `machines.terminal`, `binaries.publish` |
| Agents | `agents.run`, `agents.manage`, `agents.attach`, `agents.stop`, `experiments.run` |
| Secrets | `secrets.use:<name>`, `secrets.manage` |
| Server | `settings.manage`, `tokens.manage`, `users.manage`, `audit.read` |

**Roles** are named sets of capabilities: `viewer`, `developer` (trigger, rerun, submit, review),
`maintainer` (plus land, attach agents, manage everyone's agents, manage the queue), `admin`
(everything but ownership) and `owner`. Custom roles are allowed. A user holds roles per scope.
Grants add up across scopes, and an explicit deny wins.

**Whose agents.** A person reaches the agent sessions they opened, and those sessions' children.
The owner and anyone with `agents.manage` reach every session. Anyone else gets 404 for a session
that isn't theirs: it isn't in their list, and no route, socket or operation finds it. Opening an
agent on a machine needs `runners.terminal` there (the server's own scope for the server), as a
terminal does. So does listing, reading, taking over or branching the sessions found running on
it, and reading the sessions it keeps. Commands and closes are audited as the person who made them.
docs/agents.md, "Whose agents", has the whole rule, agents' own reach included.

## Tokens

| Kind | Format | Lifetime | Notes |
| --- | --- | --- | --- |
| Personal access token | `kiln_pat_<id>_<secret>` | chosen, ≤ 1 year | scopes and capabilities chosen at creation; shown once; revocable |
| Agent token | `kiln_agt_<id>_<secret>` | rotated hourly while the agent lives | minted automatically; revoked when the agent ends |
| Job token | `kiln_job_<id>_<secret>` | the job's lifetime | its run, plus what the job's `permissions` ask for (below) |
| Admin token | unchanged | until rotated | break-glass; the CLI prefers a PAT once one exists |

Secrets are stored hashed (SHA-256: they are long and random), and every token records its last
use. The prefixes let secret scanners and kiln's own log masking recognise leaked tokens.

**Jobs ask for what they need** with a new pipeline key, so no pipeline needs the admin token in
a secret:

```yaml
jobs:
  binaries:
    permissions: { binaries: publish }      # ci/release/publish.sh uses $KILN_TOKEN
```

## The API

- **One route table** in the server describes every route's method, path, parameters, body,
  result, required capability and scope. It drives authorization, the OpenAPI 3.1 document at
  `/api/openapi.json`, the CLI and the MCP tools, so the four can't drift apart.
- **Conventions for new routes:**
  - cursor pagination (`?cursor=&limit=`);
  - `Idempotency-Key` on POSTs that start things (trigger, submit, land);
  - ETags on reads;
  - errors as `{ error: string, code: string, details? }` (the `error` string stays, so today's clients keep working).
- **Events:**
  - `/api/events` (WebSocket) and `/api/events/stream` (Server-Sent Events), both filtered to what the caller may read;
  - outgoing webhooks: subscriptions per scope, HMAC-signed, retried with backoff.
- **Limits:** per-token rate limits, with `Retry-After` when they bite.

## The CLI

`kiln login` stores a personal access token per server in `~/.config/kiln/credentials.toml`
(mode 0600). Then:

```sh
kiln runs [--repo r] [--follow]          kiln run ci [--branch b] [--input k=v]
kiln logs <run|job> [--step n] [--follow] kiln rerun <run> [--failed] · kiln cancel <run>
kiln diffs · kiln submit · kiln land D12 · kiln review D12      (review.md)
kiln machines · kiln tokens create|list|revoke · kiln api GET /api/runs
```

## MCP

- **Serving it:** kiln is an MCP server at `/api/mcp` (streamable HTTP), and `kiln mcp` bridges it
  over stdio for local harnesses.
- **The tools** come from the route table, limited to the caller's capabilities:
  - runs, logs (search, tail and a failure excerpt), artifacts, test results and metrics;
  - comparing runs, triggering, rerunning, cancelling and bisecting;
  - diffs, stacks, comments and reviews;
  - asking the user (the choices dialog) and starting experiments.
  Logs and artifacts are also MCP resources (`kiln://runs/<id>/logs`).
- **Agents kiln starts get it automatically,** on every runner: the runner's own `kiln` server
  (`kiln reach mcp`, docs/reach.md §7), with code mode over the kiln's machines and these
  operations, which it asks of the server through the runner's own connection.
  - Claude Code through `--mcp-config`;
  - Codex through its configuration;
  - ACP agents through session parameters.
  Each acts for whoever started it (with their rights), so every call is audited under the agent,
  on behalf of its user (`agent:<session>@<runner> for <user>`). Agents the server itself starts get
  `/api/mcp` with a token of their own (`kiln_mcp_session-<id>_…`, which works only while kiln runs
  that agent), and act for whoever started them the same way (`agent:<session> for <user>`).
- **Agents' own operations** (`agents.*`, `plans.*`, `goals.*`) are tools too, with deployments on
  or off, each checked as its route (`agents.run`): an agent starts, waits on and reads other
  agents, and what it starts can do no more than it can (docs/agents.md, "Agents that run agents").

## Audit

Every change records the principal, whom it acted for, the token, the scope, the target, the
result and the time. The audit page filters by any of them, and an agent's page shows its trail.

## Later: Athame

hyper-light's Athame (identity, access and policy for agents) can take over principals, grants
and policy evaluation. kiln keeps the same capability names and scopes, so the switch changes where
decisions are made, not what they mean.
