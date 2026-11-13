# Each mutant removes or bends one guard of a tree's `skip` in src/unpack.rs; applied alone, exactly once.
MUTANTS = [
    ("M1 a skipped file is written", """                if skip.take(&rel) {
                    continue;
                }
""", """                let _ = skip.take(&rel);
"""),
    ("M2 no Missing for a skip path the archive lacks", """    if let Some(shown) = skip.missing() {
        return Err((Some(shown.to_owned()), Why::Missing));
    }
""", ""),
    ("M3 skipped files left out of the totals", """        entries = entries.saturating_add(1);
""", """        if let Goes::File { rel, .. } = &goes {
            if skip.take(rel) {
                continue;
            }
        }
        entries = entries.saturating_add(1);
"""),
    ("M4 a directory at a skip path taken", """                if skip.lists(&rel) {
                    return Err(at_fault(Why::Kind(EntryType::Directory.as_byte())));
                }
""", ""),
    ("M5 skip paths not held inside the tree", """            if escapes(path) || names(path).next().is_none() {
                return Err((Some(shown.clone()), Why::Escapes));
            }
""", ""),
    ("M6 a skip path named twice taken", """            if skip.insert(clean(path), (shown.as_str(), false)).is_some() {
                return Err((Some(shown.clone()), Why::Duplicate));
            }
""", """            skip.insert(clean(path), (shown.as_str(), false));
"""),
    ("M7 skip not a tree's field when parsed", """ || f.skip.is_some();""", """;"""),
]

import pathlib, subprocess, sys
src = pathlib.Path("src/unpack.rs")
orig = src.read_text()
failed_all = True
for name, old, new in MUTANTS:
    if orig.count(old) != 1:
        print(f"{name}: target occurs {orig.count(old)} times, not once"); failed_all = False; continue
    src.write_text(orig.replace(old, new))
    r = subprocess.run(["cargo", "test", "--quiet"], capture_output=True, text=True)
    out = r.stdout + r.stderr
    fails = sorted({l.split()[1] for l in out.splitlines() if l.startswith("test ") and l.rstrip().endswith("FAILED")})
    if not fails:
        fails = sorted({l.strip() for l in out.splitlines() if l.startswith("    unpack::tests::")})
    print(f"{name}: exit {r.returncode}; failing: {', '.join(fails) or '-'}")
    if r.returncode == 0:
        failed_all = False
    if r.returncode != 0 and not fails:
        print("\n".join(out.splitlines()[-15:]))
src.write_text(orig)
r = subprocess.run(["cargo", "test", "--quiet"], capture_output=True, text=True)
print(f"restored: exit {r.returncode}")
print("MUTANTS_ALL_CAUGHT" if failed_all and r.returncode == 0 else "MUTANTS_NOT_ALL_CAUGHT")
