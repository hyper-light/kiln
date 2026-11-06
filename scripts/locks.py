#!/usr/bin/env python3
"""The locks on kiln's hot path (goal 5 of the API runtime program, plan 01a1a59c2aae).

  scripts/locks.py census-tree --to DIR [--from TREE] [--ui UI_DIST]
      A scratch copy of the tree (git's files, tracked and untracked but not ignored) whose crates
      count every Mutex and RwLock they take (scripts/locks/census.rs, as each crate's `census`
      module): kiln, kiln-http and Quiver. Built in release, its /metrics says, for each place a lock
      is taken, how often, how often it was already held and how long those waited (`kiln_lock_*`);
      kiln-lab keeps that for each round's window and fill (`Round::locks`). The store's calls are
      counted at their callers too (`store_call`), with how long each took. Lines keep their numbers.
      So are the dependencies kiln's hot path reaches that lock inside (scripts/locks/deps.toml):
      each copied from the cargo registry, at the versions Cargo.lock has, to census-deps/<crate>/
      <version>, its locks counted by census.rs as the crate kiln-census, the copy in the tree's
      [patch.crates-io] and Cargo.lock as cargo writes it (a `--locked` build holds); their sites are
      `<crate>@<version>:<file>:<line>:<column>`. Built with kiln's `jemalloc` feature, it counts
      the allocator's mutexes too, by jemalloc's own statistics, which the tree turns on
      (scripts/locks/census_jemalloc.rs; kind `jemalloc`, sites `jemalloc:<mutex>`). The crates
      must be in the cargo registry (`cargo fetch --locked` in TREE first).
  scripts/locks.py rank RUN.json [...] [--tree TREE] [--json]
      The locks a lab run's rounds took, one row a lock (the sites that took one lock, by its
      address, together), ranked by how often the window took it, with the fill's beside it:
      kiln's, Quiver's, the dependencies' and the allocator's apart (`group` and `owner` in JSON); then
      the store's calls by caller. A dependency's lines are read from the cargo registry. Each site's
      figures are its counters' growth as kiln-lab took it, its copies summed (`copies`: one a generic
      caller's instance or an inlined one's codegen unit); a run without `copies`, or with a growth
      below zero, is refused.
  scripts/locks.py list
      Every Mutex and RwLock the instrumented crates declare, with where it's taken.
  scripts/locks.py check [--root DIR]
      The API's hot path stays lockless (plan 01a1a59c2aae step 3): no Mutex, RwLock or Condvar in
      the modules scripts/locks/hot.toml names, outside a test build's code, but those it lists with
      why each is off the hot path. Nor does kiln's code wait on event-listener (goal 5's census: its
      list's Mutex, the one lock the shards still shared): no async-lock and no event-listener named in
      the folders hot.toml's `waits` names, outside a test build's code, but those it lists under
      `waiting` with why; `crate::turn` and `crate::rt::Changed` wait with no lock. Fails on any other,
      and on a listed one that's gone.

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

# The dependencies the census counts beside kiln's (scripts/locks/deps.toml): each copied into the census tree
# at `census-deps/<crate>/<version>`, its locks routed through the crate `kiln-census` (census.rs, at
# `census-deps/kiln-census`), which kiln renders on /metrics with its own.
DEPS = Path(__file__).resolve().parent / "locks" / "deps.toml"
DEPS_DIR = "census-deps"
CENSUS_CRATE = "kiln-census"
CENSUS_PATH = "kiln_census"
# The lock types a dependency's code may name, by their full path: which of the census's modules wraps each.
DEP_LOCKS = {("std", "sync", "Mutex"): "st", ("std", "sync", "RwLock"): "st", ("parking_lot", "Mutex"): "pl", ("parking_lot", "RwLock"): "pl"}
# Modules a file may name by an alias of its own (`use std::sync;`, then `sync::Mutex`).
ALIASED = {("std", "sync"), ("parking_lot",)}
# The allocator's locks (scripts/locks/census_jemalloc.rs), as kiln's `census_jemalloc` module where the census
# build has kiln's `jemalloc` feature: jemalloc's statistics are turned on there (tikv-jemalloc-sys's `stats`).
JEMALLOC_CENSUS = ("crates/kiln/src", "census_jemalloc.rs")
JEMALLOC_FEATURE = ("crates/kiln/Cargo.toml", re.compile(r"(?m)^(jemalloc\s*=\s*\[[^\]\n]*?)\s*\]"), '"tikv-jemalloc-sys/stats"')
DEP_RENDER_CALLS = f"{CENSUS_PATH}::render(&mut o); crate::census_jemalloc::render(&mut o);"


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
    # kiln's, kiln-http's and Quiver's, then the dependencies' (`kiln-census`) and the allocator's.
    m.write_text(text.replace(anchor, f"{anchor} {RENDER_CALLS} {DEP_RENDER_CALLS}", 1))
    # The store's calls, counted at their callers (`store_call`): which of kiln's paths use its one
    # connection, and how long each call took. `call` is a function returning the call's future, so
    # it can say where it was called from (#[track_caller]); on the line it was on.
    store = dest / STORE_CALL[0]
    text = store.read_text()
    if STORE_CALL[1] not in text:
        fail(f"{STORE_CALL[0]} no longer has `{STORE_CALL[1].strip()}`: say how its calls are counted")
    store.write_text(text.replace(STORE_CALL[1], STORE_CALL[2], 1))
    for line in census_deps(dest):
        print(line)
    if ui is not None:
        shutil.copytree(ui, dest / "ui" / "dist", dirs_exist_ok=True)
    print(f"census tree at {dest}")


# ------------------------------------------------------------------------- the dependencies and the allocator


class UseTree:
    """A `use` declaration's tree: a path (its first segment `""` for a leading `::`) ending in a name (and
    the name it's bound as), a group of trees, or a glob."""

    def __init__(self, path: list[str], kind: str, alias: str | None = None, children: list[UseTree] | None = None):
        self.path, self.kind, self.alias, self.children = path, kind, alias, children or []

    def render(self) -> str:
        p = "::".join(self.path)
        if self.kind == "group":
            inner = ", ".join(c.render() for c in self.children)
            return f"{p}::{{{inner}}}" if self.path else f"{{{inner}}}"
        if self.kind == "glob":
            return f"{p}::*" if self.path else "*"
        return p + (f" as {self.alias}" if self.alias else "")


