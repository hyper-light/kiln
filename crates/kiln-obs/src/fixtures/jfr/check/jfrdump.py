#!/usr/bin/env python3
"""A generic, metadata-driven JFR reader, written apart from kiln's (from the JDK's and JMC's
readers and the format's research, docs/observe.md 8.11) to check kiln's against: jfrsum.py sums a
recording's samples with it.

Usage: python3 -I jfrdump.py <file.jfr[.gz]> [--events N] [--stacks N]

Reads every chunk, decodes the metadata event, walks the checkpoint chain,
decodes every constant pool generically, then decodes every event generically
and checks that each decode ends exactly at the event's declared size.
Nothing here executes code from the input; it only parses bytes.
"""
import gzip
import struct
import sys
from collections import Counter, defaultdict

MAX_DEPTH = 32


class Err(Exception):
    pass


class R:
    def __init__(self, buf, pos, end, compressed):
        self.b = buf
        self.p = pos
        self.end = end
        self.c = compressed

    def need(self, n):
        if self.p + n > self.end:
            raise Err(f"read past end: pos={self.p} need={n} end={self.end}")

    def u8(self):
        self.need(1)
        v = self.b[self.p]
        self.p += 1
        return v

    def varlong(self):
        v = 0
        for i in range(8):
            b = self.u8()
            v |= (b & 0x7F) << (7 * i)
            if b < 0x80:
                return v
        b = self.u8()
        return v | (b << 56)

    def int_(self, width):
        if self.c:
            return self.varlong()
        self.need(width)
        v = int.from_bytes(self.b[self.p:self.p + width], "big")
        self.p += width
        return v

    def f32(self):
        self.need(4)
        v = struct.unpack(">f", self.b[self.p:self.p + 4])[0]
        self.p += 4
        return v

    def f64(self):
        self.need(8)
        v = struct.unpack(">d", self.b[self.p:self.p + 8])[0]
        self.p += 8
        return v

    def string(self, pools=None, string_type=None):
        enc = self.u8()
        if enc == 0:
            return None
        if enc == 1:
            return ""
        if enc == 2:
            key = self.varlong() if self.c else self.int_(8)
            return ("cpool-string", key)
        if enc in (3, 5):
            n = self.int_(4)
            self.need(n)
            raw = bytes(self.b[self.p:self.p + n])
            self.p += n
            return raw.decode("utf-8" if enc == 3 else "latin-1", errors="replace")
        if enc == 4:
            n = self.int_(4)
            if n > self.end - self.p:
                raise Err(f"char array length {n} exceeds remaining {self.end - self.p}")
            chars = [self.int_(2) for _ in range(n)]
            return "".join(chr(c) if c < 0x110000 else "�" for c in chars)
        raise Err(f"unknown string encoding {enc} at {self.p - 1}")


class Cls:
    def __init__(self, attrs):
        self.id = int(attrs.get("id", "-1"))
        self.name = attrs.get("name")
        self.super = attrs.get("superType")
        self.simple = attrs.get("simpleType") == "true"
        self.fields = []
        self.annotations = []


def read_element(r, strings, depth=0):
    if depth > MAX_DEPTH:
        raise Err("metadata element tree too deep")
    name = strings[r.int_(4)]
    nattr = r.int_(4)
    attrs = {}
    for _ in range(nattr):
        k = strings[r.int_(4)]
        v = strings[r.int_(4)]
        attrs[k] = v
    nchild = r.int_(4)
    children = [read_element(r, strings, depth + 1) for _ in range(nchild)]
    return (name, attrs, children)


