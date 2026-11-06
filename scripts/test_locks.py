#!/usr/bin/env python3
"""Tests of scripts/locks.py (goal 5 of the API runtime program, plan 01a1a59c2aae).

`check` (step 3): it fails on a lock planted on the hot path, and passes what only a test build compiles,
what comments and strings say, and the locks hot.toml lists with why. Its `waits` (goal 5's top goal): it fails
on async-lock or event-listener named in kiln's code (their waits are event-listener's list, behind its Mutex),
and passes the same in comments, strings and tests, and those hot.toml lists under `waiting`.

The census of the dependencies' and the allocator's locks: `census-tree` copies the dependencies
scripts/locks/deps.toml names from the cargo registry, their locks the census's with every line where it
was, points the tree's [patch.crates-io] at them, writes Cargo.lock as cargo would and turns jemalloc's
statistics on; `rank` says kiln's, Quiver's, the dependencies' and the allocator's locks apart, each
site's rate once whatever lock its churning addresses join it to, and refuses figures that can't be
counters' growth (a kiln-lab that didn't sum a site's copies, census-B's event-listener rates below zero).

  python3 scripts/test_locks.py
"""

from __future__ import annotations

import contextlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parent.parent
LOCKS = ROOT / "scripts" / "locks.py"
sys.path.insert(0, str(ROOT / "scripts"))
import locks  # noqa: E402


def check(root: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run([sys.executable, str(LOCKS), "check", "--root", str(root)], capture_output=True, text=True)


class Tree:
    """A tree of its own: hot.toml and the files it names."""

    def __init__(self, hot: list[str], allowed: list[tuple[str, str, str]] | None = None, waits: list[str] | None = None, waiting: list[tuple[str, str, str]] | None = None):
        self.dir = Path(tempfile.mkdtemp(prefix="kiln-locks-"))
        lines = ["hot = [" + ", ".join(f'"{h}"' for h in hot) + "]"]
        if waits is not None:
            lines += ["waits = [" + ", ".join(f'"{w}"' for w in waits) + "]"]
        for at, name, why in allowed or []:
            lines += ["[[allowed]]", f'at = "{at}"', f'name = "{name}"', f'why = "{why}"']
        for at, name, why in waiting or []:
            lines += ["[[waiting]]", f'at = "{at}"', f'name = "{name}"', f'why = "{why}"']
        (self.dir / "scripts" / "locks").mkdir(parents=True)
        (self.dir / "scripts" / "locks" / "hot.toml").write_text("\n".join(lines) + "\n")

    def file(self, path: str, text: str) -> None:
        p = self.dir / path
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)

    def close(self) -> None:
        shutil.rmtree(self.dir, ignore_errors=True)


CLEAN = '''//! A hot module: a Mutex named in a comment is no lock.
use std::sync::Arc;

/// Says "RwLock" in its docs, and in a string.
pub struct Hub {
    agents: crate::rcu::Rcu<Vec<String>>,
}

impl Hub {
    pub fn say(&self) -> &'static str {
        "a Mutex<u8> in a string is no lock either: { braces } don't confuse it"
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use parking_lot::Mutex;
    static SEEN: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    fn f() { let c = '{'; let _ = c; }
}
'''


