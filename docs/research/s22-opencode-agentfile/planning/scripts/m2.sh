#!/bin/bash
# m2: verify each archive's GitHub artifact attestation (Sigstore) to anomalyco/opencode's release workflow.
set -u
D=$HOME/.cache/s22-opencode-research/dl; mkdir -p "$D"
cd "$D" || exit 1
echo "M2 run $(date -u +%Y-%m-%dT%H:%M:%SZ) gh $(gh --version | head -1 | cut -d' ' -f3)"
for f in opencode-linux-x64.tar.gz opencode-linux-arm64.tar.gz opencode-linux-x64-musl.tar.gz opencode-linux-arm64-musl.tar.gz; do
  out=$(gh attestation verify "$f" --repo anomalyco/opencode --signer-workflow anomalyco/opencode/.github/workflows/release.yml --format json 2>/tmp/s22scripts/m2.err)
  rc=$?
  echo "VERIFY $f exit=$rc"
  if [ $rc -ne 0 ]; then sed 's/^/  ERR /' /tmp/s22scripts/m2.err | head -5; continue; fi
  printf '%s' "$out" | python3 -I -c '
import json,sys
v=json.load(sys.stdin)
for r in v:
    vr=r["verificationResult"]
    st=vr["statement"]; sig=vr.get("signature",{}).get("certificate",{})
    print("  predicateType", st.get("predicateType"))
    print("  subject", [ (s.get("name"), s.get("digest",{}).get("sha256")) for s in st.get("subject",[]) ][:3], "of", len(st.get("subject",[])))
    for k in ("subjectAlternativeName","buildSignerURI","sourceRepositoryURI","sourceRepositoryRef","sourceRepositoryDigest","runInvocationURI","issuer","githubWorkflowTrigger"):
        print("  cert."+k, sig.get(k))
'
done
