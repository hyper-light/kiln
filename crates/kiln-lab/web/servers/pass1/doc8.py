#!/usr/bin/env python3
# Step 8 (plan 01a1a832e9cc): the matrix into the repository. Puts `report`'s tables between the
# doc's tables markers, the coverage between its step 8 markers, and copies the slim run file and the
# catalogue the run used under docs/browser-lab/<date>/.
#   doc8.py REPO STEP8_DIR CATALOGUE DATE
import pathlib, shutil, sys

repo, step8, catalogue = (pathlib.Path(a) for a in sys.argv[1:4])
date = sys.argv[4]
doc = repo / "docs/browser-lab.md"
s = doc.read_text()


def between(s, begin, end, body):
    head, rest = s.split(begin, 1)
    _, tail = rest.split(end, 1)
    return f"{head}{begin}\n{body.rstrip()}\n{end}{tail}"


s = between(s, "<!-- browser-lab tables: begin -->", "<!-- browser-lab tables: end -->", (step8 / "tables.md").read_text())
s = between(s, "<!-- step 8 coverage: begin -->", "<!-- step 8 coverage: end -->", (step8 / "coverage-doc.md").read_text())
doc.write_text(s)
out = repo / "docs/browser-lab" / date
out.mkdir(parents=True, exist_ok=True)
shutil.copyfile(step8 / "matrix-slim.json", out / "matrix.json")
shutil.copyfile(catalogue, repo / "docs/browser-lab/catalogue.json")
print(f"{doc}: tables {len((step8 / 'tables.md').read_text())} B; {out / 'matrix.json'} {(out / 'matrix.json').stat().st_size} B")
