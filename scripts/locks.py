#!/usr/bin/env python3
"""The locks on kiln's hot path (goal 5 of the API runtime program, plan 01a1a59c2aae).

  scripts/locks.py census-tree --to DIR [--from TREE] [--ui UI_DIST]
      A scratch copy of the tree (git's files, tracked and untracked but not ignored) whose crates
      count every Mutex and RwLock they take (scripts/locks/census.rs, as each crate's `census`
      module): kiln, kiln-http and Quiver. Built in release, its /metrics says, for each place a lock
      is taken, how often, how often it was already held and how long those waited (`kiln_lock_*`);
      kiln-lab keeps that for each round's window and fill (`Round::locks`). The store's calls are
      counted at their callers too (`store_call`), with how long each took. Lines keep their numbers.
  scripts/locks.py rank RUN.json [...] [--json]
      The locks a lab run's rounds took, one row a lock (the sites that took one lock, by its
      address, together), ranked by how often the window took it, with the fill's beside it; then
      the store's calls by caller.
  scripts/locks.py list
      Every Mutex and RwLock the instrumented crates declare, with where it's taken.

Exit status: 0 done, 1 failed (the message says why), 2 usage.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The crates the census counts: (their source folder, the path their code names the census by).
CRATES = [
    ("crates/kiln/src", "crate::census"),
    ("crates/kiln-http/src", "crate::census"),
    ("vendor/quiver/crates/quiver/src", "crate::census"),
]
# Where each crate's census reaches /metrics: kiln renders its own and the others'.
RENDER_ANCHOR = ("crates/kiln/src/metrics.rs", "    crate::pool_health::render(&mut o);")
RENDER_CALLS = "crate::census::render(&mut o); kiln_http::census::render(&mut o); quiver::census::render(&mut o);"
STORE_CALL = (
    "crates/kiln/src/store.rs",
    "    pub async fn call<R, F>(&self, f: F) -> anyhow::Result<R>\n",
    "    #[track_caller] pub fn call<R, F>(&self, f: F) -> impl std::future::Future<Output = anyhow::Result<R>> + '_ where F: FnOnce(&mut Connection) -> anyhow::Result<R> + Send + 'static, R: Send + 'static"
    " { let (at, began) = (std::panic::Location::caller(), std::time::Instant::now()); let fut = self.call_counted(f); async move { let out = fut.await; crate::census::store_call(at, began.elapsed()); out } }"
    " pub async fn call_counted<R, F>(&self, f: F) -> anyhow::Result<R>\n",
)

LOCK_NAMES = ("Mutex", "RwLock")
LOCK_CALL = re.compile(r"\.(lock|read|write|try_lock|try_read|try_write)\(")
RECEIVER = re.compile(r"([A-Za-z_][\w]*(?:(?:\.|::)[A-Za-z_][\w]*|\(\)|\.\d+)*)\s*$")


def fail(msg: str) -> None:
    print(f"locks.py: {msg}", file=sys.stderr)
    sys.exit(1)


# ------------------------------------------------------------------------------------ the census tree


def rewrite_use_list(line: str, crate_path: str) -> str:
    """`use std::sync::{Arc, Mutex};` and `use parking_lot::{Condvar, Mutex};`: the locks named from
    the census, the rest as they were, on the same line."""

    def sub(m: re.Match[str]) -> str:
        indent, family, names = m.group(1), m.group(2), [n.strip() for n in m.group(3).split(",") if n.strip()]
        locks = [n for n in names if n in LOCK_NAMES]
        if not locks:
            return m.group(0)
        rest = [n for n in names if n not in LOCK_NAMES]
        module = "pl" if family == "parking_lot" else "st"
        out = f"{indent}use {crate_path}::{module}::{{{', '.join(locks)}}};"
        if rest:
            out = f"{indent}use {family}::{{{', '.join(rest)}}}; " + out.lstrip()
        return out

    return re.sub(r"^(\s*)(?:pub(?:\([^)]*\))? )?use (std::sync|parking_lot)::\{([^}]*)\};", sub, line)


def rewrite_file(text: str, crate_path: str) -> str:
    out = []
    for line in text.split("\n"):
        line = rewrite_use_list(line, crate_path)
        line = re.sub(r"\bparking_lot::(Mutex|RwLock)\b", rf"{crate_path}::pl::\1", line)
        line = re.sub(r"\bstd::sync::(Mutex|RwLock)\b", rf"{crate_path}::st::\1", line)
        # A helper that takes a lock for its caller counts at its caller's place.
        if re.match(r"^\s*(pub(\([^)]*\))? )?(const )?fn \w+", line) and "async fn" not in line and re.search(r"->[^{]*Guard\b", line):
            line = re.sub(r"^(\s*)", r"\1#[track_caller] ", line, count=1)
        out.append(line)
    return "\n".join(out)


def tree_files(tree: Path) -> list[str]:
    out = subprocess.run(["git", "-C", str(tree), "ls-files", "-z", "--cached", "--others", "--exclude-standard"], check=True, capture_output=True).stdout
    return [f for f in out.decode().split("\0") if f]


def census_tree(src: Path, dest: Path, ui: Path | None) -> None:
    if dest.exists():
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    for rel in tree_files(src):
        a, b = src / rel, dest / rel
        if not a.is_file():
            continue
        b.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(a, b)
    census = (Path(__file__).resolve().parent / "locks" / "census.rs").read_text()
    # The census without its parking_lot wrappers, for a crate that doesn't depend on parking_lot.
    without_pl = re.sub(r"// census:parking_lot.*?// census:end\n", "", census, flags=re.S)
    for folder, crate_path in CRATES:
        root = dest / folder
        if not root.is_dir():
            fail(f"no {folder} in {src}")
        for f in sorted(root.rglob("*.rs")):
            text = f.read_text()
            new = rewrite_file(text, crate_path)
            if new != text:
                f.write_text(new)
        manifest = (root.parent / "Cargo.toml").read_text()
        (root / "census.rs").write_text(census if re.search(r"(?m)^parking_lot\s*=", manifest) else without_pl)
        lib = root / "lib.rs"
        lib.write_text(lib.read_text().rstrip("\n") + "\n#[allow(warnings)]\npub mod census;\n")
    path, anchor = RENDER_ANCHOR
    m = dest / path
    text = m.read_text()
    if anchor not in text:
        fail(f"{path} no longer has `{anchor.strip()}`: say where the census renders")
    m.write_text(text.replace(anchor, f"{anchor} {RENDER_CALLS}", 1))
    # The store's calls, counted at their callers (`store_call`): which of kiln's paths use its one
    # connection, and how long each call took. `call` is a function returning the call's future, so
    # it can say where it was called from (#[track_caller]); on the line it was on.
    store = dest / STORE_CALL[0]
    text = store.read_text()
    if STORE_CALL[1] not in text:
        fail(f"{STORE_CALL[0]} no longer has `{STORE_CALL[1].strip()}`: say how its calls are counted")
    store.write_text(text.replace(STORE_CALL[1], STORE_CALL[2], 1))
    if ui is not None:
        shutil.copytree(ui, dest / "ui" / "dist", dirs_exist_ok=True)
    print(f"census tree at {dest}")


# ------------------------------------------------------------------------------------------ ranking


def receiver_at(tree: Path, site: str) -> tuple[str, str]:
    """The expression a site takes a lock of (`self.agents`), and its line."""
    file, line, col = site.rsplit(":", 2)
    try:
        text = (tree / file).read_text().split("\n")[int(line) - 1]
    except (OSError, IndexError, ValueError):
        return "?", ""
    calls = list(LOCK_CALL.finditer(text))
    col0 = int(col) - 1
    pick = next((c for c in calls if c.start() + 1 >= col0), calls[-1] if calls else None)
    if pick is None:
        return "?", text.strip()
    m = RECEIVER.search(text[: pick.start()])
    return (m.group(1) if m else "?"), text.strip()


def declarations(tree: Path) -> list[dict]:
    """Every Mutex or RwLock field, static or local the instrumented crates declare."""
    out = []
    decl = re.compile(r"\b(?:pub(?:\([^)]*\))?\s+)?(?:static\s+|let\s+(?:mut\s+)?)?([A-Za-z_]\w*)\s*:\s*([^=;]*\b(?:Mutex|RwLock)\s*<)")
    for folder, _ in CRATES:
        for f in sorted((tree / folder).rglob("*.rs")):
            if f.name == "census.rs":
                continue
            for i, line in enumerate(f.read_text().split("\n"), 1):
                s = line.strip()
                if s.startswith("//") or s.startswith("use "):
                    continue
                for m in decl.finditer(line):
                    out.append({"file": str(f.relative_to(tree)), "line": i, "name": m.group(1), "type": m.group(2).strip()})
    return out


def rank(runs: list[Path], tree: Path) -> list[dict]:
    rounds = []
    for r in runs:
        data = json.loads(r.read_text())
        window = float(data.get("profile", {}).get("window_s") or 60)
        rounds.extend((x, window) for x in data.get("rounds", []) if x.get("locks"))
    if not rounds:
        fail("no round with a lock census: run kiln-lab on a census build (`locks.py census-tree`)")
    # Each site's figures per round, then its median over the rounds; and the addresses it took.
    sites: dict[str, dict] = {}
    for rd, secs in rounds:
        fill = rd.get("fill_s") or 1.0
        for key, seen in rd["locks"].items():
            s = sites.setdefault(key, {"taken": [], "contended": [], "wait": [], "fill": [], "fill_wait": [], "max": 0.0, "addrs": set(), "kind": seen["kind"]})
            s["taken"].append(seen["taken"] / secs)
            s["contended"].append(seen["contended"] / secs)
            s["wait"].append(seen["wait_s"] * 1000 / secs)
            s["fill"].append(seen["fill_taken"] / fill)
            s["fill_wait"].append(seen["fill_wait_s"] * 1000 / fill)
            s["max"] = max(s["max"], seen["max_wait_s"] * 1000)
            s["addrs"].update(a for a in seen.get("addrs", "").split() if a)
    n = len(rounds)
    med = lambda xs: statistics.median(xs + [0.0] * (n - len(xs)))  # noqa: E731 - a site a round didn't take counts 0 there
    # The store's calls by caller are a table of their own (`store_calls`).
    calls = {k: s for k, s in sites.items() if s["kind"] == "store_call"}
    sites = {k: s for k, s in sites.items() if s["kind"] != "store_call"}
    # Sites that took one lock (an address in common) are one lock: union-find over addresses. A lock
    # made a thousand times over (one each session) has as many addresses, and its sites only the first
    # few each: they join by their receiver too.
    parent: dict[str, str] = {k: k for k in sites}

    def find(k: str) -> str:
        while parent[k] != k:
            parent[k] = parent[parent[k]]
            k = parent[k]
        return k

    by_addr: dict[str, str] = {}
    for k, s in sites.items():
        for a in s["addrs"]:
            if a in by_addr:
                parent[find(k)] = find(by_addr[a])
            else:
                by_addr[a] = k
    groups: dict[str, list[str]] = {}
    for k in sites:
        groups.setdefault(find(k), []).append(k)
    decls = declarations(tree)
    rows = []
    for members in groups.values():
        detail = []
        for k in members:
            site, _ = k.split(" ", 1)
            recv, text = receiver_at(tree, site)
            s = sites[k]
            detail.append({"site": site, "kind": s["kind"], "receiver": recv, "line": text, "taken_per_s": med(s["taken"]), "fill_per_s": med(s["fill"])})
        detail.sort(key=lambda d: (-d["taken_per_s"], -d["fill_per_s"]))
        recvs = [d["receiver"] for d in detail if d["receiver"] != "?"]
        name = max(set(recvs), key=recvs.count) if recvs else "?"
        field = name.split(".")[-1].split("::")[-1].rstrip("()")
        files = {d["site"].rsplit(":", 2)[0] for d in detail}
        cands = [d for d in decls if d["name"] == field]
        near = [d for d in cands if d["file"] in files] or cands
        rows.append(
            {
                "lock": name,
                "declared": [f"{d['file']}:{d['line']}" for d in near[:3]],
                "files": sorted(files),
                "taken_per_s": sum(med(sites[k]["taken"]) for k in members),
                "contended_per_s": sum(med(sites[k]["contended"]) for k in members),
                "wait_ms_per_s": sum(med(sites[k]["wait"]) for k in members),
                "max_wait_ms": max(sites[k]["max"] for k in members),
                "fill_per_s": sum(med(sites[k]["fill"]) for k in members),
                "fill_wait_ms_per_s": sum(med(sites[k]["fill_wait"]) for k in members),
                "sites": detail,
            }
        )
    rows.sort(key=lambda r: (-r["taken_per_s"], -r["fill_per_s"]))
    store = []
    for k, s in calls.items():
        site, _ = k.split(" ", 1)
        per_s, ms_per_s = med(s["taken"]), med(s["wait"])
        store.append(
            {
                "site": site,
                "fn": enclosing_fn(tree, site),
                "calls_per_s": per_s,
                "mean_ms": ms_per_s / per_s if per_s > 0 else 0.0,
                "busy_ms_per_s": ms_per_s,
                "max_ms": s["max"],
                "fill_per_s": med(s["fill"]),
            }
        )
    store.sort(key=lambda r: (-r["calls_per_s"], -r["fill_per_s"]))
    return rows, store


def enclosing_fn(tree: Path, site: str) -> str:
    """The function a site is in (the nearest `fn name` above it)."""
    file, line, _ = site.rsplit(":", 2)
    try:
        lines = (tree / file).read_text().split("\n")[: int(line)]
    except (OSError, ValueError):
        return "?"
    for text in reversed(lines):
        m = re.search(r"\bfn\s+(\w+)", text)
        if m:
            return m.group(1)
    return "?"


def markdown(rows: list[dict], store: list[dict], rounds: int) -> str:
    out = [
        f"Locks the lab's load took, ranked by acquisitions a second in the window (the median of {rounds} rounds), the fill's beside them.",
        "",
        "| # | lock | where | taken/s | held when asked/s | waited ms/s | longest wait ms | fill: taken/s | fill: waited ms/s | sites |",
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |",
    ]
    for i, r in enumerate(rows, 1):
        where = ", ".join(f"`{d}`" for d in r["declared"]) or ", ".join(f"`{f}`" for f in r["files"])
        top = "; ".join(f"`{d['site'].split('/')[-1]}` {d['taken_per_s']:.1f}" for d in r["sites"][:3])
        more = f" (+{len(r['sites']) - 3})" if len(r["sites"]) > 3 else ""
        out.append(
            f"| {i} | `{r['lock']}` | {where} | {r['taken_per_s']:.1f} | {r['contended_per_s']:.2f} | {r['wait_ms_per_s']:.3f} | {r['max_wait_ms']:.2f} | {r['fill_per_s']:.1f} | {r['fill_wait_ms_per_s']:.3f} | {top}{more} |"
        )
    if store:
        out += [
            "",
            "The store's calls by caller (each took its one connection in turn): calls a second in the window, the mean and longest call (its wait for the connection and its run), the fill's beside them.",
            "",
            "| # | caller | in | calls/s | mean ms | busy ms/s | longest ms | fill: calls/s |",
            "| --- | --- | --- | --- | --- | --- | --- | --- |",
        ]
        for i, r in enumerate(store, 1):
            out.append(f"| {i} | `{r['site']}` | `{r['fn']}` | {r['calls_per_s']:.1f} | {r['mean_ms']:.2f} | {r['busy_ms_per_s']:.1f} | {r['max_ms']:.1f} | {r['fill_per_s']:.1f} |")
    return "\n".join(out)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    t = sub.add_parser("census-tree")
    t.add_argument("--to", required=True, type=Path)
    t.add_argument("--from", dest="src", type=Path, default=ROOT)
    t.add_argument("--ui", type=Path)
    r = sub.add_parser("rank")
    r.add_argument("runs", nargs="+", type=Path)
    r.add_argument("--tree", type=Path, default=ROOT)
    r.add_argument("--json", action="store_true")
    sub.add_parser("list")
    a = ap.parse_args()
    if a.cmd == "census-tree":
        census_tree(a.src.resolve(), a.to.resolve(), a.ui)
    elif a.cmd == "rank":
        rows, store = rank(a.runs, a.tree)
        n = sum(len([x for x in json.loads(p.read_text()).get("rounds", []) if x.get("locks")]) for p in a.runs)
        print(json.dumps({"locks": rows, "store_calls": store}, indent=1) if a.json else markdown(rows, store, n))
    elif a.cmd == "list":
        for d in declarations(ROOT):
            print(f"{d['file']}:{d['line']}\t{d['name']}\t{d['type']}")


if __name__ == "__main__":
    main()
