# Notifications

kiln tells you when an agent needs you, on every device you turn notifications on for: phones and
desktops alike. It uses the browser's own push (Web Push), so a phone hears it with kiln closed.
This is docs/agentic.md N1 and N2; Mission Control's side is N3.

## What you hear

| Kind | When | Sent |
|---|---|---|
| **Needs you** | an approval, a question or a plan waits on you | urgent |
| **Finished** | a turn ended, and not because you stopped it | normal |
| **Stopped** | the agent crashed or couldn't start | normal |
| **Paused** | it reached a usage limit and waits for it to reset | normal |
| **Low battery** | a machine your agents run on is at 20% or less | normal |

Each device chooses which of these it hears (Settings → This device → Notifications). Each agent
can be muted from its ⋯ menu ("Mute notifications", then "Unmute notifications"); muting is yours
alone and holds on every device.

## Who hears it

An agent's notifications go to the person who started it. On a kiln nobody else signs in to, that
is the owner. When kiln doesn't know who started an agent (one started from a shell with
`kiln spawn`, or before kiln kept track), the owner hears it. Low battery goes to everyone with an
agent on that machine.

## One notification per agent

- Its title is the agent's name, as Mission Control shows it.
- Its text is the one line Mission Control shows: "Asks: which database?", "Bash: cargo test -p
  kiln", "Finished: all 160 tests pass", "Stopped: claude: command not found".
- It carries the agent's id as its tag, so a newer state replaces the older one on the device.
  The push service gets a topic made from the id too, so a device that was away gets only the
  newest.
- Tapping it opens the agent, in a kiln window that's already open when there is one.
- None shows while a focused kiln window is already on that agent. (Safari takes push away from a
  site that receives one and shows nothing, so there kiln shows it silently and closes it at once.)

## When a note is sent

kiln follows every agent's state and sends a note only when it changes, once it has settled:

| Kind | Settles for | Then |
|---|---|---|
| Needs you | 0.4 s | sent if an approval, question or plan waits that wasn't said before |
| Finished | 8 s | sent if the agent is still idle after that turn (kiln may keep it going: then only the real end is said) |
| Stopped | 2 s | sent if it's still stopped |
| Paused | 2 s | sent if it's still paused for that limit |

So a state that comes and goes says nothing, a burst of approvals is one note, and nothing is said
twice: what was said is kept with the server's data, so a restart, or a machine coming back
online, doesn't say it again.

## Turning it on

Settings → This device → Notifications → **Notify this device** (`/settings#device.notifications`
opens it). The browser asks for permission then, on that click, never before. Mission control
offers the same in one quiet line ("Notifications are off here. Turn them on · Not now") while this
device could have them and doesn't; "Not now" puts the line away in this browser. What else it
needs:

| Need | Why | What to do |
|---|---|---|
| A secure page | browsers only push to HTTPS pages (and localhost) | open kiln through a front door (docs/front-door.md) |
| On an iPhone or iPad, kiln on the Home Screen | Safari pushes only to web apps added there | Share → Add to Home Screen, then open kiln from the Home Screen |
| Outbound HTTPS from the server | the server posts to Apple's, Google's and Mozilla's push services | allow it; Settings says when the server can't reach them |
| Push on in kiln.toml | | `[push] enabled = true` (the default) |

Settings says which one is missing when one is. It also lists your devices, what each hears, when
each last got something and why it didn't, and sends a test.

## Mission control on a phone

Mission control opens on what needs you (docs/agentic.md N3):

- Agents that need you are first, whatever the sort; the sort orders the rest. The list still
  holds still while agents work: it's ordered when it's opened, refreshed, filtered or sorted.
- On a phone the page's head is one line: its title, how the agents stand (cut to fit), Plans and
  a + for new agents. The filter and the sort share a row.
- The message box for many agents stays folded until you select agents, or open it with "Message
  agents" at the list's head; it opens and folds in the page's flow, under that line.

## On the server

- **Its key.** The server makes a VAPID key pair (P-256) once, in `<data_dir>/push/vapid.pk8`
  (mode 0600). Browsers subscribe with its public key, and push services take only what it signed.
  The private key is never logged and no API returns it. Removing the file makes a new key, and
  every device has to turn notifications on again.
