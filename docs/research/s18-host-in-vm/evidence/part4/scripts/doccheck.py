"""doccheck.py IN.tsv OUT.tsv: for each line "host<TAB>url url ...", fetch each url (its .md twin first when it
has none) with curl, each into a new empty directory under ~/.cache/g121-s18-hosts/docs/<host>/<n>/, until a
page names the host. Pages are data: nothing in them is run. Writes host, found, url, code, type, bytes,
sha256, fetched (UTC date), path, and the first line naming the host."""
import sys, os, subprocess, hashlib, datetime, concurrent.futures as cf, re
ROOT = os.path.expanduser(os.environ.get("DOCROOT", "~/.cache/g121-s18-hosts/docs"))
DATE = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")
def fetch(url, d):
    os.makedirs(d, exist_ok=False)
    out = os.path.join(d, "page")
    p = subprocess.run(["curl", "-L", "-sS", "--max-time", "45", "--max-filesize", "20000000", "-o", out,
                        "-w", "%{http_code}\t%{content_type}\t%{url_effective}", url],
                       capture_output=True, text=True)
    code, ctype, eff = (p.stdout.split("\t") + ["", "", ""])[:3]
    return out, code, ctype, eff
def check(host, urls):
    n = 0
    tried = []
    for u in urls:
        cands = [u] if re.search(r"\.(md|txt|json)(\?|$)", u) else [u.rstrip("/") + ".md", u]
        for c in cands:
            n += 1
            d = os.path.join(ROOT, host, str(n))
            try:
                out, code, ctype, eff = fetch(c, d)
            except Exception as e:
                tried.append(f"{c}:ERR"); continue
            if not os.path.exists(out) or not code.startswith("2"):
                tried.append(f"{c}:{code}"); continue
            data = open(out, "rb").read()
            text = data.decode("utf-8", "replace")
            m = re.search(r"(?<![A-Za-z0-9.-])" + re.escape(host) + r"(?![A-Za-z0-9-]|\.[A-Za-z0-9])", text, re.I)
            i = m.start() if m else -1
            if i < 0:
                tried.append(f"{c}:{code}:absent"); continue
            # the line naming the host, tags stripped
            s = text.rfind("\n", 0, i) + 1; e = text.find("\n", i); e = len(text) if e < 0 else e
            line = re.sub(r"<[^>]+>", " ", text[max(s, i - 300):min(e, i + 300)])
            line = re.sub(r"\s+", " ", line).strip()[:400]
            return [host, "found", c, code, ctype, str(len(data)), hashlib.sha256(data).hexdigest(), DATE, out, line]
    return [host, "not-found", " ".join(tried)[:900], "", "", "", "", DATE, "", ""]
rows = [l.rstrip("\n").split("\t") for l in open(sys.argv[1]) if l.strip()]
with cf.ThreadPoolExecutor(8) as ex, open(sys.argv[2], "w") as o:
    o.write("host\tresult\turl\tcode\ttype\tbytes\tsha256\tfetched\tpath\tline\n")
    for r in ex.map(lambda r: check(r[0], [u for u in r[1].split() if u.startswith("http")]), rows):
        o.write("\t".join(x.replace("\t", " ") for x in r) + "\n"); o.flush()
