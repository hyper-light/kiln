"""Export raw samples from a Prometheus (stratified: up to K series per metric name, the last H hours)
into a corpus for kiln-obs's codec measurements.

Format (little-endian): magic b"KOBS1\\n", then per series: u32 name length, name (UTF-8, the
series' labels in Prometheus text form), u8 kind (0 gauge, 1 counter, 2 histogram/summary part,
3 unknown), u32 n, then n × (i64 ts_ms, f64 value).
usage: export-prom.py <base-url> <hours> <per-metric> <out>"""
import json, struct, sys, time, urllib.parse, urllib.request

base, hours, per, out = sys.argv[1], float(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
def get(path, **q):
    url = base + path + ("?" + urllib.parse.urlencode(q, doseq=True) if q else "")
    with urllib.request.urlopen(url, timeout=120) as r:
        return json.load(r)["data"]
names = get("/api/v1/label/__name__/values")
meta = get("/api/v1/metadata")
def kind(name):
    for n in (name, name.rsplit("_", 1)[0]):
        m = meta.get(n)
        if m:
            t = m[0].get("type")
            return {"gauge": 0, "counter": 1, "histogram": 2, "summary": 2}.get(t, 3)
    return 3
now = time.time()
total_series = total_samples = 0
with open(out, "wb") as f:
    f.write(b"KOBS1\n")
    for i, name in enumerate(names):
        try:
            series = get("/api/v1/series", **{"match[]": name, "start": now - hours * 3600, "end": now})
        except Exception as e:
            print("skip", name, e, file=sys.stderr); continue
        for s in series[:per]:
            sel = "{" + ",".join(f'{k}="{v}"' for k, v in s.items() if k != "__name__") + "}"
            q = f"{name}{sel}[{int(hours * 3600)}s]"
            try:
                res = get("/api/v1/query", query=q, time=now)["result"]
            except Exception as e:
                print("skip", q[:80], e, file=sys.stderr); continue
            for r in res:
                vals = r.get("values") or []
                if len(vals) < 2: continue
                label = name + "{" + ",".join(f'{k}="{v}"' for k, v in sorted(r["metric"].items()) if k != "__name__") + "}"
                lb = label.encode()
                f.write(struct.pack("<I", len(lb))); f.write(lb); f.write(struct.pack("<BI", kind(name), len(vals)))
                buf = bytearray()
                for ts, v in vals:
                    buf += struct.pack("<qd", int(round(float(ts) * 1000)), float(v))
                f.write(buf)
                total_series += 1; total_samples += len(vals)
        if i % 100 == 0:
            print(f"{i}/{len(names)} names, {total_series} series, {total_samples} samples", file=sys.stderr)
print(f"done: {total_series} series, {total_samples} samples -> {out}")
