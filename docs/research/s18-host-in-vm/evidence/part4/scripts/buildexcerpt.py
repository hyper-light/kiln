"""buildexcerpt.py NAME URL SHA256 FILE-IN-ARCHIVE LOCALFILE N PATTERN...: up to N occurrences of each PATTERN in a
pinned build's file, 110 bytes either side, non-printable bytes as '.', and any Google installed-app client secret
(GOCSPX-...) redacted. The build is data: nothing in it is run."""
import sys, re, hashlib
name, url, sha, inner, path, n, pats = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5], int(sys.argv[6]), sys.argv[7:]
data = open(path, "rb").read()
show = lambda b: re.sub(r"GOCSPX-[A-Za-z0-9_-]+", "GOCSPX-[redacted]", "".join(chr(c) if 32 <= c < 127 else "." for c in b))
print(f"== {name}: {url}")
print(f"   archive sha256 {sha} (pin.json's, matched on download); file {inner}, sha256 {hashlib.sha256(data).hexdigest()}")
for p in pats:
    hits = [m.start() for m in re.finditer(re.escape(p.encode()), data)]
    print(f"## {p}: {len(hits)} occurrence(s); first {min(n, len(hits))}:")
    for i in hits[:n]:
        print(f"   @{i}: {show(data[max(0, i - 110):i + len(p) + 110])}")
print()
