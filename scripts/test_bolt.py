#!/usr/bin/env python3
"""Tests of the release's code layout scripts (goal 11, plan 01a1a87b2e56 step 5): ci/release/bolt.sh,
ci/release/zig-relocs.sh, ci/release/build.sh's KILN_RELEASE_BOLT and scripts/memory_lab/bolt-hook.sh, each
"do X, expect Y". Offline: zig, llvm-bolt and LLVM's release are stand-ins made here; the tests that need a C
compiler, readelf, zstd or Linux's /proc skip without them.

    python3 scripts/test_bolt.py
"""

from __future__ import annotations

import hashlib
import io
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BOLT = ROOT / "ci" / "release" / "bolt.sh"
RELOCS = ROOT / "ci" / "release" / "zig-relocs.sh"
BUILD = ROOT / "ci" / "release" / "build.sh"
HOOK = ROOT / "scripts" / "memory_lab" / "bolt-hook.sh"


def run(cmd: list[str], env: dict[str, str] | None = None, cwd: Path | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(cmd, env={**os.environ, **(env or {})}, cwd=cwd, capture_output=True, text=True, timeout=120)


def script(path: Path, body: str) -> Path:
    path.write_text("#!/usr/bin/env bash\n" + body)
    path.chmod(0o755)
    return path


class Scripts(unittest.TestCase):
    def test_g11_s5_every_script_parses(self) -> None:
        for s in (BOLT, RELOCS, BUILD, HOOK):
            r = run(["bash", "-n", str(s)])
            self.assertEqual(r.returncode, 0, f"{s}: {r.stderr}")


class ZigRelocs(unittest.TestCase):
    """zig-relocs.sh: a link runs as zig runs it, then again as the ld.lld command zig said, with --emit-relocs."""

    def setUp(self) -> None:
        self.dir = Path(tempfile.mkdtemp())
        self.log = self.dir / "calls"
        # A zig that says its link command when ZIG_VERBOSE_LINK=1, as zig 0.17 does, and notes every call.
        self.zig = script(
            self.dir / "zig",
            f"""printf '%s|%s\\n' "${{ZIG_VERBOSE_LINK:-}}" "$*" >> {self.log}
if [ "$1" = cc ] && [ "${{ZIG_VERBOSE_LINK:-}}" = 1 ]; then
  [ -n "${{NO_LINK:-}}" ] || echo "ld.lld --error-limit=0 -static -o out /cache/crt1.o a.o /cache/libc.a" >&2
  echo "warning: zig said something" >&2
  exit "${{FAIL:-0}}"
fi
exit 0
""",
        )

    def tearDown(self) -> None:
        shutil.rmtree(self.dir)

    def relocs(self, *args: str, **env: str) -> subprocess.CompletedProcess[str]:
        return run([str(RELOCS), *args], env={"KILN_ZIG_REAL": str(self.zig), "TMPDIR": str(self.dir), **env})

    def calls(self) -> list[str]:
        return self.log.read_text().splitlines() if self.log.exists() else []

    def test_g11_s5_a_link_runs_again_with_its_relocations_and_a_build_id(self) -> None:
        r = self.relocs("cc", "-target", "x86_64-linux-musl", "a.o", "-o", "out")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(
            self.calls(),
            [
                "1|cc -target x86_64-linux-musl a.o -o out",
                "|ld.lld --error-limit=0 -static -o out /cache/crt1.o a.o /cache/libc.a --emit-relocs --build-id=sha1",
            ],
        )
        self.assertIn("warning: zig said something", r.stderr, "zig's own words still reach the caller")
        self.assertNotIn("ld.lld --error-limit", r.stderr, "but not the command it was asked to print")

    def test_g11_s5_what_doesnt_link_runs_as_it_is(self) -> None:
        for args in (["cc", "-c", "x.c", "-o", "x.o"], ["cc", "-E", "x.c"], ["cc", "--version"], ["ar", "rcs", "l.a", "x.o"], ["cc", "x.c"]):
            self.log.unlink(missing_ok=True)
            r = self.relocs(*args)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(self.calls(), ["|" + " ".join(args)], args)

    def test_g11_s5_a_failed_link_fails_and_isnt_run_again(self) -> None:
        r = self.relocs("cc", "a.o", "-o", "out", FAIL="3")
        self.assertEqual(r.returncode, 3)
        self.assertEqual(len(self.calls()), 1)

    def test_g11_s5_a_link_zig_didnt_run_says_its_output_has_no_relocations(self) -> None:
        r = self.relocs("cc", "a.o", "-o", "out", NO_LINK="1")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.calls()), 1)
        self.assertIn("keeps no relocations", r.stderr)


