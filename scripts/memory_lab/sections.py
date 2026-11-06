#!/usr/bin/env python3
"""The binary's resident pages by section (goal 11, plan 01a1a87b2e56 step 5): which of a running process's
executable's sections it holds in resident memory, so a lab says which code and data the server touched at all.

    python3 -I sections.py <pid>

For each allocated section of /proc/<pid>/exe that has bytes in the file (in the file's order), and for the bytes
of the binary's pages that lie in no section, `(headers-and-gaps)` (the ELF and program headers, the gaps between
sections):
    <name> size_kb=<the section's size> resident_kb=<its bytes on resident pages> file_kb=<of those, on the file's
    own pages> anon_kb=<on the process's own copies> windows=<its 64 KiB windows with a resident page> of=<its
    64 KiB windows>
then `total`, the same summed: its resident_kb is all the process holds of the binary (smaps' Rss of the
executable's mappings), its file_kb what of the binary RssFile counts.

How: each mapping of the executable in /proc/<pid>/maps maps a range of the file from its offset; pagemap
(Documentation/admin-guide/mm/pagemap.rst, as crate::os::pages reads it) says for each of its pages whether it's
present (bit 63) and whether it's the file's own page (bit 61) or the process's anonymous copy (a page of a private
mapping it wrote: relocated or written data). A page's bytes count for the sections they hold. A window is
fault-around's: a fault on the binary maps the pages of the 64 KiB around it, by address, that the page cache holds
(the kernel's fault_around_bytes, 65536 by default; mm/memory.c's do_fault_around), so a section with one resident
page in a window costs the process that window's pages.

Reading another process's pagemap takes what reading its smaps does: the same user (ptrace's read access). The
page frame numbers read as zero without CAP_SYS_ADMIN; the bits read here don't.
"""
from __future__ import annotations

import os
import struct
import sys

PRESENT = 1 << 63
FILE = 1 << 61
ENTRY = 8
WINDOW = 65536
SHF_ALLOC = 0x2
SHT_NOBITS = 8


def sections(path: str) -> list[tuple[str, int, int]]:
    """The allocated sections with bytes in the file: (name, file offset, size), in the file's order."""
    with open(path, "rb") as f:
        head = f.read(64)
        if len(head) < 64 or head[:4] != b"\x7fELF" or head[4] != 2 or head[5] != 1:
            raise ValueError(f"{path}: not a little-endian 64-bit ELF file")
        shoff = struct.unpack_from("<Q", head, 0x28)[0]
        shentsize, shnum, shstrndx = struct.unpack_from("<HHH", head, 0x3A)
        if shoff == 0 or shnum == 0 or shentsize < 64 or shstrndx >= shnum:
            raise ValueError(f"{path}: no section headers")
        f.seek(shoff)
        raw = f.read(shentsize * shnum)
        if len(raw) < shentsize * shnum:
            raise ValueError(f"{path}: its section headers run past its end")
        heads = [struct.unpack_from("<IIQQQQ", raw, i * shentsize) for i in range(shnum)]
        str_off, str_size = heads[shstrndx][4], heads[shstrndx][5]
        f.seek(str_off)
        names = f.read(str_size)
    out = []
    for name_off, kind, flags, _addr, off, size in heads:
        if not flags & SHF_ALLOC or kind == SHT_NOBITS or size == 0:
            continue
        end = names.find(b"\0", name_off)
        out.append((names[name_off : end if end >= 0 else len(names)].decode("utf-8", "replace"), off, size))
    out.sort(key=lambda s: s[1])
    return out


def mappings(pid: int, exe: str) -> list[tuple[int, int, int]]:
    """The process's mappings of its executable: (start, end, file offset)."""
    out = []
    with open(f"/proc/{pid}/maps") as f:
        for line in f:
            parts = line.split(None, 5)
            if len(parts) < 6 or parts[5].rstrip("\n") != exe:
                continue
            lo, hi = (int(x, 16) for x in parts[0].split("-"))
            out.append((lo, hi, int(parts[2], 16)))
    return out


def resident(pid: int, maps: list[tuple[int, int, int]], page: int) -> list[tuple[int, int, bool]]:
    """Each resident page of the executable's mappings: (its address, its file offset, whether it's the file's own)."""
    out = []
    with open(f"/proc/{pid}/pagemap", "rb") as f:
        for lo, hi, off in maps:
            f.seek(lo // page * ENTRY)
            raw = f.read((hi - lo) // page * ENTRY)
            for i, entry in enumerate(struct.unpack(f"<{len(raw) // ENTRY}Q", raw)):
                if entry & PRESENT:
                    out.append((lo + i * page, off + i * page, bool(entry & FILE)))
    return out


def windows_of(lo: int, hi: int) -> set[int]:
    """The 64 KiB windows of addresses [lo, hi)."""
    return set(range(lo // WINDOW, (hi - 1) // WINDOW + 1)) if hi > lo else set()


def tally(secs: list[tuple[str, int, int]], maps: list[tuple[int, int, int]], pages: list[tuple[int, int, bool]], page: int) -> list[dict]:
    """Each section's bytes on resident pages and its windows, then the bytes of resident pages in no section."""
    rows = []
    for name, off, size in secs:
        # The section's windows: where the mappings put its bytes.
        whole: set[int] = set()
        for lo, hi, moff in maps:
            a, b = max(off, moff), min(off + size, moff + hi - lo)
            if a < b:
                whole |= windows_of(lo + a - moff, lo + b - moff)
        rows.append(dict(name=name, off=off, size=size, file=0, anon=0, windows=set(), whole=whole))
    rest = dict(name="(headers-and-gaps)", off=0, size=0, file=0, anon=0, windows=set(), whole=set())
    for addr, start, own in pages:
        covered = 0
        for r in rows:
            a, b = max(start, r["off"]), min(start + page, r["off"] + r["size"])
            if a >= b:
                continue
            r["file" if own else "anon"] += b - a
            r["windows"] |= windows_of(addr + a - start, addr + b - start)
            covered += b - a
        if covered < page:
            rest["file" if own else "anon"] += page - covered
            rest["windows"].add(addr // WINDOW)
    return rows + [rest]


def kb(n: int) -> int:
    return (n + 1023) // 1024


def line(name: str, size: int, file: int, anon: int, windows: int, whole: int) -> str:
    return f"{name} size_kb={kb(size)} resident_kb={kb(file + anon)} file_kb={kb(file)} anon_kb={kb(anon)} windows={windows} of={whole}"


def main() -> int:
    if len(sys.argv) != 2 or not sys.argv[1].isdigit():
        print("usage: sections.py <pid>", file=sys.stderr)
        return 2
    pid = int(sys.argv[1])
    page = os.sysconf("SC_PAGE_SIZE")
    try:
        exe = os.readlink(f"/proc/{pid}/exe")
        secs = sections(f"/proc/{pid}/exe")
        maps = mappings(pid, exe)
        pages = resident(pid, maps, page)
    except (OSError, ValueError) as e:
        print(f"sections.py: process {pid}: {e}", file=sys.stderr)
        return 1
    for r in tally(secs, maps, pages, page):
        print(line(r["name"], r["size"], r["file"], r["anon"], len(r["windows"]), len(r["whole"])))
    own = sum(page for _, _, f in pages if f)
    every = set().union(*(windows_of(lo, hi) for lo, hi, _ in maps)) if maps else set()
    print(line("total", sum(s for _, _, s in secs), own, len(pages) * page - own, len({a // WINDOW for a, _, _ in pages}), len(every)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
