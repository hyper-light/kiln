#!/usr/bin/env python3
"""Tests of scripts/vendor.py (CP-M1; docs/vendoring.md), each "do X, expect Y", against real git
repositories made here. Offline: upstreams are local repositories.

    python3 scripts/test_vendor.py
"""

from __future__ import annotations

import contextlib
import gzip
import importlib.util
import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("vendor", HERE / "vendor.py")
assert _spec and _spec.loader
vendor = importlib.util.module_from_spec(_spec)
sys.modules["vendor"] = vendor
_spec.loader.exec_module(vendor)

GIT_ENV = {**os.environ, **vendor.WORK_ENV}

LIB_RS = "".join(f"pub fn f{i}() -> u32 {{ {i} }}\n" for i in range(12))


def sh_git(cwd: Path, *args: str) -> str:
    r = subprocess.run(["git", *args], cwd=cwd, env=GIT_ENV, capture_output=True, check=True)
    return r.stdout.decode().strip()


def write(root: Path, files: dict[str, str]) -> None:
    for rel, text in files.items():
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)


def snapshot(root: Path) -> dict[str, bytes]:
    out = {}
    for p in sorted(root.rglob("*")):
        if p.is_file() and ".git" not in p.relative_to(root).parts:
            out[p.relative_to(root).as_posix()] = p.read_bytes()
    return out


class Lab:
    """An upstream repository and a kiln-like repository that vendors it."""

    def __init__(self, tmp: Path):
        self.tmp = tmp
        self.up = tmp / "upstream"
        self.kiln = tmp / "kiln"
        for d in (self.up, self.kiln):
            d.mkdir()
            sh_git(d, "init", "-q", "-b", "main")
        write(self.up, {"src/lib.rs": LIB_RS, "LICENSE": "MIT\n", "README.md": "upstream's, not taken\n"})
        self.c1 = self.commit_up("one")

    def commit_up(self, msg: str) -> str:
        sh_git(self.up, "add", "-A")
        sh_git(self.up, "commit", "-q", "-m", msg)
        return sh_git(self.up, "rev-parse", "HEAD")

    def manifest(self, commit: str, extra: str = "", gate: str = "[]", take: str | None = '["src", "LICENSE"]') -> None:
        take_line = f"take = {take}\n" if take is not None else ""
        write(
            self.kiln,
            {
                "vendor/MANIFEST.toml": (
                    f"format = 1\ngate = {gate}\n\n[projects.demo]\nkind = \"git\"\nrepository = \"{self.up}\"\n"
                    f"commit = \"{commit}\"\npath = \"vendor/demo\"\n{take_line}own = [\"UPSTREAM.md\"]\n{extra}"
                )
            },
        )

    def vendor(self, *args: str) -> tuple[int, str]:
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            code = vendor.main(["--root", str(self.kiln), *args])
        return code, err.getvalue()


class VendorTestCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        tmp = Path(self._tmp.name)
        os.environ["KILN_VENDOR_CACHE"] = str(tmp / "cache")
        self.lab = Lab(tmp)

    def tearDown(self) -> None:
        os.environ.pop("KILN_VENDOR_CACHE", None)
        self._tmp.cleanup()

    def vendored_with_a_change(self) -> None:
        """demo vendored at c1, with one recorded change on src/lib.rs's third line."""
        lab = self.lab
        lab.manifest(lab.c1)
        code, err = lab.vendor("demo", lab.c1, "--no-gate")
        self.assertEqual(code, 0, err)
        lib = lab.kiln / "vendor/demo/src/lib.rs"
        lib.write_text(lib.read_text().replace("f2() -> u32 { 2 }", "f2() -> u32 { 2 + 0 } // kiln"))
        code, err = lab.vendor("record", "demo", "f2 says kiln", "--why", "a test of recording")
        self.assertEqual(code, 0, err)
        write(lab.kiln, {"vendor/demo/UPSTREAM.md": "| src/lib.rs | f2 | a test | 0001-f2-says-kiln.patch |\n"})