USE_TOKEN = re.compile(r"::|[{}*,]|r#\w+|\w+|\S")
IDENT = re.compile(r"(?:r#)?[A-Za-z_]\w*")


def parse_use(text: str) -> UseTree | None:
    """The tree of a `use` declaration (what's between `use` and its `;`, comments and strings blanked), or
    None for one this doesn't read (a macro's, with `$`)."""
    toks = USE_TOKEN.findall(text)
    pos = 0

    def peek() -> str | None:
        return toks[pos] if pos < len(toks) else None

    def tree() -> UseTree:
        nonlocal pos
        path: list[str] = []
        if peek() == "::":
            path.append("")
            pos += 1
        while True:
            t = peek()
            if t == "{":
                pos += 1
                return UseTree(path, "group", children=group())
            if t == "*":
                pos += 1
                return UseTree(path, "glob")
            if t is None or t == "as" or not IDENT.fullmatch(t):
                raise ValueError(t)
            path.append(t)
            pos += 1
            if peek() == "::":
                pos += 1
                continue
            alias = None
            if peek() == "as":
                pos += 1
                alias = peek()
                if alias is None or not IDENT.fullmatch(alias):
                    raise ValueError(alias)
                pos += 1
            return UseTree(path, "name", alias)

    def group() -> list[UseTree]:
        nonlocal pos
        kids = []
        while peek() != "}":
            if peek() is None:
                raise ValueError("unclosed")
            kids.append(tree())
            if peek() == ",":
                pos += 1
            elif peek() != "}":
                raise ValueError(peek())
        pos += 1
        return kids

    try:
        t = tree()
    except ValueError:
        return None
    return t if pos == len(toks) else None


def use_declarations(code: str) -> list[tuple[int, int]]:
    """Where each `use` declaration's tree is in `code` (comments and strings blanked): from just past `use`
    to its `;`."""
    out = []
    for m in re.finditer(r"(?<![\w#$])use\b", code):
        i, depth = m.end(), 0
        while i < len(code):
            c = code[i]
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth < 0:
                    break
            elif c == ";" and depth == 0:
                out.append((m.end(), i))
                break
            i += 1
    return out


def module_aliases(trees: list[UseTree]) -> dict[str, tuple[str, ...]]:
    """The names a file binds to `std::sync` or `parking_lot` (`use std::sync;`, `use std::{sync, ..}`,
    `use std::sync::{self, ..}`, each maybe `as` another name), and which each names."""
    out: dict[str, tuple[str, ...]] = {}

    def walk(t: UseTree, prefix: list[str]) -> None:
        path = prefix + [s for s in t.path if s]
        if t.kind == "group":
            for c in t.children:
                walk(c, path)
            return
        if t.kind != "name" or not path:
            return
        target = tuple(path[:-1]) if path[-1] == "self" else tuple(path)
        name = t.alias or (target[-1] if target else None)
        if target in ALIASED and name and name != "_":
            out[name] = target

    for t in trees:
        walk(t, [])
    return out


def resolve(path: tuple[str, ...], aliases: dict[str, tuple[str, ...]]) -> tuple[str, ...]:
    return aliases[path[0]] + path[1:] if path and path[0] in aliases else path


def take_locks(t: UseTree, prefix: list[str], aliases: dict[str, tuple[str, ...]], found: list[tuple[str, str, str | None]]) -> UseTree | None:
    """`t` without the lock types it imports, each added to `found` (the census's module for it, its name, the
    name it's bound as); None if nothing's left of it."""
    path = prefix + [s for s in t.path if s]
    if t.kind == "group":
        kids = [k for c in t.children if (k := take_locks(c, path, aliases, found)) is not None]
        return UseTree(t.path, "group", children=kids) if kids else None
    if t.kind == "glob":
        return t
    module = DEP_LOCKS.get(resolve(tuple(path), aliases))
    if module is None:
        return t
    found.append((module, path[-1], t.alias))
    return None


def lock_globs(t: UseTree, prefix: list[str], aliases: dict[str, tuple[str, ...]]) -> list[tuple[str, ...]]:
    """The lock modules `t` imports everything of (`use std::sync::*`)."""
    path = prefix + [s for s in t.path if s]
    if t.kind == "group":
        return [g for c in t.children for g in lock_globs(c, path, aliases)]
    full = resolve(tuple(path), aliases)
    return [full] if t.kind == "glob" and full in ALIASED else []