class Check(unittest.TestCase):
    def test_g5_s3_a_clean_hot_module_passes(self) -> None:
        t = Tree(["src/hub.rs"])
        try:
            t.file("src/hub.rs", CLEAN)
            r = check(t.dir)
            self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        finally:
            t.close()

    def test_g5_s3_a_planted_lock_fails(self) -> None:
        for planted, name in [
            ("    planted: parking_lot::Mutex<u64>,\n", "planted"),
            ("    planted: std::sync::RwLock<Vec<u8>>,\n", "planted"),
            ("    planted: async_lock::Mutex<()>,\n", "planted"),
        ]:
            t = Tree(["src"])
            try:
                t.file("src/hub.rs", CLEAN.replace("    agents: crate::rcu::Rcu<Vec<String>>,\n", "    agents: crate::rcu::Rcu<Vec<String>>,\n" + planted))
                r = check(t.dir)
                self.assertEqual(r.returncode, 1, r.stdout)
                self.assertIn(f"src/hub.rs:7: {name}:", r.stdout)
            finally:
                t.close()

    def test_g5_s3_a_static_or_a_local_lock_fails_too(self) -> None:
        t = Tree(["src/hub.rs"])
        try:
            t.file("src/hub.rs", CLEAN + "\nstatic PLANTED: std::sync::Mutex<u8> = std::sync::Mutex::new(0);\nfn g() {\n    let held = parking_lot::Mutex::new(1);\n    drop(held);\n}\n")
            r = check(t.dir)
            self.assertEqual(r.returncode, 1, r.stdout)
            self.assertIn(": PLANTED:", r.stdout)
            self.assertIn(": held:", r.stdout)
        finally:
            t.close()

    def test_g5_s3_a_listed_lock_passes_and_a_gone_one_fails(self) -> None:
        listed = CLEAN.replace("    agents: crate::rcu::Rcu<Vec<String>>,\n", "    agents: crate::rcu::Rcu<Vec<String>>,\n    tokens: parking_lot::Mutex<Vec<String>>,\n")
        t = Tree(["src/hub.rs"], [("src/hub.rs", "tokens", "taken as a person signs in")])
        try:
            t.file("src/hub.rs", listed)
            r = check(t.dir)
            self.assertEqual(r.returncode, 0, r.stdout)
            t.file("src/hub.rs", CLEAN)
            r = check(t.dir)
            self.assertEqual(r.returncode, 1, r.stdout)
            self.assertIn("src/hub.rs tokens", r.stdout)
        finally:
            t.close()

    def test_g5_s3_a_lock_planted_in_kilns_own_hot_path_fails(self) -> None:
        """The real hot.toml and the files it names: as they are, and with a lock planted in the hub."""
        spec = tomllib.loads((ROOT / "scripts" / "locks" / "hot.toml").read_text())
        dest = Path(tempfile.mkdtemp(prefix="kiln-locks-real-"))
        try:
            (dest / "scripts" / "locks").mkdir(parents=True)
            shutil.copy2(ROOT / "scripts" / "locks" / "hot.toml", dest / "scripts" / "locks" / "hot.toml")
            for h in [*spec["hot"], *spec.get("waits", [])]:
                src = ROOT / h
                if src.is_dir():
                    shutil.copytree(src, dest / h, dirs_exist_ok=True)
                else:
                    (dest / h).parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(src, dest / h)
            before = check(dest)
            self.assertEqual(before.returncode, 0, before.stdout)
            hub = dest / "crates" / "kiln" / "src" / "hub.rs"
            hub.write_text(hub.read_text().replace("pub struct Hub {\n", "pub struct Hub {\n    planted: parking_lot::Mutex<HashMap<String, String>>,\n", 1))
            after = check(dest)
            self.assertEqual(after.returncode, 1, after.stdout)
            self.assertIn(": planted:", after.stdout)
        finally:
            shutil.rmtree(dest, ignore_errors=True)

    def test_g5_waits_on_event_listener_planted_in_kilns_code_fail(self) -> None:
        """The real hot.toml and every folder its `waits` names: as they are, then an async-lock mutex and an Event
        planted in modules off the hot path's list (a request's and a runner's), each failing the check."""
        spec = tomllib.loads((ROOT / "scripts" / "locks" / "hot.toml").read_text())
        self.assertIn("crates/kiln/src", spec.get("waits", []))
        dest = Path(tempfile.mkdtemp(prefix="kiln-locks-waits-"))
        try:
            (dest / "scripts" / "locks").mkdir(parents=True)
            shutil.copy2(ROOT / "scripts" / "locks" / "hot.toml", dest / "scripts" / "locks" / "hot.toml")
            for h in [*spec["hot"], *spec["waits"]]:
                src = ROOT / h
                if src.is_dir():
                    shutil.copytree(src, dest / h, dirs_exist_ok=True)
                else:
                    (dest / h).parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(src, dest / h)
            before = check(dest)
            self.assertEqual(before.returncode, 0, before.stdout)
            for rel, planted, name in [
                ("crates/kiln/src/plans/review.rs", "static PLANTED: async_lock::Mutex<()> = async_lock::Mutex::new(());\n", "PLANTED"),
                ("crates/kiln/src/sync/tasks.rs", "static RUNG: std::sync::LazyLock<event_listener::Event> = std::sync::LazyLock::new(event_listener::Event::new);\n", "RUNG"),
            ]:
                f = dest / rel
                was = f.read_text()
                f.write_text(planted + was)
                after = check(dest)
                self.assertEqual(after.returncode, 1, after.stdout)
                self.assertIn(f"{rel}:1: {name}:", after.stdout)
                f.write_text(was)
        finally:
            shutil.rmtree(dest, ignore_errors=True)


WAITS = '''//! A module off the hot path: async_lock::Mutex named in a comment is no wait.
use std::sync::Arc;

/// Waits as event_listener::Event did, in its docs.
pub struct Feed {
    changed: crate::rt::Changed,
}

impl Feed {
    pub fn say(&self) -> &'static str {
        "an async_lock::Semaphore in a string is no wait either"
    }
}

#[cfg(test)]
mod tests {
    static ONE: async_lock::Mutex<()> = async_lock::Mutex::new(());
    fn f() { let e = event_listener::Event::new(); drop(e); }
}
'''


