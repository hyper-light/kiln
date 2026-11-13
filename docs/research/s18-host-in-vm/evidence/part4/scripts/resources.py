"""resources.py SOURCES.tsv...: rewrite each row's sha256 from its file as it stands; report rows whose file is missing
and files in the directory that no row lists."""
import sys, os, hashlib
for p in sys.argv[1:]:
    d = os.path.dirname(p); rows = open(p).read().splitlines(); out = [rows[0]]; listed = {"SOURCES.tsv"}
    for r in rows[1:]:
        c = r.split("\t"); f = os.path.join(d, c[0]); listed.add(c[0])
        if not os.path.exists(f): print("MISSING", f); out.append(r); continue
        c[2] = hashlib.sha256(open(f, "rb").read()).hexdigest(); out.append("\t".join(c))
    open(p, "w").write("\n".join(out) + "\n")
    extra = sorted(set(os.listdir(d)) - listed)
    if extra: print("UNLISTED", d, extra)
