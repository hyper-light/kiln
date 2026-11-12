# ELF64 little-endian facts of one file: machine, type, PT_INTERP, DT_NEEDED, DT_FLAGS_1's PIE bit.
# Plan step 20 [kiln] research (kiln plan 01a1ce705d7076648c1d9f9ee26214d3): whether OpenAI's
# Linux Codex binaries need a loader or shared libraries from the image they run in.
import struct
import sys

MACHINES = {62: "x86_64", 183: "aarch64"}
TYPES = {2: "EXEC", 3: "DYN"}
PT_LOAD, PT_DYNAMIC, PT_INTERP = 1, 2, 3
DT_NEEDED, DT_STRTAB, DT_FLAGS_1 = 1, 5, 0x6FFFFFFB
DF_1_PIE = 0x08000000


def main(path):
    with open(path, "rb") as f:
        data = f.read()
    if data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
        return f"NOT_ELF64LE {data[:6]!r}"
    e_type, e_machine = struct.unpack_from("<HH", data, 16)
    e_phoff = struct.unpack_from("<Q", data, 32)[0]
    e_phentsize, e_phnum = struct.unpack_from("<HH", data, 54)
    loads, interp, dynamic = [], None, None
    for i in range(e_phnum):
        off = e_phoff + i * e_phentsize
        p_type, _flags, p_offset, p_vaddr, _paddr, p_filesz = struct.unpack_from("<IIQQQQ", data, off)
        if p_type == PT_LOAD:
            loads.append((p_vaddr, p_offset, p_filesz))
        elif p_type == PT_INTERP:
            interp = data[p_offset:p_offset + p_filesz].rstrip(b"\0").decode()
        elif p_type == PT_DYNAMIC:
            dynamic = (p_offset, p_filesz)

    def at(vaddr):
        for v, o, n in loads:
            if v <= vaddr < v + n:
                return o + (vaddr - v)
        return None

    needed, strtab, flags1 = [], None, 0
    if dynamic:
        off, size = dynamic
        entries = []
        for j in range(size // 16):
            tag, val = struct.unpack_from("<qQ", data, off + j * 16)
            if tag == 0:
                break
            entries.append((tag, val))
        for tag, val in entries:
            if tag == DT_STRTAB:
                strtab = at(val)
            elif tag == DT_FLAGS_1:
                flags1 = val
        for tag, val in entries:
            if tag == DT_NEEDED and strtab is not None:
                end = data.index(b"\0", strtab + val)
                needed.append(data[strtab + val:end].decode())
    return (
        f"machine={MACHINES.get(e_machine, e_machine)} type={TYPES.get(e_type, e_type)} "
        f"interp={interp or 'none'} dynamic={'yes' if dynamic else 'no'} "
        f"needed={','.join(needed) or 'none'} pie={'yes' if flags1 & DF_1_PIE else 'no'}"
    )


if __name__ == "__main__":
    print(main(sys.argv[1]))
