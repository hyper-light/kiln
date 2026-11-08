#!/usr/bin/env python3
"""W29-AC20/AC22: write each case's source facts and verify scenarios (kiln plan 01a19a37 step 12).

The facts are synthetic and recorded here: a commit id is the SHA-1 of a label, an owner or
repository id a small number. gh actions-lock reads them through harness/interop_test.go's fake
host and kiln through its dependency-source simulator, so both see the same sources and neither
reaches a forge. Run it again after changing a case; it rewrites only facts.json and
scenarios.json.
"""

import hashlib
import json
import pathlib

CASES = pathlib.Path(__file__).resolve().parent.parent / "cases"
WORKFLOW = ".github/workflows/ci.yml"
# Upstream's test action (command_test.go's nodeActionYAML) with the `main` GitHub requires,
# which kiln's action reader requires too.
NODE = "name: Test Action\nruns:\n  using: node20\n  main: index.js\n"


def sha(label):
    return hashlib.sha1(f"kiln-interop:{label}".encode()).hexdigest()


def repository(owner_id, repo_id, groups, files=None):
    """`groups` are the releases in history order: one commit each (a group may name its commit
    first), every ref in a group at it: a tag, unless it's spelled `refs/...`."""
    commits, refs, parents = {}, {}, []
    for i, names in enumerate(groups):
        named = len(names[0]) == 40
        commit = names[0] if named else sha(f"{owner_id}/{repo_id}@{i}")
        commits[commit] = parents
        for name in names[1:] if named else names:
            refs[name if name.startswith("refs/") else f"refs/tags/{name}"] = commit
        parents = [commit]
    refs.setdefault("refs/heads/main", parents[0])
    return {"owner_id": owner_id, "repo_id": repo_id, "default_branch": "main", "refs": refs, "commits": commits, "files": {c: dict(files or {"action.yml": NODE}) for c in commits}}


OWNERS = {"actions": 101, "docker": 102, "pypa": 103, "ilammy": 104, "Swatinem": 105, "octo": 106, "azure": 107, "dtolnay": 108, "helm": 109, "PyO3": 110}
# kiln's own workflows pin full commits: each is a release here, with the tag found for it.
RELEASES = {
    "actions/cache": [["v6", "v6.1.0"]],
    "actions/checkout": [["v4"], ["v5"], ["v7.0.1"], ["3d3c42e5aac5ba805825da76410c181273ba90b1", "v4.2.2"]],
    "actions/download-artifact": [["v4", "v4.3.0"], ["9000827ccba6bdab643e8b6fd33ac0654aef8333", "v4.1.8"]],
    "actions/setup-java": [["v6.0.1"]],
    "actions/setup-node": [["v4"], ["v6.0.0"], ["v7.0.0"], ["949feb2413d6458794dcd2491c4babbbce0c15c1", "v4.1.0"]],
    "actions/setup-python": [["v5", "v5.6.0"]],
    "actions/upload-artifact": [["v4", "v4.6.2"], ["v7.0.2"], ["cf430e030ddbb5b0abf93d22962f4752f3646cd9", "v4.4.3"]],
    "azure/setup-helm": [["v4"]],
    "docker/build-push-action": [["v6"]],
    "docker/login-action": [["v3"]],
    "docker/setup-buildx-action": [["v3"]],
    "dtolnay/rust-toolchain": [["refs/heads/stable"], ["refs/heads/nightly", "refs/heads/main"]],
    "helm/kind-action": [["v1"]],
    "ilammy/setup-nasm": [["v1.5.2"]],
    "PyO3/maturin-action": [["v1"]],
    "pypa/gh-action-pypi-publish": [["v1.14.2", "refs/heads/release/v1"]],
    "Swatinem/rust-cache": [["v2"], ["6323deb102c322ba6fcbdcafc7e3dddab59af2b6", "v2.7.5"]],
}
HOSTED = {nwo: repository(OWNERS[nwo.split("/")[0]], 2001 + i, groups) for i, (nwo, groups) in enumerate(sorted(RELEASES.items()))}
HOSTED["octo/outer"] = repository(106, 2101, [["v1"]], {"action.yml": "name: Outer\nruns:\n  using: composite\n  steps:\n    - uses: octo/inner@v2\n    - uses: actions/checkout@v5\n"})
HOSTED["octo/inner"] = repository(106, 2102, [["v2"]], {"action.yml": "name: Inner\nruns:\n  using: composite\n  steps:\n    - uses: actions/checkout@v5\n    - uses: octo/monorepo/tools/setup@v1\n"})
HOSTED["octo/monorepo"] = repository(106, 2103, [["v1"]], {"tools/setup/action.yml": NODE, "tools/lint/action.yml": NODE})

