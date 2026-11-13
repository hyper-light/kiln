"""excerpts.py HARNESS TABLE OUT: for each harness source file a TABLE row cites (not the catalog), a numbered
excerpt of its lines naming the hosts it's cited for, headed by its URL at the pin and its whole SHA-256."""
import csv, os, sys, subprocess, collections
R = os.path.expanduser("~/.cache/g121-s18-hosts")
sys.path.insert(0, f"{R}/bin")
harness, table, out = sys.argv[1:4]
from tables_local import local  # maps a source URL to its fetched copy
# lines naming a host through a constant or a setting, beside the host itself
EXTRA = {"services.rs": ["TYPESAFE_DEFAULT_HOST", "GOOSE_AUTO_EFFORT_ENABLED", "TYPESAFE_API_KEY"]}
by = collections.defaultdict(set)
for r in csv.DictReader(open(table), delimiter="\t"):
    if r["harness_source"].startswith("https://raw.githubusercontent.com/"):
        by[r["harness_source"]].add(r["host"])
with open(out, "w") as o:
    o.write(f"Numbered excerpts of {harness}'s own source at its pin, fetched 2026-11-13 with curl -L: each file's lines naming\n")
    o.write("the hosts its row in hosts.tsv cites it for, with the whole file's SHA-256, so each line can be checked against the tag.\n\n")
    for url in sorted(by):
        hosts = sorted(by[url])
        hosts += EXTRA.get(url.rsplit("/", 1)[-1], [])
        p = subprocess.run(["python3", "-I", f"{R}/bin/excerpt.py", local(url), url, "0"] + hosts, capture_output=True, text=True)
        o.write(p.stdout)
print(len(by), "files")
