#!/usr/bin/env bash
# Pins the two programs of kiln's Amp harness for linux/amd64 and linux/arm64 and writes pin.json
# beside this script (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 24; its plan
# 01a1ce705e457061b569409391cb8940): Amp's CLI, `amp`, and its ACP adapter, `amp-acp`, which kiln
# runs as the program (crates/kiln/src/harness/kinds.rs, Kind "amp").
#
#   pin.sh [-o FILE] [AMP_VERSION [AMP_ACP_VERSION]]
#
# AMP_VERSION defaults to the latest Amp CLI (static.ampcode.com/cli/cli-version.txt), and
# AMP_ACP_VERSION to the version the ACP registry lists for amp-acp. Neither publisher signs its
# builds, so each binary passes the two independent checks it has, or nothing is pinned:
#   amp      the same-origin <platform>-amp.sha256, as Amp's installer (ampcode.com/install.sh)
#            checks it; and npm's @ampcode/cli-linux-<arch> of the same version: the tarball's
#            SHA-512 is the packument's dist.integrity, npm's registry signature over
#            "<name>@<version>:<integrity>" verifies with a key npm publishes at
#            /-/npm/v1/keys (docs.npmjs.com/about-registry-signatures), and its package/amp is
#            byte for byte the static binary.
#   amp-acp  GitHub's digest of the release asset amp-acp-linux-<arch>; and the ACP registry's
#            SHA-256 of the release's archive for the platform (agentclientprotocol/registry,
#            amp-acp/agent.json at its latest commit), whose amp-acp is byte for byte the asset.
# A failed check exits non-zero naming it, and no pin is written. The base the Agentfile builds on
# is recorded as the Agentfile beside this script names it (`ARG BASE=`), or as BASE gives it.
#
# Downloads go to a temporary directory under ~/.cache (on kiln's Linux machines /tmp is a RAM
# disk), removed on exit; nothing fetched is kept. The endpoints can be pointed elsewhere, a
# mirror's or a test's: AMP_STORAGE_BASE (the name Amp's installer gives it), NPM_REGISTRY,
# GITHUB_API, GITHUB_RAW. GITHUB_TOKEN, when set, goes to GitHub's API as its token.
set -euo pipefail

usage() { echo "usage: pin.sh [-o FILE] [AMP_VERSION [AMP_ACP_VERSION]]" >&2; exit 2; }
fail() { echo "pin.sh: $*" >&2; exit 1; }

here=$(cd "$(dirname "$0")" && pwd)
out=$here/pin.json
while getopts 'o:' opt; do
  case $opt in
    o) out=$OPTARG ;;
    *) usage ;;
  esac
