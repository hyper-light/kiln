#!/usr/bin/env bash
# Checks a kiln-signed commit's CMS signature (kiln-git-sign, docs/ci.md W27-R03) against a kiln
# root certificate with openssl: the payload is the commit object without its gpgsig header, as
# git signs it; the signature is gpgsig's armored CMS.  verify-commit-sig.sh REPO COMMIT ROOT.pem
set -euo pipefail
repo=$1 commit=$2 root=$3
d=$(mktemp -d); trap 'rm -rf "$d"' EXIT
git -C "$repo" cat-file commit "$commit" > "$d/commit"
python3 -I - "$d" <<'PY'
import base64, sys
d = sys.argv[1]
lines = open(f"{d}/commit", "rb").read().split(b"\n")
payload, sig, in_sig = [], [], False
for i, line in enumerate(lines):
    if line.startswith(b"gpgsig "):
        in_sig = True
        sig.append(line[len(b"gpgsig "):])
        continue
    if in_sig and line.startswith(b" "):
        sig.append(line[1:])
        continue
    in_sig = False
    payload.append(line)
open(f"{d}/payload", "wb").write(b"\n".join(payload))
body = [l for l in sig if l and not l.startswith(b"-----")]
open(f"{d}/sig.der", "wb").write(base64.b64decode(b"".join(body)))
print("armor:", sig[0].decode(), "...", sig[-1].decode(), f"({len(body)} base64 lines)")
PY
OPENSSL=$(command -v /opt/homebrew/opt/openssl@3/bin/openssl || command -v openssl)
"$OPENSSL" version
echo "signer certificate in the CMS:"
"$OPENSSL" pkcs7 -inform DER -in "$d/sig.der" -print_certs -noout 2>/dev/null | sed 's/^/  /' | head -6 || true
"$OPENSSL" cms -verify -inform DER -in "$d/sig.der" -content "$d/payload" -binary -CAfile "$root" -purpose any -out /dev/null
echo "mutation: the payload with one byte changed must fail"
python3 -I -c 'import sys; p=bytearray(open(sys.argv[1],"rb").read()); p[-2]^=1; open(sys.argv[1]+".bad","wb").write(p)' "$d/payload"
if "$OPENSSL" cms -verify -inform DER -in "$d/sig.der" -content "$d/payload.bad" -binary -CAfile "$root" -purpose any -out /dev/null 2>/dev/null; then echo "  MUTANT VERIFIED (bad)"; exit 1; else echo "  refused, as it must be"; fi
