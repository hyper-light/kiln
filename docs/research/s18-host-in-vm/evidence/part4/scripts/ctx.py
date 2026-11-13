"""ctx.py FILE N PATTERN...: print up to N occurrences of each byte PATTERN in FILE with 110 bytes either
side, non-printable bytes shown as '.'. Reads the file as data only."""
import sys, re
path, n, pats = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
data = open(path, "rb").read()
def show(b):
    return "".join(chr(c) if 32 <= c < 127 else "." for c in b)
for p in pats:
    pb = p.encode()
    hits = [m.start() for m in re.finditer(re.escape(pb), data)]
    print(f"## {p}: {len(hits)} occurrences")
    seen = 0
    for i in hits:
        if seen >= n: break
        print(f"   @{i}: {show(data[max(0, i-110):i+len(pb)+110])}")
        seen += 1