class GitProjects(VendorTestCase):
    def test_t_m1_1_a_planted_conflict_stops_the_revendor_naming_the_file_and_the_change(self) -> None:
        """T-M1.1: a recorded change and a new upstream commit that touch the same line. The
        re-vendor stops, names the file and the change, and changes nothing."""
        self.vendored_with_a_change()
        lab = self.lab
        lib = lab.up / "src/lib.rs"
        lib.write_text(lib.read_text().replace("f2() -> u32 { 2 }", "f2() -> u32 { 22 }"))
        c2 = lab.commit_up("upstream changes f2")
        before = snapshot(lab.kiln)
        code, err = lab.vendor("demo", c2, "--no-gate")
        self.assertEqual(code, 1, err)
        self.assertIn("src/lib.rs", err)
        self.assertIn("0001-f2-says-kiln.patch", err)
        self.assertIn("f2 says kiln", err)
        self.assertIn("Nothing was changed", err)
        self.assertEqual(snapshot(lab.kiln), before)

    def conflicting_upstream(self) -> str:
        """An upstream commit that rewrites the line the recorded change edits."""
        lib = self.lab.up / "src/lib.rs"
        lib.write_text(lib.read_text().replace("f2() -> u32 { 2 }", "f2() -> u32 { 22 }"))
        return self.lab.commit_up("upstream changes f2")

    def test_a_conflict_resolved_and_continued_takes_the_revision_with_the_change_reworked(self) -> None:
        """AC-M1.1: the conflict waits in a folder of its own; resolved there, --continue takes the
        new revision with the change as resolved, refreshed, and the manifest moved."""
        self.vendored_with_a_change()
        lab = self.lab
        c2 = self.conflicting_upstream()
        before = snapshot(lab.kiln)
        code, err = lab.vendor("demo", c2, "--no-gate")
        self.assertEqual(code, 1, err)
        where = vendor.pending_dir("demo")
        self.assertIn(str(where), err)
        self.assertIn("--continue", err)
        self.assertEqual(snapshot(lab.kiln), before)
        self.assertIn("<<<<<<< ", (where / "src/lib.rs").read_text())
        # The change reworked onto upstream's new line.
        (where / "src/lib.rs").write_text(LIB_RS.replace("f2() -> u32 { 2 }", "f2() -> u32 { 22 + 0 } // kiln"))
        code, err = lab.vendor("demo", "--continue", "--no-gate")
        self.assertEqual(code, 0, err)
        self.assertIn("pub fn f2() -> u32 { 22 + 0 } // kiln", (lab.kiln / "vendor/demo/src/lib.rs").read_text())
        self.assertIn(f'commit = "{c2}"', (lab.kiln / "vendor/MANIFEST.toml").read_text())
        patch = (lab.kiln / "vendor/patches/demo/0001-f2-says-kiln.patch").read_text()
        self.assertIn("+pub fn f2() -> u32 { 22 + 0 } // kiln", patch)
        self.assertTrue(patch.startswith("Subject: f2 says kiln\n"))
        self.assertFalse(where.exists())
        code, err = lab.vendor("check", "demo")
        self.assertEqual(code, 0, err)

    def test_a_second_conflict_stops_the_continue_again(self) -> None:
        self.vendored_with_a_change()
        lab = self.lab
        lic = lab.kiln / "vendor/demo/LICENSE"
        lic.write_text("MIT, kiln's\n")
        code, err = lab.vendor("record", "demo", "LICENSE says kiln", "--why", "a second change")
        self.assertEqual(code, 0, err)
        write(lab.kiln, {"vendor/demo/UPSTREAM.md": "0001-f2-says-kiln.patch 0002-license-says-kiln.patch\n"})
        (lab.up / "LICENSE").write_text("MIT, upstream's\n")
        c2 = self.conflicting_upstream()
        code, err = lab.vendor("demo", c2, "--no-gate")
        self.assertEqual(code, 1, err)
        self.assertIn("0001-f2-says-kiln.patch", err)
        where = vendor.pending_dir("demo")
        (where / "src/lib.rs").write_text(LIB_RS.replace("f2() -> u32 { 2 }", "f2() -> u32 { 22 + 0 } // kiln"))
        code, err = lab.vendor("demo", "--continue", "--no-gate")
        self.assertEqual(code, 1, err)
        self.assertIn("0002-license-says-kiln.patch", err)
        self.assertIn("LICENSE", err)
        (where / "LICENSE").write_text("MIT, upstream's and kiln's\n")
        code, err = lab.vendor("demo", "--continue", "--no-gate")
        self.assertEqual(code, 0, err)
        self.assertEqual((lab.kiln / "vendor/demo/LICENSE").read_text(), "MIT, upstream's and kiln's\n")
        code, err = lab.vendor("check", "demo")
        self.assertEqual(code, 0, err)

    def test_continue_refuses_while_the_conflict_markers_remain(self) -> None:
        self.vendored_with_a_change()
        lab = self.lab
        code, err = lab.vendor("demo", self.conflicting_upstream(), "--no-gate")
        self.assertEqual(code, 1, err)
        code, err = lab.vendor("demo", "--continue", "--no-gate")
        self.assertEqual(code, 1, err)
        self.assertIn("not resolved yet", err)
        self.assertIn("src/lib.rs", err)
        self.assertTrue(vendor.pending_dir("demo").exists())

    def test_continue_refuses_when_kilns_folder_changed_meanwhile(self) -> None:
        self.vendored_with_a_change()
        lab = self.lab
        code, err = lab.vendor("demo", self.conflicting_upstream(), "--no-gate")
        self.assertEqual(code, 1, err)
        (lab.kiln / "vendor/demo/LICENSE").write_text("MIT, edited meanwhile\n")
        code, err = lab.vendor("demo", "--continue", "--no-gate")
        self.assertEqual(code, 1, err)
        self.assertIn("changed since the re-vendor stopped", err)

    def test_a_revendor_waits_while_one_is_pending_and_abort_drops_it(self) -> None:
        self.vendored_with_a_change()
        lab = self.lab
        c2 = self.conflicting_upstream()
        code, err = lab.vendor("demo", c2, "--no-gate")
        self.assertEqual(code, 1, err)
        code, err = lab.vendor("demo", lab.c1, "--no-gate")
        self.assertEqual(code, 1, err)
        self.assertIn("waits on a conflict", err)
        before = snapshot(lab.kiln)
        code, err = lab.vendor("demo", "--abort")
        self.assertEqual(code, 0, err)
        self.assertFalse(vendor.pending_dir("demo").exists())
        self.assertEqual(snapshot(lab.kiln), before)
        code, err = lab.vendor("demo", lab.c1, "--no-gate")
        self.assertEqual(code, 0, err)

    def test_ac_m1_1_a_new_commit_is_taken_with_the_recorded_change_re_applied(self) -> None:
        """AC-M1.1: upstream moves elsewhere in the file and adds a file: one command takes it, the
        change re-applied by three-way merge and refreshed, the manifest's commit moved."""
        self.vendored_with_a_change()
        lab = self.lab
        lib = lab.up / "src/lib.rs"
        lib.write_text(lib.read_text().replace("f9() -> u32 { 9 }", "f9() -> u32 { 99 }"))
        write(lab.up, {"src/new.rs": "pub fn new() {}\n"})
        c3 = lab.commit_up("upstream changes f9, adds new.rs")
        code, err = lab.vendor("demo", c3, "--no-gate")
        self.assertEqual(code, 0, err)
        text = (lab.kiln / "vendor/demo/src/lib.rs").read_text()
        self.assertIn("f2() -> u32 { 2 + 0 } // kiln", text)
        self.assertIn("f9() -> u32 { 99 }", text)
        self.assertTrue((lab.kiln / "vendor/demo/src/new.rs").exists())
        self.assertIn(f'commit = "{c3}"', (lab.kiln / "vendor/MANIFEST.toml").read_text())
        code, err = lab.vendor("check", "demo")
        self.assertEqual(code, 0, err)

    def test_ac_m1_3_revendoring_at_the_current_commit_changes_no_byte(self) -> None:
        """AC-M1.3: re-vendoring at the commit the manifest pins leaves every file as it was."""
        self.vendored_with_a_change()
        lab = self.lab
        before = snapshot(lab.kiln)
        code, err = lab.vendor("demo", lab.c1, "--no-gate")
        self.assertEqual(code, 0, err)
        self.assertEqual(snapshot(lab.kiln), before)

    def test_an_edit_no_change_records_fails_the_check_and_stops_a_revendor(self) -> None:
        self.vendored_with_a_change()
        lab = self.lab
        lic = lab.kiln / "vendor/demo/LICENSE"
        lic.write_text("MIT, edited\n")
        code, err = lab.vendor("check", "demo")
        self.assertEqual(code, 1, err)
        self.assertIn("differs: LICENSE", err)
        before = snapshot(lab.kiln)
        code, err = lab.vendor("demo", lab.c1, "--no-gate")
        self.assertEqual(code, 1, err)
        self.assertIn("record them first", err)
        self.assertEqual(snapshot(lab.kiln), before)

    def test_a_change_upstream_makes_itself_is_dropped_and_said(self) -> None:
        self.vendored_with_a_change()
        lab = self.lab
        lib = lab.up / "src/lib.rs"
        lib.write_text(lib.read_text().replace("f2() -> u32 { 2 }", "f2() -> u32 { 2 + 0 } // kiln"))
        c4 = lab.commit_up("upstream takes kiln's change")
        code, err = lab.vendor("demo", c4, "--no-gate")
        self.assertEqual(code, 0, err)
        self.assertIn("dropped 0001-f2-says-kiln.patch", err)
        self.assertFalse((lab.kiln / "vendor/patches/demo/0001-f2-says-kiln.patch").exists())

    def test_check_works_on_a_tree_without_git_as_ci_checks_one_out(self) -> None:
        """kiln's pipelines check a commit out without its .git: check reads the files as they are."""
        self.vendored_with_a_change()
        lab = self.lab
        export = lab.tmp / "export"
        sh_git(lab.kiln, "add", "-A")
        sh_git(lab.kiln, "commit", "-q", "-m", "vendored")
        archive = subprocess.run(["git", "archive", "HEAD"], cwd=lab.kiln, env=GIT_ENV, capture_output=True, check=True).stdout
        export.mkdir()
        with tarfile.open(fileobj=io.BytesIO(archive)) as tf:
            tf.extractall(export, filter="data")
        self.assertFalse((export / ".git").exists())
        out = io.StringIO()
        with contextlib.redirect_stderr(out):
            code = vendor.main(["--root", str(export), "check"])
        self.assertEqual(code, 0, out.getvalue())
        (export / "vendor/demo/LICENSE").write_text("MIT, edited\n")
        out = io.StringIO()
        with contextlib.redirect_stderr(out):
            code = vendor.main(["--root", str(export), "check"])
        self.assertEqual(code, 1, out.getvalue())
        self.assertIn("differs: LICENSE", out.getvalue())

    def test_every_recorded_change_must_have_its_row_in_upstream_md(self) -> None:
        self.vendored_with_a_change()
        lab = self.lab
        write(lab.kiln, {"vendor/demo/UPSTREAM.md": "no rows\n"})
        code, err = lab.vendor("check", "demo")
        self.assertEqual(code, 1, err)
        self.assertIn("doesn't name 0001-f2-says-kiln.patch", err)

    def test_recording_with_nothing_edited_is_refused(self) -> None:
        self.vendored_with_a_change()
        code, err = self.lab.vendor("record", "demo", "again", "--why", "nothing")
        self.assertEqual(code, 1, err)
        self.assertIn("nothing to record", err)

    def test_an_abbreviated_commit_is_refused(self) -> None:
        lab = self.lab
        lab.manifest(lab.c1)
        code, err = lab.vendor("demo", lab.c1[:12], "--no-gate")
        self.assertEqual(code, 1, err)
        self.assertIn("not a full commit id", err)

    def test_the_gate_runs_after_a_revendor_and_its_failure_is_reported(self) -> None:
        lab = self.lab
        lab.manifest(lab.c1, gate='["exit 3"]')
        code, err = lab.vendor("demo", lab.c1)
        self.assertEqual(code, 1, err)
        self.assertIn("gate failed: `exit 3` exited 3", err)
        lab.manifest(lab.c1, gate='["true"]')
        code, err = lab.vendor("demo", lab.c1)
        self.assertEqual(code, 0, err)
        self.assertIn("gate passed", err)