class Waits(unittest.TestCase):
    def test_g5_waits_a_clean_module_passes(self) -> None:
        t = Tree([], waits=["src"])
        try:
            t.file("src/feed.rs", WAITS)
            r = check(t.dir)
            self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        finally:
            t.close()

    def test_g5_waits_a_planted_wait_on_event_listener_fails(self) -> None:
        for planted, name in [
            ("    gate: async_lock::Mutex<()>,\n", "gate"),
            ("    copies: async_lock::Semaphore,\n", "copies"),
            ("    mirror: std::sync::Arc<async_lock::RwLock<()>>,\n", "mirror"),
            ("    ready: async_lock::OnceCell<()>,\n", "ready"),
            ("    room: event_listener::Event,\n", "room"),
        ]:
            t = Tree([], waits=["src"])
            try:
                t.file("src/feed.rs", WAITS.replace("    changed: crate::rt::Changed,\n", "    changed: crate::rt::Changed,\n" + planted))
                r = check(t.dir)
                self.assertEqual(r.returncode, 1, r.stdout)
                self.assertIn(f"src/feed.rs:7: {name}:", r.stdout)
            finally:
                t.close()

    def test_g5_waits_an_import_a_static_or_a_local_fails_too(self) -> None:
        t = Tree([], waits=["src"])
        try:
            t.file("src/feed.rs", "use event_listener::Listener as _;\n" + WAITS + "\nstatic GATE: async_lock::Mutex<()> = async_lock::Mutex::new(());\nfn g() {\n    let rung = event_listener::Event::new();\n    drop(rung);\n}\n")
            r = check(t.dir)
            self.assertEqual(r.returncode, 1, r.stdout)
            for name in ("use", "GATE", "rung"):
                self.assertIn(f": {name}:", r.stdout)
        finally:
            t.close()

    def test_g5_waits_a_listed_one_passes_and_a_gone_one_fails(self) -> None:
        listed = WAITS.replace("    changed: crate::rt::Changed,\n", "    changed: crate::rt::Changed,\n    room: event_listener::Event,\n")
        t = Tree([], waits=["src"], waiting=[("src/feed.rs", "room", "one task's: its forwarder alone waits on it")])
        try:
            t.file("src/feed.rs", listed)
            r = check(t.dir)
            self.assertEqual(r.returncode, 0, r.stdout)
            t.file("src/feed.rs", WAITS)
            r = check(t.dir)
            self.assertEqual(r.returncode, 1, r.stdout)
            self.assertIn("src/feed.rs room", r.stdout)
        finally:
            t.close()


def rewritten(text: str, track_caller: tuple[str, ...] = ()) -> tuple[str, int]:
    new, n = locks.rewrite_dep_file(text, track_caller)
    assert new.count("\n") == text.count("\n"), "every line where it was"
    return new, n


class DependencyRewrite(unittest.TestCase):
    """A dependency's copy takes the census's locks where it took std's and parking_lot's, every line where
    it was (a site's line is the registry's)."""

    def test_g5_deps_a_braced_import_names_the_census(self) -> None:
        new, n = rewritten("use std::sync::{Arc, Mutex};\n\nstruct S { inner: Arc<Mutex<u8>> }\nfn f(s: &S) { let _g = s.inner.lock().unwrap(); }\n")
        self.assertEqual(n, 1)
        self.assertEqual(new.split("\n")[0], "use {std::sync::{Arc}, kiln_census::st::Mutex};")
        self.assertIn("struct S { inner: Arc<Mutex<u8>> }", new)

    def test_g5_deps_a_nested_import_over_lines_keeps_its_lines(self) -> None:
        text = "use std::{\n    io::{Read, Write},\n    sync::{Arc, Mutex, MutexGuard},\n    task::Poll,\n};\n\nstruct Shared<S>(Mutex<S>);\n"
        new, n = rewritten(text)
        self.assertEqual(n, 1)
        lines = new.split("\n")
        self.assertEqual(lines[0], "use {std::{io::{Read, Write}, sync::{Arc, MutexGuard}, task::Poll}, kiln_census::st::Mutex}")
        self.assertEqual(lines[4], ";")
        self.assertEqual(lines[6], "struct Shared<S>(Mutex<S>);", "the code after it where it was")

    def test_g5_deps_visibility_and_attributes_stay_the_declarations(self) -> None:
        text = "mod sync {\n    #[allow(unused)]\n    #[cfg(feature = \"std\")]\n    pub(super) use std::sync::{Mutex, MutexGuard};\n}\n"
        new, _ = rewritten(text)
        self.assertIn('    #[cfg(feature = "std")]\n    pub(super) use {std::sync::{MutexGuard}, kiln_census::st::Mutex};', new)

    def test_g5_deps_a_files_own_name_for_std_sync_is_followed(self) -> None:
        text = "use std::sync;\nuse sync::{Condvar, Mutex};\nfn f() { let _m = sync::RwLock::new(0); let _c = crate::sync::Mutex::new(0); }\n"
        new, n = rewritten(text)
        self.assertEqual(n, 2)
        self.assertIn("use {sync::{Condvar}, kiln_census::st::Mutex};", new)
        self.assertIn("kiln_census::st::RwLock::new(0)", new)
        self.assertIn("crate::sync::Mutex::new(0)", new, "the crate's own module is its own")

    def test_g5_deps_paths_in_code_and_parking_lot(self) -> None:
        text = (
            "struct A(std::sync::Mutex<u8>);\n"
            "static B: ::std::sync::RwLock<u8> = ::std::sync::RwLock::new(0);\n"
            "struct C { m: parking_lot::Mutex<u8> }\n"
            "use parking_lot::{Condvar, RwLock as PlRwLock};\n"
            "fn g() -> my::std::sync::Mutex<u8> { todo() }\n"
        )
        new, n = rewritten(text)
        self.assertEqual(n, 5)
        self.assertIn("struct A(kiln_census::st::Mutex<u8>);", new)
        self.assertIn("static B: kiln_census::st::RwLock<u8> = kiln_census::st::RwLock::new(0);", new)
        self.assertIn("struct C { m: kiln_census::pl::Mutex<u8> }", new)
        self.assertIn("use {parking_lot::{Condvar}, kiln_census::pl::RwLock as PlRwLock};", new)
        self.assertIn("my::std::sync::Mutex<u8>", new, "another module's path is its own")

    def test_g5_deps_a_renamed_import_keeps_its_name(self) -> None:
        new, n = rewritten("use std::sync::Mutex as StdMutex;\nuse std::sync::{Arc, RwLock as Rw};\n")
        self.assertEqual(n, 2)
        self.assertEqual(new.split("\n")[:2], ["use kiln_census::st::Mutex as StdMutex;", "use {std::sync::{Arc}, kiln_census::st::RwLock as Rw};"])

    def test_g5_deps_comments_and_strings_stay(self) -> None:
        text = '// use std::sync::Mutex;\nconst S: &str = "std::sync::Mutex";\n/* parking_lot::RwLock */\n'
        new, n = rewritten(text)
        self.assertEqual((new, n), (text, 0))

    def test_g5_deps_a_helper_that_takes_the_lock_for_its_caller_is_track_caller(self) -> None:
        text = (
            "impl<S> Shared<S> {\n"
            "    fn lock(&self) -> MutexGuard<'_, S> { self.0.lock().unwrap() }\n"
            "    pub(crate) fn with_inner<R>(&self, f: impl FnOnce(&mut S) -> R) -> R { f(&mut self.0.lock().unwrap()) }\n"
            "    async fn wait(&self) -> MutexGuard<'_, S> { todo!() }\n"
            "    fn other(&self) {}\n"
            "}\n"
        )
        new, _ = rewritten(text, ("with_inner",))
        lines = new.split("\n")
        self.assertTrue(lines[1].startswith("    #[track_caller] fn lock("), lines[1])
        self.assertTrue(lines[2].startswith("    #[track_caller] pub(crate) fn with_inner<R>("), lines[2])
        self.assertFalse("track_caller" in lines[3] or "track_caller" in lines[4], new)
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            locks.rewrite_dep_file(text, ("gone",))

    def test_g5_deps_a_glob_import_of_a_lock_fails_unless_no_lock_is_named(self) -> None:
        with contextlib.redirect_stderr(io.StringIO()) as err, self.assertRaises(SystemExit):
            locks.rewrite_dep_file("use std::sync::*;\nstatic M: Mutex<u8> = Mutex::new(0);\n")
        self.assertIn("glob import", err.getvalue())
        self.assertEqual(rewritten("use std::sync::*;\nfn f(a: Arc<u8>) {}\n"), ("use std::sync::*;\nfn f(a: Arc<u8>) {}\n", 0))

    def test_g5_deps_every_dependency_listed_says_where_kiln_reaches_it(self) -> None:
        spec = tomllib.loads(locks.DEPS.read_text())["crate"]
        self.assertTrue(spec)
        for c in spec:
            for key in ("name", "locks", "reached"):
                self.assertTrue(str(c.get(key, "")).strip(), f"{c.get('name')}: no {key}")
            for t in c.get("track_caller", []):
                self.assertRegex(t, r"^[\w./-]+\.rs:\w+$")


