# The front door on your tailnet (Tailscale)

A runner's front door (Runners → Domain & HTTPS, see [front-door.md](front-door.md)) can serve its
names to every device on your [Tailscale](https://tailscale.com) tailnet, wherever the device is:
at home, on mobile data, on another network. With the runner's DNS on and **Tailnet** switched on:

- the tailnet's DNS sends the front door's names (say `kiln.dev`, every name under it, and the
  aliases) to that runner's DNS;
- the runner's DNS answers devices on the tailnet with its tailnet address (`100.x.y.z`, and its
  `fd7a:…` IPv6 one), so they connect over the tailnet;
- HTTPS answers on the tailnet too, and kiln's certificate for the front door also carries the
  runner's tailnet address and its MagicDNS name (`box.tail1234.ts.net`).

Any runner can be the one: whichever has DNS and Tailnet on. Two runners serving the same name are
both given to the tailnet (Tailscale asks them side by side).

## What you need

1. **Tailscale on the runner's machine**, signed in to your tailnet, with its usual TUN interface
   (not `--tun=userspace-networking`: then nothing on the machine can listen on its tailnet
   address). The runner says so if it isn't.
2. **The runner's DNS on port 53.** Tailscale asks nameservers on port 53 only. On Linux, kiln runs
   as you, so let users bind ports from 53 up, once:

   ```sh
   sudo sysctl -w net.ipv4.ip_unprivileged_port_start=53
   echo 'net.ipv4.ip_unprivileged_port_start = 53' | sudo tee /etc/sysctl.d/99-kiln.conf
   ```

   macOS and Windows need nothing. kiln never binds `0.0.0.0:53` on Linux (systemd-resolved holds
   `127.0.0.53:53`): it binds each address, the tailnet ones included.
3. **The tailnet's DNS pointing the names at the runner.** Either:
   - **Let kiln set it** (once, for every runner): in Tailscale's admin console, **Settings → Trust
     credentials**, create an **OAuth client** with the **DNS** scope (write), and paste its secret
     into the Tailnet group. kiln tries it before keeping it. From then on kiln keeps the tailnet's
     split DNS on whatever its front doors serve, as runners, names and addresses change. An API
     access token (`tskey-api-…`) works too, but it acts as you and expires after 1 to 90 days; an
     OAuth client does not expire.
   - **Or by hand**: admin console → **DNS → Nameservers → Add nameserver → Custom**: the runner's
     tailnet address (the group shows it, with a copy button), turn on **Restrict to domain** with
     the name (one nameserver entry per name), and turn on **Use with exit node**.

That's all. Devices take the tailnet's DNS settings by default ("Use Tailscale DNS settings"; on
Linux `tailscale set --accept-dns=true`). Nothing else changes on them.

For `kiln.dev` and other `.dev` names, browsers insist on a certificate they trust (the whole
`.dev` domain is on the HSTS preload list: there is no "continue anyway"). With kiln's own
certificate, trust kiln's root on the device once (see [front-door.md](front-door.md)); with your
own certificate from a public CA, nothing to do.

## How it works

**On the runner.** Every 30 seconds (and at once when the settings change) the runner asks this
machine's Tailscale about itself with `tailscale status --json` and `tailscale dns status --json`
(neither needs root): its tailnet addresses, MagicDNS name and tailnet, and the split DNS routes the
tailnet gives it. It also asks Tailscale's own resolver, `100.100.100.100`, for the domain — the
way every device on the tailnet asks — so the status says whether the tailnet really finds the
front door, not just whether it should. The tailnet addresses join the front door's addresses: DNS
listens on them, a query that arrives on one is answered with the tailnet addresses (a query on
the LAN address is still answered with the LAN address), and the certificate carries them.

**On the server.** A runner whose DNS listens on its tailnet address on port 53 reports that
address as its nameserver. With a key, the server keeps the tailnet's split DNS saying: each name a
front door with DNS and Tailnet on answers → the nameservers of the runners serving it. It looks
every 5 minutes, and at once when settings change or a runner's nameserver does.

