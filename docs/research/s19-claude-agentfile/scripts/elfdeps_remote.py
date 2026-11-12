# ELF64 LE over HTTP ranges: PT_INTERP, DT_NEEDED, DT_VERNEED library versions, DT_RPATH/RUNPATH.
import struct, sys, urllib.request
class R:
    def __init__(self, url): self.url, self.cache = url, {}
    def page(self, n):
        if n not in self.cache:
            q = urllib.request.Request(self.url, headers={'Range': f'bytes={n*65536}-{n*65536+65535}'})
            with urllib.request.urlopen(q, timeout=60) as r:
                assert r.status == 206, r.status
                self.cache[n] = r.read()
        return self.cache[n]
    def read(self, o, size):
        out = b''
        while len(out) < size:
            p = self.page((o + len(out)) // 65536); i = (o + len(out)) % 65536
            out += p[i:i + size - len(out)]
        return out
    def cstr(self, o):
        out = b''
        while True:
            c = self.read(o + len(out), 64); k = c.find(b'\0')
            if k >= 0: return (out + c[:k]).decode()
            out += c
def main(url):
    r = R(url); h = r.read(0, 64)
    e_phoff, = struct.unpack_from('<Q', h, 0x20); e_phentsize, e_phnum = struct.unpack_from('<HH', h, 0x36)
    ph = r.read(e_phoff, e_phentsize * e_phnum); loads, dyn = [], None
    for i in range(e_phnum):
        p_type, p_flags, p_offset, p_vaddr, p_paddr, p_filesz, p_memsz, p_align = struct.unpack_from('<IIQQQQQQ', ph, i * e_phentsize)
        if p_type == 1: loads.append((p_vaddr, p_offset, p_filesz))
        if p_type == 3: print('INTERP', r.read(p_offset, p_filesz).rstrip(b'\0').decode())
        if p_type == 2: dyn = (p_offset, p_filesz)
    def off(a):
        for va, fo, fs in loads:
            if va <= a < va + fs: return fo + (a - va)
        raise ValueError(hex(a))
    raw = r.read(dyn[0], dyn[1]); tags = []
    for j in range(0, len(raw), 16):
        t, v = struct.unpack_from('<qQ', raw, j)
        if t == 0: break
        tags.append((t, v))
    st = off(next(v for t, v in tags if t == 5)); s = lambda o: r.cstr(st + o)
    print('NEEDED', ' '.join(s(v) for t, v in tags if t == 1))
    for t, v in tags:
        if t in (15, 29): print('RPATH' if t == 15 else 'RUNPATH', s(v))
    vn = [v for t, v in tags if t == 0x6ffffffe]
    if vn:
        o = off(vn[0])
        while True:
            vn_version, vn_cnt, vn_file, vn_aux, vn_next = struct.unpack('<HHIII', r.read(o, 16))
            a = o + vn_aux; vs = []
            for _ in range(vn_cnt):
                vna_hash, vna_flags, vna_other, vna_name, vna_next = struct.unpack('<IHHII', r.read(a, 16)); vs.append(s(vna_name)); a += vna_next
            g = sorted({x for x in vs if x.startswith('GLIBC_2.')}, key=lambda x: tuple(int(y) for y in x[6:].split('.')))
            print('VERNEED', s(vn_file), 'max', g[-1] if g else '-', 'n', len(vs))
            if vn_next == 0: break
            o += vn_next
main(sys.argv[1])