LOCK = """# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = "demo-dep"
version = "1.2.3"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaaa"
dependencies = [
 "getrandom 0.2.17",
 "getrandom 0.4.3",
 "zeta",
]

[[package]]
name = "kiln"
version = "0.2.0"
dependencies = [
 "demo-dep",
 "kiln-http",
 "parking_lot",
 "quiver",
]

[[package]]
name = "kiln-cd"
version = "0.2.0"

[[package]]
name = "kiln-http"
version = "0.2.0"

[[package]]
name = "lone"
version = "0.1.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "bbbb"

[[package]]
name = "parking_lot"
version = "0.12.5"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "cccc"

[[package]]
name = "quiver"
version = "0.1.0"
"""


class CensusLock(unittest.TestCase):
    def test_g5_deps_cargo_lock_is_written_as_cargo_writes_it(self) -> None:
        out = locks.census_lock(LOCK, [("demo-dep", "1.2.3"), ("lone", "0.1.0")], [])
        blocks = out.rstrip("\n").split("\n\n")
        names = [b.split('name = "')[1].split('"')[0] for b in blocks[1:]]
        self.assertEqual(names, ["demo-dep", "kiln", "kiln-cd", "kiln-census", "kiln-http", "lone", "parking_lot", "quiver"])
        self.assertEqual(blocks[1], '[[package]]\nname = "demo-dep"\nversion = "1.2.3"\ndependencies = [\n "getrandom 0.2.17",\n "getrandom 0.4.3",\n "kiln-census",\n "zeta",\n]')
        self.assertIn(' "demo-dep",\n "kiln-census",\n "kiln-http",', blocks[2])
        self.assertEqual(blocks[4], '[[package]]\nname = "kiln-census"\nversion = "0.0.0"')
        self.assertEqual(blocks[6], '[[package]]\nname = "lone"\nversion = "0.1.0"\ndependencies = [\n "kiln-census",\n]')
        self.assertIn('checksum = "cccc"', blocks[7], "what isn't patched stays the registry's")
        with_pl = locks.census_lock(LOCK, [], ["parking_lot"])
        self.assertIn('[[package]]\nname = "kiln-census"\nversion = "0.0.0"\ndependencies = [\n "parking_lot",\n]', with_pl)
        self.assertTrue(out.endswith('checksum = "cccc"\n\n[[package]]\nname = "quiver"\nversion = "0.1.0"\n'), out[-120:])


DEP_SRC = """//! A dependency that locks inside.
use std::sync::{Arc, Mutex, MutexGuard};

pub struct Shared {
    inner: Arc<Mutex<u8>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, u8> {
        self.inner.lock().unwrap()
    }

    pub fn bump(&self) {
        *self.lock() += 1;
    }
}
"""