# A function declared on one line that hands its caller a guard: a helper that takes a lock for its caller.
GUARD_HELPER = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:unsafe\s+)?fn\s+(\w+)[^{;]*->[^{;]*Guard\b")
FN_LINE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:unsafe\s+)?fn\s+(\w+)\b")


def rewrite_dep_file(text: str, track_caller: tuple[str, ...] = (), census: str = CENSUS_PATH, at: str = "a dependency's file") -> tuple[str, int]:
    """A dependency's source file with its locks the census's (`<census>::st`, `<census>::pl`), every line where
    it was: the lock types its `use` declarations import taken out of them and imported from the census instead,
    under the same names and with the same visibility and attributes; its paths to them in code
    (`std::sync::Mutex<T>`, or `sync::Mutex::new` where `sync` is its own name for `std::sync`) pointed at the
    census's; and the helpers that take a lock for their callers marked `#[track_caller]` (those that return a
    guard, and the functions `track_caller` names), so a site is where the helper was called. Returns the file and
    how many lock types it rerouted; fails on what it can't reroute (a glob import of a lock's module where the
    file names a lock)."""
    code = blank(text)
    decls = [(s, e, t) for s, e in use_declarations(code) if (t := parse_use(code[s:e])) is not None]
    aliases = module_aliases([t for _, _, t in decls])
    edits: list[tuple[int, int, str]] = []
    rerouted = 0
    for start, end, tree in decls:
        if lock_globs(tree, [], aliases) and re.search(r"\b(Mutex|RwLock)\b", code[:start] + code[end:]):
            fail(f"{at}: a glob import of a lock's module (`use {' '.join(code[start:end].split())}`) beside a lock: say how its locks are counted")
        found: list[tuple[str, str, str | None]] = []
        rest = take_locks(tree, [], aliases, found)
        if not found:
            continue
        rerouted += len(found)
        adds = [f"{census}::{m}::{name}" + (f" as {alias}" if alias else "") for m, name, alias in found]
        parts = ([rest.render().removeprefix("::")] if rest is not None else []) + adds
        new = parts[0] if len(parts) == 1 else "{" + ", ".join(parts) + "}"
        # The declaration on its first line, the lines it took kept as blank ones.
        edits.append((start, end, " " + new + "\n" * code.count("\n", start, end)))
    paths = [(re.compile(r"(?<![\w:])(?:::)?std::sync::(Mutex|RwLock)\b"), "st"), (re.compile(r"(?<![\w:])(?:::)?parking_lot::(Mutex|RwLock)\b"), "pl")]
    paths += [(re.compile(rf"(?<![\w:]){re.escape(name)}::(Mutex|RwLock)\b"), "st" if target == ("std", "sync") else "pl") for name, target in aliases.items()]
    for pat, module in paths:
        for m in pat.finditer(code):
            if not any(s <= m.start() < e for s, e, _ in decls):
                edits.append((m.start(), m.end(), f"{census}::{module}::{m.group(1)}"))
                rerouted += 1
    out = text
    for s, e, new in sorted(edits, reverse=True):
        out = out[:s] + new + out[e:]
    lines, code_lines = out.split("\n"), blank(out).split("\n")
    marked: set[str] = set()
    for i, line in enumerate(code_lines):
        named = FN_LINE.match(line)
        if not named or "async fn" in line or "#[track_caller]" in line:
            continue
        if named.group(1) in track_caller or GUARD_HELPER.match(line):
            lines[i] = re.sub(r"^(\s*)", r"\1#[track_caller] ", lines[i], count=1)
            marked.add(named.group(1))
    if missing := sorted(set(track_caller) - marked):
        fail(f"{DEPS.name} marks {', '.join(missing)} #[track_caller], which {at} doesn't declare: say where its lock is taken")
    return "\n".join(lines), rerouted


def cargo_home() -> Path:
    return Path(os.environ.get("CARGO_HOME") or Path.home() / ".cargo")


def registry_source(name: str, version: str, unpack: Path | None = None) -> Path | None:
    """A crate's source as cargo unpacked it (`$CARGO_HOME/registry/src/<index>/<name>-<version>`), or, given
    where to, unpacked from cargo's download of it (`registry/cache/<index>/<name>-<version>.crate`)."""
    reg = cargo_home() / "registry"
    for d in sorted(p for p in (reg / "src").glob("*") if p.is_dir()):
        if (d / f"{name}-{version}" / "Cargo.toml").is_file():
            return d / f"{name}-{version}"
    if unpack is None:
        return None
    for d in sorted(p for p in (reg / "cache").glob("*") if p.is_dir()):
        crate = d / f"{name}-{version}.crate"
        if not crate.is_file():
            continue
        import tarfile

        unpack.mkdir(parents=True, exist_ok=True)
        with tarfile.open(crate, "r:gz") as t:
            try:
                t.extractall(unpack, filter="data")
            except TypeError:  # a Python without extraction filters
                t.extractall(unpack)
        if (unpack / f"{name}-{version}" / "Cargo.toml").is_file():
            return unpack / f"{name}-{version}"
    return None


def lock_packages(lock: str) -> list[dict[str, str]]:
    """Cargo.lock's packages: each one's name, version and source (none for a path's)."""
    out = []
    for block in lock.split("\n\n"):
        if block.startswith("[[package]]"):
            out.append({k: v for k, v in re.findall(r'(?m)^(name|version|source) = "([^"]*)"$', block)})
    return out


def lock_name(entry: str) -> str:
    """A dependency's name in Cargo.lock's list of a package's (`"name"`, `"name version"`, `"name version (source)"`)."""
    return entry.split(" ", 1)[0]


def with_dependency(block: str, dep: str) -> str:
    """A Cargo.lock package with `dep` among its dependencies, where cargo puts it (by name: the census's crate's
    name is its own)."""
    m = re.search(r"(?ms)^dependencies = \[\n(.*?)^\]", block)
    entries = re.findall(r'(?m)^ "([^"]*)",$', m.group(1)) if m else []
    if dep in entries:
        return block
    at = next((i for i, e in enumerate(entries) if lock_name(e) > lock_name(dep)), len(entries))
    entries.insert(at, dep)
    body = "dependencies = [\n" + "".join(f' "{e}",\n' for e in entries) + "]"
    return block[: m.start()] + body + block[m.end() :] if m else block + "\n" + body


def census_lock(lock: str, patched: list[tuple[str, str]], census_deps: list[str]) -> str:
    """Cargo.lock as cargo writes it for the census tree: each patched package a path's (no source, no checksum)
    that depends on the census's crate, kiln too, and the census's crate (which depends on `census_deps`)."""
    blocks = lock.rstrip("\n").split("\n\n")
    out = []
    for block in blocks:
        if block.startswith("[[package]]"):
            p = dict(re.findall(r'(?m)^(name|version|source) = "([^"]*)"$', block))
            if (p.get("name"), p.get("version")) in patched and p.get("source", "").startswith("registry+"):
                block = "\n".join(line for line in block.split("\n") if not line.startswith(("source = ", "checksum = ")))
                block = with_dependency(block, CENSUS_CRATE)
            elif p.get("name") == "kiln" and "source" not in p:
                block = with_dependency(block, CENSUS_CRATE)
        out.append(block)
    ours = f'[[package]]\nname = "{CENSUS_CRATE}"\nversion = "0.0.0"'
    for d in census_deps:
        ours = with_dependency(ours, d)
    names = [re.search(r'(?m)^name = "([^"]*)"$', b) if b.startswith("[[package]]") else None for b in out]
    at = next((i for i, n in enumerate(names) if n and n.group(1) > CENSUS_CRATE), None)
    if at is None:
        at = max((i for i, n in enumerate(names) if n), default=len(out) - 1) + 1
    out.insert(at, ours)
    return "\n\n".join(out) + "\n"


def census_deps(dest: Path) -> list[str]:
    """The census tree's dependencies counted (scripts/locks/deps.toml): each, at every version the tree's
    Cargo.lock has from the registry, copied from the cargo registry to `census-deps/<crate>/<version>` with its
    locks the census's (`kiln-census`, census.rs as a crate of its own, at `census-deps/kiln-census`), the tree's
    [patch.crates-io] pointed at it and Cargo.lock as cargo would write it (so `--locked` holds); kiln depending on
    the census's crate, and on jemalloc's statistics where it has the `jemalloc` feature. Says what it did, a line
    each."""
    import tomllib

    spec = tomllib.loads(DEPS.read_text()).get("crate", [])
    lock = (dest / "Cargo.lock").read_text()
    packages = lock_packages(lock)
    root_manifest = tomllib.loads((dest / "Cargo.toml").read_text())
    already = set(root_manifest.get("patch", {}).get("crates-io", {}))
    already |= {v.get("package") for v in root_manifest.get("patch", {}).get("crates-io", {}).values() if isinstance(v, dict)}
    deps_root = dest / DEPS_DIR
    said: list[str] = []
    patched: list[tuple[str, str]] = []
    wants_pl = False
    for c in spec:
        name = c["name"]
        versions = [p["version"] for p in packages if p.get("name") == name and p.get("source", "").startswith("registry+")]
        if name in already:
            said.append(f"{name}: the tree patches it already (a path of its own): not counted")
            continue
        if not versions:
            said.append(f"{name}: not in Cargo.lock: not counted")
            continue
        marks: dict[str, list[str]] = {}
        for t in c.get("track_caller", []):
            file, fn = t.rsplit(":", 1)
            marks.setdefault(file, []).append(fn)
        for version in versions:
            src = registry_source(name, version, deps_root / ".crates")
            if src is None:
                fail(f"{name} {version} isn't in the cargo registry ({cargo_home() / 'registry'}): `cargo fetch --locked` in the tree first")
            to = deps_root / name / version
            shutil.copytree(src, to, copy_function=shutil.copyfile, ignore=shutil.ignore_patterns("target", ".git"))
            files = sorted(to / f for f in c["files"]) if "files" in c else sorted((to / "src").rglob("*.rs"))
            rels = [f.relative_to(to).as_posix() for f in files]
            for rel in [r for r in c.get("files", [])] + list(marks):
                if rel not in rels or not (to / rel).is_file():
                    fail(f"{DEPS.name} names {name}'s {rel}, which {name} {version} doesn't have")
            rerouted = 0
            for f, rel in zip(files, rels):
                text = f.read_text()
                new, n = rewrite_dep_file(text, tuple(marks.get(rel, [])), at=f"{name} {version}'s {rel}")
                if new != text:
                    f.write_text(new)
                rerouted += n
                wants_pl |= f"{CENSUS_PATH}::pl::" in new
            manifest = to / "Cargo.toml"
            manifest.write_text(manifest.read_text().rstrip("\n") + f'\n\n[dependencies.{CENSUS_CRATE}]\npath = "../../{CENSUS_CRATE}"\n')
            patched.append((name, version))
            said.append(f"{name}@{version}: {rerouted} lock types rerouted to the census ({to.relative_to(dest)})")
    # The census's crate: census.rs, with parking_lot's wrappers only where a copy takes parking_lot's locks.
    census = (Path(__file__).resolve().parent / "locks" / "census.rs").read_text()
    crate = deps_root / CENSUS_CRATE
    (crate / "src").mkdir(parents=True, exist_ok=True)
    census_deps_of: list[str] = []
    manifest = f'# The lock census\'s counting for the dependencies it counts (scripts/locks/census.rs, made by scripts/locks.py\n# census-tree; goal 5 of the API runtime program, plan 01a1a59c2aae): never part of kiln\'s own build.\n[package]\nname = "{CENSUS_CRATE}"\nversion = "0.0.0"\nedition = "2024"\npublish = false\n\n[lib]\npath = "src/lib.rs"\n'
    if wants_pl:
        pl = sorted((p["version"] for p in packages if p.get("name") == "parking_lot"), key=lambda v: [int(x) if x.isdigit() else 0 for x in re.split(r"[.+-]", v)])
        if not pl:
            fail("a dependency takes parking_lot's locks, and Cargo.lock has no parking_lot for the census's crate")
        manifest += f'\n[dependencies]\nparking_lot = "={pl[-1]}"\n'
        census_deps_of.append("parking_lot" if len(pl) == 1 else f"parking_lot {pl[-1]}")
    (crate / "src" / "lib.rs").write_text(census if wants_pl else re.sub(r"// census:parking_lot.*?// census:end\n", "", census, flags=re.S))
    (crate / "Cargo.toml").write_text(manifest)
    # The root manifest: the copies out of the workspace, and in its [patch.crates-io].
    root = dest / "Cargo.toml"
    text = root.read_text()
    m = re.search(r"(?m)^exclude\s*=\s*\[", text)
    if m:
        text = text[: m.end()] + f'"{DEPS_DIR}", ' + text[m.end() :]
    else:
        text = re.sub(r"(?m)^\[workspace\][ \t]*$", f'[workspace]\nexclude = ["{DEPS_DIR}"]', text, count=1)
    counts = {n: sum(1 for p, _ in patched if p == n) for n, _ in patched}
    lines = ["# The lock census's copies of the dependencies it counts (scripts/locks/deps.toml, scripts/locks.py census-tree)."]
    for name, version in patched:
        if counts[name] == 1:
            lines.append(f'{name} = {{ path = "{DEPS_DIR}/{name}/{version}" }}')
        else:
            key = re.sub(r"[^A-Za-z0-9_-]", "-", f"{name}-census-{version}")
            lines.append(f'{key} = {{ path = "{DEPS_DIR}/{name}/{version}", package = "{name}" }}')
    m = re.search(r"(?m)^\[patch\.crates-io\][ \t]*$", text)
    if m:
        text = text[: m.end()] + "\n" + "\n".join(lines) + text[m.end() :]
    else:
        text = text.rstrip("\n") + "\n\n[patch.crates-io]\n" + "\n".join(lines) + "\n"
    root.write_text(text)
    # kiln: the census's crate, which it renders; jemalloc's statistics, where it has the feature.
    kiln = dest / JEMALLOC_FEATURE[0]
    text = kiln.read_text()
    m = re.search(r"(?m)^\[dependencies\][ \t]*$", text)
    if not m:
        fail(f"{JEMALLOC_FEATURE[0]} has no [dependencies]: say where the census's crate goes")
    text = text[: m.end()] + f'\n{CENSUS_CRATE} = {{ path = "../../{DEPS_DIR}/{CENSUS_CRATE}" }}' + text[m.end() :]
    feature = JEMALLOC_FEATURE[1].search(text)
    if feature:
        sep = "" if feature.group(1).rstrip().endswith("[") else ", "
        text = text[: feature.start()] + feature.group(1) + sep + JEMALLOC_FEATURE[2] + "]" + text[feature.end() :]
        said.append("jemalloc: its statistics on (tikv-jemalloc-sys's `stats`): a build with kiln's `jemalloc` feature counts its mutexes")
    else:
        said.append("jemalloc: kiln has no `jemalloc` feature here: the allocator's locks aren't counted")
    kiln.write_text(text)
    (dest / "Cargo.lock").write_text(census_lock(lock, patched, census_deps_of))
    folder, name = JEMALLOC_CENSUS
    shutil.copy2(Path(__file__).resolve().parent / "locks" / name, dest / folder / name)
    lib = dest / folder / "lib.rs"
    lib.write_text(lib.read_text().rstrip("\n") + f"\n#[allow(warnings)]\npub mod {name.removesuffix('.rs')};\n")
    return said


# ------------------------------------------------------------------------------------------ ranking

# A dependency's site (census.rs's `site_file`): `<crate>@<version>:<its file in the crate>:<line>:<column>`.
DEP_SITE = re.compile(r"^([A-Za-z0-9_][\w-]*)@([^:\s/]+):(.+)$")
# Whose locks `rank` says apart, in its order: kiln's (kiln and kiln-http), Quiver's (vendored), the dependencies'
# (scripts/locks/deps.toml) and the allocator's (scripts/locks/census_jemalloc.rs).
GROUPS = {
    "kiln": "kiln's",
    "quiver": "Quiver's (vendored: its locks are its own change)",
    "dependencies": "Its dependencies' (scripts/locks/deps.toml; a site is `<crate>@<version>:<file>:<line>:<column>`, the file as the cargo registry has it)",
    "allocator": "The allocator's (jemalloc's mutexes, by jemalloc's own counts: held when asked is spun for or blocked on, waited is the blocked alone)",
}
NONE_TAKEN = {
    "kiln": "None taken.",
    "quiver": "None taken.",
    "dependencies": "None taken (or a census tree from before the dependencies were counted).",
    "allocator": "None counted: a build without kiln's `jemalloc` feature (glibc's or musl's malloc, which count nothing), or a jemalloc without statistics (`kiln_lock_jemalloc_stats 0`).",
}


def owner_of(site: str) -> tuple[str, str]:
    """Whose lock a site takes: its group (`GROUPS`) and its owner (`kiln`, `quiver`, `<crate>@<version>`,
    `jemalloc`)."""
    if site.startswith("jemalloc:"):
        return "allocator", "jemalloc"
    if m := DEP_SITE.match(site):
        return "dependencies", f"{m.group(1)}@{m.group(2)}"
    if site.startswith("vendor/quiver/"):
        return "quiver", "quiver"
    return "kiln", "kiln"


def source_of(tree: Path, site: str) -> tuple[Path | None, str, int, int]:
    """Where a site is: the file to read (kiln's and Quiver's in the tree; a dependency's in the census tree's copy
    where the tree is a census tree, else as the cargo registry has it: the census keeps every line where it was),
    the file as the site names it, its line and its column. The allocator's sites are its mutexes, no file's."""
    if site.startswith("jemalloc:"):
        return None, site, 0, 0
    try:
        file, line, col = site.rsplit(":", 2)
        at = (int(line), int(col))
    except ValueError:
        return None, site, 0, 0
    if m := DEP_SITE.match(file):
        copy = tree / DEPS_DIR / m.group(1) / m.group(2) / m.group(3)
        if copy.is_file():
            return copy, file, *at
        crate = registry_source(m.group(1), m.group(2))
        return (crate / m.group(3) if crate else None), file, *at
    return tree / file, file, *at


def helper_calls() -> re.Pattern[str]:
    """A lock's call at a site: `.lock(` and the like, or a call of a helper deps.toml marks `#[track_caller]`
    (event-listener's `with_inner`), whose callers are its sites."""
    import tomllib

    try:
        spec = tomllib.loads(DEPS.read_text()).get("crate", [])
    except (OSError, ValueError):
        spec = []
    helpers = sorted({t.rsplit(":", 1)[1] for c in spec for t in c.get("track_caller", [])})
    return re.compile(r"\.(lock|read|write|try_lock|try_read|try_write" + "".join(f"|{re.escape(h)}" for h in helpers) + r")\(")


def receiver_at(tree: Path, site: str, calls_at: re.Pattern[str] = LOCK_CALL) -> tuple[str, str]:
    """The expression a site takes a lock of (`self.agents`; `self.with_inner()` where a helper takes it), and its
    line."""
    path, _, line, col = source_of(tree, site)
    if path is None:
        return "?", ""
    try:
        text = path.read_text().split("\n")[line - 1]
    except (OSError, IndexError, ValueError):
        return "?", ""
    calls = list(calls_at.finditer(text))
    col0 = col - 1
    pick = next((c for c in calls if c.start() + 1 >= col0), calls[-1] if calls else None)
    if pick is None:
        return "?", text.strip()
    m = RECEIVER.search(text[: pick.start()])
    recv = m.group(1) if m else "?"
    if recv != "?" and not LOCK_CALL.fullmatch(text[pick.start() : pick.end()]):
        recv = f"{recv}.{pick.group(1)}()"
    return recv, text.strip()


DECLARATION = re.compile(r"\b(?:pub(?:\([^)]*\))?\s+)?(?:static\s+|let\s+(?:mut\s+)?)?([A-Za-z_]\w*)\s*:\s*([^=;]*\b(?:Mutex|RwLock)\s*<)")


def declared_in(path: Path, shown: str) -> list[dict]:
    """Every Mutex or RwLock field, static or local `path` declares, named by `shown`."""
    try:
        lines = path.read_text().split("\n")
    except OSError:
        return []
    out = []
    for i, line in enumerate(lines, 1):
        s = line.strip()
        if s.startswith("//") or s.startswith("use "):
            continue
        for m in DECLARATION.finditer(line):
            out.append({"file": shown, "line": i, "name": m.group(1), "type": m.group(2).strip()})
    return out


def declarations(tree: Path) -> list[dict]:
    """Every Mutex or RwLock field, static or local the instrumented crates declare."""
    out = []
    for folder, _ in CRATES:
        for f in sorted((tree / folder).rglob("*.rs")):
            if f.name != "census.rs":
                out += declared_in(f, str(f.relative_to(tree)))
    return out


# The figures kiln-lab keeps of a site (`LockSeen`) that are its counters' growth, over the window and the fill.
GROWTH = ("taken", "contended", "wait_s", "fill_taken", "fill_contended", "fill_wait_s")


def unsound(run: Path, rounds: list[dict]) -> str | None:
    """Why a run's lock census can't be ranked, or None (goal 5, plan 01a1a59c2aae step 1). The census counts a
    site under each copy of its location the compiler made (a generic caller's instances, an inlined caller's
    codegen units), a series each under the same labels, and kiln-lab sums a site's copies in every scrape before
    it takes the growth, and says how many (`copies`). A kiln-lab from before took a site's start from one copy
    and its end from another wherever there were two or more (53 sites of 280 in census-B's build,
    event-listener's among them), and nothing in its run says which sites those were. Either way, counters'
    growth can't be below zero."""
    old = len({key for rd in rounds for key, seen in rd["locks"].items() if not seen.get("copies")})
    below = [f"{key} {f} {seen[f]:g} (round {rd.get('index', i + 1)})" for i, rd in enumerate(rounds) for key, seen in sorted(rd["locks"].items()) for f in GROWTH if seen.get(f, 0) < 0]
    if not old and not below:
        return None
    why = []
    if old:
        why.append(
            f"its kiln-lab didn't sum a site's copies ({old} site{'s' if old != 1 else ''} without `copies`), so a site the build counted under two or more copies of its location has one copy's count at the start and another's at the end"
        )
    if below:
        why.append(f"{len(below)} growths below zero: " + "; ".join(below[:8]) + ("; ..." if len(below) > 8 else ""))
    return f"{run}: its lock census can't be ranked: " + "; and ".join(why) + ". Run it again with a kiln-lab that sums them (the census build can stay)."


def rank(runs: list[Path], tree: Path) -> tuple[list[dict], list[dict]]:
    rounds = []
    for r in runs:
        data = json.loads(r.read_text())
        window = float(data.get("profile", {}).get("window_s") or 60)
        mine = [x for x in data.get("rounds", []) if x.get("locks")]
        if why := unsound(r, mine):
            fail(why)
        rounds.extend((x, window) for x in mine)
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
    # few each: they join by their receiver too. Only one owner's sites join (kiln's, Quiver's, one
    # dependency's): an address a freed lock of one left and another's took since is two locks.
    parent: dict[str, str] = {k: k for k in sites}

    def find(k: str) -> str:
        while parent[k] != k:
            parent[k] = parent[parent[k]]
            k = parent[k]
        return k

    by_addr: dict[tuple[str, str], str] = {}
    for k, s in sites.items():
        owner = owner_of(k.split(" ", 1)[0])[1]
        for a in s["addrs"]:
            if (owner, a) in by_addr:
                parent[find(k)] = find(by_addr[(owner, a)])
            else:
                by_addr[(owner, a)] = k
    groups: dict[str, list[str]] = {}
    for k in sites:
        groups.setdefault(find(k), []).append(k)
    decls = declarations(tree)
    dep_decls: dict[Path, list[dict]] = {}
    dep_calls = helper_calls()
    rows = []
    for members in groups.values():
        detail = []
        for k in members:
            site, _ = k.split(" ", 1)
            recv, text = receiver_at(tree, site, dep_calls if owner_of(site)[0] == "dependencies" else LOCK_CALL)
            s = sites[k]
            detail.append({"site": site, "kind": s["kind"], "receiver": recv, "line": text, "taken_per_s": med(s["taken"]), "fill_per_s": med(s["fill"])})
        detail.sort(key=lambda d: (-d["taken_per_s"], -d["fill_per_s"]))
        group, owner = owner_of(detail[0]["site"])
        # The receiver its sites name most, a tie going to the most taken site's (`detail`'s order): the
        # same name every run (a set's order is the process's string hashing's).
        recvs = [d["receiver"] for d in detail if d["receiver"] != "?"]
        name = detail[0]["site"].removeprefix("jemalloc:") if group == "allocator" else max(recvs, key=recvs.count) if recvs else "?"
        field = name.split(".")[-1].split("::")[-1].rstrip("()")
        where = [source_of(tree, d["site"]) for d in detail]
        files = {w[1] for w in where}
        if group == "dependencies":
            cands = [x for path, shown, _, _ in where if path is not None for x in dep_decls.setdefault(path, declared_in(path, shown)) if x["name"] == field]
            cands = list({(x["file"], x["line"]): x for x in cands}.values())
        elif group == "allocator":
            cands = []
        else:
            cands = [d for d in decls if d["name"] == field]
        near = [d for d in cands if d["file"] in files] or cands
        rows.append(
            {
                "group": group,
                "owner": owner,
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
    out = [f"Locks the lab's load took, ranked by acquisitions a second in the window (the median of {rounds} rounds), the fill's beside them: kiln's, Quiver's, its dependencies' and the allocator's apart."]
    for group, title in GROUPS.items():
        mine = [r for r in rows if r.get("group", "kiln") == group]
        out += ["", f"{title}:", ""]
        if not mine:
            out.append(NONE_TAKEN[group])
            continue
        out += [
            "| # | lock | where | taken/s | held when asked/s | waited ms/s | longest wait ms | fill: taken/s | fill: waited ms/s | sites |",
            "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |",
        ]
        for i, r in enumerate(mine, 1):
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


# ------------------------------------------------------------------------------------------ the check

HOT = "scripts/locks/hot.toml"
LOCK_WORD = re.compile(r"\b(Mutex|RwLock|Condvar|ReentrantMutex|FairMutex)\b")
# What waits on event-listener's list, behind its std Mutex (goal 5, plan 01a1a59c2aae, its top goal): async-lock's
# locks, semaphores and cells, and event-listener's Events, named by their crates' paths.
WAIT_WORD = re.compile(r"\b(async_lock|event_listener)::")


def blank(text: str) -> str:
    """`text` with its comments and its string and character literals' contents blanked (newlines
    kept, so lines keep their numbers): what's left is code."""
    out = []
    i, n = 0, len(text)

    def keep_lines(s: str) -> str:
        return "".join("\n" if c == "\n" else " " for c in s)

    while i < n:
        c = text[i]
        two = text[i : i + 2]
        if two == "//":
            j = text.find("\n", i)
            j = n if j < 0 else j
            out.append(" " * (j - i))
            i = j
        elif two == "/*":
            depth, j = 1, i + 2
            while j < n and depth:
                if text[j : j + 2] == "/*":
                    depth, j = depth + 1, j + 2
                elif text[j : j + 2] == "*/":
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            out.append(keep_lines(text[i:j]))
            i = j
        elif (m := re.match(r'b?r(#*)"', text[i : i + 64])) and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            end = '"' + m.group(1)
            j = text.find(end, i + m.end())
            j = n if j < 0 else j + len(end)
            out.append(keep_lines(text[i:j]))
            i = j
        elif c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            out.append('"' + keep_lines(text[i + 1 : j]) + '"')
            i = j + 1
        elif c == "'":
            # A character literal ('x', '\n', '\u{..}'), else a lifetime ('a).
            m = re.match(r"'(\\u\{[0-9a-fA-F]+\}|\\.|[^\\'\n])'", text[i : i + 16])
            if m:
                out.append(" " * m.end())
                i += m.end()
            else:
                out.append(c)
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


def item_end(code: str, i: int) -> int:
    """Where the item starting at `i` ends: past its block, or its `;` or `,` at its own depth."""
    depth = 0
    while i < len(code):
        c = code[i]
        if c in "([<":
            depth += 1
        elif c in ")]>":
            depth = max(0, depth - 1)
        elif c == "{":
            b, j = 1, i + 1
            while j < len(code) and b:
                b += {"{": 1, "}": -1}.get(code[j], 0)
                j += 1
            return j
        elif c in ";," and depth == 0:
            return i + 1
        i += 1
    return i


def outside_tests(code: str) -> str:
    """`code` without what only a test build compiles: each item `#[cfg(test)]` marks, blanked."""
    out = list(code)
    for m in re.finditer(r"#\[cfg\(test\)\]", code):
        i = m.end()
        # Past the item's other attributes.
        while True:
            a = re.match(r"\s*#\[", code[i:])
            if not a:
                break
            i += a.end()
            depth = 1
            while i < len(code) and depth:
                depth += {"[": 1, "]": -1}.get(code[i], 0)
                i += 1
        end = item_end(code, i)
        for j in range(m.start(), end):
            if out[j] != "\n":
                out[j] = " "
    return "".join(out)


def name_of(line: str) -> str:
    """What a line that names a lock declares: a field, static, binding, type or function, an import."""
    s = line.strip()
    if s.startswith("use ") or re.match(r"pub(\([^)]*\))?\s+use ", s):
        return "use"
    for pat in [
        r"(?<!')\bstatic\s+(?:mut\s+)?(\w+)",
        r"\b(?:struct|enum)\s+(\w+)",
        r"\btype\s+(\w+)",
        r"\blet\s+(?:mut\s+)?(\w+)",
        r"\bfn\s+(\w+)",
        r"^(?:pub(?:\([^)]*\))?\s+)?(\w+)\s*:(?!:)",
        r"\b([A-Z_][A-Z0-9_]+)\s*\.\s*get_or_init\b",
        r"\b(\w+)\s*:(?!:)\s*[\w:<(&' ]*?\b(?:Mutex|RwLock|Condvar)\b",
    ]:
        m = re.search(pat, s)
        if m:
            return m.group(1)
    return s


def hot_files(root: Path, spec: dict, key: str = "hot") -> list[Path]:
    files: list[Path] = []
    for p in spec.get(key, []):
        at = root / p
        if at.is_dir():
            files.extend(sorted(at.rglob("*.rs")))
        elif at.is_file():
            files.append(at)
        else:
            fail(f"{HOT} names {p}, which isn't there")
    return files


def listed(spec: dict, key: str) -> tuple[dict[tuple[str, str], str], list[str]]:
    """hot.toml's entries under `key` (`allowed`, `waiting`): each (file, name) with its why, and those still to be
    replaced, each with what replaces it (`todo`)."""
    entries = {(a["at"], a["name"]): a.get("why", "") for a in spec.get(key, [])}
    for (at, name), why in entries.items():
        if not why.strip():
            fail(f"{HOT}: {at} {name} says no why")
    return entries, sorted(f"{a['at']} {a['name']}: {a['todo']}" for a in spec.get(key, []) if a.get("todo"))


def scan(root: Path, files: list[Path], word: re.Pattern[str], entries: dict[tuple[str, str], str]) -> tuple[list[str], list[str]]:
    """The lines of `files`' code outside a test build's that `word` finds and `entries` doesn't list, and the
    entries no such line names any more (stale)."""
    seen: set[tuple[str, str]] = set()
    found = []
    for f in files:
        rel = f.relative_to(root).as_posix()
        code = outside_tests(blank(f.read_text()))
        lines = code.split("\n")
        source = f.read_text().split("\n")
        for i, line in enumerate(lines, 1):
            if not word.search(line):
                continue
            name = name_of(line)
            if (rel, name) in entries:
                seen.add((rel, name))
                continue
            found.append(f"{rel}:{i}: {name}: {source[i - 1].strip()}")
    return found, sorted(f"{at} {name}" for (at, name) in entries if (at, name) not in seen)


def check(root: Path) -> int:
    import tomllib

    spec = tomllib.loads((root / HOT).read_text())
    # Those on the hot path still, each with what replaces it: listed so the check holds the rest, and
    # said each time it runs.
    allowed, todo = listed(spec, "allowed")
    found, stale = scan(root, hot_files(root, spec), LOCK_WORD, allowed)
    waiting, waits_todo = listed(spec, "waiting")
    waits_found, waits_stale = scan(root, hot_files(root, spec, "waits"), WAIT_WORD, waiting)
    if found:
        print("A lock on the API's hot path (goal 5, plan 01a1a59c2aae): make it an owner with messages, an immutable copy swapped in whole (crate::rcu), or a copy per shard; or, if it's off the hot path, list it in " + HOT + " with why.\n")
        print("\n".join(found))
    if stale:
        print(f"\n{HOT} lists locks that aren't there any more: take them out.\n")
        print("\n".join(stale))
    if waits_found:
        print("\nA wait on event-listener's list, behind its Mutex (goal 5, plan 01a1a59c2aae, its top goal: the one lock the shards still shared): wait with crate::turn (a gate, a baton, a read-write, keyed, once) or crate::rt::Changed instead; or, if no shard takes it, or one task alone, list it in " + HOT + "'s `waiting` with why.\n")
        print("\n".join(waits_found))
    if waits_stale:
        print(f"\n{HOT}'s `waiting` lists waits that aren't there any more: take them out.\n")
        print("\n".join(waits_stale))
    if found or stale or waits_found or waits_stale:
        return 1
    print(f"locks: no lock on the hot path but the {len(allowed)} {HOT} lists, each with why; no wait on event-listener but the {len(waiting)} it lists under `waiting`")
    if todo:
        print(f"\n{len(todo)} of them are on the hot path still, each to be replaced so:\n" + "\n".join(todo))
    if waits_todo:
        print(f"\n{len(waits_todo)} of the waits are still to move, each so:\n" + "\n".join(waits_todo))
    return 0


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
    c = sub.add_parser("check")
    c.add_argument("--root", type=Path, default=ROOT)
    a = ap.parse_args()
    if a.cmd == "check":
        sys.exit(check(a.root.resolve()))
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
