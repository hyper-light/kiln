#!/bin/bash
# Goal 3 step 3's lab (plan 01a1a832eb2f711f9c6f51d88effe89d): the protocol's socket under a flood, on a
# link of 50 kbit/s each way, 300 ms round trip (150 ms each way) and a deep queue (netem's limit of 17
# full packets each way: about 4 s, the lab's 50k profile's). A lab kiln ([server] live, TLS) in a
# namespace of its own, a page (examples/live_lab.rs, its TLS keys logged) in another, joined by a
# veth pair with offloads off; both rootless (unshare -rn). The flood: a notice marked every second for
# SECS seconds (150 random keys, about 46 KB of the owner's events topic each, 7x the link), posted on
# the server's own address (within its namespace, never over the link). Meanwhile:
#   diag.txt      the kernel's numbers of the page's socket every 10 ms (diag.py: sock_diag's TCP_INFO);
#   sockets.jsonl the server's numbers of it every second (GET /api/live/sockets: mark, chunk, window,
#                 what each layer below the scheduler held, the scheduler's queue by priority);
#   capture.pcap  every byte on the page's side (capture.py, whole frames), keys.log the page's keys.
# Then analyze.py says the step's three measures from them (summary.json).
#   flood.sh KILN LIVE_LAB OUT [SECS]
set -u
K=$1; L=$2; O=$3; SECS=${4:-60}
H=$(cd "$(dirname "$0")" && pwd); WEB=$(cd "$H/../.." && pwd)
if [ -z "${BP_G3_INNER:-}" ]; then
  rm -rf "$O"; mkdir -p "$O"
  exec env BP_G3_INNER=1 unshare -rn --kill-child bash "$0" "$@"
fi
say() { printf '%s %s\n' "$(date -u +%T)" "$*"; }
PIDS=()
end() { for p in "${PIDS[@]}"; do kill -TERM "$p" 2>/dev/null; done; sleep 1; for p in "${PIDS[@]}"; do kill -KILL "$p" 2>/dev/null; done; }
trap end EXIT

# The network: this namespace is the server's (10.0.0.1), a child's the page's (10.0.0.2).
ip link set lo up
unshare -n sleep infinity & C=$!; PIDS+=("$C"); sleep 0.3
in_c() { nsenter -t "$C" -n "$@"; }
ip link add s0 type veth peer name c0 netns "$C" || exit 2
ip addr add 10.0.0.1/24 dev s0; ip link set s0 up
in_c ip link set lo up; in_c ip addr add 10.0.0.2/24 dev c0; in_c ip link set c0 up
ethtool -K s0 tso off gso off gro off tx off rx off >/dev/null 2>&1
in_c ethtool -K c0 tso off gso off gro off tx off rx off >/dev/null 2>&1
NETEM="netem limit 17 delay 150ms rate 50kbit"
tc qdisc replace dev s0 root $NETEM || exit 2
in_c tc qdisc replace dev c0 root $NETEM || exit 2
say "link: $(tc qdisc show dev s0 | head -1); page's: $(in_c tc qdisc show dev c0 | head -1); kernel $(uname -r)"

# The lab kiln.
mkdir -p "$O/certs" "$O/data" "$O/home"
( cd "$O/certs" && {
  openssl ecparam -name prime256v1 -genkey -noout -out ca.key
  openssl req -x509 -new -key ca.key -sha256 -days 30 -subj '/CN=kiln bp-g3 lab CA' -addext 'basicConstraints=critical,CA:TRUE' -addext 'keyUsage=critical,keyCertSign' -out ca.pem
  openssl ecparam -name prime256v1 -genkey -noout -out leaf.ec
  openssl pkcs8 -topk8 -nocrypt -in leaf.ec -out leaf.key
  openssl req -new -key leaf.key -subj '/CN=10.0.0.1' -out leaf.csr
  printf 'subjectAltName=IP:10.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n' > leaf.ext
  openssl x509 -req -in leaf.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 7 -sha256 -extfile leaf.ext -out leaf.pem
  cat leaf.pem ca.pem > chain.pem; } >/dev/null 2>&1 ) || exit 3
cat > "$O/kiln.toml" <<EOF
[server]
http_addr = "0.0.0.0:8443"
quic_addr = "127.0.0.1:8444"
public_url = "https://10.0.0.1:8443"
quic_advertise = "127.0.0.1:8444"
data_dir = "$O/data"
https = true
https_cert = "$O/certs/chain.pem"
https_key = "$O/certs/leaf.key"
live = true