class Fixture:
    """A tree as census-tree needs one (a checkout's files: kiln, kiln-http and Quiver, their anchors and a
    Cargo.lock) and a cargo registry with one dependency that locks inside, `demo-dep` 1.2.3."""

    def __init__(self, deps: str):
        self.dir = Path(tempfile.mkdtemp(prefix="kiln-locks-deps-"))
        self.src, self.home, self.dest = self.dir / "src", self.dir / "cargo", self.dir / "census"
        files = {
            "Cargo.toml": '[workspace]\nmembers = ["crates/*"]\nexclude = ["vendor/quiver"]\n\n[patch.crates-io]\nquiver = { path = "vendor/quiver/crates/quiver" }\n',
            "Cargo.lock": LOCK,
            "crates/kiln/Cargo.toml": '[package]\nname = "kiln"\nversion = "0.2.0"\n\n[features]\njemalloc = ["dep:tikv-jemallocator", "dep:tikv-jemalloc-sys"]\n\n[dependencies]\nparking_lot = "0.12"\ndemo-dep = "1.2.3"\n',
            "crates/kiln/src/lib.rs": "pub mod hub;\npub mod metrics;\npub mod store;\n",
            "crates/kiln/src/hub.rs": "use parking_lot::Mutex;\npub struct Hub { agents: Mutex<u8> }\n",
            "crates/kiln/src/metrics.rs": "fn render() {\n    let mut o = String::new();\n" + locks.RENDER_ANCHOR[1] + "\n}\n",
            "crates/kiln/src/store.rs": "impl Store {\n" + locks.STORE_CALL[1] + "    where F: FnOnce() {}\n}\n",
            "crates/kiln-http/Cargo.toml": '[package]\nname = "kiln-http"\nversion = "0.2.0"\n',
            "crates/kiln-http/src/lib.rs": "pub fn serve() {}\n",
            "vendor/quiver/crates/quiver/Cargo.toml": '[package]\nname = "quiver"\nversion = "0.1.0"\n',
            "vendor/quiver/crates/quiver/src/lib.rs": "pub fn node() {}\n",
        }
        for rel, text in files.items():
            (self.src / rel).parent.mkdir(parents=True, exist_ok=True)
            (self.src / rel).write_text(text)
        crate = self.home / "registry" / "src" / "index.test" / "demo-dep-1.2.3"
        (crate / "src").mkdir(parents=True)
        (crate / "Cargo.toml").write_text('[package]\nname = "demo-dep"\nversion = "1.2.3"\nedition = "2021"\n\n[dependencies.zeta]\nversion = "1"\n')
        (crate / "src" / "lib.rs").write_text(DEP_SRC)
        self.deps = self.dir / "deps.toml"
        self.deps.write_text(deps)

    def census(self) -> str:
        # Every file of the tree, as git's list of a checkout's would be (CI's python image has no git).
        listed = sorted(p.relative_to(self.src).as_posix() for p in self.src.rglob("*") if p.is_file())
        out = io.StringIO()
        with (
            mock.patch.dict(os.environ, {"CARGO_HOME": str(self.home)}),
            mock.patch.object(locks, "DEPS", self.deps),
            mock.patch.object(locks, "tree_files", lambda _tree: listed),
            contextlib.redirect_stdout(out),
        ):
            locks.census_tree(self.src, self.dest, None)
        return out.getvalue()

    def close(self) -> None:
        shutil.rmtree(self.dir, ignore_errors=True)


DEMO_DEPS = '[[crate]]\nname = "demo-dep"\nlocks = "a shared u8"\nreached = "the test"\n\n[[crate]]\nname = "absent"\nlocks = "x"\nreached = "nowhere"\n'


