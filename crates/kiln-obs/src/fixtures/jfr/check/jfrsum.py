"""Sums of a JFR recording's samples, per kind, as kiln's converter should make them: the oracle
(jfrdump.py's reader, written apart from kiln's, from the JDK's and JMC's) applied to every chunk,
each event of a kind kiln reads summed by its weight rules, and each function's self (leaf) and
total (once a stack) by the label kiln gives it. Prints JSON. kiln-obs's test of dd-trace-java's
recordings (`profile::jfr`) holds kiln's sums to these:

    python3 -I jfrsum.py <(zstd -dc ../dd-trace-java-1.67.1-burn.jfr.zst)
    python3 -I jfrsum.py <(zstd -dc ../dd-trace-java-1.67.1-burn-jdk.jfr.zst)
"""
import importlib.util
import json
import os
import re
import sys
from collections import defaultdict

here = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("jfrdump", sys.argv[2] if len(sys.argv) > 2 else os.path.join(here, "jfrdump.py"))
jd = importlib.util.module_from_spec(spec)
spec.loader.exec_module(jd)

KIND = {
    "datadog.ExecutionSample": "CpuDd", "jdk.ExecutionSample": "CpuJdk", "jdk.CPUTimeSample": "CpuTime",
    "datadog.MethodSample": "Wall", "datadog.ObjectSample": "AllocDd", "jdk.ObjectAllocationSample": "AllocJdk",
    "jdk.ObjectAllocationInNewTLAB": "AllocTlab", "jdk.ObjectAllocationOutsideTLAB": "AllocTlab",
    "datadog.HeapLiveObject": "LiveDd", "jdk.OldObjectSample": "LiveJdk",
    "jdk.JavaMonitorEnter": "Lock", "jdk.JavaMonitorWait": "Lock", "jdk.ThreadPark": "Lock",
    "datadog.ExceptionSample": "Exceptions",
    "jdk.FileRead": "FileIo", "jdk.FileWrite": "FileIo", "jdk.FileForce": "FileIo",
    "jdk.SocketRead": "SocketIo", "jdk.SocketWrite": "SocketIo",
}
OUTPUTS = [
    ("cpu.pprof", ["CpuDd", "CpuTime", "CpuJdk"]), ("wall.pprof", ["Wall"]), ("alloc.pprof", ["AllocDd", "AllocJdk", "AllocTlab"]),
    ("live.pprof", ["LiveDd", "LiveJdk"]), ("lock.pprof", ["Lock"]), ("exceptions.pprof", ["Exceptions"]),
    ("file-io.pprof", ["FileIo"]), ("socket-io.pprof", ["SocketIo"]),
]


def label(cls, method):
    if not cls:
        return method
    c = cls.replace("/", ".")
    i = c.rfind(".0x")
    if i >= 0 and len(c) > i + 3 and all(ch in "0123456789abcdefABCDEF" for ch in c[i + 3:]):
        c = c[:i]
    i = c.find("$$Lambda")
    if i >= 0:
        c = c[: i + len("$$Lambda")]
    for g in ("GeneratedMethodAccessor", "GeneratedConstructorAccessor", "GeneratedSerializationConstructorAccessor"):
        i = c.find(g)
        if i >= 0:
            j = i + len(g)
            k = j
            while k < len(c) and c[k].isdigit():
                k += 1
            c = c[:j] + c[k:]
    i = c.find("$$EnhancerBySpringCGLIB$$")
    if i >= 0:
        c = c[: i + len("$$EnhancerBySpringCGLIB$$")]
    return f"{c}.{method}"


def text(v, strings):
    if isinstance(v, dict):
        return v.get("string", "") or ""
    if isinstance(v, tuple) and v and v[0] == "cpool-string":
        return strings.get(v[1], "")
    return v if isinstance(v, str) else ""


def period_ns(s):
    m = re.match(r"\s*([0-9.]+)\s*(ns|us|ms|s|m|h|d)\s*$", s or "")
    if not m:
        return None
    return float(m.group(1)) * {"ns": 1, "us": 1e3, "ms": 1e6, "s": 1e9, "m": 60e9, "h": 3600e9, "d": 86400e9}[m.group(2)]