- It uses `GET`/`POST /api/v2/tailnet/-/dns/configuration`, the call that can mark a route
  "use with exit node" (without that mark, a device using an exit node asks the exit node instead,
  for every name). Everything else in the configuration is sent back as it was. A key that may
  not use that call falls back to `PATCH /api/v2/tailnet/-/dns/split-dns`, which changes only the
  names given but cannot set the mark; the Tailnet group says so.
- It changes only what needs changing: a name already right is left alone. A name it set and no
  front door serves any more is removed, unless someone changed it in Tailscale since; then it is
  left as it is and kiln forgets it. A name another nameserver had is pointed at the runner (you
  chose the runner as that name's DNS) and the audit log says what it replaced.
- A name whose runner has not reported where its DNS listens (after a server restart, or with
  Tailscale stopped on it) is left exactly as it is, so a working route never goes down because a
  runner is away for a moment.
- Every change is in the audit log (`tailnet.dns`).

The key is kept in the server's data directory (`tailscale-api.json`, mode 0600), never sent back
to the browser and never logged. OAuth access tokens (an hour each) are fetched when needed.

## The API

| Method | Path | |
|---|---|---|
| `GET` | `/api/tailscale` | whether a key is set (`kind`, `id`: the OAuth client ID or the token's start), what the tailnet should say (`want`), what it says for kiln's names (`routes`), what is settling (`pending`), the last sync (`synced_at`, `ok`, `error`), and `exit_nodes` |
| `PUT` | `/api/tailscale` | `{"secret": "tskey-client-…", "client_id": "…"}` (or `{"secret": "tskey-api-…"}`): tried with Tailscale, kept, synced at once. 400 says why a key was refused |
| `DELETE` | `/api/tailscale` | forget the key; the routes kiln set stay as they are |

A runner's `GET /api/runners/:name/front-door` has `settings.tailscale.enabled` and, while it is on,
`status.tailscale`: `state`, `error`, `fix`, `name` (MagicDNS), `tailnet`, `addresses`,
`nameserver`, `accept_dns`, `routes` (per name: the route deciding it, where it goes, `here`,
`exit_node`), `resolved` (the domain through `100.100.100.100`) and Tailscale's own `health`.

## When it doesn't work

| The Tailnet line says | |
|---|---|
| Tailscale is not installed on this machine | Install it on the runner's machine and sign in. |
| Tailscale is NeedsLogin / Stopped on this machine | `sudo tailscale up` (or connect in the Tailscale app). |
| userspace networking | Run `tailscaled` with its TUN interface (the default). |
| Tailscale asks a nameserver on port 53 | Set the DNS port to 53 (and the sysctl above on Linux). |
| DNS is not listening on 100.x.y.z:53 yet | The DNS line says why (usually the sysctl). |
| the tailnet does not send kiln.dev here yet | Connect a key, or add the nameserver by hand. |
| … goes to 10.0.0.53, not here | Another nameserver has the name in Tailscale: kiln replaces it once a key is connected, or edit it by hand. |
| Tailscale's resolver: no route for it (SERVFAIL) | The tailnet has no route for the name yet (Tailscale may take a few seconds to pass a new one on). |

- **One device doesn't find it.** It must use Tailscale's DNS settings: in the app, "Use Tailscale
  DNS settings"; on Linux, `tailscale set --accept-dns=true`. On Linux, `tailscale status` warns
  when systemd-resolved and NetworkManager are wired together in a way that keeps Tailscale's DNS
  from working on that machine (see Tailscale's note on it); other devices are not affected.
- **Only with an exit node it fails.** The route lacks "Use with exit node": with a key that can
  use the configuration call kiln sets it; otherwise set it on the nameserver in the admin console.
- **A tight tailnet policy.** Devices need access to the runner's TCP and UDP port 53 and TCP 443
  (and 80, if the redirect is on). The default policy allows it.
- **The firewall.** On Linux, tailscaled (managing the firewall, its default) puts its own chain
  first in `INPUT`, accepting what arrives on `tailscale0` before ufw's rules see it; so only the
  LAN needs ufw rules (the front door's setup lists them).