class CensusTreeDeps(unittest.TestCase):
    def test_g5_deps_census_tree_counts_a_dependencys_locks(self) -> None:
        f = Fixture(DEMO_DEPS)
        try:
            said = f.census()
            self.assertIn("demo-dep@1.2.3: 1 lock types rerouted", said)
            self.assertIn("absent: not in Cargo.lock: not counted", said)
            copy = f.dest / "census-deps" / "demo-dep" / "1.2.3"
            lib = (copy / "src" / "lib.rs").read_text().split("\n")
            self.assertEqual(len(lib), len(DEP_SRC.split("\n")))
            self.assertEqual(lib[1], "use {std::sync::{Arc, MutexGuard}, kiln_census::st::Mutex};")
            self.assertEqual(lib[8], "    #[track_caller] fn lock(&self) -> MutexGuard<'_, u8> {")
            manifest = tomllib.loads((copy / "Cargo.toml").read_text())
            self.assertEqual(manifest["dependencies"]["kiln-census"]["path"], "../../kiln-census")
            ours = f.dest / "census-deps" / "kiln-census"
            self.assertEqual(tomllib.loads((ours / "Cargo.toml").read_text())["package"]["name"], "kiln-census")
            self.assertNotIn("pub mod pl", (ours / "src" / "lib.rs").read_text(), "no copy takes parking_lot's locks")
            self.assertIn("fn site_file", (ours / "src" / "lib.rs").read_text())
            root = tomllib.loads((f.dest / "Cargo.toml").read_text())
            self.assertEqual(root["workspace"]["exclude"], ["census-deps", "vendor/quiver"])
            self.assertEqual(root["patch"]["crates-io"]["demo-dep"], {"path": "census-deps/demo-dep/1.2.3"})
            self.assertEqual(root["patch"]["crates-io"]["quiver"], {"path": "vendor/quiver/crates/quiver"})
            kiln = tomllib.loads((f.dest / "crates/kiln/Cargo.toml").read_text())
            self.assertEqual(kiln["dependencies"]["kiln-census"], {"path": "../../census-deps/kiln-census"})
            self.assertEqual(kiln["features"]["jemalloc"], ["dep:tikv-jemallocator", "dep:tikv-jemalloc-sys", "tikv-jemalloc-sys/stats"])
            lock = (f.dest / "Cargo.lock").read_text()
            self.assertEqual(lock, locks.census_lock(LOCK, [("demo-dep", "1.2.3")], []))
            lib_rs = (f.dest / "crates/kiln/src/lib.rs").read_text()
            self.assertIn("pub mod census;", lib_rs)
            self.assertIn("pub mod census_jemalloc;", lib_rs)
            self.assertTrue((f.dest / "crates/kiln/src/census_jemalloc.rs").is_file())
            metrics = (f.dest / "crates/kiln/src/metrics.rs").read_text()
            self.assertIn("quiver::census::render(&mut o); kiln_census::render(&mut o); crate::census_jemalloc::render(&mut o);", metrics)
            self.assertIn("use crate::census::pl::Mutex;", (f.dest / "crates/kiln/src/hub.rs").read_text(), "kiln's own, as before")
        finally:
            f.close()

    def test_g5_deps_a_crate_cargo_hasnt_got_fails_and_says_how(self) -> None:
        f = Fixture(DEMO_DEPS)
        try:
            shutil.rmtree(f.home / "registry")
            with contextlib.redirect_stderr(io.StringIO()) as err, self.assertRaises(SystemExit):
                f.census()
            self.assertIn("cargo fetch --locked", err.getvalue())
        finally:
            f.close()

    def test_g5_deps_a_crate_only_cargos_download_has_is_unpacked(self) -> None:
        import tarfile

        f = Fixture(DEMO_DEPS)
        try:
            unpacked = f.home / "registry" / "src" / "index.test" / "demo-dep-1.2.3"
            cache = f.home / "registry" / "cache" / "index.test"
            cache.mkdir(parents=True)
            with tarfile.open(cache / "demo-dep-1.2.3.crate", "w:gz") as t:
                t.add(unpacked, arcname="demo-dep-1.2.3")
            shutil.rmtree(f.home / "registry" / "src")
            f.census()
            self.assertIn("kiln_census::st::Mutex", (f.dest / "census-deps/demo-dep/1.2.3/src/lib.rs").read_text())
        finally:
            f.close()

    def test_g5_deps_two_versions_of_a_crate_are_both_counted(self) -> None:
        f = Fixture(DEMO_DEPS)
        try:
            two = LOCK.replace('[[package]]\nname = "kiln"\n', '[[package]]\nname = "demo-dep"\nversion = "2.0.0"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\nchecksum = "dddd"\n\n[[package]]\nname = "kiln"\n', 1)
            (f.src / "Cargo.lock").write_text(two)
            other = f.home / "registry" / "src" / "index.test" / "demo-dep-2.0.0"
            shutil.copytree(f.home / "registry" / "src" / "index.test" / "demo-dep-1.2.3", other)
            f.census()
            patch = tomllib.loads((f.dest / "Cargo.toml").read_text())["patch"]["crates-io"]
            self.assertEqual(patch["demo-dep-census-1-2-3"], {"path": "census-deps/demo-dep/1.2.3", "package": "demo-dep"})
            self.assertEqual(patch["demo-dep-census-2-0-0"], {"path": "census-deps/demo-dep/2.0.0", "package": "demo-dep"})
        finally:
            f.close()


def lock_seen(taken: float, contended: float = 0.0, wait_s: float = 0.0, kind: str = "std_mutex", addrs: str = "", copies: int = 1) -> dict:
    return {"kind": kind, "taken": taken, "contended": contended, "wait_s": wait_s, "fill_taken": taken / 2, "fill_contended": 0.0, "fill_wait_s": 0.0, "max_wait_s": wait_s, "addrs": addrs, "copies": copies}