def main():
    data = open(sys.argv[1], "rb").read()
    # kind -> {"events", "v0", "v1", "self": {fn: v1}, "total": {fn: v1}}
    sums = defaultdict(lambda: {"events": 0, "v0": 0.0, "v1": 0.0, "self": defaultdict(float), "total": defaultdict(float)})
    pos = 0
    chunks = 0
    while pos < len(data):
        ch = jd.Chunk(data, pos)
        if ch.end > len(data):
            break
        chunks += 1
        r = jd.R(data, pos + ch.meta_off, ch.end, ch.compressed)
        r.int_(4)
        (r.varlong() if ch.compressed else r.int_(8))
        r.int_(8); r.int_(8); r.int_(8)
        nstr = r.int_(4)
        strings = [r.string() for _ in range(nstr)]
        root = jd.read_element(r, strings)
        classes = jd.build_classes(root)
        by_name = {c.name: c for c in classes.values()}
        read_type, read_field = jd.value_reader(classes, by_name, ch.compressed)
        pools = defaultdict(dict)
        cp = pos + ch.cp_off
        seen = set()
        while cp not in seen:
            seen.add(cp)
            r = jd.R(data, cp, ch.end, ch.compressed)
            size = r.varlong() if ch.compressed else r.int_(4)
            r.end = cp + size
            (r.varlong() if ch.compressed else r.int_(8))
            r.int_(8); r.int_(8)
            delta = r.int_(8)
            if delta >= 1 << 63:
                delta -= 1 << 64
            r.u8()
            for _ in range(r.int_(4)):
                tid = r.varlong() if ch.compressed else r.int_(8)
                cnt = r.int_(4)
                c = classes[tid]
                for _ in range(cnt):
                    key = r.varlong() if ch.compressed else r.int_(8)
                    val = read_type(r, tid, 0)
                    pools[c.name].setdefault(key, val)
            if delta == 0:
                break
            cp += delta
        pstrings = {k: (v if isinstance(v, str) else text(v, {})) for k, v in pools.get("java.lang.String", {}).items()}

        def sym(k):
            v = pools.get("jdk.types.Symbol", {}).get(k)
            if v is None:
                return ""
            s = v.get("string") if isinstance(v, dict) else v
            return text(s, pstrings) if not isinstance(s, str) else s

        def frame_name(fr):
            m = fr.get("method")
            mk = m[2] if isinstance(m, tuple) else m
            meth = pools.get("jdk.types.Method", {}).get(mk, {})
            t = meth.get("type")
            ck = t[2] if isinstance(t, tuple) else t
            cls = pools.get("java.lang.Class", {}).get(ck, {})
            n = cls.get("name")
            cname = sym(n[2] if isinstance(n, tuple) else n) if cls else ""
            nm = meth.get("name")
            mname = sym(nm[2] if isinstance(nm, tuple) else nm)
            return label(cname, mname)

        cpu_iv = wall_iv = jdk_period = None
        raw = []
        r = jd.R(data, pos + 68, ch.end, ch.compressed)
        while r.p < ch.end:
            ep = r.p
            size = r.varlong() if ch.compressed else r.int_(4)
            etype = r.varlong() if ch.compressed else r.int_(8)
            c = classes.get(etype)
            if etype in (0, 1) or c is None:
                r.p = ep + size
                continue
            er = jd.R(data, r.p, ep + size, ch.compressed)
            try:
                vals = {f["name"]: read_field(er, f, 0) for f in c.fields}
            except Exception:
                r.p = ep + size
                continue
            r.p = ep + size
            if c.name == "jdk.ActiveSetting":
                target = classes.get(vals.get("id"))
                tn, nm, vv = (target.name if target else None), text(vals.get("name"), pstrings), text(vals.get("value"), pstrings)
                if tn == "datadog.ExecutionSample" and nm == "interval":
                    cpu_iv = float(vv)
                elif tn == "datadog.MethodSample" and nm == "interval":
                    wall_iv = float(vv)
                elif tn == "jdk.ExecutionSample" and nm == "period":
                    jdk_period = period_ns(vv)
            elif c.name == "datadog.DatadogProfilerConfig":
                if cpu_iv is None and (vals.get("cpuInterval") or 0) > 0:
                    cpu_iv = vals["cpuInterval"] * 1e6
                if wall_iv is None and (vals.get("wallInterval") or 0) > 0:
                    wall_iv = vals["wallInterval"] * 1e6
            k = KIND.get(c.name)
            if not k:
                continue
            st = vals.get("stackTrace")
            sk = st[2] if isinstance(st, tuple) else st
            if not sk:
                continue
            w, sz = vals.get("weight"), vals.get("size")
            dur = vals.get("duration")
            if k in ("CpuDd", "Wall"):
                w = w if (w or 0) > 0 else 1
                v = [w, w]
            elif k == "CpuJdk":
                v = [1, 1]
            elif k == "CpuTime":
                v = [1, (vals.get("samplingPeriod") or 0) * 1e9 / ch.tps]
            elif k in ("AllocDd", "LiveDd"):
                w = 1 if w is None else w
                v = [w, w * (sz or 0)]
            elif k == "AllocJdk":
                v = [1, w or 0]
            elif k == "AllocTlab":
                v = [1, vals.get("tlabSize") if c.name == "jdk.ObjectAllocationInNewTLAB" else vals.get("allocationSize") or 0]
            elif k == "LiveJdk":
                v = [1, vals.get("objectSize") or 0]
            elif k in ("Lock", "FileIo", "SocketIo"):
                v = [1, (dur or 0) * 1e9 / ch.tps]
            else:
                v = [1, 0]
            raw.append((k, sk, v))
        cpu_iv = 10e6 if cpu_iv is None else cpu_iv
        wall_iv = 50e6 if wall_iv is None else wall_iv
        jdk_period = 10e6 if jdk_period is None else jdk_period
        for k, sk, v in raw:
            stack = pools.get("jdk.types.StackTrace", {}).get(sk)
            if stack is None:
                continue
            frames = stack.get("frames") or []
            scale = {"CpuDd": cpu_iv, "Wall": wall_iv, "CpuJdk": jdk_period}.get(k, 1)
            v1 = v[1] * scale
            s = sums[k]
            s["events"] += 1
            s["v0"] += v[0]
            s["v1"] += v1
            names = [frame_name(fr) for fr in frames]
            if names:
                s["self"][names[0]] += v1
            for n in set(names):
                s["total"][n] += v1
        pos = ch.end
    out = {"chunks": chunks, "profiles": {}}
    for name, kinds in OUTPUTS:
        k = next((k for k in kinds if sums[k]["events"] > 0), None)
        if not k:
            continue
        s = sums[k]
        top = sorted(s["self"].items(), key=lambda x: -x[1])[:8]
        out["profiles"][name] = {
            "kind": k, "events": s["events"], "v0": round(s["v0"], 3), "v1": round(s["v1"], 3),
            "top_self": [(n, round(v)) for n, v in top],
            "total_of": {n: round(v) for n, v in sorted(s["total"].items(), key=lambda x: -x[1])[:12]},
        }
    print(json.dumps(out, indent=1))


main()
