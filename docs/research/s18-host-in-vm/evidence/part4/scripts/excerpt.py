"""excerpt.py FILE URL CONTEXT PATTERN...: a numbered excerpt of FILE (as fetched from URL): every line matching
any PATTERN (fixed strings) with CONTEXT lines either side, headed by the URL, the whole file's SHA-256 and size."""
import sys, hashlib
path, url, ctx, pats = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4:]
data = open(path, "rb").read()
lines = data.decode("utf-8", "replace").split("\n")
keep = set()
for i, l in enumerate(lines):
    if any(p in l for p in pats):
        keep.update(range(max(0, i - ctx), min(len(lines), i + ctx + 1)))
print(f"== {url}")
print(f"   sha256 {hashlib.sha256(data).hexdigest()}  {len(data)} bytes  (lines naming: {', '.join(pats)})")
prev = None
for i in sorted(keep):
    if prev is not None and i != prev + 1: print("   ...")
    print(f"{i+1:6d}  {lines[i][:240]}")
    prev = i
print()