class RankGroups(unittest.TestCase):
    """`rank` says kiln's, Quiver's, the dependencies' and the allocator's locks apart."""

    def setUp(self) -> None:
        self.dir = Path(tempfile.mkdtemp(prefix="kiln-locks-rank-"))
        self.tree, self.home = self.dir / "tree", self.dir / "cargo"
        for rel, text in {
            "crates/kiln/src/hub.rs": "pub struct Hub {\n    agents: parking_lot::Mutex<u8>,\n}\nfn f(h: &Hub) { let _g = h.agents.lock(); }\n",
            "vendor/quiver/crates/quiver/src/node.rs": "fn g(n: &Node) {\n    let _t = n.topics.lock();\n}\n",
        }.items():
            (self.tree / rel).parent.mkdir(parents=True, exist_ok=True)
            (self.tree / rel).write_text(text)
        crate = self.home / "registry" / "src" / "index.test" / "demo-dep-1.2.3"
        (crate / "src").mkdir(parents=True)
        (crate / "Cargo.toml").write_text('[package]\nname = "demo-dep"\nversion = "1.2.3"\n')
        (crate / "src" / "lib.rs").write_text(DEP_SRC)
        run = {
            "profile": {"window_s": 10},
            "rounds": [
                {
                    "fill_s": 5,
                    "locks": {
                        "crates/kiln/src/hub.rs:4:28 mutex": lock_seen(100, kind="mutex", addrs="7f00"),
                        "vendor/quiver/crates/quiver/src/node.rs:2:16 mutex": lock_seen(50, 1, 0.001, kind="mutex"),
                        "demo-dep@1.2.3:src/lib.rs:10:20 std_mutex": lock_seen(200, 2, 0.002, addrs="7f00"),
                        "jemalloc:arena.0.bins jemalloc": lock_seen(1000, 10, 0.01, kind="jemalloc"),
                    },
                }
            ],
        }
        self.run = self.dir / "run.json"
        self.run.write_text(json.dumps(run))

    def tearDown(self) -> None:
        shutil.rmtree(self.dir, ignore_errors=True)

    def test_g5_deps_rank_says_whose_each_lock_is(self) -> None:
        with mock.patch.dict(os.environ, {"CARGO_HOME": str(self.home)}):
            rows, _ = locks.rank([self.run], self.tree)
        by = {r["owner"]: r for r in rows}
        self.assertEqual(sorted(by), ["demo-dep@1.2.3", "jemalloc", "kiln", "quiver"], "one address, two owners: two locks")
        self.assertEqual({r["owner"]: r["group"] for r in rows}, {"kiln": "kiln", "quiver": "quiver", "demo-dep@1.2.3": "dependencies", "jemalloc": "allocator"})
        self.assertEqual(by["kiln"]["lock"], "h.agents")
        self.assertEqual(by["demo-dep@1.2.3"]["lock"], "self.inner")
        self.assertEqual(by["demo-dep@1.2.3"]["declared"], ["demo-dep@1.2.3:src/lib.rs:5"])
        self.assertEqual(by["demo-dep@1.2.3"]["files"], ["demo-dep@1.2.3:src/lib.rs"])
        self.assertEqual(by["jemalloc"]["lock"], "arena.0.bins")
        self.assertAlmostEqual(by["jemalloc"]["taken_per_s"], 100.0)
        self.assertAlmostEqual(by["jemalloc"]["wait_ms_per_s"], 1.0)

    def test_g5_deps_ranks_tables_are_by_owner(self) -> None:
        r = subprocess.run([sys.executable, str(LOCKS), "rank", str(self.run), "--tree", str(self.tree)], capture_output=True, text=True, env={**os.environ, "CARGO_HOME": str(self.home)})
        self.assertEqual(r.returncode, 0, r.stderr)
        md = r.stdout
        titles = [locks.GROUPS[g] + ":" for g in ("kiln", "quiver", "dependencies", "allocator")]
        at = [md.index(t) for t in titles]
        self.assertEqual(at, sorted(at), "kiln's, Quiver's, the dependencies', the allocator's")
        deps = md[at[2] : at[3]]
        self.assertIn("`self.inner`", deps)
        self.assertIn("`demo-dep@1.2.3:src/lib.rs:5`", deps)
        self.assertIn("`arena.0.bins`", md[at[3] :])
        j = subprocess.run([sys.executable, str(LOCKS), "rank", str(self.run), "--tree", str(self.tree), "--json"], capture_output=True, text=True, env={**os.environ, "CARGO_HOME": str(self.home)})
        self.assertEqual({row["group"] for row in json.loads(j.stdout)["locks"]}, {"kiln", "quiver", "dependencies", "allocator"})

    def test_g5_deps_rank_reads_a_census_trees_own_copy_without_the_registry(self) -> None:
        """Built on one machine and run on another, the census tree has each dependency's lines."""
        copy = self.tree / "census-deps" / "demo-dep" / "1.2.3" / "src"
        copy.mkdir(parents=True)
        shutil.copy2(self.home / "registry" / "src" / "index.test" / "demo-dep-1.2.3" / "src" / "lib.rs", copy / "lib.rs")
        with mock.patch.dict(os.environ, {"CARGO_HOME": str(self.dir / "no-cargo")}):
            rows, _ = locks.rank([self.run], self.tree)
        dep = next(r for r in rows if r["owner"] == "demo-dep@1.2.3")
        self.assertEqual((dep["lock"], dep["declared"]), ("self.inner", ["demo-dep@1.2.3:src/lib.rs:5"]))

    def test_g5_deps_a_group_with_nothing_taken_says_so(self) -> None:
        md = locks.markdown([], [], 1)
        self.assertIn(locks.NONE_TAKEN["allocator"], md)
        self.assertIn(locks.NONE_TAKEN["dependencies"], md)


