"""llmscand.py IN.tsv OUT.tsv: IN lines "host<TAB>origin origin ...". For each origin, fetch /llms.txt (data
only), take its links ranked by words that name an API's address, and write "host<TAB>url url ..." (up to 12)."""
import sys, os, subprocess, re
KEY = re.compile(r"(quick.?start|openai|api|reference|base.?url|endpoint|getting.?started|introduction|overview|auth|oauth|sign|login|compat|chat.?completion|model)", re.I)
def get(u):
    p = subprocess.run(["curl", "-L", "-sS", "--max-time", "30", "--max-filesize", "8000000", u], capture_output=True)
    return p.stdout.decode("utf-8", "replace") if p.returncode == 0 else ""
with open(sys.argv[2], "w") as o:
    for line in open(sys.argv[1]):
        host, origins = line.rstrip("\n").split("\t")
        urls = []
        for org in origins.split():
            t = get(org.rstrip("/") + "/llms.txt")
            links = re.findall(r"\((https?://[^)\s]+)\)", t)
            ranked = sorted(set(links), key=lambda u: (-len(KEY.findall(u)), len(u)))
            urls += ranked[:8]
        o.write(f"{host}\t{' '.join(urls[:12])}\n")
        print(host, len(urls))
