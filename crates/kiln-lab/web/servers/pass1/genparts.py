# Generates pass 1's parts for the two lab machines (fedora: F, adacachy: A), balanced by hours.
import json, os
JOB = {"good": 92, "100k": 229, "50k": 240}       # seconds a job, as the run plan costs them
ROUNDS = {("100k","request"): 6, ("50k","request"): 11}  # the plan at a 70 h budget; 5 elsewhere
ROWS = {"kiln-today": 2, "kiln-today-b": 2, "h2o-h2": 2, "caddy-h2": 2, "caddy-h3": 1, "$H3": 1, "h2o-h3": 1, "h2o-h3-reno": 1}
B = "--browsers chromium,webkit --k 8"
def hours(prof, case, targets, rounds, k=8):
    return sum(ROWS[t] for t in targets) * rounds * JOB[prof] / 3600
F, A = [], []
def add(m, name, prof, loss, case, targets, rounds, k=8, after=None):
    line = f"{name} " + (f"after={after} " if after else "") + f"--targets {','.join(targets)} --profiles {prof} --losses {loss} --cases {case} --rounds {rounds} --browsers chromium,webkit --k {k}"
    (F if m == "F" else A).append((hours(prof, case, targets, rounds), line))
PILOT = ["kiln-today","kiln-today-b","h2o-h2","h2o-h3","h2o-h3-reno"]
# The pilot: its reduced matrix and A/A (A: 50k/1, which also picks h2o's QUIC controller; F: good/0).
for c in ["request","stream","burst"]:
    add("A", f"pilot-50k-{c}", "50k", 1, c, PILOT, 5)
    add("F", f"pilot-good-{c}", "good", 0, c, PILOT, 5)
# K links against one, on each machine, and the same cells across the machines (kiln today and h2o
# over HTTP/2: the two machines' h2o builds differ, same source, each against its own OpenSSL).
for m in ["F","A"]:
    add(m, f"k1-50k-stream", "50k", 1, "stream", ["kiln-today"], 5, k=1)
    add(m, f"k1-good-request", "good", 0, "request", ["kiln-today"], 5, k=1)
add("F", "xm-50k-stream", "50k", 1, "stream", ["kiln-today","h2o-h2"], 5)
add("A", "xm-good-request", "good", 0, "request", ["kiln-today","h2o-h2"], 5)
# The matrix: every condition and case; the pilot's cells stand for good/0 and 50k/1 except 50k/1's
# requests, which the plan gives 11 rounds. h2o-h3's row ($H3) runs on A, after the pilot chose.
mat = []
H3 = []
for prof in ["good","100k","50k"]:
    for loss in [0,1,5]:
        for c in ["stream","burst","request"]:
            r = ROUNDS.get((prof,c), 5)
            pilot = (prof,loss) in [("good",0),("50k",1)] and not (prof=="50k" and c=="request")
            rest = ["caddy-h2","caddy-h3"] if pilot else ["kiln-today","h2o-h2","caddy-h2","caddy-h3"]
            groups = [rest] if r <= 6 else [["kiln-today","h2o-h2"],["caddy-h2","caddy-h3"]]
            for i,g in enumerate(groups):
                mat.append((hours(prof,c,g,r), f"m-{prof}-{loss}-{c}" + (f"-{i}" if len(groups)>1 else ""), prof, loss, c, g, r))
            if not pilot:
                H3.append((hours(prof,c,["$H3"],r), f"m-{prof}-{loss}-{c}-h3 after=pilot-50k-request,pilot-50k-stream,pilot-50k-burst --targets $H3 --profiles {prof} --losses {loss} --cases {c} --rounds {r} --browsers chromium,webkit --k 8"))
# Balance the rest by hours, longest first.
mat.sort(key=lambda x: -x[0])
for h,name,prof,loss,c,g,r in mat:
    m = "F" if sum(x[0] for x in F) <= sum(x[0] for x in A) + sum(x[0] for x in H3) else "A"
    add(m, name, prof, loss, c, g, r)
A.extend(H3)
for m, parts in [("F",F),("A",A)]:
    print(m, "parts", len(parts), "hours", round(sum(p[0] for p in parts),1), "longest", round(max(p[0] for p in parts),1))
    open(os.path.join(os.path.dirname(os.path.abspath(__file__)), f"{'fedora' if m=='F' else 'adacachy'}.parts"),"w").write(
        "# pass 1's parts on this machine (generated; hours as the run plan costs them)\n" + "".join(f"# {h:.2f} h\n{l}\n" for h,l in parts))
