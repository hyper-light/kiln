#!/usr/bin/env python3
# The figures docs/browser-lab.md computes from its records rather than quotes from kiln-lab's
# output (plan 01a1a832e9cc step 10's audit): each printed with what it is, from the run files.
#   figures.py DOCS_BROWSER_LAB
import collections, json, pathlib, statistics as st, sys

D = pathlib.Path(sys.argv[1])
jobs = lambda p: json.load(open(D / p))["jobs"]
load = lambda j: float(j["machine"]["load"].split()[0])
psi = lambda j: next(float(x.split("=")[1]) for x in j["machine"]["cpu_pressure"].split() if x.startswith("avg10="))


def say(what, *v):
    print(what + ": " + " ".join(f"{x:.6g}" if isinstance(x, float) else str(x) for x in v))


pl = jobs("2026-11-05/pilot.json")
say("pilot: load median, max; CPU pressure median", st.median(map(load, pl)), max(map(load, pl)), st.median(map(psi, pl)))
cells = collections.defaultdict(list)
for j in pl:
    cells[(j["target"], j["case"], j["condition"], j["browser"])].append(j)
for name, pick in (("kiln today, both copies", lambda t: t.startswith("kiln")), ("h2o-h2", lambda t: t == "h2o-h2")):
    ks = [k for k in cells if pick(k[0])]
    nz = [st.median(j["noise_us_per_s"] for j in cells[k]) for k in ks]
    fl = [st.median(j["noise_us_per_s"] * j["numbers"]["cpu_us.window_ms.p50"] / 1000 for j in cells[k]) for k in ks]
    w = [st.median(j["numbers"]["cpu_us.window_ms.p50"] for j in cells[k]) for k in ks if k[2] == "50k/1"]
    say(f"pilot {name}: cell noise least, most; floor least, most; 50k/1 reading least, most (ms)", min(nz), max(nz), min(fl), max(fl), min(w), max(w))
say("pilot kiln-today-b 50k/1 burst Chromium: CPU mean", st.median(j["numbers"]["cpu_us.mean"] for j in cells[("kiln-today-b", "burst", "50k/1", "chromium")]))
p99 = lambda js, t: [j["numbers"]["time.p99_ms"] for j in js if j["target"] == t and j["condition"] == "50k/1" and "time.p99_ms" in j["numbers"]]
for t in ("h2o-h3", "h2o-h3-reno"):
    say(f"pilot {t} at 50k/1: jobs, median p99 (ms)", len(p99(pl, t)), st.median(p99(pl, t)))
for f in ("k1-50k-stream", "k1b-50k-stream", "k2-50k-stream", "k4-50k-stream"):
    js = jobs(f"2026-11-05/{f}.json")
    say(f"{f}: load median, max", st.median(map(load, js)), max(map(load, js)))
p0 = jobs("2026-11-05/pass0-pilot-good.json") + jobs("2026-11-05/pass0-pilot-50k.json")
per = collections.defaultdict(lambda: [0, 0])
for j in p0:
    k = (j["case"], j["condition"], j["target"], j["round"])
    per[k][0] += j["numbers"].get("cpu_us.n", 0)
    per[k][1] += j["numbers"].get("bytes.n", 0)
say("pass 0: a cell's CPU and bytes samples a round, the median", st.median(v[0] for v in per.values()), st.median(v[1] for v in per.values()))
for t in ("h2o-h3", "h2o-h3-reno"):
    say(f"pass 0 {t} at 50k/1: median p99 (s)", st.median(p99(p0, t)) / 1000)
g = jobs("2026-11-05/pass0-pilot-good.json")
r0 = [j for j in g if j["round"] == 0]
say("pass 0 good link round 0: calibration worst, median (µs); load least, median, most", max(j["calibration_p99_us"] for j in r0), st.median(j["calibration_p99_us"] for j in r0), min(map(load, r0)), st.median(map(load, r0)), max(map(load, r0)))
t = [j["started_us"] for j in g]
say("pass 0 good link: hours first job to last", (max(t) - min(t)) / 3.6e9)
cell = lambda js, tgt: sorted((j for j in js if (j["case"], j["condition"], j["browser"], j["target"]) == ("burst", "50k/1", "chromium", tgt)), key=lambda j: j["round"])
b, m = jobs("2026-11-06/before-s2-50k-1-burst.json"), jobs("2026-11-06/matrix.json")
say("h2o-h2 50k/1 burst Chromium round 0 before: ops, p50, p99 (ms)", *[cell(b, "h2o-h2")[0]["numbers"][k] for k in ("ops", "time.p50_ms", "time.p99_ms")])
say("h2o-h2 50k/1 burst Chromium round 0 after: ops, p50, p99 (ms)", *[cell(m, "h2o-h2")[0]["numbers"][k] for k in ("ops", "time.p50_ms", "time.p99_ms")])
say("kiln today 50k/1 burst Chromium: ops a job", *sorted({j["numbers"]["ops"] for j in cell(m, "kiln-today")}))
after = [j["numbers"]["ops"] for j in m if j["case"] == "burst" and j["condition"] == "50k/1" and j["target"] != "kiln-today"]
say("baselines' 50k/1 burst jobs after: ops least, most", min(after), max(after))
rr = jobs("2026-11-06/rerun5/stream-100k-5-webkit-h2o-h2.json")
mm = [j for j in m if (j["case"], j["condition"], j["browser"], j["target"]) == ("stream", "100k/5", "webkit", "h2o-h2")]
for name, js in (("matrix", mm), ("rerun", rr)):
    v = [j["numbers"]["cpu_us.mean"] for j in js]
    say(f"h2o-h2 100k/5 stream WebKit {name}: CPU mean median, least, most", st.median(v), min(v), max(v))
rq = lambda js: [j for j in js if (j["case"], j["condition"], j["browser"]) == ("request", "good/1", "chromium") and j["target"] in ("kiln-today", "h2o-h2")]
for name, js in (("matrix", rq(m)), ("fresh", rq(jobs("2026-11-07/fresh-good-1-request.json"))), ("quiet", rq(jobs("2026-11-07/quiet-good-1-request.json")))):
    say(f"good/1 request Chromium {name}: load least, most", min(map(load, js)), max(map(load, js)))
fr = rq(jobs("2026-11-07/fresh-good-1-request.json"))
q = [j for j in fr if j["round"] == 2]
say("fresh round 2: load least, most; h2o p50, p99; kiln today p99", min(map(load, q)), max(map(load, q)), *[next(j["numbers"][k] for j in q if j["target"] == "h2o-h2") for k in ("time.p50_ms", "time.p99_ms")], next(j["numbers"]["time.p99_ms"] for j in q if j["target"] == "kiln-today"))
mk = [j["numbers"]["time.p99_ms"] for j in rq(m) if j["target"] == "kiln-today"]
say("matrix kiln today good/1 request p99 rounds least, most", min(mk), max(mk))
