# Peered clusters

A kiln cluster can open other kiln clusters through itself: pick one in the header's cluster
picker and its UI fills the page, in a frame with nothing of its own (no border, no background).
There is one header at a time: the opened cluster's page tells ours it's kiln, is told our
clusters, and shows the same picker in its own header, where picking one switches back (ours steps
aside meanwhile; a cluster too old to say so keeps ours above it). Each frame stays loaded once
opened, so flipping back finds it where it was. A cluster is named for the host of its public
address (else its machine's name). This works however you reach your cluster: any
runner's front door, or the server's own address.

Peering is not joining. The other cluster keeps its own runners, accounts and settings; ours only
forwards to it. It is authorized by hand, with codes an admin carries between the two, so it needs
nothing from the other cluster's network: **it never has to reach ours.** Ours has to reach it
(its front door, or its server's address), from the server or from the runner your browser came
in through.

## Peering, step by step

1. **Here: register it.** *Clusters* (Settings → Clusters, or the picker's *Manage clusters…*) →
   *Peer with a cluster*: its name, and its address as this cluster reaches it. You get a
   **challenge** (`kiln_pc1_…`), good for 30 minutes.
2. **There: let us in.** Its admin opens *Clusters* → *Let a cluster in*, pastes the challenge, and
   picks the most our people may do there: viewer, developer, maintainer or admin. They get a
   **response** (`kiln_pr1_…`) to give back, and see our key's fingerprint, which they can compare
   with ours (*This cluster* on our Clusters page).
3. **Here: peer.** Paste the response under the cluster in *Clusters*. It is checked against our
   challenge: it answers that one alone, and its signature proves the key it carries. From now on
   the cluster is in the picker, for everyone signed in here with a role on the whole cluster.

Peering goes one way. For the other cluster's people to open ours, do the same steps the other way
round. Either side ends its half at any time (*Remove*); nothing passes once either has.

## What the codes carry

| Code | Made by | Carries |
|---|---|---|
| challenge `kiln_pc1_…` | the cluster that wants to open the other | its name, its Ed25519 public key, a nonce, until when it can be answered, its address (shown only) |
| response `kiln_pr1_…` | the cluster being opened | its name, its public key, the role it gave, its CA certificate, and its signature over all of that, the challenger's key and the challenge's nonce |

Both are base64url JSON behind their prefix, so whitespace and line breaks a copy picks up don't
matter. Nothing in either is secret. Each cluster's key is made once by its server and kept in
`<data dir>/keys/peering.p8` (mode 0600); its id is the first 8 bytes of the public key's
SHA-256, and its fingerprint the first 16, in groups of four.

The response's CA is what the opened cluster's HTTPS certificates chain to (a front door's comes
from kiln's CA). Ours trusts it for that cluster's address alone, beside the public roots: no
certificate is accepted on first use, and nothing is fetched to decide.

## Opening a peered cluster

- **A grant.** Picking a cluster asks our server for a grant (`POST /api/peers/:id/frame`): who
  you are here, the most you may do there (your role on our whole cluster, at most admin), which
  cluster it's for, until when (8 hours), signed with our key. Opening a cluster again close to
  the end gets a new one.
- **Its address.** Every cluster we open has an address of its own here, so its UI runs unchanged
  at the root of an origin of its own: `peer-<name>.<domain>` for a runner's front-door domain
  (every front door answers its domain's wildcard, in DNS and in its certificate: the same site
  as the page, so the frame keeps its cookie), and otherwise a port of its own on the server (the
  HTTP port + 100, + 101, …) on the host the page is at, for kiln reached by address or at
  `localhost`. (The server also answers `peer-<name>.localhost` by name, for testing; browsers take
  each `*.localhost` name for a site of its own, so the picker doesn't use it.)
- **Entering.** The frame starts with the grant posted to `/.kiln-peer/enter` at that address (a
  form post, so it's in no address or history): it becomes that address's cookie
  (`kiln_peer_<name>`, HTTP-only), and the frame goes to the cluster's home page.
- **Forwarding.** Whatever answers that address forwards each request to the cluster, from its own
  machine: a runner's front door for its domain's names, the server for the rest (and for a front
  door too old to do it, which passes everything to the server). Requests go with what they carried
  except our cookies and credentials, the grant in their place as the other cluster's session, and
  `Host`, `Origin` and `Referer` naming its own address. Bodies stream both ways; a WebSocket's
  upgrade is asked of the cluster first, then the two connections are piped. The cluster's own
  cookies are dropped on the way back, and its redirects to its own address stay in the frame.
  Outbound connections go through the machine's proxy when its network wants one.
- **There.** The opened cluster checks the grant with the key from our challenge, by itself: it is
  ours, for it, and not expired. Its bearer gets the lower of the role it gave us and the role the
  grant carries, on its whole server, as `peer:<our name>:<who>` in its audit log.

## API

| Method | Path | |
|---|---|---|
| GET | `/api/peers` | this cluster's name, fingerprint, the roles, and every peering (admin) |
| POST | `/api/peers` | `{name, url}`: register a cluster to open; `{peer, challenge}` (admin) |
| POST | `/api/peers/:id/challenge` | a new challenge for a peering still waiting; the last stops working (admin) |
| POST | `/api/peers/:id/complete` | `{response}`: peer (admin) |
| POST | `/api/peers/authorize` | `{challenge, role}`: let the challenger in; `{peer, response}` (admin) |
| PUT | `/api/peers/:id` | `{url}`: change where a cluster we open is reached (its key is what's trusted: no new codes) (admin) |
| DELETE | `/api/peers/:id` | end a peering, either way (admin) |
| GET | `/api/peers/open` | the clusters someone signed in may open (the picker) |
| POST | `/api/peers/:id/frame` | `{origin, action, grant, expires_at}` for the person asking |

Peerings are kept in the server's store (`peers:<id>`) and sent to every runner with its front
door's settings (`Push::peers`): the clusters to forward to, and our public key to check grants
with. A runner holds no secret.

## Limits

- Opening a cluster takes a role on the whole of ours: a grant carries one role, for the whole of
  the other cluster.
- A front door's `.local` name and a tailnet's MagicDNS name have no wildcard: from there, and from
  an address on HTTPS, a peered cluster opens at the server's address (by its port) only over
  plain HTTP, else at a front-door domain.
- A grant is like a session cookie: whoever holds it opens that cluster as you until it expires.
  Between the clusters, use HTTPS (the other cluster's front door) where the network isn't yours.
- Each forwarded request opens its own connection to the other cluster.