def build_classes(root):
    classes = {}
    name, _, children = root
    assert name == "root", name
    for (cn, _, cc) in children:
        if cn != "metadata":
            continue
        for (en, ea, ec) in cc:
            if en != "class":
                continue
            c = Cls(ea)
            for (fn, fa, fc) in ec:
                if fn == "field":
                    anns = [(int(a[1]["class"]), {k: v for k, v in a[1].items() if k != "class"}) for a in fc if a[0] == "annotation"]
                    c.fields.append({
                        "name": fa["name"],
                        "type": int(fa["class"]),
                        "cpool": fa.get("constantPool") == "true",
                        "array": fa.get("dimension") == "1",
                        "anns": anns,
                    })
                elif fn == "annotation":
                    c.annotations.append((int(fa["class"]), {k: v for k, v in fa.items() if k != "class"}))
            classes[c.id] = c
    return classes


class Chunk:
    def __init__(self, buf, start):
        self.buf = buf
        self.start = start
        h = buf[start:start + 68]
        if len(h) < 68:
            raise Err("short header")
        if h[0:4] != b"FLR\x00":
            raise Err(f"bad magic {h[0:4]!r}")
        self.major, self.minor = struct.unpack(">HH", h[4:8])
        (self.size, self.cp_off, self.meta_off, self.start_ns, self.dur_ns,
         self.start_ticks, self.tps) = struct.unpack(">7q", h[8:64])
        self.features = struct.unpack(">I", h[64:68])[0]
        self.compressed = bool(self.features & 1)
        self.end = start + self.size


def value_reader(classes, by_name, compressed):
    prim = {"boolean", "byte", "char", "short", "int", "long", "float", "double", "java.lang.String"}

    def read_type(r, tid, depth):
        if depth > MAX_DEPTH:
            raise Err("struct nesting too deep")
        c = classes.get(tid)
        if c is None:
            raise Err(f"unknown type id {tid}")
        n = c.name
        if n == "boolean":
            return r.u8() != 0
        if n == "byte":
            return r.u8()
        if n == "char":
            return r.int_(2)
        if n == "short":
            return r.int_(2)
        if n == "int":
            return r.int_(4)
        if n == "long":
            return r.int_(8)
        if n == "float":
            return r.f32()
        if n == "double":
            return r.f64()
        if n == "java.lang.String":
            return r.string()
        out = {}
        for f in c.fields:
            out[f["name"]] = read_field(r, f, depth + 1)
        return out

    def read_one(r, f, depth):
        if f["cpool"]:
            return ("ref", classes[f["type"]].name, r.varlong() if compressed else r.int_(8))
        return read_type(r, f["type"], depth)

    def read_field(r, f, depth):
        if f["array"]:
            n = r.int_(4)
            if n > r.end - r.p:
                raise Err(f"array count {n} exceeds remaining bytes {r.end - r.p}")
            return [read_one(r, f, depth) for _ in range(n)]
        return read_one(r, f, depth)

    return read_type, read_field