class RankGrowth(unittest.TestCase):
    """Goal 5, plan 01a1a59c2aae step 1 (census-B's event-listener rates below zero): `rank` ranks each site's
    growth as kiln-lab took it, the site's copies summed in every scrape first (`copies`), and only then joins
    sites into locks by the addresses they took; figures that can't be counters' growth it refuses."""

    EL = "event-listener@5.4.2:src/intrusive.rs"

    def setUp(self) -> None:
        self.dir = Path(tempfile.mkdtemp(prefix="kiln-locks-growth-"))
        self.tree = self.dir / "tree"
        self.tree.mkdir()

    def tearDown(self) -> None:
        shutil.rmtree(self.dir, ignore_errors=True)

    def rank(self, rounds: list[dict]) -> list[dict]:
        run = self.dir / "run.json"
        run.write_text(json.dumps({"profile": {"window_s": 10}, "rounds": [{"index": i + 1, "fill_s": 5, "locks": seen} for i, seen in enumerate(rounds)]}))
        with mock.patch.dict(os.environ, {"CARGO_HOME": str(self.dir / "no-cargo")}):
            rows, _ = locks.rank([run], self.tree)
        return rows

    def test_g5_s1_a_sites_rate_counts_once_while_its_locks_addresses_churn(self) -> None:
        """event-listener's locks are each channel's, made and dropped all the time: each round's server has its
        own, at addresses of their own, its sites share a channel's address in one round and not the next, and
        a freed lock's address is kiln's in another. Each site counts once, in one lock, whatever it joins; no
        rate is below zero."""
        insert, remove, notify, register = (f"{self.EL}:{line}:14 std_mutex" for line in (181, 220, 226, 238))
        hub = "crates/kiln/src/hub.rs:40:20 mutex"
        rounds = [
            {
                insert: lock_seen(4000, 10, 0.004, addrs="a1 a2", copies=2),
                remove: lock_seen(3000, 5, 0.002, addrs="a2", copies=2),
                notify: lock_seen(500, addrs="a3", copies=3),
                register: lock_seen(800, 1, 0.001, addrs="a4", copies=2),
                hub: lock_seen(100, kind="mutex", addrs="a3"),
            },
            {
                insert: lock_seen(4200, 8, 0.003, addrs="b1", copies=2),
                remove: lock_seen(2900, 4, 0.002, addrs="b2", copies=2),
                notify: lock_seen(600, addrs="b1 b2", copies=3),
                register: lock_seen(700, addrs="b3", copies=2),
                hub: lock_seen(110, kind="mutex", addrs="b3"),
            },
            {
                insert: lock_seen(3800, 12, 0.005, addrs="c1", copies=2),
                register: lock_seen(900, 2, 0.002, addrs="c1 a1", copies=2),
                hub: lock_seen(90, kind="mutex", addrs="c1"),
            },
        ]
        rows = self.rank(rounds)
        # Each site's median over the rounds, a round it didn't take counting 0, per second of the 10 s window.
        taken = {insert: 400.0, remove: 290.0, notify: 50.0, register: 80.0, hub: 10.0}
        by = {r["owner"]: r for r in rows}
        self.assertEqual(sorted(by), ["event-listener@5.4.2", "kiln"], "one lock an owner: the channels' sites joined by shared addresses, never kiln's")
        listener = by["event-listener@5.4.2"]
        self.assertEqual({f"{d['site']} {d['kind']}": d["taken_per_s"] for d in listener["sites"]}, {k: v for k, v in taken.items() if k != hub})
        self.assertAlmostEqual(listener["taken_per_s"], 820.0)
        self.assertAlmostEqual(listener["contended_per_s"], 1.0 + 0.4 + 0.0 + 0.1)
        self.assertAlmostEqual(listener["wait_ms_per_s"], 0.4 + 0.2 + 0.0 + 0.1)
        self.assertAlmostEqual(by["kiln"]["taken_per_s"], 10.0)
        self.assertAlmostEqual(sum(r["taken_per_s"] for r in rows), sum(taken.values()), msg="every site counted once")
        figures = ("taken_per_s", "contended_per_s", "wait_ms_per_s", "max_wait_ms", "fill_per_s", "fill_wait_ms_per_s")
        self.assertTrue(all(r[f] >= 0 for r in rows for f in figures), rows)

    def test_g5_s1_figures_that_cant_be_counters_growth_are_refused(self) -> None:
        """census-B's first round, as the kiln-lab of the time kept it: no `copies`, and the window of
        intrusive.rs:220 a later copy's count less an earlier one's. Neither that, nor a growth below zero from
        any kiln-lab, is ranked, and the refusal says why."""
        site = f"{self.EL}:220:14 std_mutex"
        census_b = {"kind": "std_mutex", "taken": -4873.0, "contended": -542.0, "wait_s": 0.004519069, "fill_taken": 6404.0, "fill_contended": 479.0, "fill_wait_s": 0.000176259, "max_wait_s": 0.001471389, "addrs": "7f5b79042000 7f5bd5e281b0"}
        for figures, says in [
            (census_b, ["didn't sum a site's copies (1 site without `copies`)", f"{site} taken -4873 (round 1)", f"{site} contended -542 (round 1)", "a kiln-lab that sums them"]),
            ({**lock_seen(300), "copies": 0}, ["didn't sum a site's copies"]),
            ({**census_b, "copies": 2}, ["2 growths below zero", f"{site} taken -4873 (round 1)"]),
        ]:
            with self.subTest(figures=figures):
                with contextlib.redirect_stderr(io.StringIO()) as err, self.assertRaises(SystemExit):
                    self.rank([{site: figures}])
                for s in says:
                    self.assertIn(s, err.getvalue())
        self.assertEqual(len(self.rank([{site: lock_seen(300, copies=2)}])), 1, "a sound site is ranked")


if __name__ == "__main__":
    unittest.main()
