#!/usr/bin/env bash
# kiln plan 01a1d2c16eb074f19cf7ae7f1025c0be step 6: amp-acp's and Devin's ACP initialize answers, by the
# handshake alone (handshake.py: initialize, then the process ended; no session, no prompt), from the
# builds kiln's Agentfiles pin (.kiln/images/harness-amp/pin.json, harness-devin/pin.json), each held
# to its pinned SHA-256 before it runs, in an empty home with the environment its harness.json gives.
set -u
HS=$(pwd)/handshake.py
W=$HOME/g121-tmp/s35-handshake; rm -rf "$W"; mkdir -p "$W/home" "$W/amp" "$W/devin"
echo "== $(hostname) $(uname -sm) $(python3 --version) | $(date -u +%FT%TZ) | handshake.py sha256 $(sha256sum "$HS" | cut -c1-64)"
fetch() { curl -fsSL --retry 3 -o "$3" "$1" && echo "$2  $3" | sha256sum -c - 2>&1 | tail -1; }
echo "== amp-acp 0.12.0 linux/amd64"
fetch https://github.com/tao12345666333/amp-acp/releases/download/v0.12.0/amp-acp-linux-x86_64.tar.gz c25e3635159ee12f8a77bd775dd8a8fc21cc4d25a3a23483ffafec73ccb910fe "$W/amp.tgz" && tar -xzf "$W/amp.tgz" -C "$W/amp"
AMP=$(find "$W/amp" -type f -name 'amp-acp*' -perm -u+x | head -1); echo "binary: ${AMP#$W/}"
echo "== devin 3000.12.43 linux/amd64"
fetch https://static.devin.ai/cli/3000.12.43/devin-3000.12.43-x86_64-unknown-linux.tar.gz 6a19997b5fc2d6ba1a8c341d4e2b12f1b78cad5d415ee9c0ca413a8d52a99562 "$W/devin.tgz" && tar -xzf "$W/devin.tgz" -C "$W/devin"
DEVIN=$(find "$W/devin" -type f -name devin -perm -u+x | head -1); echo "binary: ${DEVIN#$W/}"
E="env -i PATH=/usr/bin:/bin HOME=$W/home TERM=dumb"
echo "== HANDSHAKE amp-acp (XDG_* and AMP_SKIP_UPDATE_CHECK=1 as harness-amp/harness.json)"
[ -n "$AMP" ] && $E XDG_CONFIG_HOME=$W/home/.config XDG_DATA_HOME=$W/home/.local/share XDG_STATE_HOME=$W/home/.local/state AMP_SKIP_UPDATE_CHECK=1 python3 "$HS" "$AMP"; echo "AMP_EXIT=$?"
echo "== HANDSHAKE devin acp (XDG_* as harness-devin/harness.json)"
[ -n "$DEVIN" ] && $E XDG_CONFIG_HOME=$W/home/.config XDG_DATA_HOME=$W/home/.local/share python3 "$HS" "$DEVIN" acp; echo "DEVIN_EXIT=$?"
rm -rf "$W"
