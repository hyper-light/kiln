#!/usr/bin/env python3
"""F-22's reusable workflow probes (docs/ci.md W22-AC03, A.8: nesting and count limits), written
into .github/workflows/ beside this file. Run it again after changing it; the files it writes are
committed with it.

- Nesting: r01.yml calls r02.yml, … r10.yml, the leaf. `reusable-depth-10.yml` enters at r02 (the
  caller and nine reusable workflows: ten levels with the caller), `reusable-depth-11.yml` at r01.
- Count: u01.yml … u51.yml, each a leaf. `reusable-count-50.yml` calls u01 to u50 once each,
  `reusable-count-51.yml` u01 to u51, `reusable-same-51.yml` u01 51 times.

    python3 -I crates/kiln-ci/tests/fixtures/probes/make_reusable.py
"""

import os

HERE = os.path.join(os.path.dirname(os.path.abspath(__file__)), ".github", "workflows")


def write(name, text):
    with open(os.path.join(HERE, name), "w") as f:
        f.write(text)


def main():
    os.makedirs(HERE, exist_ok=True)
    for i in range(1, 11):
        if i < 10:
            body = f"jobs:\n  next:\n    uses: ./.github/workflows/r{i + 1:02}.yml\n"
        else:
            body = "jobs:\n  leaf:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo \"PROBE depth.leaf=reached\"\n"
        write(f"r{i:02}.yml", f"# F-22: level {i} of the nesting probe (make_reusable.py).\nname: r{i:02}\non: workflow_call\n{body}")
    for i in range(1, 52):
        write(f"u{i:02}.yml", f"# F-22: one of the count probe's workflows (make_reusable.py).\nname: u{i:02}\non: workflow_call\njobs:\n  leaf:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo \"PROBE count.u{i:02}=ran\"\n")
    for depth, entry in [(10, "r02"), (11, "r01")]:
        write(
            f"reusable-depth-{depth}.yml",
            f"# F-22 (docs/ci.md W22-AC03, A.8): {depth} levels of workflows, this caller among them (make_reusable.py).\n"
            f"name: probe reusable depth {depth}\non: [push, workflow_dispatch]\njobs:\n  call:\n    uses: ./.github/workflows/{entry}.yml\n",
        )
    for name, calls in [("count-50", [f"u{i:02}" for i in range(1, 51)]), ("count-51", [f"u{i:02}" for i in range(1, 52)]), ("same-51", ["u01"] * 51)]:
        jobs = "".join(f"  c{n:02}:\n    uses: ./.github/workflows/{w}.yml\n" for n, w in enumerate(calls, 1))
        write(f"reusable-{name}.yml", f"# F-22 (docs/ci.md W22-AC03, A.8): {len(calls)} calls of reusable workflows, {len(set(calls))} of them different (make_reusable.py).\nname: probe reusable {name}\non: [push, workflow_dispatch]\njobs:\n{jobs}")


if __name__ == "__main__":
    main()