def main():
    path = sys.argv[1]
    max_events = 3
    max_stacks = 2
    if "--events" in sys.argv:
        max_events = int(sys.argv[sys.argv.index("--events") + 1])
    if "--stacks" in sys.argv:
        max_stacks = int(sys.argv[sys.argv.index("--stacks") + 1])
    data = open(path, "rb").read()
    if data[:2] == b"\x1f\x8b":
        data = gzip.decompress(data)
    print(f"file bytes={len(data)}")
    pos = 0
    chunk_no = 0
    while pos < len(data):
        ch = Chunk(data, pos)
        chunk_no += 1
        print(f"\n=== chunk {chunk_no} @ {pos}: version {ch.major}.{ch.minor} size={ch.size} cp_off={ch.cp_off} meta_off={ch.meta_off} "
              f"start_ns={ch.start_ns} dur_ns={ch.dur_ns} start_ticks={ch.start_ticks} ticks/s={ch.tps} "
              f"features=0x{ch.features:08x} (header bytes 64..68 = {data[pos+64:pos+68].hex()})")
        if ch.end > len(data):
            print(f"  chunk claims end {ch.end} beyond file {len(data)}: truncated")
            break
        # metadata
        r = R(data, pos + ch.meta_off, ch.end, ch.compressed)
        msize = r.int_(4)
        mstart = pos + ch.meta_off
        mtype = r.varlong() if ch.compressed else r.int_(8)
        m_st = r.int_(8)
        m_du = r.int_(8)
        m_id = r.int_(8)
        nstr = r.int_(4)
        strings = []
        encs = Counter()
        for _ in range(nstr):
            encs[r.b[r.p]] += 1
            strings.append(r.string())
        root = read_element(r, strings)
        print(f"  metadata: size={msize} type={mtype} start={m_st} dur={m_du} id={m_id} strings={nstr} string-encodings={dict(encs)} "
              f"ends at +{r.p - mstart} (declared {msize})")
        classes = build_classes(root)
        by_name = {c.name: c for c in classes.values()}
        region = [c for c in root[2] if c[0] == "region"]
        if region:
            print(f"  region attrs: {region[0][1]}")
        events = sorted([c for c in classes.values() if c.super == "jdk.jfr.Event"], key=lambda c: c.id)
        print(f"  classes={len(classes)} event types={len(events)}")
        read_type, read_field = value_reader(classes, by_name, ch.compressed)
        # checkpoints
        pools = defaultdict(dict)
        cp = pos + ch.cp_off
        seen = set()
        ncp = 0
        while True:
            if cp in seen:
                print("  checkpoint loop!")
                break
            seen.add(cp)
            ncp += 1
            r = R(data, cp, ch.end, ch.compressed)
            size = r.varlong() if ch.compressed else r.int_(4)
            r.end = cp + size
            ctype = r.varlong() if ch.compressed else r.int_(8)
            c_st = r.int_(8)
            c_du = r.int_(8)
            delta = r.int_(8)
            if delta >= 1 << 63:
                delta -= 1 << 64
            mask = r.u8()
            npools = r.int_(4)
            desc = []
            for _ in range(npools):
                tid = r.varlong() if ch.compressed else r.int_(8)
                cnt = r.int_(4)
                c = classes[tid]
                for _ in range(cnt):
                    key = r.varlong() if ch.compressed else r.int_(8)
                    val = read_type(r, tid, 0)
                    if key not in pools[c.name]:
                        pools[c.name][key] = val
                desc.append(f"{c.name}x{cnt}")
            print(f"  checkpoint @{cp - pos}: size={size} type={ctype} delta={delta} mask={mask} pools={npools} consumed={r.p - cp} "
                  f"[{', '.join(desc)}]")
            if delta == 0:
                break
            cp += delta
        # events
        r = R(data, pos + 68, ch.end, ch.compressed)
        counts = Counter()
        bad = Counter()
        shown = Counter()
        stacks_shown = 0
        while r.p < ch.end:
            ep = r.p
            size = r.varlong() if ch.compressed else r.int_(4)
            if size == 0:
                raise Err(f"zero size event at {ep}")
            etype = r.varlong() if ch.compressed else r.int_(8)
            c = classes.get(etype)
            name = c.name if c else f"#{etype}"
            counts[name] += 1
            if etype not in (0, 1) and c is not None:
                er = R(data, r.p, ep + size, ch.compressed)
                try:
                    vals = {}
                    for f in c.fields:
                        vals[f["name"]] = read_field(er, f, 0)
                    if er.p != ep + size:
                        bad[name] += 1
                    elif "--settings" in sys.argv and name == "jdk.ActiveSetting" and vals.get("name") in ("period", "throttle", "threshold", "enabled", "interval"):
                        target = classes.get(vals.get("id"))
                        tname = target.name if target else vals.get("id")
                        if any(k in str(tname) for k in ("Sample", "Monitor", "Park", "Allocation", "Exception", "CPUTime")):
                            print(f"    setting {tname}: {vals.get('name')}={vals.get('value')!r}")
                    elif shown[name] < max_events and ("Sample" in name or "Allocation" in name or "Object" in name or "Monitor" in name or "Park" in name or "Exception" in name):
                        shown[name] += 1
                        print(f"    {name} @{ep - pos} size={size}: {vals}")
                        st = vals.get("stackTrace")
                        if isinstance(st, tuple) and stacks_shown < max_stacks:
                            stacks_shown += 1
                            trace = pools["jdk.types.StackTrace"].get(st[2])
                            if trace is None:
                                print(f"      stack {st[2]} not in pool")
                            else:
                                print(f"      stack {st[2]}: truncated={trace.get('truncated')} frames={len(trace.get('frames', []))}")
                                for fr in trace.get("frames", [])[:6]:
                                    m = pools["jdk.types.Method"].get(fr["method"][2], {})
                                    cl = pools["java.lang.Class"].get(m.get("type", (0, 0, 0))[2], {}) if m else {}
                                    sym = lambda ref: pools["jdk.types.Symbol"].get(ref[2]) if isinstance(ref, tuple) else None
                                    ft = pools["jdk.types.FrameType"].get(fr["type"][2]) if isinstance(fr.get("type"), tuple) else None
                                    print(f"        {sym(cl.get('name')) if cl else None} . {sym(m.get('name')) if m else None} "
                                          f"line={fr.get('lineNumber')} bci={fr.get('bytecodeIndex')} type={ft}")
                except Err as e:
                    bad[name] += 1
                    if bad[name] < 3:
                        print(f"    decode error in {name} @{ep - pos}: {e}")
            r.p = ep + size
        print(f"  events: {sum(counts.values())} total; by type:")
        for n, k in counts.most_common():
            print(f"    {k:7d} {n}" + (f"   (BAD DECODES: {bad[n]})" if bad[n] else ""))
        for n in ["jdk.ExecutionSample", "datadog.ExecutionSample", "datadog.MethodSample", "datadog.ObjectSample", "jdk.ObjectAllocationSample",
                  "datadog.HeapLiveObject", "jdk.ThreadPark", "jdk.JavaMonitorEnter", "datadog.ExceptionSample", "jdk.OldObjectSample",
                  "jdk.ObjectAllocationInNewTLAB", "jdk.ObjectAllocationOutsideTLAB", "jdk.NativeMethodSample", "jdk.CPUTimeSample",
                  "jdk.types.StackTrace", "jdk.types.StackFrame", "jdk.types.Method", "java.lang.Class", "jdk.types.FrameType",
                  "jdk.types.ThreadState", "datadog.DatadogProfilerConfig", "jdk.types.ChunkHeader", "datadog.ExceptionCount", "datadog.Endpoint", "datadog.QueueTime", "jdk.FileRead", "jdk.SocketRead", "java.lang.Thread", "jdk.types.Package", "jdk.ActiveSetting"]:
            c = by_name.get(n)
            if c:
                fl = ", ".join(f"{f['name']}:{classes[f['type']].name}{'[]' if f['array'] else ''}{' (cp)' if f['cpool'] else ''}" +
                               (" @" + "/".join(f"{classes[a[0]].name.split('.')[-1]}={a[1].get('value','')}" for a in f["anns"] if classes[a[0]].name.split('.')[-1] in ("Timestamp", "Timespan", "DataAmount", "Unsigned", "MemoryAddress")) if f["anns"] else "")
                               for f in c.fields)
                print(f"  type {c.id} {n}{' simple' if c.simple else ''} super={c.super}: {fl}")
        if "--ids" in sys.argv:
            for tid in [int(x) for x in sys.argv[sys.argv.index("--ids") + 1].split(",")]:
                c = classes.get(tid)
                print(f"  id {tid} = {c.name if c else None} (super={c.super if c else None})")
        for pn in ["jdk.types.FrameType", "jdk.types.ThreadState", "datadog.types.ExecutionMode"]:
            if pn in pools:
                print(f"  pool {pn}: {dict(sorted(pools[pn].items()))}")
        for pn, pv in sorted(pools.items()):
            print(f"  pool size {pn}: {len(pv)}")
        pos = ch.end
    print(f"\nchunks={chunk_no}")


if __name__ == "__main__":
    main()
