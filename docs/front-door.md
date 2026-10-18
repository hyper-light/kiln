# The front door: kiln under a name of your own

A runner's **front door** makes that machine kiln's address on your network. Its DNS answers a
name you choose, such as `kiln.dev`, and HTTPS there serves kiln. Each runner has its own
settings, on or off (Runners → Domain & HTTPS), and only the runners you turn on run anything:
one runner per network is enough. Phones need HTTPS for voice input, and this is how they get it.

The server stores the settings and pushes them to the runner, which runs the listeners itself.
The runner also keeps a copy, so it serves at once after a restart, even while the server is away.

## Any name

The domain can be any name you choose: `kiln.dev`, `kiln.home`, `ci.example.com`,
`kiln.internal`. kiln answers that name and every name under it (`*.kiln.dev`). Aliases add more
names for the same front door (`kiln.home` beside `kiln.dev`).

A name is letters, digits and hyphens between dots, and it needs at least one dot. A few names
can't be used: IP addresses, names under `.local` (they belong to multicast DNS; see
[the `.local` name](#the-local-name)), `localhost`, and `kiln-server`, the name runners check the
server by.

The runner answers the name for every device that asks it, whoever owns the name on the internet:
if someone else has a site under it, devices that use this DNS reach your kiln instead. A name you
own, or one under `.internal` or `home.arpa`, hides nothing.

## DNS

The runner answers A and AAAA for the domain, `*.<domain>` and the aliases. By default it answers
with its own address on the network the question came from, so phones on the Wi-Fi get the Wi-Fi
address. To answer with fixed addresses instead, set **Answer with**. Other record types get an
empty answer at once (HTTPS records too, which browsers ask for). Answers live 60 seconds.

It runs in one of two modes:

| Mode | What it does | How devices reach it |
| --- | --- | --- |
| **Resolver** | answers your names and forwards every other question to the runner's own resolvers (or the **Upstream resolvers** you list) | devices use the runner as their DNS server: your router's DHCP hands it out, or you set it on each device |
| **Authoritative** | answers your names and refuses everything else | your router keeps answering for the network and forwards just your domain to the runner (*conditional forwarding*) |

A resolver forwards only for devices on private networks or the runner's own subnets, so it is
never an open resolver for the internet. Resolver mode makes the runner every device's DNS, and the
whole network loses DNS while that runner is down. Authoritative mode with conditional forwarding
leaves your router in charge, so it is the sturdier choice when your router can forward a domain.

On Linux the runner listens on each of its LAN addresses and on loopback, never on the wildcard
address, because systemd-resolved holds `127.0.0.53:53`. On macOS and Windows it listens on the
wildcard address. DNS works over UDP and TCP.

## HTTPS

HTTPS listens on port 443 (TLS 1.2 and 1.3) for the domain, its aliases and the `.local` name. It
passes every connection's bytes through to the kiln server, so the UI, its WebSockets and
streamed logs work as they do on the server's own port. It forwards to the runner's own
`--server` URL unless **Forward to** says otherwise. An `https://` upstream is checked against
kiln's root and the public roots.

Browsers get HTTP/2 while the upstream speaks it: one connection carries all of a page's
requests at once, where HTTP/1.1 takes six at a time. The front door asks the upstream every 30
seconds, and at once when the upstream changes. A kiln server from before HTTP/2 answers no, and
browsers get HTTP/1.1 as before. WebSockets always use HTTP/1.1 connections of their own.

Plain HTTP on port 80 answers every request with a redirect (307) to the same path over HTTPS,
keeping the name or address that was asked when it is one of the front door's (else the domain).
Three things it serves itself, because a device needs them before it trusts the HTTPS: kiln's root
at `/ca.mobileconfig`, `/ca.crt` and `/ca.pem`, and, opened by address (`http://100.107.111.49`),
a page saying how to trust that root, with a link on to the domain.
Set the port to 0 for no redirect. The server knows each front door's names: a sign-in through one gets a `Secure` cookie,
and `https://kiln.dev` counts as one of the server's own origins.

## Certificates

Each runner's HTTPS has its own certificate, of one of two kinds:

* **Self-signed: kiln makes, installs and renews it.** The runner makes a key and sends only a
  certificate request; the key never leaves the machine. kiln's own root (the kiln CA, made
  when the server first started) signs a certificate for the front door's names and the runner's
  LAN IPv4 addresses, valid for a year. The runner renews it 30 days before it ends, and at
  once when its names change. Every front-door certificate on every runner comes from the same
  root, so a device that trusts the root once trusts all of them, renewals included.
* **Your own.** Upload a certificate chain and its key, pasted or read from files. The server
  checks them first: they must parse, the key must be the certificate's, it must be for TLS
  servers (serverAuth), and it must not have expired. Then it hands them to the runner. The
  runner keeps them (mode 0600). The server keeps neither, logs neither and never sends the key
  back. A key pasted into the certificate field is kept with the key, not the certificate. To
  replace the certificate, upload a new one.

Under **Certificate**, Runners → Domain & HTTPS shows the certificate in use on the runner: its
names, when it is valid from and until, its issuer, and its SHA-256 fingerprint. Next to it are
the actions, each done in place:

* **Renew now** (self-signed): a new key and a new certificate, at once. The runner keeps
  serving the old certificate until the new one is in, then replaces both, usually within a
  second.
* **Download** the certificate chain it serves, as PEM. This holds the certificates only, never
  the key.
* **Upload a new one** (your own).
* Switching between the two kinds is the select and Save. A certificate you uploaded stays on
  the runner, so switching back to your own serves it again.

The status lines say what is wrong with the certificate in use: names the front door serves that
it does not cover, or fewer than 30 days left. For a self-signed certificate, kiln is already
renewing it.

### Trusting it on your devices

A self-signed certificate is trusted once the device trusts kiln's root. Under the certificate,
**Trust it on your devices** has the root's fingerprint (check it on the device) and these files:
`/ca.mobileconfig` (an Apple profile), `/ca.crt` (DER) and `/ca.pem`. They are served without
signing in, since a root certificate is public.

On a phone, the easiest way is the runner itself. With HTTPS and its redirect on, **Trust it on
your devices** lists the runner's plain-HTTP addresses, such as `http://100.107.111.49` (on your
tailnet) and `http://192.168.1.77` (on the LAN). Open one in Safari: the page there installs the
profile and lists what to tap. By name it couldn't: a `.dev` name opens only over HTTPS, and only once the
device trusts the root.

