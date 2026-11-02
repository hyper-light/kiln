#!/usr/bin/env bash
# Sign kiln's Windows binary (Authenticode), so Windows trusts it: where Windows 11's Smart App
# Control is on, an unsigned build runs only once Microsoft's cloud knows it, and until then a
# runner's upgrade to it is refused (docs/releases.md, "Windows: Smart App Control and signing").
#
#   ci/release/sign-windows.sh dist/kiln-x86_64-pc-windows-msvc.exe
#
# Signs in place through Microsoft's Artifact Signing (formerly Trusted Signing), with jsign
# (pinned and checked; it needs Java), which timestamps the signature, so it outlives the
# certificate. Nothing is signed, and it says so, unless WINDOWS_SIGNING_ENDPOINT is set:
#
#   WINDOWS_SIGNING_ENDPOINT  the account's regional endpoint, e.g. https://eus.codesigning.azure.net
#   WINDOWS_SIGNING_ACCOUNT   the Artifact Signing account
#   WINDOWS_SIGNING_PROFILE   its certificate profile
#   WINDOWS_SIGNING_TOKEN     an access token for https://codesigning.azure.net: a kiln secret
#                             brokered from an `azure` integration, so nothing is stored. Without
#                             it, jsign signs as the app registration AZURE_TENANT_ID,
#                             AZURE_CLIENT_ID and AZURE_CLIENT_SECRET name.
#
# KILN_JSIGN_HOME overrides where jsign is kept (default ~/.cache/kiln-jsign).
set -euo pipefail

exe=${1:?usage: ci/release/sign-windows.sh <kiln.exe>}
if [ -z "${WINDOWS_SIGNING_ENDPOINT:-}" ]; then
  echo "not signing $exe: WINDOWS_SIGNING_ENDPOINT is not set (docs/releases.md)"
  exit 0
fi
: "${WINDOWS_SIGNING_ACCOUNT:?is not set (docs/releases.md)}" "${WINDOWS_SIGNING_PROFILE:?is not set (docs/releases.md)}"
command -v java >/dev/null || { echo "signing $exe needs Java (jsign): there is none on PATH" >&2; exit 1; }

# jsign, the same on GitHub's release and Maven Central's.
jsign_version=7.6
jsign_sha256=6cf58aa080e7ff82656f320cff8d4a3bff30408df8766806dcba3740dd839fa5
jsign_home=${KILN_JSIGN_HOME:-$HOME/.cache/kiln-jsign}
jar=$jsign_home/jsign-$jsign_version.jar
if ! echo "$jsign_sha256  $jar" | sha256sum -c --status 2>/dev/null; then
  mkdir -p "$jsign_home"
  curl -fsSL -o "$jar.part" "https://repo1.maven.org/maven2/net/jsign/jsign/$jsign_version/jsign-$jsign_version.jar"
  if ! echo "$jsign_sha256  $jar.part" | sha256sum -c --status; then
    rm -f "$jar.part"
    echo "jsign $jsign_version from Maven Central isn't what it was pinned as" >&2
    exit 1
  fi
  mv "$jar.part" "$jar"
fi

# The token through the environment, so it isn't on a command line others can read.
if [ -n "${WINDOWS_SIGNING_TOKEN:-}" ]; then
  set -- --storepass env:WINDOWS_SIGNING_TOKEN "$exe"
else
  set -- "$exe"
fi
java -jar "$jar" --storetype TRUSTEDSIGNING --keystore "${WINDOWS_SIGNING_ENDPOINT#https://}" \
  --alias "$WINDOWS_SIGNING_ACCOUNT/$WINDOWS_SIGNING_PROFILE" \
  --name kiln --url https://github.com/hyper-light/kiln "$@"

# The signature is in the file: the PE's certificate table (data directory 4) isn't empty.
u32() { od -An -t u4 -j "$2" -N 4 "$1" | tr -d ' '; }
pe=$(u32 "$exe" 60)
magic=$(od -An -t u2 -j $((pe + 24)) -N 2 "$exe" | tr -d ' ')
table=$((pe + 24 + (magic == 523 ? 112 : 96) + 4 * 8))
if [ "$(u32 "$exe" $((table + 4)))" -eq 0 ]; then
  echo "jsign finished, but $exe carries no signature" >&2
  exit 1
fi
echo "signed $exe ($WINDOWS_SIGNING_ACCOUNT/$WINDOWS_SIGNING_PROFILE)"