[gitops]
auto_pipelines = false

[local_agent]
enabled = false

[terminal]
enabled = false

[kubernetes]
enabled = false

[discovery]
enabled = false
announce = false
browse = false
join_requests = false
tailscale = false

[cd]
enabled = false

[ci]
enabled = false
EOF
HOME="$O/home" KILN_MACHINE_ID="bp-g3 lab $O" NO_COLOR=1 "$K" --config "$O/kiln.toml" server > "$O/server.log" 2>&1 & PIDS+=("$!")
for _ in $(seq 240); do [ -s "$O/data/admin.token" ] && curl -s -o /dev/null --cacert "$O/certs/ca.pem" https://10.0.0.1:8443/healthz && break; sleep 0.5; done
[ -s "$O/data/admin.token" ] || { say "the lab kiln didn't start"; tail -20 "$O/server.log"; exit 4; }
# The admin token goes into a header file and the page's token file, never to the output.
printf 'Authorization: Bearer %s\n' "$(cat "$O/data/admin.token")" > "$O/auth.hdr"; chmod 600 "$O/auth.hdr"
# On the server's own address: within its namespace, never over the link.
api() { curl -s --cacert "$O/certs/ca.pem" -H @"$O/auth.hdr" "$@"; }

# The flood's notices, each of random keys (so the topic's compression can't make them small).
python3 - "$O/notices" "$SECS" <<'PY'
import base64, json, os, sys
d, n = sys.argv[1], int(sys.argv[2])
os.makedirs(d, exist_ok=True)
for i in range(n):
    keys = [f"{i:04}-{j:03}-" + base64.urlsafe_b64encode(os.urandom(216)).decode() for j in range(150)]
    open(f"{d}/{i:04}.json", "w").write(json.dumps({"keys": keys, "how": "seen"}))
PY

# What watches: the kernel's numbers every 10 ms, the capture, then the page.
python3 "$H/diag.py" 8443 10.0.0.2 10 "$O/diag.txt" & PIDS+=("$!")
in_c python3 "$WEB/capture.py" c0 "$O/capture.pcap" 65535 & PIDS+=("$!")
sleep 0.5
api -X POST https://10.0.0.1:8443/api/live/ticket > "$O/ticket.json"
in_c env SSLKEYLOGFILE="$O/keys.log" "$L" client https://10.0.0.1:8443 "$O/certs/ca.pem" "$O/data/admin.token" "$O/ticket.json" $((SECS + 30)) "$O/client.json" > "$O/client.log" 2>&1 & PAGE=$!
watch_sockets() { while :; do printf '{"t_us":%s,"v":%s}\n' "$(date +%s%6N)" "$(api https://10.0.0.1:8443/api/live/sockets)"; sleep 1; done >> "$O/sockets.jsonl"; }
watch_sockets & PIDS+=("$!")
for _ in $(seq 60); do grep -q '"id"' "$O/sockets.jsonl" 2>/dev/null && break; sleep 0.5; done
sleep 5
say "the page's socket: $(tail -1 "$O/sockets.jsonl" | cut -c1-300)"

# The flood.
printf '%s\n' "$(date +%s%6N)" > "$O/flood.times"
for f in "$O"/notices/*.json; do
  api -o /dev/null -w '%{http_code}\n' -X POST -H 'content-type: application/json' --data-binary @"$f" https://10.0.0.1:8443/api/push/notices >> "$O/posts.txt"
  sleep 1
done
printf '%s\n' "$(date +%s%6N)" >> "$O/flood.times"
say "flood done: $(sort "$O/posts.txt" | uniq -c | tr '\n' ' ')"
wait "$PAGE"; say "page: $(tr -d '\n ' < "$O/client.json" 2>/dev/null | cut -c1-300) $(tail -3 "$O/client.log")"
api https://10.0.0.1:8443/api/live/sockets > "$O/sockets.end.json"
api https://10.0.0.1:8443/metrics | grep '^kiln_proto_' > "$O/metrics.txt"
end; trap - EXIT
CHUNK=$(python3 -c 'import json,sys; print(max([s["chunk"] for l in open(sys.argv[1]) for s in json.loads(l)["v"].get("sockets",[])] or [0]))' "$O/sockets.jsonl")
"$L" records "$O/capture.pcap" "$O/keys.log" 8443 "$CHUNK" > "$O/records.json"
python3 "$H/analyze.py" "$O" > "$O/summary.json"
cat "$O/summary.json"