class Pins(VendorTestCase):
    """A product that vendors other projects, and kiln's one copy of each (docs/vendoring.md)."""

    OTHER = "https://example.com/other"

    def product_pinning(self, other_commit: str, repository: str = OTHER, change: bytes | None = None) -> str:
        lab = self.lab
        write(lab.up, {"vendor/MANIFEST.toml": f'format = 1\n[projects.other]\nkind = "git"\nrepository = "{repository}"\ncommit = "{other_commit}"\npath = "vendor/other"\n'})
        write(lab.up, {"vendor/other/x.rs": "pub fn x() {}\n"})
        if change is not None:
            (lab.up / "vendor/patches/other").mkdir(parents=True, exist_ok=True)
            (lab.up / "vendor/patches/other/0001-x.patch").write_bytes(change)
        return lab.commit_up("vendors other")

    def kiln_vendors_other(self, commit: str) -> str:
        return f'\n[projects.other]\nkind = "git"\nrepository = "{self.OTHER}"\ncommit = "{commit}"\npath = "vendor/other"\n'

    def test_a_product_taking_another_at_another_commit_is_refused_naming_both(self) -> None:
        lab = self.lab
        theirs, ours = "a" * 40, "b" * 40
        c = self.product_pinning(theirs)
        lab.manifest(c, extra=self.kiln_vendors_other(ours))
        code, err = lab.vendor("demo", c, "--no-gate")
        self.assertEqual(code, 1, err)
        self.assertIn(f"takes other at {theirs}", err)
        self.assertIn(f"kiln takes other at {ours}", err)
        self.assertFalse((lab.kiln / "vendor/demo").exists())

    def test_a_hyper_light_product_kiln_lacks_is_refused(self) -> None:
        lab = self.lab
        c = self.product_pinning("a" * 40, repository="https://github.com/hyper-light/other")
        lab.manifest(c)
        code, err = lab.vendor("demo", c, "--no-gate")
        self.assertEqual(code, 1, err)
        self.assertIn("kiln holds one copy of each product: add other", err)

    def test_third_party_code_only_a_product_vendors_stays_inside_its_copy(self) -> None:
        """As Turso in Amphora: kiln's copy of the product is kiln's one copy of it."""
        lab = self.lab
        c = self.product_pinning("a" * 40)
        lab.manifest(c, take=None)
        code, err = lab.vendor("demo", c, "--no-gate")
        self.assertEqual(code, 0, err)
        self.assertTrue((lab.kiln / "vendor/demo/vendor/other/x.rs").exists())

    def test_pins_that_agree_pass(self) -> None:
        lab = self.lab
        pin = "a" * 40
        c = self.product_pinning(pin)
        lab.manifest(c, extra=self.kiln_vendors_other(pin))
        code, err = lab.vendor("demo", c, "--no-gate")
        self.assertEqual(code, 0, err)

    def test_a_product_recording_other_changes_than_kiln_is_refused(self) -> None:
        lab = self.lab
        pin = "a" * 40
        c = self.product_pinning(pin, change=b"Subject: x\nWhy: y\n\ndiff --git a/x b/x\n")
        lab.manifest(c, extra=self.kiln_vendors_other(pin))
        code, err = lab.vendor("demo", c, "--no-gate")
        self.assertEqual(code, 1, err)
        self.assertIn("records other changes to other than kiln does (0001-x.patch)", err)

    def test_kilns_copy_of_a_product_must_leave_its_copy_of_a_shared_project_out(self) -> None:
        lab = self.lab
        pin = "a" * 40
        c = self.product_pinning(pin)
        lab.manifest(c, extra=self.kiln_vendors_other(pin), take=None)
        code, err = lab.vendor("demo", c, "--no-gate")
        self.assertEqual(code, 1, err)
        self.assertIn('would hold a second other: add "vendor/other" to [projects.demo] exclude', err)
        manifest = (lab.kiln / "vendor/MANIFEST.toml").read_text()
        write(lab.kiln, {"vendor/MANIFEST.toml": manifest.replace('own = ["UPSTREAM.md"]\n', 'own = ["UPSTREAM.md"]\nexclude = ["vendor/other"]\n', 1)})
        code, err = lab.vendor("demo", c, "--no-gate")
        self.assertEqual(code, 0, err)
        self.assertFalse((lab.kiln / "vendor/demo/vendor/other").exists())