| Device | Steps |
| --- | --- |
| iPhone, iPad | 1. In Safari (Chrome can't install profiles), open the runner's address or `/ca.mobileconfig` and allow it. 2. Settings → Profile Downloaded → Install. "Not Signed" is expected: the profile is kiln's own. 3. Settings → General → About → Certificate Trust Settings: turn on kiln CA. If Safari still can't find the name, iCloud Private Relay is sending its lookups past your network: use Chrome for it, or turn off Limit IP Address Tracking for that network (Settings → Wi-Fi → ⓘ). |
| Android | Download `/ca.crt`, then Settings → Security & privacy → More security settings → Encryption & credentials → Install a certificate → CA certificate. |
| macOS | Open `/ca.mobileconfig`, then System Settings → General → Device Management (Privacy & Security → Profiles before macOS 14) → kiln CA → Install (or `sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain kiln-ca.pem`). |
| Windows | Open `/ca.crt` → Install Certificate → Local Machine → Trusted Root Certification Authorities (or, as administrator, `certutil -addstore -f Root kiln-ca.crt`). |
| Linux | `sudo trust anchor --store kiln-ca.pem` (Arch, Fedora), or copy it to `/usr/local/share/ca-certificates/kiln-ca.crt` and run `sudo update-ca-certificates` (Debian, Ubuntu). Firefox keeps its own list: Settings → Certificates → Import. |

### `.dev` and other HSTS-preloaded domains

`.dev` is on the HSTS preload list that every major browser ships. Browsers load every name under
it over HTTPS only, and they never let anyone click through a certificate warning there. So for
`kiln.dev` to open in a browser, the device must trust the certificate's issuer: kiln's root for a
self-signed certificate. Trust the root on each device first. The same holds for `.app`, `.page`
and other Google-run top-level domains. Under a name like `kiln.home`, an untrusted certificate
shows a warning that can be clicked through. Even so, trusting the root is what makes the
warning go away.

## The `.local` name

As an extra, the runner can also answer `<name>.local` (`kiln.local`) with multicast DNS: nothing
to set up on the router or the devices. It works for devices on the same network segment that
resolve `.local` names (Apple devices do, and most desktops). HTTPS serves that name too, and a
self-signed certificate includes it. Your own domain is the main way in. `.local` is for networks
whose DNS you can't change.

## Two runner processes on one machine

While a machine runs a second runner process beside its first (an upgrade that keeps the first
one's terminals alive), the newest runs the front door. The server tells the others to stand by,
so they release the ports. The newest may try the ports before the old one has let go of them, so
for 30 seconds after it starts or is told to run the front door, it tries ports still in use every
250 ms, not only every 15 seconds: it takes them within a quarter of a second of the old one
letting go.

## One-time setup on a Linux runner

The runner runs as you, not as root. The front door shows what is missing in its status lines,
with the commands, and never runs them itself. For a Linux runner serving `kiln.dev` on ports 53
and 443 (and 80 for the redirect), do this once. The examples use the runner's LAN address
`192.168.1.77` and the LAN `192.168.1.0/24`: use yours.

**1. Let kiln bind ports below 1024** (now, and after every reboot):

```sh
sudo sysctl -w net.ipv4.ip_unprivileged_port_start=53
echo 'net.ipv4.ip_unprivileged_port_start = 53' | sudo tee /etc/sysctl.d/99-kiln.conf
```

This lets any user bind ports from 53 up. kiln doesn't use `setcap`, because an upgrade replaces
the binary and the capability with it.

**2. Let the LAN through ufw** (if ufw is on):

```sh
sudo ufw allow from 192.168.1.0/24 to any port 53,80,443 proto tcp
sudo ufw allow from 192.168.1.0/24 to any port 53 proto udp
```

**3. Turn it on** in Runners → Domain & HTTPS: pick the runner, set Domain to `kiln.dev`, turn
on DNS (Resolver or Authoritative, port 53), turn on HTTPS (443, redirect 80) with
Certificate set to Self-signed, then Save. Within a few seconds the status lines show DNS, HTTPS
and the redirect on, and the certificate in use.

**4. Point your network's DNS at it**, in one of three ways:

* *Resolver mode, the whole network:* in your router's DHCP (LAN) settings, set the DNS server it
  hands out to `192.168.1.77`, alone: with a second server beside it, devices sometimes ask the
  other one. Devices pick it up when they rejoin the Wi-Fi. On OpenWrt:

  ```sh
  uci add_list dhcp.lan.dhcp_option='6,192.168.1.77'
  uci commit dhcp
  /etc/init.d/dnsmasq restart
  ```

  If the router also hands out an IPv6 DNS server, devices may ask that one instead: point it at
  the runner too, or use conditional forwarding.
* *Resolver mode, one device:* set its DNS server to `192.168.1.77`.
  * iPhone, iPad: Settings → Wi-Fi → (i) → Configure DNS → Manual, then add `192.168.1.77` and
    remove the others.
  * macOS: `networksetup -setdnsservers Wi-Fi 192.168.1.77`
  * Windows (PowerShell as administrator):
    `Set-DnsClientServerAddress -InterfaceAlias "Wi-Fi" -ServerAddresses 192.168.1.77`
  * Linux with NetworkManager:
    `nmcli connection modify "<connection>" ipv4.dns 192.168.1.77 ipv4.ignore-auto-dns yes`,
    then `nmcli connection up "<connection>"`
  * Android takes a DNS server only with static IP settings (Wi-Fi network → IP settings: Static
    → DNS 1), so the router's DHCP is easier there.
* *Authoritative mode, conditional forwarding:* the router keeps answering everything and forwards
  only `kiln.dev` to the runner.
  * dnsmasq or Pi-hole: add `server=/kiln.dev/192.168.1.77` and `rebind-domain-ok=/kiln.dev/` (the
    second lets the router pass on a private address for that name).
  * OpenWrt:

    ```sh
    uci add_list dhcp.@dnsmasq[0].server='/kiln.dev/192.168.1.77'
    uci add_list dhcp.@dnsmasq[0].rebind_domain='kiln.dev'
    uci commit dhcp
    /etc/init.d/dnsmasq restart
    ```
  * Unbound (OPNsense and pfSense call this Query Forwarding or Domain Overrides):

    ```
    server:
      private-domain: "kiln.dev"
      domain-insecure: "kiln.dev"
    forward-zone:
      name: "kiln.dev"
      forward-addr: 192.168.1.77
    ```

    `.dev` is signed with DNSSEC, so a validating resolver rejects the runner's unsigned answers
    unless the name is marked insecure.

**5. Trust the certificate on each device** that opens `kiln.dev`. On a phone, open
`http://192.168.1.77` (the runner's address; on the tailnet, its `100.x` one) in Safari and follow
the page. Elsewhere, see [Trusting it on your devices](#trusting-it-on-your-devices). For `.dev`
this step is required: the browser offers no way past the warning.

**Check it** from any device on the LAN:

```sh
dig @192.168.1.77 kiln.dev +short          # 192.168.1.77
curl --cacert kiln-ca.pem https://kiln.dev/healthz   # ok
```

A browser that resolves names over DNS-over-HTTPS skips your network's DNS for them. Firefox
does this by default in some countries: Settings → Privacy & Security → DNS over HTTPS → Manage
Exceptions → add `kiln.dev`, or turn it off. In Chrome it is Settings → Privacy and security →
Security → Use secure DNS.

## API

The routes are in [api.md](api.md): `GET /api/front-doors`, `GET` and `PUT
/api/runners/:name/front-door`, `POST` and `GET /api/runners/:name/front-door/cert`, `POST
/api/runners/:name/front-door/cert/renew`, and the open `/ca.pem`, `/ca.crt` and
`/ca.mobileconfig`. Changes are audited (`front_door.update`, `front_door.cert`,
`front_door.renew`, and `front_door.certificate` each time kiln's root signs one).

## What the runner keeps

Under `<state>/front-door/`:

| File | What | Mode |
| --- | --- | --- |
| `settings.json` | the settings last pushed, so it serves at once after a restart | 0644 |
| `kiln.key`, `kiln.pem` | the self-signed certificate in use and its key | 0600 |
| `kiln.key.pending` | the key of a certificate request kiln's root has not answered yet | 0600 |
| `custom.key`, `custom.pem` | your own certificate (its chain's certificates only) and its key | 0600 |

The settings travel as JSON in `ServerMsg::FrontDoor`, and the runner's reports in
`AgentMsg::FrontDoorStatus`, so either side can add fields. A runner announces it can run a front
door with `CAP_FRONT_DOOR` in its `AgentMsg::Build`. Until that arrives (moments after it
connects), the server reports whether it can as unknown.
