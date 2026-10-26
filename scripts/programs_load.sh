#!/usr/bin/env bash
# The Programs view's first load against a real kiln server (CP-M3; docs/programs.md, "Tests"): a
# release build of this checkout, serving its production UI build (ui/dist/client, embedded at
# build time: `KILN_UI_SERVER=off npm run --prefix ui build` first), run as a scratch server of its
# own (a temporary config, ports and state folder under <out>), seeded through its API with a
# program of 10 tracks and 120 goals (scripts/programs_seed.py); then the Playwright test's p95 of 20
# cold loads, each in a fresh browser context, with the commit and the machine's load beside it.
# Its agents run a stub harness that exits at once, so no real harness runs. The server is stopped
# by the PID it was started with. Its admin token stays in its state folder and the environment of
# what needs it: it's never printed.
#
#   scripts/programs_load.sh <out folder>     (CARGO_TARGET_DIR, -j: as cargo's; JOBS=12 by default)
set -o pipefail
out=${1:?the folder its server, logs and summary go in}
here=$(git rev-parse --show-toplevel) || exit 1
cd "$here" || exit 1
mkdir -p "$out"; S=$out/summary.txt; : > "$S"
echo "commit $(git rev-parse HEAD), $(git status --porcelain | wc -l) uncommitted" >> "$S"
[ -f ui/dist/client/_shell.html ] || { echo "no UI build in ui/dist/client (_shell.html)" >> "$S"; echo DONE >> "$S"; exit 1; }
nice -n 10 cargo build --release --locked -j "${JOBS:-12}" -p kiln > "$out/build.log" 2>&1; echo "release build exit=$?" >> "$S"
BIN=${CARGO_TARGET_DIR:-$here/target}/release/kiln
[ -x "$BIN" ] || { echo "no release binary at $BIN" >> "$S"; echo DONE >> "$S"; exit 1; }
D=$out/server; [ -d "$D" ] && rm -rf "${D:?}"; mkdir -p "$D/data" "$D/work" "$D/home" "$D/bin"
printf '#!/bin/sh\nexit 1\n' > "$D/bin/claude"; chmod +x "$D/bin/claude"
free_port() { python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
port=$(free_port); quic=$(free_port)
cat > "$D/kiln.toml" <<EOF
[server]
http_addr = "127.0.0.1:$port"
quic_addr = "127.0.0.1:$quic"
public_url = "http://127.0.0.1:$port"
quic_advertise = "127.0.0.1:$quic"
data_dir = "$D/data"
https = false

[gitops]
auto_pipelines = false

[local_agent]
enabled = false

[kubernetes]
enabled = false

[terminal]
enabled = true

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
# Nothing of the machine's own kiln: its home, its state and its config are the scratch server's.
env -i PATH="$D/bin:$PATH" HOME="$D/home" NO_COLOR=1 TMPDIR="${TMPDIR:-/tmp}" "$BIN" --config "$D/kiln.toml" server > "$D/server.log" 2>&1 &
PID=$!; echo "$PID" > "$D/server.pid"; echo "server: pid $PID, port $port, state $D" >> "$S"
stop() {
    kill "$PID" 2> /dev/null
    for _ in $(seq 1 30); do kill -0 "$PID" 2> /dev/null || return 0; sleep 1; done
    kill -9 "$PID" 2> /dev/null
}
for _ in $(seq 1 120); do [ -s "$D/data/admin.token" ] && curl -sf "http://127.0.0.1:$port/healthz" > /dev/null && break; sleep 1; done
curl -sf "http://127.0.0.1:$port/healthz" > /dev/null || { echo "the server didn't start (server.log)" >> "$S"; stop; echo DONE >> "$S"; exit 1; }
python3 scripts/programs_seed.py "$port" "$D/data/admin.token" "$D/work" > "$out/seed.log" 2>&1; echo "seed exit=$? $(tail -2 "$out/seed.log" | head -1)" >> "$S"
PROGRAM=$(tail -1 "$out/seed.log")
# The server compresses its assets once, in the background, as it starts (crate::ui), so a server
# that's been up a minute has every one ready: each is asked for once here, both ways, so no timed
# load waits for its compression.
for f in ui/dist/client/assets/*; do
    for coding in br gzip; do curl -s -o /dev/null -H "Accept-Encoding: $coding" "http://127.0.0.1:$port/assets/$(basename "$f")"; done
done
echo "assets warmed: $(ls ui/dist/client/assets | wc -l), the biggest now in $(curl -s -o /dev/null -H 'Accept-Encoding: br' -w '%{time_total}' "http://127.0.0.1:$port/assets/$(ls -S ui/dist/client/assets | head -1)") s" >> "$S"
echo "machine: $(nproc) cores ($(lscpu 2> /dev/null | sed -n 's/^Model name: *//p')), $(free -g 2> /dev/null | awk '/^Mem:/ {print $7" GB of "$2" GB free"}'), load $(cut -d' ' -f1-3 /proc/loadavg 2> /dev/null) before the loads" >> "$S"
(cd ui && E2E_BASE_URL="http://127.0.0.1:$port" E2E_TOKEN="$(cat "$D/data/admin.token")" E2E_PROGRAM="$PROGRAM" E2E_OUT="$out/e2e" nice -n 10 npx playwright test -g "first load" --project desktop > "$out/e2e.log" 2>&1)
echo "e2e exit=$? $(grep -E 'first loads' "$out/e2e.log" | sed 's/.*first loads/first loads/')" >> "$S"
echo "load $(cut -d' ' -f1-3 /proc/loadavg 2> /dev/null) after the loads" >> "$S"
stop; echo "server stopped" >> "$S"
echo DONE >> "$S"
