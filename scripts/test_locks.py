#!/usr/bin/env python3
"""Tests of scripts/locks.py check (goal 5 of the API runtime program, plan 01a1a59c2aae step 3): it
fails on a lock planted on the hot path, and passes what only a test build compiles, what comments and
strings say, and the locks hot.toml lists with why.

  python3 scripts/test_locks.py
"""

from __future__ import annotations

import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LOCKS = ROOT / "scripts" / "locks.py"


def check(root: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run([sys.executable, str(LOCKS), "check", "--root", str(root)], capture_output=True, text=True)


class Tree:
    """A tree of its own: hot.toml and the files it names."""

    def __init__(self, hot: list[str], allowed: list[tuple[str, str, str]] | None = None):
        self.dir = Path(tempfile.mkdtemp(prefix="kiln-locks-"))
        lines = ["hot = [" + ", ".join(f'"{h}"' for h in hot) + "]"]
        for at, name, why in allowed or []:
            lines += ["[[allowed]]", f'at = "{at}"', f'name = "{name}"', f'why = "{why}"']
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
            for h in spec["hot"]:
                src = ROOT / h
                if src.is_dir():
                    shutil.copytree(src, dest / h)
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


if __name__ == "__main__":
    unittest.main()
