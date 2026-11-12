#!/usr/bin/env python3
"""Plan step 26, step 3 (kiln plan 01a1ce705e55753182217afdfe607de9): each guard the unpacker's `alone`
member adds, removed one at a time. Each mutant must make the test named beside it fail. The module is
rewritten with its own text after each (a new mtime, so cargo rebuilds it rather than keep the mutant),
and the module's whole suite runs once more at the end, unmutated.

Run on a runner, from the repository root (the crate's unpack module at crates/kiln/src/agentfiles/):
    python3 -I docs/research/s26-agy-agentfile/step3/mutate.py [name ...]
or from a scratch crate holding the module alone as src/unpack.rs (MUTATE_SCRATCH=1).
"""

import os
import subprocess
import sys

SCRATCH = os.environ.get("MUTATE_SCRATCH") == "1"
SRC = "src/unpack.rs" if SCRATCH else "crates/kiln/src/agentfiles/unpack.rs"
PREFIX = "unpack::tests::" if SCRATCH else "agentfiles::unpack::tests::"
BESIDE = "an_entry_beside_a_member_held_alone_refuses_the_archive_and_leaves_nothing"
FIELD = "alone_is_a_members_field"

# (name, the test that must fail, the text removed, the text put in its place)
MUTANTS = [
    ("hostile-unchecked", BESIDE,
     "                hostile(&entry, kind).map_err(|why| (Some(name.clone()), why))?;\n", ""),
    ("extra-after-taken", BESIDE,
     "                    Pass::Picked => return Err(extra(name)),", "                    Pass::Picked => {}"),
    ("extra-before-taken", BESIDE,
     "            Pass::Beside(name) => return Err(extra(name)),", "            Pass::Beside(_) => {}"),
    ("duplicate-taken", BESIDE,
     "            Pass::Picked => return Err(at(Why::Duplicate)),", "            Pass::Picked => {}"),
    ("beside-passed-over", BESIDE,
     "            if member.alone {", "            if false && member.alone {"),
    ("stops-after-member", BESIDE,
     "        if !member.alone {", "        if true || !member.alone {"),
    ("written-member-kept", BESIDE,
     "            let _ = if p.is_dir() { fs::remove_dir(p) } else { fs::remove_file(p) };", "            let _ = p;"),
    ("missing-told-as-extra", BESIDE,
     "        Pass::Nothing | Pass::Beside(_) => Err(at(Why::Missing)),",
     "        Pass::Nothing => Err(at(Why::Missing)),\n        Pass::Beside(name) => Err(extra(name)),"),
    ("alone-ignored", FIELD,
     "                alone: f.alone.unwrap_or(false),", "                alone: false,"),
    ("alone-not-a-member-field", FIELD,
     " || f.alone.is_some();", ";"),
]


def cargo_test(test):
    env = dict(os.environ)
    env["PATH"] = os.path.expanduser("~/.cargo/bin") + os.pathsep + env.get("PATH", "")
    env.setdefault("CARGO_BUILD_JOBS", "4")
    env["CARGO_PROFILE_DEV_DEBUG"] = "line-tables-only"
    env["KILN_UI_SERVER"] = "off"
    cmd = ["nice", "-n", "10", "cargo", "test"] + ([] if SCRATCH else ["--locked", "-p", "kiln", "--lib"]) + [test]
    run = subprocess.run(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    return run.returncode, run.stdout


def main(names):
    with open(SRC, encoding="utf-8") as f:
        original = f.read()
    chosen = [m for m in MUTANTS if not names or m[0] in names]
    results = []
    try:
        for name, test, old, new in chosen:
            if original.count(old) != 1:
                results.append((name, test, "NOT-APPLIED", f"{original.count(old)} matches"))
                print(f"{name}: NOT-APPLIED ({original.count(old)} matches)", flush=True)
                continue
            with open(SRC, "w", encoding="utf-8") as f:
                f.write(original.replace(old, new))
            code, out = cargo_test(PREFIX + test)
            failed = f"{PREFIX}{test} ... FAILED" in out or "test result: FAILED" in out
            built = "test result:" in out
            verdict = "KILLED" if code != 0 and failed else ("NOT-BUILT" if not built else "SURVIVED")
            tail = [l.strip() for l in out.splitlines() if "panicked" in l or "assertion" in l or "Error:" in l or l.startswith("error")][:3]
            results.append((name, test, verdict, " | ".join(tail)))
            print(f"{name}: {verdict} (exit {code})", flush=True)
            if verdict != "KILLED":
                print(out[-6000:], flush=True)
    finally:
        with open(SRC, "w", encoding="utf-8") as f:
            f.write(original)
    print("\n== mutants")
    for name, test, verdict, why in results:
        print(f"{verdict:12} {name:26} {test}  {why}")
    code, out = cargo_test(PREFIX.rstrip(":").rsplit("::", 1)[0] if not SCRATCH else "unpack")
    print("\n== unmutated: exit", code)
    print("\n".join(l for l in out.splitlines() if l.startswith("test ") or l.startswith("test result:")))
    killed = sum(1 for r in results if r[2] == "KILLED")
    print(f"\n== {killed} of {len(results)} mutants killed; unmutated suite exit {code}")
    return 0 if killed == len(results) and code == 0 else 1


if __name__ == "__main__":
    sys.exit(main(set(sys.argv[1:])))