done
shift $((OPTIND - 1))
[ $# -le 2 ] || usage
amp_version=${1:-}
acp_version=${2:-}
acp_version=${acp_version#v}

AMP_STORAGE_BASE=${AMP_STORAGE_BASE:-https://static.ampcode.com}
NPM_REGISTRY=${NPM_REGISTRY:-https://registry.npmjs.org}
GITHUB_API=${GITHUB_API:-https://api.github.com}
GITHUB_RAW=${GITHUB_RAW:-https://raw.githubusercontent.com}
acp_repo=tao12345666333/amp-acp
registry_repo=agentclientprotocol/registry

for tool in curl jq openssl tar cmp fold; do
  command -v "$tool" >/dev/null 2>&1 || fail "needs $tool"
done
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  fail "needs sha256sum or shasum"
fi
size() { wc -c < "$1" | tr -d ' '; }

# HTTPS only, redirects included, unless an endpoint was pointed at plain HTTP on purpose.
protos='=https'
case "$AMP_STORAGE_BASE $NPM_REGISTRY $GITHUB_API $GITHUB_RAW" in *http://*) protos='=https,http' ;; esac

base=${BASE:-}
if [ -z "$base" ] && [ -f "$here/Agentfile" ]; then
  base=$(sed -n 's/^ARG BASE=//p' "$here/Agentfile" | head -n 1)
fi
[ -n "$base" ] || fail "no base image: set BASE, or give the Agentfile beside this script its ARG BASE"
case $base in *@sha256:*) ;; *) fail "the base $base is not pinned by digest" ;; esac

cache=${XDG_CACHE_HOME:-$HOME/.cache}
mkdir -p "$cache"
work=$(mktemp -d "$cache/pin-amp.XXXXXX")
trap 'rm -rf "$work"' EXIT

fetch() { # URL FILE
  curl -fsSL --proto "$protos" --proto-redir "$protos" --retry 3 -o "$2" "$1" || fail "download failed: $1"
}
github() { # API-PATH FILE
  local auth=()
  if [ -n "${GITHUB_TOKEN:-}" ]; then auth=(-H "Authorization: Bearer $GITHUB_TOKEN"); fi
  curl -fsSL --proto "$protos" --proto-redir "$protos" --retry 3 -H 'Accept: application/vnd.github+json' \
    ${auth[@]+"${auth[@]}"} -o "$2" "$GITHUB_API/$1" || fail "GitHub API failed: $GITHUB_API/$1"
}

# npm's registry signature over "<name>@<version>:<integrity>", verified with a key npm publishes
# whose expiry, if any, is still ahead; prints the key's id.
npm_signature() { # DIR NAME VERSION INTEGRITY
  local d=$1 name=$2 version=$3 integrity=$4 n i keyid sig key expires now
  printf '%s@%s:%s' "$name" "$version" "$integrity" > "$d/npm.msg"
  n=$(jq -er '.dist.signatures | length' "$d/packument.json") || fail "$name@$version: its packument carries no registry signature"
  now=$(date -u +%Y-%m-%dT%H:%M:%S.000Z)
  for ((i = 0; i < n; i++)); do
    keyid=$(jq -er ".dist.signatures[$i].keyid" "$d/packument.json") || continue
    sig=$(jq -er ".dist.signatures[$i].sig" "$d/packument.json") || continue
    key=$(jq -er --arg k "$keyid" 'first(.keys[] | select(.keyid == $k and .keytype == "ecdsa-sha2-nistp256") | .key)' "$work/npm-keys.json") || continue
    expires=$(jq -r --arg k "$keyid" 'first(.keys[] | select(.keyid == $k) | .expires) // ""' "$work/npm-keys.json")
    if [ -n "$expires" ] && [[ $expires < $now ]]; then continue; fi
    { echo '-----BEGIN PUBLIC KEY-----'; printf '%s' "$key" | fold -w 64; echo; echo '-----END PUBLIC KEY-----'; } > "$d/npm-key.pem"
    printf '%s' "$sig" | openssl base64 -d -A > "$d/npm.sig"
    if openssl dgst -sha256 -verify "$d/npm-key.pem" -signature "$d/npm.sig" "$d/npm.msg" >/dev/null 2>&1; then
      echo "$keyid"
      return 0
    fi
  done
  fail "$name@$version: no registry signature verifies with a current key npm publishes at $NPM_REGISTRY/-/npm/v1/keys"
}

read_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)

if [ -z "$amp_version" ]; then
  fetch "$AMP_STORAGE_BASE/cli/cli-version.txt" "$work/cli-version.txt"
  amp_version=$(tr -d '[:space:]' < "$work/cli-version.txt")
fi
[[ $amp_version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || fail "$amp_version is not a version Amp publishes"

github "repos/$registry_repo/commits?path=amp-acp/agent.json&per_page=1" "$work/commits.json"
registry_commit=$(jq -er '.[0].sha' "$work/commits.json") || fail "the ACP registry has no commit of amp-acp/agent.json"
registry_url=$GITHUB_RAW/$registry_repo/$registry_commit/amp-acp/agent.json
fetch "$registry_url" "$work/agent.json"
[ "$(jq -er '.id' "$work/agent.json")" = amp-acp ] || fail "$registry_url is not amp-acp's entry"
registry_version=$(jq -er '.version' "$work/agent.json") || fail "$registry_url names no version"
[ -n "$acp_version" ] || acp_version=$registry_version
[ "$registry_version" = "$acp_version" ] || fail "the ACP registry (commit $registry_commit) names amp-acp $registry_version, not $acp_version: the second check of amp-acp needs its entry for the version pinned"
release_api=repos/$acp_repo/releases/tags/v$acp_version
github "$release_api" "$work/release.json"

fetch "$NPM_REGISTRY/-/npm/v1/keys" "$work/npm-keys.json"
: > "$work/builds.jsonl"
: > "$work/checks.jsonl"
for spec in linux/amd64:x64:x86_64 linux/arm64:arm64:aarch64; do
  IFS=: read -r platform arch registry_arch <<< "$spec"
  d=$work/$arch
  mkdir -p "$d/npm" "$d/acp"

  # amp, first check: the checksum Amp publishes beside the binary.
  amp_url=$AMP_STORAGE_BASE/cli/$amp_version/amp-linux-$arch
  fetch "$amp_url" "$d/amp"
  fetch "$AMP_STORAGE_BASE/cli/$amp_version/linux-$arch-amp.sha256" "$d/amp.sha256"
  amp_sha=$(sha256 "$d/amp")
  [ "$amp_sha" = "$(tr -d '[:space:]' < "$d/amp.sha256")" ] || fail "amp linux-$arch: its SHA-256 $amp_sha is not the same-origin checksum ($amp_url.sha256 beside it)"

  # amp, second check: npm's signed package of the same version holds the same bytes.
  npm_name=@ampcode/cli-linux-$arch
  fetch "$NPM_REGISTRY/@ampcode%2fcli-linux-$arch/$amp_version" "$d/packument.json"
  [ "$(jq -er '.name' "$d/packument.json")" = "$npm_name" ] && [ "$(jq -er '.version' "$d/packument.json")" = "$amp_version" ] \
    || fail "npm's packument for $npm_name@$amp_version names another package"
  integrity=$(jq -er '.dist.integrity' "$d/packument.json") || fail "$npm_name@$amp_version: no dist.integrity"
  case $integrity in sha512-*) ;; *) fail "$npm_name@$amp_version: its integrity is not SHA-512" ;; esac
  fetch "$(jq -er '.dist.tarball' "$d/packument.json")" "$d/npm.tgz"
  [ "sha512-$(openssl dgst -sha512 -binary "$d/npm.tgz" | openssl base64 -A)" = "$integrity" ] \
    || fail "$npm_name@$amp_version: the tarball's SHA-512 is not the packument's dist.integrity"
  npm_keyid=$(npm_signature "$d" "$npm_name" "$amp_version" "$integrity")
  tar -xzf "$d/npm.tgz" -C "$d/npm" package/amp || fail "$npm_name@$amp_version: the tarball holds no package/amp"
  cmp -s "$d/npm/package/amp" "$d/amp" || fail "amp linux-$arch: npm's $npm_name@$amp_version holds another binary than $amp_url"

  # amp-acp, first check: GitHub's digest of the release asset.
  asset=amp-acp-linux-$arch
  asset_json=$(jq -ec --arg n "$asset" 'first(.assets[] | select(.name == $n))' "$work/release.json") \
    || fail "amp-acp v$acp_version has no release asset $asset"
  acp_url=$(jq -er '.browser_download_url' <<< "$asset_json")
  digest=$(jq -er '.digest' <<< "$asset_json") || fail "GitHub gives no digest for $asset"
  case $digest in sha256:*) ;; *) fail "GitHub's digest of $asset is not a SHA-256: $digest" ;; esac
  fetch "$acp_url" "$d/amp-acp"
  acp_sha=$(sha256 "$d/amp-acp")
  [ "$acp_sha" = "${digest#sha256:}" ] || fail "amp-acp linux-$arch: its SHA-256 $acp_sha is not GitHub's digest of $asset"
  [ "$(size "$d/amp-acp")" = "$(jq -er '.size' <<< "$asset_json")" ] || fail "amp-acp linux-$arch: its size is not GitHub's for $asset"

  # amp-acp, second check: the registry's archive of the same release holds the same bytes.
  entry=$(jq -ec --arg p "linux-$registry_arch" '.distribution.binary[$p]' "$work/agent.json") \
    || fail "the ACP registry lists no linux-$registry_arch build of amp-acp"
  archive_url=$(jq -er '.archive' <<< "$entry")
  archive_sha=$(jq -er '.sha256' <<< "$entry")
  cmd=$(jq -er '.cmd' <<< "$entry")
  fetch "$archive_url" "$d/acp.tgz"
  [ "$(sha256 "$d/acp.tgz")" = "$archive_sha" ] || fail "amp-acp linux-$registry_arch: the archive's SHA-256 is not the ACP registry's ($archive_url)"
  tar -xzf "$d/acp.tgz" -C "$d/acp" || fail "amp-acp linux-$registry_arch: the registry's archive does not unpack"
  [ -f "$d/acp/${cmd#./}" ] || fail "amp-acp linux-$registry_arch: the registry's archive has no ${cmd#./}"
  cmp -s "$d/acp/${cmd#./}" "$d/amp-acp" || fail "amp-acp linux-$arch: the registry's archive holds another binary than $asset"

  echo "amp $amp_version linux-$arch: $amp_sha, the same-origin checksum; $npm_name@$amp_version's integrity and registry signature ($npm_keyid) hold, and its binary is the same"
  echo "amp-acp $acp_version linux-$arch: $acp_sha, GitHub's digest of $asset; the ACP registry's archive ($archive_sha, registry $registry_commit) holds the same binary"
  jq -nc --arg platform "$platform" \
    --arg amp_url "$amp_url" --arg amp_sha "$amp_sha" --argjson amp_size "$(size "$d/amp")" \
    --arg acp_url "$acp_url" --arg acp_sha "$acp_sha" --argjson acp_size "$(size "$d/amp-acp")" \
    '{platform: $platform, files: [
       {url: $amp_url, sha256: $amp_sha, size: $amp_size, path: "bin/amp", mode: "0755"},
       {url: $acp_url, sha256: $acp_sha, size: $acp_size, path: "bin/amp-acp", mode: "0755"}]}' >> "$work/builds.jsonl"
  jq -nc --arg platform "$platform" --arg npm "$npm_name@$amp_version" --arg integrity "$integrity" \
    --arg keyid "$npm_keyid" --arg archive "$archive_url" --arg archive_sha "$archive_sha" \
    '{platform: $platform, npm: {package: $npm, integrity: $integrity, keyid: $keyid},
      registry_archive: {url: $archive, sha256: $archive_sha}}' >> "$work/checks.jsonl"
  rm -rf "$d"
