#!/usr/bin/env python3
# "What the numbers say" (docs/browser-lab.md, plan 01a1a832e9cc step 10): the summaries the doc
# quotes, from the matrix's tables (`kiln-lab browser report docs/browser-lab/<date>/matrix.json`):
# which target is the best in how many of the 486 numbers; kiln today over the best baseline in each
# cell, by case, link and measure (the median, least and most over the cell's losses and browsers;
# CPU percentiles that fell to the mean left out); and HTTP/3's best over HTTP/2's in Chromium, by
# measure and loss.
#   say.py TABLES.md
import collections, re, statistics, sys

rows, table, head = [], None, None
for line in open(sys.argv[1]):
    line = line.rstrip("\n")
    if line.startswith("#### "):
        table = line[5:]
    elif line.startswith("| condition"):
        head = [c.strip() for c in line.strip("|").split("|")]
    elif line.startswith("| ") and not line.startswith("| ---"):
        cells = [c.strip() for c in line.strip("|").split("|")]
        values = {}
        for target, cell in zip(head[2:-1], cells[2:-1]):
            m = re.match(r"(-?[\d.]+)", cell)
            if m:
                values[target] = (float(m.group(1)), "(mean" in cell)
        case, browser = table.split(" in ")
        rows.append((case, browser, cells[0], " ".join(cells[1].split()[:2]), values, cells[-1].split()[0]))

best = collections.Counter(r[5] for r in rows)
print(f"best of {len(rows)} numbers: " + ", ".join(f"{t} {n}" for t, n in best.most_common()))

kiln = collections.defaultdict(list)
for case, browser, cond, measure, values, _ in rows:
    base = [v for t, (v, _) in values.items() if t != "kiln-today"]
    k = values.get("kiln-today")
    if k and base and not k[1] and min(base) > 0 and k[0] > 0:
        kiln[(case, cond.split("/")[0], measure)].append(k[0] / min(base))
print("\nkiln today over the best baseline (median, least, most):")
for key in sorted(kiln):
    v = kiln[key]
    print(f"  {key[0]} {key[1]} {key[2]}: {statistics.median(v):.2f} ({min(v):.2f} to {max(v):.2f}, {len(v)} cells)")

h3 = collections.defaultdict(list)
for case, browser, cond, measure, values, _ in rows:
    v = {t: x for t, (x, mean) in values.items() if not mean}
    if browser != "chromium" or not all(t in v for t in ("h2o-h2", "caddy-h2", "h2o-h3", "caddy-h3")):
        continue
    two, three = min(v["h2o-h2"], v["caddy-h2"]), min(v["h2o-h3"], v["caddy-h3"])
    if two > 0 and three > 0 and measure.split()[0] in ("time", "bytes"):
        h3[(measure, cond.split("/")[1])].append(three / two)
print("\nHTTP/3's best over HTTP/2's in Chromium (median, least, most over the 9 cases and links):")
for key in sorted(h3):
    v = h3[key]
    print(f"  {key[0]} at {key[1]}% loss: {statistics.median(v):.2f} ({min(v):.2f} to {max(v):.2f})")
