"""Print each occurrence of a literal in the given files with some context around it.

Usage: python3 -I ctx.py WIDTH MAXHITS PATTERN FILE...  (run with -I; reads untrusted files as data)
"""
import sys

width = int(sys.argv[1])
maxhits = int(sys.argv[2])
pat = sys.argv[3]
for path in sys.argv[4:]:
    try:
        data = open(path, "r", encoding="utf-8", errors="replace").read()
    except OSError as e:
        print("!", path, e)
        continue
    i = 0
    n = 0
    while n < maxhits:
        j = data.find(pat, i)
        if j < 0:
            break
        a = max(0, j - width)
        b = min(len(data), j + len(pat) + width)
        print("%s@%d: %s" % (path.rsplit("/", 1)[-1], j, data[a:b].replace("\n", "\\n")))
        n += 1
        i = j + len(pat)