done

jq -n --arg version "$amp_version" --arg read_at "$read_at" --arg base "$base" \
  --arg storage "$AMP_STORAGE_BASE" --arg acp_version "$acp_version" \
  --arg release "$GITHUB_API/$release_api" --arg registry_commit "$registry_commit" --arg registry_url "$registry_url" \
  --arg npm_keys "$NPM_REGISTRY/-/npm/v1/keys" \
  --slurpfile builds "$work/builds.jsonl" --slurpfile checks "$work/checks.jsonl" \
  '{harness: "amp", version: $version, read_at: $read_at, base: $base,
    verified: {
      amp: {version: $version,
            same_origin: ($storage + "/cli/" + $version + "/<platform>-amp.sha256"),
            npm: {keys: $npm_keys, packages: [$checks[] | {platform, package: .npm.package, integrity: .npm.integrity, keyid: .npm.keyid}]}},
      "amp-acp": {version: $acp_version, tag: ("v" + $acp_version), github_release: $release,
                  registry: {commit: $registry_commit, entry: $registry_url,
                             archives: [$checks[] | {platform, url: .registry_archive.url, sha256: .registry_archive.sha256}]}}},
    builds: $builds}' > "$work/pin.json"
# Into place in one rename, so a reader never sees half a pin.
cp "$work/pin.json" "$out.new"
mv -f "$out.new" "$out"
echo "pinned amp $amp_version and amp-acp $acp_version for linux/amd64 and linux/arm64: $out (sha256 $(sha256 "$out"))"