class Window(unittest.TestCase):
    """bolt.sh window: what ran between two dumps of one instrumented process, its counts since it started."""

    def test_g11_s5_the_window_is_the_last_dump_less_the_first(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            first, last, out = Path(d, "first.fdata"), Path(d, "last.fdata"), Path(d, "out.fdata")
            first.write_text("1 start 0 1 start 8 0 40\n1 main 4 1 work 0 1 10\n1 work 2 1 work 9 0 100\n")
            # start's edge ran only before the window; work's grew; a new edge appeared; one past 2^31 stays whole.
            last.write_text(
                "1 start 0 1 start 8 0 40\n1 main 4 1 work 0 3 25\n1 work 2 1 work 9 0 100\n"
                "1 work 9 1 tick 0 0 7\n1 tick 3 1 tick 6 0 4294967396\n"
            )
            r = run([str(BOLT), "window", str(first), str(last), str(out)])
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(
                out.read_text().splitlines(),
                ["1 main 4 1 work 0 2 15", "1 work 9 1 tick 0 0 7", "1 tick 3 1 tick 6 0 4294967396"],
            )


class Tools(unittest.TestCase):
    """bolt.sh tools: KILN_BOLT_BIN's, or LLVM's release fetched, checked against its digest and kept in part."""

    def stand_in_release(self, d: Path) -> Path:
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w") as tar:
            for name, body in {
                "bin/llvm-bolt": "#!/bin/sh\necho 'LLVM version 23.1.5'\n",
                "bin/merge-fdata": "#!/bin/sh\ncat \"$@\"\n",
                "bin/clang": "#!/bin/sh\n",
                "lib/libbolt_rt_instr.a": "!<arch>\n",
            }.items():
                data = body.encode()
                info = tarfile.TarInfo(f"LLVM-23.1.5-Linux-X64/{name}")
                info.size, info.mode = len(data), 0o755
                tar.addfile(info, io.BytesIO(data))
            link = tarfile.TarInfo("LLVM-23.1.5-Linux-X64/bin/perf2bolt")
            link.type, link.linkname = tarfile.SYMTYPE, "llvm-bolt"
            tar.addfile(link)
        plain = d / "release.tar"
        plain.write_bytes(buf.getvalue())
        subprocess.run(["zstd", "-q", "-f", "-o", str(d / "release.tar.zst"), str(plain)], check=True)
        return d / "release.tar.zst"

    @unittest.skipUnless(shutil.which("zstd") and shutil.which("curl"), "fetching takes zstd and curl")
    def test_g11_s5_llvms_release_is_fetched_checked_and_kept_in_part(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            tarball = self.stand_in_release(Path(d))
            digest = hashlib.sha256(tarball.read_bytes()).hexdigest()
            home = Path(d, "home")
            env = {"KILN_BOLT_HOME": str(home), "KILN_BOLT_URL": tarball.as_uri(), "KILN_BOLT_SHA256": digest, "KILN_BOLT_BIN": ""}
            r = run([str(BOLT), "tools"], env=env)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(r.stdout.strip(), str(home / "23.1.5" / "bin"))
            self.assertEqual(sorted(p.name for p in (home / "23.1.5" / "bin").iterdir()), ["llvm-bolt", "merge-fdata", "perf2bolt"])
            self.assertTrue((home / "23.1.5" / "lib" / "libbolt_rt_instr.a").exists(), "the instrumentation runtime too")
            self.assertEqual([p.name for p in home.iterdir()], ["23.1.5"], "the 1.2 GB download isn't kept")
            # Fetched once: a second call finds them and fetches nothing (the URL gone).
            r = run([str(BOLT), "tools"], env={**env, "KILN_BOLT_URL": "file:///nonexistent"})
            self.assertEqual(r.returncode, 0, r.stderr)

    @unittest.skipUnless(shutil.which("zstd") and shutil.which("curl"), "fetching takes zstd and curl")
    def test_g11_s5_a_release_not_the_pinned_one_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            tarball = self.stand_in_release(Path(d))
            env = {"KILN_BOLT_HOME": str(Path(d, "home")), "KILN_BOLT_URL": tarball.as_uri(), "KILN_BOLT_SHA256": "0" * 64, "KILN_BOLT_BIN": ""}
            r = run([str(BOLT), "tools"], env=env)
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("isn't the release pinned here", r.stderr)
            self.assertFalse(Path(d, "home", "23.1.5").exists())

    def test_g11_s5_kiln_bolt_bin_without_the_tools_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            r = run([str(BOLT), "tools"], env={"KILN_BOLT_BIN": d})
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("has no llvm-bolt, perf2bolt and merge-fdata", r.stderr)


class Check(unittest.TestCase):
    """bolt.sh check: a binary BOLT can move the functions of keeps its relocations and its symbols."""

    def test_g11_s5_a_file_that_isnt_elf_is_refused(self) -> None:
        with tempfile.NamedTemporaryFile() as f:
            f.write(b"not an ELF file")
            f.flush()
            r = run([str(BOLT), "check", f.name])
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("not an x86-64 ELF file", r.stderr)

    @unittest.skipUnless(
        shutil.which("cc") and shutil.which("readelf") and shutil.which("strip") and os.uname().machine == "x86_64",
        "takes cc, readelf and strip on x86-64",
    )
    def test_g11_s5_relocations_and_symbols_are_what_bolt_needs(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            src = Path(d, "m.c")
            src.write_text("int main(void) { return 0; }\n")
            kept, plain, stripped = Path(d, "kept"), Path(d, "plain"), Path(d, "stripped")
            subprocess.run(["cc", "-Wl,--emit-relocs", "-Wl,--build-id=sha1", "-o", str(kept), str(src)], check=True)
            subprocess.run(["cc", "-o", str(plain), str(src)], check=True)
            subprocess.run(["strip", "-o", str(stripped), str(kept)], check=True)
            r = run([str(BOLT), "check", str(kept)])
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("BOLT can move its functions", r.stdout)
            for b in (plain, stripped):
                r = run([str(BOLT), "check", str(b)])
                self.assertNotEqual(r.returncode, 0)
                self.assertIn("no .rela.text", r.stderr)


@unittest.skipUnless(
    os.environ.get("KILN_BOLT_BIN") and shutil.which("cc") and shutil.which("readelf") and shutil.which("strip"),
    "takes a real BOLT (KILN_BOLT_BIN), cc, readelf and strip",
)
class Optimize(unittest.TestCase):
    """bolt.sh optimize, with a real llvm-bolt: the profile's hot code in a segment of its own, the rest left."""

    def test_g11_s5_hot_code_moves_and_the_binary_keeps_its_stack_header_and_runs(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            src = Path(d, "m.c")
            src.write_text(
                "#include <stdio.h>\n"
                "__attribute__((noinline)) long hot(long n) { long s = 0; for (long i = 0; i < n; i++) s += i % 7; return s; }\n"
                "__attribute__((noinline)) long cold(long n) { return n * 3; }\n"
                "int main(int c, char **v) { printf(\"%ld\\n\", c > 5 ? cold(c) : hot(1000)); return 0; }\n"
            )
            binary, out = Path(d, "m"), Path(d, "m.bolt-out")
            subprocess.run(["cc", "-O2", "-no-pie", "-Wl,--emit-relocs", "-Wl,--build-id=sha1", "-o", str(binary), str(src)], check=True)
            # perf2bolt's basic aggregation format, by hand: samples at hot's first byte.
            profile = Path(d, "m.fdata")
            profile.write_text("no_lbr cycles:u:\n1 hot 0 1000\n")
            r = run([str(BOLT), "optimize", str(binary), str(profile), str(out)])
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(run([str(out)]).stdout, run([str(binary)]).stdout)
            heads = run(["readelf", "-lW", str(out)]).stdout
            self.assertIn("GNU_STACK", heads, "musl sizes its threads' stacks by it")
            loads = [l.split() for l in heads.splitlines() if l.strip().startswith("LOAD")]
            # hot's new home: in the last segment BOLT added, past every segment the linker made.
            hot = int(next(l.split()[0] for l in run(["nm", str(Path(f"{binary}.bolt"))]).stdout.splitlines() if l.endswith(" hot")), 16)
            self.assertGreaterEqual(hot, int(loads[-1][2], 16), heads)


class BuildSwitch(unittest.TestCase):
    """build.sh's KILN_RELEASE_BOLT, said wrong, stops the build before it starts."""

    def test_g11_s5_a_profile_that_isnt_there_is_refused(self) -> None:
        r = run([str(BUILD), "x86_64-unknown-linux-musl"], env={"KILN_RELEASE_BOLT": "/nonexistent/kiln.fdata"})
        self.assertEqual(r.returncode, 1)
        self.assertIn("is no profile", r.stderr)

    def test_g11_s5_a_build_without_linux_x86_64_has_nothing_to_lay_out(self) -> None:
        r = run([str(BUILD), "aarch64-unknown-linux-musl"], env={"KILN_RELEASE_BOLT": "relocs"})
        self.assertEqual(r.returncode, 1)
        self.assertIn("lays out x86_64-unknown-linux-musl", r.stderr)

    def test_g11_s5_a_relative_profile_is_the_callers(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            Path(d, "kiln.fdata").write_text("1 f 0 1 f 4 0 7\n")
            # The profile is checked before the targets, so a target list BOLT has nothing in stops the build there,
            # never starting one, and says the profile was found where the caller is, not in the checkout.
            r = run([str(BUILD), "aarch64-unknown-linux-musl"], env={"KILN_RELEASE_BOLT": "kiln.fdata"}, cwd=Path(d))
            self.assertEqual(r.returncode, 1)
            self.assertNotIn("is no profile", r.stderr)
            self.assertIn("lays out x86_64-unknown-linux-musl", r.stderr)


@unittest.skipUnless(sys.platform.startswith("linux"), "the hook reads Linux's /proc")
class Hook(unittest.TestCase):
    """bolt-hook.sh: the binary's resident pages by mapping as the window ends, and an instrumented window."""

    def test_g11_s5_the_window_end_says_the_binarys_pages_by_mapping(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            p = subprocess.Popen(["sleep", "30"])
            try:
                time.sleep(0.2)
                r = run([str(HOOK), "window-end", str(p.pid), str(Path(d, "r.hook"))], env={"BOLT_RECORD": "none"})
                self.assertEqual(r.returncode, 0, r.stderr)
                lines = Path(d, "r.hook", "pages.txt").read_text().splitlines()
            finally:
                p.kill()
                p.wait()
            maps = [l for l in lines if l.split()[0].count("-") == 1 and "rss_kb=" in l]
            self.assertTrue(any(" r-xp " in l for l in maps), lines)
            self.assertTrue(lines[-2].startswith("binary rss_kb="), lines)
            self.assertTrue(lines[-1].startswith("process rss_file_kb="), lines)
            total = sum(int(l.rsplit("rss_kb=", 1)[1]) for l in maps)
            self.assertEqual(lines[-2], f"binary rss_kb={total}")

    def test_g11_s5_an_instrumented_window_is_its_last_dump_less_its_first(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            prefix = str(Path(d, "dump"))
            # A server whose child is its dump's writer, as BOLT's runtime forks one.
            server = subprocess.Popen(["bash", "-c", "sleep 30 & wait"])
            try:
                child = None
                for _ in range(50):
                    kids = [k for t in Path(f"/proc/{server.pid}/task").iterdir() for k in (t / "children").read_text().split()]
                    if kids:
                        child = kids[0]
                        break
                    time.sleep(0.1)
                self.assertIsNotNone(child)
                dump = Path(f"{prefix}.{child}.fdata")
                dump.write_text("1 start 0 1 start 8 0 40\n1 main 4 1 work 0 0 10\n")
                hook = Path(d, "r.hook")
                # From the server's start by default: the window's start takes nothing then.
                env = {"BOLT_RECORD": "instrumented", "BOLT_DUMP": prefix}
                r = run([str(HOOK), "window-start", str(server.pid), str(hook)], env=env)
                self.assertEqual(r.returncode, 0, r.stderr)
                self.assertFalse(Path(hook, "instr-first.fdata").exists())
                r = run([str(HOOK), "server-up", str(server.pid), str(hook)], env=env)
                self.assertEqual(r.returncode, 0, r.stderr)
                dump.write_text("1 start 0 1 start 8 0 40\n1 main 4 1 work 0 0 35\n")
                r = run([str(HOOK), "window-end", str(server.pid), str(hook)], env=env)
                self.assertEqual(r.returncode, 0, r.stderr)
            finally:
                server.kill()
                server.wait()
            self.assertEqual(Path(hook, "instr-window.fdata").read_text(), "1 main 4 1 work 0 0 25\n")


if __name__ == "__main__":
    unittest.main(verbosity=2)