# Upstream's own facts for testdata/golden-json (check_json_golden_test.go), with the ids its
# actions.lock records and the transitive actions/cache@v4 at cacheSHA.
SETUP_GO = "name: Setup Go\nruns:\n  using: composite\n  steps:\n    - uses: actions/cache@v4\n    - uses: helper/only-transitive@v1\n"
GOLDEN = {
    "actions/checkout": repository(1, 1, [["a" * 40, "v6"]]),
    "actions/setup-go": repository(2, 2, [["b" * 40, "v6"]], {"action.yml": SETUP_GO}),
    "actions/cache": repository(3, 3, [["c" * 40, "v3", "v4"]]),
    "old/dead": repository(4, 4, [["d" * 40, "v1"]]),
    "helper/only-transitive": repository(5, 5, [["e" * 40, "v1"]]),
}


def uses(case):
    found = set()
    for path in (CASES / case / "tree").rglob("*.yml"):
        for line in path.read_text().splitlines():
            text = line.strip().removeprefix("- ")
            if text.startswith("uses:") and not text.removeprefix("uses:").strip().startswith(("./", "docker://")):
                found.add(text.removeprefix("uses:").strip().split("@")[0])
    return found


def facts(case, table, extra=()):
    names = {"/".join(n.split("/")[:2]) for n in uses(case) | set(extra)}
    wanted, todo = {}, sorted(names)
    while todo:
        name = todo.pop()
        if name in wanted:
            continue
        wanted[name] = table[name]
        for files in table[name]["files"].values():
            for text in files.values():
                for line in text.splitlines():
                    if line.strip().startswith("- uses:"):
                        todo.append("/".join(line.split("uses:")[1].strip().split("@")[0].split("/")[:2]))
    return {"repositories": dict(sorted(wanted.items()))}


def locked(nwo, tag):
    return HOSTED[nwo]["refs"][f"refs/tags/{tag}"]


def move(nwo, tag, label, parents):
    to = sha(f"{nwo}@{label}")
    return {"move": {"repository": nwo, "ref": f"refs/tags/{tag}", "to": to, "parents": parents, "files": {"action.yml": NODE}}}


def edit(old, new):
    return {"workflow": {"path": WORKFLOW, "from": old, "to": new}}


FRESH = [{"name": "fresh", "changes": []}]
# One cause each, as gh actions-lock's troubleshooting guide gives it; kiln's verify names the
# first refusal of a workflow, so a scenario changes one fact or one line.
DOCUMENTED = FRESH + [
    {"name": "not-pinned", "changes": [edit("      - uses: actions/setup-java@v6.0.1\n", "      - uses: actions/setup-node@v6.0.0\n      - uses: actions/setup-java@v6.0.1\n")]},
    {"name": "ref-changed", "changes": [edit("      - uses: actions/cache@v6.1.0\n", "      - uses: actions/cache@v6\n")]},
    {"name": "stale", "changes": [edit("      - uses: actions/upload-artifact@v4\n", "      - uses: actions/cache@v6.1.0\n")]},
    {"name": "unreachable-pin", "changes": [move("actions/cache", "v6.1.0", "unrelated", [])]},
    {"name": "tag-commit-mismatch", "changes": [move("actions/setup-java", "v6.0.1", "descendant", [locked("actions/setup-java", "v6.0.1")])]},
    {"name": "ref-moved", "changes": [move("actions/checkout", "v5", "descendant", [locked("actions/checkout", "v5")])]},
    {"name": "repo-replaced", "changes": [{"identity": {"repository": "actions/upload-artifact", "owner_id": 101, "repo_id": 9999}}]},
]

CASE_TABLE = {
    "f02-focal": (HOSTED, (), FRESH),
    "f02-mantle": (HOSTED, (), FRESH),
    "f02-hyper-raft": (HOSTED, ("actions/setup-node",), DOCUMENTED),
    "f02-slates": (HOSTED, (), FRESH),
    "f02-kiln": (HOSTED, (), FRESH),
    "nested-composite": (HOSTED, (), FRESH),
    "upstream-golden-json": (GOLDEN, ("old/dead",), [{"name": "given", "lock": "given", "changes": []}] + FRESH),
}

for case, (table, extra, scenarios) in CASE_TABLE.items():
    (CASES / case / "facts.json").write_text(json.dumps(facts(case, table, extra), indent=2, sort_keys=True) + "\n")
    (CASES / case / "scenarios.json").write_text(json.dumps(scenarios, indent=2) + "\n")
