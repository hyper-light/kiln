#!/usr/bin/env bash
# Plan step 24, step 1's tests of .kiln/images/harness-amp/pin.sh, run on a Linux runner:
#   1. pin.sh against the publishers' own endpoints: the pin, and its output as the pin's record;
#   2. every pinned URL downloaded again and hashed against the pin;
#   3. a mirror of every endpoint pin.sh reads, served on 127.0.0.1, pinning what the real ones
#      pin; then each mutation in a copy of the mirror, where pin.sh must exit non-zero naming the
#      check that failed and leave no pin behind.
# Usage: BASE=<image@sha256:...> pin-tests.sh PIN_SH OUT_DIR
# Big files go under ~/.cache (never /tmp on kiln's Linux machines) and are removed at the end.
set -uo pipefail
pin_sh=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
out=$2
mkdir -p "$out"
: "${BASE:?set BASE to the base image pin.sh records}"
for t in curl jq openssl python3 tar sha256sum; do command -v "$t" >/dev/null || { echo "needs $t" >&2; exit 2; }; done
scratch=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/pin-amp-tests.XXXXXX")
server=
cleanup() { [ -n "$server" ] && kill "$server" 2>/dev/null; wait 2>/dev/null; rm -rf "$scratch"; }
trap cleanup EXIT

say() { echo "$*" | tee -a "$out/pin-tests.txt"; }
: > "$out/pin-tests.txt"
say "# pin.sh tests on $(hostname) ($(uname -m), $(uname -r)), $(date -u +%FT%TZ); pin.sh sha256 $(sha256sum "$pin_sh" | cut -d' ' -f1)"

# 1. The pin itself.
bash "$pin_sh" -o "$out/pin.json" > "$out/pin-run.txt" 2>&1
rc=$?
cat "$out/pin-run.txt"
say "PIN_RUN exit=$rc"
[ "$rc" = 0 ] || exit 1
amp_v=$(jq -r .version "$out/pin.json")
acp_v=$(jq -r '.verified["amp-acp"].version' "$out/pin.json")
commit=$(jq -r '.verified["amp-acp"].registry.commit' "$out/pin.json")

# 2. Each pinned file downloaded again and hashed.
ok=0
while IFS=$'\t' read -r url sha size path platform; do
  f=$scratch/verify.bin
  curl -fsSL --proto '=https' --proto-redir '=https' -o "$f" "$url"
  got=$(sha256sum "$f" | cut -d' ' -f1); n=$(wc -c < "$f" | tr -d ' ')
  if [ "$got" = "$sha" ] && [ "$n" = "$size" ]; then ok=$((ok + 1)); r=match; else r="DIFFERS sha256=$got size=$n"; fi
  say "VERIFY $platform $path $url: $r"
  rm -f "$f"
done < <(jq -r '.builds[] | .platform as $p | .files[] | [.url, .sha256, (.size | tostring), .path, $p] | @tsv' "$out/pin.json")
say "VERIFY_MATCHED $ok of $(jq '[.builds[].files[]] | length' "$out/pin.json")"