- **Delivery.** Each note is encrypted for each device (RFC 8291: only the device can read it, not
  the push service), signed (RFC 8292: an ES256 token for the push service's origin, valid 12
  hours) and posted to the device's push service.

  | Push service answers | kiln |
  |---|---|
  | 201 | delivered: the device's last delivery time is kept |
  | 404 or 410 | the subscription is gone: the device is removed |
  | 429 or 5xx, or no answer | tries again after 2, 10 and 30 s (or as long as `Retry-After` asks, up to 2 minutes) |
  | anything else | not tried again; the error is kept for Settings |

  A note waits at the push service for a device that's away: 24 hours for needs you, 6 hours for
  the rest.

`[push]` in kiln.toml:

| Setting | Default | |
|---|---|---|
| `enabled` | `true` | push to people's devices at all |
| `contact` | the public URL when it's `https://`, else `mailto:kiln@localhost` | who push services may contact about this server (`mailto:` or `https://`). Apple's may refuse a contact it can't use: set a real address if iPhones hear nothing |
| `allow_private` | none | push endpoints allowed on this machine or a private network (a push service of your own, a test's), as `[ci] allow_private_targets` writes them; such an endpoint may be plain `http://` |

## API

Every route needs `agents.run`. Devices are a person's: an agent's token or a job's has none, and
nobody sees or removes another person's.

| Route | |
|---|---|
| `GET /api/push` | `{enabled, available, why, public_key, kinds, muted, reached}`: whether push works here (`why` says why not), the key to subscribe with, the kinds a device can hear, the agents you muted, and whether the server reached the push services when it last looked |
| `GET /api/push/subscriptions` | `{devices}`: your devices, each `{id, endpoint, device, hears, created_at, last_ok_at, last_error, last_error_at}` |
| `POST /api/push/subscriptions` | `{subscription, device, hears?, replaces?}`: a browser's subscription (`PushSubscription.toJSON()`), its name, and what it hears (all kinds when absent). The same endpoint again is the same device. `replaces`: the endpoint it takes over from (its name and kinds go on). At most 20 devices each (409 beyond) |
| `DELETE /api/push/subscriptions/:id` | forgets one of your devices |
| `POST /api/push/test` | `{id?}`: a test to your devices (or one), each tried once: `{sent, results: [{id, device, ok, error?}]}` |
| `PUT /api/push/mutes/:agent` | mutes an agent for you |
| `DELETE /api/push/mutes/:agent` | unmutes it |

The kinds are `agent.needs_you`, `agent.finished`, `agent.stopped`, `agent.paused` and
`machine.battery_low`. A push's data, which the service worker reads, is
`{title, body, tag, url, agent?, kind, at}`.

## The same events elsewhere

Each note is an event too, so Slack, Telegram, Discord, PagerDuty, webhooks and the rest can carry
it (docs/ci.md W19-R06 and R07):

```json
{ "type": "agent.needs_you",
  "agent": { "id": "…", "name": "Mantle", "harness": "claude", "harness_name": "Claude Code",
             "machine": "metal-1", "cwd": "/src/kiln", "opener": "owner" },
  "title": "Mantle", "text": "Asks: which database?", "url": "https://kiln.example/agents/…" }
```

- **Outgoing webhooks** take `agent.needs_you`, `agent.finished`, `agent.stopped` and
  `agent.paused` by name, or with `*`. They're about no repository, so only server-wide
  subscriptions get them.
- **Notification rules**: the server's own (`[[ci.notify]]` in kiln.toml) take them; a
  repository's `.kiln/notify.yaml` can't, since an agent is no repository's. `where` reads `agent`
  (its fields above, and the note's `text`), `throttle` holds per channel, event and agent, and
  channels are the integrations the server may use (scope `server`).

```toml
[[ci.notify]]
on = ["agent.needs_you", "agent.stopped"]
where = "agent.machine == 'metal-1'"
to = ["telegram:-1001234"]
throttle = "5m"
```

PagerDuty opens one incident per agent; finishing resolves it. Deliveries are recorded as the
others are (`GET /api/ci/notify/deliveries`, with `run` set to `agent:<id>`). Muting is for your
devices only: rules and webhooks have their own `where`.