class Pieces(unittest.TestCase):
    def test_bun_json_renders_to_kilns_file_byte_for_byte(self) -> None:
        path = HERE.parent / "vendor/bun/bun.json"
        data = path.read_bytes()
        self.assertEqual(vendor.render_bun_json(json.loads(data)), data)

    def test_the_manifest_changes_only_the_pinned_line(self) -> None:
        text = '# c\n[projects.a]\ncommit = "1"\n\n[projects.b]\ncommit = "1"  # pinned\n'
        out = vendor.set_manifest_value(text, "b", "commit", "1", "2")
        self.assertEqual(out, '# c\n[projects.a]\ncommit = "1"\n\n[projects.b]\ncommit = "2"  # pinned\n')
        with self.assertRaises(vendor.Refused):
            vendor.set_manifest_value(text, "c", "commit", "1", "2")

    def test_image_layers_apply_whiteouts_opaque_folders_and_links(self) -> None:
        def layer(entries: list[tuple[str, str, bytes | str, int]]) -> bytes:
            buf = io.BytesIO()
            with tarfile.open(fileobj=buf, mode="w") as tf:
                for name, kind, value, mode in entries:
                    ti = tarfile.TarInfo(name)
                    ti.mode = mode
                    if kind == "dir":
                        ti.type = tarfile.DIRTYPE
                        tf.addfile(ti)
                    elif kind == "link":
                        ti.type = tarfile.SYMTYPE
                        ti.linkname = str(value)
                        tf.addfile(ti)
                    else:
                        data = value if isinstance(value, bytes) else value.encode()
                        ti.size = len(data)
                        tf.addfile(ti, io.BytesIO(data))
            return gzip.compress(buf.getvalue())

        fs = vendor.ImageFS(("lib", "usr/lib", "etc"))
        fs.apply(
            layer(
                [
                    ("lib", "dir", "", 0o755),
                    ("lib/ld.so", "file", b"loader", 0o755),
                    ("lib/gone", "file", b"x", 0o644),
                    ("usr/lib/libx.so.1", "file", b"x1", 0o644),
                    ("usr/lib/libx.so", "link", "libx.so.1", 0o777),
                    ("etc/alpine-release", "file", b"3.24.2\n", 0o644),
                ]
            ),
            "application/vnd.oci.image.layer.v1.tar+gzip",
        )
        fs.apply(
            layer([("lib/.wh.gone", "file", b"", 0o644), ("usr/lib/.wh..wh..opq", "file", b"", 0o644), ("usr/lib/new.so", "file", b"n", 0o644)]),
            "application/vnd.oci.image.layer.v1.tar+gzip",
        )
        fs.apply(
            layer(
                [
                    ("lib64", "link", "lib", 0o777),
                    ("a", "link", "b", 0o777),
                    ("b", "link", "/a", 0o777),
                    ("opt/big.so", "file", b"not kept", 0o644),
                    ("lib/elsewhere.so", "link", "/opt/big.so", 0o777),
                ]
            ),
            "application/vnd.oci.image.layer.v1.tar+gzip",
        )
        with self.assertRaises(vendor.Refused) as e:
            fs.read("/lib/elsewhere.so")
        self.assertIn("outside the folders kept", str(e.exception))
        self.assertEqual(fs.read("/lib/ld.so"), (0o755, b"loader"))
        self.assertEqual(fs.read("/lib64/ld.so"), (0o755, b"loader"))
        self.assertEqual(fs.read("/usr/lib/new.so"), (0o644, b"n"))
        self.assertEqual(fs.read("/etc/alpine-release")[1], b"3.24.2\n")
        for hidden in ("/lib/gone", "/usr/lib/libx.so", "/usr/lib/libx.so.1"):
            with self.assertRaises(vendor.Refused):
                fs.read(hidden)
        with self.assertRaises(vendor.Refused) as e:
            fs.read("/a/x")
        self.assertIn("symbolic links", str(e.exception))

    def test_apk_versions_and_upstream_versions(self) -> None:
        db = b"C:Q1\nP:musl\nV:1.2.6-r2\n\nP:libstdc++\nV:15.2.0-r5\n\n"
        v = vendor.apk_versions(db)
        self.assertEqual(v, {"musl": "1.2.6-r2", "libstdc++": "15.2.0-r5"})
        self.assertEqual(vendor.upstream_version(v["libstdc++"]), "15.2.0")


if __name__ == "__main__":
    unittest.main(verbosity=2)