# 3. A mirror of every endpoint, on a port of its own.
port=$(python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
m=http://127.0.0.1:$port
mirror=$scratch/mirror
get() { mkdir -p "$(dirname "$mirror/$2")"; curl -fsSL --proto '=https' --proto-redir '=https' -o "$mirror/$2" "$1" || { echo "mirror: $1 failed" >&2; exit 1; }; }
mkdir -p "$mirror/static/cli"
echo "$amp_v" > "$mirror/static/cli/cli-version.txt"
for a in x64 arm64; do
  get "https://static.ampcode.com/cli/$amp_v/amp-linux-$a" "static/cli/$amp_v/amp-linux-$a"
  get "https://static.ampcode.com/cli/$amp_v/linux-$a-amp.sha256" "static/cli/$amp_v/linux-$a-amp.sha256"
  get "https://registry.npmjs.org/@ampcode%2fcli-linux-$a/$amp_v" "npm/@ampcode/cli-linux-$a/$amp_v.real"
  tgz=npm/@ampcode/cli-linux-$a/-/cli-linux-$a-$amp_v.tgz
  get "$(jq -r .dist.tarball "$mirror/npm/@ampcode/cli-linux-$a/$amp_v.real")" "$tgz"
  jq --arg u "$m/$tgz" '.dist.tarball = $u' "$mirror/npm/@ampcode/cli-linux-$a/$amp_v.real" > "$mirror/npm/@ampcode/cli-linux-$a/$amp_v"
  rm "$mirror/npm/@ampcode/cli-linux-$a/$amp_v.real"
done
get https://registry.npmjs.org/-/npm/v1/keys npm/-/npm/v1/keys
get "https://api.github.com/repos/agentclientprotocol/registry/commits?path=amp-acp/agent.json&per_page=1" api/repos/agentclientprotocol/registry/commits
get "https://api.github.com/repos/tao12345666333/amp-acp/releases/tags/v$acp_v" "api/release.real"
get "https://raw.githubusercontent.com/agentclientprotocol/registry/$commit/amp-acp/agent.json" agent.real
dl=gh/tao12345666333/amp-acp/releases/download/v$acp_v
for n in amp-acp-linux-x64 amp-acp-linux-arm64 amp-acp-linux-x86_64.tar.gz amp-acp-linux-aarch64.tar.gz; do
  get "https://github.com/tao12345666333/amp-acp/releases/download/v$acp_v/$n" "$dl/$n"
done
mkdir -p "$mirror/api/repos/tao12345666333/amp-acp/releases/tags" "$mirror/raw/agentclientprotocol/registry/$commit/amp-acp"
jq --arg b "$m/$dl/" '.assets |= map(.browser_download_url = ($b + .name))' "$mirror/api/release.real" > "$mirror/api/repos/tao12345666333/amp-acp/releases/tags/v$acp_v"
jq --arg b "$m/$dl/" '.distribution.binary |= with_entries(.value.archive = ($b + (.value.archive | split("/") | last)))' "$mirror/agent.real" \
  > "$mirror/raw/agentclientprotocol/registry/$commit/amp-acp/agent.json"
rm "$mirror/api/release.real" "$mirror/agent.real"
agent=raw/agentclientprotocol/registry/$commit/amp-acp/agent.json
release=api/repos/tao12345666333/amp-acp/releases/tags/v$acp_v

serve() { # DIR
  python3 -I -m http.server "$port" --bind 127.0.0.1 --directory "$1" > /dev/null 2>&1 &
  server=$!
  for _ in $(seq 100); do curl -fs -o /dev/null "$m/static/cli/cli-version.txt" && return 0; sleep 0.1; done
  echo "the mirror did not come up" >&2; exit 1
}
unserve() { kill "$server" 2>/dev/null; wait "$server" 2>/dev/null; server=; }
own() { cp "$1" "$1.own" && mv -f "$1.own" "$1"; }   # a file of the copy's own, no longer the mirror's hard link
flip() { own "$1"; python3 -I -c 'import sys; p = sys.argv[1]; b = bytearray(open(p, "rb").read()); b[4096] ^= 1; open(p, "wb").write(b)' "$1"; }

# CASE NAME EXPECT [ACP_VERSION] : MUTATE (a function of the case directory)
case_run() {
  local name=$1 expect=$2 acp=${3:-$acp_v} dir=$scratch/case-$1 rc err
  cp -al "$mirror" "$dir"
  "mutate_$name" "$dir"
  serve "$dir"
  err=$(AMP_STORAGE_BASE=$m/static NPM_REGISTRY=$m/npm GITHUB_API=$m/api GITHUB_RAW=$m/raw \
        bash "$pin_sh" -o "$dir/pin.json" "$amp_v" "$acp" 2>&1 >/dev/null)
  rc=$?
  unserve
  if [ "$expect" = "" ]; then
    if [ "$rc" = 0 ] && [ "$(jq -c '[.builds[] | {platform, files: [.files[] | {sha256, size, path, mode}]}]' "$dir/pin.json")" = \
         "$(jq -c '[.builds[] | {platform, files: [.files[] | {sha256, size, path, mode}]}]' "$out/pin.json")" ]; then
      say "CASE $name: PASS (exit 0; the mirror pins what the publishers' endpoints pin)"
    else say "CASE $name: FAIL (exit $rc: $err)"; fi
  elif [ "$rc" != 0 ] && [[ $err == *"$expect"* ]] && [ ! -e "$dir/pin.json" ]; then
    say "CASE $name: PASS (exit $rc, no pin written): $err"
  else
    say "CASE $name: FAIL (exit $rc, pin written: $([ -e "$dir/pin.json" ] && echo yes || echo no)): $err"
  fi
  rm -rf "$dir"
}

mutate_baseline() { :; }
# A flipped byte in a downloaded Amp binary.
mutate_amp_byte() { flip "$1/static/cli/$amp_v/amp-linux-x64"; }
# npm's signature checked against another key under npm's key id.
mutate_npm_key() {
  openssl ecparam -name prime256v1 -genkey -noout -out "$1/other.pem" 2>/dev/null
  local k; k=$(openssl ec -in "$1/other.pem" -pubout -outform DER 2>/dev/null | openssl base64 -A)
  own "$1/npm/-/npm/v1/keys"
  jq --arg k "$k" '.keys |= map(.key = $k)' "$1/npm/-/npm/v1/keys" > "$1/keys.new" && mv "$1/keys.new" "$1/npm/-/npm/v1/keys"
}
# An npm tarball whose amp differs (its packument as npm signed it).
mutate_npm_tarball() {
  local t=$1/npm/@ampcode/cli-linux-x64/-/cli-linux-x64-$amp_v.tgz
  mkdir "$1/t" && tar -xzf "$t" -C "$1/t" && flip "$1/t/package/amp"
  rm -f "$t" && tar -czf "$t" -C "$1/t" package
}
# The static binary and its checksum both replaced: the same-origin check passes, npm's catches it.
mutate_static_and_checksum() {
  local f=$1/static/cli/$amp_v/amp-linux-x64
  flip "$f"; own "$1/static/cli/$amp_v/linux-x64-amp.sha256"; sha256sum "$f" | cut -d' ' -f1 > "$1/static/cli/$amp_v/linux-x64-amp.sha256"
}
# A registry sha256 changed by one character.
mutate_registry_sha() {
  own "$1/$agent"
  jq '.distribution.binary["linux-x86_64"].sha256 |= (.[0:63] + (if .[63:] == "0" then "1" else "0" end))' "$1/$agent" > "$1/a.new" && mv "$1/a.new" "$1/$agent"
}
# The registry naming another version than the one asked for.
mutate_registry_version() { own "$1/$agent"; jq '.version = "0.0.0-mutated"' "$1/$agent" > "$1/a.new" && mv "$1/a.new" "$1/$agent"; }
# linux-arm64 removed.
mutate_arm64_gone() { rm -f "$1/static/cli/$amp_v/amp-linux-arm64"; }
# amp-acp's asset and GitHub's digest both replaced: GitHub's check passes, the registry's catches it.
mutate_asset_and_digest() {
  local f=$1/$dl/amp-acp-linux-x64
  flip "$f"; own "$1/$release"
  jq --arg d "sha256:$(sha256sum "$f" | cut -d' ' -f1)" '.assets |= map(if .name == "amp-acp-linux-x64" then .digest = $d else . end)' "$1/$release" > "$1/r.new" && mv "$1/r.new" "$1/$release"
}

case_run baseline ""
case_run amp_byte "is not the same-origin checksum"
case_run npm_key "no registry signature verifies"
case_run npm_tarball "the tarball's SHA-512 is not the packument's dist.integrity"
case_run static_and_checksum "holds another binary than"
case_run registry_sha "the archive's SHA-256 is not the ACP registry's"
case_run registry_version "names amp-acp 0.0.0-mutated, not $acp_v"
case_run arm64_gone "download failed: $m/static/cli/$amp_v/amp-linux-arm64"
case_run asset_and_digest "the registry's archive holds another binary than amp-acp-linux-x64"
say "CASES_PASSED $(grep -c ': PASS' "$out/pin-tests.txt") of 9"
