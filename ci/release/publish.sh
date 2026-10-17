#!/usr/bin/env bash
# Hand the binaries in dist/ to a kiln server, which then serves them at /dl/kiln/<target> and
# through install.sh / install.ps1 to every machine that installs from it.
#
#   KILN_RELEASE_URL=http://server:7070 KILN_RELEASE_TOKEN=<admin token> ci/release/publish.sh
#
# In a pipeline, both come from the server's [secrets].
set -euo pipefail
cd "$(dirname "$0")/../.."
: "${KILN_RELEASE_URL:?set KILN_RELEASE_URL (the kiln server to publish to)}"
: "${KILN_RELEASE_TOKEN:?set KILN_RELEASE_TOKEN (that server's admin token)}"

shopt -s nullglob
files=(dist/kiln-*)
[ ${#files[@]} -gt 0 ] || { echo "nothing in dist/: run ci/release/build.sh first" >&2; exit 1; }
for f in "${files[@]}"; do
  t=${f#dist/kiln-}
  t=${t%.exe}
  curl -fsS -X POST -H "Authorization: Bearer $KILN_RELEASE_TOKEN" -H 'Content-Type: application/octet-stream' \
    --data-binary @"$f" "${KILN_RELEASE_URL%/}/api/updates/binaries?target=$t" > /dev/null
  echo "published $t ($(stat -c %s "$f") bytes)"
done
